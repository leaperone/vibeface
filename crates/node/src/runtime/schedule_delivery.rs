//! Local scheduling: the owner Node drains its durable admissions one conversation at a time.
//! A message for another owner waits in the outbox until `peers` delivers it.
use super::*;
use crate::scheduling::{
    JobOutcome, JobState, PendingCheck, SchedulerJob, SteerCandidate, SteerOutcome, SteerTarget,
};
use futures_util::FutureExt;

fn job_value(job: &SchedulerJob, checks: &[PendingCheck]) -> Result<Value> {
    let mut value = serde_json::to_value(job)?;
    value["reply_message_id"] = json!(job.message_id);
    value["message_id"] = json!(job.trigger.strip_prefix("message:"));
    value["pending_check"] = json!(checks.iter().any(|check| check.trigger == job.trigger));
    if let JobState::Failed { detail, .. } | JobState::Interrupted { detail, .. } = &job.state {
        let detail = crate::logging::scrub(detail);
        value["detail"] = json!(detail);
        value["error"] = json!(detail);
    }
    Ok(value)
}

impl Runtime {
    fn scheduler_manager_metadata(&self) -> Value {
        let Some(manager) = &self.manager() else {
            return Value::Null;
        };
        let mut capabilities = manager.provider.capabilities();
        capabilities["configured"] = json!(true);
        json!({"provider":manager.provider,"model":manager.model,"capabilities":capabilities})
    }

    pub(super) fn start_schedule_delivery(self: &Arc<Self>) {
        let node = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            loop {
                tokio::select! {
                    _ = tick.tick() => {},
                    _ = node.schedule_wakeup.notified() => {},
                }
                let _work = node.maintenance.enter_when_open().await;
                if let Err(error) = node.schedule_tick().await {
                    tracing::warn!(%error, "durable scheduling reconciliation is pending");
                }
            }
        });
        let node = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                let _work = node.maintenance.enter_when_open().await;
                if let Err(error) = node.fire_schedules().await {
                    tracing::warn!(%error, "schedule runs are pending");
                }
            }
        });
    }

    /// Queues the schedules that are due in conversations this Node schedules.
    async fn fire_schedules(self: &Arc<Self>) -> Result<()> {
        if self.manager().is_none() {
            return Ok(());
        }
        let Some(network) = self.store.lock().await.network()? else {
            return Ok(());
        };
        let admitted =
            self.store
                .lock()
                .await
                .admit_due_schedules(&self.identity, &network, now_ms())?;
        for (scope, trigger) in admitted {
            self.publish_changed(&network, &scope, Changed::Job(&trigger));
            self.schedule_wakeup.notify_one();
        }
        Ok(())
    }

    async fn schedule_tick(self: &Arc<Self>) -> Result<()> {
        let Some(network) = self.store.lock().await.network()? else {
            return Ok(());
        };
        // A conversation that moved to a newer owner stops its turn here; a cancel the SDK does
        // not confirm ends as interrupted and waits for reconciliation like any other.
        let moved = self
            .store
            .lock()
            .await
            .fence_moved_turns(&network, &self.identity.device_id()?)?;
        for (scope, trigger, attempt) in moved {
            tracing::info!(
                event = "agent.turn.fenced",
                conversation_id = scope,
                trigger,
                attempt,
                "the conversation moved to another owner; its turn here stops"
            );
            self.fence_manager_attempt(&attempt).await?;
            self.publish_changed(&network, &scope, Changed::Job(&trigger));
        }
        let workers = self.schedule_workers.lock().await.clone();
        // A consumer that ended without a terminal outcome leaves its job claimed; settle it.
        let abandoned = {
            let mut store = self.store.lock().await;
            store.recover_scheduler_jobs(&self.identity, &network, crate::scheduler::boot_id())?;
            let mut abandoned = Vec::new();
            for scope in store.pending_scheduler_conversations()? {
                if workers.contains(&(network.clone(), scope.clone())) {
                    continue;
                }
                for job in store.scheduler_jobs(&scope)? {
                    if let JobState::Claimed { attempt }
                    | JobState::Running { attempt }
                    | JobState::Cancelling { attempt } = &job.state
                    {
                        abandoned.push((scope.clone(), job.trigger.clone(), attempt.id.clone()));
                    }
                }
            }
            abandoned
        };
        for (scope, trigger, attempt) in abandoned {
            self.fence_manager_attempt(&attempt).await?;
            self.store.lock().await.finish_job(
                &self.identity,
                &network,
                &scope,
                &trigger,
                &attempt,
                JobOutcome::Interrupted {
                    detail: "回复进程意外结束，结果待核对。".into(),
                    process_unconfirmed: true,
                },
                "",
                now_ms(),
            )?;
            self.finish_manager_attempt(&attempt).await;
            self.publish_changed(&network, &scope, Changed::Job(&trigger));
        }
        // After recovery, so a reply left running by an earlier process is never a target.
        self.steer_messages(&network).await?;
        if self.manager().is_some() {
            let scopes = self.store.lock().await.pending_scheduler_conversations()?;
            for scope in scopes {
                self.start_schedule_consumer(network.clone(), scope).await;
            }
        }
        Ok(())
    }

    async fn start_schedule_consumer(self: &Arc<Self>, network: String, scope: String) {
        let key = (network.clone(), scope.clone());
        if !self.schedule_workers.lock().await.insert(key.clone()) {
            return;
        }
        let node = self.clone();
        tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(node.drain_schedule(&network, &scope))
                .catch_unwind()
                .await;
            match result {
                Ok(Err(error)) => tracing::warn!(%error, "conversation scheduling is pending"),
                Err(_) => {
                    tracing::error!("conversation scheduler panicked; durable state retained")
                }
                _ => {}
            }
            node.schedule_workers.lock().await.remove(&key);
            // The periodic durable scan covers enqueue racing this final empty check.
        });
    }

    async fn drain_schedule(self: &Arc<Self>, network: &str, scope: &str) -> Result<()> {
        loop {
            let access = Access {
                network_id: network.into(),
                human: false,
                conversation_id: Some(scope.into()),
                manager: None,
            };
            let Some(result) = crate::scheduler::run(self.clone(), access, scope.into()).await?
            else {
                return Ok(());
            };
            if let Some(trigger) = result["trigger"].as_str() {
                self.publish_changed(network, scope, Changed::Job(trigger));
            }
        }
    }

    /// A queued user message joins the running Codex reply of its conversation through
    /// `turn/steer`, keyed by its message ID. The job is marked steering before the request and
    /// stays so until that reply's attempt ended; it is then settled from the thread and never
    /// sent again. A refused steer leaves the message queued for its own reply.
    async fn steer_messages(self: &Arc<Self>, network: &str) -> Result<()> {
        let local = self.identity.device_id()?;
        let boot = crate::scheduler::boot_id();
        let steering = self.store.lock().await.steering_jobs()?;
        for job in steering {
            self.settle_steer(network, &job).await?;
        }
        let candidates = self
            .store
            .lock()
            .await
            .steer_candidates(network, &local, boot)?;
        for candidate in candidates {
            let (scope, trigger) = (&candidate.conversation_id, &candidate.trigger);
            if !self.store.lock().await.begin_steer(
                network,
                &local,
                boot,
                scope,
                trigger,
                &candidate.into,
            )? {
                continue;
            }
            self.publish_changed(network, scope, Changed::Job(trigger));
            let steer = self
                .codex
                .client()
                .request("turn/steer", steer_params(&candidate));
            match tokio::time::timeout(STEER_WAIT, steer).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) if !error.uncertain => {
                    tracing::info!(
                        event = "agent.turn.steer_refused",
                        conversation_id = scope,
                        trigger,
                        error = error.message,
                        "the message waits for its own reply"
                    );
                    self.store.lock().await.settle_steer(
                        &self.identity,
                        network,
                        scope,
                        trigger,
                        JobState::Queued,
                    )?;
                    self.schedule_wakeup.notify_one();
                }
                Ok(Err(_)) | Err(_) => {
                    tracing::warn!(
                        event = "agent.turn.steer_unknown",
                        conversation_id = scope,
                        trigger,
                        "the steer is read back from the thread once the reply ends"
                    );
                }
            }
            self.publish_changed(network, scope, Changed::Job(trigger));
        }
        Ok(())
    }

    /// Settles a steer once the reply it targeted ended. Codex takes steered input only into a
    /// running turn, so the thread says where the message went: in a completed turn it was
    /// answered, in one that stopped it was not; absent from the thread while the target turn
    /// ended, it never arrived and waits for its own reply. Only a read that succeeded moves it
    /// back to the queue; a thread that stays unreadable, or keeps showing the turn running,
    /// ends it as unknown, never resent.
    async fn settle_steer(&self, network: &str, job: &SchedulerJob) -> Result<()> {
        let JobState::Steering {
            into,
            unread_since_ms,
            running_since_ms,
        } = &job.state
        else {
            return Ok(());
        };
        if self
            .store
            .lock()
            .await
            .steer_host_active(&job.conversation_id, &into.attempt_id)?
        {
            return Ok(());
        }
        let read = self.codex.client().request(
            "thread/read",
            json!({"threadId":into.thread_id,"includeTurns":true}),
        );
        let reply = tokio::time::timeout(STEER_WAIT, read)
            .await
            .ok()
            .and_then(Result::ok);
        let turns = reply
            .as_ref()
            .and_then(|reply| reply["thread"]["turns"].as_array());
        let Some(next) = next_steer_state(
            turns.map(Vec::as_slice),
            &job.reply_to,
            into,
            *unread_since_ms,
            *running_since_ms,
            now_ms(),
        ) else {
            return Ok(());
        };
        let settled = !matches!(next, JobState::Steering { .. });
        self.store.lock().await.settle_steer(
            &self.identity,
            network,
            &job.conversation_id,
            &job.trigger,
            next,
        )?;
        if settled {
            self.schedule_wakeup.notify_one();
            self.publish_changed(network, &job.conversation_id, Changed::Job(&job.trigger));
        }
        Ok(())
    }

    pub(super) fn wake_scheduler(&self) -> Result<()> {
        let _work = self.maintenance.enter()?;
        self.schedule_wakeup.notify_one();
        self.outbox_wakeup.notify_one();
        Ok(())
    }

    /// `message_id` is the idempotency key: a retried send returns the original admission.
    pub(super) async fn accept_chat_message(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        ensure!(access.human, "agents cannot recursively invoke managers");
        let network = &access.network_id;
        let scope = id(input, "conversation_id")?;
        self.require_scope(access, &scope).await?;
        let message_id = id(input, "message_id")?;
        let report_results = input["report_results"].as_bool().unwrap_or(true);
        let attachments: Vec<Attachment> = match input.get("attachments") {
            Some(value) => serde_json::from_value(value.clone()).context("invalid attachments")?,
            None => vec![],
        };
        let (message, receipt) = self.store.lock().await.accept_message(
            &self.identity,
            network,
            &scope,
            &message_id,
            field(input, "text")?,
            attachments,
            report_results,
            attestation(input)?,
            now_ms(),
        )?;
        self.publish_changed(network, &scope, Changed::Event(&message.event_hash));
        self.wake_scheduler()?;
        Ok(
            json!({"accepted":true,"state":receipt.state,"trigger":receipt.trigger,
            "message_id":message_id,"receipt":receipt}),
        )
    }

    /// The user asks this Node, now scheduling the conversation, to reply to messages a former
    /// owner left unanswered, or with `dismiss` to reply to none of them. Nothing replays on its
    /// own; a retry queues or records nothing twice.
    pub(super) async fn settle_unanswered(
        &self,
        access: &Access,
        input: &Value,
        dismiss: bool,
    ) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        ensure!(access.human, "only the user asks for these replies");
        let network = &access.network_id;
        let scope = id(input, "conversation_id")?;
        self.require_scope(access, &scope).await?;
        let message_ids: Vec<String> = serde_json::from_value(input["message_ids"].clone())
            .context("message_ids must be a list of message IDs")?;
        let local = self.identity.device_id()?;
        let mut store = self.store.lock().await;
        let receipts = if dismiss {
            store.dismiss_unanswered(network, &local, &scope, &message_ids)?
        } else {
            store.reply_unanswered(network, &local, &scope, &message_ids)?
        };
        drop(store);
        for receipt in &receipts {
            self.publish_changed(network, &scope, Changed::Job(&receipt.trigger));
        }
        self.schedule_wakeup.notify_one();
        Ok(json!({"receipts":receipts}))
    }

    pub(super) async fn manager_status(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
    ) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        let mut store = self.store.lock().await;
        store.recover_scheduler_jobs(&self.identity, network, crate::scheduler::boot_id())?;
        let checks = store.pending_checks()?;
        Ok(match store.scheduler_job(scope, trigger)? {
            Some(job) => json!({"state":job.state.name(),"receipt":job.receipt(network),
                "job":job_value(&job,&checks)?,"manager":self.scheduler_manager_metadata()}),
            None => json!({"state":"not_found","trigger":trigger}),
        })
    }

    /// The conversation's local jobs, waiting outbox entries and their pending checks.
    pub(super) async fn scheduler_snapshot_jobs(
        &self,
        network: &str,
        scope: &str,
    ) -> Result<(Vec<Value>, Vec<PendingCheck>)> {
        let store = self.store.lock().await;
        let checks = store
            .pending_checks()?
            .into_iter()
            .filter(|check| check.conversation_id == scope)
            .collect::<Vec<_>>();
        let manager = self.scheduler_manager_metadata();
        let owner = store
            .conversation(network, scope)?
            .and_then(|conversation| conversation.owner);
        let mut jobs = Vec::new();
        for job in store.scheduler_jobs(scope)? {
            let mut value = job_value(&job, &checks)?;
            value["manager"] = manager.clone();
            jobs.push(value);
        }
        for entry in store
            .scheduler_outbox()?
            .into_iter()
            .filter(|entry| entry.conversation_id == scope)
        {
            jobs.push(
                json!({"trigger":entry.trigger,"owner":owner,"conversation_id":scope,
                "state":"pending_delivery","message_id":entry.trigger.strip_prefix("message:")}),
            );
        }
        Ok((jobs, checks))
    }
}

/// The user's new message as the running turn reads it.
fn steer_params(candidate: &SteerCandidate) -> Value {
    let message = json!({"message_id":candidate.message_id,"text":candidate.text});
    let text = format!(
        "The user sent another message in this conversation while you were replying. Take it into account and answer it in this same reply.\n\n{message}"
    );
    json!({"threadId":candidate.into.thread_id,"expectedTurnId":candidate.into.turn_id,
        "clientUserMessageId":candidate.message_id,
        "input":[{"type":"text","text":text,"text_elements":[]}]})
}

/// How long one steer or read waits for the App Server inside the scheduling tick; a steer that
/// times out is unknown and read back later.
const STEER_WAIT: Duration = Duration::from_secs(5);
/// How long a steered message waits after its reply ended for its thread to become readable, or
/// for the turn it shows running to end.
const STEER_SETTLE_LIMIT_MS: i64 = 10 * 60 * 1000;

/// Where a message steered into `into` went, from its thread's turns: `None` while the read does
/// not show its target turn, `Some(None)` while the message may still arrive.
fn steer_settlement(
    turns: &[Value],
    message_id: &str,
    into: &SteerTarget,
) -> Option<Option<JobState>> {
    let target = turns.iter().find(|turn| turn["id"] == into.turn_id);
    let Some(turn) = turns
        .iter()
        .find(|turn| codex_server::sent_by(turn, message_id))
    else {
        return target.map(|turn| (turn["status"] != "inProgress").then_some(JobState::Queued));
    };
    let outcome = match turn["status"].as_str() {
        Some("inProgress") => return Some(None),
        Some("completed") if turn["id"] == into.turn_id => SteerOutcome::Answered,
        _ => SteerOutcome::Unfinished,
    };
    Some(Some(JobState::Steered {
        into: into.clone(),
        outcome,
    }))
}

/// The state a steering message moves to after a read of its thread (`None` when it failed), or
/// `None` to leave it. A successful read restarts the unreadable clock; a failed read says nothing
/// about the turn, so the running clock keeps going.
fn next_steer_state(
    turns: Option<&[Value]>,
    message_id: &str,
    into: &SteerTarget,
    unread_since_ms: Option<i64>,
    running_since_ms: Option<i64>,
    now: i64,
) -> Option<JobState> {
    let steering = |unread_since_ms, running_since_ms| JobState::Steering {
        into: into.clone(),
        unread_since_ms,
        running_since_ms,
    };
    let (since, next) = match turns.and_then(|turns| steer_settlement(turns, message_id, into)) {
        Some(Some(next)) => return Some(next),
        Some(None) => {
            let since = running_since_ms.unwrap_or(now);
            (since, steering(None, Some(since)))
        }
        None => {
            let since = unread_since_ms.unwrap_or(now);
            (since, steering(Some(since), running_since_ms))
        }
    };
    if now.saturating_sub(since) >= STEER_SETTLE_LIMIT_MS {
        return Some(JobState::Steered {
            into: into.clone(),
            outcome: SteerOutcome::Unknown,
        });
    }
    (next != steering(unread_since_ms, running_since_ms)).then_some(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn into() -> SteerTarget {
        SteerTarget {
            attempt_id: "a".into(),
            thread_id: "t".into(),
            turn_id: "r".into(),
        }
    }

    fn turn(id: &str, status: &str, sent: bool) -> Value {
        let items = if sent {
            json!([{"type":"userMessage","clientId":"m"}])
        } else {
            json!([])
        };
        json!({"id":id,"status":status,"items":items})
    }

    #[test]
    fn a_steered_message_settles_only_from_what_its_thread_shows() {
        let into = into();
        let steered = |outcome| {
            Some(Some(JobState::Steered {
                into: into.clone(),
                outcome,
            }))
        };
        let settle = |turns: &[Value]| steer_settlement(turns, "m", &into);
        assert_eq!(settle(&[turn("r", "inProgress", true)]), Some(None));
        assert_eq!(settle(&[turn("r", "inProgress", false)]), Some(None));
        assert_eq!(
            settle(&[turn("r", "completed", true)]),
            steered(SteerOutcome::Answered)
        );
        assert_eq!(
            settle(&[turn("r", "interrupted", true)]),
            steered(SteerOutcome::Unfinished)
        );
        assert_eq!(
            settle(&[turn("r", "completed", false), turn("x", "completed", true)]),
            steered(SteerOutcome::Unfinished)
        );
        assert_eq!(
            settle(&[turn("r", "interrupted", false)]),
            Some(Some(JobState::Queued))
        );
        // A read that does not show the target turn proves nothing.
        assert_eq!(settle(&[]), None);
        assert_eq!(settle(&[turn("x", "completed", false)]), None);
    }

    #[test]
    fn a_readable_thread_restarts_the_unreadable_clock() {
        let into = into();
        let steering = |unread, running| {
            Some(JobState::Steering {
                into: into.clone(),
                unread_since_ms: unread,
                running_since_ms: running,
            })
        };
        let running = [turn("r", "inProgress", false)];
        let limit = STEER_SETTLE_LIMIT_MS;
        let next = |turns, unread, since_running, now| {
            next_steer_state(turns, "m", &into, unread, since_running, now)
        };
        assert_eq!(next(None, None, None, 5), steering(Some(5), None));
        assert_eq!(next(None, Some(5), None, 6), None);
        assert_eq!(
            next(Some(&running), Some(5), None, 7),
            steering(None, Some(7))
        );
        // One failure after a good read starts a new wait instead of ending as unknown.
        assert_eq!(
            next(None, None, Some(7), limit + 6),
            steering(Some(limit + 6), Some(7))
        );
        assert_eq!(
            next(None, Some(5), None, limit + 5),
            Some(JobState::Steered {
                into: into.clone(),
                outcome: SteerOutcome::Unknown,
            })
        );
    }

    #[test]
    fn a_turn_still_running_after_its_reply_ended_settles_as_unknown() {
        let into = into();
        let unknown = Some(JobState::Steered {
            into: into.clone(),
            outcome: SteerOutcome::Unknown,
        });
        let limit = STEER_SETTLE_LIMIT_MS;
        for running in [
            [turn("r", "inProgress", false)],
            [turn("r", "inProgress", true)],
        ] {
            let next = |unread, since_running, now| {
                next_steer_state(Some(&running), "m", &into, unread, since_running, now)
            };
            assert_eq!(next(None, Some(5), limit + 4), None);
            assert_eq!(next(None, Some(5), limit + 5), unknown);
            // Failed reads in between do not restart the wait.
            assert_eq!(next(Some(limit), Some(5), limit + 5), unknown);
        }
    }
}
