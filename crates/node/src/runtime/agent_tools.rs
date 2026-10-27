use super::*;
use crate::{
    scheduler::dispatch_row,
    store::{
        bot_settings::{ChangeRequest, bot_message_id, change_id},
        bots::created_bot_id,
    },
    tools::{Effect, Tool},
    work::WorkSpec,
};

impl Runtime {
    /// The one entry for coordination tools: the Cursor SDK callback and `vibeface tool` both land here.
    pub async fn tool(self: &Arc<Self>, access: Access, name: &str, args: Value) -> Result<Value> {
        let conversation_id = access.conversation_id.clone();
        let asked_work = args["work_id"].as_str().map(str::to_owned);
        let result = self.run_tool(access, name, args).await;
        let conversation_id = conversation_id.as_deref();
        match &result {
            Ok(value) => {
                let work_id = value["work_id"].as_str().or(asked_work.as_deref());
                tracing::info!(
                    event = "agent.tool",
                    conversation_id,
                    work_id,
                    tool = name,
                    "agent called {name}"
                )
            }
            Err(error) => {
                tracing::warn!(event = "agent.tool_failed", conversation_id, work_id = asked_work.as_deref(), tool = name, error = %format!("{error:#}"), "agent call {name} failed")
            }
        }
        result
    }

    async fn run_tool(self: &Arc<Self>, access: Access, name: &str, args: Value) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        ensure!(!access.human, "tool endpoint requires a scoped Agent token");
        let scope = access
            .conversation_id
            .clone()
            .context("missing tool scope")?;
        self.require_scope(&access, &scope).await?;
        let tool = Tool::parse(name).context("unknown or unavailable coordination tool")?;
        ensure!(
            tool.effect() == Effect::Read || access.manager.is_some(),
            "work changes require a live agent invocation"
        );
        if let Some(manager) = &access.manager
            && tool.effect() != Effect::Read
        {
            let kind = self
                .store
                .lock()
                .await
                .scheduler_job(&scope, &manager.trigger)?
                .context("this turn is not running here")?
                .kind;
            ensure!(
                tool.effect().offered(kind),
                "{name} is not offered in a {kind:?} turn"
            );
        }
        let _lease =
            tokio::time::timeout(Duration::from_secs(20), self.manager_tool_lease(&access))
                .await
                .context("agent tool admission timed out")??;
        let network = access.network_id.as_str();
        match tool {
            Tool::Nodes => {
                self.rpc_inner(access.clone(), json!({"method":"nodes"}))
                    .await
            }
            Tool::Context => self.agent_context(network, &scope, &args).await,
            // Dispatches the DAG does not name yet are listed too, so none is sent twice.
            Tool::WorkStatus => {
                let store = self.store.lock().await;
                let works = store.works(network, &scope)?;
                let dispatches = store.undelivered_dispatches(network, &scope)?;
                drop(store);
                Ok(match args["work_id"].as_str() {
                    Some(id) => match works.into_iter().find(|work| work.work_id == id) {
                        Some(work) => json!(work),
                        None => dispatch_row(
                            dispatches
                                .iter()
                                .find(|dispatch| dispatch.spec.work_id == id)
                                .context("work not found in this conversation")?,
                        ),
                    },
                    None => {
                        let mut rows: Vec<Value> = works.iter().map(|work| json!(work)).collect();
                        rows.extend(dispatches.iter().map(dispatch_row));
                        json!({ "works": rows })
                    }
                })
            }
            Tool::CallLookup => {
                let manager = access
                    .manager
                    .as_ref()
                    .context("call lookup requires an agent invocation")?;
                let call_ref = field(&args, "call_ref")?;
                let id = WorkSpec::id(&scope, &manager.trigger, call_ref);
                let store = self.store.lock().await;
                if let Some(work) = store
                    .works(network, &scope)?
                    .into_iter()
                    .find(|work| work.work_id == id)
                {
                    return Ok(json!(work));
                }
                if let Some(dispatch) = store.dispatch(&id)? {
                    return Ok(dispatch_row(&dispatch));
                }
                if let Some(change) =
                    store.bot_change(network, &change_id(&scope, &manager.trigger, call_ref))?
                {
                    return Ok(json!(change));
                }
                if let Some(message) = store
                    .bot_message(network, &bot_message_id(&scope, &manager.trigger, call_ref))?
                {
                    return Ok(json!(message));
                }
                let (bot_id, _) = created_bot_id(&scope, &manager.trigger, call_ref);
                let bot = store
                    .bots(network)?
                    .into_iter()
                    .find(|bot| bot.bot_id == bot_id)
                    .context(
                        "nothing was dispatched, changed, sent or created with this call_ref",
                    )?;
                Ok(json!(bot))
            }
            Tool::Recall => {
                let query = args["query"].as_str().map(str::to_lowercase);
                let notes: Vec<_> = self
                    .store
                    .lock()
                    .await
                    .memories(network, &scope)?
                    .into_iter()
                    .filter(|note| {
                        query
                            .as_ref()
                            .is_none_or(|query| note.text.to_lowercase().contains(query))
                    })
                    .collect();
                Ok(json!({ "memory": notes }))
            }
            Tool::FetchUrl => crate::fetch::fetch_url(field(&args, "url")?).await,
            Tool::ListBots => {
                let now = now_ms();
                let store = self.store.lock().await;
                let mut bots = Vec::new();
                for bot in store.bots(network)? {
                    let schedules: Vec<_> = store
                        .schedules(network, &bot.conversation_id)?
                        .iter()
                        .filter(|entry| crate::scheduling::ahead(&entry.schedule, now))
                        .map(|entry| {
                            json!({"name":entry.schedule.name,"every_minutes":entry.schedule.every_minutes,
                                "enabled":entry.schedule.enabled,
                                "next_run_at_ms":crate::scheduling::next_run(&entry.schedule, now)})
                        })
                        .collect();
                    let instructions: String = bot.instructions.chars().take(200).collect();
                    bots.push(
                        json!({"conversation_id":bot.conversation_id,"bot_id":bot.bot_id,
                        "name":bot.name,"instructions":instructions,
                        "instructions_truncated":instructions.len() < bot.instructions.len(),
                        "project":bot.project,"models":bot.models,"skills":bot.skills,
                        "schedules":schedules,"this_bot":bot.conversation_id == scope}),
                    );
                }
                Ok(json!({ "bots": bots }))
            }
            Tool::ReadBot => {
                let now = now_ms();
                let store = self.store.lock().await;
                let bot = store.find_bot(network, field(&args, "bot")?)?;
                let schedules: Vec<_> = store
                    .schedules(network, &bot.conversation_id)?
                    .iter()
                    .map(|entry| crate::scheduler::schedule_row(entry, now))
                    .collect();
                let memory = store.memories(network, &bot.conversation_id)?;
                let library = store.skills(network)?;
                let skills = bot.skills.as_ref().map(|set| {
                    library
                        .iter()
                        .filter(|entry| set.contains(&entry.skill_id))
                        .map(|entry| json!({"skill_id":entry.skill_id,"name":entry.skill.name}))
                        .collect::<Vec<_>>()
                });
                Ok(
                    json!({"conversation_id":bot.conversation_id,"bot_id":bot.bot_id,"name":bot.name,
                    "instructions":bot.instructions,"project":bot.project,
                    "models":bot.models,"skills":skills,"uses_all_skills":bot.skills.is_none(),
                    "schedules":schedules,"memory":memory}),
                )
            }
            Tool::DeleteBot => {
                let manager = access
                    .manager
                    .as_ref()
                    .context("deleting a bot requires an agent invocation")?;
                let (deleted, event) = self.store.lock().await.delete_bot_in_turn(
                    &self.identity,
                    network,
                    &scope,
                    &manager.trigger,
                    field(&args, "bot")?,
                    now_ms(),
                )?;
                if let Some(event) = &event {
                    self.bot_deleted(network, &deleted, &event.hash);
                }
                Ok(
                    json!({"conversation_id":deleted,"deleted":true,"written":event.is_some(),
                    "primary":self.store.lock().await.primary_bot(network)?}),
                )
            }
            Tool::CreateBot => {
                let manager = access
                    .manager
                    .as_ref()
                    .context("creating a bot requires an agent invocation")?;
                let call_ref = field(&args, "call_ref")?;
                let optional = |key: &str| args[key].as_str().filter(|value| !value.is_empty());
                let project = match (optional("project_node"), optional("project_cwd")) {
                    (Some(node), Some(cwd)) => Some(BotProject {
                        target_node: node.into(),
                        cwd: cwd.into(),
                    }),
                    (None, None) => None,
                    _ => bail!("give project_node and project_cwd together"),
                };
                let (bot_id, _) = created_bot_id(&scope, &manager.trigger, call_ref);
                let existing = self
                    .store
                    .lock()
                    .await
                    .bots(network)?
                    .iter()
                    .any(|bot| bot.bot_id == bot_id);
                if !existing && let Some(project) = &project {
                    self.validate_bot_project(network, project).await?;
                }
                let (bot, created) = self.store.lock().await.create_bot_in_turn(
                    &self.identity,
                    network,
                    &scope,
                    &manager.trigger,
                    call_ref,
                    (
                        field(&args, "name")?,
                        optional("instructions").unwrap_or(""),
                        project,
                    ),
                    now_ms(),
                )?;
                if created {
                    self.publish_changed(
                        network,
                        &bot.conversation_id,
                        Changed::Event(&bot.event_hash),
                    );
                }
                Ok(json!(bot))
            }
            Tool::MessageBot => {
                let manager = access
                    .manager
                    .as_ref()
                    .context("messaging a bot requires an agent invocation")?;
                let (message, receipt, written) = self.store.lock().await.message_bot(
                    &self.identity,
                    network,
                    &scope,
                    &manager.trigger,
                    field(&args, "call_ref")?,
                    field(&args, "bot")?,
                    field(&args, "text")?,
                    now_ms(),
                )?;
                if written {
                    self.publish_changed(
                        network,
                        &receipt.conversation_id,
                        Changed::Event(&message.event_hash),
                    );
                    self.schedule_wakeup.notify_one();
                    self.outbox_wakeup.notify_one();
                }
                Ok(json!({"message":message,"delivery":receipt}))
            }
            Tool::ReadSkill => {
                let id = field(&args, "skill_id")?;
                let skill = self
                    .store
                    .lock()
                    .await
                    .skill(network, id)?
                    .with_context(|| format!("skill {id} not found"))?;
                let mut value = json!(skill);
                value["skill_id"] = json!(id);
                Ok(value)
            }
            Tool::UpdateProfile
            | Tool::ConfigureBot
            | Tool::SaveSkill
            | Tool::RemoveSkill
            | Tool::SaveSchedule
            | Tool::RemoveSchedule
            | Tool::Remember
            | Tool::Forget
            | Tool::Remind => {
                let manager = access
                    .manager
                    .as_ref()
                    .context("bot changes require an agent invocation")?;
                let call_ref = field(&args, "call_ref")?.to_owned();
                let mut request = args;
                let fields = request
                    .as_object_mut()
                    .context("tool arguments must be an object")?;
                fields.remove("call_ref");
                fields.insert("tool".into(), json!(name));
                let request: ChangeRequest = serde_json::from_value(request).map_err(|error| {
                    anyhow::anyhow!("{error}. Provider and folders change only in Settings")
                })?;
                let provider = self
                    .settings_snapshot()
                    .manager
                    .context("this Node has no configured management SDK")?
                    .provider;
                let (record, written) = self.store.lock().await.change_bot(
                    &self.identity,
                    network,
                    &scope,
                    &manager.trigger,
                    &call_ref,
                    request,
                    provider,
                    now_ms(),
                )?;
                if written {
                    self.publish_changed(
                        network,
                        &record.conversation_id,
                        Changed::Event(&record.event_hash),
                    );
                }
                Ok(json!(record))
            }
            Tool::TrackWork => self.track_work(&access, &scope, &args).await,
            Tool::BindWork => {
                let report = serde_json::from_value(args["report"].clone())
                    .context("report must be a start report")?;
                self.bind_work(network, &scope, field(&args, "work_id")?, report)
                    .await
            }
            Tool::WaitCodex => {
                drop((_lease, _work));
                Ok(json!({"connected": self.codex.ready().await}))
            }
            // The Cursor SDK calls back instead of running the CLI; the same flow runs here.
            Tool::DispatchWork => {
                let socket = self.codex.socket();
                let (access, scope) = (&access, scope.as_str());
                crate::agent_cli::dispatch(
                    &args,
                    |args| async move { self.track_work(access, scope, &args).await },
                    |work_id, report| async move {
                        self.bind_work(network, scope, &work_id, report).await
                    },
                    || async {
                        self.codex.ready().await;
                    },
                    &socket,
                )
                .await
            }
            Tool::InterruptWork => {
                let id = field(&args, "work_id")?;
                self.stop_work(network, &scope, id).await
            }
            Tool::ProposeProviderConfig => {
                let manager = access
                    .manager
                    .as_ref()
                    .context("a proposal requires an agent invocation")?;
                self.propose_provider_config(network, &scope, &manager.trigger, args)
                    .await
            }
            Tool::ApplyProviderConfig => {
                self.apply_provider_config(network, &scope, field(&args, "question_id")?)
                    .await
            }
            Tool::HandOverSession => {
                let target = match (args["to"].as_str(), args["stop_following"] == true) {
                    (Some(bot), false) => Some(
                        self.store
                            .lock()
                            .await
                            .find_bot(network, bot)?
                            .conversation_id,
                    ),
                    (None, true) => None,
                    _ => bail!("give either to or stop_following=true"),
                };
                self.hand_over_session(
                    network,
                    &scope,
                    field(&args, "node")?,
                    field(&args, "session_id")?,
                    field(&args, "version")?,
                    target.as_deref(),
                )
                .await
            }
        }
    }

    async fn agent_context(&self, network: &str, scope: &str, args: &Value) -> Result<Value> {
        let offset = args["offset"].as_u64().unwrap_or(0).min(usize::MAX as u64) as usize;
        let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
        let store = self.store.lock().await;
        if let Some(hash) = args["blob_hash"].as_str() {
            // ponytail: each page re-reads the whole file; blobs are capped at MAX_BLOB_BYTES.
            let (blob, bytes) = store.read_blob(network, scope, hash)?;
            ensure!(offset <= bytes.len(), "invalid blob offset");
            let end = offset.saturating_add(limit * 1024).min(bytes.len());
            return Ok(json!({"hash":blob.hash,"size":blob.size,"offset":offset,
                "data_hex":hex::encode(&bytes[offset..end]),"next_offset":(end<bytes.len()).then_some(end)}));
        }
        let events = store.context(network, scope, None)?.events;
        let end = offset.saturating_add(limit).min(events.len());
        ensure!(offset <= end, "invalid event offset");
        let page = events[offset..end]
            .iter()
            .map(|event| {
                json!({"author":event.event.author,"created_at_ms":event.event.created_at_ms,
                    "body":crate::scheduler::agent_event_body(&event.event.body),"blobs":event.event.blobs})
            })
            .collect::<Vec<_>>();
        Ok(json!({"events":page,"next_offset":(end<events.len()).then_some(end)}))
    }

    /// Registers a dispatch before anything is sent. On this device it returns `may_send` only to
    /// the call that created the work; another device's bot gets the task instead.
    async fn track_work(
        self: &Arc<Self>,
        access: &Access,
        scope: &str,
        args: &Value,
    ) -> Result<Value> {
        let network = access.network_id.as_str();
        let manager = access
            .manager
            .as_ref()
            .context("dispatch requires an agent invocation")?;
        let call_ref = field(args, "call_ref")?;
        vibeface_protocol::validate_id(call_ref).map_err(anyhow::Error::msg)?;
        let local = self.identity.device_id()?;
        let optional = |key: &str| args[key].as_str().map(String::from);
        let task = manager.trigger.strip_prefix("task:");
        let target = match task {
            Some(_) => local.clone(),
            None => optional("target_node").unwrap_or_else(|| local.clone()),
        };
        let thread = optional("thread");
        if let Some(thread) = &thread {
            vibeface_protocol::validate_id(thread).map_err(anyhow::Error::msg)?;
            ensure!(
                target == local,
                "a thread is sent to on its own device; give another device a task instead"
            );
            ensure!(
                args["continues"].is_null(),
                "thread and continues are exclusive"
            );
            ensure!(
                self.store
                    .lock()
                    .await
                    .native_thread_conversation(thread)?
                    .is_none(),
                "this thread is a bot's own reply thread; message that bot with message_bot instead"
            );
        }
        let spec = WorkSpec {
            work_id: WorkSpec::id(scope, &manager.trigger, call_ref),
            conversation: scope.into(),
            trigger: manager.trigger.clone(),
            target: target.clone(),
            dispatcher: local.clone(),
            call_ref: call_ref.into(),
            kind: if target == local {
                WorkKind::Codex
            } else {
                WorkKind::Task
            },
            // A thread's directory is only informational and may be unknown.
            cwd: match &thread {
                Some(_) => optional("cwd").unwrap_or_default(),
                None => field(args, "cwd")?.into(),
            },
            title: field(args, "title")?.chars().take(120).collect(),
            prompt: field(args, "prompt")?.into(),
            continues: optional("continues"),
            parent: task.map(String::from).or_else(|| optional("parent")),
            thread: thread.clone(),
        };
        if target != local {
            let dispatch = self.send_dispatch(network, spec, false).await?;
            return Ok(dispatch_row(&dispatch));
        }
        if !self.providers().switches.execution {
            return Err(vibeface_bridge::failure::ExecFailure::new(
                vibeface_bridge::failure::FailureKind::ExecutionDisabled,
                "Codex execution is turned off on this device",
            )
            .into());
        }
        let cwd = spec.cwd.clone();
        let continues = spec.continues.clone();
        let (record, created) = self.accept_dispatch(network, spec).await?;
        let thread = match &continues {
            None => thread,
            Some(original) => self
                .store
                .lock()
                .await
                .works(network, scope)?
                .into_iter()
                .find(|work| &work.work_id == original)
                .and_then(|work| work.thread_id),
        };
        Ok(
            json!({"work_id":record.work_id,"call_ref":call_ref,"state":record.state,
            "may_send":created,"send_by_ms":record.send_by_ms,"cwd":cwd,"thread_id":thread}),
        )
    }
}
