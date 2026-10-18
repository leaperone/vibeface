//! Cross-Node recovery consumes genuine policy history and exact durable approvals.
use super::*;
use crate::authorization::{
    execution_handoff::ExecutionProofUsage,
    policy_import::VerifiedPolicySnapshot,
    policy_proof::{CachedPolicyProof, policy_entry_payload_usage, policy_entry_usage_with_cache},
};

fn cross_request(read: &RecoveryReadRequest) -> Result<&Signed<CrossNodeRecoveryRequest>> {
    match read {
        RecoveryReadRequest::CrossNode(value) => Ok(value),
        RecoveryReadRequest::Account(_) => Err(AuthorizationError::InvalidProof(
            "cross-node recovery requires its original cross-node read",
        )),
    }
}

fn exact_heads(read: &RecoveryReadRequest, state: &VerifiedPolicySet) -> Result<Vec<PolicyStamp>> {
    [
        PolicyDomain::Account(read.account().clone()),
        PolicyDomain::Space(read.space().clone()),
    ]
    .into_iter()
    .map(|domain| {
        state
            .head(&domain)
            .cloned()
            .ok_or(AuthorizationError::PolicyConflict)
    })
    .collect()
}

fn verify_context_reference(
    possession: &StreamedCrossNodeRecoveryPossessionRequest,
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    now: u64,
) -> Result<()> {
    let original = cross_request(read)?;
    let reference = &possession.context;
    let value = &snapshot.payload;
    let sealed = &witness.payload;
    let source = read.source_node_public_key();
    let request_hash = read.request_hash()?;
    require(
        possession.request == *original
            && reference.request_hash == request_hash
            && reference.snapshot_id == value.snapshot_id
            && reference.snapshot_hash == snapshot.hash()?
            && reference.witness_hash == witness.hash()?
            && value.version == AUTH_VERSION
            && sealed.version == AUTH_VERSION
            && value.kind == RecoveryContextKind::CrossNode
            && sealed.kind == value.kind
            && value.request_hash == request_hash
            && sealed.request_hash == request_hash
            && value.saved_pins_hash == read.saved_pins_hash()?
            && snapshot.issuer == *source
            && witness.issuer == *source
            && value.source_node_public_key == *source
            && sealed.source_node_public_key == *source
            && value.claim == original.payload.target_claim
            && value.claim == possession.node_request.claim
            && sealed.claim_hash == value.claim.hash()?
            && sealed.snapshot_id == value.snapshot_id
            && value.target_heads == exact_heads(read, before)?
            && sealed.target_heads == value.target_heads
            && sealed.page_count > 0
            && sealed.entry_count <= sealed.page_count
            && value.issued_at_ms >= read.issued_at_ms()
            && value.expires_at_ms <= read.expires_at_ms()
            && sealed.issued_at_ms >= value.issued_at_ms
            && sealed.expires_at_ms <= value.expires_at_ms,
        "cross-node recovery changed its exact completed public context",
    )?;
    validate_window(value.issued_at_ms, value.expires_at_ms, now)?;
    validate_window(sealed.issued_at_ms, sealed.expires_at_ms, now)?;
    snapshot.verify_signature()?;
    witness.verify_signature()?;
    require(
        value.target_heads
            == [
                possession.node_request.expected_account_head.clone(),
                possession.node_request.expected_space_head.clone(),
            ]
            && possession.node_request.account_recovery.payload.account == *read.account()
            && possession
                .node_request
                .account_recovery
                .payload
                .current_root
                == original.payload.new_root_public_key
            && possession.node_request.account_recovery.issuer == original.issuer,
        "cross-node proposal changed the exact approved account or keys",
    )?;
    read.validate_saved_pins()?;
    let account = before
        .account(read.account())
        .ok_or(AuthorizationError::Revoked)?;
    let space = before
        .space(read.space())
        .ok_or(AuthorizationError::Revoked)?;
    require(
        account.authority.public_key == *source
            && space.authority.public_key == *source
            && account.recovery.threshold == 1
            && account.recovery.keys == [original.issuer.clone()]
            && read.saved_pins().iter().all(|pin| {
                pin.genesis_root == account.genesis_root
                    && pin
                        .minimum_head
                        .as_ref()
                        .is_some_and(|head| before.roots.contains_key(head))
            }),
        "cross-node approved read changed its current recovery or source authority",
    )?;
    before.ensure_personal_current(space)?;
    validate_window(read.issued_at_ms(), read.expires_at_ms(), now)?;
    validate_claim(
        &original.payload.target_claim,
        &original.payload.target_claim.payload.node_public_key,
        &original.payload.target_claim.payload.claim_nonce,
        now,
    )?;
    original.verify_signature()?;
    before
        .verify_service_device(read.space(), &DeviceId::from_key(source), now)
        .map(|_| ())
}

fn verify_context_binding(
    possession: &StreamedCrossNodeRecoveryPossessionRequest,
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    now: u64,
) -> Result<()> {
    verify_context_reference(possession, read, snapshot, witness, before, now)?;
    cross_node::verify_possession_in_state(
        cross_request(read)?,
        &possession.node_request,
        &possession.source_certificate,
        before,
        now,
    )
}

fn possession_usage(
    possession: &StreamedCrossNodeRecoveryPossessionRequest,
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    cached: Option<&CachedPolicyProof>,
    carrier_bytes: usize,
) -> Result<RecoveryProofUsage> {
    let mut usage = RecoveryProofUsage::new(carrier_bytes)?;
    usage.record_snapshot(snapshot)?;
    usage.record_witness(witness)?;
    usage.record_request(read)?;
    usage.record_cross_possession_cached(possession, before, cached)?;
    Ok(usage)
}

impl RecoveryProofUsage {
    pub fn record_cross_possession_carrier(
        &mut self,
        possession: &StreamedCrossNodeRecoveryPossessionRequest,
    ) -> Result<()> {
        self.record_request(&RecoveryReadRequest::CrossNode(possession.request.clone()))?;
        let mut extra = ExecutionProofUsage::default();
        extra.record_signed(&possession.node_request.claim)?;
        extra.record_metadata(&(
            &possession.context,
            &possession.node_request.expected_account_head,
            &possession.node_request.expected_space_head,
            &possession.node_request.node_certificate,
            &possession.source_certificate,
        ))?;
        self.merge_usage(&extra);
        self.merge_usage(&policy_entry_payload_usage(&JournalEntry::Account(
            possession.node_request.account_recovery.clone(),
        ))?);
        Ok(())
    }
    pub fn record_cross_possession_in_context(
        &mut self,
        possession: &StreamedCrossNodeRecoveryPossessionRequest,
        context: &VerifiedRecoveryPolicyContext,
    ) -> Result<()> {
        self.record_snapshot(context.snapshot())?;
        self.record_witness(context.witness())?;
        let cached = CachedPolicyProof::from_verified_prefix(
            context.verified_state(),
            context.policy().journal(),
        )?;
        self.record_cross_possession_cached(possession, context.verified_state(), Some(&cached))
    }
    pub fn record_cross_possession_with_source_proof(
        &mut self,
        possession: &StreamedCrossNodeRecoveryPossessionRequest,
        before: &VerifiedPolicySet,
        source: &VerifiedRecoverySourceProof,
    ) -> Result<()> {
        self.record_cross_possession_cached(
            possession,
            before,
            Some(source.cache_for_context(&possession.context, before)?),
        )
    }
    pub fn record_cross_possession(
        &mut self,
        possession: &StreamedCrossNodeRecoveryPossessionRequest,
        before: &VerifiedPolicySet,
    ) -> Result<()> {
        self.record_cross_possession_cached(possession, before, None)
    }
    fn record_cross_possession_cached(
        &mut self,
        possession: &StreamedCrossNodeRecoveryPossessionRequest,
        before: &VerifiedPolicySet,
        cached: Option<&CachedPolicyProof>,
    ) -> Result<()> {
        let read = RecoveryReadRequest::CrossNode(possession.request.clone());
        self.record_request(&read)?;
        self.record_current_source(&read, before)?;
        self.record_node_request_cached(
            &StreamedRecoveryNodeRequest {
                context: possession.context.clone(),
                request: possession.node_request.clone(),
            },
            before,
            cached,
        )?;
        let mut extra = ExecutionProofUsage::default();
        extra.record_metadata(&possession.source_certificate)?;
        self.merge_usage(&extra);
        Ok(())
    }
    pub fn record_cross_commit(
        &mut self,
        input: &StreamedCrossNodeRecoveryCommitRequest,
        before: &VerifiedPolicySet,
    ) -> Result<()> {
        self.record_cross_possession(&input.possession, before)?;
        record_delta_usage(self, input, before, None)
    }
    pub fn record_cross_commit_with_source_proof(
        &mut self,
        input: &StreamedCrossNodeRecoveryCommitRequest,
        before: &VerifiedPolicySet,
        source: &VerifiedRecoverySourceProof,
    ) -> Result<()> {
        let cached = source.cache_for_context(&input.possession.context, before)?;
        self.record_cross_possession_cached(&input.possession, before, Some(cached))?;
        record_delta_usage(self, input, before, Some(cached))
    }
    pub fn record_cross_completion(
        &mut self,
        receipt: &Signed<StreamedCrossNodeRecoveryCommitReceipt>,
    ) -> Result<()> {
        let mut extra = ExecutionProofUsage::default();
        extra.record_signed(receipt)?;
        self.merge_usage(&extra);
        Ok(())
    }
    pub fn record_cross_approval(
        &mut self,
        approval: &Signed<CrossNodeRecoveryImportApproval>,
    ) -> Result<()> {
        let mut extra = ExecutionProofUsage::default();
        extra.record_signed(approval)?;
        self.merge_usage(&extra);
        Ok(())
    }
    pub fn record_cross_committed_carrier(
        &mut self,
        committed: &StreamedCrossNodeRecoveryCommitted,
    ) -> Result<()> {
        let input = &committed.request;
        self.record_request(&RecoveryReadRequest::CrossNode(
            input.possession.request.clone(),
        ))?;
        let mut extra = ExecutionProofUsage::default();
        extra.record_signed(&input.possession.node_request.claim)?;
        extra.record_metadata(&(
            &input.possession.context,
            &input.possession.node_request.expected_account_head,
            &input.possession.node_request.expected_space_head,
            &input.possession.node_request.node_certificate,
            &input.possession.source_certificate,
            &input.pins,
            &input.grant_leaf,
        ))?;
        self.merge_usage(&extra);
        self.merge_usage(&policy_entry_payload_usage(&JournalEntry::Account(
            input.possession.node_request.account_recovery.clone(),
        ))?);
        for entry in &input.journal_delta {
            self.merge_usage(&policy_entry_payload_usage(entry)?);
        }
        self.record_cross_completion(&committed.receipt)
    }
}

fn record_delta_usage(
    usage: &mut RecoveryProofUsage,
    input: &StreamedCrossNodeRecoveryCommitRequest,
    before: &VerifiedPolicySet,
    cached: Option<&CachedPolicyProof>,
) -> Result<()> {
    let mut metadata = ExecutionProofUsage::default();
    metadata.record_metadata(&(&input.pins, &input.grant_leaf))?;
    usage.merge_usage(&metadata);
    let mut staged = before.clone();
    for entry in &input.journal_delta {
        usage.merge_usage(&policy_entry_usage_with_cache(&staged, entry, cached)?);
        apply_policy_entry(&mut staged, &input.pins, entry.clone())?;
    }
    Ok(())
}

fn sign_possession(certificate: &DeviceCertificate, signer: &Identity) -> Result<Signature> {
    require(
        certificate.public_key == PublicKey::of(signer),
        "cross-node possession signer changed",
    )?;
    Signature::parse(
        &signer
            .sign("device-possession-v2", &certificate.possession_bytes()?)
            .map_err(|_| {
                AuthorizationError::InvalidProof("cross-node possession signing failed")
            })?,
    )
}

pub fn sign_cross_node_recovery_source_possession_in_state(
    possession: &StreamedCrossNodeRecoveryPossessionRequest,
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    source: &Identity,
    now: u64,
) -> Result<Signature> {
    sign_source_possession_cached(
        possession, read, snapshot, witness, before, source, now, None,
    )
}

pub fn sign_cross_node_recovery_source_possession_with_source_proof(
    possession: &StreamedCrossNodeRecoveryPossessionRequest,
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    source_proof: &VerifiedRecoverySourceProof,
    source: &Identity,
    now: u64,
) -> Result<Signature> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    sign_source_possession_cached(
        possession,
        read,
        snapshot,
        witness,
        before,
        source,
        now,
        Some(source_proof.cache_for(read, before)?),
    )
}

fn sign_source_possession_cached(
    possession: &StreamedCrossNodeRecoveryPossessionRequest,
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    source: &Identity,
    now: u64,
    cached: Option<&CachedPolicyProof>,
) -> Result<Signature> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let mut usage = possession_usage(
        possession,
        read,
        snapshot,
        witness,
        before,
        cached,
        canonical(possession)?.len(),
    )?;
    require(
        PublicKey::of(source) == *read.source_node_public_key(),
        "cross-node possession reached another source",
    )?;
    verify_context_binding(possession, read, snapshot, witness, before, now)?;
    let proof = sign_possession(&possession.source_certificate, source)?;
    let mut metadata = ExecutionProofUsage::default();
    metadata.record_metadata(&proof)?;
    usage.merge_usage(&metadata);
    usage.finish()?;
    Ok(proof)
}

pub fn approve_cross_node_recovery_source_proposal(
    possession: &StreamedCrossNodeRecoveryPossessionRequest,
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    source: &Identity,
    accepted_prefix_commitment: &ObjectHash,
    now: u64,
) -> Result<(Signature, Signed<RecoverySourceProposalApproval>)> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let proof = sign_cross_node_recovery_source_possession_in_state(
        possession, read, snapshot, witness, before, source, now,
    )?;
    let approval = approval::sign_source_proposal_approval_after_verification(
        read,
        &possession.context,
        &ObjectHash::body(possession)?,
        &possession.node_request.account_recovery,
        &proof,
        before,
        source,
        accepted_prefix_commitment,
        now,
    )?;
    let mut usage = possession_usage(
        possession,
        read,
        snapshot,
        witness,
        before,
        None,
        canonical(&(possession, &proof, &approval))?.len(),
    )?;
    usage.record_source_proposal_approval(&approval)?;
    usage.finish()?;
    Ok((proof, approval))
}

pub fn approve_cross_node_recovery_import(
    context: &VerifiedRecoveryPolicyContext,
    possession: &StreamedCrossNodeRecoveryPossessionRequest,
    target: &Identity,
    expected_nonce: &Nonce,
    now: u64,
) -> Result<(Signature, Signed<CrossNodeRecoveryImportApproval>)> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let read = context.request();
    let original = cross_request(read)?;
    let r = &original.payload;
    require(
        r.target_claim.payload.node_public_key == PublicKey::of(target)
            && &r.target_claim.payload.claim_nonce == expected_nonce,
        "cross-node import approval changed the local target claim",
    )?;
    verify_context_binding(
        possession,
        read,
        context.snapshot(),
        context.witness(),
        context.verified_state(),
        now,
    )?;
    let proof = sign_possession(&possession.node_request.node_certificate, target)?;
    let approval = Signed::sign(
        CrossNodeRecoveryImportApproval {
            version: AUTH_VERSION,
            context: context.reference()?,
            account: r.account.clone(),
            space: r.space.clone(),
            new_device_public_key: r.new_device_public_key.clone(),
            new_root_public_key: r.new_root_public_key.clone(),
            source_node_public_key: r.source_node_public_key.clone(),
            target_node_public_key: r.target_claim.payload.node_public_key.clone(),
            possession_hash: ObjectHash::body(possession)?,
            claim_hash: r.target_claim.hash()?,
            approved_at_ms: now,
            expires_at_ms: context.witness().payload.expires_at_ms,
        },
        target,
    )?;
    let cached = CachedPolicyProof::from_verified_prefix(
        context.verified_state(),
        context.policy().journal(),
    )?;
    let mut usage = possession_usage(
        possession,
        read,
        context.snapshot(),
        context.witness(),
        context.verified_state(),
        Some(&cached),
        canonical(&(possession, &proof, &approval))?.len(),
    )?;
    let mut extra = ExecutionProofUsage::default();
    extra.record_signed(&approval)?;
    extra.record_metadata(&proof)?;
    usage.merge_usage(&extra);
    usage.finish()?;
    Ok((proof, approval))
}

pub struct VerifiedStreamedCrossNodeRecoveryCommit {
    request: StreamedCrossNodeRecoveryCommitRequest,
    state: VerifiedPolicySet,
    expected_heads: Vec<PolicyStamp>,
    committed_heads: Vec<PolicyStamp>,
    input_hash: ObjectHash,
    verified_at_ms: u64,
    proof_usage: RecoveryProofCounts,
    proof: RecoveryProofUsage,
    approved_account: Option<approval::LocallyApprovedRecoveryAccount>,
}

impl VerifiedStreamedCrossNodeRecoveryCommit {
    pub fn request(&self) -> &StreamedCrossNodeRecoveryCommitRequest {
        &self.request
    }
    pub fn expected_heads(&self) -> &[PolicyStamp] {
        &self.expected_heads
    }
    pub fn committed_heads(&self) -> &[PolicyStamp] {
        &self.committed_heads
    }
    pub fn input_hash(&self) -> &ObjectHash {
        &self.input_hash
    }
    pub fn account(&self) -> &AccountId {
        &self.request.possession.request.payload.account
    }
    pub fn space(&self) -> &SpaceId {
        &self.request.possession.request.payload.space
    }
    pub fn source_node_public_key(&self) -> &PublicKey {
        &self
            .request
            .possession
            .request
            .payload
            .source_node_public_key
    }
    pub fn target_node_public_key(&self) -> &PublicKey {
        &self
            .request
            .possession
            .request
            .payload
            .target_claim
            .payload
            .node_public_key
    }
    pub fn new_device_public_key(&self) -> &PublicKey {
        &self
            .request
            .possession
            .request
            .payload
            .new_device_public_key
    }
    pub fn proof_usage(&self) -> &RecoveryProofCounts {
        &self.proof_usage
    }
    pub fn record_proof_usage_into(&self, usage: &mut RecoveryProofUsage) {
        usage.merge(&self.proof);
    }
    pub fn into_store_parts(self) -> (VerifiedPolicySet, Vec<PolicyPin>, Vec<JournalEntry>) {
        (self.state, self.request.pins, self.request.journal_delta)
    }
}

fn approved_commit_usage(
    input: &StreamedCrossNodeRecoveryCommitRequest,
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    cached: Option<&CachedPolicyProof>,
    carrier_bytes: usize,
) -> Result<RecoveryProofUsage> {
    let mut usage = RecoveryProofUsage::new(carrier_bytes)?;
    usage.record_request(read)?;
    usage.record_snapshot(snapshot)?;
    usage.record_witness(witness)?;
    usage.record_current_source(read, before)?;
    usage.record_cross_possession_carrier(&input.possession)?;
    let mut metadata = ExecutionProofUsage::default();
    metadata.record_metadata(&(&input.pins, &input.grant_leaf))?;
    usage.merge_usage(&metadata);
    let mut staged = before.clone();
    for (index, entry) in input.journal_delta.iter().enumerate() {
        if index == 0 {
            require(
                entry
                    == &JournalEntry::Account(
                        input.possession.node_request.account_recovery.clone(),
                    ),
                "cross-node commit changed its locally approved account",
            )?;
            usage.merge_usage(&policy_entry_payload_usage(entry)?);
            approval::apply_locally_approved_account_recovery(
                &mut staged,
                &input.possession.node_request.account_recovery,
                &input.possession.node_request.expected_account_head,
            )?;
        } else {
            usage.merge_usage(&policy_entry_usage_with_cache(&staged, entry, cached)?);
            apply_policy_entry(&mut staged, &input.pins, entry.clone())?;
        }
    }
    Ok(usage)
}

fn verify_stream_commit_after_local_approval(
    input: &StreamedCrossNodeRecoveryCommitRequest,
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    now: u64,
    cached: Option<&CachedPolicyProof>,
) -> Result<VerifiedStreamedCrossNodeRecoveryCommit> {
    verify_context_reference(&input.possession, read, snapshot, witness, before, now)?;
    let mut after_account = before.clone();
    let approved_account = approval::apply_locally_approved_account_recovery(
        &mut after_account,
        &input.possession.node_request.account_recovery,
        &input.possession.node_request.expected_account_head,
    )?;
    let (state, account_head, space_head) =
        cross_node::verify_delta_after_locally_approved_account(
            &input.possession.request,
            &input.possession.node_request,
            &input.possession.source_certificate,
            &input.pins,
            &input.journal_delta,
            &input.grant_leaf,
            before,
            now,
            after_account,
        )?;
    let usage = approved_commit_usage(
        input,
        read,
        snapshot,
        witness,
        before,
        cached,
        canonical(input)?.len(),
    )?;
    let proof_usage = usage.clone().finish()?;
    Ok(VerifiedStreamedCrossNodeRecoveryCommit {
        request: input.clone(),
        state,
        expected_heads: exact_heads(read, before)?,
        committed_heads: vec![account_head, space_head],
        input_hash: ObjectHash::body(input)?,
        verified_at_ms: now,
        proof_usage,
        proof: usage,
        approved_account: Some(approved_account),
    })
}

pub fn verify_cross_node_recovery_stream_commit_with_source_approval(
    input: &StreamedCrossNodeRecoveryCommitRequest,
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    source_approval: &VerifiedRecoverySourceProposalApproval,
    local_source: &PublicKey,
    now: u64,
) -> Result<VerifiedStreamedCrossNodeRecoveryCommit> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    require(
        read.source_node_public_key() == local_source
            && source_approval.account() == &input.possession.node_request.account_recovery,
        "cross-node approved commit reached another source or account",
    )?;
    source_approval.validate(
        read,
        &input.possession.context,
        &ObjectHash::body(&input.possession)?,
        before,
        now,
    )?;
    let mut verified = verify_stream_commit_after_local_approval(
        input,
        read,
        snapshot,
        witness,
        before,
        now,
        Some(source_approval.cache_for_current(before)?),
    )?;
    source_approval.record_proof_usage_into(&mut verified.proof)?;
    verified.proof_usage = verified.proof.clone().finish()?;
    Ok(verified)
}

pub(crate) fn verify_cross_node_recovery_stream_commit_locally_prepared(
    input: &StreamedCrossNodeRecoveryCommitRequest,
    approved: &StreamedCrossNodeRecoveryPossessionRequest,
    context: &VerifiedRecoveryPolicyContext,
    now: u64,
) -> Result<VerifiedStreamedCrossNodeRecoveryCommit> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    require(
        &input.possession == approved && input.possession.context == context.reference()?,
        "cross-node finish changed its genuine prepared proposal",
    )?;
    let cached = CachedPolicyProof::from_verified_prefix(
        context.verified_state(),
        context.policy().journal(),
    )?;
    verify_stream_commit_after_local_approval(
        input,
        context.request(),
        context.snapshot(),
        context.witness(),
        context.verified_state(),
        now,
        Some(&cached),
    )
}

pub fn verify_cross_node_recovery_stream_commit_in_state(
    input: &StreamedCrossNodeRecoveryCommitRequest,
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    local_source: &PublicKey,
    now: u64,
) -> Result<VerifiedStreamedCrossNodeRecoveryCommit> {
    verify_stream_commit_cached(
        input,
        read,
        snapshot,
        witness,
        before,
        local_source,
        now,
        None,
    )
}

pub fn verify_cross_node_recovery_stream_commit_with_source_proof(
    input: &StreamedCrossNodeRecoveryCommitRequest,
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    source_proof: &VerifiedRecoverySourceProof,
    local_source: &PublicKey,
    now: u64,
) -> Result<VerifiedStreamedCrossNodeRecoveryCommit> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    verify_stream_commit_cached(
        input,
        read,
        snapshot,
        witness,
        before,
        local_source,
        now,
        Some(source_proof.cache_for(read, before)?),
    )
}

fn verify_stream_commit_cached(
    input: &StreamedCrossNodeRecoveryCommitRequest,
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    local_source: &PublicKey,
    now: u64,
    cached: Option<&CachedPolicyProof>,
) -> Result<VerifiedStreamedCrossNodeRecoveryCommit> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let mut usage = possession_usage(
        &input.possession,
        read,
        snapshot,
        witness,
        before,
        cached,
        canonical(input)?.len(),
    )?;
    require(
        read.source_node_public_key() == local_source,
        "cross-node recovery commit reached another source",
    )?;
    verify_context_binding(&input.possession, read, snapshot, witness, before, now)?;
    let (state, account_head, space_head) = cross_node::verify_delta_in_state(
        &input.possession.request,
        &input.possession.node_request,
        &input.possession.source_certificate,
        &input.pins,
        &input.journal_delta,
        &input.grant_leaf,
        before,
        now,
    )?;
    record_delta_usage(&mut usage, input, before, cached)?;
    let proof_usage = usage.clone().finish()?;
    Ok(VerifiedStreamedCrossNodeRecoveryCommit {
        request: input.clone(),
        state,
        expected_heads: exact_heads(read, before)?,
        committed_heads: vec![account_head, space_head],
        input_hash: ObjectHash::body(input)?,
        verified_at_ms: now,
        proof_usage,
        proof: usage,
        approved_account: None,
    })
}

pub fn accept_cross_node_recovery_stream_commit(
    verified: &VerifiedStreamedCrossNodeRecoveryCommit,
    source: &Identity,
    now: u64,
) -> Result<StreamedCrossNodeRecoveryCommitted> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    require(
        verified.source_node_public_key() == &PublicKey::of(source)
            && verified.verified_at_ms == now,
        "cross-node receipt changed its verified source or accepted clock",
    )?;
    let receipt = Signed::sign(
        StreamedCrossNodeRecoveryCommitReceipt {
            version: AUTH_VERSION,
            context: verified.request.possession.context.clone(),
            request_hash: verified.request.possession.request.hash()?,
            input_hash: verified.input_hash.clone(),
            expected_account_head: verified.expected_heads[0].clone(),
            expected_space_head: verified.expected_heads[1].clone(),
            committed_account_head: verified.committed_heads[0].clone(),
            committed_space_head: verified.committed_heads[1].clone(),
            target_node_public_key: verified.target_node_public_key().clone(),
            new_device_public_key: verified.new_device_public_key().clone(),
            grant_leaf: verified.request.grant_leaf.clone(),
            accepted_at_ms: now,
        },
        source,
    )?;
    let committed = StreamedCrossNodeRecoveryCommitted {
        request: verified.request.clone(),
        receipt,
    };
    let mut usage = RecoveryProofUsage::new(canonical(&committed)?.len())?;
    verified.record_proof_usage_into(&mut usage);
    usage.record_cross_completion(&committed.receipt)?;
    usage.finish()?;
    Ok(committed)
}

pub(crate) fn verify_receipt_binding(
    committed: &StreamedCrossNodeRecoveryCommitted,
    now: u64,
) -> Result<()> {
    let input = &committed.request;
    let original = &input.possession.request;
    let r = &original.payload;
    let receipt = &committed.receipt;
    let value = &receipt.payload;
    let account_head = input
        .journal_delta
        .iter()
        .rev()
        .find_map(|entry| match entry {
            JournalEntry::Account(value) => Some(account_stamp(value)),
            _ => None,
        })
        .ok_or(AuthorizationError::PolicyConflict)??;
    let space_head = input
        .journal_delta
        .iter()
        .rev()
        .find_map(|entry| match entry {
            JournalEntry::Space(value) => Some(space_stamp(value)),
            _ => None,
        })
        .ok_or(AuthorizationError::PolicyConflict)??;
    require(
        value.version == AUTH_VERSION
            && receipt.issuer == r.source_node_public_key
            && value.context == input.possession.context
            && value.request_hash == original.hash()?
            && value.input_hash == ObjectHash::body(input)?
            && value.expected_account_head == input.possession.node_request.expected_account_head
            && value.expected_space_head == input.possession.node_request.expected_space_head
            && value.committed_account_head == account_head
            && value.committed_space_head == space_head
            && value.target_node_public_key == r.target_claim.payload.node_public_key
            && value.new_device_public_key == r.new_device_public_key
            && value.grant_leaf == input.grant_leaf
            && value.accepted_at_ms <= now,
        "cross-node source receipt changed its exact original transition",
    )?;
    validate_window(r.issued_at_ms, r.expires_at_ms, value.accepted_at_ms)?;
    validate_claim(
        &r.target_claim,
        &r.target_claim.payload.node_public_key,
        &r.target_claim.payload.claim_nonce,
        value.accepted_at_ms,
    )?;
    original.verify_signature()?;
    receipt.verify_signature()
}

pub struct VerifiedCrossNodeRecoveryInstallation {
    policy: VerifiedPolicySnapshot,
    accepted_prefix_len: usize,
    account: AccountId,
    space: SpaceId,
    new_device_public_key: PublicKey,
    target_node_public_key: PublicKey,
    input_hash: ObjectHash,
    proof_usage: RecoveryProofCounts,
    proof: RecoveryProofUsage,
}

impl VerifiedCrossNodeRecoveryInstallation {
    pub fn accepted_prefix_len(&self) -> usize {
        self.accepted_prefix_len
    }
    pub fn account(&self) -> &AccountId {
        &self.account
    }
    pub fn space(&self) -> &SpaceId {
        &self.space
    }
    pub fn new_device_public_key(&self) -> &PublicKey {
        &self.new_device_public_key
    }
    pub fn target_node_public_key(&self) -> &PublicKey {
        &self.target_node_public_key
    }
    pub fn input_hash(&self) -> &ObjectHash {
        &self.input_hash
    }
    pub fn proof_usage(&self) -> &RecoveryProofCounts {
        &self.proof_usage
    }
    pub fn record_proof_usage_into(&self, usage: &mut RecoveryProofUsage) {
        usage.merge(&self.proof);
    }
    pub fn into_store_parts(self) -> (VerifiedPolicySet, Vec<PolicyPin>, Vec<JournalEntry>) {
        self.policy.into_parts()
    }
}

pub fn verify_cross_node_recovery_stream_installation(
    context: VerifiedRecoveryPolicyContext,
    approval: &Signed<CrossNodeRecoveryImportApproval>,
    committed: &StreamedCrossNodeRecoveryCommitted,
    local_target: &PublicKey,
    expected_nonce: &Nonce,
    now: u64,
) -> Result<VerifiedCrossNodeRecoveryInstallation> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let original = cross_request(context.request())?;
    let r = &original.payload;
    let approved = &approval.payload;
    let value = &committed.receipt.payload;
    require(
        approved.version == AUTH_VERSION
            && approval.issuer == *local_target
            && approved.context == context.reference()?
            && approved.account == r.account
            && approved.space == r.space
            && approved.new_device_public_key == r.new_device_public_key
            && approved.new_root_public_key == r.new_root_public_key
            && approved.source_node_public_key == r.source_node_public_key
            && approved.target_node_public_key == *local_target
            && approved.target_node_public_key == r.target_claim.payload.node_public_key
            && &r.target_claim.payload.claim_nonce == expected_nonce
            && approved.possession_hash == ObjectHash::body(&committed.request.possession)?
            && approved.claim_hash == r.target_claim.hash()?
            && approved.expires_at_ms == context.witness().payload.expires_at_ms
            && approved.approved_at_ms <= value.accepted_at_ms
            && value.accepted_at_ms < approved.expires_at_ms
            && value.accepted_at_ms <= now,
        "cross-node installation changed the original local approval or accepted window",
    )?;
    approval.verify_signature()?;
    verify_receipt_binding(committed, now)?;
    verify_context_reference(
        &committed.request.possession,
        context.request(),
        context.snapshot(),
        context.witness(),
        context.verified_state(),
        approved.approved_at_ms,
    )?;
    let cached = CachedPolicyProof::from_verified_prefix(
        context.verified_state(),
        context.policy().journal(),
    )?;
    let verified = verify_stream_commit_after_local_approval(
        &committed.request,
        context.request(),
        context.snapshot(),
        context.witness(),
        context.verified_state(),
        value.accepted_at_ms,
        Some(&cached),
    )?;
    require(
        verified.committed_heads()
            == [
                value.committed_account_head.clone(),
                value.committed_space_head.clone(),
            ],
        "cross-node installation receipt differs from verified final policy",
    )?;
    let policy = context
        .policy()
        .apply_locally_approved_recovery_delta(
            &committed.request.journal_delta,
            verified
                .approved_account
                .as_ref()
                .ok_or(AuthorizationError::InvalidProof(
                    "cross-node installation lost its exact local account approval",
                ))?,
        )?
        .with_independent_pins(&committed.request.pins)?;
    policy
        .state()
        .verify_transport_device(&DeviceId::from_key(local_target), now)?;
    let mut usage = RecoveryProofUsage::new(canonical(&(approval, committed))?.len())?;
    verified.record_proof_usage_into(&mut usage);
    let mut extra = ExecutionProofUsage::default();
    extra.record_signed(approval)?;
    extra.record_signed(&committed.receipt)?;
    usage.merge_usage(&extra);
    let proof_usage = usage.clone().finish()?;
    Ok(VerifiedCrossNodeRecoveryInstallation {
        policy,
        accepted_prefix_len: context.policy().journal().len(),
        account: r.account.clone(),
        space: r.space.clone(),
        new_device_public_key: r.new_device_public_key.clone(),
        target_node_public_key: local_target.clone(),
        input_hash: ObjectHash::body(&committed.request)?,
        proof_usage,
        proof: usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature_verification::SignatureVerification;

    thread_local! {
        static STAGE_USAGE: std::cell::RefCell<Vec<serde_json::Value>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    const NOW: u64 = 20_000;
    const EXPIRY: u64 = 80_000;
    const END: u64 = 1_000_000;

    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }

    fn checked<T, E: std::fmt::Debug>(
        label: &str,
        action: impl FnOnce() -> std::result::Result<T, E>,
    ) -> T {
        let signatures = SignatureVerification::new();
        let value = signatures.run(action).expect(label);
        assert!(
            signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES,
            "{label}"
        );
        eprintln!(
            "cross stream {label}: {}",
            serde_json::to_string(&signatures.counts()).unwrap()
        );
        STAGE_USAGE.with(|stages| {
            stages.borrow_mut().push(serde_json::json!({
                "stage": label,
                "actual": signatures.counts(),
            }));
        });
        value
    }

    fn record_stage_proof(proof: impl serde::Serialize) {
        STAGE_USAGE.with(|stages| {
            stages.borrow_mut().last_mut().unwrap()["proof"] = serde_json::to_value(proof).unwrap();
        });
    }

    struct Fixture {
        original: CreatedAccount,
        material: String,
        read: RecoveryReadRequest,
        snapshot: Signed<RecoveryContextSnapshot>,
        pages: Vec<Signed<RecoveryContextPage>>,
        witness: Signed<RecoveryContextWitness>,
    }

    impl Fixture {
        fn new(nodes: u8) -> Self {
            let original = super::super::super::tests::account_with_enrolled_nodes(nodes);
            let material = Self::material(&original);
            Self::from_backup(original, material)
        }

        fn material(original: &CreatedAccount) -> String {
            checked("material", || {
                build_account_recovery_material(
                    original,
                    &key(1),
                    &key(2),
                    "https://source.example",
                    10_000.0,
                )
            })
        }

        fn with_later_source_project() -> Self {
            let mut original = super::super::super::tests::account_with_enrolled_nodes(0);
            let material = Self::material(&original);
            let mut state = original.authorization.policy_snapshot.clone();
            let space_id = original.authorization.scope.space_id.clone();
            let first = state
                .resources
                .values()
                .find(|record| record.payload.kind == ResourceKind::Project)
                .unwrap();
            let first_hash = first.hash().unwrap();
            let first_resource = first.payload.resource.clone();
            let resource = ResourceId::from_bytes([199; 32]);
            assert_ne!(resource, first_resource);
            let record = (0..128)
                .find_map(|index| {
                    let record = Signed::sign(
                        ResourceRecord {
                            version: AUTH_VERSION,
                            space: space_id.clone(),
                            resource: resource.clone(),
                            kind: ResourceKind::Project,
                            host: ServiceId::from_key(&PublicKey::of(&key(4))),
                            issued_under: state
                                .head(&PolicyDomain::Space(space_id.clone()))
                                .unwrap()
                                .clone(),
                            nonce: Nonce::from_bytes([index; 32]),
                        },
                        &key(1),
                    )
                    .unwrap();
                    (record.hash().unwrap().cmp(&first_hash) != resource.cmp(&first_resource))
                        .then_some(record)
                })
                .expect("fixture must put ObjectHash and ResourceId in opposite orders");
            let hash = state.install_resource(record.clone()).unwrap();
            let mut space = state.space(&space_id).unwrap().clone();
            space.previous = state.head(&PolicyDomain::Space(space_id.clone())).cloned();
            space.sequence += 1;
            space.resources.push(hash);
            space.resources.sort();
            let space = Signed::sign(space, &key(1)).unwrap();
            let head = state.install_space(space.clone()).unwrap();
            original
                .bundle
                .journal
                .extend([JournalEntry::Resource(record), JournalEntry::Space(space)]);
            original
                .pins
                .iter_mut()
                .find(|pin| pin.domain == head.domain)
                .unwrap()
                .minimum_head = Some(head.clone());
            original.authorization = checked("latest_source_policy", || {
                verify_client_policy_state(
                    state,
                    &original.bundle,
                    &original.pins,
                    original.authorization.scope.account_id.clone(),
                    space_id,
                    PublicKey::of(&key(3)),
                    NOW,
                )
            });
            Self::from_backup(original, material)
        }

        fn from_backup(original: CreatedAccount, material: String) -> Self {
            let mut projects = original
                .authorization
                .policy_snapshot
                .resources
                .values()
                .filter(|record| {
                    record.payload.kind == ResourceKind::Project
                        && record.payload.host == ServiceId::from_key(&PublicKey::of(&key(4)))
                })
                .map(|record| ResourceRef {
                    space: record.payload.space.clone(),
                    resource: record.payload.resource.clone(),
                })
                .collect::<Vec<_>>();
            projects.sort_by(|left, right| left.resource.cmp(&right.resource));
            let nonce = Nonce::from_bytes([44; 32]);
            let claim_json =
                create_node_claim(&key(7), nonce.as_str(), NOW as f64, EXPIRY as f64).unwrap();
            let claim = verify_node_claim(
                &claim_json,
                &key(7).public_key_hex(),
                nonce.as_str(),
                NOW as f64,
            )
            .unwrap();
            let request_json = checked("request", || {
                cross_node::build_cross_node_recovery_request(
                    &material,
                    &key(51),
                    &key(50),
                    &claim,
                    &encode(&projects).unwrap(),
                    (END - 1000) as f64,
                    NOW as f64,
                    EXPIRY as f64,
                )
            });
            let request: Signed<CrossNodeRecoveryRequest> =
                serde_json::from_str(&request_json).unwrap();
            let read = RecoveryReadRequest::CrossNode(request.clone());
            let snapshot = Signed::sign(
                RecoveryContextSnapshot {
                    version: AUTH_VERSION,
                    kind: RecoveryContextKind::CrossNode,
                    request_hash: read.request_hash().unwrap(),
                    snapshot_id: Nonce::from_bytes([90; 32]),
                    saved_pins_hash: read.saved_pins_hash().unwrap(),
                    target_heads: original
                        .pins
                        .iter()
                        .filter_map(|pin| pin.minimum_head.clone())
                        .collect(),
                    source_node_public_key: PublicKey::of(&key(4)),
                    claim: request.payload.target_claim.clone(),
                    issued_at_ms: NOW,
                    expires_at_ms: EXPIRY,
                },
                &key(4),
            )
            .unwrap();
            let mut previous = snapshot.hash().unwrap();
            let mut pages = Vec::new();
            for (index, entry) in original.bundle.journal.iter().enumerate() {
                let complete = index + 1 == original.bundle.journal.len();
                let page = Signed::sign(
                    RecoveryContextPage {
                        version: AUTH_VERSION,
                        kind: RecoveryContextKind::CrossNode,
                        request_hash: read.request_hash().unwrap(),
                        snapshot_id: snapshot.payload.snapshot_id.clone(),
                        target_heads_hash: recovery_target_heads_hash(
                            &snapshot.payload.target_heads,
                        )
                        .unwrap(),
                        page_index: index as u64,
                        previous_page_hash: previous,
                        entry: Some(entry.clone()),
                        next_cursor: if complete {
                            None
                        } else {
                            Some(ObjectHash::body(&("cross-cursor", index)).unwrap())
                        },
                        complete,
                    },
                    &key(4),
                )
                .unwrap();
                previous = page.hash().unwrap();
                pages.push(page);
            }
            let witness = Signed::sign(
                RecoveryContextWitness {
                    version: AUTH_VERSION,
                    kind: RecoveryContextKind::CrossNode,
                    request_hash: read.request_hash().unwrap(),
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    target_heads: snapshot.payload.target_heads.clone(),
                    final_page_hash: previous,
                    page_count: pages.len() as u64,
                    entry_count: pages.len() as u64,
                    claim_hash: snapshot.payload.claim.hash().unwrap(),
                    source_node_public_key: PublicKey::of(&key(4)),
                    issued_at_ms: NOW,
                    expires_at_ms: EXPIRY,
                },
                &key(4),
            )
            .unwrap();
            Self {
                original,
                material,
                read,
                snapshot,
                pages,
                witness,
            }
        }

        fn import(&self, at: u64) -> VerifiedRecoveryPolicyContext {
            let mut import = checked("import_begin", || {
                begin_cross_node_recovery_policy_import(
                    &encode(cross_request(&self.read).unwrap()).unwrap(),
                    &encode(&self.snapshot).unwrap(),
                    &key(7).public_key_hex(),
                    Nonce::from_bytes([44; 32]).as_str(),
                    at,
                )
            });
            record_stage_proof(
                serde_json::from_str::<serde_json::Value>(&import.proof_usage_json().unwrap())
                    .unwrap(),
            );
            for page in &self.pages {
                checked("import_page", || {
                    import.append_page(&encode(page).unwrap(), at as f64)
                });
                record_stage_proof(
                    serde_json::from_str::<serde_json::Value>(&import.proof_usage_json().unwrap())
                        .unwrap(),
                );
            }
            let context = checked("import_finish", || {
                import.finish(&encode(&self.witness).unwrap(), at as f64)
            });
            record_stage_proof(
                serde_json::from_str::<serde_json::Value>(&context.proof_usage_json().unwrap())
                    .unwrap(),
            );
            context
        }

        fn prepare(
            &self,
            context: &VerifiedRecoveryPolicyContext,
        ) -> cross_node::PreparedCrossNodeAccountRecovery {
            checked("prepare", || {
                cross_node::prepare_cross_node_account_recovery(
                    &self.material,
                    context,
                    &key(51),
                    &key(50),
                    END as f64,
                    NOW as f64,
                )
            })
        }

        fn import_client(&self, at: u64) -> VerifiedRecoveryPolicyContext {
            let mut import = checked("client_import_begin", || {
                begin_cross_node_recovery_context_import(
                    &self.material,
                    &encode(cross_request(&self.read).unwrap()).unwrap(),
                    &encode(&self.snapshot).unwrap(),
                    at as f64,
                )
            });
            record_stage_proof(
                serde_json::from_str::<serde_json::Value>(&import.proof_usage_json().unwrap())
                    .unwrap(),
            );
            for page in &self.pages {
                checked("client_import_page", || {
                    import.append_page(&encode(page).unwrap(), at as f64)
                });
                record_stage_proof(
                    serde_json::from_str::<serde_json::Value>(&import.proof_usage_json().unwrap())
                        .unwrap(),
                );
            }
            let context = checked("client_import_finish", || {
                import.finish(&encode(&self.witness).unwrap(), at as f64)
            });
            record_stage_proof(
                serde_json::from_str::<serde_json::Value>(&context.proof_usage_json().unwrap())
                    .unwrap(),
            );
            context
        }

        fn prefix_commitment(&self) -> ObjectHash {
            ObjectHash::body(&self.original.bundle.journal).unwrap()
        }

        fn commit(
            &self,
        ) -> (
            StreamedCrossNodeRecoveryCommitRequest,
            Signed<CrossNodeRecoveryImportApproval>,
            Signed<RecoverySourceProposalApproval>,
            cross_node::PreparedCrossNodeRecoveryCommit,
        ) {
            let client_context = self.import_client(NOW);
            let prepared = self.prepare(&client_context);
            let possession: StreamedCrossNodeRecoveryPossessionRequest =
                serde_json::from_str(&prepared.possession_request_json().unwrap()).unwrap();
            let target_context = self.import(NOW);
            let (target_proof, approval) = checked("target_approval", || {
                approve_cross_node_recovery_import(
                    &target_context,
                    &possession,
                    &key(7),
                    &Nonce::from_bytes([44; 32]),
                    NOW,
                )
            });
            let (source_proof, source_approval) = checked("source_possession", || {
                approve_cross_node_recovery_source_proposal(
                    &possession,
                    &self.read,
                    &self.snapshot,
                    &self.witness,
                    &self.original.authorization.policy_snapshot,
                    &key(4),
                    &self.prefix_commitment(),
                    NOW,
                )
            });
            assert!(
                cross_node::finish_cross_node_account_recovery(
                    &prepared,
                    &key(51),
                    source_proof.as_str(),
                    target_proof.as_str(),
                    NOW as f64,
                )
                .is_err()
            );
            let pending = checked("finish", || {
                cross_node::finish_cross_node_account_recovery(
                    &prepared,
                    &key(51),
                    target_proof.as_str(),
                    source_proof.as_str(),
                    NOW as f64,
                )
            });
            let commit = serde_json::from_str(&pending.request_json().unwrap()).unwrap();
            (commit, approval, source_approval, pending)
        }

        fn accept(
            &self,
            input: &StreamedCrossNodeRecoveryCommitRequest,
            source_approval: &Signed<RecoverySourceProposalApproval>,
        ) -> StreamedCrossNodeRecoveryCommitted {
            let (committed, proof_usage) = checked("source_commit", || {
                let source_proof = input
                    .journal_delta
                    .iter()
                    .find_map(|entry| match entry {
                        JournalEntry::Device(value)
                            if value.payload.public_key == *self.read.source_node_public_key() =>
                        {
                            value.payload.possession.as_ref()
                        }
                        _ => None,
                    })
                    .unwrap();
                let approved = verify_recovery_source_proposal_approval(
                    source_approval,
                    &self.read,
                    &input.possession.context,
                    &ObjectHash::body(&input.possession).unwrap(),
                    &input.possession.node_request.account_recovery,
                    source_proof,
                    &self.original.authorization.policy_snapshot,
                    &PublicKey::of(&key(4)),
                    &self.prefix_commitment(),
                    NOW,
                )?;
                let verified = verify_cross_node_recovery_stream_commit_with_source_approval(
                    input,
                    &self.read,
                    &self.snapshot,
                    &self.witness,
                    &self.original.authorization.policy_snapshot,
                    &approved,
                    &PublicKey::of(&key(4)),
                    NOW,
                )
                .unwrap();
                let committed = accept_cross_node_recovery_stream_commit(&verified, &key(4), NOW)?;
                let mut usage = RecoveryProofUsage::new(canonical(&committed)?.len())?;
                verified.record_proof_usage_into(&mut usage);
                usage.record_cross_completion(&committed.receipt)?;
                Ok::<_, AuthorizationError>((committed, usage.finish()?))
            });
            record_stage_proof(proof_usage);
            committed
        }
    }

    #[test]
    fn twelve_nodes_dual_possession_receipt_and_one_use_installation() {
        let fixture = Fixture::new(12);
        assert!(canonical(&fixture.original.bundle).unwrap().len() > MAX_PROOF_BYTES);
        let (input, approval, source_approval, mut pending) = fixture.commit();
        assert!(canonical(&input).unwrap().len() <= MAX_PROOF_BYTES);
        let committed = fixture.accept(&input, &source_approval);
        assert!(canonical(&committed).unwrap().len() <= MAX_PROOF_BYTES);
        let context = fixture.import(approval.payload.approved_at_ms);
        let token = checked("target_install", || {
            verify_cross_node_recovery_stream_installation(
                context,
                &approval,
                &committed,
                &PublicKey::of(&key(7)),
                &Nonce::from_bytes([44; 32]),
                EXPIRY + 1,
            )
        });
        assert_eq!(
            token.accepted_prefix_len(),
            fixture.original.bundle.journal.len()
        );
        let (after, pins, journal) = token.into_store_parts();
        assert!(journal.starts_with(&fixture.original.bundle.journal));
        assert_eq!(
            &journal[fixture.original.bundle.journal.len()..],
            input.journal_delta
        );
        assert_eq!(
            after.account(fixture.read.account()).unwrap().current_root,
            PublicKey::of(&key(51))
        );
        assert_eq!(
            after
                .account(fixture.read.account())
                .unwrap()
                .authority
                .public_key,
            PublicKey::of(&key(7))
        );
        for old in fixture
            .original
            .authorization
            .policy_snapshot
            .resources
            .values()
        {
            assert_eq!(
                after.resource(&old.payload.space, &old.payload.resource),
                Some(old)
            );
        }
        assert!(
            after
                .verify_transport_device(&DeviceId::from_key(&PublicKey::of(&key(3))), NOW)
                .is_err()
        );
        for node in 20..32 {
            assert!(
                after
                    .verify_transport_device(&DeviceId::from_key(&PublicKey::of(&key(node))), NOW)
                    .is_err()
            );
        }
        for resource in &cross_request(&fixture.read)
            .unwrap()
            .payload
            .source_read_resources
        {
            let leaf = input
                .journal_delta
                .iter()
                .find_map(|entry| match entry {
                    JournalEntry::Grant(value)
                        if value.payload.scope == ResourceScope::Exact(resource.clone())
                            && value.payload.audience.device
                                == DeviceId::from_key(&PublicKey::of(&key(4))) =>
                    {
                        Some(value.hash().unwrap())
                    }
                    _ => None,
                })
                .unwrap();
            let authorization = checked("source_read", || {
                verify_client_policy_state(
                    after.clone(),
                    &ClientAuthorizationBundle {
                        journal: journal.clone(),
                        grant_leaf: leaf,
                    },
                    &pins,
                    fixture.read.account().clone(),
                    fixture.read.space().clone(),
                    PublicKey::of(&key(50)),
                    NOW,
                )
            });
            assert_eq!(authorization.actions, cross_node::source_read_actions());
            assert_eq!(authorization.audience_public_key, PublicKey::of(&key(4)));
            let mut invocation = Invocation {
                version: AUTH_VERSION,
                actor: Actor::Account(fixture.read.account().clone()),
                device: authorization.scope.device_id.clone(),
                initiator: Actor::Account(fixture.read.account().clone()),
                initiating_grant: None,
                resource: resource.clone(),
                action: Action::ConversationRead,
                audience: authorization.audience.clone(),
                presenter: authorization.scope.device_id.clone(),
                operation_id: "streamed-source-read".to_owned(),
                body_hash: ObjectHash::body(&serde_json::json!({})).unwrap(),
                grant_leaf: authorization.grant_leaf.clone(),
                expected_policies: authorization.heads.clone(),
                context: InvocationContext::default(),
                nonce: Nonce::from_bytes([55; 32]),
                not_before_ms: NOW,
                expires_at_ms: NOW + 1000,
            };
            let transport = TransportEvidence::DirectNoise {
                device: authorization.scope.device_id.clone(),
            };
            let facts = |value: &Invocation| RequestFacts {
                resource: value.resource.clone(),
                action: value.action,
                audience: value.audience.clone(),
                operation_id: value.operation_id.clone(),
                body_hash: value.body_hash.clone(),
                context: value.context.clone(),
            };
            checked("source_read_invocation", || {
                after.verify_invocation(
                    &Signed::sign(invocation.clone(), &key(50)).unwrap(),
                    &facts(&invocation),
                    &transport,
                    NOW,
                )
            });
            for action in [
                Action::MessageAppend,
                Action::TaskCreate,
                Action::AgentStart,
                Action::ExecutionOpen,
                Action::MemberManage,
                Action::AttachmentWrite,
                Action::OperationResolve,
            ] {
                invocation.action = action;
                assert!(
                    after
                        .verify_invocation(
                            &Signed::sign(invocation.clone(), &key(50)).unwrap(),
                            &facts(&invocation),
                            &transport,
                            NOW
                        )
                        .is_err()
                );
            }
        }
        let pending_material = checked("pending_material", || {
            pending.recovery_material_json(
                &key(51),
                &fixture.material,
                "https://target.example",
                NOW as f64,
            )
        });
        let restored = checked("pending_restore", || {
            cross_node::restore_prepared_cross_node_recovery_commit(
                &pending_material,
                (EXPIRY + 1) as f64,
            )
        });
        assert_eq!(
            restored.request_json().unwrap(),
            pending.request_json().unwrap()
        );
        let mut changed_pending: serde_json::Value =
            serde_json::from_str(&pending_material).unwrap();
        changed_pending["client_secret_hex"] = key(8).secret_hex().into();
        assert!(
            cross_node::restore_prepared_cross_node_recovery_commit(
                &changed_pending.to_string(),
                NOW as f64,
            )
            .is_err()
        );
        let result = checked("client_receipt", || {
            cross_node::finish_cross_node_recovery_commit(
                &restored,
                &encode(&committed).unwrap(),
                (EXPIRY + 1) as f64,
            )
        });
        assert!(result.authorization().is_err());
        assert_eq!(result.scope_json().unwrap(), "null");
        let material = checked("final_material", || {
            result.final_recovery_material_json("https://target.example", (EXPIRY + 1) as f64)
        });
        assert!(material.len() <= MAX_RECOVERY_MATERIAL_BYTES);
        checked("inspect_final", || {
            inspect_account_recovery_material(&material, (EXPIRY + 1) as f64)
        });
    }

    #[test]
    fn streamed_cross_uses_latest_source_projects_in_resource_order() {
        STAGE_USAGE.with(|stages| stages.borrow_mut().clear());
        let fixture = Fixture::with_later_source_project();
        let old_sources: serde_json::Value = serde_json::from_str(
            &cross_node::inspect_cross_node_recovery_sources(&fixture.material, NOW as f64)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(old_sources["source_projects"].as_array().unwrap().len(), 1);
        let read = cross_request(&fixture.read).unwrap();
        assert_eq!(read.payload.source_read_resources.len(), 2);
        assert!(
            read.payload.source_read_resources[0].resource
                < read.payload.source_read_resources[1].resource
        );
        let before = &fixture.original.authorization.policy_snapshot;
        let hash_order = before
            .space(fixture.read.space())
            .unwrap()
            .resources
            .iter()
            .filter_map(|hash| before.resources.get(hash))
            .filter(|record| record.payload.kind == ResourceKind::Project)
            .map(|record| record.payload.resource.clone())
            .collect::<Vec<_>>();
        assert_ne!(
            hash_order,
            read.payload
                .source_read_resources
                .iter()
                .map(|resource| resource.resource.clone())
                .collect::<Vec<_>>()
        );
        let mut missing = read.payload.clone();
        missing.source_read_resources = vec![ResourceRef {
            space: fixture.read.space().clone(),
            resource: ResourceId::from_bytes([198; 32]),
        }];
        assert!(
            cross_node::verify_cross_node_recovery_request(
                before,
                &Signed::sign(missing, &key(2)).unwrap(),
                &PublicKey::of(&key(4)),
                NOW,
            )
            .is_err()
        );
        let claim = verify_node_claim(
            &encode(&read.payload.target_claim).unwrap(),
            &key(7).public_key_hex(),
            Nonce::from_bytes([44; 32]).as_str(),
            NOW as f64,
        )
        .unwrap();
        assert!(
            cross_node::build_cross_node_recovery_request(
                &fixture.material,
                &key(51),
                &key(50),
                &claim,
                &encode(&vec![ResourceRef {
                    space: SpaceId::personal(&AccountId::from_root(&PublicKey::of(&key(8)))),
                    resource: read.payload.source_read_resources[0].resource.clone(),
                }])
                .unwrap(),
                (END - 1000) as f64,
                NOW as f64,
                EXPIRY as f64,
            )
            .is_err()
        );
        let (input, approval, source_approval, mut pending) = fixture.commit();
        let committed = fixture.accept(&input, &source_approval);
        let context = fixture.import(NOW);
        let token = checked("target_install", || {
            verify_cross_node_recovery_stream_installation(
                context,
                &approval,
                &committed,
                &PublicKey::of(&key(7)),
                &Nonce::from_bytes([44; 32]),
                NOW,
            )
        });
        record_stage_proof(token.proof_usage());
        let (after, pins, journal) = token.into_store_parts();
        assert_eq!(
            after.space(fixture.read.space()).unwrap().resources.len(),
            3
        );
        for resource in &read.payload.source_read_resources {
            assert_eq!(
                after.resource(&resource.space, &resource.resource),
                before.resource(&resource.space, &resource.resource)
            );
        }
        let source_grants = input
            .journal_delta
            .iter()
            .filter_map(|entry| match entry {
                JournalEntry::Grant(value)
                    if value.payload.audience.device
                        == DeviceId::from_key(&PublicKey::of(&key(4))) =>
                {
                    Some(value)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(source_grants.len(), 2);
        for (grant, resource) in source_grants
            .iter()
            .zip(&read.payload.source_read_resources)
        {
            assert_eq!(grant.payload.scope, ResourceScope::Exact(resource.clone()));
            assert_eq!(grant.payload.actions, cross_node::source_read_actions());
            assert_eq!(grant.payload.remaining_delegations, 0);
            let authorization = checked("source_read", || {
                verify_client_policy_state(
                    after.clone(),
                    &ClientAuthorizationBundle {
                        journal: journal.clone(),
                        grant_leaf: grant.hash().unwrap(),
                    },
                    &pins,
                    fixture.read.account().clone(),
                    fixture.read.space().clone(),
                    PublicKey::of(&key(50)),
                    NOW,
                )
            });
            assert_eq!(authorization.audience_public_key(), &PublicKey::of(&key(4)));
            assert_eq!(authorization.resources().len(), 1);
            assert_eq!(
                authorization.resources()[0].payload.resource,
                resource.resource
            );
        }
        let pending_material = checked("pending_material", || {
            pending.recovery_material_json(
                &key(51),
                &fixture.material,
                "https://target.example",
                NOW as f64,
            )
        });
        assert!(
            cross_node::finish_cross_node_recovery_commit(
                &pending,
                &encode(&committed).unwrap(),
                (NOW - 1) as f64,
            )
            .is_err()
        );
        let completed = checked("client_receipt", || {
            cross_node::finish_cross_node_recovery_commit(
                &pending,
                &encode(&committed).unwrap(),
                NOW as f64,
            )
        });
        let authorization = checked("target_authorization", || completed.authorization());
        assert_eq!(authorization.audience_public_key(), &PublicKey::of(&key(7)));
        assert_eq!(authorization.resources().len(), 1);
        assert_eq!(
            authorization.resources()[0].payload.host,
            ServiceId::from_key(&PublicKey::of(&key(7)))
        );
        let selections: serde_json::Value =
            serde_json::from_str(&completed.source_read_selections_json().unwrap()).unwrap();
        assert_eq!(selections.as_array().unwrap().len(), 2);
        for (selection, resource) in selections
            .as_array()
            .unwrap()
            .iter()
            .zip(&read.payload.source_read_resources)
        {
            assert_eq!(
                selection["project"],
                serde_json::to_value(resource).unwrap()
            );
            assert!(selection.get("authorization").is_none());
            assert!(selection.get("bundle").is_none());
        }
        let after_material = checked("final_material", || {
            completed.final_recovery_material_json("https://target.example", NOW as f64)
        });
        checked("inspect_final", || {
            inspect_account_recovery_material(&after_material, NOW as f64)
        });
        assert!(
            verify_cross_node_recovery_stream_commit_in_state(
                &input,
                &fixture.read,
                &fixture.snapshot,
                &fixture.witness,
                &after,
                &PublicKey::of(&key(4)),
                NOW,
            )
            .is_err()
        );
        #[cfg(unix)]
        if let Ok(path) = std::env::var("VIBEFACE_CROSS_NODE_RECOVERY_VECTOR") {
            use std::io::Write;
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let stages = STAGE_USAGE.with(|stages| stages.borrow().clone());
            let vector = serde_json::json!({
                "format": "vibeface-cross-node-recovery-stream-vector",
                "version": AUTH_VERSION,
                "now_ms": NOW,
                "expires_at_ms": EXPIRY,
                "device_expires_at_ms": END,
                "material_before": fixture.material,
                "material_after": after_material,
                "pending_material": pending_material,
                "independent_pins": fixture.original.pins,
                "request": read,
                "snapshot": fixture.snapshot,
                "pages": fixture.pages,
                "witness": fixture.witness,
                "target_approval": approval,
                "source_approval": source_approval,
                "source_prefix_commitment": fixture.prefix_commitment(),
                "possession": input.possession,
                "commit_request": input,
                "committed": committed,
                "expected_scope": authorization.scope(),
                "expected_heads": authorization.heads,
                "source_read_selections": selections,
                "stage_usage": stages,
                "synthetic_keys": {
                    "next_root_secret_hex": key(51).secret_hex(),
                    "next_client_secret_hex": key(50).secret_hex(),
                },
            });
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)
                .unwrap();
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .unwrap();
            file.write_all(encode(&vector).unwrap().as_bytes()).unwrap();
        }
    }

    #[test]
    fn streamed_cross_rejects_changed_context_approvals_receipts_and_source_privileges() {
        let fixture = Fixture::new(0);
        let (input, approval, source_approval, _) = fixture.commit();
        let committed = fixture.accept(&input, &source_approval);
        let context = fixture.import(NOW);
        assert!(
            approve_cross_node_recovery_import(
                &context,
                &input.possession,
                &key(4),
                &Nonce::from_bytes([44; 32]),
                NOW
            )
            .is_err()
        );
        assert!(
            approve_cross_node_recovery_import(
                &context,
                &input.possession,
                &key(7),
                &Nonce::from_bytes([45; 32]),
                NOW
            )
            .is_err()
        );
        assert!(
            sign_cross_node_recovery_source_possession_in_state(
                &input.possession,
                &fixture.read,
                &fixture.snapshot,
                &fixture.witness,
                &fixture.original.authorization.policy_snapshot,
                &key(7),
                NOW
            )
            .is_err()
        );
        let mut missing_source = input.clone();
        missing_source.journal_delta.retain(|entry| {
            !matches!(entry,JournalEntry::Device(value)
            if value.payload.public_key == *fixture.read.source_node_public_key())
        });
        assert!(
            verify_cross_node_recovery_stream_commit_in_state(
                &missing_source,
                &fixture.read,
                &fixture.snapshot,
                &fixture.witness,
                &fixture.original.authorization.policy_snapshot,
                &PublicKey::of(&key(4)),
                NOW
            )
            .is_err()
        );
        let mut oversized = input.clone();
        oversized
            .journal_delta
            .extend(std::iter::repeat_n(input.journal_delta[0].clone(), 200));
        assert!(
            verify_cross_node_recovery_stream_commit_in_state(
                &oversized,
                &fixture.read,
                &fixture.snapshot,
                &fixture.witness,
                &fixture.original.authorization.policy_snapshot,
                &PublicKey::of(&key(4)),
                NOW
            )
            .is_err()
        );
        assert!(
            verify_cross_node_recovery_stream_installation(
                context.clone(),
                &approval,
                &committed,
                &PublicKey::of(&key(7)),
                &Nonce::from_bytes([44; 32]),
                END + 1
            )
            .is_err()
        );
        let reject = |approval: &Signed<CrossNodeRecoveryImportApproval>,
                      committed: &StreamedCrossNodeRecoveryCommitted| {
            assert!(
                verify_cross_node_recovery_stream_installation(
                    context.clone(),
                    approval,
                    committed,
                    &PublicKey::of(&key(7)),
                    &Nonce::from_bytes([44; 32]),
                    EXPIRY + 1,
                )
                .is_err()
            );
        };
        for change in 0..8 {
            let mut wrong = approval.payload.clone();
            match change {
                0 => wrong.possession_hash = ObjectHash::body(&"another possession").unwrap(),
                1 => wrong.context.snapshot_id = Nonce::from_bytes([91; 32]),
                2 => wrong.claim_hash = ObjectHash::body(&"another claim").unwrap(),
                3 => wrong.new_device_public_key = PublicKey::of(&key(8)),
                4 => wrong.source_node_public_key = PublicKey::of(&key(8)),
                5 => wrong.target_node_public_key = PublicKey::of(&key(8)),
                6 => wrong.approved_at_ms = NOW + 1,
                _ => wrong.expires_at_ms = EXPIRY + 1,
            }
            reject(&Signed::sign(wrong, &key(7)).unwrap(), &committed);
        }
        reject(
            &Signed::sign(approval.payload.clone(), &key(8)).unwrap(),
            &committed,
        );
        for change in 0..7 {
            let mut wrong = committed.clone();
            match change {
                0 => wrong.receipt.payload.input_hash = ObjectHash::body(&"another input").unwrap(),
                1 => wrong.receipt.payload.context.snapshot_id = Nonce::from_bytes([91; 32]),
                2 => {
                    wrong.receipt.payload.request_hash =
                        ObjectHash::body(&"another request").unwrap()
                }
                3 => wrong.receipt.payload.accepted_at_ms = EXPIRY,
                4 => wrong.receipt.payload.target_node_public_key = PublicKey::of(&key(8)),
                5 => wrong.receipt.payload.grant_leaf = ObjectHash::body(&"another grant").unwrap(),
                _ => wrong.receipt.payload.committed_space_head.sequence += 1,
            }
            wrong.receipt = Signed::sign(wrong.receipt.payload, &key(4)).unwrap();
            reject(&approval, &wrong);
        }
        let mut wrong = input.clone();
        let read = cross_request(&fixture.read).unwrap();
        let source_grant = wrong
            .journal_delta
            .iter_mut()
            .find_map(|entry| match entry {
                JournalEntry::Grant(value)
                    if value.payload.audience.device
                        == DeviceId::from_key(&read.payload.source_node_public_key) =>
                {
                    Some(value)
                }
                _ => None,
            })
            .unwrap();
        source_grant.payload.actions.push(Action::ExecutionOpen);
        *source_grant = Signed::sign(source_grant.payload.clone(), &key(51)).unwrap();
        assert!(
            verify_cross_node_recovery_stream_commit_in_state(
                &wrong,
                &fixture.read,
                &fixture.snapshot,
                &fixture.witness,
                &fixture.original.authorization.policy_snapshot,
                &PublicKey::of(&key(4)),
                NOW,
            )
            .is_err()
        );
        assert!(
            cross_node::prepare_cross_node_account_recovery(
                &fixture.material,
                &context,
                &key(51),
                &key(50),
                END as f64,
                EXPIRY as f64,
            )
            .is_err()
        );
        let mut advanced = fixture.original.authorization.policy_snapshot.clone();
        let mut account = advanced.account(&read.payload.account).unwrap().clone();
        account.previous = advanced
            .head(&PolicyDomain::Account(read.payload.account.clone()))
            .cloned();
        account.sequence += 1;
        advanced
            .install_account(Signed::sign(account, &key(1)).unwrap())
            .unwrap();
        assert!(
            verify_cross_node_recovery_stream_commit_in_state(
                &input,
                &fixture.read,
                &fixture.snapshot,
                &fixture.witness,
                &advanced,
                &PublicKey::of(&key(4)),
                NOW,
            )
            .is_err()
        );
        let mut advanced_space = fixture.original.authorization.policy_snapshot.clone();
        let mut document = advanced_space.space(fixture.read.space()).unwrap().clone();
        document.previous = advanced_space
            .head(&PolicyDomain::Space(document.space.clone()))
            .cloned();
        document.sequence += 1;
        advanced_space
            .install_space(Signed::sign(document, &key(1)).unwrap())
            .unwrap();
        assert!(
            verify_cross_node_recovery_stream_commit_in_state(
                &input,
                &fixture.read,
                &fixture.snapshot,
                &fixture.witness,
                &advanced_space,
                &PublicKey::of(&key(4)),
                NOW
            )
            .is_err()
        );
        let old_recovery = PublicKey::of(&key(2));
        let mut rotated = fixture.original.authorization.policy_snapshot.clone();
        assert!(!rotated.roots.values().any(|root| root == &old_recovery));
        assert!(
            !rotated
                .devices
                .values()
                .any(|device| device.payload.public_key == old_recovery)
        );
        let mut account = rotated.account(fixture.read.account()).unwrap().clone();
        account.previous = rotated
            .head(&PolicyDomain::Account(account.account.clone()))
            .cloned();
        account.sequence += 1;
        account.recovery.keys = vec![PublicKey::of(&key(9))];
        account.recovery.generation += 1;
        account.root_acceptance = None;
        account.recovery_approvals.clear();
        account.approve_recovery(&key(2)).unwrap();
        let recovery_policy = account.recovery.clone();
        let account = Signed::sign(account, &key(1)).unwrap();
        rotated.install_account(account.clone()).unwrap();
        let mut space = rotated.space(fixture.read.space()).unwrap().clone();
        space.previous = rotated
            .head(&PolicyDomain::Space(space.space.clone()))
            .cloned();
        space.sequence += 1;
        space.recovery = recovery_policy;
        space.root_acceptance = None;
        space.recovery_approvals.clear();
        let space = Signed::sign(space, &key(1)).unwrap();
        rotated.install_space(space.clone()).unwrap();
        let mut rotated_created = fixture.original.clone();
        rotated_created
            .bundle
            .journal
            .extend([JournalEntry::Account(account), JournalEntry::Space(space)]);
        for pin in &mut rotated_created.pins {
            pin.minimum_head = rotated.head(&pin.domain).cloned();
        }
        rotated_created.authorization = checked("rotated_policy", || {
            verify_client_policy_state(
                rotated.clone(),
                &rotated_created.bundle,
                &rotated_created.pins,
                fixture.read.account().clone(),
                fixture.read.space().clone(),
                PublicKey::of(&key(3)),
                NOW,
            )
        });
        let rotated_material = checked("rotated_material", || {
            build_account_recovery_material(
                &rotated_created,
                &key(1),
                &key(9),
                "https://source.example",
                NOW as f64,
            )
        });
        let rotated_fixture = Fixture::from_backup(rotated_created, rotated_material);
        let fresh = cross_request(&rotated_fixture.read).unwrap();
        cross_node::verify_cross_node_recovery_request(
            &rotated,
            fresh,
            &PublicKey::of(&key(4)),
            NOW,
        )
        .unwrap();
        let (fresh_input, fresh_approval, fresh_source_approval, _) = rotated_fixture.commit();
        let fresh_committed = rotated_fixture.accept(&fresh_input, &fresh_source_approval);
        let fresh_context = rotated_fixture.import(NOW);
        checked("rotated_target_install", || {
            verify_cross_node_recovery_stream_installation(
                fresh_context.clone(),
                &fresh_approval,
                &fresh_committed,
                &PublicKey::of(&key(7)),
                &Nonce::from_bytes([44; 32]),
                NOW,
            )
        });
        let claim = verify_node_claim(
            &encode(&fresh.payload.target_claim).unwrap(),
            &key(7).public_key_hex(),
            Nonce::from_bytes([44; 32]).as_str(),
            NOW as f64,
        )
        .unwrap();
        let reused_key_error =
            "recovery requires independent root and client keys unused by historical policy";
        let public_reused_key_error = format!("InvalidProof({reused_key_error:?})");
        for old_key_as_root in [true, false] {
            let next_root = key(if old_key_as_root { 2 } else { 51 });
            let next_client = key(if old_key_as_root { 50 } else { 2 });
            assert_eq!(
                cross_node::build_cross_node_recovery_request(
                    &rotated_fixture.material,
                    &next_root,
                    &next_client,
                    &claim,
                    &encode(&fresh.payload.source_read_resources).unwrap(),
                    (END - 1000) as f64,
                    NOW as f64,
                    EXPIRY as f64,
                )
                .unwrap_err(),
                public_reused_key_error,
            );
            let mut changed = fresh.payload.clone();
            if old_key_as_root {
                changed.new_root_public_key = old_recovery.clone();
            } else {
                changed.new_device_public_key = old_recovery.clone();
            }
            let changed = Signed::sign(changed, &key(9)).unwrap();
            assert!(matches!(
                cross_node::verify_cross_node_recovery_request(
                    &rotated,
                    &changed,
                    &PublicKey::of(&key(4)),
                    NOW,
                ),
                Err(AuthorizationError::InvalidProof(message)) if message == reused_key_error
            ));
            let request_hash = changed.hash().unwrap();
            let mut snapshot = rotated_fixture.snapshot.payload.clone();
            snapshot.request_hash = request_hash.clone();
            let snapshot = Signed::sign(snapshot, &key(4)).unwrap();
            assert!(matches!(
                begin_cross_node_recovery_context_import(
                    &rotated_fixture.material,
                    &encode(&changed).unwrap(),
                    &encode(&snapshot).unwrap(),
                    NOW as f64,
                ),
                Err(message) if message == public_reused_key_error
            ));
            let mut import = checked("reused_key_target_import_begin", || {
                begin_cross_node_recovery_policy_import(
                    &encode(&changed).unwrap(),
                    &encode(&snapshot).unwrap(),
                    &key(7).public_key_hex(),
                    Nonce::from_bytes([44; 32]).as_str(),
                    NOW,
                )
            });
            let mut previous = snapshot.hash().unwrap();
            for page in &rotated_fixture.pages {
                let mut page = page.payload.clone();
                page.request_hash = request_hash.clone();
                page.previous_page_hash = previous;
                let page = Signed::sign(page, &key(4)).unwrap();
                previous = page.hash().unwrap();
                checked("reused_key_target_import_page", || {
                    import.append_page(&encode(&page).unwrap(), NOW as f64)
                });
            }
            let mut witness = rotated_fixture.witness.payload.clone();
            witness.request_hash = request_hash;
            witness.final_page_hash = previous;
            let witness = Signed::sign(witness, &key(4)).unwrap();
            checked("reused_key_target_import_rejected", || {
                match import.finish_native(witness, NOW) {
                    Err(AuthorizationError::InvalidProof(message))
                        if message == reused_key_error =>
                    {
                        Ok(())
                    }
                    _ => Err("C import did not reject the exact historical key reuse"),
                }
            });
            let mut changed_possession = fresh_input.possession.clone();
            changed_possession.request = changed;
            assert!(
                approve_cross_node_recovery_import(
                    &fresh_context,
                    &changed_possession,
                    &key(7),
                    &Nonce::from_bytes([44; 32]),
                    NOW,
                )
                .is_err()
            );
            let mut changed_committed = fresh_committed.clone();
            changed_committed.request.possession = changed_possession;
            let mut changed_receipt = changed_committed.receipt.payload.clone();
            changed_receipt.request_hash =
                changed_committed.request.possession.request.hash().unwrap();
            changed_receipt.input_hash = ObjectHash::body(&changed_committed.request).unwrap();
            changed_receipt.new_device_public_key = PublicKey::of(&next_client);
            changed_committed.receipt = Signed::sign(changed_receipt, &key(4)).unwrap();
            let mut changed_approval = fresh_approval.payload.clone();
            changed_approval.new_root_public_key = PublicKey::of(&next_root);
            changed_approval.new_device_public_key = PublicKey::of(&next_client);
            changed_approval.possession_hash =
                ObjectHash::body(&changed_committed.request.possession).unwrap();
            assert!(
                verify_cross_node_recovery_stream_installation(
                    fresh_context.clone(),
                    &Signed::sign(changed_approval, &key(7)).unwrap(),
                    &changed_committed,
                    &PublicKey::of(&key(7)),
                    &Nonce::from_bytes([44; 32]),
                    NOW,
                )
                .is_err()
            );
        }
    }
}
