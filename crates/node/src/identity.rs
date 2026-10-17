//! Independent device identities and signed, recoverable personal-network membership.

use crate::{
    crypto::{self, LocalIdentity},
    model::{NetworkId, NodeCertificate, NodeRole, validate_role_dependencies},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub mod local;
pub mod recovery;
pub use local::IdentityStore;

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
    pub subject_id: String,
    pub public_key: String,
    pub kind: IdentityKind,
    pub name: String,
    pub node_roles: Vec<NodeRole>,
    pub manage: bool,
}

impl Member {
    pub fn node(identity: &LocalIdentity, name: &str, roles: Vec<NodeRole>) -> Result<Self> {
        Self::create(identity, name, IdentityKind::Node, roles, false)
    }

    pub fn client(identity: &LocalIdentity, name: &str, manage: bool) -> Result<Self> {
        Self::create(identity, name, IdentityKind::Client, vec![], manage)
    }

    fn create(
        identity: &LocalIdentity,
        name: &str,
        kind: IdentityKind,
        mut node_roles: Vec<NodeRole>,
        manage: bool,
    ) -> Result<Self> {
        node_roles.sort_by_key(role_key);
        let member = Self {
            subject_id: identity.node_id()?,
            public_key: identity.public_key_hex()?,
            kind,
            name: name.to_owned(),
            node_roles,
            manage,
        };
        member.validate()?;
        Ok(member)
    }

    pub fn validate(&self) -> Result<()> {
        validate_id(&self.public_key)?;
        ensure!(
            self.subject_id == crypto::fingerprint(&self.public_key)?,
            "member fingerprint mismatch"
        );
        ensure!(
            !self.name.is_empty()
                && self.name.len() <= 128
                && !self.name.chars().any(char::is_control),
            "invalid device name"
        );
        ensure!(
            self.node_roles.len() <= 5
                && self
                    .node_roles
                    .windows(2)
                    .all(|roles| role_key(&roles[0]) < role_key(&roles[1])),
            "duplicate or noncanonical member roles"
        );
        match self.kind {
            IdentityKind::Client => {
                ensure!(self.node_roles.is_empty(), "clients cannot hold Node roles");
            }
            IdentityKind::Node => {
                ensure!(!self.manage, "Node identities cannot manage membership");
                ensure!(!self.node_roles.is_empty(), "Node member needs a role");
                validate_role_dependencies(&self.node_roles)?;
            }
        }
        Ok(())
    }

    pub fn certificate(&self) -> Result<NodeCertificate> {
        ensure!(self.kind == IdentityKind::Node, "member is not a Node");
        Ok(NodeCertificate {
            node_id: self.subject_id.clone(),
            public_key: self.public_key.clone(),
            roles: self.node_roles.clone(),
            revoked: false,
        })
    }
}

fn role_key(role: &NodeRole) -> &'static str {
    match role {
        NodeRole::Gateway => "gateway",
        NodeRole::Relay => "relay",
        NodeRole::Replica => "replica",
        NodeRole::Scheduler => "scheduler",
        NodeRole::Executor => "executor",
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Genesis {
    pub version: u32,
    pub network_id: NetworkId,
    pub owner_public_key: String,
    pub initial_members: Vec<Member>,
    pub signature: String,
}

impl Genesis {
    pub fn create(root: &LocalIdentity, mut initial_members: Vec<Member>) -> Result<Self> {
        initial_members.sort_by(|a, b| a.subject_id.cmp(&b.subject_id));
        let mut genesis = Self {
            version: IDENTITY_VERSION,
            network_id: root.network_id()?,
            owner_public_key: root.public_key_hex()?,
            initial_members,
            signature: String::new(),
        };
        genesis.validate_shape()?;
        genesis.signature = root.sign_payload("identity-genesis", &genesis.signing_bytes()?)?;
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
        validate_id(&self.owner_public_key)?;
        ensure!(
            self.initial_members
                .iter()
                .all(|member| member.public_key != self.owner_public_key),
            "recovery root cannot be a daily device or client identity"
        );
        ensure!(
            self.network_id == crypto::fingerprint(&self.owner_public_key)?,
            "personal-network root fingerprint mismatch"
        );
        validate_members(&self.initial_members)
    }

    pub fn verify(&self) -> Result<()> {
        self.validate_shape()?;
        crypto::verify_payload(
            &self.owner_public_key,
            "identity-genesis",
            &self.signing_bytes()?,
            &self.signature,
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RootCheckpoint {
    pub version: u32,
    pub network_id: NetworkId,
    pub epoch: u64,
    pub members: Vec<Member>,
    pub signature: String,
}

impl RootCheckpoint {
    pub fn create(root: &LocalIdentity, epoch: u64, mut members: Vec<Member>) -> Result<Self> {
        ensure!(
            (2..=MAX_SAFE_INTEGER).contains(&epoch),
            "invalid recovery epoch"
        );
        members.sort_by(|a, b| a.subject_id.cmp(&b.subject_id));
        validate_members(&members)?;
        let root_public_key = root.public_key_hex()?;
        ensure!(
            members
                .iter()
                .all(|member| member.public_key != root_public_key),
            "recovery root cannot be a daily device or client identity"
        );
        let mut checkpoint = Self {
            version: IDENTITY_VERSION,
            network_id: root.network_id()?,
            epoch,
            members,
            signature: String::new(),
        };
        checkpoint.signature =
            root.sign_payload("identity-recovery", &checkpoint.signing_bytes()?)?;
        Ok(checkpoint)
    }

    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut unsigned = self.clone();
        unsigned.signature.clear();
        canonical_json(&unsigned)
    }

    pub fn verify(&self, genesis: &Genesis) -> Result<()> {
        ensure!(
            self.version == IDENTITY_VERSION
                && self.network_id == genesis.network_id
                && (2..=MAX_SAFE_INTEGER).contains(&self.epoch),
            "invalid recovery checkpoint"
        );
        validate_members(&self.members)?;
        ensure!(
            self.members
                .iter()
                .all(|member| member.public_key != genesis.owner_public_key),
            "recovery root cannot be a daily device or client identity"
        );
        crypto::verify_payload(
            &genesis.owner_public_key,
            "identity-recovery",
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
            .all(|pair| pair[0].subject_id < pair[1].subject_id),
        "duplicate or noncanonical members"
    );
    for member in members {
        member.validate()?;
    }
    ensure!(
        members
            .iter()
            .any(|member| member.manage && member.kind == IdentityKind::Client),
        "network requires an explicitly authorized management client"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GovernanceAction {
    Grant { member: Member },
    Revoke { subject_id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GovernanceEvent {
    pub version: u32,
    pub network_id: NetworkId,
    pub epoch: u64,
    pub parents: Vec<String>,
    pub issuer: String,
    pub action: GovernanceAction,
    pub signature: String,
}

impl GovernanceEvent {
    pub fn grant(admin: &LocalIdentity, view: &AuthorizationView, member: Member) -> Result<Self> {
        member.validate()?;
        ensure!(
            !member.manage,
            "management clients require a root checkpoint"
        );
        ensure!(
            !view
                .members
                .iter()
                .any(|existing| existing.subject_id == member.subject_id && existing.manage),
            "management-client changes require a root checkpoint"
        );
        Self::create(admin, view, GovernanceAction::Grant { member })
    }

    pub fn revoke(
        admin: &LocalIdentity,
        view: &AuthorizationView,
        subject_id: &str,
    ) -> Result<Self> {
        validate_id(subject_id)?;
        ensure!(
            !view
                .members
                .iter()
                .any(|member| member.subject_id == subject_id && member.manage),
            "management-client changes require a root checkpoint"
        );
        Self::create(
            admin,
            view,
            GovernanceAction::Revoke {
                subject_id: subject_id.into(),
            },
        )
    }

    fn create(
        admin: &LocalIdentity,
        view: &AuthorizationView,
        action: GovernanceAction,
    ) -> Result<Self> {
        let issuer = admin.node_id()?;
        ensure!(
            view.authorize_client(&issuer)?.manage,
            "management permission required"
        );
        let mut event = Self {
            version: IDENTITY_VERSION,
            network_id: view.network_id.clone(),
            epoch: view.epoch,
            parents: view.heads.clone(),
            issuer,
            action,
            signature: String::new(),
        };
        event.signature = admin.sign_payload("identity-governance", &event.signing_bytes()?)?;
        Ok(event)
    }

    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut unsigned = self.clone();
        unsigned.signature.clear();
        canonical_json(&unsigned)
    }

    pub fn hash(&self) -> Result<String> {
        Ok(crypto::digest(&canonical_json(self)?))
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
pub struct AuthorizationView {
    pub network_id: NetworkId,
    pub epoch: u64,
    pub heads: Vec<String>,
    pub members: Vec<Member>,
    pub revoked: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientPrincipal {
    pub network_id: NetworkId,
    pub subject_id: String,
    pub epoch: u64,
    pub can_manage: bool,
}

impl AuthorizationView {
    pub fn authorize_member(&self, subject: &str) -> Result<Member> {
        ensure!(
            !self.revoked.iter().any(|id| id == subject),
            "identity has been revoked"
        );
        self.members
            .iter()
            .find(|member| member.subject_id == subject)
            .cloned()
            .context("identity is not authorized in this personal network")
    }

    pub fn authorize_client(&self, subject: &str) -> Result<Member> {
        let member = self.authorize_member(subject)?;
        ensure!(
            member.kind == IdentityKind::Client,
            "client identity required"
        );
        Ok(member)
    }

    pub fn authorize_node(&self, subject: &str, role: Option<NodeRole>) -> Result<Member> {
        let member = self.authorize_member(subject)?;
        ensure!(member.kind == IdentityKind::Node, "Node identity required");
        if let Some(role) = role {
            ensure!(
                member.node_roles.contains(&role),
                "Node lacks required role"
            );
        }
        Ok(member)
    }

    pub fn principal(&self, subject: &str) -> Result<ClientPrincipal> {
        let member = self.authorize_client(subject)?;
        Ok(ClientPrincipal {
            network_id: self.network_id.clone(),
            subject_id: subject.into(),
            epoch: self.epoch,
            can_manage: member.manage,
        })
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

    pub fn recover(&mut self, checkpoint: RootCheckpoint) -> Result<()> {
        checkpoint.verify(&self.genesis)?;
        if self.checkpoint.as_ref() == Some(&checkpoint) {
            return Ok(());
        }
        ensure!(
            checkpoint.epoch > self.epoch(),
            "recovery epoch rollback or conflict"
        );
        self.checkpoint = Some(checkpoint);
        self.events.clear();
        Ok(())
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

    pub fn project(&self) -> Result<AuthorizationView> {
        self.genesis.verify()?;
        if let Some(checkpoint) = &self.checkpoint {
            checkpoint.verify(&self.genesis)?;
        }
        ensure!(
            self.events.len() <= MAX_GOVERNANCE_EVENTS,
            "too many management events"
        );
        let base = self
            .checkpoint
            .as_ref()
            .map_or(&self.genesis.initial_members, |c| &c.members);
        let epoch = self.epoch();
        let mut pending = BTreeMap::new();
        let mut identity_kinds: BTreeMap<String, IdentityKind> = base
            .iter()
            .map(|member| (member.subject_id.clone(), member.kind))
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
                validate_id(parent)?;
            }
            validate_id(&event.issuer)?;
            match &event.action {
                GovernanceAction::Grant { member } => {
                    member.validate()?;
                    ensure!(
                        !member.manage,
                        "management clients require a root checkpoint"
                    );
                    ensure!(
                        !base
                            .iter()
                            .any(|existing| existing.subject_id == member.subject_id
                                && existing.manage),
                        "management-client changes require a root checkpoint"
                    );
                    if let Some(kind) =
                        identity_kinds.insert(member.subject_id.clone(), member.kind)
                    {
                        ensure!(
                            kind == member.kind,
                            "identity cannot change between Node and client"
                        );
                    }
                }
                GovernanceAction::Revoke { subject_id } => {
                    validate_id(subject_id)?;
                    ensure!(
                        !base
                            .iter()
                            .any(|member| member.subject_id == *subject_id && member.manage),
                        "management-client changes require a root checkpoint"
                    );
                }
            }
            let hash = event.hash()?;
            ensure!(
                pending.insert(hash, event.clone()).is_none(),
                "duplicate management event"
            );
        }
        let mut validated: BTreeMap<String, ValidEvent> = BTreeMap::new();
        let mut order = vec![];
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
                let issuer = base
                    .iter()
                    .find(|member| member.subject_id == event.issuer)
                    .context("management issuer is unauthorized")?;
                ensure!(
                    issuer.kind == IdentityKind::Client && issuer.manage,
                    "management permission required"
                );
                crypto::verify_payload(
                    &issuer.public_key,
                    "identity-governance",
                    &event.signing_bytes()?,
                    &event.signature,
                )?;
                validated.insert(
                    hash.clone(),
                    ValidEvent {
                        event,
                        ancestors,
                        depth,
                    },
                );
                order.push(hash);
            }
        }
        let all = order.iter().cloned().collect();
        let (members, revoked) = project_members(base, &validated, &all)?;
        ensure!(members.len() <= MAX_MEMBERS, "too many current members");
        let mut heads: BTreeSet<String> = order.into_iter().collect();
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
            revoked: revoked.into_iter().collect(),
        })
    }
}

fn project_members(
    base: &[Member],
    events: &BTreeMap<String, ValidEvent>,
    basis: &BTreeSet<String>,
) -> Result<(BTreeMap<String, Member>, BTreeSet<String>)> {
    let mut members: BTreeMap<String, Member> = base
        .iter()
        .map(|member| (member.subject_id.clone(), member.clone()))
        .collect();
    let mut revoked = BTreeSet::new();
    let mut candidates: BTreeMap<String, Vec<&String>> = BTreeMap::new();
    for hash in basis {
        let entry = &events[hash];
        match &entry.event.action {
            GovernanceAction::Revoke { subject_id } => {
                revoked.insert(subject_id.clone());
            }
            GovernanceAction::Grant { member } => {
                candidates
                    .entry(member.subject_id.clone())
                    .or_default()
                    .push(hash);
            }
        }
    }
    for (subject, grants) in candidates {
        let latest = grants
            .iter()
            .filter(|hash| {
                !grants
                    .iter()
                    .any(|later| events[*later].ancestors.contains(hash.as_str()))
            })
            .collect::<Vec<_>>();
        let mut chosen: Option<Member> = None;
        for hash in latest {
            let GovernanceAction::Grant { member } = &events[*hash].event.action else {
                unreachable!()
            };
            if let Some(current) = &mut chosen {
                ensure!(
                    current.public_key == member.public_key && current.kind == member.kind,
                    "member identity changed"
                );
                current.manage &= member.manage;
                current
                    .node_roles
                    .retain(|role| member.node_roles.contains(role));
                if member.name < current.name {
                    current.name.clone_from(&member.name);
                }
            } else {
                chosen = Some(member.clone());
            }
        }
        if let Some(member) = chosen {
            if member.kind == IdentityKind::Client || !member.node_roles.is_empty() {
                members.insert(subject, member);
            } else {
                members.remove(&subject);
            }
        }
    }
    for subject in &revoked {
        members.remove(subject);
    }
    Ok((members, revoked))
}

pub fn validate_id(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid identity or event hash"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Result<(LocalIdentity, LocalIdentity, LocalIdentity, NetworkState)> {
        let root = LocalIdentity::generate();
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let state = NetworkState::new(Genesis::create(
            &root,
            vec![
                Member::client(&a, "a", true)?,
                Member::client(&b, "b", true)?,
            ],
        )?)?;
        Ok((root, a, b, state))
    }

    #[test]
    fn independent_clients_cannot_forge_or_self_promote() -> Result<()> {
        let (_, a, _, mut state) = setup()?;
        let ordinary = LocalIdentity::generate();
        state.append(GovernanceEvent::grant(
            &a,
            &state.project()?,
            Member::client(&ordinary, "ordinary", false)?,
        )?)?;
        assert!(!state.project()?.principal(&ordinary.node_id()?)?.can_manage);
        assert!(
            GovernanceEvent::grant(
                &ordinary,
                &state.project()?,
                Member::client(&ordinary, "ordinary", true)?
            )
            .is_err()
        );
        assert!(
            GovernanceEvent::revoke(&ordinary, &state.project()?, &ordinary.node_id()?).is_err()
        );
        let mut forged = GovernanceEvent::revoke(&a, &state.project()?, &ordinary.node_id()?)?;
        forged.signature =
            ordinary.sign_payload("identity-governance", &forged.signing_bytes()?)?;
        assert!(state.append(forged).is_err());
        Ok(())
    }

    #[test]
    fn concurrent_revocation_wins_grants_and_later_regrants() -> Result<()> {
        let (_, a, b, base) = setup()?;
        let c = LocalIdentity::generate();
        let d = LocalIdentity::generate();
        let mut fork = base.clone();
        fork.append(GovernanceEvent::grant(
            &b,
            &fork.project()?,
            Member::client(&c, "c", false)?,
        )?)?;
        fork.append(GovernanceEvent::grant(
            &b,
            &fork.project()?,
            Member::client(&c, "renamed", false)?,
        )?)?;
        fork.append(GovernanceEvent::grant(
            &b,
            &fork.project()?,
            Member::client(&d, "d", false)?,
        )?)?;
        let mut revoked = base.clone();
        revoked.append(GovernanceEvent::revoke(
            &a,
            &revoked.project()?,
            &c.node_id()?,
        )?)?;
        let mut merged = revoked.merge(&fork)?;
        assert!(merged.project()?.authorize_client(&c.node_id()?).is_err());
        assert!(merged.project()?.authorize_client(&d.node_id()?).is_ok());
        assert_eq!(merged.project()?, fork.merge(&revoked)?.project()?);
        merged.append(GovernanceEvent::grant(
            &a,
            &merged.project()?,
            Member::client(&c, "rejoin", false)?,
        )?)?;
        assert!(merged.project()?.authorize_client(&c.node_id()?).is_err());
        let fresh = LocalIdentity::generate();
        merged.append(GovernanceEvent::grant(
            &a,
            &merged.project()?,
            Member::client(&fresh, "fresh", false)?,
        )?)?;
        assert!(
            merged
                .project()?
                .authorize_client(&fresh.node_id()?)
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn concurrent_permission_conflicts_only_intersect() -> Result<()> {
        let (_, a, b, base) = setup()?;
        let c = LocalIdentity::generate();
        let mut left = base.clone();
        let mut right = base;
        left.append(GovernanceEvent::grant(
            &a,
            &left.project()?,
            Member::node(&c, "c", vec![NodeRole::Replica, NodeRole::Scheduler])?,
        )?)?;
        right.append(GovernanceEvent::grant(
            &b,
            &right.project()?,
            Member::node(&c, "c", vec![NodeRole::Replica, NodeRole::Executor])?,
        )?)?;
        assert_eq!(
            left.merge(&right)?
                .project()?
                .authorize_node(&c.node_id()?, None)?
                .node_roles,
            vec![NodeRole::Replica]
        );
        Ok(())
    }

    #[test]
    fn root_fixed_admin_set_cannot_be_changed_by_governance() -> Result<()> {
        let (_, a, b, mut state) = setup()?;
        let newcomer = LocalIdentity::generate();
        assert!(
            GovernanceEvent::grant(
                &a,
                &state.project()?,
                Member::client(&newcomer, "newadmin", true)?
            )
            .is_err()
        );
        assert!(
            GovernanceEvent::grant(
                &a,
                &state.project()?,
                Member::client(&b, "downgrade", false)?
            )
            .is_err()
        );
        assert!(GovernanceEvent::revoke(&a, &state.project()?, &b.node_id()?).is_err());
        let mut forged = GovernanceEvent {
            version: 1,
            network_id: state.genesis.network_id.clone(),
            epoch: 1,
            parents: vec![],
            issuer: a.node_id()?,
            action: GovernanceAction::Grant {
                member: Member::client(&newcomer, "newadmin", true)?,
            },
            signature: String::new(),
        };
        forged.signature = a.sign_payload("identity-governance", &forged.signing_bytes()?)?;
        assert!(state.append(forged).is_err());
        let mut remove = GovernanceEvent {
            version: 1,
            network_id: state.genesis.network_id.clone(),
            epoch: 1,
            parents: vec![],
            issuer: a.node_id()?,
            action: GovernanceAction::Revoke {
                subject_id: b.node_id()?,
            },
            signature: String::new(),
        };
        remove.signature = a.sign_payload("identity-governance", &remove.signing_bytes()?)?;
        assert!(state.append(remove).is_err());
        Ok(())
    }

    #[test]
    fn recovery_changes_admins_and_epoch_but_preserves_network() -> Result<()> {
        let (root, a, _, mut state) = setup()?;
        let original = state.clone();
        let recovered = LocalIdentity::generate();
        let checkpoint = RootCheckpoint::create(
            &root,
            2,
            vec![Member::client(&recovered, "recovered", true)?],
        )?;
        state.recover(checkpoint.clone())?;
        assert_eq!(state.project()?.network_id, original.genesis.network_id);
        assert!(
            state
                .project()?
                .principal(&recovered.node_id()?)?
                .can_manage
        );
        assert!(state.project()?.authorize_client(&a.node_id()?).is_err());
        assert!(state.merge(&original).is_err());
        state.recover(checkpoint)?;
        let mut conflict = original.clone();
        conflict.recover(RootCheckpoint::create(
            &root,
            2,
            vec![Member::client(&a, "conflicting", true)?],
        )?)?;
        assert!(state.merge(&conflict).is_err());
        let attacker = LocalIdentity::generate();
        assert!(
            state
                .recover(RootCheckpoint::create(
                    &attacker,
                    3,
                    vec![Member::client(&recovered, "r", true)?]
                )?)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn missing_parents_wrong_epoch_and_tampering_fail_closed() -> Result<()> {
        let (_, a, _, mut state) = setup()?;
        let ordinary = LocalIdentity::generate();
        let valid = GovernanceEvent::revoke(&a, &state.project()?, &ordinary.node_id()?)?;
        let mut missing = valid.clone();
        missing.parents = vec!["0".repeat(64)];
        assert!(state.append(missing).is_err());
        let mut epoch = valid.clone();
        epoch.epoch += 1;
        assert!(state.append(epoch).is_err());
        let mut changed = state.genesis.clone();
        changed.initial_members[0].manage = false;
        assert!(changed.verify().is_err());
        state.append(valid.clone())?;
        assert!(!state.append(valid)?);
        Ok(())
    }

    #[test]
    fn node_identity_cannot_become_a_management_client() -> Result<()> {
        let (_, a, _, mut state) = setup()?;
        let node = LocalIdentity::generate();
        state.append(GovernanceEvent::grant(
            &a,
            &state.project()?,
            Member::node(&node, "node", vec![NodeRole::Replica])?,
        )?)?;
        assert!(
            state
                .append(GovernanceEvent::grant(
                    &a,
                    &state.project()?,
                    Member::client(&node, "client", false)?
                )?)
                .is_err()
        );
        assert!(Member::node(&node, "node", vec![NodeRole::Scheduler]).is_err());
        Ok(())
    }

    #[test]
    fn recovery_root_is_not_a_daily_client_identity() -> Result<()> {
        let root = LocalIdentity::generate();
        let root_member = Member::client(&root, "unsafe client", true)?;
        assert!(Genesis::create(&root, vec![root_member.clone()]).is_err());
        assert!(RootCheckpoint::create(&root, 2, vec![root_member]).is_err());
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
}
