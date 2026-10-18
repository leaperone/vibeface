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

fn verify_context_binding(
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
    cross_node::verify_possession_in_state(
        original,
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

pub(crate) fn verify_cross_node_recovery_stream_commit_in_context(
    input: &StreamedCrossNodeRecoveryCommitRequest,
    context: &VerifiedRecoveryPolicyContext,
    now: u64,
) -> Result<VerifiedStreamedCrossNodeRecoveryCommit> {
    let cached = CachedPolicyProof::from_verified_prefix(
        context.verified_state(),
        context.policy().journal(),
    )?;
    verify_stream_commit_cached(
        input,
        context.request(),
        context.snapshot(),
        context.witness(),
        context.verified_state(),
        context.request().source_node_public_key(),
        now,
        Some(&cached),
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
    verify_context_binding(
        &committed.request.possession,
        context.request(),
        context.snapshot(),
        context.witness(),
        context.verified_state(),
        approved.approved_at_ms,
    )?;
    let verified = verify_cross_node_recovery_stream_commit_in_context(
        &committed.request,
        &context,
        value.accepted_at_ms,
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
        .apply_delta(&committed.request.journal_delta)?
        .with_independent_pins(&committed.request.pins)?;
    policy
        .state()
        .verify_transport_device(&DeviceId::from_key(local_target), now)?;
    let cached = CachedPolicyProof::from_verified_prefix(
        context.verified_state(),
        context.policy().journal(),
    )?;
    let mut usage = possession_usage(
        &committed.request.possession,
        context.request(),
        context.snapshot(),
        context.witness(),
        context.verified_state(),
        Some(&cached),
        canonical(&(approval, committed))?.len(),
    )?;
    record_delta_usage(
        &mut usage,
        &committed.request,
        context.verified_state(),
        Some(&cached),
    )?;
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
        value
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
            let material = checked("material", || {
                build_account_recovery_material(
                    &original,
                    &key(1),
                    &key(2),
                    "https://source.example",
                    10_000.0,
                )
            });
            let project = original
                .authorization
                .policy_snapshot
                .resources
                .values()
                .find(|record| {
                    record.payload.kind == ResourceKind::Project
                        && record.payload.host == ServiceId::from_key(&PublicKey::of(&key(4)))
                })
                .unwrap();
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
                    &encode(&vec![ResourceRef {
                        space: project.payload.space.clone(),
                        resource: project.payload.resource.clone(),
                    }])
                    .unwrap(),
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
            for page in &self.pages {
                checked("import_page", || {
                    import.append_page(&encode(page).unwrap(), at as f64)
                });
            }
            checked("import_finish", || {
                import.finish(&encode(&self.witness).unwrap(), at as f64)
            })
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
            for page in &self.pages {
                checked("client_import_page", || {
                    import.append_page(&encode(page).unwrap(), at as f64)
                });
            }
            checked("client_import_finish", || {
                import.finish(&encode(&self.witness).unwrap(), at as f64)
            })
        }

        fn source_cache(&self) -> Result<VerifiedRecoverySourceProof> {
            verify_recovery_source_proof_prefix(
                &self.read,
                &self.snapshot,
                &self.witness,
                &self.original.authorization.policy_snapshot,
                &self.original.bundle.journal,
                &PublicKey::of(&key(4)),
                NOW,
            )
        }

        fn commit(
            &self,
        ) -> (
            StreamedCrossNodeRecoveryCommitRequest,
            Signed<CrossNodeRecoveryImportApproval>,
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
            let source_proof = checked("source_possession", || {
                let source_cache = self.source_cache()?;
                sign_cross_node_recovery_source_possession_with_source_proof(
                    &possession,
                    &self.read,
                    &self.snapshot,
                    &self.witness,
                    &self.original.authorization.policy_snapshot,
                    &source_cache,
                    &key(4),
                    NOW,
                )
            });
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
            (commit, approval, pending)
        }

        fn accept(
            &self,
            input: &StreamedCrossNodeRecoveryCommitRequest,
        ) -> StreamedCrossNodeRecoveryCommitted {
            let signatures = SignatureVerification::new();
            let committed = signatures
                .run(|| {
                    let source_cache = self.source_cache()?;
                    let verified = verify_cross_node_recovery_stream_commit_with_source_proof(
                        input,
                        &self.read,
                        &self.snapshot,
                        &self.witness,
                        &self.original.authorization.policy_snapshot,
                        &source_cache,
                        &PublicKey::of(&key(4)),
                        NOW,
                    )
                    .unwrap();
                    accept_cross_node_recovery_stream_commit(&verified, &key(4), NOW)
                })
                .unwrap();
            assert!(signatures.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
            eprintln!(
                "cross stream source commit: {}",
                serde_json::to_string(&signatures.counts()).unwrap()
            );
            committed
        }
    }

    #[test]
    fn twelve_nodes_dual_possession_receipt_and_one_use_installation() {
        let fixture = Fixture::new(12);
        assert!(canonical(&fixture.original.bundle).unwrap().len() > MAX_PROOF_BYTES);
        let (input, approval, mut pending) = fixture.commit();
        assert!(canonical(&input).unwrap().len() <= MAX_PROOF_BYTES);
        let committed = fixture.accept(&input);
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
    fn streamed_cross_rejects_changed_context_approvals_receipts_and_source_privileges() {
        let fixture = Fixture::new(0);
        let (input, approval, _) = fixture.commit();
        let committed = fixture.accept(&input);
        let context = fixture.import(NOW);
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
    }
}
