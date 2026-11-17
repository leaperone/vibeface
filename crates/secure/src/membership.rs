//! Signed, recoverable personal-network membership.
//!
//! A `DeviceId` and a `network_id` are Ed25519 public keys. The recovery root signs
//! only `Genesis` and `RootCheckpoint`; management clients sign `GovernanceEvent`s.
//!
//! A checkpoint that replaces the directory waits `RECOVERY_DELAY_MS` from its signed time;
//! until then any manager of the epoch it would end can stop it with a signed veto, and the same
//! recovery key can retire it with a rotation. Signed times and causality decide the outcome, so
//! every replica reaches the same one; a local clock only decides when a replica sees it. Nothing
//! is final: a veto signed in time that arrives late puts the earlier directory back everywhere.

// The public membership API is defined once in these private domain modules.
// Records preserve signed bytes; checked history authenticates causal input;
// projection resolves effects and recovery time; state owns history operations.
mod bindings;
mod checked_history;
mod projection;
mod records;
mod state;

pub use bindings::{VerifiedMembership, create_genesis, governance_payload, verify_membership};
pub use projection::{AuthorizationView, PendingRecovery, WriteBound};
pub use records::{
    Directory, Genesis, GovernanceAction, GovernanceEvent, IdentityKind, Member, MembershipSigner,
    Revocation, RootCheckpoint, canonical_json, validate_hash, validate_name,
};
pub use state::NetworkState;

pub const GENESIS_DOMAIN: &str = "identity-genesis";
pub const RECOVERY_DOMAIN: &str = "identity-recovery";
/// A recovery file whose key a later recovery replaced.
pub const SUPERSEDED: &str = "this recovery key was replaced by a newer recovery";
pub const RECOVERY_WAITING: &str = "another recovery is waiting to take effect";
/// A recovery file whose recovery a device stopped, or one built on it.
pub const RECOVERY_STOPPED: &str = "one of your devices stopped this recovery";
pub const NO_RECOVERY_WAITING: &str = "no recovery is waiting";
pub const LAST_MANAGER: &str = "the network needs at least one management device";
/// A recovery resting on management changes this replica does not hold yet.
pub const RECOVERY_INCOMPLETE: &str =
    "this recovery rests on device changes this Node has not received; sync it first";
pub const GOVERNANCE_DOMAIN: &str = "identity-governance";
/// The default wait for a recovery that replaces the directory.
pub const RECOVERY_DELAY_MS: u64 = 48 * 60 * 60 * 1000;
/// How far a new recovery's or veto's signed time may be from the clock of the Node taking it.
pub const RECOVERY_SKEW_MS: u64 = 10 * 60 * 1000;

/// This process's clock.
pub fn now_ms() -> u64 {
    #[cfg(target_arch = "wasm32")]
    return js_sys::Date::now() as u64;
    #[cfg(not(target_arch = "wasm32"))]
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

pub const IDENTITY_VERSION: u32 = 1;
pub const MAX_MEMBERS: usize = 1024;
pub const MAX_GOVERNANCE_EVENTS: usize = 1024;
/// Checkpoints that were not vetoed or retired by a rotation.
pub const MAX_CHECKPOINTS: usize = 64;
/// Every checkpoint, stopped ones included; each stopped one cost a veto or a rotation.
const MAX_CHECKPOINT_RECORDS: usize = 1024;
const MAX_PARENTS: usize = 64;
const MAX_SCOPE_PARENTS: usize = MAX_PARENTS * (MAX_CHECKPOINT_RECORDS + 1);
const MAX_MANAGEMENT_DEPTH: usize = 128;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[cfg(test)]
mod tests;
