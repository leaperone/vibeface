//! Local scheduling: the owner Node drains its durable admissions one conversation at a time.
//! A message for another owner waits in the outbox until `peers` delivers it.
use super::*;
use crate::scheduling::{JobOutcome, JobState, PendingCheck, SchedulerJob};
use futures_util::FutureExt;

fn job_value(job: &SchedulerJob, checks: &[PendingCheck]) -> Result<Value> {
    let mut value = serde_json::to_value(job)?;
    value["read_only"] = json!(job.state.attempt().is_some_and(|attempt| attempt.read_only));
    value["reply_message_id"] = json!(job.message_id);
    value["message_id"] = json!(job.trigger.strip_prefix("message:"));
    value["pending_check"] = json!(checks.iter().any(|check| check.trigger == job.trigger));
    if let JobState::Failed { detail, .. } | JobState::Interrupted { detail, .. } = &job.state {
        value["error"] = json!(detail);
    }
    Ok(value)
}

impl Runtime {
    fn scheduler_manager_metadata(&self) -> Value {
        let settings = self.settings_snapshot();
        let Some(manager) = &settings.manager else {
            return Value::Null;
        };
        let kind = match manager.provider.as_str() {
            "cursor_sdk" => crate::manager::ManagerKind::CursorSdk,
            "codex_sdk" => crate::manager::ManagerKind::CodexSdk,
            _ => return Value::Null,
        };
        let mut capabilities = kind.capabilities();
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
    }

    async fn schedule_tick(self: &Arc<Self>) -> Result<()> {
        if !self.config.roles.contains(&NodeRole::Scheduler) {
            return Ok(());
        }
        let networks = self.store.lock().await.networks()?;
        for network in networks {
            let workers = self.schedule_workers.lock().await.clone();
            // A consumer that ended without a terminal outcome leaves its job claimed; settle it.
            let abandoned = {
                let mut store = self.store.lock().await;
                store.recover_scheduler_jobs(
                    &self.identity,
                    &network,
                    crate::scheduler::boot_id(),
                )?;
                let mut abandoned = Vec::new();
                for scope in store.pending_scheduler_conversations(&network)? {
                    if workers.contains(&(network.clone(), scope.clone())) {
                        continue;
                    }
                    for job in store.scheduler_jobs(&network, &scope)? {
                        if let JobState::Claimed { attempt }
                        | JobState::Running { attempt }
                        | JobState::Cancelling { attempt } = &job.state
                        {
                            abandoned.push((
                                scope.clone(),
                                job.trigger.clone(),
                                attempt.id.clone(),
                            ));
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
                self.publish_changed(&network, &scope, &trigger);
            }
            if self.settings_snapshot().manager.is_some() {
                let scopes = self
                    .store
                    .lock()
                    .await
                    .pending_scheduler_conversations(&network)?;
                for scope in scopes {
                    self.start_schedule_consumer(network.clone(), scope).await;
                }
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
                self.publish_changed(network, scope, trigger);
            }
        }
    }

    pub(super) fn spawn_scheduler(&self, _network: &str, _scope: &str) -> Result<()> {
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
        let attachments: Vec<BlobRef> = match input.get("attachments") {
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
        self.publish_changed(network, &scope, &message.event_hash);
        self.spawn_scheduler(network, &scope)?;
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
        let checks = store.pending_checks(network)?;
        Ok(match store.scheduler_job(network, scope, trigger)? {
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
            .pending_checks(network)?
            .into_iter()
            .filter(|check| check.conversation_id == scope)
            .collect::<Vec<_>>();
        let manager = self.scheduler_manager_metadata();
        let owner = store
            .conversation(network, scope)?
            .and_then(|conversation| conversation.owner);
        let mut jobs = Vec::new();
        for job in store.scheduler_jobs(network, scope)? {
            let mut value = job_value(&job, &checks)?;
            value["manager"] = manager.clone();
            jobs.push(value);
        }
        for entry in store
            .scheduler_outbox(network)?
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
