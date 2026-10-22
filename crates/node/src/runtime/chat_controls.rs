use super::*;
use crate::{scheduling::PendingCheck, work::DispatchState};

const ARTIFACT_PAGE: usize = 65_536;
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
        let (jobs, checks) = self.scheduler_snapshot_jobs(network, scope).await?;
        let store = self.store.lock().await;
        let works = store.works(network, scope)?;
        let checks = without_work_checks(checks, &works);
        let deliveries = deliveries(&store, scope, &works)?;
        let conversation = store.conversation(network, scope)?;
        let bot = store.bot_for_conversation(network, scope)?;
        let messages = store
            .messages(network, scope)?
            .into_iter()
            .map(|message| message_value(&store, network, message))
            .collect::<Result<Vec<_>>>()?;
        let changes = store.bot_changes(network, scope)?;
        let sent = store.sent_bot_messages(network, scope)?;
        let questions = store.conversation_questions(network, scope)?;
        let sessions = store.attributed_sessions(network, scope)?;
        let now = now_ms();
        let skills: Vec<_> = store
            .skills(network)?
            .into_iter()
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
        let memory = store.memories(network, scope)?;
        drop(store);
        let manager = self.manager();
        let manager = manager.as_ref();
        Ok(json!({
            "conversation":conversation,
            "bot":bot,
            "messages":messages,
            "works":works,
            "deliveries":deliveries,
            "changes":changes,
            "sent":sent,
            "questions":questions,
            "sessions":sessions,
            "bot_settings":{"skills":skills,"schedules":schedules,"memory":memory},
            "checks":checks,
            "local_node":self.identity.device_id()?,
            "scheduler":{"jobs":jobs,"provider":manager.map(|m|&m.provider),"model":manager.map(|m|&m.model),
                "capabilities":manager.map(|m| m.provider.capabilities())},
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
                EventBody::QuestionAsked { .. } | EventBody::NotificationAcked { .. } => {}
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
        let messages = message
            .map(|message| message_value(&store, network, message))
            .transpose()?;
        let works = store.works(network, scope)?;
        let changed: Vec<_> = works
            .iter()
            .filter(|work| work_id.as_deref() == Some(work.work_id.as_str()))
            .collect();
        let notifications: Vec<_> = store
            .pending_notifications(network, client)?
            .into_iter()
            .filter(|notification| notification.conversation_id == scope)
            .collect();
        Ok(Some(json!({
            "type":"records",
            "conversation_id":scope,
            "messages":messages.into_iter().collect::<Vec<_>>(),
            "works":changed,
            "last_message":last_message,
            "questions":store.conversation_questions(network, scope)?,
            "sent":store.sent_bot_messages(network, scope)?,
            "notifications":notifications,
            "deliveries":deliveries(&store, scope, &works)?,
            "checks":without_work_checks(checks, &works),
            "jobs":jobs,
        })))
    }
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

/// Requests this device still carries to another one; the DAG learns of them only once the
/// target answers.
fn deliveries(store: &Store, scope: &str, works: &[WorkProjection]) -> Result<Vec<Value>> {
    let mut deliveries = Vec::new();
    for dispatch in store.dispatches()? {
        let spec = &dispatch.spec;
        let state = match &dispatch.state {
            DispatchState::Accepted => continue,
            DispatchState::Pending => "dispatch_waiting",
            DispatchState::Rejected { .. } => "dispatch_rejected",
        };
        if spec.conversation != scope || works.iter().any(|w| w.work_id == spec.work_id) {
            continue;
        }
        let reason = match &dispatch.state {
            DispatchState::Rejected { reason } => Some(reason),
            _ => None,
        };
        deliveries.push(
            json!({"work_id":spec.work_id,"target_node":spec.target,"state":state,
            "kind":spec.kind,"title":spec.title,"cwd":spec.cwd,"reason":reason}),
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

fn message_value(store: &Store, network: &str, message: Message) -> Result<Value> {
    let attachments = store.event_blobs(network, &message.event_hash)?;
    let mut value = json!(message);
    if !attachments.is_empty() {
        value["blobs"] = json!(attachments);
    }
    Ok(value)
}
