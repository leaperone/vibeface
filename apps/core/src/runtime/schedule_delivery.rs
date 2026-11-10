//! A message for another owner waits in the outbox until `peers` delivers it.
use super::*;
use crate::scheduling::{
    JobOutcome, JobState, PendingCheck, SchedulerJob, SessionKey, SteerOutcome, SteerTarget,
};
use executor::{SteerAck, SteerTrace};
use futures_util::FutureExt;
use vibeface_bridge::failure::{ExecFailure, FailureKind};

/// The jobs a chat shows. An agent task runs in its bot's conversation, but it is that work: its
/// card and details show it, and the chat never takes it for a reply or stops it as one.
fn chat_jobs(jobs: Vec<SchedulerJob>) -> Vec<SchedulerJob> {
    jobs.into_iter()
        .filter(|job| job.scope == crate::scheduling::SessionScope::Main)
        .collect()
}

fn job_value(job: &SchedulerJob, checks: &[PendingCheck]) -> Result<Value> {
    let mut value = serde_json::to_value(job)?;
    value["lane"] = json!(job.kind.lane());
    value["reply_message_id"] = json!(job.message_id);
    value["message_id"] = json!(job.trigger.strip_prefix("message:"));
    value["pending_check"] = json!(
        checks
            .iter()
            .any(|check| check.conversation_id == job.conversation_id
                && check.trigger == job.trigger)
    );
    value["retryable"] = json!(job.retryable() && value["pending_check"] != true);
    if let JobState::Failed { failure, .. }
    | JobState::Interrupted { failure, .. }
    | JobState::Reconciling { failure, .. } = &job.state
    {
        value["failure"]["detail"] = json!(crate::logging::scrub(&failure.detail));
    }
    Ok(value)
}

pub(super) struct SchedulerProjection {
    local: String,
    manager: Option<Provider>,
    limits: crate::config::SchedulingLimits,
    pending: BTreeSet<Provider>,
}
impl SchedulerProjection {
    fn hold_reason(
        &self,
        store: &Store,
        network: &str,
        job: &SchedulerJob,
    ) -> Result<Option<crate::scheduling::QueueHoldReason>> {
        if job.state != JobState::Queued {
            return Ok(None);
        }
        let mut provider = self.manager
            .or_else(|| job.runtime.as_ref().map(|runtime| runtime.provider));
        if provider.is_none()
            && let crate::scheduling::SessionScope::Task { work_id } = &job.scope
        {
            provider = store
                .work(network, work_id)?
                .map(|work| work.resolved.provider);
        }
        let Some(provider) = provider else {
            return Ok(Some(
                crate::scheduling::QueueHoldReason::BackendConfigPending,
            ));
        };
        let hold = store.scheduler_job_hold(
            network,
            &self.local,
            job,
            provider,
            &self.limits,
        )?;
        Ok(hold.or_else(|| {
            self.pending.contains(&provider)
                .then_some(crate::scheduling::QueueHoldReason::BackendConfigPending)
        }))
    }

    pub(super) fn scope_job(
        &self,
        store: &Store,
        network: &str,
        job: &SchedulerJob,
    ) -> Result<Value> {
        let owner = store
            .conversation(network, &job.conversation_id)?
            .and_then(|conversation| conversation.owner);
        Ok(
            json!({"conversation_id":job.conversation_id,"trigger":job.trigger,"kind":job.kind,
            "scope":job.scope,"state":job.state.name(),"lane":job.kind.lane(),"sequence":job.sequence,
            "source_node":self.local.clone(),"owner":owner,"message_id":job.trigger.strip_prefix("message:"),
            "reply_message_id":job.message_id,"hold_reason":self.hold_reason(store,network,job)?}),
        )
    }

    pub(super) fn job(
        &self,
        store: &Store,
        network: &str,
        job: &SchedulerJob,
        checks: &[PendingCheck],
    ) -> Result<Value> {
        let mut value = job_value(job, checks)?;
        let local = self.local.clone();
        value["source_node"] = json!(local);
        value["owner_epoch"] = json!(
            store
                .conversation(network, &job.conversation_id)?
                .map_or(0, |conversation| conversation.owner_epoch)
        );
        let hold = self.hold_reason(store, network, job)?;
        value["hold_reason"] = json!(hold);
        value["context"] = json!(store.scheduler_job_context_projection(job)?);
        if let Some(fields) = value.as_object_mut() {
            fields.remove("basis");
        }
        Ok(value)
    }

}

impl Runtime {
    async fn prepare_scope_input(&self, network: &str, key: &SessionKey) -> Result<bool> {
        let input = {
            let network = network.to_owned(); let key = key.clone();
            self.store.call("prepare_scope_input", move |store| {
            let mut jobs = store
                .scheduler_jobs(&key.conversation_id)?
                .into_iter()
                .filter(|job| job.session_key() == key && job.state == JobState::Queued)
                .collect::<Vec<_>>();
            jobs.sort_by(|left, right| left.priority().cmp(&right.priority()));
            Ok(jobs.first()
                .map(|job| store.queued_workspace_input(&network, &job.reply_to))
                .transpose()?
                .flatten())
            }).await?
        };
        let Some(input) = input else {
            return Ok(true);
        };
        let events = self
            .store.call("input_attachment_events", { let network = network.to_owned();
                let message_id = input.input.message_id.to_owned();
                move |store| store.input_attachment_events(&network, &message_id) }).await?;
        if !self
            .fetch_turn_attachments(network, events.iter())
            .await
            .is_empty()
        {
            return Ok(false);
        }
        self.store.call("scope_input_blobs", move |store| {
        for attachment in &input.input.attachments {
            if !store.has_blob(&attachment.blob())? {
                return Ok(false);
            }
        }
        Ok(true)
        }).await
    }

    async fn workspace_execution_roots(&self) -> Result<Vec<String>> {
        let snapshot = self.settings_snapshot();
        let mut paths = snapshot.roots;
        if let Some(manager) = snapshot.manager {
            paths.push(manager.workspace);
        }
        let mut roots = paths
            .into_iter()
            .map(|path| {
                path.canonicalize()
                    .map(|path| path.to_string_lossy().into_owned())
                    .map_err(Into::into)
            })
            .collect::<Result<Vec<_>>>()?;
        roots.sort();
        roots.dedup();
        Ok(roots)
    }

    async fn reconcile_workspace_jobs(self: &Arc<Self>, network: &str) -> Result<()> {
        let before = self.store.call("version_vector", { let network = network.to_owned();
                move |store| store.version_vector(&network) }).await?;
        let jobs = self.store.call("scheduler_jobs_all", |store| store.scheduler_jobs_all()).await?;
        for job in jobs {
            let crate::scheduling::SessionScope::WorkspaceTask { task_id } = &job.scope else {
                continue;
            };
            let task = self.store.call("workspace_task", { let network = network.to_owned();
                let task_id = task_id.to_owned();
                move |store| store.workspace_task(&network, &task_id) }).await?;
            let Some(task) = task else {
                continue;
            };
            let active_fenced = matches!(
                job.state,
                JobState::Claimed { .. } | JobState::Running { .. } | JobState::Steering { .. }
            ) && !self
                .store.call("workspace_task_input_allowed", { let network = network.to_owned();
                let task_id = task_id.to_owned();
                move |store| store.workspace_task_input_allowed(&network, &task_id) }).await?;
            if active_fenced
                || (task.action != crate::workspace::TaskAction::Continue
                    && matches!(
                        job.state,
                        JobState::Queued
                            | JobState::Claimed { .. }
                            | JobState::Running { .. }
                            | JobState::Steering { .. }
                    ))
            {
                let stopped = self.store.call("request_manager_cancel", { let identity = self.identity.to_owned();
                let network = network.to_owned();
                let conversation_id = job.conversation_id.to_owned();
                let trigger = job.trigger.to_owned();
                move |store| store.request_manager_cancel(&identity, &network, &conversation_id, &trigger) }).await?;
                if let Some(JobState::Cancelling { attempt }) = stopped.map(|job| job.state) {
                    let node = self.clone();
                    self.background_tasks.spawn(async move {
                        if let Err(error) = node.fence_manager_attempt(&attempt.id).await {
                            tracing::warn!(event="workspace.manager_fence_pending",%error,"manager cancellation remains pending");
                        }
                    })?;
                }
                let works = self
                    .store.call("works", { let network = network.to_owned();
                let conversation_id = job.conversation_id.to_owned();
                move |store| store.works(&network, &conversation_id) }).await?;
                for work in works {
                    if work.outcome.is_none()
                        && self.store.call("scheduler_work_in_scope", { let network = network.to_owned();
                let job = job.to_owned();
                let work_id = work.work_id.to_owned();
                move |store| store.scheduler_work_in_scope(&network, &job, &work_id) }).await?
                    {
                        let node = self.clone();
                        let network = network.to_owned();
                        let conversation = job.conversation_id.clone();
                        self.background_tasks.spawn(async move {
                            if let Err(error) = node.stop_work(&network, &conversation, &work.work_id).await {
                                tracing::warn!(event="workspace.work_stop_pending",%error,"work cancellation remains pending");
                            }
                        })?;
                    }
                }
                self.publish_changed(network, &job.conversation_id, Changed::Job(&job.trigger));
            }
            if !matches!(
                job.state,
                JobState::Steering { .. } | JobState::Steered { .. }
            ) && let Err(error) = self.store.call("settle_workspace_assignment", { let identity = self.identity.to_owned();
                let network = network.to_owned();
                let job = job.to_owned();
                move |store| store.settle_workspace_assignment(&identity, &network, &job, now_ms()) }).await {
                tracing::warn!(event="workspace.assignment.reconcile_pending",%error,"assignment lifecycle remains pending");
            }
        }
        let assignments = self
            .store.call("workspace_active_assignments", { let network = network.to_owned();
                move |store| store.workspace_active_assignments(&network) }).await?;
        for assignment in assignments {
            if assignment.execution_node != self.identity.device_id()? {
                continue;
            }
            if let Some(work_id) = &assignment.work_id {
                let task = self
                    .store.call("workspace_task", { let network = network.to_owned();
                let task_id = assignment.task_id.to_owned();
                move |store| store.workspace_task(&network, &task_id) }).await?
                    .context("assignment task missing")?;
                if task.action != crate::workspace::TaskAction::Continue
                    || !self
                        .store.call("workspace_task_input_allowed", { let network = network.to_owned();
                let task_id = assignment.task_id.to_owned();
                move |store| store.workspace_task_input_allowed(&network, &task_id) }).await?
                {
                    let node = self.clone();
                    let network = network.to_owned();
                    let work_id = work_id.clone();
                    let conversation = self
                        .store.call("work", { let network = network.as_str().to_owned();
                let work_id = work_id.to_owned();
                move |store| store.work(&network, &work_id) }).await?
                        .map(|work| work.conversation_id);
                    if let Some(conversation) = conversation {
                        self.background_tasks.spawn(async move {
                            if let Err(error) = node.stop_work(&network, &conversation, &work_id).await {
                                tracing::warn!(event="workspace.assignment_stop_pending",%error,"assignment cancellation remains pending");
                            }
                        })?;
                    }
                }
                self.store.call("settle_workspace_work_assignment", { let identity = self.identity.to_owned();
                let network = network.to_owned();
                let assignment = assignment.to_owned();
                move |store| store.settle_workspace_work_assignment(&identity, &network, &assignment, now_ms()) }).await?;
            }
        }
        if self.store.call("version_vector", { let network = network.to_owned();
                move |store| store.version_vector(&network) }).await? != before {
            self.publish_changed(network, crate::workspace::WORKSPACE_SCOPE, Changed::Reload);
            self.schedule_wakeup.notify_one();
        }
        Ok(())
    }
    pub(super) async fn scheduler_context_observation(
        &self,
        network: &str,
        key: &SessionKey,
    ) -> Result<Option<crate::scheduling::ContextProjection>> {
        let desired = crate::scheduler::session_configuration(self, network, key)
            .await
            .ok();
        let key = key.clone();
        self.store.read("scheduler_context_observation", move |store| {
        let mut context = store.scheduler_scoped_context_projection(&key)?;
        if let Some(context) = &mut context {
            context.session_status = desired
                .as_ref()
                .map(|config| store.scheduler_session_status(&key, Some(config)))
                .transpose()?;
        }
        Ok(context)
        }).await
    }

    pub(super) fn scheduler_projection(&self) -> Result<SchedulerProjection> {
        let providers = self.providers();
        Ok(SchedulerProjection {
            local: self.identity.device_id()?,
            manager: self.manager().map(|manager| manager.provider),
            limits: self.config.scheduling.clone(),
            pending: Provider::ALL.into_iter()
                .filter(|provider| self.provider_configuration_pending(*provider, &providers))
                .collect(),
        })
    }

    pub(super) fn scheduler_manager_metadata(&self) -> Value {
        let Some(manager) = &self.manager() else {
            return Value::Null;
        };
        let mut capabilities = manager.provider.capabilities();
        capabilities["configured"] = json!(true);
        json!({"provider":manager.provider,"model":manager.model,"capabilities":capabilities})
    }

    pub(super) fn start_schedule_delivery(self: &Arc<Self>) {
        let node = self.clone();
        if let Err(error) = self.background_tasks.spawn(async move {
            if let Some(manager) = node.manager() {
                if manager.provider.on_app_server() {
                    node.codex.ready().await;
                }
                node.probe_provider(manager.provider).await;
            }
        }) {
            tracing::debug!(%error, "provider readiness probe skipped while the Node is shutting down");
        }
        let node = self.clone();
        if let Err(error) = self.background_tasks.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            loop {
                tokio::select! {
                    _ = tick.tick() => {},
                    _ = node.schedule_wakeup.notified() => {},
                }
                let _work = node.maintenance.enter_when_open().await;
                if let Err(error) = node.schedule_tick().await {
                    tracing::warn!(event = "schedule.reconcile_pending", %error, "durable scheduling reconciliation is pending");
                }
            }
        }) {
            tracing::debug!(%error, "schedule reconciliation worker skipped while the Node is shutting down");
        }
        let node = self.clone();
        if let Err(error) = self.background_tasks.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                let _work = node.maintenance.enter_when_open().await;
                if let Err(error) = node.fire_schedules().await {
                    tracing::warn!(event = "schedule.runs_pending", %error, "schedule runs are pending");
                }
            }
        }) {
            tracing::debug!(%error, "scheduled-run worker skipped while the Node is shutting down");
        }
    }

    /// Queues the schedules that are due in conversations this Node schedules.
    async fn fire_schedules(self: &Arc<Self>) -> Result<()> {
        if self.manager().is_none() || !self.writable() {
            return Ok(());
        }
        let Some(network) = self.store.call("network", |store| store.network()).await? else {
            return Ok(());
        };
        let admitted =
            self.store.call("admit_due_schedules", { let identity = self.identity.to_owned();
                let network = network.to_owned();
                move |store| store.admit_due_schedules(&identity, &network, now_ms()) }).await?;
        for (scope, trigger) in admitted {
            self.publish_changed(&network, &scope, Changed::Job(&trigger));
            self.schedule_wakeup.notify_one();
        }
        Ok(())
    }

    /// A non-writable Node only stops and reads existing turns; it never admits inputs.
    pub(super) async fn schedule_tick(self: &Arc<Self>) -> Result<()> {
        if !self.writable() {
            return self.reconcile_execution_fence().await;
        }
        let Some(network) = self.store.call("network", |store| store.network()).await? else {
            return Ok(());
        };
        self.finish_incompatible_tasks(&network, None).await?;
        self.reconcile_workspace_jobs(&network).await?;
        // A conversation that moved to a newer owner stops its turn here; a cancel the SDK does
        // not confirm ends as interrupted and waits for reconciliation like any other.
        let moved = self
            .store.call("fence_moved_turns", { let network = network.to_owned();
                let local_id = self.identity.device_id()?.to_owned();
                move |store| store.fence_moved_turns(&network, &local_id) }).await?;
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
            let identity = self.identity.clone(); let network = network.clone();
            self.store.call("recover_scheduler_jobs", move |store| {
            store.recover_scheduler_jobs(&identity, &network, crate::scheduler::boot_id())?;
            let mut abandoned = Vec::new();
            for key in store.pending_scheduler_sessions()? {
                if workers.contains(&(network.clone(), key.clone())) {
                    continue;
                }
                for job in store.scheduler_jobs(&key.conversation_id)? {
                    if job.session_key() != key {
                        continue;
                    }
                    if let JobState::Claimed { attempt }
                    | JobState::Running { attempt }
                    | JobState::Cancelling { attempt } = &job.state
                    {
                        abandoned.push((
                            key.conversation_id.clone(),
                            job.trigger.clone(),
                            attempt.id.clone(),
                        ));
                    }
                }
            }
            Ok(abandoned)
            }).await?
        };
        for (scope, trigger, attempt) in abandoned {
            self.fence_manager_attempt(&attempt).await?;
            let requeued = self.store.call("requeue_abandoned_scheduler_claim", { let scope = scope.to_owned();
                let trigger = trigger.to_owned();
                let attempt = attempt.to_owned();
                move |store| store.requeue_abandoned_scheduler_claim(&scope, &trigger, &attempt, crate::scheduler::boot_id()) }).await?;
            if !requeued {
                self.store.call("finish_job", { let identity = self.identity.to_owned();
                let network = network.to_owned();
                let scope = scope.to_owned();
                let trigger = trigger.to_owned();
                let attempt = attempt.to_owned();
                move |store| store.finish_job(&identity, &network, &scope, &trigger, &attempt, JobOutcome::Interrupted {
                        failure: ExecFailure::new(
                            FailureKind::Lost,
                            "the reply process ended unexpectedly; its result is unverified",
                        ),
                        process_unconfirmed: true,
                    }, "", now_ms()) }).await?;
            }
            self.finish_manager_attempt(&attempt).await;
            self.publish_changed(&network, &scope, Changed::Job(&trigger));
        }
        // After recovery, so a reply left running by an earlier process is never a target.
        self.steer_messages(&network).await?;
        if self.manager().is_some() {
            let sessions = self.store.call("pending_scheduler_sessions", |store| store.pending_scheduler_sessions()).await?;
            for key in sessions {
                self.start_schedule_consumer(network.clone(), key).await;
            }
        }
        Ok(())
    }

    async fn start_schedule_consumer(self: &Arc<Self>, network: String, session: SessionKey) {
        let key = (network.clone(), session.clone());
        if !self.schedule_workers.lock().await.insert(key.clone()) {
            return;
        }
        let node = self.clone();
        let failed_key = key.clone();
        if let Err(error) = self.background_tasks.spawn(async move {
            let result = std::panic::AssertUnwindSafe(node.drain_schedule(&network, &session))
                .catch_unwind()
                .await;
            match result {
                Ok(Err(error)) => {
                    tracing::warn!(event = "schedule.drain_pending", %error, "conversation scheduling is pending")
                }
                Err(_) => {
                    tracing::error!(
                        event = "schedule.panicked",
                        "conversation scheduler panicked; durable state retained"
                    )
                }
                Ok(Ok(true)) => node.schedule_wakeup.notify_one(),
                _ => {}
            }
            node.schedule_workers.lock().await.remove(&key);
            // The periodic durable scan covers enqueue racing this final empty check.
        }) {
            self.schedule_workers.lock().await.remove(&failed_key);
            tracing::debug!(%error, "schedule consumer skipped while the Node is shutting down");
        }
    }

    async fn drain_schedule(self: &Arc<Self>, network: &str, key: &SessionKey) -> Result<bool> {
        let scope = key.conversation_id.as_str();
        let mut ran = false;
        loop {
            if !self.writable() {
                return Ok(ran);
            }
            if let crate::scheduling::SessionScope::WorkspaceTask { task_id } = &key.scope {
                let roots = self.workspace_execution_roots().await?;
                if self
                    .store.call("ensure_workspace_assignment", { let identity = self.identity.to_owned();
                let network = network.to_owned();
                let task_id = task_id.to_owned();
                let roots = roots.to_owned();
                move |store| store.ensure_workspace_assignment(&identity, &network, &task_id, &roots, now_ms()) }).await?
                    .is_none()
                {
                    return Ok(ran);
                }
            }
            if !self.prepare_scope_input(network, key).await? {
                return Ok(ran);
            }
            let access = Access {
                network_id: network.into(),
                human: false,
                conversation_id: Some(scope.into()),
                manager: None,
            };
            let Some(result) =
                crate::scheduler::run(self.clone(), access, scope.into(), key.scope.clone())
                    .await?
            else {
                return Ok(ran);
            };
            ran = true;
            if let Some(trigger) = result["trigger"].as_str() {
                self.publish_changed(network, scope, Changed::Job(trigger));
            }
        }
    }

    async fn steer_messages(self: &Arc<Self>, network: &str) -> Result<()> {
        for job in self.store.call("steering_jobs", |store| store.steering_jobs()).await? {
            let key = (
                network.to_owned(),
                job.conversation_id.clone(),
                job.trigger.clone(),
            );
            if !self.steer_read_workers.lock().await.insert(key.clone()) {
                continue;
            }
            let node = self.clone();
            let network = network.to_owned();
            tokio::spawn(async move {
                let _slot = node.steer_read_slots.clone().acquire_owned().await;
                if let Err(error) = node.settle_steer(&network, &job).await {
                    tracing::warn!(event="agent.turn.steer_read_pending", %error, "input receipt query remains pending");
                }
                node.steer_read_workers.lock().await.remove(&key);
            });
        }
        if !self.writable() {
            return Ok(());
        }
        let candidates = self.store.call("steer_candidates", { let network = network.to_owned();
                let local_id = self.identity.device_id()?.to_owned();
                move |store| store.steer_candidates(&network, &local_id, crate::scheduler::boot_id()) }).await?;
        for candidate in candidates {
            let session = SessionKey {
                conversation_id: candidate.conversation_id,
                scope: candidate.scope,
            };
            let key = (network.to_owned(), session.clone());
            if !self.steer_workers.lock().await.insert(key.clone()) {
                continue;
            }
            let node = self.clone();
            let network = network.to_owned();
            tokio::spawn(async move {
                let result = std::panic::AssertUnwindSafe(node.drain_steer(&network, &session))
                    .catch_unwind()
                    .await;
                let progressed = matches!(&result, Ok(Ok(true)));
                match result {
                    Ok(Err(error)) => {
                        tracing::warn!(event="agent.turn.steer_pending", %error, "input delivery remains pending")
                    }
                    Err(_) => tracing::error!(
                        event = "agent.turn.steer_panicked",
                        "input delivery panicked; durable state retained"
                    ),
                    _ => {}
                }
                node.steer_workers.lock().await.remove(&key);
                if progressed {
                    node.schedule_wakeup.notify_one();
                }
            });
        }
        Ok(())
    }

    async fn drain_steer(&self, network: &str, key: &SessionKey) -> Result<bool> {
        let local = self.identity.device_id()?;
        let boot = crate::scheduler::boot_id();
        let mut submitted = false;
        loop {
            if !self.writable() {
                return Ok(submitted);
            }
            let candidate = {
                let network = network.to_owned(); let key = key.clone(); let local = local.clone();
                self.store.call("steer_candidate", move |store| {
                if store.steering_jobs()?.iter().any(|job| {
                    job.session_key() == key
                        && matches!(job.state, JobState::Steering { receipt: None, .. })
                }) {
                    return Ok(None);
                }
                Ok(store
                    .steer_candidates(&network, &local, boot)?
                    .into_iter()
                    .find(|candidate| {
                        candidate.conversation_id == key.conversation_id
                            && candidate.scope == key.scope
                    }))
                }).await?
            };
            let Some(candidate) = candidate else {
                return Ok(submitted);
            };
            let mut attachment_events = self
                .store.call("input_attachment_events", { let network = network.to_owned();
                let message_id = candidate.message_id.to_owned();
                move |store| store.input_attachment_events(&network, &message_id) }).await?;
            if attachment_events.is_empty() {
                attachment_events.push(candidate.source_event.clone());
            }
            let missing = self
                .fetch_turn_attachments(network, attachment_events.iter())
                .await;
            if !missing.is_empty() {
                return Ok(submitted);
            }
            let files = {
                let network = network.to_owned();
                let conversation = candidate.conversation_id.clone();
                let message = candidate.message_id.clone();
                let attachments = candidate.attachments.clone();
                self.store.call("steer_input_files", move |store| {
                attachments
                    .iter()
                    .map(|attachment| {
                        let (local_path, bytes) = store.verified_input_attachment(
                            &network,
                            &conversation,
                            &message,
                            attachment,
                        )?;
                        Ok(executor::PreparedAttachment {
                            attachment: attachment.clone(),
                            local_path,
                            mime_type: attachment_mime(&attachment.name, &bytes).into(),
                            bytes,
                        })
                    })
                    .collect::<Result<Vec<_>>>()
                }).await?
            };
            let input = executor::SteerInput {
                text: candidate.text,
                files,
            };
            let _lease = if candidate.into.attempt_id.starts_with("work:") {
                self.work_input_lease(network, &candidate.conversation_id, &candidate.into)
                    .await?
            } else {
                self.manager_input_lease(&candidate.into.attempt_id, candidate.into.provider)
                    .await?
            };
            if !self.store.call("begin_steer", { let network = network.to_owned();
                let local = local.to_owned();
                let boot = boot.to_owned();
                let conversation_id = candidate.conversation_id.to_owned();
                let trigger = candidate.trigger.to_owned();
                let target = candidate.into.to_owned();
                let event_hash = candidate.source_event.hash.to_owned();
                move |store| store.begin_steer(&network, &local, &boot, (&conversation_id, &trigger), &target, Some(&event_hash)) }).await? {
                continue;
            }
            self.publish_changed(
                network,
                &candidate.conversation_id,
                Changed::Job(&candidate.trigger),
            );
            submitted = true;
            let executor = self.executor(candidate.into.provider)?;
            let ack = tokio::time::timeout(
                STEER_WAIT,
                executor.send_turn(&candidate.into, &candidate.message_id, &input),
            )
            .await
            .unwrap_or(SteerAck::Unknown);
            match ack {
                SteerAck::Accepted { turn_id } => self.store.call("record_steer_submission", { let conversation_id = candidate.conversation_id.to_owned();
                let trigger = candidate.trigger.to_owned();
                let target = candidate.into.to_owned();
                let turn_id = turn_id.to_owned();
                move |store| store.record_steer_submission(&conversation_id, &trigger, &target, Some(turn_id)) }).await?,
                SteerAck::Refused(error) => {
                    tracing::info!(
                        event = "agent.turn.steer_refused",
                        conversation_id = candidate.conversation_id,
                        trigger = candidate.trigger,
                        error,
                        "native input was refused before delivery"
                    );
                    self.store.call("settle_steer", { let identity = self.identity.to_owned();
                let network = network.to_owned();
                let conversation_id = candidate.conversation_id.to_owned();
                let trigger = candidate.trigger.to_owned();
                move |store| store.settle_steer(&identity, &network, &conversation_id, &trigger, JobState::Queued) }).await?;
                    self.schedule_wakeup.notify_one();
                    return Ok(submitted);
                }
                SteerAck::Unknown => {
                    self.store.call("record_steer_submission", { let conversation_id = candidate.conversation_id.to_owned();
                let trigger = candidate.trigger.to_owned();
                let target = candidate.into.to_owned();
                move |store| store.record_steer_submission(&conversation_id, &trigger, &target, None) }).await?;
                    self.publish_changed(
                        network,
                        &candidate.conversation_id,
                        Changed::Job(&candidate.trigger),
                    );
                    return Ok(submitted);
                }
            }
            self.publish_changed(
                network,
                &candidate.conversation_id,
                Changed::Job(&candidate.trigger),
            );
        }
    }

    async fn settle_steer(&self, network: &str, job: &SchedulerJob) -> Result<()> {
        let JobState::Steering {
            into,
            receipt,
            uncertain,
            unread_since_ms,
            running_since_ms,
        } = &job.state
        else {
            return Ok(());
        };
        let active = self
            .store.call("steer_host_active", { let conversation_id = job.conversation_id.to_owned();
                let attempt_id = into.attempt_id.to_owned();
                move |store| store.steer_host_active(&conversation_id, &attempt_id) }).await?;
        if active && receipt.is_some() {
            return Ok(());
        }
        let read = async {
            self.executor(into.provider)
                .ok()?
                .read_back(into, &job.reply_to)
                .await
        };
        let trace = tokio::time::timeout(STEER_WAIT, read).await.ok().flatten();
        if !self.writable() {
            return Ok(());
        }
        let Some(next) = next_steer_state(
            trace,
            into,
            receipt.as_ref(),
            *uncertain,
            *unread_since_ms,
            *running_since_ms,
            now_ms(),
        ) else {
            return Ok(());
        };
        self.store.call("settle_steer", { let identity = self.identity.to_owned();
                let network = network.to_owned();
                let conversation_id = job.conversation_id.to_owned();
                let trigger = job.trigger.to_owned();
                let next = next.to_owned();
                move |store| store.settle_steer(&identity, &network, &conversation_id, &trigger, next) }).await?;
        self.schedule_wakeup.notify_one();
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
        ensure!(access.human, "agents cannot recursively invoke managers");
        let conversation = id(input, "conversation_id")?;
        self.require_scope(access, &conversation).await?;
        match self.accept_chat_message_inner(access, input).await {
            Ok(value) => Ok(value),
            Err(error) => {
                let message_id = id(input, "message_id")?;
                let stored = self
                    .store.call("workspace_input", { let network_id = access.network_id.to_owned();
                let message_id = message_id.to_owned();
                move |store| store.workspace_input(&network_id, &message_id) }).await?
                    .is_some();
                Ok(
                    json!({"accepted":false,"code":if stored { "input_conflict" } else { "input_not_saved" },
                    "stored":stored,"message_id":message_id,"reason":crate::logging::scrub(&error.to_string())}),
                )
            }
        }
    }

    async fn accept_chat_message_inner(
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
        let request: crate::workspace::InputRequest = serde_json::from_value(input.clone())
            .context("message requires a typed target, intent and sequence")?;
        ensure!(
            !matches!(
                request.route.target,
                crate::workspace::InputTarget::ChannelMember { .. }
            ),
            "channel member input must bind a stable channel task before submission"
        );
        ensure!(
            request.message_id == message_id && request.route.sequence > 0,
            "invalid input identity or sequence"
        );
        let local = self.identity.device_id()?;
        let roots = self.workspace_execution_roots().await?;
        let (message, receipt) = {
            let identity = self.identity.clone(); let network = network.clone(); let scope = scope.clone();
            let message_id = message_id.clone(); let attestation = attestation(input)?;
            self.store.call("accept_chat_message", move |store| {
            let resolved = store.resolve_input_target(&network, &request.route.target)?;
            ensure!(
                resolved.conversation_id == scope,
                "input target belongs to another conversation"
            );
            ensure!(
                request.route.intent != crate::workspace::InputIntent::NewTask
                    || matches!(
                        resolved.scope,
                        crate::scheduling::SessionScope::WorkspaceTask { .. }
                    ),
                "new_task requires a stable task target"
            );
            let message = store.post_workspace_input(
                &identity,
                &network,
                request,
                attestation,
                now_ms(),
            )?;
            let admission = (|| -> Result<_> {
                if let Some(task_id) = &resolved.task_id
                    && resolved.owner_node == local
                {
                    store.ensure_workspace_assignment(
                        &identity,
                        &network,
                        task_id,
                        &roots,
                        now_ms(),
                    )?;
                }
                let receipt = store.admit_workspace_input(&network, &local, &message_id)?;
                if receipt.state == "queued"
                    && let Some(task_id) = &resolved.task_id
                    && resolved.owner_node == local
                    && let Err(error) = store.ensure_workspace_assignment(
                        &identity,
                        &network,
                        task_id,
                        &roots,
                        now_ms(),
                    )
                {
                    tracing::debug!(%error, "accepted task input waits for assignment recovery");
                }
                Ok(receipt)
            })();
            let receipt = match admission {
                Ok(receipt) => receipt,
                Err(error) => store
                    .refuse_workspace_input(
                        &network,
                        &message_id,
                        &crate::logging::scrub(&error.to_string()),
                    )
                    .unwrap_or(crate::scheduling::AdmissionReceipt {
                        network_id: network.clone(),
                        conversation_id: scope.clone(),
                        trigger: crate::scheduling::message_trigger(&message_id),
                        state: "unknown".into(),
                    }),
            };
            Ok((message, receipt))
            }).await?
        };
        self.publish_changed(network, &scope, Changed::Event(&message.source_event));
        if let Err(error) = self.wake_scheduler() {
            tracing::debug!(%error,"saved input waits for scheduler recovery");
        }
        Ok(
            json!({"accepted":true,"state":receipt.state,"trigger":receipt.trigger,
            "message_id":message_id,"receipt":receipt}),
        )
    }

    pub(super) async fn read_input_receipt(&self, network: &str, input: &Value) -> Result<Value> {
        let message_id = id(input, "message_id")?;
        let conversation = id(input, "conversation_id")?;
        let target: crate::workspace::InputTarget =
            serde_json::from_value(input["target"].clone())?;
        let network = network.to_owned(); let local = self.identity.device_id()?;
        self.store.call("read_input_receipt", move |store| {
        let original = store
            .workspace_input(&network, &message_id)?
            .context("input not found")?;
        ensure!(
            original.conversation_id == conversation && original.input.route.target == target,
            "input receipt identity mismatch"
        );
        let resolved = store.resolve_input_target(&network, &target)?;
        ensure!(
            resolved.owner_node == local,
            "this Node is not the input owner"
        );
        store
            .input_receipt(&network, &local, &message_id)?
            .context("input not found")
        }).await
    }

    pub(super) async fn read_scope_context(&self, network: &str, input: &Value) -> Result<Value> {
        let conversation = id(input, "conversation_id")?;
        let target: crate::workspace::InputTarget =
            serde_json::from_value(input["target"].clone())?;
        let key = {
            let network = network.to_owned(); let conversation = conversation.clone(); let target = target.clone();
            let local = self.identity.device_id()?;
            self.store.call("scope_context_target", move |store| {
            let resolved = store.resolve_input_target(&network, &target)?;
            ensure!(
                resolved.conversation_id == conversation
                    && resolved.owner_node == local,
                "context target is not owned by this Node"
            );
            Ok(SessionKey {
                conversation_id: conversation.clone(),
                scope: resolved.scope,
            })
            }).await?
        };
        let context = self.scheduler_context_observation(network, &key).await?;
        Ok(
            json!({"conversation_id":conversation,"target":target,"available":context.is_some(),"reason":if context.is_some() { Value::Null } else { json!("scope_has_not_run") },"context":context}),
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
        let query_network = network.clone(); let query_scope = scope.clone();
        let receipts = self.store.call("settle_unanswered", move |store| {
            if dismiss {
                store.dismiss_unanswered(&query_network, &local, &query_scope, &message_ids)
            } else {
                store.reply_unanswered(&query_network, &local, &query_scope, &message_ids)
            }
        }).await?;
        for receipt in &receipts {
            self.publish_changed(network, &scope, Changed::Job(&receipt.trigger));
        }
        self.schedule_wakeup.notify_one();
        Ok(json!({"receipts":receipts}))
    }

    /// This Node's latest jobs of a conversation, read by a device that shows it; `replied`
    /// says whether the job's reply is already written here.
    pub(super) async fn scheduler_scope_jobs(
        &self,
        network: &str,
        conversation: &str,
    ) -> Result<Vec<Value>> {
        let projection = self.scheduler_projection()?;
        let network = network.to_owned(); let conversation = conversation.to_owned();
        self.store.read("scheduler_scope_jobs", move |store| {
            let jobs = store.scheduler_jobs(&conversation)?;
            jobs[jobs.len().saturating_sub(MAX_OWNER_JOBS)..].iter()
                .map(|job| projection.scope_job(store, &network, job)).collect()
        }).await
    }

    pub(super) async fn owner_jobs(&self, network: &str, scope: &str) -> Result<Value> {
        let access = Access {
            network_id: network.into(),
            human: false,
            conversation_id: Some(scope.into()),
            manager: None,
        };
        self.require_scope(&access, scope).await?;
        let scope_jobs = self.scheduler_scope_jobs(network, scope).await?;
        let context = self
            .scheduler_context_observation(
                network,
                &SessionKey {
                    conversation_id: scope.into(),
                    scope: crate::scheduling::SessionScope::Main,
                },
            )
            .await?;
        let projection = self.scheduler_projection()?;
        let query_network = network.to_owned(); let query_scope = scope.to_owned();
        let (jobs, staged, conflicts) = self.store.read("owner_jobs", move |store| {
            let checks = store.pending_checks()?;
            let all = chat_jobs(store.scheduler_jobs(&query_scope)?);
            let mut jobs = Vec::new();
            for job in &all[all.len().saturating_sub(MAX_OWNER_JOBS)..] {
                let mut value = projection.job(store, &query_network, job, &checks)?;
                value["replied"] = json!(store.message(&query_network, &query_scope, &job.message_id)?.is_some());
                jobs.push(value);
            }
            Ok((jobs, store.scheduler_memory_staged_count(&query_scope)?, store.scheduler_memory_conflicts(&query_scope)?))
        }).await?;
        Ok(json!({"jobs":jobs,"scope_jobs":scope_jobs,"manager":self.scheduler_manager_metadata(),
            "context":context,"memory_staged_count":staged,"memory_conflicts":conflicts}))
    }

    /// When another device schedules the conversation, its jobs join `jobs` for display only,
    /// marked with `source_node`; nothing is stored. Returns that owner, whether it answered
    /// in time, and its bot's provider.
    pub(super) async fn merge_owner_jobs(
        &self,
        network: &str,
        scope: &str,
        jobs: &mut Vec<Value>,
    ) -> Result<Option<(Value, Value)>> {
        let local = self.identity.device_id()?;
        let Some(owner) = self
            .store.call("conversation", { let network = network.to_owned();
                let scope = scope.to_owned();
                move |store| store.conversation(&network, &scope) }).await?
            .and_then(|conversation| conversation.owner)
            .filter(|owner| owner != &local)
        else {
            return Ok(None);
        };
        let request = json!({"type":"scheduler_jobs","conversation_id":scope});
        let reply = match tokio::time::timeout(
            OWNER_READ_WAIT,
            self.peer_request(network, &owner, request),
        )
        .await
        {
            Ok(Ok(reply)) => reply,
            outcome => {
                if let Ok(Err(error)) = outcome {
                    tracing::debug!(%error, "the conversation's owner did not report its jobs");
                }
                return Ok(Some((
                    json!({"owner":owner,"reachable":false}),
                    Value::Null,
                )));
            }
        };
        let remote = reply["jobs"].as_array().cloned().unwrap_or_default();
        let query_network = network.to_owned(); let query_scope = scope.to_owned(); let query_owner = owner.clone();
        let remote = self.store.read("merge_owner_jobs", move |store| {
        let mut result = Vec::new();
        for mut job in remote.into_iter().take(MAX_OWNER_JOBS) {
            let Some(trigger) = job["trigger"].as_str().map(str::to_owned) else {
                continue;
            };
            // Written there, not synced here yet: the reply is on its way, not lost.
            if job["replied"] == true
                && let Some(reply) = job["reply_message_id"].as_str()
                && store.message(&query_network, &query_scope, reply)?.is_none()
            {
                job["state"] = json!("publishing");
            }
            job["source_node"] = json!(query_owner);
            result.push((trigger, job));
        }
        Ok(result)
        }).await?;
        for (trigger, job) in remote {
            jobs.retain(|known| known["trigger"] != trigger.as_str() || known["state"] != "pending_delivery");
            jobs.push(job);
        }
        Ok(Some((
            json!({"owner":owner,"reachable":true,"context":reply["context"],
                "scope_jobs":reply["scope_jobs"],
                "memory_staged_count":reply["memory_staged_count"],"memory_conflicts":reply["memory_conflicts"]}),
            reply["manager"].clone(),
        )))
    }

    /// The conversation's local jobs, waiting outbox entries and their pending checks.
    pub(super) async fn scheduler_snapshot_jobs(
        &self,
        network: &str,
        scope: &str,
    ) -> Result<(Vec<Value>, Vec<PendingCheck>)> {
        let projection = self.scheduler_projection()?;
        let manager = self.scheduler_manager_metadata();
        let network = network.to_owned(); let scope = scope.to_owned();
        self.store.read("scheduler_snapshot_jobs", move |store| {
        let checks = store
            .pending_checks()?
            .into_iter()
            .filter(|check| check.conversation_id == scope)
            .collect::<Vec<_>>();
        let owner = store
            .conversation(&network, &scope)?
            .and_then(|conversation| conversation.owner);
        let mut jobs = Vec::new();
        for job in chat_jobs(store.scheduler_jobs(&scope)?) {
            let mut value = projection.job(store, &network, &job, &checks)?;
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
        }).await
    }
}

/// The most recent jobs a conversation's owner reports to another device.
const MAX_OWNER_JOBS: usize = 64;
/// How long showing a conversation waits for its owner's jobs.
const OWNER_READ_WAIT: Duration = Duration::from_secs(3);
/// How long one steer or read waits for the App Server inside the scheduling tick; a steer that
/// times out is unknown and read back later.
const STEER_WAIT: Duration = Duration::from_secs(5);
/// How long a steered message waits after its reply ended for its thread to become readable, or
/// for the turn it shows running to end.
const STEER_SETTLE_LIMIT_MS: i64 = 10 * 60 * 1000;

/// The state a steering message moves to after a read of its thread (`None` when it failed), or
/// `None` to leave it. A successful read restarts the unreadable clock; a failed read says nothing
/// about the turn, so the running clock keeps going.
fn next_steer_state(
    trace: Option<SteerTrace>,
    into: &SteerTarget,
    receipt: Option<&crate::scheduling::SteerReceipt>,
    uncertain: bool,
    unread_since_ms: Option<i64>,
    running_since_ms: Option<i64>,
    now: i64,
) -> Option<JobState> {
    let original_receipt = receipt.cloned();
    let original_uncertain = uncertain;
    let mut receipt = original_receipt.clone();
    let mut uncertain = uncertain;
    let steering = |receipt, uncertain, unread_since_ms, running_since_ms| JobState::Steering {
        into: into.clone(),
        receipt,
        uncertain,
        unread_since_ms,
        running_since_ms,
    };
    let (since, next) = match trace {
        Some(SteerTrace::NotArrived) if receipt.is_none() => return Some(JobState::Queued),
        Some(SteerTrace::NotArrived) => {
            return Some(JobState::Steered {
                into: into.clone(),
                receipt,
                outcome: SteerOutcome::Unknown,
            });
        }
        Some(SteerTrace::Ended { turn_id, outcome }) => {
            let receipt = receipt.get_or_insert(crate::scheduling::SteerReceipt {
                accepted_at_ms: now,
                actual_turn_id: None,
            });
            receipt.actual_turn_id = Some(turn_id);
            return Some(JobState::Steered {
                into: into.clone(),
                receipt: Some(receipt.clone()),
                outcome,
            });
        }
        Some(SteerTrace::Received { turn_id }) => {
            let receipt = receipt.get_or_insert(crate::scheduling::SteerReceipt {
                accepted_at_ms: now,
                actual_turn_id: None,
            });
            receipt.actual_turn_id = Some(turn_id);
            let since = running_since_ms.unwrap_or(now);
            (
                since,
                steering(Some(receipt.clone()), false, None, Some(since)),
            )
        }
        Some(SteerTrace::Pending) => {
            let since = running_since_ms.unwrap_or(now);
            uncertain |= receipt.is_none();
            (
                since,
                steering(receipt.clone(), uncertain, None, Some(since)),
            )
        }
        None | Some(SteerTrace::TargetUnseen) => {
            let since = unread_since_ms.unwrap_or(now);
            uncertain |= receipt.is_none();
            (
                since,
                steering(receipt.clone(), uncertain, Some(since), running_since_ms),
            )
        }
    };
    if now.saturating_sub(since) >= STEER_SETTLE_LIMIT_MS {
        return Some(JobState::Steered {
            into: into.clone(),
            receipt,
            outcome: SteerOutcome::Unknown,
        });
    }
    (next
        != steering(
            original_receipt,
            original_uncertain,
            unread_since_ms,
            running_since_ms,
        ))
    .then_some(next)
}

fn attachment_mime(name: &str, bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "image/png";
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return "image/jpeg";
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return "image/gif";
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return "image/webp";
    }
    if bytes.starts_with(b"%PDF-") {
        return "application/pdf";
    }
    match Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        Some("txt" | "md" | "rs" | "ts" | "tsx" | "js" | "py" | "csv" | "log") => "text/plain",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::test_support::{
        access as test_access, manager_settings, signed_main_input,
    };

    #[tokio::test]
    async fn readiness_and_registration_failures_do_not_reserve_scheduler_budget() -> Result<()> {
        use crate::{
            model::ResolvedModel,
            runtime::triage::tests::{bot, node},
            work::WorkSpec,
        };
        for case in ["needs_setup", "registration"] {
            let (node, admin, directory) = node().await?;
            let result = async {
                {
                    let mut settings = node.settings.lock().unwrap();
                    settings.roots = vec![directory.clone()];
                    settings.manager = Some(manager_settings(Provider::Pi, directory.clone(), "fixture".into()));
                    let mut providers = node.providers.lock().unwrap();
                    providers.switches.execution = [Provider::Pi].into();
                    let binary = if case == "needs_setup" { directory.join("missing-pi") } else {
                        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
                            .join(if cfg!(windows) { "fake-pi.cmd" } else { "fake-pi" })
                    };
                    providers.bins.insert(Provider::Pi, binary);
                }
                node.tool_url.set("http://127.0.0.1:1".into()).unwrap();
                bot(&node, &admin, "c", "Budget", &[]).await?;
                let network = node.network_id().await?;
                let local = node.identity.device_id()?;
                let work_id = WorkSpec::id("c", "message:origin", case);
                {
                    let identity = node.identity.clone(); let admin = admin.clone(); let network = network.clone();
                    let work_id = work_id.clone(); let directory = directory.clone();
                    node.store.call("fixture_budget_work", move |store| {
                    let posted = signed_main_input(&admin, &network, "c", "origin", "fixture", vec![], 1)?;
                    store.append_event(&identity, &network, "c", posted, vec![], now_ms())?;
                    store.accept_work_fixture(&identity, &network, WorkSpec {
                        work_id: work_id.clone(), conversation: "c".into(), trigger: "message:origin".into(),
                        target: local.clone(), dispatcher: local, call_ref: case.into(), kind: WorkKind::Task,
                        model: None, resolved: ResolvedModel { provider: Provider::Pi, model: Some("fixture".into()) },
                        cwd: directory.to_string_lossy().into_owned(), title: "Budget".into(), prompt: "fixture".into(),
                        continues: None, parent: None, thread: None,
                    }, now_ms())?;
                    Ok(())
                    }).await?;
                }
                let claimed = Arc::new(tokio::sync::Notify::new());
                let resume = Arc::new(tokio::sync::Notify::new());
                if case == "registration" {
                    *node.claim_to_start_barrier.lock().unwrap() = Some((claimed.clone(), resume.clone()));
                }
                let (running_node, running_network, running_id) = (node.clone(), network.clone(), work_id.clone());
                let running = tokio::spawn(async move {
                    crate::scheduler::run(running_node, test_access(running_network, false, Some("c".into())), "c".into(),
                        crate::scheduling::SessionScope::Task { work_id: running_id }).await
                });
                if case == "registration" {
                    tokio::time::timeout(Duration::from_secs(5), claimed.notified()).await?;
                    node.local_permission_epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    resume.notify_one();
                }
                assert_eq!(tokio::time::timeout(Duration::from_secs(5), running).await???
                    .context("refused invocation")?["state"], "failed");
                let identity = node.identity.clone(); let network = network.clone();
                node.store.call("fixture_budget_assertions", move |store| {
                let used: u64 = store.connection.query_row(
                    "SELECT COALESCE(sum(used),0) FROM budget_days WHERE network_id=?", [&network], |row| row.get(0))?;
                assert_eq!(used, 0, "{case}");
                assert!(store.pending_checks()?.is_empty());
                let native: u64 = store.connection.query_row(
                    "SELECT (SELECT count(*) FROM manager_turns)+(SELECT count(*) FROM manager_processes)", [], |row| row.get(0))?;
                assert_eq!(native, 0);
                let task = store.scheduler_job("c", &crate::scheduling::task_trigger(&work_id))?.context("task job")?;
                assert!(matches!(task.state, JobState::Failed { .. }), "{case}: {:?}", task.state);
                let report = crate::scheduling::work_trigger(&work_id);
                let queued: Vec<(String, crate::scheduling::TriggerKind)> = store.scheduler_jobs("c")?.into_iter()
                    .filter(|job| job.state == JobState::Queued).map(|job| (job.trigger, job.kind)).collect();
                assert_eq!(queued, vec![(report.clone(), crate::scheduling::TriggerKind::WorkResult)], "{case}");
                store.request_manager_cancel(&identity, &network, "c", &report)?;
                store.scheduler_maintenance_idle()?;
                Ok(())
                }).await?;
                assert!(node.agent_tokens.lock().await.is_empty());
                assert!(node.manager_controls.lock().await.is_empty());
                Ok::<_, anyhow::Error>(())
            }.await;
            node.shutdown().await;
            drop(node);
            std::fs::remove_dir_all(directory)?;
            result?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn preparation_write_failures_leave_no_native_call_or_fabricated_pending_check()
    -> Result<()> {
        use crate::runtime::triage::tests::{bot, claim_reply, node};

        for case in ["runtime", "runtime_and_finish", "failure_record"] {
            let (node, admin, directory) = node().await?;
            let result = async {
                node.settings.lock().unwrap().manager = Some(manager_settings(Provider::Codex, directory.clone(), "fixture".into()));
                node.tool_url.set("http://127.0.0.1:1".into()).unwrap();
                bot(&node, &admin, "c", "Preparation", &[]).await?;
                let network = node.network_id().await?;
                let (trigger, old_attempt) = claim_reply(&node, &admin, "c", "prepare", crate::scheduler::boot_id()).await?;
                {
                    let trigger = trigger.clone();
                    node.store.call("fixture_preparation_failures", move |store| {
                    assert!(store.requeue_abandoned_scheduler_claim("c", &trigger, &old_attempt, crate::scheduler::boot_id())?);
                    if case == "failure_record" {
                        store.connection.execute_batch(
                            "CREATE TRIGGER fail_context BEFORE UPDATE OF shown ON scheduler_sessions
                             WHEN json_extract(NEW.shown,'$.context.stage')='compiled'
                             BEGIN SELECT RAISE(FAIL,'controlled compiled-context failure'); END;
                             CREATE TRIGGER fail_runtime BEFORE UPDATE OF runtime ON scheduler_jobs
                             WHEN OLD.runtime IS NOT NULL AND NEW.runtime IS NOT NULL
                             BEGIN SELECT RAISE(FAIL,'controlled failure-record write failure'); END;"
                        )?;
                    } else {
                        store.connection.execute_batch(
                            "CREATE TRIGGER fail_runtime BEFORE UPDATE OF runtime ON scheduler_jobs
                             WHEN NEW.runtime IS NOT NULL
                             BEGIN SELECT RAISE(FAIL,'controlled initial runtime failure'); END;"
                        )?;
                    }
                    if case == "runtime_and_finish" {
                        store.connection.execute_batch(
                            "CREATE TRIGGER fail_finish BEFORE UPDATE OF state ON scheduler_jobs
                             WHEN NEW.state='failed'
                             BEGIN SELECT RAISE(FAIL,'controlled terminal write failure'); END;"
                        )?;
                    }
                    Ok(())
                    }).await?;
                }
                let invocation = tokio::time::timeout(Duration::from_secs(5), crate::scheduler::run(
                    node.clone(), test_access(network.clone(), false, Some("c".into())),
                    "c".into(), crate::scheduling::SessionScope::Main,
                )).await?;
                if case == "runtime" {
                    assert_eq!(invocation?.context("failed preparation")?["state"], "failed");
                } else {
                    assert!(invocation.is_err(), "{case}");
                }
                let abandoned = node.store.call("scheduler_job", { let trigger = trigger.to_owned();
                move |store| store.scheduler_job("c", &trigger) }).await?.context("job")?;
                assert!(node.agent_tokens.lock().await.is_empty());
                assert!(node.manager_controls.lock().await.is_empty());
                {
                    node.store.call("fixture_no_native_call", move |store| {
                    assert!(store.pending_checks()?.is_empty());
                    let native: u64 = store.connection.query_row(
                        "SELECT (SELECT count(*) FROM manager_turns)+(SELECT count(*) FROM manager_processes)",
                        [], |row| row.get(0))?;
                    assert_eq!(native, 0, "{case}");
                    store.connection.execute_batch("DROP TRIGGER fail_runtime; DROP TRIGGER IF EXISTS fail_context; DROP TRIGGER IF EXISTS fail_finish;")?;
                    Ok(())
                    }).await?;
                }
                node.settings.lock().unwrap().manager = None;
                node.schedule_tick().await?;
                if case != "runtime" {
                    let identity = node.identity.clone();
                    node.store.call("fixture_abandoned_turn", move |store| {
                    assert_eq!(store.scheduler_job("c", &trigger)?.context("recovered job")?.state, JobState::Queued);
                    let previous = abandoned.state.attempt().context("old attempt")?;
                    assert!(!store.manager_attempt_allowed_with_version(&network, &identity.device_id()?, "c", &trigger,
                        &previous.id, previous.permission_version)?);
                    store.request_manager_cancel(&identity, &network, "c", &trigger)?;
                    Ok(())
                    }).await?;
                }
                node.store.call("scheduler_maintenance_idle", |store| store.scheduler_maintenance_idle()).await?;
                Ok::<_, anyhow::Error>(())
            }.await;
            node.shutdown().await;
            drop(node);
            std::fs::remove_dir_all(directory)?;
            result?;
        }
        Ok(())
    }

    fn into() -> SteerTarget {
        SteerTarget {
            provider: Provider::Codex,
            attempt_id: "a".into(),
            thread_id: "t".into(),
            turn_id: "r".into(),
        }
    }

    #[test]
    fn a_readable_thread_restarts_the_unreadable_clock() {
        let into = into();
        let steering = |unread, running| {
            Some(JobState::Steering {
                into: into.clone(),
                receipt: None,
                uncertain: true,
                unread_since_ms: unread,
                running_since_ms: running,
            })
        };
        let limit = STEER_SETTLE_LIMIT_MS;
        let next = |trace, unread, since_running, now| {
            next_steer_state(trace, &into, None, true, unread, since_running, now)
        };
        assert_eq!(next(None, None, None, 5), steering(Some(5), None));
        assert_eq!(next(None, Some(5), None, 6), None);
        assert_eq!(
            next(Some(SteerTrace::Pending), Some(5), None, 7),
            steering(None, Some(7))
        );
        assert_eq!(
            next(None, None, Some(7), limit + 6),
            steering(Some(limit + 6), Some(7))
        );
        assert_eq!(
            next(None, Some(5), None, limit + 5),
            Some(JobState::Steered {
                into: into.clone(),
                receipt: None,
                outcome: SteerOutcome::Unknown,
            })
        );
        assert_eq!(
            next(Some(SteerTrace::NotArrived), Some(5), None, 7),
            Some(JobState::Queued),
            "only a read that shows the target ended without it requeues the message"
        );
    }

    #[test]
    fn a_turn_still_running_after_its_reply_ended_settles_as_unknown() {
        let into = into();
        let unknown = Some(JobState::Steered {
            into: into.clone(),
            receipt: None,
            outcome: SteerOutcome::Unknown,
        });
        let limit = STEER_SETTLE_LIMIT_MS;
        let next = |unread, since_running, now| {
            next_steer_state(
                Some(SteerTrace::Pending),
                &into,
                None,
                true,
                unread,
                since_running,
                now,
            )
        };
        assert_eq!(next(None, Some(5), limit + 4), None);
        assert_eq!(next(None, Some(5), limit + 5), unknown);
        assert_eq!(next(Some(limit), Some(5), limit + 5), unknown);
    }

    #[test]
    fn accepted_inputs_keep_their_actual_turn_and_never_requeue_on_missing_history() {
        let target = into();
        let accepted = crate::scheduling::SteerReceipt {
            accepted_at_ms: 4,
            actual_turn_id: None,
        };
        assert_eq!(
            next_steer_state(
                Some(SteerTrace::NotArrived),
                &target,
                None,
                true,
                None,
                None,
                5
            ),
            Some(JobState::Queued)
        );
        assert_eq!(
            next_steer_state(
                Some(SteerTrace::NotArrived),
                &target,
                Some(&accepted),
                false,
                None,
                None,
                5
            ),
            Some(JobState::Steered {
                into: target.clone(),
                receipt: Some(accepted.clone()),
                outcome: SteerOutcome::Unknown,
            })
        );
        let observed = next_steer_state(
            Some(SteerTrace::Received {
                turn_id: "actual-turn".into(),
            }),
            &target,
            Some(&accepted),
            false,
            None,
            None,
            6,
        )
        .unwrap();
        let JobState::Steering {
            into,
            receipt: Some(receipt),
            uncertain,
            ..
        } = observed
        else {
            panic!("received input must retain its running receipt");
        };
        assert_eq!(
            (
                into.turn_id.as_str(),
                receipt.actual_turn_id.as_deref(),
                receipt.accepted_at_ms,
                uncertain
            ),
            ("r", Some("actual-turn"), 4, false)
        );
        assert_eq!(
            next_steer_state(
                Some(SteerTrace::Ended {
                    turn_id: "actual-turn".into(),
                    outcome: SteerOutcome::Unfinished
                }),
                &target,
                Some(&receipt),
                false,
                None,
                None,
                7
            ),
            Some(JobState::Steered {
                into: target,
                receipt: Some(receipt),
                outcome: SteerOutcome::Unfinished,
            })
        );
    }

    #[tokio::test]
    async fn stopped_workspace_inputs_read_back_their_original_turn_without_resending() -> Result<()>
    {
        use crate::{
            model::EventBody,
            runtime::triage::tests::{bot, node, scripted_app_server},
            scheduling::{SessionScope, message_trigger},
            workspace::{
                AssignmentState, InputIntent, InputRequest, InputRoute, InputTarget, TaskAction,
                WORKSPACE_SCOPE, WorkspaceCommand,
            },
        };
        for native_has_inputs in [true, false] {
            let (node, admin, directory) = node().await?;
            let socket = node
                .providers()
                .switches
                .codex_app_server
                .socket
                .context("fixture socket")?;
            let result = async {
                bot(&node, &admin, "c", "Workspace", &[]).await?;
                let network = node.store.call("network", |store| store.network()).await?.context("network")?;
                let local = node.identity.device_id()?;
                let roots = vec![directory.canonicalize()?.display().to_string()];
                let assignment = {
                    let identity = node.identity.clone(); let admin = admin.clone(); let network = network.clone();
                    let roots = roots.clone(); let local = local.clone();
                    node.store.call("fixture_stopped_workspace_inputs", move |store| {
                let command = |store: &mut Store, id: &str, command: WorkspaceCommand| -> Result<()> {
                    let signed = admin.attest(&network, WORKSPACE_SCOPE, EventBody::WorkspaceCommanded {
                        command_id:id.into(), command:command.clone(), attestation:None, acting_assignment:None, cause:None,
                    })?;
                    store.apply_workspace_command(&identity, &network, id, command,
                        signed.attestation().cloned(), None, None, now_ms())?;
                    Ok(())
                };
                let input = |store: &mut Store, id: &str, sequence: u64| -> Result<crate::workspace::WorkspaceInput> {
                    let request = InputRequest {
                        message_id:id.into(), route:InputRoute { target:InputTarget::Task { task_id:"task".into() },
                            intent:InputIntent::Continue, sequence, expected_revision:Some(1) },
                        text:id.into(), attachments:vec![], report_results:Some(true),
                    };
                    let signed = admin.attest(&network, "c", EventBody::InputPosted {
                        message_id:request.message_id.clone(), target:request.route.target.clone(), intent:request.route.intent,
                        sequence, expected_revision:Some(1), text:request.text.clone(), attachments:vec![], report_results:Some(true), attestation:None,
                    })?;
                    store.post_workspace_input(&identity, &network, request,
                        signed.attestation().cloned().context("input signature")?, now_ms())
                };

                    let bot_id = store.bot_for_conversation(&network, "c")?.context("bot")?.bot_id;
                    command(store, "create", WorkspaceCommand::CreateTask {
                        task_id:"task".into(), channel_id:None, parent_task_id:None, owner_bot:bot_id,
                        title:"task".into(), request:"perform task".into(), expected_result:"result".into(),
                        source_events:vec![], dependencies:vec![], reply_only:false,
                    })?;
                    let first = input(store, "original", 1)?;
                    let assignment = store.ensure_workspace_assignment(&identity, &network, "task", &roots, now_ms())?.context("assignment")?;
                    let host = store.claim_next_fixture(&identity, &network, "c", "boot", "fixture")?.context("host")?.job;
                    assert_eq!(host.scope, SessionScope::WorkspaceTask { task_id:"task".into() });
                    let attempt = host.state.attempt().context("attempt")?.id.clone();
                    store.mark_started("c", &host.trigger, &attempt)?;
                    store.record_manager_turn(&attempt, "system", "thread", Some("turn"), &Default::default())?;
                    for (id, state, input_event) in [
                        ("accept", AssignmentState::Accepted, None),
                        ("adopt", AssignmentState::Adopted, Some(first.source_event)),
                    ] {
                        store.apply_workspace_command(&identity, &network, id, WorkspaceCommand::AssignmentProgress {
                            assignment_id:assignment.assignment_id.clone(), revision:1, state, input_event,
                            execution:None, evidence_event:None, result:None,
                        }, None, None, None, now_ms())?;
                    }
                    for (id, sequence) in [("second", 2), ("third", 3)] {
                        input(store, id, sequence)?;
                        let admitted = store.admit_workspace_input(&network, &local, id)?;
                        assert_eq!(admitted.state, "queued");
                        assert_eq!(store.scheduler_job("c", &message_trigger(id))?.context("owner-admitted input")?.scope,
                            SessionScope::WorkspaceTask { task_id:"task".into() });
                        let candidate = store.steer_candidates(&network, &local, "boot")?.into_iter()
                            .find(|candidate| candidate.message_id == id).context("steer candidate")?;
                        assert!(store.begin_steer(&network, &local, "boot", ("c", &candidate.trigger), &candidate.into, Some(&candidate.source_event.hash))?);
                        store.record_steer_submission("c", &candidate.trigger, &candidate.into, Some(Some("turn".into())))?;
                    }
                    command(store, "stop", WorkspaceCommand::ControlTask { task_id:"task".into(), expected_revision:1, action:TaskAction::Stop })?;
                    store.request_manager_cancel(&identity, &network, "c", &host.trigger)?;
                    store.finish_job(&identity, &network, "c", &host.trigger, &attempt,
                        JobOutcome::Cancelled("native turn interrupted".into()), "", now_ms())?;
                    assert!(!store.steer_host_active("c", &attempt)?);
                    Ok(assignment.assignment_id)
                }).await?
                };
                for _ in 0..2 { node.reconcile_workspace_jobs(&network).await?; }
                let pending = node.store.call("steering_jobs", |store| store.steering_jobs()).await?;
                assert_eq!(pending.len(), 2, "Stop keeps both receipts eligible for native readback");
                for job in &pending {
                    assert!(matches!(&job.state, JobState::Steering { into, receipt:Some(receipt), .. }
                        if into.thread_id=="thread" && into.turn_id=="turn" && receipt.actual_turn_id.as_deref()==Some("turn")));
                }
                let mut requests = scripted_app_server(&node, move |request| {
                    if request["method"] == "thread/read" {
                        assert_eq!(request["params"]["threadId"], "thread");
                        assert_eq!(request["params"]["includeTurns"], true);
                        let items = if native_has_inputs { vec![json!({"type":"userMessage","clientId":"second"}), json!({"type":"userMessage","clientId":"third"})] } else { vec![] };
                        json!({"result":{"thread":{"id":"thread","turns":[{"id":"turn","status":"interrupted","items":items}]}}})
                    } else {
                        assert!(!request["method"].as_str().is_some_and(|method| method.starts_with("turn/")), "readback must not send or interrupt again");
                        json!({"result":{}})
                    }
                }).await?;
                tokio::time::timeout(Duration::from_secs(5), async {
                    while node.codex.status().await["connection"]["state"] != "connected" {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }).await?;
                node.steer_messages(&network).await?;
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if node.store.call("steering_jobs", |store| store.steering_jobs()).await?.is_empty() { break Ok::<_, anyhow::Error>(()); }
                        tokio::task::yield_now().await;
                    }
                }).await??;
                let expected = if native_has_inputs { SteerOutcome::Unfinished } else { SteerOutcome::Unknown };
                {
                    let network = network.clone(); let local = local.clone(); let roots = roots.clone(); let assignment = assignment.clone();
                    node.store.call("fixture_stopped_workspace_receipts", move |store| {
                    for id in ["second", "third"] {
                        assert!(matches!(store.scheduler_job("c", &message_trigger(id))?.context("settled input")?.state,
                            JobState::Steered { outcome, receipt:Some(receipt), .. } if outcome == expected && receipt.actual_turn_id.as_deref()==Some("turn")));
                    }
                    assert!(store.steer_candidates(&network, &local, "boot")?.is_empty());
                    if !native_has_inputs {
                        assert!(store.workspace_resource_conflicts(&network, &local, &roots, None)?.contains(&assignment),
                            "unknown native input keeps the Assignment's resources reserved");
                    }
                    Ok(())
                    }).await?;
                }
                node.reconcile_workspace_jobs(&network).await?;
                node.steer_messages(&network).await?;
                let mut reads = 0;
                while let Ok(request) = requests.try_recv() {
                    if request["method"] == "thread/read" { reads += 1; }
                    assert!(!request["method"].as_str().is_some_and(|method| method.starts_with("turn/")));
                }
                assert_eq!(reads, 2, "each original input is read once and never resubmitted");
                if !native_has_inputs {
                    assert!(node.store.call("workspace_resource_conflicts", { let network = network.to_owned();
                let local = local.to_owned();
                let roots = roots.to_owned();
                move |store| store.workspace_resource_conflicts(&network, &local, &roots, None) }).await?.contains(&assignment),
                        "later Stop reconciliation cannot release unknown input resources");
                }
                Ok::<_, anyhow::Error>(())
            }.await;
            node.shutdown().await;
            let _ = std::fs::remove_file(socket);
            drop(node);
            std::fs::remove_dir_all(directory)?;
            result?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn stopping_an_unacknowledged_main_input_never_requeues_after_native_not_arrived()
    -> Result<()> {
        use crate::{
            runtime::triage::tests::{bot, node, scripted_app_server, start_reply},
            scheduling::{SessionScope, message_trigger},
        };
        let (node, admin, directory) = node().await?;
        let socket = node
            .providers()
            .switches
            .codex_app_server
            .socket
            .context("fixture socket")?;
        let result = async {
            bot(&node, &admin, "c", "Main", &[]).await?;
            let network = node.store.call("network", |store| store.network()).await?.context("network")?;
            let local = node.identity.device_id()?;
            let (host_trigger, attempt) = start_reply(&node, &admin, "original").await?;
            let trigger = message_trigger("unacknowledged");
            let pending = {
                let identity = node.identity.clone(); let admin = admin.clone(); let network = network.clone();
                let local = local.clone(); let trigger = trigger.clone();
                node.store.call("fixture_unacknowledged_main_input", move |store| {
                store.record_manager_turn(&attempt, "system", "thread", Some("turn"), &Default::default())?;
                store.post_input_fixture(&identity, &network, signed_main_input(&admin, &network, "c", "unacknowledged", "supplement", vec![], 2)?, now_ms())?;
                let candidate = store.steer_candidates(&network, &local, "boot")?.into_iter()
                    .find(|candidate| candidate.message_id=="unacknowledged").context("Main steer candidate")?;
                assert_eq!(candidate.scope, SessionScope::Main);
                assert!(store.begin_steer(&network, &local, "boot", ("c", &trigger), &candidate.into, Some(&candidate.source_event.hash))?);
                store.record_steer_submission("c", &trigger, &candidate.into, None)?;
                let pending = store.scheduler_job("c", &trigger)?.context("pending input")?;
                assert!(matches!(pending.state, JobState::Steering { receipt:None, uncertain:true, .. }));
                store.finish_job(&identity, &network, "c", &host_trigger, &attempt, JobOutcome::Completed, "done", now_ms())?;
                assert!(!store.steer_host_active("c", &attempt)?);
                Ok(pending)
                }).await?
            };
            node.cancel_manager(&test_access(network.clone(), true, Some("c".into())),
                &json!({"conversation_id":"c","trigger":trigger})).await?;
            let mut requests = scripted_app_server(&node, |request| {
                if request["method"] == "thread/read" {
                    assert_eq!(request["params"]["threadId"], "thread");
                    assert_eq!(request["params"]["includeTurns"], true);
                    json!({"result":{"thread":{"id":"thread","turns":[{"id":"turn","status":"completed","items":[]}]}}})
                } else {
                    assert!(!request["method"].as_str().is_some_and(|method| method.starts_with("turn/")), "a stopped Main input cannot write natively");
                    json!({"result":{}})
                }
            }).await?;
            tokio::time::timeout(Duration::from_secs(5), async {
                while node.codex.status().await["connection"]["state"] != "connected" {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }).await?;
            node.settle_steer(&network, &pending).await?;
            node.steer_messages(&network).await?;
            {
                let identity = node.identity.clone();
                node.store.call("fixture_stopped_main_assertions", move |store| {
                assert!(matches!(store.scheduler_job("c", &trigger)?.context("stopped input")?.state,
                    JobState::Steered { outcome:SteerOutcome::Unknown, receipt:None, .. }));
                assert!(store.steering_jobs()?.is_empty());
                assert!(store.steer_candidates(&network, &local, "boot")?.is_empty());
                assert!(store.claim_next_fixture(&identity, &network, "c", "boot", "cfg")?.is_none(),
                    "late NotArrived cannot make a stopped Main input claimable");
                Ok(())
                }).await?;
            }
            let mut reads = 0;
            while let Ok(request) = requests.try_recv() {
                if request["method"] == "thread/read" { reads += 1; }
                assert!(!request["method"].as_str().is_some_and(|method| method.starts_with("turn/")));
            }
            assert_eq!(reads, 1, "only the reader captured before Stop checks the original turn");
            Ok::<_, anyhow::Error>(())
        }.await;
        node.shutdown().await;
        let _ = std::fs::remove_file(socket);
        drop(node);
        std::fs::remove_dir_all(directory)?;
        result
    }

    #[test]
    fn attachment_content_takes_precedence_over_its_filename() {
        assert_eq!(
            attachment_mime("notes.txt", b"\x89PNG\r\n\x1a\nfixture"),
            "image/png"
        );
        assert_eq!(attachment_mime("notes.md", b"requirements"), "text/plain");
    }
}
