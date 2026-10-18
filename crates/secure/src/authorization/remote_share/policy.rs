use super::*;
use crate::authorization::{
    client_policy_import::require_confirmed_client_policy,
    execution_handoff::{ExecutionHandoffReceipt, ExecutionProofUsage},
    policy_import::{BoundedPolicyImport, VerifiedPolicySnapshot},
    task_binding::SignedBinding,
};
use crate::signature_verification::{ScopedSignatureVerification, SignatureVerification};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSharePurpose {
    Execution,
    Resolution,
    Rebind,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RemoteShareIntent {
    pub version: u16,
    pub purpose: RemoteSharePurpose,
    pub project: ResourceRef,
    pub work: RemoteShareWork,
    pub source_node_public_key: PublicKey,
    pub requester: AccountId,
    pub requester_device: DeviceId,
    pub saved_pins: Vec<PolicyPin>,
    pub policy_heads: Vec<PolicyStamp>,
    pub execution_resource: Option<ResourceRef>,
    pub target_public_key: Option<PublicKey>,
    pub old_binding: Option<SignedBinding>,
    pub old_execution_receipt: Option<Signed<ExecutionHandoffReceipt>>,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteShareWork {
    TaskSession {
        task_creation: Signed<RemoteShareTaskAttestation>,
        session_id: String,
        executor_kind: String,
        binding_generation: u64,
    },
    ConversationSession {
        source: Signed<RemoteShareOperationAttestation>,
        open: ConversationOpenEvidence,
    },
    Command {
        source: Signed<RemoteShareOperationAttestation>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RemoteShareOperationAttestation {
    pub version: u16,
    pub project: ResourceRef,
    pub work: execution_work::ExecutionWorkIntent,
    pub context: prepare::PrepareContext,
    pub request: Value,
    pub source_node_public_key: PublicKey,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConversationOpenEvidence {
    Opening,
    Accepted {
        open: Box<execution_handoff::ExecutionHandoffResult>,
    },
}

impl sealed::Sealed for RemoteShareOperationAttestation {}
impl SignedObject for RemoteShareOperationAttestation {
    const DOMAIN: &'static str = "remote-share-operation-attestation-v2";
}

impl RemoteShareWork {
    fn operation_source(&self) -> Option<&Signed<RemoteShareOperationAttestation>> {
        match self {
            Self::TaskSession { .. } => None,
            Self::ConversationSession { source, .. } | Self::Command { source } => Some(source),
        }
    }
}

impl RemoteShareIntent {
    pub fn execution_context(
        &self,
        target: &PublicKey,
        resource: &ResourceRef,
    ) -> Result<InvocationContext> {
        let mut context = match &self.work {
            RemoteShareWork::TaskSession {
                task_creation,
                session_id,
                executor_kind,
                ..
            } => InvocationContext {
                conversation: Some(task_creation.payload.conversation_id.clone()),
                task: Some(task_creation.payload.task.clone()),
                execution_session: Some(SessionRef {
                    target: DeviceId::from_key(target),
                    session_id: session_id.clone(),
                }),
                executor_kind: Some(executor_kind.clone()),
                ..Default::default()
            },
            RemoteShareWork::ConversationSession { source, .. }
            | RemoteShareWork::Command { source } => {
                require(
                    source.payload.context.execution_target_public_key.as_ref() == Some(target),
                    "sharing changed the operation's selected executor",
                )?;
                source.payload.context.context.clone()
            }
        };
        context.sdk_session = None;
        context.workspace_resource = Some(resource.resource.clone());
        Ok(context)
    }

    pub fn work_intent(&self, target: &PublicKey) -> execution_work::ExecutionWorkIntent {
        match &self.work {
            RemoteShareWork::TaskSession {
                task_creation,
                session_id,
                binding_generation,
                ..
            } => execution_work::ExecutionWorkIntent::TaskSession {
                task: task_creation.payload.task.clone(),
                binding_generation: *binding_generation,
                execution_session: SessionRef {
                    target: DeviceId::from_key(target),
                    session_id: session_id.clone(),
                },
            },
            RemoteShareWork::ConversationSession { source, .. }
            | RemoteShareWork::Command { source } => source.payload.work.clone(),
        }
    }

    pub fn canonical_cwd(&self) -> Option<&str> {
        self.work
            .operation_source()
            .and_then(|source| source.payload.context.canonical_cwd.as_deref())
    }

    pub fn is_command(&self) -> bool {
        matches!(self.work, RemoteShareWork::Command { .. })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SignedRemoteShareIntent {
    pub intent: RemoteShareIntent,
    pub invocation: Signed<Invocation>,
}
impl SignedRemoteShareIntent {
    pub fn intent(&self) -> &RemoteShareIntent {
        &self.intent
    }
    pub fn invocation(&self) -> &Signed<Invocation> {
        &self.invocation
    }
    pub fn hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(self)
    }
}

macro_rules! share_policy_object {
    ($name:ident, $domain:literal) => {
        impl sealed::Sealed for $name {}
        impl SignedObject for $name {
            const DOMAIN: &'static str = $domain;
        }
    };
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteShareTaskAttestation {
    pub version: u16,
    pub project: ResourceRef,
    pub task: EventRef,
    pub task_id: String,
    pub conversation_id: String,
    pub author: String,
    pub sequence: u64,
    pub source_node_public_key: PublicKey,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
share_policy_object!(
    RemoteShareTaskAttestation,
    "remote-share-task-attestation-v2"
);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RemoteSharePolicyReadRequest {
    pub version: u16,
    pub intent: SignedRemoteShareIntent,
    pub reader_public_key: PublicKey,
    pub source_node_public_key: PublicKey,
    pub saved_pins: Vec<PolicyPin>,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
share_policy_object!(RemoteSharePolicyReadRequest, "remote-share-policy-read-v2");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteSharePolicySnapshot {
    pub version: u16,
    pub intent_hash: ObjectHash,
    pub request_hash: ObjectHash,
    pub snapshot_id: Nonce,
    pub reader_public_key: PublicKey,
    pub saved_pins_hash: ObjectHash,
    pub target_heads: Vec<PolicyStamp>,
    pub source_node_public_key: PublicKey,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
share_policy_object!(RemoteSharePolicySnapshot, "remote-share-policy-snapshot-v2");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteSharePolicyPageRequest {
    pub snapshot_id: Nonce,
    pub request_hash: ObjectHash,
    pub cursor: Option<ObjectHash>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteSharePolicyPage {
    pub version: u16,
    pub intent_hash: ObjectHash,
    pub request_hash: ObjectHash,
    pub snapshot_id: Nonce,
    pub target_heads_hash: ObjectHash,
    pub page_index: u64,
    pub request_cursor: Option<ObjectHash>,
    pub previous_page_hash: ObjectHash,
    pub entry: Option<JournalEntry>,
    pub next_cursor: Option<ObjectHash>,
    pub complete: bool,
}
share_policy_object!(RemoteSharePolicyPage, "remote-share-policy-page-v2");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteSharePolicyWitness {
    pub version: u16,
    pub intent_hash: ObjectHash,
    pub request_hash: ObjectHash,
    pub snapshot_id: Nonce,
    pub target_heads: Vec<PolicyStamp>,
    pub final_page_hash: ObjectHash,
    pub page_count: u64,
    pub entry_count: u64,
    pub snapshot_hash: ObjectHash,
    pub reader_public_key: PublicKey,
    pub source_node_public_key: PublicKey,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
share_policy_object!(RemoteSharePolicyWitness, "remote-share-policy-witness-v2");

fn carrier<T: Serialize>(value: &T) -> Result<()> {
    require(
        canonical(value)?.len() <= MAX_PROOF_BYTES,
        "source sharing carrier exceeds byte limit",
    )
}
fn validity(issued: u64, expires: u64, now: u64) -> Result<()> {
    validate_window(issued, expires, now)?;
    require(
        expires - issued <= INVOCATION_MAX_AGE_MS,
        "source sharing validity is too long",
    )
}
fn usage_finish(usage: &mut ExecutionProofUsage) -> Result<()> {
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()
}
fn intent_context(intent: &RemoteShareIntent, now: u64) -> Result<InvocationContext> {
    match &intent.work {
        RemoteShareWork::TaskSession {
            task_creation: signed,
            session_id,
            executor_kind,
            binding_generation,
        } => {
            let task = &signed.payload;
            signed.verify_signature()?;
            validity(task.issued_at_ms, task.expires_at_ms, now)?;
            require(
                signed.issuer == intent.source_node_public_key
                    && task.version == AUTH_VERSION
                    && task.project == intent.project
                    && task.source_node_public_key == intent.source_node_public_key
                    && text(&task.task_id, 512)
                    && text(&task.conversation_id, 512)
                    && text(&task.author, 512)
                    && text(session_id, 512)
                    && text(executor_kind, 128)
                    && *binding_generation > 0
                    && task.sequence > 0
                    && task.issued_at_ms <= intent.issued_at_ms
                    && task.expires_at_ms >= intent.expires_at_ms,
                "source sharing Task attestation changed its exact Project, host or validity",
            )?;
            Ok(InvocationContext {
                conversation: Some(task.conversation_id.clone()),
                task: Some(task.task.clone()),
                workspace_resource: Some(intent.project.resource.clone()),
                ..Default::default()
            })
        }
        RemoteShareWork::ConversationSession { source, .. }
        | RemoteShareWork::Command { source } => {
            use execution_work::{ConversationOpenIntent, ExecutionWorkIntent};
            source.verify_signature()?;
            let statement = &source.payload;
            validity(statement.issued_at_ms, statement.expires_at_ms, now)?;
            let target =
                intent
                    .target_public_key
                    .as_ref()
                    .ok_or(AuthorizationError::InvalidProof(
                        "operation sharing has no selected executor",
                    ))?;
            let context = &statement.context;
            let request = &statement.request;
            require(
                source.issuer == intent.source_node_public_key
                    && statement.version == AUTH_VERSION
                    && statement.project == intent.project
                    && statement.source_node_public_key == intent.source_node_public_key
                    && statement.issued_at_ms <= intent.issued_at_ms
                    && statement.expires_at_ms >= intent.expires_at_ms
                    && context.context.sdk_session.is_none()
                    && context.context.workspace_resource.as_ref()
                        == Some(&intent.project.resource)
                    && request["space_id"].as_str() == Some(intent.project.space.as_str())
                    && request["conversation_id"].as_str()
                        == context.context.conversation.as_deref()
                    && request.is_object()
                    && request["operation_id"]
                        .as_str()
                        .is_some_and(valid_operation_id)
                    && request["task_id"].is_null()
                    && request["task_ref"].is_null()
                    && request["binding"].is_null()
                    && request["target_node"].as_str()
                        == Some(
                            node_fingerprint(target.as_str())
                                .map_err(|_| {
                                    AuthorizationError::InvalidProof("invalid shared executor key")
                                })?
                                .as_str(),
                        ),
                "operation sharing changed its source, request, target or validity",
            )?;
            statement.work.validate_context(context, target)?;
            match (&intent.work, &statement.work) {
                (
                    RemoteShareWork::Command { .. },
                    ExecutionWorkIntent::Command {
                        request_hash,
                        canonical_cwd,
                    },
                ) => {
                    require(
                        *request_hash == ObjectHash::body(request)?
                            && request["session_id"].is_null()
                            && request["expected_revision"].as_u64() == Some(0)
                            && request["action"]["type"] == "command"
                            && request["action"]["command"]
                                .as_str()
                                .is_some_and(|command| text(command, MAX_PROOF_BYTES))
                            && request["action"]["timeout_secs"]
                                .as_u64()
                                .is_some_and(|seconds| seconds > 0)
                            && request["action"]["cwd"].as_str() == Some(canonical_cwd.as_str()),
                        "command sharing changed its complete request or claimed a session",
                    )?;
                }
                (
                    RemoteShareWork::ConversationSession { open: evidence, .. },
                    ExecutionWorkIntent::ConversationSession {
                        execution_session,
                        executor,
                        canonical_cwd,
                        open,
                        ..
                    },
                ) => {
                    require(
                        request["session_id"].as_str()
                            == Some(execution_session.session_id.as_str()),
                        "conversation sharing changed its execution session",
                    )?;
                    match open {
                        ConversationOpenIntent::Opening { operation_id } => require(
                            request["action"]["type"] == "open"
                                && request["operation_id"].as_str() == Some(operation_id.as_str())
                                && request["action"]["executor"].as_str()
                                    == Some(executor.as_str())
                                && request["action"]["cwd"].as_str()
                                    == Some(canonical_cwd.as_str())
                                && request["expected_revision"].as_u64() == Some(0)
                                && matches!(evidence, ConversationOpenEvidence::Opening),
                            "conversation sharing changed its original Open",
                        )?,
                        ConversationOpenIntent::Existing { open } => {
                            let ConversationOpenEvidence::Accepted { open: accepted } = evidence
                            else {
                                return Err(AuthorizationError::InvalidProof(
                                    "conversation sharing has no accepted Open",
                                ));
                            };
                            execution_handoff::verify_execution_handoff_receipt_signature(
                                &accepted.handoff,
                                &accepted.receipt,
                                &accepted.result,
                            )?;
                            let original = accepted.handoff.handoff.source_statement()?;
                            require(
                                matches!(
                                    &accepted.handoff.handoff.origin,
                                    execution_handoff::ExecutionOrigin::ConversationOpen
                                ) && original.project == intent.project
                                    && original.operation_id == open.operation_id
                                    && original.original_invocation_hash == open.invocation_hash
                                    && original.original_admission_hash == open.admission_hash
                                    && original.work.intent() == statement.work
                                    && accepted.handoff.handoff.target_public_key == *target
                                    && request["action"]["type"] != "open",
                                "conversation sharing changed its accepted Open or executor",
                            )?;
                        }
                    }
                }
                _ => {
                    return Err(AuthorizationError::InvalidProof(
                        "sharing work has another type",
                    ));
                }
            }
            Ok(InvocationContext {
                conversation: context.context.conversation.clone(),
                workspace_resource: Some(intent.project.resource.clone()),
                ..Default::default()
            })
        }
    }
}
fn intent_facts(intent: &RemoteShareIntent, now: u64) -> Result<RequestFacts> {
    Ok(RequestFacts {
        resource: intent.project.clone(),
        action: Action::ConversationRead,
        audience: Audience {
            service: ServiceId::from_key(&intent.source_node_public_key),
            device: DeviceId::from_key(&intent.source_node_public_key),
        },
        operation_id: intent.nonce.to_string(),
        body_hash: ObjectHash::body(intent)?,
        context: intent_context(intent, now)?,
    })
}

fn old_tuple(intent: &RemoteShareIntent, usage: &mut ExecutionProofUsage, now: u64) -> Result<()> {
    if intent.purpose == RemoteSharePurpose::Execution {
        require(
            intent.old_binding.is_none() && intent.old_execution_receipt.is_none(),
            "ordinary execution sharing cannot carry an old execution decision",
        )?;
        require(
            intent.execution_resource.is_some() == intent.target_public_key.is_some(),
            "execution sharing destination is incomplete",
        )?;
        return Ok(());
    }
    let RemoteShareWork::TaskSession {
        task_creation,
        session_id,
        executor_kind,
        binding_generation,
    } = &intent.work
    else {
        require(
            intent.purpose != RemoteSharePurpose::Rebind && intent.old_binding.is_none(),
            "only Task sharing can rebind or carry a Task binding",
        )?;
        let receipt =
            intent
                .old_execution_receipt
                .as_ref()
                .ok_or(AuthorizationError::InvalidProof(
                    "resolution sharing has no original executor receipt",
                ))?;
        let target = intent
            .target_public_key
            .as_ref()
            .ok_or(AuthorizationError::InvalidProof(
                "resolution sharing has no executor pin",
            ))?;
        let resource =
            intent
                .execution_resource
                .as_ref()
                .ok_or(AuthorizationError::InvalidProof(
                    "resolution sharing has no execution service",
                ))?;
        let source = intent
            .work
            .operation_source()
            .ok_or(AuthorizationError::InvalidProof(
                "resolution sharing has no operation source",
            ))?;
        receipt.verify_signature()?;
        source.payload.work.matches_work(&receipt.payload.work)?;
        require(
            receipt.issuer == *target
                && receipt.payload.original_space_id == intent.project.space
                && receipt.payload.execution_space_id == resource.space
                && receipt.payload.issued_at_ms <= now
                && source.payload.request["operation_id"].as_str()
                    == Some(receipt.payload.operation_id.as_str())
                && receipt.payload.target == DeviceId::from_key(target),
            "resolution sharing changed its original execution receipt",
        )?;
        usage.record_signed(receipt)?;
        return Ok(());
    };
    let task = &task_creation.payload;
    let binding = intent
        .old_binding
        .as_ref()
        .ok_or(AuthorizationError::InvalidProof(
            "existing execution binding is missing",
        ))?;
    let receipt = intent
        .old_execution_receipt
        .as_ref()
        .ok_or(AuthorizationError::InvalidProof(
            "existing executor receipt is missing",
        ))?;
    let execution = intent
        .execution_resource
        .as_ref()
        .ok_or(AuthorizationError::InvalidProof(
            "old execution service is missing",
        ))?;
    let target = intent
        .target_public_key
        .as_ref()
        .ok_or(AuthorizationError::InvalidProof(
            "old executor pin is missing",
        ))?;
    task_binding::verify_binding(binding, intent.source_node_public_key.as_str()).map_err(
        |_| AuthorizationError::InvalidProof("old binding has another Project authority"),
    )?;
    receipt.verify_signature()?;
    let snapshot = receipt
        .payload
        .session
        .as_ref()
        .ok_or(AuthorizationError::InvalidProof(
            "old receipt has no accepted session",
        ))?;
    require(
        receipt.issuer == *target
            && receipt.payload.version == AUTH_VERSION
            && receipt.payload.original_space_id == intent.project.space
            && receipt.payload.execution_space_id == execution.space
            && receipt.payload.target == DeviceId::from_key(target)
            && receipt
                .payload
                .work
                .execution_session()
                .is_some_and(|session| session.session_id == *session_id)
            && receipt.payload.work.binding_generation() == Some(*binding_generation)
            && *binding_generation == binding.binding.generation
            && receipt.payload.issued_at_ms <= now
            && snapshot.expires_at_ms > receipt.payload.issued_at_ms
            && snapshot.expires_at_ms - receipt.payload.issued_at_ms <= EXECUTION_LEASE_MS
            && snapshot.work == receipt.payload.work
            && snapshot.original_project == intent.project
            && snapshot.execution_resource == *execution
            && snapshot.work.task_ref() == Some(&task.task)
            && snapshot.work.binding_hash()
                == Some(
                    task_binding::binding_hash(&binding.binding)
                        .map_err(|_| AuthorizationError::InvalidProof("invalid old binding"))?
                        .as_str(),
                )
            && snapshot.work.binding_generation() == Some(binding.binding.generation)
            && snapshot.target == receipt.payload.target
            && snapshot.session_id == *session_id
            && snapshot.executor == *executor_kind
            && binding.binding.network_id == intent.project.space.as_str()
            && binding.binding.conversation_id == task.conversation_id
            && binding.binding.task_ref == task.task.as_str()
            && binding.binding.task_id == task.task_id
            && binding.binding.session_id == *session_id
            && binding.binding.target_node
                == node_fingerprint(target.as_str())
                    .map_err(|_| AuthorizationError::InvalidProof("invalid old executor pin"))?
            && binding.binding.open_spec["executor"].as_str() == Some(executor_kind.as_str())
            && binding.binding.open_spec["cwd"].as_str() == Some(snapshot.cwd.as_str()),
        "source sharing changed the exact old executor, binding or Task",
    )?;
    usage.record_binding(binding)?;
    usage.record_signed(receipt)?;
    Ok(())
}

fn check_pins(state: &VerifiedPolicySet, pins: &[PolicyPin], heads: &[PolicyStamp]) -> Result<()> {
    require(
        !pins.is_empty()
            && pins.len() <= MAX_POLICY_DOMAINS
            && pins.windows(2).all(|pair| pair[0].domain < pair[1].domain),
        "invalid independent source pins",
    )?;
    for head in heads {
        let pin = pins.iter().find(|pin| pin.domain == head.domain).ok_or(
            AuthorizationError::InvalidProof("source sharing requires independent saved trust"),
        )?;
        let root = match &head.domain {
            PolicyDomain::Account(id) => state.account(id).map(|doc| doc.genesis_root.clone()),
            PolicyDomain::Space(id) => state.space(id).map(|doc| match &doc.root {
                SpaceRoot::Personal { genesis_root, .. } | SpaceRoot::Team { genesis_root } => {
                    genesis_root.clone()
                }
            }),
        }
        .ok_or_else(|| AuthorizationError::NeedPolicySync(vec![head.clone()]))?;
        require(
            pin.genesis_root == root,
            "source sharing changed a saved genesis root",
        )?;
    }
    for pin in pins {
        if let Some(minimum) = &pin.minimum_head {
            if minimum.domain != pin.domain || !state.roots.contains_key(minimum) {
                return Err(AuthorizationError::NeedPolicySync(vec![minimum.clone()]));
            }
        }
    }
    Ok(())
}

fn verify_intent(
    state: &VerifiedPolicySet,
    signed: &SignedRemoteShareIntent,
    now: u64,
) -> Result<ExecutionProofUsage> {
    carrier(signed)?;
    let intent = &signed.intent;
    let invocation = &signed.invocation.payload;
    validity(intent.issued_at_ms, intent.expires_at_ms, now)?;
    intent_context(intent, now)?;
    require(
        intent.version == AUTH_VERSION
            && matches!(&invocation.actor,Actor::Account(account) if account==&intent.requester)
            && invocation.initiator == invocation.actor
            && invocation.initiating_grant.is_none()
            && invocation.device == intent.requester_device
            && invocation.presenter == intent.requester_device
            && intent.requester_device == DeviceId::from_key(&signed.invocation.issuer)
            && invocation.nonce == intent.nonce
            && invocation.not_before_ms == intent.issued_at_ms
            && invocation.expires_at_ms == intent.expires_at_ms
            && invocation.expected_policies == intent.policy_heads,
        "source share intent differs from its original Account, device, nonce or policy heads",
    )?;
    let facts = intent_facts(intent, now)?;
    let auth = state.verify_invocation(
        &signed.invocation,
        &facts,
        &TransportEvidence::DirectNoise {
            device: intent.requester_device.clone(),
        },
        now,
    )?;
    require(
        auth.dependencies() == intent.policy_heads,
        "source share policy heads changed",
    )?;
    let resource = state
        .resource(&intent.project.space, &intent.project.resource)
        .ok_or(AuthorizationError::InvalidProof(
            "source Project is missing",
        ))?;
    let source = state.verify_service_device(&intent.project.space, &facts.audience.device, now)?;
    require(
        resource.payload.kind == ResourceKind::Project
            && resource.payload.host == facts.audience.service
            && source.payload.public_key == intent.source_node_public_key,
        "source sharing is not served by the Project's current host",
    )?;
    check_pins(state, &intent.saved_pins, &intent.policy_heads)?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_authorization(&auth)?;
    usage.record_metadata(intent)?;
    match &intent.work {
        RemoteShareWork::TaskSession { task_creation, .. } => usage.record_signed(task_creation)?,
        RemoteShareWork::ConversationSession { source, .. }
        | RemoteShareWork::Command { source } => {
            usage.record_signed(source)?;
            if let RemoteShareWork::ConversationSession {
                open: ConversationOpenEvidence::Accepted { open: accepted },
                ..
            } = &intent.work
            {
                let verified =
                    state.verify_project_execution_source(&accepted.handoff.handoff, now)?;
                usage.merge(verified.proof_usage());
            }
        }
    }
    old_tuple(intent, &mut usage, now)?;
    usage_finish(&mut usage)?;
    Ok(usage)
}

pub struct VerifiedRemoteSharePolicyRead {
    heads: Vec<PolicyStamp>,
    usage: ExecutionProofUsage,
}
impl VerifiedRemoteSharePolicyRead {
    pub fn heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
    pub fn domains(&self) -> BTreeSet<PolicyDomain> {
        self.heads.iter().map(|head| head.domain.clone()).collect()
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
}

pub fn verify_remote_share_policy_read(
    state: &VerifiedPolicySet,
    request: &Signed<RemoteSharePolicyReadRequest>,
    local_node: &PublicKey,
    noise: &PublicKey,
    now: u64,
) -> Result<VerifiedRemoteSharePolicyRead> {
    let _scope = ScopedSignatureVerification::new();
    carrier(request)?;
    let value = &request.payload;
    validity(value.issued_at_ms, value.expires_at_ms, now)?;
    require(
        value.version == AUTH_VERSION
            && request.issuer == *noise
            && value.reader_public_key == *noise
            && value.source_node_public_key == *local_node
            && value.intent.intent.source_node_public_key == *local_node
            && value.expires_at_ms <= value.intent.intent.expires_at_ms,
        "source sharing read changed its true Noise reader, source or original expiry",
    )?;
    request.verify_signature()?;
    let mut usage = verify_intent(state, &value.intent, now)?;
    check_pins(state, &value.saved_pins, &value.intent.intent.policy_heads)?;
    usage.record_signed(request)?;
    usage_finish(&mut usage)?;
    Ok(VerifiedRemoteSharePolicyRead {
        heads: value.intent.intent.policy_heads.clone(),
        usage,
    })
}

#[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=buildRemoteShareIntent))]
pub fn build_remote_share_intent(
    intent_json: &str,
    source: &VerifiedClientAuthorization,
    signer: &Identity,
    now_ms: f64,
) -> crate::Result<String> {
    let _scope = ScopedSignatureVerification::new();
    let build = || -> Result<_> {
        let snapshot = require_confirmed_client_policy(source)?;
        let now = time(now_ms)?;
        let mut intent: RemoteShareIntent = parse(intent_json, MAX_PROOF_BYTES)?;
        require(
            source.scope.device_id == DeviceId::from_key(&PublicKey::of(signer))
                && source.scope.space_id == intent.project.space
                && scope_covers(&source.grant_scope, &intent.project)
                && source.actions.contains(&Action::ConversationRead),
            "source share intent requires this genuine Account reader and selected Project",
        )?;
        intent.version = AUTH_VERSION;
        intent.requester = source.scope.account_id.clone();
        intent.requester_device = source.scope.device_id.clone();
        intent.source_node_public_key = source.audience_public_key.clone();
        intent.saved_pins = snapshot.pins().to_vec();
        intent.policy_heads = source.heads.clone();
        intent.nonce = Nonce::random()?;
        intent.issued_at_ms = now;
        validity(now, intent.expires_at_ms, now)?;
        let facts = intent_facts(&intent, now)?;
        require(
            context_tighter(&source.constraints.context, &facts.context),
            "source sharing escaped the current Task or conversation context",
        )?;
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Account(source.scope.account_id.clone()),
                device: source.scope.device_id.clone(),
                initiator: Actor::Account(source.scope.account_id.clone()),
                initiating_grant: None,
                resource: facts.resource,
                action: facts.action,
                audience: facts.audience,
                presenter: source.scope.device_id.clone(),
                operation_id: facts.operation_id,
                body_hash: facts.body_hash,
                grant_leaf: source.grant_leaf.clone(),
                expected_policies: source.heads.clone(),
                context: facts.context,
                nonce: intent.nonce.clone(),
                not_before_ms: now,
                expires_at_ms: intent.expires_at_ms,
            },
            signer,
        )?;
        let signed = SignedRemoteShareIntent { intent, invocation };
        verify_intent(snapshot.state(), &signed, now)?;
        Ok(signed)
    };
    encode(&build().map_err(|error| error.to_string())?)
}

#[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=buildRemoteSharePolicyReadRequest))]
pub fn build_remote_share_policy_read_request(
    intent_json: &str,
    reader: &Identity,
    independent_pins_json: &str,
    expected_source_node_public_key: &str,
    now_ms: f64,
    expires_at_ms: f64,
) -> crate::Result<String> {
    let _scope = ScopedSignatureVerification::new();
    let build = || -> Result<_> {
        let intent: SignedRemoteShareIntent = parse(intent_json, MAX_PROOF_BYTES)?;
        intent.invocation.verify_signature()?;
        let pins: Vec<PolicyPin> = parse(independent_pins_json, MAX_PROOF_BYTES)?;
        let node = PublicKey::parse(expected_source_node_public_key)?;
        let now = time(now_ms)?;
        let expires = time(expires_at_ms)?;
        validity(now, expires, now)?;
        validity(intent.intent.issued_at_ms, intent.intent.expires_at_ms, now)?;
        require(
            node == intent.intent.source_node_public_key
                && expires <= intent.intent.expires_at_ms
                && !pins.is_empty()
                && pins.len() <= MAX_POLICY_DOMAINS
                && pins.windows(2).all(|pair| pair[0].domain < pair[1].domain),
            "source sharing read changed independently saved source, pins or validity",
        )?;
        let signed = Signed::sign(
            RemoteSharePolicyReadRequest {
                version: AUTH_VERSION,
                intent,
                reader_public_key: PublicKey::of(reader),
                source_node_public_key: node,
                saved_pins: pins,
                nonce: Nonce::random()?,
                issued_at_ms: now,
                expires_at_ms: expires,
            },
            reader,
        )?;
        carrier(&signed)?;
        Ok(signed)
    };
    encode(&build().map_err(|error| error.to_string())?)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct RemoteSharePolicyImport {
    request: Signed<RemoteSharePolicyReadRequest>,
    snapshot: Signed<RemoteSharePolicySnapshot>,
    import: BoundedPolicyImport,
    previous_page_hash: ObjectHash,
    next_cursor: Option<ObjectHash>,
    last_page: Option<Signed<RemoteSharePolicyPage>>,
    page_count: u64,
    entry_count: u64,
    complete: bool,
    usage: ExecutionProofUsage,
}

#[derive(Clone)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedRemoteShareRequest {
    original: SignedRemoteShareIntent,
    snapshot: VerifiedPolicySnapshot,
    read_request: Signed<RemoteSharePolicyReadRequest>,
    header: Signed<RemoteSharePolicySnapshot>,
    witness: Signed<RemoteSharePolicyWitness>,
    reader_public_key: PublicKey,
    usage: ExecutionProofUsage,
}
impl VerifiedRemoteShareRequest {
    pub(crate) fn snapshot(&self) -> &VerifiedPolicySnapshot {
        &self.snapshot
    }
    pub(crate) fn original(&self) -> &SignedRemoteShareIntent {
        &self.original
    }
    pub(crate) fn reader_public_key(&self) -> &PublicKey {
        &self.reader_public_key
    }
    pub(crate) fn revalidate(&self, now: u64) -> Result<ExecutionProofUsage> {
        validity(
            self.original.intent.issued_at_ms,
            self.original.intent.expires_at_ms,
            now,
        )?;
        validity(
            self.read_request.payload.issued_at_ms,
            self.read_request.payload.expires_at_ms,
            now,
        )?;
        validity(
            self.header.payload.issued_at_ms,
            self.header.payload.expires_at_ms,
            now,
        )?;
        validity(
            self.witness.payload.issued_at_ms,
            self.witness.payload.expires_at_ms,
            now,
        )?;
        verify_intent(self.snapshot.state(), &self.original, now)
    }
}

#[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=beginRemoteSharePolicyImport))]
pub fn begin_remote_share_policy_import(
    request_json: &str,
    snapshot_json: &str,
    independent_pins_json: &str,
    expected_source_node_public_key: &str,
    now_ms: f64,
) -> crate::Result<RemoteSharePolicyImport> {
    let _scope = ScopedSignatureVerification::new();
    let begin = || -> Result<_> {
        let request: Signed<RemoteSharePolicyReadRequest> = parse(request_json, MAX_PROOF_BYTES)?;
        let snapshot: Signed<RemoteSharePolicySnapshot> = parse(snapshot_json, MAX_PROOF_BYTES)?;
        require(
            canonical(&(&request, &snapshot))?.len() <= MAX_PROOF_BYTES,
            "source sharing request and snapshot exceed carrier byte limit",
        )?;
        let independent: Vec<PolicyPin> = parse(independent_pins_json, MAX_PROOF_BYTES)?;
        let node = PublicKey::parse(expected_source_node_public_key)?;
        let now = time(now_ms)?;
        request.verify_signature()?;
        snapshot.verify_signature()?;
        validity(
            request.payload.issued_at_ms,
            request.payload.expires_at_ms,
            now,
        )?;
        validity(
            snapshot.payload.issued_at_ms,
            snapshot.payload.expires_at_ms,
            now,
        )?;
        let head = &snapshot.payload;
        require(
            request.payload.version == AUTH_VERSION
                && head.version == AUTH_VERSION
                && request.payload.saved_pins == independent
                && !independent.is_empty()
                && independent.len() <= MAX_POLICY_DOMAINS
                && independent
                    .windows(2)
                    .all(|pair| pair[0].domain < pair[1].domain)
                && request.issuer == request.payload.reader_public_key
                && request.payload.source_node_public_key == node
                && snapshot.issuer == node
                && head.source_node_public_key == node
                && head.reader_public_key == request.issuer
                && request.payload.intent.intent.source_node_public_key == node
                && head.intent_hash == request.payload.intent.hash()?
                && head.request_hash == request.hash()?
                && head.saved_pins_hash == ObjectHash::body(&independent)?
                && head.target_heads == request.payload.intent.intent.policy_heads
                && head.expires_at_ms <= request.payload.expires_at_ms
                && request.payload.expires_at_ms <= request.payload.intent.intent.expires_at_ms
                && !head.target_heads.is_empty()
                && head.target_heads.len() <= MAX_POLICY_DOMAINS
                && head
                    .target_heads
                    .windows(2)
                    .all(|pair| pair[0].domain < pair[1].domain),
            "source sharing snapshot changed original intent, true reader, saved pins or exact heads",
        )?;
        let pins = independent
            .into_iter()
            .filter(|pin| {
                head.target_heads
                    .iter()
                    .any(|head| head.domain == pin.domain)
            })
            .collect::<Vec<_>>();
        require(
            pins.len() == head.target_heads.len(),
            "source sharing introduced an independently untrusted domain",
        )?;
        let required = pins
            .iter()
            .filter_map(|pin| pin.minimum_head.clone())
            .collect();
        let import = BoundedPolicyImport::new(pins, required)?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_signed(&request)?;
        usage.record_signed(&snapshot)?;
        usage_finish(&mut usage)?;
        Ok(RemoteSharePolicyImport {
            request,
            previous_page_hash: snapshot.hash()?,
            snapshot,
            import,
            next_cursor: None,
            last_page: None,
            page_count: 0,
            entry_count: 0,
            complete: false,
            usage,
        })
    };
    begin().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl RemoteSharePolicyImport {
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=pageRequestJson))]
    pub fn page_request_json(&self) -> crate::Result<String> {
        encode(&RemoteSharePolicyPageRequest {
            snapshot_id: self.snapshot.payload.snapshot_id.clone(),
            request_hash: self.request.hash().map_err(|error| error.to_string())?,
            cursor: self.next_cursor.clone(),
        })
    }
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        encode(&self.usage.counts())
    }
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=appendPage))]
    pub fn append_page(&mut self, page_json: &str, now_ms: f64) -> crate::Result<()> {
        let _scope = ScopedSignatureVerification::new();
        let mut append = || -> Result<()> {
            let now = time(now_ms)?;
            validity(
                self.snapshot.payload.issued_at_ms,
                self.snapshot.payload.expires_at_ms,
                now,
            )?;
            let signed: Signed<RemoteSharePolicyPage> = parse(page_json, MAX_PROOF_BYTES)?;
            signed.verify_signature()?;
            let page = &signed.payload;
            require(
                signed.issuer == self.snapshot.issuer
                    && page.version == AUTH_VERSION
                    && page.intent_hash == self.request.payload.intent.hash()?
                    && page.request_hash == self.request.hash()?
                    && page.snapshot_id == self.snapshot.payload.snapshot_id
                    && page.target_heads_hash
                        == ObjectHash::body(&self.snapshot.payload.target_heads)?,
                "source sharing page changed its original intent, reader request or exact snapshot",
            )?;
            if self.last_page.as_ref() == Some(&signed) {
                return Ok(());
            }
            require(
                !self.complete
                    && page.page_index == self.page_count
                    && page.request_cursor == self.next_cursor
                    && page.previous_page_hash == self.previous_page_hash
                    && page.complete == page.next_cursor.is_none(),
                "source sharing page changed its signed cursor or causal predecessor",
            )?;
            let mut candidate = self.import.clone();
            let mut usage = ExecutionProofUsage::default();
            usage.record_signed(&signed)?;
            if let Some(entry) = &page.entry {
                usage.merge(&crate::authorization::policy_proof::policy_entry_usage(
                    self.import.state(),
                    entry,
                )?);
                usage.enforce_limits()?;
                candidate.apply_entry(entry.clone())?;
            }
            usage_finish(&mut usage)?;
            self.import = candidate;
            self.previous_page_hash = signed.hash()?;
            self.next_cursor = page.next_cursor.clone();
            self.complete = page.complete;
            self.page_count = self
                .page_count
                .checked_add(1)
                .ok_or(AuthorizationError::BudgetExceeded)?;
            self.entry_count = self
                .entry_count
                .checked_add(u64::from(page.entry.is_some()))
                .ok_or(AuthorizationError::BudgetExceeded)?;
            self.last_page = Some(signed);
            self.usage = usage;
            Ok(())
        };
        append().map_err(|error| error.to_string())
    }
    pub fn finish(
        self,
        witness_json: &str,
        now_ms: f64,
    ) -> crate::Result<VerifiedRemoteShareRequest> {
        let _scope = ScopedSignatureVerification::new();
        let finish = || -> Result<_> {
            let now = time(now_ms)?;
            require(self.complete, "source sharing policy stream is incomplete")?;
            let signed: Signed<RemoteSharePolicyWitness> = parse(witness_json, MAX_PROOF_BYTES)?;
            signed.verify_signature()?;
            let witness = &signed.payload;
            validity(witness.issued_at_ms, witness.expires_at_ms, now)?;
            validity(
                self.snapshot.payload.issued_at_ms,
                self.snapshot.payload.expires_at_ms,
                now,
            )?;
            require(
                signed.issuer == self.snapshot.issuer
                    && witness.version == AUTH_VERSION
                    && witness.intent_hash == self.request.payload.intent.hash()?
                    && witness.request_hash == self.request.hash()?
                    && witness.snapshot_id == self.snapshot.payload.snapshot_id
                    && witness.target_heads == self.snapshot.payload.target_heads
                    && witness.final_page_hash == self.previous_page_hash
                    && witness.page_count == self.page_count
                    && witness.entry_count == self.entry_count
                    && witness.snapshot_hash == self.snapshot.hash()?
                    && witness.reader_public_key == self.request.issuer
                    && witness.source_node_public_key == self.snapshot.issuer
                    && witness.expires_at_ms <= self.snapshot.payload.expires_at_ms,
                "source sharing witness changed its original intent, reader, complete stream or exact heads",
            )?;
            let snapshot = self.import.finish(&witness.target_heads)?;
            let mut usage = verify_intent(snapshot.state(), &self.request.payload.intent, now)?;
            usage.record_signed(&signed)?;
            usage.record_signed(&self.snapshot)?;
            usage_finish(&mut usage)?;
            Ok(VerifiedRemoteShareRequest {
                original: self.request.payload.intent.clone(),
                snapshot,
                read_request: self.request.clone(),
                header: self.snapshot,
                reader_public_key: self.request.issuer,
                witness: signed,
                usage,
            })
        };
        finish().map_err(|error| error.to_string())
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedRemoteShareRequest {
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=summaryJson))]
    pub fn summary_json(&self) -> crate::Result<String> {
        let intent = &self.original.intent;
        let context = intent_context(intent, self.header.payload.issued_at_ms)
            .map_err(|error| error.to_string())?;
        let (task_id, session_id, executor, request) = match &intent.work {
            RemoteShareWork::TaskSession {
                task_creation,
                session_id,
                executor_kind,
                ..
            } => (
                Some(task_creation.payload.task_id.as_str()),
                Some(session_id.as_str()),
                Some(executor_kind.as_str()),
                None,
            ),
            RemoteShareWork::ConversationSession { source, .. }
            | RemoteShareWork::Command { source } => (
                None,
                source
                    .payload
                    .context
                    .context
                    .execution_session
                    .as_ref()
                    .map(|s| s.session_id.as_str()),
                source.payload.context.context.executor_kind.as_deref(),
                Some(&source.payload.request),
            ),
        };
        encode(&serde_json::json!({
            "purpose":intent.purpose,"requesterAccountId":intent.requester,
            "requesterDevice":intent.requester_device,"project":intent.project,
            "work":intent.work,"taskRef":context.task,"taskId":task_id,
            "conversationId":context.conversation,"sessionId":session_id,
            "executorKind":executor,"request":request,"executionResource":intent.execution_resource,
            "targetPublicKey":intent.target_public_key,"intentHash":self.original.hash().map_err(|error|error.to_string())?,
            "expiresAtMs":intent.expires_at_ms,
        }))
    }
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        encode(&self.usage.counts())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = revalidate))]
    pub fn revalidate_request(
        &self,
        latest_independent_pins_json: &str,
        now_ms: f64,
    ) -> crate::Result<VerifiedRemoteShareRequest> {
        let _scope = ScopedSignatureVerification::new();
        (|| -> Result<_> {
            let pins: Vec<PolicyPin> = parse(latest_independent_pins_json, MAX_PROOF_BYTES)?;
            let mut verified = self.clone();
            verified.snapshot = self.snapshot.with_independent_pins(&pins)?;
            verified.usage = verified.revalidate(time(now_ms)?)?;
            Ok(verified)
        })()
        .map_err(|error| error.to_string())
    }
}
