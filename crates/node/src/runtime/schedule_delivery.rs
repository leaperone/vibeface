//! Local scheduling: the owner Node drains its durable admissions one conversation at a time.
//! A message for another owner waits in the outbox until `peers` delivers it.
use super::*;
use crate::scheduling::{JobOutcome, JobState, PendingCheck, SchedulerJob, SteerCandidate};
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
        self.steer_messages(&network).await?;
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
    /// `turn/steer`, keyed by its message ID. The job is marked steering before the request, so
    /// an unknown result is read back from the thread and never sent again; a refused steer
    /// leaves the message queued for its own reply.
    async fn steer_messages(self: &Arc<Self>, network: &str) -> Result<()> {
        let local = self.identity.device_id()?;
        let steering = self.store.lock().await.steering_jobs()?;
        for job in steering {
            self.settle_steer(network, &job).await?;
        }
        let candidates = self.store.lock().await.steer_candidates(network, &local)?;
        for candidate in candidates {
            let (scope, trigger) = (&candidate.conversation_id, &candidate.trigger);
            if !self.store.lock().await.begin_steer(
                network,
                &local,
                scope,
                trigger,
                &candidate.into,
            )? {
                continue;
            }
            self.publish_changed(network, scope, Changed::Job(trigger));
            match self
                .codex
                .client()
                .request("turn/steer", steer_params(&candidate))
                .await
            {
                Ok(_) => self.store.lock().await.settle_steer(scope, trigger, true)?,
                Err(error) if !error.uncertain => {
                    tracing::info!(
                        event = "agent.turn.steer_refused",
                        conversation_id = scope,
                        trigger,
                        error = error.message,
                        "the message waits for its own reply"
                    );
                    self.store
                        .lock()
                        .await
                        .settle_steer(scope, trigger, false)?;
                    self.schedule_wakeup.notify_one();
                }
                Err(error) => {
                    tracing::warn!(
                        event = "agent.turn.steer_unknown",
                        conversation_id = scope,
                        trigger,
                        error = error.message,
                        "the steer is read back from the thread"
                    );
                }
            }
            self.publish_changed(network, scope, Changed::Job(trigger));
        }
        Ok(())
    }

    /// A steer with an unknown result joined the turn when the thread holds its message. Codex
    /// takes steered input into the running turn, so once that turn ended without it, it never
    /// will and the message is queued again.
    async fn settle_steer(&self, network: &str, job: &SchedulerJob) -> Result<()> {
        let JobState::Steering { into } = &job.state else {
            return Ok(());
        };
        let joined = match self
            .codex
            .client()
            .request(
                "thread/read",
                json!({"threadId":into.thread_id,"includeTurns":true}),
            )
            .await
        {
            Err(error) if error.uncertain => return Ok(()),
            Err(_) => false,
            Ok(reply) => {
                let turns = reply["thread"]["turns"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if turns
                    .iter()
                    .any(|turn| codex_server::sent_by(turn, &job.reply_to))
                {
                    true
                } else if turns
                    .iter()
                    .any(|turn| turn["id"] == into.turn_id && turn["status"] == "inProgress")
                {
                    return Ok(());
                } else {
                    false
                }
            }
        };
        self.store
            .lock()
            .await
            .settle_steer(&job.conversation_id, &job.trigger, joined)?;
        if !joined {
            self.schedule_wakeup.notify_one();
        }
        self.publish_changed(network, &job.conversation_id, Changed::Job(&job.trigger));
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
