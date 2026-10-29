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
        let network = access.network_id.as_str();
        match tool {
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
                    .find(|bot| bot.bot_id == bot_id)
                    .context(
                        "nothing was dispatched, changed, sent or created with this call_ref",
                    )?;
                Ok(tool_bot(&bot))
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
                        "project":bot.project,"models":bot.models,"skills":bot.skills,"avatar":bot.shown_avatar(),
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
                    "avatar":bot.shown_avatar(),"schedules":schedules,"memory":memory}),
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
                        (self.ready_for(network, &args).await).map(|agent| agent.name().to_owned())
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
            ensure!(
                self.store
                    .lock()
                    .await
                    .native_thread_conversation(thread)?
                    .is_none(),
                "this thread is a bot's own reply thread; message that bot with message_bot instead"
            );
        }
        let agent: Option<Provider> =
            serde_json::from_value(args["agent"].clone()).context("agent names a provider")?;
        let work_id = WorkSpec::id(scope, &manager.trigger, call_ref);
        // A retry keeps the provider the first call settled on, whatever the switches say now.
        let retried = match agent {
            None => (self.store.lock().await.work_record(&work_id)?).map(|run| run.kind),
            Some(_) => None,
        };
        let kind = match (agent, retried) {
            (None, Some(kind)) if target == local => kind,
            _ if target == local => WorkKind::Agent(self.executing(network, args).await?),
            (None, _) => WorkKind::Task,
            (Some(_), _) => {
                bail!("agent picks a provider of this device; another device's bot picks its own")
            }
        };
        // The Codex App Server runs every turn of its threads itself. Another provider's session
        // may be open in a program the Node does not see, so it takes work only in a session one
        // of its works on this device ran in.
        if let (Some(thread), WorkKind::Agent(provider)) = (&thread, kind)
            && provider != Provider::Codex
        {
            let ran = self.ran_on(network, thread).await?;
            ensure!(
                ran == Some(provider),
                "{} takes work only in sessions this device's works ran in; this one is {}'s, so set agent to {} or leave agent out",
                provider.label(),
                ran.unwrap_or(Provider::Codex).label(),
                ran.unwrap_or(Provider::Codex).name()
            );
        }
        let spec = WorkSpec {
            work_id,
            conversation: scope.into(),
            trigger: manager.trigger.clone(),
            target: target.clone(),
            dispatcher: local.clone(),
            call_ref: call_ref.into(),
            kind,
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
        let (record, created) = self.accept_dispatch(network, spec.clone()).await?;
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
            "may_send":may_send,"send_by_ms":record.send_by_ms,"cwd":spec.cwd,"thread_id":thread}),
        )
    }

    /// The provider a dispatch with these `args` would run on here, once the Node's App Server
    /// connection is up when that is Codex; `None` when none would, as `track_work` then says.
    async fn ready_for(&self, network: &str, args: &Value) -> Option<Provider> {
        let provider = self.executing(network, args).await.ok()?;
        if provider == Provider::Codex {
            self.codex.ready().await;
        }
        Some(provider)
    }

    /// The provider a work on this device runs on: `agent`; else the one its thread's works ran
    /// on, a thread none ran in being a Codex one; else the one the work it continues ran on;
    /// else the bot's own provider when it executes work here, else the first one that does.
    async fn executing(&self, network: &str, args: &Value) -> Result<Provider> {
        use vibeface_bridge::failure::{ExecFailure, FailureKind};
        let agent: Option<Provider> =
            serde_json::from_value(args["agent"].clone()).context("agent names a provider")?;
        let earlier = match (agent, args["thread"].as_str(), args["continues"].as_str()) {
            (Some(_), _, _) => None,
            (None, Some(thread), _) => {
                Some((self.ran_on(network, thread).await?).unwrap_or(Provider::Codex))
            }
            (None, None, Some(original)) => (self.store.lock().await.work_record(original)?)
                .and_then(|run| match run.kind {
                    WorkKind::Agent(provider) => Some(provider),
                    WorkKind::Task => None,
                }),
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
    async fn ran_on(&self, network: &str, thread: &str) -> Result<Option<Provider>> {
        let store = self.store.lock().await;
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
        .context("no event with this author and seq in this conversation")?
        .event;
    let text = crate::scheduler::agent_event_body(&event.body).to_string();
    ensure!(
        offset <= text.len() && text.is_char_boundary(offset),
        "invalid body offset"
    );
    let mut page = json!({"author":event.author,"seq":event.seq,"created_at_ms":event.created_at_ms,
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
    Ok(json!({"works":page,"more":more}))
}
