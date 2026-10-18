//! Compact references and public-policy reads for retained execution history.

use super::{
    events::{AdmissionProof, Event, EventCause, StoredTransport},
    execution_handoff::*,
    prepare::PrepareContext,
    *,
};
use crate::signature_verification::{ScopedSignatureVerification, SignatureVerification};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteExecutionHistoryKind {
    ExecutionResult,
    UnknownResolution,
    TerminalReconciliation,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteExecutionHistoryReference {
    pub version: u16,
    pub kind: RemoteExecutionHistoryKind,
    pub actual_hash: ObjectHash,
    pub expected_body_hash: ObjectHash,
    pub expected_invocation_hash: ObjectHash,
    pub account_invocation_hash: ObjectHash,
    pub expected_binding_hash: Option<ObjectHash>,
    pub account_admitted_at_ms: u64,
    pub dispatch_ref: EventRef,
    pub dispatch_envelope_hash: ObjectHash,
    pub source: Signed<ProjectExecutionSource>,
    pub old_submission: Signed<Invocation>,
    pub observation_receipt: Signed<ExecutionHandoffReceipt>,
    pub target_public_key: PublicKey,
    pub handoff_hash: ObjectHash,
    pub resolution_hash: Option<ObjectHash>,
    pub freshness_hash: Option<ObjectHash>,
    pub terminal_freshness: Option<ExecutionHandoffResolutionFreshness>,
    pub result_hash: ObjectHash,
}

impl RemoteExecutionHistoryReference {
    pub fn execution_resource(&self) -> &ResourceRef {
        &self.old_submission.payload.resource
    }
    pub fn grant_leaf(&self) -> &ObjectHash {
        &self.old_submission.payload.grant_leaf
    }
    pub fn target_public_key(&self) -> &PublicKey {
        &self.target_public_key
    }
    pub fn context(&self) -> Result<PrepareContext> {
        let mut context = self.old_submission.payload.context.clone();
        require(
            context.task.as_ref() == self.source.payload.work.task_ref()
                && context.execution_session.as_ref()
                    == self.source.payload.work.execution_session()
                && context.conversation.as_deref()
                    == Some(self.source.payload.conversation.as_str())
                && (context.executor_kind.is_some()
                    == self.source.payload.work.execution_session().is_some()),
            "historical reference changed its original task or session",
        )?;
        context.workspace_resource = Some(self.source.payload.project.resource.clone());
        context.sdk_session = None;
        Ok(PrepareContext {
            context,
            binding_generation: self.source.payload.work.binding_generation(),
            canonical_cwd: self.source.payload.work.canonical_cwd().map(str::to_owned),
            execution_target_public_key: Some(self.target_public_key.clone()),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteExecutionHistoryAcceptance {
    ExecutionResult {
        receipt: Signed<ExecutionHandoffAdmissionReceipt>,
        freshness: ExecutionHandoffFreshness,
        transport: StoredTransport,
    },
    UnknownResolution {
        receipt: Signed<ExecutionHandoffResolutionReceipt>,
        freshness: ExecutionHandoffResolutionFreshness,
    },
    TerminalReconciliation {
        freshness: ExecutionHandoffResolutionFreshness,
        observation_receipt: Signed<ExecutionHandoffReceipt>,
    },
}

fn parse<T: for<'de> Deserialize<'de>>(json: &str) -> Result<T> {
    require(
        json.len() <= MAX_EXECUTION_HANDOFF_BYTES,
        "historical result is too large",
    )?;
    serde_json::from_str(json)
        .map_err(|_| AuthorizationError::InvalidProof("invalid historical result"))
}

fn carrier<T: Serialize>(value: &T) -> Result<()> {
    require(
        canonical(value)?.len() <= MAX_PROOF_BYTES,
        "historical policy carrier is too large",
    )
}

fn dispatch_proof(handoff: &ExecutionHandoff) -> Result<AdmissionProof> {
    let event: Event = parse(&handoff.project_dispatch.canonical_body)?;
    match event.cause {
        EventCause::Invocation { admission } => Ok(admission),
        _ => Err(AuthorizationError::InvalidProof(
            "historical result has no original admission",
        )),
    }
}

pub(crate) fn derive_reference(
    actual_json: &str,
    body_json: &str,
    invocation_json: &str,
    binding_json: &str,
) -> Result<RemoteExecutionHistoryReference> {
    let _signatures = ScopedSignatureVerification::new();
    let raw: Value = parse(actual_json)?;
    let body: Value = parse(body_json)?;
    let invocation: Signed<Invocation> = parse(invocation_json)?;
    invocation.verify_signature()?;
    let binding: Option<task_binding::SignedBinding> = if binding_json.trim().is_empty() {
        None
    } else {
        parse(binding_json)?
    };
    let (kind, observation, old_submission, proof, resolution_hash, freshness_hash, result_hash) =
        if raw.get("resolution").is_some() {
            let actual: ExecutionHandoffResolutionResult = serde_json::from_value(raw.clone())
                .map_err(|_| AuthorizationError::InvalidProof("invalid historical resolution"))?;
            require(
                binding.is_none(),
                "resolution history cannot replace the original binding",
            )?;
            verify_execution_resolution_receipt_signature(&actual)?;
            (
                RemoteExecutionHistoryKind::UnknownResolution,
                actual.resolution.resolution.observation.clone(),
                actual.resolution.invocation.clone(),
                actual.resolution.resolution.account_admission.clone(),
                Some(actual.resolution.hash()?),
                Some(ObjectHash::body(&actual.freshness)?),
                ObjectHash::body(&actual.result)?,
            )
        } else if raw.get("reconciliation").is_some() {
            let actual: ExecutionHandoffTerminalReconciliationResult =
                serde_json::from_value(raw.clone()).map_err(|_| {
                    AuthorizationError::InvalidProof("invalid historical reconciliation")
                })?;
            require(
                binding.is_none()
                    && actual.reconciliation.resolution.purpose
                        == ExecutionResolutionPurpose::ReconcileTerminal,
                "historical reconciliation changed its purpose or binding",
            )?;
            execution_resolution_account_facts(&actual.reconciliation.resolution)?;
            (
                RemoteExecutionHistoryKind::TerminalReconciliation,
                actual.reconciliation.resolution.observation.clone(),
                actual.reconciliation.invocation.clone(),
                actual.reconciliation.resolution.account_admission.clone(),
                Some(actual.reconciliation.hash()?),
                Some(ObjectHash::body(&actual.freshness)?),
                ObjectHash::body(&actual.reconciliation.resolution.observation.result)?,
            )
        } else {
            let actual: ExecutionHandoffResult =
                serde_json::from_value(raw.clone()).map_err(|_| {
                    AuthorizationError::InvalidProof("invalid historical execution result")
                })?;
            let proof = dispatch_proof(&actual.handoff.handoff)?;
            match &actual.handoff.handoff.source_statement()?.work {
                ExecutionWork::TaskSession { .. } => {
                    let binding = binding.as_ref().ok_or(AuthorizationError::InvalidProof(
                        "historical Task result requires its original binding",
                    ))?;
                    require(
                        body["request"]["binding"]
                            == serde_json::to_value(binding).map_err(|_| {
                                AuthorizationError::InvalidProof("invalid historical binding")
                            })?,
                        "historical execution result changed its original binding",
                    )?;
                }
                _ => require(
                    binding.is_none() && body["request"]["binding"].is_null(),
                    "non-Task history carries a binding",
                )?,
            }
            (
                RemoteExecutionHistoryKind::ExecutionResult,
                actual.clone(),
                actual.handoff.invocation.clone(),
                proof,
                None,
                None,
                ObjectHash::body(&actual.result)?,
            )
        };
    require(
        proof.source_body == body
            && proof.invocation == invocation
            && invocation.payload.body_hash == ObjectHash::body(&body)?,
        "historical result changed its complete original Account request",
    )?;
    let handoff = &observation.handoff.handoff;
    verify_execution_handoff_receipt_signature(
        &observation.handoff,
        &observation.receipt,
        &observation.result,
    )?;
    old_submission.verify_signature()?;
    let source = handoff
        .source_attestation
        .clone()
        .ok_or(AuthorizationError::InvalidProof(
            "historical result requires its original Project source attestation",
        ))?;
    source.verify_signature()?;
    require(
        source.payload == handoff.source_statement()?
            && source.issuer == observation.handoff.invocation.issuer,
        "historical result changed its original Project source",
    )?;
    if let Some(binding) = &binding {
        task_binding::verify_binding(binding, source.issuer.as_str())
            .map_err(|_| AuthorizationError::InvalidProof("invalid original historical binding"))?;
    }
    let terminal_freshness = if kind == RemoteExecutionHistoryKind::TerminalReconciliation {
        Some(
            serde_json::from_value(raw["freshness"].clone()).map_err(|_| {
                AuthorizationError::InvalidProof("invalid original terminal freshness")
            })?,
        )
    } else {
        None
    };
    let reference = RemoteExecutionHistoryReference {
        version: AUTH_VERSION,
        kind,
        actual_hash: ObjectHash::body(&raw)?,
        expected_body_hash: ObjectHash::body(&body)?,
        expected_invocation_hash: ObjectHash::body(&invocation)?,
        account_invocation_hash: invocation.hash()?,
        expected_binding_hash: binding.as_ref().map(ObjectHash::body).transpose()?,
        account_admitted_at_ms: proof.admitted_at_ms,
        dispatch_ref: EventRef::parse(&handoff.project_dispatch.hash)?,
        dispatch_envelope_hash: ObjectHash::body(&handoff.project_dispatch)?,
        source,
        old_submission,
        observation_receipt: observation.receipt.clone(),
        target_public_key: handoff.target_public_key.clone(),
        handoff_hash: observation.handoff.hash()?,
        resolution_hash,
        freshness_hash,
        terminal_freshness,
        result_hash,
    };
    check_reference(&reference)?;
    Ok(reference)
}

pub fn derive_remote_execution_history_reference(
    actual_json: &str,
    body_json: &str,
    invocation_json: &str,
    binding_json: &str,
) -> crate::Result<String> {
    serde_json::to_string(
        &derive_reference(actual_json, body_json, invocation_json, binding_json)
            .map_err(|error| error.to_string())?,
    )
    .map_err(|_| "historical reference encoding failed".into())
}

fn check_reference(reference: &RemoteExecutionHistoryReference) -> Result<()> {
    carrier(reference)?;
    reference.source.verify_signature()?;
    reference.old_submission.verify_signature()?;
    reference.observation_receipt.verify_signature()?;
    let submission = &reference.old_submission.payload;
    let source = &reference.source.payload;
    let observation = &reference.observation_receipt.payload;
    require(
        reference.version == AUTH_VERSION
            && source.version == AUTH_VERSION
            && submission.version == AUTH_VERSION
            && reference.observation_receipt.issuer == reference.target_public_key
            && observation.version == AUTH_VERSION
            && observation.handoff_hash == reference.handoff_hash
            && observation.operation_id == source.operation_id
            && observation.original_space_id == source.project.space
            && observation.execution_space_id == submission.resource.space
            && observation.target == DeviceId::from_key(&reference.target_public_key)
            && observation.work == source.work
            && (reference.kind == RemoteExecutionHistoryKind::UnknownResolution
                || observation.result_hash == reference.result_hash)
            && submission.actor
                == Actor::Service(ServiceId::from_key(&reference.old_submission.issuer))
            && submission.device == DeviceId::from_key(&reference.old_submission.issuer)
            && submission.presenter == submission.device
            && submission.audience.service == ServiceId::from_key(&reference.target_public_key)
            && submission.audience.device == DeviceId::from_key(&reference.target_public_key)
            && submission.context.task.as_ref() == source.work.task_ref()
            && submission.context.execution_session.as_ref() == source.work.execution_session()
            && submission.context.workspace_resource.as_ref()
                == Some(&submission.resource.resource)
            && source
                .work
                .execution_session()
                .is_none_or(|session| session.target == submission.audience.device)
            && source.target_public_key == reference.target_public_key
            && submission.context.conversation.as_deref() == Some(source.conversation.as_str())
            && source.operation_id.len() <= 1_024
            && !source.operation_id.is_empty()
            && !submission.expected_policies.is_empty()
            && submission.expected_policies.len() <= MAX_POLICY_DOMAINS
            && submission
                .expected_policies
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain)
            && match reference.kind {
                RemoteExecutionHistoryKind::ExecutionResult => {
                    submission.action == Action::ExecutionSubmit
                        && reference.expected_binding_hash.is_some()
                            == matches!(source.work, ExecutionWork::TaskSession { .. })
                        && reference.resolution_hash.is_none()
                        && reference.freshness_hash.is_none()
                        && reference.terminal_freshness.is_none()
                        && reference.account_invocation_hash == source.original_invocation_hash
                        && submission.operation_id == source.operation_id
                        && reference.old_submission.issuer == reference.source.issuer
                }
                RemoteExecutionHistoryKind::UnknownResolution
                | RemoteExecutionHistoryKind::TerminalReconciliation => {
                    submission.action == Action::ExecutionResolve
                        && reference.expected_binding_hash.is_none()
                        && reference.resolution_hash.is_some()
                        && reference.freshness_hash.is_some()
                        && if reference.kind == RemoteExecutionHistoryKind::TerminalReconciliation {
                            reference.terminal_freshness.as_ref().is_some_and(|proof| {
                                ObjectHash::body(proof).ok().as_ref()
                                    == reference.freshness_hash.as_ref()
                            })
                        } else {
                            reference.terminal_freshness.is_none()
                        }
                }
            },
        "historical policy reference changed its purpose, original signer or task",
    )?;
    reference.context()?;
    Ok(())
}

pub fn validate_remote_execution_history_reference(
    reference: &RemoteExecutionHistoryReference,
) -> Result<()> {
    let _scope = ScopedSignatureVerification::new();
    check_reference(reference)?;
    let mut usage = ExecutionProofUsage::default();
    record_history_reference_metadata(reference, &mut usage)?;
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()
}

pub(crate) fn record_history_reference_metadata(
    reference: &RemoteExecutionHistoryReference,
    usage: &mut ExecutionProofUsage,
) -> Result<()> {
    usage.record_signed(&reference.source)?;
    usage.record_signed(&reference.old_submission)?;
    usage.record_signed(&reference.observation_receipt)?;
    let mut metadata = serde_json::to_value(reference)
        .map_err(|_| AuthorizationError::InvalidProof("invalid historical reference metadata"))?;
    for (field, hash) in [
        ("source", ObjectHash::body(&reference.source)?),
        (
            "old_submission",
            ObjectHash::body(&reference.old_submission)?,
        ),
        (
            "observation_receipt",
            ObjectHash::body(&reference.observation_receipt)?,
        ),
    ] {
        metadata[field] = serde_json::json!({"signed_object_hash": hash});
    }
    if let Some(proof) = &reference.terminal_freshness {
        metadata["terminal_freshness"] =
            serde_json::json!({"proof_hash": ObjectHash::body(proof)?});
    }
    usage.record_metadata(&serde_json::json!({
        "reference_hash": ObjectHash::body(reference)?, "reference": metadata,
    }))?;
    if let Some(proof) = &reference.terminal_freshness {
        require(
            proof.receipts.len() <= MAX_POLICY_DOMAINS
                && proof.challenge.policies.len() <= MAX_POLICY_DOMAINS,
            "historical reference has too many freshness items",
        )?;
        usage.record_metadata(&proof.challenge)?;
        for receipt in &proof.receipts {
            usage.record_signed(receipt)?;
        }
    }
    usage.enforce_limits()
}

pub struct VerifiedHistoryAcceptance {
    decision_at_ms: u64,
    heads: Vec<PolicyStamp>,
    usage: ExecutionProofUsage,
}

impl VerifiedHistoryAcceptance {
    pub fn decision_at_ms(&self) -> u64 {
        self.decision_at_ms
    }
    pub fn heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
}

fn proof_time(
    reference: &RemoteExecutionHistoryReference,
    proof: &ExecutionHandoffResolutionFreshness,
    observation: &Signed<ExecutionHandoffReceipt>,
    now: u64,
) -> Result<u64> {
    let lower = proof
        .receipts
        .iter()
        .map(|receipt| receipt.payload.issued_at_ms)
        .fold(
            reference
                .account_admitted_at_ms
                .max(reference.old_submission.payload.not_before_ms)
                .max(proof.challenge.issued_at_ms)
                .max(observation.payload.issued_at_ms),
            u64::max,
        );
    let upper = proof
        .receipts
        .iter()
        .map(|receipt| receipt.payload.expires_at_ms)
        .fold(
            reference
                .old_submission
                .payload
                .expires_at_ms
                .min(proof.challenge.expires_at_ms),
            u64::min,
        );
    require(
        lower < upper && lower <= now,
        "historical reconciliation proof has no valid time intersection",
    )?;
    Ok(lower)
}

pub fn verify_remote_execution_history_acceptance(
    state: &VerifiedPolicySet,
    reference: &RemoteExecutionHistoryReference,
    acceptance: &RemoteExecutionHistoryAcceptance,
    local_c: &PublicKey,
    now: u64,
) -> Result<VerifiedHistoryAcceptance> {
    let _signatures = ScopedSignatureVerification::new();
    check_reference(reference)?;
    carrier(acceptance)?;
    require(
        local_c == reference.target_public_key(),
        "history reached another executor",
    )?;
    let mut usage = ExecutionProofUsage::default();
    record_history_reference_metadata(reference, &mut usage)?;
    let (decision_at_ms, heads, challenge_hash, issued, expires, receipts) = match acceptance {
        RemoteExecutionHistoryAcceptance::ExecutionResult {
            receipt,
            freshness,
            transport,
        } => {
            receipt.verify_signature()?;
            let value = &receipt.payload;
            let challenge = &freshness.challenge;
            require(
                reference.kind == RemoteExecutionHistoryKind::ExecutionResult
                    && receipt.issuer == *local_c
                    && value.version == AUTH_VERSION
                    && value.handoff_hash == reference.handoff_hash
                    && value.submission_invocation_hash == reference.old_submission.hash()?
                    && value.original_invocation_hash
                        == reference.source.payload.original_invocation_hash
                    && value.original_request_hash == challenge.original_request_hash
                    && value.admission_hash == ObjectHash::body(freshness)?
                    && value.transport_hash == ObjectHash::body(transport)?
                    && transport.existing_evidence()?
                        == TransportEvidence::VerifiedPeer {
                            origin: reference.old_submission.payload.presenter.clone(),
                        }
                    && challenge.version == AUTH_VERSION
                    && challenge.stage == CheckStage::Admit
                    && challenge.handoff_hash == reference.handoff_hash
                    && challenge.original_invocation_hash == value.original_invocation_hash
                    && challenge.submission_invocation_hash == value.submission_invocation_hash
                    && challenge.original_body_hash == reference.expected_body_hash
                    && challenge.original_resource == reference.source.payload.project
                    && challenge.execution_resource == *reference.execution_resource()
                    && challenge.execution_audience == reference.old_submission.payload.audience
                    && challenge.target_public_key == *local_c
                    && challenge.project_presenter == reference.old_submission.payload.presenter
                    && challenge.operation_id == reference.source.payload.operation_id
                    && challenge.work == reference.source.payload.work,
                "historical execution acceptance changed its original signed checkpoint",
            )?;
            usage.record_signed(receipt)?;
            usage.record_metadata(freshness)?;
            usage.record_metadata(transport)?;
            (
                value.admitted_at_ms,
                challenge.policies.clone(),
                challenge.hash()?,
                challenge.issued_at_ms,
                challenge.expires_at_ms,
                &freshness.receipts,
            )
        }
        RemoteExecutionHistoryAcceptance::UnknownResolution { receipt, freshness } => {
            receipt.verify_signature()?;
            let value = &receipt.payload;
            require(
                reference.kind == RemoteExecutionHistoryKind::UnknownResolution
                    && receipt.issuer == *local_c
                    && value.version == AUTH_VERSION
                    && Some(&value.resolution_hash) == reference.resolution_hash.as_ref()
                    && value.freshness_hash == ObjectHash::body(freshness)?,
                "historical Unknown resolution changed its original executor receipt",
            )?;
            resolution_challenge(reference, freshness)?;
            usage.record_signed(receipt)?;
            usage.record_metadata(freshness)?;
            (
                value.issued_at_ms,
                freshness.challenge.policies.clone(),
                freshness.challenge.hash()?,
                freshness.challenge.issued_at_ms,
                freshness.challenge.expires_at_ms,
                &freshness.receipts,
            )
        }
        RemoteExecutionHistoryAcceptance::TerminalReconciliation {
            freshness,
            observation_receipt,
        } => {
            observation_receipt.verify_signature()?;
            let observation = &observation_receipt.payload;
            require(
                reference.kind == RemoteExecutionHistoryKind::TerminalReconciliation
                    && observation_receipt == &reference.observation_receipt
                    && reference.terminal_freshness.as_ref() == Some(freshness)
                    && observation_receipt.issuer == *local_c
                    && observation.version == AUTH_VERSION
                    && observation.handoff_hash == reference.handoff_hash
                    && observation.operation_id == reference.source.payload.operation_id
                    && observation.original_space_id == reference.source.payload.project.space
                    && observation.execution_space_id == reference.execution_resource().space
                    && observation.target == DeviceId::from_key(local_c)
                    && observation.work == reference.source.payload.work
                    && observation.result_hash == reference.result_hash
                    && matches!(
                        observation.state,
                        ExecutionHandoffState::Succeeded
                            | ExecutionHandoffState::Failed
                            | ExecutionHandoffState::Rejected
                    ),
                "historical terminal reconciliation changed its original executor result",
            )?;
            resolution_challenge(reference, freshness)?;
            usage.record_signed(observation_receipt)?;
            usage.record_metadata(freshness)?;
            (
                proof_time(reference, freshness, observation_receipt, now)?,
                freshness.challenge.policies.clone(),
                freshness.challenge.hash()?,
                freshness.challenge.issued_at_ms,
                freshness.challenge.expires_at_ms,
                &freshness.receipts,
            )
        }
    };
    require(
        decision_at_ms <= now
            && reference.account_admitted_at_ms <= decision_at_ms
            && !heads.is_empty()
            && heads.len() <= MAX_POLICY_DOMAINS
            && heads.windows(2).all(|pair| pair[0].domain < pair[1].domain),
        "historical policy proof changed its exact heads or decision time",
    )?;
    validate_window(issued, expires, decision_at_ms)?;
    require(
        expires - issued <= EXECUTION_LEASE_MS,
        "historical freshness proof exceeds its original window limit",
    )?;
    validate_window(
        reference.old_submission.payload.not_before_ms,
        reference.old_submission.payload.expires_at_ms,
        decision_at_ms,
    )?;
    let known: Vec<_> = heads
        .iter()
        .filter(|head| state.head(&head.domain).is_some())
        .cloned()
        .collect();
    let historical = remote_resolution_history::at_heads(state, &known)?;
    let mut seen = BTreeSet::new();
    let mut budget = Budget::default();
    for signed in receipts {
        budget.verify(signed)?;
        let receipt = &signed.payload;
        receipt.validate_currents()?;
        require(
            receipt.version == AUTH_VERSION
                && receipt.challenge_hash == challenge_hash
                && receipt.issued_at_ms >= issued
                && receipt.expires_at_ms <= expires,
            "historical freshness changed its exact policy proof",
        )?;
        validate_window(receipt.issued_at_ms, receipt.expires_at_ms, decision_at_ms)?;
        for head in &receipt.currents {
            require(
                heads.contains(head) && seen.insert(head.domain.clone()),
                "historical freshness changed its exact policy proof",
            )?;
            if historical.head(&head.domain).is_some() {
                require(
                    historical.head(&head.domain) == Some(head)
                        && historical.roots.contains_key(head),
                    "historical freshness is missing its exact policy witness",
                )?;
                let authority = match &head.domain {
                    PolicyDomain::Account(account) => {
                        &historical.account(account).unwrap().authority
                    }
                    PolicyDomain::Space(space) => &historical.space(space).unwrap().authority,
                };
                require(
                    signed.issuer == authority.public_key
                        && DeviceId::from_key(&signed.issuer) == authority.device
                        && ServiceId::from_key(&signed.issuer) == authority.service,
                    "historical freshness changed its known authority",
                )?;
            }
        }
        usage.record_signed(signed)?;
    }
    require(
        seen.len() == heads.len(),
        "historical freshness changed its exact policy proof",
    )?;
    usage.record_budget(&budget)?;
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(VerifiedHistoryAcceptance {
        decision_at_ms,
        heads,
        usage,
    })
}

fn resolution_challenge(
    reference: &RemoteExecutionHistoryReference,
    freshness: &ExecutionHandoffResolutionFreshness,
) -> Result<()> {
    let challenge = &freshness.challenge;
    require(
        challenge.version == AUTH_VERSION
            && Some(&challenge.resolution_hash) == reference.resolution_hash.as_ref()
            && Some(ObjectHash::body(freshness)?) == reference.freshness_hash
            && challenge.submission_invocation_hash == reference.old_submission.hash()?
            && challenge.resolver_invocation_hash == reference.account_invocation_hash,
        "historical resolution freshness changed its original signed invocation",
    )
}

pub struct VerifiedHistoryPolicyRead {
    heads: Vec<PolicyStamp>,
    allowed_hashes: BTreeSet<ObjectHash>,
    decision_at_ms: u64,
    usage: ExecutionProofUsage,
}
impl VerifiedHistoryPolicyRead {
    pub fn target_heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
    pub fn domains(&self) -> Vec<PolicyDomain> {
        self.heads.iter().map(|head| head.domain.clone()).collect()
    }
    pub fn allowed_hashes(&self) -> &BTreeSet<ObjectHash> {
        &self.allowed_hashes
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
    pub fn decision_at_ms(&self) -> u64 {
        self.decision_at_ms
    }
}

pub fn verify_remote_execution_history_policy(
    state: &VerifiedPolicySet,
    reference: &RemoteExecutionHistoryReference,
    acceptance: &RemoteExecutionHistoryAcceptance,
    independent_pins: &[PolicyPin],
    local_c: &PublicKey,
    now: u64,
) -> Result<VerifiedHistoryPolicyRead> {
    let _signatures = ScopedSignatureVerification::new();
    let accepted =
        verify_remote_execution_history_acceptance(state, reference, acceptance, local_c, now)?;
    let known: Vec<_> = accepted
        .heads()
        .iter()
        .filter(|head| state.head(&head.domain).is_some())
        .cloned()
        .collect();
    let historical = remote_resolution_history::at_heads(state, &known)?;
    let mut budget = Budget::default();
    let mut dependencies = BTreeMap::new();
    let leaf = historical
        .grant(reference.grant_leaf())
        .ok_or(AuthorizationError::InvalidProof(
            "historical execution grant is missing",
        ))?;
    let submission = &reference.old_submission.payload;
    let resource = historical
        .resource(&submission.resource.space, &submission.resource.resource)
        .ok_or(AuthorizationError::InvalidProof(
            "historical ExecutionService is missing",
        ))?;
    let target = historical.verify_service_device(
        &submission.resource.space,
        &DeviceId::from_key(local_c),
        accepted.decision_at_ms(),
    )?;
    require(
        resource.payload.kind == ResourceKind::ExecutionService
            && resource.payload.host == submission.audience.service
            && target.payload.public_key == *local_c
            && target.payload.subject == Actor::Service(submission.audience.service.clone())
            && leaf.payload.scope == ResourceScope::Exact(submission.resource.clone())
            && leaf.payload.grantee == Grantee::Actor(submission.actor.clone())
            && leaf.payload.audience == submission.audience
            && leaf.payload.constraints.context == submission.context
            && !leaf.payload.constraints.require_account_actor
            && !leaf.payload.constraints.require_webauthn_uv
            && leaf.payload.remaining_delegations == 0
            && leaf.payload.expires_at_ms < u64::MAX
            && leaf.payload.actions.contains(&submission.action)
            && prepare::remote_actions(
                &leaf.payload.actions,
                reference.kind != RemoteExecutionHistoryKind::ExecutionResult,
            ),
        "historical policy read exceeds its original exact Service grant",
    )?;
    budget.verify(resource)?;
    historical.validate_certificate(
        target,
        accepted.decision_at_ms(),
        &mut budget,
        &mut dependencies,
        &mut BTreeSet::new(),
    )?;
    let chain = historical.grant_chain(Some(reference.grant_leaf()))?;
    historical.verify_grant_path(
        &chain,
        accepted.decision_at_ms(),
        &mut budget,
        &mut dependencies,
        true,
    )?;
    historical.expand_authorities(accepted.decision_at_ms(), &mut budget, &mut dependencies)?;
    require(
        dependencies.values().all(|head| {
            accepted.heads().contains(head) && submission.expected_policies.contains(head)
        }),
        "historical grant changed its signed exact heads",
    )?;
    for head in accepted.heads() {
        if state.head(&head.domain).is_some() {
            dependencies
                .entry(head.domain.clone())
                .or_insert_with(|| head.clone());
        }
    }
    let required: Vec<_> = dependencies.into_values().collect();
    let (heads, allowed_hashes) =
        remote_execution_policy::reference_closure(state, &required, independent_pins)?;
    let mut usage = accepted.usage;
    usage.record_budget(&budget)?;
    usage.record_chain_depth(chain.len());
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(VerifiedHistoryPolicyRead {
        heads,
        allowed_hashes,
        decision_at_ms: accepted.decision_at_ms,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorization::execution_observation::tests::retained_history_fixture;

    const NOW: u64 = 10_000;

    fn wire<T: Serialize>(value: &T) -> String {
        serde_json::to_string(value).unwrap()
    }

    #[test]
    fn retained_result_uses_its_original_acceptance_after_the_execution_grant_is_revoked() {
        let mut fixture = retained_history_fixture();
        let actual = wire(&fixture.actual);
        let body = wire(&fixture.body);
        let invocation = wire(&fixture.invocation);
        let binding = wire(&fixture.binding);
        let reference = derive_reference(&actual, &body, &invocation, &binding).unwrap();
        assert_eq!(
            reference.actual_hash,
            ObjectHash::body(&fixture.actual).unwrap()
        );
        assert_eq!(
            reference.expected_body_hash,
            ObjectHash::body(&fixture.body).unwrap()
        );
        assert_eq!(
            reference.expected_invocation_hash,
            ObjectHash::body(&fixture.invocation).unwrap()
        );
        assert_eq!(
            reference.expected_binding_hash,
            Some(ObjectHash::body(&fixture.binding).unwrap())
        );
        assert_eq!(
            reference.dispatch_envelope_hash,
            ObjectHash::body(&fixture.actual.handoff.handoff.project_dispatch).unwrap()
        );
        let target = PublicKey::of(&fixture.executor);
        let checked = verify_remote_execution_history_policy(
            &fixture.state,
            &reference,
            &fixture.acceptance,
            &fixture.pins,
            &target,
            NOW + 20,
        )
        .unwrap();
        assert_eq!(checked.decision_at_ms(), NOW);
        assert_ne!(
            checked.decision_at_ms(),
            fixture.actual.receipt.payload.issued_at_ms
        );
        let mut document = fixture
            .state
            .space(&reference.execution_resource().space)
            .unwrap()
            .clone();
        document.previous = fixture
            .state
            .head(&PolicyDomain::Space(document.space.clone()))
            .cloned();
        document.sequence += 1;
        document
            .grants
            .retain(|hash| hash != reference.grant_leaf());
        fixture
            .state
            .install_space(Signed::sign(document, &fixture.root).unwrap())
            .unwrap();
        for pin in &mut fixture.pins {
            pin.minimum_head = fixture.state.head(&pin.domain).cloned();
        }
        let rejected_now = fixture
            .state
            .grant_chain(Some(reference.grant_leaf()))
            .unwrap();
        assert!(
            fixture
                .state
                .verify_grant_path(
                    &rejected_now,
                    NOW + 20,
                    &mut Budget::default(),
                    &mut BTreeMap::new(),
                    true
                )
                .is_err()
        );
        let history = verify_remote_execution_history_policy(
            &fixture.state,
            &reference,
            &fixture.acceptance,
            &fixture.pins,
            &target,
            NOW + 20,
        )
        .unwrap();
        assert_eq!(history.decision_at_ms(), NOW);
        assert!(
            history
                .target_heads()
                .iter()
                .all(|head| fixture.state.head(&head.domain) == Some(head))
        );
        let mut wrong_pins = fixture.pins.clone();
        wrong_pins[0].genesis_root = reference.source.issuer.clone();
        assert!(
            verify_remote_execution_history_policy(
                &fixture.state,
                &reference,
                &fixture.acceptance,
                &wrong_pins,
                &target,
                NOW + 20
            )
            .is_err()
        );
        let mut wrong_body = fixture.body.clone();
        wrong_body["authorization_id"] = Value::String("another-operation".into());
        assert!(derive_reference(&actual, &wire(&wrong_body), &invocation, &binding).is_err());
        assert!(derive_reference(&actual, &body, &invocation, "null").is_err());
        let mut wrong_binding = fixture.binding.clone();
        wrong_binding.binding.generation += 1;
        assert!(derive_reference(&actual, &body, &invocation, &wire(&wrong_binding)).is_err());
    }

    #[test]
    fn retained_acceptance_requires_exact_canonical_freshness_batches() {
        let fixture = retained_history_fixture();
        let reference = derive_reference(
            &wire(&fixture.actual),
            &wire(&fixture.body),
            &wire(&fixture.invocation),
            &wire(&fixture.binding),
        )
        .unwrap();
        let target = PublicKey::of(&fixture.executor);
        verify_remote_execution_history_acceptance(
            &fixture.state,
            &reference,
            &fixture.acceptance,
            &target,
            NOW + 20,
        )
        .unwrap();
        let RemoteExecutionHistoryAcceptance::ExecutionResult { freshness, .. } =
            &fixture.acceptance
        else {
            panic!("fixture must retain an original executor admission")
        };
        let receipts = &freshness.receipts;
        let batch = receipts
            .iter()
            .position(|receipt| {
                receipt.payload.currents.len() > 1
                    && receipt
                        .payload
                        .currents
                        .iter()
                        .all(|head| fixture.state.head(&head.domain).is_some())
            })
            .expect("fixture must batch known heads under one authority");
        let assert_rejected = |receipts: Vec<Signed<FreshnessReceipt>>| {
            let mut acceptance = fixture.acceptance.clone();
            let RemoteExecutionHistoryAcceptance::ExecutionResult {
                receipt, freshness, ..
            } = &mut acceptance
            else {
                unreachable!()
            };
            freshness.receipts = receipts;
            receipt.payload.admission_hash = ObjectHash::body(freshness).unwrap();
            *receipt = Signed::sign(receipt.payload.clone(), &fixture.executor).unwrap();
            assert!(
                verify_remote_execution_history_acceptance(
                    &fixture.state,
                    &reference,
                    &acceptance,
                    &target,
                    NOW + 20,
                )
                .is_err()
            );
        };
        let mut changed = receipts.clone();
        changed.push(receipts[batch].clone());
        assert_rejected(changed);
        let mut changed = receipts.clone();
        changed.remove(batch);
        assert_rejected(changed);
        let mut changed = receipts.clone();
        let mut payload = changed[batch].payload.clone();
        payload.currents.swap(0, 1);
        changed[batch] = Signed::sign(payload, &fixture.executor).unwrap();
        assert_rejected(changed);
        let mut changed = receipts.clone();
        let mut payload = changed[batch].payload.clone();
        payload.currents.push(payload.currents[0].clone());
        payload
            .currents
            .sort_by(|left, right| left.domain.cmp(&right.domain));
        changed[batch] = Signed::sign(payload, &fixture.executor).unwrap();
        assert_rejected(changed);
        let mut changed = receipts.clone();
        let mut payload = changed[batch].payload.clone();
        let mut extra = payload.currents[0].clone();
        extra.domain = PolicyDomain::Account(AccountId::from_root(&PublicKey::of(
            &Identity::generate().unwrap(),
        )));
        payload.currents.push(extra);
        payload
            .currents
            .sort_by(|left, right| left.domain.cmp(&right.domain));
        changed[batch] = Signed::sign(payload, &fixture.executor).unwrap();
        assert_rejected(changed);
        let mut changed = receipts.clone();
        changed[batch] =
            Signed::sign(changed[batch].payload.clone(), &fixture.project_node).unwrap();
        assert_rejected(changed);
        let mut changed = receipts.clone();
        let unknown = changed
            .iter()
            .position(|receipt| {
                receipt
                    .payload
                    .currents
                    .iter()
                    .all(|head| fixture.state.head(&head.domain).is_none())
            })
            .expect("fixture must retain the source authority's unknown domains");
        let mut payload = changed[unknown].payload.clone();
        payload
            .currents
            .extend(changed[batch].payload.currents.clone());
        payload
            .currents
            .sort_by(|left, right| left.domain.cmp(&right.domain));
        changed[unknown] = Signed::sign(payload, &fixture.project_node).unwrap();
        changed.remove(batch);
        assert_rejected(changed);
    }

    #[test]
    fn terminal_proof_time_requires_an_existing_nonempty_window_intersection() {
        let fixture = retained_history_fixture();
        let reference = derive_reference(
            &wire(&fixture.actual),
            &wire(&fixture.body),
            &wire(&fixture.invocation),
            &wire(&fixture.binding),
        )
        .unwrap();
        let RemoteExecutionHistoryAcceptance::ExecutionResult {
            freshness: original,
            ..
        } = fixture.acceptance
        else {
            panic!("fixture must retain an original executor admission")
        };
        let challenge = ExecutionHandoffResolutionChallenge {
            version: AUTH_VERSION,
            resolution_hash: ObjectHash::body(&"resolution").unwrap(),
            resolver_invocation_hash: reference.account_invocation_hash.clone(),
            submission_invocation_hash: reference.old_submission.hash().unwrap(),
            policies: original.challenge.policies,
            nonce: Nonce::random().unwrap(),
            issued_at_ms: NOW + 5,
            expires_at_ms: NOW + 80,
        };
        let mut freshness = ExecutionHandoffResolutionFreshness {
            challenge,
            receipts: original.receipts,
        };
        for receipt in &mut freshness.receipts {
            receipt.payload.issued_at_ms = NOW + 20;
            receipt.payload.expires_at_ms = NOW + 70;
        }
        assert_eq!(
            proof_time(&reference, &freshness, &fixture.actual.receipt, NOW + 20).unwrap(),
            NOW + 20
        );
        assert!(proof_time(&reference, &freshness, &fixture.actual.receipt, NOW + 19).is_err());
        freshness.receipts[0].payload.expires_at_ms = NOW + 20;
        assert!(proof_time(&reference, &freshness, &fixture.actual.receipt, NOW + 40).is_err());
    }
}
