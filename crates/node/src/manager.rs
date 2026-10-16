//! Management SDKs receive visible context and explicit Node capabilities.
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
use vibeface_bridge::agent::{
    self, AgentResult, CustomToolConfig, McpServerConfig, SdkAgentConfig, ToolCallbackConfig,
};

const MAX_LINE: usize = 1_048_576;
const MAX_STREAM: usize = 16_777_216;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagerKind {
    CursorSdk,
    CodexSdk,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerConfig {
    pub kind: ManagerKind,
    pub workspace: PathBuf,
    pub model: String,
    pub timeout_secs: u64,
    pub cursor_sdk_bin: PathBuf,
    pub node_bin: PathBuf,
    pub codex_bin: PathBuf,
    pub codex_socket: Option<PathBuf>,
    pub codex_sdk_script: PathBuf,
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    #[serde(default)]
    pub custom_tools: BTreeMap<String, CustomToolConfig>,
    pub tool_callback_url: Option<String>,
    #[serde(skip)]
    pub tool_callback_token: Option<String>,
    pub tool_cli: Option<PathBuf>,
    #[serde(skip)]
    pub tool_env: BTreeMap<String, String>,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            kind: ManagerKind::CursorSdk,
            workspace: ".".into(),
            model: "composer-2.5".into(),
            timeout_secs: 600,
            cursor_sdk_bin: "cursor-sdk-bridge".into(),
            node_bin: "node".into(),
            codex_bin: "codex".into(),
            codex_socket: None,
            codex_sdk_script: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("sdk/codex-manager.mjs"),
            mcp_servers: BTreeMap::new(),
            custom_tools: BTreeMap::new(),
            tool_callback_url: None,
            tool_callback_token: None,
            tool_cli: None,
            tool_env: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
pub struct ManagerError {
    pub uncertain: bool,
    pub session_id: Option<String>,
    pub message: String,
}
impl std::fmt::Display for ManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ManagerError {}

pub async fn run_manager<F, P, PF>(
    config: &ManagerConfig,
    prompt: &str,
    previous_session_id: Option<&str>,
    persist_session: impl FnOnce(String) -> F,
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
    match config.kind {
        ManagerKind::CursorSdk => {
            let callback = match (&config.tool_callback_url, &config.tool_callback_token) {
                (Some(url), Some(token)) => Some(ToolCallbackConfig {
                    url: url.clone(),
                    auth_token: token.clone(),
                }),
                (None, None) => None,
                _ => return Err(anyhow!("custom tool callback requires both URL and token")),
            };
            agent::run_sdk(
                &SdkAgentConfig {
                    executable: config.cursor_sdk_bin.clone(),
                    workspace: config.workspace.clone(),
                    timeout_secs: config.timeout_secs,
                    model: config.model.clone(),
                    mcp_servers: config.mcp_servers.clone(),
                    custom_tools: config.custom_tools.clone(),
                    tool_callback: callback,
                },
                prompt,
                previous_session_id,
                persist_session,
                progress,
            )
            .await
            .map_err(|error| {
                if let Some(sdk) = error.downcast_ref::<agent::AgentRunError>() {
                    ManagerError {
                        uncertain: sdk.uncertain,
                        session_id: sdk.session_id.clone(),
                        message: sdk.to_string(),
                    }
                    .into()
                } else {
                    error
                }
            })
        }
        ManagerKind::CodexSdk => {
            run_codex(
                config,
                prompt,
                previous_session_id,
                persist_session,
                progress,
            )
            .await
        }
    }
}

async fn run_codex<F, P, PF>(
    config: &ManagerConfig,
    prompt: &str,
    session_id: Option<&str>,
    persist_session: impl FnOnce(String) -> F,
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
    ensure!(
        config.codex_sdk_script.is_file(),
        "Codex SDK sidecar is missing"
    );
    let codex = resolve_executable(&config.codex_bin)?;
    let tool_cli = config
        .tool_cli
        .as_ref()
        .map(|path| resolve_executable(path))
        .transpose()?;
    let owned_id = match session_id {
        Some(id) => id.to_owned(),
        None => open_codex_thread(config).await?,
    };
    let id = owned_id.as_str();
    persist_session(id.to_owned())
        .await
        .map_err(|error| ManagerError {
            uncertain: false,
            session_id: Some(id.into()),
            message: format!("persist Codex SDK session before send: {error}"),
        })?;
    let mut command = Command::new(&config.node_bin);
    command
        .arg(&config.codex_sdk_script)
        .current_dir(&config.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("start official Codex SDK sidecar; install the pinned SDK dependencies first")?;
    let mut stdin = child.stdin.take().context("Codex SDK input unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("Codex SDK output unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("Codex SDK diagnostic stream unavailable")?;
    let mut stderr_task = tokio::spawn(async move {
        let mut limited = stderr.take((MAX_STREAM + 1) as u64);
        let mut discarded = Vec::new();
        limited.read_to_end(&mut discarded).await?;
        ensure!(
            discarded.len() <= MAX_STREAM,
            "Codex SDK diagnostic stream exceeded the size limit"
        );
        Ok::<(), anyhow::Error>(())
    });
    let mut sent = false;
    let secrets = secret_values(config);
    let operation = async {
        write_message(&mut stdin,&json!({"type":"init","prompt":prompt,"session_id":id,"cwd":config.workspace,"model":config.model,"codex_path":codex,
            "mcp_servers":config.mcp_servers,"tool_cli":tool_cli,"tool_env":config.tool_env})).await?;
        let mut reader = BufReader::new(stdout);
        let ready = read_message(&mut reader).await?;
        ensure!(
            ready["type"] == "ready" && ready["session_id"].as_str() == Some(id),
            "Codex SDK did not confirm the expected native session"
        );
        sent = true;
        write_message(&mut stdin, &json!({"type":"ack","session_id":id})).await?;
        let mut total = 0;
        loop {
            let message = redact(read_message(&mut reader).await?, &secrets);
            total += serde_json::to_vec(&message)?.len();
            ensure!(
                total <= MAX_STREAM,
                "Codex SDK stream exceeded the size limit"
            );
            match message["type"].as_str() {
                Some("event") => progress(message["event"].clone()).await?,
                Some("result") => {
                    ensure!(
                        message["session_id"].as_str() == Some(id),
                        "Codex SDK returned a different native session"
                    );
                    let output = message["final_response"]
                        .as_str()
                        .context("Codex SDK result has no final response")?;
                    ensure!(output.len() <= 65_536, "Codex SDK result is too large");
                    let status = child.wait().await.context("wait for Codex SDK sidecar")?;
                    ensure!(
                        status.success(),
                        "Codex SDK sidecar exited unsuccessfully after its result"
                    );
                    return Ok(AgentResult {
                        session_id: Some(id.into()),
                        output: output.into(),
                        usage: message
                            .get("usage")
                            .filter(|value| !value.is_null())
                            .cloned(),
                    });
                }
                Some("error") => {
                    return Err(anyhow!(
                        "{}",
                        message["message"].as_str().unwrap_or("Codex SDK failed")
                    ));
                }
                _ => return Err(anyhow!("Codex SDK emitted an unsupported protocol message")),
            }
        }
    };
    let outcome = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(config.timeout_secs), operation) => result.unwrap_or_else(|_|Err(anyhow!("Codex SDK management timed out; reconcile before retrying"))),
        diagnostics = async {
            match (&mut stderr_task).await {
                Ok(Ok(())) => std::future::pending::<Result<AgentResult>>().await,
                _ => Err(anyhow!("Codex SDK diagnostic stream failed")),
            }
        } => diagnostics,
    };
    stderr_task.abort();
    if outcome.is_err() {
        let _ = child.kill().await;
    }
    outcome.map_err(|error| {
        ManagerError {
            uncertain: sent,
            session_id: Some(id.into()),
            message: redact_string(error.to_string(), &secrets),
        }
        .into()
    })
}

async fn open_codex_thread(config: &ManagerConfig) -> Result<String> {
    use vibeface_bridge::codex::CodexClient;
    use vibeface_protocol::Command as CodexCommand;
    let socket = config
        .codex_socket
        .clone()
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                PathBuf::from(home).join(".codex/app-server-control/app-server-control.sock")
            })
        })
        .context("Codex socket is not configured")?;
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let client = CodexClient::start(socket, events);
    let ready = async {
        loop {
            if client.snapshot().await["connection"]["state"] == "connected" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(config.timeout_secs.min(10)), ready)
        .await
        .context("Codex local app-server is unavailable")?;
    let cwd = config
        .workspace
        .to_str()
        .context("manager workspace must be UTF-8")?;
    let response = client
        .execute(&CodexCommand::StartThread { cwd: cwd.into() })
        .await
        .map_err(|error| ManagerError {
            uncertain: error.uncertain,
            session_id: None,
            message: error.message,
        })?;
    let id = response["thread"]["id"]
        .as_str()
        .context("Codex did not return a native thread ID")?;
    vibeface_protocol::validate_id(id).map_err(anyhow::Error::msg)?;
    Ok(id.into())
}

fn resolve_executable(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        ensure!(path.is_file(), "configured executable is missing");
        return Ok(path.to_path_buf());
    }
    ensure!(
        path.components().count() == 1,
        "executable must be an absolute path or a PATH command"
    );
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(path))
        .find(|candidate| candidate.is_file())
        .context("configured executable was not found on PATH")
}

async fn write_message(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &Value,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_LINE, "Codex SDK request is too large");
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}
async fn read_message(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Value> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_LINE + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_LINE && bytes.ends_with(b"\n"),
        "Codex SDK stream closed or exceeded its frame limit"
    );
    serde_json::from_slice(&bytes).context("Codex SDK emitted invalid JSON")
}
fn secret_values(config: &ManagerConfig) -> Vec<String> {
    let mut values: Vec<_> = config
        .tool_env
        .values()
        .filter(|value| value.len() >= 8)
        .cloned()
        .collect();
    if let Some(token) = &config.tool_callback_token {
        values.push(token.clone());
    }
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
    values
}
fn redact_string(mut text: String, secrets: &[String]) -> String {
    for secret in secrets.iter().filter(|value| !value.is_empty()) {
        text = text.replace(secret, "[redacted]");
    }
    text
}
fn redact(value: Value, secrets: &[String]) -> Value {
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
    #[tokio::test]
    async fn sdk_frames_require_newline_and_valid_bounded_json() {
        assert_eq!(
            read_message(&mut BufReader::new(&b"{\"type\":\"ready\"}\n"[..]))
                .await
                .unwrap()["type"],
            "ready"
        );
        assert!(read_message(&mut BufReader::new(&b"{}"[..])).await.is_err());
        assert!(
            read_message(&mut BufReader::new(&b"bad\n"[..]))
                .await
                .is_err()
        );
    }
    #[test]
    fn callback_credentials_are_not_serialized() {
        let config = ManagerConfig {
            tool_callback_token: Some("secret-token".into()),
            tool_env: BTreeMap::from([("TOKEN".into(), "other-secret".into())]),
            ..ManagerConfig::default()
        };
        let encoded = serde_json::to_string(&config).unwrap();
        assert!(!encoded.contains("secret-token") && !encoded.contains("other-secret"));
        assert_eq!(
            redact(json!({"nested":["secret-token"]}), &secret_values(&config)),
            json!({"nested":["[redacted]"]})
        );
    }
}
