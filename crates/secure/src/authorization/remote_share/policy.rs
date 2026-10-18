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
    pub task_creation: Signed<RemoteShareTaskAttestation>,
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
fn intent_task(intent: &RemoteShareIntent, now: u64) -> Result<&RemoteShareTaskAttestation> {
    let signed = &intent.task_creation;
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
            && task.sequence > 0
            && task.issued_at_ms <= intent.issued_at_ms
            && task.expires_at_ms >= intent.expires_at_ms,
        "source sharing Task attestation changed its exact Project, host or validity",
    )?;
    Ok(task)
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
            conversation: Some(task.conversation_id.clone()),
            task: Some(task.task.clone()),
            workspace_resource: Some(intent.project.resource.clone()),
            ..Default::default()
        },
    })
}

fn old_tuple(
    intent: &RemoteShareIntent,
    task: &RemoteShareTaskAttestation,
    usage: &mut ExecutionProofUsage,
    now: u64,
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
    require(
        receipt.issuer == *target
            && receipt.payload.version == AUTH_VERSION
            && receipt.payload.original_space_id == intent.project.space
            && receipt.payload.execution_space_id == execution.space
            && receipt.payload.target == DeviceId::from_key(target)
            && receipt.payload.session_id == intent.session_id
            && receipt.payload.binding_generation == binding.binding.generation
            && receipt.payload.issued_at_ms <= now
            && snapshot.expires_at_ms > receipt.payload.issued_at_ms
            && snapshot.expires_at_ms - receipt.payload.issued_at_ms <= EXECUTION_LEASE_MS
            && snapshot.original_project == intent.project
            && snapshot.execution_resource == *execution
            && snapshot.task == task.task
            && snapshot.binding_hash
                == task_binding::binding_hash(&binding.binding)
                    .map_err(|_| AuthorizationError::InvalidProof("invalid old binding"))?
            && snapshot.binding_generation == binding.binding.generation
            && snapshot.target == receipt.payload.target
            && snapshot.session_id == intent.session_id
            && snapshot.executor == intent.executor_kind
            && binding.binding.network_id == intent.project.space.as_str()
            && binding.binding.conversation_id == task.conversation_id
            && binding.binding.task_ref == task.task.as_str()
            && binding.binding.task_id == task.task_id
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
    usage.record_signed(&intent.task_creation)?;
    old_tuple(intent, &task, &mut usage, now)?;
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
        let task = intent_task(intent, self.header.payload.issued_at_ms)
            .map_err(|error| error.to_string())?;
        encode(
            &serde_json::json!({"purpose":intent.purpose,"requesterAccountId":intent.requester,
            "requesterDevice":intent.requester_device,"project":intent.project,
            "taskRef":task.task,"taskId":task.task_id,
            "conversationId":task.conversation_id,"sessionId":intent.session_id,
            "executorKind":intent.executor_kind,"executionResource":intent.execution_resource,
            "targetPublicKey":intent.target_public_key,"intentHash":self.original.hash().map_err(|error|error.to_string())?,
            "expiresAtMs":intent.expires_at_ms}),
        )
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
