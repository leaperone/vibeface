//! Task-scoped execution sharing from a bounded, independently trusted source read.
use super::execution_handoff::ExecutionProofUsage;
use super::policy_import::merge_same_head_snapshots;
use super::{governance::*, *};
use crate::signature_verification::{ScopedSignatureVerification, SignatureVerification};
mod policy;
pub use policy::*;

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
fn finish_usage(usage: &mut ExecutionProofUsage) -> Result<()> {
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteExecutionShareIntent {
    pub execution_resource_id: ResourceId,
    pub canonical_cwd: Option<String>,
    pub actions: Vec<Action>,
    pub expires_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RemoteExecutionShareSelection {
    execution_resource: ResourceRef,
    target_public_key: PublicKey,
    execution_grant: ObjectHash,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedRemoteExecutionShare {
    prepared: PreparedSpaceGovernanceChange,
    grant: Signed<Grant>,
    selection: RemoteExecutionShareSelection,
    usage: ExecutionProofUsage,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareRemoteExecutionShare))]
pub fn prepare_remote_execution_share(
    intent_json: &str,
    signer: &Identity,
    owner: &VerifiedClientAuthorization,
    source: &VerifiedRemoteShareRequest,
    now_ms: f64,
) -> crate::Result<PreparedRemoteExecutionShare> {
    let _signatures = ScopedSignatureVerification::new();
    let build = || -> Result<_> {
        let intent: RemoteExecutionShareIntent = parse(intent_json, MAX_PROOF_BYTES)?;
        let now = time(now_ms)?;
        let owner_snapshot = client_policy_import::require_confirmed_client_policy(owner)?;
        let merged = merge_same_head_snapshots(owner_snapshot, source.snapshot())?;
        let original = source.original().intent();
        let task = &original.task_creation.payload;
        let resource = ResourceRef {
            space: owner.scope.space_id.clone(),
            resource: intent.execution_resource_id,
        };
        require(
            owner.scope.device_id == DeviceId::from_key(&PublicKey::of(signer))
                && *source.reader_public_key() == PublicKey::of(signer)
                && resource.space == SpaceId::personal(&owner.scope.account_id)
                && scope_covers(&owner.grant_scope, &resource)
                && owner.actions.contains(&Action::ResourceManage)
                && owner.resources.len() == 1
                && owner.resources[0].payload.resource == resource.resource
                && intent
                    .canonical_cwd
                    .as_ref()
                    .is_none_or(|cwd| text(cwd, 4096)),
            "execution sharing requires the current genuine owner and original policy reader",
        )?;
        let state = merged.state();
        let record = state.resource(&resource.space, &resource.resource).ok_or(
            AuthorizationError::InvalidProof("execution service is not registered"),
        )?;
        require(
            record.payload.kind == ResourceKind::ExecutionService,
            "Project resources cannot be shared as execution services",
        )?;
        let target = owner.audience_public_key.clone();
        let certificate =
            state.verify_service_device(&resource.space, &DeviceId::from_key(&target), now)?;
        require(
            record.payload.host == ServiceId::from_key(&target)
                && owner.audience.service == record.payload.host
                && owner.audience.device == certificate.payload.device
                && certificate.payload.public_key == target
                && certificate.payload.owner
                    == PolicyDomain::Account(owner.scope.account_id.clone())
                && target != original.source_node_public_key,
            "execution sharing changed the selected service or current owner audience",
        )?;
        if let Some(selected) = &original.execution_resource {
            require(
                selected == &resource && original.target_public_key.as_ref() == Some(&target),
                "execution sharing changed the original exact executor",
            )?;
        }
        let mut usage = source.revalidate(now)?;
        let mut authority_budget = Budget::default();
        state.governance_capability(
            &owner.grant_leaf,
            &PublicKey::of(signer),
            &resource.space,
            Some(Action::ResourceManage),
            Some(&resource.resource),
            now,
            &mut authority_budget,
        )?;
        usage.record_budget(&authority_budget)?;
        let mut actions = intent.actions;
        actions.sort();
        require(
            !actions.is_empty() && actions.windows(2).all(|pair| pair[0] != pair[1]),
            "execution sharing actions must be distinct",
        )?;
        let grantee = match original.purpose {
            RemoteSharePurpose::Execution => {
                require(
                    actions.iter().all(|action| {
                        matches!(
                            action,
                            Action::ExecutionOpen
                                | Action::ExecutionSend
                                | Action::ExecutionSteer
                                | Action::ExecutionInterrupt
                                | Action::ExecutionRespondApproval
                                | Action::ExecutionSubmit
                                | Action::OperationStatusRead
                        )
                    }) && actions
                        .iter()
                        .any(|action| *action != Action::ExecutionSubmit),
                    "execution sharing requires ordinary execution actions",
                )?;
                if !actions.contains(&Action::ExecutionSubmit) {
                    actions.push(Action::ExecutionSubmit);
                    actions.sort();
                }
                Actor::Service(ServiceId::from_key(&original.source_node_public_key))
            }
            RemoteSharePurpose::Resolution => {
                require(
                    actions == [Action::ExecutionResolve, Action::OperationResolve],
                    "resolution sharing requires its exact purpose pair",
                )?;
                Actor::Service(ServiceId::from_key(&original.source_node_public_key))
            }
            RemoteSharePurpose::Rebind => {
                require(
                    actions == [Action::ExecutionRebind, Action::OperationStatusRead],
                    "rebind sharing requires its exact purpose pair",
                )?;
                Actor::Account(original.requester.clone())
            }
        };
        require(
            intent.expires_at > now && intent.expires_at <= 9_007_199_254_740_991,
            "execution sharing requires a finite future deadline",
        )?;
        let directional = DirectionalGrantIntent {
            space: resource.space.clone(),
            grantee: Grantee::Actor(grantee),
            scope: ResourceScope::Exact(resource.clone()),
            actions,
            audience_node_public_key: target.clone(),
            constraints: GrantConstraints {
                require_account_actor: original.purpose == RemoteSharePurpose::Rebind,
                context: InvocationContext {
                    conversation: Some(task.conversation_id.clone()),
                    task: Some(task.task.clone()),
                    sdk_session: None,
                    execution_session: Some(SessionRef {
                        target: DeviceId::from_key(&target),
                        session_id: original.session_id.clone(),
                    }),
                    executor_kind: Some(original.executor_kind.clone()),
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
            owner,
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
        usage.record_signed(grant)?;
        usage.record_metadata(&directional)?;
        finish_usage(&mut usage)?;
        Ok(PreparedRemoteExecutionShare {
            selection: RemoteExecutionShareSelection {
                execution_resource: resource,
                target_public_key: target,
                execution_grant: grant.hash()?,
            },
            grant: grant.clone(),
            prepared,
            usage,
        })
    };
    build().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedRemoteExecutionShare {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = requestJson))]
    pub fn request_json(&self) -> crate::Result<String> {
        self.prepared.request_json()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = grantHash))]
    pub fn grant_hash(&self) -> String {
        self.selection.execution_grant.to_string()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = resourceId))]
    pub fn resource_id(&self) -> String {
        self.selection.execution_resource.resource.to_string()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        encode(&self.usage.counts())
    }
}

fn verify_finished_leaf(
    prepared: &PreparedRemoteExecutionShare,
    change: &VerifiedSpaceGovernanceChange,
    now: u64,
) -> Result<ExecutionProofUsage> {
    let state = change.snapshot().state();
    let grant = state.grant(&prepared.selection.execution_grant).ok_or(
        AuthorizationError::InvalidProof("committed sharing leaf is missing"),
    )?;
    require(
        grant == &prepared.grant,
        "committed sharing leaf changed its approved purpose or recipient",
    )?;
    let target = state.verify_service_device(
        &prepared.selection.execution_resource.space,
        &DeviceId::from_key(&prepared.selection.target_public_key),
        now,
    )?;
    let record = state
        .resource(
            &prepared.selection.execution_resource.space,
            &prepared.selection.execution_resource.resource,
        )
        .ok_or(AuthorizationError::InvalidProof(
            "committed execution service is missing",
        ))?;
    require(
        record.payload.kind == ResourceKind::ExecutionService
            && record.payload.host == grant.payload.audience.service
            && target.payload.public_key == prepared.selection.target_public_key
            && target.payload.device == grant.payload.audience.device,
        "committed sharing leaf changed its selected execution service",
    )?;
    let mut budget = Budget::default();
    let mut dependencies = BTreeMap::new();
    budget.verify(record)?;
    state.validate_certificate(
        target,
        now,
        &mut budget,
        &mut dependencies,
        &mut BTreeSet::new(),
    )?;
    let path = state.grant_chain(Some(&prepared.selection.execution_grant))?;
    state.verify_grant_path(&path, now, &mut budget, &mut dependencies, true)?;
    state.expand_authorities(now, &mut budget, &mut dependencies)?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_budget(&budget)?;
    usage.record_chain_depth(path.len());
    finish_usage(&mut usage)?;
    Ok(usage)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedRemoteExecutionShare {
    change: VerifiedSpaceGovernanceChange,
    selection: Option<RemoteExecutionShareSelection>,
    usage: ExecutionProofUsage,
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishRemoteExecutionShare))]
pub fn finish_remote_execution_share(
    prepared: &PreparedRemoteExecutionShare,
    committed_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedRemoteExecutionShare> {
    let _signatures = ScopedSignatureVerification::new();
    (|| -> Result<_> {
        let now = time(now_ms)?;
        let committed: Committed<SpaceDocument> = parse(committed_json, MAX_PROOF_BYTES)?;
        let change = finish_space_governance_change(&prepared.prepared, committed_json, now_ms)
            .map_err(|_| {
                AuthorizationError::InvalidProof("invalid committed execution sharing change")
            })?;
        let mut usage =
            verify_finished_leaf(prepared, &change, committed.commit.payload.accepted_at_ms)?;
        let selection = match verify_finished_leaf(prepared, &change, now) {
            Ok(current) => {
                usage.merge(&current);
                Some(prepared.selection.clone())
            }
            Err(AuthorizationError::BudgetExceeded) => {
                return Err(AuthorizationError::BudgetExceeded);
            }
            Err(_) => None,
        };
        usage.record_signed(&committed.commit)?;
        usage.record_metadata(&committed)?;
        finish_usage(&mut usage)?;
        Ok(VerifiedRemoteExecutionShare {
            change,
            selection,
            usage,
        })
    })()
    .map_err(|error| error.to_string())
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedRemoteExecutionShare {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        self.change.pins_json()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = scopeJson))]
    pub fn scope_json(&self) -> crate::Result<String> {
        self.change.scope_json()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = selectionJson))]
    pub fn selection_json(&self) -> crate::Result<String> {
        encode(&self.selection)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        encode(&self.usage.counts())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorization::bootstrap::*;
    use crate::authorization::events::*;
    use crate::authorization::execution_handoff::{
        ExecutionHandoffReceipt, ExecutionHandoffState, ExecutionSessionSnapshot,
    };
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
            Action::ExecutionRebind,
            Action::OperationStatusRead,
            Action::OperationResolve,
            Action::ResourceManage,
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

    fn confirmed(
        pins: &[PolicyPin],
        bundle: &ClientAuthorizationBundle,
        resource: &ResourceRef,
        own: &Identity,
        node: &Identity,
    ) -> VerifiedClientAuthorization {
        governance::tests::confirmed_authorization(
            &replay_policies(pins, &bundle.journal).unwrap(),
            pins,
            bundle,
            resource,
            own,
            node,
            NOW,
        )
    }
    fn source_request(
        purpose: RemoteSharePurpose,
        resource: &ResourceRef,
        reader: &Identity,
    ) -> (
        Vec<PolicyPin>,
        ClientAuthorizationBundle,
        SignedRemoteShareIntent,
    ) {
        let (pins, bundle, project) = personal(1);
        let creation = task(&pins, &bundle, &project);
        let event: Event = serde_json::from_str(&creation.canonical_body).unwrap();
        let attestation = Signed::sign(
            RemoteShareTaskAttestation {
                version: AUTH_VERSION,
                project: project.clone(),
                task: EventRef::parse(&creation.hash).unwrap(),
                task_id: "task-1".into(),
                conversation_id: event.scope_id,
                author: event.author,
                sequence: event.sequence,
                source_node_public_key: PublicKey::of(&key(4)),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 100_000,
            },
            &key(4),
        )
        .unwrap();
        let mut intent = RemoteShareIntent {
            version: AUTH_VERSION,
            purpose,
            project: project.clone(),
            task_creation: attestation,
            source_node_public_key: PublicKey::of(&key(4)),
            requester: AccountId::from_root(&PublicKey::of(&key(1))),
            requester_device: DeviceId::from_key(&PublicKey::of(&key(3))),
            saved_pins: vec![],
            policy_heads: vec![],
            session_id: "selected-session".into(),
            executor_kind: "codex".into(),
            execution_resource: Some(resource.clone()),
            target_public_key: Some(PublicKey::of(&key(14))),
            old_binding: None,
            old_execution_receipt: None,
            nonce: Nonce::random().unwrap(),
            issued_at_ms: NOW,
            expires_at_ms: NOW + 100_000,
        };
        if purpose != RemoteSharePurpose::Execution {
            let binding = task_binding::TaskBinding {
                network_id: project.space.to_string(),
                conversation_id: "chat".into(),
                task_ref: creation.hash,
                task_id: "task-1".into(),
                authority_node: key(4).node_id(),
                generation: 1,
                target_node: key(14).node_id(),
                session_id: intent.session_id.clone(),
                open_spec: serde_json::json!({"type":"open","executor":"codex","cwd":"/configured/workspace"}),
                previous_binding_hash: None,
                previous_seal: None,
            };
            let binding = task_binding::SignedBinding {
                signature: key(4)
                    .sign(
                        task_binding::BINDING_DOMAIN,
                        &serde_json::to_vec(&binding).unwrap(),
                    )
                    .unwrap(),
                binding,
            };
            let receipt = Signed::sign(
                ExecutionHandoffReceipt {
                    version: AUTH_VERSION,
                    operation_id: "old-operation".into(),
                    original_space_id: project.space.clone(),
                    execution_space_id: resource.space.clone(),
                    handoff_hash: ObjectHash::body(&"signed old handoff").unwrap(),
                    target: DeviceId::from_key(&PublicKey::of(&key(14))),
                    session_id: intent.session_id.clone(),
                    binding_generation: 1,
                    state: ExecutionHandoffState::Succeeded,
                    result_hash: ObjectHash::body(&"old result").unwrap(),
                    session: Some(ExecutionSessionSnapshot {
                        original_project: project.clone(),
                        execution_resource: resource.clone(),
                        task: intent.task_creation.payload.task.clone(),
                        binding_hash: task_binding::binding_hash(&binding.binding).unwrap(),
                        binding_generation: 1,
                        target: DeviceId::from_key(&PublicKey::of(&key(14))),
                        session_id: intent.session_id.clone(),
                        revision: 7,
                        executor: "codex".into(),
                        cwd: "/configured/workspace".into(),
                        state: "closed".into(),
                        expires_at_ms: NOW + EXECUTION_LEASE_MS,
                    }),
                    issued_at_ms: NOW,
                },
                &key(14),
            )
            .unwrap();
            intent.old_binding = Some(binding);
            intent.old_execution_receipt = Some(receipt);
        }
        let source = confirmed(&pins, &bundle, &project, &key(3), &key(4));
        let original =
            build_remote_share_intent(&encode(&intent).unwrap(), &source, &key(3), NOW as f64)
                .unwrap();
        let original: SignedRemoteShareIntent = serde_json::from_str(&original).unwrap();
        let _ = reader;
        (pins, bundle, original)
    }
    fn import_source(
        pins: &[PolicyPin],
        bundle: &ClientAuthorizationBundle,
        original: &SignedRemoteShareIntent,
        reader: &Identity,
    ) -> VerifiedRemoteShareRequest {
        let request_json = build_remote_share_policy_read_request(
            &encode(original).unwrap(),
            reader,
            &encode(&pins).unwrap(),
            &key(4).public_key_hex(),
            NOW as f64,
            (NOW + 90_000) as f64,
        )
        .unwrap();
        let request: Signed<RemoteSharePolicyReadRequest> =
            serde_json::from_str(&request_json).unwrap();
        let state = replay_policies(pins, &bundle.journal).unwrap();
        let read = verify_remote_share_policy_read(
            &state,
            &request,
            &PublicKey::of(&key(4)),
            &PublicKey::of(reader),
            NOW,
        )
        .unwrap();
        assert!(
            verify_remote_share_policy_read(
                &state,
                &request,
                &PublicKey::of(&key(4)),
                &PublicKey::of(&key(99)),
                NOW
            )
            .is_err()
        );
        let header = Signed::sign(
            RemoteSharePolicySnapshot {
                version: AUTH_VERSION,
                intent_hash: original.hash().unwrap(),
                request_hash: request.hash().unwrap(),
                snapshot_id: Nonce::random().unwrap(),
                reader_public_key: PublicKey::of(reader),
                saved_pins_hash: ObjectHash::body(&pins).unwrap(),
                target_heads: read.heads().to_vec(),
                source_node_public_key: PublicKey::of(&key(4)),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 80_000,
            },
            &key(4),
        )
        .unwrap();
        assert!(
            begin_remote_share_policy_import(
                &request_json,
                &encode(&header).unwrap(),
                "[]",
                &key(4).public_key_hex(),
                NOW as f64
            )
            .is_err()
        );
        let mut import = begin_remote_share_policy_import(
            &request_json,
            &encode(&header).unwrap(),
            &encode(&pins).unwrap(),
            &key(4).public_key_hex(),
            NOW as f64,
        )
        .unwrap();
        let mut previous = header.hash().unwrap();
        let mut cursor = None;
        for (index, entry) in bundle.journal.iter().enumerate() {
            let last = index + 1 == bundle.journal.len();
            let page = Signed::sign(
                RemoteSharePolicyPage {
                    version: AUTH_VERSION,
                    intent_hash: original.hash().unwrap(),
                    request_hash: request.hash().unwrap(),
                    snapshot_id: header.payload.snapshot_id.clone(),
                    target_heads_hash: ObjectHash::body(&read.heads()).unwrap(),
                    page_index: index as u64,
                    request_cursor: cursor.clone(),
                    previous_page_hash: previous,
                    entry: Some(entry.clone()),
                    next_cursor: (!last)
                        .then(|| ObjectHash::body(&("source cursor", index)).unwrap()),
                    complete: last,
                },
                &key(4),
            )
            .unwrap();
            let mut wrong = page.payload.clone();
            wrong.request_cursor = Some(ObjectHash::body(&"wrong cursor").unwrap());
            let wrong = Signed::sign(wrong, &key(4)).unwrap();
            assert!(
                import
                    .append_page(&encode(&wrong).unwrap(), NOW as f64)
                    .is_err()
            );
            import
                .append_page(&encode(&page).unwrap(), NOW as f64)
                .unwrap();
            import
                .append_page(&encode(&page).unwrap(), NOW as f64)
                .unwrap();
            previous = page.hash().unwrap();
            cursor = page.payload.next_cursor;
        }
        let witness = Signed::sign(
            RemoteSharePolicyWitness {
                version: AUTH_VERSION,
                intent_hash: original.hash().unwrap(),
                request_hash: request.hash().unwrap(),
                snapshot_id: header.payload.snapshot_id.clone(),
                target_heads: read.heads().to_vec(),
                final_page_hash: previous,
                page_count: bundle.journal.len() as u64,
                entry_count: bundle.journal.len() as u64,
                snapshot_hash: header.hash().unwrap(),
                reader_public_key: PublicKey::of(reader),
                source_node_public_key: PublicKey::of(&key(4)),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 70_000,
            },
            &key(4),
        )
        .unwrap();
        import
            .finish(&encode(&witness).unwrap(), NOW as f64)
            .unwrap()
    }
    fn share_intent(resource: &ResourceRef, actions: &[Action]) -> String {
        serde_json::json!({"executionResourceId":resource.resource,"canonicalCwd":"/configured/workspace","actions":actions,"expiresAt":END}).to_string()
    }
    fn commit_share(
        prepared: &PreparedRemoteExecutionShare,
        owner: &VerifiedClientAuthorization,
    ) -> String {
        let snapshot = client_policy_import::require_confirmed_client_policy(owner).unwrap();
        let challenge_json = issue_space_governance_challenge(
            &prepared.request_json().unwrap(),
            &key(14),
            owner,
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
        assert!(!snapshot.heads().is_empty());
        accept_space_governance_change(
            &prepared.request_json().unwrap(),
            &challenge_json,
            &encode(&receipts).unwrap(),
            &key(14),
            owner,
            NOW as f64,
        )
        .unwrap()
    }
    #[test]
    fn bounded_source_policy_keeps_original_account_and_exact_share_purposes() {
        let (pins, bundle, resource) = execution();
        let owner = confirmed(&pins, &bundle, &resource, &key(13), &key(14));
        for purpose in [
            RemoteSharePurpose::Execution,
            RemoteSharePurpose::Resolution,
            RemoteSharePurpose::Rebind,
        ] {
            let (source_pins, source_bundle, original) =
                source_request(purpose, &resource, &key(13));
            let source = import_source(&source_pins, &source_bundle, &original, &key(13));
            let summary: serde_json::Value =
                serde_json::from_str(&source.summary_json().unwrap()).unwrap();
            assert_eq!(
                summary["requesterAccountId"],
                serde_json::to_value(&original.intent.requester).unwrap()
            );
            assert_eq!(
                summary["taskRef"],
                serde_json::to_value(&original.intent.task_creation.payload.task).unwrap()
            );
            assert!(summary.get("authorization").is_none());
            let actions = match purpose {
                RemoteSharePurpose::Execution => vec![Action::ExecutionSend],
                RemoteSharePurpose::Resolution => {
                    vec![Action::ExecutionResolve, Action::OperationResolve]
                }
                RemoteSharePurpose::Rebind => {
                    vec![Action::ExecutionRebind, Action::OperationStatusRead]
                }
            };
            let prepared = prepare_remote_execution_share(
                &share_intent(&resource, &actions),
                &key(13),
                &owner,
                &source,
                NOW as f64,
            )
            .unwrap();
            let request: SpaceGovernanceRequest =
                serde_json::from_str(&prepared.request_json().unwrap()).unwrap();
            let [JournalEntry::Grant(grant)] = request.new_objects.as_slice() else {
                panic!("one exact sharing leaf")
            };
            let expected = if purpose == RemoteSharePurpose::Rebind {
                Actor::Account(original.intent.requester.clone())
            } else {
                Actor::Service(ServiceId::from_key(&PublicKey::of(&key(4))))
            };
            assert_eq!(grant.payload.grantee, Grantee::Actor(expected));
            assert_eq!(grant.payload.scope, ResourceScope::Exact(resource.clone()));
            assert_eq!(grant.payload.remaining_delegations, 0);
            assert_eq!(
                grant.payload.constraints.context.task,
                Some(original.intent.task_creation.payload.task.clone())
            );
            assert_eq!(
                grant.payload.constraints.context.workspace_resource,
                Some(resource.resource.clone())
            );
            assert_eq!(
                grant.payload.constraints.require_account_actor,
                purpose == RemoteSharePurpose::Rebind
            );
            assert_eq!(
                grant.payload.actions.contains(&Action::ExecutionSubmit),
                purpose == RemoteSharePurpose::Execution
            );
            let committed = commit_share(&prepared, &owner);
            let finished =
                finish_remote_execution_share(&prepared, &committed, NOW as f64).unwrap();
            let selection: RemoteExecutionShareSelection =
                serde_json::from_str(&finished.selection_json().unwrap()).unwrap();
            assert_eq!(selection.execution_grant, grant.hash().unwrap());
            assert_eq!(selection.execution_resource, resource);
            let late =
                finish_remote_execution_share(&prepared, &committed, (END + 1) as f64).unwrap();
            assert_eq!(late.selection_json().unwrap(), "null");
            assert_eq!(late.pins_json().unwrap(), finished.pins_json().unwrap());
            let mut future = source_pins.clone();
            let minimum = future[0].minimum_head.as_mut().unwrap();
            minimum.sequence += 1;
            minimum.head = ObjectHash::body(&"unknown higher source head").unwrap();
            assert!(
                source
                    .revalidate_request(&encode(&future).unwrap(), NOW as f64)
                    .is_err()
            );
            assert!(
                source
                    .revalidate_request(&encode(&source_pins).unwrap(), NOW as f64)
                    .is_ok()
            );
            let wrong = if purpose == RemoteSharePurpose::Rebind {
                vec![
                    Action::ExecutionRebind,
                    Action::OperationStatusRead,
                    Action::ExecutionSubmit,
                ]
            } else {
                vec![Action::ExecutionRebind, Action::OperationStatusRead]
            };
            assert!(
                prepare_remote_execution_share(
                    &share_intent(&resource, &wrong),
                    &key(13),
                    &owner,
                    &source,
                    NOW as f64
                )
                .is_err()
            );
            assert!(
                prepare_remote_execution_share(
                    &share_intent(&resource, &actions),
                    &key(99),
                    &owner,
                    &source,
                    NOW as f64
                )
                .is_err()
            );
        }
    }
}
