use super::*;
use crate::authorization::{
    client_policy_import::require_confirmed_client_policy,
    execution_handoff::{ExecutionHandoffReceipt, ExecutionProofUsage},
    policy_import::{BoundedPolicyImport, VerifiedPolicySnapshot},
    task_binding::SignedBinding,
};
use crate::signature_verification::{ScopedSignatureVerification, SignatureVerification};

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
    pub task_creation: SignedEvent,
    pub source_node_public_key: PublicKey,
    pub requester: AccountId,
    pub requester_device: DeviceId,
    pub saved_pins: Vec<PolicyPin>,
    pub policy_heads: Vec<PolicyStamp>,
    pub session_id: String,
    pub executor_kind: String,
    pub execution_resource: Option<ResourceRef>,
    pub target_public_key: Option<PublicKey>,
    pub old_binding: Option<SignedBinding>,
    pub old_execution_receipt: Option<Signed<ExecutionHandoffReceipt>>,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
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
fn intent_task(intent: &RemoteShareIntent, now: u64) -> Result<Event> {
    let envelope = &intent.task_creation;
    let event: Event = serde_json::from_str(&envelope.canonical_body)
        .map_err(|_| AuthorizationError::InvalidProof("invalid sharing task"))?;
    require(
        hex::encode(Sha256::digest(envelope.canonical_body.as_bytes())) == envelope.hash
            && canonical(&event)? == envelope.canonical_body.as_bytes()
            && event.network_id == intent.project.space.as_str()
            && event.resource_id == intent.project.resource
            && matches!(event.body, EventBody::TaskCreated { .. })
            && event.created_at_ms >= 0
            && event.created_at_ms as u64 <= now
            && event.blobs.is_empty(),
        "source sharing task changed its exact Project or canonical event",
    )?;
    Ok(event)
}
fn intent_facts(intent: &RemoteShareIntent, now: u64) -> Result<RequestFacts> {
    let task = intent_task(intent, now)?;
    Ok(RequestFacts {
        resource: intent.project.clone(),
        action: Action::ConversationRead,
        audience: Audience {
            service: ServiceId::from_key(&intent.source_node_public_key),
            device: DeviceId::from_key(&intent.source_node_public_key),
        },
        operation_id: intent.nonce.to_string(),
        body_hash: ObjectHash::body(intent)?,
        context: InvocationContext {
            conversation: Some(task.scope_id),
            task: Some(EventRef::parse(&intent.task_creation.hash)?),
            workspace_resource: Some(intent.project.resource.clone()),
            ..Default::default()
        },
    })
}

fn old_tuple(
    intent: &RemoteShareIntent,
    task: &Event,
    usage: &mut ExecutionProofUsage,
) -> Result<()> {
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
    let EventBody::TaskCreated { task_id, .. } = &task.body else {
        return Err(AuthorizationError::PolicyConflict);
    };
    require(
        receipt.issuer == *target
            && receipt.payload.version == AUTH_VERSION
            && receipt.payload.original_space_id == intent.project.space
            && receipt.payload.execution_space_id == execution.space
            && receipt.payload.target == DeviceId::from_key(target)
            && receipt.payload.session_id == intent.session_id
            && receipt.payload.binding_generation == binding.binding.generation
            && snapshot.original_project == intent.project
            && snapshot.execution_resource == *execution
            && snapshot.task.as_str() == intent.task_creation.hash
            && snapshot.binding_hash
                == task_binding::binding_hash(&binding.binding)
                    .map_err(|_| AuthorizationError::InvalidProof("invalid old binding"))?
            && snapshot.binding_generation == binding.binding.generation
            && snapshot.target == receipt.payload.target
            && snapshot.session_id == intent.session_id
            && snapshot.executor == intent.executor_kind
            && binding.binding.network_id == intent.project.space.as_str()
            && binding.binding.conversation_id == task.scope_id
            && binding.binding.task_ref == intent.task_creation.hash
            && binding.binding.task_id == *task_id
            && binding.binding.session_id == intent.session_id
            && binding.binding.target_node
                == node_fingerprint(target.as_str())
                    .map_err(|_| AuthorizationError::InvalidProof("invalid old executor pin"))?
            && binding.binding.open_spec["executor"].as_str()
                == Some(intent.executor_kind.as_str())
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
    let task = intent_task(intent, now)?;
    require(
        intent.version == AUTH_VERSION
            && text(&intent.session_id, 512)
            && text(&intent.executor_kind, 128)
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
    old_tuple(intent, &task, &mut usage)?;
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
