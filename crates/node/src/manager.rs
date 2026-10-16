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
    let mut native = session_id.map(str::to_owned);
    let mut persist = Some(persist_session);
    if let Some(id) = &native {
        persist.take().unwrap()(id.clone())
            .await
            .map_err(|error| ManagerError {
                uncertain: false,
                session_id: native.clone(),
                message: format!("persist Codex SDK session before resume: {error}"),
            })?;
    }
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
        write_message(&mut stdin,&json!({"type":"init","prompt":prompt,"session_id":native,"cwd":config.workspace,"model":config.model,"codex_path":codex,
            "mcp_servers":config.mcp_servers,"tool_cli":tool_cli,"tool_env":config.tool_env})).await?;
        let mut reader = BufReader::new(stdout);
        let ready = read_message(&mut reader).await?;
        ensure!(
            ready["type"] == "ready" && ready["session_id"].as_str() == native.as_deref(),
            "Codex SDK did not confirm the expected native session"
        );
        sent = true;
        write_message(&mut stdin, &json!({"type":"ack","session_id":native})).await?;
        let mut total = 0;
        loop {
            let message = redact(read_message(&mut reader).await?, &secrets);
            total += serde_json::to_vec(&message)?.len();
            ensure!(
                total <= MAX_STREAM,
                "Codex SDK stream exceeded the size limit"
            );
            match message["type"].as_str() {
                Some("session") => {
                    let id = message["session_id"]
                        .as_str()
                        .context("Codex SDK session event has no native ID")?;
                    vibeface_protocol::validate_id(id).map_err(anyhow::Error::msg)?;
                    ensure!(
                        native.as_deref().is_none_or(|previous| previous == id),
                        "Codex SDK changed its native session"
                    );
                    native = Some(id.into());
                    if let Some(persist) = persist.take() {
                        persist(id.into())
                            .await
                            .context("persist observed Codex SDK native session")?;
                    }
                    write_message(&mut stdin, &json!({"type":"session_ack","session_id":id}))
                        .await?;
                }
                Some("event") => progress(message["event"].clone()).await?,
                Some("result") => {
                    ensure!(
                        native.is_some() && message["session_id"].as_str() == native.as_deref(),
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
                        session_id: native.clone(),
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
        let _ = write_message(&mut stdin, &json!({"type":"cancel"})).await;
        if tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
    outcome.map_err(|error| {
        ManagerError {
            uncertain: sent,
            session_id: native,
            message: redact_string(error.to_string(), &secrets),
        }
        .into()
    })
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
    #[tokio::test]
    async fn fresh_sdk_native_id_is_persisted_and_failed_persistence_is_unknown() {
        let directory =
            std::env::temp_dir().join(format!("vibeface-manager-fixture-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        let script = directory.join("fixture.sh");
        tokio::fs::write(
            &script,
            r#"IFS= read -r init || exit 1
printf '%s\n' '{"type":"ready","session_id":null}'
IFS= read -r ack || exit 2
printf '%s\n' '{"type":"session","session_id":"fixture-native"}'
IFS= read -r confirmed || exit 3
case "$confirmed" in *session_ack*) ;; *) exit 4 ;; esac
test -f persisted || exit 5
printf '%s\n' '{"type":"result","session_id":"fixture-native","final_response":"done","usage":null}'
"#,
        )
        .await
        .unwrap();
        let config = ManagerConfig {
            kind: ManagerKind::CodexSdk,
            workspace: directory.clone(),
            node_bin: "/bin/sh".into(),
            codex_bin: "/usr/bin/true".into(),
            codex_sdk_script: script,
            timeout_secs: 5,
            ..ManagerConfig::default()
        };
        let marker = directory.join("persisted");
        let result = run_manager(
            &config,
            "fixture",
            None,
            move |id| async move {
                assert_eq!(id, "fixture-native");
                tokio::fs::write(marker, b"yes").await?;
                Ok(())
            },
            |_| async { Ok(()) },
        )
        .await
        .unwrap();
        assert_eq!(result.session_id.as_deref(), Some("fixture-native"));
        assert_eq!(result.output, "done");
        let error = run_manager(
            &config,
            "fixture",
            None,
            |_| async { anyhow::bail!("fixture storage failed") },
            |_| async { Ok(()) },
        )
        .await
        .err()
        .unwrap();
        let error = error.downcast_ref::<ManagerError>().unwrap();
        assert!(error.uncertain);
        assert_eq!(error.session_id.as_deref(), Some("fixture-native"));
        tokio::fs::remove_dir_all(directory).await.unwrap();
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
