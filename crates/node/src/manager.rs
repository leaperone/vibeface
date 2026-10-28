//! Management SDKs receive visible context and explicit Node capabilities.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, watch};
pub use vibeface_bridge::agent::AgentFailure as ManagerFailure;
use vibeface_bridge::{
    agent::{self, AgentResult, CustomToolConfig, SdkBridge, SdkTurn},
    codex::DirectSession,
    failure::{ExecFailure, FailureKind},
    turn::{TurnStatus, turn_snapshot},
};

/// Codex accepts 65536 bytes of user input per message.
const MAX_PROMPT: usize = 65_536;

/// The SDK a turn runs in, with only what that SDK needs.
#[derive(Clone)]
pub enum Sdk {
    /// Turns share the Node's resident bridge. A tool callback names its agent, so the turn's
    /// tool token is filed under the agent's ID in `agents` once the SDK reports it.
    Cursor {
        bridge: Option<Arc<SdkBridge>>,
        sdk_bin: PathBuf,
        agents: Arc<Mutex<HashMap<String, String>>>,
    },
    /// Turns run on this Node's Codex App Server at `socket`; `vibeface tool` reads the turn
    /// from the Shell environment the thread was created with.
    Codex { socket: PathBuf },
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
}

impl Sdk {
    pub fn provider(&self) -> crate::providers::Provider {
        match self {
            Sdk::Cursor { .. } => crate::providers::Provider::Cursor,
            Sdk::Codex { .. } => crate::providers::Provider::Codex,
            Sdk::Claude { .. } => crate::providers::Provider::Claude,
            Sdk::Pi { .. } => crate::providers::Provider::Pi,
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
    /// The prompt for a new thread, sent instead when the resumed thread is gone: the turn's
    /// own prompt then leaves out what only the old thread had seen.
    pub fresh_prompt: Option<String>,
}

#[derive(Clone)]
pub struct ManagerConfig {
    pub sdk: Sdk,
    pub workspace: PathBuf,
    pub model: String,
    pub timeout_secs: u64,
    pub custom_tools: BTreeMap<String, CustomToolConfig>,
    pub tool_cli: Option<PathBuf>,
    /// Guidance that holds for the whole Codex thread; see `codex_instructions`.
    pub instructions: String,
    pub tool_env: BTreeMap<String, String>,
    /// Lets this turn's tool calls act as it; without one they are refused.
    pub tool_token: Option<String>,
    /// The variables this Node set for the provider of `sdk`, over its own environment.
    pub provider_env: BTreeMap<String, String>,
    pub process_started: Option<agent::ProcessStarted>,
    pub codex_turn: Option<CodexTurn>,
}

/// `outcome` is how certain the end is; `failure` why the turn failed.
#[derive(Debug)]
pub struct ManagerError {
    pub outcome: ManagerFailure,
    pub session_id: Option<String>,
    pub failure: ExecFailure,
}
impl ManagerError {
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
    ensure!(config.timeout_secs > 0, "manager timeout must be positive");
    ensure!(
        !prompt.trim().is_empty() && prompt.len() <= 65_536,
        "management prompt must be nonempty and at most 65536 bytes"
    );
    if let Some(id) = previous_session_id {
        vibeface_protocol::validate_id(id).map_err(anyhow::Error::msg)?;
    }
    if *cancel.borrow() {
        return Err(ManagerError {
            outcome: ManagerFailure::Cancelled,
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
            agents,
        } => {
            let not_started = |kind, detail: &str| ManagerError {
                outcome: ManagerFailure::Failed,
                session_id: previous_session_id.map(str::to_owned),
                failure: ExecFailure::new(kind, detail),
            };
            let bridge = bridge.as_ref().ok_or_else(|| {
                not_started(
                    FailureKind::MissingBinary,
                    "this Node has no Cursor SDK bridge yet",
                )
            })?;
            let (api_key, env) = crate::providers::cursor_launch(&config.provider_env);
            let api_key = api_key.ok_or_else(|| {
                not_started(
                    FailureKind::MissingKey,
                    "CURSOR_API_KEY is required for the Cursor SDK manager",
                )
            })?;
            let turn = SdkTurn {
                executable: sdk_bin.clone(),
                workspace: config.workspace.clone(),
                api_key,
                env,
                model: config.model.clone(),
                timeout_secs: config.timeout_secs,
                custom_tools: config.custom_tools.clone(),
                process_started: config.process_started.clone(),
            };
            let (agents, token) = (agents.clone(), config.tool_token.clone());
            let persist_session = move |id: String| async move {
                persist_session(id.clone()).await?;
                if let Some(token) = token {
                    agents.lock().await.insert(id, token);
                }
                Ok(())
            };
            bridge
                .run(
                    &turn,
                    prompt,
                    previous_session_id,
                    persist_session,
                    cancel,
                    progress,
                )
                .await
                .map_err(|error| {
                    ManagerError {
                        outcome: error.outcome,
                        session_id: error.session_id,
                        failure: error.failure,
                    }
                    .into()
                })
        }
        Sdk::Codex { socket } => {
            run_codex(
                config,
                socket,
                prompt,
                previous_session_id,
                persist_session,
                cancel,
                progress,
            )
            .await
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
    }
}

/// The developer instructions a Codex thread starts and resumes with: how to reach Node tools,
/// their catalog, then `instructions`. Codex keeps them with the thread, so turns carry only
/// their own prompt. Node tools go through the CLI in its Shell, whose environment carries the
/// turn's scope.
pub fn codex_instructions(
    tool_cli: Option<&Path>,
    tools: &BTreeMap<String, CustomToolConfig>,
    instructions: &str,
) -> String {
    let Some(cli) = tool_cli else {
        return instructions.into();
    };
    let executable = format!("'{}'", cli.to_string_lossy().replace('\'', r#"'"'"'"#));
    let with_call_ref = tools
        .iter()
        .filter(|(_, spec)| {
            spec.input_schema["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|name| name == "call_ref"))
        })
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let catalog = tools
        .iter()
        .map(|(name, spec)| {
            format!(
                "{name}\n{}\nJSON schema: {}",
                spec.description.as_deref().unwrap_or_default(),
                spec.input_schema
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let host = "Your shell has full access to this host: you can edit files, run git and gh, and call this device's Codex App Server with `vibeface codex`. When a session asks something (an approval, a question, a login), `vibeface requests` lists the waiting requests and `vibeface respond REQUEST_ID 'RESULT_JSON'` answers one; raise a decision card with `vibeface triage raise --request-id` only when the user must decide, explaining the decision in plain words without the command itself.";
    let text = format!(
        "Node tools are available through your command-execution tool. Run:\n{executable} tool TOOL_NAME --json 'JSON_OBJECT'\nFor {with_call_ref}, add --call-ref 'UNIQUE_INTENT_REFERENCE'. Choose the reference once before the first request and preserve it across CLI processes. Reuse that reference with identical arguments after a partial response. On a timeout, query call_lookup with {{\"call_ref\":\"THE_SAME_REFERENCE\"}}; never replace an uncertain call with a new reference. Different authorized intentions need different references.\nThe CLI uses its scoped loopback connection and prints JSON. Do not read or print credential variables. Report failed calls honestly. Conversation and network scope come from the CLI. {host}\n\nAvailable tools:\n{catalog}"
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
    let error = |outcome, session_id: Option<&str>, failure: ExecFailure| -> anyhow::Error {
        ManagerError {
            outcome,
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
    let instructions = codex_instructions(
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
        None => uuid::Uuid::new_v4().to_string(),
    };
    let mut turn_params = json!({"threadId":thread,"clientUserMessageId":client_id,
        "input":[{"type":"text","text":prompt,"text_elements":[]}]});
    if !config.model.is_empty() {
        turn_params["model"] = json!(config.model);
    }
    if *cancel.borrow() {
        return Err(stopped(native));
    }
    let turn = match session.request("turn/start", turn_params).await {
        Ok(reply) => reply["turn"]["id"].as_str().map(str::to_owned),
        Err(e) if !e.uncertain => return Err(failed(native, e.failure())),
        Err(e) => match session.read_back(&thread, &client_id).await {
            Ok(Some(turn)) => Some(turn),
            _ => return Err(error(ManagerFailure::Unknown, native, e.failure())),
        },
    }
    .context("Codex started a turn without an ID")
    .map_err(|e| error(ManagerFailure::Unknown, native, own(e.to_string())))?;
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
    let mut answer = Answer::default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(config.timeout_secs);
    let outcome = loop {
        let message = tokio::select! {
            biased;
            _ = agent::wait_for_cancel(&mut cancel) => {
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
                Some(TurnStatus::Interrupted) => Err(error(
                    ManagerFailure::Cancelled,
                    native,
                    own("Codex turn was interrupted".into()),
                )),
                _ => Err(failed(
                    native,
                    ended
                        .error
                        .unwrap_or_else(|| own("Codex turn failed".into())),
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
/// Node sets for Codex, which may point it at another `CODEX_HOME`.
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

    fn codex(workspace: PathBuf, socket: PathBuf) -> ManagerConfig {
        ManagerConfig {
            sdk: Sdk::Codex { socket },
            workspace,
            model: String::new(),
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

    #[derive(Clone, Copy, PartialEq)]
    enum Turn {
        Answers,
        Runs { confirm: bool },
        Drops,
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
    async fn a_turn_runs_on_the_app_server_and_is_recorded_before_it_starts() {
        let fake = fake_app_server(Turn::Answers);
        let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = recorded.clone();
        let config = ManagerConfig {
            model: "gpt-fixture".into(),
            tool_cli: Some("/opt/vibeface".into()),
            instructions: "fixed-thread-guide".into(),
            tool_token: Some("turn-token-secret".into()),
            codex_turn: Some(CodexTurn {
                client_id: "attempt-1".into(),
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
    async fn a_new_thread_in_place_of_a_lost_one_gets_the_full_prompt() {
        let fake = fake_app_server(Turn::Answers);
        let config = ManagerConfig {
            codex_turn: Some(CodexTurn {
                client_id: "attempt-1".into(),
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
        assert_eq!(fake.calls("turn/start").len(), 1);
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
