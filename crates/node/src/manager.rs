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
    sync::watch,
};
pub use vibeface_bridge::agent::AgentFailure as ManagerFailure;
use vibeface_bridge::agent::{
    self, AgentResult, CustomToolConfig, SdkAgentConfig, ToolCallbackConfig,
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
    pub codex_sdk_script: PathBuf,
    #[serde(default)]
    pub custom_tools: BTreeMap<String, CustomToolConfig>,
    pub tool_callback_url: Option<String>,
    #[serde(skip)]
    pub tool_callback_token: Option<String>,
    pub tool_cli: Option<PathBuf>,
    #[serde(skip)]
    pub tool_env: BTreeMap<String, String>,
    #[serde(skip)]
    pub process_started: Option<agent::ProcessStarted>,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            kind: ManagerKind::CursorSdk,
            workspace: ".".into(),
            model: "auto".into(),
            timeout_secs: 600,
            cursor_sdk_bin: "cursor-sdk-bridge".into(),
            node_bin: "node".into(),
            codex_bin: "codex".into(),
            codex_sdk_script: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("sdk/codex-manager.mjs"),
            custom_tools: BTreeMap::new(),
            tool_callback_url: None,
            tool_callback_token: None,
            tool_cli: None,
            tool_env: BTreeMap::new(),
            process_started: None,
        }
    }
}

impl ManagerKind {
    pub fn capabilities(self) -> Value {
        json!({
            "verified": false,
            "streaming": true,
            "cancel": true,
            "steer": false,
            "resume_local_session": true,
            "text_stream_mode": match self { ManagerKind::CursorSdk => "delta_and_snapshot", ManagerKind::CodexSdk => "snapshot" },
            "cancel_request": true,
            "cancel_confirmation": true,
            "cancel_confirmation_evidence": match self { ManagerKind::CursorSdk => "provider_terminal_or_managed_process_exit", ManagerKind::CodexSdk => "managed_process_tree_exit" },
            "native_steer": false,
            "run_reconcile": false,
        })
    }
}

#[derive(Debug)]
pub struct ManagerError {
    pub kind: ManagerFailure,
    pub session_id: Option<String>,
    pub message: String,
}
impl ManagerError {
    pub fn uncertain(&self) -> bool {
        self.kind == ManagerFailure::Unknown
    }
    pub fn cancelled(&self) -> bool {
        self.kind == ManagerFailure::Cancelled
    }
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
            kind: ManagerFailure::Cancelled,
            session_id: previous_session_id.map(str::to_owned),
            message: "management cancelled before SDK launch".into(),
        }
        .into());
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
                    tool_env: config.tool_env.clone(),
                    custom_tools: config.custom_tools.clone(),
                    tool_callback: callback,
                    process_started: config.process_started.clone(),
                },
                prompt,
                previous_session_id,
                persist_session,
                cancel,
                progress,
            )
            .await
            .map_err(|error| {
                if let Some(sdk) = error.downcast_ref::<agent::AgentRunError>() {
                    ManagerError {
                        kind: sdk.kind,
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
                cancel,
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
                kind: ManagerFailure::Failed,
                session_id: native.clone(),
                message: format!("persist Codex SDK session before resume: {error}"),
            })?;
    }
    let mut command = Command::new(&config.node_bin);
    let provider_keys = provider_environment_keys()?;
    command
        .arg(&config.codex_sdk_script)
        .env(
            "VIBEFACE_CODEX_PROVIDER_ENV_KEYS",
            serde_json::to_string(&provider_keys)?,
        )
        .current_dir(&config.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("start official Codex SDK sidecar; install the pinned SDK dependencies first")?;
    let process_group = child.id().context("Codex SDK sidecar has no process id")?;
    if let Some(record) = &config.process_started
        && let Err(error) = record(process_group).await
    {
        let _ = agent::terminate_process_group(&mut child, process_group).await;
        return Err(ManagerError {
            kind: ManagerFailure::Failed,
            session_id: native,
            message: format!("record Codex SDK process group before start: {error}"),
        }
        .into());
    }
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
    let mut reader = BufReader::new(stdout);
    let operation = async {
        write_message(&mut stdin,&json!({"type":"init","prompt":prompt,"session_id":native,"cwd":config.workspace,"model":config.model,"codex_path":codex,
            "tool_cli":tool_cli,"tool_env":config.tool_env,"custom_tools":config.custom_tools})).await?;
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
                Some("event") => {
                    if let Some(event) = codex_progress(&message["event"]) {
                        progress(event).await?;
                    }
                }
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
    let mut cancellation_requested = false;
    let outcome = tokio::select! {
        biased;
        result = tokio::time::timeout(Duration::from_secs(config.timeout_secs), operation) => result.unwrap_or_else(|_|Err(anyhow!("Codex SDK management timed out; reconcile before retrying"))),
        diagnostics = async {
            match (&mut stderr_task).await {
                Ok(Ok(())) => std::future::pending::<Result<AgentResult>>().await,
                _ => Err(anyhow!("Codex SDK diagnostic stream failed")),
            }
        } => diagnostics,
        _ = agent::wait_for_cancel(&mut cancel) => {
            cancellation_requested = true;
            Err(anyhow!("Codex SDK management cancellation requested"))
        }
    };
    let mut cancellation_confirmed = false;
    if outcome.is_err() {
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            write_message(&mut stdin, &json!({"type":"cancel","session_id":native})),
        )
        .await;
        if cancellation_requested {
            cancellation_confirmed = tokio::time::timeout(Duration::from_secs(6), async {
                let mut total = 0;
                loop {
                    let message = read_message(&mut reader).await?;
                    total += serde_json::to_vec(&message)?.len();
                    ensure!(
                        total <= MAX_STREAM,
                        "Codex cancellation stream exceeded its size limit"
                    );
                    match message["type"].as_str() {
                        Some("cancelled") => {
                            ensure!(
                                message["session_id"].as_str() == native.as_deref()
                                    && message["confirmation"] == "managed_processes_exited",
                                "Codex SDK returned an invalid cancellation confirmation"
                            );
                            ensure!(
                                child.wait().await?.success(),
                                "Codex SDK sidecar failed after cancellation"
                            );
                            return Ok::<(), anyhow::Error>(());
                        }
                        Some("session") if native.is_none() => {
                            let id = message["session_id"]
                                .as_str()
                                .context("Codex SDK session event has no native ID")?;
                            vibeface_protocol::validate_id(id).map_err(anyhow::Error::msg)?;
                            native = Some(id.into());
                            if let Some(persist) = persist.take() {
                                persist(id.into()).await?;
                            }
                        }
                        Some("event") => {}
                        _ => return Err(anyhow!("Codex SDK cancellation was not confirmed")),
                    }
                }
            })
            .await
            .is_ok_and(|result| result.is_ok());
        } else {
            let _ = tokio::time::timeout(Duration::from_secs(6), child.wait()).await;
        }
        let _ = agent::terminate_process_group(&mut child, process_group).await;
    }
    stderr_task.abort();
    outcome.map_err(|error| {
        ManagerError {
            kind: if cancellation_confirmed {
                ManagerFailure::Cancelled
            } else if cancellation_requested || sent {
                ManagerFailure::Unknown
            } else {
                ManagerFailure::Failed
            },
            session_id: native,
            message: redact_string(error.to_string(), &secrets),
        }
        .into()
    })
}

fn codex_progress(event: &Value) -> Option<Value> {
    let kind = event.get("type")?.as_str()?;
    match kind {
        "item.started" | "item.updated" | "item.completed" => {
            let item = event.get("item")?;
            let id = item.get("id")?.as_str()?;
            if vibeface_protocol::validate_id(id).is_err() {
                return None;
            }
            match item.get("type")?.as_str()? {
                "agent_message" => {
                    let text: String = item
                        .get("text")?
                        .as_str()?
                        .chars()
                        .filter(|ch| !ch.is_control() || matches!(*ch, '\n' | '\r' | '\t'))
                        .take(16_384)
                        .collect();
                    Some(json!({"type":"text_snapshot","item_id":id,"text":text}))
                }
                "command_execution" | "mcp_tool_call" | "file_change" | "web_search" => {
                    let status = if kind == "item.completed" {
                        "completed"
                    } else {
                        "running"
                    };
                    let mut value =
                        json!({"type":"tool","item_id":id,"status":status,"name":item["type"]});
                    if let Some(status) =
                        item.get("status").and_then(Value::as_str).filter(|text| {
                            !text.is_empty()
                                && text.len() <= 128
                                && !text.chars().any(char::is_control)
                        })
                    {
                        value["status"] = json!(status);
                    }
                    Some(value)
                }
                _ => None,
            }
        }
        "turn.started" => Some(json!({"type":"status","status":"running"})),
        "turn.completed" => Some(json!({"type":"status","status":"finishing"})),
        "turn.failed" | "error" => Some(json!({"type":"status","status":"failed"})),
        _ => None,
    }
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
    if config.kind == ManagerKind::CodexSdk
        && let Ok(keys) = provider_environment_keys()
    {
        values.extend(
            keys.iter()
                .filter_map(|key| std::env::var(key).ok())
                .filter(|value| !value.is_empty()),
        );
    }
    values
}
fn provider_environment_keys() -> Result<Vec<String>> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".codex")));
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
            watch::channel(false).1,
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
            watch::channel(false).1,
            |_| async { Ok(()) },
        )
        .await
        .err()
        .unwrap();
        let error = error.downcast_ref::<ManagerError>().unwrap();
        assert!(error.uncertain());
        assert_eq!(error.session_id.as_deref(), Some("fixture-native"));
        tokio::fs::remove_dir_all(directory).await.unwrap();
    }

    #[test]
    fn codex_full_item_updates_are_snapshots_without_tool_arguments() {
        for (kind, status) in [
            ("turn.started", "running"),
            ("turn.completed", "finishing"),
            ("turn.failed", "failed"),
        ] {
            assert_eq!(
                codex_progress(&json!({"type":kind})),
                Some(json!({"type":"status","status":status}))
            );
        }
        let started = codex_progress(&json!({"type":"item.started","item":{"id":"m1","type":"agent_message","text":"hello"}})).unwrap();
        let updated = codex_progress(&json!({"type":"item.updated","item":{"id":"m1","type":"agent_message","text":"hello\nworld"}})).unwrap();
        assert_eq!(started["type"], "text_snapshot");
        assert_eq!(
            updated,
            json!({"type":"text_snapshot","item_id":"m1","text":"hello\nworld"})
        );
        assert_eq!(codex_progress(&json!({"type":"item.completed","item":{"id":"t1","type":"command_execution","command":"secret-token","aggregated_output":"private","status":"completed"}})).unwrap(),
            json!({"type":"tool","item_id":"t1","status":"completed","name":"command_execution"}));
        assert_eq!(codex_progress(&json!({"type":"item.started","item":{"id":"t1","type":"command_execution","status":""}})).unwrap(),
            json!({"type":"tool","item_id":"t1","status":"running","name":"command_execution"}));
        assert!(codex_progress(&json!({"type":"item.updated","item":{"id":"r1","type":"reasoning","text":"private"}})).is_none());
    }

    #[tokio::test]
    async fn cancellation_requires_bound_managed_process_confirmation() {
        let directory =
            std::env::temp_dir().join(format!("vibeface-manager-cancel-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        let script = directory.join("fixture.sh");
        // Exact lines codex-manager.mjs writes after confirmed and unconfirmed process-tree exit.
        for (reply, expected) in [
            (
                r#"{"type":"cancelled","session_id":"fixture-native","confirmation":"managed_processes_exited"}"#,
                ManagerFailure::Cancelled,
            ),
            (
                r#"{"type":"cancelled","session_id":"wrong-native","confirmation":"managed_processes_exited"}"#,
                ManagerFailure::Unknown,
            ),
            (
                r#"{"type":"error","message":"Codex cancellation could not confirm managed process exit","uncertain":true}"#,
                ManagerFailure::Unknown,
            ),
        ] {
            let fixture = format!(
                r#"IFS= read -r init || exit 1
printf '%s\n' '{{"type":"ready","session_id":"fixture-native"}}'
IFS= read -r ack || exit 2
printf '%s\n' '{{"type":"event","event":{{"type":"turn.started"}}}}'
IFS= read -r cancel || exit 3
case "$cancel" in *cancel*) ;; *) exit 4 ;; esac
printf '%s\n' '{reply}'
"#
            );
            tokio::fs::write(&script, fixture).await.unwrap();
            let config = ManagerConfig {
                kind: ManagerKind::CodexSdk,
                workspace: directory.clone(),
                node_bin: "/bin/sh".into(),
                codex_bin: "/usr/bin/true".into(),
                codex_sdk_script: script.clone(),
                timeout_secs: 5,
                ..ManagerConfig::default()
            };
            let (cancel_tx, cancel_rx) = watch::channel(false);
            let error = run_manager(
                &config,
                "fixture",
                Some("fixture-native"),
                |_| async { Ok(()) },
                cancel_rx,
                move |event| {
                    assert_eq!(event, json!({"type":"status","status":"running"}));
                    cancel_tx.send(true).unwrap();
                    async { Ok(()) }
                },
            )
            .await
            .err()
            .unwrap();
            assert_eq!(error.downcast_ref::<ManagerError>().unwrap().kind, expected);
        }
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
