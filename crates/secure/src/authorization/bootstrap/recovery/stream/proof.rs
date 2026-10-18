use super::*;
use crate::authorization::{
    execution_handoff::{ExecutionProofCounts, ExecutionProofUsage},
    policy_proof::{
        CachedPolicyProof, policy_entry_payload_usage, policy_entry_usage,
        policy_entry_usage_with_cache,
    },
};

#[derive(Clone, Debug, Serialize)]
pub struct RecoveryProofCounts {
    pub carrier_bytes: usize,
    pub proof: ExecutionProofCounts,
}

#[derive(Clone, Debug)]
pub struct RecoveryProofUsage {
    carrier_bytes: usize,
    usage: ExecutionProofUsage,
}

impl RecoveryProofUsage {
    #[cfg(test)]
    pub(crate) fn diagnostic_counts(&self) -> RecoveryProofCounts {
        RecoveryProofCounts {
            carrier_bytes: self.carrier_bytes,
            proof: self.usage.counts(),
        }
    }
    pub fn new(carrier_bytes: usize) -> Result<Self> {
        require(
            carrier_bytes <= MAX_PROOF_BYTES,
            "complete recovery carrier exceeds 64 KiB",
        )?;
        Ok(Self {
            carrier_bytes,
            usage: ExecutionProofUsage::default(),
        })
    }
    pub fn record_request(&mut self, request: &RecoveryReadRequest) -> Result<()> {
        match request {
            RecoveryReadRequest::Account(value) => self.usage.record_signed(value)?,
            RecoveryReadRequest::CrossNode(value) => {
                self.usage.record_signed(value)?;
                self.usage.record_signed(&value.payload.target_claim)?;
            }
        }
        self.usage.enforce_limits()
    }
    pub fn record_snapshot(&mut self, value: &Signed<RecoveryContextSnapshot>) -> Result<()> {
        self.usage.record_signed(value)?;
        self.usage.record_signed(&value.payload.claim)?;
        self.usage.enforce_limits()
    }
    /// The old page authenticates a stored cursor; its old policy entry is not replayed.
    pub fn record_page_binding(&mut self, value: &Signed<RecoveryContextPage>) -> Result<()> {
        self.usage.record_signed(value)?;
        self.usage.enforce_limits()
    }
    /// Source-side output is already accepted history, without a new transition.
    pub fn record_outgoing_page(&mut self, value: &Signed<RecoveryContextPage>) -> Result<()> {
        self.record_page_binding(value)?;
        if let Some(entry) = &value.payload.entry {
            self.usage.merge(&policy_entry_payload_usage(entry)?);
        }
        self.usage.enforce_limits()
    }
    /// Importing a new page uses the exact verified pre-state of its transition.
    pub fn record_page(
        &mut self,
        value: &Signed<RecoveryContextPage>,
        state: &VerifiedPolicySet,
    ) -> Result<()> {
        self.record_page_binding(value)?;
        if let Some(entry) = &value.payload.entry {
            self.usage.merge(&policy_entry_usage(state, entry)?);
        }
        self.usage.enforce_limits()
    }
    pub fn record_witness(&mut self, value: &Signed<RecoveryContextWitness>) -> Result<()> {
        self.usage.record_signed(value)?;
        self.usage.enforce_limits()
    }
    pub fn record_completion(&mut self, value: &Signed<RecoveryCompletionReceipt>) -> Result<()> {
        self.usage.record_signed(value)?;
        self.usage.enforce_limits()
    }
    pub fn record_source_proposal_approval(
        &mut self,
        value: &Signed<RecoverySourceProposalApproval>,
    ) -> Result<()> {
        self.usage.record_signed(value)?;
        self.usage.enforce_limits()
    }
    pub fn record_current_source(
        &mut self,
        request: &RecoveryReadRequest,
        state: &VerifiedPolicySet,
    ) -> Result<()> {
        let account = state
            .account_history(request.account())
            .and_then(|history| history.last())
            .ok_or(AuthorizationError::Revoked)?;
        let space = state
            .space_history(request.space())
            .and_then(|history| history.last())
            .ok_or(AuthorizationError::Revoked)?;
        self.usage.record_signed(account)?;
        self.usage.record_signed(space)?;
        let device = state.find_device(&DeviceId::from_key(request.source_node_public_key()))?;
        self.usage.record_signed(device)?;
        self.record_stamp(state, &device.payload.issued_under)?;
        self.usage.enforce_limits()
    }
    fn record_stamp(&mut self, state: &VerifiedPolicySet, stamp: &PolicyStamp) -> Result<()> {
        match &stamp.domain {
            PolicyDomain::Account(id) => {
                let value = state
                    .account_history(id)
                    .and_then(|history| {
                        history
                            .iter()
                            .find(|value| account_stamp(value).as_ref() == Ok(stamp))
                    })
                    .ok_or(AuthorizationError::InvalidProof(
                        "missing recovery policy witness",
                    ))?;
                self.usage.record_signed(value)?;
            }
            PolicyDomain::Space(id) => {
                let value = state
                    .space_history(id)
                    .and_then(|history| {
                        history
                            .iter()
                            .find(|value| space_stamp(value).as_ref() == Ok(stamp))
                    })
                    .ok_or(AuthorizationError::InvalidProof(
                        "missing recovery policy witness",
                    ))?;
                self.usage.record_signed(value)?;
            }
        }
        self.usage.enforce_limits()
    }
    pub fn record_node_request(
        &mut self,
        value: &StreamedRecoveryNodeRequest,
        state: &VerifiedPolicySet,
    ) -> Result<()> {
        self.record_node_request_cached(value, state, None)
    }
    pub(crate) fn record_approved_node_request(
        &mut self,
        value: &StreamedRecoveryNodeRequest,
    ) -> Result<()> {
        self.usage.record_metadata(&value.context)?;
        self.usage
            .record_metadata(&value.request.node_certificate)?;
        self.usage.record_signed(&value.request.claim)?;
        self.usage
            .merge(&policy_entry_payload_usage(&JournalEntry::Account(
                value.request.account_recovery.clone(),
            ))?);
        self.usage.enforce_limits()
    }
    pub fn record_node_request_with_source_proof(
        &mut self,
        value: &StreamedRecoveryNodeRequest,
        state: &VerifiedPolicySet,
        source: &VerifiedRecoverySourceProof,
    ) -> Result<()> {
        self.record_node_request_cached(
            value,
            state,
            Some(source.cache_for_context(&value.context, state)?),
        )
    }
    pub(crate) fn record_node_request_cached(
        &mut self,
        value: &StreamedRecoveryNodeRequest,
        state: &VerifiedPolicySet,
        cached: Option<&CachedPolicyProof>,
    ) -> Result<()> {
        self.usage.record_metadata(&value.context)?;
        self.usage
            .record_metadata(&value.request.node_certificate)?;
        self.usage.record_signed(&value.request.claim)?;
        self.usage.merge(&policy_entry_usage_with_cache(
            state,
            &JournalEntry::Account(value.request.account_recovery.clone()),
            cached,
        )?);
        self.usage.enforce_limits()
    }
    pub fn record_commit(
        &mut self,
        value: &StreamedRecoveryCommitRequest,
        state: &VerifiedPolicySet,
    ) -> Result<()> {
        self.record_commit_cached(value, state, None)
    }
    pub fn record_commit_with_source_proof(
        &mut self,
        value: &StreamedRecoveryCommitRequest,
        state: &VerifiedPolicySet,
        source: &VerifiedRecoverySourceProof,
    ) -> Result<()> {
        self.record_commit_cached(
            value,
            state,
            Some(source.cache_for_context(&value.context, state)?),
        )
    }
    pub(crate) fn record_commit_cached(
        &mut self,
        value: &StreamedRecoveryCommitRequest,
        state: &VerifiedPolicySet,
        cached: Option<&CachedPolicyProof>,
    ) -> Result<()> {
        self.record_commit_with_account_approval(value, state, cached, None)
    }
    pub(crate) fn record_approved_commit(
        &mut self,
        value: &StreamedRecoveryCommitRequest,
        state: &VerifiedPolicySet,
        approval: &VerifiedRecoverySourceProposalApproval,
    ) -> Result<()> {
        self.record_commit_with_account_approval(
            value,
            state,
            Some(approval.cache_for_current(state)?),
            Some(approval),
        )
    }
    fn record_commit_with_account_approval(
        &mut self,
        value: &StreamedRecoveryCommitRequest,
        state: &VerifiedPolicySet,
        cached: Option<&CachedPolicyProof>,
        approval: Option<&VerifiedRecoverySourceProposalApproval>,
    ) -> Result<()> {
        self.usage.record_metadata(&serde_json::json!({"context":value.context,"request_hash":value.request_hash,
            "expected_account_head":value.expected_account_head,"expected_space_head":value.expected_space_head,
            "grant_leaf":value.grant_leaf,"new_device_public_key":value.new_device_public_key,"claim_nonce":value.claim_nonce}))?;
        let mut staged = state.clone();
        for (index, entry) in value.journal_delta.iter().enumerate() {
            if let Some(approval) = approval.filter(|_| index == 0) {
                require(
                    entry == &JournalEntry::Account(approval.account().clone()),
                    "approved recovery delta changed its exact account proposal",
                )?;
                self.usage.merge(&policy_entry_payload_usage(entry)?);
                approval::apply_locally_approved_account_recovery(
                    &mut staged,
                    approval.account(),
                    &value.expected_account_head,
                )?;
                continue;
            }
            self.usage
                .merge(&policy_entry_usage_with_cache(&staged, entry, cached)?);
            apply_policy_entry(&mut staged, &[], entry.clone())?;
        }
        #[cfg(test)]
        eprintln!(
            "stream commit union {}",
            serde_json::to_string(&self.usage.counts()).unwrap()
        );
        self.usage.enforce_limits()
    }
    pub fn merge(&mut self, other: &Self) {
        self.carrier_bytes = self.carrier_bytes.max(other.carrier_bytes);
        self.usage.merge(&other.usage);
    }
    pub(crate) fn merge_usage(&mut self, usage: &ExecutionProofUsage) {
        self.usage.merge(usage);
    }
    pub fn finish(mut self) -> Result<RecoveryProofCounts> {
        if let Some(actual) = crate::signature_verification::SignatureVerification::current_counts()
        {
            self.usage.record_actual_signature_counts(actual);
        }
        self.usage.enforce_limits()?;
        Ok(RecoveryProofCounts {
            carrier_bytes: self.carrier_bytes,
            proof: self.usage.counts(),
        })
    }
}

pub fn validate_recovery_context_page_carrier(
    page: &Signed<RecoveryContextPage>,
    state: &VerifiedPolicySet,
) -> Result<RecoveryProofCounts> {
    let mut usage = RecoveryProofUsage::new(canonical(page)?.len())?;
    usage.record_page(page, state)?;
    usage.finish()
}
