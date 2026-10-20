//! The owner's local scheduling state. A job is keyed by its trigger, and its reply is written
//! in the same transaction that ends it, so nothing is published twice or lost.
use super::{
    Store, authorize_node_from, autonomy_from, conversation_from, local_load, local_prefix,
    local_save, log, message_from,
};
use crate::{crypto::LocalIdentity, model::*, scheduling::*};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, TransactionBehavior, params};
use std::path::Path;

fn job_key(conversation: &str, trigger: &str) -> String {
    scheduler_key("job", conversation, trigger)
}

fn session_key(conversation: &str) -> String {
    scheduler_key("session", conversation, "")
}

fn load_session(
    connection: &Connection,
    network: &str,
    conversation: &str,
) -> Result<SchedulerSession> {
    let session = local_load::<SchedulerSession>(connection, network, &session_key(conversation))?
        .unwrap_or_else(|| SchedulerSession::new(conversation));
    ensure!(
        session.version == SCHEDULING_VERSION,
        "unsupported scheduler session version"
    );
    Ok(session)
}

fn save_session(connection: &Connection, network: &str, session: &SchedulerSession) -> Result<()> {
    local_save(
        connection,
        network,
        &session_key(&session.conversation_id),
        session,
    )
}

fn load_job(
    connection: &Connection,
    network: &str,
    conversation: &str,
    trigger: &str,
) -> Result<Option<SchedulerJob>> {
    local_load(connection, network, &job_key(conversation, trigger))
}

fn save_job(connection: &Connection, network: &str, job: &SchedulerJob) -> Result<()> {
    local_save(
        connection,
        network,
        &job_key(&job.conversation_id, &job.trigger),
        job,
    )
}

fn sessions(connection: &Connection, network: &str) -> Result<Vec<SchedulerSession>> {
    local_prefix(connection, network, "scheduler:v2:session:")
}

fn process_key(attempt_id: &str) -> String {
    format!("manager-process:v2:{attempt_id}")
}

pub(crate) fn pending_checks_from(
    connection: &Connection,
    network: &str,
) -> Result<Vec<PendingCheck>> {
    let mut checks: Vec<PendingCheck> = local_prefix(connection, network, "scheduler:v2:pending:")?;
    checks.sort_by_key(|check| check.created_at_ms);
    Ok(checks)
}

fn save_pending_check(
    connection: &Connection,
    network: &str,
    job: &SchedulerJob,
    attempt: &ManagerAttempt,
    detail: &str,
    at: i64,
) -> Result<()> {
    let process_groups =
        local_load::<Vec<u32>>(connection, network, &process_key(&attempt.id))?.unwrap_or_default();
    local_save(
        connection,
        network,
        &scheduler_key("pending", &job.conversation_id, &job.trigger),
        &PendingCheck {
            version: SCHEDULING_VERSION,
            check_id: PendingCheck::id(&attempt.id),
            conversation_id: job.conversation_id.clone(),
            trigger: job.trigger.clone(),
            attempt_id: attempt.id.clone(),
            detail: detail.into(),
            process_groups,
            created_at_ms: at,
        },
    )
}

fn forget_process(
    connection: &Connection,
    network: &str,
    attempt: Option<&ManagerAttempt>,
) -> Result<()> {
    if let Some(attempt) = attempt {
        connection.execute(
            "DELETE FROM local_values WHERE network_id=? AND key=?",
            params![network, process_key(&attempt.id)],
        )?;
    }
    Ok(())
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
    if let Some(job) = load_job(connection, network, conversation, trigger)? {
        return Ok(job.receipt(network));
    }
    let mut session = load_session(connection, network, conversation)?;
    ensure!(
        session.pending.len() < MAX_PENDING_JOBS,
        "scheduler inbox is full; trigger was not admitted"
    );
    let job = SchedulerJob {
        version: SCHEDULING_VERSION,
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
    session.pending.push(trigger.into());
    save_job(connection, network, &job)?;
    save_session(connection, network, &session)?;
    Ok(job.receipt(network))
}

/// Keeps a trigger for the conversation's owner until it returns a receipt.
pub(crate) fn queue_delivery_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    trigger: &str,
) -> Result<AdmissionReceipt> {
    let key = scheduler_key("outbox", conversation, trigger);
    let entry = match local_load::<OutboxEntry>(connection, network, &key)? {
        Some(entry) => entry,
        None => {
            let entry = OutboxEntry {
                version: SCHEDULING_VERSION,
                network_id: network.into(),
                conversation_id: conversation.into(),
                trigger: trigger.into(),
            };
            local_save(connection, network, &key, &entry)?;
            entry
        }
    };
    Ok(entry.receipt())
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
            Some(("message", message)) => return Ok(Some(message.into())),
            Some(("work" | "task", parent)) => work_id = parent.into(),
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

fn reply_status(outcome: &JobOutcome) -> (ReplyStatus, String) {
    match outcome {
        JobOutcome::Completed => (ReplyStatus::Completed, "（没有回复内容）".into()),
        JobOutcome::Failed(detail) => (ReplyStatus::Failed, format!("回复失败：{detail}")),
        JobOutcome::Cancelled(_) => (ReplyStatus::Stopped, "已停止回复。".into()),
        JobOutcome::Interrupted { detail, .. } => {
            (ReplyStatus::Interrupted, format!("回复被中断：{detail}"))
        }
    }
}

/// Ends the job and writes its assistant reply in one transaction. Partial text of a stopped or
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
                save_pending_check(connection, network, &job, &attempt, &detail, at)?;
            }
            JobState::Interrupted { attempt, detail }
        }
    };
    if session.active.as_deref() == Some(job.trigger.as_str()) {
        session.active = None;
    }
    forget_process(connection, network, job.state.attempt())?;
    save_job(connection, network, &job)?;
    save_session(connection, network, session)?;
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
    let mut job = load_job(connection, network, &session.conversation_id, &trigger)?
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
            if !session.pending.contains(&trigger) {
                session.pending.push(trigger);
            }
            session.active = None;
            save_job(connection, network, &job)?;
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
                    detail: "Node 重启，这次回复没有重放".into(),
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

    pub fn scheduler_job(
        &self,
        network: &str,
        conversation: &str,
        trigger: &str,
    ) -> Result<Option<SchedulerJob>> {
        load_job(&self.connection, network, conversation, trigger)
    }

    pub fn scheduler_status(
        &self,
        network: &str,
        conversation: &str,
        trigger: &str,
    ) -> Result<Option<AdmissionReceipt>> {
        if let Some(job) = self.scheduler_job(network, conversation, trigger)? {
            return Ok(Some(job.receipt(network)));
        }
        Ok(local_load::<OutboxEntry>(
            &self.connection,
            network,
            &scheduler_key("outbox", conversation, trigger),
        )?
        .map(|entry| entry.receipt()))
    }

    pub fn scheduler_jobs(&self, network: &str, conversation: &str) -> Result<Vec<SchedulerJob>> {
        let mut jobs: Vec<SchedulerJob> = local_prefix(
            &self.connection,
            network,
            &scheduler_prefix("job", conversation),
        )?;
        jobs.sort_by_key(|job| job.sequence);
        Ok(jobs)
    }

    pub fn scheduler_outbox(&self, network: &str) -> Result<Vec<OutboxEntry>> {
        local_prefix(&self.connection, network, "scheduler:v2:outbox:")
    }

    pub fn pending_scheduler_conversations(&self, network: &str) -> Result<Vec<String>> {
        Ok(sessions(&self.connection, network)?
            .into_iter()
            .filter(|s| !s.pending.is_empty())
            .map(|s| s.conversation_id)
            .collect())
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
        for mut session in sessions(&tx, network)? {
            recover_session(&tx, &self.blob_dir, identity, network, &mut session, boot)?;
            save_session(&tx, network, &session)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Human messages go first. A work result runs read-only under `report` and is cancelled
    /// under `off`; the stricter setting always wins.
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
        let mut session = load_session(&tx, network, conversation)?;
        recover_session(&tx, &self.blob_dir, identity, network, &mut session, boot)?;
        if session.active.is_some() {
            save_session(&tx, network, &session)?;
            tx.commit()?;
            return Ok(None);
        }
        let current = conversation_from(&tx, network, conversation)?;
        let owner_epoch = current.as_ref().map_or(0, |c| c.owner_epoch);
        let owned = current.and_then(|c| c.owner) == Some(identity.device_id()?);
        let mut queued = Vec::new();
        for trigger in session.pending.clone() {
            let job = load_job(&tx, network, conversation, &trigger)?
                .context("missing queued scheduler job")?;
            ensure!(
                matches!(job.state, JobState::Queued),
                "pending scheduler job is not queued"
            );
            // The user moved the conversation: what never started goes to the new owner.
            if !owned && job.kind != TriggerKind::AgentTask {
                tx.execute(
                    "DELETE FROM local_values WHERE network_id=? AND key=?",
                    params![network, job_key(conversation, &trigger)],
                )?;
                queue_delivery_from(&tx, network, conversation, &trigger)?;
                session.pending.retain(|pending| pending != &trigger);
                continue;
            }
            queued.push(job);
        }
        queued.sort_by_key(|job| (job.kind != TriggerKind::Human, job.sequence));
        let mode = autonomy_from(&tx, network, conversation)?.mode;
        let mut selected = None;
        for mut job in queued {
            if job.kind == TriggerKind::WorkResult && mode == AutonomyMode::Off {
                job.state = JobState::Cancelled {
                    attempt: None,
                    evidence: "自动跟进已关闭".into(),
                };
                session.pending.retain(|pending| pending != &job.trigger);
                save_job(&tx, network, &job)?;
                continue;
            }
            selected = Some(job);
            break;
        }
        let Some(mut job) = selected else {
            save_session(&tx, network, &session)?;
            tx.commit()?;
            return Ok(None);
        };
        if session.sdk_config.as_deref() != Some(sdk_config) || session.owner_epoch != owner_epoch {
            session.native_id = None;
            session.covers.clear();
            session.sdk_config = Some(sdk_config.into());
            session.owner_epoch = owner_epoch;
        }
        let read_only = job.kind == TriggerKind::WorkResult && mode == AutonomyMode::Report;
        // Reused only under the same ownership and when this basis contains what it saw.
        let resumable = !read_only
            && job.kind != TriggerKind::AgentTask
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
                read_only,
                permission_version: session.permission_version,
            },
        };
        session.pending.retain(|trigger| trigger != &job.trigger);
        session.active = Some(job.trigger.clone());
        save_job(&tx, network, &job)?;
        save_session(&tx, network, &session)?;
        tx.commit()?;
        Ok(Some(ClaimedJob {
            job,
            previous_native_id,
            covers,
        }))
    }

    fn update_attempt(
        &mut self,
        network: &str,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
        update: impl FnOnce(&mut SchedulerJob, &mut SchedulerSession) -> Result<()>,
    ) -> Result<SchedulerJob> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = load_session(&tx, network, conversation)?;
        let mut job =
            load_job(&tx, network, conversation, trigger)?.context("scheduler job missing")?;
        ensure!(
            job.state
                .attempt()
                .is_some_and(|attempt| attempt.id == attempt_id),
            "scheduler attempt does not match"
        );
        update(&mut job, &mut session)?;
        save_job(&tx, network, &job)?;
        save_session(&tx, network, &session)?;
        tx.commit()?;
        Ok(job)
    }

    pub fn mark_started(
        &mut self,
        network: &str,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
    ) -> Result<SchedulerJob> {
        self.update_attempt(network, conversation, trigger, attempt_id, |job, _| {
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
        network: &str,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
        native_id: &str,
    ) -> Result<()> {
        self.update_attempt(
            network,
            conversation,
            trigger,
            attempt_id,
            |job, session| {
                ensure!(
                    matches!(
                        job.state,
                        JobState::Running { .. } | JobState::Claimed { .. }
                    ),
                    "scheduler attempt is no longer active"
                );
                if !job
                    .state
                    .attempt()
                    .context("scheduler attempt missing")?
                    .read_only
                    && job.kind != TriggerKind::AgentTask
                {
                    session.native_id = Some(native_id.into());
                    session.covers = job.basis.clone();
                }
                Ok(())
            },
        )?;
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
        let mut session = load_session(&tx, network, conversation)?;
        let Some(mut job) = load_job(&tx, network, conversation, trigger)? else {
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
            session.pending.retain(|pending| pending != trigger);
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
        save_job(&tx, network, &job)?;
        save_session(&tx, network, &session)?;
        tx.commit()?;
        Ok(Some(job))
    }

    pub(crate) fn fence_manager_permission(
        &mut self,
        network: &str,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
    ) -> Result<()> {
        self.update_attempt(
            network,
            conversation,
            trigger,
            attempt_id,
            |job, session| {
                let attempt = job.state.attempt().context("manager attempt missing")?;
                if attempt.permission_version == session.permission_version {
                    session.permission_version = session
                        .permission_version
                        .checked_add(1)
                        .context("scheduler permission version overflow")?;
                }
                Ok(())
            },
        )?;
        Ok(())
    }

    pub fn manager_attempt_allowed(
        &self,
        network: &str,
        conversation: &str,
        trigger: &str,
        attempt_id: &str,
        write: bool,
    ) -> Result<bool> {
        let session = load_session(&self.connection, network, conversation)?;
        let Some(job) = self.scheduler_job(network, conversation, trigger)? else {
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
            && (!write || !attempt.read_only))
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
        let mut session = load_session(&tx, network, conversation)?;
        let job =
            load_job(&tx, network, conversation, trigger)?.context("scheduler job missing")?;
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

    pub fn record_manager_process(
        &mut self,
        network: &str,
        attempt_id: &str,
        process_group: u32,
    ) -> Result<()> {
        let key = process_key(attempt_id);
        let mut groups =
            local_load::<Vec<u32>>(&self.connection, network, &key)?.unwrap_or_default();
        if !groups.contains(&process_group) {
            groups.push(process_group);
        }
        local_save(&self.connection, network, &key, &groups)
    }

    pub fn pending_checks(&self, network: &str) -> Result<Vec<PendingCheck>> {
        pending_checks_from(&self.connection, network)
    }

    pub fn pending_check(&self, network: &str, check_id: &str) -> Result<Option<PendingCheck>> {
        Ok(self
            .pending_checks(network)?
            .into_iter()
            .find(|check| check.check_id == check_id))
    }

    /// Runtime verifies the user and the stopped process before this.
    pub fn resolve_pending_check(
        &mut self,
        network: &str,
        check_id: &str,
        evidence: &str,
    ) -> Result<PendingCheck> {
        ensure!(
            !evidence.trim().is_empty(),
            "reconciliation evidence is required"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let check = pending_checks_from(&tx, network)?
            .into_iter()
            .find(|check| check.check_id == check_id)
            .context("pending check not found")?;
        tx.execute(
            "DELETE FROM local_values WHERE network_id=? AND key=?",
            params![
                network,
                scheduler_key("pending", &check.conversation_id, &check.trigger)
            ],
        )?;
        tx.commit()?;
        Ok(check)
    }

    pub fn scheduler_maintenance_idle(&self, network: &str) -> Result<()> {
        for session in sessions(&self.connection, network)? {
            ensure!(
                session.pending.is_empty() && session.active.is_none(),
                crate::maintenance::Busy(
                    "scheduler invocation requires completion or reconciliation".into()
                )
            );
        }
        ensure!(
            self.pending_checks(network)?.is_empty(),
            crate::maintenance::Busy("an interrupted invocation awaits reconciliation".into())
        );
        ensure!(
            self.work_records(network)?
                .iter()
                .all(|work| work.state.finished()),
            crate::maintenance::Busy("a dispatched work has not finished".into())
        );
        Ok(())
    }
}
