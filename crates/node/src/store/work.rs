//! The target Node's work journal. Each signal saves the new state and appends the event it
//! implies in one transaction, so a crash never leaves the DAG ahead of or behind the journal.
use super::{Store, enum_from, enum_text, log, reduce, scheduler};
use crate::{
    crypto::LocalIdentity,
    model::*,
    scheduling::{AdmissionReceipt, JobOutcome, TriggerKind, task_trigger},
    work::{
        Dispatch, DispatchState, Handle, Signal, StopRequest, WorkRecord, WorkSpec, WorkState,
        accept, transition,
    },
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, TransactionBehavior, params};
use std::path::Path;

const RUN_COLUMNS: &str = "work_id,conversation_id,kind,state,thread_id,turn_id,codex_epoch,outcome,evidence,probe,result_text,send_by_ms,progress,progress_at_ms";

type RunRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<u64>,
    Option<String>,
    Option<String>,
    Option<bool>,
    String,
    i64,
    String,
    i64,
);

fn record_from(row: RunRow) -> Result<WorkRecord> {
    let (work_id, conversation, kind, state, thread, turn, epoch, outcome, evidence, probe) = (
        row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9,
    );
    let handle = match (thread.clone(), turn, epoch) {
        (Some(thread_id), Some(turn_id), Some(epoch)) => Some(Handle {
            thread_id,
            turn_id,
            epoch,
        }),
        _ => None,
    };
    let state = match state.as_str() {
        "accepted" => WorkState::Accepted,
        "starting" => WorkState::Starting { thread },
        "running" => WorkState::Running {
            handle: handle.context("work run handle missing")?,
        },
        "interrupting" => WorkState::Interrupting {
            handle: handle.context("work run handle missing")?,
        },
        "finished" => WorkState::Finished {
            outcome: enum_from(&outcome.context("work run outcome missing")?)?,
        },
        "unverified" => WorkState::Unverified {
            handle,
            evidence: evidence.unwrap_or_default(),
            probe: probe.unwrap_or(false),
        },
        other => bail!("unknown work run state {other}"),
    };
    Ok(WorkRecord {
        work_id,
        conversation,
        kind: enum_from(&kind)?,
        state,
        text: row.10,
        send_by_ms: row.11,
        progress: row.12,
        progress_at_ms: row.13,
    })
}

fn query_runs(
    connection: &Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<WorkRecord>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {RUN_COLUMNS} FROM work_runs WHERE {filter} ORDER BY work_id"
    ))?;
    let rows = statement
        .query_map(params, |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get(12)?,
                r.get(13)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<RunRow>>>()?;
    rows.into_iter().map(record_from).collect()
}

fn load_run(connection: &Connection, work_id: &str) -> Result<Option<WorkRecord>> {
    Ok(query_runs(connection, "work_id=?", [work_id])?.pop())
}

pub(super) fn run_finished(connection: &Connection, work_id: &str) -> Result<bool> {
    Ok(load_run(connection, work_id)?.is_some_and(|run| run.state.finished()))
}

fn save_run(connection: &Connection, record: &WorkRecord) -> Result<()> {
    let handle = record.state.handle();
    let thread = match &record.state {
        WorkState::Starting { thread } => thread.as_ref(),
        _ => handle.map(|handle| &handle.thread_id),
    };
    let (outcome, evidence, probe) = match &record.state {
        WorkState::Finished { outcome } => (Some(enum_text(outcome)), None, None),
        WorkState::Unverified {
            evidence, probe, ..
        } => (None, Some(evidence), Some(*probe)),
        _ => (None, None, None),
    };
    connection.execute(
        &format!(
            "INSERT OR REPLACE INTO work_runs({RUN_COLUMNS}) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)"
        ),
        params![
            record.work_id,
            record.conversation,
            enum_text(&record.kind),
            record.state.name(),
            thread,
            handle.map(|handle| &handle.turn_id),
            handle.map(|handle| handle.epoch),
            outcome,
            evidence,
            probe,
            record.text,
            record.send_by_ms,
            record.progress,
            record.progress_at_ms,
        ],
    )?;
    Ok(())
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
    let mut record = load_run(connection, work_id)?.context("work not found")?;
    let event = transition(&mut record, signal);
    save_run(connection, &record)?;
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
            &record.conversation,
            event,
            vec![],
            at,
        )?;
        // The user's settlement already reported this work; a late result stays on record only.
        if let Some(outcome) = finished
            && !reduce::settled_by_user(connection, network, work_id)?
        {
            receipt = scheduler::admit_work_result_from(
                connection,
                network,
                &identity.device_id()?,
                &record.conversation,
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
        if let Some(existing) = load_run(&tx, &spec.work_id)? {
            let same = reduce::work_from(&tx, network, &spec.work_id)?.is_some_and(|work| {
                work.conversation_id == spec.conversation
                    && work.trigger == spec.trigger
                    && work.target_node == spec.target
                    && work.dispatcher == spec.dispatcher
                    && work.call_ref == spec.call_ref
                    && work.kind == spec.kind
                    && work.cwd == spec.cwd
                    && work.title == spec.title
                    && work.prompt == spec.prompt
                    && work.continues == spec.continues
                    && work.parent == spec.parent
                    && work.thread == spec.thread
            });
            ensure!(same, "work ID reused with a different request");
            return Ok((existing, false));
        }
        let busy = spec.kind == WorkKind::Task
            && !query_runs(
                &tx,
                "kind='task' AND conversation_id=? AND state<>'finished'",
                [&spec.conversation],
            )?
            .is_empty();
        let (mut record, event) = accept(spec, at);
        let work_id = record.work_id.clone();
        save_run(&tx, &record)?;
        log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            &record.conversation,
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
        } else if record.kind == WorkKind::Task {
            scheduler::admit_from(
                &tx,
                network,
                &record.conversation,
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

    /// The user's signed choice for a result its device cannot vouch for, written by whichever
    /// entry Node they use: an `unverified` one, or an open one when the entry saw that its
    /// device cannot report (`unvouched`). The result then reports like any other.
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
        unvouched: bool,
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
            work.outcome == Some(WorkOutcome::Unverified) || unvouched && work.outcome.is_none(),
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

    pub fn work_record(&self, work_id: &str) -> Result<Option<WorkRecord>> {
        load_run(&self.connection, work_id)
    }

    pub fn work_records(&self) -> Result<Vec<WorkRecord>> {
        query_runs(&self.connection, "1", [])
    }
}

/// A run that has not started stays unstarted once the user settled it; a running one keeps
/// going and its late result stays on record without reporting again.
fn settle_local_from(connection: &Connection, network: &str) -> Result<()> {
    for mut record in query_runs(
        connection,
        "state IN ('unverified','accepted','starting')",
        [],
    )? {
        let outcome =
            reduce::work_from(connection, network, &record.work_id)?.and_then(|work| work.outcome);
        if let Some(outcome) = outcome.filter(|outcome| *outcome != WorkOutcome::Unverified) {
            transition(&mut record, Signal::Settled(outcome));
            save_run(connection, &record)?;
        }
    }
    Ok(())
}

fn query_dispatches(
    connection: &Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Dispatch>> {
    let mut statement = connection.prepare(&format!(
        "SELECT spec,state,reason FROM work_dispatches WHERE {filter} ORDER BY work_id"
    ))?;
    let rows = statement
        .query_map(params, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(spec, state, reason)| {
            let state = match state.as_str() {
                "pending" => DispatchState::Pending,
                "accepted" => DispatchState::Accepted,
                "rejected" => DispatchState::Rejected {
                    reason: reason.unwrap_or_default(),
                },
                other => bail!("unknown dispatch state {other}"),
            };
            Ok(Dispatch {
                spec: serde_json::from_str(&spec)?,
                state,
            })
        })
        .collect()
}

fn save_dispatch_row(connection: &Connection, dispatch: &Dispatch) -> Result<()> {
    let (state, reason) = match &dispatch.state {
        DispatchState::Pending => ("pending", None),
        DispatchState::Accepted => ("accepted", None),
        DispatchState::Rejected { reason } => ("rejected", Some(reason)),
    };
    connection.execute(
        "INSERT OR REPLACE INTO work_dispatches(work_id,conversation_id,target_node,spec,state,reason) VALUES(?,?,?,?,?,?)",
        params![
            dispatch.spec.work_id,
            dispatch.spec.conversation,
            dispatch.spec.target,
            serde_json::to_string(&dispatch.spec)?,
            state,
            reason,
        ],
    )?;
    Ok(())
}

impl Store {
    /// The first save fixes the request; a retry with other content is refused.
    pub fn save_dispatch(&mut self, dispatch: &Dispatch) -> Result<Dispatch> {
        if let Some(existing) = self.dispatch(&dispatch.spec.work_id)? {
            ensure!(
                existing.spec == dispatch.spec,
                "work ID reused with a different request"
            );
            if existing.state != DispatchState::Pending {
                return Ok(existing);
            }
        }
        save_dispatch_row(&self.connection, dispatch)?;
        Ok(dispatch.clone())
    }

    pub fn dispatch(&self, work_id: &str) -> Result<Option<Dispatch>> {
        Ok(query_dispatches(&self.connection, "work_id=?", [work_id])?.pop())
    }

    pub fn dispatches(&self) -> Result<Vec<Dispatch>> {
        query_dispatches(&self.connection, "1", [])
    }

    pub fn save_stop_request(&mut self, request: &StopRequest) -> Result<()> {
        self.connection.execute(
            "INSERT OR REPLACE INTO work_stops(work_id,conversation_id,target_node,requested_at_ms,deadline_ms,timed_out) VALUES(?,?,?,?,?,?)",
            params![
                request.work_id,
                request.conversation,
                request.target,
                request.requested_at_ms,
                request.deadline_ms,
                request.timed_out,
            ],
        )?;
        Ok(())
    }

    pub fn stop_requests(&self) -> Result<Vec<StopRequest>> {
        let mut statement = self.connection.prepare(
            "SELECT work_id,conversation_id,target_node,requested_at_ms,deadline_ms,timed_out FROM work_stops ORDER BY work_id",
        )?;
        Ok(statement
            .query_map([], |r| {
                Ok(StopRequest {
                    work_id: r.get(0)?,
                    conversation: r.get(1)?,
                    target: r.get(2)?,
                    requested_at_ms: r.get(3)?,
                    deadline_ms: r.get(4)?,
                    timed_out: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn forget_stop_request(&mut self, work_id: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM work_stops WHERE work_id=?", [work_id])?;
        Ok(())
    }

    /// A removed device will never answer: its dispatches are refused and stop requests dropped.
    pub fn drop_requests_to(&mut self, removed: &[DeviceId]) -> Result<()> {
        for mut dispatch in query_dispatches(&self.connection, "state='pending'", [])? {
            if removed.contains(&dispatch.spec.target) {
                dispatch.state = DispatchState::Rejected {
                    reason: "设备已移除，任务已放弃、未执行".into(),
                };
                save_dispatch_row(&self.connection, &dispatch)?;
            }
        }
        for request in self.stop_requests()? {
            if removed.contains(&request.target) {
                self.forget_stop_request(&request.work_id)?;
            }
        }
        Ok(())
    }
}
