use super::*;
use crate::scheduling::{
    AdmissionReceipt, JobOutcome, JobState, OutboxEntry, SCHEDULING_VERSION, SchedulerJob,
};
use crate::store::followup::FollowupMode;
use futures_util::FutureExt;

const MAX_WATCH_SCOPES: usize = 32;
const MAX_WATCH_JOBS: usize = 128;
const MAX_STATUS_JOBS: usize = 512;
const WATCH_LEASE_MS: i64 = 120_000;

fn watch_key(scope: &str) -> String {
    format!("scheduler:v1:watch:{}", crypto::digest(scope.as_bytes()))
}

fn remote_session_key(scope: &str, target: &str) -> String {
    format!(
        "scheduler:v1:remote-session:{}",
        crypto::digest(&serde_json::to_vec(&(scope, target)).expect("string tuple serializes"))
    )
}

fn observation_entries(
    store: &Store,
    network: &str,
    scope: &str,
    local: &str,
) -> Result<Vec<OutboxEntry>> {
    store.authorize_node(network, local, None)?;
    let context = store.context(network, scope, &[])?;
    let mut entries = Vec::new();
    let mut targets = BTreeSet::new();
    for event in context.events.into_iter().rev() {
        if event.scope_id != scope {
            continue;
        }
        let target = match &event.body {
            EventBody::MessagePosted {
                role,
                scheduler_node: Some(target),
                ..
            } if role == "user" => target.clone(),
            EventBody::ExecutionObserved {
                task_id: Some(task),
                target_node,
                status,
                ..
            } if event.author == *target_node
                && matches!(status.as_str(), "succeeded" | "failed" | "rejected") =>
            {
                let Ok(policy) = store.historical_followup_policy(network, scope, task) else {
                    continue;
                };
                policy.target_node
            }
            _ => continue,
        };
        if target == local
            || store
                .authorize_node(network, &target, Some(NodeRole::Scheduler))
                .is_err()
        {
            continue;
        }
        if !targets.insert(target.clone()) {
            continue;
        }
        let trigger = crypto::digest(&serde_json::to_vec(&event)?);
        let envelope = store
            .event(network, &trigger)?
            .context("observed scheduling event missing")?;
        let verified = store.verify_event(network, &envelope)?;
        ensure!(
            verified.scope_id == scope && verified == event,
            "observed scheduling event mismatch"
        );
        entries.push(OutboxEntry {
            version: SCHEDULING_VERSION,
            network_id: network.into(),
            conversation_id: scope.into(),
            trigger,
            target_node: target,
            delivered: true,
        });
        if entries.len() == MAX_WATCH_JOBS {
            break;
        }
    }
    Ok(entries)
}

fn register_observation(store: &mut Store, network: &str, scope: &str, now: i64) -> Result<()> {
    let mut watched = Vec::new();
    for candidate in store.scopes(network)? {
        let key = watch_key(&candidate);
        if let Some(marker) = store.local_value(network, &key)? {
            let expires = marker["expires_at_ms"].as_i64().unwrap_or(0);
            if marker["version"] != SCHEDULING_VERSION || expires <= now {
                store.local_delete(network, &key)?;
            } else if candidate != scope {
                watched.push((expires, key));
            }
        }
    }
    watched.sort();
    let remove = watched
        .len()
        .saturating_add(1)
        .saturating_sub(MAX_WATCH_SCOPES);
    for (_, key) in watched.into_iter().take(remove) {
        store.local_delete(network, &key)?;
    }
    store.local_set(
        network,
        &watch_key(scope),
        &json!({"version":SCHEDULING_VERSION,"expires_at_ms":now.saturating_add(WATCH_LEASE_MS)}),
    )
}

fn remote_key(entry: &OutboxEntry) -> String {
    format!(
        "scheduler:v1:remote:{}",
        crypto::digest(
            &serde_json::to_vec(&(&entry.conversation_id, &entry.target_node, &entry.trigger,))
                .expect("string tuple serializes")
        )
    )
}

fn terminal(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "cancelled" | "unknown")
}

fn job_value(job: &SchedulerJob) -> Result<Value> {
    let mut value = serde_json::to_value(job)?;
    value["read_only"] = json!(job.state.attempt().is_some_and(|attempt| attempt.read_only));
    if matches!(job.state, JobState::Completed { .. }) {
        value["reply_message_id"] = json!(job.message_id);
    }
    match &job.state {
        JobState::Failed { detail, .. } | JobState::Unknown { detail, .. } => {
            value["error"] = json!(detail);
        }
        _ => {}
    }
    Ok(value)
}

fn write_frozen(store: &Store, network: &str, scope: &str, target: &str) -> Result<bool> {
    for job in store.scheduler_jobs(network, scope, target)? {
        if matches!(job.state, JobState::Unknown { .. })
            && store
                .local_value(
                    network,
                    &crate::scheduling::scheduler_key("resolution", scope, target, &job.trigger),
                )?
                .is_none()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn origin_message_id(store: &Store, network: &str, trigger: &str) -> Result<Option<String>> {
    let envelope = store
        .event(network, trigger)?
        .context("scheduler trigger missing")?;
    let event = store.verify_event(network, &envelope)?;
    Ok(match event.body {
        EventBody::MessagePosted {
            role, message_id, ..
        } if role == "user" => Some(message_id),
        _ => None,
    })
}

fn validate_reply(entry: &OutboxEntry, reply: &Value) -> Result<(AdmissionReceipt, SchedulerJob)> {
    let receipt: AdmissionReceipt = serde_json::from_value(reply["receipt"].clone())
        .context("target did not return a reliable scheduling receipt")?;
    ensure!(
        receipt.version == SCHEDULING_VERSION
            && entry.version == SCHEDULING_VERSION
            && receipt.network_id == entry.network_id
            && receipt.conversation_id == entry.conversation_id
            && receipt.trigger == entry.trigger
            && receipt.target_node == entry.target_node,
        "scheduling receipt identity or version mismatch"
    );
    let job: SchedulerJob = serde_json::from_value(reply["job"].clone())
        .context("target did not return its durable job")?;
    ensure!(
        job.version == SCHEDULING_VERSION
            && job.sequence > 0
            && job.receipt(&entry.network_id) == receipt,
        "scheduling job does not match its receipt"
    );
    Ok((receipt, job))
}

fn observed_job_value(store: &Store, entry: &OutboxEntry, job: &SchedulerJob) -> Result<Value> {
    let identity = OutboxEntry {
        trigger: job.trigger.clone(),
        ..entry.clone()
    };
    validate_reply(
        &identity,
        &json!({"receipt":job.receipt(&entry.network_id),"job":job}),
    )?;
    let envelope = store
        .event(&entry.network_id, &job.trigger)?
        .context("observed job trigger missing")?;
    let event = store.verify_event(&entry.network_id, &envelope)?;
    ensure!(
        event.scope_id == entry.conversation_id,
        "observed job belongs to another conversation"
    );
    match &event.body {
        EventBody::MessagePosted {
            role,
            scheduler_node: Some(target),
            ..
        } if role == "user" && target == &entry.target_node => {
            ensure!(
                job.kind == crate::scheduling::TriggerKind::Human,
                "observed job trigger kind mismatch"
            );
        }
        EventBody::ExecutionObserved {
            task_id: Some(task),
            target_node,
            status,
            ..
        } if event.author == *target_node
            && matches!(status.as_str(), "succeeded" | "failed" | "rejected") =>
        {
            ensure!(
                job.kind == crate::scheduling::TriggerKind::Autonomous,
                "observed job trigger kind mismatch"
            );
            ensure!(
                store
                    .historical_followup_policy(&entry.network_id, &entry.conversation_id, task)?
                    .target_node
                    == entry.target_node,
                "observed job belongs to another task scheduler"
            );
        }
        _ => bail!("observed job has no verified scheduling identity"),
    }
    let mut value = job_value(job)?;
    value["message_id"] = json!(origin_message_id(store, &entry.network_id, &job.trigger)?);
    Ok(value)
}

fn proof_heads(input: &Value, trigger: &str) -> Result<Vec<String>> {
    let mut wanted: Vec<String> = serde_json::from_value(input["basis_heads"].clone())
        .context("reliable scheduling needs its proof heads")?;
    ensure!(wanted.len() <= 256, "too many scheduling proof heads");
    wanted.push(trigger.into());
    wanted.sort();
    wanted.dedup();
    ensure!(
        wanted
            .iter()
            .all(|hash| hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit())),
        "invalid scheduling proof hash"
    );
    Ok(wanted)
}

impl Runtime {
    fn scheduler_manager_metadata(&self) -> Value {
        let Some(manager) = &self.config.manager else {
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
                if let Err(error) = node.schedule_delivery_tick().await {
                    tracing::warn!(%error, "durable scheduling reconciliation is pending");
                }
            }
        });
        let node = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            loop {
                tick.tick().await;
                let _work = node.maintenance.enter_when_open().await;
                if let Err(error) = node.flush_schedule_deliveries().await {
                    tracing::warn!(%error, "remote scheduling delivery is pending");
                }
            }
        });
        let node = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            loop {
                tick.tick().await;
                let _work = node.maintenance.enter_when_open().await;
                if let Err(error) = node.poll_watched_schedules().await {
                    tracing::debug!(%error, "read-only scheduler observation is pending");
                }
            }
        });
    }

    async fn poll_watched_schedules(&self) -> Result<()> {
        let local = self.identity.node_id()?;
        let mut watched = Vec::new();
        {
            let mut store = self.store.lock().await;
            for network in store.networks()? {
                let mut scopes = Vec::new();
                for scope in store.scopes(&network)? {
                    let key = watch_key(&scope);
                    if let Some(marker) = store.local_value(&network, &key)? {
                        let expires = marker["expires_at_ms"].as_i64().unwrap_or(0);
                        if marker["version"] != SCHEDULING_VERSION || expires <= now_ms() {
                            store.local_delete(&network, &key)?;
                        } else {
                            scopes.push((expires, scope));
                        }
                    }
                }
                scopes.sort_by(|a, b| b.cmp(a));
                for (_, scope) in scopes.into_iter().take(MAX_WATCH_SCOPES) {
                    watched.extend(
                        observation_entries(&store, &network, &scope, &local)?
                            .into_iter()
                            .enumerate(),
                    );
                }
            }
        }
        watched.sort_by_key(|(rank, _)| *rank);
        watched.truncate(MAX_WATCH_JOBS);
        futures_util::stream::iter(watched.into_iter().map(|(_, entry)| entry))
            .for_each_concurrent(4, |entry| async move {
                let baseline = self
                    .store
                    .lock()
                    .await
                    .local_value(
                        &entry.network_id,
                        &remote_session_key(&entry.conversation_id, &entry.target_node),
                    )
                    .ok()
                    .flatten()
                    .and_then(|value| value["status_revision"].as_u64())
                    .unwrap_or(0);
                if let Err(error) = self.refresh_remote_schedule(&entry, false, true).await
                    && let Err(cache_error) =
                        self.cache_observation_error(&entry, &error, baseline).await
                {
                    tracing::debug!(%cache_error,"cannot retain scheduler observation");
                }
            })
            .await;
        Ok(())
    }

    async fn cache_observation_error(
        &self,
        entry: &OutboxEntry,
        error: &anyhow::Error,
        baseline: u64,
    ) -> Result<()> {
        let mut store = self.store.lock().await;
        let key = remote_key(entry);
        let session_key = remote_session_key(&entry.conversation_id, &entry.target_node);
        let session = store.local_value(&entry.network_id, &session_key)?;
        if session
            .as_ref()
            .and_then(|v| v["status_revision"].as_u64())
            .is_some_and(|current| current > baseline)
        {
            return Ok(());
        }
        let mut changed = false;
        if let Some(previous) = store.local_value(&entry.network_id, &key)? {
            let mut value = previous.clone();
            value["freshness"] = json!("unknown");
            value["observation_error"] = json!(format!("{error:#}"));
            if previous != value {
                store.local_set(&entry.network_id, &key, &value)?;
                changed = true;
            }
        }
        if let Some(mut session) = session
            && session["freshness"] != "unknown"
        {
            session["freshness"] = json!("unknown");
            store.local_set(&entry.network_id, &session_key, &session)?;
            changed = true;
        }
        if changed {
            drop(store);
            self.publish_changed(&entry.network_id, &entry.conversation_id, &entry.trigger);
        }
        Ok(())
    }

    async fn schedule_delivery_tick(self: &Arc<Self>) -> Result<()> {
        let local = self.identity.node_id()?;
        let networks = self.store.lock().await.networks()?;
        for network in networks {
            if self.config.roles.contains(&NodeRole::Scheduler) {
                let publications = self.store.lock().await.flush_scheduler_publications(
                    &self.identity,
                    &network,
                    &local,
                )?;
                for publication in publications.published {
                    for hash in publication.event_hashes {
                        self.publish_changed(&network, &publication.job.conversation_id, &hash);
                    }
                }
                for error in publications.errors {
                    tracing::warn!(%error, "known management outcome awaits history publication");
                }
                let workers = self.schedule_workers.lock().await.clone();
                let (unknown, abandoned) = {
                    let mut store = self.store.lock().await;
                    store.recover_scheduler_jobs(&network, &local, crate::scheduler::boot_id())?;
                    let mut unknown = Vec::new();
                    let mut abandoned = Vec::new();
                    for scope in store.scopes(&network)? {
                        for job in store.scheduler_jobs(&network, &scope, &local)? {
                            if store.scheduler_publication_pending(
                                &network,
                                &scope,
                                &local,
                                &job.trigger,
                            )? {
                                continue;
                            }
                            if let JobState::Unknown {
                                attempt,
                                fenced: false,
                                ..
                            } = &job.state
                            {
                                unknown.push((scope.clone(), job.trigger, attempt.id.clone()));
                            } else if matches!(
                                job.state,
                                JobState::Claimed { .. }
                                    | JobState::Running { .. }
                                    | JobState::Cancelling { .. }
                            ) && !workers.contains(&(network.clone(), scope.clone()))
                            {
                                let attempt = job
                                    .state
                                    .attempt()
                                    .context("active scheduler attempt missing")?;
                                abandoned.push((
                                    scope.clone(),
                                    job.trigger.clone(),
                                    attempt.id.clone(),
                                ));
                            }
                        }
                    }
                    (unknown, abandoned)
                };
                for (scope, trigger, attempt) in abandoned {
                    self.fence_manager_attempt(&attempt).await?;
                    self.store.lock().await.finish_job(&self.identity,&network,&scope,&local,&trigger,&attempt,
                        JobOutcome::Unknown("management consumer ended before a durable terminal outcome; reconcile the original attempt".into()),vec![],now_ms())?;
                    self.store
                        .lock()
                        .await
                        .fence_manager_unknown(&network, &scope, &local, &trigger, &attempt)?;
                    self.finish_manager_attempt(&attempt).await;
                    self.publish_changed(&network, &scope, &trigger);
                }
                for (scope, trigger, attempt) in unknown {
                    // Tokens are process-local; draining the admission lease also accounts for live tools.
                    self.fence_manager_attempt(&attempt).await?;
                    self.store
                        .lock()
                        .await
                        .fence_manager_unknown(&network, &scope, &local, &trigger, &attempt)?;
                    self.finish_manager_attempt(&attempt).await;
                }
                if self.config.manager.is_some() {
                    let scopes = self
                        .store
                        .lock()
                        .await
                        .pending_scheduler_conversations(&network, &local)?;
                    for scope in scopes {
                        self.start_schedule_consumer(network.clone(), scope).await;
                    }
                }
            }
            self.flush_followup_markers(&network).await?;
        }
        Ok(())
    }

    async fn flush_schedule_deliveries(&self) -> Result<()> {
        let networks = self.store.lock().await.networks()?;
        for network in networks {
            let entries = self.store.lock().await.scheduler_outbox_all(&network)?;
            futures_util::stream::iter(entries)
                .for_each_concurrent(4, |entry| async move {
                    if let Err(error) = self.deliver_schedule(&entry).await {
                        let mut store = self.store.lock().await;
                        let key = remote_key(&entry);
                        let previous = match store.local_value(&entry.network_id,&key) {
                            Ok(previous) => previous,
                            Err(cache_error) => {
                                tracing::warn!(%cache_error,"cannot read scheduling observation");
                                return;
                            }
                        };
                        let mut value = previous.clone()
                            .unwrap_or_else(|| json!({"state":if entry.delivered { "delivered" } else { "pending_delivery" },"trigger":entry.trigger,"target_node":entry.target_node,"conversation_id":entry.conversation_id,"kind":"human","sequence":0}));
                        value["delivery_error"] = json!(format!("{error:#}"));
                        value["freshness"] = json!("unreachable");
                        if let Ok(message_id) = origin_message_id(&store, &entry.network_id, &entry.trigger) {
                            value["kind"] = json!(if message_id.is_some() { "human" } else { "autonomous" });
                            value["message_id"] = json!(message_id);
                        }
                        if previous.as_ref() != Some(&value) {
                            if let Err(cache_error) = store.local_set(&entry.network_id, &key, &value) {
                                tracing::warn!(%cache_error,"cannot save scheduling observation");
                                return;
                            }
                            drop(store);
                            self.publish_changed(&entry.network_id,&entry.conversation_id,&entry.trigger);
                        }
                    }
                })
                .await;
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
        let local = self.identity.node_id()?;
        loop {
            let Some(trigger) = self
                .store
                .lock()
                .await
                .scheduler_jobs(network, scope, &local)?
                .into_iter()
                .find(|job| matches!(job.state, JobState::Queued))
                .map(|job| job.trigger)
            else {
                return Ok(());
            };
            let result = crate::scheduler::run(
                self.clone(),
                Access {
                    network_id: network.into(),
                    human: false,
                    conversation_id: Some(scope.into()),
                    manager: None,
                },
                scope.into(),
                trigger.clone(),
            )
            .await?;
            self.publish_changed(network, scope, &trigger);
            if matches!(
                result["state"].as_str(),
                Some("queued" | "claimed" | "running" | "cancelling")
            ) {
                return Ok(());
            }
        }
    }

    pub(super) fn spawn_scheduler(
        self: &Arc<Self>,
        _network: String,
        _scope: String,
        _trigger: String,
    ) -> Result<()> {
        let _work = self.maintenance.enter()?;
        self.schedule_wakeup.notify_one();
        Ok(())
    }

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
        self.require_role(network, NodeRole::Gateway).await?;
        let message_id = id(input, "message_id")?;
        let local = self.identity.node_id()?;
        if let Some(entry) = input.get("entry_node") {
            ensure!(
                entry.as_str() == Some(local.as_str()),
                "retry this message through its original gateway; do not resubmit it at another entry"
            );
        }
        let target = input["scheduler_node"]
            .as_str()
            .unwrap_or(&local)
            .to_owned();
        if target == local {
            self.require_role(network, NodeRole::Scheduler).await?;
            ensure!(
                self.config.manager.is_some(),
                "this Node has no configured management SDK"
            );
        }
        let text = field(input, "text")?.to_owned();
        let (event, receipt) = {
            let mut store = self.store.lock().await;
            ensure!(
                store
                    .context(network, &scope, &heads(input)?)?
                    .missing
                    .is_complete(),
                "context is incomplete; wait for synchronization"
            );
            store.accept_message(
                &self.identity,
                network,
                &scope,
                EventBody::MessagePosted {
                    message_id: message_id.clone(),
                    role: "user".into(),
                    text,
                    scheduler_node: Some(target.clone()),
                },
                vec![],
                now_ms(),
                &local,
                &target,
                input["report_results"].as_bool().unwrap_or(false),
            )?
        };
        self.publish_changed(network, &scope, &event.hash);
        self.spawn_scheduler(network.clone(), scope, event.hash.clone())?;
        Ok(
            json!({"accepted":true,"queued":target == local,"state":receipt.state,"trigger":event.hash,
            "message_id":message_id,"scheduler_node":target,"receipt":receipt}),
        )
    }

    pub(super) async fn receive_schedule(self: &Arc<Self>, message: &Inbound) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        ensure!(
            !message.broadcast,
            "scheduling requires an acknowledged request"
        );
        let network = &message.network_id;
        let input = &message.payload;
        ensure!(
            input["version"].as_u64() == Some(u64::from(SCHEDULING_VERSION)),
            "reliable scheduling protocol version 1 is required; upgrade the sender"
        );
        let local = self.identity.node_id()?;
        ensure!(
            input["target_node"].as_str() == Some(local.as_str()),
            "wrong scheduling target"
        );
        self.require_role(network, NodeRole::Scheduler).await?;
        ensure!(
            self.config.manager.is_some(),
            "this Node has no configured management SDK"
        );
        let sender = self
            .store
            .lock()
            .await
            .authorize_node(network, &message.origin, None)?;
        ensure!(
            sender.roles.iter().any(|role| matches!(
                role,
                NodeRole::Gateway | NodeRole::Scheduler | NodeRole::Executor
            )),
            "origin cannot schedule"
        );
        let scope = id(input, "conversation_id")?;
        let trigger = field(input, "trigger")?;
        let wanted = proof_heads(input, trigger)?;
        self.fetch_closure(network, &message.origin, &wanted)
            .await?;
        {
            let mut store = self.store.lock().await;
            let envelope = store
                .event(network, trigger)?
                .context("scheduling trigger is missing")?;
            let event = store.verify_event(network, &envelope)?;
            match &event.body {
                EventBody::MessagePosted { role, .. } if role == "user" => {
                    ensure!(
                        sender.roles.contains(&NodeRole::Gateway)
                            || sender.roles.contains(&NodeRole::Scheduler),
                        "executor may only schedule its own result in this conversation"
                    );
                    ensure!(
                        store
                            .report_intent_head(network, &scope, trigger)?
                            .is_some(),
                        "scheduling intent proof is missing"
                    );
                }
                EventBody::ExecutionObserved { .. } => {
                    if !sender.roles.contains(&NodeRole::Gateway)
                        && !sender.roles.contains(&NodeRole::Scheduler)
                    {
                        ensure!(
                            event.author == message.origin,
                            "executor may only schedule its own result in this conversation"
                        );
                    }
                    store.validate_followup_result(network, &scope, trigger, &local)?;
                }
                _ => bail!("invalid scheduling trigger"),
            }
            store.enqueue_trigger(network, &scope, trigger, &local)?;
        }
        self.spawn_scheduler(network.clone(), scope.clone(), trigger.into())?;
        self.manager_status(network, &scope, trigger).await
    }

    pub(super) async fn manager_status(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
    ) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        let local = self.identity.node_id()?;
        let mut store = self.store.lock().await;
        store.recover_scheduler_jobs(network, &local, crate::scheduler::boot_id())?;
        let mut jobs = Vec::new();
        let mut recent_terminal = 0;
        for job in store
            .scheduler_jobs(network, scope, &local)?
            .into_iter()
            .rev()
        {
            let unresolved = matches!(job.state, JobState::Unknown { .. })
                && store
                    .local_value(
                        network,
                        &crate::scheduling::scheduler_key(
                            "resolution",
                            scope,
                            &local,
                            &job.trigger,
                        ),
                    )?
                    .is_none();
            let pending = matches!(
                job.state,
                JobState::Queued
                    | JobState::Claimed { .. }
                    | JobState::Running { .. }
                    | JobState::Cancelling { .. }
            );
            if pending || unresolved {
                jobs.push(job);
            } else if recent_terminal < 32 {
                jobs.push(job);
                recent_terminal += 1;
            }
        }
        ensure!(
            jobs.len() <= MAX_STATUS_JOBS,
            "too many unresolved scheduler jobs to observe"
        );
        let revision_key = format!(
            "scheduler:v1:status-revision:{}",
            crypto::digest(scope.as_bytes())
        );
        let revision = store
            .local_value(network, &revision_key)?
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .checked_add(1)
            .context("scheduler status revision exhausted")?;
        store.local_set(network, &revision_key, &json!(revision))?;
        let mut reply = json!({"state":"not_found","trigger":trigger,"version":SCHEDULING_VERSION,
            "network_id":network,"conversation_id":scope,"target_node":local,"status_revision":revision,
            "jobs":jobs,"heads":store.heads(network,scope)?,"manager":self.scheduler_manager_metadata(),
            "write_frozen":write_frozen(&store,network,scope,&local)?});
        if let Some(job) = store.scheduler_job(network, scope, trigger, &local)? {
            reply["accepted"] = json!(true);
            reply["queued"] = json!(true);
            reply["state"] = json!(job.state.name());
            reply["receipt"] = json!(job.receipt(network));
            reply["job"] = job_value(&job)?;
        }
        Ok(reply)
    }

    async fn deliver_schedule(&self, entry: &OutboxEntry) -> Result<()> {
        self.refresh_remote_schedule(entry, true, false).await
    }

    async fn refresh_remote_schedule(
        &self,
        entry: &OutboxEntry,
        acknowledge: bool,
        force: bool,
    ) -> Result<()> {
        let key = remote_key(entry);
        if !force
            && entry.delivered
            && self
                .store
                .lock()
                .await
                .local_value(&entry.network_id, &key)?
                .is_some_and(|value| {
                    terminal(value["state"].as_str().unwrap_or(""))
                        && !(value["state"] == "unknown" && value["write_frozen"] != false)
                })
        {
            return Ok(());
        }
        let capability = self
            .read_peer(
                &entry.network_id,
                &entry.target_node,
                json!({"type":"capabilities"}),
            )
            .await?;
        ensure!(
            capability["reliable_schedule"]["version"].as_u64()
                == Some(u64::from(SCHEDULING_VERSION)),
            "target does not support reliable scheduling protocol 1; upgrade the original target"
        );
        let request = if entry.delivered || !acknowledge {
            json!({"type":"manager_status","conversation_id":entry.conversation_id,"trigger":entry.trigger})
        } else {
            let basis = self
                .store
                .lock()
                .await
                .heads(&entry.network_id, &entry.conversation_id)?;
            ensure!(basis.len() <= 256, "too many scheduling proof heads");
            json!({"type":"schedule","version":SCHEDULING_VERSION,"target_node":entry.target_node,
                "conversation_id":entry.conversation_id,"trigger":entry.trigger,"basis_heads":basis})
        };
        let reply = self
            .read_peer(&entry.network_id, &entry.target_node, request)
            .await?;
        ensure!(
            reply["version"] == SCHEDULING_VERSION
                && reply["network_id"] == entry.network_id
                && reply["conversation_id"] == entry.conversation_id
                && reply["target_node"] == entry.target_node
                && reply["trigger"] == entry.trigger,
            "scheduler status scope or version mismatch"
        );
        let revision = reply["status_revision"]
            .as_u64()
            .filter(|v| *v > 0)
            .context("scheduler status revision is required")?;
        let primary = if !acknowledge && reply["state"] == "not_found" {
            None
        } else {
            Some(validate_reply(entry, &reply)?)
        };
        let jobs: Vec<SchedulerJob> = serde_json::from_value(reply["jobs"].clone())?;
        ensure!(
            jobs.len() <= MAX_STATUS_JOBS,
            "too many observed scheduler jobs"
        );
        ensure!(
            reply["write_frozen"].is_boolean(),
            "scheduler omitted its write-frozen state"
        );
        if acknowledge {
            let receipt = &primary
                .as_ref()
                .context("scheduling admission receipt is missing")?
                .0;
            self.store
                .lock()
                .await
                .acknowledge_outbox(&entry.network_id, receipt)?;
        }
        {
            let wanted: Vec<String> = serde_json::from_value(reply["heads"].clone())?;
            ensure!(wanted.len() <= 256, "too many result proof heads");
            if !self
                .store
                .lock()
                .await
                .missing_dependencies(&entry.network_id, &wanted)?
                .is_complete()
            {
                self.fetch_closure(&entry.network_id, &entry.target_node, &wanted)
                    .await?;
            }
            ensure!(
                self.store
                    .lock()
                    .await
                    .context(&entry.network_id, &entry.conversation_id, &wanted)?
                    .missing
                    .is_complete(),
                "observed scheduling history is incomplete"
            );
        }
        let changed = {
            let mut store = self.store.lock().await;
            store.authorize_node(
                &entry.network_id,
                &entry.target_node,
                Some(NodeRole::Scheduler),
            )?;
            let session_key = remote_session_key(&entry.conversation_id, &entry.target_node);
            let previous = store.local_value(&entry.network_id, &session_key)?;
            if previous
                .as_ref()
                .and_then(|value| value["status_revision"].as_u64())
                .is_some_and(|previous| previous >= revision)
            {
                return Ok(());
            }
            let mut observed = jobs
                .iter()
                .map(|job| observed_job_value(&store, entry, job))
                .collect::<Result<Vec<_>>>()?;
            for value in &mut observed {
                value["status_revision"] = json!(revision);
                value["freshness"] = json!("observed");
            }
            let session = json!({"manager":reply["manager"],"write_frozen":reply["write_frozen"],"checked_at_ms":now_ms(),
                "freshness":"observed","status_revision":revision,"jobs":observed});
            let mut changed = previous.is_none_or(|mut previous| {
                previous["checked_at_ms"] = session["checked_at_ms"].clone();
                previous["status_revision"] = session["status_revision"].clone();
                previous != session
            });
            if let Some((_, job)) = primary {
                let mut value = observed_job_value(&store, entry, &job)?;
                value["manager"] = reply["manager"].clone();
                value["write_frozen"] = reply["write_frozen"].clone();
                value["freshness"] = json!("observed");
                value["checked_at_ms"] = session["checked_at_ms"].clone();
                value["status_revision"] = json!(revision);
                changed |=
                    store
                        .local_value(&entry.network_id, &key)?
                        .is_none_or(|mut previous| {
                            previous["checked_at_ms"] = value["checked_at_ms"].clone();
                            previous != value
                        });
                store.local_set(&entry.network_id, &key, &value)?;
            }
            store.local_set(&entry.network_id, &session_key, &session)?;
            changed
        };
        if changed {
            self.publish_changed(&entry.network_id, &entry.conversation_id, &entry.trigger);
        }
        Ok(())
    }

    pub(super) async fn scheduler_snapshot_jobs(
        &self,
        network: &str,
        scope: &str,
    ) -> Result<Vec<Value>> {
        let local = self.identity.node_id()?;
        let mut store = self.store.lock().await;
        register_observation(&mut store, network, scope, now_ms())?;
        let mut jobs = store
            .scheduler_jobs(network, scope, &local)?
            .iter()
            .map(job_value)
            .collect::<Result<Vec<_>>>()?;
        let frozen = write_frozen(&store, network, scope, &local)?;
        for job in &mut jobs {
            job["manager"] = self.scheduler_manager_metadata();
            job["write_frozen"] = json!(frozen);
            let trigger = field(job, "trigger")?.to_owned();
            job["message_id"] = json!(origin_message_id(&store, network, &trigger)?);
        }
        for entry in store
            .scheduler_outbox_all(network)?
            .into_iter()
            .filter(|entry| entry.conversation_id == scope)
        {
            if let Some(value) = store.local_value(network, &remote_key(&entry))? {
                jobs.push(value);
                continue;
            }
            let event = store
                .event(network, &entry.trigger)?
                .context("outbox trigger missing")?;
            let event = store.verify_event(network, &event)?;
            let (kind, message_id) = match event.body {
                EventBody::MessagePosted { message_id, .. } => ("human", message_id),
                _ => ("autonomous", String::new()),
            };
            jobs.push(json!({"trigger":entry.trigger,"target_node":entry.target_node,
                "conversation_id":scope,"state":if entry.delivered {"delivered"} else {"pending_delivery"},
                "kind":kind,"message_id":message_id,"sequence":0,"freshness":"unknown"}));
        }
        for entry in observation_entries(&store, network, scope, &local)? {
            let mut observed = Vec::new();
            if let Some(value) = store.local_value(network, &remote_key(&entry))? {
                observed.push(value);
            }
            if let Some(session) =
                store.local_value(network, &remote_session_key(scope, &entry.target_node))?
            {
                observed.extend(session["jobs"].as_array().cloned().unwrap_or_default());
            }
            for value in observed {
                if let Some(index) = jobs.iter().position(|job| {
                    job["trigger"] == value["trigger"] && job["target_node"] == value["target_node"]
                }) {
                    if value["status_revision"].as_u64().unwrap_or(0)
                        > jobs[index]["status_revision"].as_u64().unwrap_or(0)
                    {
                        jobs[index] = value;
                    }
                } else {
                    jobs.push(value);
                }
            }
        }
        for job in &mut jobs {
            let Some(target) = job["target_node"].as_str() else {
                continue;
            };
            if target == local {
                continue;
            }
            if let Some(session) = store.local_value(network, &remote_session_key(scope, target))? {
                job["manager"] = session["manager"].clone();
                job["write_frozen"] = session["write_frozen"].clone();
                if session["freshness"] != "observed"
                    || session["checked_at_ms"]
                        .as_i64()
                        .unwrap_or(0)
                        .saturating_add(WATCH_LEASE_MS)
                        < now_ms()
                {
                    job["freshness"] = json!("unknown");
                }
            }
        }
        Ok(jobs)
    }

    pub(super) async fn maybe_followup(
        self: &Arc<Self>,
        request: &OperationRequest,
        result_hash: &str,
    ) -> Result<bool> {
        let _work = self.maintenance.enter()?;
        if request.task_id.is_none() || request.target_node != self.identity.node_id()? {
            return Ok(true);
        }
        let key = format!("followup-wait:v1:{}", request.operation_id);
        {
            let mut store = self.store.lock().await;
            if let Some(previous) = store.local_value(&request.network_id, &key)? {
                ensure!(
                    previous["trigger"] == result_hash,
                    "follow-up result identity changed"
                );
            } else {
                store.local_set(&request.network_id, &key, &json!({"trigger":result_hash}))?;
            }
        }
        if let Err(error) = self.admit_followup_marker(request, result_hash).await {
            self.store.lock().await.local_set(
                &request.network_id,
                &key,
                &json!({"trigger":result_hash,"error":format!("{error:#}")}),
            )?;
        }
        // Publishing is complete even while consent is absent. The separate local marker survives.
        Ok(true)
    }

    async fn flush_followup_markers(self: &Arc<Self>, network: &str) -> Result<()> {
        let local = self.identity.node_id()?;
        let operations = self.store.lock().await.operations(network)?;
        for operation in operations.into_iter().filter(|operation| {
            operation.state.terminal() && operation.request.target_node == local
        }) {
            let marker = self.store.lock().await.local_value(
                network,
                &format!("followup-wait:v1:{}", operation.request.operation_id),
            )?;
            if let Some(marker) = marker {
                let trigger = field(&marker, "trigger")?;
                if let Err(error) = self
                    .admit_followup_marker(&operation.request, trigger)
                    .await
                {
                    tracing::debug!(%error, operation_id=%operation.request.operation_id, "result follow-up remains pending");
                }
            }
        }
        Ok(())
    }

    async fn admit_followup_marker(
        self: &Arc<Self>,
        request: &OperationRequest,
        result_hash: &str,
    ) -> Result<()> {
        let Some(task) = request.task_id.as_deref() else {
            return Ok(());
        };
        let local = self.identity.node_id()?;
        {
            let mut store = self.store.lock().await;
            let policy =
                match store.followup_policy(&request.network_id, &request.conversation_id, task) {
                    Ok(policy) if policy.mode != FollowupMode::Off => policy,
                    _ => return Ok(()),
                };
            store.validate_followup_result(
                &request.network_id,
                &request.conversation_id,
                result_hash,
                &policy.target_node,
            )?;
            store.admit_trigger(
                &request.network_id,
                &request.conversation_id,
                result_hash,
                &local,
                &policy.target_node,
            )?;
            // Admission is durable first; a crash before deletion only repeats the same identity.
            store.local_delete(
                &request.network_id,
                &format!("followup-wait:v1:{}", request.operation_id),
            )?;
        }
        self.schedule_wakeup.notify_one();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::TriggerKind;

    #[test]
    fn remote_ack_requires_the_original_complete_durable_identity() -> Result<()> {
        let entry = OutboxEntry {
            version: 1,
            network_id: "personal".into(),
            conversation_id: "chat".into(),
            trigger: "result".into(),
            target_node: "original".into(),
            delivered: false,
        };
        let job = SchedulerJob {
            version: 1,
            conversation_id: "chat".into(),
            trigger: "result".into(),
            target_node: "original".into(),
            kind: TriggerKind::Human,
            sequence: 1,
            basis_heads: vec![],
            message_id: "reply".into(),
            state: JobState::Queued,
        };
        let reply = json!({"receipt":job.receipt("personal"),"job":job});
        assert_eq!(validate_reply(&entry, &reply)?.0.state, "queued");
        assert!(validate_reply(&entry, &json!({"state":"accepted"})).is_err());
        for key in [
            "version",
            "network_id",
            "conversation_id",
            "trigger",
            "target_node",
            "state",
        ] {
            let mut changed = reply.clone();
            changed["receipt"][key] = if key == "version" {
                json!(2)
            } else {
                json!("different")
            };
            assert!(
                validate_reply(&entry, &changed).is_err(),
                "accepted mismatched {key}"
            );
        }
        let mut changed = reply;
        changed["job"]["sequence"] = json!(0);
        assert!(validate_reply(&entry, &changed).is_err());
        Ok(())
    }

    #[test]
    fn scheduling_proof_requires_bounded_explicit_signed_event_references() -> Result<()> {
        let trigger = "a".repeat(64);
        let proof = "b".repeat(64);
        assert_eq!(
            proof_heads(
                &json!({"basis_heads":[proof.clone(),trigger.clone(),proof.clone()]}),
                &trigger
            )?,
            vec![trigger.clone(), proof]
        );
        assert!(proof_heads(&json!({}), &trigger).is_err());
        assert!(proof_heads(&json!({"basis_heads":["not-a-hash"]}), &trigger).is_err());
        assert!(proof_heads(&json!({"basis_heads":vec![trigger.clone();257]}), &trigger).is_err());
        Ok(())
    }

    #[test]
    fn third_entry_observation_uses_signed_targets_without_admitting_work() -> Result<()> {
        let gateway = LocalIdentity::generate();
        let manager = LocalIdentity::generate();
        let viewer = LocalIdentity::generate();
        let network = gateway.fixture_network_id()?;
        let mut store = Store::open(":memory:")?;
        store.install_trust_bundle(
            &network,
            &gateway.sign_network_fixture(
                1,
                vec![
                    gateway.certificate(vec![NodeRole::Gateway, NodeRole::Replica])?,
                    manager.certificate(vec![NodeRole::Scheduler, NodeRole::Replica])?,
                    viewer.certificate(vec![NodeRole::Replica])?,
                ],
            )?,
        )?;
        let target = manager.node_id()?;
        store.append_event(
            &gateway,
            &network,
            "chat",
            EventBody::ConversationCreated {
                title: "chat".into(),
            },
            vec![],
            1,
        )?;
        let message = store.append_event(
            &gateway,
            &network,
            "chat",
            EventBody::MessagePosted {
                message_id: "human".into(),
                role: "user".into(),
                text: "delegate".into(),
                scheduler_node: Some(target.clone()),
            },
            vec![],
            2,
        )?;
        let entries = observation_entries(&store, &network, "chat", &viewer.node_id()?)?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].trigger, message.hash);
        assert_eq!(entries[0].target_node, target);
        let mut job = SchedulerJob {
            version: 1,
            conversation_id: "chat".into(),
            trigger: message.hash.clone(),
            target_node: target.clone(),
            kind: crate::scheduling::TriggerKind::Human,
            sequence: 1,
            basis_heads: vec![message.hash.clone()],
            message_id: "reply".into(),
            state: JobState::Queued,
        };
        assert_eq!(
            observed_job_value(&store, &entries[0], &job)?["message_id"],
            "human"
        );
        job.target_node = viewer.node_id()?;
        assert!(observed_job_value(&store, &entries[0], &job).is_err());
        let mut revoked = gateway.certificate(vec![NodeRole::Gateway, NodeRole::Replica])?;
        revoked.revoked = true;
        store.install_trust_bundle(
            &network,
            &gateway.sign_network_fixture(
                2,
                vec![
                    revoked,
                    manager.certificate(vec![NodeRole::Scheduler, NodeRole::Replica])?,
                    viewer.certificate(vec![NodeRole::Replica])?,
                ],
            )?,
        )?;
        assert!(
            store
                .authorize_node(&network, &gateway.node_id()?, None)
                .is_err()
        );
        let historical = observation_entries(&store, &network, "chat", &viewer.node_id()?)?;
        assert_eq!(historical[0].trigger, message.hash);
        job.target_node = target.clone();
        assert_eq!(
            observed_job_value(&store, &historical[0], &job)?["message_id"],
            "human"
        );
        assert!(
            store
                .scheduler_status(&network, "chat", &message.hash, &target)?
                .is_none()
        );
        assert!(store.scheduler_outbox_all(&network)?.is_empty());
        assert!(store.scheduler_jobs(&network, "chat", &target)?.is_empty());
        register_observation(&mut store, &network, "chat", 10)?;
        assert_eq!(
            store.local_value(&network, &watch_key("chat"))?.unwrap()["expires_at_ms"],
            10 + WATCH_LEASE_MS
        );
        assert!(store.scheduler_outbox_all(&network)?.is_empty());
        Ok(())
    }
}
