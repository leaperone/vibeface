//! Source approval records a completed possession check; it grants no writer capability.
use super::*;

#[derive(Clone, Debug)]
pub struct VerifiedRecoverySourceProposalApproval {
    approval: Signed<RecoverySourceProposalApproval>,
    account_recovery: Signed<AccountDocument>,
    metadata_cache: crate::authorization::policy_proof::CachedPolicyProof,
}

#[derive(Debug)]
pub(crate) struct LocallyApprovedRecoveryAccount {
    account: Signed<AccountDocument>,
    expected_head: PolicyStamp,
}
impl LocallyApprovedRecoveryAccount {
    pub(crate) fn account(&self) -> &Signed<AccountDocument> {
        &self.account
    }
    pub(crate) fn expected_head(&self) -> &PolicyStamp {
        &self.expected_head
    }
    pub(crate) fn apply(&self, state: &mut VerifiedPolicySet) -> Result<()> {
        require(
            state.head(&self.expected_head.domain) == Some(&self.expected_head),
            "locally approved account prefix changed",
        )?;
        require(
            state
                .accounts
                .get(&self.account.payload.account)
                .is_some_and(|account| account.history.len() < MAX_POLICY_HISTORY),
            "account history requires checkpoint",
        )?;
        let stamp = account_stamp(&self.account)?;
        state
            .roots
            .insert(stamp.clone(), self.account.payload.current_root.clone());
        let account = state
            .accounts
            .get_mut(&self.account.payload.account)
            .ok_or(AuthorizationError::PolicyConflict)?;
        account.history.push(self.account.clone());
        account.stamp = stamp;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub fn approve_recovery_source_proposal(
    proposal: &StreamedRecoveryNodeRequest,
    read: &RecoveryReadRequest,
    snapshot: &Signed<RecoveryContextSnapshot>,
    witness: &Signed<RecoveryContextWitness>,
    before: &VerifiedPolicySet,
    source: &Identity,
    expected_nonce: &Nonce,
    accepted_prefix_commitment: &ObjectHash,
    now: u64,
) -> Result<(Signature, Signed<RecoverySourceProposalApproval>)> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let proof = sign_recovery_node_possession_in_state(
        proposal,
        read,
        snapshot,
        witness,
        before,
        source,
        expected_nonce,
        now,
    )?;
    let approval = sign_source_proposal_approval_after_verification(
        read,
        &proposal.context,
        &ObjectHash::body(proposal)?,
        &proposal.request.account_recovery,
        &proof,
        before,
        source,
        accepted_prefix_commitment,
        now,
    )?;
    Ok((proof, approval))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn sign_source_proposal_approval_after_verification(
    read: &RecoveryReadRequest,
    context: &RecoveryContextReference,
    proposal_hash: &ObjectHash,
    account_recovery: &Signed<AccountDocument>,
    proof: &Signature,
    before: &VerifiedPolicySet,
    source: &Identity,
    prefix: &ObjectHash,
    now: u64,
) -> Result<Signed<RecoverySourceProposalApproval>> {
    require(
        PublicKey::of(source) == *read.source_node_public_key(),
        "source proposal approval reached another node",
    )?;
    require(
        context.request_hash == read.request_hash()?
            && account_recovery.payload.previous.as_ref()
                == before.head(&PolicyDomain::Account(read.account().clone())),
        "source proposal approval changed its original read or account prefix",
    )?;
    validate_window(read.issued_at_ms(), read.expires_at_ms(), now)?;
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
    Signed::sign(
        RecoverySourceProposalApproval {
            version: AUTH_VERSION,
            kind: read.kind(),
            context: context.clone(),
            request_hash: read.request_hash()?,
            proposal_hash: proposal_hash.clone(),
            account_recovery_hash: account_recovery.hash()?,
            source_node_possession_hash: ObjectHash::body(proof)?,
            source_node_public_key: PublicKey::of(source),
            new_device_public_key: read.new_device_public_key().clone(),
            expected_heads: heads,
            saved_pins_hash: read.saved_pins_hash()?,
            accepted_prefix_commitment: prefix.clone(),
            approved_at_ms: now,
            expires_at_ms: read.expires_at_ms(),
        },
        source,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn verify_recovery_source_proposal_approval(
    approval: &Signed<RecoverySourceProposalApproval>,
    read: &RecoveryReadRequest,
    context: &RecoveryContextReference,
    proposal_hash: &ObjectHash,
    account_recovery: &Signed<AccountDocument>,
    node_possession: &Signature,
    before: &VerifiedPolicySet,
    local_source: &PublicKey,
    accepted_prefix_commitment: &ObjectHash,
    now: u64,
) -> Result<VerifiedRecoverySourceProposalApproval> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let value = &approval.payload;
    require(
        value.version == AUTH_VERSION
            && value.kind == read.kind()
            && value.context == *context
            && value.request_hash == read.request_hash()?
            && value.proposal_hash == *proposal_hash
            && value.account_recovery_hash == account_recovery.hash()?
            && value.source_node_possession_hash == ObjectHash::body(node_possession)?
            && value.source_node_public_key == *local_source
            && approval.issuer == *local_source
            && local_source == read.source_node_public_key()
            && value.new_device_public_key == *read.new_device_public_key()
            && value.saved_pins_hash == read.saved_pins_hash()?
            && value.accepted_prefix_commitment == *accepted_prefix_commitment
            && value.expected_heads.len() == 2
            && value
                .expected_heads
                .iter()
                .zip([
                    PolicyDomain::Account(read.account().clone()),
                    PolicyDomain::Space(read.space().clone()),
                ])
                .all(|(head, domain)| head.domain == domain && before.head(&domain) == Some(head))
            && value.approved_at_ms >= read.issued_at_ms()
            && value.expires_at_ms <= read.expires_at_ms(),
        "source proposal approval changed the original proposal, possession, client or accepted prefix",
    )?;
    validate_window(value.approved_at_ms, value.expires_at_ms, now)?;
    approval.verify_signature()?;
    read.verify_approved_credentials(before, local_source, now)?;
    before.verify_service_device(read.space(), &DeviceId::from_key(local_source), now)?;
    Ok(VerifiedRecoverySourceProposalApproval {
        approval: approval.clone(),
        account_recovery: account_recovery.clone(),
        metadata_cache:
            crate::authorization::policy_proof::CachedPolicyProof::from_locally_approved_state(
                before,
            ),
    })
}

impl VerifiedRecoverySourceProposalApproval {
    pub(crate) fn cache_for_current(
        &self,
        before: &VerifiedPolicySet,
    ) -> Result<&crate::authorization::policy_proof::CachedPolicyProof> {
        require(
            self.approval
                .payload
                .expected_heads
                .iter()
                .all(|head| before.head(&head.domain) == Some(head)),
            "source-approved metadata no longer has its original exact heads",
        )?;
        Ok(&self.metadata_cache)
    }
    pub fn record_proof_usage_into(&self, usage: &mut RecoveryProofUsage) -> Result<()> {
        usage.record_source_proposal_approval(&self.approval)
    }
    pub(super) fn account(&self) -> &Signed<AccountDocument> {
        &self.account_recovery
    }
    pub(super) fn validate(
        &self,
        read: &RecoveryReadRequest,
        context: &RecoveryContextReference,
        proposal_hash: &ObjectHash,
        before: &VerifiedPolicySet,
        now: u64,
    ) -> Result<()> {
        require(
            self.approval.payload.request_hash == read.request_hash()?
                && self.approval.payload.context == *context
                && self.approval.payload.proposal_hash == *proposal_hash
                && self
                    .approval
                    .payload
                    .expected_heads
                    .iter()
                    .all(|head| before.head(&head.domain) == Some(head)),
            "approved recovery no longer matches its original proposal and exact heads",
        )?;
        validate_window(
            self.approval.payload.approved_at_ms,
            self.approval.payload.expires_at_ms,
            now,
        )
    }
}

/// Call only after verifying this node's purpose approval for the exact proposal and prefix.
pub(in crate::authorization::bootstrap::recovery) fn apply_locally_approved_account_recovery(
    state: &mut VerifiedPolicySet,
    signed: &Signed<AccountDocument>,
    expected_head: &PolicyStamp,
) -> Result<LocallyApprovedRecoveryAccount> {
    let previous = state
        .account(&signed.payload.account)
        .ok_or(AuthorizationError::Revoked)?;
    let value = &signed.payload;
    require(
        state.head(&expected_head.domain) == Some(expected_head)
            && value.previous.as_ref() == Some(expected_head)
            && expected_head.domain == PolicyDomain::Account(value.account.clone())
            && value.genesis_root == previous.genesis_root
            && value.authority_epoch > previous.authority_epoch
            && value.sequence == 0
            && value.root_generation
                == previous
                    .root_generation
                    .checked_add(1)
                    .ok_or(AuthorizationError::PolicyConflict)?
            && previous.recovery.keys.contains(&signed.issuer)
            && signed.issuer != previous.current_root
            && value.current_root != previous.current_root,
        "approved recovery changed its current account or root transition",
    )?;
    let mut budget = Budget::default();
    let bytes = value.governance_bytes()?;
    validate_root_transition(
        &signed.issuer,
        &previous.current_root,
        &value.current_root,
        previous.root_generation,
        value.root_generation,
        &value.root_acceptance,
        "account-root-accept-v2",
        &bytes,
        &mut budget,
    )?;
    recovery_approved(
        &previous.recovery,
        &value.recovery_approvals,
        "account-recovery-v2",
        &bytes,
        &mut budget,
    )?;
    validate_recovery_transition(&previous.recovery, &value.recovery)?;
    signed.verify_with_budget(&mut budget)?;
    let approved = LocallyApprovedRecoveryAccount {
        account: signed.clone(),
        expected_head: expected_head.clone(),
    };
    approved.apply(state)?;
    Ok(approved)
}
