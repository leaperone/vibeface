use super::*;
use crate::{scheduling::TriggerKind, store::scope_projection};

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
    pub(super) async fn work_detail(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "work detail access requires the user");
        let scope = id(input, "conversation_id")?;
        let work_id = id(input, "work_id")?;
        self.require_scope(access, &scope).await?;
        let (work, target, sources) = {
            let network = access.network_id.clone();
            self.store
                .call("chat_controls.work_detail", move |store| {
                    let work = store.work(&network, &work_id)?.context("work not found")?;
                    ensure!(
                        work.conversation_id == scope,
                        "work belongs to another conversation"
                    );
                    let target = store.work_input_target(&network, &scope, &work_id)?;
                    let sources = store.work_source_events(&network, &scope, &work_id)?;
                    Ok((work, target, sources))
                })
                .await?
        };
        let mut value = serde_json::to_value(&work)?;
        value["target"] = json!(target);
        value["source_events"] = json!(sources);
        Ok(json!({"work":value,"coverage":{"source":"shared_history","complete":false}}))
    }

    async fn require_attachment_scope(&self, access: &Access, scope: &str) -> Result<()> {
        if scope != crate::workspace::WORKSPACE_SCOPE {
            return self.require_scope(access, scope).await;
        }
        ensure!(
            access.human && access.conversation_id.is_none(),
            "workspace attachments need the user's personal network access"
        );
        let state = self.membership_state().await?;
        ensure!(
            state.genesis.network_id == access.network_id,
            "wrong personal network"
        );
        let network = access.network_id.clone();
        let local = self.identity.device_id()?;
        self.store
            .call("chat_controls.attachment_scope", move |store| {
                store.authorize_node(&network, &local)?;
                Ok(())
            })
            .await
    }

    pub(super) async fn read_artifact(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "artifact access requires the user");
        let scope = field(input, "conversation_id")?;
        self.require_attachment_scope(access, scope).await?;
        let hash = field(input, "blob_hash")?;
        if input["binary"] == true {
            use base64::Engine as _;
            let offset = input["offset"].as_u64().unwrap_or(0);
            let (size, bytes) = {
                let network_id = access.network_id.to_owned();
                let scope = scope.to_owned();
                let hash = hash.to_owned();
                self.store
                    .read("chat_controls.read_blob_range", move |store| {
                        store.read_blob_range(&network_id, &scope, &hash, offset, BINARY_PAGE)
                    })
                    .await
            }?;
            let start = offset.min(size);
            let end = start + bytes.len() as u64;
            return Ok(json!({"blob_hash":hash,"offset":start,"size":size,
                "next_offset":(end < size).then_some(end),
                "data":base64::engine::general_purpose::STANDARD.encode(&bytes)}));
        }
        let (_, bytes) = {
            let network_id = access.network_id.to_owned();
            let scope = scope.to_owned();
            let hash = hash.to_owned();
            self.store
                .read("chat_controls.read_blob", move |store| {
                    store.read_blob(&network_id, &scope, &hash)
                })
                .await
        }?;
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
        use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};
        ensure!(access.human, "only the user sends attachments");
        let scope = field(input, "conversation_id")?;
        self.require_attachment_scope(access, scope).await?;
        let blob = BlobRef {
            hash: field(input, "hash")?.into(),
            size: input["size"].as_u64().context("size is required")?,
        };
        ensure!(
            blob.size > 0 && blob.size as usize <= MAX_BLOB_BYTES,
            "attachment must be 1 byte to 64 MiB"
        );
        let _upload = self.blob_fetch.lock().await;
        self.require_attachment_scope(access, scope).await?;
        let (part, target, committed) = {
            let hash = blob.hash.clone();
            self.store
                .call("chat_controls.upload_paths", move |store| {
                    Ok((
                        store.partial_blob_file(&hash)?,
                        store.blob_file(&hash)?,
                        store.committed_offset(&hash)?,
                    ))
                })
                .await?
        };
        if tokio::fs::metadata(&target)
            .await
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() == blob.size)
        {
            return Ok(json!({"offset":blob.size,"complete":true}));
        }
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
        tokio::fs::create_dir_all(dir).await?;
        if !tokio::fs::try_exists(&part).await? {
            let dir = dir.to_owned();
            tokio::task::spawn_blocking(move || super::peers::staging_room(&dir, blob.size))
                .await??;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&part)
            .await?;
        file.set_len(committed).await?;
        file.seek(std::io::SeekFrom::Start(committed)).await?;
        file.write_all(&data).await?;
        file.sync_data().await?;
        let offset = committed + data.len() as u64;
        {
            let hash = blob.hash.to_owned();
            let committed_offset = Some(offset);
            self.store
                .call("chat_controls.commit_offset", move |store| {
                    store.commit_offset(&hash, committed_offset)
                })
                .await
        }?;
        if offset < blob.size {
            return Ok(json!({"offset":offset,"complete":false}));
        }
        file.sync_all().await?;
        drop(file);
        let hash_part = part.clone();
        let hash = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            super::triage::tests::pause_attachment_hash(&hash_part);
            std::fs::read(hash_part).map(|bytes| crate::crypto::digest(&bytes))
        })
        .await??;
        if hash != blob.hash {
            let _ = tokio::fs::remove_file(&part).await;
            {
                let hash = blob.hash.to_owned();
                self.store
                    .call("chat_controls.commit_offset", move |store| {
                        store.commit_offset(&hash, None)
                    })
                    .await
            }?;
            bail!("attachment failed its hash check; send it again");
        }
        tokio::fs::create_dir_all(target.parent().context("blob directory")?).await?;
        tokio::fs::rename(&part, &target).await?;
        {
            let hash = blob.hash.to_owned();
            self.store
                .call("chat_controls.commit_offset", move |store| {
                    store.commit_offset(&hash, None)
                })
                .await
        }?;
        Ok(json!({"offset":offset,"complete":true}))
    }

    pub(super) async fn conversation_snapshot(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        let scope = field(input, "conversation_id")?;
        self.read_display(
            access,
            input,
            "chat_controls.conversation_snapshot.authorize",
            |_| Ok(()),
        )
        .await?;
        let network = &access.network_id;
        let projection = self.scheduler_projection()?;
        let local_manager = self.scheduler_manager_metadata();
        let owner = self.scheduler_owner_observation(network, scope).await?;
        let local = self.identity.device_id()?;
        let (
            jobs,
            scope_jobs,
            works,
            task_jobs,
            checks,
            deliveries,
            conversation,
            targets,
            bot,
            messages,
            changes,
            overwritten,
            omitted_sent,
            questions,
            sessions,
            skills,
            schedules,
            now,
            memory,
            memory_sync_conflicts,
            memory_staged_count,
            memory_conflicts,
            unanswered,
            history_starts,
            history_authority,
        ) = {
            let (network, scope, local) = (network.clone(), scope.to_owned(), local.clone());
            self.read_display(
                access,
                input,
                "chat_controls.conversation_snapshot",
                move |store| {
                    let (network, scope) = (network.as_str(), scope.as_str());

                    use vibeface_protocol::history::HistorySection;
                    let history_authority = store.conversation_history_authority(network, scope)?;
                    let mut history_starts = BTreeMap::new();
                    for section in HistorySection::ALL.into_iter().filter(|section| {
                        !matches!(
                            section,
                            HistorySection::OwnerJobs
                                | HistorySection::OwnerScopeJobs
                                | HistorySection::OwnerMemoryConflicts
                        )
                    }) {
                        let name = serde_json::to_value(section)?
                            .as_str()
                            .context("history section name")?
                            .to_owned();
                        history_starts.insert(
                            name,
                            store.conversation_history_start_cursor(network, scope, section)?,
                        );
                    }
                    let works = store.conversation_works_page(network, scope, None, 50)?;
                    let task_jobs = task_observations(&store, scope, &works.items)?;
                    let jobs = projection.history_page(
                        store,
                        network,
                        scope,
                        HistorySection::Jobs,
                        None,
                        50,
                        &local_manager,
                    )?;
                    let scope_jobs = projection.history_page(
                        store,
                        network,
                        scope,
                        HistorySection::ScopeJobs,
                        None,
                        50,
                        &local_manager,
                    )?;
                    let checks = projection.history_page(
                        store,
                        network,
                        scope,
                        HistorySection::Checks,
                        None,
                        50,
                        &local_manager,
                    )?;
                    let deliveries = projection.history_page(
                        store,
                        network,
                        scope,
                        HistorySection::Deliveries,
                        None,
                        50,
                        &local_manager,
                    )?;
                    let conversation = store.conversation(network, scope)?;
                    // Devices whose live progress this conversation shows: its owner and those running its open works.
                    let mut targets = Vec::new();
                    let sources = conversation.iter().filter_map(|c| c.owner.as_ref()).chain(
                        works
                            .items
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
                    let messages = store.conversation_messages_page(
                        network,
                        scope,
                        None,
                        vibeface_protocol::history::HISTORY_DEFAULT_LIMIT,
                    )?;
                    let changes = store.conversation_changes_page(network, scope, None, 50)?;
                    let overwritten =
                        store.conversation_overwritten_page(network, scope, None, 50)?;
                    let omitted_sent = store.sent_bot_message_count(network, scope)?;
                    let questions = store.conversation_questions_page(network, scope, None, 50)?;
                    let sessions = store.conversation_sessions_page(network, scope, None, 50)?;
                    let skills = store.conversation_skills_page(network, scope, None, 50)?;
                    let schedules = store.conversation_schedules_page(network, scope, None, 50)?;
                    let now = now_ms();
                    let memory = store.conversation_memory_page(network, scope, None, 50)?;
                    let memory_sync_conflicts =
                        store.conversation_memory_sync_conflicts_page(network, scope, None, 50)?;
                    let memory_staged_count = store.conversation_memory_staged_count(scope)?;
                    let memory_conflicts =
                        store.conversation_memory_conflicts_page(network, scope, None, 50)?;
                    let unanswered = projection.history_page(
                        store,
                        network,
                        scope,
                        HistorySection::Unanswered,
                        None,
                        50,
                        &local_manager,
                    )?;

                    Ok((
                        jobs,
                        scope_jobs,
                        works,
                        task_jobs,
                        checks,
                        deliveries,
                        conversation,
                        targets,
                        bot,
                        messages,
                        changes,
                        overwritten,
                        omitted_sent,
                        questions,
                        sessions,
                        skills,
                        schedules,
                        now,
                        memory,
                        memory_sync_conflicts,
                        memory_staged_count,
                        memory_conflicts,
                        unanswered,
                        history_starts,
                        history_authority,
                    ))
                },
            )
            .await?
        };
        let work_values = works
            .items
            .iter()
            .map(|work| work_view(work, &task_jobs))
            .collect::<Result<Vec<_>>>()?;
        let skill_values: Vec<_> = skills.items.iter().map(skill_summary).collect();
        let schedule_values: Vec<_> = schedules
            .items
            .iter()
            .filter_map(|entry| schedule_summary(entry, now))
            .collect();
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
        let mut snapshot = json!({
            "conversation":conversation,
            "history_authority":history_authority,
            "bot":bot,
            "messages":&messages.items,
            "history":{"messages":messages.coverage(),"works":works.coverage(),"changes":changes.coverage(),
                "questions":questions.coverage(),"sessions":sessions.coverage(),
                "jobs":jobs.coverage(),"scope_jobs":scope_jobs.coverage(),"checks":checks.coverage(),
                "deliveries":deliveries.coverage(),"unanswered":unanswered.coverage(),
                "skills":skills.coverage(),"schedules":schedules.coverage(),"memory":memory.coverage(),
                "memory_conflicts":memory_conflicts.coverage(),"memory_sync_conflicts":memory_sync_conflicts.coverage(),
                "overwritten":overwritten.coverage()},
            "works":work_values,
            "deliveries":&deliveries.items,
            "changes":&changes.items,
            "overwritten":&overwritten.items,
            "sent":[],
            "sent_coverage":{"scope":"main","omitted_unknown":omitted_sent},
            "questions":&questions.items,
            "sessions":&sessions.items,
            "bot_settings":{"skills":skill_values,"schedules":schedule_values,"memory":&memory.items,
                "memory_staged_count":memory_staged_count,"memory_conflicts":&memory_conflicts.items,
                "memory_sync_conflicts":{"items":&memory_sync_conflicts.items,"has_more":!memory_sync_conflicts.complete}},
            "checks":&checks.items,
            "local_node":local,
            "scheduler":{"jobs":&jobs.items,"scope_jobs":&scope_jobs.items,"unanswered":&unanswered.items,"provider":metadata["provider"],"model":metadata["model"],
                "capabilities":metadata["capabilities"],"remote":remote,"context":context},
            "observation_targets":targets,
        });
        let mut history_starts = history_starts;
        for section in vibeface_protocol::history::HistorySection::ALL {
            let name = serde_json::to_value(section)?
                .as_str()
                .context("history section name")?
                .to_owned();
            if snapshot["history"][&name].is_null() {
                snapshot["history"][&name] = json!({"next_cursor":null,"complete":true});
            }
        }
        for name in ["owner_jobs", "owner_scope_jobs", "owner_memory_conflicts"] {
            if let Some(coverage) = snapshot["scheduler"]["remote"]["history"]
                .get(name)
                .cloned()
            {
                snapshot["history"][name] = coverage;
            }
            if let Some(cursor) = snapshot["scheduler"]["remote"]["history_starts"][name].as_str() {
                history_starts.insert(name.into(), cursor.to_owned());
            }
        }
        if let Some(remote) = snapshot["scheduler"]["remote"].as_object_mut() {
            remote.remove("history_starts");
            remote.remove("history");
        }
        fit_history_snapshot(&mut snapshot, &history_starts)?;
        Ok(snapshot)
    }

    pub(super) async fn conversation_history(
        &self,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        use vibeface_protocol::history::{HistoryRequest, HistorySection};
        let mut parameters = input.clone();
        parameters
            .as_object_mut()
            .context("history request must be an object")?
            .remove("method");
        let request: HistoryRequest = serde_json::from_value(parameters)?;
        if matches!(
            request.section,
            HistorySection::OwnerJobs
                | HistorySection::OwnerScopeJobs
                | HistorySection::OwnerMemoryConflicts
        ) {
            return self.conversation_owner_history(access, input).await;
        }
        let projection = self.scheduler_projection()?;
        let manager = self.scheduler_manager_metadata();
        let network = access.network_id.clone();
        let page = self
            .read_display(
                access,
                input,
                "chat_controls.conversation_history",
                move |store| match request.section {
                    HistorySection::Messages => {
                        Ok(serde_json::to_value(store.conversation_messages_page(
                            &network,
                            &request.conversation_id,
                            request.cursor.as_deref(),
                            request.limit,
                        )?)?)
                    }
                    HistorySection::Works => {
                        Ok(serde_json::to_value(store.conversation_works_page(
                            &network,
                            &request.conversation_id,
                            request.cursor.as_deref(),
                            request.limit,
                        )?)?)
                    }
                    HistorySection::Changes => {
                        Ok(serde_json::to_value(store.conversation_changes_page(
                            &network,
                            &request.conversation_id,
                            request.cursor.as_deref(),
                            request.limit,
                        )?)?)
                    }
                    HistorySection::Questions => {
                        Ok(serde_json::to_value(store.conversation_questions_page(
                            &network,
                            &request.conversation_id,
                            request.cursor.as_deref(),
                            request.limit,
                        )?)?)
                    }
                    HistorySection::Sessions => {
                        Ok(serde_json::to_value(store.conversation_sessions_page(
                            &network,
                            &request.conversation_id,
                            request.cursor.as_deref(),
                            request.limit,
                        )?)?)
                    }
                    HistorySection::Jobs
                    | HistorySection::ScopeJobs
                    | HistorySection::Checks
                    | HistorySection::Deliveries
                    | HistorySection::Unanswered => {
                        Ok(serde_json::to_value(projection.history_page(
                            store,
                            &network,
                            &request.conversation_id,
                            request.section,
                            request.cursor.as_deref(),
                            request.limit,
                            &manager,
                        )?)?)
                    }
                    HistorySection::Skills => {
                        let page = store.conversation_skills_page(
                            &network,
                            &request.conversation_id,
                            request.cursor.as_deref(),
                            request.limit,
                        )?;
                        let items: Vec<_> = page.items.iter().map(skill_summary).collect();
                        let mut value = serde_json::to_value(&page)?;
                        value["items"] = json!(items);
                        Ok(value)
                    }
                    HistorySection::Schedules => {
                        let page = store.conversation_schedules_page(
                            &network,
                            &request.conversation_id,
                            request.cursor.as_deref(),
                            request.limit,
                        )?;
                        let now = now_ms();
                        let items: Vec<_> = page
                            .items
                            .iter()
                            .filter_map(|entry| schedule_summary(entry, now))
                            .collect();
                        let mut value = serde_json::to_value(&page)?;
                        value["items"] = json!(items);
                        Ok(value)
                    }
                    HistorySection::Memory => {
                        Ok(serde_json::to_value(store.conversation_memory_page(
                            &network,
                            &request.conversation_id,
                            request.cursor.as_deref(),
                            request.limit,
                        )?)?)
                    }
                    HistorySection::MemoryConflicts => Ok(serde_json::to_value(
                        store.conversation_memory_conflicts_page(
                            &network,
                            &request.conversation_id,
                            request.cursor.as_deref(),
                            request.limit,
                        )?,
                    )?),
                    HistorySection::MemorySyncConflicts => Ok(serde_json::to_value(
                        store.conversation_memory_sync_conflicts_page(
                            &network,
                            &request.conversation_id,
                            request.cursor.as_deref(),
                            request.limit,
                        )?,
                    )?),
                    HistorySection::Overwritten => {
                        Ok(serde_json::to_value(store.conversation_overwritten_page(
                            &network,
                            &request.conversation_id,
                            request.cursor.as_deref(),
                            request.limit,
                        )?)?)
                    }
                    HistorySection::OwnerJobs
                    | HistorySection::OwnerScopeJobs
                    | HistorySection::OwnerMemoryConflicts => {
                        unreachable!("owner history is routed before opening the local read")
                    }
                },
            )
            .await?;
        ensure!(
            serde_json::to_vec(&page)?.len() < vibeface_protocol::history::HISTORY_RESPONSE_BYTES,
            "history record exceeds the response byte limit"
        );
        Ok(page)
    }

    /// Message/work patches carry only exact records. Other changes request a fresh bounded
    /// snapshot; clients retain loaded history unless its projection authority changed.
    pub(crate) async fn changed_records(
        &self,
        network: &str,
        _client: &str,
        change: &Value,
    ) -> Result<Option<Value>> {
        let scope = field(change, "conversation_id")?.to_owned();
        let network = network.to_owned();
        let event_hash = change["event_hash"].as_str().map(str::to_owned);
        let mut work_id = change["work_id"].as_str().map(str::to_owned);
        self.store.read("chat_controls.changed_records", move |store| {
            let mut message_id = None;
            if let Some(hash) = event_hash {
                let Some(envelope) = store.event(&network, &hash)? else { return Ok(None) };
                let event: Event = serde_json::from_str(&envelope.canonical_body)?;
                ensure!(event.scope_id == scope, "changed record belongs to another conversation");
                match &event.body {
                    EventBody::MessagePosted { message_id: id, .. }
                    | EventBody::InputPosted { message_id: id, .. }
                    | EventBody::BotMessaged { message_id: id, .. } => message_id = Some(id.clone()),
                    body => match body.work_id() {
                        Some(id) => work_id = Some(id.into()),
                        None => return Ok(None),
                    },
                }
            } else if work_id.is_none() {
                return Ok(None);
            }
            let mut messages = Vec::new();
            if let Some(id) = message_id {
                if scope_projection::message_is_main_from(&store.connection, &network, &scope, &id)? {
                    if let Some(message) = store.message(&network, &scope, &id)? { messages.push(message); }
                }
            }
            let mut works = Vec::new();
            if let Some(id) = work_id {
                if let Some(work) = store.work(&network, &id)? {
                    if work.conversation_id == scope && scope_projection::work_is_main_from(
                        &store.connection, &network, &scope, &id,
                    )? { works.push(work); }
                }
            }
            let jobs = task_observations(store, &scope, &works)?;
            let works = works.iter().map(|work| work_view(work, &jobs)).collect::<Result<Vec<_>>>()?;
            let patch = json!({"type":"records","conversation_id":scope,"messages":messages,"works":works,
                "history_authority":store.conversation_history_authority(&network, &scope)?,"refresh_metadata":true});
            if serde_json::to_vec(&patch)?.len() >= vibeface_protocol::history::HISTORY_RESPONSE_BYTES {
                return Ok(None);
            }
            Ok(Some(patch))
        }).await
    }
}

/// Defer whole sections to their real first-page cursor when their combined payload is
/// larger than one response. No record is shortened or made unreachable.
fn fit_history_snapshot(snapshot: &mut Value, starts: &BTreeMap<String, String>) -> Result<()> {
    use vibeface_protocol::history::{HISTORY_RESPONSE_BYTES, HistorySection};
    let mut size = serde_json::to_vec(snapshot)?.len();
    if size < HISTORY_RESPONSE_BYTES {
        return Ok(());
    }
    let mut sections = Vec::new();
    for name in starts.keys() {
        let section: HistorySection = serde_json::from_value(json!(name))?;
        let value = section
            .snapshot_path()
            .iter()
            .fold(&*snapshot, |value, key| &value[*key]);
        if value.is_array() {
            sections.push((serde_json::to_vec(value)?.len(), name, section));
        }
    }
    sections.sort_by(|left, right| right.0.cmp(&left.0));
    for (previous_size, name, section) in sections {
        let coverage_size = serde_json::to_vec(&snapshot["history"][name])?.len();
        let value = section
            .snapshot_path()
            .iter()
            .fold(&mut *snapshot, |value, key| &mut value[*key]);
        *value = json!([]);
        snapshot["history"][name] = json!({"next_cursor":starts[name],"complete":false});
        size = size - previous_size + 2 - coverage_size
            + serde_json::to_vec(&snapshot["history"][name])?.len();
        if section == HistorySection::MemorySyncConflicts {
            let has_more = &mut snapshot["bot_settings"]["memory_sync_conflicts"]["has_more"];
            let previous = serde_json::to_vec(has_more)?.len();
            *has_more = json!(true);
            size = size - previous + 4;
        }
        if size < HISTORY_RESPONSE_BYTES {
            break;
        }
    }
    // Verify the actual final payload once, after accounting for replacements. Repeated
    // whole-snapshot serialization would otherwise grow with the number of deferred sections.
    ensure!(
        serde_json::to_vec(snapshot)?.len() < HISTORY_RESPONSE_BYTES,
        "conversation metadata exceeds the response byte limit"
    );
    Ok(())
}

fn task_observations(
    store: &Store,
    scope: &str,
    works: &[WorkProjection],
) -> Result<BTreeMap<String, Value>> {
    works
        .iter()
        .filter(|work| work.kind == crate::model::WorkKind::Task)
        .map(|work| store.scheduler_job(scope, &crate::scheduling::task_trigger(&work.work_id)))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .filter(|job| job.kind == TriggerKind::AgentTask)
        .map(|job| {
            Ok((
                job.reply_to.clone(),
                json!({
                    "conversation_id":job.conversation_id,"trigger":job.trigger,
                    "scope":job.session_scope(),"state":job.state.name(),
                    "sequence":job.sequence,"reply_message_id":job.message_id,
                }),
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

fn skill_summary(entry: &SkillEntry) -> Value {
    json!({"skill_id":entry.skill_id,"name":entry.skill.name,"description":entry.skill.description})
}

fn schedule_summary(entry: &ScheduleEntry, now: i64) -> Option<Value> {
    if !crate::scheduling::ahead(&entry.schedule, now) {
        return None;
    }
    let mut value = json!(entry.schedule);
    value["schedule_id"] = json!(entry.schedule_id);
    value["next_run_at_ms"] = json!(crate::scheduling::next_run(&entry.schedule, now));
    Some(value)
}

/// Works in `scope`, or every conversation, that want the user, as `open_works` lists them: the
/// DAG's open works and unanswered failures, then this Node's refused dispatches still unanswered.
pub(super) fn open_works(store: &Store, network: &str, scope: Option<&str>) -> Result<Vec<Value>> {
    let mut works = store
        .open_works(network, scope)?
        .into_iter()
        .map(|work| {
            let target = store.work_input_target(network, &work.conversation_id, &work.work_id)?;
            let mut value = serde_json::to_value(work)?;
            if let Some(target) = target {
                value["target"] = json!(target);
            }
            Ok(value)
        })
        .collect::<Result<Vec<_>>>()?;
    for dispatch in store.open_dispatches(network, scope)? {
        let spec = &dispatch.spec;
        let mut value = json!({"work_id":spec.work_id,"conversation_id":spec.conversation,
            "target_node":spec.target,"trigger":spec.trigger,"dispatcher":spec.dispatcher,
            "kind":spec.kind,"cwd":spec.cwd,"title":spec.title,"prompt":spec.prompt,
            "call_ref":spec.call_ref,"state":dispatch.state.name(),"summary":"","outcome":null,
            "result_text":"","failure_reason":dispatch.state.reason(),"failure":dispatch.state.failure()});
        if let Some(target) = store.work_input_target(network, &spec.conversation, &spec.work_id)? {
            value["target"] = json!(target);
        }
        works.push(value);
    }
    Ok(works)
}

#[cfg(test)]
mod attachment_tests {
    use super::*;
    use crate::runtime::{
        test_support::access as test_access,
        triage::tests::{bot, node},
    };
    use crate::workspace::{WORKSPACE_SCOPE, WorkspaceCommand};
    use base64::Engine as _;

    #[tokio::test]
    async fn workspace_channel_upload_and_download_require_unscoped_human_network_access()
    -> Result<()> {
        let (node, admin, directory) = node().await?;
        let result = async {
            bot(&node, &admin, "c", "Channel bot", &[]).await?;
            let network = node.network_id().await?;
            let access = test_access(network.clone(), true, None);
            let bytes = b"workspace channel attachment";
            let attachment = Attachment { hash: crate::crypto::digest(bytes), size: bytes.len() as u64, name: "channel.txt".into() };
            let upload = json!({"conversation_id":WORKSPACE_SCOPE,"hash":attachment.hash,"size":bytes.len(),"offset":0,
                "data":base64::engine::general_purpose::STANDARD.encode(bytes)});
            for restricted in [test_access(network.clone(), false, None), test_access(network.clone(), false, Some("c".into())),
                test_access(network.clone(), true, Some("c".into())), test_access("another-network".into(), true, None)] {
                assert!(node.upload_attachment(&restricted, &upload).await.is_err());
            }
            assert_eq!(node.upload_attachment(&access, &upload).await?, json!({"offset":bytes.len(),"complete":true}));
            let read = json!({"conversation_id":WORKSPACE_SCOPE,"blob_hash":attachment.hash,"binary":true});
            assert!(node.read_artifact(&access, &read).await.is_err(), "upload alone grants no reference");
            for (id, command) in [
                ("create-channel", WorkspaceCommand::CreateChannel { channel_id: "channel".into(), title: "Synthetic".into(),
                    purpose: "File round trip".into(), members: vec!["c".into()], coordinator_bot: "c".into(), coordinator_node: node.identity.device_id()? }),
                ("post-channel", WorkspaceCommand::PostChannelMessage { channel_id: "channel".into(), message_id: "channel-message".into(),
                    text: "Read attached file".into(), attachments: vec![attachment.clone()], recipient_bot: None, task_id: None, reply_to: None }),
            ] {
                let signed = admin.attest(&network, WORKSPACE_SCOPE, EventBody::WorkspaceCommanded { command_id: id.into(), command: command.clone(),
                    attestation: None, cause: None, acting_assignment: None })?;
                let reply = node.workspace_command(&access, &json!({"command_id":id,"command":command,"attestation":signed.attestation()})).await?;
                assert!(reply["event_hash"].as_str().is_some());
            }
            let reply = node.read_artifact(&access, &read).await?;
            assert_eq!(base64::engine::general_purpose::STANDARD.decode(reply["data"].as_str().context("binary page")?)?, bytes);
            let text = node.read_artifact(&access, &json!({"conversation_id":WORKSPACE_SCOPE,"blob_hash":attachment.hash})).await?;
            assert_eq!(text["text"], "workspace channel attachment");
            for restricted in [test_access(network.clone(), false, None), test_access(network.clone(), false, Some("c".into())),
                test_access(network.clone(), true, Some("c".into())), test_access("another-network".into(), true, None)] {
                assert!(node.read_artifact(&restricted, &read).await.is_err());
            }
            assert!(node.read_artifact(&access, &json!({"conversation_id":"c","blob_hash":attachment.hash})).await.is_err());
            assert!(node.read_artifact(&access, &json!({"conversation_id":WORKSPACE_SCOPE,"blob_hash":crate::crypto::digest(b"absent")})).await.is_err());
            Ok::<_, anyhow::Error>(())
        }.await;
        node.shutdown().await;
        drop(node);
        std::fs::remove_dir_all(directory)?;
        result
    }
}

#[cfg(test)]
mod conversation_history_tests {
    use super::*;
    use crate::runtime::{
        test_support::{access as test_access, signed_main_input},
        triage::tests::{bot, node},
    };
    use vibeface_protocol::history::HISTORY_RESPONSE_BYTES;

    #[tokio::test]
    async fn real_rpc_pages_signed_long_history_and_enforces_scope() -> Result<()> {
        let (node, admin, directory) = node().await?;
        let result = async {
            bot(&node, &admin, "history", "History", &[]).await?;
            let network = node.network_id().await?;
            let access = test_access(network.clone(), true, None);
            let bodies = (0..1024).map(|index| signed_main_input(&admin, &network, "history",
                &format!("message-{index}"), &format!("{index}:{}", "完整消息".repeat(1024)), vec![], index + 1))
                .collect::<Result<Vec<_>>>()?;
            let identity = node.identity.clone();
            let stored_network = network.clone();
            let oracle = node.store.call("history_fixture", move |store| {
                for (index, body) in bodies.into_iter().enumerate() {
                    store.append_event(&identity, &stored_network, "history", body, vec![], 10 + index as i64)?;
                }
                store.main_messages(&stored_network, "history")
            }).await?;
            assert!(serde_json::to_vec(&oracle)?.len() > 4 * 1024 * 1024);
            let snapshot = node.rpc(access.clone(), json!({"method":"conversation","conversation_id":"history"})).await?;
            assert!(serde_json::to_vec(&snapshot)?.len() < HISTORY_RESPONSE_BYTES);
            assert!(snapshot["messages"].as_array().context("messages")?.len() < oracle.len());
            let cursor_network = network.clone();
            let start = node.store.read("history_budget_fixture", move |store| {
                store.conversation_history_start_cursor(&cursor_network, "history", vibeface_protocol::history::HistorySection::Messages)
            }).await?;
            let mut aggregate = snapshot.clone();
            aggregate["messages"] = serde_json::to_value(&oracle)?;
            assert!(serde_json::to_vec(&aggregate)?.len() > HISTORY_RESPONSE_BYTES);
            fit_history_snapshot(&mut aggregate, &BTreeMap::from([("messages".into(), start)]))?;
            assert!(serde_json::to_vec(&aggregate)?.len() < HISTORY_RESPONSE_BYTES);
            assert!(aggregate["messages"].as_array().context("deferred messages")?.is_empty());
            let mut all = Vec::new();
            let mut cursor = aggregate["history"]["messages"]["next_cursor"].as_str().map(str::to_owned);
            while let Some(before) = cursor {
                let page = node.rpc(access.clone(), json!({"method":"conversation_history","conversation_id":"history",
                    "section":"messages","cursor":before,"limit":100})).await?;
                assert!(serde_json::to_vec(&page)?.len() < HISTORY_RESPONSE_BYTES);
                all.splice(0..0, page["items"].as_array().context("items")?.clone());
                cursor = page["next_cursor"].as_str().map(str::to_owned);
            }
            assert_eq!(serde_json::to_value(all)?, serde_json::to_value(oracle)?);
            for denied in [test_access("wrong-network".into(), true, None),
                test_access(network.clone(), false, None),
                test_access(network.clone(), true, Some("other".into()))] {
                assert!(node.rpc(denied, json!({"method":"conversation_history","conversation_id":"history","section":"messages"})).await.is_err());
            }
            let delete = admin.attest(&network, "history", EventBody::BotDeleted {attestation:None,cause:None})?;
            let identity = node.identity.clone();
            node.store.call("history_delete_fixture", move |store| {
                store.append_event(&identity, &network, "history", delete, vec![], 2000)?;
                Ok(())
            }).await?;
            let after_delete = node.rpc(access, json!({"method":"conversation_history","conversation_id":"history","section":"messages"})).await?;
            assert!(after_delete["items"].as_array().is_some_and(|items| !items.is_empty()),
                "deleting a Bot preserves its authorized conversation history");
            Ok::<_, anyhow::Error>(())
        }.await;
        node.shutdown().await;
        drop(node);
        std::fs::remove_dir_all(directory)?;
        result
    }
}
