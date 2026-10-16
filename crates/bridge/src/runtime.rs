use crate::{
    agent::{self, AgentConfig},
    codex::CodexClient,
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use futures_util::{Sink, SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashSet},
    fs::{File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, Notify, mpsc},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::{HeaderValue, Uri, header::AUTHORIZATION},
        protocol::WebSocketConfig,
    },
};
use vibeface_protocol::{
    BridgeEvent, ChatMessage, Command, CommandRecord, CommandRequest, CommandStatus, TaskControl,
    TaskRecord, TaskStatus, ToBridge, ToServer, VERSION, now_ms, validate_id,
};

const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_LOCAL_REQUEST_BYTES: usize = 128 * 1024;

#[derive(Args)]
pub struct RunConfig {
    #[arg(long, requires = "token")]
    server: Option<String>,
    #[arg(long, default_value = "mac-mini")]
    device_id: String,
    #[arg(long, env = "VIBEFACE_BRIDGE_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[arg(long, requires = "client_token")]
    listen: Option<std::net::SocketAddr>,
    #[arg(long, env = "VIBEFACE_CLIENT_TOKEN", hide_env_values = true)]
    client_token: Option<String>,
    #[arg(long)]
    codex_socket: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long, default_value = ".")]
    allowed_root: PathBuf,
    #[arg(
        long,
        requires = "agent_model",
        help = "Path to the official cursor-sdk-bridge binary"
    )]
    agent_bin: Option<PathBuf>,
    #[arg(
        long,
        env = "VIBEFACE_AGENT_MODEL",
        help = "Explicit Cursor SDK model id"
    )]
    agent_model: Option<String>,
    #[arg(long, default_value_t = false)]
    auto_manage: bool,
    #[arg(long, default_value_t = 20)]
    agent_daily_limit: u32,
    #[arg(long, default_value_t = 300)]
    agent_timeout_secs: u64,
}

pub(crate) struct Bridge {
    codex: Arc<CodexClient>,
    store: Mutex<Store>,
    allowed_root: PathBuf,
    agent: Option<AgentConfig>,
    auto_workspace: PathBuf,
    agent_daily_limit: u32,
    auto_manage: bool,
    manager_gate: Mutex<()>,
    rpc_gate: Mutex<()>,
    active_turns: Mutex<HashSet<String>>,
    auto_pending: Mutex<BTreeSet<String>>,
    changed: Notify,
    auto_changed: Notify,
    task_gate: Mutex<()>,
}

pub async fn run(config: RunConfig) -> Result<()> {
    if let Some(server) = &config.server {
        validate_server(server)?;
    }
    ensure!(
        config.server.is_some() == config.token.is_some(),
        "--server and VIBEFACE_BRIDGE_TOKEN must be configured together"
    );
    validate_id(&config.device_id).map_err(anyhow::Error::msg)?;
    if let Some(token) = &config.token {
        ensure!(!token.trim().is_empty(), "bridge token is empty");
    }
    if config.listen.is_some() {
        ensure!(
            config
                .client_token
                .as_ref()
                .is_some_and(|token| token.len() >= 24 && !token.chars().any(char::is_whitespace)),
            "HTTP client token must contain at least 24 characters without whitespace"
        );
    }
    ensure!(
        config.agent_timeout_secs > 0,
        "agent timeout must be positive"
    );
    ensure!(
        !config.auto_manage || config.agent_bin.is_some(),
        "--auto-manage requires --agent-bin"
    );
    let state_dir = match config.state_dir.clone() {
        Some(path) => path,
        None => default_state_dir()?,
    };
    std::fs::create_dir_all(&state_dir).context("create private state directory")?;
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))?;
    let state_dir = state_dir.canonicalize()?;
    let socket = config
        .socket
        .clone()
        .unwrap_or(state_dir.join("control.sock"));
    let (listener, _socket_guard) = bind_private_socket(&socket, &state_dir).await?;
    let allowed_root = config
        .allowed_root
        .canonicalize()
        .context("allowed root must exist")?;
    ensure!(allowed_root.is_dir(), "allowed root must be a directory");
    let manager_workspace = state_dir.join("manager");
    let auto_workspace = state_dir.join("manager-readonly");
    if config.agent_bin.is_some() {
        std::fs::create_dir_all(&manager_workspace)?;
        std::fs::set_permissions(&manager_workspace, std::fs::Permissions::from_mode(0o700))?;
    }
    if config.auto_manage {
        std::fs::create_dir_all(&auto_workspace)?;
        std::fs::set_permissions(&auto_workspace, std::fs::Permissions::from_mode(0o700))?;
    }
    let codex_socket = match config.codex_socket.clone() {
        Some(path) => path,
        None => home_dir()?.join(".codex/app-server-control/app-server-control.sock"),
    };
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let codex = Arc::new(CodexClient::start(codex_socket, events_tx));
    let agent = config.agent_bin.clone().map(|executable| AgentConfig {
        executable,
        workspace: manager_workspace,
        bridge_socket: socket,
        timeout_secs: config.agent_timeout_secs,
        read_only: false,
        model: config.agent_model.clone().unwrap_or_default(),
        task_id: None,
    });
    if let Some(configured) = &agent {
        agent::prepare_workspace(configured).await?;
        if config.auto_manage {
            let mut read_only = configured.clone();
            read_only.workspace = auto_workspace.clone();
            read_only.read_only = true;
            agent::prepare_workspace(&read_only).await?;
        }
    }
    let bridge = Arc::new(Bridge {
        codex,
        store: Mutex::new(Store::open(&state_dir.join("bridge.sqlite3"))?),
        allowed_root: allowed_root.clone(),
        agent,
        auto_workspace,
        agent_daily_limit: config.agent_daily_limit,
        auto_manage: config.auto_manage,
        manager_gate: Mutex::new(()),
        rpc_gate: Mutex::new(()),
        active_turns: Mutex::new(HashSet::new()),
        auto_pending: Mutex::new(BTreeSet::new()),
        changed: Notify::new(),
        auto_changed: Notify::new(),
        task_gate: Mutex::new(()),
    });
    let collector = tokio::spawn(bridge.clone().collect_events(events_rx));
    let local = tokio::spawn(bridge.clone().serve_local(listener));
    let remote_bridge = bridge.clone();
    let http_bridge = bridge.clone();
    let listen = config.listen;
    let client_token = config.client_token.clone();
    let remote = tokio::spawn(async move {
        if config.server.is_some() {
            remote_bridge.serve_remote(config).await
        } else {
            std::future::pending::<Result<()>>().await
        }
    });
    let http = tokio::spawn(async move {
        if let Some(address) = listen {
            let listener = tokio::net::TcpListener::bind(address)
                .await
                .context("bind local HTTP listener")?;
            tracing::info!(address=%listener.local_addr()?, "local HTTP listener ready");
            crate::local_api::serve(
                http_bridge,
                listener,
                client_token.context("HTTP token missing")?,
            )
            .await
        } else {
            std::future::pending::<Result<()>>().await
        }
    });
    let scheduler = tokio::spawn(bridge.clone().drive_tasks());
    let automatic = tokio::spawn(bridge.auto_manager());
    tokio::select! {
        result = collector => result.context("event collector stopped")??,
        result = local => result.context("local listener stopped")??,
        result = remote => result.context("server connection task stopped")??,
        result = automatic => result.context("automatic manager stopped")??,
        result = scheduler => result.context("task scheduler stopped")??,
        result = http => result.context("local HTTP stopped")??,
        result = tokio::signal::ctrl_c() => result.context("listen for shutdown")?,
    }
    Ok(())
}

impl Bridge {
    pub(crate) async fn submit(
        self: &Arc<Self>,
        request: CommandRequest,
        wait: bool,
    ) -> Result<CommandRecord> {
        ensure!(
            !matches!(request.command, Command::RunTask { .. }),
            "run_task is an internal scheduler command"
        );
        let (record, fresh) = self.store.lock().await.accept(&request)?;
        self.changed.notify_waiters();
        if !fresh {
            return Ok(record);
        }
        if wait {
            return self.clone().execute(record).await;
        }
        let bridge = self.clone();
        tokio::spawn(async move {
            if let Err(error) = bridge.execute(record).await {
                tracing::error!(%error, "command persistence failed");
            }
        });
        Ok(CommandRecord::new(request, CommandStatus::Queued))
    }

    async fn execute(self: Arc<Self>, mut record: CommandRecord) -> Result<CommandRecord> {
        record.status = CommandStatus::Running;
        record.updated_at_ms = now_ms();
        self.store.lock().await.update(&record)?;
        self.changed.notify_waiters();
        let outcome = self.execute_command(&record.request).await;
        let uncertain = outcome.as_ref().err().is_some_and(|error| {
            error
                .downcast_ref::<agent::AgentRunError>()
                .is_some_and(|error| error.uncertain)
                || error
                    .downcast_ref::<CodexFailure>()
                    .is_some_and(|error| error.uncertain)
        });
        record = record.finish(outcome.map_err(|error| error.to_string()));
        if uncertain {
            record.status = CommandStatus::Unknown;
        }
        if let Command::RunTask {
            task_id,
            captured_event_seq,
        } = &record.request.command
        {
            let task = self
                .store
                .lock()
                .await
                .task(task_id)?
                .context("task disappeared")?;
            let active = {
                let active = self.active_turns.lock().await;
                task.thread_ids.iter().any(|thread| active.contains(thread))
            };
            self.store.lock().await.finish_task_run(
                task_id,
                *captured_event_seq,
                record.status,
                active,
                record.error.clone(),
            )?;
        }
        self.store.lock().await.update(&record)?;
        self.changed.notify_waiters();
        Ok(record)
    }

    async fn execute_command(&self, request: &CommandRequest) -> Result<Value> {
        match &request.command {
            Command::Manage { prompt } => self.manage(prompt, false, None, &request.id).await,
            Command::RunTask {
                task_id,
                captured_event_seq,
            } => {
                let task = self
                    .store
                    .lock()
                    .await
                    .task(task_id)?
                    .context("task not found")?;
                if !task.auto_continue || !task.status.allows_auto() {
                    return Ok(json!({"stopped":true}));
                }
                self.reconcile_task(&task).await?;
                let active = {
                    let active = self.active_turns.lock().await;
                    task.thread_ids.iter().any(|thread| active.contains(thread))
                };
                if active {
                    return Ok(json!({"waiting_for_running_turn":true}));
                }
                let prompt = format!(
                    "Continue this explicitly authorized task.\nTASK_JSON:{}\nCAPTURED_EVENT_SEQ:{}\nInspect current thread state and latest results before taking action. Use only this task's bound sessions or start a new session in its cwd; session_start automatically binds the new thread, so never assign thread_ids yourself. Do not approve requests. Do not retry unknown writes. When work satisfies the acceptance criteria, record evidence and set awaiting_review; only the user can accept completion. Otherwise start the next necessary Codex turn, or explicitly set waiting_user/blocked/paused when intervention is required. Avoid repeated reads and summarize concisely.",
                    task_context(&task)?,
                    captured_event_seq
                );
                self.manage(&prompt, false, Some(task_id), &request.id)
                    .await
            }
            Command::Snapshot => self.snapshot().await,
            Command::ListTasks => Ok(serde_json::to_value(self.list_tasks().await?)?),
            Command::ReadTask { task_id } => Ok(serde_json::to_value(
                self.store
                    .lock()
                    .await
                    .task(task_id)?
                    .context("task not found")?,
            )?),
            Command::ChatHistory { after, limit } => Ok(serde_json::to_value(
                self.chat_history(*after, *limit).await?,
            )?),
            Command::CreateTask { task } => {
                let mut task = task.clone();
                task.cwd = self.checked_cwd(&task.cwd)?;
                ensure!(
                    !task.auto_continue || self.agent.is_some(),
                    "automatic tasks require a configured Cursor manager"
                );
                Ok(serde_json::to_value(
                    self.store.lock().await.create_task(task)?,
                )?)
            }
            Command::UpdateTask { task_id, update } => {
                let _task_gate = self.task_gate.lock().await;
                if let Some(threads) = &update.thread_ids {
                    let existing = self
                        .store
                        .lock()
                        .await
                        .task(task_id)?
                        .context("task not found")?;
                    for thread in threads
                        .iter()
                        .filter(|thread| !existing.thread_ids.contains(thread))
                    {
                        self.execute_codex(&Command::ResumeThread {
                            thread_id: thread.clone(),
                        })
                        .await?;
                    }
                }
                Ok(serde_json::to_value(
                    self.store
                        .lock()
                        .await
                        .update_task(task_id, update.clone())?,
                )?)
            }
            Command::ControlTask { task_id, action } => self.control_task(task_id, action).await,
            Command::ResolveCommand {
                command_id,
                evidence,
            } => {
                let mut store = self.store.lock().await;
                let mut record = store.command(command_id)?.context("command not found")?;
                ensure!(
                    record.status == CommandStatus::Unknown,
                    "only unknown commands can be acknowledged"
                );
                record.status = CommandStatus::Failed;
                record.error = Some(format!(
                    "user acknowledged uncertain execution without replay: {evidence}"
                ));
                store.update(&record)?;
                Ok(serde_json::to_value(record)?)
            }
            Command::TaskCommand { task_id, command } => {
                let _task_gate = self.task_gate.lock().await;
                let task = self
                    .store
                    .lock()
                    .await
                    .task(task_id)?
                    .context("task not found")?;
                if let Some(thread) = command.thread_id() {
                    ensure!(
                        task.thread_ids.iter().any(|bound| bound == thread),
                        "thread is outside the task scope"
                    );
                }
                let mut command = command.as_ref().clone();
                if let Command::StartThread { cwd } = &command {
                    ensure!(
                        self.checked_cwd(cwd)? == task.cwd,
                        "new session cwd must equal task cwd"
                    );
                    command = Command::StartThread {
                        cwd: task.cwd.clone(),
                    };
                }
                if matches!(
                    command,
                    Command::StartThread { .. }
                        | Command::StartTurn { .. }
                        | Command::SteerTurn { .. }
                        | Command::InterruptTurn { .. }
                ) {
                    ensure!(
                        task.status.allows_auto(),
                        "task is stopped; explicitly resume before writing"
                    );
                    ensure!(
                        !self.task_has_unknown(&task).await?,
                        "task has unknown commands; inspect and acknowledge them before writing"
                    );
                }
                let output = self.execute_codex(&command).await?;
                if let Command::StartThread { .. } = &command {
                    let thread = output
                        .pointer("/thread/id")
                        .and_then(Value::as_str)
                        .context("Codex start response has no thread id")?;
                    self.store.lock().await.bind_task_thread(task_id, thread)?;
                }
                Ok(output)
            }
            command => self.execute_codex(command).await,
        }
    }

    fn checked_cwd(&self, cwd: &str) -> Result<String> {
        let path = Path::new(cwd)
            .canonicalize()
            .context("thread cwd must exist")?;
        ensure!(
            path.is_dir() && path.starts_with(&self.allowed_root),
            "thread cwd must be a directory inside --allowed-root"
        );
        Ok(path.to_string_lossy().into_owned())
    }

    async fn execute_codex(&self, original: &Command) -> Result<Value> {
        let command = self.check_command(original)?;
        let _rpc = ordinary_rpc_guard(&self.rpc_gate, &command).await;
        if let Command::StartTurn { thread_id, .. } = &command {
            ensure!(
                self.active_turns.lock().await.insert(thread_id.clone()),
                "thread already has a running or unresolved turn; inspect its state first"
            );
        }
        match self.codex.execute(&command).await {
            Ok(output) => {
                if let Command::ReadThread { thread_id } | Command::ResumeThread { thread_id } =
                    &command
                {
                    match output
                        .pointer("/thread/status/type")
                        .and_then(Value::as_str)
                    {
                        Some("idle") => {
                            self.active_turns.lock().await.remove(thread_id);
                        }
                        Some("active") => {
                            self.active_turns.lock().await.insert(thread_id.clone());
                        }
                        _ => {}
                    }
                }
                let thread = command.thread_id().map(str::to_owned).or_else(|| {
                    output
                        .pointer("/thread/id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                });
                if matches!(
                    command,
                    Command::StartThread { .. }
                        | Command::ResumeThread { .. }
                        | Command::StartTurn { .. }
                ) && let Some(thread) = thread
                {
                    self.store.lock().await.track_thread(&thread)?;
                }
                Ok(output)
            }
            Err(error) => {
                if !error.uncertain
                    && let Command::StartTurn { thread_id, .. } = &command
                {
                    self.active_turns.lock().await.remove(thread_id);
                }
                Err(CodexFailure {
                    uncertain: error.uncertain,
                    message: error.message,
                }
                .into())
            }
        }
    }

    async fn task_has_unknown(&self, task: &TaskRecord) -> Result<bool> {
        Ok(self.store.lock().await.commands()?.iter().any(|record| {
            record.status == CommandStatus::Unknown
                && (record.request.command.task_id() == Some(task.id.as_str())
                    || record
                        .request
                        .command
                        .thread_id()
                        .is_some_and(|thread| task.thread_ids.iter().any(|bound| bound == thread)))
        }))
    }

    async fn control_task(&self, id: &str, action: &TaskControl) -> Result<Value> {
        let _task_gate = self.task_gate.lock().await;
        let mut task = self
            .store
            .lock()
            .await
            .task(id)?
            .context("task not found")?;
        ensure!(
            task.status != TaskStatus::Completed,
            "completed task is immutable"
        );
        let observed = (task.status, task.dirty_seq, task.thread_ids.clone());
        match action {
            TaskControl::Pause => {
                task.status = TaskStatus::Paused;
                task.auto_continue = false;
            }
            TaskControl::Resume {
                auto_continue,
                decision_limit,
            } => {
                ensure!(
                    task.status != TaskStatus::Completed,
                    "completed task cannot resume"
                );
                ensure!(
                    !*auto_continue || self.agent.is_some(),
                    "automatic tasks require a configured Cursor manager"
                );
                ensure!(
                    !self.task_has_unknown(&task).await?,
                    "inspect and acknowledge unknown commands before resuming"
                );
                self.reconcile_task(&task).await?;
                task.status = if task.thread_ids.iter().any(|thread| {
                    self.active_turns
                        .try_lock()
                        .is_ok_and(|active| active.contains(thread))
                }) {
                    TaskStatus::Running
                } else {
                    TaskStatus::NeedsContinue
                };
                task.auto_continue = *auto_continue;
                task.remaining_decisions = *decision_limit;
            }
            TaskControl::Accept { evidence } => {
                ensure!(
                    task.status == TaskStatus::AwaitingReview,
                    "task must be awaiting_review before acceptance"
                );
                ensure!(
                    !self.store.lock().await.task_has_pending_commands(&task)?,
                    "task has pending or unknown commands"
                );
                self.reconcile_task(&task).await?;
                let active = self.active_turns.lock().await;
                ensure!(
                    !task.thread_ids.iter().any(|thread| active.contains(thread)),
                    "task still has an active turn"
                );
                task.status = TaskStatus::Completed;
                task.auto_continue = false;
                task.evidence = Some(evidence.clone());
            }
        }
        let mut store = self.store.lock().await;
        let current = store.task(id)?.context("task disappeared")?;
        if matches!(
            action,
            TaskControl::Resume { .. } | TaskControl::Accept { .. }
        ) {
            ensure!(
                (
                    current.status,
                    current.dirty_seq,
                    current.thread_ids.clone()
                ) == observed,
                "task changed during reconciliation; inspect it and retry the control command"
            );
        }
        if matches!(action, TaskControl::Accept { .. }) {
            ensure!(
                current.status == TaskStatus::AwaitingReview
                    && !store.task_has_pending_commands(&current)?,
                "task changed during acceptance; inspect it again"
            );
        }
        if matches!(action, TaskControl::Resume { .. }) {
            ensure!(
                !store
                    .commands()?
                    .iter()
                    .any(|record| record.status == CommandStatus::Unknown
                        && (record.request.command.task_id() == Some(id)
                            || record
                                .request
                                .command
                                .thread_id()
                                .is_some_and(|thread| current
                                    .thread_ids
                                    .iter()
                                    .any(|bound| bound == thread)))),
                "task became uncertain during resume"
            );
        }
        task.manager_session = current.manager_session;
        task.thread_ids = current.thread_ids;
        task.dirty_seq = current.dirty_seq;
        task.handled_seq = current.handled_seq;
        task.summary = current.summary;
        if !matches!(action, TaskControl::Accept { .. }) {
            task.evidence = current.evidence;
        }
        Ok(serde_json::to_value(store.save_task_event(
            task,
            "task.controlled",
            matches!(action, TaskControl::Resume { .. }),
        )?)?)
    }

    async fn reconcile_task(&self, task: &TaskRecord) -> Result<()> {
        let snapshot = self.codex.snapshot().await;
        ensure!(
            snapshot
                .pointer("/connection/state")
                .and_then(Value::as_str)
                == Some("connected"),
            "Codex is not connected; task remains stopped"
        );
        for thread in &task.thread_ids {
            let output = self
                .execute_codex(&Command::ResumeThread {
                    thread_id: thread.clone(),
                })
                .await?;
            let status = output
                .pointer("/thread/status/type")
                .and_then(Value::as_str);
            ensure!(
                matches!(status, Some("idle" | "active")),
                "thread state is unknown; inspect it before continuing"
            );
        }
        let snapshot = self.codex.snapshot().await;
        let pending = snapshot.get("pending_requests").and_then(Value::as_array);
        ensure!(
            !pending.is_some_and(|pending| pending.iter().any(|request| request
                .get("params")
                .is_some_and(|params| string_field(params, &["threadId", "thread_id"])
                    .is_some_and(|thread| task.thread_ids.contains(&thread))))),
            "task is waiting for a human request"
        );
        Ok(())
    }

    pub(crate) async fn command(&self, id: &str) -> Result<Option<CommandRecord>> {
        self.store.lock().await.command(id)
    }
    pub(crate) async fn events_after(&self, after: u64, limit: usize) -> Result<Vec<BridgeEvent>> {
        self.store.lock().await.events_after(after, limit)
    }
    pub(crate) async fn list_tasks(&self) -> Result<Vec<TaskRecord>> {
        self.store.lock().await.tasks()
    }
    pub(crate) async fn chat_history(&self, after: u64, limit: usize) -> Result<Vec<ChatMessage>> {
        self.store.lock().await.chat_history(after, limit)
    }

    async fn drive_tasks(self: Arc<Self>) -> Result<()> {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tick.tick().await;
            if self.agent.is_none() || self.manager_gate.try_lock().is_err() {
                continue;
            }
            if self
                .codex
                .snapshot()
                .await
                .pointer("/connection/state")
                .and_then(Value::as_str)
                != Some("connected")
            {
                continue;
            }
            let claimed = self.store.lock().await.claim_task()?;
            if let Some((_task, record)) = claimed {
                self.clone().execute(record).await?;
            }
        }
    }

    fn check_command(&self, command: &Command) -> Result<Command> {
        if let Command::StartThread { cwd } = command {
            let cwd = Path::new(cwd)
                .canonicalize()
                .context("thread cwd must exist")?;
            ensure!(cwd.is_dir(), "thread cwd must be a directory");
            ensure!(
                cwd.starts_with(&self.allowed_root),
                "thread cwd is outside --allowed-root"
            );
            return Ok(Command::StartThread {
                cwd: cwd.to_string_lossy().into_owned(),
            });
        }
        Ok(command.clone())
    }

    pub(crate) async fn snapshot(&self) -> Result<Value> {
        let codex = self.codex.snapshot().await;
        let calls_today = self.store.lock().await.agent_calls(now_ms() / 86_400_000)?;
        Ok(json!({
            "codex": codex,
            "manager": {
                "enabled": self.agent.is_some(),
                "auto_manage": self.auto_manage,
                "daily_limit": self.agent_daily_limit,
                "calls_today": calls_today,
                "running": self.manager_gate.try_lock().is_err(),
                "task_count": self.store.lock().await.tasks()?.len()
            }
        }))
    }

    async fn manage(
        &self,
        prompt: &str,
        read_only: bool,
        task_id: Option<&str>,
        command_id: &str,
    ) -> Result<Value> {
        let mut config = self
            .agent
            .clone()
            .context("Cursor manager is disabled; configure --agent-bin")?;
        if read_only {
            config.read_only = true;
            config.workspace = self.auto_workspace.clone();
        }
        config.task_id = task_id.map(str::to_owned);
        let _manager = self.manager_gate.lock().await;
        if let Some(task_id) = task_id {
            let task = self
                .store
                .lock()
                .await
                .task(task_id)?
                .context("task not found")?;
            ensure!(
                task.auto_continue && task.status.allows_auto(),
                "task stopped before manager execution"
            );
        }
        let previous_session = {
            let mut store = self.store.lock().await;
            ensure!(
                store.reserve_agent_call(now_ms() / 86_400_000, self.agent_daily_limit)?,
                "Cursor manager daily call limit reached"
            );
            if let Some(task_id) = task_id {
                store.task(task_id)?.and_then(|task| task.manager_session)
            } else if read_only {
                store.auto_manager_session()?
            } else {
                store.manager_session()?
            }
        };
        let snapshot = self.snapshot().await?;
        let outcome = agent::run(
            &config,
            prompt,
            snapshot,
            previous_session.as_deref(),
            |session_id| async move {
                let mut store = self.store.lock().await;
                if let Some(task_id) = task_id {
                    store.set_task_session(task_id, &session_id)
                } else if read_only {
                    store.set_auto_manager_session(&session_id)
                } else {
                    store.set_manager_session(&session_id)
                }
            },
            |progress| async move {
                self.store.lock().await.push_event(
                    "agent.progress",
                    None,
                    None,
                    json!({"command_id":command_id,"task_id":task_id,"progress":progress}),
                )?;
                self.changed.notify_waiters();
                Ok(())
            },
        )
        .await;
        match outcome {
            Ok(result) => {
                let output = json!({"command_id":command_id,"task_id":task_id,"session_id": result.session_id, "output": result.output, "usage": result.usage});
                let mut store = self.store.lock().await;
                store.push_event("agent.completed", None, None, output.clone())?;
                drop(store);
                self.changed.notify_waiters();
                Ok(output)
            }
            Err(error) => {
                let sdk_error = error.downcast_ref::<agent::AgentRunError>();
                let mut store = self.store.lock().await;
                store.push_event(
                    "agent.failed",
                    None,
                    None,
                    json!({"command_id":command_id,"task_id":task_id,"error":error.to_string(), "uncertain":sdk_error.is_some_and(|error| error.uncertain), "session_id":sdk_error.and_then(|error| error.session_id.as_deref())}),
                )?;
                drop(store);
                self.changed.notify_waiters();
                Err(error)
            }
        }
    }

    async fn collect_events(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Value>) -> Result<()> {
        while let Some(raw) = rx.recv().await {
            let kind = raw
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("codex.event");
            let mut payload = raw
                .get("params")
                .or_else(|| raw.get("data"))
                .cloned()
                .unwrap_or(raw.clone());
            let thread_id = string_field(&payload, &["threadId", "thread_id"]).or_else(|| {
                payload
                    .pointer("/thread/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
            let turn_id = string_field(&payload, &["turnId", "turn_id"]).or_else(|| {
                payload
                    .pointer("/turn/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
            if let Some(request_id) = raw.get("id")
                && let Some(object) = payload.as_object_mut()
            {
                object.insert("request_id".into(), request_id.clone());
            }
            self.store.lock().await.push_event(
                kind,
                thread_id.clone(),
                turn_id.clone(),
                payload,
            )?;
            self.changed.notify_waiters();
            if kind == "bridge.codex_connected" {
                let bridge = self.clone();
                tokio::spawn(async move {
                    if let Err(error) = bridge.restore_tracked_threads().await {
                        tracing::warn!(%error, "tracked Codex thread restore failed");
                    }
                });
            }
            if let Some(thread_id) = thread_id {
                if kind == "turn/started" {
                    self.active_turns.lock().await.insert(thread_id.clone());
                }
                if kind == "turn/completed" {
                    self.active_turns.lock().await.remove(&thread_id);
                    if self.auto_manage
                        && let Some(turn_id) = turn_id
                    {
                        let should_wake = {
                            let mut store = self.store.lock().await;
                            store.tracked_thread(&thread_id)?
                                && store.claim_auto_turn(&thread_id, &turn_id)?
                        };
                        if should_wake {
                            self.auto_pending.lock().await.insert(thread_id);
                            self.auto_changed.notify_one();
                        }
                    }
                }
            }
        }
        bail!("Codex event stream closed")
    }

    async fn restore_tracked_threads(&self) -> Result<()> {
        let threads = self.store.lock().await.tracked_threads()?;
        let mut reconciled = HashSet::new();
        for thread_id in threads {
            let result = {
                let _rpc = self.rpc_gate.lock().await;
                self.codex
                    .execute(&Command::ResumeThread {
                        thread_id: thread_id.clone(),
                    })
                    .await
            };
            let (kind, payload) = match result {
                Ok(output) => {
                    if matches!(
                        output
                            .pointer("/thread/status/type")
                            .and_then(Value::as_str),
                        Some("idle" | "active")
                    ) {
                        reconciled.insert(thread_id.clone());
                    }
                    match output
                        .pointer("/thread/status/type")
                        .and_then(Value::as_str)
                    {
                        Some("idle") => {
                            self.active_turns.lock().await.remove(&thread_id);
                        }
                        Some("active") => {
                            self.active_turns.lock().await.insert(thread_id.clone());
                        }
                        _ => {}
                    }
                    (
                        "bridge.codex_resubscribed",
                        json!({"thread_id": thread_id, "output": output}),
                    )
                }
                Err(error) => (
                    "bridge.codex_resubscribe_failed",
                    json!({"thread_id": thread_id, "uncertain": error.uncertain, "error": error.message}),
                ),
            };
            self.store
                .lock()
                .await
                .push_event(kind, Some(thread_id), None, payload)?;
            self.changed.notify_waiters();
        }
        let tasks = self.store.lock().await.tasks()?;
        for mut task in tasks {
            if task.status == TaskStatus::Running
                && task.auto_continue
                && !task.thread_ids.is_empty()
                && task
                    .thread_ids
                    .iter()
                    .all(|thread| reconciled.contains(thread))
                && !self.task_has_unknown(&task).await?
            {
                let _task_gate = self.task_gate.lock().await;
                let active = self.active_turns.lock().await;
                let idle = task
                    .thread_ids
                    .iter()
                    .all(|thread| !active.contains(thread));
                drop(active);
                if idle {
                    let mut store = self.store.lock().await;
                    task = store
                        .task(&task.id)?
                        .context("task disappeared during restore")?;
                    if task.status == TaskStatus::Running && task.auto_continue {
                        task.status = TaskStatus::NeedsContinue;
                        store.save_task_event(task, "task.reconciled", true)?;
                        self.changed.notify_waiters();
                    }
                }
            }
        }
        Ok(())
    }

    async fn auto_manager(self: Arc<Self>) -> Result<()> {
        loop {
            self.auto_changed.notified().await;
            tokio::time::sleep(Duration::from_secs(2)).await;
            let threads = std::mem::take(&mut *self.auto_pending.lock().await);
            if threads.is_empty() {
                continue;
            }
            let prompt = format!(
                "Review only these Bridge-managed Codex threads whose turns just ended: {}. Read their latest results. Return one concise result card per thread and say whether the user must intervene. This automatic invocation is for summaries only. Do not start or resume tasks, send or steer turns, interrupt execution, answer approvals, modify files, or operate on other threads.",
                serde_json::to_string(&threads)?
            );
            if let Err(error) = self
                .manage(
                    &prompt,
                    true,
                    None,
                    &format!("summary-{}", uuid::Uuid::new_v4()),
                )
                .await
            {
                tracing::warn!(%error, "automatic manager did not run successfully");
            }
        }
    }

    async fn serve_local(self: Arc<Self>, listener: UnixListener) -> Result<()> {
        loop {
            let (stream, _) = listener.accept().await?;
            let bridge = self.clone();
            tokio::spawn(async move {
                if let Err(error) = bridge.local_request(stream).await {
                    tracing::warn!(%error, "local request failed");
                }
            });
        }
    }

    async fn local_request(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let bytes =
            tokio::time::timeout(Duration::from_secs(10), read_request_line(&mut reader)).await??;
        let request: CommandRequest =
            serde_json::from_slice(&bytes).context("invalid local command JSON")?;
        let record = match self.submit(request.clone(), true).await {
            Ok(record) => record,
            Err(error) => {
                CommandRecord::new(request, CommandStatus::Failed).finish(Err(error.to_string()))
            }
        };
        write.write_all(&serde_json::to_vec(&record)?).await?;
        write.write_all(b"\n").await?;
        Ok(())
    }

    async fn serve_remote(self: Arc<Self>, config: RunConfig) -> Result<()> {
        let mut delay = 1;
        loop {
            let started = Instant::now();
            if let Err(error) = self.connect_server(&config).await {
                tracing::warn!(%error, "remote server disconnected");
            }
            if started.elapsed() > Duration::from_secs(30) {
                delay = 1;
            }
            tokio::time::sleep(Duration::from_secs(delay)).await;
            delay = (delay * 2).min(30);
        }
    }

    async fn connect_server(self: &Arc<Self>, config: &RunConfig) -> Result<()> {
        let mut request = config
            .server
            .as_deref()
            .context("remote server missing")?
            .into_client_request()?;
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!(
                "Bearer {}",
                config.token.as_deref().context("remote token missing")?
            ))?,
        );
        let ws_config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES));
        let (mut socket, _) = tokio::time::timeout(
            Duration::from_secs(15),
            connect_async_with_config(request, Some(ws_config), false),
        )
        .await??;
        send_frame(
            &mut socket,
            ToServer::Hello {
                version: VERSION,
                device_id: config.device_id.clone(),
            },
        )
        .await?;
        let welcome = tokio::time::timeout(Duration::from_secs(15), socket.next())
            .await?
            .context("server closed before welcome")??;
        let ToBridge::Welcome {
            version,
            last_event_seq,
            pending_command_ids,
        } = parse_server_message(welcome)?
        else {
            bail!("server did not send welcome");
        };
        ensure!(version == VERSION, "unsupported server protocol version");
        self.store.lock().await.align_ack(last_event_seq)?;
        let (mut sink, mut stream) = socket.split();
        for id in pending_command_ids {
            let record = self.store.lock().await.command(&id)?;
            let update = match record {
                Some(record) => ToServer::CommandUpdate { record: wire_record(record)? },
                None => ToServer::CommandUnknown { id, error: "command absent from local ledger; execution outcome is unknown and will not be replayed".into() },
            };
            send_frame(&mut sink, update).await?;
        }
        let mut event_cursor = last_event_seq;
        let mut update_cursor = 0;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        let mut scan = tokio::time::interval(Duration::from_millis(250));
        let mut last_received = Instant::now();
        loop {
            tokio::select! {
                message = stream.next() => {
                    let message = message.context("server connection closed")??;
                    last_received = Instant::now();
                    match message {
                        Message::Ping(bytes) => { send_message(&mut sink, Message::Pong(bytes)).await?; }
                        Message::Pong(_) => {}
                        Message::Close(_) => bail!("server closed connection"),
                        message => match parse_server_message(message)? {
                            ToBridge::Ack { seq } => self.store.lock().await.ack(seq)?,
                            ToBridge::Command { request } => {
                                if let Err(error) = self.submit(request.clone(), false).await {
                                    let record = CommandRecord::new(request, CommandStatus::Failed).finish(Err(error.to_string()));
                                    send_frame(&mut sink, ToServer::CommandUpdate { record }).await?;
                                }
                            }
                            ToBridge::Welcome { .. } => bail!("duplicate welcome"),
                        }
                    }
                }
                _ = heartbeat.tick() => {
                    ensure!(last_received.elapsed() < Duration::from_secs(45), "server heartbeat timed out");
                    send_frame(&mut sink, ToServer::Heartbeat).await?;
                    send_message(&mut sink, Message::Ping(Vec::new().into())).await?;
                }
                _ = self.changed.notified() => {}
                _ = scan.tick() => {}
            }
            let (events, updates) = {
                let store = self.store.lock().await;
                (
                    store.events_after(event_cursor, 16)?,
                    store.updates_after(update_cursor, 16)?,
                )
            };
            if events.len() == 16 || updates.len() == 16 {
                self.changed.notify_waiters();
            }
            for event in events {
                let seq = event.seq;
                let message = ToServer::Event {
                    event: wire_event(event)?,
                };
                send_frame(&mut sink, message).await?;
                event_cursor = seq;
            }
            for (seq, record) in updates {
                let message = ToServer::CommandUpdate {
                    record: wire_record(record)?,
                };
                send_frame(&mut sink, message).await?;
                update_cursor = seq;
            }
        }
    }
}

fn task_context(task: &TaskRecord) -> Result<String> {
    let full = serde_json::to_string(task)?;
    if full.len() <= 32_768 {
        return Ok(full);
    }
    Ok(json!({"id":task.id,"goal":task.goal.chars().take(4096).collect::<String>(),"cwd":task.cwd,"status":task.status,"context_truncated":true,"required_action":"Call task_read for this task before making any decision; this preview omits full goal, criteria and evidence"}).to_string())
}

#[derive(Debug)]
struct CodexFailure {
    uncertain: bool,
    message: String,
}
impl std::fmt::Display for CodexFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for CodexFailure {}

async fn ordinary_rpc_guard<'a>(
    gate: &'a Mutex<()>,
    command: &Command,
) -> Option<tokio::sync::MutexGuard<'a, ()>> {
    if matches!(
        command,
        Command::RespondRequest { .. } | Command::InterruptTurn { .. }
    ) {
        None
    } else {
        Some(gate.lock().await)
    }
}

async fn send_frame<S>(sink: &mut S, message: ToServer) -> Result<()>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let frame = serde_json::to_string(&message)?;
    ensure!(
        frame.len() <= MAX_FRAME_BYTES,
        "outbound server frame exceeds transport limit"
    );
    send_message(sink, Message::Text(frame.into())).await
}

async fn send_message<S>(sink: &mut S, message: Message) -> Result<()>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    tokio::time::timeout(Duration::from_secs(15), sink.send(message)).await??;
    Ok(())
}

fn string_field(value: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value.get(name).and_then(Value::as_str).map(str::to_owned))
}

fn parse_server_message(message: Message) -> Result<ToBridge> {
    let Message::Text(text) = message else {
        bail!("expected server JSON text frame");
    };
    Ok(serde_json::from_str(&text)?)
}

fn wire_event(mut event: BridgeEvent) -> Result<BridgeEvent> {
    let bytes = serde_json::to_vec(&event)?.len();
    if bytes > MAX_FRAME_BYTES - 1024 {
        event.payload = json!({
            "bridge_notice": "payload exceeds remote transport limit",
            "original_bytes": bytes,
            "full_payload_in_local_database": true
        });
    }
    Ok(event)
}

fn wire_record(mut record: CommandRecord) -> Result<CommandRecord> {
    let bytes = serde_json::to_vec(&record)?.len();
    if bytes > MAX_FRAME_BYTES - 1024 {
        record.output = Some(json!({
            "bridge_notice": "output exceeds remote transport limit",
            "original_bytes": bytes,
            "full_output_in_local_database": true
        }));
    }
    Ok(record)
}

async fn read_request_line<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        ensure!(
            !available.is_empty(),
            "local connection closed before command newline"
        );
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        ensure!(
            bytes.len() + count <= MAX_LOCAL_REQUEST_BYTES,
            "local command is too large"
        );
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(bytes);
        }
    }
}

fn validate_server(server: &str) -> Result<()> {
    let uri: Uri = server.parse().context("invalid server URI")?;
    ensure!(
        uri.path_and_query().and_then(|path| path.query()).is_none(),
        "server URL must not include query parameters"
    );
    ensure!(
        !uri.authority()
            .is_some_and(|authority| authority.as_str().contains('@')),
        "server URL must not contain credentials"
    );
    let host = uri.host().context("server URL has no host")?;
    let loopback = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    match uri.scheme_str() {
        Some("wss") => Ok(()),
        Some("ws") if loopback => Ok(()),
        _ => bail!("remote server requires wss; ws is allowed only for loopback"),
    }
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; specify all state paths")
}

fn default_state_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("vibeface"));
    }
    Ok(home_dir()?.join(".local/state/vibeface"))
}

struct SocketGuard {
    path: PathBuf,
    inode: u64,
    _lock: File,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.ino() == self.inode && metadata.file_type().is_socket())
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn bind_private_socket(path: &Path, state_dir: &Path) -> Result<(UnixListener, SocketGuard)> {
    let parent = path
        .parent()
        .context("control socket has no parent directory")?
        .canonicalize()?;
    ensure!(
        parent == state_dir,
        "control socket must be directly inside --state-dir"
    );
    let path = parent.join(path.file_name().context("invalid control socket path")?);
    let lock_path = state_dir.join("bridge.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(lock_path)?;
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    // One state directory must have one writer even when a different socket name is requested.
    if unsafe { flock(lock.as_raw_fd(), 2 | 4) } != 0 {
        return Err(io::Error::last_os_error()).context("another bridge owns the state directory");
    }
    let owner_path = path.with_extension("owner");
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        ensure!(
            metadata.file_type().is_socket(),
            "control socket path exists and is not a socket"
        );
        ensure!(
            UnixStream::connect(&path).await.is_err(),
            "control socket already has a live listener"
        );
        let owner = std::fs::read_to_string(&owner_path).unwrap_or_default();
        ensure!(
            owner == path.to_string_lossy(),
            "refusing to remove an unrecognized stale control socket"
        );
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    let inode = std::fs::symlink_metadata(&path)?.ino();
    let guard = SocketGuard {
        path: path.clone(),
        inode,
        _lock: lock,
    };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let mut owner = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(owner_path)?;
    std::io::Write::write_all(&mut owner, path.to_string_lossy().as_bytes())?;
    Ok((listener, guard))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approval_and_interrupt_bypass_a_blocked_ordinary_rpc() {
        let gate = Mutex::new(());
        let _blocked = gate.lock().await;
        let commands = [
            Command::RespondRequest {
                request_id: vibeface_protocol::RpcId::Number(1),
                response: json!({"decision":"decline"}),
            },
            Command::InterruptTurn {
                thread_id: "t".into(),
                turn_id: "u".into(),
            },
        ];
        for command in commands {
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(20),
                    ordinary_rpc_guard(&gate, &command)
                )
                .await
                .unwrap()
                .is_none()
            );
        }
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                ordinary_rpc_guard(&gate, &Command::ListThreads { cursor: None })
            )
            .await
            .is_err()
        );
    }

    #[test]
    fn remote_authentication_requires_tls_and_no_url_credentials() {
        assert!(validate_server("ws://127.0.0.1:8787/v1/bridge").is_ok());
        assert!(validate_server("ws://[::1]:8787/v1/bridge").is_ok());
        assert!(validate_server("wss://example.com/v1/bridge").is_ok());
        assert!(validate_server("ws://example.com/v1/bridge").is_err());
        assert!(validate_server("wss://example.com/v1/bridge?token=bad").is_err());
        assert!(validate_server("wss://user:secret@example.com/v1/bridge").is_err());
    }

    #[tokio::test]
    async fn local_requests_require_bounded_newline_frames() {
        let mut input = BufReader::new(&b"{\"id\":\"one\"}\nsecond\n"[..]);
        assert_eq!(
            read_request_line(&mut input).await.unwrap(),
            b"{\"id\":\"one\"}\n"
        );
        let oversized = vec![b'x'; MAX_LOCAL_REQUEST_BYTES + 1];
        assert!(
            read_request_line(&mut BufReader::new(oversized.as_slice()))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn socket_recovery_preserves_unknown_sockets_and_rejects_live_bridges() {
        let directory = std::env::temp_dir().join(format!(
            "vf-{}-{}",
            std::process::id(),
            &uuid::Uuid::new_v4().to_string()[..6]
        ));
        std::fs::create_dir(&directory).unwrap();
        let directory = directory.canonicalize().unwrap();
        let path = directory.join("control.sock");
        let (listener, guard) = bind_private_socket(&path, &directory).await.unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(bind_private_socket(&path, &directory).await.is_err());
        assert!(
            bind_private_socket(&directory.join("second.sock"), &directory)
                .await
                .is_err()
        );
        drop(listener);
        drop(guard);
        assert!(!path.exists());
        let unknown = directory.join("unknown.sock");
        drop(UnixListener::bind(&unknown).unwrap());
        assert!(bind_private_socket(&unknown, &directory).await.is_err());
        assert!(unknown.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn large_task_context_requires_a_full_tool_read_without_rejecting_valid_goal() {
        let task = TaskRecord {
            id: "task-a".into(),
            goal: "x".repeat(vibeface_protocol::MAX_TEXT_BYTES),
            cwd: "/tmp".into(),
            acceptance_criteria: vec!["verify output".into()],
            thread_ids: vec![],
            status: TaskStatus::Queued,
            auto_continue: true,
            remaining_decisions: 2,
            summary: None,
            evidence: None,
            dirty_seq: 1,
            handled_seq: 0,
            manager_session: None,
            updated_at_ms: 0,
        };
        let context = task_context(&task).unwrap();
        assert!(context.len() < 32_768);
        let value: Value = serde_json::from_str(&context).unwrap();
        assert_eq!(value["id"], "task-a");
        assert_eq!(value["context_truncated"], true);
        assert!(
            value["required_action"]
                .as_str()
                .unwrap()
                .contains("task_read")
        );
    }
}
