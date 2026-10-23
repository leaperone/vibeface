//! The owner's local scheduling state. A job is keyed by its trigger, and its reply is written
//! in the same transaction that ends it, so nothing is published twice or lost.
use super::{Store, authorize_node_from, conversation_from, log, message_from};
use crate::{crypto::LocalIdentity, model::*, scheduling::*};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::path::Path;

const JOB_COLUMNS: &str = "conversation_id,trigger,kind,sequence,basis,reply_to,message_id,state,attempt_id,attempt_boot,attempt_permission,detail";

type JobRow = (
    String,
    String,
    String,
    u64,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<u64>,
    Option<String>,
);

fn job_from_row(row: JobRow) -> Result<SchedulerJob> {
    let (
        conversation_id,
        trigger,
        kind,
        sequence,
        basis,
        reply_to,
        message_id,
        state,
        id,
        boot,
        permission,
        detail,
    ) = row;
    let attempt = match (id, boot, permission) {
        (Some(id), Some(boot), Some(permission_version)) => Some(ManagerAttempt {
            id,
            boot,
            permission_version,
        }),
        _ => None,
    };
    let attempt_or = || attempt.clone().context("scheduler job attempt missing");
    let detail = detail.unwrap_or_default();
    let state = match state.as_str() {
        "queued" => JobState::Queued,
        "claimed" => JobState::Claimed {
            attempt: attempt_or()?,
        },
        "running" => JobState::Running {
            attempt: attempt_or()?,
        },
        "cancelling" => JobState::Cancelling {
            attempt: attempt_or()?,
        },
        "completed" => JobState::Completed {
            attempt: attempt_or()?,
        },
        "failed" => JobState::Failed {
            attempt: attempt_or()?,
            detail,
        },
        "cancelled" => JobState::Cancelled {
            attempt,
            evidence: detail,
        },
        "interrupted" => JobState::Interrupted {
            attempt: attempt_or()?,
            detail,
        },
        other => bail!("unknown scheduler job state {other}"),
    };
    Ok(SchedulerJob {
        conversation_id,
        trigger,
        kind: super::enum_from(&kind)?,
        sequence,
        basis: serde_json::from_str(&basis)?,
        reply_to,
        message_id,
        state,
    })
}

fn query_jobs(
    connection: &Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<SchedulerJob>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {JOB_COLUMNS} FROM scheduler_jobs WHERE {filter} ORDER BY sequence"
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
            ))
        })?
        .collect::<rusqlite::Result<Vec<JobRow>>>()?;
    rows.into_iter().map(job_from_row).collect()
}

fn load_job(
    connection: &Connection,
    conversation: &str,
    trigger: &str,
) -> Result<Option<SchedulerJob>> {
    Ok(query_jobs(
        connection,
        "conversation_id=? AND trigger=?",
        params![conversation, trigger],
    )?
    .pop())
}

/// Queued jobs are the conversation's inbox, oldest first.
fn queued_jobs(connection: &Connection, conversation: &str) -> Result<Vec<SchedulerJob>> {
    query_jobs(
        connection,
        "conversation_id=? AND state='queued'",
        [conversation],
    )
}

fn save_job(connection: &Connection, job: &SchedulerJob) -> Result<()> {
    let attempt = job.state.attempt();
    let detail = match &job.state {
        JobState::Failed { detail, .. } | JobState::Interrupted { detail, .. } => Some(detail),
        JobState::Cancelled { evidence, .. } => Some(evidence),
        _ => None,
    };
    connection.execute(
        &format!(
            "INSERT OR REPLACE INTO scheduler_jobs({JOB_COLUMNS}) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)"
        ),
        params![
            job.conversation_id,
            job.trigger,
            super::enum_text(&job.kind),
            job.sequence,
            serde_json::to_string(&job.basis)?,
            job.reply_to,
            job.message_id,
            job.state.name(),
            attempt.map(|a| &a.id),
            attempt.map(|a| &a.boot),
            attempt.map(|a| a.permission_version),
            detail,
        ],
    )?;
    Ok(())
}

const SESSION_COLUMNS: &str = "conversation_id,next_sequence,active,native_id,covers,owner_epoch,sdk_config,permission_version";

fn query_sessions(
    connection: &Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<SchedulerSession>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {SESSION_COLUMNS} FROM scheduler_sessions WHERE {filter} ORDER BY conversation_id"
    ))?;
    let rows = statement
        .query_map(params, |r| {
            Ok((
                SchedulerSession {
                    conversation_id: r.get(0)?,
                    next_sequence: r.get(1)?,
                    active: r.get(2)?,
                    native_id: r.get(3)?,
                    covers: VersionVector::new(),
                    owner_epoch: r.get(5)?,
                    sdk_config: r.get(6)?,
                    permission_version: r.get(7)?,
                },
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(session, covers)| {
            Ok(SchedulerSession {
                covers: serde_json::from_str(&covers)?,
                ..session
            })
        })
        .collect()
}

fn load_session(connection: &Connection, conversation: &str) -> Result<SchedulerSession> {
    Ok(
        query_sessions(connection, "conversation_id=?", [conversation])?
            .pop()
            .unwrap_or_else(|| SchedulerSession::new(conversation)),
    )
}

fn save_session(connection: &Connection, session: &SchedulerSession) -> Result<()> {
    connection.execute(
        &format!(
            "INSERT OR REPLACE INTO scheduler_sessions({SESSION_COLUMNS}) VALUES(?,?,?,?,?,?,?,?)"
        ),
        params![
            session.conversation_id,
            session.next_sequence,
            session.active,
            session.native_id,
            serde_json::to_string(&session.covers)?,
            session.owner_epoch,
            session.sdk_config,
            session.permission_version,
        ],
    )?;
    Ok(())
}

pub(crate) fn pending_checks_from(connection: &Connection) -> Result<Vec<PendingCheck>> {
    let mut statement = connection.prepare(
        "SELECT c.attempt_id,c.conversation_id,c.trigger,c.detail,c.created_at_ms,
            (SELECT json_group_array(process_group) FROM manager_processes p WHERE p.attempt_id=c.attempt_id)
         FROM pending_checks c ORDER BY c.created_at_ms",
    )?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get::<_, String>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(
            |(attempt_id, conversation_id, trigger, detail, created_at_ms, groups)| {
                Ok(PendingCheck {
                    check_id: PendingCheck::id(&attempt_id),
                    conversation_id,
                    trigger,
                    attempt_id,
                    detail,
                    process_groups: serde_json::from_str(&groups)?,
                    created_at_ms,
                })
            },
        )
        .collect()
}

/// The attempt's process groups stay recorded until its check is resolved.
fn save_pending_check(
    connection: &Connection,
    job: &SchedulerJob,
    attempt: &ManagerAttempt,
    detail: &str,
    at: i64,
) -> Result<()> {
    connection.execute(
        "INSERT OR REPLACE INTO pending_checks(attempt_id,conversation_id,trigger,detail,created_at_ms) VALUES(?,?,?,?,?)",
        params![attempt.id, job.conversation_id, job.trigger, detail, at],
    )?;
    Ok(())
}

fn forget_process(connection: &Connection, attempt_id: &str) -> Result<()> {
    connection.execute(
        "DELETE FROM manager_processes WHERE attempt_id=?",
        [attempt_id],
    )?;
    Ok(())
}

fn outbox_entry(
    connection: &Connection,
    conversation: &str,
    trigger: &str,
) -> Result<Option<OutboxEntry>> {
    Ok(connection
        .query_row(
            "SELECT conversation_id,trigger FROM scheduler_outbox WHERE conversation_id=? AND trigger=?",
            params![conversation, trigger],
            |r| {
                Ok(OutboxEntry {
                    conversation_id: r.get(0)?,
                    trigger: r.get(1)?,
                })
            },
        )
        .optional()?)
}

fn fence(session: &mut SchedulerSession) -> Result<()> {
    session.native_id = None;
    session.permission_version = session
        .permission_version
        .checked_add(1)
        .context("scheduler permission version overflow")?;
    Ok(())
}

fn reply_message_id(conversation: &str, trigger: &str) -> String {
    format!(
        "reply-{}",
        crate::crypto::digest(&serde_json::to_vec(&(conversation, trigger)).expect("strings"))
    )
}

/// Queues a trigger on the local owner. The caller has checked that this Node owns the conversation.
pub(crate) fn admit_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    trigger: &str,
    kind: TriggerKind,
    reply_to: &str,
) -> Result<AdmissionReceipt> {
    if let Some(job) = load_job(connection, conversation, trigger)? {
        return Ok(job.receipt(network));
    }
    let mut session = load_session(connection, conversation)?;
    ensure!(
        queued_jobs(connection, conversation)?.len() < MAX_PENDING_JOBS,
        "scheduler inbox is full; trigger was not admitted"
    );
    let job = SchedulerJob {
        conversation_id: conversation.into(),
        trigger: trigger.into(),
        kind,
        sequence: session.next_sequence,
        basis: log::version_vector_from(connection, network)?,
        reply_to: reply_to.into(),
        message_id: reply_message_id(conversation, trigger),
        state: JobState::Queued,
    };
    session.next_sequence = session
        .next_sequence
        .checked_add(1)
        .context("scheduler queue sequence overflow")?;
    save_job(connection, &job)?;
    save_session(connection, &session)?;
    Ok(job.receipt(network))
}

impl Store {
    /// Queues the turn in which `conversation`'s bot routes the session `question` is about,
    /// once. Only the Node that schedules the conversation admits it; returns whether it is new.
    pub fn admit_route(
        &mut self,
        local: &str,
        network: &str,
        conversation: &str,
        question: &str,
    ) -> Result<bool> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let trigger = route_trigger(question);
        let owner = conversation_from(&tx, network, conversation)?.and_then(|c| c.owner);
        if owner.as_deref() != Some(local) || load_job(&tx, conversation, &trigger)?.is_some() {
            return Ok(false);
        }
        admit_from(
            &tx,
            network,
            conversation,
            &trigger,
            TriggerKind::Route,
            question,
        )?;
        tx.commit()?;
        Ok(true)
    }
}

/// The enabled schedule a schedule job runs, if it still belongs to the job's conversation.
pub(crate) fn live_schedule(
    connection: &Connection,
    network: &str,
    job: &SchedulerJob,
) -> Result<Option<Schedule>> {
    let Some((schedule_id, _)) = parse_schedule_trigger(&job.trigger) else {
        return Ok(None);
    };
    Ok(
        super::bot_settings::schedule_from(connection, network, schedule_id)?
            .filter(|entry| entry.conversation_id == job.conversation_id && entry.schedule.enabled)
            .map(|entry| entry.schedule),
    )
}

/// How many automatic wakes led to the turn of `trigger`: none for the user's own message, an
/// answer or a routing turn; a schedule's own depth; a bot message's hops; and for a work, the
/// depth of the turn that dispatched it.
pub(crate) fn chain_depth(
    connection: &Connection,
    network: &str,
    conversation: &str,
    trigger: &str,
) -> Result<u8> {
    let (mut conversation, mut trigger) = (conversation.to_owned(), trigger.to_owned());
    for _ in 0..64 {
        match trigger.split_once(':') {
            Some(("message" | "route", _)) => return Ok(0),
            Some(("schedule", _)) => {
                let (schedule_id, _) =
                    parse_schedule_trigger(&trigger).context("invalid schedule trigger")?;
                return Ok(
                    super::bot_settings::schedule_from(connection, network, schedule_id)?
                        .map_or(MAX_CHAIN_DEPTH, |entry| entry.schedule.depth),
                );
            }
            Some(("bot_message", message_id)) => {
                let message = message_from(connection, network, &conversation, message_id)?
                    .context("the bot message is not stored here")?;
                let event = log::event_from(connection, network, &message.event_hash)?
                    .context("the bot message's event is not stored here")?;
                let EventBody::BotMessaged { hops, .. } = event.body else {
                    bail!("{message_id} is not a message from another bot");
                };
                return Ok(hops);
            }
            Some(("work" | "task", work_id)) => {
                let work = super::reduce::work_from(connection, network, work_id)?
                    .context("the work is not stored here")?;
                (conversation, trigger) = (work.conversation_id, work.trigger);
            }
            _ => bail!("unknown trigger {trigger}"),
        }
    }
    Ok(MAX_CHAIN_DEPTH)
}

/// A time this long before a schedule's last change still runs, so "starting now" runs now;
/// anything older waits for the next time, so resuming never runs a missed one.
const LATE_START_MS: i64 = 5 * 60_000;

/// A due time found later than this (the device was off or asleep) is skipped, not run late.
pub(crate) const MAX_LATE_MS: i64 = 10 * 60_000;

/// Queues the latest due time of each schedule in conversations this Node schedules. Earlier
/// missed times are never run, a time found more than `MAX_LATE_MS` late is recorded as run and
/// skipped, a time any owner already answered is skipped, and a schedule waits while its
/// previous run is still queued. Returns `(conversation, trigger)` per job.
fn admit_due_schedules_from(
    connection: &Connection,
    network: &str,
    local: &str,
    now: i64,
) -> Result<Vec<(String, String)>> {
    let mut admitted = Vec::new();
    for entry in super::bot_settings::schedules_from(connection, network, None)? {
        let conversation = entry.conversation_id.as_str();
        let Some(slot) = latest_slot(&entry.schedule, now)
            .filter(|slot| entry.schedule.enabled && *slot >= entry.changed_at_ms - LATE_START_MS)
        else {
            continue;
        };
        let owner = conversation_from(connection, network, conversation)?.and_then(|c| c.owner);
        let fired: Option<i64> = connection
            .query_row(
                "SELECT slot_ms FROM schedule_runs WHERE conversation_id=? AND schedule_id=?",
                params![conversation, entry.schedule_id],
                |r| r.get(0),
            )
            .optional()?;
        if owner.as_deref() != Some(local) || fired.is_some_and(|fired| fired >= slot) {
            continue;
        }
        if now - slot > MAX_LATE_MS {
            connection.execute(
                "INSERT OR REPLACE INTO schedule_runs(conversation_id,schedule_id,slot_ms) VALUES(?,?,?)",
                params![conversation, entry.schedule_id, slot],
            )?;
            tracing::info!(
                event = "schedule.skipped_late",
                conversation_id = conversation,
                schedule_id = entry.schedule_id,
                slot_ms = slot,
                late_ms = now - slot,
                "a schedule time was missed and is skipped"
            );
            continue;
        }
        let queued = queued_jobs(connection, conversation)?;
        if queued.len() >= MAX_PENDING_JOBS
            || queued.iter().any(|job| {
                parse_schedule_trigger(&job.trigger).is_some_and(|(id, _)| id == entry.schedule_id)
            })
        {
            continue;
        }
        connection.execute(
            "INSERT OR REPLACE INTO schedule_runs(conversation_id,schedule_id,slot_ms) VALUES(?,?,?)",
            params![conversation, entry.schedule_id, slot],
        )?;
        let trigger = schedule_trigger(&entry.schedule_id, slot);
        // Another owner already ran this time before the conversation moved here.
        if message_from(
            connection,
            network,
            conversation,
            &reply_message_id(conversation, &trigger),
        )?
        .is_some()
        {
            continue;
        }
        admit_from(
            connection,
            network,
            conversation,
            &trigger,
            TriggerKind::Schedule,
            &trigger,
        )?;
        admitted.push((conversation.to_owned(), trigger));
    }
    Ok(admitted)
}

/// Keeps a trigger for the conversation's owner until it returns a receipt.
pub(crate) fn queue_delivery_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    trigger: &str,
) -> Result<AdmissionReceipt> {
    connection.execute(
        "INSERT OR IGNORE INTO scheduler_outbox(conversation_id,trigger) VALUES(?,?)",
        params![conversation, trigger],
    )?;
    Ok(OutboxEntry {
        conversation_id: conversation.into(),
        trigger: trigger.into(),
    }
    .receipt(network))
}

/// The user message a work ultimately answers, followed through the DAG's dispatch chain, so
/// any owner can report a result whose turn ran on another Node.
fn origin_message(connection: &Connection, network: &str, work_id: &str) -> Result<Option<String>> {
    let mut work_id = work_id.to_owned();
    for _ in 0..64 {
        let Some(work) = super::reduce::work_from(connection, network, &work_id)? else {
            return Ok(None);
        };
        match work.trigger.split_once(':') {
            Some(("message" | "bot_message", message)) => return Ok(Some(message.into())),
            Some(("work" | "task", parent)) => work_id = parent.into(),
            // A schedule run has no message; its result answers the run.
            Some(("schedule", _)) => return Ok(Some(work.trigger)),
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// A finished work reports back to the owner through its own trigger, unless the user opted out
/// on the message that started the chain. Unverified results wait for the user or a probe.
/// Another Node's result waits in the outbox for the owner.
pub(crate) fn admit_work_result_from(
    connection: &Connection,
    network: &str,
    local: &str,
    conversation: &str,
    work_id: &str,
    outcome: WorkOutcome,
) -> Result<Option<AdmissionReceipt>> {
    if outcome == WorkOutcome::Unverified {
        return Ok(None);
    }
    let trigger = work_trigger(work_id);
    let owner = conversation_from(connection, network, conversation)?.and_then(|c| c.owner);
    if owner.as_deref() != Some(local) {
        return queue_delivery_from(connection, network, conversation, &trigger).map(Some);
    }
    let Some(reply_to) = origin_message(connection, network, work_id)? else {
        return Ok(None);
    };
    let report = message_from(connection, network, conversation, &reply_to)?
        .and_then(|message| message.report_results)
        .unwrap_or(true);
    if !report {
        return Ok(None);
    }
    admit_from(
        connection,
        network,
        conversation,
        &trigger,
        TriggerKind::WorkResult,
        &reply_to,
    )
    .map(Some)
}

/// Provider errors as the person reads them in the chat, each with where to go next. The raw
/// English cause stays on the local job and in node.log; the reply is synced to every device.
const PROVIDER_FAILURES: &[(&[&str], &str)] = &[
    (
        &[
            "api_key is required",
            "api_key is missing",
            "missing environment variable",
        ],
        "AI 服务缺少密钥或密钥无效。去「设备 → AI」填好后再发一次。",
    ),
    (
        &["execution is turned off"],
        "这台设备的 Codex 没开执行任务。去「设备 → AI」打开后再发一次。",
    ),
    (
        &[
            "executable was not found",
            "command not found",
            "no cursor sdk bridge",
            "sidecar is missing",
            "enoent",
        ],
        "这台设备上找不到 AI 服务的程序。去「设备 → AI」检查后再发一次。",
    ),
    (
        &["usage limit", "rate limit", "quota", "too many requests"],
        "AI 服务的用量到上限了。等额度恢复后再发一次。",
    ),
    (
        &[
            "unauthorized",
            "authentication failed",
            "not logged in",
            "invalid api key",
        ],
        "AI 服务登录没通过。去「设备 → AI」检查密钥后再发一次。",
    ),
];

fn plain_failure(detail: &str) -> Option<&'static str> {
    let detail = detail.to_lowercase();
    PROVIDER_FAILURES
        .iter()
        .find(|(needles, _)| needles.iter().any(|needle| detail.contains(needle)))
        .map(|(_, text)| *text)
}

fn reply_status(outcome: &JobOutcome) -> (ReplyStatus, String) {
    let chinese = |detail: &str| {
        detail
            .chars()
            .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
    };
    match outcome {
        JobOutcome::Completed => (ReplyStatus::Completed, "（没有回复内容）".into()),
        JobOutcome::Failed(detail) => (
            ReplyStatus::Failed,
            match plain_failure(detail) {
                Some(text) => text.into(),
                None if chinese(detail) => format!("这次没能回复：{detail}"),
                None => "这次没能回复。再发一次试试；一直不行就去「设备 → AI」看看。".into(),
            },
        ),
        JobOutcome::Cancelled(_) => (ReplyStatus::Stopped, "已停止回复。".into()),
        JobOutcome::Interrupted { detail, .. } => (
            ReplyStatus::Interrupted,
            match plain_failure(detail) {
                Some(text) => text.into(),
                None if chinese(detail) => format!("回复被中断：{detail}"),
                None => "回复中途断开，结果未知。先看看有没有改动，再决定要不要重发。".into(),
            },
        ),
    }
}

/// Ends the job and writes its bot reply in one transaction. Partial text of a stopped or
/// interrupted turn is kept with its status.
#[expect(
    clippy::too_many_arguments,
    reason = "the reply and the job end together"
)]
fn end_job_from(
    connection: &Connection,
    blob_dir: &Path,
    identity: &LocalIdentity,
    network: &str,
    session: &mut SchedulerSession,
    mut job: SchedulerJob,
    outcome: JobOutcome,
    text: &str,
    at: i64,
) -> Result<SchedulerJob> {
    let attempt = job
        .state
        .attempt()
        .context("scheduler attempt missing")?
        .clone();
    let attempt_id = attempt.id.clone();
    let (status, fallback) = reply_status(&outcome);
    let text = if text.trim().is_empty() {
        fallback
    } else {
        text.to_owned()
    };
    if job.kind == TriggerKind::AgentTask {
        super::work::finish_agent_work_from(
            connection,
            blob_dir,
            identity,
            network,
            &job.reply_to,
            &outcome,
            &text,
            at,
        )?;
    } else if message_from(connection, network, &job.conversation_id, &job.message_id)?.is_none() {
        log::append_from(
            connection,
            blob_dir,
            identity,
            network,
            &job.conversation_id,
            EventBody::MessagePosted {
                message_id: job.message_id.clone(),
                role: Role::Assistant,
                text,
                attachments: vec![],
                report_results: None,
                in_reply_to: Some(job.reply_to.clone()),
                status: Some(status),
                answers: None,
                attestation: None,
            },
            vec![],
            at,
        )?;
        let context = super::since_checkpoint_from(connection, network, &job.conversation_id)?;
        let works = super::reduce::works_from(connection, network, &job.conversation_id)?;
        if let Some(checkpoint) = crate::scheduler::checkpoint(&context, &works) {
            log::append_from(
                connection,
                blob_dir,
                identity,
                network,
                &job.conversation_id,
                checkpoint,
                vec![],
                at,
            )?;
        }
    }
    job.state = match outcome {
        JobOutcome::Completed => JobState::Completed { attempt },
        JobOutcome::Failed(detail) => JobState::Failed { attempt, detail },
        JobOutcome::Cancelled(evidence) => JobState::Cancelled {
            attempt: Some(attempt),
            evidence,
        },
        JobOutcome::Interrupted {
            detail,
            process_unconfirmed,
        } => {
            fence(session)?;
            if process_unconfirmed {
                save_pending_check(connection, &job, &attempt, &detail, at)?;
            }
            JobState::Interrupted { attempt, detail }
        }
    };
    if session.active.as_deref() == Some(job.trigger.as_str()) {
        session.active = None;
    }
    if !matches!(&job.state, JobState::Interrupted { .. })
        || pending_checks_from(connection)?
            .iter()
            .all(|check| check.attempt_id != attempt_id)
    {
        forget_process(connection, &attempt_id)?;
    }
    save_job(connection, &job)?;
    save_session(connection, session)?;
    Ok(job)
}

fn recover_session(
    connection: &Connection,
    blob_dir: &Path,
    identity: &LocalIdentity,
    network: &str,
    session: &mut SchedulerSession,
    boot: &str,
) -> Result<()> {
    let Some(trigger) = session.active.clone() else {
        return Ok(());
    };
    let mut job = load_job(connection, &session.conversation_id, &trigger)?
        .context("missing active scheduler job")?;
    let Some(attempt) = job.state.attempt().cloned() else {
        bail!("active scheduler job has no attempt");
    };
    if attempt.boot == boot {
        return Ok(());
    }
    match job.state {
        JobState::Claimed { .. } => {
            job.state = JobState::Queued;
            session.active = None;
            save_job(connection, &job)?;
        }
        JobState::Running { .. } | JobState::Cancelling { .. } => {
            end_job_from(
                connection,
                blob_dir,
                identity,
                network,
                session,
                job,
                JobOutcome::Interrupted {
                    detail: "设备重启了，没有自动重做".into(),
                    process_unconfirmed: true,
                },
                "",
                crate::runtime::now_ms(),
            )?;
        }
        _ => session.active = None,
    }
    Ok(())
}

impl Store {
    /// Stores the user's message once per `message_id` and queues it on the owner.
    #[expect(
        clippy::too_many_arguments,
        reason = "a user message and its admission"
    )]
    pub fn accept_message(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        conversation: &str,
        message_id: &str,
        text: &str,
        blobs: Vec<BlobRef>,
        report_results: bool,
        attestation: Attestation,
        created_at_ms: i64,
    ) -> Result<(Message, AdmissionReceipt)> {
        let local = identity.device_id()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize_node_from(&tx, network, &local)?;
        let owner = conversation_from(&tx, network, conversation)?
            .context("conversation not found")?
            .owner
            .context("conversation has no scheduling Node")?;
        let mut blobs = blobs;
        blobs.sort();
        blobs.dedup();
        if let Some(existing) = message_from(&tx, network, conversation, message_id)? {
            ensure!(
                existing.role == Role::User
                    && existing.answers.is_none()
                    && existing.text == text
                    && existing.blobs == blobs
                    && existing.report_results == Some(report_results),
                "message ID reused with different content"
            );
        } else {
            log::append_from(
                &tx,
                &self.blob_dir,
                identity,
                network,
                conversation,
                EventBody::MessagePosted {
                    message_id: message_id.into(),
                    role: Role::User,
                    text: text.into(),
                    attachments: blobs.clone(),
                    report_results: Some(report_results),
                    in_reply_to: None,
                    status: None,
                    answers: None,
                    attestation: Some(attestation),
                },
                blobs,
                created_at_ms,
            )?;
        }
        let trigger = message_trigger(message_id);
        let receipt = if owner == local {
            admit_from(
                &tx,
                network,
                conversation,
                &trigger,
                TriggerKind::Human,
                message_id,
            )?
        } else {
            queue_delivery_from(&tx, network, conversation, &trigger)?
        };
        let message =
            message_from(&tx, network, conversation, message_id)?.context("message missing")?;
        tx.commit()?;
        Ok((message, receipt))
    }

    pub fn admit_due_schedules(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        now: i64,
    ) -> Result<Vec<(String, String)>> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let admitted = admit_due_schedules_from(&tx, network, &identity.device_id()?, now)?;
        tx.commit()?;
        Ok(admitted)
    }

    pub fn scheduler_job(&self, conversation: &str, trigger: &str) -> Result<Option<SchedulerJob>> {
        load_job(&self.connection, conversation, trigger)
    }

    pub fn scheduler_status(
        &self,
        network: &str,
        conversation: &str,
        trigger: &str,
    ) -> Result<Option<AdmissionReceipt>> {
        if let Some(job) = self.scheduler_job(conversation, trigger)? {
            return Ok(Some(job.receipt(network)));
        }
        Ok(outbox_entry(&self.connection, conversation, trigger)?
            .map(|entry| entry.receipt(network)))
    }

    pub fn scheduler_jobs(&self, conversation: &str) -> Result<Vec<SchedulerJob>> {
        query_jobs(&self.connection, "conversation_id=?", [conversation])
    }

    pub fn scheduler_outbox(&self) -> Result<Vec<OutboxEntry>> {
        let mut statement = self.connection.prepare(
            "SELECT conversation_id,trigger FROM scheduler_outbox ORDER BY conversation_id,trigger",
        )?;
        Ok(statement
            .query_map([], |r| {
                Ok(OutboxEntry {
                    conversation_id: r.get(0)?,
                    trigger: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn remove_outbox(&mut self, conversation: &str, trigger: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM scheduler_outbox WHERE conversation_id=? AND trigger=?",
            params![conversation, trigger],
        )?;
        Ok(())
    }

    pub fn pending_scheduler_conversations(&self) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT conversation_id FROM scheduler_jobs WHERE state='queued' ORDER BY conversation_id",
        )?;
        Ok(statement
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn recover_scheduler_jobs(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        boot: &str,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for mut session in query_sessions(&tx, "active IS NOT NULL", [])? {
            recover_session(&tx, &self.blob_dir, identity, network, &mut session, boot)?;
            save_session(&tx, &session)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Human messages go first.
    pub fn claim_next(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        conversation: &str,
        boot: &str,
        sdk_config: &str,
    ) -> Result<Option<ClaimedJob>> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = load_session(&tx, conversation)?;
        recover_session(&tx, &self.blob_dir, identity, network, &mut session, boot)?;
        if session.active.is_some() {
            save_session(&tx, &session)?;
            tx.commit()?;
            return Ok(None);
        }
        let current = conversation_from(&tx, network, conversation)?;
        let owner_epoch = current.as_ref().map_or(0, |c| c.owner_epoch);
        let owned = current.and_then(|c| c.owner) == Some(identity.device_id()?);
        let mut queued = Vec::new();
        for job in queued_jobs(&tx, conversation)? {
            // The user moved the conversation: what never started goes to the new owner, which
            // runs schedules by its own clock.
            if !owned && job.kind != TriggerKind::AgentTask {
                tx.execute(
                    "DELETE FROM scheduler_jobs WHERE conversation_id=? AND trigger=?",
                    params![conversation, job.trigger],
                )?;
                if job.kind != TriggerKind::Schedule {
                    queue_delivery_from(&tx, network, conversation, &job.trigger)?;
                }
                continue;
            }
            queued.push(job);
        }
        queued.sort_by_key(|job| {
            (
                !matches!(job.kind, TriggerKind::Human | TriggerKind::Answer),
                job.sequence,
            )
        });
        let mut selected = None;
        for mut job in queued {
            let evidence = match job.kind {
                TriggerKind::Schedule if live_schedule(&tx, network, &job)?.is_none() => {
                    Some("定时任务已暂停或删除")
                }
                TriggerKind::AgentTask if super::work::run_finished(&tx, &job.reply_to)? => {
                    Some("任务结果已由你确认")
                }
                _ => None,
            };
            if let Some(evidence) = evidence {
                job.state = JobState::Cancelled {
                    attempt: None,
                    evidence: evidence.into(),
                };
                save_job(&tx, &job)?;
                continue;
            }
            selected = Some(job);
            break;
        }
        let Some(mut job) = selected else {
            save_session(&tx, &session)?;
            tx.commit()?;
            return Ok(None);
        };
        if session.sdk_config.as_deref() != Some(sdk_config) || session.owner_epoch != owner_epoch {
            session.native_id = None;
            session.covers.clear();
            session.sdk_config = Some(sdk_config.into());
            session.owner_epoch = owner_epoch;
        }
        // A queued turn reads what came before it ran, such as the reply to the message ahead.
        job.basis = log::version_vector_from(&tx, network)?;
        // Reused only under the same ownership and when this basis contains what it saw.
        let resumable = job.kind != TriggerKind::AgentTask
            && session.native_id.is_some()
            && session
                .covers
                .iter()
                .all(|(author, seq)| job.basis.get(author).is_some_and(|have| have >= seq));
        let previous_native_id = resumable.then(|| session.native_id.clone()).flatten();
        let covers = resumable.then(|| session.covers.clone());
        job.state = JobState::Claimed {
            attempt: ManagerAttempt {
                id: uuid::Uuid::new_v4().to_string(),
                boot: boot.into(),
                permission_version: session.permission_version,
            },
        };
        session.active = Some(job.trigger.clone());
        save_job(&tx, &job)?;
        save_session(&tx, &session)?;
        tx.commit()?;
        Ok(Some(ClaimedJob {
            job,
            previous_native_id,
            covers,
        }))
    }

    fn update_attempt(
        &mut self,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
        update: impl FnOnce(&mut SchedulerJob, &mut SchedulerSession) -> Result<()>,
    ) -> Result<SchedulerJob> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = load_session(&tx, conversation)?;
        let mut job = load_job(&tx, conversation, trigger)?.context("scheduler job missing")?;
        ensure!(
            job.state
                .attempt()
                .is_some_and(|attempt| attempt.id == attempt_id),
            "scheduler attempt does not match"
        );
        update(&mut job, &mut session)?;
        save_job(&tx, &job)?;
        save_session(&tx, &session)?;
        tx.commit()?;
        Ok(job)
    }

    pub fn mark_started(
        &mut self,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
    ) -> Result<SchedulerJob> {
        self.update_attempt(conversation, trigger, attempt_id, |job, _| {
            let JobState::Claimed { attempt } = &job.state else {
                bail!("scheduler job is not claimed");
            };
            job.state = JobState::Running {
                attempt: attempt.clone(),
            };
            Ok(())
        })
    }

    pub fn save_scheduler_native_id(
        &mut self,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
        native_id: &str,
    ) -> Result<()> {
        self.update_attempt(conversation, trigger, attempt_id, |job, session| {
            ensure!(
                matches!(
                    job.state,
                    JobState::Running { .. } | JobState::Claimed { .. }
                ),
                "scheduler attempt is no longer active"
            );
            job.state.attempt().context("scheduler attempt missing")?;
            if job.kind != TriggerKind::AgentTask {
                session.native_id = Some(native_id.into());
                session.covers = job.basis.clone();
            }
            Ok(())
        })?;
        Ok(())
    }

    pub fn request_manager_cancel(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        conversation: &str,
        trigger: &str,
    ) -> Result<Option<SchedulerJob>> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = load_session(&tx, conversation)?;
        let Some(mut job) = load_job(&tx, conversation, trigger)? else {
            return Ok(None);
        };
        if matches!(job.state, JobState::Queued) {
            let evidence = "用户在开始前取消".to_owned();
            if job.kind == TriggerKind::AgentTask {
                super::work::finish_agent_work_from(
                    &tx,
                    &self.blob_dir,
                    identity,
                    network,
                    &job.reply_to,
                    &JobOutcome::Cancelled(evidence.clone()),
                    "",
                    crate::runtime::now_ms(),
                )?;
            }
            job.state = JobState::Cancelled {
                attempt: None,
                evidence,
            };
        } else if let JobState::Claimed { attempt } | JobState::Running { attempt } = &job.state {
            ensure!(
                session.active.as_deref() == Some(trigger),
                "management reply changed; refresh before stopping"
            );
            job.state = JobState::Cancelling {
                attempt: attempt.clone(),
            };
            fence(&mut session)?;
        }
        save_job(&tx, &job)?;
        save_session(&tx, &session)?;
        tx.commit()?;
        Ok(Some(job))
    }

    pub(crate) fn fence_manager_permission(
        &mut self,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
    ) -> Result<()> {
        self.update_attempt(conversation, trigger, attempt_id, |job, session| {
            let attempt = job.state.attempt().context("manager attempt missing")?;
            if attempt.permission_version == session.permission_version {
                session.permission_version = session
                    .permission_version
                    .checked_add(1)
                    .context("scheduler permission version overflow")?;
            }
            Ok(())
        })?;
        Ok(())
    }

    pub fn manager_attempt_allowed(
        &self,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
    ) -> Result<bool> {
        let session = load_session(&self.connection, conversation)?;
        let Some(job) = self.scheduler_job(conversation, trigger)? else {
            return Ok(false);
        };
        let Some(attempt) = job.state.attempt() else {
            return Ok(false);
        };
        Ok(session.active.as_deref() == Some(trigger)
            && attempt.id == attempt_id
            && attempt.permission_version == session.permission_version
            && matches!(
                job.state,
                JobState::Claimed { .. } | JobState::Running { .. }
            ))
    }

    /// Idempotent: a job that already ended returns as is.
    #[expect(
        clippy::too_many_arguments,
        reason = "the attempt identity and its reply"
    )]
    pub fn finish_job(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
        outcome: JobOutcome,
        text: &str,
        created_at_ms: i64,
    ) -> Result<SchedulerJob> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = load_session(&tx, conversation)?;
        let job = load_job(&tx, conversation, trigger)?.context("scheduler job missing")?;
        ensure!(
            job.state
                .attempt()
                .is_some_and(|attempt| attempt.id == attempt_id),
            "scheduler attempt does not match"
        );
        if job.state.terminal() {
            return Ok(job);
        }
        ensure!(
            session.active.as_deref() == Some(trigger),
            "scheduler attempt is no longer active"
        );
        let job = end_job_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            &mut session,
            job,
            outcome,
            text,
            created_at_ms,
        )?;
        tx.commit()?;
        Ok(job)
    }

    pub fn record_manager_process(&mut self, attempt_id: &str, process_group: u32) -> Result<()> {
        self.connection.execute(
            "INSERT OR IGNORE INTO manager_processes(attempt_id,process_group) VALUES(?,?)",
            params![attempt_id, process_group],
        )?;
        Ok(())
    }

    pub fn pending_checks(&self) -> Result<Vec<PendingCheck>> {
        pending_checks_from(&self.connection)
    }

    pub fn resolve_pending_check(&mut self, attempt_id: &str) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM pending_checks WHERE attempt_id=?",
            [attempt_id],
        )?;
        forget_process(&tx, attempt_id)?;
        tx.commit()?;
        Ok(())
    }

    pub fn scheduler_maintenance_idle(&self) -> Result<()> {
        let busy: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduler_jobs WHERE state='queued')
                OR EXISTS(SELECT 1 FROM scheduler_sessions WHERE active IS NOT NULL)",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            !busy,
            crate::maintenance::Busy(
                "scheduler invocation requires completion or reconciliation".into()
            )
        );
        ensure!(
            self.pending_checks()?.is_empty(),
            crate::maintenance::Busy("an interrupted invocation awaits reconciliation".into())
        );
        ensure!(
            self.work_records()?
                .iter()
                .all(|work| work.state.finished()),
            crate::maintenance::Busy("a dispatched work has not finished".into())
        );
        Ok(())
    }
}
