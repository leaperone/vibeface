//! Read-only qualification of raw client intent before business-data normalization.

use super::*;
use serde_json::Value;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PrepareContext {
    pub context: InvocationContext,
    pub binding_generation: Option<u64>,
    pub canonical_cwd: Option<String>,
    pub execution_target_public_key: Option<PublicKey>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PrepareIntent {
    pub request: Invocation,
    pub binding_generation: Option<u64>,
    pub canonical_cwd: Option<String>,
    pub execution_target_public_key: Option<PublicKey>,
}
signed_object!(PrepareIntent, "prepare-intent-v2");

#[derive(Clone, Debug)]
pub struct VerifiedPrepareAuthorization {
    resource: ResourceRef,
    raw_body: Value,
    action: Action,
    operation_id: String,
    audience: Audience,
    presenter: DeviceId,
}
impl VerifiedPrepareAuthorization {
    pub fn resource(&self) -> &ResourceRef {
        &self.resource
    }
    pub fn raw_body(&self) -> &Value {
        &self.raw_body
    }
    pub fn action(&self) -> Action {
        self.action
    }
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn audience(&self) -> &Audience {
        &self.audience
    }
    pub fn presenter(&self) -> &DeviceId {
        &self.presenter
    }
}

fn text(value: Option<&Value>) -> Result<Option<&str>> {
    value
        .map(|value| {
            value.as_str().ok_or(AuthorizationError::InvalidProof(
                "raw context value is not text",
            ))
        })
        .transpose()
}
fn match_text(
    expected: Option<&str>,
    values: &[Option<&str>],
    reason: &'static str,
) -> Result<bool> {
    for value in values.iter().flatten() {
        require(expected == Some(*value), reason)?;
    }
    Ok(values.iter().any(Option::is_some))
}
fn raw_resource(body: &Value) -> Result<ResourceRef> {
    require(body.is_object(), "raw request must be an object")?;
    Ok(ResourceRef {
        space: SpaceId::parse(
            text(body.get("space_id"))?
                .ok_or(AuthorizationError::InvalidProof("raw request has no space"))?,
        )?,
        resource: ResourceId::parse(text(body.get("resource_id"))?.ok_or(
            AuthorizationError::InvalidProof("raw request has no resource"),
        )?)?,
    })
}
fn execution_key<'a>(
    intent: &'a PrepareIntent,
    audience_key: &'a PublicKey,
) -> Result<&'a PublicKey> {
    let Some(session) = &intent.request.context.execution_session else {
        require(
            intent.execution_target_public_key.is_none(),
            "execution key has no selected session",
        )?;
        return Ok(audience_key);
    };
    if session.target == intent.request.audience.device {
        require(
            intent
                .execution_target_public_key
                .as_ref()
                .is_none_or(|key| key == audience_key),
            "execution key differs from audience pin",
        )?;
        return Ok(audience_key);
    }
    let key =
        intent
            .execution_target_public_key
            .as_ref()
            .ok_or(AuthorizationError::InvalidProof(
                "independently verified execution key is required",
            ))?;
    require(
        DeviceId::from_key(key) == session.target,
        "execution key differs from selected device",
    )?;
    Ok(key)
}
fn raw_binding(value: Option<&Value>) -> Result<Option<task_binding::SignedBinding>> {
    value
        .filter(|value| !value.is_null())
        .map(|value| {
            serde_json::from_value(value.clone())
                .map_err(|_| AuthorizationError::InvalidProof("invalid raw task binding"))
        })
        .transpose()
}
fn check_raw_context(intent: &PrepareIntent, body: &Value, audience_key: &PublicKey) -> Result<()> {
    let i = &intent.request;
    let context = &i.context;
    validate_context(context)?;
    require(
        !matches!(i.actor, Actor::Account(_)) || context.sdk_session.is_none(),
        "account prepare cannot claim SDK adapter session",
    )?;
    require(
        context
            .workspace_resource
            .as_ref()
            .is_none_or(|resource| resource == &i.resource.resource),
        "raw workspace belongs to another resource",
    )?;
    let request = body.get("request");
    if let Some(request) = request {
        require(
            request.is_object(),
            "raw operation request must be an object",
        )?;
        if let Some(space) = text(request.get("space_id"))? {
            require(
                space == i.resource.space.as_str(),
                "raw operation belongs to another space",
            )?;
        }
    }
    let top_binding = raw_binding(body.get("binding"))?;
    let nested_binding = raw_binding(request.and_then(|request| request.get("binding")))?;
    if let (Some(top), Some(nested)) = (&top_binding, &nested_binding) {
        require(top == nested, "raw request contains conflicting bindings")?;
    }
    let binding = top_binding.or(nested_binding);
    if let Some(binding) = &binding {
        // These fields constrain preparation; final admission authenticates the binding.
        task_binding::validate_binding(&binding.binding)
            .map_err(|_| AuthorizationError::InvalidProof("invalid raw task binding"))?;
        require(
            binding.binding.network_id == i.resource.space.as_str(),
            "raw binding belongs to another space",
        )?;
    }
    let conversation = [
        text(body.get("conversation_id"))?,
        text(request.and_then(|request| request.get("conversation_id")))?,
        binding
            .as_ref()
            .map(|binding| binding.binding.conversation_id.as_str()),
    ];
    let found = match_text(
        context.conversation.as_deref(),
        &conversation,
        "raw conversation differs from prepare context",
    )?;
    require(
        context.conversation.is_none() || found,
        "prepare conversation has no raw evidence",
    )?;

    let task = [
        text(body.get("task_ref"))?,
        binding
            .as_ref()
            .map(|binding| binding.binding.task_ref.as_str()),
    ];
    let expected_task = context.task.as_ref().map(|reference| reference.as_str());
    let found = match_text(
        expected_task,
        &task,
        "raw task reference differs from prepare context",
    )?;
    require(
        context.task.is_none() || found,
        "prepare task has no raw creation reference",
    )?;

    let raw_generation = body
        .get("binding_generation")
        .map(|value| {
            value.as_u64().ok_or(AuthorizationError::InvalidProof(
                "invalid raw binding generation",
            ))
        })
        .transpose()?;
    let generations = [
        raw_generation,
        binding.as_ref().map(|binding| binding.binding.generation),
    ];
    for generation in generations.iter().flatten() {
        require(
            *generation > 0 && intent.binding_generation == Some(*generation),
            "raw binding generation differs from prepare context",
        )?;
    }
    require(
        intent.binding_generation.is_none() || generations.iter().any(Option::is_some),
        "prepare generation has no raw binding evidence",
    )?;

    let sessions = [
        text(body.get("session_id"))?,
        text(request.and_then(|request| request.get("session_id")))?,
        binding
            .as_ref()
            .map(|binding| binding.binding.session_id.as_str()),
    ];
    let found = match_text(
        context
            .execution_session
            .as_ref()
            .map(|session| session.session_id.as_str()),
        &sessions,
        "raw execution session differs from prepare context",
    )?;
    if context.execution_session.is_some() {
        require(found, "prepare execution session has no raw session")?;
    }
    if body["method"].as_str() == Some("operate") {
        require(
            context
                .execution_session
                .as_ref()
                .is_some_and(|session| session.target == i.audience.device),
            "operation execution target differs from audience",
        )?;
    }
    let raw_target = hex::encode(Sha256::digest(
        crate::decode32(execution_key(intent, audience_key)?.as_str())
            .map_err(|_| AuthorizationError::InvalidProof("invalid audience key"))?,
    ));
    match_text(
        Some(&raw_target),
        &[
            text(body.get("target_node"))?,
            text(request.and_then(|request| request.get("target_node")))?,
            binding
                .as_ref()
                .map(|binding| binding.binding.target_node.as_str()),
        ],
        "raw execution target differs from audience",
    )?;

    let action = request.and_then(|request| request.get("action"));
    match_text(
        context.executor_kind.as_deref(),
        &[
            text(body.get("executor"))?,
            text(action.and_then(|action| action.get("executor")))?,
            binding
                .as_ref()
                .and_then(|binding| binding.binding.open_spec.get("executor"))
                .and_then(Value::as_str),
        ],
        "raw executor differs from prepare context",
    )?;
    if let Some(cwd) = &intent.canonical_cwd {
        require(
            !cwd.is_empty() && cwd.len() <= 4096,
            "invalid prepare working directory",
        )?;
    }
    match_text(
        intent.canonical_cwd.as_deref(),
        &[
            text(body.get("cwd"))?,
            text(action.and_then(|action| action.get("cwd")))?,
            binding
                .as_ref()
                .and_then(|binding| binding.binding.open_spec.get("cwd"))
                .and_then(Value::as_str),
        ],
        "raw working directory differs from prepare context",
    )?;
    Ok(())
}

/// The body and context come from user intent; the Gateway key comes from a pinned Noise route.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signClientPrepareIntent))]
pub fn sign_client_prepare_intent(
    auth: &VerifiedClientAuthorization,
    raw_body_json: &str,
    raw_context_json: &str,
    expected_action: &str,
    operation_id: &str,
    expected_gateway_public_key: &str,
    identity: &Identity,
    now_ms: f64,
) -> crate::Result<String> {
    (|| -> Result<String> {
        require(
            raw_body_json.len() <= MAX_PROOF_BYTES
                && raw_context_json.len() <= MAX_PROOF_BYTES
                && now_ms.is_finite()
                && now_ms >= 0.0
                && now_ms.fract() == 0.0
                && now_ms <= 9_007_199_254_740_991.0,
            "invalid client prepare input",
        )?;
        let body: Value = serde_json::from_str(raw_body_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid raw request JSON"))?;
        let raw: PrepareContext = serde_json::from_str(raw_context_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid raw context JSON"))?;
        let action: Action = serde_json::from_value(Value::String(expected_action.to_owned()))
            .map_err(|_| AuthorizationError::InvalidProof("unknown expected prepare action"))?;
        let gateway = PublicKey::parse(expected_gateway_public_key)?;
        require(
            gateway != PublicKey::of(identity),
            "gateway cannot be client key",
        )?;
        let resource = raw_resource(&body)?;
        require(
            auth.scope.device_id == DeviceId::from_key(&PublicKey::of(identity))
                && auth.scope.space_id == resource.space
                && scope_covers(&auth.grant_scope, &resource)
                && auth.actions.contains(&action)
                && valid_operation_id(operation_id)
                && text(body.get("authorization_id"))? == Some(operation_id)
                && context_tighter(&auth.constraints.context, &raw.context),
            "prepare intent differs from verified client grant",
        )?;
        if auth.constraints.require_webauthn_uv {
            return Err(AuthorizationError::UserPresenceRequired);
        }
        let actor = Actor::Account(auth.scope.account_id.clone());
        let now = now_ms as u64;
        let intent = PrepareIntent {
            request: Invocation {
                version: AUTH_VERSION,
                actor: actor.clone(),
                device: auth.scope.device_id.clone(),
                initiator: actor,
                initiating_grant: None,
                resource,
                action,
                audience: auth.audience.clone(),
                presenter: if gateway == auth.audience_public_key {
                    auth.scope.device_id.clone()
                } else {
                    DeviceId::from_key(&gateway)
                },
                operation_id: operation_id.to_owned(),
                body_hash: ObjectHash::body(&body)?,
                grant_leaf: auth.grant_leaf.clone(),
                expected_policies: auth.heads.clone(),
                context: raw.context,
                nonce: Nonce::random()?,
                not_before_ms: now,
                expires_at_ms: now + INVOCATION_MAX_AGE_MS,
            },
            binding_generation: raw.binding_generation,
            canonical_cwd: raw.canonical_cwd,
            execution_target_public_key: raw.execution_target_public_key,
        };
        check_raw_context(&intent, &body, &auth.audience_public_key)?;
        serde_json::to_string(&Signed::sign(intent, identity)?)
            .map_err(|_| AuthorizationError::InvalidProof("prepare intent encoding failed"))
    })()
    .map_err(|error| error.to_string())
}

impl VerifiedPolicySet {
    /// expected_action is parsed from the raw method; local_audience comes from the local service pin.
    pub fn verify_prepare_intent(
        &self,
        signed: &Signed<PrepareIntent>,
        raw_body: &Value,
        expected_action: Action,
        local_audience: &Audience,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<VerifiedPrepareAuthorization> {
        require(
            canonical(raw_body)?.len() <= MAX_PROOF_BYTES,
            "raw prepare body too large",
        )?;
        let mut budget = Budget::default();
        let hash = budget.verify(signed)?;
        let intent = &signed.payload;
        let i = &intent.request;
        require(
            i.resource == raw_resource(raw_body)?
                && i.action == expected_action
                && i.audience == *local_audience
                && text(raw_body.get("authorization_id"))? == Some(i.operation_id.as_str())
                && i.body_hash == ObjectHash::body(raw_body)?,
            "prepare does not match raw request",
        )?;
        let audience_key = &self.find_device(&i.audience.device)?.payload.public_key;
        check_raw_context(intent, raw_body, audience_key)?;
        if let Some(session) = &i.context.execution_session {
            let certificate = self.find_device(&session.target)?;
            require(
                certificate.payload.public_key == *execution_key(intent, audience_key)?,
                "execution key differs from policy certificate",
            )?;
        }
        let facts = RequestFacts {
            resource: i.resource.clone(),
            action: expected_action,
            audience: local_audience.clone(),
            operation_id: i.operation_id.clone(),
            body_hash: ObjectHash::body(raw_body)?,
            context: i.context.clone(),
        };
        // The verified prepare signature is never exposed as an executable Invocation signature.
        let request = Signed {
            payload: i.clone(),
            issuer: signed.issuer.clone(),
            signature: signed.signature.clone(),
        };
        self.verify_signed_invocation_body(
            &request,
            hash,
            budget,
            &facts,
            transport,
            now_ms,
            AuthorizationPhase::Admission,
        )?;
        Ok(VerifiedPrepareAuthorization {
            resource: i.resource.clone(),
            raw_body: raw_body.clone(),
            action: i.action,
            operation_id: i.operation_id.clone(),
            audience: i.audience.clone(),
            presenter: i.presenter.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorization::tests::{Fixture, NOW};
    use serde_json::json;

    fn client(f: &Fixture, leaf: Option<ObjectHash>) -> (VerifiedClientAuthorization, Identity) {
        let user = Identity::from_secret_hex(&hex::encode([4; 32])).unwrap();
        let space = SpaceId::personal(&f.account);
        let leaf = leaf.unwrap_or_else(|| f.state.grants.keys().next().unwrap().clone());
        let ah = f.state.account_history(&f.account).unwrap();
        let sh = f.state.space_history(&space).unwrap();
        let mut journal = vec![JournalEntry::AccountGenesis(ah[0].clone())];
        journal.extend(f.state.devices.values().cloned().map(JournalEntry::Device));
        journal.extend(ah[1..].iter().cloned().map(JournalEntry::Account));
        for (index, document) in sh.iter().enumerate() {
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
        let bundle = ClientAuthorizationBundle {
            journal,
            grant_leaf: leaf,
        };
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
                minimum_head: f.state.head(&PolicyDomain::Space(space.clone())).cloned(),
            },
        ];
        let auth = verify_client_authorization(
            &serde_json::to_string(&bundle).unwrap(),
            &serde_json::to_string(&pins).unwrap(),
            f.account.as_str(),
            space.as_str(),
            PublicKey::of(&user).as_str(),
            NOW as f64,
        )
        .unwrap();
        (auth, user)
    }
    fn body(auth: &VerifiedClientAuthorization) -> Value {
        let resource = &auth.resources[0].payload;
        json!({"space_id":resource.space,"resource_id":resource.resource,
            "authorization_id":"prepare_1","method":"message","conversation_id":"chat","text":"hello"})
    }
    fn context(auth: &VerifiedClientAuthorization) -> PrepareContext {
        PrepareContext {
            context: InvocationContext {
                conversation: Some("chat".into()),
                workspace_resource: Some(auth.resources[0].payload.resource.clone()),
                ..Default::default()
            },
            ..Default::default()
        }
    }
    fn sign(
        auth: &VerifiedClientAuthorization,
        body: &Value,
        context: &PrepareContext,
        user: &Identity,
        action: &str,
    ) -> crate::Result<Signed<PrepareIntent>> {
        let json = sign_client_prepare_intent(
            auth,
            &body.to_string(),
            &serde_json::to_string(context).unwrap(),
            action,
            "prepare_1",
            auth.audience_public_key.as_str(),
            user,
            NOW as f64,
        )?;
        Ok(serde_json::from_str(&json).unwrap())
    }
    fn check(
        f: &Fixture,
        signed: &Signed<PrepareIntent>,
        body: &Value,
        action: Action,
        now: u64,
    ) -> Result<VerifiedPrepareAuthorization> {
        f.state.verify_prepare_intent(
            signed,
            body,
            action,
            &signed.payload.request.audience,
            &TransportEvidence::DirectNoise {
                device: signed.payload.request.device.clone(),
            },
            now,
        )
    }

    #[test]
    fn prepares_raw_intent_without_producing_executable_authorization() {
        let f = Fixture::new();
        let (auth, user) = client(&f, None);
        let body = body(&auth);
        let signed = sign(&auth, &body, &context(&auth), &user, "message_append").unwrap();
        let verified = check(&f, &signed, &body, Action::MessageAppend, NOW).unwrap();
        assert_eq!(verified.resource(), &signed.payload.request.resource);
        assert_eq!(verified.raw_body(), &body);
        assert_eq!(verified.action(), Action::MessageAppend);
        assert_eq!(verified.operation_id(), "prepare_1");
        assert_eq!(verified.audience(), auth.audience());
        assert_eq!(verified.presenter(), &auth.scope.device_id);
        assert!(
            serde_json::from_str::<Signed<Invocation>>(&serde_json::to_string(&signed).unwrap())
                .is_err()
        );
        let request = signed.payload.request.clone();
        let facts = RequestFacts {
            resource: request.resource.clone(),
            action: request.action,
            audience: request.audience.clone(),
            operation_id: request.operation_id.clone(),
            body_hash: request.body_hash.clone(),
            context: request.context.clone(),
        };
        let counterfeit = Signed {
            payload: request,
            issuer: signed.issuer.clone(),
            signature: signed.signature.clone(),
        };
        assert_eq!(
            f.state
                .verify_invocation(
                    &counterfeit,
                    &facts,
                    &TransportEvidence::DirectNoise {
                        device: auth.scope.device_id.clone()
                    },
                    NOW
                )
                .unwrap_err(),
            AuthorizationError::InvalidProof("invalid object signature")
        );
        let again = sign(&auth, &body, &context(&auth), &user, "message_append").unwrap();
        assert_ne!(signed.payload.request.nonce, again.payload.request.nonce);
    }

    #[test]
    fn verifies_actor_grant_transport_heads_expiry_and_original_body() {
        let f = Fixture::new();
        let (auth, user) = client(&f, None);
        let body = body(&auth);
        let signed = sign(&auth, &body, &context(&auth), &user, "message_append").unwrap();
        let mut changed_body = body.clone();
        changed_body["text"] = json!("changed during normalization");
        assert!(check(&f, &signed, &changed_body, Action::MessageAppend, NOW).is_err());
        assert_eq!(
            check(
                &f,
                &signed,
                &body,
                Action::MessageAppend,
                NOW + INVOCATION_MAX_AGE_MS
            )
            .unwrap_err(),
            AuthorizationError::Expired
        );
        assert!(check(&f, &signed, &body, Action::TaskAccept, NOW).is_err());
        let wrong_issuer = Signed::sign(signed.payload.clone(), &f.node).unwrap();
        assert!(check(&f, &wrong_issuer, &body, Action::MessageAppend, NOW).is_err());
        let transport = TransportEvidence::DirectNoise {
            device: DeviceId::from_key(&PublicKey::of(&f.node)),
        };
        assert!(
            f.state
                .verify_prepare_intent(
                    &signed,
                    &body,
                    Action::MessageAppend,
                    auth.audience(),
                    &transport,
                    NOW
                )
                .is_err()
        );
        let mut stale = signed.payload.clone();
        stale.request.expected_policies[0].head =
            ObjectHash::parse(&format!("h2_{}", "f".repeat(64))).unwrap();
        assert!(matches!(
            check(
                &f,
                &Signed::sign(stale, &user).unwrap(),
                &body,
                Action::MessageAppend,
                NOW
            ),
            Err(AuthorizationError::NeedPolicySync(_))
        ));
        let mut not_granted = signed.payload.clone();
        not_granted.request.action = Action::TaskCreate;
        let mut create_body = body.clone();
        create_body["method"] = json!("create_task");
        not_granted.request.body_hash = ObjectHash::body(&create_body).unwrap();
        assert!(
            check(
                &f,
                &Signed::sign(not_granted, &user).unwrap(),
                &create_body,
                Action::TaskCreate,
                NOW
            )
            .is_err()
        );
        let mut other_resource = signed.payload.clone();
        other_resource.request.resource.resource = ResourceId::from_bytes([90; 32]);
        other_resource.request.context.workspace_resource =
            Some(other_resource.request.resource.resource.clone());
        changed_body = body.clone();
        changed_body["resource_id"] = json!(other_resource.request.resource.resource);
        other_resource.request.body_hash = ObjectHash::body(&changed_body).unwrap();
        assert!(
            check(
                &f,
                &Signed::sign(other_resource, &user).unwrap(),
                &changed_body,
                Action::MessageAppend,
                NOW
            )
            .is_err()
        );
    }

    #[test]
    fn raw_context_must_match_client_intent_before_any_normalization() {
        let f = Fixture::new();
        let (auth, user) = client(&f, None);
        let body = body(&auth);
        let signed = sign(&auth, &body, &context(&auth), &user, "message_append").unwrap();
        let mut wrong = signed.payload.clone();
        wrong.request.context.conversation = Some("another-chat".into());
        assert!(
            check(
                &f,
                &Signed::sign(wrong, &user).unwrap(),
                &body,
                Action::MessageAppend,
                NOW
            )
            .is_err()
        );
        let mut wrong = context(&auth);
        wrong.context.sdk_session = Some(SessionRef {
            target: auth.audience.device.clone(),
            session_id: "adapter".into(),
        });
        assert!(sign(&auth, &body, &wrong, &user, "message_append").is_err());
        let mut wrong = context(&auth);
        wrong.context.task = Some(EventRef::parse(&"a".repeat(64)).unwrap());
        assert!(sign(&auth, &body, &wrong, &user, "message_append").is_err());
        let mut cwd_body = body.clone();
        cwd_body["cwd"] = json!("/selected");
        let mut cwd = context(&auth);
        cwd.canonical_cwd = Some("/selected".into());
        let proof = sign(&auth, &cwd_body, &cwd, &user, "message_append").unwrap();
        assert!(check(&f, &proof, &cwd_body, Action::MessageAppend, NOW).is_ok());
        cwd.canonical_cwd = Some("/expanded".into());
        assert!(sign(&auth, &cwd_body, &cwd, &user, "message_append").is_err());

        let mut execution = body.clone();
        execution["method"] = json!("operate");
        execution["request"] = json!({"conversation_id":"chat","session_id":"session-1","target_node":f.node.node_id(),"action":{"type":"send"}});
        let mut selected = context(&auth);
        selected.context.execution_session = Some(SessionRef {
            target: auth.audience.device.clone(),
            session_id: "session-1".into(),
        });
        let proof = sign(&auth, &execution, &selected, &user, "execution_send").unwrap();
        assert!(check(&f, &proof, &execution, Action::ExecutionSend, NOW).is_ok());
        let binding = task_binding::TaskBinding {
            network_id: auth.scope.space_id.to_string(),
            conversation_id: "chat".into(),
            task_ref: "a".repeat(64),
            task_id: "task".into(),
            authority_node: f.node.node_id(),
            generation: 1,
            target_node: f.node.node_id(),
            session_id: "session-1".into(),
            open_spec: json!({"type":"open","executor":"codex","cwd":"/selected"}),
            previous_binding_hash: None,
            previous_seal: None,
        };
        let binding = task_binding::SignedBinding {
            signature: f
                .node
                .sign(
                    task_binding::BINDING_DOMAIN,
                    &serde_json::to_vec(&binding).unwrap(),
                )
                .unwrap(),
            binding,
        };
        execution["binding"] = serde_json::to_value(&binding).unwrap();
        execution["executor"] = json!("codex");
        selected.context.task = Some(EventRef::parse(&"a".repeat(64)).unwrap());
        selected.context.executor_kind = Some("codex".into());
        selected.binding_generation = Some(1);
        selected.canonical_cwd = Some("/selected".into());
        let proof = sign(&auth, &execution, &selected, &user, "execution_send").unwrap();
        assert!(check(&f, &proof, &execution, Action::ExecutionSend, NOW).is_ok());
        let mut conflict = binding;
        conflict.binding.task_id = "other-task".into();
        execution["request"]["binding"] = serde_json::to_value(conflict).unwrap();
        assert!(sign(&auth, &execution, &selected, &user, "execution_send").is_err());
        execution["request"]
            .as_object_mut()
            .unwrap()
            .remove("binding");
        selected.execution_target_public_key = Some(PublicKey::of(&user));
        assert!(sign(&auth, &execution, &selected, &user, "execution_send").is_err());
        selected.execution_target_public_key = None;
        selected
            .context
            .execution_session
            .as_mut()
            .unwrap()
            .session_id = "other-session".into();
        assert!(sign(&auth, &execution, &selected, &user, "execution_send").is_err());
    }

    #[test]
    fn task_limited_grant_requires_matching_raw_creation_reference() {
        let mut f = Fixture::new();
        let (original, _) = client(&f, None);
        let task = EventRef::parse(&"a".repeat(64)).unwrap();
        let mut grant = f.state.grants[&original.grant_leaf].payload.clone();
        grant.constraints.context.task = Some(task.clone());
        grant.issued_under = f
            .state
            .head(&PolicyDomain::Space(original.scope.space_id.clone()))
            .unwrap()
            .clone();
        grant.nonce = Nonce::from_bytes([91; 32]);
        let leaf = f
            .state
            .install_grant(Signed::sign(grant, &f.root).unwrap(), NOW)
            .unwrap();
        let mut space = f.state.space(&original.scope.space_id).unwrap().clone();
        space.previous = f
            .state
            .head(&PolicyDomain::Space(space.space.clone()))
            .cloned();
        space.sequence += 1;
        space.grants.push(leaf.clone());
        space.grants.sort();
        f.state
            .install_space(Signed::sign(space, &f.root).unwrap())
            .unwrap();
        let (auth, user) = client(&f, Some(leaf));
        let mut raw = body(&auth);
        let mut selected = context(&auth);
        selected.context.task = Some(task);
        assert!(sign(&auth, &raw, &selected, &user, "message_append").is_err());
        raw["task_ref"] = json!("a".repeat(64));
        let proof = sign(&auth, &raw, &selected, &user, "message_append").unwrap();
        assert!(check(&f, &proof, &raw, Action::MessageAppend, NOW).is_ok());
        raw["task_ref"] = json!("b".repeat(64));
        assert!(sign(&auth, &raw, &selected, &user, "message_append").is_err());
    }
}
