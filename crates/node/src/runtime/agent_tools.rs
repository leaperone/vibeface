use super::*;
use crate::{
    scheduler::dispatch_row,
    store::{
        bot_settings::{ChangeRequest, bot_message_id, change_id},
        bots::created_bot_id,
    },
    tools::{Effect, Tool, ToolFailure},
    work::{RequestedWorkKind, WorkRequest, WorkSpec},
};

impl Runtime {
    /// The one entry for coordination tools: the Cursor SDK callback and `vibeface tool` both land here.
    pub async fn tool(self: &Arc<Self>, access: Access, name: &str, args: Value) -> Result<Value> {
        let conversation_id = access.conversation_id.clone();
        let asked_work = args["work_id"].as_str().map(str::to_owned);
        let mut stage = "validation";
        let result = match Tool::invocation(name, args.clone()) {
            Ok((tool, args)) => self
                .run_tool(access, tool, args.clone(), &mut stage)
                .await
                .map(|value| tool.annotate(&args, value))
                .map_err(|error| {
                    ToolFailure::new(
                        tool.name(),
                        &args,
                        stage,
                        stage == "execution" && tool.effect() != Effect::Read,
                        &error,
                    )
                    .into()
                }),
            Err(error) => Err(ToolFailure::new(name, &args, stage, false, &error).into()),
        };
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

    async fn run_tool(
        self: &Arc<Self>,
        access: Access,
        tool: Tool,
        args: Value,
        stage: &mut &'static str,
    ) -> Result<Value> {
        *stage = "authorization";
        let name = tool.name();
        let _work = self.maintenance.enter()?;
        ensure!(!access.human, "tool endpoint requires a scoped Agent token");
        let scope = access
            .conversation_id
            .clone()
            .context("missing tool scope")?;
        self.require_scope(&access, &scope).await?;
        if let Some(call_ref) = args["call_ref"].as_str()
            && vibeface_protocol::validate_id(call_ref).is_err()
        {
            bail!(
                "call_ref must contain 1-128 ASCII letters, digits, hyphens, dots or underscores, such as create-dokilove; it only keys retries of this call"
            );
        }
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
        *stage = "execution";
        let network = access.network_id.as_str();
        match tool {
            Tool::ToolSearch | Tool::ToolDescribe => {
                let kind = match &access.manager {
                    Some(manager) => Some(
                        self.store
                            .lock()
                            .await
                            .scheduler_job(&scope, &manager.trigger)?
                            .context("this turn is not running here")?
                            .kind,
                    ),
                    None => None,
                };
                if tool == Tool::ToolDescribe {
                    let target = Tool::parse(field(&args, "name")?)
                        .filter(|tool| tool.available(kind))
                        .context("unknown or unavailable target tool")?;
                    return Ok(
                        json!({"name":target.name(),"description":target.schema().expect("available schema").0,
                        "input_schema":target.input_schema()}),
                    );
                }
                let query = args["query"].as_str().unwrap_or("").to_lowercase();
                let mut found: Vec<_> = Tool::ALL.into_iter().filter(|tool| tool.available(kind))
                    .filter_map(|tool| {
                        let description = tool.schema()?.0;
                        format!("{} {description}", tool.name()).to_lowercase().contains(&query)
                            .then(|| json!({"name":tool.name(),"description":description.split('.').next().unwrap_or(description)}))
                    }).collect();
                let limit = args["limit"].as_u64().unwrap_or(20) as usize;
                let more = found.len() > limit;
                found.truncate(limit);
                Ok(json!({"tools":found,"more":more}))
            }
            Tool::ToolCall => unreachable!("discovery calls are resolved before admission"),
            Tool::HistorySearch | Tool::HistoryRead => {
                self.agent_history(&access, &scope, tool, &args).await
            }
            Tool::Nodes => {
                self.rpc_inner(access.clone(), json!({"method":"nodes"}))
                    .await
            }
            Tool::Context => self.agent_context(&access, &scope, &args).await,
            // Dispatches the DAG does not name yet are listed too, so none is sent twice.
            Tool::WorkStatus => {
                let store = self.store.lock().await;
                let works = store.works(network, &scope)?;
                let dispatches = store.undelivered_dispatches(network, &scope)?;
                drop(store);
                work_status(&works, &dispatches, &args)
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
                    .find(|bot| bot.bot_id == bot_id);
                Ok(match bot {
                    Some(bot) => tool_bot(&bot),
                    None => json!({"found":false,"call_ref":call_ref,
                        "reason":"no confirmed record for this call_ref; an absent record does not authorize a new reference or prove no effect"}),
                })
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
                        "project":bot.project,"models":bot.models,"reasoning_efforts":bot.reasoning_efforts,"skills":bot.skills,"uses_all_skills":bot.skills.is_none(),"avatar":bot.shown_avatar(),
                        "schedules":schedules,"this_bot":bot.conversation_id == scope}),
                    );
                }
                Ok(json!({ "bots": bots }))
            }
            Tool::ReadBot => read_bot(
                &*self.store.lock().await,
                network,
                field(&args, "bot")?,
                now_ms(),
            ),
            Tool::ListSessions => {
                let sessions = self
                    .store
                    .lock()
                    .await
                    .attributed_sessions(network, &scope)?;
                list_sessions(&sessions, &args)
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
                Ok(tool_bot(&bot))
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
            Tool::WaitWork => {
                drop((_lease, _work));
                Ok(json!({"agent": self.ready_for(network, &args).await}))
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
                    |args| async move {
                        Ok((self.ready_for(network, &args).await)
                            .map(|agent| agent.name().to_owned()))
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

    async fn agent_history(
        &self,
        access: &Access,
        scope: &str,
        tool: Tool,
        args: &Value,
    ) -> Result<Value> {
        let store = self.store.lock().await;
        let basis = match &access.manager {
            Some(manager) => {
                store
                    .scheduler_job(scope, &manager.trigger)?
                    .context("this turn is not running here")?
                    .basis
            }
            None => store.version_vector(&access.network_id)?,
        };
        let at = if tool == Tool::HistoryRead {
            &args["at"]
        } else {
            &args["before"]
        };
        let at = if at.is_null() {
            None
        } else {
            let (author, seq) = position(at, &basis)?;
            let event = store
                .scope_event(&access.network_id, scope, author, seq)?
                .context("no event with this author and seq in this conversation")?;
            ensure!(
                at["event_id"].is_null() || at["event_id"] == event.hash,
                "event_id does not match this conversation's author and seq"
            );
            Some((author, seq, false))
        };
        if tool == Tool::HistoryRead {
            let (author, seq, _) = at.context("at is required")?;
            let mut result = event_page(
                &store,
                &access.network_id,
                scope,
                author,
                seq,
                args["offset"].as_u64().unwrap_or(0) as usize,
            )?;
            result["partial"] = json!(true);
            result["coverage"] = json!("local_conversation_at_basis");
            return Ok(result);
        }
        let kinds: Vec<String> = match &args["types"] {
            Value::Null => vec![],
            value => serde_json::from_value(value.clone())?,
        };
        let query = args["query"].as_str().unwrap_or("").to_lowercase();
        let limit = args["limit"].as_u64().unwrap_or(20) as usize;
        let (events, mut more) =
            store.history_page(&access.network_id, scope, &basis, at, &kinds, 100)?;
        let mut rows = Vec::new();
        let mut next = Value::Null;
        let mut used = 0;
        for (index, event) in events.iter().enumerate() {
            let cursor =
                json!({"author":event.event.author,"seq":event.event.seq,"event_id":event.hash});
            let body = crate::scheduler::agent_event_body(&event.event.body);
            if !body.to_string().to_lowercase().contains(&query) {
                next = cursor;
                continue;
            }
            let row = json!({"event_id":event.hash,"author":event.event.author,"seq":event.event.seq,
                "created_at_ms":event.event.created_at_ms,"kind":event.event.body.kind(),
                "body":crate::scheduler::bounded(&body, EVENT_BYTES)});
            if !rows.is_empty() && used + row.to_string().len() > PAGE_BYTES {
                more = true;
                break;
            }
            used += row.to_string().len();
            rows.push(row);
            next = cursor;
            if rows.len() == limit {
                more |= index + 1 < events.len();
                break;
            }
        }
        Ok(
            json!({"events":rows,"more":more,"next":if more { next } else { Value::Null },
            "partial":true,"coverage":"local_conversation_at_basis"}),
        )
    }

    /// Reads history as of the turn's basis, the same history its prompt was built from, so a
    /// position the prompt names still points at the same event while new ones arrive.
    async fn agent_context(&self, access: &Access, scope: &str, args: &Value) -> Result<Value> {
        let network = access.network_id.as_str();
        let store = self.store.lock().await;
        if let Some(hash) = args["blob_hash"].as_str() {
            return blob_page(&store, network, scope, hash, args);
        }
        let basis = match &access.manager {
            Some(manager) => {
                store
                    .scheduler_job(scope, &manager.trigger)?
                    .context("this turn is not running here")?
                    .basis
            }
            None => store.version_vector(network)?,
        };
        if !args["at"].is_null() {
            let (author, seq) = position(&args["at"], &basis)?;
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            return event_page(&store, network, scope, author, seq, offset);
        }
        let at = match (&args["before"], &args["after"]) {
            (Value::Null, Value::Null) => None,
            (at, Value::Null) => Some((at, false)),
            (Value::Null, at) => Some((at, true)),
            _ => bail!("give before or after, not both"),
        };
        let at = at
            .map(|(at, after)| position(at, &basis).map(|(author, seq)| (author, seq, after)))
            .transpose()?;
        let kinds: Vec<String> = match &args["types"] {
            Value::Null => vec![],
            types => serde_json::from_value(types.clone()).context("types lists event types")?,
        };
        let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
        let (events, more) = store.history_page(network, scope, &basis, at, &kinds, limit)?;
        drop(store);
        let after = at.is_some_and(|(_, _, after)| after);
        let mut rows = Vec::new();
        let mut used = 0;
        let mut cut = false;
        for event in &events {
            let row = json!({"author":event.event.author,"seq":event.event.seq,"created_at_ms":event.event.created_at_ms,
                "body":crate::scheduler::bounded(&crate::scheduler::agent_event_body(&event.event.body), EVENT_BYTES),
                "blobs":event.event.blobs});
            used += row.to_string().len();
            if !rows.is_empty() && used > PAGE_BYTES {
                cut = true;
                break;
            }
            rows.push(row);
        }
        if !after {
            rows.reverse();
        }
        Ok(json!({"events":rows,"more":more || cut}))
    }

    /// Registers a dispatch before anything is sent, then has its provider start it. Only the
    /// call that created the work gets `may_send`, and only when the dispatching Agent sends it
    /// itself; another device's bot gets the task instead.
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
        }
        let agent: Option<Provider> =
            serde_json::from_value(args["agent"].clone()).context("agent names a provider")?;
        let work_id = WorkSpec::id(scope, &manager.trigger, call_ref);
        let kind = if target == local {
            RequestedWorkKind::Agent(agent)
        } else {
            ensure!(
                agent.is_none(),
                "agent picks a provider of this device; another device's bot picks its own"
            );
            RequestedWorkKind::Task
        };
        let requested_model = optional("model")
            .map(|model| model.trim().to_owned())
            .filter(|model| !model.is_empty());
        if target != local && requested_model.is_some() {
            bail!(
                "a remote task uses the target Bot's model; omit model or dispatch locally with an execution provider"
            );
        }
        let request = WorkRequest {
            work_id,
            conversation: scope.into(),
            trigger: manager.trigger.clone(),
            target: target.clone(),
            dispatcher: local.clone(),
            call_ref: call_ref.into(),
            kind,
            model: requested_model,
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
            let dispatch = self.send_dispatch(network, request, false).await?;
            return Ok(dispatch_row(&dispatch));
        }
        let admitted = self.accept_dispatch(network, request, args).await?;
        let (spec, record, created) = (admitted.spec, admitted.record, admitted.created);
        let thread = match &spec.continues {
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
        let may_send = created
            && self
                .start_work(
                    self.work_executor(&record)?,
                    network,
                    &spec,
                    thread.as_deref(),
                    args,
                )
                .await?;
        let record = match created {
            true => (self.store.lock().await.work_record(&record.work_id)?)
                .context("work not found on this device")?,
            false => record,
        };
        Ok(
            json!({"work_id":record.work_id,"call_ref":call_ref,"state":record.state,
            "may_send":may_send,"send_by_ms":record.send_by_ms,"cwd":spec.cwd,"thread_id":thread,
            "model":spec.model,"resolved":spec.resolved}),
        )
    }

    /// The provider a dispatch with these `args` would run on here, once the Node's App Server
    /// connection is up when that is Codex; `None` when none would, as `track_work` then says.
    async fn ready_for(&self, network: &str, args: &Value) -> Option<Provider> {
        let provider = self.executing(network, args).await.ok()?;
        if provider.on_app_server() {
            self.codex.ready().await;
        }
        Some(provider)
    }

    /// The provider a work on this device runs on: `agent`; else the one its thread's works ran
    /// on, a thread none ran in going to the provider that takes work in any thread; else the
    /// one the work it continues ran on; else the bot's own provider when it executes work
    /// here, else the first one that does.
    async fn executing(&self, network: &str, args: &Value) -> Result<Provider> {
        let agent: Option<Provider> =
            serde_json::from_value(args["agent"].clone()).context("agent names a provider")?;
        let store = self.store.lock().await;
        self.executing_from(
            &store,
            network,
            agent,
            args["thread"].as_str(),
            args["continues"].as_str(),
        )
    }

    pub(super) fn executing_from(
        &self,
        store: &Store,
        network: &str,
        agent: Option<Provider>,
        thread: Option<&str>,
        continues: Option<&str>,
    ) -> Result<Provider> {
        use vibeface_bridge::failure::{ExecFailure, FailureKind};
        let earlier = match (agent, thread, continues) {
            (Some(_), _, _) => None,
            (None, Some(thread), _) => self.ran_on_from(store, network, thread)?.or_else(|| {
                Provider::ALL
                    .into_iter()
                    .find(|provider| provider.work().any_thread)
            }),
            (None, None, Some(original)) => {
                store.work_record(original)?.and_then(|run| match run.kind {
                    WorkKind::Agent(provider) => Some(provider),
                    WorkKind::Task => None,
                })
            }
            (None, None, None) => None,
        };
        let execution = self.providers().switches.execution;
        let off = |what: String| -> anyhow::Error {
            ExecFailure::new(FailureKind::ExecutionDisabled, what).into()
        };
        match agent.or(earlier) {
            Some(agent) if execution.contains(&agent) => Ok(agent),
            Some(agent) => Err(off(format!(
                "{} execution is turned off on this device; ask the user to turn it on with propose_provider_config execution=[\"{}\"]",
                agent.label(),
                agent.name()
            ))),
            None => (self.manager().map(|manager| manager.provider))
                .filter(|provider| execution.contains(provider))
                .or_else(|| execution.first().copied())
                .ok_or_else(|| off(
                    "execution is turned off on this device; ask the user to turn it on with propose_provider_config execution".into(),
                )),
        }
    }

    /// The provider this device's works in a session of this network ran on.
    pub(super) fn ran_on_from(
        &self,
        store: &Store,
        network: &str,
        thread: &str,
    ) -> Result<Option<Provider>> {
        let runs = store.work_records_on_thread(thread)?;
        Ok(runs.iter().find_map(|run| match run.kind {
            WorkKind::Agent(provider)
                if matches!(store.conversation(network, &run.conversation), Ok(Some(_))) =>
            {
                Some(provider)
            }
            _ => None,
        }))
    }
}

/// A bot as `list_bots` and `read_bot` show its face: the one it shows and whether it was picked.
/// The face's version only names a panel write, so a tool leaves it out.
fn tool_bot(bot: &crate::model::Bot) -> Value {
    let mut value = json!(bot);
    value["avatar"] = json!(bot.shown_avatar());
    if let Some(fields) = value.as_object_mut() {
        fields.remove("avatar_version");
    }
    value
}

/// Bytes one tool result carries, and one event within it; the rest is read page by page.
const PAGE_BYTES: usize = 32_000;
const EVENT_BYTES: usize = 8_000;

/// An `{author, seq}` position within the turn's history.
fn position<'a>(at: &'a Value, basis: &crate::model::VersionVector) -> Result<(&'a str, u64)> {
    let author = at["author"].as_str().context("author is required")?;
    let seq = at["seq"].as_u64().context("seq is required")?;
    ensure!(
        seq <= basis.get(author).copied().unwrap_or(0),
        "that position is after this turn's history"
    );
    Ok((author, seq))
}

/// One event with its body as JSON text from byte `offset`, as much as fits in `PAGE_BYTES`;
/// the pages joined are the whole body.
pub(crate) fn event_page(
    store: &crate::store::Store,
    network: &str,
    scope: &str,
    author: &str,
    seq: u64,
    offset: usize,
) -> Result<Value> {
    let event = store
        .scope_event(network, scope, author, seq)?
        .context("no event with this author and seq in this conversation")?;
    let hash = event.hash;
    let event = event.event;
    let text = crate::scheduler::agent_event_body(&event.body).to_string();
    ensure!(
        offset <= text.len() && text.is_char_boundary(offset),
        "invalid body offset"
    );
    let mut page = json!({"event_id":hash,"author":event.author,"seq":event.seq,"created_at_ms":event.created_at_ms,
        "blobs":event.blobs,"offset":offset,"size":text.len(),"next_offset":u64::MAX,"body_json":""});
    let mut room = PAGE_BYTES.saturating_sub(page.to_string().len());
    let mut end = offset;
    for c in text[offset..].chars() {
        let escaped = match c {
            '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
            c if (c as u32) < 0x20 => 6,
            c => c.len_utf8(),
        };
        if escaped > room {
            break;
        }
        room -= escaped;
        end += c.len_utf8();
    }
    page["body_json"] = json!(text[offset..end]);
    page["next_offset"] = json!((end < text.len()).then_some(end));
    Ok(page)
}

/// Up to `limit` KiB of an attachment from `offset`: text as UTF-8, other content as base64.
/// An attachment not on this device yet says `pending_download`.
pub(crate) fn blob_page(
    store: &crate::store::Store,
    network: &str,
    scope: &str,
    hash: &str,
    args: &Value,
) -> Result<Value> {
    use base64::Engine as _;
    let size = store.referenced_size(network, scope, hash)?;
    let blob = BlobRef {
        hash: hash.into(),
        size,
    };
    if !store.has_blob(&blob)? {
        return Ok(json!({"hash":hash,"size":size,"pending_download":true}));
    }
    let offset = args["offset"].as_u64().unwrap_or(0);
    ensure!(offset <= size, "invalid blob offset");
    let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 32) as usize * 1024;
    let (_, mut bytes) = store.read_blob_range(network, scope, hash, offset, limit)?;
    let text = match std::str::from_utf8(&bytes) {
        Ok(_) => true,
        // A character split by the page end continues on the next page.
        Err(error)
            if error.error_len().is_none()
                && error.valid_up_to() > 0
                && offset + (bytes.len() as u64) < size =>
        {
            bytes.truncate(error.valid_up_to());
            true
        }
        Err(_) => false,
    };
    let end = offset + bytes.len() as u64;
    let mut page =
        json!({"hash":hash,"size":size,"offset":offset,"next_offset":(end < size).then_some(end)});
    if text {
        page["text"] = json!(String::from_utf8(bytes)?);
    } else {
        page["data_base64"] = json!(base64::engine::general_purpose::STANDARD.encode(&bytes));
    }
    Ok(page)
}

/// `read_bot`: one bot's whole configuration, every skill it uses included, the whole library
/// when it uses all of them.
pub(crate) fn read_bot(
    store: &crate::store::Store,
    network: &str,
    wanted: &str,
    now: i64,
) -> Result<Value> {
    let bot = store.find_bot(network, wanted)?;
    let schedules: Vec<_> = store
        .schedules(network, &bot.conversation_id)?
        .iter()
        .map(|entry| crate::scheduler::schedule_row(entry, now))
        .collect();
    let memory = store.memories(network, &bot.conversation_id)?;
    let skills: Vec<_> = store
        .skills(network)?
        .into_iter()
        .filter(|entry| {
            bot.skills
                .as_ref()
                .is_none_or(|set| set.contains(&entry.skill_id))
        })
        .map(|entry| json!({"skill_id":entry.skill_id,"name":entry.skill.name}))
        .collect();
    Ok(
        json!({"conversation_id":bot.conversation_id,"bot_id":bot.bot_id,"name":bot.name,
        "instructions":bot.instructions,"project":bot.project,
        "models":bot.models,"reasoning_efforts":bot.reasoning_efforts,"skills":skills,"uses_all_skills":bot.skills.is_none(),
        "avatar":bot.shown_avatar(),"schedules":schedules,"memory":memory}),
    )
}

/// `list_sessions`: a page of the sessions this bot follows, newest first, older than the
/// `before` a previous page returned as `next`. The cursor is a session's rank, so a session
/// handed over or muted between pages neither breaks the next page nor hides another.
pub(crate) fn list_sessions(sessions: &[Value], args: &Value) -> Result<Value> {
    let before = args["before"].as_str();
    let older: Vec<&Value> = sessions
        .iter()
        .rev()
        .filter(|row| before.is_none_or(|before| row["rank"].as_str() < Some(before)))
        .collect();
    let rows: Vec<Value> = older
        .iter()
        .map(|row| crate::scheduler::session_row(row))
        .collect();
    let (page, more) = page(&rows, 0, args);
    let next = more.then(|| older[page.len() - 1]["rank"].clone());
    Ok(json!({"sessions":page,"more":more,"next":next}))
}

/// Up to `limit` rows from `start` within `PAGE_BYTES`, and whether more follow.
fn page(rows: &[Value], start: usize, args: &Value) -> (Vec<Value>, bool) {
    let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 50) as usize;
    let mut page = Vec::new();
    let mut used = 0;
    for row in rows.iter().skip(start).take(limit) {
        used += row.to_string().len();
        if !page.is_empty() && used > PAGE_BYTES {
            break;
        }
        page.push(row.clone());
    }
    let more = start + page.len() < rows.len();
    (page, more)
}

/// `work_status`: one work by `work_id` in full, or a page of this conversation's works newest
/// first, after the work `before` names, within `PAGE_BYTES`.
pub(crate) fn work_status(
    works: &[crate::model::WorkProjection],
    dispatches: &[crate::work::Dispatch],
    args: &Value,
) -> Result<Value> {
    if let Some(id) = args["work_id"].as_str() {
        if let Some(work) = works.iter().find(|work| work.work_id == id) {
            let mut row = json!(work);
            if work.result_text.len() > PAGE_BYTES {
                row["result_text"] = json!(crate::scheduler::prefix(&work.result_text, PAGE_BYTES));
                row["result_truncated"] = json!(true);
            }
            return Ok(row);
        }
        return Ok(dispatch_row(
            dispatches
                .iter()
                .find(|dispatch| dispatch.spec.work_id == id)
                .context("work not found in this conversation")?,
        ));
    }
    let rows: Vec<Value> = dispatches
        .iter()
        .rev()
        .map(dispatch_row)
        .chain(works.iter().rev().map(crate::scheduler::work_row))
        .collect();
    let start = match args["before"].as_str() {
        Some(before) => {
            rows.iter()
                .position(|row| row["work_id"] == before)
                .context("before names no work of this conversation")?
                + 1
        }
        None => 0,
    };
    let (page, more) = page(&rows, start, args);
    Ok(json!({"works":page,"more":more}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::triage::tests::{bot, claim_reply, node, start_reply};

    async fn post(
        node: &Runtime,
        admin: &LocalIdentity,
        network: &str,
        scope: &str,
        id: &str,
        text: &str,
    ) -> Result<HashedEvent> {
        let body = admin.attest(
            network,
            scope,
            EventBody::MessagePosted {
                message_id: id.into(),
                role: Role::User,
                text: text.into(),
                attachments: vec![],
                report_results: Some(true),
                in_reply_to: None,
                status: None,
                answers: None,
                attestation: None,
                failure: None,
            },
        )?;
        let signed = node.store.lock().await.append_event(
            &node.identity,
            network,
            scope,
            body,
            vec![],
            now_ms(),
        )?;
        Ok(HashedEvent {
            hash: signed.hash,
            event: serde_json::from_str(&signed.canonical_body)?,
        })
    }

    #[tokio::test]
    async fn history_discovery_is_scoped_bounded_and_stable_at_the_turn_basis() -> Result<()> {
        let (node, admin, dir) = node().await?;
        bot(&node, &admin, "c", "Ops", &[]).await?;
        bot(&node, &admin, "d", "Private", &[]).await?;
        let network = node.store.lock().await.network()?.context("network")?;
        for i in 0..5 {
            post(
                &node,
                &admin,
                &network,
                "c",
                &format!("older-{i}"),
                &format!("needle-{i} {}", "x".repeat(9_000)),
            )
            .await?;
        }
        let elsewhere = post(&node, &admin, &network, "d", "secret", "needle-private").await?;
        let (trigger, attempt) = start_reply(&node, &admin, "trigger").await?;
        let idle = Access {
            network_id: network.clone(),
            human: false,
            conversation_id: Some("c".into()),
            manager: None,
        };
        node.register_manager_attempt(
            "tools-history".into(),
            idle,
            &attempt,
            &trigger,
            Provider::Codex,
        )
        .await?;
        let access = node
            .agent_tokens
            .lock()
            .await
            .get("tools-history")
            .cloned()
            .context("registered tool token")?;
        post(&node, &admin, &network, "c", "late", "needle-late").await?;
        let mut before = Value::Null;
        let mut ids = Vec::new();
        loop {
            let mut args = json!({"query":"needle","types":["message_posted"],"limit":50});
            if !before.is_null() {
                args["before"] = before;
            }
            let result = node
                .tool(
                    access.clone(),
                    "tool_call",
                    json!({"name":"history_search","arguments":args}),
                )
                .await?;
            assert!(result.to_string().len() <= PAGE_BYTES + 1_024);
            assert_eq!(result["partial"], true);
            for event in result["events"].as_array().context("events")? {
                ids.push(event["event_id"].as_str().context("event id")?.to_owned());
                let read = node
                    .tool(
                        access.clone(),
                        "history_read",
                        json!({"at":{
                    "author":event["author"],"seq":event["seq"],"event_id":event["event_id"]}}),
                    )
                    .await?;
                assert_eq!(read["event_id"], event["event_id"]);
                assert!(!read.to_string().contains("attestation"));
                assert!(!read.to_string().contains("needle-private"));
                assert!(!read.to_string().contains("needle-late"));
            }
            if result["more"] != true {
                break;
            }
            before = result["next"].clone();
            assert!(!before.is_null());
        }
        assert_eq!(ids.len(), 5);
        assert_eq!(
            ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
            5
        );
        assert!(
            node.tool(
                access.clone(),
                "history_read",
                json!({"at":{
            "author":elsewhere.event.author,"seq":elsewhere.event.seq,"event_id":elsewhere.hash}})
            )
            .await
            .is_err()
        );
        assert!(
            node.tool(
                access.clone(),
                "history_search",
                json!({"conversation_id":"d"})
            )
            .await
            .is_err()
        );
        let searched = node
            .tool(access.clone(), "tool_search", json!({"query":"history"}))
            .await?;
        assert!(
            searched["tools"]
                .as_array()
                .context("tools")?
                .iter()
                .any(|tool| tool["name"] == "history_read")
        );
        node.fence_manager_attempt(&attempt).await?;
        assert!(
            node.tool(access, "history_search", json!({}))
                .await
                .is_err()
        );
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn discovery_replays_the_existing_call_and_keeps_missing_outcomes_unknown() -> Result<()>
    {
        let (node, admin, dir) = node().await?;
        bot(&node, &admin, "c", "Ops", &[]).await?;
        let network = node.store.lock().await.network()?.context("network")?;
        let (trigger, attempt) = start_reply(&node, &admin, "trigger").await?;
        let idle = Access {
            network_id: network.clone(),
            human: false,
            conversation_id: Some("c".into()),
            manager: None,
        };
        node.register_manager_attempt(
            "tools-replay".into(),
            idle.clone(),
            &attempt,
            &trigger,
            Provider::Codex,
        )
        .await?;
        let access = node
            .agent_tokens
            .lock()
            .await
            .get("tools-replay")
            .cloned()
            .context("registered tool token")?;
        assert!(
            node.tool(
                idle,
                "tool_call",
                json!({"name":"create_bot","arguments":{"call_ref":"one","name":"Docs"}})
            )
            .await
            .is_err()
        );
        let first = node
            .tool(
                access.clone(),
                "tool_call",
                json!({"name":"create_bot","arguments":{"name":"Docs"},"call_ref":"one"}),
            )
            .await?;
        let basis = node.store.lock().await.version_vector(&network)?;
        let replay = node
            .tool(
                access.clone(),
                "create_bot",
                json!({"name":"Docs","call_ref":"one"}),
            )
            .await?;
        assert_eq!(first["bot_id"], replay["bot_id"]);
        assert_eq!(first["event_hash"], replay["event_hash"]);
        assert_eq!(basis, node.store.lock().await.version_vector(&network)?);
        let lookup = node
            .tool(access.clone(), "call_lookup", json!({"call_ref":"one"}))
            .await?;
        assert_eq!(lookup["bot_id"], first["bot_id"]);
        assert_eq!(lookup["call_record"]["execution"], "applied");
        let missing = node
            .tool(access.clone(), "call_lookup", json!({"call_ref":"lost"}))
            .await?;
        assert_eq!(missing["found"], false);
        assert_eq!(missing["call_record"]["execution"], "unknown");
        assert_eq!(missing["call_record"]["retry"], "lookup_same_ref");
        assert!(
            node.tool(
                access,
                "tool_call",
                json!({"name":"create_bot","arguments":{"name":"Different"},"call_ref":"one"})
            )
            .await
            .is_err()
        );
        assert_eq!(basis, node.store.lock().await.version_vector(&network)?);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_cloned_access_cannot_reenter_after_backup_reregistration() -> Result<()> {
        use crate::scheduling::{RuntimeSelection, TurnExecution, TurnRuntime};
        use vibeface_bridge::failure::{FailureKind, ProviderStage};

        let (node, admin, dir) = node().await?;
        bot(&node, &admin, "c", "Ops", &[]).await?;
        let network = node.store.lock().await.network()?.context("network")?;
        let (trigger, attempt) = claim_reply(&node, &admin, "c", "trigger", "boot").await?;
        let claimed = node
            .store
            .lock()
            .await
            .scheduler_job("c", &trigger)?
            .context("claimed job")?;
        assert!(matches!(
            claimed.state,
            crate::scheduling::JobState::Claimed { .. }
        ));
        assert_eq!(
            claimed.runtime.context("claimed runtime")?.execution,
            TurnExecution::NotStarted
        );
        let idle = Access {
            network_id: network.clone(),
            human: false,
            conversation_id: Some("c".into()),
            manager: None,
        };
        let mut runtime = TurnRuntime {
            provider: Provider::Codex,
            model: Some("primary".into()),
            reasoning_effort: None,
            generation: "primary-configuration".into(),
            session_generation: node
                .store
                .lock()
                .await
                .advance_scheduler_lineage("c", &trigger, &attempt)?,
            delivery_id: attempt.clone(),
            stage: ProviderStage::Validate,
            execution: TurnExecution::NotStarted,
            attempted_at_ms: now_ms(),
            switched_from: None,
            switch_reason: None,
        };
        node.store
            .lock()
            .await
            .record_turn_runtime("c", &trigger, &attempt, &runtime)?;
        node.register_manager_attempt(
            "old-token".into(),
            idle.clone(),
            &attempt,
            &trigger,
            Provider::Codex,
        )
        .await?;
        let old = node
            .agent_tokens
            .lock()
            .await
            .get("old-token")
            .cloned()
            .context("old bearer access")?;
        let original_permission = old
            .manager
            .as_ref()
            .context("old manager")?
            .permission_version;
        node.seal_manager_tools(&attempt).await?;
        node.finish_manager_attempt(&attempt).await;
        assert!(!node.agent_tokens.lock().await.contains_key("old-token"));

        runtime.switched_from = Some(RuntimeSelection {
            provider: runtime.provider,
            model: runtime.model.clone(),
            reasoning_effort: runtime.reasoning_effort.clone(),
        });
        runtime.switch_reason = Some(FailureKind::Quota);
        runtime.provider = Provider::Pi;
        runtime.model = Some("backup".into());
        runtime.session_generation = node
            .store
            .lock()
            .await
            .advance_scheduler_lineage("c", &trigger, &attempt)?;
        node.store
            .lock()
            .await
            .record_turn_runtime("c", &trigger, &attempt, &runtime)?;
        node.register_manager_attempt("new-token".into(), idle, &attempt, &trigger, Provider::Pi)
            .await?;
        let current = node
            .agent_tokens
            .lock()
            .await
            .get("new-token")
            .cloned()
            .context("new bearer access")?;
        assert!(
            current
                .manager
                .as_ref()
                .context("current manager")?
                .permission_version
                > original_permission
        );
        let local = node.identity.device_id()?;
        {
            let store = node.store.lock().await;
            assert!(Runtime::check_manager_permission(&store, &local, &old).is_err());
            Runtime::check_manager_permission(&store, &local, &current)?;
        }
        let before = node.store.lock().await.version_vector(&network)?;
        assert!(
            node.tool(
                old,
                "create_bot",
                json!({"call_ref":"stale-access","name":"Stale bot"})
            )
            .await
            .is_err()
        );
        assert_eq!(node.store.lock().await.version_vector(&network)?, before);
        assert!(
            !node
                .store
                .lock()
                .await
                .scheduler_job("c", &trigger)?
                .context("job")?
                .tool_calls
        );
        let created = node
            .tool(
                current,
                "create_bot",
                json!({"call_ref":"fresh-access","name":"Fresh bot"}),
            )
            .await?;
        assert_eq!(created["name"], "Fresh bot");
        assert!(created["bot_id"].as_str().is_some());
        assert_ne!(node.store.lock().await.version_vector(&network)?, before);
        node.finish_manager_attempt(&attempt).await;
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn epoch8_directory_is_rejected_before_loading_any_provider_or_history() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("vibeface-old-epoch-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        std::fs::write(dir.join("data-epoch.json"), br#"{"data_epoch":8}"#)?;
        let error = Runtime::open(&dir)
            .await
            .err()
            .context("old epoch must fail")?;
        assert!(
            error
                .to_string()
                .contains("requires an initialized data epoch 9 directory")
        );
        assert_eq!(std::fs::read_dir(&dir)?.count(), 1);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn accepted_execution_keeps_its_first_model_after_defaults_change() -> Result<()> {
        let (node, admin, dir) = node().await?;
        bot(&node, &admin, "c", "Bot", &[]).await?;
        let network = node.store.lock().await.network()?.context("network")?;
        post(&node, &admin, &network, "c", "m1", "go").await?;
        node.settings.lock().unwrap().roots = vec![dir.clone()];
        node.providers
            .lock()
            .unwrap()
            .switches
            .execution_models
            .insert(Provider::Codex, "first".into());
        let spec = WorkRequest {
            work_id: WorkSpec::id("c", "message:m1", "frozen"),
            conversation: "c".into(),
            trigger: "message:m1".into(),
            target: node.identity.device_id()?,
            dispatcher: node.identity.device_id()?,
            call_ref: "frozen".into(),
            kind: RequestedWorkKind::Agent(Some(Provider::Codex)),
            model: None,
            cwd: dir.display().to_string(),
            title: "work".into(),
            prompt: "go".into(),
            continues: None,
            parent: None,
            thread: None,
        };
        assert!(
            node.accept_dispatch(&network, spec.clone(), &Value::Null)
                .await?
                .created
        );
        node.providers
            .lock()
            .unwrap()
            .switches
            .execution_models
            .insert(Provider::Codex, "changed".into());
        let (left, right) = tokio::join!(
            node.accept_dispatch(&network, spec.clone(), &Value::Null),
            node.accept_dispatch(&network, spec.clone(), &Value::Null)
        );
        assert!(!left?.created && !right?.created);
        let works = node.store.lock().await.works(&network, "c")?;
        assert_eq!(works.len(), 1);
        assert_eq!(works[0].resolved.model.as_deref(), Some("first"));
        let explicit = WorkRequest {
            work_id: WorkSpec::id("c", "message:m1", "explicit"),
            call_ref: "explicit".into(),
            model: Some("  chosen  ".into()),
            ..spec.clone()
        };
        assert!(
            node.accept_dispatch(&network, explicit.clone(), &Value::Null)
                .await?
                .created
        );
        assert!(
            !node
                .accept_dispatch(
                    &network,
                    WorkRequest {
                        model: Some("chosen".into()),
                        ..explicit.clone()
                    },
                    &Value::Null,
                )
                .await?
                .created
        );
        assert!(
            node.accept_dispatch(
                &network,
                WorkRequest {
                    model: Some("other".into()),
                    ..explicit
                },
                &Value::Null,
            )
            .await
            .is_err()
        );
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
