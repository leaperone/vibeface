//! Resolve authorization effects, frozen recovery votes, and epoch standing at a time.

use super::checked_history::{CheckedHistory, ValidEvent, leaves};
use super::records::ensure_not_root;
use super::{
    GovernanceAction, IdentityKind, LAST_MANAGER, MAX_CHECKPOINTS, MAX_MEMBERS, Member,
    NO_RECOVERY_WAITING, NetworkState, RECOVERY_INCOMPLETE, Revocation, RootCheckpoint,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

fn widen(revoked: &mut BTreeMap<String, Revocation>, revocation: &Revocation) {
    match revoked.get_mut(&revocation.device_id) {
        Some(known) if known.bound() >= revocation.bound() => {}
        Some(known) => *known = revocation.clone(),
        None => {
            revoked.insert(revocation.device_id.clone(), revocation.clone());
        }
    }
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
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
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
    /// Hashes of the checkpoints a veto or a rotation stopped, and of any signed on top of them.
    pub stopped: Vec<String>,
    #[serde(skip)]
    signed_removals: Vec<String>,
    #[serde(skip)]
    historical_nodes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingRecovery {
    pub checkpoint: String,
    pub effective_at_ms: u64,
    pub members: Vec<Member>,
}

impl AuthorizationView {
    /// Whether `issuer` may take `action` on this directory. A client checks this before
    /// signing, and a Node checks it again on the directory in effect before accepting: the
    /// history keeps a signed event whose issuer lacks authority and gives it no effect, and a
    /// revocation cuts the revoked device's concurrent actions, so the projection after one can
    /// still show a manager this directory had already seen demoted.
    pub fn authorize(&self, issuer: &str, action: &GovernanceAction) -> Result<()> {
        ensure!(
            self.member(issuer)?.is_manager()
                || leaves(issuer, action, self.client(issuer).is_ok()),
            "management permission required"
        );
        match action {
            GovernanceAction::Grant { member } => {
                member.validate()?;
                ensure!(
                    member.manage || self.keeps_manager_without(&member.device_id),
                    LAST_MANAGER
                );
            }
            GovernanceAction::Revoke(revocation) => {
                revocation.validate()?;
                ensure!(
                    self.keeps_manager_without(&revocation.device_id),
                    LAST_MANAGER
                );
            }
            GovernanceAction::Veto { checkpoint, .. } => ensure!(
                self.pending.as_ref().map(|p| &p.checkpoint) == Some(checkpoint),
                NO_RECOVERY_WAITING
            ),
            GovernanceAction::Approve { checkpoint, at_ms } => {
                let pending = self
                    .pending
                    .as_ref()
                    .filter(|pending| pending.checkpoint == *checkpoint)
                    .context(NO_RECOVERY_WAITING)?;
                ensure!(
                    *at_ms < pending.effective_at_ms,
                    "this recovery is already due"
                );
            }
        }
        Ok(())
    }

    /// Whether a manager other than `device` remains.
    pub fn keeps_manager_without(&self, device: &str) -> bool {
        self.members
            .iter()
            .any(|member| member.device_id != device && member.is_manager())
    }

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
            Some(revocation) if self.historical_node(device) => {
                Some(WriteBound::Cutoff(revocation))
            }
            Some(_) => None,
            None => self.node(device).ok().map(|_| WriteBound::Unlimited),
        }
    }

    /// Only definite daily Revoke effects in live scopes contribute this source list.
    pub fn signed_removals(&self) -> &[String] {
        &self.signed_removals
    }

    fn historical_node(&self, device: &str) -> bool {
        self.historical_nodes
            .binary_search_by(|known| known.as_str().cmp(device))
            .is_ok()
    }

    /// A removed Client or an unproven Node never enters the old-Node sync channel.
    pub fn removed_node(&self, device: &str) -> bool {
        self.revocation(device).is_some() && self.historical_node(device)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Effect {
    No,
    Unknown,
    Yes,
}

impl Effect {
    fn from_bool(value: bool) -> Self {
        if value { Self::Yes } else { Self::No }
    }

    pub(super) fn not(self) -> Self {
        match self {
            Self::No => Self::Yes,
            Self::Unknown => Self::Unknown,
            Self::Yes => Self::No,
        }
    }

    pub(super) fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::No, _) | (_, Self::No) => Self::No,
            (Self::Yes, Self::Yes) => Self::Yes,
            _ => Self::Unknown,
        }
    }

    pub(super) fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::Yes, _) | (_, Self::Yes) => Self::Yes,
            (Self::No, Self::No) => Self::No,
            _ => Self::Unknown,
        }
    }
}

fn any_effect(values: impl Iterator<Item = Effect>) -> Effect {
    values.fold(Effect::No, Effect::or)
}

struct GrantGate {
    effect: usize,
    superseded_by: Vec<usize>,
    manager: bool,
}

struct PermissionGate {
    base_present: bool,
    base_manager: bool,
    retired: bool,
    grants: Vec<GrantGate>,
    revokes: Vec<usize>,
}

impl PermissionGate {
    fn evaluate(&self, effects: &[Effect]) -> (Effect, Effect) {
        let base_selected = any_effect(self.grants.iter().map(|grant| effects[grant.effect])).not();
        let mut present = Effect::from_bool(self.base_present).and(base_selected);
        let mut manager = Effect::from_bool(self.base_manager).and(base_selected);
        let mut demoted = Effect::No;
        for grant in &self.grants {
            let selected = effects[grant.effect]
                .and(any_effect(grant.superseded_by.iter().map(|index| effects[*index])).not());
            present = present.or(selected);
            if grant.manager {
                manager = manager.or(selected);
            } else {
                demoted = demoted.or(selected);
            }
        }
        let not_revoked = any_effect(self.revokes.iter().map(|index| effects[*index]))
            .or(Effect::from_bool(self.retired))
            .not();
        (
            present.and(not_revoked),
            manager.and(demoted.not()).and(not_revoked),
        )
    }
}

struct Sources<'a> {
    base: Option<&'a Member>,
    retired: bool,
    grants: Vec<usize>,
    revokes: Vec<usize>,
}

struct Barrier {
    effect: usize,
    /// A signed causal re-promotion can end a demotion, never an explicit Revoke.
    promotions: Vec<usize>,
}

struct EffectGate {
    permission: PermissionGate,
    self_leave: bool,
    barriers: Vec<Barrier>,
}

/// One scope's effects, from its explicit root constants and one immutable cut.
pub(super) struct EpochView {
    pub(super) heads: Vec<String>,
    members: BTreeMap<String, Member>,
    pub(super) revoked: BTreeMap<String, Revocation>,
    clients: BTreeSet<String>,
    pub(super) nodes: BTreeSet<String>,
    signed_removals: BTreeSet<String>,
    pub(super) removal: RemovalEvidence,
}

#[derive(Clone, Default)]
pub(super) struct RemovalEvidence {
    pub(super) support: BTreeSet<String>,
    pub(super) before: BTreeSet<String>,
}

impl CheckedHistory<'_> {
    fn permission(
        &self,
        source: &Sources<'_>,
        variables: &[(&String, &ValidEvent<'_>)],
        basis: &BTreeSet<String>,
    ) -> PermissionGate {
        let grants: Vec<usize> = source
            .grants
            .iter()
            .copied()
            .filter(|index| basis.contains(variables[*index].0))
            .collect();
        PermissionGate {
            base_present: source.base.is_some(),
            base_manager: source.base.is_some_and(Member::is_manager),
            retired: source.retired,
            revokes: source
                .revokes
                .iter()
                .copied()
                .filter(|index| basis.contains(variables[*index].0))
                .collect(),
            grants: grants
                .iter()
                .map(|index| {
                    let GovernanceAction::Grant { member } = &variables[*index].1.event.action
                    else {
                        unreachable!("grant source is a Grant")
                    };
                    GrantGate {
                        effect: *index,
                        superseded_by: grants
                            .iter()
                            .copied()
                            .filter(|later| {
                                variables[*later].1.ancestors.contains(variables[*index].0)
                            })
                            .collect(),
                        manager: member.is_manager(),
                    }
                })
                .collect(),
        }
    }

    fn project(&self, epoch: usize, basis: &BTreeSet<String>) -> Result<EpochView> {
        self.project_with_removal(epoch, basis, None)
    }

    pub(super) fn project_with_removal(
        &self,
        epoch: usize,
        basis: &BTreeSet<String>,
        removed: Option<&str>,
    ) -> Result<EpochView> {
        let root = &self.epochs[self.epochs[epoch].scope];
        let variables: Vec<_> = basis
            .iter()
            .filter_map(|hash| {
                let entry = &self.events[hash];
                matches!(
                    entry.event.action,
                    GovernanceAction::Grant { .. } | GovernanceAction::Revoke(_)
                )
                .then_some((hash, entry))
            })
            .collect();
        let mut sources = BTreeMap::<String, Sources<'_>>::new();
        for member in &root.base {
            sources.insert(
                member.device_id.clone(),
                Sources {
                    base: Some(member),
                    retired: false,
                    grants: vec![],
                    revokes: vec![],
                },
            );
        }
        for retired in &root.retired {
            sources
                .entry(retired.device_id.clone())
                .or_insert(Sources {
                    base: None,
                    retired: false,
                    grants: vec![],
                    revokes: vec![],
                })
                .retired = true;
        }
        for (index, (_, entry)) in variables.iter().enumerate() {
            let (device, grant) = match &entry.event.action {
                GovernanceAction::Grant { member } => (&member.device_id, true),
                GovernanceAction::Revoke(revocation) => (&revocation.device_id, false),
                _ => unreachable!("only Grant and Revoke have recursive effects"),
            };
            let source = sources.entry(device.clone()).or_insert(Sources {
                base: None,
                retired: false,
                grants: vec![],
                revokes: vec![],
            });
            if grant {
                source.grants.push(index);
            } else {
                source.revokes.push(index);
            }
        }
        let mut gates = Vec::with_capacity(variables.len());
        for (hash, entry) in &variables {
            let source = &sources[&entry.event.issuer];
            let self_leave = matches!(&entry.event.action,
                GovernanceAction::Revoke(revocation) if revocation.device_id == entry.event.issuer);
            let mut barriers = Vec::new();
            for index in &source.revokes {
                let (barrier_hash, barrier) = variables[*index];
                if barrier_hash != *hash && !barrier.ancestors.contains(*hash) {
                    barriers.push(Barrier {
                        effect: *index,
                        promotions: vec![],
                    });
                }
            }
            if !self_leave {
                for index in &source.grants {
                    let (barrier_hash, barrier) = variables[*index];
                    let GovernanceAction::Grant { member } = &barrier.event.action else {
                        unreachable!("grant source is a Grant")
                    };
                    if member.kind != IdentityKind::Client
                        || member.manage
                        || barrier_hash == *hash
                        || barrier.ancestors.contains(*hash)
                    {
                        continue;
                    }
                    let promotions = source
                        .grants
                        .iter()
                        .copied()
                        .filter(|promotion| {
                            let (promotion_hash, promotion) = variables[*promotion];
                            entry.ancestors.contains(promotion_hash)
                                && promotion.ancestors.contains(barrier_hash)
                                && matches!(&promotion.event.action,
                                GovernanceAction::Grant { member } if member.is_manager())
                        })
                        .collect();
                    barriers.push(Barrier {
                        effect: *index,
                        promotions,
                    });
                }
            }
            gates.push(EffectGate {
                permission: self.permission(source, &variables, &entry.ancestors),
                self_leave,
                barriers,
            });
        }
        let mut effects = vec![Effect::Unknown; variables.len()];
        let mut stable = false;
        // Strong Kleene gates are information-monotone; each variable refines at most once.
        for _ in 0..=gates.len() {
            let next: Vec<_> = gates
                .iter()
                .map(|gate| {
                    let (client, manager) = gate.permission.evaluate(&effects);
                    let authority = if gate.self_leave { client } else { manager };
                    let barrier = any_effect(gate.barriers.iter().map(|barrier| {
                        effects[barrier.effect].and(
                            any_effect(barrier.promotions.iter().map(|index| effects[*index]))
                                .not(),
                        )
                    }));
                    authority.and(barrier.not())
                })
                .collect();
            debug_assert!(
                effects
                    .iter()
                    .zip(&next)
                    .all(|(old, new)| *old == Effect::Unknown || old == new)
            );
            if effects == next {
                stable = true;
                break;
            }
            effects = next;
        }
        ensure!(stable, "governance effect projection did not converge");
        let mut removal = RemovalEvidence::default();
        if let Some(device) = removed {
            let retired_at_root = sources.get(device).is_some_and(|source| source.retired);
            let revocation = variables.iter().enumerate()
                .filter(|(index, (_, entry))| !retired_at_root && effects[*index] == Effect::Yes
                    && matches!(&entry.event.action, GovernanceAction::Revoke(revocation) if revocation.device_id == device))
                .min_by_key(|(_, (hash, entry))| (entry.ancestors.len(), *hash))
                .map(|(index, _)| index);
            let node = variables.iter().enumerate()
                .filter(|(index, (_, entry))| effects[*index] == Effect::Yes
                    && matches!(&entry.event.action, GovernanceAction::Grant { member } if member.device_id == device && member.kind == IdentityKind::Node))
                .min_by_key(|(_, (hash, entry))| (entry.ancestors.len(), *hash))
                .map(|(index, _)| index);
            let indices: BTreeMap<_, _> = variables
                .iter()
                .enumerate()
                .map(|(index, (hash, _))| (hash.as_str(), index))
                .collect();
            let mut pending: Vec<_> = revocation.into_iter().chain(node).collect();
            if let Some(index) = revocation {
                removal.before.insert(variables[index].0.clone());
                removal
                    .before
                    .extend(variables[index].1.ancestors.iter().cloned());
            }
            while let Some(index) = pending.pop() {
                let (hash, entry) = variables[index];
                if !removal.support.insert(hash.clone()) {
                    continue;
                }
                for ancestor in &entry.ancestors {
                    if let Some(index) = indices.get(ancestor.as_str()) {
                        pending.push(*index);
                    } else {
                        removal.support.insert(ancestor.clone());
                    }
                }
                // Authority and barriers also depend on concurrent events outside `parents`.
                let gate = &gates[index];
                pending.extend(&gate.permission.revokes);
                for grant in &gate.permission.grants {
                    pending.push(grant.effect);
                    pending.extend(&grant.superseded_by);
                }
                for barrier in &gate.barriers {
                    pending.push(barrier.effect);
                    pending.extend(&barrier.promotions);
                }
            }
        }
        let mut members = BTreeMap::new();
        let mut revoked: BTreeMap<_, _> = root
            .retired
            .iter()
            .map(|revocation| (revocation.device_id.clone(), revocation.clone()))
            .collect();
        let mut clients: BTreeSet<_> = root
            .base
            .iter()
            .filter(|member| member.kind == IdentityKind::Client)
            .map(|member| member.device_id.clone())
            .collect();
        let mut nodes: BTreeSet<_> = root
            .base
            .iter()
            .filter(|member| member.kind == IdentityKind::Node)
            .map(|member| member.device_id.clone())
            .collect();
        let mut signed_removals = BTreeSet::new();
        for (index, (_, entry)) in variables.iter().enumerate() {
            if effects[index] != Effect::Yes {
                continue;
            }
            match &entry.event.action {
                GovernanceAction::Grant { member } => {
                    if member.kind == IdentityKind::Client {
                        clients.insert(member.device_id.clone());
                    } else {
                        nodes.insert(member.device_id.clone());
                    }
                }
                GovernanceAction::Revoke(revocation) => {
                    widen(&mut revoked, revocation);
                    signed_removals.insert(revocation.device_id.clone());
                }
                _ => unreachable!("only Grant and Revoke have recursive effects"),
            }
        }
        for (device, source) in &sources {
            let (present, manager) = self
                .permission(source, &variables, basis)
                .evaluate(&effects);
            if present != Effect::Yes {
                continue;
            }
            let grants: Vec<_> = source
                .grants
                .iter()
                .copied()
                .filter(|index| effects[*index] == Effect::Yes)
                .collect();
            let mut chosen = None::<Member>;
            for index in grants.iter().filter(|index| {
                !grants.iter().any(|later| {
                    self.events[variables[*later].0]
                        .ancestors
                        .contains(variables[**index].0)
                })
            }) {
                let GovernanceAction::Grant { member } = &variables[*index].1.event.action else {
                    unreachable!("grant source is a Grant")
                };
                if let Some(current) = &mut chosen {
                    if member.name < current.name {
                        current.name.clone_from(&member.name);
                    }
                } else {
                    chosen = Some(member.clone());
                }
            }
            if let Some(mut member) = chosen.or_else(|| source.base.cloned()) {
                member.manage = member.kind == IdentityKind::Client && manager == Effect::Yes;
                members.insert(device.clone(), member);
            }
        }
        let mut heads = basis.clone();
        for hash in basis {
            for parent in &self.events[hash].event.parents {
                heads.remove(parent);
            }
        }
        let heads: Vec<_> = heads.into_iter().collect();
        self.references(epoch, &heads)?;
        ensure!(members.len() <= MAX_MEMBERS, "too many current members");
        Ok(EpochView {
            heads,
            members,
            revoked,
            clients,
            nodes,
            signed_removals,
            removal,
        })
    }

    fn electorate(
        &self,
        parent: usize,
        checkpoint: &RootCheckpoint,
    ) -> Result<Option<BTreeSet<String>>> {
        if !self.references(parent, &checkpoint.heads)? {
            return Ok(None);
        }
        let mut basis = BTreeSet::new();
        for head in &checkpoint.heads {
            basis.insert(head.clone());
            basis.extend(self.events[head].ancestors.iter().cloned());
        }
        Ok(Some(
            self.project(parent, &basis)?
                .members
                .into_values()
                .filter(Member::is_manager)
                .map(|member| member.device_id)
                .collect(),
        ))
    }

    pub(super) fn votes(
        &self,
        parent: usize,
        checkpoint: &RootCheckpoint,
        hash: &str,
    ) -> Result<(bool, Option<u64>, bool, BTreeSet<String>)> {
        let Some(electorate) = self.electorate(parent, checkpoint)? else {
            return Ok((false, None, true, BTreeSet::new()));
        };
        let mut veto = None::<String>;
        let mut approval = None::<(u64, String)>;
        for (event_hash, entry) in self.events.iter().filter(|(_, entry)| {
            entry.event.epoch == parent as u64 + 1
                && electorate.contains(&entry.event.issuer)
                && checkpoint
                    .heads
                    .iter()
                    .all(|head| entry.ancestors.contains(head))
        }) {
            match &entry.event.action {
                GovernanceAction::Veto {
                    checkpoint: target,
                    at_ms,
                } if target == hash && *at_ms < checkpoint.effective_at_ms() => {
                    veto.get_or_insert_with(|| event_hash.clone());
                }
                GovernanceAction::Approve {
                    checkpoint: target,
                    at_ms,
                } if target == hash
                    && *at_ms < checkpoint.effective_at_ms()
                    && approval.as_ref().is_none_or(|(known, _)| *at_ms < *known) =>
                {
                    approval = Some((*at_ms, event_hash.clone()));
                }
                _ => {}
            }
        }
        let support = veto
            .clone()
            .or_else(|| approval.as_ref().map(|(_, hash)| hash.clone()));
        Ok((
            veto.is_some(),
            approval.map(|(at, _)| at),
            false,
            support.into_iter().collect(),
        ))
    }
}

/// Where an epoch stands at one time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Standing {
    Live,
    Pending,
    Waiting,
    Vetoed,
    Void,
}

pub(super) struct Epoch {
    pub(super) key: String,
    pub(super) parent: Option<usize>,
    checkpoint: Option<String>,
    pub(super) scope: usize,
    rotation: bool,
    effective_at_ms: u64,
    approved_at_ms: Option<u64>,
    pub(super) stopped: bool,
    pub(super) incomplete: bool,
    pub(super) standing: Standing,
    pub(super) view: EpochView,
}

pub(super) fn live(epochs: &[Epoch]) -> usize {
    epochs
        .iter()
        .rposition(|epoch| epoch.standing == Standing::Live)
        .expect("the genesis epoch is in effect")
}

fn retires(rotation: bool, issued_at_ms: u64, earlier: &Epoch) -> bool {
    rotation && !earlier.rotation && issued_at_ms < earlier.effective_at_ms
}

fn pending(epochs: &[Epoch], live: usize) -> Option<usize> {
    epochs
        .iter()
        .position(|epoch| epoch.parent == Some(live) && epoch.standing == Standing::Pending)
}

impl NetworkState {
    /// Checks the complete signed history and returns the directory in effect at `now_ms`.
    pub fn project_at(&self, now_ms: u64) -> Result<AuthorizationView> {
        let mut epochs = self.epochs(now_ms)?;
        let live = live(&epochs);
        let pending = pending(&epochs, live).map(|index| PendingRecovery {
            checkpoint: epochs[index].checkpoint.clone().unwrap_or_default(),
            effective_at_ms: epochs[index]
                .approved_at_ms
                .unwrap_or(epochs[index].effective_at_ms),
            members: epochs[index].view.members.values().cloned().collect(),
        });
        let stopped = epochs
            .iter()
            .filter(|epoch| matches!(epoch.standing, Standing::Vetoed | Standing::Void))
            .filter_map(|epoch| epoch.checkpoint.clone())
            .collect();
        // A later rotation's effects apply to the whole scope, including historical grant sources.
        let mut scopes = BTreeMap::new();
        for (index, epoch) in epochs.iter().enumerate() {
            if epoch.standing == Standing::Live {
                scopes.insert(epoch.scope, index);
            }
        }
        let attesters: BTreeSet<_> = scopes
            .values()
            .flat_map(|index| epochs[*index].view.clients.iter().cloned())
            .collect();
        let historical_nodes: BTreeSet<_> = scopes
            .values()
            .flat_map(|index| epochs[*index].view.nodes.iter().cloned())
            .collect();
        let signed_removals: BTreeSet<_> = scopes
            .values()
            .flat_map(|index| epochs[*index].view.signed_removals.iter().cloned())
            .filter(|device| epochs[live].view.revoked.contains_key(device))
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
            signed_removals: signed_removals.into_iter().collect(),
            historical_nodes: historical_nodes.into_iter().collect(),
        })
    }

    /// Only standing depends on wall time; signed history and frozen vote cuts do not.
    pub(super) fn epochs(&self, now_ms: u64) -> Result<Vec<Epoch>> {
        let history = self.checked_history()?;
        let mut epochs = Vec::<Epoch>::with_capacity(history.epochs.len());
        for (index, checked) in history.epochs.iter().enumerate() {
            let view = history.project(index, &history.basis(index))?;
            ensure_not_root(&checked.key, view.members.values())?;
            let Some(parent) = checked.parent else {
                epochs.push(Epoch {
                    key: checked.key.clone(),
                    parent: None,
                    checkpoint: None,
                    scope: checked.scope,
                    rotation: false,
                    effective_at_ms: 0,
                    approved_at_ms: None,
                    stopped: false,
                    incomplete: false,
                    standing: Standing::Live,
                    view,
                });
                continue;
            };
            let checkpoint = &self.checkpoints[index - 1];
            let rotation = checkpoint.directory.is_none();
            let (stopped, approved_at_ms, incomplete, _) = if rotation {
                (false, None, false, BTreeSet::new())
            } else {
                history.votes(
                    parent,
                    checkpoint,
                    checked.checkpoint.as_deref().expect("checkpoint hash"),
                )?
            };
            let mut retired = Vec::new();
            for (sibling_index, sibling) in epochs.iter().enumerate() {
                if sibling.parent == Some(parent) && !sibling.stopped {
                    ensure!(
                        retires(rotation, checkpoint.issued_at_ms, sibling),
                        "a recovery key signs again only after its earlier recoveries were vetoed"
                    );
                    retired.push(sibling_index);
                }
            }
            for sibling in retired {
                epochs[sibling].stopped = true;
            }
            epochs.push(Epoch {
                key: checked.key.clone(),
                parent: Some(parent),
                checkpoint: checked.checkpoint.clone(),
                scope: checked.scope,
                rotation,
                effective_at_ms: checkpoint.effective_at_ms(),
                approved_at_ms,
                stopped,
                incomplete,
                standing: Standing::Live,
                view,
            });
        }
        ensure!(
            epochs.iter().filter(|epoch| !epoch.stopped).count() <= MAX_CHECKPOINTS + 1,
            "active recovery checkpoint limit reached; existing history is unchanged"
        );
        for index in 1..epochs.len() {
            let epoch = &epochs[index];
            let parent = epochs[epoch.parent.expect("only genesis has no parent")].standing;
            epochs[index].standing = match parent {
                Standing::Vetoed | Standing::Void => Standing::Void,
                _ if epoch.stopped => Standing::Vetoed,
                _ if epoch.incomplete => Standing::Waiting,
                Standing::Pending | Standing::Waiting => Standing::Waiting,
                Standing::Live
                    if epoch.rotation
                        || epoch.approved_at_ms.is_some_and(|at| at <= now_ms)
                        || now_ms >= epoch.effective_at_ms =>
                {
                    Standing::Live
                }
                Standing::Live => Standing::Pending,
            };
        }
        Ok(epochs)
    }

    pub(super) fn ensure_checkpoint_voter(&self, checkpoint: &str, issuer: &str) -> Result<()> {
        let history = self.checked_history()?;
        let index = history
            .epochs
            .iter()
            .position(|epoch| epoch.checkpoint.as_deref() == Some(checkpoint))
            .context("unknown recovery checkpoint")?;
        let parent = history.epochs[index]
            .parent
            .expect("checkpoint has a parent");
        let voters = history
            .electorate(parent, &self.checkpoints[index - 1])?
            .context(RECOVERY_INCOMPLETE)?;
        ensure!(
            voters.contains(issuer),
            "device is not a manager in this recovery's signed cut"
        );
        Ok(())
    }
}
