//! Current checks confined to an executor-accepted immutable Submit operation.

use super::super::{
    events::StoredTransport,
    execution_handoff::{
        ExecutionHandoffAdmissionReceipt, ExecutionHandoffChallenge, ExecutionHandoffFreshness,
        verify_execution_handoff_admission_checkpoint,
    },
    remote_execution_history::RemoteExecutionHistoryAcceptance,
};
use super::*;

fn submit_acceptance<'a>(
    acceptance: &'a RemoteExecutionHistoryAcceptance,
) -> Result<(
    &'a Signed<ExecutionHandoffAdmissionReceipt>,
    &'a ExecutionHandoffFreshness,
    &'a StoredTransport,
)> {
    match acceptance {
        RemoteExecutionHistoryAcceptance::ExecutionResult {
            receipt,
            freshness,
            transport,
        } => Ok((receipt, freshness, transport)),
        _ => Err(AuthorizationError::InvalidProof(
            "Task operation requires its signed executor Submit acceptance",
        )),
    }
}

fn accepted_handoff_at(
    state: &VerifiedPolicySet,
    wire: &SignedExecutionHandoff,
    acceptance: &RemoteExecutionHistoryAcceptance,
    now: u64,
) -> Result<VerifiedExecutionHandoff> {
    carrier(acceptance)?;
    let (receipt, freshness, transport) = submit_acceptance(acceptance)?;
    require(
        transport.clone().into_evidence()
            == TransportEvidence::VerifiedPeer {
                origin: wire.invocation.payload.presenter.clone(),
            },
        "Task operation changed its accepted Project transport",
    )?;
    let checkpoint = verify_execution_handoff_admission_checkpoint(
        wire,
        receipt,
        freshness,
        transport,
        receipt.payload.admitted_at_ms,
    )?;
    let event: Event = serde_json::from_str(&wire.handoff.project_dispatch.canonical_body)
        .map_err(|_| AuthorizationError::InvalidProof("invalid accepted Task dispatch"))?;
    let EventCause::Invocation { admission } = event.cause else {
        return Err(AuthorizationError::InvalidProof(
            "Task operation has no original admitted source request",
        ));
    };
    let facts = request_facts(&admission.invocation.payload);
    let admitted = remote_resolution_history::admitted(state, &admission, &facts)?;
    let original = state.verify_operation_authorization(
        &admitted,
        &facts,
        &admission.transport.clone().into_evidence(),
        now,
    )?;
    let verified = state.verify_execution_handoff(
        wire,
        &original,
        &transport.clone().into_evidence(),
        Some(&checkpoint),
        now,
    )?;
    require(
        verified.original().phase == AuthorizationPhase::Operation
            && verified.submission().phase == AuthorizationPhase::Operation,
        "Task operation cannot authorize a new admission",
    )?;
    Ok(verified)
}

pub(super) struct VerifiedTaskBusinessOperationReceipt {
    authorization: VerifiedExecutionHandoff,
    acceptance_envelope_hash: ObjectHash,
    usage: ExecutionProofUsage,
}

impl VerifiedTaskBusinessOperationReceipt {
    pub(super) fn authorization(&self) -> &VerifiedExecutionHandoff {
        &self.authorization
    }

    pub(super) fn acceptance_envelope_hash(&self) -> &ObjectHash {
        &self.acceptance_envelope_hash
    }

    pub(super) fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
}

pub(super) fn verify_task_business_receipt_operation(
    current: &VerifiedTaskCurrentPolicy,
    header: &Signed<TaskBusinessPolicyHeader>,
    wire: &SignedExecutionHandoff,
    acceptance: &RemoteExecutionHistoryAcceptance,
    challenge: &ExecutionHandoffChallenge,
    now: u64,
) -> Result<VerifiedTaskBusinessOperationReceipt> {
    let _signatures = ScopedSignatureVerification::new();
    carrier(header)?;
    carrier(challenge)?;
    let raw = TaskBusinessWire::Submit(wire.clone());
    let scope = raw.scope()?;
    let (receipt, freshness, transport) = submit_acceptance(acceptance)?;
    verify_execution_handoff_admission_checkpoint(
        wire,
        receipt,
        freshness,
        transport,
        receipt.payload.admitted_at_ms,
    )?;
    header.verify_signature()?;
    require(
        header.payload.version == AUTH_VERSION
            && header.payload.scope == scope
            && header.issuer == scope.source_node_public_key
            && header
                .payload
                .source_heads
                .iter()
                .all(|head| freshness.challenge.policies.contains(head))
            && matches!(
                challenge.stage,
                CheckStage::Start | CheckStage::Renew | CheckStage::RemoteRead
            ),
        "Task operation receipt changed its accepted source, exact heads or stage",
    )?;
    window(
        header.payload.issued_at_ms,
        header.payload.expires_at_ms,
        receipt.payload.admitted_at_ms,
    )?;
    check_source_policy_scope(current, &raw, &scope, now)?;
    let mut usage = ExecutionProofUsage::default();
    let (approved, _) = source_grant(
        current.state(),
        &scope,
        &scope.target_public_key,
        now,
        &mut usage,
    )?;
    let project_head = current
        .state()
        .head(&PolicyDomain::Space(scope.source_project.space.clone()))
        .ok_or(AuthorizationError::PolicyConflict)?;
    let (heads, _) = remote_execution_policy::reference_closure(
        current.state(),
        std::slice::from_ref(project_head),
        &approved.pins,
    )?;
    if heads != header.payload.source_heads {
        return Err(AuthorizationError::NeedPolicySync(heads));
    }
    let verified = accepted_handoff_at(current.state(), wire, acceptance, now)?;
    let expected = ExecutionHandoffChallenge::bound_to(
        &verified,
        challenge.stage,
        challenge.nonce.clone(),
        challenge.issued_at_ms,
        challenge.expires_at_ms,
    )?;
    validate_window(challenge.issued_at_ms, challenge.expires_at_ms, now)?;
    if challenge.policies != expected.policies {
        return Err(AuthorizationError::NeedPolicySync(expected.policies));
    }
    require(
        challenge == &expected,
        "Task operation receipt changed its complete typed challenge",
    )?;
    usage.merge(verified.proof_usage());
    usage.record_signed(header)?;
    usage.record_metadata(challenge)?;
    finish_usage(&mut usage)?;
    Ok(VerifiedTaskBusinessOperationReceipt {
        authorization: verified,
        acceptance_envelope_hash: ObjectHash::body(acceptance)?,
        usage,
    })
}

#[derive(Clone)]
pub struct TaskBusinessOperationCheck {
    original: VerifiedTaskBusinessOriginalSource,
    combined: VerifiedPolicySnapshot,
    authorization: VerifiedExecutionHandoff,
    primary_heads: Vec<PolicyStamp>,
    checked_at_ms: u64,
    usage: ExecutionProofUsage,
}

pub fn begin_task_business_operation_current(
    primary: &VerifiedPolicySet,
    original: &VerifiedTaskBusinessOriginalSource,
    local_c: &PublicKey,
    saved_floors: &[PolicyPin],
    now: u64,
) -> Result<TaskBusinessOperationCheck> {
    let _signatures = ScopedSignatureVerification::new();
    let TaskBusinessWire::Submit(wire) = original.wire() else {
        return Err(AuthorizationError::InvalidProof(
            "Task execution lease requires its original accepted Submit",
        ));
    };
    let mut usage = ExecutionProofUsage::default();
    let (approved, _) = source_grant(primary, original.scope(), local_c, now, &mut usage)?;
    let selected: Vec<_> = approved
        .pins
        .iter()
        .filter(|pin| {
            original
                .foreign_heads()
                .iter()
                .any(|head| head.domain == pin.domain)
        })
        .cloned()
        .collect();
    original
        .source_snapshot()
        .with_independent_pins(&selected)?;
    let (effective, required) = effective_pins(original.pins(), saved_floors)?;
    for head in required
        .iter()
        .chain(effective.iter().filter_map(|pin| pin.minimum_head.as_ref()))
    {
        if !original.source_snapshot().state().roots.contains_key(head) {
            return Err(AuthorizationError::NeedPolicySync(vec![head.clone()]));
        }
    }
    original
        .source_snapshot()
        .with_independent_pins(&effective)?;
    let local_authority = local_authority(primary, original.scope())?;
    let local = policy_import::native_task_business_primary_snapshot(&local_authority)?;
    let combined = policy_import::merge_same_head_snapshots(&local, original.source_snapshot())?;
    let authorization = accepted_handoff_at(combined.state(), wire, original.acceptance(), now)?;
    usage.merge(authorization.proof_usage());
    usage.record_verified_object(original.header())?;
    finish_usage(&mut usage)?;
    Ok(TaskBusinessOperationCheck {
        original: original.clone(),
        combined,
        authorization,
        primary_heads: local_authority.heads,
        checked_at_ms: now,
        usage,
    })
}

impl TaskBusinessOperationCheck {
    pub fn wire(&self) -> &SignedExecutionHandoff {
        self.authorization.wire()
    }

    pub fn wire_hash(&self) -> &ObjectHash {
        self.authorization.hash()
    }

    pub fn binding(&self) -> &task_binding::SignedBinding {
        self.authorization.binding()
    }

    pub fn dependencies(&self) -> &[PolicyStamp] {
        self.authorization.dependencies()
    }

    pub fn primary_heads(&self) -> &[PolicyStamp] {
        &self.primary_heads
    }

    pub fn foreign_heads(&self) -> &[PolicyStamp] {
        self.original.foreign_heads()
    }

    pub fn checked_at_ms(&self) -> u64 {
        self.checked_at_ms
    }

    pub fn expires_at_ms(&self) -> u64 {
        self.authorization.expires_at_ms()
    }

    pub fn original_header_envelope_hash(&self) -> Result<ObjectHash> {
        self.original.header_envelope_hash()
    }

    pub fn acceptance_envelope_hash(&self) -> Result<ObjectHash> {
        self.original.acceptance_envelope_hash()
    }

    pub fn admission_receipt(&self) -> Result<&Signed<ExecutionHandoffAdmissionReceipt>> {
        submit_acceptance(self.original.acceptance()).map(|(receipt, _, _)| receipt)
    }

    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }

    pub fn issue_challenge(
        &self,
        stage: CheckStage,
        nonce: Nonce,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Result<ExecutionHandoffChallenge> {
        require(
            matches!(stage, CheckStage::Start | CheckStage::Renew),
            "accepted Task operation cannot issue a new admission challenge",
        )?;
        ExecutionHandoffChallenge::new(
            &self.authorization,
            stage,
            nonce,
            issued_at_ms,
            expires_at_ms,
        )
    }

    pub fn finish(
        self,
        proof: &ExecutionHandoffFreshness,
        now: u64,
    ) -> Result<VerifiedTaskBusinessOperationCurrent> {
        let _signatures = ScopedSignatureVerification::new();
        carrier(proof)?;
        require(
            now >= self.checked_at_ms
                && matches!(proof.challenge.stage, CheckStage::Start | CheckStage::Renew),
            "Task operation changed its checked clock or execution lease stage",
        )?;
        for head in self.original.foreign_heads() {
            if !proof.challenge.policies.contains(head) {
                return Err(AuthorizationError::NeedPolicySync(
                    self.authorization.dependencies().to_vec(),
                ));
            }
        }
        let fresh = self.combined.state().verify_execution_handoff_freshness(
            &self.authorization,
            proof,
            now,
        )?;
        let mut usage = self.usage;
        usage.merge(fresh.proof_usage());
        finish_usage(&mut usage)?;
        Ok(VerifiedTaskBusinessOperationCurrent {
            original: self.original,
            authorization: self.authorization,
            freshness: proof.clone(),
            primary_heads: self.primary_heads,
            checked_at_ms: now,
            deadline_ms: fresh.expires_at_ms(),
            usage,
        })
    }
}

#[derive(Clone)]
pub struct VerifiedTaskBusinessOperationCurrent {
    original: VerifiedTaskBusinessOriginalSource,
    authorization: VerifiedExecutionHandoff,
    freshness: ExecutionHandoffFreshness,
    primary_heads: Vec<PolicyStamp>,
    checked_at_ms: u64,
    deadline_ms: u64,
    usage: ExecutionProofUsage,
}

impl VerifiedTaskBusinessOperationCurrent {
    pub fn handoff_authorization(&self) -> &VerifiedExecutionHandoff {
        &self.authorization
    }

    pub fn wire(&self) -> &SignedExecutionHandoff {
        self.authorization.wire()
    }

    pub fn wire_hash(&self) -> &ObjectHash {
        self.authorization.hash()
    }

    pub fn admission_receipt(&self) -> Result<&Signed<ExecutionHandoffAdmissionReceipt>> {
        submit_acceptance(self.original.acceptance()).map(|(receipt, _, _)| receipt)
    }

    pub fn accepted_at_ms(&self) -> u64 {
        self.original.accepted_at_ms()
    }

    pub fn binding(&self) -> &task_binding::SignedBinding {
        self.authorization.binding()
    }

    pub fn freshness(&self) -> &ExecutionHandoffFreshness {
        &self.freshness
    }

    pub fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }

    pub fn checked_at_ms(&self) -> u64 {
        self.checked_at_ms
    }

    pub fn dependencies(&self) -> &[PolicyStamp] {
        self.authorization.dependencies()
    }

    pub fn primary_heads(&self) -> &[PolicyStamp] {
        &self.primary_heads
    }

    pub fn foreign_heads(&self) -> &[PolicyStamp] {
        self.original.foreign_heads()
    }

    pub fn expires_at_ms(&self) -> u64 {
        self.authorization.expires_at_ms()
    }

    pub fn original_header_envelope_hash(&self) -> Result<ObjectHash> {
        self.original.header_envelope_hash()
    }

    pub fn acceptance_envelope_hash(&self) -> Result<ObjectHash> {
        self.original.acceptance_envelope_hash()
    }

    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
}
