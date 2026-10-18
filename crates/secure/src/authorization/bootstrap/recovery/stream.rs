//! Recovery-only wire carriers. Public history is authenticated one entry at a time.

use super::*;
use context::RecoveryContextRequest;
use cross_node::CrossNodeRecoveryRequest;

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
