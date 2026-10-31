use super::*;
use crate::{
    scheduling::{PendingCheck, TriggerKind},
    work::DispatchState,
};

const ARTIFACT_PAGE: usize = 65_536;
/// Progress sources one conversation view follows, within the observer's own limit.
const MAX_OBSERVED: usize = 8;
/// Raw bytes per upload request, well inside the client channel's message limit.
const UPLOAD_CHUNK: usize = 1024 * 1024;

/// Raw pages for downloads; base64 of 512 KiB stays under one forwarded frame.
const BINARY_PAGE: usize = 512 * 1024;

fn artifact_page(hash: &str, bytes: &[u8], offset: u64, limit: u64) -> Result<Value> {
    ensure!(
        (4..=ARTIFACT_PAGE as u64).contains(&limit),
        "artifact limit must be between 4 and 65536 bytes"
    );
    let text = std::str::from_utf8(bytes).context("artifact is not UTF-8 text")?;
    let start = usize::try_from(offset).context("artifact offset is out of range")?;
    ensure!(
        start <= bytes.len() && text.is_char_boundary(start),
        "artifact offset is not a valid UTF-8 boundary"
    );
    let mut end = start.saturating_add(limit as usize).min(bytes.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let next = (end < bytes.len()).then_some(end);
    Ok(
        json!({"blob_hash":hash,"text":&text[start..end],"offset":offset,"next_offset":next,"size":bytes.len()}),
    )
}

impl Runtime {
    pub(super) async fn read_artifact(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "artifact access requires the user");
        let scope = field(input, "conversation_id")?;
        self.require_scope(access, scope).await?;
        let hash = field(input, "blob_hash")?;
        if input["binary"] == true {
            use base64::Engine as _;
            let offset = input["offset"].as_u64().unwrap_or(0);
            let (size, bytes) = self.store.lock().await.read_blob_range(
                &access.network_id,
                scope,
                hash,
                offset,
                BINARY_PAGE,
            )?;
            let start = offset.min(size);
            let end = start + bytes.len() as u64;
            return Ok(json!({"blob_hash":hash,"offset":start,"size":size,
                "next_offset":(end < size).then_some(end),
                "data":base64::engine::general_purpose::STANDARD.encode(&bytes)}));
        }
        let (_, bytes) = self
            .store
            .lock()
            .await
            .read_blob(&access.network_id, scope, hash)?;
        artifact_page(
            hash,
            &bytes,
            input["offset"].as_u64().unwrap_or(0),
            input["limit"].as_u64().unwrap_or(ARTIFACT_PAGE as u64),
        )
    }

    /// One chunk of an attachment the user is sending. Bytes synced to disk survive a restart;
    /// the reply's offset says where the client continues.
    pub(super) async fn upload_attachment(&self, access: &Access, input: &Value) -> Result<Value> {
        use base64::Engine as _;
        use std::io::{Seek as _, Write as _};
        ensure!(access.human, "only the user sends attachments");
        let scope = field(input, "conversation_id")?;
        self.require_scope(access, scope).await?;
        let blob = BlobRef {
            hash: field(input, "hash")?.into(),
            size: input["size"].as_u64().context("size is required")?,
        };
        ensure!(
            blob.size > 0 && blob.size as usize <= MAX_BLOB_BYTES,
            "attachment must be 1 byte to 64 MiB"
        );
        let mut store = self.store.lock().await;
        if store.has_blob(&blob)? {
            return Ok(json!({"offset":blob.size,"complete":true}));
        }
        let (part, target) = (
            store.partial_blob_file(&blob.hash)?,
            store.blob_file(&blob.hash)?,
        );
        let committed = store.committed_offset(&blob.hash)?;
        let offset = input["offset"].as_u64().context("offset is required")?;
        if offset != committed {
            return Ok(json!({"offset":committed,"complete":false}));
        }
        // A 1 MiB chunk is longer than `field` allows for ordinary text.
        let data = base64::engine::general_purpose::STANDARD
            .decode(input["data"].as_str().context("data is required")?)
            .context("attachment data must be base64")?;
        ensure!(
            data.len() <= UPLOAD_CHUNK && committed + data.len() as u64 <= blob.size,
            "attachment chunk does not fit"
        );
        let dir = part.parent().context("partial directory")?;
        std::fs::create_dir_all(dir)?;
        if !part.exists() {
            super::peers::staging_room(dir, blob.size)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&part)?;
        file.set_len(committed)?;
        file.seek(std::io::SeekFrom::Start(committed))?;
        file.write_all(&data)?;
        file.sync_data()?;
        let offset = committed + data.len() as u64;
        store.commit_offset(&blob.hash, Some(offset))?;
        if offset < blob.size {
            return Ok(json!({"offset":offset,"complete":false}));
        }
        drop(file);
        // ponytail: hashes the whole file under the store lock once per attachment (<= 64 MiB).
        if crate::crypto::digest(&std::fs::read(&part)?) != blob.hash {
            let _ = std::fs::remove_file(&part);
            store.commit_offset(&blob.hash, None)?;
            bail!("attachment failed its hash check; send it again");
        }
        std::fs::create_dir_all(target.parent().context("blob directory")?)?;
        std::fs::rename(&part, &target)?;
        store.commit_offset(&blob.hash, None)?;
        Ok(json!({"offset":offset,"complete":true}))
    }

    pub(super) async fn conversation_snapshot(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        let scope = field(input, "conversation_id")?;
        self.require_scope(access, scope).await?;
        let network = &access.network_id;
        let (mut jobs, checks) = self.scheduler_snapshot_jobs(network, scope).await?;
        let owner = self.merge_owner_jobs(network, scope, &mut jobs).await?;
        let local = self.identity.device_id()?;
        let store = self.store.lock().await;
        let works = store.works(network, scope)?;
        let task_jobs = task_observations(self, &store, network, scope, &checks)?;
        let mut work_values = works
            .iter()
            .map(|work| work_view(work, &task_jobs))
            .collect::<Result<Vec<_>>>()?;
        let checks = without_work_checks(checks, &works);
        let deliveries = deliveries(&store, network, scope)?;
        let conversation = store.conversation(network, scope)?;
        // Devices whose live progress this conversation shows: its owner and those running its open works.
        let mut targets = Vec::new();
        let sources = conversation.iter().filter_map(|c| c.owner.as_ref()).chain(
            works
                .iter()
                .filter(|work| work.outcome.is_none())
                .map(|work| &work.target_node),
        );
        for source in sources {
            if source != &local
                && !targets.contains(source)
                && targets.len() < MAX_OBSERVED
                && store.authorize_node(network, source).is_ok()
            {
                targets.push(source.clone());
            }
        }
        let bot = store.bot_for_conversation(network, scope)?;
        let messages = store.messages(network, scope)?;
        let changes = store.bot_changes(network, scope)?;
        let overwritten = store.overwritten(network, scope)?;
        let sent = store.sent_bot_messages(network, scope)?;
        let questions = store.conversation_questions(network, scope)?;
        let sessions = store.attributed_sessions(network, scope)?;
        let now = now_ms();
        let enabled = bot.as_ref().and_then(|bot| bot.skills.as_ref());
        let skills: Vec<_> = store
            .skills(network)?
            .into_iter()
            .filter(|entry| enabled.is_none_or(|set| set.contains(&entry.skill_id)))
            .map(|entry| json!({"skill_id":entry.skill_id,"name":entry.skill.name,"description":entry.skill.description}))
            .collect();
        let schedules: Vec<_> = store
            .schedules(network, scope)?
            .into_iter()
            .filter(|entry| crate::scheduling::ahead(&entry.schedule, now))
            .map(|entry| {
                let mut value = json!(entry.schedule);
                value["schedule_id"] = json!(entry.schedule_id);
                value["next_run_at_ms"] = json!(crate::scheduling::next_run(&entry.schedule, now));
                value
            })
            .collect();
        let memory = memory_view(&store, network, scope)?;
        let memory_staged_count = store.scheduler_memory_staged_count(scope)?;
        let memory_conflicts = store.scheduler_memory_conflicts(scope)?;
        let unanswered = store.unanswered_messages(network, &local, scope)?;
        drop(store);
        self.refresh_task_contexts(network, &mut work_values)
            .await?;
        let local_context = self
            .scheduler_context_observation(
                network,
                &crate::scheduling::SessionKey {
                    conversation_id: scope.into(),
                    scope: crate::scheduling::SessionScope::Main,
                },
            )
            .await?;
        let (remote, metadata) = match owner {
            Some((remote, metadata)) if metadata.is_object() => (remote, metadata),
            Some((remote, _)) => (remote, Value::Null),
            None => (Value::Null, self.scheduler_manager_metadata()),
        };
        let context = if remote.is_null() {
            json!(local_context)
        } else {
            remote["context"].clone()
        };
        let (memory_staged_count, memory_conflicts) = if remote.is_null() {
            (json!(memory_staged_count), json!(memory_conflicts))
        } else {
            (
                remote["memory_staged_count"].clone(),
                remote["memory_conflicts"].clone(),
            )
        };
        Ok(json!({
            "conversation":conversation,
            "bot":bot,
            "messages":messages,
            "works":work_values,
            "deliveries":deliveries,
            "changes":changes,
            "overwritten":overwritten,
            "sent":sent,
            "questions":questions,
            "sessions":sessions,
            "bot_settings":{"skills":skills,"schedules":schedules,"memory":memory,
                "memory_staged_count":memory_staged_count,"memory_conflicts":memory_conflicts},
            "checks":checks,
            "local_node":local,
            "scheduler":{"jobs":jobs,"unanswered":unanswered,"provider":metadata["provider"],"model":metadata["model"],
                "capabilities":metadata["capabilities"],"remote":remote,"context":context},
            "observation_targets":targets,
        }))
    }

    /// The records one change touched, in the shapes `conversation` returns, so a client merges
    /// them without a reload: the changed message or work, and the conversation's small lists
    /// whole. `None` when the client should re-read the conversation instead.
    pub(crate) async fn changed_records(
        &self,
        network: &str,
        client: &str,
        change: &Value,
    ) -> Result<Option<Value>> {
        let scope = field(change, "conversation_id")?;
        let (jobs, checks) = self.scheduler_snapshot_jobs(network, scope).await?;
        let context = self
            .scheduler_context_observation(
                network,
                &crate::scheduling::SessionKey {
                    conversation_id: scope.into(),
                    scope: crate::scheduling::SessionScope::Main,
                },
            )
            .await?;
        let store = self.store.lock().await;
        let mut message_id = None;
        let mut work_id = change["work_id"].as_str().map(str::to_owned);
        if let Some(hash) = change["event_hash"].as_str() {
            let Some(event) = store.event(network, hash)? else {
                return Ok(None);
            };
            let event: Event = serde_json::from_str(&event.canonical_body)?;
            match &event.body {
                EventBody::MessagePosted { message_id: id, .. } => message_id = Some(id.clone()),
                EventBody::QuestionAsked { .. }
                | EventBody::QuestionClosed { .. }
                | EventBody::SessionAttributed { .. }
                | EventBody::NotificationAcked { .. } => {}
                body => match body.work_id() {
                    Some(id) => work_id = Some(id.into()),
                    None => return Ok(None),
                },
            }
        } else if let Some(trigger) = change["trigger"].as_str() {
            message_id = store
                .scheduler_job(scope, trigger)?
                .map(|job| job.message_id);
        } else if work_id.is_none() {
            return Ok(None);
        }
        let message = match message_id {
            Some(id) => store.message(network, scope, &id)?,
            None => None,
        };
        let last_message = match message {
            Some(_) => store.last_messages(network)?.remove(scope),
            None => None,
        };
        let messages = message.map(|message| json!(message));
        let works = store.works(network, scope)?;
        let task_jobs = task_observations(self, &store, network, scope, &checks)?;
        let changed: Vec<_> = works
            .iter()
            .filter(|work| work_id.as_deref() == Some(work.work_id.as_str()))
            .map(|work| work_view(work, &task_jobs))
            .collect::<Result<_>>()?;
        let notifications: Vec<_> = store
            .pending_notifications(network, client)?
            .into_iter()
            .filter(|notification| notification.conversation_id == scope)
            .collect();
        let mut value = json!({
            "type":"records",
            "conversation_id":scope,
            "messages":messages.into_iter().collect::<Vec<_>>(),
            "works":changed,
            "last_message":last_message,
            "busy":store.busy_conversations()?.remove(scope),
            "questions":store.conversation_questions(network, scope)?,
            "sent":store.sent_bot_messages(network, scope)?,
            "open_works":open_works(&store, network, Some(scope))?,
            "notifications":notifications,
            "deliveries":deliveries(&store, network, scope)?,
            "checks":without_work_checks(checks, &works),
            "jobs":jobs,
            "context":context,
            "memory":memory_view(&store, network, scope)?,
            "memory_staged_count":store.scheduler_memory_staged_count(scope)?,
            "memory_conflicts":store.scheduler_memory_conflicts(scope)?,
            "unanswered":store.unanswered_messages(network, &self.identity.device_id()?, scope)?,
        });
        drop(store);
        if let Some(works) = value["works"].as_array_mut() {
            self.refresh_task_contexts(network, works).await?;
        }
        Ok(Some(value))
    }
}

impl Runtime {
    async fn refresh_task_contexts(&self, network: &str, works: &mut [Value]) -> Result<()> {
        for work in works {
            let Some(expected) = work["coordination_job"]["context"]["attempt_id"]
                .as_str()
                .map(str::to_owned)
            else {
                continue;
            };
            let key = crate::scheduling::SessionKey {
                conversation_id: field(work, "conversation_id")?.into(),
                scope: crate::scheduling::SessionScope::Task {
                    work_id: field(work, "work_id")?.into(),
                },
            };
            work["coordination_job"]["context"] = json!(
                self.scheduler_context_observation(network, &key)
                    .await?
                    .filter(|context| context.attempt_id == expected)
            );
        }
        Ok(())
    }
}

fn task_observations(
    runtime: &Runtime,
    store: &Store,
    network: &str,
    scope: &str,
    checks: &[PendingCheck],
) -> Result<BTreeMap<String, Value>> {
    store
        .scheduler_jobs(scope)?
        .into_iter()
        .filter(|job| job.kind == TriggerKind::AgentTask)
        .map(|job| {
            Ok((
                job.reply_to.clone(),
                runtime.scheduler_job_projection(store, network, &job, checks)?,
            ))
        })
        .collect()
}

fn work_view(work: &WorkProjection, jobs: &BTreeMap<String, Value>) -> Result<Value> {
    let mut value = serde_json::to_value(work)?;
    if work.kind == crate::model::WorkKind::Task {
        value["coordination_job"] = jobs.get(&work.work_id).cloned().unwrap_or(Value::Null);
    }
    Ok(value)
}

fn memory_view(store: &Store, network: &str, scope: &str) -> Result<Vec<Value>> {
    Ok(store
        .memory_records(network, scope)?
        .into_iter()
        .map(|record| {
            json!({
                "memory_id":record.entry.memory_id,"text":record.entry.text,
                "metadata":{"epoch":record.epoch,"revision":record.revision,"origin":record.origin,
                    "user_controlled":record.user_controlled,"essential":record.essential,
                    "source_event_hash":record.source_event_hash},
            })
        })
        .collect())
}

/// One card per work: an unverified agent task's coordination check is answered by its work card.
fn without_work_checks(checks: Vec<PendingCheck>, works: &[WorkProjection]) -> Vec<PendingCheck> {
    let unverified: BTreeSet<_> = works
        .iter()
        .filter(|work| work.state == WorkStatus::Unverified)
        .map(|work| crate::scheduling::task_trigger(&work.work_id))
        .collect();
    checks
        .into_iter()
        .filter(|check| !unverified.contains(&check.trigger))
        .collect()
}

/// Requests this device still carries: dispatches the DAG learns of only once the target
/// answers, and stops not yet confirmed.
fn deliveries(store: &Store, network: &str, scope: &str) -> Result<Vec<Value>> {
    let mut deliveries = Vec::new();
    for dispatch in store.undelivered_dispatches(network, scope)? {
        if dispatch.state == DispatchState::Accepted {
            continue;
        }
        let spec = &dispatch.spec;
        deliveries.push(
            json!({"work_id":spec.work_id,"target_node":spec.target,"state":dispatch.state.name(),
            "kind":spec.kind,"title":spec.title,"cwd":spec.cwd,"reason":dispatch.state.reason(),
            "failure":dispatch.state.failure(),"trigger":spec.trigger,"prompt":spec.prompt}),
        );
    }
    for stop in store.stop_requests()? {
        if stop.conversation == scope {
            deliveries.push(json!({"work_id":stop.work_id,"target_node":stop.target,
                "state":if stop.timed_out {"stop_timed_out"} else {"stop_waiting"}}));
        }
    }
    Ok(deliveries)
}

/// Works in `scope`, or every conversation, that want the user, as `open_works` lists them: the
/// DAG's open works and unanswered failures, then this Node's refused dispatches still unanswered.
pub(super) fn open_works(store: &Store, network: &str, scope: Option<&str>) -> Result<Vec<Value>> {
    let mut works = store
        .open_works(network, scope)?
        .into_iter()
        .map(|work| Ok(serde_json::to_value(work)?))
        .collect::<Result<Vec<_>>>()?;
    for dispatch in store.open_dispatches(network, scope)? {
        let spec = &dispatch.spec;
        works.push(
            json!({"work_id":spec.work_id,"conversation_id":spec.conversation,
            "target_node":spec.target,"trigger":spec.trigger,"dispatcher":spec.dispatcher,
            "kind":spec.kind,"cwd":spec.cwd,"title":spec.title,"prompt":spec.prompt,
            "call_ref":spec.call_ref,"state":dispatch.state.name(),"summary":"","outcome":null,
            "result_text":"","failure_reason":dispatch.state.reason(),"failure":dispatch.state.failure()}),
        );
    }
    Ok(works)
}
