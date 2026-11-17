//! Signed records, their shape constraints, and canonical signing bytes.

use super::{
    AuthorizationView, GENESIS_DOMAIN, GOVERNANCE_DOMAIN, IDENTITY_VERSION, MAX_MEMBERS,
    MAX_SAFE_INTEGER, MAX_SCOPE_PARENTS, NetworkState, RECOVERY_DELAY_MS, RECOVERY_DOMAIN,
};
use crate::{Identity, validate_device_id};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

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

pub(super) fn verify_payload(
    device_id: &str,
    domain: &str,
    payload: &[u8],
    signature: &str,
) -> Result<()> {
    Ok(crate::verify(device_id, domain, payload, signature)?)
}

pub(super) fn device_id(value: &str) -> Result<()> {
    Ok(validate_device_id(value)?)
}

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

    pub(super) fn is_manager(&self) -> bool {
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

pub(super) fn ensure_not_root<'a>(
    root: &str,
    mut members: impl Iterator<Item = &'a Member>,
) -> Result<()> {
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
    /// The management heads of the epoch that signs it, as the root saw them; empty for a
    /// rotation. Only a veto resting on all of them stops the new directory, so whether a veto
    /// counts depends on nothing that arrives after it.
    pub heads: Vec<String>,
    /// When the root signed it; a new directory normally takes effect `RECOVERY_DELAY_MS` later.
    /// A rotation retires a sibling recovery only if signed before that one's natural deadline.
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
        let heads = state.heads_of(&root.device_id()?)?;
        Self::create(
            state.network_id(),
            epoch,
            root,
            next,
            directory,
            heads,
            issued_at_ms,
        )
    }

    /// The next checkpoint of `state`: hands recovery to `next` and keeps the directory. Signed
    /// before a waiting recovery's time, it retires that recovery.
    pub fn rotation(
        state: &NetworkState,
        root: &impl MembershipSigner,
        next: &str,
        issued_at_ms: u64,
    ) -> Result<Self> {
        let epoch = state.epoch() + 1;
        Self::create(
            state.network_id(),
            epoch,
            root,
            next,
            None,
            vec![],
            issued_at_ms,
        )
    }

    pub(super) fn create(
        network_id: &str,
        epoch: u64,
        root: &impl MembershipSigner,
        next: &str,
        directory: Option<Directory>,
        heads: Vec<String>,
        issued_at_ms: u64,
    ) -> Result<Self> {
        let mut checkpoint = Self {
            version: IDENTITY_VERSION,
            network_id: network_id.to_owned(),
            epoch,
            recovery_key: next.to_owned(),
            directory,
            heads,
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
        ensure!(
            self.heads.len() <= MAX_SCOPE_PARENTS
                && self.heads.windows(2).all(|pair| pair[0] < pair[1])
                && (self.directory.is_some() || self.heads.is_empty()),
            "invalid recovery checkpoint heads"
        );
        for head in &self.heads {
            validate_hash(head)?;
        }
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

    /// The new directory's natural activation time, unless approved earlier or vetoed.
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

    pub(super) fn bound(&self) -> (u64, Option<&str>) {
        (self.cutoff_seq, self.cutoff_hash.as_deref())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GovernanceAction {
    Grant {
        member: Member,
    },
    Revoke(Revocation),
    /// Stops a recovery in this epoch if signed before its natural activation deadline.
    Veto {
        checkpoint: String,
        at_ms: u64,
    },
    /// Activates a pending recovery early if signed before its natural activation deadline.
    Approve {
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
        view.authorize(issuer, &action)?;
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
