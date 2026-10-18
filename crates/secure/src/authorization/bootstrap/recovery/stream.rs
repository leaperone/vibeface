//! Recovery-only wire carriers. Public history is authenticated one entry at a time.

use super::*;
use context::RecoveryContextRequest;
use cross_node::CrossNodeRecoveryRequest;

pub(crate) mod approval;
pub(crate) mod cross;
mod import;
mod node;
pub mod pending;
mod proof;
mod source;
pub use approval::{
    VerifiedRecoverySourceProposalApproval, approve_recovery_source_proposal,
    verify_recovery_source_proposal_approval,
};
pub use cross::{
    VerifiedCrossNodeRecoveryInstallation, VerifiedStreamedCrossNodeRecoveryCommit,
    accept_cross_node_recovery_stream_commit, approve_cross_node_recovery_import,
    approve_cross_node_recovery_source_proposal,
    sign_cross_node_recovery_source_possession_in_state,
    sign_cross_node_recovery_source_possession_with_source_proof,
    verify_cross_node_recovery_stream_commit_in_state,
    verify_cross_node_recovery_stream_commit_with_source_approval,
    verify_cross_node_recovery_stream_commit_with_source_proof,
    verify_cross_node_recovery_stream_installation,
};
pub use import::{
    RecoveryContextImport, VerifiedRecoveryPolicyContext, begin_cross_node_recovery_context_import,
    begin_cross_node_recovery_policy_import, begin_recovery_context_import,
};
pub use node::{
    VerifiedStreamedRecoveryCommit, sign_recovery_node_possession_in_state,
    verify_recovery_stream_commit_in_state, verify_recovery_stream_commit_with_source_approval,
    verify_recovery_stream_commit_with_source_proof,
};
pub use pending::{
    ConfirmedAccountRecovery, PendingAccountRecovery, build_account_recovery_pending,
    restore_account_recovery_pending,
};
pub use proof::{RecoveryProofCounts, RecoveryProofUsage, validate_recovery_context_page_carrier};
pub use source::{VerifiedRecoverySourceProof, verify_recovery_source_proof_prefix};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryContextKind {
    Account,
    CrossNode,
}

/// Local persistence wrapper; each RPC retains its original signed request shape.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RecoveryReadRequest {
    Account(Signed<RecoveryContextRequest>),
    CrossNode(Signed<CrossNodeRecoveryRequest>),
}

impl RecoveryReadRequest {
    pub(crate) fn verify_approved_credentials(
        &self,
        state: &VerifiedPolicySet,
        local_source: &PublicKey,
        now: u64,
    ) -> Result<()> {
        self.validate_saved_pins()?;
        let account = state
            .account(self.account())
            .ok_or(AuthorizationError::Revoked)?;
        let space = state
            .space(self.space())
            .ok_or(AuthorizationError::Revoked)?;
        require(
            self.source_node_public_key() == local_source
                && account.authority.public_key == *local_source
                && space.authority.public_key == *local_source
                && space.root
                    == (SpaceRoot::Personal {
                        owner: self.account().clone(),
                        genesis_root: account.genesis_root.clone(),
                    })
                && account.recovery.threshold == 1
                && self.saved_pins().iter().all(|pin| {
                    pin.genesis_root == account.genesis_root
                        && pin
                            .minimum_head
                            .as_ref()
                            .is_some_and(|head| state.roots.contains_key(head))
                }),
            "approved recovery no longer has its original current source or saved trust",
        )?;
        state.ensure_personal_current(space)?;
        validate_window(self.issued_at_ms(), self.expires_at_ms(), now)?;
        require(
            self.expires_at_ms() - self.issued_at_ms() <= NODE_CLAIM_MAX_AGE_MS,
            "approved recovery validity is too long",
        )?;
        match self {
            Self::Account(value) => {
                require(
                    value.payload.version == AUTH_VERSION
                        && account.recovery.keys == [value.issuer.clone()]
                        && value.payload.new_device_public_key != value.issuer
                        && value.payload.new_device_public_key != *local_source
                        && value.payload.new_device_public_key != account.current_root,
                    "approved recovery credentials changed",
                )?;
                value.verify_signature()
            }
            Self::CrossNode(value) => {
                let request = &value.payload;
                require(
                    request.version == AUTH_VERSION
                        && account.recovery.keys == [value.issuer.clone()]
                        && request.target_claim.payload.node_public_key != *local_source
                        && request.expires_at_ms <= request.target_claim.payload.expires_at_ms
                        && request.source_read_expires_at_ms > request.expires_at_ms
                        && BTreeSet::from([
                            request.new_root_public_key.clone(),
                            request.new_device_public_key.clone(),
                            request.target_claim.payload.node_public_key.clone(),
                            local_source.clone(),
                            value.issuer.clone(),
                        ])
                        .len()
                            == 5
                        && !request.source_read_resources.is_empty()
                        && request.source_read_resources.len() <= MAX_PROOF_OBJECTS
                        && request
                            .source_read_resources
                            .windows(2)
                            .all(|pair| pair[0].resource < pair[1].resource)
                        && request.source_read_resources.iter().all(|resource| {
                            resource.space == request.space
                                && state
                                    .resource(&resource.space, &resource.resource)
                                    .is_some_and(|record| {
                                        record.payload.kind == ResourceKind::Project
                                            && record.payload.host
                                                == ServiceId::from_key(local_source)
                                    })
                        }),
                    "approved cross-node recovery credentials or exact source selection changed",
                )?;
                validate_claim(
                    &request.target_claim,
                    &request.target_claim.payload.node_public_key,
                    &request.target_claim.payload.claim_nonce,
                    now,
                )?;
                value.verify_signature()
            }
        }
    }
    pub fn kind(&self) -> RecoveryContextKind {
        match self {
            Self::Account(_) => RecoveryContextKind::Account,
            Self::CrossNode(_) => RecoveryContextKind::CrossNode,
        }
    }
    pub fn request_hash(&self) -> Result<ObjectHash> {
        match self {
            Self::Account(value) => value.hash(),
            Self::CrossNode(value) => value.hash(),
        }
    }
    pub fn saved_pins(&self) -> &[PolicyPin] {
        match self {
            Self::Account(value) => &value.payload.saved_pins,
            Self::CrossNode(value) => &value.payload.saved_pins,
        }
    }
    pub fn saved_pins_hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(&self.saved_pins())
    }
    pub fn account(&self) -> &AccountId {
        match self {
            Self::Account(value) => &value.payload.account,
            Self::CrossNode(value) => &value.payload.account,
        }
    }
    pub fn space(&self) -> &SpaceId {
        match self {
            Self::Account(value) => &value.payload.space,
            Self::CrossNode(value) => &value.payload.space,
        }
    }
    pub fn source_node_public_key(&self) -> &PublicKey {
        match self {
            Self::Account(value) => &value.payload.node_public_key,
            Self::CrossNode(value) => &value.payload.source_node_public_key,
        }
    }
    pub fn new_device_public_key(&self) -> &PublicKey {
        match self {
            Self::Account(value) => &value.payload.new_device_public_key,
            Self::CrossNode(value) => &value.payload.new_device_public_key,
        }
    }
    pub fn nonce(&self) -> &Nonce {
        match self {
            Self::Account(value) => &value.payload.nonce,
            Self::CrossNode(value) => &value.payload.nonce,
        }
    }
    pub fn issued_at_ms(&self) -> u64 {
        match self {
            Self::Account(value) => value.payload.issued_at_ms,
            Self::CrossNode(value) => value.payload.issued_at_ms,
        }
    }
    pub fn expires_at_ms(&self) -> u64 {
        match self {
            Self::Account(value) => value.payload.expires_at_ms,
            Self::CrossNode(value) => value.payload.expires_at_ms,
        }
    }
    pub fn validate_saved_pins(&self) -> Result<()> {
        let pins = self.saved_pins();
        require(
            pins.len() == 2
                && pins.windows(2).all(|pair| pair[0].domain < pair[1].domain)
                && self.space() == &SpaceId::personal(self.account())
                && pins.iter().all(|pin| {
                    (pin.domain == PolicyDomain::Account(self.account().clone())
                        || pin.domain == PolicyDomain::Space(self.space().clone()))
                        && pin
                            .minimum_head
                            .as_ref()
                            .is_some_and(|head| head.domain == pin.domain)
                })
                && pins[0].genesis_root == pins[1].genesis_root
                && self.account() == &AccountId::from_root(&pins[0].genesis_root),
            "recovery stream requires independent personal genesis and exact saved heads",
        )
    }
    pub fn verify(
        &self,
        state: &VerifiedPolicySet,
        local_node: &PublicKey,
        now_ms: u64,
    ) -> Result<()> {
        self.validate_saved_pins()?;
        let account = state
            .account(self.account())
            .ok_or(AuthorizationError::Revoked)?;
        require(
            self.saved_pins().iter().all(|pin| {
                pin.genesis_root == account.genesis_root
                    && pin
                        .minimum_head
                        .as_ref()
                        .is_some_and(|head| state.roots.contains_key(head))
            }),
            "recovery stream changed saved genesis or exact heads",
        )?;
        require(
            self.source_node_public_key() == local_node,
            "recovery stream source pin mismatch",
        )?;
        match self {
            Self::Account(value) => state
                .verify_recovery_context_request(value, local_node, now_ms)
                .map(|_| ()),
            Self::CrossNode(value) => cross_node::verify_request_identity(state, value, now_ms),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryContextSnapshot {
    pub version: u16,
    pub kind: RecoveryContextKind,
    pub request_hash: ObjectHash,
    pub snapshot_id: Nonce,
    pub saved_pins_hash: ObjectHash,
    pub target_heads: Vec<PolicyStamp>,
    pub source_node_public_key: PublicKey,
    pub claim: Signed<NodeClaim>,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for RecoveryContextSnapshot {}
impl SignedObject for RecoveryContextSnapshot {
    const DOMAIN: &'static str = "recovery-context-snapshot-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryContextPageRequest {
    pub snapshot_id: Nonce,
    pub request_hash: ObjectHash,
    pub cursor: Option<ObjectHash>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryContextPage {
    pub version: u16,
    pub kind: RecoveryContextKind,
    pub request_hash: ObjectHash,
    pub snapshot_id: Nonce,
    pub target_heads_hash: ObjectHash,
    pub page_index: u64,
    pub previous_page_hash: ObjectHash,
    pub entry: Option<JournalEntry>,
    pub next_cursor: Option<ObjectHash>,
    pub complete: bool,
}
impl sealed::Sealed for RecoveryContextPage {}
impl SignedObject for RecoveryContextPage {
    const DOMAIN: &'static str = "recovery-context-page-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryContextWitness {
    pub version: u16,
    pub kind: RecoveryContextKind,
    pub request_hash: ObjectHash,
    pub snapshot_id: Nonce,
    pub target_heads: Vec<PolicyStamp>,
    pub final_page_hash: ObjectHash,
    pub page_count: u64,
    pub entry_count: u64,
    pub claim_hash: ObjectHash,
    pub source_node_public_key: PublicKey,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for RecoveryContextWitness {}
impl SignedObject for RecoveryContextWitness {
    const DOMAIN: &'static str = "recovery-context-stream-witness-v2";
}

pub fn recovery_target_heads_hash(heads: &[PolicyStamp]) -> Result<ObjectHash> {
    ObjectHash::body(&heads)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryContextReference {
    pub snapshot_id: Nonce,
    pub request_hash: ObjectHash,
    pub snapshot_hash: ObjectHash,
    pub witness_hash: ObjectHash,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StreamedRecoveryNodeRequest {
    pub context: RecoveryContextReference,
    pub request: RecoveryNodeRequest,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StreamedRecoveryCommitRequest {
    pub context: RecoveryContextReference,
    pub request_hash: ObjectHash,
    pub expected_account_head: PolicyStamp,
    pub expected_space_head: PolicyStamp,
    pub journal_delta: Vec<JournalEntry>,
    pub grant_leaf: ObjectHash,
    pub new_device_public_key: PublicKey,
    pub claim_nonce: Nonce,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryCompletionReceipt {
    pub version: u16,
    pub context: RecoveryContextReference,
    pub request_hash: ObjectHash,
    pub input_hash: ObjectHash,
    pub committed_account_head: PolicyStamp,
    pub committed_space_head: PolicyStamp,
    pub grant_leaf: ObjectHash,
    pub new_device_public_key: PublicKey,
    pub source_node_public_key: PublicKey,
    pub accepted_at_ms: u64,
}
impl sealed::Sealed for RecoveryCompletionReceipt {}
impl SignedObject for RecoveryCompletionReceipt {
    const DOMAIN: &'static str = "recovery-context-completion-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryCompletionStatusRequest {
    pub context: RecoveryContextReference,
    pub input_hash: ObjectHash,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoverySourceProposalApproval {
    pub version: u16,
    pub kind: RecoveryContextKind,
    pub context: RecoveryContextReference,
    pub request_hash: ObjectHash,
    pub proposal_hash: ObjectHash,
    pub account_recovery_hash: ObjectHash,
    pub source_node_possession_hash: ObjectHash,
    pub source_node_public_key: PublicKey,
    pub new_device_public_key: PublicKey,
    pub expected_heads: Vec<PolicyStamp>,
    pub saved_pins_hash: ObjectHash,
    pub accepted_prefix_commitment: ObjectHash,
    pub approved_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for RecoverySourceProposalApproval {}
impl SignedObject for RecoverySourceProposalApproval {
    const DOMAIN: &'static str = "recovery-source-proposal-approval-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StreamedCrossNodeRecoveryPossessionRequest {
    pub context: RecoveryContextReference,
    pub request: Signed<CrossNodeRecoveryRequest>,
    pub node_request: RecoveryNodeRequest,
    pub source_certificate: DeviceCertificate,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StreamedCrossNodeRecoveryCommitRequest {
    pub possession: StreamedCrossNodeRecoveryPossessionRequest,
    pub pins: Vec<PolicyPin>,
    pub journal_delta: Vec<JournalEntry>,
    pub grant_leaf: ObjectHash,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StreamedCrossNodeRecoveryCommitReceipt {
    pub version: u16,
    pub context: RecoveryContextReference,
    pub request_hash: ObjectHash,
    pub input_hash: ObjectHash,
    pub expected_account_head: PolicyStamp,
    pub expected_space_head: PolicyStamp,
    pub committed_account_head: PolicyStamp,
    pub committed_space_head: PolicyStamp,
    pub target_node_public_key: PublicKey,
    pub new_device_public_key: PublicKey,
    pub grant_leaf: ObjectHash,
    pub accepted_at_ms: u64,
}
impl sealed::Sealed for StreamedCrossNodeRecoveryCommitReceipt {}
impl SignedObject for StreamedCrossNodeRecoveryCommitReceipt {
    const DOMAIN: &'static str = "cross-node-recovery-stream-commit-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StreamedCrossNodeRecoveryCommitted {
    pub request: StreamedCrossNodeRecoveryCommitRequest,
    pub receipt: Signed<StreamedCrossNodeRecoveryCommitReceipt>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CrossNodeRecoveryImportApproval {
    pub version: u16,
    pub context: RecoveryContextReference,
    pub account: AccountId,
    pub space: SpaceId,
    pub new_device_public_key: PublicKey,
    pub new_root_public_key: PublicKey,
    pub source_node_public_key: PublicKey,
    pub target_node_public_key: PublicKey,
    pub possession_hash: ObjectHash,
    pub claim_hash: ObjectHash,
    pub approved_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for CrossNodeRecoveryImportApproval {}
impl SignedObject for CrossNodeRecoveryImportApproval {
    const DOMAIN: &'static str = "cross-node-recovery-import-approval-v2";
}
