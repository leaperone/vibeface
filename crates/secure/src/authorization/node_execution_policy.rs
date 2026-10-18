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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
pub enum TaskExecutionPolicyRequest {
    Account(remote_execution_policy::RemoteExecutionPolicyReadRequest),
    Node(NodeExecutionPolicyReadRequest),
}
impl TaskExecutionPolicyRequest {
    pub fn account_request(
        &self,
    ) -> Option<&remote_execution_policy::RemoteExecutionPolicyReadRequest> {
        match self {
            Self::Account(value) => Some(value),
            Self::Node(_) => None,
        }
    }
    pub fn node_request(&self) -> Option<&NodeExecutionPolicyReadRequest> {
        match self {
            Self::Account(_) => None,
            Self::Node(value) => Some(value),
        }
    }
    pub fn source_project(&self) -> &ResourceRef {
        match self {
            Self::Account(value) => &value.intent.payload.source_selection.resource,
            Self::Node(value) => value.source_project(),
        }
    }
    pub fn source_node(&self) -> &PublicKey {
        match self {
            Self::Account(value) => &value.delegation.issuer,
            Self::Node(value) => value.issuer(),
        }
    }
    pub fn selection(&self) -> Result<RemoteExecutionPolicySelection> {
        match self {
            Self::Account(value) => Ok(value.intent.payload.selection.clone()),
            Self::Node(value) => value.selection(),
        }
    }
    pub fn context(&self) -> Result<prepare::PrepareContext> {
        match self {
            Self::Account(value) => Ok(value.intent.payload.context.clone()),
            Self::Node(value) => value.context(),
        }
    }
    pub fn purpose(&self) -> RemoteExecutionPolicyPurpose {
        match self {
            Self::Account(value) => value.intent.payload.purpose,
            Self::Node(value) => value.purpose(),
        }
    }
    pub fn expected_action(&self) -> Action {
        match self {
            Self::Account(value) => value.intent.payload.expected_action,
            Self::Node(value) => value.expected_action(),
        }
    }
    pub fn execution_pins(&self) -> &[PolicyPin] {
        match self {
            Self::Account(value) => &value.intent.payload.execution_pins,
            Self::Node(value) => value.execution_pins(),
        }
    }
    pub fn history(&self) -> Option<&remote_execution_history::RemoteExecutionHistoryReference> {
        match self {
            Self::Account(value) => value.intent.payload.history.as_ref(),
            Self::Node(value) => value.history(),
        }
    }
    pub fn request_hash(&self) -> Result<ObjectHash> {
        match self {
            Self::Account(value) => ObjectHash::body(value),
            Self::Node(value) => ObjectHash::body(value),
        }
    }
    pub fn expires_at_ms(&self) -> u64 {
        match self {
            Self::Account(value) => value
                .intent
                .payload
                .expires_at_ms
                .min(value.delegation.payload.expires_at_ms),
            Self::Node(value) => value.expires_at_ms(),
        }
    }
    fn check_window(&self, now: u64) -> Result<()> {
        match self {
            Self::Account(value) => {
                read_window(
                    value.intent.payload.issued_at_ms,
                    value.intent.payload.expires_at_ms,
                    now,
                )?;
                read_window(
                    value.delegation.payload.issued_at_ms,
                    value.delegation.payload.expires_at_ms,
                    now,
                )
            }
            Self::Node(value) => read_window(value.issued_at_ms(), value.expires_at_ms(), now),
        }
    }
}

#[derive(Clone)]
pub struct VerifiedTaskPolicySource {
    snapshot: policy_import::VerifiedPolicySnapshot,
    primary_pins: Vec<PolicyPin>,
    intent: Signed<RemoteExecutionPolicyReadIntent>,
    verified_at_ms: u64,
    expires_at_ms: u64,
    usage: ExecutionProofUsage,
    retained: bool,
}

pub fn task_policy_source_from_current(
    current: &VerifiedRemoteExecutionPolicySourceIntent,
    primary_pins: &[PolicyPin],
    now: u64,
) -> Result<VerifiedTaskPolicySource> {
    require(
        current.verified_at_ms() == now,
        "native policy source requires current local verification",
    )?;
    let snapshot = policy_import::native_execution_source_snapshot(current, primary_pins)?;
    let state = &current.authorization().policy_snapshot;
    for pin in primary_pins {
        let genesis = match &pin.domain {
            PolicyDomain::Account(id) => state.account(id).map(|value| value.genesis_root.clone()),
            PolicyDomain::Space(id) => state.space(id).map(|value| match &value.root {
                SpaceRoot::Personal { genesis_root, .. } | SpaceRoot::Team { genesis_root } => {
                    genesis_root.clone()
                }
            }),
        }
        .ok_or(AuthorizationError::InvalidProof(
            "native primary pin has no verified local policy",
        ))?;
        require(
            genesis == pin.genesis_root,
            "native primary pin changed its verified genesis",
        )?;
        if let Some(minimum) = &pin.minimum_head {
            require(
                minimum.domain == pin.domain,
                "native primary minimum belongs to another domain",
            )?;
            if !state.roots.contains_key(minimum) {
                return Err(AuthorizationError::NeedPolicySync(vec![minimum.clone()]));
            }
        }
    }
    Ok(VerifiedTaskPolicySource {
        snapshot,
        primary_pins: primary_pins.to_vec(),
        intent: current.intent().clone(),
        verified_at_ms: now,
        expires_at_ms: current
            .intent()
            .payload
            .expires_at_ms
            .min(current.authorization().budget.expires_at_ms),
        usage: current.proof_usage().clone(),
        retained: false,
    })
}

pub fn task_policy_source_from_retained(
    trust: &VerifiedTaskExecutionTrust,
    primary: &VerifiedPolicySet,
    primary_pins: &[PolicyPin],
    request: &remote_execution_policy::RemoteExecutionPolicyReadRequest,
    original_clock: u64,
) -> Result<VerifiedTaskPolicySource> {
    let _signatures = ScopedSignatureVerification::new();
    request.intent.verify_signature()?;
    request.delegation.verify_signature()?;
    let typed = TaskExecutionPolicyRequest::Account(request.clone());
    check_task_read_scope(trust, &typed)?;
    typed.check_window(original_clock)?;
    require(
        request.delegation.payload.intent_hash == request.intent.hash()?
            && request.delegation.issuer == trust.approval.issuer
            && request.delegation.payload.source_node_public_key == trust.approval.issuer,
        "retained policy source changed its original local delegation",
    )?;
    let known: Vec<_> = request
        .intent
        .payload
        .source_heads
        .iter()
        .filter(|head| primary.head(&head.domain).is_some())
        .cloned()
        .collect();
    let historical = remote_resolution_history::at_heads(primary, &known)?;
    let current = remote_execution_policy::verify_native_execution_policy_source_claims(
        &historical,
        &request.intent,
        &trust.approval.issuer,
        original_clock,
    )?;
    let original_pins: Vec<_> = primary_pins
        .iter()
        .cloned()
        .map(|mut pin| {
            pin.minimum_head = known.iter().find(|head| head.domain == pin.domain).cloned();
            pin
        })
        .collect();
    let mut source = task_policy_source_from_current(&current, &original_pins, original_clock)?;
    source.retained = true;
    Ok(source)
}

fn stronger_minimum(
    left: Option<PolicyStamp>,
    right: Option<&PolicyStamp>,
) -> Result<Option<PolicyStamp>> {
    let Some(right) = right else { return Ok(left) };
    let Some(left) = left else {
        return Ok(Some(right.clone()));
    };
    if (left.authority_epoch, left.sequence) == (right.authority_epoch, right.sequence) {
        require(
            left == *right,
            "task policy minimum conflicts at the same version",
        )?;
        return Ok(Some(left));
    }
    Ok(Some(
        if (left.authority_epoch, left.sequence) > (right.authority_epoch, right.sequence) {
            left
        } else {
            right.clone()
        },
    ))
}

fn effective_task_pins(
    trust: &VerifiedTaskExecutionTrust,
    source: &VerifiedTaskPolicySource,
    request: &TaskExecutionPolicyRequest,
    floors: &[PolicyPin],
) -> Result<Vec<PolicyPin>> {
    validate_pins(request.execution_pins())?;
    let mut result = Vec::new();
    for requested in request.execution_pins() {
        let approved = trust
            .pins()
            .iter()
            .find(|pin| pin.domain == requested.domain);
        let primary = source
            .primary_pins
            .iter()
            .find(|pin| pin.domain == requested.domain);
        require(
            approved.is_some() || primary.is_some(),
            "task policy introduced an independently unapproved root",
        )?;
        let mut pin = requested.clone();
        for saved in approved
            .into_iter()
            .chain(primary)
            .chain(floors.iter().filter(|pin| pin.domain == requested.domain))
        {
            require(
                saved.genesis_root == requested.genesis_root,
                "task policy changed an approved genesis",
            )?;
            if let Some(minimum) = &saved.minimum_head {
                require(
                    minimum.domain == saved.domain,
                    "task policy floor belongs to another domain",
                )?;
            }
            pin.minimum_head = stronger_minimum(pin.minimum_head, saved.minimum_head.as_ref())?;
        }
        result.push(pin);
    }
    Ok(result)
}

fn check_task_read_scope(
    trust: &VerifiedTaskExecutionTrust,
    request: &TaskExecutionPolicyRequest,
) -> Result<()> {
    let original = &trust.original_intent.payload;
    let context = request.context()?;
    let selection = request.selection()?;
    let original_context = &original.context.context;
    let actual = &context.context;
    require(
        request.source_node() == &trust.approval.issuer
            && request.source_project() == &original.source_selection.resource
            && selection.execution_resource == original.selection.execution_resource
            && selection.target_public_key == original.selection.target_public_key
            && actual.task == original_context.task
            && actual.conversation == original_context.conversation
            && actual.workspace_resource == original_context.workspace_resource
            && actual.execution_session == original_context.execution_session
            && actual.executor_kind == original_context.executor_kind
            && actual.sdk_session.is_none()
            && context.binding_generation == original.context.binding_generation
            && context.execution_target_public_key == original.context.execution_target_public_key,
        "task policy read escaped its approved Task, binding or target",
    )?;
    if request.purpose() == RemoteExecutionPolicyPurpose::Execution {
        require(
            selection.execution_grant == original.selection.execution_grant
                && context.canonical_cwd == original.context.canonical_cwd,
            "current execution policy changed its approved writer grant or cwd",
        )?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct TaskExecutionPolicyImport {
    inner: remote_execution_policy::RemoteExecutionPolicyImport,
    trust: VerifiedTaskExecutionTrust,
    source: VerifiedTaskPolicySource,
    origin: PolicyCarrierOrigin,
    page_envelope_hashes: Vec<ObjectHash>,
}

pub fn begin_task_policy_import(
    trust: &VerifiedTaskExecutionTrust,
    source: &VerifiedTaskPolicySource,
    request: &remote_execution_policy::RemoteExecutionPolicyReadRequest,
    snapshot: &Signed<remote_execution_policy::RemoteExecutionPolicySnapshot>,
    verified_floors: &[PolicyPin],
    origin: PolicyCarrierOrigin,
    now: u64,
) -> Result<TaskExecutionPolicyImport> {
    require(
        source.verified_at_ms <= now
            && now < source.expires_at_ms
            && source.intent == request.intent,
        "task policy import changed its current native source",
    )?;
    let typed = TaskExecutionPolicyRequest::Account(request.clone());
    check_task_read_scope(trust, &typed)?;
    if let PolicyCarrierOrigin::DirectNoise(peer) = &origin {
        require(
            peer.public_key() == &trust.selection().target_public_key,
            "task policy import reached another actual Noise peer",
        )?;
    }
    let effective = effective_task_pins(trust, source, &typed, verified_floors)?;
    let inner = remote_execution_policy::begin_native_execution_policy_import(
        request,
        snapshot,
        &effective,
        &trust.selection().target_public_key,
        &source.usage,
        now,
        typed.purpose() == RemoteExecutionPolicyPurpose::History,
    )?;
    Ok(TaskExecutionPolicyImport {
        inner,
        trust: trust.clone(),
        source: source.clone(),
        origin,
        page_envelope_hashes: Vec::new(),
    })
}

impl TaskExecutionPolicyImport {
    pub fn page_request(
        &self,
    ) -> Result<remote_execution_policy::RemoteExecutionPolicyPageRequest> {
        let encoded = self
            .inner
            .page_request_json()
            .map_err(|_| AuthorizationError::InvalidProof("invalid native policy page request"))?;
        serde_json::from_str(&encoded)
            .map_err(|_| AuthorizationError::InvalidProof("invalid native policy page request"))
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        self.inner.native_proof_usage()
    }
    pub fn append_page(
        &mut self,
        page: &Signed<remote_execution_policy::RemoteExecutionPolicyPage>,
        now: u64,
    ) -> Result<()> {
        require(
            page.payload.entry.is_some() || page.payload.complete,
            "native policy import rejects empty nonfinal pages",
        )?;
        self.inner.append_native_page(page, now)?;
        let index = usize::try_from(page.payload.page_index)
            .map_err(|_| AuthorizationError::InvalidProof("native policy page index overflow"))?;
        let hash = ObjectHash::body(page)?;
        if index == self.page_envelope_hashes.len() {
            self.page_envelope_hashes.push(hash);
        } else {
            require(
                self.page_envelope_hashes.get(index) == Some(&hash),
                "native policy page envelope changed",
            )?;
        }
        Ok(())
    }
    pub fn finish_current(
        self,
        witness: &Signed<remote_execution_policy::RemoteExecutionPolicyWitness>,
        now: u64,
    ) -> Result<VerifiedTaskCurrentPolicy> {
        require(
            !self.source.retained
                && self.inner.native_request().intent.payload.purpose
                    != RemoteExecutionPolicyPurpose::History,
            "retained or history policy cannot become current execution policy",
        )?;
        let remote = self.inner.finish_native(witness, now, false)?;
        VerifiedTaskCurrentPolicy::from_remote(
            self.trust,
            self.source,
            self.origin,
            self.page_envelope_hashes,
            remote,
            now,
        )
    }
    pub fn finish_original_admission(
        self,
        witness: &Signed<remote_execution_policy::RemoteExecutionPolicyWitness>,
        proof: &events::AdmissionProof,
    ) -> Result<VerifiedTaskOriginalAdmissionPolicy> {
        require(
            self.source.retained
                && self.source.verified_at_ms == proof.admitted_at_ms
                && self.inner.native_request().intent.payload.purpose
                    != RemoteExecutionPolicyPurpose::History,
            "retained policy requires its exact original admission clock and nonhistory purpose",
        )?;
        let remote = self
            .inner
            .finish_native(witness, proof.admitted_at_ms, false)?;
        let policy = VerifiedTaskCurrentPolicy::from_remote(
            self.trust,
            self.source,
            self.origin,
            self.page_envelope_hashes,
            remote,
            proof.admitted_at_ms,
        )?;
        original_admission(policy, proof)
    }
    pub fn finish_retained_floors(
        self,
        witness: &Signed<remote_execution_policy::RemoteExecutionPolicyWitness>,
        original_clock: u64,
    ) -> Result<Vec<PolicyPin>> {
        require(
            self.source.retained && self.source.verified_at_ms == original_clock,
            "retained floor verification requires its original source clock",
        )?;
        let history = self.inner.native_request().intent.payload.purpose
            == RemoteExecutionPolicyPurpose::History;
        let remote = self.inner.finish_native(witness, original_clock, history)?;
        let mut usage = remote.native_proof_usage().clone();
        usage.merge(&self.source.usage);
        remote.record_metadata(&mut usage)?;
        usage.record_signed(remote.native_header())?;
        usage.record_signed(self.trust.original_intent())?;
        usage.record_signed(self.trust.approval())?;
        usage.enforce_limits()?;
        remote
            .snapshot()
            .pins()
            .iter()
            .filter(|pin| {
                !self
                    .source
                    .primary_pins
                    .iter()
                    .any(|primary| primary.domain == pin.domain)
            })
            .map(|pin| {
                let mut pin = pin.clone();
                pin.minimum_head = Some(
                    remote
                        .snapshot()
                        .heads()
                        .iter()
                        .find(|head| head.domain == pin.domain)
                        .cloned()
                        .ok_or(AuthorizationError::PolicyConflict)?,
                );
                Ok(pin)
            })
            .collect()
    }
    pub fn finish_history(
        self,
        witness: &Signed<remote_execution_policy::RemoteExecutionPolicyWitness>,
        now: u64,
    ) -> Result<VerifiedTaskHistoryPolicy> {
        require(
            !self.source.retained
                && self.inner.native_request().intent.payload.purpose
                    == RemoteExecutionPolicyPurpose::History,
            "retained or current policy cannot become history policy",
        )?;
        require(
            self.source.verified_at_ms <= now && now < self.source.expires_at_ms,
            "task history policy source permission expired",
        )?;
        let remote = self.inner.finish_native(witness, now, true)?;
        let request = TaskExecutionPolicyRequest::Account(remote.native_request().clone());
        let mut usage = remote.native_proof_usage().clone();
        usage.merge(&self.source.usage);
        remote.record_metadata(&mut usage)?;
        usage.record_signed(remote.native_header())?;
        usage.record_signed(self.trust.original_intent())?;
        usage.record_signed(self.trust.approval())?;
        usage.enforce_limits()?;
        Ok(VerifiedTaskHistoryPolicy {
            source: self.source,
            trust: self.trust,
            request,
            snapshot: remote.native_header().clone(),
            witness: remote.native_witness().clone(),
            foreign: remote.snapshot().clone(),
            usage,
            page_envelope_hashes: self.page_envelope_hashes,
            origin: self.origin,
        })
    }
}

#[derive(Clone)]
pub struct VerifiedTaskCurrentPolicy {
    source: VerifiedTaskPolicySource,
    trust: VerifiedTaskExecutionTrust,
    request: TaskExecutionPolicyRequest,
    snapshot: Signed<remote_execution_policy::RemoteExecutionPolicySnapshot>,
    witness: Signed<remote_execution_policy::RemoteExecutionPolicyWitness>,
    combined: policy_import::VerifiedPolicySnapshot,
    primary_heads: Vec<PolicyStamp>,
    foreign_heads: Vec<PolicyStamp>,
    selection: RemoteExecutionPolicySelection,
    context: prepare::PrepareContext,
    usage: ExecutionProofUsage,
    page_envelope_hashes: Vec<ObjectHash>,
    origin: PolicyCarrierOrigin,
}
impl VerifiedTaskCurrentPolicy {
    fn from_remote(
        trust: VerifiedTaskExecutionTrust,
        source: VerifiedTaskPolicySource,
        origin: PolicyCarrierOrigin,
        page_envelope_hashes: Vec<ObjectHash>,
        remote: remote_execution_policy::VerifiedRemoteExecutionPolicy,
        now: u64,
    ) -> Result<Self> {
        let request = TaskExecutionPolicyRequest::Account(remote.native_request().clone());
        check_task_read_scope(&trust, &request)?;
        let combined =
            policy_import::merge_same_head_snapshots(&source.snapshot, remote.snapshot())?;
        let (primary_heads, foreign_heads) = combined.heads().iter().cloned().partition(|head| {
            source
                .primary_pins
                .iter()
                .any(|pin| pin.domain == head.domain)
        });
        let mut usage = remote.native_proof_usage().clone();
        usage.merge(&source.usage);
        remote.record_metadata(&mut usage)?;
        usage.record_signed(remote.native_header())?;
        usage.record_signed(trust.original_intent())?;
        usage.record_signed(trust.approval())?;
        usage.enforce_limits()?;
        let policy = Self {
            source,
            trust,
            selection: request.selection()?,
            context: request.context()?,
            request,
            snapshot: remote.native_header().clone(),
            witness: remote.native_witness().clone(),
            combined,
            primary_heads,
            foreign_heads,
            usage,
            page_envelope_hashes,
            origin,
        };
        policy.check_window(now)?;
        Ok(policy)
    }
    fn check_window(&self, now: u64) -> Result<()> {
        self.request.check_window(now)?;
        read_window(
            self.snapshot.payload.issued_at_ms,
            self.snapshot.payload.expires_at_ms,
            now,
        )?;
        read_window(
            self.witness.payload.issued_at_ms,
            self.witness.payload.expires_at_ms,
            now,
        )?;
        require(
            now >= self.source.verified_at_ms && now < self.source.expires_at_ms,
            "task policy source permission expired",
        )
    }
    pub fn check_scope(
        &self,
        facts: &RequestFacts,
        selection: &RemoteExecutionPolicySelection,
        context: &prepare::PrepareContext,
        now: u64,
    ) -> Result<()> {
        require(
            !self.source.retained,
            "retained policy cannot authorize a current request",
        )?;
        self.check_scope_inner(facts, selection, context, now)
    }
    fn check_scope_inner(
        &self,
        facts: &RequestFacts,
        selection: &RemoteExecutionPolicySelection,
        context: &prepare::PrepareContext,
        now: u64,
    ) -> Result<()> {
        self.check_window(now)?;
        require(
            facts.resource == *self.request.source_project()
                && facts.audience.service == ServiceId::from_key(self.request.source_node())
                && facts.audience.device == DeviceId::from_key(self.request.source_node())
                && facts.action == self.request.expected_action()
                && facts.context == context.context
                && *context == self.context
                && *selection == self.selection,
            "task policy does not match this exact request scope, purpose or selection",
        )
    }
    pub fn check_body_scope(
        &self,
        invocation: &Signed<Invocation>,
        facts: &RequestFacts,
        body: &serde_json::Value,
        now: u64,
    ) -> Result<()> {
        require(
            !self.source.retained,
            "retained policy cannot authorize a current body",
        )?;
        self.check_body_scope_inner(invocation, facts, body, now)
    }
    pub fn check_prepare_scope(
        &self,
        intent: &Signed<prepare::PrepareIntent>,
        body: &serde_json::Value,
        now: u64,
    ) -> Result<()> {
        require(
            !self.source.retained,
            "retained policy cannot authorize a current prepare",
        )?;
        let signatures = ScopedSignatureVerification::new();
        intent.verify_signature()?;
        let prepared = &intent.payload;
        let request = &prepared.request;
        require(
            request.body_hash == ObjectHash::body(body)?,
            "task policy body differs from the actual signed prepare",
        )?;
        let (execution_resource, target_public_key, execution_grant) =
            prepare::remote_selection(body)?.ok_or(AuthorizationError::InvalidProof(
                "task policy prepare has no remote selection",
            ))?;
        let facts = RequestFacts {
            resource: request.resource.clone(),
            action: request.action,
            audience: request.audience.clone(),
            operation_id: request.operation_id.clone(),
            body_hash: request.body_hash.clone(),
            context: request.context.clone(),
        };
        let context = prepare::PrepareContext {
            context: request.context.clone(),
            binding_generation: prepared.binding_generation,
            canonical_cwd: prepared.canonical_cwd.clone(),
            execution_target_public_key: prepared.execution_target_public_key.clone(),
        };
        self.check_scope(
            &facts,
            &RemoteExecutionPolicySelection {
                execution_resource,
                target_public_key,
                execution_grant,
            },
            &context,
            now,
        )?;
        prepare::check_raw_context(prepared, body, self.request.source_node())?;
        let mut usage = self.usage.clone();
        usage.record_signed(intent)?;
        usage.record_actual_signature_counts(signatures.counts());
        usage.enforce_limits()
    }
    fn check_body_scope_inner(
        &self,
        invocation: &Signed<Invocation>,
        facts: &RequestFacts,
        body: &serde_json::Value,
        now: u64,
    ) -> Result<()> {
        invocation.verify_signature()?;
        let signed = &invocation.payload;
        require(
            signed.resource == facts.resource
                && signed.audience == facts.audience
                && signed.action == facts.action
                && signed.context == facts.context
                && signed.operation_id == facts.operation_id
                && signed.body_hash == facts.body_hash
                && ObjectHash::body(body)? == facts.body_hash,
            "task policy body differs from the actual signed invocation",
        )?;
        let (execution_resource, target_public_key, execution_grant) =
            prepare::remote_selection(body)?.ok_or(AuthorizationError::InvalidProof(
                "task policy body has no remote selection",
            ))?;
        let selection = RemoteExecutionPolicySelection {
            execution_resource,
            target_public_key,
            execution_grant,
        };
        self.check_scope_inner(facts, &selection, &self.context, now)?;
        let raw = prepare::PrepareIntent {
            request: signed.clone(),
            binding_generation: self.context.binding_generation,
            canonical_cwd: self.context.canonical_cwd.clone(),
            execution_target_public_key: self.context.execution_target_public_key.clone(),
        };
        prepare::check_raw_context(&raw, body, self.request.source_node())
    }
    pub fn state(&self) -> &VerifiedPolicySet {
        self.combined.state()
    }
    pub fn primary_heads(&self) -> &[PolicyStamp] {
        &self.primary_heads
    }
    pub fn foreign_heads(&self) -> &[PolicyStamp] {
        &self.foreign_heads
    }
    pub fn pins(&self) -> &[PolicyPin] {
        self.combined.pins()
    }
    pub fn request(&self) -> &TaskExecutionPolicyRequest {
        &self.request
    }
    pub fn snapshot(&self) -> &Signed<remote_execution_policy::RemoteExecutionPolicySnapshot> {
        &self.snapshot
    }
    pub fn witness(&self) -> &Signed<remote_execution_policy::RemoteExecutionPolicyWitness> {
        &self.witness
    }
    pub fn selection(&self) -> &RemoteExecutionPolicySelection {
        &self.selection
    }
    pub fn context(&self) -> &prepare::PrepareContext {
        &self.context
    }
    pub fn trust(&self) -> &VerifiedTaskExecutionTrust {
        &self.trust
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.request
            .expires_at_ms()
            .min(self.snapshot.payload.expires_at_ms)
            .min(self.witness.payload.expires_at_ms)
            .min(self.source.expires_at_ms)
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn original_page_envelope_hashes(&self) -> &[ObjectHash] {
        &self.page_envelope_hashes
    }
    pub fn carrier_origin(&self) -> &PolicyCarrierOrigin {
        &self.origin
    }
}

#[derive(Clone)]
pub struct VerifiedTaskHistoryPolicy {
    source: VerifiedTaskPolicySource,
    trust: VerifiedTaskExecutionTrust,
    request: TaskExecutionPolicyRequest,
    snapshot: Signed<remote_execution_policy::RemoteExecutionPolicySnapshot>,
    witness: Signed<remote_execution_policy::RemoteExecutionPolicyWitness>,
    foreign: policy_import::VerifiedPolicySnapshot,
    usage: ExecutionProofUsage,
    page_envelope_hashes: Vec<ObjectHash>,
    origin: PolicyCarrierOrigin,
}
impl VerifiedTaskHistoryPolicy {
    pub fn state(&self) -> &VerifiedPolicySet {
        self.foreign.state()
    }
    pub fn primary_heads(&self) -> &[PolicyStamp] {
        self.source.snapshot.heads()
    }
    pub fn foreign_heads(&self) -> &[PolicyStamp] {
        self.foreign.heads()
    }
    pub fn pins(&self) -> &[PolicyPin] {
        self.foreign.pins()
    }
    pub fn request(&self) -> &TaskExecutionPolicyRequest {
        &self.request
    }
    pub fn snapshot(&self) -> &Signed<remote_execution_policy::RemoteExecutionPolicySnapshot> {
        &self.snapshot
    }
    pub fn witness(&self) -> &Signed<remote_execution_policy::RemoteExecutionPolicyWitness> {
        &self.witness
    }
    pub fn trust(&self) -> &VerifiedTaskExecutionTrust {
        &self.trust
    }
    pub fn reference(&self) -> &remote_execution_history::RemoteExecutionHistoryReference {
        self.request.history().expect("verified History request")
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.request
            .expires_at_ms()
            .min(self.snapshot.payload.expires_at_ms)
            .min(self.witness.payload.expires_at_ms)
            .min(self.source.expires_at_ms)
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn original_page_envelope_hashes(&self) -> &[ObjectHash] {
        &self.page_envelope_hashes
    }
    pub fn carrier_origin(&self) -> &PolicyCarrierOrigin {
        &self.origin
    }
}

#[derive(Clone)]
pub struct VerifiedTaskOriginalAdmissionPolicy {
    policy: VerifiedTaskCurrentPolicy,
    state: VerifiedPolicySet,
    primary_heads: Vec<PolicyStamp>,
    foreign_heads: Vec<PolicyStamp>,
    admission: VerifiedAdmission,
    invocation_hash: ObjectHash,
    proof_hash: ObjectHash,
    usage: ExecutionProofUsage,
}
pub fn from_task_original_admission(
    policy: VerifiedTaskCurrentPolicy,
    proof: &events::AdmissionProof,
) -> Result<VerifiedTaskOriginalAdmissionPolicy> {
    require(
        !policy.source.retained,
        "retained policy requires the original admission importer",
    )?;
    original_admission(policy, proof)
}
fn original_admission(
    policy: VerifiedTaskCurrentPolicy,
    proof: &events::AdmissionProof,
) -> Result<VerifiedTaskOriginalAdmissionPolicy> {
    let signatures = ScopedSignatureVerification::new();
    let value = &proof.invocation.payload;
    require(
        value.body_hash == ObjectHash::body(&proof.source_body)?,
        "original task policy admission body changed",
    )?;
    let facts = RequestFacts {
        resource: value.resource.clone(),
        action: value.action,
        audience: value.audience.clone(),
        operation_id: value.operation_id.clone(),
        body_hash: value.body_hash.clone(),
        context: value.context.clone(),
    };
    let selection = prepare::remote_selection(&proof.source_body)?.ok_or(
        AuthorizationError::InvalidProof("original task policy has no execution selection"),
    )?;
    let selection = RemoteExecutionPolicySelection {
        execution_resource: selection.0,
        target_public_key: selection.1,
        execution_grant: selection.2,
    };
    let admission = remote_resolution_history::admitted(policy.state(), proof, &facts)?;
    policy.check_scope_inner(&facts, &selection, &policy.context, proof.admitted_at_ms)?;
    policy.check_body_scope_inner(
        &proof.invocation,
        &facts,
        &proof.source_body,
        proof.admitted_at_ms,
    )?;
    let mut usage = policy.usage.clone();
    usage.record_admission(proof)?;
    usage.record_actual_signature_counts(signatures.counts());
    usage.enforce_limits()?;
    let state = remote_resolution_history::at_heads(policy.state(), &proof.challenge.policies)?;
    let (primary_heads, foreign_heads) =
        proof.challenge.policies.iter().cloned().partition(|head| {
            policy
                .source
                .primary_pins
                .iter()
                .any(|pin| pin.domain == head.domain)
        });
    Ok(VerifiedTaskOriginalAdmissionPolicy {
        invocation_hash: proof.invocation.hash()?,
        proof_hash: ObjectHash::body(proof)?,
        usage,
        state,
        primary_heads,
        foreign_heads,
        policy,
        admission,
    })
}
impl VerifiedTaskOriginalAdmissionPolicy {
    pub fn state(&self) -> &VerifiedPolicySet {
        &self.state
    }
    pub fn primary_heads(&self) -> &[PolicyStamp] {
        &self.primary_heads
    }
    pub fn foreign_heads(&self) -> &[PolicyStamp] {
        &self.foreign_heads
    }
    pub fn pins(&self) -> &[PolicyPin] {
        self.policy.pins()
    }
    pub fn request(&self) -> &TaskExecutionPolicyRequest {
        self.policy.request()
    }
    pub fn snapshot(&self) -> &Signed<remote_execution_policy::RemoteExecutionPolicySnapshot> {
        self.policy.snapshot()
    }
    pub fn witness(&self) -> &Signed<remote_execution_policy::RemoteExecutionPolicyWitness> {
        self.policy.witness()
    }
    pub fn admitted_at_ms(&self) -> u64 {
        self.admission.admitted_at_ms()
    }
    pub fn signed_invocation_hash(&self) -> &ObjectHash {
        &self.invocation_hash
    }
    pub fn proof_hash(&self) -> &ObjectHash {
        &self.proof_hash
    }
    pub fn trust(&self) -> &VerifiedTaskExecutionTrust {
        self.policy.trust()
    }
    pub fn admission(&self) -> &VerifiedAdmission {
        &self.admission
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn original_page_envelope_hashes(&self) -> &[ObjectHash] {
        self.policy.original_page_envelope_hashes()
    }
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
    fn native_task_import_keeps_foreign_policy_isolated_and_reverifies_retained_carriers() {
        let fixture = retained_history_fixture();
        let event: events::Event = serde_json::from_str(
            &fixture
                .actual
                .handoff
                .handoff
                .project_dispatch
                .canonical_body,
        )
        .unwrap();
        let events::EventCause::Invocation { admission: proof } = event.cause else {
            panic!("fixture has no original admission")
        };
        let now = proof.admitted_at_ms;
        let retained = require_confirmed_client_policy(&fixture.source).unwrap();
        let project = fixture
            .actual
            .handoff
            .handoff
            .source_statement()
            .unwrap()
            .project;
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
            &project,
            now,
        )
        .unwrap();
        let source_policy = require_confirmed_client_policy(&source_auth).unwrap();
        let source_key = PublicKey::of(&fixture.project_node);
        let target = PublicKey::of(&fixture.executor);
        let leaf = fixture
            .state
            .grants
            .values()
            .find(|grant| {
                grant.hash().unwrap() == fixture.actual.handoff.invocation.payload.grant_leaf
                    && grant.payload.grantee
                        == Grantee::Actor(Actor::Service(ServiceId::from_key(&source_key)))
                    && grant.payload.actions.contains(&Action::ExecutionOpen)
                    && fixture
                        .state
                        .space(&grant.payload.owner_space)
                        .is_some_and(|space| space.grants.contains(&grant.hash().unwrap()))
            })
            .unwrap();
        let ResourceScope::Exact(resource) = &leaf.payload.scope else {
            panic!("fixture execution grant is not exact")
        };
        let selection = RemoteExecutionPolicySelection {
            execution_resource: resource.clone(),
            target_public_key: target.clone(),
            execution_grant: leaf.hash().unwrap(),
        };
        let mut source_context = leaf.payload.constraints.context.clone();
        source_context.workspace_resource = Some(project.resource.clone());
        let context = prepare::PrepareContext {
            context: source_context,
            binding_generation: Some(1),
            canonical_cwd: fixture.binding.binding.open_spec["cwd"]
                .as_str()
                .map(str::to_owned),
            execution_target_public_key: Some(target.clone()),
        };
        let stream = super::super::remote_execution_policy::tests::remote_policy_stream(
            &source_auth,
            &fixture.client,
            &fixture.project_node,
            &fixture.execution_bundle,
            &fixture.pins,
            &fixture.executor,
            &selection,
            &context,
            Action::ExecutionOpen,
            RemoteExecutionPolicyPurpose::Execution,
            now,
        )
        .unwrap();
        let current = verify_remote_execution_policy_delegation_source(
            source_policy.state(),
            &stream.request.intent,
            &source_key,
            &PublicKey::of(&fixture.client),
            now,
        )
        .unwrap();
        let trust =
            approve_task_execution_policy(&current, None, &fixture.project_node, now).unwrap();
        let source = task_policy_source_from_current(&current, source_policy.pins(), now).unwrap();
        assert!(
            source
                .snapshot
                .state()
                .resource(
                    &selection.execution_resource.space,
                    &selection.execution_resource.resource
                )
                .is_none()
        );
        let mut import = begin_task_policy_import(
            &trust,
            &source,
            &stream.request,
            &stream.header,
            &[],
            PolicyCarrierOrigin::SignedTranscript,
            now,
        )
        .unwrap();
        let mut empty = stream.pages[0].payload.clone();
        empty.entry = None;
        empty.complete = false;
        let empty = Signed::sign(empty, &fixture.executor).unwrap();
        assert!(import.append_page(&empty, now).is_err());
        for page in &stream.pages {
            import.append_page(page, now).unwrap();
        }
        assert!(import.clone().finish_history(&stream.witness, now).is_err());
        assert!(
            import
                .clone()
                .finish_current(&stream.witness, now + 1_000)
                .is_err()
        );
        let completed = import.finish_current(&stream.witness, now).unwrap();
        assert!(
            completed
                .state()
                .resource(
                    &selection.execution_resource.space,
                    &selection.execution_resource.resource
                )
                .is_some()
        );
        assert!(
            source
                .snapshot
                .state()
                .resource(
                    &selection.execution_resource.space,
                    &selection.execution_resource.resource
                )
                .is_none()
        );
        assert_eq!(completed.selection(), &selection);
        assert_eq!(
            completed.original_page_envelope_hashes(),
            stream
                .pages
                .iter()
                .map(ObjectHash::body)
                .collect::<Result<Vec<_>>>()
                .unwrap()
        );
        assert!(matches!(
            completed.carrier_origin(),
            PolicyCarrierOrigin::SignedTranscript
        ));
        let before = completed.proof_usage().counts();
        let mut accounted = completed.proof_usage().clone();
        accounted.record_signed(&stream.header).unwrap();
        assert_eq!(
            accounted.counts().unique_signed_objects,
            before.unique_signed_objects
        );
        assert_eq!(
            accounted.counts().unique_signatures,
            before.unique_signatures
        );
        let prepared = prepare::PrepareIntent {
            request: proof.invocation.payload.clone(),
            binding_generation: context.binding_generation,
            canonical_cwd: context.canonical_cwd.clone(),
            execution_target_public_key: context.execution_target_public_key.clone(),
        };
        let signed_prepare = Signed::sign(prepared.clone(), &fixture.client).unwrap();
        completed
            .check_prepare_scope(&signed_prepare, &proof.source_body, now)
            .unwrap();
        let mut wrong_prepare = prepared.clone();
        wrong_prepare.binding_generation = Some(2);
        assert!(
            completed
                .check_prepare_scope(
                    &Signed::sign(wrong_prepare, &fixture.client).unwrap(),
                    &proof.source_body,
                    now
                )
                .is_err()
        );
        let mut wrong_cwd = prepared;
        wrong_cwd.canonical_cwd = Some("/another-directory".into());
        assert!(
            completed
                .check_prepare_scope(
                    &Signed::sign(wrong_cwd, &fixture.client).unwrap(),
                    &proof.source_body,
                    now
                )
                .is_err()
        );
        let mut changed_body = proof.source_body.clone();
        changed_body["authorization_id"] = json!("another-operation");
        assert!(
            completed
                .check_prepare_scope(&signed_prepare, &changed_body, now)
                .is_err()
        );
        let original = from_task_original_admission(completed.clone(), &proof).unwrap();
        assert_eq!(original.proof_hash(), &ObjectHash::body(&proof).unwrap());
        assert!(
            original
                .primary_heads()
                .iter()
                .chain(original.foreign_heads())
                .all(|head| proof.challenge.policies.contains(head)
                    && original.state().head(&head.domain) == Some(head))
        );
        assert!(
            original.proof_usage().counts().unique_signed_objects
                > completed.proof_usage().counts().unique_signed_objects
        );
        let facts = RequestFacts {
            resource: project,
            audience: stream
                .request
                .intent
                .payload
                .source_selection
                .audience
                .clone(),
            action: Action::ExecutionOpen,
            context: context.context.clone(),
            operation_id: fixture
                .actual
                .handoff
                .invocation
                .payload
                .operation_id
                .clone(),
            body_hash: ObjectHash::body(&"scope").unwrap(),
        };
        completed
            .check_scope(&facts, &selection, &context, now)
            .unwrap();
        let mut wrong = context.clone();
        wrong.binding_generation = Some(2);
        assert!(
            completed
                .check_scope(&facts, &selection, &wrong, now)
                .is_err()
        );
        let mut changed_genesis = fixture.pins.clone();
        changed_genesis[0].genesis_root = PublicKey::of(&fixture.client);
        assert!(
            begin_task_policy_import(
                &trust,
                &source,
                &stream.request,
                &stream.header,
                &changed_genesis,
                PolicyCarrierOrigin::SignedTranscript,
                now
            )
            .is_err()
        );
        let mut future_floor = fixture.pins.clone();
        let mut missing = stream
            .header
            .payload
            .target_heads
            .iter()
            .find(|head| head.domain == future_floor[0].domain)
            .unwrap()
            .clone();
        missing.sequence += 1;
        future_floor[0].minimum_head = Some(missing);
        let mut too_old = begin_task_policy_import(
            &trust,
            &source,
            &stream.request,
            &stream.header,
            &future_floor,
            PolicyCarrierOrigin::SignedTranscript,
            now,
        )
        .unwrap();
        for page in &stream.pages {
            too_old.append_page(page, now).unwrap();
        }
        assert!(too_old.finish_current(&stream.witness, now).is_err());

        let retained_source = task_policy_source_from_retained(
            &trust,
            source_policy.state(),
            source_policy.pins(),
            &stream.request,
            now,
        )
        .unwrap();
        let mut restored = begin_task_policy_import(
            &trust,
            &retained_source,
            &stream.request,
            &stream.header,
            &[],
            PolicyCarrierOrigin::SignedTranscript,
            now,
        )
        .unwrap();
        for page in &stream.pages {
            restored.append_page(page, now).unwrap();
        }
        assert!(
            restored
                .clone()
                .finish_current(&stream.witness, now)
                .is_err()
        );
        assert!(
            restored
                .clone()
                .finish_history(&stream.witness, now)
                .is_err()
        );
        let original_restored = restored
            .clone()
            .finish_original_admission(&stream.witness, &proof)
            .unwrap();
        assert_eq!(original_restored.proof_hash(), original.proof_hash());
        assert_eq!(original_restored.foreign_heads(), original.foreign_heads());
        assert_eq!(
            original_restored.original_page_envelope_hashes(),
            completed.original_page_envelope_hashes()
        );
        let floors = restored
            .finish_retained_floors(&stream.witness, now)
            .unwrap();
        assert!(!floors.is_empty());
        assert!(floors.iter().all(|pin| {
            completed
                .foreign_heads()
                .iter()
                .any(|head| pin.minimum_head.as_ref() == Some(head) && pin.domain == head.domain)
        }));
        let mut stolen = stream.request.clone();
        stolen.delegation = Signed::sign(stolen.delegation.payload, &fixture.client).unwrap();
        assert!(
            task_policy_source_from_retained(
                &trust,
                source_policy.state(),
                source_policy.pins(),
                &stolen,
                now
            )
            .is_err()
        );
    }

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
