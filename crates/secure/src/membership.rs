//! Signed, recoverable personal-network membership.
//!
//! A `DeviceId` and a `network_id` are Ed25519 public keys. The recovery root signs
//! only `Genesis` and `RootCheckpoint`; management clients sign `GovernanceEvent`s.
//!
//! A checkpoint that replaces the directory waits `RECOVERY_DELAY_MS` from its signed time;
//! until then any manager of the epoch it would end can stop it with a signed veto. Signed times
//! and causality decide the outcome, so every replica reaches the same one; a local clock only
//! decides when a replica sees it.

use crate::{Identity, validate_device_id};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

pub const GENESIS_DOMAIN: &str = "identity-genesis";
pub const RECOVERY_DOMAIN: &str = "identity-recovery";
/// A recovery file whose key a later recovery replaced.
pub const SUPERSEDED: &str = "this recovery key was replaced by a newer recovery";
pub const RECOVERY_WAITING: &str = "another recovery is waiting to take effect";
/// A recovery file whose recovery a device stopped, or one built on it.
pub const RECOVERY_STOPPED: &str = "one of your devices stopped this recovery";
pub const NO_RECOVERY_WAITING: &str = "no recovery is waiting";
pub const GOVERNANCE_DOMAIN: &str = "identity-governance";
/// How long a recovery that replaces the directory waits for a veto.
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

/// A key held in this process: a Node device key or an unlocked recovery root.
pub trait MembershipSigner {
    fn device_id(&self) -> Result<String>;
    fn sign_payload(&self, domain: &str, payload: &[u8]) -> Result<String>;
}

impl MembershipSigner for Identity {
    fn device_id(&self) -> Result<String> {
        Ok(Identity::device_id(self))
    }

    fn sign_payload(&self, domain: &str, payload: &[u8]) -> Result<String> {
        Ok(self.sign(domain, payload)?)
    }
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn verify_payload(device_id: &str, domain: &str, payload: &[u8], signature: &str) -> Result<()> {
    Ok(crate::verify(device_id, domain, payload, signature)?)
}

fn device_id(value: &str) -> Result<()> {
    Ok(validate_device_id(value)?)
}

pub const IDENTITY_VERSION: u32 = 1;
pub const MAX_MEMBERS: usize = 1024;
pub const MAX_GOVERNANCE_EVENTS: usize = 1024;
pub const MAX_CHECKPOINTS: usize = 64;
const MAX_PARENTS: usize = 64;
const MAX_MANAGEMENT_DEPTH: usize = 128;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IdentityKind {
    Node,
    Client,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub device_id: String,
    pub kind: IdentityKind,
    pub name: String,
    pub manage: bool,
}

impl Member {
    pub fn new(device_id: &str, kind: IdentityKind, name: &str, manage: bool) -> Result<Self> {
        let member = Self {
            device_id: device_id.to_owned(),
            kind,
            name: name.to_owned(),
            manage,
        };
        member.validate()?;
        Ok(member)
    }

    pub fn node(identity: &impl MembershipSigner, name: &str) -> Result<Self> {
        Self::new(&identity.device_id()?, IdentityKind::Node, name, false)
    }

    pub fn client(identity: &impl MembershipSigner, name: &str, manage: bool) -> Result<Self> {
        Self::new(&identity.device_id()?, IdentityKind::Client, name, manage)
    }

    pub fn validate(&self) -> Result<()> {
        device_id(&self.device_id)?;
        validate_name(&self.name)?;
        ensure!(
            self.kind == IdentityKind::Client || !self.manage,
            "Node identities cannot manage membership"
        );
        Ok(())
    }

    fn is_manager(&self) -> bool {
        self.kind == IdentityKind::Client && self.manage
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty() && name.len() <= 128 && !name.chars().any(char::is_control),
        "invalid device name"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Genesis {
    pub version: u32,
    /// The recovery root's public key.
    pub network_id: String,
    pub first_client: Member,
    pub first_node: Member,
    pub signature: String,
}

impl Genesis {
    pub fn create(
        root: &impl MembershipSigner,
        first_client: Member,
        first_node: Member,
    ) -> Result<Self> {
        let mut genesis = Self {
            version: IDENTITY_VERSION,
            network_id: root.device_id()?,
            first_client,
            first_node,
            signature: String::new(),
        };
        genesis.validate_shape()?;
        genesis.signature = root.sign_payload(GENESIS_DOMAIN, &genesis.signing_bytes()?)?;
        Ok(genesis)
    }

    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut unsigned = self.clone();
        unsigned.signature.clear();
        canonical_json(&unsigned)
    }

    fn validate_shape(&self) -> Result<()> {
        ensure!(
            self.version == IDENTITY_VERSION,
            "unsupported identity version"
        );
        device_id(&self.network_id)?;
        self.first_client.validate()?;
        self.first_node.validate()?;
        ensure!(
            self.first_client.is_manager() && self.first_node.kind == IdentityKind::Node,
            "genesis needs a management client and a Node"
        );
        ensure!(
            self.first_client.device_id != self.first_node.device_id,
            "genesis members must be different devices"
        );
        ensure_not_root(&self.network_id, self.members().iter())
    }

    pub fn members(&self) -> Vec<Member> {
        let mut members = vec![self.first_client.clone(), self.first_node.clone()];
        members.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        members
    }

    pub fn verify(&self) -> Result<()> {
        self.validate_shape()?;
        verify_payload(
            &self.network_id,
            GENESIS_DOMAIN,
            &self.signing_bytes()?,
            &self.signature,
        )
    }

    pub fn hash(&self) -> Result<String> {
        Ok(digest(&canonical_json(self)?))
    }
}

fn ensure_not_root<'a>(root: &str, mut members: impl Iterator<Item = &'a Member>) -> Result<()> {
    ensure!(
        members.all(|member| member.device_id != root),
        "recovery root cannot be a daily device"
    );
    Ok(())
}

/// The directory a recovery puts in place of the current one.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Directory {
    pub members: Vec<Member>,
    /// The final write bound of every earlier Node this directory does not keep. A former Node
    /// listed in neither has no write that counts.
    pub retired: Vec<Revocation>,
}

/// The only record the recovery key signs after genesis. Each one names the next recovery key,
/// so the key that signed it stops working once it takes effect: a key signs again only after
/// every recovery it signed was stopped.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RootCheckpoint {
    pub version: u32,
    pub network_id: String,
    pub epoch: u64,
    /// The key that signs the next checkpoint.
    pub recovery_key: String,
    /// `None` only rotates the key, at once: the epoch starts from everything the previous
    /// epoch decided, including its events that arrive later, so no concurrent revocation is lost.
    pub directory: Option<Directory>,
    /// When the root signed it; a new directory takes effect `RECOVERY_DELAY_MS` later.
    pub issued_at_ms: u64,
    pub signature: String,
}

impl RootCheckpoint {
    /// The next checkpoint of `state`: replaces the directory and hands recovery to `next`.
    pub fn recovery(
        state: &NetworkState,
        root: &impl MembershipSigner,
        next: &str,
        mut members: Vec<Member>,
        mut retired: Vec<Revocation>,
        issued_at_ms: u64,
    ) -> Result<Self> {
        members.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        retired.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        let directory = Some(Directory { members, retired });
        let epoch = state.epoch() + 1;
        Self::create(
            state.network_id(),
            epoch,
            root,
            next,
            directory,
            issued_at_ms,
        )
    }

    /// The next checkpoint of `state`: hands recovery to `next` and keeps the directory.
    pub fn rotation(
        state: &NetworkState,
        root: &impl MembershipSigner,
        next: &str,
    ) -> Result<Self> {
        let epoch = state.epoch() + 1;
        Self::create(state.network_id(), epoch, root, next, None, now_ms())
    }

    fn create(
        network_id: &str,
        epoch: u64,
        root: &impl MembershipSigner,
        next: &str,
        directory: Option<Directory>,
        issued_at_ms: u64,
    ) -> Result<Self> {
        let mut checkpoint = Self {
            version: IDENTITY_VERSION,
            network_id: network_id.to_owned(),
            epoch,
            recovery_key: next.to_owned(),
            directory,
            issued_at_ms,
            signature: String::new(),
        };
        checkpoint.validate_shape()?;
        ensure!(
            root.device_id()? != checkpoint.recovery_key,
            "a recovery must hand over to a new recovery key"
        );
        checkpoint.signature = root.sign_payload(RECOVERY_DOMAIN, &checkpoint.signing_bytes()?)?;
        Ok(checkpoint)
    }

    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut unsigned = self.clone();
        unsigned.signature.clear();
        canonical_json(&unsigned)
    }

    fn validate_shape(&self) -> Result<()> {
        ensure!(
            self.version == IDENTITY_VERSION && (2..=MAX_SAFE_INTEGER).contains(&self.epoch),
            "invalid recovery checkpoint"
        );
        device_id(&self.recovery_key)?;
        let Some(directory) = &self.directory else {
            return Ok(());
        };
        validate_members(&directory.members)?;
        ensure!(
            directory.retired.len() <= MAX_MEMBERS
                && directory
                    .retired
                    .windows(2)
                    .all(|pair| pair[0].device_id < pair[1].device_id),
            "duplicate or noncanonical retired devices"
        );
        for revocation in &directory.retired {
            revocation.validate()?;
            ensure!(
                directory
                    .members
                    .binary_search_by(|member| member.device_id.cmp(&revocation.device_id))
                    .is_err(),
                "a retired device cannot stay a member"
            );
        }
        ensure_not_root(&self.recovery_key, directory.members.iter())
    }

    /// `signer` is the recovery key current before this checkpoint.
    pub fn verify(&self, genesis: &Genesis, signer: &str) -> Result<()> {
        ensure!(
            self.network_id == genesis.network_id,
            "recovery checkpoint belongs to another personal network"
        );
        self.validate_shape()?;
        ensure!(
            self.recovery_key != signer,
            "a recovery must hand over to a new recovery key"
        );
        if let Some(directory) = &self.directory {
            ensure_not_root(signer, directory.members.iter())?;
        }
        verify_payload(
            signer,
            RECOVERY_DOMAIN,
            &self.signing_bytes()?,
            &self.signature,
        )
        .context("recovery checkpoint is not signed by the current recovery key")
    }

    /// What a veto names.
    pub fn hash(&self) -> Result<String> {
        Ok(digest(&canonical_json(self)?))
    }

    /// When a new directory takes effect unless vetoed.
    pub fn effective_at_ms(&self) -> u64 {
        self.issued_at_ms.saturating_add(RECOVERY_DELAY_MS)
    }
}

fn validate_members(members: &[Member]) -> Result<()> {
    ensure!(
        !members.is_empty() && members.len() <= MAX_MEMBERS,
        "invalid member count"
    );
    ensure!(
        members
            .windows(2)
            .all(|pair| pair[0].device_id < pair[1].device_id),
        "duplicate or noncanonical members"
    );
    for member in members {
        member.validate()?;
    }
    ensure!(
        members.iter().any(Member::is_manager),
        "network requires a management client"
    );
    Ok(())
}

/// A device's final write bound: its log counts up to `cutoff_seq`, along the branch that ends
/// at `cutoff_hash` if the log forked.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Revocation {
    pub device_id: String,
    pub cutoff_seq: u64,
    pub cutoff_hash: Option<String>,
}

impl Revocation {
    /// `cutoff` is the last event that counts, `None` when none does.
    pub fn new(device_id: &str, cutoff: Option<(u64, &str)>) -> Result<Self> {
        let revocation = Self {
            device_id: device_id.into(),
            cutoff_seq: cutoff.map_or(0, |(seq, _)| seq),
            cutoff_hash: cutoff.map(|(_, hash)| hash.into()),
        };
        revocation.validate()?;
        Ok(revocation)
    }

    pub fn validate(&self) -> Result<()> {
        device_id(&self.device_id)?;
        ensure!(
            self.cutoff_seq <= MAX_SAFE_INTEGER
                && (self.cutoff_seq > 0) == self.cutoff_hash.is_some(),
            "invalid cutoff"
        );
        if let Some(hash) = &self.cutoff_hash {
            validate_hash(hash)?;
        }
        Ok(())
    }

    fn bound(&self) -> (u64, Option<&str>) {
        (self.cutoff_seq, self.cutoff_hash.as_deref())
    }
}

/// Concurrent revocations of one device keep the widest bound, the same on every replica.
fn widen(revoked: &mut BTreeMap<String, Revocation>, revocation: &Revocation) {
    match revoked.get_mut(&revocation.device_id) {
        Some(known) if known.bound() >= revocation.bound() => {}
        Some(known) => *known = revocation.clone(),
        None => {
            revoked.insert(revocation.device_id.clone(), revocation.clone());
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GovernanceAction {
    Grant {
        member: Member,
    },
    Revoke(Revocation),
    /// Stops the recovery waiting on this epoch; counts only if signed before it takes effect.
    Veto {
        checkpoint: String,
        at_ms: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GovernanceEvent {
    pub version: u32,
    pub network_id: String,
    pub epoch: u64,
    pub parents: Vec<String>,
    pub issuer: String,
    pub action: GovernanceAction,
    pub signature: String,
}

impl GovernanceEvent {
    /// Builds the event on the current heads; the issuer signs `signing_bytes` afterwards.
    pub fn unsigned(
        view: &AuthorizationView,
        issuer: &str,
        action: GovernanceAction,
    ) -> Result<Self> {
        ensure!(
            view.member(issuer)?.is_manager(),
            "management permission required"
        );
        match &action {
            GovernanceAction::Grant { member } => member.validate()?,
            GovernanceAction::Revoke(revocation) => revocation.validate()?,
            GovernanceAction::Veto { checkpoint, .. } => ensure!(
                view.pending.as_ref().map(|p| &p.checkpoint) == Some(checkpoint),
                NO_RECOVERY_WAITING
            ),
        }
        Ok(Self {
            version: IDENTITY_VERSION,
            network_id: view.network_id.clone(),
            epoch: view.epoch,
            parents: view.heads.clone(),
            issuer: issuer.to_owned(),
            action,
            signature: String::new(),
        })
    }

    pub fn grant(
        admin: &impl MembershipSigner,
        view: &AuthorizationView,
        member: Member,
    ) -> Result<Self> {
        Self::unsigned(
            view,
            &admin.device_id()?,
            GovernanceAction::Grant { member },
        )?
        .sign(admin)
    }

    pub fn revoke(
        admin: &impl MembershipSigner,
        view: &AuthorizationView,
        revocation: Revocation,
    ) -> Result<Self> {
        Self::unsigned(
            view,
            &admin.device_id()?,
            GovernanceAction::Revoke(revocation),
        )?
        .sign(admin)
    }

    pub fn sign(mut self, admin: &impl MembershipSigner) -> Result<Self> {
        self.signature = admin.sign_payload(GOVERNANCE_DOMAIN, &self.signing_bytes()?)?;
        Ok(self)
    }

    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut unsigned = self.clone();
        unsigned.signature.clear();
        canonical_json(&unsigned)
    }

    pub fn hash(&self) -> Result<String> {
        Ok(digest(&canonical_json(self)?))
    }
}

/// The whole governance history: every root recovery and every epoch's events, so a replica
/// that joins after a recovery can still check who could write before it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NetworkState {
    pub genesis: Genesis,
    /// In increasing epochs; the last one is the current directory.
    pub checkpoints: Vec<RootCheckpoint>,
    /// Sorted by `(epoch, hash)`.
    pub events: Vec<GovernanceEvent>,
}

/// How much of a device's log counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteBound<'a> {
    /// A current member Node.
    Unlimited,
    /// A former Node, up to its final bound.
    Cutoff(&'a Revocation),
}

/// The directory as of one time: the last epoch in effect.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationView {
    pub network_id: String,
    pub epoch: u64,
    /// The key the next recovery must be signed with; every earlier one is superseded.
    pub recovery_key: String,
    pub heads: Vec<String>,
    pub members: Vec<Member>,
    /// Every former device with its final write bound: this epoch's revocations and the ones
    /// the last recovery retired.
    pub revoked: Vec<Revocation>,
    /// Every device that was a client in any epoch in effect; their signatures on history stay valid.
    pub attesters: Vec<String>,
    /// The recovery waiting to replace this epoch's directory.
    pub pending: Option<PendingRecovery>,
    /// Hashes of the checkpoints a veto stopped, and of any signed on top of them.
    pub stopped: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingRecovery {
    pub checkpoint: String,
    pub effective_at_ms: u64,
    pub members: Vec<Member>,
}

impl AuthorizationView {
    pub fn member(&self, device: &str) -> Result<Member> {
        ensure!(self.cutoff(device).is_none(), "device has been revoked");
        self.members
            .iter()
            .find(|member| member.device_id == device)
            .cloned()
            .context("device is not a member of this personal network")
    }

    pub fn client(&self, device: &str) -> Result<Member> {
        let member = self.member(device)?;
        ensure!(
            member.kind == IdentityKind::Client,
            "client device required"
        );
        Ok(member)
    }

    pub fn node(&self, device: &str) -> Result<Member> {
        let member = self.member(device)?;
        ensure!(member.kind == IdentityKind::Node, "Node device required");
        Ok(member)
    }

    pub fn manager(&self, device: &str) -> Result<Member> {
        let member = self.client(device)?;
        ensure!(member.manage, "management permission required");
        Ok(member)
    }

    pub fn cutoff(&self, device: &str) -> Option<u64> {
        self.revocation(device)
            .map(|revocation| revocation.cutoff_seq)
    }

    pub fn revocation(&self, device: &str) -> Option<&Revocation> {
        self.revoked
            .iter()
            .find(|revoked| revoked.device_id == device)
    }

    /// `None` when nothing `device` wrote can count here.
    pub fn write_bound(&self, device: &str) -> Option<WriteBound<'_>> {
        match self.revocation(device) {
            Some(revocation) => Some(WriteBound::Cutoff(revocation)),
            None => self.node(device).ok().map(|_| WriteBound::Unlimited),
        }
    }

    pub fn attester(&self, device: &str) -> bool {
        self.attesters
            .binary_search_by(|known| known.as_str().cmp(device))
            .is_ok()
    }

    /// The `retired` list of a recovery keeping `kept`: former devices keep their bound, and each
    /// current Node left out ends at the head this replica holds of it.
    pub fn retire(
        &self,
        kept: &[Member],
        heads: &BTreeMap<String, (u64, String)>,
    ) -> Result<Vec<Revocation>> {
        let stays = |device: &str| kept.iter().any(|member| member.device_id == device);
        let mut retired: Vec<Revocation> = self
            .revoked
            .iter()
            .filter(|revocation| !stays(&revocation.device_id))
            .cloned()
            .collect();
        for member in &self.members {
            if member.kind == IdentityKind::Node && !stays(&member.device_id) {
                let head = heads
                    .get(&member.device_id)
                    .filter(|(seq, _)| *seq > 0)
                    .map(|(seq, hash)| (*seq, hash.as_str()));
                retired.push(Revocation::new(&member.device_id, head)?);
            }
        }
        Ok(retired)
    }
}

struct ValidEvent {
    event: GovernanceEvent,
    ancestors: BTreeSet<String>,
    depth: usize,
}

/// One epoch's directory after its events.
struct EpochView {
    heads: Vec<String>,
    members: BTreeMap<String, Member>,
    revoked: BTreeMap<String, Revocation>,
    /// Its base's clients and every client granted in it.
    clients: Vec<String>,
    /// The earliest signed time of each veto here, by the checkpoint it names.
    vetoes: BTreeMap<String, u64>,
}

/// Where an epoch stands at one time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    Live,
    /// A new directory waiting for its time.
    Pending,
    /// Signed on top of an epoch not in effect yet.
    Waiting,
    Vetoed,
    /// Signed on top of a vetoed epoch.
    Void,
}

struct Epoch {
    key: String,
    /// The epoch whose key signed this one's checkpoint.
    parent: Option<usize>,
    checkpoint: Option<String>,
    effective_at_ms: u64,
    vetoed: bool,
    standing: Standing,
    view: EpochView,
}

/// The last epoch in effect; the genesis epoch always is.
fn live(epochs: &[Epoch]) -> usize {
    epochs
        .iter()
        .rposition(|epoch| epoch.standing == Standing::Live)
        .expect("the genesis epoch is in effect")
}

fn pending(epochs: &[Epoch], live: usize) -> Option<usize> {
    epochs
        .iter()
        .position(|epoch| epoch.parent == Some(live) && epoch.standing == Standing::Pending)
}

impl NetworkState {
    pub fn new(genesis: Genesis) -> Result<Self> {
        genesis.verify()?;
        Ok(Self {
            genesis,
            checkpoints: vec![],
            events: vec![],
        })
    }

    pub fn network_id(&self) -> &str {
        &self.genesis.network_id
    }

    /// The highest checkpoint epoch, whether or not it is in effect.
    pub fn epoch(&self) -> u64 {
        self.checkpoints
            .last()
            .map_or(1, |checkpoint| checkpoint.epoch)
    }

    pub fn append(&mut self, event: GovernanceEvent) -> Result<bool> {
        let hash = event.hash()?;
        if self
            .events
            .iter()
            .any(|old| old.hash().ok().as_ref() == Some(&hash))
        {
            return Ok(false);
        }
        let mut incoming = self.clone();
        incoming.events = vec![event];
        *self = self.merge(&incoming)?;
        Ok(true)
    }

    /// Every key that held or holds recovery here, oldest first.
    pub fn recovery_keys(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.genesis.network_id.as_str())
            .chain(self.checkpoints.iter().map(|c| c.recovery_key.as_str()))
    }

    /// Why `key` cannot sign the next checkpoint at `now_ms`, if it cannot.
    pub fn check_root(&self, key: &str, now_ms: u64) -> Result<()> {
        let epochs = self.epochs(now_ms)?;
        let live = live(&epochs);
        let index = epochs
            .iter()
            .position(|epoch| epoch.key == key)
            .context("recovery key does not own this personal network")?;
        match epochs[index].standing {
            Standing::Vetoed | Standing::Void => bail!(RECOVERY_STOPPED),
            Standing::Pending | Standing::Waiting => bail!(RECOVERY_WAITING),
            Standing::Live if index < live => bail!(SUPERSEDED),
            Standing::Live => ensure!(pending(&epochs, live).is_none(), RECOVERY_WAITING),
        }
        Ok(())
    }

    pub fn recover(&mut self, checkpoint: RootCheckpoint) -> Result<bool> {
        self.recover_at(checkpoint, now_ms())
    }

    /// Returns false when this exact checkpoint is already here. Earlier history is kept.
    pub fn recover_at(&mut self, checkpoint: RootCheckpoint, now_ms: u64) -> Result<bool> {
        if self.checkpoints.contains(&checkpoint) {
            return Ok(false);
        }
        ensure!(
            checkpoint.epoch == self.epoch() + 1,
            "recovery epoch rollback or conflict"
        );
        match self
            .recovery_keys()
            .find(|key| checkpoint.verify(&self.genesis, key).is_ok())
        {
            Some(signer) => self.check_root(signer, now_ms)?,
            // Not ours, or malformed: the reason against the current key.
            None => checkpoint.verify(&self.genesis, &self.project_at(now_ms)?.recovery_key)?,
        }
        let mut next = self.clone();
        next.checkpoints.push(checkpoint);
        next.project_at(now_ms)?;
        *self = next;
        Ok(true)
    }

    /// The union of both histories: commutative, associative and idempotent.
    pub fn merge(&self, incoming: &Self) -> Result<Self> {
        ensure!(
            self.genesis == incoming.genesis,
            "different personal-network genesis"
        );
        let mut checkpoints: BTreeMap<u64, &RootCheckpoint> = BTreeMap::new();
        for checkpoint in self.checkpoints.iter().chain(&incoming.checkpoints) {
            if let Some(known) = checkpoints.insert(checkpoint.epoch, checkpoint) {
                ensure!(known == checkpoint, "conflicting recovery checkpoint");
            }
        }
        let mut events = BTreeMap::new();
        for event in self.events.iter().chain(&incoming.events) {
            events.insert((event.epoch, event.hash()?), event.clone());
        }
        let merged = Self {
            genesis: self.genesis.clone(),
            checkpoints: checkpoints.into_values().cloned().collect(),
            events: events.into_values().collect(),
        };
        merged.project()?;
        Ok(merged)
    }

    /// The directory in effect now.
    pub fn project(&self) -> Result<AuthorizationView> {
        self.project_at(now_ms())
    }

    /// Checks every epoch against its own base and returns the directory in effect at `now_ms`.
    pub fn project_at(&self, now_ms: u64) -> Result<AuthorizationView> {
        let mut epochs = self.epochs(now_ms)?;
        let live = live(&epochs);
        let pending = pending(&epochs, live).map(|index| PendingRecovery {
            checkpoint: epochs[index].checkpoint.clone().unwrap_or_default(),
            effective_at_ms: epochs[index].effective_at_ms,
            members: epochs[index].view.members.values().cloned().collect(),
        });
        let stopped = epochs
            .iter()
            .filter(|epoch| matches!(epoch.standing, Standing::Vetoed | Standing::Void))
            .filter_map(|epoch| epoch.checkpoint.clone())
            .collect();
        let attesters: BTreeSet<String> = epochs
            .iter()
            .filter(|epoch| epoch.standing == Standing::Live)
            .flat_map(|epoch| epoch.view.clients.iter().cloned())
            .collect();
        let current = epochs.swap_remove(live);
        Ok(AuthorizationView {
            network_id: self.genesis.network_id.clone(),
            epoch: live as u64 + 1,
            recovery_key: current.key,
            heads: current.view.heads,
            members: current.view.members.into_values().collect(),
            revoked: current.view.revoked.into_values().collect(),
            attesters: attesters.into_iter().collect(),
            pending,
            stopped,
        })
    }

    /// Every epoch checked against its own base, and where each stands at `now_ms`. Only the
    /// standing depends on the time; whether the history is valid does not.
    fn epochs(&self, now_ms: u64) -> Result<Vec<Epoch>> {
        self.genesis.verify()?;
        ensure!(
            self.checkpoints.len() <= MAX_CHECKPOINTS
                && self
                    .checkpoints
                    .windows(2)
                    .all(|pair| pair[0].epoch < pair[1].epoch),
            "invalid recovery history"
        );
        let mut keys = Vec::with_capacity(self.events.len());
        for event in &self.events {
            keys.push((event.epoch, event.hash()?));
        }
        ensure!(
            keys.windows(2).all(|pair| pair[0] < pair[1]),
            "duplicate or noncanonical management events"
        );
        let root = &self.genesis.network_id;
        let events = self.epoch_events(1);
        let mut checked = events.len();
        let view = self.project_epoch(1, &self.genesis.members(), &[], root, &events)?;
        let mut epochs = vec![Epoch {
            key: root.clone(),
            parent: None,
            checkpoint: None,
            effective_at_ms: 0,
            vetoed: false,
            standing: Standing::Live,
            view,
        }];
        for (index, checkpoint) in self.checkpoints.iter().enumerate() {
            ensure!(
                checkpoint.epoch == index as u64 + 2,
                "recovery checkpoints must follow each other epoch by epoch"
            );
            ensure!(
                epochs
                    .iter()
                    .all(|epoch| epoch.key != checkpoint.recovery_key),
                "a recovery key cannot be used twice"
            );
            let parent = (0..epochs.len())
                .rev()
                .find(|parent| {
                    checkpoint
                        .verify(&self.genesis, &epochs[*parent].key)
                        .is_ok()
                })
                .context("recovery checkpoint is not signed by a recovery key of this network")?;
            ensure!(
                epochs
                    .iter()
                    .all(|epoch| epoch.parent != Some(parent) || epoch.vetoed),
                "a recovery key signs again only after its earlier recoveries were vetoed"
            );
            let hash = checkpoint.hash()?;
            let effective_at_ms = checkpoint.effective_at_ms();
            let vetoed = checkpoint.directory.is_some()
                && epochs[parent]
                    .view
                    .vetoes
                    .get(&hash)
                    .is_some_and(|at| *at < effective_at_ms);
            let standing = match epochs[parent].standing {
                Standing::Vetoed | Standing::Void => Standing::Void,
                Standing::Pending | Standing::Waiting => Standing::Waiting,
                Standing::Live if vetoed => Standing::Vetoed,
                Standing::Live if checkpoint.directory.is_none() || now_ms >= effective_at_ms => {
                    Standing::Live
                }
                Standing::Live => Standing::Pending,
            };
            let (base, retired) = match &checkpoint.directory {
                Some(directory) => (directory.members.clone(), directory.retired.clone()),
                None => (
                    epochs[parent].view.members.values().cloned().collect(),
                    epochs[parent].view.revoked.values().cloned().collect(),
                ),
            };
            let events = self.epoch_events(checkpoint.epoch);
            checked += events.len();
            let key = &checkpoint.recovery_key;
            let view = self.project_epoch(checkpoint.epoch, &base, &retired, key, &events)?;
            epochs.push(Epoch {
                key: key.clone(),
                parent: Some(parent),
                checkpoint: Some(hash),
                effective_at_ms,
                vetoed,
                standing,
                view,
            });
        }
        ensure!(
            checked == self.events.len(),
            "management event epoch has no recovery checkpoint"
        );
        Ok(epochs)
    }

    fn epoch_events(&self, epoch: u64) -> Vec<&GovernanceEvent> {
        self.events
            .iter()
            .filter(|event| event.epoch == epoch)
            .collect()
    }

    /// `root` is the recovery key of this epoch, which no member may be.
    fn project_epoch(
        &self,
        epoch: u64,
        base: &[Member],
        retired: &[Revocation],
        root: &str,
        events: &[&GovernanceEvent],
    ) -> Result<EpochView> {
        ensure_not_root(root, base.iter())?;
        ensure!(
            events.len() <= MAX_GOVERNANCE_EVENTS,
            "too many management events"
        );
        let mut pending = BTreeMap::new();
        let mut kinds: BTreeMap<&str, IdentityKind> = base
            .iter()
            .map(|member| (member.device_id.as_str(), member.kind))
            .collect();
        let mut clients: Vec<String> = base
            .iter()
            .filter(|member| member.kind == IdentityKind::Client)
            .map(|member| member.device_id.clone())
            .collect();
        let mut vetoes = BTreeMap::new();
        for event in events {
            ensure!(
                event.version == IDENTITY_VERSION
                    && event.network_id == self.genesis.network_id
                    && event.epoch == epoch,
                "management event network or epoch mismatch"
            );
            ensure!(
                event.parents.len() <= MAX_PARENTS
                    && event.parents.windows(2).all(|pair| pair[0] < pair[1]),
                "invalid management parents"
            );
            for parent in &event.parents {
                validate_hash(parent)?;
            }
            device_id(&event.issuer)?;
            match &event.action {
                GovernanceAction::Grant { member } => {
                    member.validate()?;
                    ensure_not_root(root, std::iter::once(member))?;
                    if let Some(kind) = kinds.insert(&member.device_id, member.kind) {
                        ensure!(
                            kind == member.kind,
                            "device cannot change between Node and client"
                        );
                    }
                    if member.kind == IdentityKind::Client {
                        clients.push(member.device_id.clone());
                    }
                }
                GovernanceAction::Revoke(revocation) => revocation.validate()?,
                GovernanceAction::Veto { checkpoint, at_ms } => {
                    validate_hash(checkpoint)?;
                    let earliest = vetoes.entry(checkpoint.clone()).or_insert(*at_ms);
                    *earliest = (*earliest).min(*at_ms);
                }
            }
            pending.insert(event.hash()?, (*event).clone());
        }
        let mut validated: BTreeMap<String, ValidEvent> = BTreeMap::new();
        while !pending.is_empty() {
            let ready = pending
                .iter()
                .filter(|(_, event)| event.parents.iter().all(|p| validated.contains_key(p)))
                .map(|(hash, _)| hash.clone())
                .collect::<Vec<_>>();
            ensure!(
                !ready.is_empty(),
                "missing or cyclic management dependencies"
            );
            for hash in ready {
                let event = pending.remove(&hash).expect("ready event exists");
                let mut ancestors = BTreeSet::new();
                let mut depth = 1;
                for parent in &event.parents {
                    let previous = &validated[parent];
                    ancestors.extend(previous.ancestors.iter().cloned());
                    ancestors.insert(parent.clone());
                    depth = depth.max(previous.depth + 1);
                }
                ensure!(
                    depth <= MAX_MANAGEMENT_DEPTH,
                    "management history is too deep"
                );
                // ponytail: re-projects the ancestor set per event, O(n²) over at most 1024 events.
                let (members, revoked) = project_members(base, retired, &validated, &ancestors);
                ensure!(
                    !revoked.contains_key(&event.issuer)
                        && members.get(&event.issuer).is_some_and(Member::is_manager),
                    "management permission required"
                );
                verify_payload(
                    &event.issuer,
                    GOVERNANCE_DOMAIN,
                    &event.signing_bytes()?,
                    &event.signature,
                )?;
                validated.insert(
                    hash,
                    ValidEvent {
                        event,
                        ancestors,
                        depth,
                    },
                );
            }
        }
        let all = validated.keys().cloned().collect();
        let (members, revoked) = project_members(base, retired, &validated, &all);
        ensure!(members.len() <= MAX_MEMBERS, "too many current members");
        let mut heads: BTreeSet<String> = validated.keys().cloned().collect();
        for entry in validated.values() {
            for parent in &entry.event.parents {
                heads.remove(parent);
            }
        }
        ensure!(
            heads.len() <= MAX_PARENTS,
            "too many concurrent management heads"
        );
        Ok(EpochView {
            heads: heads.into_iter().collect(),
            members,
            revoked,
            clients,
            vetoes,
        })
    }
}

/// Revocation wins over any grant; concurrent grants keep the narrower permission.
fn project_members(
    base: &[Member],
    retired: &[Revocation],
    events: &BTreeMap<String, ValidEvent>,
    basis: &BTreeSet<String>,
) -> (BTreeMap<String, Member>, BTreeMap<String, Revocation>) {
    let mut members: BTreeMap<String, Member> = base
        .iter()
        .map(|member| (member.device_id.clone(), member.clone()))
        .collect();
    let mut revoked: BTreeMap<String, Revocation> = retired
        .iter()
        .map(|revocation| (revocation.device_id.clone(), revocation.clone()))
        .collect();
    let mut candidates: BTreeMap<String, Vec<&String>> = BTreeMap::new();
    for hash in basis {
        match &events[hash].event.action {
            GovernanceAction::Veto { .. } => {}
            GovernanceAction::Revoke(revocation) => widen(&mut revoked, revocation),
            GovernanceAction::Grant { member } => {
                candidates
                    .entry(member.device_id.clone())
                    .or_default()
                    .push(hash);
            }
        }
    }
    for (device, grants) in candidates {
        let mut chosen: Option<Member> = None;
        for hash in grants.iter().filter(|hash| {
            !grants
                .iter()
                .any(|later| events[*later].ancestors.contains(hash.as_str()))
        }) {
            let GovernanceAction::Grant { member } = &events[*hash].event.action else {
                unreachable!("candidates are grants")
            };
            if let Some(current) = &mut chosen {
                current.manage &= member.manage;
                if member.name < current.name {
                    current.name.clone_from(&member.name);
                }
            } else {
                chosen = Some(member.clone());
            }
        }
        if let Some(member) = chosen {
            members.insert(device, member);
        }
    }
    for device in revoked.keys() {
        members.remove(device);
    }
    (members, revoked)
}

pub fn validate_hash(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "invalid hash"
    );
    Ok(())
}

/// This restricted JSON encoding is shared with the browser signing adapter.
pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    fn normalize(value: &mut Value) -> Result<()> {
        match value {
            Value::Object(object) => {
                object.sort_keys();
                for value in object.values_mut() {
                    normalize(value)?;
                }
            }
            Value::Array(array) => {
                for value in array {
                    normalize(value)?;
                }
            }
            Value::Number(number) => {
                let valid = number.as_u64().is_some_and(|n| n <= MAX_SAFE_INTEGER)
                    || number
                        .as_i64()
                        .is_some_and(|n| n.unsigned_abs() <= MAX_SAFE_INTEGER);
                ensure!(valid, "signed JSON requires safe integers");
            }
            _ => {}
        }
        Ok(())
    }
    let mut value = serde_json::to_value(value)?;
    normalize(&mut value)?;
    let bytes = serde_json::to_vec(&value)?;
    ensure!(bytes.len() <= 2 * 1024 * 1024, "identity payload too large");
    Ok(bytes)
}

fn js<T>(result: Result<T>) -> crate::Result<T> {
    Ok(result?)
}

/// Builds a genesis with the unlocked recovery root; the browser then seals that root.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = createGenesis))]
pub fn create_genesis(
    root: &Identity,
    first_client_json: &str,
    first_node_json: &str,
) -> crate::Result<String> {
    js((|| {
        let genesis = Genesis::create(
            root,
            serde_json::from_str(first_client_json)?,
            serde_json::from_str(first_node_json)?,
        )?;
        Ok(serde_json::to_string(&genesis)?)
    })())
}

/// Canonical bytes of an unsigned governance event; sign with `signingMessage("identity-governance", ...)`.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = governancePayload))]
pub fn governance_payload(event_json: &str) -> crate::Result<Vec<u8>> {
    js((|| {
        serde_json::from_str::<GovernanceEvent>(event_json)?.signing_bytes()
    })())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct VerifiedMembership {
    state: NetworkState,
    view: AuthorizationView,
    now: u64,
}

/// `now`: the Node's clock as its last answer gave it, so both sides see the same epoch in
/// effect; this process's clock when absent.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = verifyMembership))]
pub fn verify_membership(
    state_json: &str,
    expected_network: &str,
    now: Option<f64>,
) -> crate::Result<VerifiedMembership> {
    js((|| {
        ensure!(
            state_json.len() <= crate::MAX_APPLICATION_MESSAGE,
            "identity payload too large"
        );
        let state: NetworkState = serde_json::from_str(state_json)?;
        ensure!(
            state.genesis.network_id == expected_network,
            "membership belongs to another personal network"
        );
        let now = now.map_or_else(now_ms, |now| now as u64);
        let view = state.project_at(now)?;
        Ok(VerifiedMembership { state, view, now })
    })())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl VerifiedMembership {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = viewJson))]
    pub fn view_json(&self) -> crate::Result<String> {
        Ok(serde_json::to_string(&self.view)?)
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = stateJson))]
    pub fn state_json(&self) -> crate::Result<String> {
        Ok(serde_json::to_string(&self.state)?)
    }

    /// An unsigned `GovernanceEvent` for `{"type":"grant",...}`, `{"type":"revoke",...}` or
    /// `{"type":"veto","checkpoint":...,"at_ms":...}`.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = governanceTemplate))]
    pub fn governance_template(&self, issuer: &str, action_json: &str) -> crate::Result<String> {
        js((|| {
            let event =
                GovernanceEvent::unsigned(&self.view, issuer, serde_json::from_str(action_json)?)?;
            Ok(serde_json::to_string(&event)?)
        })())
    }

    /// The next recovery checkpoint, handing recovery to `next`. `heads_json` is
    /// `{device: [seq, hash]}` from a Node's log; each current Node left out keeps what it wrote
    /// up to there.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = createRecoveryCheckpoint))]
    pub fn create_recovery_checkpoint(
        &self,
        root: &Identity,
        next: &str,
        members_json: &str,
        heads_json: &str,
    ) -> crate::Result<String> {
        js((|| {
            self.current_root(root)?;
            let members: Vec<Member> = serde_json::from_str(members_json)?;
            let retired = self
                .view
                .retire(&members, &serde_json::from_str(heads_json)?)?;
            let checkpoint =
                RootCheckpoint::recovery(&self.state, root, next, members, retired, self.now)?;
            self.state
                .clone()
                .recover_at(checkpoint.clone(), self.now)?;
            Ok(serde_json::to_string(&checkpoint)?)
        })())
    }

    /// The next checkpoint, handing recovery to `next` without touching the directory.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = createKeyRotation))]
    pub fn create_key_rotation(&self, root: &Identity, next: &str) -> crate::Result<String> {
        js((|| {
            self.current_root(root)?;
            let checkpoint = RootCheckpoint::rotation(&self.state, root, next)?;
            self.state
                .clone()
                .recover_at(checkpoint.clone(), self.now)?;
            Ok(serde_json::to_string(&checkpoint)?)
        })())
    }

    fn current_root(&self, root: &Identity) -> Result<()> {
        self.state.check_root(&root.device_id(), self.now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity::generate().expect("test identity randomness")
    }

    fn setup() -> Result<(Identity, Identity, Identity, NetworkState)> {
        let root = identity();
        let a = identity();
        let node = identity();
        let state = NetworkState::new(Genesis::create(
            &root,
            Member::client(&a, "a", true)?,
            Member::node(&node, "node")?,
        )?)?;
        Ok((root, a, node, state))
    }

    fn id(identity: &Identity) -> String {
        identity.device_id()
    }

    fn hash(n: u64) -> String {
        format!("{n:064x}")
    }

    fn cut(device: &Identity, seq: u64) -> Revocation {
        let hash = hash(seq);
        Revocation::new(&id(device), (seq > 0).then_some((seq, hash.as_str())))
            .expect("valid test revocation")
    }

    #[test]
    fn device_and_network_ids_are_public_keys() -> Result<()> {
        let (root, a, node, state) = setup()?;
        assert_eq!(state.network_id(), id(&root));
        let view = state.project()?;
        assert!(view.manager(&id(&a)).is_ok());
        assert!(view.node(&id(&node)).is_ok());
        assert!(Member::new("00".repeat(32).as_str(), IdentityKind::Client, "x", false).is_err());
        assert!(Member::new(&id(&node), IdentityKind::Node, "n", true).is_err());
        assert!(Member::new(&id(&node), IdentityKind::Node, " ", false).is_err());
        Ok(())
    }

    #[test]
    fn managers_grant_management_and_ordinary_clients_cannot_govern() -> Result<()> {
        let (_, a, _, mut state) = setup()?;
        let b = identity();
        let ordinary = identity();
        state.append(GovernanceEvent::grant(
            &a,
            &state.project()?,
            Member::client(&ordinary, "ordinary", false)?,
        )?)?;
        assert!(
            GovernanceEvent::grant(
                &ordinary,
                &state.project()?,
                Member::client(&ordinary, "ordinary", true)?
            )
            .is_err()
        );
        let mut forged = GovernanceEvent::unsigned(
            &state.project()?,
            &id(&a),
            GovernanceAction::Grant {
                member: Member::client(&ordinary, "ordinary", true)?,
            },
        )?;
        forged.issuer = id(&ordinary);
        assert!(state.append(forged.sign(&ordinary)?).is_err());
        state.append(GovernanceEvent::grant(
            &a,
            &state.project()?,
            Member::client(&b, "b", true)?,
        )?)?;
        state.append(GovernanceEvent::grant(
            &b,
            &state.project()?,
            Member::client(&ordinary, "promoted", true)?,
        )?)?;
        assert!(state.project()?.manager(&id(&ordinary)).is_ok());
        Ok(())
    }

    #[test]
    fn issuer_authority_comes_from_causal_ancestors_only() -> Result<()> {
        let (_, a, _, base) = setup()?;
        let b = identity();
        let c = identity();
        let mut granted = base.clone();
        granted.append(GovernanceEvent::grant(
            &a,
            &granted.project()?,
            Member::client(&b, "b", true)?,
        )?)?;
        let early = GovernanceEvent {
            version: 1,
            network_id: base.genesis.network_id.clone(),
            epoch: 1,
            parents: vec![],
            issuer: id(&b),
            action: GovernanceAction::Grant {
                member: Member::client(&c, "c", false)?,
            },
            signature: String::new(),
        }
        .sign(&b)?;
        assert!(granted.append(early).is_err());
        Ok(())
    }

    #[test]
    fn revocation_wins_and_records_the_largest_cutoff() -> Result<()> {
        let (_, a, node, base) = setup()?;
        let b = identity();
        let mut with_b = base.clone();
        with_b.append(GovernanceEvent::grant(
            &a,
            &with_b.project()?,
            Member::client(&b, "b", true)?,
        )?)?;
        let mut left = with_b.clone();
        let mut right = with_b.clone();
        left.append(GovernanceEvent::grant(
            &b,
            &left.project()?,
            Member::node(&node, "renamed")?,
        )?)?;
        right.append(GovernanceEvent::revoke(
            &a,
            &right.project()?,
            cut(&node, 7),
        )?)?;
        let mut merged = left.merge(&right)?;
        assert_eq!(merged.project()?, right.merge(&left)?.project()?);
        assert!(merged.project()?.node(&id(&node)).is_err());
        assert_eq!(merged.project()?.cutoff(&id(&node)), Some(7));
        merged.append(GovernanceEvent::revoke(
            &a,
            &merged.project()?,
            cut(&node, 3),
        )?)?;
        assert_eq!(merged.project()?.cutoff(&id(&node)), Some(7));
        let same_seq = Revocation::new(&id(&node), Some((7, &"f".repeat(64))))?;
        merged.append(GovernanceEvent::revoke(
            &a,
            &merged.project()?,
            same_seq.clone(),
        )?)?;
        assert_eq!(merged.project()?.revocation(&id(&node)), Some(&same_seq));
        merged.append(GovernanceEvent::grant(
            &a,
            &merged.project()?,
            Member::node(&node, "rejoin")?,
        )?)?;
        assert!(merged.project()?.node(&id(&node)).is_err());
        merged.append(GovernanceEvent::revoke(&b, &merged.project()?, cut(&a, 0))?)?;
        let view = merged.project()?;
        assert!(GovernanceEvent::revoke(&a, &view, cut(&b, 0)).is_err());
        merged.append(GovernanceEvent::revoke(&b, &view, cut(&b, 0))?)?;
        assert!(merged.project()?.members.iter().all(|m| !m.manage));
        Ok(())
    }

    #[test]
    fn concurrent_grants_keep_the_narrower_permission() -> Result<()> {
        let (_, a, _, base) = setup()?;
        let b = identity();
        let c = identity();
        let mut with_b = base;
        with_b.append(GovernanceEvent::grant(
            &a,
            &with_b.project()?,
            Member::client(&b, "b", true)?,
        )?)?;
        let mut left = with_b.clone();
        let mut right = with_b;
        left.append(GovernanceEvent::grant(
            &a,
            &left.project()?,
            Member::client(&c, "zeta", true)?,
        )?)?;
        right.append(GovernanceEvent::grant(
            &b,
            &right.project()?,
            Member::client(&c, "alpha", false)?,
        )?)?;
        let member = left.merge(&right)?.project()?.client(&id(&c))?;
        assert!(!member.manage);
        assert_eq!(member.name, "alpha");
        Ok(())
    }

    #[test]
    fn device_kind_and_root_key_are_fixed() -> Result<()> {
        let (root, a, node, mut state) = setup()?;
        assert!(
            state
                .append(GovernanceEvent::grant(
                    &a,
                    &state.project()?,
                    Member::client(&node, "client", false)?
                )?)
                .is_err()
        );
        assert!(
            state
                .append(GovernanceEvent::grant(
                    &a,
                    &state.project()?,
                    Member::client(&root, "root", false)?
                )?)
                .is_err()
        );
        assert!(
            Genesis::create(
                &root,
                Member::client(&root, "r", true)?,
                Member::node(&node, "n")?
            )
            .is_err()
        );
        let next = identity();
        let keeps =
            |member: Member| -> Result<Vec<Member>> { Ok(vec![member, Member::node(&node, "n")?]) };
        assert!(
            RootCheckpoint::recovery(
                &state,
                &root,
                &id(&next),
                keeps(Member::client(&next, "n", true)?)?,
                vec![],
                0
            )
            .is_err(),
            "the next recovery key cannot be a daily device"
        );
        let keeps_root = RootCheckpoint::recovery(
            &state,
            &root,
            &id(&next),
            keeps(Member::client(&root, "r", true)?)?,
            vec![],
            0,
        )?;
        assert!(state.clone().recover(keeps_root).is_err());
        assert!(RootCheckpoint::rotation(&state, &root, &id(&root)).is_err());
        Ok(())
    }

    #[test]
    fn recovery_replaces_members_and_rejects_rollback_or_foreign_roots() -> Result<()> {
        let (root, a, node, mut state) = setup()?;
        let original = state.clone();
        let recovered = identity();
        let members = vec![
            Member::client(&recovered, "recovered", true)?,
            Member::node(&node, "node")?,
        ];
        let next = identity();
        let checkpoint =
            RootCheckpoint::recovery(&state, &root, &id(&next), members.clone(), vec![], 0)?;
        assert!(state.recover(checkpoint.clone())?);
        assert!(!state.recover(checkpoint)?);
        assert!(state.project()?.manager(&id(&recovered)).is_ok());
        assert!(state.project()?.client(&id(&a)).is_err());
        assert_eq!(state.project()?.recovery_key, id(&next));
        assert_eq!(state.merge(&original)?, state);
        assert_eq!(original.merge(&state)?, state);
        let mut conflict = original.clone();
        conflict.recover(RootCheckpoint::recovery(
            &original,
            &root,
            &id(&identity()),
            vec![Member::client(&a, "conflicting", true)?],
            vec![],
            0,
        )?)?;
        assert!(state.merge(&conflict).is_err());
        let foreign = identity();
        assert!(
            state
                .recover(RootCheckpoint::recovery(
                    &state,
                    &foreign,
                    &id(&identity()),
                    members.clone(),
                    vec![],
                    0
                )?)
                .is_err()
        );
        assert!(
            RootCheckpoint::recovery(
                &state,
                &next,
                &id(&identity()),
                vec![Member::node(&node, "n")?],
                vec![],
                0
            )
            .is_err()
        );
        assert!(
            RootCheckpoint::recovery(
                &state,
                &next,
                &id(&identity()),
                members.clone(),
                vec![cut(&node, 2)],
                0
            )
            .is_err(),
            "a retired device cannot stay a member"
        );
        let mut renamed = members;
        renamed[0].name = "renamed".into();
        assert!(
            state
                .recover(RootCheckpoint::recovery(
                    &original,
                    &root,
                    &id(&next),
                    renamed,
                    vec![],
                    0
                )?)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn a_superseded_recovery_key_signs_nothing_anywhere_in_the_chain() -> Result<()> {
        let (root, a, node, mut state) = setup()?;
        let genesis_only = state.clone();
        let next = identity();
        let keep = vec![Member::client(&a, "a", true)?, Member::node(&node, "n")?];
        state.recover(RootCheckpoint::recovery(
            &state,
            &root,
            &id(&next),
            keep.clone(),
            vec![],
            0,
        )?)?;
        let now = now_ms();
        let refused = state.check_root(&id(&root), now).unwrap_err();
        assert!(format!("{refused:#}").contains(SUPERSEDED));
        state.check_root(&id(&next), now)?;
        assert!(state.check_root(&id(&identity()), now).is_err());

        let stolen = RootCheckpoint::rotation(&state, &root, &id(&identity()))?;
        let refused = state.clone().recover(stolen.clone()).unwrap_err();
        assert!(format!("{refused:#}").contains("replaced by a newer recovery"));
        let mut forked = state.clone();
        forked.checkpoints.push(stolen);
        assert!(forked.project().is_err() && state.merge(&forked).is_err());

        let mut rival = genesis_only.clone();
        rival.recover(RootCheckpoint::recovery(
            &genesis_only,
            &root,
            &id(&identity()),
            keep.clone(),
            vec![],
            0,
        )?)?;
        assert!(state.merge(&rival).is_err() && rival.merge(&state).is_err());

        let skip = RootCheckpoint::create(state.network_id(), 4, &next, &id(&identity()), None, 0)?;
        assert!(state.clone().recover(skip.clone()).is_err());
        let mut gap = state.clone();
        gap.checkpoints.push(skip);
        assert!(gap.project().is_err());

        let verified = verify_membership(&serde_json::to_string(&state)?, &id(&root), None)?;
        let error = verified
            .create_key_rotation(&root, &id(&identity()))
            .unwrap_err();
        assert!(format!("{error:?}").contains("replaced by a newer recovery"));
        assert!(
            verified
                .create_key_rotation(&identity(), &id(&identity()))
                .is_err()
        );
        let rotated: RootCheckpoint =
            serde_json::from_str(&verified.create_key_rotation(&next, &id(&identity()))?)?;
        assert!(state.recover(rotated)?);
        let refused = state.check_root(&id(&next), now_ms()).unwrap_err();
        assert!(format!("{refused:#}").contains(SUPERSEDED));
        Ok(())
    }

    #[test]
    fn a_key_rotation_keeps_the_directory_and_late_revocations() -> Result<()> {
        let (root, a, node, mut state) = setup()?;
        let b = identity();
        state.append(GovernanceEvent::grant(
            &a,
            &state.project()?,
            Member::client(&b, "b", true)?,
        )?)?;
        let mut late = state.clone();
        late.append(GovernanceEvent::revoke(
            &b,
            &late.project()?,
            cut(&node, 7),
        )?)?;
        let next = identity();
        state.recover(RootCheckpoint::rotation(&state, &root, &id(&next))?)?;
        let view = state.project()?;
        assert_eq!(
            (view.epoch, view.recovery_key.as_str()),
            (2, id(&next).as_str()),
            "a rotation takes effect at once"
        );
        assert!(view.pending.is_none());
        assert!(view.manager(&id(&b)).is_ok() && view.node(&id(&node)).is_ok());

        state.append(GovernanceEvent::grant(
            &a,
            &view,
            Member::client(&identity(), "c", false)?,
        )?)?;
        let merged = state.merge(&late)?;
        assert_eq!(merged, late.merge(&state)?);
        let view = merged.project()?;
        assert_eq!(view.epoch, 2);
        assert_eq!(
            view.write_bound(&id(&node)),
            Some(WriteBound::Cutoff(&cut(&node, 7))),
            "a revocation the rotation never saw still counts"
        );
        assert_eq!(view.members.len(), 3);
        Ok(())
    }

    #[test]
    fn missing_parents_wrong_epoch_and_tampering_fail_closed() -> Result<()> {
        let (_, a, node, mut state) = setup()?;
        let valid = GovernanceEvent::revoke(&a, &state.project()?, cut(&node, 1))?;
        let mut missing = valid.clone();
        missing.parents = vec!["0".repeat(64)];
        assert!(state.append(missing.sign(&a)?).is_err());
        let mut epoch = valid.clone();
        epoch.epoch += 1;
        assert!(state.append(epoch.sign(&a)?).is_err());
        let mut tampered = valid.clone();
        tampered.action = GovernanceAction::Revoke(cut(&a, 1));
        assert!(state.append(tampered).is_err());
        let mut changed = state.genesis.clone();
        changed.first_client.name = "other".into();
        assert!(changed.verify().is_err());
        assert!(state.append(valid.clone())?);
        assert!(!state.append(valid)?);
        Ok(())
    }

    #[test]
    fn canonical_signatures_have_shared_browser_encoding() -> Result<()> {
        assert_eq!(
            canonical_json(&serde_json::json!({"z":1,"a":{"b":2,"a":true}}))?,
            br#"{"a":{"a":true,"b":2},"z":1}"#
        );
        assert!(canonical_json(&serde_json::json!({"n":9_007_199_254_740_992u64})).is_err());
        assert!(canonical_json(&serde_json::json!({"n":1.5})).is_err());
        Ok(())
    }

    #[test]
    fn browser_helpers_verify_before_building_or_signing() -> Result<()> {
        let (root, a, node, state) = setup()?;
        let state_json = serde_json::to_string(&state)?;
        let verified = verify_membership(&state_json, &id(&root), None)?;
        assert!(verify_membership(&state_json, &id(&a), None).is_err());
        let mut forged = state.clone();
        forged.genesis.signature = "00".repeat(64);
        assert!(verify_membership(&serde_json::to_string(&forged)?, &id(&root), None).is_err());

        let action = serde_json::json!({"type":"revoke","device_id":id(&node),
            "cutoff_seq":4,"cutoff_hash":hash(4)});
        assert_eq!(
            serde_json::to_value(GovernanceAction::Revoke(cut(&node, 4)))?,
            action
        );
        let unhashed = serde_json::json!({"type":"revoke","device_id":id(&node),"cutoff_seq":4});
        assert!(
            verified
                .governance_template(&id(&a), &unhashed.to_string())
                .is_err()
        );
        let template = verified.governance_template(&id(&a), &action.to_string())?;
        assert!(
            verified
                .governance_template(&id(&node), &action.to_string())
                .is_err()
        );
        let mut event: GovernanceEvent = serde_json::from_str(&template)?;
        let payload = governance_payload(&template)?;
        let message = crate::signing_message(GOVERNANCE_DOMAIN, &payload)?;
        event.signature = a.sign_message(&message);
        let mut next = state.clone();
        next.append(event)?;
        assert_eq!(next.project()?.cutoff(&id(&node)), Some(4));

        let members = serde_json::to_string(&vec![Member::client(&a, "a", true)?])?;
        let heads = serde_json::json!({ id(&node): [9, hash(9)] }).to_string();
        let next = id(&identity());
        let signed = verified.create_recovery_checkpoint(&root, &next, &members, &heads)?;
        let checkpoint: RootCheckpoint = serde_json::from_str(&signed)?;
        checkpoint.verify(&state.genesis, &id(&root))?;
        assert_eq!((checkpoint.epoch, &checkpoint.recovery_key), (2, &next));
        assert_eq!(
            checkpoint.directory.map(|directory| directory.retired),
            Some(vec![cut(&node, 9)])
        );
        let signed = verified.create_recovery_checkpoint(&root, &next, &members, "{}")?;
        let checkpoint: RootCheckpoint = serde_json::from_str(&signed)?;
        assert_eq!(
            checkpoint.directory.map(|directory| directory.retired),
            Some(vec![cut(&node, 0)])
        );
        assert!(
            verified
                .create_recovery_checkpoint(&a, &next, &members, &heads)
                .is_err()
        );
        assert!(
            verified
                .create_recovery_checkpoint(&root, &id(&root), &members, &heads)
                .is_err()
        );
        let genesis = create_genesis(
            &root,
            &serde_json::to_string(&Member::client(&a, "a", true)?)?,
            &serde_json::to_string(&Member::node(&node, "n")?)?,
        )?;
        serde_json::from_str::<Genesis>(&genesis)?.verify()?;
        Ok(())
    }

    #[test]
    fn merge_is_a_union_in_any_order() -> Result<()> {
        let (root, a, node, base) = setup()?;
        let b = identity();
        let c = identity();
        let mut granted = base.clone();
        granted.append(GovernanceEvent::grant(
            &a,
            &granted.project()?,
            Member::client(&b, "b", true)?,
        )?)?;
        let mut revoked = base.clone();
        revoked.append(GovernanceEvent::revoke(
            &a,
            &revoked.project()?,
            cut(&node, 2),
        )?)?;
        let mut recovered = base.clone();
        recovered.recover(RootCheckpoint::recovery(
            &base,
            &root,
            &id(&identity()),
            vec![Member::client(&a, "a", true)?, Member::node(&node, "node")?],
            vec![],
            0,
        )?)?;
        recovered.append(GovernanceEvent::grant(
            &a,
            &recovered.project()?,
            Member::client(&c, "c", false)?,
        )?)?;
        let parts = [&base, &granted, &revoked, &recovered];
        let mut results = Vec::new();
        for order in [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1], [1, 3, 0, 2]] {
            let mut state = parts[order[0]].clone();
            for index in &order[1..] {
                state = state.merge(parts[*index])?;
            }
            assert_eq!(state.merge(&state)?, state);
            results.push(state);
        }
        assert!(results.windows(2).all(|pair| pair[0] == pair[1]));
        let state = &results[0];
        assert_eq!(state.epoch(), 2);
        assert_eq!(state.events.len(), 3);
        let view = state.project()?;
        assert!(view.client(&id(&b)).is_err() && view.attester(&id(&b)));
        assert_eq!(view.write_bound(&id(&node)), Some(WriteBound::Unlimited));
        assert!(view.client(&id(&c)).is_ok());
        Ok(())
    }

    #[test]
    fn recovery_keeps_history_retires_left_out_nodes_and_old_events_cannot_widen() -> Result<()> {
        let (root, a, node, mut state) = setup()?;
        let b = identity();
        let kept = identity();
        let left = identity();
        let gone = identity();
        for member in [
            Member::client(&b, "b", false)?,
            Member::node(&kept, "kept")?,
            Member::node(&left, "left")?,
            Member::node(&gone, "gone")?,
        ] {
            state.append(GovernanceEvent::grant(&a, &state.project()?, member)?)?;
        }
        state.append(GovernanceEvent::revoke(
            &a,
            &state.project()?,
            cut(&gone, 3),
        )?)?;
        let epoch_one = state.clone();
        let recovered = identity();
        let members = vec![
            Member::client(&recovered, "recovered", true)?,
            Member::node(&kept, "kept")?,
        ];
        let heads = serde_json::json!({ id(&left): [5, hash(5)], id(&kept): [8, hash(8)] });
        let verified = verify_membership(&serde_json::to_string(&state)?, &id(&root), Some(0.0))?;
        let checkpoint: RootCheckpoint =
            serde_json::from_str(&verified.create_recovery_checkpoint(
                &root,
                &id(&identity()),
                &serde_json::to_string(&members)?,
                &heads.to_string(),
            )?)?;
        assert_eq!(
            checkpoint.directory.as_ref().map(|d| d.retired.clone()),
            Some({
                let mut retired = vec![cut(&gone, 3), cut(&left, 5), cut(&node, 0)];
                retired.sort_by(|x, y| x.device_id.cmp(&y.device_id));
                retired
            })
        );
        state.recover(checkpoint)?;
        assert_eq!(state.events.len(), 5);
        assert_eq!(state.checkpoints.len(), 1);
        let view = state.project()?;
        assert_eq!(view.epoch, 2);
        assert_eq!(view.write_bound(&id(&kept)), Some(WriteBound::Unlimited));
        assert_eq!(
            view.write_bound(&id(&left)),
            Some(WriteBound::Cutoff(&cut(&left, 5)))
        );
        assert_eq!(
            view.write_bound(&id(&gone)),
            Some(WriteBound::Cutoff(&cut(&gone, 3)))
        );
        assert_eq!(view.write_bound(&id(&identity())), None);
        for client in [&a, &b, &recovered] {
            assert!(view.attester(&id(client)));
        }
        assert!(!view.attester(&id(&kept)));
        assert!(view.client(&id(&a)).is_err());

        state.append(GovernanceEvent::grant(
            &recovered,
            &view,
            Member::node(&left, "back")?,
        )?)?;
        assert!(state.project()?.node(&id(&left)).is_err());

        let mut late = epoch_one.clone();
        late.append(GovernanceEvent::revoke(
            &a,
            &late.project()?,
            cut(&left, 9),
        )?)?;
        let merged = state.merge(&late)?;
        assert_eq!(merged, late.merge(&state)?);
        assert_eq!(
            merged.project()?.revocation(&id(&left)),
            Some(&cut(&left, 5))
        );
        Ok(())
    }

    /// Signed at a fixed time, so each test picks the moment it projects at.
    const T: u64 = 1_800_000_000_000;

    /// A recovery of `state` that keeps only `node` and adds a new manager, signed at `issued`.
    fn recovering(
        state: &NetworkState,
        root: &Identity,
        node: &Identity,
        issued: u64,
    ) -> Result<(Identity, Identity, RootCheckpoint)> {
        let (manager, next) = (identity(), identity());
        let members = vec![
            Member::client(&manager, "found", true)?,
            Member::node(node, "node")?,
        ];
        let checkpoint =
            RootCheckpoint::recovery(state, root, &id(&next), members, vec![], issued)?;
        Ok((manager, next, checkpoint))
    }

    fn veto(
        admin: &Identity,
        view: &AuthorizationView,
        checkpoint: &RootCheckpoint,
        at_ms: u64,
    ) -> Result<GovernanceEvent> {
        let checkpoint = checkpoint.hash()?;
        GovernanceEvent::unsigned(
            view,
            &id(admin),
            GovernanceAction::Veto { checkpoint, at_ms },
        )?
        .sign(admin)
    }

    fn refused(result: Result<impl std::fmt::Debug>, reason: &str) {
        let error = format!("{:#}", result.expect_err(reason));
        assert!(error.contains(reason), "{error}");
    }

    #[test]
    fn a_waiting_recovery_takes_effect_after_the_delay() -> Result<()> {
        let (root, a, node, mut state) = setup()?;
        let (found, next, checkpoint) = recovering(&state, &root, &node, T)?;
        assert!(state.recover_at(checkpoint.clone(), T)?);
        let due = T + RECOVERY_DELAY_MS;
        let before = state.project_at(due - 1)?;
        assert_eq!(
            (before.epoch, before.recovery_key.as_str()),
            (1, id(&root).as_str())
        );
        assert!(before.manager(&id(&a)).is_ok() && before.client(&id(&found)).is_err());
        assert!(!before.attester(&id(&found)));
        let pending = before.pending.context("the recovery waits")?;
        assert_eq!(pending.checkpoint, checkpoint.hash()?);
        assert_eq!(pending.effective_at_ms, due);
        assert!(pending.members.iter().any(|m| m.device_id == id(&found)));

        // Nothing more is signed while it waits: not by the old file, not by the new one.
        refused(state.check_root(&id(&root), T), RECOVERY_WAITING);
        refused(state.check_root(&id(&next), T), RECOVERY_WAITING);
        let rotation = RootCheckpoint::rotation(&state, &root, &id(&identity()))?;
        refused(
            state.clone().recover_at(rotation.clone(), T),
            RECOVERY_WAITING,
        );
        let stacked = RootCheckpoint::rotation(&state, &next, &id(&identity()))?;
        refused(state.clone().recover_at(stacked, T), RECOVERY_WAITING);
        let mut sibling = state.clone();
        sibling.checkpoints.push(rotation);
        assert!(
            sibling.project_at(T).is_err(),
            "a second branch fails closed"
        );

        let after = state.project_at(due)?;
        assert_eq!(
            (after.epoch, after.recovery_key.as_str()),
            (2, id(&next).as_str())
        );
        assert!(after.manager(&id(&found)).is_ok() && after.client(&id(&a)).is_err());
        assert!(after.pending.is_none() && after.stopped.is_empty());
        assert!(after.attester(&id(&found)) && after.attester(&id(&a)));
        state.check_root(&id(&next), due)?;
        refused(state.check_root(&id(&root), due), SUPERSEDED);
        Ok(())
    }

    #[test]
    fn a_veto_keeps_the_directory_and_the_old_file() -> Result<()> {
        let (root, a, node, mut state) = setup()?;
        let (found, next, checkpoint) = recovering(&state, &root, &node, T)?;
        state.recover_at(checkpoint.clone(), T)?;
        // The browser builds it from the Node's clock.
        let verified =
            verify_membership(&serde_json::to_string(&state)?, &id(&root), Some(T as f64))?;
        let action =
            serde_json::json!({"type":"veto","checkpoint":checkpoint.hash()?,"at_ms":T + 1});
        let mut event: GovernanceEvent =
            serde_json::from_str(&verified.governance_template(&id(&a), &action.to_string())?)?;
        event.signature = a.sign_message(&crate::signing_message(
            GOVERNANCE_DOMAIN,
            &event.signing_bytes()?,
        )?);
        assert!(state.append(event)?);
        for at in [T, T + RECOVERY_DELAY_MS, T * 2] {
            let view = state.project_at(at)?;
            assert_eq!(
                (view.epoch, view.recovery_key.as_str()),
                (1, id(&root).as_str())
            );
            assert!(view.manager(&id(&a)).is_ok() && view.client(&id(&found)).is_err());
            assert!(view.pending.is_none() && !view.attester(&id(&found)));
            assert_eq!(view.stopped, vec![checkpoint.hash()?]);
        }
        state.check_root(&id(&root), T)?;
        refused(state.check_root(&id(&next), T), RECOVERY_STOPPED);
        let mut stacked = state.clone();
        stacked
            .checkpoints
            .push(RootCheckpoint::rotation(&state, &next, &id(&identity()))?);
        assert_eq!(
            stacked.project_at(T + RECOVERY_DELAY_MS)?.stopped.len(),
            2,
            "built on it, void too"
        );
        refused(
            state.clone().recover(stacked.checkpoints[1].clone()),
            RECOVERY_STOPPED,
        );

        // The old file still rotates at once, or starts another recovery.
        let mut rotated = state.clone();
        rotated.recover_at(
            RootCheckpoint::rotation(&state, &root, &id(&identity()))?,
            T,
        )?;
        assert_eq!(rotated.project_at(T)?.epoch, 3);
        let (_, _, again) = recovering(&state, &root, &node, T)?;
        assert!(state.recover_at(again, T)?);
        assert!(state.project_at(T)?.pending.is_some());
        Ok(())
    }

    #[test]
    fn only_a_manager_of_the_epoch_it_ends_vetoes_and_only_in_time() -> Result<()> {
        let (root, a, node, mut state) = setup()?;
        let (ordinary, removed, other) = (identity(), identity(), identity());
        for member in [
            Member::client(&ordinary, "ordinary", false)?,
            Member::client(&removed, "removed", true)?,
            Member::client(&other, "other", true)?,
        ] {
            state.append(GovernanceEvent::grant(&a, &state.project()?, member)?)?;
        }
        state.append(GovernanceEvent::revoke(
            &a,
            &state.project()?,
            cut(&removed, 0),
        )?)?;
        let (_, _, checkpoint) = recovering(&state, &root, &node, T)?;
        state.recover_at(checkpoint.clone(), T)?;
        let view = state.project_at(T)?;
        let signed_by = |device: &Identity| -> Result<GovernanceEvent> {
            let mut event = veto(&a, &view, &checkpoint, T)?;
            event.issuer = id(device);
            event.sign(device)
        };
        refused(
            state.clone().append(signed_by(&ordinary)?),
            "management permission required",
        );
        refused(
            state.clone().append(signed_by(&removed)?),
            "management permission required",
        );

        let mut late = state.clone();
        late.append(veto(&a, &view, &checkpoint, T + RECOVERY_DELAY_MS)?)?;
        assert!(
            late.project_at(T)?.pending.is_some(),
            "a veto signed too late counts for nothing"
        );
        assert_eq!(late.project_at(T + RECOVERY_DELAY_MS)?.epoch, 2);

        // Removing the device that vetoed, afterwards, brings nothing back.
        state.append(veto(&a, &view, &checkpoint, T + RECOVERY_DELAY_MS - 1)?)?;
        state.append(GovernanceEvent::revoke(
            &other,
            &state.project_at(T)?,
            cut(&a, 0),
        )?)?;
        let view = state.project_at(T + RECOVERY_DELAY_MS)?;
        assert_eq!(view.epoch, 1);
        assert!(view.pending.is_none() && view.client(&id(&a)).is_err());
        Ok(())
    }

    #[test]
    fn replicas_reach_the_same_outcome_in_any_order_and_replays_change_nothing() -> Result<()> {
        let (root, a, node, base) = setup()?;
        let (_, _, checkpoint) = recovering(&base, &root, &node, T)?;
        let mut recovered = base.clone();
        recovered.recover_at(checkpoint.clone(), T)?;
        let event = veto(&a, &recovered.project_at(T)?, &checkpoint, T + 1)?;
        let mut vetoed = recovered.clone();
        assert!(vetoed.append(event.clone())?);
        assert!(!vetoed.append(event)? && !vetoed.recover_at(checkpoint, T)?);
        let parts = [&base, &recovered, &vetoed];
        for order in [[0, 1, 2], [2, 1, 0], [1, 2, 0], [2, 0, 1]] {
            let merged = order[1..]
                .iter()
                .try_fold(parts[order[0]].clone(), |state, index| {
                    state.merge(parts[*index])
                })?;
            assert_eq!(merged, vetoed);
        }
        // Without the veto every replica flips at the same signed time; a veto that arrives
        // later still wins, because it was signed in time.
        let due = T + RECOVERY_DELAY_MS;
        assert_eq!(recovered.merge(&base)?.project_at(due)?.epoch, 2);
        assert_eq!(recovered.merge(&vetoed)?.project_at(due)?.epoch, 1);
        Ok(())
    }
}
