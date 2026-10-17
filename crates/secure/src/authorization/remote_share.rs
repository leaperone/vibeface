//! Task-scoped execution sharing, approved before the source creates its execution binding.
use super::{events::*, governance::*, *};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionShareRequest {
    pub version: u16,
    pub project: ResourceRef,
    pub task_creation: SignedEvent,
    pub source_node_public_key: PublicKey,
    pub context_hash: ObjectHash,
    pub policy_heads: Vec<PolicyStamp>,
    pub session_id: String,
    pub executor_kind: String,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for ExecutionShareRequest {}
impl SignedObject for ExecutionShareRequest {
    const DOMAIN: &'static str = "execution-share-request-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionShareRequestEnvelope {
    pub request: Signed<ExecutionShareRequest>,
    pub authorization: ClientAuthorizationBundle,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RequestSummary {
    account_id: AccountId,
    space_id: SpaceId,
    project: ResourceRef,
    task_ref: EventRef,
    task_id: String,
    conversation_id: String,
    session_id: String,
    executor_kind: String,
    source_node_public_key: PublicKey,
    request_hash: ObjectHash,
    issued_at_ms: u64,
    expires_at_ms: u64,
    task_title: String,
}

fn parse<T: for<'de> Deserialize<'de>>(json: &str, limit: usize) -> Result<T> {
    require(json.len() <= limit, "execution sharing proof is too large")?;
    serde_json::from_str(json)
        .map_err(|_| AuthorizationError::InvalidProof("invalid execution sharing JSON"))
}
fn encode<T: Serialize>(value: &T) -> crate::Result<String> {
    serde_json::to_string(value).map_err(|_| "execution sharing encoding failed".to_owned())
}
fn time(value: f64) -> Result<u64> {
    bootstrap::timestamp(value)
}
fn text(value: &str, limit: usize) -> bool {
    !value.trim().is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}

fn source_task(
    state: &VerifiedPolicySet,
    project: &ResourceRef,
    task_creation: &SignedEvent,
    source_key: &PublicKey,
    now: u64,
    budget: &mut Budget,
) -> Result<(AccountId, Event, Vec<PolicyStamp>)> {
    let record = state.resource(&project.space, &project.resource).ok_or(
        AuthorizationError::InvalidProof("source Project is missing"),
    )?;
    let certificate =
        state.verify_service_device(&project.space, &DeviceId::from_key(source_key), now)?;
    require(
        record.payload.kind == ResourceKind::Project
            && record.payload.host == ServiceId::from_key(source_key)
            && certificate.payload.public_key == *source_key
            && certificate.payload.subject == Actor::Service(record.payload.host.clone()),
        "execution share request must come from its real Project host",
    )?;
    let PolicyDomain::Account(account) = &certificate.payload.owner else {
        return Err(AuthorizationError::InvalidProof(
            "source service has no personal owner",
        ));
    };
    let verified = events::verify_signed_event(
        &encode(task_creation).map_err(|_| AuthorizationError::PolicyConflict)?,
        project.space.as_str(),
        project.resource.as_str(),
        source_key.as_str(),
    )
    .map_err(|_| AuthorizationError::InvalidProof("invalid source TaskCreated proof"))?;
    require(
        matches!(verified.event().body, EventBody::TaskCreated { .. })
            && verified.event().created_at_ms >= 0
            && verified.event().created_at_ms as u64 <= now
            && verified.event().blobs.is_empty(),
        "execution share request requires a real TaskCreated event",
    )?;
    budget.verify(record)?;
    budget.external_signature(ObjectHash::body(task_creation)?)?;
    let mut dependencies = BTreeMap::new();
    state.validate_certificate(
        certificate,
        now,
        budget,
        &mut dependencies,
        &mut BTreeSet::new(),
    )?;
    state.dependency(
        &PolicyDomain::Space(project.space.clone()),
        &mut dependencies,
    )?;
    state.expand_authorities(now, budget, &mut dependencies)?;
    Ok((
        account.clone(),
        verified.event().clone(),
        dependencies.into_values().collect(),
    ))
}

fn verify_request(
    state: &VerifiedPolicySet,
    envelope: &ExecutionShareRequestEnvelope,
    now: u64,
) -> Result<RequestSummary> {
    require(
        canonical(envelope)?.len() <= MAX_PROOF_BYTES,
        "execution share request requires a bounded checkpoint",
    )?;
    let signed = &envelope.request;
    let request = &signed.payload;
    validate_window(request.issued_at_ms, request.expires_at_ms, now)?;
    require(
        request.version == AUTH_VERSION
            && signed.issuer == request.source_node_public_key
            && request.expires_at_ms - request.issued_at_ms <= INVOCATION_MAX_AGE_MS
            && text(&request.session_id, 512)
            && text(&request.executor_kind, 128)
            && request.context_hash == ObjectHash::body(&envelope.authorization)?,
        "execution share request changed its exact source context",
    )?;
    let mut budget = Budget::default();
    budget.verify(signed)?;
    let (account, event, heads) = source_task(
        state,
        &request.project,
        &request.task_creation,
        &signed.issuer,
        now,
        &mut budget,
    )?;
    require(
        request.policy_heads == heads,
        "execution share request has stale or substituted source policy heads",
    )?;
    let EventBody::TaskCreated { task_id, title, .. } = &event.body else {
        return Err(AuthorizationError::InvalidProof(
            "source event is not a task",
        ));
    };
    Ok(RequestSummary {
        account_id: account,
        space_id: request.project.space.clone(),
        project: request.project.clone(),
        task_ref: EventRef::parse(&request.task_creation.hash)?,
        task_id: task_id.clone(),
        conversation_id: event.scope_id,
        session_id: request.session_id.clone(),
        executor_kind: request.executor_kind.clone(),
        source_node_public_key: signed.issuer.clone(),
        request_hash: ObjectHash::body(signed)?,
        issued_at_ms: request.issued_at_ms,
        expires_at_ms: request.expires_at_ms,
        task_title: title.clone(),
    })
}

/// The caller obtains this task from the source Store after its current Account authorization.
pub fn build_execution_share_request(
    state: &VerifiedPolicySet,
    pins: &[PolicyPin],
    bundle: &ClientAuthorizationBundle,
    project: &ResourceRef,
    task_creation: &SignedEvent,
    session_id: &str,
    executor_kind: &str,
    source: &Identity,
    now: u64,
    expires: u64,
) -> Result<ExecutionShareRequestEnvelope> {
    let public_state = replay_policies(pins, &bundle.journal)?;
    let source_key = PublicKey::of(source);
    let (_, _, heads) = source_task(
        state,
        project,
        task_creation,
        &source_key,
        now,
        &mut Budget::default(),
    )?;
    require(
        heads
            .iter()
            .all(|head| public_state.head(&head.domain) == Some(head)),
        "source sharing context is not its current Store state",
    )?;
    let expires = expires.min(now.saturating_add(INVOCATION_MAX_AGE_MS));
    let envelope = ExecutionShareRequestEnvelope {
        authorization: bundle.clone(),
        request: Signed::sign(
            ExecutionShareRequest {
                version: AUTH_VERSION,
                project: project.clone(),
                task_creation: task_creation.clone(),
                source_node_public_key: source_key,
                context_hash: ObjectHash::body(bundle)?,
                policy_heads: heads,
                session_id: session_id.to_owned(),
                executor_kind: executor_kind.to_owned(),
                nonce: Nonce::random()?,
                issued_at_ms: now,
                expires_at_ms: expires,
            },
            source,
        )?,
    };
    verify_request(&public_state, &envelope, now)?;
    Ok(envelope)
}

fn inspect(
    request_json: &str,
    pins_json: &str,
    bundle_json: &str,
    now: u64,
) -> Result<RequestSummary> {
    Ok(inspect_with_pins(request_json, pins_json, bundle_json, now)?.0)
}
fn inspect_with_pins(
    request_json: &str,
    pins_json: &str,
    bundle_json: &str,
    now: u64,
) -> Result<(RequestSummary, Vec<PolicyPin>)> {
    let envelope: ExecutionShareRequestEnvelope = parse(request_json, MAX_PROOF_BYTES)?;
    let mut pins: Vec<PolicyPin> = parse(pins_json, MAX_PROOF_BYTES)?;
    let bundle: ClientAuthorizationBundle = parse(bundle_json, 1_048_576)?;
    require(
        envelope.authorization == bundle,
        "source authorization differs from the original signed request",
    )?;
    let state = replay_policies(&pins, &bundle.journal)?;
    let summary = verify_request(&state, &envelope, now)?;
    for pin in &mut pins {
        if let Some(head) = envelope
            .request
            .payload
            .policy_heads
            .iter()
            .find(|head| head.domain == pin.domain)
        {
            pin.minimum_head = Some(head.clone());
        }
    }
    Ok((summary, pins))
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = inspectRemoteExecutionRequest))]
pub fn inspect_remote_execution_request(
    request_json: &str,
    pins_json: &str,
    bundle_json: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let (summary, pins) = inspect_with_pins(
        request_json,
        pins_json,
        bundle_json,
        time(now_ms).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let mut value = serde_json::to_value(summary).map_err(|_| "source summary encoding failed")?;
    value["trust"] = "verified".into();
    value["pins"] =
        serde_json::to_value(pins).map_err(|_| "verified source pins encoding failed")?;
    encode(&value)
}

/// Mathematical validity of self-claimed roots is only a preview, never saved trust.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = previewRemoteExecutionRequest))]
pub fn preview_remote_execution_request(request_json: &str, now_ms: f64) -> crate::Result<String> {
    let preview = || -> Result<_> {
        let envelope: ExecutionShareRequestEnvelope = parse(request_json, MAX_PROOF_BYTES)?;
        let mut roots = BTreeMap::new();
        for entry in &envelope.authorization.journal {
            match entry {
                JournalEntry::AccountGenesis(value) => {
                    roots.insert(
                        PolicyDomain::Account(value.payload.account.clone()),
                        value.payload.genesis_root.clone(),
                    );
                }
                JournalEntry::SpaceGenesis(value) => {
                    let root = match &value.payload.root {
                        SpaceRoot::Personal { genesis_root, .. }
                        | SpaceRoot::Team { genesis_root } => genesis_root.clone(),
                    };
                    roots.insert(PolicyDomain::Space(value.payload.space.clone()), root);
                }
                _ => {}
            }
        }
        let pins: Vec<_> = roots
            .into_iter()
            .map(|(domain, genesis_root)| PolicyPin {
                domain,
                genesis_root,
                minimum_head: None,
            })
            .collect();
        let state = replay_policies(&pins, &envelope.authorization.journal)?;
        let summary = verify_request(&state, &envelope, time(now_ms)?)?;
        let fingerprint = hex::encode(Sha256::digest(
            crate::decode32(summary.source_node_public_key.as_str())
                .map_err(|_| AuthorizationError::InvalidProof("invalid source key"))?,
        ));
        let mut value =
            serde_json::to_value(summary).map_err(|_| AuthorizationError::PolicyConflict)?;
        value["trust"] = "unverified".into();
        value["sourceNodeFingerprint"] = fingerprint.into();
        value["claimedRoots"] = serde_json::to_value(
            pins.iter()
                .map(|pin| serde_json::json!({"domain":pin.domain,"genesis_root":pin.genesis_root}))
                .collect::<Vec<_>>(),
        )
        .map_err(|_| AuthorizationError::PolicyConflict)?;
        Ok(value)
    };
    encode(&preview().map_err(|e| e.to_string())?)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteExecutionShareIntent {
    pub request_json: String,
    pub execution_resource_id: ResourceId,
    pub canonical_cwd: Option<String>,
    pub actions: Vec<Action>,
    pub expires_at: u64,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedRemoteExecutionShare {
    prepared: PreparedSpaceGovernanceChange,
    grant_hash: ObjectHash,
    resource: ResourceRef,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareRemoteExecutionShare))]
pub fn prepare_remote_execution_share(
    intent_json: &str,
    signer: &Identity,
    pins_json: &str,
    bundle_json: &str,
    source_pins_json: &str,
    source_bundle_json: &str,
    now_ms: f64,
) -> crate::Result<PreparedRemoteExecutionShare> {
    let build = || -> Result<_> {
        let intent: RemoteExecutionShareIntent = parse(intent_json, MAX_PROOF_BYTES)?;
        let now = time(now_ms)?;
        let source = inspect(
            &intent.request_json,
            source_pins_json,
            source_bundle_json,
            now,
        )?;
        require(
            intent
                .canonical_cwd
                .as_ref()
                .is_none_or(|cwd| text(cwd, 4096)),
            "invalid execution directory hint",
        )?;
        let pins: Vec<PolicyPin> = parse(pins_json, MAX_PROOF_BYTES)?;
        let bundle: ClientAuthorizationBundle = parse(bundle_json, 1_048_576)?;
        let state = replay_policies(&pins, &bundle.journal)?;
        let parent = state
            .grant(&bundle.grant_leaf)
            .ok_or(AuthorizationError::InvalidProof(
                "missing execution management parent",
            ))?;
        let resource = ResourceRef {
            space: parent.payload.owner_space.clone(),
            resource: intent.execution_resource_id,
        };
        let record = state.resource(&resource.space, &resource.resource).ok_or(
            AuthorizationError::InvalidProof("execution service is not registered"),
        )?;
        require(
            record.payload.kind == ResourceKind::ExecutionService,
            "Project resources cannot be shared as execution services",
        )?;
        let certificate = state
            .devices
            .values()
            .find(|certificate| {
                certificate.payload.subject == Actor::Service(record.payload.host.clone())
                    && state
                        .verify_service_device(&resource.space, &certificate.payload.device, now)
                        .is_ok()
            })
            .ok_or(AuthorizationError::InvalidProof(
                "execution service has no current personal certificate",
            ))?;
        let target = certificate.payload.public_key.clone();
        require(
            target != source.source_node_public_key,
            "remote execution requires another Node",
        )?;
        let mut actions = intent.actions;
        actions.sort();
        require(
            !actions.is_empty()
                && actions.windows(2).all(|pair| pair[0] != pair[1])
                && actions.iter().all(|action| {
                    matches!(
                        action,
                        Action::ExecutionOpen
                            | Action::ExecutionSend
                            | Action::ExecutionSteer
                            | Action::ExecutionInterrupt
                            | Action::ExecutionRespondApproval
                            | Action::ExecutionSubmit
                            | Action::ExecutionResolve
                            | Action::OperationStatusRead
                            | Action::OperationResolve
                    )
                })
                && (actions == [Action::ExecutionResolve, Action::OperationResolve]
                    || (!actions.contains(&Action::ExecutionResolve)
                        && !actions.contains(&Action::OperationResolve)
                        && actions
                            .iter()
                            .any(|action| *action != Action::ExecutionSubmit))),
            "execution sharing needs execution actions or the exact resolution pair",
        )?;
        if !actions.contains(&Action::ExecutionResolve)
            && !actions.contains(&Action::ExecutionSubmit)
        {
            actions.push(Action::ExecutionSubmit);
            actions.sort();
        }
        require(
            intent.expires_at > now && intent.expires_at <= 9_007_199_254_740_991,
            "execution sharing needs a finite future deadline",
        )?;
        let directional = DirectionalGrantIntent {
            space: resource.space.clone(),
            grantee: Grantee::Actor(Actor::Service(ServiceId::from_key(
                &source.source_node_public_key,
            ))),
            scope: ResourceScope::Exact(resource.clone()),
            actions,
            audience_node_public_key: target.clone(),
            constraints: GrantConstraints {
                context: InvocationContext {
                    conversation: Some(source.conversation_id),
                    task: Some(source.task_ref),
                    sdk_session: None,
                    execution_session: Some(SessionRef {
                        target: DeviceId::from_key(&target),
                        session_id: source.session_id,
                    }),
                    executor_kind: Some(source.executor_kind),
                    workspace_resource: Some(resource.resource.clone()),
                },
                ..Default::default()
            },
            remaining_delegations: 0,
            expires_at_ms: intent.expires_at,
        };
        let prepared = prepare_directional_grant(
            &encode(&directional).map_err(|_| AuthorizationError::PolicyConflict)?,
            signer,
            pins_json,
            bundle_json,
            now_ms,
        )
        .map_err(|_| {
            AuthorizationError::InvalidProof(
                "execution sharing exceeds current management authority",
            )
        })?;
        let request: SpaceGovernanceRequest = parse(
            &prepared
                .request_json()
                .map_err(|_| AuthorizationError::PolicyConflict)?,
            MAX_PROOF_BYTES,
        )?;
        let [JournalEntry::Grant(grant)] = request.new_objects.as_slice() else {
            return Err(AuthorizationError::InvalidProof(
                "remote sharing did not prepare one exact Grant",
            ));
        };
        Ok(PreparedRemoteExecutionShare {
            grant_hash: grant.hash()?,
            resource,
            prepared,
        })
    };
    build().map_err(|e| e.to_string())
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedRemoteExecutionShare {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = requestJson))]
    pub fn request_json(&self) -> crate::Result<String> {
        self.prepared.request_json()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = grantHash))]
    pub fn grant_hash(&self) -> String {
        self.grant_hash.to_string()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = resourceId))]
    pub fn resource_id(&self) -> String {
        self.resource.resource.to_string()
    }
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedRemoteExecutionShare {
    change: VerifiedSpaceGovernanceChange,
    share: Option<String>,
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishRemoteExecutionShare))]
pub fn finish_remote_execution_share(
    prepared: &PreparedRemoteExecutionShare,
    committed_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedRemoteExecutionShare> {
    let change = finish_space_governance_change(&prepared.prepared, committed_json, now_ms)?;
    let mut child: ClientAuthorizationBundle = serde_json::from_str(&change.bundle_json()?)
        .map_err(|_| "invalid completed sharing bundle")?;
    child.grant_leaf = prepared.grant_hash.clone();
    let committed: Committed<SpaceDocument> =
        serde_json::from_str(committed_json).map_err(|_| "invalid sharing receipt")?;
    prepare::verify_directional_grant_snapshot(
        &encode(&child)?,
        &change.pins_json()?,
        committed.commit.payload.accepted_at_ms as f64,
    )?;
    let share =
        prepare::verify_directional_grant_snapshot(&encode(&child)?, &change.pins_json()?, now_ms)
            .ok()
            .map(|snapshot| snapshot.share_json());
    Ok(VerifiedRemoteExecutionShare { change, share })
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedRemoteExecutionShare {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = bundleJson))]
    pub fn bundle_json(&self) -> crate::Result<String> {
        self.change.bundle_json()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        self.change.pins_json()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = scopeJson))]
    pub fn scope_json(&self) -> crate::Result<String> {
        self.change.scope_json()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = shareJson))]
    pub fn share_json(&self) -> crate::Result<String> {
        self.share
            .clone()
            .ok_or_else(|| "committed execution sharing grant is no longer current".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorization::bootstrap::*;
    use ed25519_dalek::Signer;
    const NOW: u64 = 20_000;
    const END: u64 = 500_000;
    fn key(n: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([n; 32])).unwrap()
    }
    fn personal(n: u8) -> (Vec<PolicyPin>, ClientAuthorizationBundle, ResourceRef) {
        let nonce = Nonce::from_bytes([n; 32]).to_string();
        let claim = verify_node_claim(
            &create_node_claim(&key(n + 3), &nonce, 10_000.0, 100_000.0).unwrap(),
            &key(n + 3).public_key_hex(),
            &nonce,
            10_000.0,
        )
        .unwrap();
        let prepared = prepare_account_creation(
            &key(n),
            &key(n + 1),
            &key(n + 2),
            &claim,
            1_000_000.0,
            10_000.0,
        )
        .unwrap();
        let proof = sign_node_possession(
            &prepared.node_request_json().unwrap(),
            &key(n + 3),
            &nonce,
            10_000.0,
        )
        .unwrap();
        let created = finish_account_creation(&prepared, &key(n), &proof, 10_000.0).unwrap();
        let bundle: ClientAuthorizationBundle =
            serde_json::from_str(&created.bundle_json().unwrap()).unwrap();
        let project = bundle
            .journal
            .iter()
            .find_map(|entry| match entry {
                JournalEntry::Resource(record) => Some(ResourceRef {
                    space: record.payload.space.clone(),
                    resource: record.payload.resource.clone(),
                }),
                _ => None,
            })
            .unwrap();
        (
            serde_json::from_str(&created.pins_json().unwrap()).unwrap(),
            bundle,
            project,
        )
    }
    fn task(
        pins: &[PolicyPin],
        bundle: &ClientAuthorizationBundle,
        project: &ResourceRef,
    ) -> SignedEvent {
        let state = replay_policies(pins, &bundle.journal).unwrap();
        let body = serde_json::json!({"method":"create_task","space_id":project.space,"resource_id":project.resource,
            "authorization_id":"share_task","conversation_id":"chat","task_id":"task-1","title":"real selected task","goal":"goal","acceptance":"done"});
        let context = InvocationContext {
            conversation: Some("chat".to_owned()),
            workspace_resource: Some(project.resource.clone()),
            ..Default::default()
        };
        let scope = AccountId::from_root(&PublicKey::of(&key(1)));
        let audience = Audience {
            service: ServiceId::from_key(&PublicKey::of(&key(4))),
            device: DeviceId::from_key(&PublicKey::of(&key(4))),
        };
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Account(scope.clone()),
                device: DeviceId::from_key(&PublicKey::of(&key(3))),
                initiator: Actor::Account(scope),
                initiating_grant: None,
                resource: project.clone(),
                action: Action::TaskCreate,
                audience: audience.clone(),
                presenter: DeviceId::from_key(&PublicKey::of(&key(3))),
                operation_id: "share_task".to_owned(),
                body_hash: ObjectHash::body(&body).unwrap(),
                grant_leaf: bundle.grant_leaf.clone(),
                expected_policies: pins
                    .iter()
                    .map(|pin| state.head(&pin.domain).unwrap().clone())
                    .collect(),
                context: context.clone(),
                nonce: Nonce::from_bytes([44; 32]),
                not_before_ms: NOW,
                expires_at_ms: NOW + 1000,
            },
            &key(3),
        )
        .unwrap();
        let transport = TransportEvidence::DirectNoise {
            device: invocation.payload.device.clone(),
        };
        let auth = state
            .verify_invocation(
                &invocation,
                &RequestFacts {
                    resource: project.clone(),
                    action: Action::TaskCreate,
                    audience,
                    operation_id: "share_task".to_owned(),
                    body_hash: invocation.payload.body_hash.clone(),
                    context,
                },
                &transport,
                NOW,
            )
            .unwrap();
        let challenge = FreshnessChallenge::new(
            &auth,
            CheckStage::Admit,
            Nonce::from_bytes([45; 32]),
            NOW,
            NOW + 1000,
        )
        .unwrap();
        let receipts: Vec<_> = auth
            .dependencies()
            .iter()
            .map(|head| {
                Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: head.clone(),
                        challenge_hash: challenge.hash().unwrap(),
                        issued_at_ms: NOW,
                        expires_at_ms: NOW + 1000,
                    },
                    &key(4),
                )
                .unwrap()
            })
            .collect();
        let freshness = state
            .verify_freshness(&auth, &challenge, &receipts, NOW)
            .unwrap();
        state.admit_operation(&auth, &freshness, NOW).unwrap();
        let event = Event {
            version: EVENT_VERSION,
            network_id: project.space.to_string(),
            resource_id: project.resource.clone(),
            cause: EventCause::Invocation {
                admission: AdmissionProof {
                    invocation,
                    source_body: body,
                    challenge,
                    receipts,
                    admitted_at_ms: NOW,
                    transport: StoredTransport::from(&transport),
                },
            },
            scope_id: "chat".to_owned(),
            author: key(4).node_id(),
            sequence: 1,
            previous: None,
            parents: vec![],
            created_at_ms: NOW as i64,
            body: EventBody::TaskCreated {
                task_id: "task-1".to_owned(),
                title: "real selected task".to_owned(),
                goal: "goal".to_owned(),
                acceptance: "done".to_owned(),
            },
            blobs: vec![],
        };
        let canonical_body = serde_json::to_string(&event).unwrap();
        let hash = hex::encode(Sha256::digest(canonical_body.as_bytes()));
        let mut bytes = b"vibeface:event:v2:".to_vec();
        bytes.extend_from_slice(hash.as_bytes());
        SignedEvent {
            hash,
            canonical_body,
            signature: hex::encode(key(4).key.sign(&bytes).to_bytes()),
        }
    }
    fn execution() -> (Vec<PolicyPin>, ClientAuthorizationBundle, ResourceRef) {
        let (mut pins, mut bundle, project) = personal(11);
        let mut state = replay_policies(&pins, &bundle.journal).unwrap();
        let stamp = state
            .head(&PolicyDomain::Space(project.space.clone()))
            .unwrap()
            .clone();
        let resource = ResourceRef {
            space: project.space.clone(),
            resource: ResourceId::from_bytes([50; 32]),
        };
        let record = Signed::sign(
            ResourceRecord {
                version: AUTH_VERSION,
                space: resource.space.clone(),
                resource: resource.resource.clone(),
                kind: ResourceKind::ExecutionService,
                host: ServiceId::from_key(&PublicKey::of(&key(14))),
                issued_under: stamp.clone(),
                nonce: Nonce::from_bytes([51; 32]),
            },
            &key(11),
        )
        .unwrap();
        let record_hash = state.install_resource(record.clone()).unwrap();
        let mut actions = vec![
            Action::ExecutionOpen,
            Action::ExecutionSend,
            Action::ExecutionSubmit,
            Action::ExecutionResolve,
            Action::OperationResolve,
            Action::GrantIssue,
            Action::GrantRevoke,
        ];
        actions.sort();
        let parent = Signed::sign(
            Grant {
                version: AUTH_VERSION,
                owner_space: resource.space.clone(),
                scope: ResourceScope::Exact(resource.clone()),
                grantee: Grantee::Actor(Actor::Account(AccountId::from_root(&PublicKey::of(
                    &key(11),
                )))),
                actions,
                audience: Audience {
                    service: ServiceId::from_key(&PublicKey::of(&key(14))),
                    device: DeviceId::from_key(&PublicKey::of(&key(14))),
                },
                constraints: GrantConstraints::default(),
                parent: None,
                remaining_delegations: 1,
                issued_under: stamp.clone(),
                not_before_ms: NOW,
                expires_at_ms: END,
                nonce: Nonce::from_bytes([52; 32]),
            },
            &key(11),
        )
        .unwrap();
        let parent_hash = state.install_grant(parent.clone(), NOW).unwrap();
        let mut document = state.space(&resource.space).unwrap().clone();
        document.previous = Some(stamp);
        document.sequence += 1;
        document.resources.push(record_hash);
        document.resources.sort();
        document.grants.push(parent_hash.clone());
        document.grants.sort();
        let document = Signed::sign(document, &key(11)).unwrap();
        let head = state.install_space(document.clone()).unwrap();
        for pin in &mut pins {
            if pin.domain == head.domain {
                pin.minimum_head = Some(head.clone());
            }
        }
        bundle.journal.extend([
            JournalEntry::Resource(record),
            JournalEntry::Grant(parent),
            JournalEntry::Space(document),
        ]);
        bundle.grant_leaf = parent_hash;
        (pins, bundle, resource)
    }
    fn request() -> (
        Vec<PolicyPin>,
        ClientAuthorizationBundle,
        ExecutionShareRequestEnvelope,
    ) {
        let (pins, bundle, project) = personal(1);
        let event = task(&pins, &bundle, &project);
        let envelope = build_execution_share_request(
            &replay_policies(&pins, &bundle.journal).unwrap(),
            &pins,
            &bundle,
            &project,
            &event,
            "selected-session",
            "codex",
            &key(4),
            NOW,
            NOW + 100_000,
        )
        .unwrap();
        (pins, bundle, envelope)
    }
    fn intent(
        envelope: &ExecutionShareRequestEnvelope,
        resource: &ResourceRef,
    ) -> serde_json::Value {
        serde_json::json!({"requestJson":encode(envelope).unwrap(),"executionResourceId":resource.resource,"canonicalCwd":"/configured/workspace","actions":["execution_send"],"expiresAt":END})
    }
    fn commit_share(
        prepared: &PreparedRemoteExecutionShare,
        pins: &[PolicyPin],
        bundle: &ClientAuthorizationBundle,
    ) -> String {
        let challenge_json = issue_space_governance_challenge(
            &prepared.request_json().unwrap(),
            &key(14),
            &encode(&pins).unwrap(),
            &encode(&bundle).unwrap(),
            NOW as f64,
        )
        .unwrap();
        let challenge: Signed<PolicyCommit> = serde_json::from_str(&challenge_json).unwrap();
        let receipts: Vec<_> = challenge
            .payload
            .dependencies
            .iter()
            .map(|head| {
                Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: head.clone(),
                        challenge_hash: challenge.challenge_hash().unwrap(),
                        issued_at_ms: NOW,
                        expires_at_ms: challenge.payload.expires_at_ms,
                    },
                    &key(14),
                )
                .unwrap()
            })
            .collect();
        accept_space_governance_change(
            &prepared.request_json().unwrap(),
            &challenge_json,
            &encode(&receipts).unwrap(),
            &key(14),
            &encode(&pins).unwrap(),
            &encode(&bundle).unwrap(),
            NOW as f64,
        )
        .unwrap()
    }
    #[test]
    fn signed_task_request_produces_a_current_exact_non_delegable_share_and_keeps_management_leaf()
    {
        let (source_pins, source_bundle, envelope) = request();
        let (pins, bundle, resource) = execution();
        let input = intent(&envelope, &resource);
        let preview: serde_json::Value = serde_json::from_str(
            &preview_remote_execution_request(&encode(&envelope).unwrap(), NOW as f64).unwrap(),
        )
        .unwrap();
        assert_eq!(preview["trust"], "unverified");
        assert!(preview["claimedRoots"].as_array().unwrap().len() >= 2);
        let summary: serde_json::Value = serde_json::from_str(
            &inspect_remote_execution_request(
                &encode(&envelope).unwrap(),
                &encode(&source_pins).unwrap(),
                &encode(&source_bundle).unwrap(),
                NOW as f64,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(summary["trust"], "verified");
        assert_eq!(
            summary["taskRef"],
            envelope.request.payload.task_creation.hash
        );
        let prepared = prepare_remote_execution_share(
            &input.to_string(),
            &key(13),
            &encode(&pins).unwrap(),
            &encode(&bundle).unwrap(),
            &encode(&source_pins).unwrap(),
            &encode(&source_bundle).unwrap(),
            NOW as f64,
        )
        .unwrap();
        let request: SpaceGovernanceRequest =
            serde_json::from_str(&prepared.request_json().unwrap()).unwrap();
        let [JournalEntry::Grant(grant)] = request.new_objects.as_slice() else {
            panic!("one grant");
        };
        assert_eq!(
            grant.payload.actions,
            vec![Action::ExecutionSend, Action::ExecutionSubmit]
        );
        assert_eq!(grant.payload.remaining_delegations, 0);
        assert_eq!(
            grant.payload.grantee,
            Grantee::Actor(Actor::Service(ServiceId::from_key(&PublicKey::of(&key(4)))))
        );
        assert_eq!(grant.payload.scope, ResourceScope::Exact(resource.clone()));
        assert_eq!(
            grant
                .payload
                .constraints
                .context
                .task
                .as_ref()
                .unwrap()
                .as_str(),
            envelope.request.payload.task_creation.hash
        );
        assert_eq!(
            grant.payload.constraints.context.workspace_resource,
            Some(resource.resource.clone())
        );
        assert!(grant.payload.constraints.context.sdk_session.is_none());
        assert!(
            !prepared
                .request_json()
                .unwrap()
                .contains("/configured/workspace")
        );
        let committed = commit_share(&prepared, &pins, &bundle);
        let finished = finish_remote_execution_share(&prepared, &committed, NOW as f64).unwrap();
        let management: ClientAuthorizationBundle =
            serde_json::from_str(&finished.bundle_json().unwrap()).unwrap();
        assert_eq!(management.grant_leaf, bundle.grant_leaf);
        let share: serde_json::Value =
            serde_json::from_str(&finished.share_json().unwrap()).unwrap();
        assert_eq!(share["authorization"]["grant_leaf"], prepared.grant_hash());
        assert_eq!(
            share["selection"]["execution_resource"],
            serde_json::to_value(&resource).unwrap()
        );
        assert!(share.get("pins").is_none());
        let late = finish_remote_execution_share(
            &prepared,
            &committed,
            (NOW + INVOCATION_MAX_AGE_MS + 1) as f64,
        )
        .unwrap();
        assert_eq!(late.pins_json().unwrap(), finished.pins_json().unwrap());
        let expired =
            finish_remote_execution_share(&prepared, &committed, (END + 1) as f64).unwrap();
        assert_eq!(expired.pins_json().unwrap(), finished.pins_json().unwrap());
        assert!(expired.share_json().is_err());
        assert_eq!(expired.scope_json().unwrap(), "null");
        if let Ok(path) = std::env::var("VIBEFACE_REMOTE_SHARE_VECTOR") {
            std::fs::write(path,encode(&serde_json::json!({"source_pins":source_pins,"source_bundle":source_bundle,"request":envelope,
            "pins":pins,"bundle":bundle,"resource":resource,"intent":input,"prepared":request,"committed":serde_json::from_str::<serde_json::Value>(&committed).unwrap(),"share":share,"now":NOW,"end":END})).unwrap()).unwrap();
        }
    }
    #[test]
    fn exact_resolution_pair_commits_and_finishes_without_execution_submission() {
        let (source_pins, source_bundle, envelope) = request();
        let (pins, bundle, resource) = execution();
        let mut input = intent(&envelope, &resource);
        input["actions"] = serde_json::json!(["operation_resolve", "execution_resolve"]);
        let prepared = prepare_remote_execution_share(
            &input.to_string(),
            &key(13),
            &encode(&pins).unwrap(),
            &encode(&bundle).unwrap(),
            &encode(&source_pins).unwrap(),
            &encode(&source_bundle).unwrap(),
            NOW as f64,
        )
        .unwrap();
        let request: SpaceGovernanceRequest =
            serde_json::from_str(&prepared.request_json().unwrap()).unwrap();
        let [JournalEntry::Grant(grant)] = request.new_objects.as_slice() else {
            panic!("one exact resolver grant");
        };
        assert_eq!(
            grant.payload.actions,
            [Action::ExecutionResolve, Action::OperationResolve]
        );
        assert_eq!(grant.payload.remaining_delegations, 0);
        assert_eq!(grant.payload.scope, ResourceScope::Exact(resource));
        assert_eq!(grant.payload.expires_at_ms, END);
        let committed = commit_share(&prepared, &pins, &bundle);
        let finished = finish_remote_execution_share(&prepared, &committed, NOW as f64).unwrap();
        let share: serde_json::Value =
            serde_json::from_str(&finished.share_json().unwrap()).unwrap();
        assert_eq!(share["selection"]["execution_grant"], prepared.grant_hash());
        assert_eq!(share["authorization"]["grant_leaf"], prepared.grant_hash());
    }
    #[test]
    fn request_and_producer_reject_untrusted_sources_forgery_scope_substitution_and_permission_expansion()
     {
        let (source_pins, source_bundle, envelope) = request();
        let (pins, bundle, resource) = execution();
        assert!(
            inspect_remote_execution_request(
                &encode(&envelope).unwrap(),
                "[]",
                &encode(&source_bundle).unwrap(),
                NOW as f64
            )
            .is_err()
        );
        assert!(
            inspect_remote_execution_request(
                &encode(&envelope).unwrap(),
                &encode(&pins).unwrap(),
                &encode(&source_bundle).unwrap(),
                NOW as f64
            )
            .is_err()
        );
        let mut changed = envelope.clone();
        changed.request.payload.task_creation.hash = "ff".repeat(32);
        assert!(
            inspect_remote_execution_request(
                &encode(&changed).unwrap(),
                &encode(&source_pins).unwrap(),
                &encode(&source_bundle).unwrap(),
                NOW as f64
            )
            .is_err()
        );
        changed = envelope.clone();
        changed.authorization.journal.pop();
        assert!(
            inspect_remote_execution_request(
                &encode(&changed).unwrap(),
                &encode(&source_pins).unwrap(),
                &encode(&changed.authorization).unwrap(),
                NOW as f64
            )
            .is_err()
        );
        assert!(
            inspect_remote_execution_request(
                &encode(&envelope).unwrap(),
                &encode(&source_pins).unwrap(),
                &encode(&source_bundle).unwrap(),
                (NOW + 100_001) as f64
            )
            .is_err()
        );
        let call = |input: serde_json::Value| {
            prepare_remote_execution_share(
                &input.to_string(),
                &key(13),
                &encode(&pins).unwrap(),
                &encode(&bundle).unwrap(),
                &encode(&source_pins).unwrap(),
                &encode(&source_bundle).unwrap(),
                NOW as f64,
            )
        };
        for actions in [
            serde_json::json!(["message_append"]),
            serde_json::json!(["execution_resolve"]),
            serde_json::json!(["operation_resolve"]),
            serde_json::json!(["execution_submit"]),
            serde_json::json!(["execution_resolve", "operation_resolve", "execution_submit"]),
            serde_json::json!(["execution_resolve", "operation_resolve", "execution_send"]),
            serde_json::json!([
                "execution_resolve",
                "operation_resolve",
                "operation_status_read"
            ]),
            serde_json::json!(["execution_send", "execution_send"]),
        ] {
            let mut input = intent(&envelope, &resource);
            input["actions"] = actions;
            assert!(call(input).is_err());
        }
        let (_, _, project) = personal(11);
        let mut input = intent(&envelope, &resource);
        input["executionResourceId"] = project.resource.to_string().into();
        assert!(call(input).is_err());
        let mut input = intent(&envelope, &resource);
        input["expiresAt"] = NOW.into();
        assert!(call(input).is_err());
        let mut input = intent(&envelope, &resource);
        input["taskRef"] = "ff".repeat(32).into();
        assert!(call(input).is_err());
    }
}
