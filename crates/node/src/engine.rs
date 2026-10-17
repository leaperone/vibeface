//! Local execution transports. Durable admission and authorization belong to the Node runtime.
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::{Mutex, mpsc};
use vibeface_bridge::codex::{CodexClient, CodexError};
use vibeface_protocol::{Command, RpcId};

#[path = "engine_isolation.rs"]
mod isolation;
#[path = "engine_process.rs"]
mod process;
use isolation::DockerIsolation;
pub use isolation::{ExecutionContext, ExecutionEgressConfig, ExecutionIsolationConfig};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    Codex,
    ClaudeCode,
    CursorAgent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorConfig {
    pub codex_socket: Option<PathBuf>,
    pub claude_bin: PathBuf,
    pub cursor_bin: PathBuf,
    pub timeout_secs: u64,
    #[serde(default)]
    pub execution_isolation: Option<ExecutionIsolationConfig>,
    #[serde(skip)]
    pub data_dir: Option<PathBuf>,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            codex_socket: None,
            claude_bin: "claude".into(),
            cursor_bin: "cursor-agent".into(),
            timeout_secs: 600,
            execution_isolation: None,
            data_dir: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorSession {
    pub executor: ExecutorKind,
    pub cwd: PathBuf,
    pub native_session_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EngineAction {
    Open {},
    Read {},
    Send { text: String },
    Steer { turn_id: String, text: String },
    Interrupt { turn_id: String },
    RespondRequest { request_id: RpcId, response: Value },
}

impl EngineAction {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Send { text } | Self::Steer { text, .. } => {
                ensure!(
                    !text.trim().is_empty() && text.len() <= 65_536,
                    "text must be nonempty and at most 65536 bytes"
                );
            }
            Self::RespondRequest {
                request_id,
                response,
            } => {
                if let RpcId::String(id) = request_id {
                    vibeface_protocol::validate_id(id).map_err(anyhow::Error::msg)?;
                }
                ensure!(
                    serde_json::to_vec(response)?.len() <= 65_536,
                    "approval response is too large"
                );
            }
            _ => {}
        }
        if let Self::Steer { turn_id, .. } | Self::Interrupt { turn_id } = self {
            vibeface_protocol::validate_id(turn_id).map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct EngineResult {
    pub native_session_id: Option<String>,
    pub result: Value,
}

#[derive(Debug)]
pub struct EngineError {
    pub uncertain: bool,
    pub native_session_id: Option<String>,
    pub message: String,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for EngineError {}

impl EngineError {
    pub(crate) fn rejected(session: &ExecutorSession, message: impl Into<String>) -> Self {
        Self {
            uncertain: false,
            native_session_id: session.native_session_id.clone(),
            message: message.into(),
        }
    }
}

#[derive(Clone)]
pub struct Engine {
    config: ExecutorConfig,
    codex: Arc<OnceLock<CodexClient>>,
    events: mpsc::UnboundedSender<Value>,
    isolation: Option<DockerIsolation>,
    codex_contexts: Arc<Mutex<BTreeMap<(Option<String>, String), ExecutionContext>>>,
}

impl Engine {
    pub fn new(config: ExecutorConfig, events: mpsc::UnboundedSender<Value>) -> Self {
        let codex_contexts = Arc::new(Mutex::new(BTreeMap::new()));
        let codex = Arc::new(OnceLock::new());
        let isolation = config
            .execution_isolation
            .clone()
            .zip(config.data_dir.clone())
            .map(|(settings, data_dir)| DockerIsolation::new(settings, data_dir));
        Self {
            config,
            codex,
            events,
            isolation,
            codex_contexts,
        }
    }

    fn personal_codex(&self) -> Result<&CodexClient> {
        let socket = self
            .config
            .codex_socket
            .clone()
            .context("personal Codex socket is not configured")?;
        Ok(self.codex.get_or_init(|| {
            CodexClient::start(
                socket,
                codex_events(self.events.clone(), self.codex_contexts.clone(), None),
            )
        }))
    }

    pub async fn snapshot(&self) -> Value {
        match self.codex.get() {
            Some(codex) => codex.snapshot().await,
            None => {
                json!({"connection":{"state":"unconfigured"},"threads":{},"pending_requests":[]})
            }
        }
    }

    pub(crate) async fn read_turn(&self, native_id: &str, turn_id: &str) -> Result<Option<Value>> {
        let codex = self.codex.get().context("Codex socket is not configured")?;
        Self::read_turn_from(codex, native_id, turn_id).await
    }

    pub async fn snapshot_authorized(&self, context: &ExecutionContext) -> Result<Value> {
        if context.shared {
            return Ok(self
                .isolation
                .as_ref()
                .context("shared execution isolation is not configured")?
                .existing_codex(context)
                .await?
                .snapshot()
                .await);
        }
        Ok(self.personal_codex()?.snapshot().await)
    }

    pub async fn read_turn_authorized(
        &self,
        context: &ExecutionContext,
        native_id: &str,
        turn_id: &str,
    ) -> Result<Option<Value>> {
        if context.shared {
            let codex = self
                .isolation
                .as_ref()
                .context("shared execution isolation is not configured")?
                .existing_codex(context)
                .await?;
            return Self::read_turn_from(&codex, native_id, turn_id).await;
        }
        self.personal_codex()?;
        self.read_turn(native_id, turn_id).await
    }

    async fn read_turn_from(
        codex: &CodexClient,
        native_id: &str,
        turn_id: &str,
    ) -> Result<Option<Value>> {
        vibeface_protocol::validate_id(native_id).map_err(anyhow::Error::msg)?;
        vibeface_protocol::validate_id(turn_id).map_err(anyhow::Error::msg)?;
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut cursor = None;
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..64 {
                let page = codex
                    .execute(&Command::ListTurns {
                        thread_id: native_id.into(),
                        cursor: cursor.clone(),
                    })
                    .await
                    .map_err(|error| anyhow::anyhow!(error.message))?;
                let turns = page["data"]
                    .as_array()
                    .context("Codex turn page has no data")?;
                if let Some(turn) = turns.iter().find(|turn| turn["id"] == turn_id) {
                    return Ok(Some(turn.clone()));
                }
                let Some(next) = page["nextCursor"].as_str() else {
                    return Ok(None);
                };
                ensure!(
                    seen.insert(next.to_owned()),
                    "Codex turn pagination repeated a cursor"
                );
                cursor = Some(next.to_owned());
            }
            anyhow::bail!("Codex turn lookup exceeded its page limit")
        })
        .await
        .context("Codex turn lookup timed out")?
    }

    pub async fn cancel_operation(&self, context: &ExecutionContext) -> Result<Vec<String>> {
        ensure!(
            context.shared,
            "host execution cannot confirm operation cancellation; preserve unknown state"
        );
        self.isolation
            .as_ref()
            .context("shared execution isolation is not configured")?
            .cancel(context)
            .await
    }

    pub async fn affected_operations(&self, context: &ExecutionContext) -> Vec<String> {
        match &self.isolation {
            Some(isolation) if context.shared => isolation.affected(context).await,
            _ => vec![context.operation_id.clone()],
        }
    }

    pub async fn complete_operation(&self, context: &ExecutionContext) -> Result<()> {
        if let Some(isolation) = &self.isolation {
            isolation.complete(context).await?;
        }
        self.codex_contexts.lock().await.retain(|_, bound| {
            bound.space_id != context.space_id
                || bound.resource_id != context.resource_id
                || bound.operation_id != context.operation_id
        });
        Ok(())
    }

    pub async fn renew_operation(
        &self,
        context: &ExecutionContext,
        lease_expires_at_ms: u64,
    ) -> Result<()> {
        ensure!(
            context.shared,
            "host execution has no isolated lease watchdog"
        );
        self.isolation
            .as_ref()
            .context("shared execution isolation is not configured")?
            .renew(context, lease_expires_at_ms)
            .await
    }

    pub async fn execute_authorized<F, Fut>(
        &self,
        context: &ExecutionContext,
        session: &ExecutorSession,
        action: EngineAction,
        persist_native: F,
    ) -> std::result::Result<EngineResult, EngineError>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        action
            .validate()
            .map_err(|error| EngineError::rejected(session, error.to_string()))?;
        if self.config.timeout_secs == 0 {
            return Err(EngineError::rejected(
                session,
                "executor timeout must be positive",
            ));
        }
        if !session.cwd.is_absolute() || !session.cwd.is_dir() {
            return Err(EngineError::rejected(
                session,
                "execution directory must be an existing absolute directory",
            ));
        }
        if let Some(id) = &session.native_session_id {
            vibeface_protocol::validate_id(id)
                .map_err(|error| EngineError::rejected(session, error))?;
        }
        let isolated = if context.shared {
            let isolation = self.isolation.as_ref().ok_or_else(|| {
                EngineError::rejected(session, "shared execution isolation is not configured")
            })?;
            if session.executor == ExecutorKind::CursorAgent
                && self
                    .config
                    .execution_isolation
                    .as_ref()
                    .is_some_and(|settings| settings.egress.is_some())
            {
                return Err(EngineError::rejected(
                    session,
                    "shared Cursor execution has no verified restricted model gateway transport",
                ));
            }
            Some(
                isolation
                    .prepare(context, &session.cwd)
                    .await
                    .map_err(|error| EngineError::rejected(session, error.to_string()))?,
            )
        } else {
            None
        };
        if session.executor != ExecutorKind::Codex {
            let result = process::execute(
                &self.config,
                session,
                action,
                persist_native,
                scoped_events(
                    self.events.clone(),
                    context.clone(),
                    isolated
                        .as_ref()
                        .map(|process| process.container_id().to_owned()),
                ),
                isolated.as_ref(),
            )
            .await;
            if context.shared {
                if result.as_ref().is_err_and(|error| error.uncertain) {
                    let _ = self.cancel_operation(context).await;
                } else {
                    if let Err(error) = self.complete_operation(context).await {
                        return Err(EngineError {
                            uncertain: true,
                            native_session_id: result
                                .as_ref()
                                .ok()
                                .and_then(|result| result.native_session_id.clone())
                                .or_else(|| session.native_session_id.clone()),
                            message: format!(
                                "isolated execution cleanup could not be confirmed: {error}"
                            ),
                        });
                    }
                }
            }
            return result;
        }
        let isolated_codex = if context.shared {
            Some(
                self.isolation
                    .as_ref()
                    .unwrap()
                    .codex(
                        context,
                        isolated.as_ref().unwrap(),
                        codex_events(
                            self.events.clone(),
                            self.codex_contexts.clone(),
                            Some(isolated.as_ref().unwrap().container_id().to_owned()),
                        ),
                    )
                    .await
                    .map_err(|error| EngineError::rejected(session, error.to_string()))?,
            )
        } else {
            None
        };
        let codex = match isolated_codex.as_ref() {
            Some(codex) => codex,
            None => self
                .personal_codex()
                .map_err(|error| EngineError::rejected(session, error.to_string()))?,
        };
        let container_id = isolated
            .as_ref()
            .map(|process| process.container_id().to_owned());
        if let Some(id) = &session.native_session_id {
            let mut bindings = self.codex_contexts.lock().await;
            let key = (container_id.clone(), id.clone());
            if let Some(bound) = bindings.get(&key) {
                ensure_same_host_scope(bound, context)
                    .map_err(|error| EngineError::rejected(session, error.to_string()))?;
            }
            if matches!(action, EngineAction::Send { .. }) || !bindings.contains_key(&key) {
                bindings.insert(key, context.clone());
            }
        }
        {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let state = codex.snapshot().await;
                    if state["connection"]["state"] == "connected" {
                        break;
                    }
                    if state["connection"]["state"] == "disconnected"
                        && state["connection"]["error"].is_string()
                    {
                        anyhow::bail!("isolated Codex failed to connect");
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
            .map_err(|_| EngineError::rejected(session, "isolated Codex initialization timed out"))?
            .map_err(|error| EngineError::rejected(session, error.to_string()))?;
        }
        let mut native = session.native_session_id.clone();
        let mut sent = false;
        let running_turn = matches!(action, EngineAction::Send { .. });
        let operation = async {
            if matches!(action, EngineAction::Open {}) && native.is_some() {
                return Err(EngineError::rejected(
                    session,
                    "execution session is already open",
                ));
            }
            let mut opened = None;
            if native.is_none()
                && matches!(action, EngineAction::Open {} | EngineAction::Send { .. })
            {
                let cwd = if context.shared {
                    Some("/project")
                } else {
                    session.cwd.to_str()
                }
                .ok_or_else(|| {
                    EngineError::rejected(session, "execution directory must be UTF-8")
                })?;
                sent = true;
                let reply = codex
                    .execute(&Command::StartThread { cwd: cwd.into() })
                    .await
                    .map_err(|error| codex_error(error, native.clone()))?;
                let id = reply["thread"]["id"].as_str().ok_or_else(|| EngineError {
                    uncertain: true,
                    native_session_id: None,
                    message: "Codex created a thread without returning its ID".into(),
                })?;
                vibeface_protocol::validate_id(id).map_err(|error| EngineError {
                    uncertain: true,
                    native_session_id: None,
                    message: error,
                })?;
                native = Some(id.into());
                persist_native(id.into())
                    .await
                    .map_err(|error| EngineError {
                        uncertain: true,
                        native_session_id: native.clone(),
                        message: format!("persist Codex session before send: {error}"),
                    })?;
                self.codex_contexts
                    .lock()
                    .await
                    .insert((container_id.clone(), id.into()), context.clone());
                opened = Some(reply);
                sent = false;
            }
            let thread_id = native
                .clone()
                .ok_or_else(|| EngineError::rejected(session, "execution session is not open"))?;
            let command = match action {
                EngineAction::Open {} => {
                    return Ok(EngineResult {
                        native_session_id: native.clone(),
                        result: opened.unwrap_or(json!({})),
                    });
                }
                EngineAction::Read {} => Command::ReadThread { thread_id },
                EngineAction::Send { text } => Command::StartTurn { thread_id, text },
                EngineAction::Steer { turn_id, text } => Command::SteerTurn {
                    thread_id,
                    turn_id,
                    text,
                },
                EngineAction::Interrupt { turn_id } => {
                    Command::InterruptTurn { thread_id, turn_id }
                }
                EngineAction::RespondRequest {
                    request_id,
                    response,
                } => {
                    let snapshot = codex.snapshot().await;
                    let belongs = snapshot["pending_requests"]
                        .as_array()
                        .is_some_and(|requests| {
                            requests.iter().any(|request| {
                                request.get("id") == Some(&json!(request_id))
                                    && request["params"]["threadId"].as_str()
                                        == Some(thread_id.as_str())
                            })
                        });
                    if !belongs {
                        return Err(EngineError::rejected(
                            session,
                            "approval request does not belong to this execution session",
                        ));
                    }
                    Command::RespondRequest {
                        request_id,
                        response,
                    }
                }
            };
            sent = !matches!(command, Command::ReadThread { .. });
            let result = codex
                .execute(&command)
                .await
                .map_err(|error| codex_error(error, native.clone()))?;
            Ok(EngineResult {
                native_session_id: native.clone(),
                result,
            })
        };
        let result =
            match tokio::time::timeout(Duration::from_secs(self.config.timeout_secs), operation)
                .await
            {
                Ok(result) => result,
                Err(_) => Err(EngineError {
                    uncertain: sent,
                    native_session_id: native.clone(),
                    message:
                        "Codex command timed out; reconcile its native session before retrying"
                            .into(),
                }),
            };
        if context.shared {
            if result.as_ref().is_err_and(|error| error.uncertain) {
                let _ = self.cancel_operation(context).await;
            } else if !running_turn || result.is_err() {
                if let Err(error) = self.complete_operation(context).await {
                    return Err(EngineError {
                        uncertain: true,
                        native_session_id: native,
                        message: format!(
                            "isolated execution cleanup could not be confirmed: {error}"
                        ),
                    });
                }
            }
        }
        result
    }
}

fn ensure_same_host_scope(left: &ExecutionContext, right: &ExecutionContext) -> Result<()> {
    ensure!(
        left.space_id == right.space_id && left.resource_id == right.resource_id,
        "native host session is bound to a different space or resource"
    );
    Ok(())
}

fn scoped_event(event: &Value, context: &ExecutionContext, container_id: Option<&str>) -> Value {
    json!({"execution_context":{"space_id":context.space_id,"resource_id":context.resource_id,"operation_id":context.operation_id,"container_id":container_id},"event":event})
}

fn codex_events(
    output: mpsc::UnboundedSender<Value>,
    contexts: Arc<Mutex<BTreeMap<(Option<String>, String), ExecutionContext>>>,
    container_id: Option<String>,
) -> mpsc::UnboundedSender<Value> {
    let (events, mut incoming) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        while let Some(event) = incoming.recv().await {
            let contexts = contexts.lock().await;
            let thread = event["params"]["threadId"]
                .as_str()
                .or_else(|| event["params"]["thread"]["id"].as_str());
            if let Some(context) =
                thread.and_then(|thread| contexts.get(&(container_id.clone(), thread.to_owned())))
            {
                let _ = output.send(scoped_event(&event, context, container_id.as_deref()));
            } else if event["method"] == "bridge.codex_disconnected" {
                for ((source, _), context) in contexts
                    .iter()
                    .filter(|((source, _), _)| source == &container_id)
                {
                    let _ = output.send(scoped_event(&event, context, source.as_deref()));
                }
            }
        }
    });
    events
}

fn scoped_events(
    output: mpsc::UnboundedSender<Value>,
    context: ExecutionContext,
    container_id: Option<String>,
) -> mpsc::UnboundedSender<Value> {
    let (events, mut incoming) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = incoming.recv().await {
            if output
                .send(scoped_event(&event, &context, container_id.as_deref()))
                .is_err()
            {
                break;
            }
        }
    });
    events
}

fn codex_error(error: CodexError, native_session_id: Option<String>) -> EngineError {
    EngineError {
        uncertain: error.uncertain,
        native_session_id,
        message: error.message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn codex_events_use_trusted_thread_and_container_bindings() -> Result<()> {
        use vibeface_secure::authorization::{ResourceId, SpaceId};
        let context = ExecutionContext {
            space_id: SpaceId::parse(&format!("spc2_{}", "1".repeat(64)))?,
            resource_id: ResourceId::parse(&format!("res2_{}", "2".repeat(64)))?,
            operation_id: "first-operation".into(),
            shared: true,
            lease_expires_at_ms: 0,
        };
        let mut other = context.clone();
        other.operation_id = "second-operation".into();
        let contexts = Arc::new(Mutex::new(BTreeMap::from([
            (
                (Some("container-a".into()), "same-thread".into()),
                context.clone(),
            ),
            (
                (Some("container-b".into()), "same-thread".into()),
                other.clone(),
            ),
        ])));
        let (output, mut received) = mpsc::unbounded_channel();
        let source = codex_events(output, contexts.clone(), Some("container-a".into()));
        source.send(json!({"execution_context":{"operation_id":"forged"},"method":"turn/completed","params":{"threadId":"same-thread"}}))?;
        let first = tokio::time::timeout(Duration::from_secs(1), received.recv())
            .await?
            .context("event missing")?;
        assert_eq!(
            first["execution_context"]["operation_id"],
            "first-operation"
        );
        assert_eq!(first["execution_context"]["container_id"], "container-a");
        contexts
            .lock()
            .await
            .insert((Some("container-a".into()), "same-thread".into()), other);
        source.send(json!({"method":"turn/completed","params":{"threadId":"same-thread"}}))?;
        let second = tokio::time::timeout(Duration::from_secs(1), received.recv())
            .await?
            .context("event missing")?;
        assert_eq!(
            second["execution_context"]["operation_id"],
            "second-operation"
        );
        assert_eq!(
            second["execution_context"]["space_id"],
            context.space_id.as_str()
        );
        Ok(())
    }
    #[tokio::test]
    async fn turn_lookup_pages_until_the_exact_native_turn() -> Result<()> {
        use futures_util::{SinkExt, StreamExt};
        use tokio::net::UnixListener;
        use tokio_tungstenite::tungstenite::Message;
        let dir = std::env::temp_dir().join(format!("vft-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let socket = dir.join("codex.sock");
        let listener = UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let initialize: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            ws.send(Message::Text(
                json!({"id":initialize["id"],"result":{}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let initialized: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(initialized["method"], "initialized");
            for (cursor, result) in [
                (
                    Value::Null,
                    json!({"data":[{"id":"other","status":"completed"}],"nextCursor":"page-2"}),
                ),
                (
                    json!("page-2"),
                    json!({"data":[{"id":"wanted","status":"failed"}],"nextCursor":null}),
                ),
            ] {
                let request: Value =
                    serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                assert_eq!(request["method"], "thread/turns/list");
                assert_eq!(request["params"]["threadId"], "native-thread");
                assert_eq!(request["params"]["cursor"], cursor);
                ws.send(Message::Text(
                    json!({"id":request["id"],"result":result})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            }
        });
        let (events, _receiver) = mpsc::unbounded_channel();
        let engine = Engine::new(
            ExecutorConfig {
                codex_socket: Some(socket),
                ..ExecutorConfig::default()
            },
            events,
        );
        assert_eq!(
            engine.snapshot().await["connection"]["state"],
            "unconfigured"
        );
        engine.personal_codex()?;
        tokio::time::timeout(Duration::from_secs(3), async {
            while engine.snapshot().await["connection"]["state"] != "connected" {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        assert_eq!(
            engine.read_turn("native-thread", "wanted").await?,
            Some(json!({"id":"wanted","status":"failed"}))
        );
        server.await?;
        drop(engine);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn actions_reject_invalid_text_and_unknown_fields() {
        assert!(EngineAction::Send { text: " ".into() }.validate().is_err());
        assert!(
            EngineAction::Send {
                text: "x".repeat(65_537)
            }
            .validate()
            .is_err()
        );
        assert!(
            EngineAction::Send {
                text: "continue".into()
            }
            .validate()
            .is_ok()
        );
        assert!(
            serde_json::from_value::<EngineAction>(
                json!({"type":"send","text":"ok","auto_approve":true})
            )
            .is_err()
        );
        assert!(
            EngineAction::Interrupt {
                turn_id: "bad\nturn".into()
            }
            .validate()
            .is_err()
        );
    }
}
