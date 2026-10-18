//! Local execution transports. Durable admission and authorization belong to the Node runtime.
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::{Mutex, mpsc};
use vibeface_bridge::codex::{CodexClient, CodexError};
use vibeface_protocol::{Command, RpcId};
use vibeface_secure::authorization::{ResourceId, SpaceId};

#[path = "engine_isolation.rs"]
mod isolation;
#[path = "engine_process.rs"]
mod process;
use isolation::DockerIsolation;
pub(crate) use isolation::IsolatedProcess as ManagerProcess;
pub use isolation::{ExecutionContext, ExecutionEgressConfig, ExecutionIsolationConfig};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    Codex,
    ClaudeCode,
    CursorAgent,
}

impl ExecutorKind {
    pub fn capabilities(self) -> &'static [&'static str] {
        match self {
            Self::Codex => &[
                "open",
                "send",
                "read",
                "steer",
                "interrupt",
                "respond_request",
            ],
            Self::ClaudeCode | Self::CursorAgent => &["open", "send"],
        }
    }
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

pub(crate) struct StoppedExecution {
    context: ExecutionContext,
    owner: Arc<Mutex<BTreeSet<ExecutionGeneration>>>,
    scope: isolation::StoppedScope,
    command: bool,
}

impl StoppedExecution {
    pub(crate) fn matches_context(&self, context: &ExecutionContext) -> bool {
        self.context.shared
            && context.shared
            && self.context.space_id == context.space_id
            && self.context.resource_id == context.resource_id
            && self.context.operation_id == context.operation_id
    }
}

type ExecutionGeneration = (SpaceId, ResourceId, String);

fn execution_generation(context: &ExecutionContext) -> ExecutionGeneration {
    (
        context.space_id.clone(),
        context.resource_id.clone(),
        context.operation_id.clone(),
    )
}

#[derive(Clone)]
pub struct Engine {
    config: ExecutorConfig,
    codex: Arc<OnceLock<CodexClient>>,
    events: mpsc::UnboundedSender<Value>,
    isolation: Option<DockerIsolation>,
    manager_isolation: Option<DockerIsolation>,
    command_isolation: Option<DockerIsolation>,
    codex_contexts: Arc<Mutex<BTreeMap<(Option<String>, String), ExecutionContext>>>,
    personal_turns: Arc<Mutex<BTreeMap<(String, String, String), (String, String)>>>,
    fenced_operations: Arc<Mutex<BTreeSet<ExecutionGeneration>>>,
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
        let manager_isolation = config
            .execution_isolation
            .clone()
            .zip(config.data_dir.clone())
            .map(|(settings, data_dir)| DockerIsolation::manager(settings, data_dir));
        let command_isolation = config
            .execution_isolation
            .clone()
            .zip(config.data_dir.clone())
            .map(|(settings, data_dir)| DockerIsolation::command(settings, data_dir));
        Self {
            config,
            codex,
            events,
            isolation,
            manager_isolation,
            command_isolation,
            codex_contexts,
            personal_turns: Arc::new(Mutex::new(BTreeMap::new())),
            fenced_operations: Arc::new(Mutex::new(BTreeSet::new())),
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

    pub(crate) async fn restore_observed_thread(
        &self,
        context: &ExecutionContext,
        native_id: &str,
        project: &std::path::Path,
    ) -> Result<()> {
        vibeface_protocol::validate_id(native_id).map_err(anyhow::Error::msg)?;
        let fenced = self.fenced_operations.lock().await;
        ensure!(
            !fenced.contains(&execution_generation(context)),
            "execution generation was fenced; reconcile its original operation"
        );
        let (codex, generation) = if context.shared {
            let isolation = self
                .isolation
                .as_ref()
                .context("shared execution isolation is not configured")?;
            let prepared = isolation.restore(context, project).await?;
            let generation = prepared.container_id().to_owned();
            let codex = isolation
                .codex(
                    context,
                    &prepared,
                    codex_events(
                        self.events.clone(),
                        self.codex_contexts.clone(),
                        Some(generation.clone()),
                    ),
                )
                .await?;
            (codex, Some(generation))
        } else {
            (self.personal_codex()?.clone(), None)
        };
        let mut bindings = self.codex_contexts.lock().await;
        if let Some(existing) = bindings.get(&(generation.clone(), native_id.to_owned())) {
            ensure_same_host_scope(existing, context)?;
        }
        bindings.insert((generation.clone(), native_id.to_owned()), context.clone());
        let threads = bindings
            .iter()
            .filter(|((source, _), _)| *source == generation)
            .map(|((_, thread), _)| thread.clone())
            .collect();
        codex.set_observed_threads(threads)?;
        Ok(())
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
        let (codex, generation) = if context.shared {
            let (codex, generation) = self
                .isolation
                .as_ref()
                .context("shared execution isolation is not configured")?
                .existing_codex(context)
                .await?;
            (codex, Some(generation))
        } else {
            (
                self.codex
                    .get()
                    .context("personal Provider is not connected")?
                    .clone(),
                None,
            )
        };
        let allowed: BTreeSet<String> = self
            .codex_contexts
            .lock()
            .await
            .iter()
            .filter(|((source, _), bound)| {
                *source == generation
                    && bound.space_id == context.space_id
                    && bound.resource_id == context.resource_id
                    && bound.operation_id == context.operation_id
            })
            .map(|((_, thread), _)| thread.clone())
            .collect();
        let snapshot = codex.snapshot().await;
        let threads: serde_json::Map<String, Value> = snapshot["threads"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(thread, _)| allowed.contains(*thread))
            .map(|(thread, value)| (thread.clone(), value.clone()))
            .collect();
        let requests = snapshot["pending_requests"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|request| {
                request["params"]["threadId"]
                    .as_str()
                    .is_some_and(|thread| allowed.contains(thread))
            })
            .cloned()
            .collect::<Vec<_>>();
        Ok(
            json!({"connection":snapshot["connection"],"threads":threads,"pending_requests":requests}),
        )
    }

    pub async fn read_turn_authorized(
        &self,
        context: &ExecutionContext,
        native_id: &str,
        turn_id: &str,
    ) -> Result<Option<Value>> {
        let codex = self.existing_authorized_codex(context, native_id).await?;
        Self::read_turn_from(&codex, native_id, turn_id).await
    }

    pub async fn read_session_authorized(
        &self,
        context: &ExecutionContext,
        native_id: &str,
    ) -> Result<Value> {
        let codex = self.existing_authorized_codex(context, native_id).await?;
        codex
            .execute(&Command::ReadThread {
                thread_id: native_id.into(),
            })
            .await
            .map_err(|error| anyhow::anyhow!(error.message))
    }

    async fn existing_authorized_codex(
        &self,
        context: &ExecutionContext,
        native_id: &str,
    ) -> Result<CodexClient> {
        vibeface_protocol::validate_id(native_id).map_err(anyhow::Error::msg)?;
        let (codex, generation) = if context.shared {
            let (codex, generation) = self
                .isolation
                .as_ref()
                .context("shared execution isolation is not configured")?
                .existing_codex(context)
                .await?;
            (codex, Some(generation))
        } else {
            (
                self.codex
                    .get()
                    .context("personal Provider is not connected")?
                    .clone(),
                None,
            )
        };
        let bindings = self.codex_contexts.lock().await;
        let bound = bindings
            .get(&(generation, native_id.to_owned()))
            .context("native session has no trusted execution binding")?;
        ensure_same_host_scope(bound, context)?;
        Ok(codex)
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

    pub(crate) async fn prepare_manager(
        &self,
        context: &ExecutionContext,
        workspace: &std::path::Path,
    ) -> Result<ManagerProcess> {
        self.manager_isolation
            .as_ref()
            .context("management requires configured process isolation")?
            .prepare(context, workspace)
            .await
    }

    pub async fn renew_manager(
        &self,
        context: &ExecutionContext,
        lease_expires_at_ms: u64,
    ) -> Result<()> {
        self.manager_isolation
            .as_ref()
            .context("management process isolation is not configured")?
            .renew(context, lease_expires_at_ms)
            .await
    }

    pub async fn cancel_manager(&self, context: &ExecutionContext) -> Result<Vec<String>> {
        self.manager_isolation
            .as_ref()
            .context("management process isolation is not configured")?
            .cancel(context)
            .await
    }

    pub async fn complete_manager(&self, context: &ExecutionContext) -> Result<()> {
        self.manager_isolation
            .as_ref()
            .context("management process isolation is not configured")?
            .complete(context)
            .await
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

    pub(crate) async fn fence_operation(
        &self,
        context: &ExecutionContext,
    ) -> Result<(StoppedExecution, Vec<String>)> {
        self.fence_execution(context, false).await
    }

    pub(crate) async fn fence_command(
        &self,
        context: &ExecutionContext,
    ) -> Result<(StoppedExecution, Vec<String>)> {
        self.fence_execution(context, true).await
    }

    async fn fence_execution(
        &self,
        context: &ExecutionContext,
        command: bool,
    ) -> Result<(StoppedExecution, Vec<String>)> {
        ensure!(context.shared, "only isolated execution can be fenced");
        vibeface_protocol::validate_id(&context.operation_id).map_err(anyhow::Error::msg)?;
        let mut fenced = self.fenced_operations.lock().await;
        fenced.insert(execution_generation(context));
        let isolation = if command {
            &self.command_isolation
        } else {
            &self.isolation
        };
        let (scope, affected) = isolation
            .as_ref()
            .context("shared execution isolation is not configured")?
            .fence(context)
            .await?;
        for operation in &affected {
            fenced.insert((
                context.space_id.clone(),
                context.resource_id.clone(),
                operation.clone(),
            ));
        }
        Ok((
            StoppedExecution {
                context: context.clone(),
                owner: self.fenced_operations.clone(),
                scope,
                command,
            },
            affected,
        ))
    }

    pub async fn affected_operations(&self, context: &ExecutionContext) -> Vec<String> {
        match &self.isolation {
            Some(isolation) if context.shared => isolation.affected(context).await,
            _ => vec![context.operation_id.clone()],
        }
    }

    #[cfg(test)]
    pub(crate) async fn prepare_operation_for_test(
        &self,
        context: &ExecutionContext,
        project: &std::path::Path,
    ) -> Result<String> {
        ensure!(context.shared, "only isolated execution can be prepared");
        let fenced = self.fenced_operations.lock().await;
        ensure!(
            !fenced.contains(&execution_generation(context)),
            "execution generation was fenced; reconcile its original operation"
        );
        let prepared = self
            .isolation
            .as_ref()
            .context("shared execution isolation is not configured")?
            .prepare(context, project)
            .await?;
        drop(fenced);
        Ok(prepared.container_id().to_owned())
    }

    #[cfg(test)]
    pub(crate) async fn operation_is_fenced_for_test(&self, context: &ExecutionContext) -> bool {
        self.fenced_operations
            .lock()
            .await
            .contains(&execution_generation(context))
    }

    pub async fn complete_operation(&self, context: &ExecutionContext) -> Result<()> {
        let _fenced = self.fenced_operations.lock().await;
        if let Some(isolation) = &self.isolation {
            isolation.complete(context).await?;
        }
        self.forget_operation(context).await;
        Ok(())
    }

    pub(crate) async fn complete_fenced_operation(&self, stopped: &StoppedExecution) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&self.fenced_operations, &stopped.owner),
            "stopped execution belongs to a different Engine"
        );
        let fenced = self.fenced_operations.lock().await;
        ensure!(
            fenced.contains(&execution_generation(&stopped.context)),
            "stopped execution generation is not fenced"
        );
        let isolation = if stopped.command {
            &self.command_isolation
        } else {
            &self.isolation
        };
        isolation
            .as_ref()
            .context("shared execution isolation is not configured")?
            .complete_fenced(&stopped.scope)
            .await?;
        self.forget_operation(&stopped.context).await;
        Ok(())
    }

    async fn forget_operation(&self, context: &ExecutionContext) {
        self.codex_contexts.lock().await.retain(|_, bound| {
            bound.space_id != context.space_id
                || bound.resource_id != context.resource_id
                || bound.operation_id != context.operation_id
        });
        self.personal_turns
            .lock()
            .await
            .remove(&personal_turn_key(context));
    }

    pub async fn cancel_personal_turn(
        &self,
        context: &ExecutionContext,
        native_id: &str,
        turn_id: &str,
    ) -> Result<()> {
        ensure!(
            !context.shared,
            "personal cancellation cannot target shared execution"
        );
        vibeface_protocol::validate_id(native_id).map_err(anyhow::Error::msg)?;
        vibeface_protocol::validate_id(turn_id).map_err(anyhow::Error::msg)?;
        let codex = self
            .codex
            .get()
            .context("personal Provider is not connected; cancellation was not confirmed")?;
        let bindings = self.codex_contexts.lock().await;
        let bound = bindings
            .get(&(None, native_id.to_owned()))
            .context("native session has no trusted execution binding")?;
        ensure_same_host_scope(bound, context)?;
        ensure!(
            bound.operation_id == context.operation_id,
            "native session belongs to another operation"
        );
        drop(bindings);
        ensure!(
            self.personal_turns
                .lock()
                .await
                .get(&personal_turn_key(context))
                == Some(&(native_id.to_owned(), turn_id.to_owned())),
            "turn does not match the original operation"
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            let terminal = |turn: &Value| {
                matches!(
                    turn["status"].as_str(),
                    Some("completed" | "failed" | "interrupted")
                )
            };
            let observed = Self::read_turn_from(codex, native_id, turn_id)
                .await?
                .context("original turn is not present; cancellation was not confirmed")?;
            if terminal(&observed) {
                return Ok(());
            }
            codex
                .execute(&Command::InterruptTurn {
                    thread_id: native_id.into(),
                    turn_id: turn_id.into(),
                })
                .await
                .map_err(|error| anyhow::anyhow!(error.message))?;
            loop {
                if let Some(turn) = Self::read_turn_from(codex, native_id, turn_id).await? {
                    if terminal(&turn) {
                        return Ok(());
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("personal turn stop was not confirmed; preserve unknown state")?
    }

    pub(crate) async fn execute_command_authorized(
        &self,
        context: &ExecutionContext,
        spec: &crate::command::CommandSpec,
    ) -> std::result::Result<crate::command::CommandOutput, crate::command::CommandError> {
        let prepared: Result<_> = async {
            ensure!(context.shared, "isolated command requires shared execution");
            let fenced = self.fenced_operations.lock().await;
            ensure!(
                !fenced.contains(&execution_generation(context)),
                "execution generation was fenced; reconcile its original operation"
            );
            let isolation = self
                .command_isolation
                .as_ref()
                .context("shared command isolation is not configured")?;
            let prepared = isolation.prepare(context, &spec.cwd).await?;
            drop(fenced);
            Ok(prepared)
        }
        .await;
        let prepared = prepared.map_err(|error| crate::command::CommandError {
            uncertain: false,
            message: error.to_string(),
        })?;
        crate::command::execute_in_container(spec, prepared.command("/bin/sh")).await
    }

    pub(crate) async fn renew_command(
        &self,
        context: &ExecutionContext,
        lease_expires_at_ms: u64,
    ) -> Result<()> {
        self.command_isolation
            .as_ref()
            .context("shared command isolation is not configured")?
            .renew(context, lease_expires_at_ms)
            .await
    }

    pub(crate) async fn complete_command(&self, context: &ExecutionContext) -> Result<()> {
        let _fenced = self.fenced_operations.lock().await;
        self.command_isolation
            .as_ref()
            .context("shared command isolation is not configured")?
            .complete(context)
            .await
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
            let fenced = self.fenced_operations.lock().await;
            if fenced.contains(&execution_generation(context)) {
                return Err(EngineError::rejected(
                    session,
                    "execution generation was fenced; reconcile its original operation",
                ));
            }
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
            let prepared = isolation
                .prepare(context, &session.cwd)
                .await
                .map_err(|error| EngineError::rejected(session, error.to_string()))?;
            drop(fenced);
            Some(prepared)
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
            if matches!(
                action,
                EngineAction::Steer { .. }
                    | EngineAction::Interrupt { .. }
                    | EngineAction::RespondRequest { .. }
            ) {
                codex
                    .execute(&Command::ReadThread {
                        thread_id: thread_id.clone(),
                    })
                    .await
                    .map_err(|error| codex_error(error, native.clone()))?;
                let snapshot = codex.snapshot().await;
                let turn = &snapshot["threads"][&thread_id]["last_turn"];
                let current = turn["id"]
                    .as_str()
                    .filter(|_| {
                        matches!(
                            turn["status"].as_str(),
                            Some("inProgress" | "in_progress" | "running")
                        )
                    })
                    .ok_or_else(|| {
                        EngineError::rejected(session, "execution has no current active turn")
                    })?;
                let belongs = match &action {
                    EngineAction::Steer { turn_id, .. } | EngineAction::Interrupt { turn_id } => {
                        turn_id == current
                    }
                    EngineAction::RespondRequest { request_id, .. } => snapshot["pending_requests"]
                        .as_array()
                        .is_some_and(|requests| {
                            requests.iter().any(|request| {
                                request["id"] == json!(request_id)
                                    && request["params"]["threadId"].as_str()
                                        == Some(thread_id.as_str())
                                    && request["params"]["turnId"].as_str() == Some(current)
                            })
                        }),
                    _ => true,
                };
                if !belongs {
                    return Err(EngineError::rejected(
                        session,
                        "control no longer matches the active turn or approval request",
                    ));
                }
            }
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
        if !context.shared && running_turn {
            if let Ok(reply) = &result {
                if let (Some(native), Some(turn)) = (
                    &reply.native_session_id,
                    reply.result["turn"]["id"].as_str(),
                ) {
                    vibeface_protocol::validate_id(turn).map_err(|error| EngineError {
                        uncertain: true,
                        native_session_id: Some(native.clone()),
                        message: error,
                    })?;
                    self.personal_turns.lock().await.insert(
                        personal_turn_key(context),
                        (native.clone(), turn.to_owned()),
                    );
                }
            }
        }
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

fn personal_turn_key(context: &ExecutionContext) -> (String, String, String) {
    (
        context.space_id.to_string(),
        context.resource_id.to_string(),
        context.operation_id.clone(),
    )
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
    #[ignore = "requires the pinned isolation fixture image and a local Docker daemon"]
    async fn ordinary_cleanup_serializes_a_real_replacement_and_rejects_stale_fencing() -> Result<()>
    {
        use std::os::unix::fs::PermissionsExt;
        use tokio::process::Command;

        async fn inspect(docker: &std::path::Path, id: &str) -> Result<Option<Value>> {
            let output = tokio::time::timeout(
                Duration::from_secs(15),
                Command::new(docker)
                    .args(["container", "inspect", id])
                    .kill_on_drop(true)
                    .output(),
            )
            .await??;
            if !output.status.success() {
                let error = String::from_utf8_lossy(&output.stderr);
                ensure!(
                    error.contains("No such container") || error.contains("No such object"),
                    "inspect test container failed: {error}"
                );
                return Ok(None);
            }
            let values: Vec<Value> = serde_json::from_slice(&output.stdout)?;
            ensure!(
                values.len() == 1,
                "inspect must return the exact test container"
            );
            Ok(values.into_iter().next())
        }

        let root = PathBuf::from(std::env::var("VIBEFACE_ISOLATION_TEST_ROOT")?)
            .join(uuid::Uuid::new_v4().to_string());
        let data = root.join("node");
        let project = root.join("project");
        crate::config::private_dir(&data)?;
        std::fs::create_dir(&project)?;
        let docker = PathBuf::from(std::env::var("VIBEFACE_ISOLATION_TEST_DOCKER")?);
        let wrapper = root.join("docker-cleanup-barrier");
        let armed = root.join("block-cleanup");
        let marker = root.join("cleanup-started");
        let release = root.join("release-cleanup");
        let quoted = |path: &std::path::Path| -> Result<String> {
            Ok(format!(
                "'{}'",
                path.to_str()
                    .context("test path must be UTF-8")?
                    .replace('\'', "'\\''")
            ))
        };
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nif [ \"$1\" = rm ] && [ -f {} ]; then\n  : > {}\n  while [ ! -f {} ]; do sleep 0.01; done\nfi\nexec {} \"$@\"\n",
                quoted(&armed)?,
                quoted(&marker)?,
                quoted(&release)?,
                quoted(&docker)?,
            ),
        )?;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))?;
        let (events, _) = mpsc::unbounded_channel();
        let engine = Engine::new(
            ExecutorConfig {
                execution_isolation: Some(ExecutionIsolationConfig {
                    image: std::env::var("VIBEFACE_ISOLATION_TEST_IMAGE")?,
                    docker_bin: wrapper,
                    egress: None,
                }),
                data_dir: Some(data),
                ..ExecutorConfig::default()
            },
            events,
        );
        let mut context = personal_context();
        context.shared = true;
        context.lease_expires_at_ms =
            vibeface_protocol::now_ms() + vibeface_secure::authorization::EXECUTION_LEASE_MS;
        let original = engine
            .prepare_operation_for_test(&context, &project)
            .await?;
        let mut name = original.clone();
        let mut completion = None;
        let mut preparing = None;
        let outcome = async {
            let initial = inspect(&docker, &original).await?.context("original container missing")?;
            name = initial["Name"].as_str().context("test container name missing")?
                .trim_start_matches('/').to_owned();
            ensure!(initial["State"]["Running"] == true, "original container must be running");
            std::fs::write(&armed, b"armed")?;
            let completing = engine.clone();
            let old = context.clone();
            completion = Some(tokio::spawn(async move { completing.complete_operation(&old).await }));
            tokio::time::timeout(Duration::from_secs(15), async {
                while !marker.exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }).await?;
            let mut replacement = context.clone();
            replacement.operation_id.push_str("-replacement");
            replacement.lease_expires_at_ms = vibeface_protocol::now_ms()
                + vibeface_secure::authorization::EXECUTION_LEASE_MS;
            let starting = engine.clone();
            let next = replacement.clone();
            let next_project = project.clone();
            let (entered, started) = tokio::sync::oneshot::channel();
            preparing = Some(tokio::spawn(async move {
                let _ = entered.send(());
                starting.prepare_operation_for_test(&next, &next_project).await
            }));
            started.await?;
            if let Ok(result) =
                tokio::time::timeout(Duration::from_millis(100), preparing.as_mut().unwrap()).await
            {
                preparing.take();
                anyhow::bail!("replacement completed before ordinary cleanup was released: {result:?}");
            }
            std::fs::write(&release, b"release")?;
            completion.take().unwrap().await??;
            let replacement_id = preparing.take().unwrap().await??;
            ensure!(replacement_id != original, "replacement reused the old container generation");
            ensure!(inspect(&docker, &original).await?.is_none(), "ordinary cleanup left the old container");
            let current = inspect(&docker, &replacement_id).await?.context("replacement container missing")?;
            ensure!(current["State"]["Running"] == true, "ordinary cleanup stopped the replacement");
            let failure = engine.fence_operation(&context).await.err().context("stale fence stopped a replacement")?;
            ensure!(failure.to_string().contains("stale operation"), "unexpected stale fence error: {failure:#}");
            engine.complete_operation(&context).await?;
            let current = inspect(&docker, &replacement_id).await?.context("stale cleanup removed the replacement")?;
            ensure!(current["State"]["Running"] == true, "stale fence stopped the replacement");
            std::fs::remove_file(&armed)?;
            engine.complete_operation(&replacement).await?;
            ensure!(inspect(&docker, &replacement_id).await?.is_none(), "replacement cleanup did not finish");
            println!("cleanup-concurrency-proof={{\"replacement_waited\":true,\"new_container_survived\":true,\"stale_fence_rejected\":true,\"both_generations_removed\":true}}");
            Ok::<_, anyhow::Error>(())
        }.await;
        let _ = std::fs::write(&release, b"release");
        let _ = std::fs::remove_file(&armed);
        if let Some(task) = completion {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = preparing {
            task.abort();
            let _ = task.await;
        }
        let cleanup = tokio::time::timeout(
            Duration::from_secs(15),
            Command::new(&docker)
                .args(["rm", "--force", &name])
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        if !cleanup.status.success() {
            let error = String::from_utf8_lossy(&cleanup.stderr);
            ensure!(
                error.contains("No such container") || error.contains("No such object"),
                "cleanup test container failed: {error}"
            );
        }
        ensure!(
            inspect(&docker, &name).await?.is_none(),
            "test container survived final cleanup"
        );
        std::fs::remove_dir_all(&root)?;
        outcome
    }

    #[tokio::test]
    async fn confirmed_fencing_blocks_a_concurrent_late_execution_generation() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("vff-{}", uuid::Uuid::new_v4()));
        let data = root.join("node");
        let project = root.join("project");
        crate::config::private_dir(&data)?;
        std::fs::create_dir(&project)?;
        let marker = root.join("inspect-started");
        let release = root.join("release-inspect");
        let docker = root.join("docker");
        std::fs::write(
            &docker,
            format!(
                "#!/bin/sh\nprintf 'inspect\\n' >> '{}'\nwhile [ ! -f '{}' ]; do sleep 0.01; done\nprintf 'No such container\\n' >&2\nexit 1\n",
                marker.display(),
                release.display()
            ),
        )?;
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700))?;
        let (events, _) = mpsc::unbounded_channel();
        let engine = Engine::new(
            ExecutorConfig {
                execution_isolation: Some(ExecutionIsolationConfig {
                    image: format!("sha256:{}", "a".repeat(64)),
                    docker_bin: docker,
                    egress: None,
                }),
                data_dir: Some(data),
                ..ExecutorConfig::default()
            },
            events,
        );
        let mut context = personal_context();
        assert!(engine.fence_operation(&context).await.is_err());
        assert!(!marker.exists());
        context.shared = true;
        let stopping = engine.clone();
        let stopping_context = context.clone();
        let stopped =
            tokio::spawn(async move { stopping.fence_operation(&stopping_context).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        let delayed = engine.clone();
        let delayed_context = context.clone();
        let mut late = tokio::spawn(async move {
            delayed
                .execute_authorized(
                    &delayed_context,
                    &ExecutorSession {
                        executor: ExecutorKind::ClaudeCode,
                        cwd: project,
                        native_session_id: None,
                    },
                    EngineAction::Send {
                        text: "must not execute".into(),
                    },
                    |_| async { anyhow::bail!("a fenced operation persisted a native session") },
                )
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut late)
                .await
                .is_err()
        );
        std::fs::write(release, b"release")?;
        let (token, affected) = stopped.await??;
        assert_eq!(affected, vec![context.operation_id.clone()]);
        assert!(token.matches_context(&context));
        let mut alternatives = [
            context.clone(),
            context.clone(),
            context.clone(),
            context.clone(),
        ];
        alternatives[0].shared = false;
        alternatives[1].operation_id.push_str("-other");
        alternatives[2].resource_id = ResourceId::from_bytes([8; 32]);
        alternatives[3].space_id = SpaceId::parse(&format!("spc2_{}", "f".repeat(64)))?;
        for other in alternatives {
            assert!(!token.matches_context(&other));
        }
        let failure = late
            .await?
            .expect_err("a late execution generation must be rejected");
        assert!(!failure.uncertain);
        assert!(failure.message.contains("generation was fenced"));
        assert_eq!(std::fs::read_to_string(&marker)?, "inspect\n");
        let (events, _) = mpsc::unbounded_channel();
        let other_engine = Engine::new(engine.config.clone(), events);
        assert!(
            other_engine
                .complete_fenced_operation(&token)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&marker)?, "inspect\n");
        engine.clone().complete_fenced_operation(&token).await?;
        engine.complete_fenced_operation(&token).await?;
        assert_eq!(
            std::fs::read_to_string(&marker)?,
            "inspect\ninspect\ninspect\n"
        );
        assert!(
            engine
                .fenced_operations
                .lock()
                .await
                .contains(&execution_generation(&context))
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

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

    fn personal_context() -> ExecutionContext {
        use vibeface_secure::authorization::{ResourceId, SpaceId};
        ExecutionContext {
            space_id: SpaceId::parse(&format!("spc2_{}", "a".repeat(64))).unwrap(),
            resource_id: ResourceId::parse(&format!("res2_{}", "b".repeat(64))).unwrap(),
            operation_id: "personal-operation".into(),
            shared: false,
            lease_expires_at_ms: 0,
        }
    }

    #[tokio::test]
    async fn reconciliation_never_initializes_a_personal_provider() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("vfr-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir)?;
        let socket = dir.join("codex.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let (events, _) = mpsc::unbounded_channel();
        let engine = Engine::new(
            ExecutorConfig {
                codex_socket: Some(socket),
                ..ExecutorConfig::default()
            },
            events,
        );
        let context = personal_context();
        assert!(engine.snapshot_authorized(&context).await.is_err());
        assert!(
            engine
                .read_turn_authorized(&context, "native", "turn")
                .await
                .is_err()
        );
        assert!(
            engine
                .read_session_authorized(&context, "native")
                .await
                .is_err()
        );
        assert!(
            engine
                .cancel_personal_turn(&context, "native", "turn")
                .await
                .is_err()
        );
        assert!(engine.codex.get().is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn personal_cancellation_interrupts_only_the_original_turn_and_confirms_terminal()
    -> Result<()> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let dir = std::env::temp_dir().join(format!("vfc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir)?;
        let socket = dir.join("codex.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut ws = tokio_tungstenite::accept_async(stream).await?;
            let init: Value =
                serde_json::from_str(ws.next().await.context("initialize missing")??.to_text()?)?;
            ws.send(Message::Text(
                json!({"id":init["id"],"result":{}}).to_string().into(),
            ))
            .await?;
            let initialized: Value =
                serde_json::from_str(ws.next().await.context("initialized missing")??.to_text()?)?;
            ensure!(
                initialized["method"] == "initialized",
                "initialize handshake missing"
            );
            let steps = [
                ("thread/start", json!({"thread":{"id":"original-native"}})),
                (
                    "turn/start",
                    json!({"turn":{"id":"original-turn","status":"inProgress"}}),
                ),
                (
                    "thread/read",
                    json!({"thread":{"id":"original-native","turns":[]}}),
                ),
                (
                    "thread/turns/list",
                    json!({"data":[{"id":"original-turn","status":"inProgress"}],"nextCursor":null}),
                ),
                ("turn/interrupt", json!({})),
                (
                    "thread/turns/list",
                    json!({"data":[{"id":"original-turn","status":"inProgress"}],"nextCursor":null}),
                ),
                (
                    "thread/turns/list",
                    json!({"data":[{"id":"original-turn","status":"interrupted"}],"nextCursor":null}),
                ),
                (
                    "thread/turns/list",
                    json!({"data":[{"id":"original-turn","status":"interrupted"}],"nextCursor":null}),
                ),
            ];
            for (method, result) in steps {
                let request: Value = serde_json::from_str(
                    ws.next()
                        .await
                        .context("expected request missing")??
                        .to_text()?,
                )?;
                ensure!(
                    request["method"] == method,
                    "unexpected Provider request: {}",
                    request["method"]
                );
                if method != "thread/start" {
                    ensure!(
                        request["params"]["threadId"] == "original-native",
                        "wrong native session targeted"
                    );
                }
                if method == "turn/interrupt" {
                    ensure!(
                        request["params"]["turnId"] == "original-turn",
                        "wrong turn interrupted"
                    );
                }
                ws.send(Message::Text(
                    json!({"id":request["id"],"result":result})
                        .to_string()
                        .into(),
                ))
                .await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let (events, _) = mpsc::unbounded_channel();
        let engine = Engine::new(
            ExecutorConfig {
                codex_socket: Some(socket),
                ..ExecutorConfig::default()
            },
            events,
        );
        let context = personal_context();
        let result = engine
            .execute_authorized(
                &context,
                &ExecutorSession {
                    executor: ExecutorKind::Codex,
                    cwd: dir.clone(),
                    native_session_id: None,
                },
                EngineAction::Send {
                    text: "fixed original operation".into(),
                },
                |_| async { Ok(()) },
            )
            .await?;
        assert_eq!(result.native_session_id.as_deref(), Some("original-native"));
        assert_eq!(result.result["turn"]["id"], "original-turn");
        let mut wrong_scope = context.clone();
        wrong_scope.space_id =
            vibeface_secure::authorization::SpaceId::parse(&format!("spc2_{}", "c".repeat(64)))?;
        let mut wrong_operation = context.clone();
        wrong_operation.operation_id = "unrelated-operation".into();
        for bad in [&wrong_scope, &wrong_operation] {
            assert!(
                engine
                    .cancel_personal_turn(bad, "original-native", "original-turn")
                    .await
                    .is_err()
            );
        }
        assert!(
            engine
                .cancel_personal_turn(&context, "wrong-native", "original-turn")
                .await
                .is_err()
        );
        assert!(
            engine
                .cancel_personal_turn(&context, "original-native", "wrong-turn")
                .await
                .is_err()
        );
        assert!(
            engine
                .read_session_authorized(&wrong_scope, "original-native")
                .await
                .is_err()
        );
        assert_eq!(
            engine
                .read_session_authorized(&context, "original-native")
                .await?["thread"]["id"],
            "original-native"
        );
        engine
            .cancel_personal_turn(&context, "original-native", "original-turn")
            .await?;
        engine
            .cancel_personal_turn(&context, "original-native", "original-turn")
            .await?;
        tokio::time::timeout(Duration::from_secs(2), server).await???;
        drop(engine);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
