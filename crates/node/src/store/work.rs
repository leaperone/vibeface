//! The target Node's work journal. Each signal saves the new state and appends the event it
//! implies in one transaction, so a crash never leaves the DAG ahead of or behind the journal.
use super::{Store, enum_from, enum_text, log, reduce, scheduler};
use crate::{
    crypto::LocalIdentity,
    model::*,
    scheduling::{AdmissionReceipt, TriggerKind, task_trigger},
    work::{
        Dispatch, DispatchState, Handle, Signal, StopRequest, WorkRecord, WorkSpec, WorkState,
        accept, transition,
    },
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use std::path::Path;

/// Why a removed device's work or dispatch can only be settled by the user.
const REMOVED: &str = "设备已移除，无法核对";

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
            thread: thread.filter(|_| handle.is_none()),
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
    let thread = record.state.thread();
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
pub(super) fn apply_signal_from(
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
            let work = reduce::work_from(&tx, network, &spec.work_id)?
                .context("work ID reused with a different request")?;
            let fields = [
                ("conversation", work.conversation_id == spec.conversation),
                ("trigger", work.trigger == spec.trigger),
                ("target_node", work.target_node == spec.target),
                ("dispatcher", work.dispatcher == spec.dispatcher),
                ("call_ref", work.call_ref == spec.call_ref),
                ("agent", work.kind == spec.kind),
                ("model", work.model == spec.model),
                ("cwd", work.cwd == spec.cwd),
                ("title", work.title == spec.title),
                ("prompt", work.prompt == spec.prompt),
                ("continues", work.continues == spec.continues),
                ("parent", work.parent == spec.parent),
                ("thread", work.thread == spec.thread),
            ];
            let differ: Vec<&str> = (fields.iter())
                .filter(|(_, same)| !same)
                .map(|(field, _)| *field)
                .collect();
            ensure!(
                differ.is_empty(),
                "work ID reused with a different request; changed: {}. Retry with the same values or use a new call_ref",
                differ.join(", ")
            );
            return Ok((existing, false));
        }
        ensure!(
            !super::bots::bot_deleted_from(&tx, network, &spec.conversation)?,
            "the bot that dispatched this work was deleted"
        );
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
                    reason: Some("这台电脑在这个对话里还有没做完的任务".into()),
                    failure: None,
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
            "这个结果已经不需要你确认了"
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
                failure: None,
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

    /// Open works on removed devices become `unverified`, recorded once by the conversation's
    /// owner: the device can no longer report, and whether it ran them is unknown. The user
    /// settles them like any other unverified result.
    pub fn unverify_removed_works(
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
        let mut unverified = Vec::new();
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
                        outcome: WorkOutcome::Unverified,
                        result_text: String::new(),
                        failure_reason: Some(REMOVED.into()),
                        attestation: None,
                        failure: None,
                    },
                    vec![],
                    at,
                )?;
                unverified.push((conversation.conversation_id.clone(), work.work_id));
            }
        }
        tx.commit()?;
        Ok(unverified)
    }

    pub fn work_record(&self, work_id: &str) -> Result<Option<WorkRecord>> {
        load_run(&self.connection, work_id)
    }

    pub fn work_records(&self) -> Result<Vec<WorkRecord>> {
        query_runs(&self.connection, "1", [])
    }
    /// The runs on one App Server thread, by index: a streamed event reads only these.
    pub fn work_records_on_thread(&self, thread: &str) -> Result<Vec<WorkRecord>> {
        query_runs(&self.connection, "thread_id=?", [thread])
    }

    /// The folder a dispatched work runs in, found by its App Server thread.
    pub fn work_cwd_on_thread(&self, thread: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT json_extract(w.dispatch,'$.cwd') FROM work_runs r JOIN works w ON w.work_id=r.work_id
                 WHERE r.thread_id=? AND w.dispatch IS NOT NULL LIMIT 1",
                [thread],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
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
        "SELECT spec,state,reason,failure FROM work_dispatches WHERE {filter} ORDER BY work_id"
    ))?;
    let rows = statement
        .query_map(params, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(spec, state, reason, failure)| {
            let state = match state.as_str() {
                "pending" => DispatchState::Pending,
                "accepted" => DispatchState::Accepted,
                "rejected" => DispatchState::Rejected {
                    reason: reason.unwrap_or_default(),
                    failure: failure.as_deref().map(super::enum_from).transpose()?,
                },
                "unverified" => DispatchState::Unverified {
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
    let (state, reason, failure) = match &dispatch.state {
        DispatchState::Pending => ("pending", None, None),
        DispatchState::Accepted => ("accepted", None, None),
        DispatchState::Rejected { reason, failure } => ("rejected", Some(reason), *failure),
        DispatchState::Unverified { reason } => ("unverified", Some(reason), None),
    };
    connection.execute(
        "INSERT OR REPLACE INTO work_dispatches(work_id,spec,state,reason,failure) VALUES(?,?,?,?,?)",
        params![
            dispatch.spec.work_id,
            serde_json::to_string(&dispatch.spec)?,
            state,
            reason,
            failure.as_ref().map(super::enum_text),
        ],
    )?;
    Ok(())
}

pub(super) fn dispatch_from(connection: &Connection, work_id: &str) -> Result<Option<Dispatch>> {
    Ok(query_dispatches(connection, "work_id=?", [work_id])?.pop())
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

    /// The target's answer to a waiting dispatch; the first answer stands. With `notify`, a
    /// refusal the bot did not see in its own turn wakes it once through the work's trigger.
    pub fn answer_dispatch(
        &mut self,
        network: &str,
        local: &str,
        work_id: &str,
        state: DispatchState,
        notify: bool,
    ) -> Result<(Dispatch, Option<AdmissionReceipt>)> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut dispatch = dispatch_from(&tx, work_id)?.context("dispatch not found")?;
        if dispatch.state != DispatchState::Pending {
            return Ok((dispatch, None));
        }
        dispatch.state = state;
        save_dispatch_row(&tx, &dispatch)?;
        let receipt = match notify {
            true => scheduler::admit_dispatch_result_from(&tx, network, local, &dispatch)?,
            false => None,
        };
        tx.commit()?;
        Ok((dispatch, receipt))
    }

    pub fn dispatch(&self, work_id: &str) -> Result<Option<Dispatch>> {
        dispatch_from(&self.connection, work_id)
    }

    pub fn dispatches(&self) -> Result<Vec<Dispatch>> {
        query_dispatches(&self.connection, "1", [])
    }

    /// Whether the user closed this failed work or refused dispatch in 待处理.
    pub fn closed_in_inbox(&self, network: &str, work_id: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM notification_acks WHERE network_id=? AND notification_id=? AND client_id=?)",
            params![network, work_id, crate::model::INBOX_CLIENT],
            |r| r.get(0),
        )?)
    }

    /// Whether this work in `scope` failed or was refused, so the user can close it in 待处理.
    pub fn failed_work(&self, network: &str, scope: &str, work_id: &str) -> Result<bool> {
        let failed = reduce::work_from(&self.connection, network, work_id)?.is_some_and(|work| {
            work.conversation_id == scope
                && matches!(
                    work.outcome,
                    Some(WorkOutcome::Failed | WorkOutcome::Rejected)
                )
        });
        Ok(failed
            || !query_dispatches(
                &self.connection,
                "work_id=?1 AND state='rejected' AND json_extract(spec,'$.conversation')=?2",
                params![work_id, scope],
            )?
            .is_empty())
    }

    /// This Node's refused dispatches in `scope`, or every conversation, that still want the
    /// user: not closed in 待处理 and not followed by a completed bot reply, from the reply of
    /// their report turn on or, only when there is no report turn (a refusal answered inside its
    /// dispatching turn, or reports turned off), to a message written after their dispatching
    /// turn's reply.
    pub fn open_dispatches(&self, network: &str, scope: Option<&str>) -> Result<Vec<Dispatch>> {
        query_dispatches(
            &self.connection,
            "state='rejected' AND (?2 IS NULL OR json_extract(spec,'$.conversation')=?2)
             AND NOT EXISTS(SELECT 1 FROM works w WHERE w.network_id=?1
                AND w.work_id=work_dispatches.work_id AND w.dispatch IS NOT NULL)
             AND NOT EXISTS(SELECT 1 FROM notification_acks a WHERE a.network_id=?1
                AND a.notification_id=work_dispatches.work_id AND a.client_id=?3)
             AND NOT EXISTS(SELECT 1 FROM scheduler_jobs j
                JOIN messages report ON report.network_id=?1
                    AND report.conversation_id=j.conversation_id AND report.message_id=j.message_id
                JOIN messages m ON m.network_id=?1 AND m.conversation_id=j.conversation_id
                    AND m.role='assistant' AND m.status='completed' AND m.rank>=report.rank
                WHERE j.conversation_id=json_extract(work_dispatches.spec,'$.conversation')
                    AND j.trigger='work:'||work_dispatches.work_id)
             AND NOT EXISTS(SELECT 1 FROM scheduler_jobs j
                JOIN messages turn ON turn.network_id=?1
                    AND turn.conversation_id=j.conversation_id AND turn.message_id=j.message_id
                JOIN messages u ON u.network_id=?1 AND u.conversation_id=j.conversation_id
                    AND u.role='user' AND u.rank>turn.rank
                JOIN messages r ON r.network_id=?1 AND r.conversation_id=j.conversation_id
                    AND r.in_reply_to=u.message_id AND r.role='assistant' AND r.status='completed'
                WHERE j.conversation_id=json_extract(work_dispatches.spec,'$.conversation')
                    AND j.trigger=json_extract(work_dispatches.spec,'$.trigger')
                    AND NOT EXISTS(SELECT 1 FROM scheduler_jobs report
                        WHERE report.conversation_id=j.conversation_id
                            AND report.trigger='work:'||work_dispatches.work_id))",
            params![network, scope, crate::model::INBOX_CLIENT],
        )
    }

    /// This conversation's dispatches the DAG does not name yet: waiting for their device,
    /// refused, unknown, or accepted but not synced back.
    pub fn undelivered_dispatches(&self, network: &str, scope: &str) -> Result<Vec<Dispatch>> {
        let works = reduce::works_from(&self.connection, network, scope)?;
        Ok(self
            .dispatches()?
            .into_iter()
            .filter(|dispatch| {
                dispatch.spec.conversation == scope
                    && !works
                        .iter()
                        .any(|work| work.work_id == dispatch.spec.work_id)
            })
            .collect())
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

    /// A removed device will never answer: whether it took a waiting dispatch is unknown, which
    /// wakes the bot once; its stop requests are dropped.
    pub fn drop_requests_to(
        &mut self,
        network: &str,
        local: &str,
        removed: &[DeviceId],
    ) -> Result<usize> {
        let mut woken = 0;
        for dispatch in query_dispatches(&self.connection, "state='pending'", [])? {
            if removed.contains(&dispatch.spec.target) {
                let state = DispatchState::Unverified {
                    reason: REMOVED.into(),
                };
                let (_, receipt) =
                    self.answer_dispatch(network, local, &dispatch.spec.work_id, state, true)?;
                woken += usize::from(receipt.is_some());
            }
        }
        for request in self.stop_requests()? {
            if removed.contains(&request.target) {
                self.forget_stop_request(&request.work_id)?;
            }
        }
        Ok(woken)
    }
}
