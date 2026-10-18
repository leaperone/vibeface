use super::*;
use crate::scheduling::{
    AdmissionReceipt, JobOutcome, JobState, OutboxEntry, SCHEDULING_VERSION, SchedulerJob,
};
use futures_util::{FutureExt, StreamExt};
use vibeface_secure::authorization::Action;

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
                task_id,
                target_node,
                status,
                ..
            } if event.author == *target_node
                && matches!(status.as_str(), "succeeded" | "failed" | "rejected") =>
            {
                let target = if let Some(task) = task_id {
                    store
                        .historical_followup_policy(network, scope, task)
                        .map(|policy| policy.target_node)
                } else {
                    let trigger = crypto::digest(&serde_json::to_vec(&event)?);
                    store
                        .execution_result_policy(network, scope, &trigger)
                        .map(|policy| policy.target_node)
                };
                let Ok(target) = target else { continue };
                target
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
            space_id: network.into(),
            resource_id: event.resource_id.clone(),
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
            && receipt.space_id == entry.space_id
            && receipt.resource_id == entry.resource_id
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
            && job.receipt(&entry.space_id) == receipt,
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
        &json!({"receipt":job.receipt(&entry.space_id),"job":job}),
    )?;
    let envelope = store
        .event(&entry.space_id, &job.trigger)?
        .context("observed job trigger missing")?;
    let event = store.verify_event(&entry.space_id, &envelope)?;
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
            task_id,
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
            let target = if let Some(task) = task_id {
                store
                    .historical_followup_policy(&entry.space_id, &entry.conversation_id, task)?
                    .target_node
            } else {
                store
                    .validate_followup_result(
                        &entry.space_id,
                        &entry.conversation_id,
                        &job.trigger,
                        &entry.target_node,
                    )?
                    .target_node
            };
            ensure!(
                target == entry.target_node,
                "observed job belongs to another scheduler"
            );
        }
        _ => bail!("observed job has no verified scheduling identity"),
    }
    let mut value = job_value(job)?;
    value["message_id"] = json!(origin_message_id(store, &entry.space_id, &job.trigger)?);
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
                        &entry.space_id,
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
        let session = store.local_value(&entry.space_id, &session_key)?;
        if session
            .as_ref()
            .and_then(|v| v["status_revision"].as_u64())
            .is_some_and(|current| current > baseline)
        {
            return Ok(());
        }
        let mut changed = false;
        if let Some(previous) = store.local_value(&entry.space_id, &key)? {
            let mut value = previous.clone();
            value["freshness"] = json!("unknown");
            value["observation_error"] = json!(format!("{error:#}"));
            if previous != value {
                store.local_set(&entry.space_id, &key, &value)?;
                changed = true;
            }
        }
        if let Some(mut session) = session
            && session["freshness"] != "unknown"
        {
            session["freshness"] = json!("unknown");
            store.local_set(&entry.space_id, &session_key, &session)?;
            changed = true;
        }
        if changed {
            drop(store);
            self.publish_changed(&entry.space_id, &entry.conversation_id, &entry.trigger);
        }
        Ok(())
    }

    async fn schedule_delivery_tick(self: &Arc<Self>) -> Result<()> {
        let local = self.identity.node_id()?;
        let networks = self.store.lock().await.networks()?;
        for network in networks {
            if self.config.roles.contains(&NodeRole::Scheduler) {
                let known = self
                    .store
                    .lock()
                    .await
                    .pending_scheduler_results(&network)?;
                for result in known {
                    let current = self
                        .store
                        .lock()
                        .await
                        .scheduler_job(
                            &network,
                            &result.job.conversation_id,
                            &result.job.trigger,
                            &local,
                        )?
                        .context("known result job missing")?;
                    if matches!(
                        current.state,
                        JobState::Running { .. } | JobState::Unknown { .. }
                    ) && !self.store.lock().await.scheduler_publication_pending(
                        &network,
                        &current.conversation_id,
                        &local,
                        &current.trigger,
                    )? {
                        let value = self
                            .store
                            .lock()
                            .await
                            .local_value(&network, &format!("agent-session:{}", current.trigger))?;
                        if let Some(value) = value {
                            let session: AgentSession = serde_json::from_value(value)?;
                            if let Err(error) = self
                                .publish_scheduler_result(
                                    &session,
                                    &current,
                                    result.output,
                                    result.blobs,
                                )
                                .await
                            {
                                tracing::debug!(%error,"known manager result awaits current publication authority");
                            }
                        }
                    }
                }
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
                    let job = self
                        .store
                        .lock()
                        .await
                        .scheduler_job(&network, &scope, &trigger, &local)?
                        .context("abandoned job missing")?;
                    self.store.lock().await.finish_scheduler_locally(&network,&job,
                        JobOutcome::Unknown("management consumer ended before a durable terminal outcome; reconcile the original attempt".into()))?;
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
                        let previous = match store.local_value(&entry.space_id,&key) {
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
                        if let Ok(message_id) = origin_message_id(&store, &entry.space_id, &entry.trigger) {
                            value["kind"] = json!(if message_id.is_some() { "human" } else { "autonomous" });
                            value["message_id"] = json!(message_id);
                        }
                        if previous.as_ref() != Some(&value) {
                            if let Err(cache_error) = store.local_set(&entry.space_id, &key, &value) {
                                tracing::warn!(%cache_error,"cannot save scheduling observation");
                                return;
                            }
                            drop(store);
                            self.publish_changed(&entry.space_id,&entry.conversation_id,&entry.trigger);
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
        let mut deferred = BTreeSet::new();
        loop {
            let selection = self.store.lock().await.next_scheduler_candidate(
                network,
                scope,
                &local,
                crate::scheduler::boot_id(),
                &deferred,
            )?;
            for item in selection.deferred {
                tracing::warn!(trigger = %item.trigger, error = ?item.error, "scheduler eligibility is deferred; queued trigger retained");
                deferred.insert(item.trigger);
            }
            let Some(job) = selection.job else {
                return Ok(());
            };
            let trigger = job.trigger;
            let audience = self.audience()?;
            let session = self.store.lock().await.agent_grant(
                network,
                scope,
                &audience.service,
                &vibeface_secure::authorization::SessionRef {
                    target: audience.device,
                    session_id: trigger.clone(),
                },
                &trigger,
                now_ms() as u64,
            );
            let session = match session {
                Ok(session) => session,
                Err(error) => {
                    tracing::warn!(%trigger, error = ?error, "scheduler service delegation is deferred; queued trigger retained");
                    deferred.insert(trigger);
                    continue;
                }
            };
            let result =
                crate::scheduler::run(self.clone(), session, scope.into(), trigger.clone()).await?;
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
        access.require_body(input)?;
        ensure!(
            access.account_actor(),
            "agents cannot recursively invoke managers"
        );
        let network = &access.network_id;
        let scope = id(input, "conversation_id")?;
        self.require_scope(access, &scope).await?;
        let target = field(input, "scheduler_node")?.to_owned();
        let local = self.identity.node_id()?;
        ensure!(
            input["report_results"].is_boolean(),
            "signed report_results is required"
        );
        if let Some(entry) = input.get("entry_node") {
            ensure!(
                entry.as_str() == Some(local.as_str()),
                "message belongs to its original entry"
            );
        }
        let (event, receipt) = {
            let mut store = self.store.lock().await;
            let _verification = access.verification.enter();
            store.accept_message_authorized(
                &self.identity,
                &access.authorization,
                &access.freshness,
                &access.transport,
                &access.body,
                now_ms() as u64,
            )?
        };
        self.publish_changed(network, &scope, &event.hash);
        self.spawn_scheduler(network.clone(), scope, event.hash.clone())?;
        Ok(
            json!({"accepted":true,"queued":target==local,"state":receipt.state,"trigger":event.hash,"message_id":input["message_id"],"scheduler_node":target,"receipt":receipt}),
        )
    }

    pub(super) async fn receive_schedule(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        access.require_body(input)?;
        self.validate_subscription(access).await?;
        let network = &access.network_id;
        let scope = id(input, "conversation_id")?;
        self.require_scope(access, &scope).await?;
        let trigger = field(input, "trigger")?;
        let local = self.identity.node_id()?;
        ensure!(
            input["version"].as_u64() == Some(u64::from(SCHEDULING_VERSION))
                && input["target_node"].as_str() == Some(local.as_str()),
            "wrong scheduling version or target"
        );
        ensure!(
            self.config.manager.is_some(),
            "this Node has no configured management SDK"
        );
        let wanted = proof_heads(input, trigger)?;
        if !self
            .store
            .lock()
            .await
            .missing_dependencies(network, &wanted)?
            .is_complete()
        {
            let (peer, audience) = {
                let store = self.store.lock().await;
                let certificate = store.authorization_state().verify_transport_device(
                    &access.authorization.signed_invocation().payload.presenter,
                    now_ms() as u64,
                )?;
                let key = &certificate.payload.public_key;
                (
                    crypto::fingerprint(key.as_str())?,
                    Audience {
                        service: ServiceId::from_key(key),
                        device: DeviceId::from_key(key),
                    },
                )
            };
            self.fetch_resource_closure(
                network,
                &access.authorization.resource().resource,
                &audience,
                &peer,
                &wanted,
            )
            .await?;
        }
        self.store
            .lock()
            .await
            .enqueue_trigger(network, &scope, trigger, &local)?;
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
            "space_id":network,"resource_id":crate::store::conversation_resource_in(&store.connection,network,scope)?,"conversation_id":scope,"target_node":local,"status_revision":revision,
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

    async fn signed_scheduler_request(
        &self,
        space: &str,
        target: &str,
        body: Value,
    ) -> Result<Value> {
        let identity = vibeface_secure::Identity::from_secret_hex(&self.identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let key = PublicKey::of(&identity);
        let service = ServiceId::from_key(&key);
        let invocation = {
            let store = self.store.lock().await;
            let now = now_ms() as u64;
            let snapshot = store.connection.unchecked_transaction()?;
            let current =
                crate::identity::journal::PolicyState::reload_verified(&snapshot, &store.policy)?;
            let target = store.authorize_node(space, target, None)?;
            let target_key = PublicKey::parse(&target.public_key)?;
            let audience = Audience {
                service: ServiceId::from_key(&target_key),
                device: DeviceId::from_key(&target_key),
            };
            let facts = crate::identity::policy::request_facts_from(
                &snapshot,
                current.verified(),
                &body,
                &audience,
            )?;
            ensure!(
                facts.action == Action::ConversationRead,
                "invalid progress read action"
            );
            let space = current
                .verified()
                .space(&SpaceId::parse(space)?)
                .context("progress read space policy is missing")?;
            let mut invocation = None;
            for leaf in &space.grants {
                let Ok(grant) = current
                    .verified()
                    .verify_service_grant(&service, leaf, &facts, now)
                else {
                    continue;
                };
                ensure!(
                    grant.device() == &DeviceId::from_key(&key),
                    "progress read grant belongs to another service device"
                );
                let expires_at_ms = now
                    .saturating_add(vibeface_secure::authorization::INVOCATION_MAX_AGE_MS)
                    .min(grant.expires_at_ms());
                ensure!(now < expires_at_ms, "progress read grant expired");
                invocation = Some(Signed::sign(
                    Invocation {
                        version: AUTH_VERSION,
                        actor: Actor::Service(service.clone()),
                        device: grant.device().clone(),
                        initiator: grant.initiator().clone(),
                        initiating_grant: grant.initiating_grant().cloned(),
                        resource: facts.resource.clone(),
                        action: facts.action,
                        audience: facts.audience.clone(),
                        presenter: grant.device().clone(),
                        operation_id: facts.operation_id.clone(),
                        body_hash: facts.body_hash.clone(),
                        grant_leaf: grant.grant_leaf().clone(),
                        expected_policies: grant.heads().to_vec(),
                        context: facts.context.clone(),
                        nonce: Nonce::random()?,
                        not_before_ms: now,
                        expires_at_ms,
                    },
                    &identity,
                )?);
                break;
            }
            snapshot.commit()?;
            invocation.context(
                "service has no current conversation read grant for this progress source",
            )?
        };
        self.peer_request(
            space,
            target,
            json!({
                "type":body["method"], "authorization_body":body, "invocation":invocation,
            }),
        )
        .await
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
                .local_value(&entry.space_id, &key)?
                .is_some_and(|value| {
                    terminal(value["state"].as_str().unwrap_or(""))
                        && !(value["state"] == "unknown" && value["write_frozen"] != false)
                })
        {
            return Ok(());
        }
        let request = if entry.delivered || !acknowledge {
            json!({"method":"manager_status","space_id":entry.space_id,"resource_id":entry.resource_id,"authorization_id":uuid::Uuid::new_v4().to_string(),"conversation_id":entry.conversation_id,"trigger":entry.trigger})
        } else {
            let basis = self
                .store
                .lock()
                .await
                .heads(&entry.space_id, &entry.conversation_id)?;
            ensure!(basis.len() <= 256, "too many scheduling proof heads");
            json!({"method":"schedule","space_id":entry.space_id,"resource_id":entry.resource_id,"authorization_id":uuid::Uuid::new_v4().to_string(),"version":SCHEDULING_VERSION,"target_node":entry.target_node,
                "conversation_id":entry.conversation_id,"trigger":entry.trigger,"basis_heads":basis})
        };
        let reply = self
            .signed_scheduler_request(&entry.space_id, &entry.target_node, request)
            .await?;
        ensure!(
            reply["version"] == SCHEDULING_VERSION
                && reply["space_id"] == entry.space_id
                && reply["resource_id"] == serde_json::to_value(&entry.resource_id)?
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
                .acknowledge_outbox(&entry.space_id, receipt)?;
        }
        {
            let wanted: Vec<String> = serde_json::from_value(reply["heads"].clone())?;
            ensure!(wanted.len() <= 256, "too many result proof heads");
            if !self
                .store
                .lock()
                .await
                .missing_dependencies(&entry.space_id, &wanted)?
                .is_complete()
            {
                let audience = {
                    let store = self.store.lock().await;
                    let target = store.authorize_node(&entry.space_id, &entry.target_node, None)?;
                    let key = PublicKey::parse(&target.public_key)?;
                    Audience {
                        service: ServiceId::from_key(&key),
                        device: DeviceId::from_key(&key),
                    }
                };
                self.fetch_resource_closure(
                    &entry.space_id,
                    &entry.resource_id,
                    &audience,
                    &entry.target_node,
                    &wanted,
                )
                .await?;
            }
            ensure!(
                self.store
                    .lock()
                    .await
                    .context(&entry.space_id, &entry.conversation_id, &wanted)?
                    .missing
                    .is_complete(),
                "observed scheduling history is incomplete"
            );
        }
        let changed = {
            let mut store = self.store.lock().await;
            store.authorize_node(
                &entry.space_id,
                &entry.target_node,
                Some(NodeRole::Scheduler),
            )?;
            let session_key = remote_session_key(&entry.conversation_id, &entry.target_node);
            let previous = store.local_value(&entry.space_id, &session_key)?;
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
                changed |= store
                    .local_value(&entry.space_id, &key)?
                    .is_none_or(|mut previous| {
                        previous["checked_at_ms"] = value["checked_at_ms"].clone();
                        previous != value
                    });
                store.local_set(&entry.space_id, &key, &value)?;
            }
            store.local_set(&entry.space_id, &session_key, &session)?;
            changed
        };
        if changed {
            self.publish_changed(&entry.space_id, &entry.conversation_id, &entry.trigger);
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
        _verification: &SignatureVerification,
    ) -> Result<bool> {
        let _work = self.maintenance.enter()?;
        if request.target_node != self.identity.node_id()?
            || (request.task_id.is_none() && request.manager_authority.is_none())
            || (request.task_id.is_none()
                && request.action["type"] == "open"
                && self
                    .store
                    .lock()
                    .await
                    .operation(&request.network_id, &request.operation_id)?
                    .is_some_and(|operation| operation.state == OperationState::Succeeded))
        {
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
        let local = self.identity.node_id()?;
        {
            let mut store = self.store.lock().await;
            let policy = store.execution_result_policy(
                &request.network_id,
                &request.conversation_id,
                result_hash,
            )?;
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
            space_id: "personal".into(),
            resource_id: vibeface_secure::authorization::ResourceId::random()?,
            conversation_id: "chat".into(),
            trigger: "result".into(),
            target_node: "original".into(),
            delivered: false,
        };
        let job = SchedulerJob {
            resource_id: entry.resource_id.clone(),
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
            "space_id",
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
        let fixture = crate::identity::test_support::TestNetwork::new()?;
        let mut store = fixture.memory()?;
        fixture.conversation(&mut store, "chat")?;
        let message = fixture.message(&mut store, "chat", "delegate this work")?;
        let space = fixture.space.as_str();
        let target = fixture.node.node_id()?;
        let entries = observation_entries(&store, space, "chat", "another-local-observer")?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].trigger, message.hash);
        assert_eq!(entries[0].target_node, target);
        let mut job = SchedulerJob {
            resource_id: fixture.resource.clone(),
            version: SCHEDULING_VERSION,
            conversation_id: "chat".into(),
            trigger: message.hash.clone(),
            target_node: target.clone(),
            kind: TriggerKind::Human,
            sequence: 1,
            basis_heads: vec![message.hash.clone()],
            message_id: "reply".into(),
            state: JobState::Queued,
        };
        assert_eq!(
            observed_job_value(&store, &entries[0], &job)?["message_id"],
            store.messages(space, "chat")?[0].message_id
        );
        job.target_node = "another-target".into();
        assert!(observed_job_value(&store, &entries[0], &job).is_err());
        assert!(store.scheduler_outbox_all(space)?.is_empty());
        assert!(store.scheduler_jobs(space, "chat", &target)?.is_empty());
        register_observation(&mut store, space, "chat", 10)?;
        assert_eq!(
            store.local_value(space, &watch_key("chat"))?.unwrap()["expires_at_ms"],
            10 + WATCH_LEASE_MS
        );
        assert!(store.scheduler_outbox_all(space)?.is_empty());
        Ok(())
    }
}
