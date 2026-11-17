//! Authenticate signed histories and establish their causal epochs before projection.

use super::records::{device_id, ensure_not_root, verify_payload};
use super::{
    GOVERNANCE_DOMAIN, GovernanceAction, GovernanceEvent, IDENTITY_VERSION, IdentityKind,
    MAX_CHECKPOINT_RECORDS, MAX_GOVERNANCE_EVENTS, MAX_MANAGEMENT_DEPTH, MAX_PARENTS,
    MAX_SCOPE_PARENTS, Member, NetworkState, Revocation, validate_hash,
};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

/// Concurrent revocations of one device keep the widest bound, the same on every replica.
/// A client may revoke itself, and nothing else, without management permission.
pub(super) fn leaves(issuer: &str, action: &GovernanceAction, client: bool) -> bool {
    client
        && matches!(action, GovernanceAction::Revoke(revocation) if revocation.device_id == issuer)
}

pub(super) struct ValidEvent<'a> {
    pub(super) event: &'a GovernanceEvent,
    pub(super) ancestors: BTreeSet<String>,
    /// Only edges within this event's epoch count toward the existing depth limit.
    depth: usize,
}

pub(super) struct HistoryEpoch {
    pub(super) key: String,
    pub(super) parent: Option<usize>,
    pub(super) checkpoint: Option<String>,
    /// The explicit root directory that starts this authorization scope.
    pub(super) scope: usize,
    pub(super) lineage: BTreeSet<u64>,
    pub(super) base: Vec<Member>,
    pub(super) retired: Vec<Revocation>,
}

pub(super) struct CheckedHistory<'a> {
    pub(super) epochs: Vec<HistoryEpoch>,
    pub(super) events: BTreeMap<String, ValidEvent<'a>>,
}

impl CheckedHistory<'_> {
    pub(super) fn basis(&self, epoch: usize) -> BTreeSet<String> {
        self.events
            .iter()
            .filter(|(_, entry)| self.epochs[epoch].lineage.contains(&entry.event.epoch))
            .map(|(hash, _)| hash.clone())
            .collect()
    }

    /// Unknown hashes keep recovery incomplete; known hashes must belong to this root branch.
    pub(super) fn references(&self, epoch: usize, hashes: &[String]) -> Result<bool> {
        let mut counts = BTreeMap::<u64, usize>::new();
        let mut complete = true;
        for hash in hashes {
            let Some(entry) = self.events.get(hash) else {
                complete = false;
                continue;
            };
            ensure!(
                self.epochs[epoch].lineage.contains(&entry.event.epoch),
                "management dependency belongs to another root scope or branch"
            );
            let count = counts.entry(entry.event.epoch).or_default();
            *count += 1;
            ensure!(
                *count <= MAX_PARENTS,
                "too many management references in one epoch"
            );
        }
        Ok(complete)
    }
}

impl NetworkState {
    pub(super) fn checked_history(&self) -> Result<CheckedHistory<'_>> {
        self.genesis.verify()?;
        ensure!(
            self.checkpoints.len() <= MAX_CHECKPOINT_RECORDS,
            "recovery checkpoint record limit reached; existing history is unchanged"
        );
        ensure!(
            self.checkpoints
                .windows(2)
                .all(|pair| pair[0].epoch < pair[1].epoch),
            "invalid recovery history"
        );
        let root = &self.genesis.network_id;
        let mut epochs = vec![HistoryEpoch {
            key: root.clone(),
            parent: None,
            checkpoint: None,
            scope: 0,
            lineage: BTreeSet::from([1]),
            base: self.genesis.members(),
            retired: vec![],
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
            let (scope, mut lineage, base, retired) = match &checkpoint.directory {
                Some(directory) => (
                    index + 1,
                    BTreeSet::new(),
                    directory.members.clone(),
                    directory.retired.clone(),
                ),
                None => (
                    epochs[parent].scope,
                    epochs[parent].lineage.clone(),
                    vec![],
                    vec![],
                ),
            };
            lineage.insert(checkpoint.epoch);
            epochs.push(HistoryEpoch {
                key: checkpoint.recovery_key.clone(),
                parent: Some(parent),
                checkpoint: Some(checkpoint.hash()?),
                scope,
                lineage,
                base,
                retired,
            });
        }
        let mut keys = Vec::with_capacity(self.events.len());
        for event in &self.events {
            keys.push((event.epoch, event.hash()?));
        }
        ensure!(
            keys.windows(2).all(|pair| pair[0] < pair[1]),
            "duplicate or noncanonical management events"
        );
        ensure!(
            self.events
                .iter()
                .all(|event| (1..=epochs.len() as u64).contains(&event.epoch)),
            "management event epoch has no recovery checkpoint"
        );
        let mut validated = BTreeMap::<String, ValidEvent<'_>>::new();
        for (index, epoch) in epochs.iter().enumerate() {
            let base = &epochs[epoch.scope].base;
            let mut kinds: BTreeMap<String, IdentityKind> = base
                .iter()
                .map(|member| (member.device_id.clone(), member.kind))
                .collect();
            for entry in validated
                .values()
                .filter(|entry| epoch.lineage.contains(&entry.event.epoch))
            {
                if let GovernanceAction::Grant { member } = &entry.event.action {
                    kinds.insert(member.device_id.clone(), member.kind);
                }
            }
            let events = self.epoch_events(index as u64 + 1);
            ensure!(
                events.len() <= MAX_GOVERNANCE_EVENTS,
                "too many management events"
            );
            let mut pending = BTreeMap::new();
            for event in events {
                ensure!(
                    event.version == IDENTITY_VERSION
                        && event.network_id == self.genesis.network_id,
                    "management event network or epoch mismatch"
                );
                ensure!(
                    event.parents.len() <= MAX_SCOPE_PARENTS
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
                        ensure_not_root(&epoch.key, std::iter::once(member))?;
                        if let Some(kind) = kinds.insert(member.device_id.clone(), member.kind) {
                            ensure!(
                                kind == member.kind,
                                "device cannot change between Node and client"
                            );
                        }
                    }
                    GovernanceAction::Revoke(revocation) => revocation.validate()?,
                    GovernanceAction::Veto { checkpoint, .. }
                    | GovernanceAction::Approve { checkpoint, .. } => {
                        validate_hash(checkpoint)?;
                    }
                }
                pending.insert(event.hash()?, event);
            }
            while !pending.is_empty() {
                let ready: Vec<_> = pending
                    .iter()
                    .filter(|(_, event)| {
                        event
                            .parents
                            .iter()
                            .all(|parent| validated.contains_key(parent))
                    })
                    .map(|(hash, _)| hash.clone())
                    .collect();
                ensure!(
                    !ready.is_empty(),
                    "missing or cyclic management dependencies"
                );
                for hash in ready {
                    let event = pending.remove(&hash).expect("ready event exists");
                    let mut ancestors = BTreeSet::new();
                    let mut depth = 1;
                    let mut counts = BTreeMap::<u64, usize>::new();
                    for parent in &event.parents {
                        let previous = &validated[parent];
                        ensure!(
                            epoch.lineage.contains(&previous.event.epoch),
                            "management dependency belongs to another root scope or branch"
                        );
                        let count = counts.entry(previous.event.epoch).or_default();
                        *count += 1;
                        ensure!(
                            *count <= MAX_PARENTS,
                            "too many management references in one epoch"
                        );
                        ancestors.extend(previous.ancestors.iter().cloned());
                        ancestors.insert(parent.clone());
                        if previous.event.epoch == event.epoch {
                            depth = depth.max(previous.depth + 1);
                        }
                    }
                    ensure!(
                        depth <= MAX_MANAGEMENT_DEPTH,
                        "management history is too deep"
                    );
                    let historical_client = base.iter().any(|member| {
                        member.device_id == event.issuer && member.kind == IdentityKind::Client
                    }) || ancestors.iter().any(|hash| {
                        matches!(&validated[hash].event.action,
                        GovernanceAction::Grant { member } if member.device_id == event.issuer
                            && member.kind == IdentityKind::Client)
                    });
                    let historical_manager = base.iter().any(|member| {
                        member.device_id == event.issuer && member.is_manager()
                    }) || ancestors.iter().any(|hash| matches!(&validated[hash].event.action,
                        GovernanceAction::Grant { member } if member.device_id == event.issuer && member.is_manager()));
                    ensure!(
                        historical_manager
                            || leaves(&event.issuer, &event.action, historical_client),
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
            let mut heads: BTreeSet<_> = validated
                .iter()
                .filter(|(_, entry)| entry.event.epoch == index as u64 + 1)
                .map(|(hash, _)| hash.clone())
                .collect();
            for entry in validated
                .values()
                .filter(|entry| entry.event.epoch == index as u64 + 1)
            {
                for parent in &entry.event.parents {
                    heads.remove(parent);
                }
            }
            ensure!(
                heads.len() <= MAX_PARENTS,
                "too many concurrent management heads in one epoch"
            );
        }
        Ok(CheckedHistory {
            epochs,
            events: validated,
        })
    }

    fn epoch_events(&self, epoch: u64) -> Vec<&GovernanceEvent> {
        self.events
            .iter()
            .filter(|event| event.epoch == epoch)
            .collect()
    }
}
