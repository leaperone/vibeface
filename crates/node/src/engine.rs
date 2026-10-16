//! Local execution transports. Durable admission and authorization belong to the Node runtime.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{future::Future, path::PathBuf, time::Duration};
use tokio::sync::mpsc;
use vibeface_bridge::codex::{CodexClient, CodexError};
use vibeface_protocol::{Command, RpcId};

#[path = "engine_process.rs"]
mod process;

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
    pub codex_bin: PathBuf,
    pub claude_bin: PathBuf,
    pub cursor_bin: PathBuf,
    pub timeout_secs: u64,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            codex_socket: None,
            codex_bin: "codex".into(),
            claude_bin: "claude".into(),
            cursor_bin: "cursor-agent".into(),
            timeout_secs: 600,
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
    codex: Option<CodexClient>,
    events: mpsc::UnboundedSender<Value>,
}

impl Engine {
    pub fn new(config: ExecutorConfig, events: mpsc::UnboundedSender<Value>) -> Self {
        let socket = config.codex_socket.clone().or_else(|| {
            std::env::var_os("HOME").map(|home| {
                PathBuf::from(home).join(".codex/app-server-control/app-server-control.sock")
            })
        });
        let codex = socket.map(|socket| CodexClient::start(socket, events.clone()));
        Self {
            config,
            codex,
            events,
        }
    }

    pub async fn snapshot(&self) -> Value {
        match &self.codex {
            Some(codex) => codex.snapshot().await,
            None => {
                json!({"connection":{"state":"unconfigured"},"threads":{},"pending_requests":[]})
            }
        }
    }

    pub async fn execute<F, Fut>(
        &self,
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
        if session.executor != ExecutorKind::Codex {
            return process::execute(
                &self.config,
                session,
                action,
                persist_native,
                self.events.clone(),
            )
            .await;
        }
        let codex = self
            .codex
            .as_ref()
            .ok_or_else(|| EngineError::rejected(session, "Codex socket is not configured"))?;
        let mut native = session.native_session_id.clone();
        let mut sent = false;
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
                let cwd = session.cwd.to_str().ok_or_else(|| {
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
        match tokio::time::timeout(Duration::from_secs(self.config.timeout_secs), operation).await {
            Ok(result) => result,
            Err(_) => Err(EngineError {
                uncertain: sent,
                native_session_id: native,
                message: "Codex command timed out; reconcile its native session before retrying"
                    .into(),
            }),
        }
    }
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
