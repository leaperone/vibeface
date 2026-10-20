//! The target Node's work journal. Each signal saves the new state and appends the event it
//! implies in one transaction, so a crash never leaves the DAG ahead of or behind the journal.
use super::{Store, local_load, local_prefix, local_save, log, reduce, scheduler};
use crate::{
    crypto::LocalIdentity,
    model::*,
    scheduling::{AdmissionReceipt, JobOutcome, TriggerKind, task_trigger},
    work::{
        Dispatch, DispatchState, Signal, StopRequest, WorkRecord, WorkSpec, WorkState, accept,
        transition,
    },
};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, TransactionBehavior};
use std::path::Path;

fn work_key(work_id: &str) -> String {
    format!("work:v1:{work_id}")
}

/// Applies one signal inside the caller's transaction; a finished result is admitted after the
/// append, so it is inside the reporting turn's basis.
fn apply_signal_from(
    connection: &Connection,
    blob_dir: &Path,
    identity: &LocalIdentity,
    network: &str,
    work_id: &str,
    signal: Signal,
    at: i64,
) -> Result<(WorkRecord, Option<AdmissionReceipt>)> {
    let mut record = local_load::<WorkRecord>(connection, network, &work_key(work_id))?
        .context("work not found")?;
    let event = transition(&mut record, signal);
    local_save(connection, network, &work_key(work_id), &record)?;
    let mut receipt = None;
    if let Some(event) = event {
        let finished = match &event {
            EventBody::WorkFinished { outcome, .. } => Some(*outcome),
            _ => None,
        };
        log::append_from(
            connection,
            blob_dir,
            identity,
            network,
            &record.spec.conversation,
            event,
            vec![],
            at,
        )?;
        if let Some(outcome) = finished {
            receipt = scheduler::admit_work_result_from(
                connection,
                network,
                &identity.device_id()?,
                &record.spec.conversation,
                work_id,
                outcome,
            )?;
        }
    }
    Ok((record, receipt))
}

/// Ends an agent task's work with its coordination turn; the reply becomes the result.
#[expect(clippy::too_many_arguments, reason = "the turn's end and its result")]
pub(crate) fn finish_agent_work_from(
    connection: &Connection,
    blob_dir: &Path,
    identity: &LocalIdentity,
    network: &str,
    work_id: &str,
    outcome: &JobOutcome,
    text: &str,
    at: i64,
) -> Result<()> {
    let (outcome, reason) = match outcome {
        JobOutcome::Completed => (WorkOutcome::Completed, None),
        JobOutcome::Failed(detail) => (WorkOutcome::Failed, Some(detail.clone())),
        JobOutcome::Cancelled(evidence) => (WorkOutcome::Interrupted, Some(evidence.clone())),
        JobOutcome::Interrupted {
            detail,
            process_unconfirmed,
        } => (
            if *process_unconfirmed {
                WorkOutcome::Unverified
            } else {
                WorkOutcome::Interrupted
            },
            Some(detail.clone()),
        ),
    };
    apply_signal_from(
        connection,
        blob_dir,
        identity,
        network,
        work_id,
        Signal::Concluded {
            outcome,
            text: text.into(),
            reason,
        },
        at,
    )?;
    Ok(())
}

fn settled(record: &WorkRecord) -> bool {
    !matches!(record.state, WorkState::Unverified { .. })
}

impl Store {
    /// Retrying the same `work_id` returns the existing record and never starts a second turn.
    /// The returned flag says whether this call created it. An agent task is queued for this
    /// Node's coordination Agent in the same transaction.
    pub fn accept_work(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        spec: WorkSpec,
        at: i64,
    ) -> Result<(WorkRecord, bool)> {
        ensure!(
            spec.target == identity.device_id()?,
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
        let busy = spec.kind == WorkKind::Task
            && local_prefix::<WorkRecord>(&tx, network, "work:v1:")?
                .iter()
                .any(|other| {
                    other.spec.kind == WorkKind::Task
                        && other.spec.conversation == spec.conversation
                        && !other.state.finished()
                });
        let (mut record, event) = accept(spec, at);
        let work_id = record.spec.work_id.clone();
        local_save(&tx, network, &work_key(&work_id), &record)?;
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
        if busy {
            record = apply_signal_from(
                &tx,
                &self.blob_dir,
                identity,
                network,
                &work_id,
                Signal::Concluded {
                    outcome: WorkOutcome::Interrupted,
                    text: String::new(),
                    reason: Some("这台设备在这个对话里还有没做完的任务".into()),
                },
                at,
            )?
            .0;
        } else if record.spec.kind == WorkKind::Task {
            scheduler::admit_from(
                &tx,
                network,
                &record.spec.conversation,
                &task_trigger(&work_id),
                TriggerKind::AgentTask,
                &work_id,
            )?;
        }
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
        let result =
            apply_signal_from(&tx, &self.blob_dir, identity, network, work_id, signal, at)?;
        tx.commit()?;
        Ok(result)
    }

    /// The user's signed choice for an unverified result, written by whichever entry Node they
    /// use. Only `unverified` can be superseded; the result then reports like any other.
    #[expect(clippy::too_many_arguments, reason = "the user's signed settlement")]
    pub fn settle_work(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        scope: &str,
        work_id: &str,
        outcome: WorkOutcome,
        note: &str,
        attestation: Attestation,
        at: i64,
    ) -> Result<Option<AdmissionReceipt>> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let work = reduce::works_from(&tx, network, scope)?
            .into_iter()
            .find(|work| work.work_id == work_id)
            .context("work not found in this conversation")?;
        ensure!(
            work.outcome == Some(WorkOutcome::Unverified),
            "this result no longer needs your confirmation"
        );
        log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            scope,
            EventBody::WorkFinished {
                work_id: work_id.into(),
                outcome,
                result_text: note.into(),
                failure_reason: None,
                attestation: Some(attestation),
            },
            vec![],
            at,
        )?;
        settle_local_from(&tx, network)?;
        let receipt = scheduler::admit_work_result_from(
            &tx,
            network,
            &identity.device_id()?,
            scope,
            work_id,
            outcome,
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    /// Local journals the DAG has settled (the user's choice arrived by sync) stop waiting.
    pub fn settle_local_works(&mut self, network: &str) -> Result<()> {
        settle_local_from(&self.connection, network)
    }

    /// Open works on removed devices become `abandoned`, recorded once by the conversation's
    /// owner, so nothing waits forever on a device that can no longer report.
    pub fn abandon_works(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        removed: &[DeviceId],
        at: i64,
    ) -> Result<Vec<(String, String)>> {
        let local = identity.device_id()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut abandoned = Vec::new();
        for conversation in super::conversations_from(&tx, network)? {
            if conversation.owner.as_deref() != Some(local.as_str()) {
                continue;
            }
            for work in reduce::works_from(&tx, network, &conversation.conversation_id)? {
                if work.outcome.is_some() || !removed.contains(&work.target_node) {
                    continue;
                }
                log::append_from(
                    &tx,
                    &self.blob_dir,
                    identity,
                    network,
                    &conversation.conversation_id,
                    EventBody::WorkFinished {
                        work_id: work.work_id.clone(),
                        outcome: WorkOutcome::Abandoned,
                        result_text: String::new(),
                        failure_reason: Some("设备已移除，任务已放弃、未执行".into()),
                        attestation: None,
                    },
                    vec![],
                    at,
                )?;
                scheduler::admit_work_result_from(
                    &tx,
                    network,
                    &local,
                    &conversation.conversation_id,
                    &work.work_id,
                    WorkOutcome::Abandoned,
                )?;
                abandoned.push((conversation.conversation_id.clone(), work.work_id));
            }
        }
        tx.commit()?;
        Ok(abandoned)
    }

    pub fn work_record(&self, network: &str, work_id: &str) -> Result<Option<WorkRecord>> {
        local_load(&self.connection, network, &work_key(work_id))
    }

    pub fn work_records(&self, network: &str) -> Result<Vec<WorkRecord>> {
        local_prefix(&self.connection, network, "work:v1:")
    }
}

fn settle_local_from(connection: &Connection, network: &str) -> Result<()> {
    for mut record in local_prefix::<WorkRecord>(connection, network, "work:v1:")? {
        if settled(&record) {
            continue;
        }
        let outcome = reduce::work_from(connection, network, &record.spec.work_id)?
            .and_then(|work| work.outcome);
        if let Some(outcome) = outcome.filter(|outcome| *outcome != WorkOutcome::Unverified) {
            transition(&mut record, Signal::Settled(outcome));
            local_save(
                connection,
                network,
                &work_key(&record.spec.work_id),
                &record,
            )?;
        }
    }
    Ok(())
}

fn dispatch_key(work_id: &str) -> String {
    format!("dispatch:v1:{work_id}")
}

fn stop_key(work_id: &str) -> String {
    format!("stop:v1:{work_id}")
}

impl Store {
    /// The first save fixes the request; a retry with other content is refused.
    pub fn save_dispatch(&mut self, network: &str, dispatch: &Dispatch) -> Result<Dispatch> {
        let key = dispatch_key(&dispatch.spec.work_id);
        if let Some(existing) = local_load::<Dispatch>(&self.connection, network, &key)? {
            ensure!(
                existing.spec == dispatch.spec,
                "work ID reused with a different request"
            );
            if existing.state != DispatchState::Pending {
                return Ok(existing);
            }
        }
        local_save(&self.connection, network, &key, dispatch)?;
        Ok(dispatch.clone())
    }

    pub fn dispatch(&self, network: &str, work_id: &str) -> Result<Option<Dispatch>> {
        local_load(&self.connection, network, &dispatch_key(work_id))
    }

    pub fn dispatches(&self, network: &str) -> Result<Vec<Dispatch>> {
        local_prefix(&self.connection, network, "dispatch:v1:")
    }

    pub fn save_stop_request(&mut self, network: &str, request: &StopRequest) -> Result<()> {
        local_save(
            &self.connection,
            network,
            &stop_key(&request.work_id),
            request,
        )
    }

    pub fn stop_requests(&self, network: &str) -> Result<Vec<StopRequest>> {
        local_prefix(&self.connection, network, "stop:v1:")
    }

    pub fn forget_stop_request(&mut self, network: &str, work_id: &str) -> Result<()> {
        self.local_delete(network, &stop_key(work_id))
    }

    /// A removed device will never answer: its dispatches are refused and stop requests dropped.
    pub fn drop_requests_to(&mut self, network: &str, removed: &[DeviceId]) -> Result<()> {
        for mut dispatch in self.dispatches(network)? {
            if dispatch.state == DispatchState::Pending && removed.contains(&dispatch.spec.target) {
                dispatch.state = DispatchState::Rejected {
                    reason: "设备已移除，任务已放弃、未执行".into(),
                };
                local_save(
                    &self.connection,
                    network,
                    &dispatch_key(&dispatch.spec.work_id),
                    &dispatch,
                )?;
            }
        }
        for request in self.stop_requests(network)? {
            if removed.contains(&request.target) {
                self.forget_stop_request(network, &request.work_id)?;
            }
        }
        Ok(())
    }
}
