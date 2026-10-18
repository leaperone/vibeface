//! Node-local, Task-scoped approval and bounded foreign execution policy reads.
use super::{
    execution_handoff::ExecutionProofUsage,
    remote_execution_policy::{
        RemoteExecutionPolicyPurpose, RemoteExecutionPolicyReadIntent,
        RemoteExecutionPolicySelection, VerifiedRemoteExecutionPolicySourceIntent,
    },
    task_binding::{self, SignedBinding},
    *,
};
use crate::signature_verification::ScopedSignatureVerification;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NodeExecutionPolicyReadIntent {
    pub version: u16,
    pub source_project: ResourceRef,
    pub selection: RemoteExecutionPolicySelection,
    pub context: prepare::PrepareContext,
    pub expected_action: Action,
    pub purpose: RemoteExecutionPolicyPurpose,
    pub execution_pins: Vec<PolicyPin>,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for NodeExecutionPolicyReadIntent {}
impl SignedObject for NodeExecutionPolicyReadIntent {
    const DOMAIN: &'static str = "node-execution-policy-read-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NodeExecutionHistoryPolicyReadIntent {
    pub version: u16,
    pub source_project: ResourceRef,
    pub expected_action: Action,
    pub reference: remote_execution_history::RemoteExecutionHistoryReference,
    pub execution_pins: Vec<PolicyPin>,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for NodeExecutionHistoryPolicyReadIntent {}
impl SignedObject for NodeExecutionHistoryPolicyReadIntent {
    const DOMAIN: &'static str = "node-execution-history-policy-read-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "intent", rename_all = "snake_case")]
pub enum NodeExecutionPolicyReadRequest {
    Current(Signed<NodeExecutionPolicyReadIntent>),
    History(Signed<NodeExecutionHistoryPolicyReadIntent>),
}

impl NodeExecutionPolicyReadRequest {
    pub fn issuer(&self) -> &PublicKey {
        match self {
            Self::Current(value) => &value.issuer,
            Self::History(value) => &value.issuer,
        }
    }
    pub fn source_project(&self) -> &ResourceRef {
        match self {
            Self::Current(value) => &value.payload.source_project,
            Self::History(value) => &value.payload.source_project,
        }
    }
    pub fn selection(&self) -> Result<RemoteExecutionPolicySelection> {
        Ok(match self {
            Self::Current(value) => value.payload.selection.clone(),
            Self::History(value) => RemoteExecutionPolicySelection {
                execution_resource: value
                    .payload
                    .reference
                    .old_submission
                    .payload
                    .resource
                    .clone(),
                target_public_key: value.payload.reference.target_public_key.clone(),
                execution_grant: value
                    .payload
                    .reference
                    .old_submission
                    .payload
                    .grant_leaf
                    .clone(),
            },
        })
    }
    pub fn context(&self) -> Result<prepare::PrepareContext> {
        match self {
            Self::Current(value) => Ok(value.payload.context.clone()),
            Self::History(value) => value.payload.reference.context(),
        }
    }
    pub fn purpose(&self) -> RemoteExecutionPolicyPurpose {
        match self {
            Self::Current(value) => value.payload.purpose,
            Self::History(_) => RemoteExecutionPolicyPurpose::History,
        }
    }
    pub fn expected_action(&self) -> Action {
        match self {
            Self::Current(value) => value.payload.expected_action,
            Self::History(value) => value.payload.expected_action,
        }
    }
    pub fn execution_pins(&self) -> &[PolicyPin] {
        match self {
            Self::Current(value) => &value.payload.execution_pins,
            Self::History(value) => &value.payload.execution_pins,
        }
    }
    pub fn issued_at_ms(&self) -> u64 {
        match self {
            Self::Current(value) => value.payload.issued_at_ms,
            Self::History(value) => value.payload.issued_at_ms,
        }
    }
    pub fn expires_at_ms(&self) -> u64 {
        match self {
            Self::Current(value) => value.payload.expires_at_ms,
            Self::History(value) => value.payload.expires_at_ms,
        }
    }
    pub fn history(&self) -> Option<&remote_execution_history::RemoteExecutionHistoryReference> {
        match self {
            Self::Current(_) => None,
            Self::History(value) => Some(&value.payload.reference),
        }
    }
}

#[derive(Clone)]
pub struct ActualPolicyNoisePeer {
    public_key: PublicKey,
}
impl ActualPolicyNoisePeer {
    pub fn from_finished_handshake(handshake: &crate::Handshake) -> Result<Self> {
        require(
            handshake.is_finished(),
            "policy peer requires a completed Noise handshake",
        )?;
        let key = handshake
            .remote_public_key()
            .ok_or(AuthorizationError::InvalidProof(
                "policy peer has no actual Noise identity",
            ))?;
        Ok(Self {
            public_key: PublicKey::parse(&key)?,
        })
    }
    pub fn public_key(&self) -> &PublicKey {
        &self.public_key
    }
}

#[derive(Clone)]
pub enum PolicyCarrierOrigin {
    SignedTranscript,
    DirectNoise(ActualPolicyNoisePeer),
}

pub struct VerifiedNodeExecutionPolicyRead {
    request: NodeExecutionPolicyReadRequest,
    heads: Vec<PolicyStamp>,
    allowed_hashes: BTreeSet<ObjectHash>,
    usage: ExecutionProofUsage,
}
impl VerifiedNodeExecutionPolicyRead {
    pub fn request(&self) -> &NodeExecutionPolicyReadRequest {
        &self.request
    }
    pub fn target_heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
    pub fn domains(&self) -> Vec<PolicyDomain> {
        self.heads.iter().map(|head| head.domain.clone()).collect()
    }
    pub fn allows_hash(&self, hash: &ObjectHash) -> bool {
        self.allowed_hashes.contains(hash)
    }
    pub fn allows_entry(&self, entry: &JournalEntry) -> Result<bool> {
        Ok(self.allows_hash(&policy_import::entry_hash(entry)?))
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.request.expires_at_ms()
    }
    pub fn request_hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(&self.request)
    }
}

fn read_window(issued: u64, expires: u64, now: u64) -> Result<()> {
    require(
        issued <= now && now < expires,
        "node policy read is outside its signed window",
    )?;
    require(
        expires
            .checked_sub(issued)
            .is_some_and(|age| age <= INVOCATION_MAX_AGE_MS),
        "node policy read exceeds its time limit",
    )
}

/// The caller supplies the real Noise key, without creating a business Peer capability.
pub fn verify_node_execution_policy_read(
    state: &VerifiedPolicySet,
    request: &NodeExecutionPolicyReadRequest,
    local_c: &PublicKey,
    actual_noise_b: &PublicKey,
    history_acceptance: Option<&remote_execution_history::RemoteExecutionHistoryAcceptance>,
    now: u64,
) -> Result<VerifiedNodeExecutionPolicyRead> {
    let signatures = ScopedSignatureVerification::new();
    require(
        canonical(request)?.len() <= MAX_PROOF_BYTES,
        "node policy read carrier exceeds 64 KiB",
    )?;
    require(
        request.issuer() == actual_noise_b,
        "node policy reader differs from its actual Noise key",
    )?;
    read_window(request.issued_at_ms(), request.expires_at_ms(), now)?;
    validate_pins(request.execution_pins())?;
    let (heads, allowed_hashes, mut usage) = match request {
        NodeExecutionPolicyReadRequest::Current(intent) => {
            intent.verify_signature()?;
            let value = &intent.payload;
            require(
                value.version == AUTH_VERSION
                    && value.purpose != RemoteExecutionPolicyPurpose::History
                    && history_acceptance.is_none()
                    && value.context.context.workspace_resource.as_ref()
                        == Some(&value.source_project.resource)
                    && value.context.context.sdk_session.is_none()
                    && value.context.context.task.is_some()
                    && value.context.context.conversation.is_some()
                    && value.context.context.executor_kind.is_some()
                    && value
                        .context
                        .binding_generation
                        .is_some_and(|generation| generation > 0),
                "node policy read changed its Task, purpose or source Project",
            )?;
            let closure = remote_execution_policy::verify_execution_policy_closure(
                state,
                &value.selection,
                actual_noise_b,
                &value.context,
                value.expected_action,
                value.purpose,
                &value.execution_pins,
                local_c,
                now,
            )?;
            let mut usage = closure.usage;
            usage.record_signed(intent)?;
            (closure.heads, closure.allowed_hashes, usage)
        }
        NodeExecutionPolicyReadRequest::History(intent) => {
            intent.verify_signature()?;
            let value = &intent.payload;
            let reference = &value.reference;
            require(
                value.version == AUTH_VERSION
                    && matches!(
                        value.expected_action,
                        Action::OperationStatusRead
                            | Action::OperationResolve
                            | Action::OperationReconcile
                    )
                    && reference.source.issuer == *actual_noise_b
                    && reference.old_submission.issuer == *actual_noise_b
                    && reference.source.payload.project == value.source_project
                    && reference.target_public_key == *local_c,
                "node history read changed its original source or accepted artifact",
            )?;
            let acceptance = history_acceptance.ok_or(AuthorizationError::InvalidProof(
                "node history read has no original C acceptance",
            ))?;
            let checked = remote_execution_history::verify_remote_execution_history_policy(
                state,
                reference,
                acceptance,
                &value.execution_pins,
                local_c,
                now,
            )?;
            let mut usage = checked.proof_usage().clone();
            usage.record_signed(intent)?;
            (
                checked.target_heads().to_vec(),
                checked.allowed_hashes().clone(),
                usage,
            )
        }
    };
    usage.record_metadata(request)?;
    usage.record_actual_signature_counts(signatures.counts());
    usage.enforce_limits()?;
    Ok(VerifiedNodeExecutionPolicyRead {
        request: request.clone(),
        heads,
        allowed_hashes,
        usage,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TaskExecutionTrustApproval {
    pub version: u16,
    pub original_intent_hash: ObjectHash,
    pub source_node_public_key: PublicKey,
    pub current_binding: Option<SignedBinding>,
    pub approved_at_ms: u64,
}
impl sealed::Sealed for TaskExecutionTrustApproval {}
impl SignedObject for TaskExecutionTrustApproval {
    const DOMAIN: &'static str = "task-execution-trust-approval-v2";
}

#[derive(Clone)]
pub struct VerifiedTaskExecutionTrust {
    original_intent: Signed<RemoteExecutionPolicyReadIntent>,
    approval: Signed<TaskExecutionTrustApproval>,
    choice_key: ObjectHash,
}

/// This is a lookup key. It grants no authority and ignores a read's nonce and window.
pub fn task_execution_policy_choice_key(
    intent: &Signed<RemoteExecutionPolicyReadIntent>,
) -> Result<ObjectHash> {
    let value = &intent.payload;
    validate_pins(&value.execution_pins)?;
    let roots: Vec<_> = value
        .execution_pins
        .iter()
        .map(|pin| (&pin.domain, &pin.genesis_root))
        .collect();
    ObjectHash::body(&(
        "task-execution-policy-choice-v2",
        &value.source_selection.resource,
        &value.source_selection.audience,
        &value.selection,
        &value.context,
        roots,
    ))
}

fn validate_pins(pins: &[PolicyPin]) -> Result<()> {
    require(
        !pins.is_empty()
            && pins.len() <= MAX_POLICY_DOMAINS
            && pins.windows(2).all(|pair| pair[0].domain < pair[1].domain)
            && pins.iter().all(|pin| {
                pin.minimum_head
                    .as_ref()
                    .is_none_or(|head| head.domain == pin.domain)
            }),
        "task execution trust requires sorted independent pins",
    )
}

fn approval_anchor(
    intent: &Signed<RemoteExecutionPolicyReadIntent>,
    binding: Option<&SignedBinding>,
    source: &PublicKey,
) -> Result<()> {
    let value = &intent.payload;
    let context = &value.context.context;
    let task = context
        .task
        .as_ref()
        .ok_or(AuthorizationError::InvalidProof("task trust has no Task"))?;
    let session = context
        .execution_session
        .as_ref()
        .ok_or(AuthorizationError::InvalidProof(
            "task trust has no execution session",
        ))?;
    require(
        value.version == AUTH_VERSION
            && value.purpose == RemoteExecutionPolicyPurpose::Execution
            && value.history.is_none()
            && value.source_selection.audience.service == ServiceId::from_key(source)
            && value.source_selection.audience.device == DeviceId::from_key(source)
            && context.workspace_resource.as_ref()
                == Some(&value.source_selection.resource.resource)
            && context.sdk_session.is_none()
            && session.target == DeviceId::from_key(&value.selection.target_public_key)
            && value.context.execution_target_public_key.as_ref()
                == Some(&value.selection.target_public_key),
        "task trust changed its source, Task or selected executor",
    )?;
    match (value.expected_action, binding) {
        (Action::ExecutionOpen, None) => require(
            value.context.binding_generation == Some(1),
            "first task trust requires binding generation one",
        ),
        (Action::ExecutionRebind, Some(binding)) => {
            task_binding::verify_binding(binding, source.as_str()).map_err(|_| {
                AuthorizationError::InvalidProof("task trust has an invalid current binding")
            })?;
            let prior = &binding.binding;
            require(
                prior.network_id == value.source_selection.resource.space.as_str()
                    && prior.task_ref == task.as_str()
                    && context.conversation.as_deref() == Some(prior.conversation_id.as_str())
                    && prior.generation.checked_add(1) == value.context.binding_generation,
                "task trust rebind changed its current Task or next generation",
            )
        }
        _ => Err(AuthorizationError::InvalidProof(
            "only initial execution selection or rebind can approve task roots",
        )),
    }
}

/// The Store checks its ready Task and current binding in the same approval transaction.
pub fn approve_task_execution_policy(
    source: &VerifiedRemoteExecutionPolicySourceIntent,
    current_binding: Option<&SignedBinding>,
    identity: &Identity,
    now: u64,
) -> Result<VerifiedTaskExecutionTrust> {
    let _scope = ScopedSignatureVerification::new();
    let intent = source.intent();
    let local = PublicKey::of(identity);
    require(
        source.source_node_public_key() == &local
            && source.verified_at_ms() == now
            && intent.payload.issued_at_ms <= now
            && now < intent.payload.expires_at_ms,
        "task trust requires this Node's current source verification",
    )?;
    approval_anchor(intent, current_binding, &local)?;
    let approval = Signed::sign(
        TaskExecutionTrustApproval {
            version: AUTH_VERSION,
            original_intent_hash: intent.hash()?,
            source_node_public_key: local,
            current_binding: current_binding.cloned(),
            approved_at_ms: now,
        },
        identity,
    )?;
    let mut usage = source.proof_usage().clone();
    usage.record_signed(&approval)?;
    if let Some(binding) = current_binding {
        usage.record_binding(binding)?;
    }
    usage.enforce_limits()?;
    verify_task_execution_trust(intent, &approval, source.source_node_public_key())
}

/// Restore an immutable local approval, without granting a current reader or writer.
pub fn verify_task_execution_trust(
    original_intent: &Signed<RemoteExecutionPolicyReadIntent>,
    approval: &Signed<TaskExecutionTrustApproval>,
    expected_source_node: &PublicKey,
) -> Result<VerifiedTaskExecutionTrust> {
    let _scope = ScopedSignatureVerification::new();
    original_intent.verify_signature()?;
    approval.verify_signature()?;
    let value = &approval.payload;
    require(
        approval.issuer == *expected_source_node
            && value.version == AUTH_VERSION
            && value.source_node_public_key == *expected_source_node
            && value.original_intent_hash == original_intent.hash()?
            && original_intent.payload.issued_at_ms <= value.approved_at_ms
            && value.approved_at_ms < original_intent.payload.expires_at_ms,
        "task trust approval changed its original signed selection or source",
    )?;
    validate_pins(&original_intent.payload.execution_pins)?;
    approval_anchor(
        original_intent,
        value.current_binding.as_ref(),
        expected_source_node,
    )?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_signed(original_intent)?;
    usage.record_signed(approval)?;
    if let Some(binding) = &value.current_binding {
        usage.record_binding(binding)?;
    }
    usage.enforce_limits()?;
    Ok(VerifiedTaskExecutionTrust {
        original_intent: original_intent.clone(),
        approval: approval.clone(),
        choice_key: task_execution_policy_choice_key(original_intent)?,
    })
}

impl VerifiedTaskExecutionTrust {
    pub fn original_intent(&self) -> &Signed<RemoteExecutionPolicyReadIntent> {
        &self.original_intent
    }
    pub fn approval(&self) -> &Signed<TaskExecutionTrustApproval> {
        &self.approval
    }
    pub fn choice_key(&self) -> &ObjectHash {
        &self.choice_key
    }
    pub fn selection(&self) -> &RemoteExecutionPolicySelection {
        &self.original_intent.payload.selection
    }
    pub fn pins(&self) -> &[PolicyPin] {
        &self.original_intent.payload.execution_pins
    }
    pub fn binding_generation(&self) -> u64 {
        self.original_intent
            .payload
            .context
            .binding_generation
            .unwrap_or(0)
    }
    pub fn original_binding(&self) -> Option<&SignedBinding> {
        self.approval.payload.current_binding.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        client_policy_import::require_confirmed_client_policy,
        execution_observation::tests::retained_history_fixture,
        remote_execution_policy::{
            build_remote_execution_policy_read_intent,
            verify_remote_execution_policy_delegation_source,
        },
    };
    use super::*;
    use serde_json::json;

    #[test]
    fn node_policy_read_uses_c_grant_and_real_noise_without_business_peer() {
        let fixture = retained_history_fixture();
        let now = 10_020;
        let source = PublicKey::of(&fixture.project_node);
        let target = PublicKey::of(&fixture.executor);
        let project = fixture
            .actual
            .handoff
            .handoff
            .source_statement()
            .unwrap()
            .project;
        let leaf = fixture
            .state
            .grants
            .values()
            .find(|grant| {
                grant.payload.grantee
                    == Grantee::Actor(Actor::Service(ServiceId::from_key(&source)))
                    && grant.payload.actions.contains(&Action::ExecutionOpen)
                    && fixture
                        .state
                        .space(&grant.payload.owner_space)
                        .is_some_and(|space| space.grants.contains(&grant.hash().unwrap()))
            })
            .unwrap();
        let ResourceScope::Exact(resource) = &leaf.payload.scope else {
            panic!("fixture grant is not exact")
        };
        let mut context = leaf.payload.constraints.context.clone();
        context.workspace_resource = Some(project.resource.clone());
        let value = NodeExecutionPolicyReadIntent {
            version: AUTH_VERSION,
            source_project: project,
            selection: RemoteExecutionPolicySelection {
                execution_resource: resource.clone(),
                target_public_key: target.clone(),
                execution_grant: leaf.hash().unwrap(),
            },
            context: prepare::PrepareContext {
                context,
                binding_generation: Some(1),
                canonical_cwd: None,
                execution_target_public_key: Some(target.clone()),
            },
            expected_action: Action::ExecutionOpen,
            purpose: RemoteExecutionPolicyPurpose::Execution,
            execution_pins: fixture.pins.clone(),
            nonce: Nonce::random().unwrap(),
            issued_at_ms: now,
            expires_at_ms: now + 1_000,
        };
        let request = NodeExecutionPolicyReadRequest::Current(
            Signed::sign(value.clone(), &fixture.project_node).unwrap(),
        );
        let read = verify_node_execution_policy_read(
            &fixture.state,
            &request,
            &target,
            &source,
            None,
            now,
        )
        .unwrap();
        assert!(!read.target_heads().is_empty());
        assert!(
            read.allows_entry(&JournalEntry::Grant(leaf.clone()))
                .unwrap()
        );
        assert_eq!(
            read.request_hash().unwrap(),
            ObjectHash::body(&request).unwrap()
        );
        assert!(
            verify_node_execution_policy_read(
                &fixture.state,
                &request,
                &target,
                &PublicKey::of(&fixture.client),
                None,
                now
            )
            .is_err()
        );
        let stolen = NodeExecutionPolicyReadRequest::Current(
            Signed::sign(value.clone(), &fixture.client).unwrap(),
        );
        assert!(
            verify_node_execution_policy_read(
                &fixture.state,
                &stolen,
                &target,
                &PublicKey::of(&fixture.client),
                None,
                now
            )
            .is_err()
        );
        let mut wrong_task = value;
        wrong_task.context.context.task = Some(EventRef::parse(&"ad".repeat(32)).unwrap());
        let changed = NodeExecutionPolicyReadRequest::Current(
            Signed::sign(wrong_task, &fixture.project_node).unwrap(),
        );
        assert!(
            verify_node_execution_policy_read(
                &fixture.state,
                &changed,
                &target,
                &source,
                None,
                now
            )
            .is_err()
        );
    }

    #[test]
    fn task_trust_uses_real_source_and_retains_immutable_selection() {
        let fixture = retained_history_fixture();
        let now = 10_020;
        let retained = require_confirmed_client_policy(&fixture.source).unwrap();
        let grant_leaf = retained
            .state()
            .grants
            .values()
            .find(|grant| {
                grant.payload.actions.contains(&Action::ExecutionOpen)
                    && grant.payload.grantee
                        == Grantee::Actor(Actor::Account(fixture.source.scope.account_id.clone()))
            })
            .unwrap()
            .hash()
            .unwrap();
        let source_auth = super::super::remote_execution_policy::tests::confirmed_source(
            &ClientAuthorizationBundle {
                journal: retained.journal().to_vec(),
                grant_leaf,
            },
            retained.pins(),
            &fixture.client,
            &fixture.project_node,
            &fixture
                .actual
                .handoff
                .handoff
                .source_statement()
                .unwrap()
                .project,
            now,
        )
        .unwrap();
        let target = PublicKey::of(&fixture.executor);
        let mut source_context = fixture.actual.handoff.invocation.payload.context.clone();
        source_context.workspace_resource =
            Some(fixture.source.resources[0].payload.resource.clone());
        let context = prepare::PrepareContext {
            context: source_context,
            binding_generation: Some(1),
            canonical_cwd: None,
            execution_target_public_key: Some(target.clone()),
        };
        let input = json!({
            "selection": {
                "execution_resource":fixture.actual.handoff.handoff.execution_resource,
                "target_public_key":target,
                "execution_grant":fixture.actual.handoff.invocation.payload.grant_leaf,
            },
            "context":context,
            "expected_action":Action::ExecutionOpen,
            "purpose":RemoteExecutionPolicyPurpose::Execution,
            "execution_pins":fixture.pins,
        });
        let encoded = build_remote_execution_policy_read_intent(
            &source_auth,
            &serde_json::to_string(&input).unwrap(),
            &fixture.client,
            now as f64,
            (now + 1_000) as f64,
        )
        .unwrap();
        let intent: Signed<RemoteExecutionPolicyReadIntent> =
            serde_json::from_str(&encoded).unwrap();
        let source = verify_remote_execution_policy_delegation_source(
            require_confirmed_client_policy(&source_auth)
                .unwrap()
                .state(),
            &intent,
            &PublicKey::of(&fixture.project_node),
            &PublicKey::of(&fixture.client),
            now,
        )
        .unwrap();
        let approved =
            approve_task_execution_policy(&source, None, &fixture.project_node, now).unwrap();
        assert_eq!(approved.original_intent(), &intent);
        assert_eq!(approved.binding_generation(), 1);
        assert!(approve_task_execution_policy(&source, None, &fixture.executor, now).is_err());
        assert!(
            approve_task_execution_policy(&source, None, &fixture.project_node, now + 1).is_err()
        );

        let mut refreshed = intent.payload.clone();
        refreshed.nonce = Nonce::random().unwrap();
        refreshed.purpose = RemoteExecutionPolicyPurpose::ObservationRead;
        refreshed.expected_action = Action::OperationResolve;
        for pin in &mut refreshed.execution_pins {
            pin.minimum_head = None;
        }
        let refreshed = Signed::sign(refreshed, &fixture.client).unwrap();
        assert_eq!(
            task_execution_policy_choice_key(&refreshed).unwrap(),
            *approved.choice_key()
        );
        let mut changed = refreshed.payload.clone();
        changed.selection.execution_grant = ObjectHash::body(&"another grant").unwrap();
        let changed = Signed::sign(changed, &fixture.client).unwrap();
        assert_ne!(
            task_execution_policy_choice_key(&changed).unwrap(),
            *approved.choice_key()
        );
        assert!(
            verify_task_execution_trust(
                &changed,
                approved.approval(),
                &PublicKey::of(&fixture.project_node)
            )
            .is_err()
        );
        let restored = verify_task_execution_trust(
            &intent,
            approved.approval(),
            &PublicKey::of(&fixture.project_node),
        )
        .unwrap();
        assert_eq!(restored.choice_key(), approved.choice_key());
    }
}
