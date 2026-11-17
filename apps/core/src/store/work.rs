//! The target Node's work journal. Each signal saves the new state and appends the event it
//! implies in one transaction, so a crash never leaves the DAG ahead of or behind the journal.
use super::{Store, enum_from, enum_text, log, reduce, scheduler};
use crate::{
    crypto::LocalIdentity,
    model::*,
    scheduling::{AdmissionReceipt, TriggerKind, task_trigger},
    work::{
        Dispatch, DispatchState, Handle, RECONCILE_GRACE_MS, RequestedWorkKind, Signal,
        StopRequest, WorkAdmission, WorkRecord, WorkRequest, WorkSpec, WorkState, accept,
        transition,
    },
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use std::path::Path;

/// Why a removed device's work or dispatch can only be settled by the user.
const REMOVED: &str = "设备已移除，结果无法确认。";

const RUN_COLUMNS: &str = "work_id,conversation_id,kind,state,thread_id,turn_id,codex_epoch,outcome,evidence,probe,result_text,send_by_ms,progress,progress_at_ms";

struct DbRunRow {
    work_id: String,
    conversation_id: String,
    kind: String,
    state: String,
    thread_id: Option<String>,
    turn_id: Option<String>,
    codex_epoch: Option<u64>,
    outcome: Option<String>,
    evidence: Option<String>,
    probe: Option<bool>,
    result_text: String,
    send_by_ms: i64,
    progress: String,
    progress_at_ms: i64,
}

impl DbRunRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            work_id: row.get("work_id")?,
            conversation_id: row.get("conversation_id")?,
            kind: row.get("kind")?,
            state: row.get("state")?,
            thread_id: row.get("thread_id")?,
            turn_id: row.get("turn_id")?,
            codex_epoch: row.get("codex_epoch")?,
            outcome: row.get("outcome")?,
            evidence: row.get("evidence")?,
            probe: row.get("probe")?,
            result_text: row.get("result_text")?,
            send_by_ms: row.get("send_by_ms")?,
            progress: row.get("progress")?,
            progress_at_ms: row.get("progress_at_ms")?,
        })
    }
}

impl TryFrom<DbRunRow> for WorkRecord {
    type Error = anyhow::Error;

    fn try_from(row: DbRunRow) -> Result<Self> {
        let DbRunRow {
            work_id,
            conversation_id: conversation,
            kind,
            state,
            thread_id: thread,
            turn_id: turn,
            codex_epoch: epoch,
            outcome,
            evidence,
            probe,
            result_text: text,
            send_by_ms,
            progress,
            progress_at_ms,
        } = row;
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
            text,
            send_by_ms,
            progress,
            progress_at_ms,
        })
    }
}

fn query_runs(
    connection: &Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<WorkRecord>> {
    query_runs_sql(
        connection,
        &format!("SELECT {RUN_COLUMNS} FROM work_runs WHERE {filter} ORDER BY work_id"),
        params,
    )
}

fn query_runs_sql(
    connection: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<WorkRecord>> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement
        .query_map(params, DbRunRow::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(WorkRecord::try_from).collect()
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

fn matching_admission_from(
    connection: &Connection,
    network: &str,
    request: &WorkRequest,
) -> Result<Option<WorkAdmission>> {
    let Some(record) = load_run(connection, &request.work_id)? else {
        ensure!(
            reduce::work_from(connection, network, &request.work_id)?.is_none(),
            "work exists in shared history without admission on this Node; cannot register it again"
        );
        return Ok(None);
    };
    let work = reduce::work_from(connection, network, &request.work_id)?
        .context("work ID reused with a different request")?;
    let same_kind = match (request.kind, work.kind) {
        (RequestedWorkKind::Agent(None), WorkKind::Agent(_)) => true,
        (RequestedWorkKind::Agent(Some(requested)), WorkKind::Agent(accepted)) => {
            requested == accepted
        }
        (RequestedWorkKind::Task, WorkKind::Task) => true,
        _ => false,
    };
    let fields = [
        ("conversation", work.conversation_id == request.conversation),
        ("trigger", work.trigger == request.trigger),
        ("target_node", work.target_node == request.target),
        ("dispatcher", work.dispatcher == request.dispatcher),
        ("call_ref", work.call_ref == request.call_ref),
        ("agent", same_kind),
        ("model", work.model == request.model),
        ("cwd", work.cwd == request.cwd),
        ("title", work.title == request.title),
        ("prompt", work.prompt == request.prompt),
        ("continues", work.continues == request.continues),
        ("parent", work.parent == request.parent),
        ("thread", work.thread == request.thread),
    ];
    let differ: Vec<&str> = fields
        .iter()
        .filter(|(_, same)| !same)
        .map(|(field, _)| *field)
        .collect();
    ensure!(
        differ.is_empty(),
        "work ID reused with a different request; changed: {}. Retry with the same values or use a new call_ref",
        differ.join(", ")
    );
    Ok(Some(WorkAdmission {
        spec: WorkSpec {
            work_id: work.work_id,
            conversation: work.conversation_id,
            trigger: work.trigger,
            target: work.target_node,
            dispatcher: work.dispatcher,
            call_ref: work.call_ref,
            kind: work.kind,
            model: work.model,
            resolved: work.resolved,
            cwd: work.cwd,
            title: work.title,
            prompt: work.prompt,
            continues: work.continues,
            parent: work.parent,
            thread: work.thread,
        },
        record,
        created: false,
    }))
}

impl Store {
    pub fn accepted_work(
        &self,
        network: &str,
        request: &WorkRequest,
    ) -> Result<Option<WorkAdmission>> {
        matching_admission_from(&self.connection, network, request)
    }

    pub fn accept_work(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        request: WorkRequest,
        proposed: ResolvedModel,
        at: i64,
    ) -> Result<WorkAdmission> {
        ensure!(
            request.target == identity.device_id()?,
            "work targets another Node"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = matching_admission_from(&tx, network, &request)? {
            return Ok(existing);
        }
        let kind = match request.kind {
            RequestedWorkKind::Agent(provider) => {
                ensure!(
                    provider.is_none_or(|provider| provider == proposed.provider),
                    "resolved work provider does not match its agent"
                );
                WorkKind::Agent(proposed.provider)
            }
            RequestedWorkKind::Task => WorkKind::Task,
        };
        let spec = WorkSpec {
            work_id: request.work_id,
            conversation: request.conversation,
            trigger: request.trigger,
            target: request.target,
            dispatcher: request.dispatcher,
            call_ref: request.call_ref,
            kind,
            model: request.model,
            resolved: proposed,
            cwd: request.cwd,
            title: request.title,
            prompt: request.prompt,
            continues: request.continues,
            parent: request.parent,
            thread: request.thread,
        };
        ensure!(
            !super::bots::bot_deleted_from(&tx, network, &spec.conversation)?,
            "the bot that dispatched this work was deleted"
        );
        let (record, event) = accept(spec.clone(), at);
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
        if record.kind == WorkKind::Task {
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
        Ok(WorkAdmission {
            spec,
            record,
            created: true,
        })
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

    /// A local journal can vouch only for the same work accepted by this target in this scope.
    pub(crate) fn work_record_in_scope(
        &self,
        network: &str,
        conversation: &str,
        work_id: &str,
        target: &str,
    ) -> Result<Option<WorkRecord>> {
        self.authorize_node(network, target)?;
        let Some(work) = reduce::work_from(&self.connection, network, work_id)? else {
            return Ok(None);
        };
        ensure!(
            work.conversation_id == conversation && work.target_node == target,
            "work evidence belongs to another conversation or target"
        );
        let record = load_run(&self.connection, work_id)?;
        ensure!(
            record.as_ref().is_none_or(|record| {
                record.conversation == conversation && record.kind == work.kind
            }),
            "local work evidence does not match its shared work"
        );
        Ok(record)
    }

    pub fn work_records(&self) -> Result<Vec<WorkRecord>> {
        query_runs(&self.connection, "1", [])
    }
    pub fn open_work_records(&self) -> Result<Vec<WorkRecord>> {
        query_runs(&self.connection, "state<>'finished'", [])
    }

    /// Only runs whose next reconciliation can do work, excluding coordination tasks.
    pub fn reconcile_work_records(&self, at: i64, read_live: bool) -> Result<Vec<WorkRecord>> {
        let filter = if read_live {
            "kind<>?1 AND ((state IN ('accepted','starting') AND send_by_ms<=?2)
                OR state='running' OR (state='unverified' AND probe=1))"
        } else {
            "kind<>?1 AND state IN ('accepted','starting') AND send_by_ms<=?2"
        };
        query_runs(
            &self.connection,
            filter,
            params![enum_text(&WorkKind::Task), at - RECONCILE_GRACE_MS],
        )
    }

    /// The runs on one App Server thread, by index: a streamed event reads only these.
    pub fn work_records_on_thread(&self, thread: &str) -> Result<Vec<WorkRecord>> {
        query_runs(&self.connection, "thread_id=?", [thread])
    }

    pub(crate) fn local_work_provider_on_thread(
        &self,
        network: &str,
        node: &str,
        thread: &str,
    ) -> Result<Option<crate::providers::Provider>> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT r.kind FROM works w JOIN conversations c
                ON c.network_id=w.network_id AND c.conversation_id=w.conversation_id
             JOIN work_runs r
                ON r.work_id=w.work_id AND r.conversation_id=w.conversation_id
             WHERE w.network_id=?1 AND w.target_node=?2 AND w.dispatch IS NOT NULL
                AND (r.thread_id=?3 OR w.thread_id=?3 OR json_extract(w.dispatch,'$.thread')=?3)",
        )?;
        let kinds = statement.query_map(params![network, node, thread], |row| {
            row.get::<_, String>(0)
        })?;
        let mut provider = None;
        for kind in kinds {
            if let WorkKind::Agent(ran) = enum_from(&kind?)? {
                ensure!(
                    provider.is_none_or(|provider| provider == ran),
                    "native session has conflicting provider evidence"
                );
                provider = Some(ran);
            }
        }
        Ok(provider)
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
    let columns = RUN_COLUMNS
        .split(',')
        .map(|column| format!("r.{column}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT {columns},json_extract(w.finish,'$.outcome') AS settled_outcome FROM work_runs r CROSS JOIN works w
        WHERE w.network_id=?1 AND w.work_id=r.work_id AND r.state IN ('unverified','accepted','starting')
            AND w.dispatch IS NOT NULL AND w.finish IS NOT NULL
            AND json_extract(w.finish,'$.outcome')<>?2 ORDER BY r.work_id");
    let rows = connection
        .prepare(&sql)?
        .query_map(
            params![network, enum_text(&WorkOutcome::Unverified)],
            |row| {
                Ok((
                    DbRunRow::from_row(row)?,
                    row.get::<_, String>("settled_outcome")?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (row, outcome) in rows {
        let mut record = WorkRecord::try_from(row)?;
        transition(&mut record, Signal::Settled(enum_from(&outcome)?));
        save_run(connection, &record)?;
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

    /// Scoped UI projection; accepted rows remain visible until shared sync arrives.
    pub fn dispatches_for_scopes(&self, scopes: &[String]) -> Result<Vec<Dispatch>> {
        query_dispatches(
            &self.connection,
            "json_extract(spec,'$.conversation') IN (SELECT value FROM json_each(?))",
            [serde_json::to_string(scopes)?],
        )
    }

    pub fn pending_dispatches(&self) -> Result<Vec<Dispatch>> {
        query_dispatches(&self.connection, "state='pending'", [])
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
        query_dispatches(
            &self.connection,
            "json_extract(spec,'$.conversation')=?2 AND NOT EXISTS(
                SELECT 1 FROM works w WHERE w.network_id=?1
                    AND w.conversation_id=?2 AND w.work_id=work_dispatches.work_id
                    AND w.dispatch IS NOT NULL)",
            params![network, scope],
        )
    }

    /// Holds the first request unchanged, including its target and original deadline.
    pub fn hold_stop_request(&mut self, request: &StopRequest) -> Result<bool> {
        Ok(self.connection.execute(
            "INSERT INTO work_stops(work_id,conversation_id,target_node,requested_at_ms,deadline_ms,timed_out)
             VALUES(?,?,?,?,?,?) ON CONFLICT(work_id) DO NOTHING",
            params![request.work_id, request.conversation, request.target,
                request.requested_at_ms, request.deadline_ms, request.timed_out],
        )? != 0)
    }

    pub fn stop_request(&self, work_id: &str) -> Result<Option<StopRequest>> {
        Ok(query_stops(&self.connection, "work_id=?", [work_id])?.pop())
    }

    pub fn stop_requests_for_target(&self, target: &str) -> Result<Vec<StopRequest>> {
        query_stops(&self.connection, "target_node=?", [target])
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
        query_stops(&self.connection, "1", [])
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

fn query_stops(
    connection: &Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<StopRequest>> {
    let mut statement = connection.prepare(&format!(
        "SELECT work_id,conversation_id,target_node,requested_at_ms,deadline_ms,timed_out
         FROM work_stops WHERE {filter} ORDER BY work_id"
    ))?;
    Ok(statement
        .query_map(params, |row| {
            Ok(StopRequest {
                work_id: row.get(0)?,
                conversation: row.get(1)?,
                target: row.get(2)?,
                requested_at_ms: row.get(3)?,
                deadline_ms: row.get(4)?,
                timed_out: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod db_row_codec_tests {
    use super::*;

    fn record(state: WorkState) -> WorkRecord {
        WorkRecord {
            work_id: "work".into(),
            conversation: "conversation".into(),
            kind: WorkKind::Agent(crate::providers::Provider::Codex),
            state,
            text: "final answer".into(),
            send_by_ms: 123,
            progress: "latest progress".into(),
            progress_at_ms: 97,
        }
    }

    #[test]
    fn run_states_reopen_and_decode_reordered_columns() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("vibeface-run-codec-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let path = dir.join("node.sqlite3");
        let store = Store::open(&path)?;
        let handle = Handle {
            thread_id: "thread".into(),
            turn_id: "turn".into(),
            epoch: 41,
        };
        let states = [
            WorkState::Accepted,
            WorkState::Starting { thread: None },
            WorkState::Starting {
                thread: Some("starting-thread".into()),
            },
            WorkState::Running {
                handle: handle.clone(),
            },
            WorkState::Interrupting {
                handle: handle.clone(),
            },
            WorkState::Finished {
                outcome: WorkOutcome::Completed,
            },
            WorkState::Finished {
                outcome: WorkOutcome::Failed,
            },
            WorkState::Finished {
                outcome: WorkOutcome::Interrupted,
            },
            WorkState::Finished {
                outcome: WorkOutcome::Rejected,
            },
            WorkState::Finished {
                outcome: WorkOutcome::Unverified,
            },
            WorkState::Unverified {
                handle: Some(handle),
                thread: None,
                evidence: "unknown".into(),
                probe: true,
            },
            WorkState::Unverified {
                handle: None,
                thread: Some("late-thread".into()),
                evidence: "late start".into(),
                probe: false,
            },
            WorkState::Unverified {
                handle: None,
                thread: None,
                evidence: String::new(),
                probe: false,
            },
        ];
        let mut expected = Vec::new();
        for (index, state) in states.into_iter().enumerate() {
            let mut value = record(state);
            value.work_id = format!("work-{index:02}");
            if index % 2 == 0 {
                value.kind = WorkKind::Task;
            }
            save_run(&store.connection, &value)?;
            expected.push(value);
        }
        assert_eq!(query_runs(&store.connection, "1", [])?, expected);
        drop(store);
        let store = Store::open(&path)?;
        assert_eq!(query_runs(&store.connection, "1", [])?, expected);
        assert_eq!(
            query_runs_sql(
                &store.connection,
                "SELECT progress_at_ms,progress,send_by_ms,result_text,probe,evidence,outcome,codex_epoch,turn_id,thread_id,state,kind,conversation_id,work_id FROM work_runs ORDER BY work_id",
                []
            )?,
            expected
        );
        drop(store);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn run_codec_preserves_required_handle_and_enum_rejections_and_null_defaults() -> Result<()> {
        let store = Store::open(":memory:")?;
        let handle = Handle {
            thread_id: "thread".into(),
            turn_id: "turn".into(),
            epoch: 41,
        };
        for state in [
            WorkState::Running {
                handle: handle.clone(),
            },
            WorkState::Interrupting { handle },
        ] {
            for column in ["thread_id", "turn_id", "codex_epoch"] {
                save_run(&store.connection, &record(state.clone()))?;
                store
                    .connection
                    .execute(&format!("UPDATE work_runs SET {column}=NULL"), [])?;
                let error = query_runs(&store.connection, "1", []).unwrap_err();
                assert!(
                    error.to_string().contains("work run handle missing"),
                    "{state:?}/{column}: {error}"
                );
            }
        }
        for (column, value) in [
            ("state", "unknown"),
            ("kind", "{"),
            ("outcome", "\"unknown\""),
        ] {
            save_run(
                &store.connection,
                &record(WorkState::Finished {
                    outcome: WorkOutcome::Completed,
                }),
            )?;
            store
                .connection
                .execute(&format!("UPDATE work_runs SET {column}=?"), [value])?;
            assert!(query_runs(&store.connection, "1", []).is_err(), "{column}");
        }
        save_run(
            &store.connection,
            &record(WorkState::Finished {
                outcome: WorkOutcome::Completed,
            }),
        )?;
        store
            .connection
            .execute("UPDATE work_runs SET outcome=NULL", [])?;
        assert!(query_runs(&store.connection, "1", []).is_err());
        let expected = record(WorkState::Unverified {
            handle: None,
            thread: Some("late-thread".into()),
            evidence: String::new(),
            probe: false,
        });
        save_run(&store.connection, &expected)?;
        store
            .connection
            .execute("UPDATE work_runs SET evidence=NULL,probe=NULL", [])?;
        assert_eq!(load_run(&store.connection, "work")?, Some(expected));
        Ok(())
    }

    #[test]
    fn settled_run_uses_joined_outcome_instead_of_local_outcome() -> Result<()> {
        let store = Store::open(":memory:")?;
        save_run(&store.connection, &record(WorkState::Accepted))?;
        store.connection.execute(r#"INSERT INTO works(network_id,work_id,conversation_id,dispatch,finish) VALUES('network','work','conversation','{}','{"outcome":"completed"}')"#, [])?;
        settle_local_from(&store.connection, "network")?;
        assert_eq!(
            load_run(&store.connection, "work")?,
            Some(record(WorkState::Finished {
                outcome: WorkOutcome::Completed
            }))
        );
        Ok(())
    }
}
