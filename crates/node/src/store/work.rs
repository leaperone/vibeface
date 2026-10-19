//! The target Node's work journal. Each signal saves the new state and appends the event it
//! implies in one transaction, so a crash never leaves the DAG ahead of or behind the journal.
use super::{Store, local_load, local_prefix, local_save, log, scheduler};
use crate::{
    crypto::LocalIdentity,
    model::*,
    scheduling::AdmissionReceipt,
    work::{Signal, WorkRecord, WorkSpec, accept, transition},
};
use anyhow::{Context as _, Result, ensure};
use rusqlite::TransactionBehavior;

fn work_key(work_id: &str) -> String {
    format!("work:v1:{work_id}")
}

impl Store {
    /// Retrying the same `work_id` returns the existing record and never starts a second turn.
    /// The returned flag says whether this call created it.
    pub fn accept_work(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        spec: WorkSpec,
        at: i64,
    ) -> Result<(WorkRecord, bool)> {
        ensure!(
            spec.target == identity.peer_id()?,
            "work targets another Node"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = local_load::<WorkRecord>(&tx, network, &work_key(&spec.work_id))? {
            ensure!(
                existing.spec == spec,
                "work ID reused with a different request"
            );
            return Ok((existing, false));
        }
        let (record, event) = accept(spec);
        local_save(&tx, network, &work_key(&record.spec.work_id), &record)?;
        log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            &record.spec.conversation,
            event,
            vec![],
            at,
        )?;
        tx.commit()?;
        Ok((record, true))
    }

    pub fn apply_work_signal(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        work_id: &str,
        signal: Signal,
        at: i64,
    ) -> Result<(WorkRecord, Option<AdmissionReceipt>)> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut record = local_load::<WorkRecord>(&tx, network, &work_key(work_id))?
            .context("work not found")?;
        let event = transition(&mut record, signal);
        local_save(&tx, network, &work_key(work_id), &record)?;
        let mut receipt = None;
        if let Some(event) = event {
            let finished = match &event {
                EventBody::WorkFinished { outcome, .. } => Some(*outcome),
                _ => None,
            };
            log::append_from(
                &tx,
                &self.blob_dir,
                identity,
                network,
                &record.spec.conversation,
                event,
                vec![],
                at,
            )?;
            // Admitted after the append, so the result is inside the turn's basis.
            if let Some(outcome) = finished {
                receipt = scheduler::admit_work_result_from(
                    &tx,
                    network,
                    &identity.peer_id()?,
                    &record.spec.conversation,
                    work_id,
                    &record.spec.trigger,
                    outcome,
                )?;
            }
        }
        tx.commit()?;
        Ok((record, receipt))
    }

    pub fn work_record(&self, network: &str, work_id: &str) -> Result<Option<WorkRecord>> {
        local_load(&self.connection, network, &work_key(work_id))
    }

    pub fn work_records(&self, network: &str) -> Result<Vec<WorkRecord>> {
        local_prefix(&self.connection, network, "work:v1:")
    }
}
