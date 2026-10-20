use super::*;
use crate::work::{DispatchState, WorkState};

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
        let network = &access.network_id;
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
        let committed = store.committed_offset(network, &blob.hash)?;
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
        store.commit_offset(network, &blob.hash, Some(offset))?;
        if offset < blob.size {
            return Ok(json!({"offset":offset,"complete":false}));
        }
        drop(file);
        // ponytail: hashes the whole file under the store lock once per attachment (<= 64 MiB).
        if crate::crypto::digest(&std::fs::read(&part)?) != blob.hash {
            let _ = std::fs::remove_file(&part);
            store.commit_offset(network, &blob.hash, None)?;
            bail!("attachment failed its hash check; send it again");
        }
        std::fs::create_dir_all(target.parent().context("blob directory")?)?;
        std::fs::rename(&part, &target)?;
        store.commit_offset(network, &blob.hash, None)?;
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
        let mut pending_checks = checks
            .iter()
            .map(|check| {
                let mut value = json!(check);
                value["kind"] = json!("manager_process");
                value
            })
            .collect::<Vec<_>>();
        let records = store
            .work_records(network)?
            .into_iter()
            .filter(|record| record.spec.conversation == scope)
            .map(|record| (record.spec.work_id.clone(), record))
            .collect::<BTreeMap<_, _>>();
        let stops = store
            .stop_requests(network)?
            .into_iter()
            .map(|stop| (stop.work_id.clone(), stop))
            .collect::<BTreeMap<_, _>>();
        let mut works = Vec::new();
        let unverified: BTreeSet<_> = store
            .works(network, scope)?
            .into_iter()
            .filter(|work| work.outcome == Some(WorkOutcome::Unverified))
            .map(|work| crate::scheduling::task_trigger(&work.work_id))
            .collect();
        // One card per work: an agent task's coordination check is answered by its work card.
        pending_checks
            .retain(|check| !unverified.contains(check["trigger"].as_str().unwrap_or_default()));
        for work in store.works(network, scope)? {
            let mut value = json!(work);
            let record = records.get(&work.work_id);
            value["state"] = match record {
                // An agent task runs as its device's coordination turn, without Codex phases.
                _ if work.executor == Executor::Agent && work.outcome.is_none() => json!("running"),
                Some(record) => json!(record.state.name()),
                // Another device runs it: its progress events say where it is.
                // `Started` is written when the native turn starts, so it already means running.
                None => json!(match (work.outcome, work.phase) {
                    (Some(_), _) => "finished",
                    (None, Some(WorkPhase::Interrupting)) => "interrupting",
                    (None, Some(_)) => "running",
                    (None, None) => "accepted",
                }),
            };
            if work.outcome == Some(WorkOutcome::Unverified) {
                value["state"] = json!("unverified");
                let (detail, probe) = match record.map(|r| &r.state) {
                    Some(WorkState::Unverified {
                        evidence, probe, ..
                    }) => (evidence.clone(), *probe),
                    _ => (work.failure_reason.clone().unwrap_or_default(), false),
                };
                pending_checks.push(
                    json!({"check_id":format!("work:{}",work.work_id),"kind":"work",
                    "conversation_id":scope,"work_id":work.work_id,"target_node":work.target_node,
                    "detail":detail,"probe":probe}),
                );
            }
            if let Some(stop) = stops.get(&work.work_id) {
                value["stop"] =
                    json!({"waiting_for_device":!stop.timed_out,"timed_out":stop.timed_out});
            }
            works.push(value);
        }
        // Dispatches another device has not taken yet show as work waiting for it.
        for dispatch in store.dispatches(network)? {
            let spec = &dispatch.spec;
            if spec.conversation != scope
                || works.iter().any(|w| w["work_id"] == spec.work_id.as_str())
            {
                continue;
            }
            let (state, reason) = match &dispatch.state {
                DispatchState::Pending => ("waiting_for_device", None),
                DispatchState::Accepted => ("accepted", None),
                DispatchState::Rejected { reason } => ("rejected", Some(reason.clone())),
            };
            works.push(json!({"work_id":spec.work_id,"conversation_id":scope,"target_node":spec.target,
                "trigger":spec.trigger,"dispatcher":spec.dispatcher,"executor":spec.executor,"cwd":spec.cwd,
                "prompt":spec.prompt,"phase":null,"summary":"","thread_id":null,"turn_id":null,"outcome":null,
                "result_text":"","failure_reason":reason,"state":state}));
        }
        let conversation = store.conversation(network, scope)?;
        let bot = store.bot_for_conversation(network, scope)?;
        let mut messages = Vec::new();
        for message in store.messages(network, scope)? {
            let mut value = json!(message);
            let attachments = store.event_blobs(network, &message.event_hash)?;
            if !attachments.is_empty() {
                value["blobs"] = json!(attachments);
            }
            messages.push(value);
        }
        let autonomy = store.autonomy(network, scope)?;
        drop(store);
        let settings = self.settings_snapshot();
        let manager = settings.manager.as_ref();
        Ok(json!({
            "conversation":conversation,
            "bot":bot,
            "messages":messages,
            "works":works,
            "autonomy":autonomy,
            "pending_checks":pending_checks,
            "local_node":self.identity.device_id()?,
            "scheduler":{"jobs":jobs,"provider":manager.map(|m|&m.provider),"model":manager.map(|m|&m.model),
                "capabilities":manager.and_then(|m| match m.provider.as_str() {
                    "codex_sdk"=>Some(crate::manager::ManagerKind::CodexSdk.capabilities()),
                    "cursor_sdk"=>Some(crate::manager::ManagerKind::CursorSdk.capabilities()),
                    _=>None})},
        }))
    }
}
