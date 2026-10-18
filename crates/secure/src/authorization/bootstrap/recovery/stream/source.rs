//! A local source proof cache counts accepted metadata without granting authorization.
use super::*;
use crate::authorization::policy_proof::CachedPolicyProof;

#[derive(Clone, Debug)]
pub struct VerifiedRecoverySourceProof {
    request_hash: ObjectHash,
    reference: RecoveryContextReference,
    heads: Vec<PolicyStamp>,
    cache: CachedPolicyProof,
    proof_usage: RecoveryProofUsage,
}

/// The caller supplies its fixed accepted DB prefix after checking the original source
/// record and current heads in the same transaction. RPC input cannot supply this prefix.
pub fn verify_recovery_source_proof_prefix(
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    accepted_rows: &[JournalEntry],
    local_source: &PublicKey,
    now: u64,
) -> Result<VerifiedRecoverySourceProof> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    require(
        read.source_node_public_key() == local_source,
        "source proof belongs to another node",
    )?;
    read.verify(before, local_source, now)?;
    import::validate_snapshot(read, snapshot, now)?;
    let heads = [
        PolicyDomain::Account(read.account().clone()),
        PolicyDomain::Space(read.space().clone()),
    ]
    .into_iter()
    .map(|domain| {
        before
            .head(&domain)
            .cloned()
            .ok_or(AuthorizationError::PolicyConflict)
    })
    .collect::<Result<Vec<_>>>()?;
    let value = &witness.payload;
    require(
        witness.issuer == *local_source
            && value.version == AUTH_VERSION
            && value.kind == read.kind()
            && value.request_hash == read.request_hash()?
            && value.snapshot_id == snapshot.payload.snapshot_id
            && value.source_node_public_key == *local_source
            && value.claim_hash == snapshot.payload.claim.hash()?
            && value.target_heads == heads
            && snapshot.payload.target_heads == heads
            && value.page_count > 0
            && value.entry_count <= value.page_count
            && value.issued_at_ms >= snapshot.payload.issued_at_ms
            && value.expires_at_ms <= snapshot.payload.expires_at_ms,
        "source proof changed its completed read or current exact heads",
    )?;
    validate_window(value.issued_at_ms, value.expires_at_ms, now)?;
    witness.verify_signature()?;
    let cache = CachedPolicyProof::from_verified_prefix(before, accepted_rows)?;
    for value in before
        .account_history(read.account())
        .ok_or(AuthorizationError::Revoked)?
    {
        cache.require_signed(value)?;
    }
    for value in before
        .space_history(read.space())
        .ok_or(AuthorizationError::Revoked)?
    {
        cache.require_signed(value)?;
    }
    for value in before.devices.values().filter(|value| {
        value.payload.owner == PolicyDomain::Account(read.account().clone())
            || value.payload.owner == PolicyDomain::Space(read.space().clone())
    }) {
        cache.require_signed(value)?;
    }
    for value in before
        .authenticators
        .values()
        .filter(|value| &value.payload.account == read.account())
    {
        cache.require_signed(value)?;
    }
    for value in before
        .resources
        .values()
        .filter(|value| &value.payload.space == read.space())
    {
        cache.require_signed(value)?;
    }
    for value in before
        .memberships
        .values()
        .filter(|value| &value.payload.space == read.space())
    {
        cache.require_signed(value)?;
    }
    for value in before
        .grants
        .values()
        .filter(|value| &value.payload.owner_space == read.space())
    {
        cache.require_signed(value)?;
    }
    let reference = RecoveryContextReference {
        snapshot_id: snapshot.payload.snapshot_id.clone(),
        request_hash: read.request_hash()?,
        snapshot_hash: snapshot.hash()?,
        witness_hash: witness.hash()?,
    };
    let mut proof_usage = RecoveryProofUsage::new(0)?;
    proof_usage.record_request(read)?;
    proof_usage.record_snapshot(snapshot)?;
    proof_usage.record_witness(witness)?;
    proof_usage.record_current_source(read, before)?;
    proof_usage.clone().finish()?;
    Ok(VerifiedRecoverySourceProof {
        request_hash: read.request_hash()?,
        reference,
        heads,
        cache,
        proof_usage,
    })
}

impl VerifiedRecoverySourceProof {
    pub fn record_proof_usage_into(&self, usage: &mut RecoveryProofUsage) {
        usage.merge(&self.proof_usage);
    }
    pub(crate) fn cache_for_context(
        &self,
        context: &RecoveryContextReference,
        before: &VerifiedPolicySet,
    ) -> Result<&CachedPolicyProof> {
        require(
            context == &self.reference
                && self
                    .heads
                    .iter()
                    .all(|head| before.head(&head.domain) == Some(head)),
            "source proof no longer matches its exact read and current heads",
        )?;
        Ok(&self.cache)
    }
    pub(crate) fn cache_for(
        &self,
        read: &RecoveryReadRequest,
        before: &VerifiedPolicySet,
    ) -> Result<&CachedPolicyProof> {
        require(
            read.request_hash()? == self.request_hash,
            "source proof changed its original request",
        )?;
        self.cache_for_context(&self.reference, before)
    }
}
