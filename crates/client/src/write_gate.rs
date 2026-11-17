//! Write authorization observations for one outbox's authenticated Node targets.
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// Local authorization only: the original read-only/aligning response stays intact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub(crate) enum WriteGate {
    Unknown,
    Writable,
    Forked,
    Restricted,
}

impl WriteGate {
    /// A confirmed fork is terminal for this authenticated Node target.
    pub(crate) fn merge(self, observed: Self) -> Self {
        if self == Self::Forked || observed == Self::Forked {
            Self::Forked
        } else {
            observed
        }
    }

    pub(crate) fn from_response(value: &Value) -> Result<Self> {
        match value.get("state").and_then(Value::as_str) {
            Some("writable") => Ok(Self::Writable),
            Some("forked") => Ok(Self::Forked),
            Some("read_only" | "aligning") => Ok(Self::Restricted),
            _ => bail!("invalid Node write state"),
        }
    }
}

// These are the sole persisted codes, independent of enum declaration order.
impl From<WriteGate> for u8 {
    fn from(gate: WriteGate) -> Self {
        match gate {
            WriteGate::Unknown => 0,
            WriteGate::Writable => 1,
            WriteGate::Forked => 2,
            WriteGate::Restricted => 3,
        }
    }
}
impl TryFrom<u8> for WriteGate {
    type Error = &'static str;
    fn try_from(code: u8) -> std::result::Result<Self, Self::Error> {
        match code {
            0 => Ok(Self::Unknown),
            1 => Ok(Self::Writable),
            2 => Ok(Self::Forked),
            3 => Ok(Self::Restricted),
            _ => Err("invalid cached write state"),
        }
    }
}

/// An ordinary observation may only be published by the owning generation.
#[derive(Clone, Copy)]
pub(crate) struct WriteGateGeneration(u64);

// The low two bits encode WriteGate; the remaining bits identify the generation.
// Keeping them in one CAS prevents a superseded reader from granting writes.
pub(crate) struct AtomicWriteGate(AtomicU64);
impl AtomicWriteGate {
    pub(crate) fn new(gate: WriteGate) -> Self {
        Self(AtomicU64::new(u64::from(u8::from(gate))))
    }

    fn decode(snapshot: u64) -> WriteGate {
        WriteGate::try_from((snapshot & 3) as u8)
            .expect("only WriteGate writes its atomic representation")
    }

    pub(crate) fn load(&self) -> WriteGate {
        Self::decode(self.0.load(Ordering::Acquire))
    }

    pub(crate) fn is_writable(&self, generation: WriteGateGeneration) -> bool {
        let snapshot = self.0.load(Ordering::Acquire);
        snapshot >> 2 == generation.0 && Self::decode(snapshot) == WriteGate::Writable
    }

    // Keep the existing CAS semantics on MSRV 1.91, where try_update is unstable.
    fn update(&self, mut update: impl FnMut(u64) -> Option<u64>) -> std::result::Result<u64, u64> {
        let mut previous = self.0.load(Ordering::Acquire);
        loop {
            let Some(next) = update(previous) else {
                return Err(previous);
            };
            match self
                .0
                .compare_exchange_weak(previous, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(snapshot) => return Ok(snapshot),
                Err(current) => previous = current,
            }
        }
    }

    /// Publish the replacement's owner and initial observation atomically, before
    /// its cache job runs and before the replaced connection is closed.
    pub(crate) fn begin_generation(&self, observed: WriteGate) -> Result<WriteGateGeneration> {
        let mut previous = self.0.load(Ordering::Acquire);
        loop {
            // Reserve the final epoch as an unissued, fail-closed sentinel.
            // No old token can regain ownership after counter exhaustion.
            let next_generation = (previous >> 2)
                .checked_add(1)
                .filter(|generation| *generation < u64::MAX >> 2);
            let exhausted = next_generation.is_none();
            let generation = next_generation.unwrap_or(u64::MAX >> 2);
            let observed = if exhausted {
                WriteGate::Unknown
            } else {
                observed
            };
            let next =
                (generation << 2) | u64::from(u8::from(Self::decode(previous).merge(observed)));
            match self
                .0
                .compare_exchange_weak(previous, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    ensure!(!exhausted, "write gate generation exhausted");
                    return Ok(WriteGateGeneration(generation));
                }
                Err(current) => previous = current,
            }
        }
    }

    pub(crate) fn observe(
        &self,
        generation: WriteGateGeneration,
        observed: WriteGate,
    ) -> WriteGate {
        let previous = self.update(|snapshot| {
            if snapshot >> 2 != generation.0 && observed != WriteGate::Forked {
                return None;
            }
            Some((snapshot & !3) | u64::from(u8::from(Self::decode(snapshot).merge(observed))))
        });
        match previous {
            Ok(snapshot) => Self::decode(snapshot).merge(observed),
            Err(snapshot) => Self::decode(snapshot),
        }
    }

    pub(crate) fn confirm_fork(&self) {
        // A confirmed fork remains valid even when its observing generation was replaced.
        self.update(|snapshot| {
            Some(
                (snapshot & !3)
                    | u64::from(u8::from(Self::decode(snapshot).merge(WriteGate::Forked))),
            )
        })
        .expect("a fork always has a merged value");
    }

    /// The caller holds its outbox mutex for the entire callback. Never persist a
    /// response captured before that lock, including from a replaced generation.
    pub(crate) fn persist(
        &self,
        mut store: impl FnMut(WriteGate) -> Result<WriteGate>,
    ) -> Result<()> {
        let mut snapshot = self.0.load(Ordering::Acquire);
        loop {
            let persisted = store(Self::decode(snapshot))?;
            if persisted == WriteGate::Forked {
                self.confirm_fork();
            }
            let current = self.0.load(Ordering::Acquire);
            if current == snapshot || Self::decode(current) == persisted {
                return Ok(());
            }
            // A newer generation/refresh published while SQLite was writing.
            // Finish this job with that current value, rather than the stale one.
            snapshot = current;
        }
    }
}

/// Shared by all generations of the same network/client outbox, including A→B→A.
#[derive(Default)]
pub(crate) struct WriteGates(Mutex<BTreeMap<String, Arc<AtomicWriteGate>>>);
impl WriteGates {
    pub(crate) fn for_target(&self, target: &str) -> Result<Arc<AtomicWriteGate>> {
        let mut gates = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("write gates lock poisoned"))?;
        ensure!(
            gates.len() < 64 || gates.contains_key(target),
            "too many Node write gates"
        );
        Ok(gates
            .entry(target.to_owned())
            .or_insert_with(|| Arc::new(AtomicWriteGate::new(WriteGate::Unknown)))
            .clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_codec_has_one_strict_numeric_representation() -> Result<()> {
        for (gate, code) in [
            (WriteGate::Unknown, "0"),
            (WriteGate::Writable, "1"),
            (WriteGate::Forked, "2"),
            (WriteGate::Restricted, "3"),
        ] {
            assert_eq!(serde_json::to_string(&gate)?, code);
            assert_eq!(serde_json::from_str::<WriteGate>(code)?, gate);
        }
        for invalid in [
            "4",
            "255",
            "256",
            "-1",
            "1.0",
            "null",
            "\"1\"",
            "\"forked\"",
            "{}",
        ] {
            assert!(
                serde_json::from_str::<WriteGate>(invalid).is_err(),
                "{invalid}"
            );
        }
        Ok(())
    }

    #[test]
    fn responses_preserve_the_distinct_restricted_facts_and_reject_missing_or_invalid_state()
    -> Result<()> {
        for state in ["read_only", "aligning"] {
            let value = serde_json::json!({"state":state});
            assert_eq!(WriteGate::from_response(&value)?, WriteGate::Restricted);
            assert_eq!(value["state"], state);
        }
        for value in [
            serde_json::json!({}),
            serde_json::json!({"state":"unknown"}),
            serde_json::json!({"state":"Writable"}),
            serde_json::json!({"state":2}),
            Value::Null,
        ] {
            assert!(WriteGate::from_response(&value).is_err());
        }
        let gate = AtomicWriteGate::new(WriteGate::Unknown);
        let generation = gate.begin_generation(WriteGate::Unknown)?;
        assert_eq!(
            gate.observe(generation, WriteGate::Writable),
            WriteGate::Writable
        );
        assert_eq!(
            gate.observe(generation, WriteGate::Restricted),
            WriteGate::Restricted
        );
        Ok(())
    }

    #[test]
    fn a_confirmed_fork_survives_concurrent_refreshes_and_all_later_observations() {
        let gate = Arc::new(AtomicWriteGate::new(WriteGate::Writable));
        let generation = gate.begin_generation(WriteGate::Writable).unwrap();
        let start = Arc::new(std::sync::Barrier::new(5));
        std::thread::scope(|scope| {
            for observed in [
                WriteGate::Unknown,
                WriteGate::Writable,
                WriteGate::Restricted,
            ] {
                let gate = gate.clone();
                let start = start.clone();
                scope.spawn(move || {
                    start.wait();
                    for _ in 0..10_000 {
                        gate.observe(generation, observed);
                    }
                });
            }
            let fork = gate.clone();
            let start_fork = start.clone();
            scope.spawn(move || {
                start_fork.wait();
                assert_eq!(
                    fork.observe(generation, WriteGate::Forked),
                    WriteGate::Forked
                );
                for _ in 0..10_000 {
                    assert_eq!(fork.load(), WriteGate::Forked);
                }
            });
            start.wait();
        });
        assert_eq!(gate.load(), WriteGate::Forked);
        for stale in [
            WriteGate::Unknown,
            WriteGate::Writable,
            WriteGate::Restricted,
        ] {
            assert_eq!(gate.observe(generation, stale), WriteGate::Forked);
        }
    }

    #[test]
    fn publishing_cache_evidence_retains_a_fork_confirmed_during_the_write() -> Result<()> {
        let gate = AtomicWriteGate::new(WriteGate::Writable);
        let mut writes = vec![];
        gate.persist(|observed| {
            writes.push(observed);
            if writes.len() == 1 {
                // The network task confirms the fork before the older cache write returns.
                gate.confirm_fork();
            }
            Ok(observed)
        })?;
        assert_eq!(writes, [WriteGate::Writable, WriteGate::Forked]);
        assert_eq!(gate.load(), WriteGate::Forked);
        let fresh = AtomicWriteGate::new(WriteGate::Writable);
        fresh.persist(|_| Ok(WriteGate::Forked))?;
        assert_eq!(
            fresh.load(),
            WriteGate::Forked,
            "durable evidence also closes memory"
        );
        Ok(())
    }

    #[test]
    fn an_old_generation_paused_before_publication_cannot_grant_a_replacements_writes() -> Result<()>
    {
        let gate = Arc::new(AtomicWriteGate::new(WriteGate::Unknown));
        let active = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let old = gate.begin_generation(WriteGate::Writable)?;
        assert!(gate.is_writable(old));
        let (entered, paused) = std::sync::mpsc::channel();
        let (release, resumed) = std::sync::mpsc::channel();
        std::thread::scope(|scope| -> Result<()> {
            let reader = gate.clone();
            let old_active = active.clone();
            let task = scope.spawn(move || {
                assert!(old_active.load(Ordering::Acquire));
                // Pause after the old reader's successful external active check.
                entered.send(()).unwrap();
                resumed.recv().unwrap();
                reader.observe(old, WriteGate::Writable)
            });
            paused.recv()?;
            let replacement = gate.begin_generation(WriteGate::Restricted)?;
            active.store(false, Ordering::Release);
            release.send(())?;
            assert_eq!(task.join().unwrap(), WriteGate::Restricted);
            assert_eq!(gate.load(), WriteGate::Restricted);
            assert!(!gate.is_writable(old));
            gate.observe(replacement, WriteGate::Writable);
            assert!(gate.is_writable(replacement));
            assert!(
                !gate.is_writable(old),
                "a superseded owner cannot borrow the new owner's permission"
            );
            gate.observe(replacement, WriteGate::Restricted);
            assert_eq!(gate.observe(old, WriteGate::Unknown), WriteGate::Restricted);
            gate.observe(replacement, WriteGate::Unknown);
            assert_eq!(gate.observe(old, WriteGate::Writable), WriteGate::Unknown);
            assert_eq!(gate.observe(old, WriteGate::Forked), WriteGate::Forked);
            assert_eq!(
                gate.observe(replacement, WriteGate::Writable),
                WriteGate::Forked
            );
            Ok(())
        })
    }

    #[test]
    fn generation_exhaustion_never_reuses_an_old_token_or_keeps_write_admission() {
        let last = WriteGateGeneration((u64::MAX >> 2) - 1);
        let gate = AtomicWriteGate(AtomicU64::new(
            (last.0 << 2) | u64::from(u8::from(WriteGate::Writable)),
        ));
        assert!(gate.begin_generation(WriteGate::Writable).is_err());
        assert_eq!(gate.load(), WriteGate::Unknown);
        assert_eq!(gate.observe(last, WriteGate::Writable), WriteGate::Unknown);
        assert!(gate.begin_generation(WriteGate::Restricted).is_err());
        assert_eq!(gate.observe(last, WriteGate::Forked), WriteGate::Forked);
        assert!(gate.begin_generation(WriteGate::Writable).is_err());
        assert_eq!(gate.load(), WriteGate::Forked);
    }

    #[test]
    fn publishing_an_old_cache_job_finishes_with_a_replacements_current_observation() -> Result<()>
    {
        let gate = AtomicWriteGate::new(WriteGate::Unknown);
        let old = gate.begin_generation(WriteGate::Writable)?;
        let mut writes = vec![];
        gate.persist(|observed| {
            writes.push(observed);
            if writes.len() == 1 {
                gate.begin_generation(WriteGate::Restricted)?;
                assert_eq!(
                    gate.observe(old, WriteGate::Writable),
                    WriteGate::Restricted
                );
            }
            Ok(observed)
        })?;
        assert_eq!(writes, [WriteGate::Writable, WriteGate::Restricted]);
        assert_eq!(gate.load(), WriteGate::Restricted);
        Ok(())
    }

    #[test]
    fn target_registry_keeps_a_late_fork_bound_across_generation_target_switches() -> Result<()> {
        let gates = WriteGates::default();
        let old_a = gates.for_target("a")?;
        let b = gates.for_target("b")?;
        let new_a = gates.for_target("a")?;
        old_a.confirm_fork();
        new_a.begin_generation(WriteGate::Writable)?;
        b.begin_generation(WriteGate::Writable)?;
        assert_eq!(new_a.load(), WriteGate::Forked);
        assert_eq!(b.load(), WriteGate::Writable);
        assert_eq!(
            WriteGates::default().for_target("a")?.load(),
            WriteGate::Unknown
        );
        Ok(())
    }
}
