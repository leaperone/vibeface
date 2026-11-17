//! The native history API: admission, merge, recovery, and bounded removal evidence.

use super::projection::{Standing, live};
use super::{
    AuthorizationView, Genesis, GovernanceAction, GovernanceEvent, MAX_CHECKPOINTS,
    MAX_GOVERNANCE_EVENTS, RECOVERY_INCOMPLETE, RECOVERY_STOPPED, RECOVERY_WAITING, RootCheckpoint,
    SUPERSEDED, now_ms,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

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

    /// Why `key` cannot sign the next checkpoint at `now_ms`, if it cannot. A `rotation` is not
    /// held up by a waiting recovery: it retires it, so a leaked file can always be replaced.
    pub fn check_root(&self, key: &str, now_ms: u64, rotation: bool) -> Result<()> {
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
            Standing::Live => ensure!(
                rotation
                    || !epochs
                        .iter()
                        .any(|epoch| epoch.parent == Some(live) && !epoch.stopped),
                RECOVERY_WAITING
            ),
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
            Some(signer) => self.check_root(signer, now_ms, checkpoint.directory.is_none())?,
            // Not ours, or malformed: the reason against the current key.
            None => checkpoint.verify(&self.genesis, &self.project_at(now_ms)?.recovery_key)?,
        }
        let mut next = self.clone();
        next.checkpoints.push(checkpoint);
        next.project_at(now_ms)?;
        ensure!(
            !next
                .epochs(now_ms)?
                .last()
                .is_some_and(|epoch| epoch.incomplete),
            RECOVERY_INCOMPLETE
        );
        *self = next;
        Ok(true)
    }

    /// The union of compatible checkpoint chains; conflicting checkpoints fail closed.
    pub fn merge(&self, incoming: &Self) -> Result<Self> {
        ensure!(
            self.genesis == incoming.genesis,
            "different personal-network genesis"
        );
        let mut checkpoints: BTreeMap<u64, &RootCheckpoint> = BTreeMap::new();
        for checkpoint in self.checkpoints.iter().chain(&incoming.checkpoints) {
            if let Some(known) = checkpoints.insert(checkpoint.epoch, checkpoint) {
                ensure!(
                    known == checkpoint,
                    "conflicting recovery checkpoint at epoch {}; reuse the same prepared checkpoint",
                    checkpoint.epoch
                );
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

    /// The management heads of the epoch `key` opened, which a checkpoint it signs names; none
    /// for a key not of this network.
    pub fn heads_of(&self, key: &str) -> Result<Vec<String>> {
        Ok(self
            .epochs(0)?
            .into_iter()
            .find(|epoch| epoch.key == key)
            .map(|epoch| epoch.view.heads)
            .unwrap_or_default())
    }

    /// The directory in effect now.
    pub fn project(&self) -> Result<AuthorizationView> {
        self.project_at(now_ms())
    }

    /// Signed evidence of this Node's current removal, excluding later member grants and
    /// renames it has not proved it already holds. Recovery and effect barriers stay verifiable.
    pub fn removal_state(&self, device: &str, known: Option<&Self>) -> Result<Self> {
        self.removal_state_at(device, known, now_ms())
    }

    pub(super) fn removal_state_at(
        &self,
        device: &str,
        known: Option<&Self>,
        now: u64,
    ) -> Result<Self> {
        // A denial is a bounded recovery path, not a second unbounded directory sync.
        ensure!(
            self.checkpoints.len() <= MAX_CHECKPOINTS && self.events.len() <= MAX_GOVERNANCE_EVENTS,
            "removal evidence exceeds the recovery sync limit"
        );
        ensure!(
            self.project_at(now)?.removed_node(device),
            "Node is not currently removed"
        );
        let mut known_events = BTreeSet::new();
        let mut known_checkpoints = BTreeSet::new();
        if let Some(known) = known {
            ensure!(
                known.genesis == self.genesis,
                "different personal-network genesis"
            );
            ensure!(
                known.checkpoints.len() <= MAX_CHECKPOINTS
                    && known.events.len() <= MAX_GOVERNANCE_EVENTS,
                "known removal history exceeds the recovery sync limit"
            );
            known.project_at(now)?;
            for event in &known.events {
                known_events.insert(event.hash()?);
            }
            for checkpoint in &known.checkpoints {
                known_checkpoints.insert(checkpoint.hash()?);
            }
        }
        let epochs = self.epochs(now)?;
        let current = live(&epochs);
        let history = self.checked_history()?;
        let mut scope_heads = BTreeMap::new();
        for (index, epoch) in epochs.iter().enumerate().take(current + 1) {
            if epoch.standing == Standing::Live {
                scope_heads.insert(epoch.scope, index);
            }
        }
        let mut scope_evidence = BTreeMap::new();
        for (scope, index) in &scope_heads {
            scope_evidence.insert(
                *scope,
                history
                    .project_with_removal(*index, &history.basis(*index), Some(device))?
                    .removal,
            );
        }
        let mut votes = Vec::new();
        for (offset, checkpoint) in self.checkpoints[..current].iter().enumerate() {
            let parent = history.epochs[offset + 1]
                .parent
                .expect("checkpoint has a parent");
            votes.push(if checkpoint.directory.is_some() {
                history.votes(parent, checkpoint, &checkpoint.hash()?)?.3
            } else {
                BTreeSet::new()
            });
        }
        // An earlier removal stops being a proof after a live recovery restored this Node.
        let restored = (1..=current)
            .rev()
            .find(|index| {
                epochs[*index].standing == Standing::Live
                    && self.checkpoints[*index - 1]
                        .directory
                        .as_ref()
                        .is_some_and(|directory| {
                            directory
                                .members
                                .iter()
                                .any(|member| member.device_id == device)
                        })
            })
            .unwrap_or(0);
        let boundary = (restored..=current)
            .find(|index| {
                epochs[*index].standing == Standing::Live
                    && epochs[*index].view.revoked.contains_key(device)
            })
            .context("current removal has no signed evidence")?;
        'prefix: for index in boundary..=current {
            if epochs[index].standing != Standing::Live {
                continue;
            }
            let scope = epochs[index].scope;
            if !epochs[scope_heads[&scope]]
                .view
                .revoked
                .contains_key(device)
            {
                continue;
            }
            for checkpoint in &self.checkpoints[boundary..index] {
                if checkpoint.directory.is_some()
                    && !known_checkpoints.contains(&checkpoint.hash()?)
                {
                    continue 'prefix;
                }
            }
            // Choose effects from the complete live scope, including later rotations. A
            // shorter prefix must not turn a currently ineffective revoke into a proof.
            let mut evidence = scope_evidence[&scope].clone();
            // A retired Node may have joined in an earlier scope. Preserve its signed Node
            // identity and authority support so the verifier can admit the old-Node channel.
            let mut scopes = BTreeSet::new();
            for earlier in (0..=index).rev() {
                if epochs[earlier].standing == Standing::Live
                    && epochs[earlier].view.nodes.contains(device)
                    && scopes.insert(epochs[earlier].scope)
                {
                    let previous = &scope_evidence[&epochs[earlier].scope];
                    evidence.support.extend(previous.support.iter().cloned());
                    evidence.before.extend(previous.before.iter().cloned());
                }
            }
            let mut pending = Vec::new();
            for (offset, checkpoint) in self.checkpoints[..index].iter().enumerate() {
                pending.extend(checkpoint.heads.iter().cloned());
                pending.extend(votes[offset].iter().cloned());
                for head in checkpoint.heads.iter().filter(|_| offset < boundary) {
                    if let Some(entry) = history.events.get(head) {
                        evidence.before.insert(head.clone());
                        evidence.before.extend(entry.ancestors.iter().cloned());
                    }
                }
            }
            pending.extend(evidence.support.iter().cloned());
            while let Some(hash) = pending.pop() {
                let Some(entry) = history.events.get(&hash) else {
                    continue 'prefix;
                };
                if entry.event.epoch > index as u64 + 1 {
                    continue 'prefix;
                }
                if evidence.support.insert(hash) {
                    pending.extend(entry.ancestors.iter().cloned());
                }
            }
            if evidence.support.iter().any(|hash| {
                matches!(
                    &history.events[hash].event.action,
                    GovernanceAction::Grant { .. }
                ) && !evidence.before.contains(hash)
                    && !known_events.contains(hash)
            }) {
                continue;
            }
            let proof = Self {
                genesis: self.genesis.clone(),
                checkpoints: self.checkpoints[..index].to_vec(),
                events: self
                    .events
                    .iter()
                    .filter(|event| {
                        event
                            .hash()
                            .is_ok_and(|hash| evidence.support.contains(&hash))
                    })
                    .cloned()
                    .collect(),
            };
            if !proof
                .project_at(now)
                .is_ok_and(|view| view.removed_node(device))
            {
                continue;
            }
            if let Some(known) = known
                && !known
                    .merge(&proof)
                    .and_then(|merged| merged.project_at(now))
                    .is_ok_and(|view| view.removed_node(device))
            {
                continue;
            }
            return Ok(proof);
        }
        bail!("current removal cannot be proved without disclosing later member changes")
    }
}
