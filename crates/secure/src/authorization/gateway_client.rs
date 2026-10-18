//! Client-held request bindings for independently pinned gateway replies.

use super::{
    client_policy_import::{
        ClientPolicyContextReference, confirmed_client_policy_source_node,
        record_confirmed_client_policy_metadata, require_confirmed_client_policy,
    },
    execution_handoff::ExecutionProofUsage,
    gateway_forward::{
        GatewayForwardHeader, GatewayOriginal, GatewayOutcome, GatewayReplyHandle,
        GatewayReplyHeader, GatewayRouteApproval, verify_gateway_reply,
    },
    prepare::{PrepareIntent, check_raw_context},
    *,
};
use crate::signature_verification::{ScopedSignatureVerification, SignatureVerification};
use serde::de::DeserializeOwned;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmedContext {
    client_policy_context: ClientPolicyContextReference,
    #[serde(rename = "confirmation_hash")]
    _confirmation_hash: ObjectHash,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedReply {
    body: Value,
    invocation: Invocation,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct GatewayClientReplyHandle {
    inner: GatewayReplyHandle,
    original: GatewayOriginal,
    usage: ExecutionProofUsage,
}

fn client_time(now: f64) -> Result<u64> {
    require(
        now.is_finite() && now >= 0.0 && now.fract() == 0.0 && now <= 9_007_199_254_740_991.0,
        "invalid gateway client time",
    )?;
    Ok(now as u64)
}

fn parse_bounded<T: DeserializeOwned>(json: &str) -> Result<T> {
    require(
        json.len() <= MAX_PROOF_BYTES,
        "gateway client JSON exceeds byte limit",
    )?;
    serde_json::from_str(json)
        .map_err(|_| AuthorizationError::InvalidProof("invalid gateway client JSON"))
}

fn encode_bounded<T: Serialize>(value: &T) -> Result<String> {
    let json = serde_json::to_string(value)
        .map_err(|_| AuthorizationError::InvalidProof("gateway client encoding failed"))?;
    require(
        json.len() <= MAX_PROOF_BYTES,
        "gateway client JSON exceeds byte limit",
    )?;
    Ok(json)
}

fn finish_usage(usage: &mut ExecutionProofUsage) -> Result<()> {
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()
}

fn record_original(usage: &mut ExecutionProofUsage, original: &GatewayOriginal) -> Result<()> {
    match original {
        GatewayOriginal::Prepare { intent, .. } => usage.record_signed(intent)?,
        GatewayOriginal::Invoke { invocation, .. } | GatewayOriginal::Status { invocation, .. } => {
            usage.record_signed(invocation)?;
        }
    }
    usage.record_metadata(original.body())
}

/// The authorization and gateway pin must belong to the client's current local session.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareGatewayClientReply))]
pub fn prepare_gateway_client_reply(
    authorization: &VerifiedClientAuthorization,
    original_json: &str,
    expected_route_hash: &str,
    pinned_gateway_key: &str,
    identity: &Identity,
    now_ms: f64,
) -> crate::Result<GatewayClientReplyHandle> {
    let _scope = ScopedSignatureVerification::new();
    (|| -> Result<_> {
        let now = client_time(now_ms)?;
        require_confirmed_client_policy(authorization)?;
        let target = confirmed_client_policy_source_node(authorization)?.clone();
        let gateway = PublicKey::parse(pinned_gateway_key)?;
        let client = PublicKey::of(identity);
        let original: GatewayOriginal = parse_bounded(original_json)?;
        let request = original.request();
        require(
            now < authorization.expires_at_ms()
                && target == authorization.audience_public_key
                && client != gateway
                && client != target
                && gateway != target
                && original.issuer() == &client
                && request.device == authorization.scope.device_id
                && request.actor == Actor::Account(authorization.scope.account_id.clone())
                && request.initiator == request.actor
                && request.initiating_grant.is_none()
                && request.resource.space == authorization.scope.space_id
                && scope_covers(&authorization.grant_scope, &request.resource)
                && authorization.actions.contains(&request.action)
                && request.audience == authorization.audience
                && request.grant_leaf == authorization.grant_leaf
                && authorization
                    .heads
                    .iter()
                    .all(|head| request.expected_policies.contains(head))
                && context_tighter(&authorization.constraints.context, &request.context),
            "gateway original differs from the confirmed client authorization",
        )?;
        if let GatewayOriginal::Prepare { body, intent } = &original {
            check_raw_context(&intent.payload, body, &target)?;
        }
        let context: ConfirmedContext =
            parse_bounded(&authorization.client_policy_context_json().map_err(|_| {
                AuthorizationError::InvalidProof("gateway client authorization is not confirmed")
            })?)?;
        let route_hash = ObjectHash::parse(expected_route_hash)?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_budget(&authorization.budget)?;
        record_confirmed_client_policy_metadata(authorization, &mut usage)?;
        record_original(&mut usage, &original)?;
        let inner = GatewayReplyHandle::new(
            original.clone(),
            context.client_policy_context,
            route_hash,
            gateway,
            target,
            &client,
            now,
        )?;
        finish_usage(&mut usage)?;
        Ok(GatewayClientReplyHandle {
            inner,
            original,
            usage,
        })
    })()
    .map_err(|error| error.to_string())
}

fn check_prepared_reply(
    intent: &PrepareIntent,
    outcome: &Value,
    forward: &Signed<GatewayForwardHeader>,
    reply: &Signed<GatewayReplyHeader>,
    now: u64,
) -> Result<()> {
    let prepared: PreparedReply = serde_json::from_value(outcome.clone())
        .map_err(|_| AuthorizationError::InvalidProof("invalid gateway prepare reply"))?;
    let template = &prepared.invocation;
    let mut expected = intent.request.clone();
    expected.body_hash = ObjectHash::body(&prepared.body)?;
    expected.nonce = template.nonce.clone();
    expected.not_before_ms = template.not_before_ms;
    expected.expires_at_ms = template.expires_at_ms;
    require(
        template == &expected
            && template.not_before_ms >= intent.request.not_before_ms
            && template.not_before_ms >= forward.payload.issued_at_ms
            && template.expires_at_ms <= intent.request.expires_at_ms
            && template.expires_at_ms <= forward.payload.expires_at_ms
            && template.expires_at_ms <= reply.payload.expires_at_ms,
        "gateway preparation changed client authorization or extended its window",
    )?;
    validate_window(template.not_before_ms, template.expires_at_ms, now)?;
    let prepared_intent = PrepareIntent {
        request: template.clone(),
        ..intent.clone()
    };
    check_raw_context(&prepared_intent, &prepared.body, &reply.payload.target_key)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl GatewayClientReplyHandle {
    /// The caller must still check its allowed body normalization before signing preparation.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyReply))]
    pub fn verify_reply(
        &self,
        forward_json: &str,
        reply_json: &str,
        outcome_json: &str,
        now_ms: f64,
    ) -> crate::Result<String> {
        let _scope = ScopedSignatureVerification::new();
        (|| -> Result<_> {
            let now = client_time(now_ms)?;
            let forward: Signed<GatewayForwardHeader> = parse_bounded(forward_json)?;
            let reply: Signed<GatewayReplyHeader> = parse_bounded(reply_json)?;
            let outcome: GatewayOutcome = parse_bounded(outcome_json)?;
            require(
                canonical(&(&self.original, &forward, &reply, &outcome))?.len() <= MAX_PROOF_BYTES,
                "gateway reply carrier exceeds byte limit",
            )?;
            let verified = verify_gateway_reply(&self.inner, &forward, &reply, &outcome, now)?;
            if let (GatewayOriginal::Prepare { intent, .. }, GatewayOutcome::Ok(value)) =
                (&self.original, verified.outcome())
            {
                check_prepared_reply(&intent.payload, value, &forward, &reply, now)?;
            }
            let mut usage = self.usage.clone();
            usage.record_signed(&forward)?;
            usage.record_signed(&reply)?;
            usage.record_metadata(verified.outcome())?;
            finish_usage(&mut usage)?;
            encode_bounded(verified.outcome())
        })()
        .map_err(|error| error.to_string())
    }
}

/// Produces an owner signature; current ownership and route generation are checked by the gateway.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signGatewayRouteApproval))]
pub fn sign_gateway_route_approval(
    payload_json: &str,
    identity: &Identity,
) -> crate::Result<String> {
    (|| -> Result<_> {
        let payload: GatewayRouteApproval = parse_bounded(payload_json)?;
        encode_bounded(&Signed::sign(payload, identity)?)
    })()
    .map_err(|error| error.to_string())
}

/// Verifies only the original signature, without granting gateway authority.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = gatewayRouteApprovalHash))]
pub fn gateway_route_approval_hash(signed_json: &str) -> crate::Result<String> {
    let _scope = ScopedSignatureVerification::new();
    (|| -> Result<_> {
        let signed: Signed<GatewayRouteApproval> = parse_bounded(signed_json)?;
        signed.verify_signature()?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_signed(&signed)?;
        finish_usage(&mut usage)?;
        Ok(signed.hash()?.to_string())
    })()
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorization::{
        gateway_forward::{GatewayRouteState, sign_gateway_reply, verify_directed_gateway},
        prepare::{PrepareContext, sign_client_prepare_intent},
        remote_execution_policy::tests::confirmed_source,
        tests::{Fixture, NOW},
    };
    use serde_json::json;

    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }

    fn wire<T: Serialize>(value: &T) -> String {
        serde_json::to_string(value).unwrap()
    }

    fn confirmed_client() -> (VerifiedClientAuthorization, Identity, Identity) {
        let f = Fixture::new();
        let user = key(4);
        let space = SpaceId::personal(&f.account);
        let grant = f.state.grants.values().next().unwrap();
        let ResourceScope::Exact(resource) = &grant.payload.scope else {
            panic!("fixture requires one exact resource");
        };
        let mut journal = Vec::new();
        for (index, document) in f
            .state
            .account_history(&f.account)
            .unwrap()
            .iter()
            .enumerate()
        {
            journal.push(if index == 0 {
                JournalEntry::AccountGenesis(document.clone())
            } else {
                JournalEntry::Account(document.clone())
            });
            let stamp = account_stamp(document).unwrap();
            journal.extend(
                f.state
                    .devices
                    .values()
                    .filter(|certificate| certificate.payload.issued_under == stamp)
                    .cloned()
                    .map(JournalEntry::Device),
            );
        }
        for (index, document) in f.state.space_history(&space).unwrap().iter().enumerate() {
            journal.push(if index == 0 {
                JournalEntry::SpaceGenesis(document.clone())
            } else {
                JournalEntry::Space(document.clone())
            });
            let stamp = space_stamp(document).unwrap();
            journal.extend(
                f.state
                    .resources
                    .values()
                    .filter(|record| record.payload.issued_under == stamp)
                    .cloned()
                    .map(JournalEntry::Resource),
            );
            journal.extend(
                f.state
                    .grants
                    .values()
                    .filter(|grant| grant.payload.issued_under == stamp)
                    .cloned()
                    .map(JournalEntry::Grant),
            );
        }
        let pins = vec![
            PolicyPin {
                domain: PolicyDomain::Account(f.account.clone()),
                genesis_root: PublicKey::of(&f.root),
                minimum_head: f
                    .state
                    .head(&PolicyDomain::Account(f.account.clone()))
                    .cloned(),
            },
            PolicyPin {
                domain: PolicyDomain::Space(space.clone()),
                genesis_root: PublicKey::of(&f.root),
                minimum_head: f.state.head(&PolicyDomain::Space(space)).cloned(),
            },
        ];
        let auth = confirmed_source(
            &ClientAuthorizationBundle {
                journal,
                grant_leaf: grant.hash().unwrap(),
            },
            &pins,
            &user,
            &f.node,
            resource,
            NOW,
        )
        .unwrap();
        (auth, user, f.node)
    }

    struct Exchange {
        authorization: VerifiedClientAuthorization,
        client: Identity,
        gateway: Identity,
        target: Identity,
        original: GatewayOriginal,
        forward: Signed<GatewayForwardHeader>,
        outcome: GatewayOutcome,
        handle: GatewayClientReplyHandle,
    }

    impl Exchange {
        fn new() -> Self {
            let (authorization, client, target) = confirmed_client();
            let gateway = key(91);
            let record = &authorization.resources[0].payload;
            let body = json!({
                "method":"message", "space_id":record.space, "resource_id":record.resource,
                "conversation_id":"chat", "text":"hello", "authorization_id":"pending_write"
            });
            let context = PrepareContext {
                context: InvocationContext {
                    conversation: Some("chat".into()),
                    workspace_resource: Some(record.resource.clone()),
                    ..Default::default()
                },
                ..Default::default()
            };
            let intent: Signed<PrepareIntent> = serde_json::from_str(
                &sign_client_prepare_intent(
                    &authorization,
                    &wire(&body),
                    &wire(&context),
                    "message_append",
                    "pending_write",
                    PublicKey::of(&gateway).as_str(),
                    &client,
                    NOW as f64,
                )
                .unwrap(),
            )
            .unwrap();
            let mut template = intent.payload.request.clone();
            template.not_before_ms = NOW + 1;
            template.expires_at_ms = NOW + 500;
            template.nonce = Nonce::from_bytes([111; 32]);
            let outcome = GatewayOutcome::Ok(json!({"body":body,"invocation":template}));
            let original = GatewayOriginal::Prepare { body, intent };
            let context: ConfirmedContext =
                parse_bounded(&authorization.client_policy_context_json().unwrap()).unwrap();
            let forward = Signed::sign(
                GatewayForwardHeader {
                    version: AUTH_VERSION,
                    purpose: original.purpose(),
                    route_approval_hash: ObjectHash::body(&"independently pinned route").unwrap(),
                    gateway_key: PublicKey::of(&gateway),
                    client_key: PublicKey::of(&client),
                    target_key: PublicKey::of(&target),
                    source: context.client_policy_context,
                    original_hash: original.hash().unwrap(),
                    authorization_id: original.request().operation_id.clone(),
                    lookup_operation_id: None,
                    nonce: Nonce::from_bytes([112; 32]),
                    issued_at_ms: NOW,
                    expires_at_ms: NOW + 500,
                },
                &gateway,
            )
            .unwrap();
            let handle = prepare_gateway_client_reply(
                &authorization,
                &wire(&original),
                forward.payload.route_approval_hash.as_str(),
                PublicKey::of(&gateway).as_str(),
                &client,
                NOW as f64,
            )
            .unwrap();
            Self {
                authorization,
                client,
                gateway,
                target,
                original,
                forward,
                outcome,
                handle,
            }
        }

        fn reply(
            &self,
            forward: &Signed<GatewayForwardHeader>,
            outcome: &GatewayOutcome,
        ) -> Signed<GatewayReplyHeader> {
            let directed = verify_directed_gateway(
                &PublicKey::of(&self.gateway),
                forward,
                &self.original,
                &PublicKey::of(&self.target),
                NOW + 1,
            )
            .unwrap();
            sign_gateway_reply(&directed, &self.original, outcome, &self.target, NOW + 1).unwrap()
        }

        fn verify(
            &self,
            forward: &Signed<GatewayForwardHeader>,
            reply: &Signed<GatewayReplyHeader>,
            outcome: &GatewayOutcome,
        ) -> crate::Result<String> {
            self.handle.verify_reply(
                &wire(forward),
                &wire(reply),
                &wire(outcome),
                (NOW + 1) as f64,
            )
        }
    }

    #[test]
    fn confirmed_client_verifies_real_target_reply_and_rejects_swapped_bindings() {
        let exchange = Exchange::new();
        let reply = exchange.reply(&exchange.forward, &exchange.outcome);
        assert!(exchange.original.request().expires_at_ms > exchange.forward.payload.expires_at_ms);
        assert_eq!(
            serde_json::from_str::<GatewayOutcome>(
                &exchange
                    .verify(&exchange.forward, &reply, &exchange.outcome)
                    .unwrap()
            )
            .unwrap(),
            exchange.outcome,
        );
        let wrong_target = Signed::sign(reply.payload.clone(), &key(92)).unwrap();
        assert!(
            exchange
                .verify(&exchange.forward, &wrong_target, &exchange.outcome)
                .is_err()
        );
        let mut changed = exchange.forward.payload.clone();
        changed.route_approval_hash = ObjectHash::body(&"other route").unwrap();
        let other_route = Signed::sign(changed, &exchange.gateway).unwrap();
        let other_reply = exchange.reply(&other_route, &exchange.outcome);
        assert!(
            exchange
                .verify(&other_route, &other_reply, &exchange.outcome)
                .is_err()
        );
        let mut changed = exchange.forward.payload.clone();
        changed.original_hash = ObjectHash::body(&"other pending request").unwrap();
        let other_original = Signed::sign(changed, &exchange.gateway).unwrap();
        assert!(
            exchange
                .verify(&other_original, &reply, &exchange.outcome)
                .is_err()
        );
        let wrong_gateway = Signed::sign(exchange.forward.payload.clone(), &key(93)).unwrap();
        assert!(
            exchange
                .verify(&wrong_gateway, &reply, &exchange.outcome)
                .is_err()
        );
        assert!(
            exchange
                .verify(&exchange.forward, &reply, &GatewayOutcome::Error(json!({})))
                .is_err()
        );
    }

    #[test]
    fn target_signature_cannot_extend_prepare_window_or_change_authority() {
        let exchange = Exchange::new();
        let GatewayOutcome::Ok(value) = &exchange.outcome else {
            unreachable!()
        };
        let parsed: PreparedReply = serde_json::from_value(value.clone()).unwrap();
        for field in [
            "expires_at_ms",
            "device",
            "grant_leaf",
            "expected_policies",
            "context",
        ] {
            let mut changed = value.clone();
            changed["invocation"][field] = match field {
                "expires_at_ms" => json!(exchange.forward.payload.expires_at_ms + 1),
                "device" => json!(DeviceId::from_key(&PublicKey::of(&exchange.gateway))),
                "grant_leaf" => json!(ObjectHash::body(&"other grant").unwrap()),
                "expected_policies" => json!([]),
                "context" => json!(InvocationContext::default()),
                _ => unreachable!(),
            };
            let outcome = GatewayOutcome::Ok(changed);
            let reply = exchange.reply(&exchange.forward, &outcome);
            assert!(
                exchange
                    .verify(&exchange.forward, &reply, &outcome)
                    .is_err(),
                "{field}"
            );
        }
        assert!(parsed.invocation.expires_at_ms < exchange.original.request().expires_at_ms);
    }

    #[test]
    fn reply_handle_requires_confirmed_authorization_local_pins_and_integer_time() {
        let exchange = Exchange::new();
        let create = |auth: &VerifiedClientAuthorization, gateway: &Identity, now| {
            prepare_gateway_client_reply(
                auth,
                &wire(&exchange.original),
                exchange.forward.payload.route_approval_hash.as_str(),
                PublicKey::of(gateway).as_str(),
                &exchange.client,
                now,
            )
        };
        let mut unconfirmed = exchange.authorization.clone();
        unconfirmed.client_policy_context = None;
        assert!(create(&unconfirmed, &exchange.gateway, NOW as f64).is_err());
        let mut wrong_target = exchange.authorization.clone();
        wrong_target.audience_public_key = PublicKey::of(&key(92));
        assert!(create(&wrong_target, &exchange.gateway, NOW as f64).is_err());
        assert!(create(&exchange.authorization, &key(94), NOW as f64).is_err());
        for now in [
            f64::NAN,
            f64::INFINITY,
            -1.0,
            NOW as f64 + 0.5,
            9_007_199_254_740_992.0,
        ] {
            assert!(create(&exchange.authorization, &exchange.gateway, now).is_err());
        }
        assert!(
            create(
                &exchange.authorization,
                &exchange.gateway,
                exchange.authorization.expires_at_ms() as f64
            )
            .is_err()
        );
    }

    #[test]
    fn route_wrapper_preserves_typed_original_and_rejects_forgery_and_oversize() {
        let (auth, client, target) = confirmed_client();
        let owner = key(90);
        let route = GatewayRouteApproval {
            version: AUTH_VERSION,
            route_id: Nonce::from_bytes([113; 32]),
            generation: 1,
            gateway: PublicKey::of(&key(91)),
            client_account: auth.scope.account_id.clone(),
            client_key: PublicKey::of(&client),
            target_key: PublicKey::of(&target),
            resource: ResourceRef {
                space: auth.scope.space_id.clone(),
                resource: auth.resources[0].payload.resource.clone(),
            },
            allowed_actions: vec![Action::MessageAppend],
            source_pins: auth.policy_pins.clone(),
            state: GatewayRouteState::Active,
            issued_at_ms: NOW,
            expires_at_ms: NOW + 500,
        };
        let signed = sign_gateway_route_approval(&wire(&route), &owner).unwrap();
        let parsed: Signed<GatewayRouteApproval> = serde_json::from_str(&signed).unwrap();
        assert_eq!(parsed.payload, route);
        assert_eq!(
            gateway_route_approval_hash(&signed).unwrap(),
            parsed.hash().unwrap().as_str()
        );
        let mut forged = parsed;
        forged.payload.generation += 1;
        assert!(gateway_route_approval_hash(&wire(&forged)).is_err());
        assert!(sign_gateway_route_approval(&" ".repeat(MAX_PROOF_BYTES + 1), &owner).is_err());
        assert!(gateway_route_approval_hash(&" ".repeat(MAX_PROOF_BYTES + 1)).is_err());
    }
}
