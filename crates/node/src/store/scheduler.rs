//! The owner's local scheduling state. A job is keyed by its trigger, and its reply is written
//! in the same transaction that ends it, so nothing is published twice or lost.
use super::{Store, authorize_node_from, conversation_from, log, message_from};
use crate::{crypto::LocalIdentity, model::*, providers::Provider, scheduling::*};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::path::Path;
use vibeface_bridge::failure::{ExecFailure, ProviderStage};

const JOB_COLUMNS: &str = "conversation_id,trigger,kind,sequence,basis,reply_to,message_id,state,attempt_id,attempt_boot,attempt_permission,detail,failure,runtime,tool_calls";

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
    Option<String>,
    Option<String>,
    bool,
    bool,
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
        failure,
        runtime,
        tool_calls,
        pending_check,
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
    let failure = || -> Result<ExecFailure> {
        Ok(ExecFailure {
            kind: super::enum_from(
                failure
                    .as_deref()
                    .context("scheduler job failure missing")?,
            )?,
            detail: detail.clone(),
        })
    };
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
        "reconciling" => JobState::Reconciling {
            attempt: attempt_or()?,
            failure: failure()?,
        },
        "completed" => JobState::Completed {
            attempt: attempt_or()?,
        },
        "failed" => JobState::Failed {
            attempt: attempt_or()?,
            failure: failure()?,
        },
        "cancelled" => JobState::Cancelled {
            attempt,
            evidence: detail,
        },
        "interrupted" => JobState::Interrupted {
            attempt: attempt_or()?,
            failure: failure()?,
        },
        "steering" | "steered" => {
            let steer: JobState = serde_json::from_str(&detail)?;
            ensure!(steer.name() == state, "scheduler job steer state mismatch");
            steer
        }
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
        runtime: runtime
            .map(|value| serde_json::from_str(&value))
            .transpose()?,
        tool_calls,
        pending_check,
        state,
    })
}

fn query_jobs(
    connection: &Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<SchedulerJob>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {JOB_COLUMNS},EXISTS(SELECT 1 FROM pending_checks p WHERE p.attempt_id=scheduler_jobs.attempt_id) FROM scheduler_jobs WHERE {filter} ORDER BY sequence"
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
                r.get(14)?,
                r.get(15)?,
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

fn queued_jobs(connection: &Connection, conversation: &str) -> Result<Vec<SchedulerJob>> {
    query_jobs(
        connection,
        "conversation_id=? AND state='queued'",
        [conversation],
    )
}

fn save_job(connection: &Connection, job: &SchedulerJob) -> Result<()> {
    let attempt = job.state.attempt();
    let failure = match &job.state {
        JobState::Failed { failure, .. }
        | JobState::Interrupted { failure, .. }
        | JobState::Reconciling { failure, .. } => Some(failure),
        _ => None,
    };
    let detail = match &job.state {
        JobState::Failed { failure, .. }
        | JobState::Interrupted { failure, .. }
        | JobState::Reconciling { failure, .. } => Some(failure.detail.clone()),
        JobState::Cancelled { evidence, .. } => Some(evidence.clone()),
        JobState::Steering { .. } | JobState::Steered { .. } => {
            Some(serde_json::to_string(&job.state)?)
        }
        _ => None,
    };
    connection.execute(
        &format!(
            "INSERT INTO scheduler_jobs({JOB_COLUMNS}) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(conversation_id,trigger) DO UPDATE SET
             kind=excluded.kind,sequence=excluded.sequence,basis=excluded.basis,
             reply_to=excluded.reply_to,message_id=excluded.message_id,state=excluded.state,
             attempt_id=excluded.attempt_id,attempt_boot=excluded.attempt_boot,
             attempt_permission=excluded.attempt_permission,detail=excluded.detail,
             failure=excluded.failure,runtime=excluded.runtime,tool_calls=excluded.tool_calls"
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
            failure.map(|f| super::enum_text(&f.kind)),
            job.runtime
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            job.tool_calls,
        ],
    )?;
    Ok(())
}

const SESSION_COLUMNS: &str = "conversation_id,next_sequence,active,native_id,covers,owner_epoch,sdk_config,permission_version,native_token,shown,session_generation,handoff_revision,handoff_source";

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
                    native_token: r.get(8)?,
                    covers: VersionVector::new(),
                    owner_epoch: r.get(5)?,
                    sdk_config: r.get(6)?,
                    permission_version: r.get(7)?,
                    shown: Default::default(),
                    session_generation: r.get(10)?,
                    handoff_revision: r.get(11)?,
                    handoff_source: VersionVector::new(),
                },
                r.get::<_, String>(4)?,
                r.get::<_, String>(9)?,
                r.get::<_, String>(12)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(session, covers, shown, handoff_source)| {
            Ok(SchedulerSession {
                covers: serde_json::from_str(&covers)?,
                shown: serde_json::from_str(&shown)?,
                handoff_source: serde_json::from_str(&handoff_source)?,
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
            "INSERT OR REPLACE INTO scheduler_sessions({SESSION_COLUMNS}) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)"
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
            session.native_token,
            serde_json::to_string(&session.shown)?,
            session.session_generation,
            session.handoff_revision,
            serde_json::to_string(&session.handoff_source)?,
        ],
    )?;
    Ok(())
}

pub(crate) fn pending_checks_from(connection: &Connection) -> Result<Vec<PendingCheck>> {
    let mut statement = connection.prepare(
        "SELECT c.attempt_id,c.conversation_id,c.trigger,c.detail,c.created_at_ms,
            (SELECT json_group_array(process_group) FROM manager_processes p WHERE p.attempt_id=c.attempt_id),
            t.server,t.thread_id,t.turn_id
         FROM pending_checks c LEFT JOIN manager_turns t ON t.attempt_id=c.attempt_id
         ORDER BY c.created_at_ms",
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
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(
            |(
                attempt_id,
                conversation_id,
                trigger,
                detail,
                created_at_ms,
                groups,
                server,
                thread,
                turn,
            )| {
                Ok(PendingCheck {
                    check_id: PendingCheck::id(&attempt_id),
                    conversation_id,
                    trigger,
                    attempt_id,
                    detail,
                    process_groups: serde_json::from_str(&groups)?,
                    codex_turn: server.zip(thread).map(|(server, thread)| ReplyTurn {
                        server,
                        thread,
                        turn,
                    }),
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
    connection.execute("DELETE FROM manager_turns WHERE attempt_id=?", [attempt_id])?;
    Ok(())
}

fn manager_turn(connection: &Connection, attempt_id: &str) -> Result<Option<(String, String)>> {
    Ok(connection
        .query_row(
            "SELECT thread_id,turn_id FROM manager_turns WHERE attempt_id=? AND turn_id IS NOT NULL",
            [attempt_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

fn advance_lineage(session: &mut SchedulerSession, source: &VersionVector) -> Result<()> {
    session.session_generation = session
        .session_generation
        .checked_add(1)
        .context("scheduler session generation overflow")?;
    session.handoff_revision = session
        .handoff_revision
        .checked_add(1)
        .context("scheduler handoff revision overflow")?;
    session.handoff_source = source.clone();
    session.native_id = None;
    session.native_token = None;
    session.covers.clear();
    session.shown = Shown::default();
    Ok(())
}

fn fence(session: &mut SchedulerSession) -> Result<()> {
    advance_lineage(session, &session.covers.clone())?;
    session.permission_version = session
        .permission_version
        .checked_add(1)
        .context("scheduler permission version overflow")?;
    Ok(())
}

const BOT_DELETED: &str = "bot 已删除";
const DECLINED: &str = "用户选择不回复";

/// A turn runs only under the ownership it was claimed in: this Node at the session's owner
/// epoch. A move away and back raises the epoch, so the old turn stays fenced. An agent task
/// runs on its work's device, whoever owns the conversation.
fn holds_turn(
    connection: &Connection,
    network: &str,
    local: &str,
    session: &SchedulerSession,
    job: &SchedulerJob,
) -> Result<bool> {
    if job.kind == TriggerKind::AgentTask {
        return Ok(true);
    }
    Ok(
        conversation_from(connection, network, &session.conversation_id)?.is_some_and(|c| {
            c.owner.as_deref() == Some(local) && c.owner_epoch == session.owner_epoch
        }),
    )
}

/// The turn a user message may join: the session's running reply to a user message, started by
/// this process and still held by this Node, on a turn of `provider`.
fn steer_target(
    connection: &Connection,
    network: &str,
    local: &str,
    boot: &str,
    session: &SchedulerSession,
    provider: Provider,
) -> Result<Option<SteerTarget>> {
    let Some(trigger) = &session.active else {
        return Ok(None);
    };
    let Some(job) = load_job(connection, &session.conversation_id, trigger)? else {
        return Ok(None);
    };
    let JobState::Running { attempt } = &job.state else {
        return Ok(None);
    };
    if job.kind != TriggerKind::Human
        || attempt.boot != boot
        || attempt.permission_version != session.permission_version
        || !holds_turn(connection, network, local, session, &job)?
    {
        return Ok(None);
    }
    Ok(
        manager_turn(connection, &attempt.id)?.map(|(thread_id, turn_id)| SteerTarget {
            provider,
            attempt_id: attempt.id.clone(),
            thread_id,
            turn_id,
        }),
    )
}

/// The text of a queued user message that can join a running turn as is: no attachments, and
/// results reported as usual.
fn steerable_text(
    connection: &Connection,
    network: &str,
    job: &SchedulerJob,
) -> Result<Option<String>> {
    if job.kind != TriggerKind::Human {
        return Ok(None);
    }
    Ok(
        message_from(connection, network, &job.conversation_id, &job.reply_to)?
            .filter(|message| {
                message.attachments.is_empty() && message.report_results != Some(false)
            })
            .map(|message| message.text),
    )
}

fn pending_conversations_from(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT DISTINCT conversation_id FROM scheduler_jobs WHERE state='queued' ORDER BY conversation_id",
    )?;
    Ok(statement
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// The user moved the conversation: what never started here goes to the new owner, which runs
/// schedules by its own clock. An agent task stays with its work's device.
fn hand_off_from(connection: &Connection, network: &str, conversation: &str) -> Result<()> {
    for job in queued_jobs(connection, conversation)? {
        if job.kind == TriggerKind::AgentTask {
            continue;
        }
        connection.execute(
            "DELETE FROM scheduler_jobs WHERE conversation_id=? AND trigger=?",
            params![conversation, job.trigger],
        )?;
        if job.kind != TriggerKind::Schedule {
            queue_delivery_from(connection, network, conversation, &job.trigger)?;
        }
    }
    Ok(())
}

/// A new owner answering a message the old one left unanswered writes its own reply, beside any
/// status reply the old owner already wrote.
pub(super) fn reply_message_id(conversation: &str, trigger: &str, owner_epoch: u64) -> String {
    format!(
        "reply-{}",
        crate::crypto::digest(
            &serde_json::to_vec(&(conversation, trigger, owner_epoch)).expect("strings")
        )
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
    admit_ended_from(
        connection,
        network,
        conversation,
        trigger,
        kind,
        reply_to,
        None,
    )
}

/// `declined` records the trigger as already cancelled with that evidence, so it never runs here.
fn admit_ended_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    trigger: &str,
    kind: TriggerKind,
    reply_to: &str,
    declined: Option<&str>,
) -> Result<AdmissionReceipt> {
    if let Some(job) = load_job(connection, conversation, trigger)? {
        return Ok(job.receipt(network));
    }
    let mut session = load_session(connection, conversation)?;
    // What reaches a deleted bot is recorded as cancelled, so a sender's retry reads it back.
    // An agent task waits for `claim_next`, which also ends its work.
    let deleted = kind != TriggerKind::AgentTask
        && super::bots::bot_deleted_from(connection, network, conversation)?;
    let ended = declined.or(deleted.then_some(BOT_DELETED));
    ensure!(
        ended.is_some() || queued_jobs(connection, conversation)?.len() < MAX_PENDING_JOBS,
        "scheduler inbox is full; trigger was not admitted"
    );
    let job = SchedulerJob {
        conversation_id: conversation.into(),
        trigger: trigger.into(),
        kind,
        sequence: session.next_sequence,
        basis: log::version_vector_from(connection, network)?,
        reply_to: reply_to.into(),
        message_id: reply_message_id(
            conversation,
            trigger,
            conversation_from(connection, network, conversation)?.map_or(0, |c| c.owner_epoch),
        ),
        runtime: None,
        tool_calls: false,
        pending_check: false,
        state: match ended {
            Some(evidence) => JobState::Cancelled {
                attempt: None,
                evidence: evidence.into(),
            },
            None => JobState::Queued,
        },
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
    /// The user messages of a conversation this Node schedules that no device answered, in
    /// conversation order: no completed reply, not answered inside a reply they were steered
    /// into, and no turn here. A former owner left them; only the user starts them again.
    pub fn unanswered_messages(
        &self,
        network: &str,
        local: &str,
        conversation: &str,
    ) -> Result<Vec<String>> {
        Ok(
            unanswered_from(&self.connection, network, local, conversation)?
                .into_iter()
                .map(|(message_id, _)| message_id)
                .collect(),
        )
    }

    /// Queues a reply here to each of `message_ids` that is still unanswered. A retry finds them
    /// queued and admits nothing again.
    pub fn reply_unanswered(
        &mut self,
        network: &str,
        local: &str,
        conversation: &str,
        message_ids: &[String],
    ) -> Result<Vec<AdmissionReceipt>> {
        self.settle_unanswered(network, local, conversation, message_ids, None)
    }

    /// The user wants no reply to `message_ids`: each still unanswered is recorded here as
    /// cancelled before it started, so it is no longer listed and never runs. Like the queue, the
    /// record stays on this owner. A retry finds them recorded and changes nothing.
    pub fn dismiss_unanswered(
        &mut self,
        network: &str,
        local: &str,
        conversation: &str,
        message_ids: &[String],
    ) -> Result<Vec<AdmissionReceipt>> {
        self.settle_unanswered(network, local, conversation, message_ids, Some(DECLINED))
    }

    fn settle_unanswered(
        &mut self,
        network: &str,
        local: &str,
        conversation: &str,
        message_ids: &[String],
        declined: Option<&str>,
    ) -> Result<Vec<AdmissionReceipt>> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut receipts = Vec::new();
        for (message_id, answers) in unanswered_from(&tx, network, local, conversation)? {
            if !message_ids.contains(&message_id) {
                continue;
            }
            let kind = match answers {
                Some(_) => TriggerKind::Answer,
                None => TriggerKind::Human,
            };
            receipts.push(admit_ended_from(
                &tx,
                network,
                conversation,
                &message_trigger(&message_id),
                kind,
                &message_id,
                declined,
            )?);
        }
        tx.commit()?;
        Ok(receipts)
    }
}

/// `(message_id, answers)` of each unanswered user message; see `Store::unanswered_messages`.
fn unanswered_from(
    connection: &Connection,
    network: &str,
    local: &str,
    conversation: &str,
) -> Result<Vec<(String, Option<String>)>> {
    let owner = conversation_from(connection, network, conversation)?.and_then(|c| c.owner);
    if owner.as_deref() != Some(local)
        || super::bots::bot_deleted_from(connection, network, conversation)?
    {
        return Ok(vec![]);
    }
    let mut statement = connection.prepare(
        "SELECT m.message_id,m.answers FROM messages m
        WHERE m.network_id=?1 AND m.conversation_id=?2 AND m.role='user' AND m.steered_into IS NULL
        AND NOT EXISTS(SELECT 1 FROM messages r WHERE r.network_id=?1 AND r.conversation_id=?2
            AND r.in_reply_to=m.message_id AND r.status='completed')
        AND NOT EXISTS(SELECT 1 FROM scheduler_jobs j WHERE j.conversation_id=?2
            AND j.trigger='message:'||m.message_id)
        AND NOT EXISTS(SELECT 1 FROM scheduler_outbox o WHERE o.conversation_id=?2
            AND o.trigger='message:'||m.message_id)
        ORDER BY m.rank",
    )?;
    Ok(statement
        .query_map(params![network, conversation], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// Whether a completed reply to `reply_to` is in the shared record, written by any device.
fn replied_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    reply_to: &str,
) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM messages WHERE network_id=? AND conversation_id=?
            AND in_reply_to=? AND status='completed')",
        params![network, conversation, reply_to],
        |r| r.get(0),
    )?)
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
            Some(("message", _)) => return Ok(0),
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
                (conversation, trigger) = cause_from(connection, network, work_id)?
                    .context("the work is not stored here")?;
            }
            _ => bail!("unknown trigger {trigger}"),
        }
    }
    Ok(MAX_CHAIN_DEPTH)
}

/// A time this long before a schedule's first local observation still runs, so "starting now" runs;
/// anything older waits for the next time, so resuming never runs a missed one.
const LATE_START_MS: i64 = 5 * 60_000;

/// A due time found later than this (the device was off or asleep) is skipped, not run late.
pub(crate) const MAX_LATE_MS: i64 = 10 * 60_000;

/// How long a new owner leaves due times alone after the handover, so a former owner that has
/// not seen it yet can still deliver the claim of a time it ran. Stays below `MAX_LATE_MS`, so
/// a time due in this window still runs, only late.
pub(crate) const TAKEOVER_GRACE_MS: i64 = 5 * 60_000;

/// Which Node took a schedule's due time: of all claims the network holds, the one of the
/// highest owner epoch, ties going to the smaller author.
fn schedule_claim(
    connection: &Connection,
    network: &str,
    conversation: &str,
    schedule_id: &str,
    slot: i64,
) -> Result<Option<String>> {
    Ok(connection
        .query_row(
            "SELECT author FROM schedule_claims WHERE network_id=? AND conversation_id=? AND schedule_id=? AND slot_ms=?",
            params![network, conversation, schedule_id, slot],
            |r| r.get(0),
        )
        .optional()?)
}

/// When the user handed this Node the conversation at `epoch`: the earliest record naming it.
/// None for the conversation's first owner, which took nothing over.
fn taken_over_at(
    connection: &Connection,
    network: &str,
    conversation: &str,
    local: &str,
    epoch: u64,
) -> Result<Option<i64>> {
    if epoch <= 1 {
        return Ok(None);
    }
    let mut statement = connection.prepare(
        "SELECT seen_at_ms,envelope FROM events WHERE network_id=? AND scope_id=? AND kind='conversation_owner' AND effective=1",
    )?;
    let mut rows = statement.query(params![network, conversation])?;
    let mut since: Option<i64> = None;
    while let Some(row) = rows.next()? {
        let envelope: SignedEvent = serde_json::from_str(&row.get::<_, String>(1)?)?;
        if let EventBody::ConversationOwner {
            scheduler_node,
            epoch: named,
            ..
        } = serde_json::from_str::<Event>(&envelope.canonical_body)?.body
            && named == epoch
            && scheduler_node == local
        {
            let at: i64 = row.get(0)?;
            since = Some(since.map_or(at, |since| since.min(at)));
        }
    }
    Ok(since)
}

/// Queues the latest due time of each schedule in conversations this Node schedules. Earlier
/// missed times are never run, and a time any Node already claimed is left to it. This Node
/// claims a time in the network's log before it runs or, found more than `MAX_LATE_MS` late,
/// skips it, so no other owner takes it again. A time due before the user handed this Node the
/// conversation is the former owner's: its claim may still be on the way, so it is left alone.
/// For `TAKEOVER_GRACE_MS` after the handover nothing is claimed yet, so a former owner that ran
/// a later time before it saw the handover can still deliver its claim.
/// A schedule waits while its previous run is still queued. Returns `(conversation, trigger)`
/// per job.
fn admit_due_schedules_from(
    connection: &Connection,
    blob_dir: &Path,
    identity: &LocalIdentity,
    network: &str,
    now: i64,
) -> Result<Vec<(String, String)>> {
    let local = identity.device_id()?;
    let mut admitted = Vec::new();
    for entry in super::bot_settings::schedules_from(connection, network, None)? {
        let conversation = entry.conversation_id.as_str();
        let Some(slot) = latest_slot(&entry.schedule, now)
            .filter(|slot| entry.schedule.enabled && *slot >= entry.changed_at_ms - LATE_START_MS)
        else {
            continue;
        };
        let Some(current) = conversation_from(connection, network, conversation)?
            .filter(|c| c.owner.as_deref() == Some(local.as_str()))
        else {
            continue;
        };
        if schedule_claim(connection, network, conversation, &entry.schedule_id, slot)?.is_some()
            || taken_over_at(
                connection,
                network,
                conversation,
                &local,
                current.owner_epoch,
            )?
            .is_some_and(|since| slot <= since || now < since + TAKEOVER_GRACE_MS)
        {
            continue;
        }
        let late = now - slot > MAX_LATE_MS;
        if !late {
            let queued = queued_jobs(connection, conversation)?;
            if queued.len() >= MAX_PENDING_JOBS
                || queued.iter().any(|job| {
                    parse_schedule_trigger(&job.trigger)
                        .is_some_and(|(id, _)| id == entry.schedule_id)
                })
            {
                continue;
            }
        }
        log::append_from(
            connection,
            blob_dir,
            identity,
            network,
            conversation,
            EventBody::ScheduleRunClaimed {
                schedule_id: entry.schedule_id.clone(),
                slot_ms: slot,
                owner_epoch: current.owner_epoch,
            },
            vec![],
            now,
        )?;
        if late {
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
        let trigger = schedule_trigger(&entry.schedule_id, slot);
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

/// Whether this Node holds the claim of the schedule time a queued job runs.
fn holds_schedule_claim(
    connection: &Connection,
    network: &str,
    local: &str,
    job: &SchedulerJob,
) -> Result<bool> {
    let Some((schedule_id, slot)) = parse_schedule_trigger(&job.trigger) else {
        return Ok(false);
    };
    Ok(
        schedule_claim(connection, network, &job.conversation_id, schedule_id, slot)?.as_deref()
            == Some(local),
    )
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

/// The conversation and trigger of the turn that dispatched a work: from the DAG, else from this
/// Node's own dispatch the target never took, which only this Node can follow.
fn cause_from(
    connection: &Connection,
    network: &str,
    work_id: &str,
) -> Result<Option<(String, String)>> {
    if let Some(work) = super::reduce::work_from(connection, network, work_id)? {
        return Ok(Some((work.conversation_id, work.trigger)));
    }
    Ok(super::work::dispatch_from(connection, work_id)?
        .map(|dispatch| (dispatch.spec.conversation, dispatch.spec.trigger)))
}

/// The user message a trigger ultimately answers, followed through the dispatch chain, so any
/// owner can report a result whose turn ran on another Node.
fn origin_message(connection: &Connection, network: &str, trigger: &str) -> Result<Option<String>> {
    let mut trigger = trigger.to_owned();
    for _ in 0..64 {
        match trigger.split_once(':') {
            Some(("message" | "bot_message", message)) => return Ok(Some(message.into())),
            Some(("work" | "task", work_id)) => match cause_from(connection, network, work_id)? {
                Some((_, cause)) => trigger = cause,
                None => return Ok(None),
            },
            Some(("schedule", _)) => return Ok(Some(trigger)),
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// Admits the work's report turn for the message the chain started from, unless the user
/// opted out there.
fn admit_report_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    work_id: &str,
) -> Result<Option<AdmissionReceipt>> {
    let trigger = work_trigger(work_id);
    let Some(reply_to) = origin_message(connection, network, &trigger)? else {
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

/// A dispatch refused, or left unknown by a removed device, has no result in the DAG; the
/// dispatching owner wakes its bot through the work's trigger instead, once per work.
pub(crate) fn admit_dispatch_result_from(
    connection: &Connection,
    network: &str,
    local: &str,
    dispatch: &crate::work::Dispatch,
) -> Result<Option<AdmissionReceipt>> {
    let spec = &dispatch.spec;
    let owner = conversation_from(connection, network, &spec.conversation)?.and_then(|c| c.owner);
    if !dispatch.state.ended()
        || owner.as_deref() != Some(local)
        || super::reduce::work_from(connection, network, &spec.work_id)?.is_some()
    {
        return Ok(None);
    }
    admit_report_from(connection, network, &spec.conversation, &spec.work_id)
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
    let owner = conversation_from(connection, network, conversation)?.and_then(|c| c.owner);
    if owner.as_deref() != Some(local) {
        let trigger = work_trigger(work_id);
        return queue_delivery_from(connection, network, conversation, &trigger).map(Some);
    }
    admit_report_from(connection, network, conversation, work_id)
}

/// The reply's status, the failure it shows, and its text when the turn said nothing. The
/// failure's detail stays on the local job and in node.log; clients word its kind.
fn reply_status(outcome: &JobOutcome) -> (ReplyStatus, Option<FailureKind>, &'static str) {
    match outcome {
        JobOutcome::Completed => (ReplyStatus::Completed, None, "（没有回复内容）"),
        JobOutcome::Failed(failure) => (ReplyStatus::Failed, Some(failure.kind), ""),
        JobOutcome::Cancelled(_) => (ReplyStatus::Stopped, None, "已停止回复。"),
        JobOutcome::Interrupted { failure, .. } => {
            (ReplyStatus::Interrupted, Some(failure.kind), "")
        }
    }
}

/// The job's bot reply, unless one is already written; returns whether it wrote it.
#[expect(
    clippy::too_many_arguments,
    reason = "a reply is one signed event of the job"
)]
fn post_reply_from(
    connection: &Connection,
    blob_dir: &Path,
    identity: &LocalIdentity,
    network: &str,
    job: &SchedulerJob,
    (status, failure): (ReplyStatus, Option<FailureKind>),
    text: String,
    at: i64,
) -> Result<bool> {
    if message_from(connection, network, &job.conversation_id, &job.message_id)?.is_some() {
        return Ok(false);
    }
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
            failure,
            answers: None,
            attestation: None,
        },
        vec![],
        at,
    )?;
    Ok(true)
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
    if let JobOutcome::Interrupted {
        failure,
        process_unconfirmed: true,
    } = &outcome
    {
        if !matches!(job.state, JobState::Reconciling { .. })
            && attempt.permission_version == session.permission_version
        {
            fence(session)?;
            session.handoff_source = job.basis.clone();
        }
        save_pending_check(connection, &job, &attempt, &failure.detail, at)?;
        job.state = JobState::Reconciling {
            attempt,
            failure: failure.clone(),
        };
        job.pending_check = true;
        if let Some(runtime) = &mut job.runtime {
            runtime.stage = ProviderStage::ReadBack;
        }
        save_job(connection, &job)?;
        connection.execute(
            "UPDATE scheduler_jobs SET partial_text=? WHERE conversation_id=? AND trigger=?",
            params![text, job.conversation_id, job.trigger],
        )?;
        save_session(connection, session)?;
        return Ok(job);
    }
    let (status, failure, fallback) = reply_status(&outcome);
    let text = if text.trim().is_empty() {
        fallback.to_owned()
    } else {
        text.to_owned()
    };
    let change = TaskJobChange::Ended {
        outcome: &outcome,
        text: &text,
    };
    if let Some((work_id, signal)) = task_work_signal(&job.trigger, change) {
        super::work::apply_signal_from(
            connection, blob_dir, identity, network, work_id, signal, at,
        )?;
    } else if post_reply_from(
        connection,
        blob_dir,
        identity,
        network,
        &job,
        (status, failure),
        text,
        at,
    )? {
        let context = super::since_checkpoint_from(connection, network, &job.conversation_id)?;
        let works = super::reduce::works_from(connection, network, &job.conversation_id)?;
        // A Node the conversation moved away from mid-turn leaves condensing to the new owner.
        let owned = conversation_from(connection, network, &job.conversation_id)?
            .and_then(|c| c.owner)
            == Some(identity.device_id()?);
        if owned && let Some(checkpoint) = crate::scheduler::checkpoint(&context, &works) {
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
        JobOutcome::Failed(failure) => JobState::Failed { attempt, failure },
        JobOutcome::Cancelled(evidence) => JobState::Cancelled {
            attempt: Some(attempt),
            evidence,
        },
        JobOutcome::Interrupted { failure, .. } => {
            if attempt.permission_version == session.permission_version {
                fence(session)?;
            }
            JobState::Interrupted { attempt, failure }
        }
    };
    if session.active.as_deref() == Some(job.trigger.as_str()) {
        session.active = None;
    }
    connection.execute(
        "DELETE FROM pending_checks WHERE attempt_id=?",
        [&attempt_id],
    )?;
    job.pending_check = false;
    forget_process(connection, &attempt_id)?;
    if matches!(job.state, JobState::Completed { .. })
        && let Some(runtime) = &mut job.runtime
    {
        runtime.stage = ProviderStage::Record;
        runtime.execution = TurnExecution::Started;
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
            job.runtime = None;
            job.tool_calls = false;
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
                    failure: ExecFailure::new(
                        FailureKind::Restarted,
                        "the device restarted during this reply; it was not redone",
                    ),
                    process_unconfirmed: true,
                },
                "",
                crate::runtime::now_ms(),
            )?;
        }
        JobState::Reconciling { .. } => {}
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
        attachments: Vec<Attachment>,
        report_results: bool,
        attestation: Attestation,
        created_at_ms: i64,
    ) -> Result<(Message, AdmissionReceipt)> {
        let local = identity.device_id()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize_node_from(&tx, network, &local)?;
        ensure!(
            !super::bots::bot_deleted_from(&tx, network, conversation)?,
            "this bot was deleted"
        );
        let owner = conversation_from(&tx, network, conversation)?
            .context("conversation not found")?
            .owner
            .context("conversation has no scheduling Node")?;
        if let Some(existing) = message_from(&tx, network, conversation, message_id)? {
            ensure!(
                existing.role == Role::User
                    && existing.answers.is_none()
                    && existing.text == text
                    && existing.attachments == attachments
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
                    attachments: attachments.clone(),
                    report_results: Some(report_results),
                    in_reply_to: None,
                    status: None,
                    answers: None,
                    attestation: Some(attestation),
                    failure: None,
                },
                attachments.iter().map(Attachment::blob).collect(),
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
        let admitted = admit_due_schedules_from(&tx, &self.blob_dir, identity, network, now)?;
        tx.commit()?;
        Ok(admitted)
    }

    pub fn scheduler_job(&self, conversation: &str, trigger: &str) -> Result<Option<SchedulerJob>> {
        load_job(&self.connection, conversation, trigger)
    }

    pub fn scheduler_jobs(&self, conversation: &str) -> Result<Vec<SchedulerJob>> {
        query_jobs(&self.connection, "conversation_id=?", [conversation])
    }

    /// All local scheduler rows for bounded, read-only workspace projections.
    pub fn scheduler_jobs_all(&self) -> Result<Vec<SchedulerJob>> {
        query_jobs(&self.connection, "1", [])
    }

    /// Each busy conversation on this device; a reply running outranks messages still waiting.
    /// An agent task runs in its bot's conversation but is that work, not a reply in the chat.
    pub fn busy_conversations(&self) -> Result<std::collections::BTreeMap<String, Busy>> {
        let mut statement = self.connection.prepare(
            "SELECT conversation_id,state FROM scheduler_jobs WHERE state IN ('queued','claimed','running','cancelling','steering') AND kind<>'agent_task'
             UNION ALL SELECT conversation_id,'queued' FROM scheduler_outbox",
        )?;
        let rows =
            statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut busy = std::collections::BTreeMap::new();
        for row in rows {
            let (conversation, state) = row?;
            let now = match state.as_str() {
                "running" | "cancelling" => Busy::Replying,
                _ => Busy::Thinking,
            };
            let entry = busy.entry(conversation).or_insert(now);
            *entry = (*entry).max(now);
        }
        Ok(busy)
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
        pending_conversations_from(&self.connection)
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
        let local = identity.device_id()?;
        let current = conversation_from(&tx, network, conversation)?;
        let owner_epoch = current.as_ref().map_or(0, |c| c.owner_epoch);
        let owned = current.and_then(|c| c.owner) == Some(local.clone());
        // A message whose steer is unsettled keeps its place here; nothing behind it runs first.
        // A conversation that moved away still hands its queue on.
        if session.active.is_some()
            || owned
                && !query_jobs(
                    &tx,
                    "conversation_id=? AND state='steering'",
                    [conversation],
                )?
                .is_empty()
        {
            save_session(&tx, &session)?;
            tx.commit()?;
            return Ok(None);
        }
        if !owned {
            hand_off_from(&tx, network, conversation)?;
        }
        let mut queued = queued_jobs(&tx, conversation)?;
        queued.sort_by_key(|job| {
            (
                !matches!(job.kind, TriggerKind::Human | TriggerKind::Answer),
                job.sequence,
            )
        });
        let deleted = super::bots::bot_deleted_from(&tx, network, conversation)?;
        let mut selected = None;
        for mut job in queued {
            if deleted {
                let outcome = JobOutcome::Cancelled(BOT_DELETED.into());
                let change = TaskJobChange::Ended {
                    outcome: &outcome,
                    text: "",
                };
                if let Some((work_id, signal)) = task_work_signal(&job.trigger, change) {
                    super::work::apply_signal_from(
                        &tx,
                        &self.blob_dir,
                        identity,
                        network,
                        work_id,
                        signal,
                        crate::runtime::now_ms(),
                    )?;
                }
            }
            let evidence = match job.kind {
                _ if deleted => Some(BOT_DELETED),
                TriggerKind::Schedule if live_schedule(&tx, network, &job)?.is_none() => {
                    Some("定时任务已暂停或删除")
                }
                TriggerKind::Schedule if !holds_schedule_claim(&tx, network, &local, &job)? => {
                    Some("这次定时已由另一台设备领取")
                }
                TriggerKind::AgentTask if super::work::run_finished(&tx, &job.reply_to)? => {
                    Some("任务结果已由你确认")
                }
                // A restored backup, or a new owner retrying a delivery, may queue a trigger
                // some device already answered: its turn's effects happened once already.
                TriggerKind::Human
                | TriggerKind::Answer
                | TriggerKind::BotMessage
                | TriggerKind::Schedule
                    if replied_from(&tx, network, conversation, &job.reply_to)? =>
                {
                    Some("已在别处回复")
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
        job.basis = log::version_vector_from(&tx, network)?;
        if session.sdk_config.as_deref() != Some(sdk_config) || session.owner_epoch != owner_epoch {
            advance_lineage(&mut session, &job.basis)?;
            session.sdk_config = Some(sdk_config.into());
            session.owner_epoch = owner_epoch;
        }
        let resumable = job.kind != TriggerKind::AgentTask
            && session.native_id.is_some()
            && session
                .covers
                .iter()
                .all(|(author, seq)| job.basis.get(author).is_some_and(|have| have >= seq));
        let previous_native_id = resumable.then(|| session.native_id.clone()).flatten();
        let previous_token = resumable.then(|| session.native_token.clone()).flatten();
        // A thread that never started a turn has seen nothing: the prompt carries the context.
        let covers = resumable
            .then(|| session.covers.clone())
            .filter(|covers| !covers.is_empty());
        let shown = match covers {
            Some(_) => session.shown.clone(),
            None => Shown::default(),
        };
        let previous_runtime = tx.query_row(
            "SELECT runtime FROM scheduler_jobs WHERE conversation_id=? AND sequence<? AND runtime IS NOT NULL ORDER BY sequence DESC LIMIT 1",
            params![conversation, job.sequence],
            |row| row.get::<_, String>(0),
        ).optional()?.map(|json| serde_json::from_str::<TurnRuntime>(&json)).transpose()?
        .map(|runtime| RuntimeSelection {
            provider: runtime.provider,
            model: runtime.model,
            reasoning_effort: runtime.reasoning_effort,
        });
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
            previous_token,
            covers,
            shown,
            session_generation: session.session_generation,
            handoff_revision: session.handoff_revision,
            handoff_source: session.handoff_source,
            previous_runtime,
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

    pub fn record_turn_runtime(
        &mut self,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
        runtime: &TurnRuntime,
    ) -> Result<SchedulerJob> {
        ensure!(
            runtime.delivery_id == attempt_id,
            "runtime delivery does not match attempt"
        );
        ensure!(
            runtime.execution == TurnExecution::NotStarted,
            "new runtime must not have started"
        );
        self.update_attempt(conversation, trigger, attempt_id, |job, session| {
            ensure!(
                matches!(
                    job.state,
                    JobState::Claimed { .. } | JobState::Running { .. }
                ),
                "scheduler attempt is no longer active"
            );
            ensure!(
                !job.pending_check && !job.tool_calls,
                "runtime has pending execution evidence"
            );
            ensure!(
                job.runtime
                    .as_ref()
                    .is_none_or(|saved| saved.execution == TurnExecution::NotStarted),
                "runtime may already have started"
            );
            ensure!(
                runtime.session_generation == session.session_generation,
                "runtime session generation does not match"
            );
            if runtime.switched_from.is_some() {
                match &mut job.state {
                    JobState::Claimed { attempt } | JobState::Running { attempt } => {
                        attempt.permission_version = session.permission_version;
                    }
                    _ => unreachable!("active state checked above"),
                }
            }
            job.runtime = Some(runtime.clone());
            Ok(())
        })
    }

    pub fn update_turn_runtime(
        &mut self,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
        stage: ProviderStage,
        execution: TurnExecution,
    ) -> Result<SchedulerJob> {
        self.update_attempt(conversation, trigger, attempt_id, |job, _| {
            ensure!(!job.state.terminal(), "scheduler attempt already ended");
            ensure!(
                execution != TurnExecution::NotStarted || !job.tool_calls,
                "a turn that called tools cannot be marked not started"
            );
            let runtime = job.runtime.as_mut().context("turn runtime missing")?;
            ensure!(
                runtime.execution != TurnExecution::Started
                    || execution != TurnExecution::NotStarted,
                "a started turn cannot be marked not started"
            );
            runtime.stage = stage;
            runtime.execution = execution;
            Ok(())
        })
    }

    pub fn record_manager_tool_call(
        &mut self,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
    ) -> Result<()> {
        self.update_attempt(conversation, trigger, attempt_id, |job, _| {
            ensure!(
                matches!(
                    job.state,
                    JobState::Running { .. } | JobState::Claimed { .. }
                ),
                "scheduler attempt is no longer active"
            );
            job.tool_calls = true;
            if let Some(runtime) = &mut job.runtime {
                runtime.execution = TurnExecution::Started;
            }
            Ok(())
        })?;
        Ok(())
    }

    pub fn record_context_compaction(
        &mut self,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
    ) -> Result<u64> {
        let mut generation = 0;
        self.update_attempt(conversation, trigger, attempt_id, |job, session| {
            ensure!(
                session.active.as_deref() == Some(trigger)
                    && matches!(
                        job.state,
                        JobState::Claimed { .. } | JobState::Running { .. }
                    ),
                "scheduler attempt is no longer active"
            );
            session.session_generation = session
                .session_generation
                .checked_add(1)
                .context("scheduler session generation overflow")?;
            session.handoff_revision = session
                .handoff_revision
                .checked_add(1)
                .context("scheduler handoff revision overflow")?;
            session.handoff_source = job.basis.clone();
            generation = session.session_generation;
            if let Some(runtime) = &mut job.runtime {
                runtime.session_generation = generation;
                runtime.stage = ProviderStage::Compact;
            }
            Ok(())
        })?;
        Ok(generation)
    }

    pub fn advance_scheduler_lineage(
        &mut self,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
    ) -> Result<u64> {
        let mut generation = 0;
        self.update_attempt(conversation, trigger, attempt_id, |job, session| {
            ensure!(
                session.active.as_deref() == Some(trigger)
                    && matches!(
                        job.state,
                        JobState::Claimed { .. } | JobState::Running { .. }
                    ),
                "scheduler attempt is no longer active"
            );
            advance_lineage(session, &job.basis)?;
            generation = session.session_generation;
            if let Some(runtime) = &mut job.runtime {
                runtime.session_generation = generation;
            }
            Ok(())
        })?;
        Ok(generation)
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
            if let Some(runtime) = &mut job.runtime {
                runtime.execution = TurnExecution::Unknown;
                runtime.stage = ProviderStage::TurnStart;
            }
            Ok(())
        })
    }

    pub fn save_scheduler_native_id(
        &mut self,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
        native_id: &str,
        token: &str,
        sent: bool,
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
                if session
                    .native_id
                    .as_deref()
                    .is_some_and(|previous| previous != native_id)
                {
                    advance_lineage(session, &job.basis)?;
                    if let Some(runtime) = &mut job.runtime {
                        runtime.session_generation = session.session_generation;
                    }
                }
                session.native_id = Some(native_id.into());
                session.native_token = Some(token.into());
                if sent {
                    session.covers = job.basis.clone();
                }
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
        if let JobState::Steering { into, .. } = &job.state {
            let into = into.clone();
            match query_jobs(
                &tx,
                "conversation_id=? AND attempt_id=?",
                params![conversation, into.attempt_id],
            )?
            .pop()
            .filter(|host| !host.state.terminal())
            {
                Some(host) => job = host,
                // The reply it went into already ended: there is nothing left to stop, and the
                // message is never sent again.
                None => {
                    job.state = JobState::Steered {
                        into,
                        outcome: SteerOutcome::Unknown,
                    }
                }
            }
        }
        if matches!(job.state, JobState::Queued) {
            let evidence = "用户在开始前取消".to_owned();
            let outcome = JobOutcome::Cancelled(evidence.clone());
            let (status, failure, text) = reply_status(&outcome);
            let change = TaskJobChange::Ended {
                outcome: &outcome,
                text: "",
            };
            let at = crate::runtime::now_ms();
            if let Some((work_id, signal)) = task_work_signal(&job.trigger, change) {
                super::work::apply_signal_from(
                    &tx,
                    &self.blob_dir,
                    identity,
                    network,
                    work_id,
                    signal,
                    at,
                )?;
            } else {
                post_reply_from(
                    &tx,
                    &self.blob_dir,
                    identity,
                    network,
                    &job,
                    (status, failure),
                    text.into(),
                    at,
                )?;
            }
            job.state = JobState::Cancelled {
                attempt: None,
                evidence,
            };
        } else if let JobState::Claimed { attempt } | JobState::Running { attempt } = &job.state {
            ensure!(
                session.active.as_deref() == Some(job.trigger.as_str()),
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

    /// Stops the turns of conversations that moved to a newer owner: each becomes cancelling
    /// and loses its tool permission here, before the runtime cancels its SDK call. What never
    /// started there goes to the new owner, whether or not a bot runs here. Returns
    /// `(conversation, trigger, attempt_id)` per fenced turn.
    pub fn fence_moved_turns(
        &mut self,
        network: &str,
        local: &str,
    ) -> Result<Vec<(String, String, String)>> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for conversation in pending_conversations_from(&tx)? {
            if conversation_from(&tx, network, &conversation)?
                .and_then(|c| c.owner)
                .as_deref()
                != Some(local)
            {
                hand_off_from(&tx, network, &conversation)?;
            }
        }
        let mut fenced = Vec::new();
        for mut session in query_sessions(&tx, "active IS NOT NULL", [])? {
            let Some(trigger) = session.active.clone() else {
                continue;
            };
            let Some(mut job) = load_job(&tx, &session.conversation_id, &trigger)? else {
                continue;
            };
            let (JobState::Claimed { attempt } | JobState::Running { attempt }) = &job.state else {
                continue;
            };
            if holds_turn(&tx, network, local, &session, &job)? {
                continue;
            }
            let attempt = attempt.clone();
            job.state = JobState::Cancelling {
                attempt: attempt.clone(),
            };
            fence(&mut session)?;
            save_job(&tx, &job)?;
            save_session(&tx, &session)?;
            fenced.push((session.conversation_id, trigger, attempt.id));
        }
        tx.commit()?;
        Ok(fenced)
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
        network: &str,
        local: &str,
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
            )
            && holds_turn(&self.connection, network, local, &session, &job)?)
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

    /// The App Server turn of a reply, recorded before `turn/start` with its thread only and
    /// again once the turn is known. Only a started turn has shown the thread the reply's basis
    /// and `shown`; a refused `turn/start` leaves them for the next turn.
    pub fn record_manager_turn(
        &mut self,
        attempt_id: &str,
        server: &str,
        thread_id: &str,
        turn_id: Option<&str>,
        shown: &Shown,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR REPLACE INTO manager_turns(attempt_id,server,thread_id,turn_id) VALUES(?,?,?,?)",
            params![attempt_id, server, thread_id, turn_id],
        )?;
        if turn_id.is_some()
            && let Some(mut job) = query_jobs(&tx, "attempt_id=?", [attempt_id])?.pop()
        {
            if let Some(runtime) = &mut job.runtime {
                runtime.execution = TurnExecution::Started;
                runtime.stage = ProviderStage::Run;
                save_job(&tx, &job)?;
            }
            if job.kind != TriggerKind::AgentTask {
                let mut session = load_session(&tx, &job.conversation_id)?;
                if session.native_id.as_deref() == Some(thread_id) {
                    session.covers = job.basis;
                    session.shown = shown.clone();
                    save_session(&tx, &session)?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The conversation whose own reply thread this is: its session's native thread, or the
    /// thread a reply or agent task runs on now.
    pub fn native_thread_conversation(&self, thread_id: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT conversation_id FROM scheduler_sessions WHERE native_id=?1
                 UNION SELECT j.conversation_id FROM manager_turns t
                    JOIN scheduler_jobs j ON j.attempt_id=t.attempt_id WHERE t.thread_id=?1
                 LIMIT 1",
                [thread_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Queued user messages that may join the running reply of their conversation: plain text
    /// while a reply to a user message runs on a turn of `provider` this Node holds.
    pub fn steer_candidates(
        &self,
        network: &str,
        local: &str,
        boot: &str,
        provider: Provider,
    ) -> Result<Vec<SteerCandidate>> {
        let mut candidates = Vec::new();
        for session in query_sessions(&self.connection, "active IS NOT NULL", [])? {
            if let Some(target) =
                steer_target(&self.connection, network, local, boot, &session, provider)?
            {
                for job in queued_jobs(&self.connection, &session.conversation_id)? {
                    if let Some(text) = steerable_text(&self.connection, network, &job)? {
                        candidates.push(SteerCandidate {
                            conversation_id: job.conversation_id,
                            trigger: job.trigger,
                            message_id: job.reply_to,
                            text,
                            into: target.clone(),
                        });
                    }
                }
            }
        }
        Ok(candidates)
    }

    /// Marks the message as being steered into `into` if that reply still runs here under the
    /// same permission. Only then may `turn/steer` be sent.
    pub fn begin_steer(
        &mut self,
        network: &str,
        local: &str,
        boot: &str,
        conversation: &str,
        trigger: &str,
        into: &SteerTarget,
    ) -> Result<bool> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session = load_session(&tx, conversation)?;
        let Some(mut job) = load_job(&tx, conversation, trigger)? else {
            return Ok(false);
        };
        if job.state != JobState::Queued
            || steerable_text(&tx, network, &job)?.is_none()
            || steer_target(&tx, network, local, boot, &session, into.provider)?.as_ref()
                != Some(into)
        {
            return Ok(false);
        }
        job.state = JobState::Steering {
            into: into.clone(),
            unread_since_ms: None,
            running_since_ms: None,
        };
        save_job(&tx, &job)?;
        tx.commit()?;
        Ok(true)
    }

    /// Moves a steering message on: back to the queue when nothing reached the turn, steered,
    /// or still steering with its updated wait clocks. Anything else is left. An answered steer
    /// is recorded in the conversation, so a later owner does not count the message unanswered.
    pub fn settle_steer(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        conversation: &str,
        trigger: &str,
        next: JobState,
    ) -> Result<()> {
        ensure!(
            matches!(
                next,
                JobState::Queued | JobState::Steering { .. } | JobState::Steered { .. }
            ),
            "a steer settles only into the queue or a steer state"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut job = load_job(&tx, conversation, trigger)?.context("scheduler job missing")?;
        if !matches!(job.state, JobState::Steering { .. }) {
            return Ok(());
        }
        if let JobState::Steered {
            into,
            outcome: SteerOutcome::Answered,
        } = &next
        {
            let host = query_jobs(
                &tx,
                "conversation_id=? AND attempt_id=?",
                params![conversation, into.attempt_id],
            )?
            .pop()
            .context("steered reply missing")?;
            log::append_from(
                &tx,
                &self.blob_dir,
                identity,
                network,
                conversation,
                EventBody::MessageSteered {
                    message_id: job.reply_to.clone(),
                    reply: host.message_id,
                },
                vec![],
                crate::runtime::now_ms(),
            )?;
        }
        job.state = next;
        save_job(&tx, &job)?;
        tx.commit()?;
        Ok(())
    }

    /// Whether the reply attempt a message was steered into is still claimed or running.
    pub fn steer_host_active(&self, conversation: &str, attempt_id: &str) -> Result<bool> {
        Ok(query_jobs(
            &self.connection,
            "conversation_id=? AND attempt_id=?",
            params![conversation, attempt_id],
        )?
        .iter()
        .any(|job| !job.state.terminal()))
    }

    pub fn steering_jobs(&self) -> Result<Vec<SchedulerJob>> {
        query_jobs(&self.connection, "state='steering'", [])
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

    pub fn confirm_manager_stopped(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        check: &PendingCheck,
        at: i64,
    ) -> Result<SchedulerJob> {
        let job = load_job(&self.connection, &check.conversation_id, &check.trigger)?
            .context("scheduler job missing")?;
        let JobState::Reconciling { failure, .. } = &job.state else {
            bail!("scheduler job is not awaiting reconciliation");
        };
        let partial: String = self.connection.query_row(
            "SELECT partial_text FROM scheduler_jobs WHERE conversation_id=? AND trigger=?",
            params![check.conversation_id, check.trigger],
            |row| row.get(0),
        )?;
        self.finish_job(
            identity,
            network,
            &check.conversation_id,
            &check.trigger,
            &check.attempt_id,
            JobOutcome::Interrupted {
                failure: failure.clone(),
                process_unconfirmed: false,
            },
            &partial,
            at,
        )
    }

    pub fn scheduler_maintenance_idle(&self) -> Result<()> {
        let busy: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduler_jobs WHERE state IN ('queued','steering'))
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
