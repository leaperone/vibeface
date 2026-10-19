//! Signed, recoverable personal-network membership.
//!
//! A `DeviceId` and a `network_id` are Ed25519 public keys. The recovery root signs
//! only `Genesis` and `RootCheckpoint`; management clients sign `GovernanceEvent`s.

use crate::{Identity, validate_device_id};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

pub const GENESIS_DOMAIN: &str = "identity-genesis";
pub const RECOVERY_DOMAIN: &str = "identity-recovery";
pub const GOVERNANCE_DOMAIN: &str = "identity-governance";

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
        self.sign(domain, payload).map_err(anyhow::Error::msg)
    }
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn verify_payload(device_id: &str, domain: &str, payload: &[u8], signature: &str) -> Result<()> {
    crate::verify(device_id, domain, payload, signature).map_err(anyhow::Error::msg)
}

fn device_id(value: &str) -> Result<()> {
    validate_device_id(value).map_err(anyhow::Error::msg)
}

pub const IDENTITY_VERSION: u32 = 1;
pub const MAX_MEMBERS: usize = 1024;
pub const MAX_GOVERNANCE_EVENTS: usize = 1024;
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RootCheckpoint {
    pub version: u32,
    pub network_id: String,
    pub epoch: u64,
    pub members: Vec<Member>,
    pub signature: String,
}

impl RootCheckpoint {
    pub fn create(
        root: &impl MembershipSigner,
        epoch: u64,
        mut members: Vec<Member>,
    ) -> Result<Self> {
        members.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        let mut checkpoint = Self {
            version: IDENTITY_VERSION,
            network_id: root.device_id()?,
            epoch,
            members,
            signature: String::new(),
        };
        checkpoint.validate_shape()?;
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
        device_id(&self.network_id)?;
        validate_members(&self.members)?;
        ensure_not_root(&self.network_id, self.members.iter())
    }

    pub fn verify(&self, genesis: &Genesis) -> Result<()> {
        ensure!(
            self.network_id == genesis.network_id,
            "recovery checkpoint belongs to another personal network"
        );
        self.validate_shape()?;
        verify_payload(
            &genesis.network_id,
            RECOVERY_DOMAIN,
            &self.signing_bytes()?,
            &self.signature,
        )
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GovernanceAction {
    Grant { member: Member },
    Revoke { device_id: String, cutoff_seq: u64 },
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
            GovernanceAction::Revoke {
                device_id: id,
                cutoff_seq,
            } => {
                device_id(id)?;
                ensure!(*cutoff_seq <= MAX_SAFE_INTEGER, "invalid cutoff sequence");
            }
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
        device_id: &str,
        cutoff_seq: u64,
    ) -> Result<Self> {
        let action = GovernanceAction::Revoke {
            device_id: device_id.into(),
            cutoff_seq,
        };
        Self::unsigned(view, &admin.device_id()?, action)?.sign(admin)
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NetworkState {
    pub genesis: Genesis,
    pub checkpoint: Option<RootCheckpoint>,
    pub events: Vec<GovernanceEvent>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Revocation {
    pub device_id: String,
    /// The largest log sequence of this device that replicas still accept.
    pub cutoff_seq: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationView {
    pub network_id: String,
    pub epoch: u64,
    pub heads: Vec<String>,
    pub members: Vec<Member>,
    pub revoked: Vec<Revocation>,
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
        self.revoked
            .iter()
            .find(|revoked| revoked.device_id == device)
            .map(|revoked| revoked.cutoff_seq)
    }
}

struct ValidEvent {
    event: GovernanceEvent,
    ancestors: BTreeSet<String>,
    depth: usize,
}

impl NetworkState {
    pub fn new(genesis: Genesis) -> Result<Self> {
        genesis.verify()?;
        Ok(Self {
            genesis,
            checkpoint: None,
            events: vec![],
        })
    }

    pub fn network_id(&self) -> &str {
        &self.genesis.network_id
    }

    pub fn epoch(&self) -> u64 {
        self.checkpoint
            .as_ref()
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
        let mut next = self.clone();
        next.events.push(event);
        next.project()?;
        *self = next;
        Ok(true)
    }

    /// Returns false when this exact checkpoint is already current.
    pub fn recover(&mut self, checkpoint: RootCheckpoint) -> Result<bool> {
        checkpoint.verify(&self.genesis)?;
        if self.checkpoint.as_ref() == Some(&checkpoint) {
            return Ok(false);
        }
        ensure!(
            checkpoint.epoch > self.epoch(),
            "recovery epoch rollback or conflict"
        );
        self.checkpoint = Some(checkpoint);
        self.events.clear();
        Ok(true)
    }

    pub fn merge(&self, incoming: &Self) -> Result<Self> {
        self.project()?;
        incoming.project()?;
        ensure!(
            self.genesis == incoming.genesis,
            "different personal-network genesis"
        );
        ensure!(
            incoming.epoch() >= self.epoch(),
            "recovery epoch rollback rejected"
        );
        if incoming.epoch() > self.epoch() {
            return Ok(incoming.clone());
        }
        ensure!(
            self.checkpoint == incoming.checkpoint,
            "conflicting recovery checkpoint"
        );
        let mut events: BTreeMap<String, GovernanceEvent> = BTreeMap::new();
        for event in self.events.iter().chain(&incoming.events) {
            events.insert(event.hash()?, event.clone());
        }
        let merged = Self {
            genesis: self.genesis.clone(),
            checkpoint: self.checkpoint.clone(),
            events: events.into_values().collect(),
        };
        merged.project()?;
        Ok(merged)
    }

    fn base(&self) -> Vec<Member> {
        self.checkpoint
            .as_ref()
            .map_or_else(|| self.genesis.members(), |c| c.members.clone())
    }

    pub fn project(&self) -> Result<AuthorizationView> {
        self.genesis.verify()?;
        if let Some(checkpoint) = &self.checkpoint {
            checkpoint.verify(&self.genesis)?;
        }
        ensure!(
            self.events.len() <= MAX_GOVERNANCE_EVENTS,
            "too many management events"
        );
        let base = self.base();
        let epoch = self.epoch();
        let mut pending = BTreeMap::new();
        let mut kinds: BTreeMap<&str, IdentityKind> = base
            .iter()
            .map(|member| (member.device_id.as_str(), member.kind))
            .collect();
        for event in &self.events {
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
                    ensure_not_root(&self.genesis.network_id, std::iter::once(member))?;
                    if let Some(kind) = kinds.insert(&member.device_id, member.kind) {
                        ensure!(
                            kind == member.kind,
                            "device cannot change between Node and client"
                        );
                    }
                }
                GovernanceAction::Revoke {
                    device_id: id,
                    cutoff_seq,
                } => {
                    device_id(id)?;
                    ensure!(*cutoff_seq <= MAX_SAFE_INTEGER, "invalid cutoff sequence");
                }
            }
            ensure!(
                pending.insert(event.hash()?, event.clone()).is_none(),
                "duplicate management event"
            );
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
                let (members, revoked) = project_members(&base, &validated, &ancestors);
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
        let (members, revoked) = project_members(&base, &validated, &all);
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
        Ok(AuthorizationView {
            network_id: self.genesis.network_id.clone(),
            epoch,
            heads: heads.into_iter().collect(),
            members: members.into_values().collect(),
            revoked: revoked
                .into_iter()
                .map(|(device_id, cutoff_seq)| Revocation {
                    device_id,
                    cutoff_seq,
                })
                .collect(),
        })
    }
}

/// Revocation wins over any grant; concurrent grants keep the narrower permission.
fn project_members(
    base: &[Member],
    events: &BTreeMap<String, ValidEvent>,
    basis: &BTreeSet<String>,
) -> (BTreeMap<String, Member>, BTreeMap<String, u64>) {
    let mut members: BTreeMap<String, Member> = base
        .iter()
        .map(|member| (member.device_id.clone(), member.clone()))
        .collect();
    let mut revoked: BTreeMap<String, u64> = BTreeMap::new();
    let mut candidates: BTreeMap<String, Vec<&String>> = BTreeMap::new();
    for hash in basis {
        match &events[hash].event.action {
            GovernanceAction::Revoke {
                device_id,
                cutoff_seq,
            } => {
                let cutoff = revoked.entry(device_id.clone()).or_default();
                *cutoff = (*cutoff).max(*cutoff_seq);
            }
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
    result.map_err(|error| format!("{error:#}"))
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
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = verifyMembership))]
pub fn verify_membership(
    state_json: &str,
    expected_network: &str,
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
        let view = state.project()?;
        Ok(VerifiedMembership { state, view })
    })())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl VerifiedMembership {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = viewJson))]
    pub fn view_json(&self) -> crate::Result<String> {
        serde_json::to_string(&self.view).map_err(|error| error.to_string())
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = stateJson))]
    pub fn state_json(&self) -> crate::Result<String> {
        serde_json::to_string(&self.state).map_err(|error| error.to_string())
    }

    /// An unsigned `GovernanceEvent` for `{"type":"grant",...}` or `{"type":"revoke",...}`.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = governanceTemplate))]
    pub fn governance_template(&self, issuer: &str, action_json: &str) -> crate::Result<String> {
        js((|| {
            let event =
                GovernanceEvent::unsigned(&self.view, issuer, serde_json::from_str(action_json)?)?;
            Ok(serde_json::to_string(&event)?)
        })())
    }

    /// Signs a checkpoint with an epoch above both this state and the backup's.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = createRecoveryCheckpoint))]
    pub fn create_recovery_checkpoint(
        &self,
        root: &Identity,
        members_json: &str,
        backup_epoch: f64,
    ) -> crate::Result<String> {
        js((|| {
            ensure!(
                root.device_id() == self.state.genesis.network_id,
                "recovery root does not own this personal network"
            );
            ensure!(
                backup_epoch.is_finite()
                    && backup_epoch.fract() == 0.0
                    && (1.0..MAX_SAFE_INTEGER as f64).contains(&backup_epoch),
                "invalid backup recovery epoch"
            );
            let epoch = self.view.epoch.max(backup_epoch as u64) + 1;
            let checkpoint =
                RootCheckpoint::create(root, epoch, serde_json::from_str(members_json)?)?;
            checkpoint.verify(&self.state.genesis)?;
            Ok(serde_json::to_string(&checkpoint)?)
        })())
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
        // b signs on the genesis heads, where b is not yet a member.
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
            &id(&node),
            7,
        )?)?;
        let mut merged = left.merge(&right)?;
        assert_eq!(merged.project()?, right.merge(&left)?.project()?);
        assert!(merged.project()?.node(&id(&node)).is_err());
        assert_eq!(merged.project()?.cutoff(&id(&node)), Some(7));
        merged.append(GovernanceEvent::revoke(
            &a,
            &merged.project()?,
            &id(&node),
            3,
        )?)?;
        assert_eq!(merged.project()?.cutoff(&id(&node)), Some(7));
        merged.append(GovernanceEvent::grant(
            &a,
            &merged.project()?,
            Member::node(&node, "rejoin")?,
        )?)?;
        assert!(merged.project()?.node(&id(&node)).is_err());
        // A manager may revoke another manager and itself; later events by them fail.
        merged.append(GovernanceEvent::revoke(&b, &merged.project()?, &id(&a), 0)?)?;
        let view = merged.project()?;
        assert!(GovernanceEvent::revoke(&a, &view, &id(&b), 0).is_err());
        merged.append(GovernanceEvent::revoke(&b, &view, &id(&b), 0)?)?;
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
        assert!(RootCheckpoint::create(&root, 2, vec![Member::client(&root, "r", true)?]).is_err());
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
        let checkpoint = RootCheckpoint::create(&root, 2, members.clone())?;
        assert!(state.recover(checkpoint.clone())?);
        assert!(!state.recover(checkpoint)?);
        assert!(state.project()?.manager(&id(&recovered)).is_ok());
        assert!(state.project()?.client(&id(&a)).is_err());
        assert!(state.merge(&original).is_err());
        assert_eq!(original.merge(&state)?, state);
        let mut conflict = original.clone();
        conflict.recover(RootCheckpoint::create(
            &root,
            2,
            vec![Member::client(&a, "conflicting", true)?],
        )?)?;
        assert!(state.merge(&conflict).is_err());
        assert!(
            state
                .recover(RootCheckpoint::create(&identity(), 3, members.clone())?)
                .is_err()
        );
        assert!(RootCheckpoint::create(&root, 3, vec![Member::node(&node, "n")?]).is_err());
        Ok(())
    }

    #[test]
    fn missing_parents_wrong_epoch_and_tampering_fail_closed() -> Result<()> {
        let (_, a, node, mut state) = setup()?;
        let valid = GovernanceEvent::revoke(&a, &state.project()?, &id(&node), 1)?;
        let mut missing = valid.clone();
        missing.parents = vec!["0".repeat(64)];
        assert!(state.append(missing.sign(&a)?).is_err());
        let mut epoch = valid.clone();
        epoch.epoch += 1;
        assert!(state.append(epoch.sign(&a)?).is_err());
        let mut tampered = valid.clone();
        tampered.action = GovernanceAction::Revoke {
            device_id: id(&a),
            cutoff_seq: 1,
        };
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
        let verified = verify_membership(&state_json, &id(&root)).map_err(anyhow::Error::msg)?;
        assert!(verify_membership(&state_json, &id(&a)).is_err());
        let mut forged = state.clone();
        forged.genesis.signature = "00".repeat(64);
        assert!(verify_membership(&serde_json::to_string(&forged)?, &id(&root)).is_err());

        let action = serde_json::json!({"type":"revoke","device_id":id(&node),"cutoff_seq":4});
        let template = verified
            .governance_template(&id(&a), &action.to_string())
            .map_err(anyhow::Error::msg)?;
        assert!(
            verified
                .governance_template(&id(&node), &action.to_string())
                .is_err()
        );
        let mut event: GovernanceEvent = serde_json::from_str(&template)?;
        let payload = governance_payload(&template).map_err(anyhow::Error::msg)?;
        let message =
            crate::signing_message(GOVERNANCE_DOMAIN, &payload).map_err(anyhow::Error::msg)?;
        event.signature = a.sign_message(&message);
        let mut next = state.clone();
        next.append(event)?;
        assert_eq!(next.project()?.cutoff(&id(&node)), Some(4));

        let members = serde_json::to_string(&vec![Member::client(&a, "a", true)?])?;
        let signed = verified
            .create_recovery_checkpoint(&root, &members, 4.0)
            .map_err(anyhow::Error::msg)?;
        let checkpoint: RootCheckpoint = serde_json::from_str(&signed)?;
        checkpoint.verify(&state.genesis)?;
        assert_eq!(checkpoint.epoch, 5);
        assert!(
            verified
                .create_recovery_checkpoint(&a, &members, 4.0)
                .is_err()
        );
        assert!(
            verified
                .create_recovery_checkpoint(&root, &members, 4.5)
                .is_err()
        );
        let genesis = create_genesis(
            &root,
            &serde_json::to_string(&Member::client(&a, "a", true)?)?,
            &serde_json::to_string(&Member::node(&node, "n")?)?,
        )
        .map_err(anyhow::Error::msg)?;
        serde_json::from_str::<Genesis>(&genesis)?.verify()?;
        Ok(())
    }
}
