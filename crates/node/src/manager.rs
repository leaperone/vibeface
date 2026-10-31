//! Management SDKs receive visible context and explicit Node capabilities.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::sync::watch;
pub use vibeface_bridge::agent::{AgentFailure as ManagerFailure, StartEvidence};
use vibeface_bridge::{
    agent::{self, AgentResult, CustomToolConfig, SdkBridge, SdkTurn},
    codex::DirectSession,
    failure::{ExecFailure, FailureKind, ProviderStage},
    turn::{TurnStatus, turn_snapshot},
};

/// Codex accepts 65536 bytes of user input per message.
const MAX_PROMPT: usize = 65_536;

/// Stable owner of a resident Cursor bridge process and its private native store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CursorInstance {
    Bot {
        network: String,
        conversation: String,
    },
    Task {
        network: String,
        conversation: String,
        work_id: String,
    },
    Routine {
        network: String,
        conversation: String,
        trigger: String,
    },
    Work {
        network: String,
        conversation: String,
        root_work: String,
    },
    Coordination,
}

impl CursorInstance {
    pub fn key(&self) -> String {
        match self {
            Self::Bot {
                network,
                conversation,
            } => format!(
                "bot:{}:{network}:{}:{conversation}",
                network.len(),
                conversation.len()
            ),
            Self::Task {
                network,
                conversation,
                work_id,
            } => format!(
                "task:{}:{network}:{}:{conversation}:{}:{work_id}",
                network.len(),
                conversation.len(),
                work_id.len()
            ),
            Self::Routine {
                network,
                conversation,
                trigger,
            } => format!(
                "routine:{}:{network}:{}:{conversation}:{}:{trigger}",
                network.len(),
                conversation.len(),
                trigger.len()
            ),
            Self::Work {
                network,
                conversation,
                root_work,
            } => format!(
                "work:{}:{network}:{}:{conversation}:{}:{root_work}",
                network.len(),
                conversation.len(),
                root_work.len()
            ),
            Self::Coordination => "coordination".into(),
        }
    }
}

/// The SDK a turn runs in, with only what that SDK needs.
#[derive(Clone)]
pub enum Sdk {
    /// Each stable owner runs on its private resident bridge; the active attempt supplies the
    /// process-wide custom-tool callback bearer before Send.
    Cursor {
        bridge: Option<Arc<SdkBridge>>,
        sdk_bin: PathBuf,
        owner: Option<String>,
        local_store_root: Option<PathBuf>,
    },
    /// Turns run on this Node's Codex App Server at `socket`; `vibeface tool` reads the turn
    /// from the Shell environment the thread was created with.
    Codex {
        socket: PathBuf,
        launch: Option<Arc<std::sync::Mutex<crate::runtime::codex_server::CodexLease>>>,
    },
    /// Each turn is one `claude -p` process; `processes` records its group.
    Claude {
        bin: PathBuf,
        processes: Arc<crate::runtime::claude::Processes>,
    },
    /// Each turn runs its own `pi --mode rpc` process; see `runtime::pi`.
    Pi {
        bin: PathBuf,
        pi: Arc<crate::runtime::pi::Pi>,
    },
    /// Turns run on this Node's opencode serve; `vibeface tool` reads the turn from the tool
    /// environment the serve's plugin gives the session's shells.
    Opencode {
        server: Arc<crate::runtime::opencode::Opencode>,
        bin: PathBuf,
        launch: Option<Arc<crate::runtime::opencode::OpencodeLease>>,
    },
}

impl Sdk {
    pub fn provider(&self) -> crate::providers::Provider {
        match self {
            Sdk::Cursor { .. } => crate::providers::Provider::Cursor,
            Sdk::Codex { .. } => crate::providers::Provider::Codex,
            Sdk::Claude { .. } => crate::providers::Provider::Claude,
            Sdk::Pi { .. } => crate::providers::Provider::Pi,
            Sdk::Opencode { .. } => crate::providers::Provider::Opencode,
        }
    }
}

/// Records a turn's `(thread, turn)` on the App Server; the turn is `None` before `turn/start`.
pub type TurnRecorder = Arc<
    dyn Fn(String, Option<String>) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>
        + Send
        + Sync,
>;

/// How the Node finds a Codex turn again: its user message carries `client_id`, and `record`
/// keeps where it runs before anything can run in it.
#[derive(Clone)]
pub struct CodexTurn {
    pub client_id: String,
    pub record: TurnRecorder,
    pub started: Option<agent::RunStarted>,
    /// The prompt for a new thread, sent instead when the resumed thread is gone: the turn's
    /// own prompt then leaves out what only the old thread had seen.
    pub fresh_prompt: Option<String>,
}

#[derive(Clone)]
pub struct ManagerConfig {
    pub sdk: Sdk,
    pub workspace: PathBuf,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub timeout_secs: u64,
    pub custom_tools: BTreeMap<String, CustomToolConfig>,
    pub tool_cli: Option<PathBuf>,
    /// Guidance that holds for the bot's whole thread; see `bot_instructions`.
    pub instructions: String,
    pub tool_env: BTreeMap<String, String>,
    /// Lets this turn's tool calls act as it; without one they are refused.
    pub tool_token: Option<String>,
    /// The variables this Node set for the provider of `sdk`, over its own environment.
    pub provider_env: BTreeMap<String, String>,
    pub process_started: Option<agent::ProcessStarted>,
    pub codex_turn: Option<CodexTurn>,
}

impl ManagerConfig {
    pub(crate) fn runtime_fingerprint(&self, local: &crate::providers::Local) -> Result<String> {
        let launch = match &self.sdk {
            Sdk::Cursor {
                sdk_bin,
                owner,
                local_store_root,
                ..
            } => json!({"bin":sdk_bin,"owner":owner,"local_store_root":local_store_root}),
            Sdk::Codex { socket, launch } => {
                let lease = launch
                    .as_ref()
                    .map(|lease| lease.lock().expect("Codex lease"));
                let target = lease.as_ref().map(|lease| lease.target());
                match target {
                    Some(crate::providers::CodexTarget::System(path)) => {
                        json!({"mode":"system","socket":path,"agent_socket":socket})
                    }
                    Some(crate::providers::CodexTarget::Spawn(launch)) => {
                        json!({"mode":"spawn","bin":launch.bin,"agent_socket":socket})
                    }
                    _ => json!({"mode":"direct","socket":socket}),
                }
            }
            Sdk::Claude { bin, .. } | Sdk::Pi { bin, .. } => json!({"bin":bin}),
            Sdk::Opencode { bin, .. } => json!({"bin":bin}),
        };
        let environment_digest = crate::crypto::digest(&serde_json::to_vec(&self.provider_env)?);
        Ok(crate::crypto::digest(&serde_json::to_vec(&json!({
            "provider":self.sdk.provider(), "launch":launch,
            "workspace":self.workspace,"model":self.model,
            "environment_digest":environment_digest,
            "execution":local.switches.execution,
        }))?))
    }

    pub fn for_cursor_instance(
        mut self,
        instance: CursorInstance,
        local_store_root: PathBuf,
    ) -> Self {
        if let Sdk::Cursor {
            owner,
            local_store_root: root,
            ..
        } = &mut self.sdk
        {
            *owner = Some(instance.key());
            *root = Some(local_store_root);
        }
        self
    }
}

/// `outcome` is how certain the end is; `failure` why the turn failed.
#[derive(Debug)]
pub struct ManagerError {
    pub stage: ProviderStage,
    pub outcome: ManagerFailure,
    pub start: StartEvidence,
    pub session_id: Option<String>,
    pub failure: ExecFailure,
}
impl ManagerError {
    pub(crate) fn not_started(error: anyhow::Error) -> anyhow::Error {
        Self::not_started_at(ProviderStage::Validate, error)
    }
    pub(crate) fn not_started_at(stage: ProviderStage, error: anyhow::Error) -> anyhow::Error {
        Self {
            stage,
            outcome: ManagerFailure::Failed,
            start: StartEvidence::NotStarted,
            session_id: None,
            failure: ExecFailure::new(FailureKind::Unknown, format!("{error:#}")),
        }
        .into()
    }
    pub fn uncertain(&self) -> bool {
        self.outcome == ManagerFailure::Unknown
    }
    pub fn cancelled(&self) -> bool {
        self.outcome == ManagerFailure::Cancelled
    }
}
impl std::fmt::Display for ManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.failure.fmt(f)
    }
}
impl std::error::Error for ManagerError {}

pub async fn run_manager<F, P, PF>(
    config: &ManagerConfig,
    prompt: &str,
    previous_session_id: Option<&str>,
    persist_session: impl FnOnce(String) -> F,
    cancel: watch::Receiver<bool>,
    progress: P,
) -> Result<AgentResult>
where
    F: Future<Output = Result<()>>,
    P: FnMut(Value) -> PF,
    PF: Future<Output = Result<()>>,
{
    (|| -> Result<()> {
        ensure!(config.timeout_secs > 0, "manager timeout must be positive");
        ensure!(
            config.workspace.is_absolute() && config.workspace.is_dir(),
            "manager workspace must be an existing absolute directory"
        );
        ensure!(
            !prompt.trim().is_empty() && prompt.len() <= 65_536,
            "management prompt must be nonempty and at most 65536 bytes"
        );
        if let Some(id) = previous_session_id {
            vibeface_protocol::validate_id(id).map_err(anyhow::Error::msg)?;
        }
        config
            .sdk
            .provider()
            .validate_reasoning_effort(config.reasoning_effort.as_deref())?;
        Ok(())
    })()
    .map_err(ManagerError::not_started)?;
    if *cancel.borrow() {
        return Err(ManagerError {
            stage: ProviderStage::Validate,
            outcome: ManagerFailure::Cancelled,
            start: StartEvidence::NotStarted,
            session_id: previous_session_id.map(str::to_owned),
            failure: ExecFailure::new(
                FailureKind::Unknown,
                "management cancelled before SDK launch",
            ),
        }
        .into());
    }
    match &config.sdk {
        Sdk::Cursor {
            bridge,
            sdk_bin,
            owner,
            local_store_root,
        } => {
            let not_started = |stage, kind, detail: &str| ManagerError {
                stage,
                outcome: ManagerFailure::Failed,
                start: StartEvidence::NotStarted,
                session_id: previous_session_id.map(str::to_owned),
                failure: ExecFailure::new(kind, detail),
            };
            let bridge = bridge.as_ref().ok_or_else(|| {
                not_started(
                    ProviderStage::Launch,
                    FailureKind::MissingBinary,
                    "this Node has no Cursor SDK bridge yet",
                )
            })?;
            let (api_key, env) = crate::providers::cursor_launch(&config.provider_env);
            let api_key = api_key.ok_or_else(|| {
                not_started(
                    ProviderStage::Authentication,
                    FailureKind::MissingKey,
                    "CURSOR_API_KEY is required for the Cursor SDK manager",
                )
            })?;
            let owner = owner.as_deref().ok_or_else(|| {
                not_started(
                    ProviderStage::Validate,
                    FailureKind::Unknown,
                    "Cursor SDK manager has no stable process owner",
                )
            })?;
            let local_store_root = local_store_root.as_ref().ok_or_else(|| {
                not_started(
                    ProviderStage::Validate,
                    FailureKind::Unknown,
                    "Cursor SDK manager has no private native store",
                )
            })?;
            let native_agent = Arc::new(std::sync::Mutex::new(None::<String>));
            let record = config.codex_turn.as_ref().map(|turn| turn.record.clone());
            let started = config
                .codex_turn
                .as_ref()
                .and_then(|turn| turn.started.clone());
            let record_for_start = record.clone();
            let started_for_start = started.clone();
            let run_started =
                (record_for_start.is_some() || started_for_start.is_some()).then(|| {
                    let native_agent = native_agent.clone();
                    let record = record_for_start;
                    let started = started_for_start;
                    let callback: agent::RunStarted = Arc::new(move |run| {
                        let native_agent = native_agent
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clone();
                        let record = record.clone();
                        let started = started.clone();
                        Box::pin(async move {
                            if let Some(record) = record {
                                record(
                                    native_agent.context("Cursor run has no persisted agent ID")?,
                                    Some(run.clone()),
                                )
                                .await?;
                            }
                            if let Some(started) = started {
                                started(run).await?;
                            }
                            Ok(())
                        })
                    });
                    callback
                });
            let turn = SdkTurn {
                owner: owner.into(),
                executable: sdk_bin.clone(),
                workspace: config.workspace.clone(),
                local_store_root: local_store_root.clone(),
                api_key,
                env,
                model: config.model.clone(),
                timeout_secs: config.timeout_secs,
                custom_tools: config.custom_tools.clone(),
                callback_token: config.tool_token.clone(),
                process_started: config.process_started.clone(),
                run_started,
            };
            let result = bridge
                .run(
                    &turn,
                    prompt,
                    previous_session_id,
                    |id| {
                        *native_agent
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id.clone());
                        let locator_turn = turn.clone();
                        let record = record.clone();
                        async move {
                            persist_session(id.clone()).await?;
                            if let Some(native) = &config.codex_turn {
                                crate::runtime::record_cursor_reply(
                                    &locator_turn,
                                    &config.provider_env,
                                    &native.client_id,
                                    &id,
                                )?;
                            }
                            if let Some(record) = record {
                                record(id, None).await?;
                            }
                            Ok(())
                        }
                    },
                    cancel,
                    progress,
                )
                .await
                .map_err(|error| -> anyhow::Error {
                    ManagerError {
                        stage: error.stage,
                        outcome: error.outcome,
                        start: error.start,
                        session_id: error.session_id,
                        failure: error.failure,
                    }
                    .into()
                })?;
            Ok(result)
        }
        Sdk::Codex { socket, launch } => {
            if let (Some(lease), Some(turn)) = (launch, &config.codex_turn) {
                lease
                    .lock()
                    .expect("Codex lease")
                    .bind_turn(&turn.client_id)
                    .map_err(ManagerError::not_started)?;
            }
            let result = run_codex(
                config,
                socket,
                prompt,
                previous_session_id,
                persist_session,
                cancel,
                progress,
            )
            .await;
            if result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<ManagerError>())
                .is_some_and(ManagerError::uncertain)
                && let Some(lease) = launch
            {
                lease.lock().expect("Codex lease").retain_until_read_back();
            }
            result
        }
        Sdk::Claude { bin, processes } => {
            crate::runtime::claude::run_bot(
                config,
                bin,
                processes,
                prompt,
                previous_session_id,
                persist_session,
                cancel,
                progress,
            )
            .await
        }
        Sdk::Pi { bin, pi } => {
            pi.run_bot(
                config,
                bin,
                prompt,
                previous_session_id,
                persist_session,
                cancel,
                progress,
            )
            .await
        }
        Sdk::Opencode { server, .. } => {
            server
                .run_turn(
                    config,
                    prompt,
                    previous_session_id,
                    persist_session,
                    cancel,
                    progress,
                )
                .await
        }
    }
}

/// Stable Bot instructions and the Node tool catalog. The adapter uses native instructions
/// or a user-turn prefix.
/// Node tools go through the CLI in the bot's shell, whose environment carries the turn's scope.
pub fn bot_instructions(
    tool_cli: Option<&Path>,
    tools: &BTreeMap<String, CustomToolConfig>,
    instructions: &str,
) -> String {
    let Some(cli) = tool_cli else {
        return instructions.into();
    };
    let executable = format!("'{}'", cli.to_string_lossy().replace('\'', r#"'"'"'"#));
    let discovery = if tools.contains_key("tool_search") {
        "Only discovery tools and essential reads are listed initially. Use tool_search with a query to find a tool, tool_describe with {\"tool\":\"TOOL_NAME\"} to read its full schema, then tool_call with {\"tool\":\"TOOL_NAME\",\"arguments\":{}}. The described target's permissions and call_ref requirement still apply. You can also call a discovered tool directly through this scoped CLI; dispatch_work uses the CLI's work execution flow. "
    } else {
        ""
    };
    let catalog = tools
        .iter()
        .filter(|(name, _)| {
            crate::tools::Tool::parse(name).is_some_and(crate::tools::Tool::initially_exposed)
        })
        .map(|(name, spec)| {
            format!(
                "{name}\n{}\nJSON schema: {}",
                spec.description.as_deref().unwrap_or_default(),
                spec.input_schema
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let host = "Your shell has full access to this host: you can edit files and run git and gh. When this device runs Codex, `vibeface codex` calls its App Server. When a session asks something (an approval, a question, a login), `vibeface requests` lists the waiting requests and `vibeface respond REQUEST_ID 'RESULT_JSON'` answers one; raise a decision card with `vibeface triage raise --request-id` only when the user must decide, explaining the decision in plain words without the command itself. Add `--attention-kind browser-takeover` when the user must continue in a browser on the event's device; omit it to infer question or approval from the held request. The card kind grants no authority.";
    let text = format!(
        "Node tools are available through your command-execution tool. Run:\n{executable} tool TOOL_NAME --json 'JSON_OBJECT'\n{discovery}For any tool whose schema requires call_ref, add --call-ref 'UNIQUE_INTENT_REFERENCE'. For tool_call the flag is passed to arguments.call_ref; when both are given they must agree. Choose the reference once before the first request and preserve it across CLI processes. Reuse that reference with identical arguments after a partial response. On a timeout, query call_lookup with {{\"call_ref\":\"THE_SAME_REFERENCE\"}}; never replace an uncertain call with a new reference. Different authorized intentions need different references.\nThe CLI uses its scoped loopback connection and prints JSON. Do not read or print credential variables. Report failed calls honestly. Conversation and network scope come from the CLI. {host}\n\nAvailable tools:\n{catalog}"
    );
    if instructions.is_empty() {
        text
    } else {
        format!("{text}\n\n{instructions}")
    }
}

/// Whether an App Server notification is about `turn` of `thread`.
fn of_turn(message: &Value, thread: &str, turn: &str) -> bool {
    let params = &message["params"];
    params["threadId"] == thread && (params["turnId"] == turn || params["turn"]["id"] == turn)
}

/// The reply text and the tools of a turn as the chat shows them; tool arguments, outputs,
/// reasoning and the user's own messages stay out.
fn codex_progress(message: &Value) -> Option<Value> {
    let method = message["method"].as_str()?;
    if method == "item/completed" && message["params"]["item"]["type"] == "contextCompaction" {
        return Some(json!({"type":"context_compacted"}));
    }
    let shown = match method {
        "item/agentMessage/delta" | "turn/completed" => true,
        "item/started" | "item/completed" => matches!(
            message["params"]["item"]["type"].as_str(),
            Some(
                "agentMessage"
                    | "commandExecution"
                    | "fileChange"
                    | "mcpToolCall"
                    | "dynamicToolCall"
                    | "webSearch"
            )
        ),
        _ => false,
    };
    shown.then(|| crate::runtime::native_progress(message))
}

/// The last answer of the turn the notifications described.
#[derive(Default)]
struct Answer(Vec<(String, String)>);

impl Answer {
    fn apply(&mut self, message: &Value) {
        let params = &message["params"];
        let (id, text) = match message["method"].as_str() {
            Some("item/agentMessage/delta") => {
                let (Some(id), Some(delta)) = (params["itemId"].as_str(), params["delta"].as_str())
                else {
                    return;
                };
                let previous = self.text(id);
                (id, previous + delta)
            }
            Some("item/completed") if params["item"]["type"] == "agentMessage" => {
                let (Some(id), Some(text)) = (
                    params["item"]["id"].as_str(),
                    params["item"]["text"].as_str(),
                ) else {
                    return;
                };
                (id, text.to_owned())
            }
            _ => return,
        };
        self.0.retain(|(item, _)| item != id);
        self.0.push((id.into(), text));
    }
    fn text(&self, id: &str) -> String {
        self.0
            .iter()
            .find(|(item, _)| item == id)
            .map(|(_, text)| text.clone())
            .unwrap_or_default()
    }
    fn last(&self) -> String {
        self.0
            .last()
            .map(|(_, text)| text.clone())
            .unwrap_or_default()
    }
}

/// The status of `turn` as the thread records it, `None` while it runs or when the thread
/// cannot be read.
async fn turn_status(session: &mut DirectSession, thread: &str, turn: &str) -> Option<TurnStatus> {
    let reply = session
        .request(
            "thread/read",
            json!({"threadId":thread,"includeTurns":true}),
        )
        .await
        .ok()?;
    reply["thread"]["turns"]
        .as_array()?
        .iter()
        .find(|t| t["id"] == turn)
        .and_then(|t| turn_snapshot(t).status)
        .filter(|status| *status != TurnStatus::InProgress)
}

/// Interrupts the turn and waits for it to end; true once Codex reports it ended.
async fn stop_turn(session: &mut DirectSession, thread: &str, turn: &str) -> bool {
    let _ = session
        .request("turn/interrupt", json!({"threadId":thread,"turnId":turn}))
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    loop {
        match tokio::time::timeout_at(deadline, session.notification()).await {
            Ok(Ok(message)) => {
                if message["method"] == "turn/completed" && of_turn(&message, thread, turn) {
                    return true;
                }
            }
            Ok(Err(_)) | Err(_) => return turn_status(session, thread, turn).await.is_some(),
        }
    }
}

/// One turn on this Node's Codex App Server. The thread is created with the Shell environment of
/// its tools; the turn's user message carries `client_id`, so an unknown `turn/start` is found
/// again by it and never sent twice.
async fn run_codex<F, P, PF>(
    config: &ManagerConfig,
    socket: &Path,
    prompt: &str,
    previous: Option<&str>,
    persist_session: impl FnOnce(String) -> F,
    mut cancel: watch::Receiver<bool>,
    mut progress: P,
) -> Result<AgentResult>
where
    F: Future<Output = Result<()>>,
    P: FnMut(Value) -> PF,
    PF: Future<Output = Result<()>>,
{
    ensure!(
        config.workspace.is_absolute() && config.workspace.is_dir(),
        "manager workspace must be an existing absolute directory"
    );
    let secrets = secret_values(config);
    let start = std::sync::Mutex::new(StartEvidence::NotStarted);
    let stage = vibeface_bridge::failure::ProviderTrace::new("codex", ProviderStage::Launch);
    let error = |outcome, session_id: Option<&str>, failure: ExecFailure| -> anyhow::Error {
        ManagerError {
            stage: stage.current(),
            outcome,
            start: *start
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            session_id: session_id.map(str::to_owned),
            failure: ExecFailure {
                detail: redact_string(failure.detail, &secrets),
                ..failure
            },
        }
        .into()
    };
    let failed = |session_id: Option<&str>, failure: ExecFailure| {
        error(ManagerFailure::Failed, session_id, failure)
    };
    // Faults of this Node's own bookkeeping, not of Codex.
    let own = |detail: String| ExecFailure::new(FailureKind::Unknown, detail);
    // A stop before the turn started ends the reply as stopped; nothing ran.
    let stopped = |session_id: Option<&str>| {
        error(
            ManagerFailure::Cancelled,
            session_id,
            own("stopped before the Codex turn started".into()),
        )
    };
    if *cancel.borrow() {
        return Err(stopped(previous));
    }
    let mut session = DirectSession::connect(socket).await.map_err(|e| {
        let failure = e.failure();
        failed(
            previous,
            ExecFailure {
                detail: format!("Codex App Server: {}", failure.detail),
                ..failure
            },
        )
    })?;
    stage.enter(ProviderStage::SessionStart);
    let mut env = config.tool_env.clone();
    if let Some(token) = &config.tool_token {
        env.insert("VIBEFACE_TOOL_TOKEN".into(), token.clone());
    }
    let mut thread_params = json!({"cwd":config.workspace,"approvalPolicy":"never",
        "sandbox":"danger-full-access","config":{"shell_environment_policy.set":env,"web_search":"live"}});
    if !config.model.is_empty() {
        thread_params["model"] = json!(config.model);
    }
    // Resuming sends them again: a thread Codex reloads rebuilds its context from them.
    let instructions = bot_instructions(
        config.tool_cli.as_deref(),
        &config.custom_tools,
        &config.instructions,
    );
    if !instructions.is_empty() {
        thread_params["developerInstructions"] = json!(instructions);
    }
    let resumed = match previous {
        Some(id) => {
            let mut params = thread_params.clone();
            params["threadId"] = json!(id);
            match session.request("thread/resume", params).await {
                Ok(_) => Some(id.to_owned()),
                Err(e) if e.uncertain => return Err(failed(previous, e.failure())),
                // Codex no longer has the thread; the conversation goes on in a new one.
                Err(_) => None,
            }
        }
        None => None,
    };
    let prompt = match (&resumed, previous, &config.codex_turn) {
        (
            None,
            Some(_),
            Some(CodexTurn {
                fresh_prompt: Some(fresh),
                ..
            }),
        ) => fresh.as_str(),
        _ => prompt,
    };
    let thread = match resumed {
        Some(id) => id,
        None => {
            thread_params["threadSource"] = json!("vibeface-bot");
            let started = session
                .request("thread/start", thread_params)
                .await
                .map_err(|e| failed(previous, e.failure()))?;
            let id = started["thread"]["id"]
                .as_str()
                .context("Codex started a thread without an ID")?
                .to_owned();
            vibeface_protocol::validate_id(&id).map_err(anyhow::Error::msg)?;
            id
        }
    };
    let native = Some(thread.as_str());
    stage.enter(ProviderStage::Record);
    if let Err(e) = persist_session(thread.clone()).await {
        // A stop fences the attempt, so saving its thread fails then.
        return Err(if *cancel.borrow() {
            stopped(native)
        } else {
            failed(native, own(format!("persist the Codex thread: {e}")))
        });
    }
    let client_id = match &config.codex_turn {
        Some(turn) => {
            (turn.record)(thread.clone(), None)
                .await
                .map_err(|e| failed(native, own(format!("record the Codex turn: {e}"))))?;
            turn.client_id.clone()
        }
        None => match &config.sdk {
            Sdk::Codex {
                launch: Some(lease),
                ..
            } => lease.lock().expect("Codex lease").client_id().to_owned(),
            _ => uuid::Uuid::new_v4().to_string(),
        },
    };
    let mut turn_params = json!({"threadId":thread,"clientUserMessageId":client_id,
        "input":[{"type":"text","text":prompt,"text_elements":[]}]});
    if !config.model.is_empty() {
        turn_params["model"] = json!(config.model);
    }
    if let Some(effort) = &config.reasoning_effort {
        turn_params["effort"] = json!(effort);
    }
    if *cancel.borrow() {
        return Err(stopped(native));
    }
    stage.enter(ProviderStage::TurnStart);
    *start.lock().unwrap() = StartEvidence::Unknown;
    let turn = match session.request("turn/start", turn_params).await {
        Ok(reply) => reply["turn"]["id"].as_str().map(str::to_owned),
        Err(e) if !e.uncertain => {
            *start.lock().unwrap() = StartEvidence::NotStarted;
            return Err(failed(native, e.failure()));
        }
        Err(e) => match session.read_back(&thread, &client_id).await {
            Ok(Some(turn)) => Some(turn),
            _ => return Err(error(ManagerFailure::Unknown, native, e.failure())),
        },
    }
    .context("Codex started a turn without an ID")
    .map_err(|e| error(ManagerFailure::Unknown, native, own(e.to_string())))?;
    *start.lock().unwrap() = StartEvidence::Started;
    stage.enter(ProviderStage::Record);
    if let Some(record) = &config.codex_turn
        && let Err(e) = (record.record)(thread.clone(), Some(turn.clone())).await
    {
        let stopped = stop_turn(&mut session, &thread, &turn).await;
        let kind = if stopped {
            ManagerFailure::Failed
        } else {
            ManagerFailure::Unknown
        };
        return Err(error(
            kind,
            native,
            own(format!("record the Codex turn: {e}")),
        ));
    }
    stage.enter(ProviderStage::Run);
    let mut answer = Answer::default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(config.timeout_secs);
    let outcome = loop {
        let message = tokio::select! {
            biased;
            _ = agent::wait_for_cancel(&mut cancel) => {
                stage.enter(ProviderStage::Interrupt);
                break if stop_turn(&mut session, &thread, &turn).await {
                    Err(error(ManagerFailure::Cancelled, native, own("Codex turn interrupted".into())))
                } else {
                    Err(error(ManagerFailure::Unknown, native, own("Codex did not confirm the interrupt".into())))
                };
            }
            _ = tokio::time::sleep_until(deadline) => {
                let kind = if stop_turn(&mut session, &thread, &turn).await {
                    ManagerFailure::Failed
                } else {
                    ManagerFailure::Unknown
                };
                break Err(error(kind, native, ExecFailure::new(FailureKind::TimedOut, "Codex management timed out")));
            }
            message = session.notification() => match message {
                Ok(message) => message,
                // The turn may go on without this connection; reconciliation reads it.
                Err(e) => break Err(error(ManagerFailure::Unknown, native, e.failure())),
            },
        };
        if !of_turn(&message, &thread, &turn) {
            continue;
        }
        let message = redact(message, &secrets);
        answer.apply(&message);
        if let Some(update) = codex_progress(&message) {
            progress(update)
                .await
                .map_err(|e| error(ManagerFailure::Unknown, native, own(e.to_string())))?;
        }
        if message["method"] == "turn/completed" {
            let ended = turn_snapshot(&message["params"]["turn"]);
            break match ended.status {
                Some(TurnStatus::Completed) => Ok(()),
                Some(TurnStatus::Interrupted) => {
                    let (outcome, failure) =
                        interrupted(&cancel, crate::providers::Provider::Codex);
                    Err(error(outcome, native, failure))
                }
                Some(TurnStatus::Failed) => Err(failed(
                    native,
                    ended
                        .error
                        .unwrap_or_else(|| own("Codex turn failed".into())),
                )),
                // As for a work, an end this build cannot read is one to reconcile.
                status => Err(error(
                    ManagerFailure::Unknown,
                    native,
                    ended.error.unwrap_or_else(|| {
                        own(format!(
                            "Codex ended the turn as {}",
                            status.as_ref().map_or("nothing", TurnStatus::as_str)
                        ))
                    }),
                )),
            };
        }
    };
    session.close().await;
    outcome?;
    let output = answer.last();
    ensure!(output.len() <= MAX_PROMPT, "Codex result is too large");
    Ok(AgentResult {
        session_id: Some(thread),
        output,
        usage: None,
    })
}
/// How a bot turn its provider reports interrupted ends: stopped when the Node asked it to stop,
/// else failed, as when the owner interrupted it in their own program on a shared App Server.
pub(crate) fn interrupted(
    cancel: &watch::Receiver<bool>,
    provider: crate::providers::Provider,
) -> (ManagerFailure, ExecFailure) {
    match *cancel.borrow() {
        true => (
            ManagerFailure::Cancelled,
            ExecFailure::new(
                FailureKind::Unknown,
                format!("{} turn was interrupted", provider.label()),
            ),
        ),
        false => (
            ManagerFailure::Failed,
            ExecFailure::new(
                FailureKind::Unknown,
                format!(
                    "{} interrupted the turn without being asked to stop",
                    provider.label()
                ),
            ),
        ),
    }
}

pub(crate) fn secret_values(config: &ManagerConfig) -> Vec<String> {
    let mut values: Vec<_> = config
        .tool_env
        .values()
        .chain(&config.tool_token)
        .chain(config.provider_env.values())
        .filter(|value| value.len() >= 8)
        .cloned()
        .collect();
    values.extend(
        std::env::vars()
            .filter(|(key, value)| {
                value.len() >= 8
                    && (key.ends_with("_KEY")
                        || key.ends_with("_TOKEN")
                        || key.ends_with("_SECRET")
                        || key.ends_with("_PASSWORD"))
            })
            .map(|(_, value)| value),
    );
    if let Ok(keys) = config.sdk.provider().key_variables(&config.provider_env) {
        values.extend(
            keys.iter()
                .filter_map(|key| std::env::var(key).ok())
                .filter(|value| !value.is_empty()),
        );
    }
    values
}
/// The variables the active model provider of Codex's configuration reads; `env` is what this
/// Node sets for Codex, including its Node-owned `CODEX_HOME`.
pub(crate) fn provider_environment_keys(env: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let home = env.get("CODEX_HOME").map(PathBuf::from);
    let Some(home) = home else {
        return Ok(vec![]);
    };
    let path = home.join("config.toml");
    if !path.exists() {
        return Ok(vec![]);
    }
    ensure!(
        std::fs::metadata(&path)?.len() <= 1_048_576,
        "Codex configuration is too large"
    );
    provider_keys_from_config(&std::fs::read_to_string(path)?)
}
fn provider_keys_from_config(source: &str) -> Result<Vec<String>> {
    let config: toml::Value =
        toml::from_str(source).context("invalid Codex provider configuration")?;
    let profile = config
        .get("profile")
        .and_then(toml::Value::as_str)
        .and_then(|p| config.get("profiles")?.get(p));
    let provider = profile
        .and_then(|p| p.get("model_provider"))
        .or_else(|| config.get("model_provider"))
        .and_then(toml::Value::as_str);
    let Some(provider) = provider.and_then(|p| config.get("model_providers")?.get(p)) else {
        return Ok(vec![]);
    };
    let mut keys = vec![];
    if let Some(key) = provider.get("env_key").and_then(toml::Value::as_str) {
        keys.push(key.to_owned());
    }
    if let Some(headers) = provider
        .get("env_http_headers")
        .and_then(toml::Value::as_table)
    {
        for value in headers.values() {
            keys.push(
                value
                    .as_str()
                    .context("invalid provider header environment key")?
                    .to_owned(),
            );
        }
    }
    ensure!(keys.len() <= 64, "too many provider environment keys");
    for key in &keys {
        ensure!(
            !key.is_empty()
                && key.len() <= 256
                && key.bytes().enumerate().all(|(i, c)| c == b'_'
                    || c.is_ascii_alphabetic()
                    || (i > 0 && c.is_ascii_digit())),
            "invalid provider environment key"
        );
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}
pub(crate) fn redact_string(mut text: String, secrets: &[String]) -> String {
    for secret in secrets.iter().filter(|value| !value.is_empty()) {
        text = text.replace(secret, "[redacted]");
    }
    text
}
pub(crate) fn redact(value: Value, secrets: &[String]) -> Value {
    match value {
        Value::String(text) => Value::String(redact_string(text, secrets)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| redact(item, secrets))
                .collect(),
        ),
        Value::Object(items) => Value::Object(
            items
                .into_iter()
                .map(|(key, value)| (key, redact(value, secrets)))
                .collect(),
        ),
        value => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn tool_guidance_discovers_targets_and_preserves_scoped_cli_references() {
        let tools =
            crate::providers::Provider::Codex.tools_for(crate::scheduling::TriggerKind::Human);
        let guide = bot_instructions(
            Some(Path::new("/opt/a'b/vibeface")),
            &tools,
            "standing rule",
        );
        assert!(guide.contains("'/opt/a'\"'\"'b/vibeface' tool TOOL_NAME"));
        assert!(guide.contains("tool_search with a query"));
        assert!(guide.contains("tool_describe with {\"tool\":\"TOOL_NAME\"}"));
        assert!(guide.contains("flag is passed to arguments.call_ref"));
        assert!(guide.contains("both are given they must agree"));
        assert!(guide.contains("dispatch_work uses the CLI's work execution flow"));
        assert!(guide.ends_with("standing rule"));
        let catalog = guide.split_once("Available tools:\n").unwrap().1;
        assert!(catalog.contains("tool_call\n"));
        assert!(catalog.contains("context\n"));
        assert!(catalog.contains("read_skill\n"));
        assert!(!catalog.contains("dispatch_work\n"));
        assert_eq!(
            bot_instructions(None, &tools, "standing rule"),
            "standing rule"
        );
    }

    fn codex(workspace: PathBuf, socket: PathBuf) -> ManagerConfig {
        ManagerConfig {
            sdk: Sdk::Codex {
                socket,
                launch: None,
            },
            workspace,
            model: String::new(),
            reasoning_effort: None,
            timeout_secs: 5,
            custom_tools: BTreeMap::new(),
            tool_cli: None,
            instructions: String::new(),
            tool_env: BTreeMap::new(),
            tool_token: None,
            provider_env: BTreeMap::new(),
            process_started: None,
            codex_turn: None,
        }
    }

    #[test]
    fn runtime_identity_uses_the_captured_sdk_and_private_environment() -> Result<()> {
        let mut config = codex(
            "/fixture/workspace".into(),
            "/fixture/app-server.sock".into(),
        );
        config
            .provider_env
            .insert("FIXTURE_MODEL_KEY".into(), "private-fixture-value".into());
        let mut local = crate::providers::Local {
            switches: Default::default(),
            bins: BTreeMap::new(),
            env: Default::default(),
        };
        let original = config.runtime_fingerprint(&local)?;
        assert_eq!(original.len(), 64);
        assert!(!original.contains("private-fixture-value"));
        local
            .bins
            .insert(crate::providers::Provider::Codex, "/another/codex".into());
        assert_eq!(
            config.runtime_fingerprint(&local)?,
            original,
            "the captured direct SDK still uses its original socket"
        );
        config
            .provider_env
            .insert("FIXTURE_MODEL_KEY".into(), "changed-fixture-value".into());
        let changed = config.runtime_fingerprint(&local)?;
        assert_ne!(original, changed);
        config.model = "fixture-model".into();
        assert_ne!(changed, config.runtime_fingerprint(&local)?);
        Ok(())
    }

    #[test]
    fn cursor_task_and_routine_instances_do_not_share_the_bot_or_work_store() {
        let network = "network".to_owned();
        let conversation = "conversation".to_owned();
        let instances = [
            CursorInstance::Bot {
                network: network.clone(),
                conversation: conversation.clone(),
            },
            CursorInstance::Task {
                network: network.clone(),
                conversation: conversation.clone(),
                work_id: "same".into(),
            },
            CursorInstance::Routine {
                network: network.clone(),
                conversation: conversation.clone(),
                trigger: "same".into(),
            },
            CursorInstance::Work {
                network,
                conversation,
                root_work: "same".into(),
            },
            CursorInstance::Coordination,
        ];
        let keys: std::collections::BTreeSet<_> = instances
            .into_iter()
            .map(|instance| instance.key())
            .collect();
        assert_eq!(keys.len(), 5);
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Turn {
        Answers,
        Runs {
            confirm: bool,
        },
        Drops,
        /// Codex ends the turn as this status on its own.
        Ends(&'static str),
    }

    struct Fake {
        socket: PathBuf,
        requests: Arc<std::sync::Mutex<Vec<Value>>>,
        _server: tokio::task::JoinHandle<()>,
    }

    impl Fake {
        fn calls(&self, method: &str) -> Vec<Value> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request["method"] == method)
                .map(|request| request["params"].clone())
                .collect()
        }
    }

    fn fake_app_server(turn: Turn) -> Fake {
        let socket = std::env::temp_dir().join(format!(
            "vf-manager-{}.sock",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requests.clone();
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut server = tokio_tungstenite::accept_async(stream).await.unwrap();
                    let mut status = "inProgress";
                    while let Some(Ok(message)) = server.next().await {
                        let Ok(request) = serde_json::from_slice::<Value>(&message.into_data())
                        else {
                            continue;
                        };
                        seen.lock().unwrap().push(request.clone());
                        let method = request["method"].as_str().unwrap_or_default();
                        if method == "turn/start" && turn == Turn::Drops {
                            return;
                        }
                        if method == "thread/resume" && request["params"]["threadId"] == "gone" {
                            let error = json!({"id":request["id"],"error":{"code":-32600,"message":"no rollout found"}});
                            let _ = server.send(Message::Text(error.to_string().into())).await;
                            continue;
                        }
                        let result = match method {
                            "thread/start" => json!({"thread":{"id":"thread-1"}}),
                            "turn/start" => json!({"turn":{"id":"turn-1"}}),
                            "thread/read" => {
                                json!({"thread":{"turns":[{"id":"turn-1","status":status}]}})
                            }
                            "initialized" => continue,
                            _ => json!({}),
                        };
                        let mut replies = vec![json!({"id":request["id"],"result":result})];
                        let note = |method: &str, params: Value| {
                            let mut params = params;
                            params["threadId"] = json!("thread-1");
                            params["turnId"] = json!("turn-1");
                            json!({"method":method,"params":params})
                        };
                        match (method, turn) {
                            ("turn/start", Turn::Answers) => replies.extend([
                                note("item/agentMessage/delta", json!({"itemId":"a","delta":"hel"})),
                                note("item/started", json!({"item":{"id":"u","type":"userMessage"}})),
                                note("item/started", json!({"item":{"id":"c","type":"commandExecution","command":"secret"}})),
                                note("item/completed", json!({"item":{"id":"a","type":"agentMessage","text":"hello"}})),
                                note("turn/completed", json!({"turn":{"id":"turn-1","status":"completed"}})),
                            ]),
                            ("turn/start", Turn::Ends(status)) => replies.push(note(
                                "turn/completed",
                                json!({"turn":{"id":"turn-1","status":status}}),
                            )),
                            ("turn/interrupt", Turn::Runs { confirm: true }) => {
                                status = "interrupted";
                                replies.push(note(
                                    "turn/completed",
                                    json!({"turn":{"id":"turn-1","status":"interrupted"}}),
                                ));
                            }
                            _ => {}
                        }
                        for reply in replies {
                            let _ = server.send(Message::Text(reply.to_string().into())).await;
                        }
                    }
                });
            }
        });
        Fake {
            socket,
            requests,
            _server: server,
        }
    }

    struct ConcurrentGate {
        expected: usize,
        entered: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
        released: std::sync::atomic::AtomicBool,
        all_entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    impl ConcurrentGate {
        fn new(expected: usize) -> Arc<Self> {
            Arc::new(Self {
                expected,
                entered: Default::default(),
                peak: Default::default(),
                released: Default::default(),
                all_entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            })
        }

        fn enter(&self) {
            let count = self
                .entered
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                + 1;
            self.peak
                .fetch_max(count, std::sync::atomic::Ordering::AcqRel);
            if count == self.expected {
                self.all_entered.notify_waiters();
            }
        }

        async fn wait_all(&self) {
            while self.entered.load(std::sync::atomic::Ordering::Acquire) < self.expected {
                let notified = self.all_entered.notified();
                if self.entered.load(std::sync::atomic::Ordering::Acquire) >= self.expected {
                    return;
                }
                notified.await;
            }
        }

        async fn wait_release(&self) {
            while !self.released.load(std::sync::atomic::Ordering::Acquire) {
                let notified = self.release.notified();
                if self.released.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                notified.await;
            }
        }

        fn release(&self) {
            self.released
                .store(true, std::sync::atomic::Ordering::Release);
            self.release.notify_waiters();
        }
    }

    struct ConcurrentFake {
        socket: PathBuf,
        requests: Arc<std::sync::Mutex<Vec<Value>>>,
        gate: Arc<ConcurrentGate>,
        server: tokio::task::JoinHandle<()>,
    }

    impl ConcurrentFake {
        fn new(expected: usize) -> Self {
            let socket = std::env::temp_dir().join(format!(
                "vf-manager-concurrent-{}.sock",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            ));
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let gate = ConcurrentGate::new(expected);
            let seen = requests.clone();
            let shared_gate = gate.clone();
            let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let server = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let seen = seen.clone();
                    let gate = shared_gate.clone();
                    let next = next.clone();
                    tokio::spawn(async move {
                        let Ok(mut server) = tokio_tungstenite::accept_async(stream).await else {
                            return;
                        };
                        let mut pending: Option<(String, String, String)> = None;
                        loop {
                            let message = if pending.is_some() {
                                tokio::select! {
                                    _ = gate.wait_release() => {
                                        let (thread, turn, prompt) = pending.take().unwrap();
                                        let status = if prompt.starts_with("fail ") {
                                            "failed"
                                        } else {
                                            "completed"
                                        };
                                        let item = json!({"type":"agentMessage","id":format!("item-{turn}"),
                                            "text":format!("done: {prompt}")});
                                        let mut notes = Vec::new();
                                        if status == "completed" {
                                            notes.push(json!({"method":"item/completed","params":{
                                                "threadId":thread,"turnId":turn,"item":item}}));
                                        }
                                        notes.push(json!({"method":"turn/completed","params":{
                                            "threadId":thread,"turnId":turn,
                                            "turn":{"id":turn,"status":status}}}));
                                        for note in notes {
                                            let _ = server.send(Message::Text(note.to_string().into())).await;
                                        }
                                        continue;
                                    }
                                    message = server.next() => match message {
                                        Some(Ok(message)) => message,
                                        _ => return,
                                    },
                                }
                            } else {
                                match server.next().await {
                                    Some(Ok(message)) => message,
                                    _ => return,
                                }
                            };
                            let Ok(request) = serde_json::from_slice::<Value>(&message.into_data())
                            else {
                                continue;
                            };
                            seen.lock().unwrap().push(request.clone());
                            let method = request["method"].as_str().unwrap_or_default();
                            let reply = |result: Value| json!({"id":request["id"],"result":result});
                            match method {
                                "initialized" => continue,
                                "thread/start" => {
                                    let id = format!(
                                        "thread-{}",
                                        next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                    );
                                    let response = reply(json!({"thread":{"id":id}}));
                                    let _ = server
                                        .send(Message::Text(response.to_string().into()))
                                        .await;
                                }
                                "turn/start" => {
                                    let thread = request["params"]["threadId"]
                                        .as_str()
                                        .unwrap_or_default()
                                        .to_owned();
                                    let turn = format!(
                                        "turn-{}",
                                        next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                    );
                                    let prompt = request["params"]["input"][0]["text"]
                                        .as_str()
                                        .unwrap_or_default()
                                        .to_owned();
                                    let response = reply(json!({"turn":{"id":turn}}));
                                    let _ = server
                                        .send(Message::Text(response.to_string().into()))
                                        .await;
                                    gate.enter();
                                    pending = Some((thread, turn, prompt));
                                }
                                "turn/interrupt" => {
                                    let response = reply(json!({}));
                                    let _ = server
                                        .send(Message::Text(response.to_string().into()))
                                        .await;
                                    let Some((thread, turn, _)) = pending.take() else {
                                        continue;
                                    };
                                    let note = json!({"method":"turn/completed","params":{
                                        "threadId":thread,"turnId":turn,
                                        "turn":{"id":turn,"status":"interrupted"}}});
                                    let _ =
                                        server.send(Message::Text(note.to_string().into())).await;
                                }
                                "thread/read" => {
                                    let response = reply(json!({"thread":{"turns":[]}}));
                                    let _ = server
                                        .send(Message::Text(response.to_string().into()))
                                        .await;
                                }
                                _ => {
                                    let response = reply(json!({}));
                                    let _ = server
                                        .send(Message::Text(response.to_string().into()))
                                        .await;
                                }
                            }
                        }
                    });
                }
            });
            Self {
                socket,
                requests,
                gate,
                server,
            }
        }

        async fn wait_all(&self) {
            self.gate.wait_all().await;
        }

        fn release(&self) {
            self.gate.release();
        }

        fn peak(&self) -> usize {
            self.gate.peak.load(std::sync::atomic::Ordering::Acquire)
        }

        async fn wait_for(&self, method: &str, count: usize) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if self
                        .requests
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|request| request["method"] == method)
                        .count()
                        >= count
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
        }
    }

    impl Drop for ConcurrentFake {
        fn drop(&mut self) {
            self.server.abort();
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    fn workspace() -> PathBuf {
        let directory = std::env::temp_dir().join(format!("vf-manager-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn provider_environment_uses_only_selected_profile_and_header_keys() -> Result<()> {
        let config = "model_provider='unused'\nprofile='local'\n[profiles.local]\nmodel_provider='active'\n[model_providers.active]\nenv_key='CUSTOM_API_KEY'\nenv_http_headers={X-Test='HEADER_TOKEN'}\n[model_providers.unused]\nenv_key='UNRELATED_SECRET'";
        assert_eq!(
            provider_keys_from_config(config)?,
            vec!["CUSTOM_API_KEY", "HEADER_TOKEN"]
        );
        assert!(
            provider_keys_from_config(
                "model_provider='p'\n[model_providers.p]\nenv_key='invalid-name'"
            )
            .is_err()
        );
        assert!(provider_keys_from_config("model='default'")?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn unsupported_cursor_effort_is_rejected_before_launch() {
        let mut config = codex(workspace(), PathBuf::from("/unused.sock"));
        config.sdk = Sdk::Cursor {
            bridge: None,
            sdk_bin: "/unused-cursor".into(),
            owner: None,
            local_store_root: None,
        };
        config.reasoning_effort = Some("high".into());
        let error = run_manager(
            &config,
            "hello",
            None,
            |_| async { panic!("must not persist") },
            watch::channel(false).1,
            |_| async { panic!("must not send") },
        )
        .await
        .err()
        .expect("expected failure");
        let error = error.downcast_ref::<ManagerError>().unwrap();
        assert_eq!(error.stage, ProviderStage::Validate);
        assert_eq!(error.start, StartEvidence::NotStarted);
        assert!(error.failure.detail.contains("effort"));
    }

    #[tokio::test]
    async fn a_turn_runs_on_the_app_server_and_is_recorded_before_it_starts() {
        let fake = fake_app_server(Turn::Answers);
        let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = recorded.clone();
        let config = ManagerConfig {
            model: "gpt-fixture".into(),
            reasoning_effort: Some("high".into()),
            tool_cli: Some("/opt/vibeface".into()),
            instructions: "fixed-thread-guide".into(),
            tool_token: Some("turn-token-secret".into()),
            codex_turn: Some(CodexTurn {
                client_id: "attempt-1".into(),
                started: None,
                record: Arc::new(move |thread, turn| {
                    record.lock().unwrap().push((thread, turn));
                    Box::pin(async { Ok(()) })
                }),
                fresh_prompt: None,
            }),
            ..codex(workspace(), fake.socket.clone())
        };
        let shown = Arc::new(std::sync::Mutex::new(Vec::new()));
        let progress = shown.clone();
        let result = run_manager(
            &config,
            "fixture",
            None,
            |id| async move {
                assert_eq!(id, "thread-1");
                Ok(())
            },
            watch::channel(false).1,
            move |event| {
                progress.lock().unwrap().push(event);
                async { Ok(()) }
            },
        )
        .await
        .unwrap();
        assert_eq!(
            (result.session_id.as_deref(), result.output.as_str()),
            (Some("thread-1"), "hello")
        );
        assert_eq!(
            *recorded.lock().unwrap(),
            [
                ("thread-1".to_owned(), None),
                ("thread-1".to_owned(), Some("turn-1".to_owned()))
            ]
        );
        let started = fake.calls("thread/start");
        assert_eq!(
            started[0]["config"]["shell_environment_policy.set"]["VIBEFACE_TOOL_TOKEN"],
            "turn-token-secret"
        );
        let guide = started[0]["developerInstructions"].as_str().unwrap();
        assert!(guide.contains("'/opt/vibeface' tool TOOL_NAME"));
        assert!(guide.ends_with("fixed-thread-guide"));
        let turn = &fake.calls("turn/start")[0];
        assert_eq!(turn["clientUserMessageId"], "attempt-1");
        assert_eq!(turn["model"], "gpt-fixture");
        assert_eq!(turn["effort"], "high");
        assert_eq!(
            turn["input"][0]["text"], "fixture",
            "the turn carries only its own prompt"
        );
        let shown = serde_json::to_string(&*shown.lock().unwrap()).unwrap();
        assert!(shown.contains("hello") && shown.contains("\"command\""));
        assert!(!shown.contains("secret") && !shown.contains("userMessage"));
    }

    #[tokio::test]
    async fn a_stopped_turn_is_cancelled_only_once_codex_confirms_it_ended() {
        for (confirm, expected) in [
            (true, ManagerFailure::Cancelled),
            (false, ManagerFailure::Unknown),
        ] {
            let fake = fake_app_server(Turn::Runs { confirm });
            let config = codex(workspace(), fake.socket.clone());
            let (stop, cancel) = watch::channel(false);
            let stopper = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                stop.send(true).unwrap();
                stop
            });
            let error = run_manager(
                &config,
                "fixture",
                Some("thread-1"),
                |_| async { Ok(()) },
                cancel,
                |_| async { Ok(()) },
            )
            .await
            .err()
            .unwrap();
            drop(stopper.await);
            let error = error.downcast_ref::<ManagerError>().unwrap();
            assert_eq!(
                (error.outcome, error.session_id.as_deref()),
                (expected, Some("thread-1"))
            );
            assert_eq!(fake.calls("thread/resume").len(), 1);
            assert_eq!(fake.calls("turn/interrupt").len(), 1);
        }
    }

    #[tokio::test]
    async fn a_turn_codex_ends_on_its_own_is_no_stop_and_an_unread_end_stays_unknown() {
        for (status, expected) in [
            ("interrupted", ManagerFailure::Failed),
            ("paused", ManagerFailure::Unknown),
            ("failed", ManagerFailure::Failed),
        ] {
            let fake = fake_app_server(Turn::Ends(status));
            let config = codex(workspace(), fake.socket.clone());
            let error = run_manager(
                &config,
                "fixture",
                Some("thread-1"),
                |_| async { Ok(()) },
                watch::channel(false).1,
                |_| async { Ok(()) },
            )
            .await
            .err()
            .unwrap();
            let error = error.downcast_ref::<ManagerError>().unwrap();
            assert_eq!(error.stage, ProviderStage::Run);
            assert_eq!(error.outcome, expected, "{status}: {error}");
            assert_eq!(
                error.start,
                StartEvidence::Started,
                "a terminal failure consumed the model turn"
            );
            assert!(fake.calls("turn/interrupt").is_empty(), "{status}");
        }
    }

    #[tokio::test]
    async fn a_new_thread_in_place_of_a_lost_one_gets_the_full_prompt() {
        let fake = fake_app_server(Turn::Answers);
        let config = ManagerConfig {
            codex_turn: Some(CodexTurn {
                client_id: "attempt-1".into(),
                started: None,
                record: Arc::new(|_, _| Box::pin(async { Ok(()) })),
                fresh_prompt: Some("full context".into()),
            }),
            ..codex(workspace(), fake.socket.clone())
        };
        let result = run_manager(
            &config,
            "delta only",
            Some("gone"),
            |_| async { Ok(()) },
            watch::channel(false).1,
            |_| async { Ok(()) },
        )
        .await
        .unwrap();
        assert_eq!(result.session_id.as_deref(), Some("thread-1"));
        assert_eq!(fake.calls("thread/start").len(), 1);
        assert_eq!(
            fake.calls("turn/start")[0]["input"][0]["text"],
            "full context"
        );
    }

    #[tokio::test]
    async fn an_unknown_turn_start_is_not_sent_again() {
        let fake = fake_app_server(Turn::Drops);
        let config = codex(workspace(), fake.socket.clone());
        let error = run_manager(
            &config,
            "fixture",
            None,
            |_| async { Ok(()) },
            watch::channel(false).1,
            |_| async { Ok(()) },
        )
        .await
        .err()
        .unwrap();
        assert!(error.downcast_ref::<ManagerError>().unwrap().uncertain());
        assert_eq!(
            error.downcast_ref::<ManagerError>().unwrap().start,
            StartEvidence::Unknown
        );
        assert_eq!(
            error.downcast_ref::<ManagerError>().unwrap().stage,
            ProviderStage::TurnStart
        );
        assert_eq!(fake.calls("turn/start").len(), 1);
    }

    fn concurrent_codex(fake: &ConcurrentFake, client: &str) -> ManagerConfig {
        let mut config = codex(workspace(), fake.socket.clone());
        config.timeout_secs = 30;
        config.codex_turn = Some(CodexTurn {
            client_id: client.into(),
            started: None,
            record: Arc::new(|_, _| Box::pin(async { Ok(()) })),
            fresh_prompt: None,
        });
        config
    }

    fn concurrent_codex_turns(
        fake: &ConcurrentFake,
        prompts: Vec<String>,
    ) -> Vec<tokio::task::JoinHandle<Result<AgentResult>>> {
        let mut tasks = Vec::with_capacity(prompts.len());
        for (index, prompt) in prompts.into_iter().enumerate() {
            let config = concurrent_codex(fake, &format!("client-{index}"));
            let cancel = watch::channel(false).1;
            tasks.push(tokio::spawn(async move {
                run_manager(
                    &config,
                    &prompt,
                    None,
                    |_| async { Ok(()) },
                    cancel,
                    |_| async { Ok(()) },
                )
                .await
            }));
        }
        tasks
    }

    #[tokio::test]
    async fn sixteen_codex_turns_overlap_and_keep_their_outputs() -> Result<()> {
        let fake = ConcurrentFake::new(16);
        let prompts: Vec<_> = (0..16).map(|i| format!("marker-{i}")).collect();
        let tasks = concurrent_codex_turns(&fake, prompts.clone());
        tokio::time::timeout(Duration::from_secs(10), fake.wait_all())
            .await
            .context("all Codex turns reached the fixture gate")?;
        assert_eq!(fake.peak(), 16);
        fake.release();
        let mut sessions = std::collections::HashSet::new();
        for (index, task) in tasks.into_iter().enumerate() {
            let result = task.await??;
            sessions.insert(result.session_id.context("Codex thread")?);
            assert_eq!(result.output, format!("done: marker-{index}"));
        }
        assert_eq!(sessions.len(), 16);
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_one_of_sixteen_codex_turns_leaves_the_others_running() -> Result<()> {
        let fake = ConcurrentFake::new(16);
        let prompts: Vec<_> = (0..16).map(|i| format!("marker-{i}")).collect();
        let mut tasks = Vec::with_capacity(16);
        let mut stops = Vec::with_capacity(16);
        for (index, prompt) in prompts.iter().enumerate() {
            let config = concurrent_codex(&fake, &format!("client-{index}"));
            let prompt = prompt.clone();
            let (stop, cancel) = watch::channel(false);
            stops.push(stop);
            tasks.push(tokio::spawn(async move {
                run_manager(
                    &config,
                    &prompt,
                    None,
                    |_| async { Ok(()) },
                    cancel,
                    |_| async { Ok(()) },
                )
                .await
            }));
        }
        tokio::time::timeout(Duration::from_secs(10), fake.wait_all())
            .await
            .context("all Codex turns reached the fixture gate")?;
        stops[0].send(true)?;
        fake.wait_for("turn/interrupt", 1).await;
        fake.release();
        for (index, task) in tasks.into_iter().enumerate() {
            let result = task.await?;
            if index == 0 {
                let error = result.err().context("cancelled Codex turn")?;
                assert!(
                    error
                        .downcast_ref::<ManagerError>()
                        .context("manager error")?
                        .cancelled()
                );
            } else {
                assert_eq!(result?.output, format!("done: marker-{index}"));
            }
        }
        assert_eq!(
            fake.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request["method"] == "turn/interrupt")
                .count(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn one_failed_codex_turn_does_not_stop_the_other_fifteen() -> Result<()> {
        let fake = ConcurrentFake::new(16);
        let prompts: Vec<_> = (0..16)
            .map(|i| {
                if i == 0 {
                    "fail marker-0".into()
                } else {
                    format!("marker-{i}")
                }
            })
            .collect();
        let tasks = concurrent_codex_turns(&fake, prompts);
        tokio::time::timeout(Duration::from_secs(10), fake.wait_all())
            .await
            .context("all Codex turns reached the fixture gate")?;
        fake.release();
        for (index, task) in tasks.into_iter().enumerate() {
            let result = task.await?;
            if index == 0 {
                let error = result.err().context("failed Codex turn")?;
                assert_eq!(
                    error
                        .downcast_ref::<ManagerError>()
                        .context("manager error")?
                        .outcome,
                    ManagerFailure::Failed
                );
            } else {
                assert_eq!(result?.output, format!("done: marker-{index}"));
            }
        }
        Ok(())
    }

    #[test]
    fn tool_credentials_are_redacted() {
        let config = ManagerConfig {
            tool_env: BTreeMap::from([("TOKEN".into(), "other-secret".into())]),
            tool_token: Some("turn-token-secret".into()),
            ..codex(".".into(), "app-server.sock".into())
        };
        assert_eq!(
            redact(
                json!({"nested":["other-secret","turn-token-secret"]}),
                &secret_values(&config)
            ),
            json!({"nested":["[redacted]","[redacted]"]})
        );
    }
}
