//! Management SDKs receive visible context and explicit Node capabilities.
use crate::engine::{Engine, ExecutionContext};
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, future::Future, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
use vibeface_bridge::agent::{AgentResult, CustomToolConfig};

const MAX_LINE: usize = 1_048_576;
const MAX_STREAM: usize = 16_777_216;

fn validate_callback(config: &ManagerConfig) -> Result<()> {
    if config.custom_tools.is_empty() {
        return Ok(());
    }
    let url = reqwest::Url::parse(
        config
            .tool_callback_url
            .as_deref()
            .context("manager tools require a scoped Node callback")?,
    )?;
    let ip = url
        .host_str()
        .context("Node callback host missing")?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()?;
    ensure!(
        url.scheme() == "http"
            && ip.is_loopback()
            && url.port().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "manager callback must be a fixed loopback HTTP origin"
    );
    let token = config
        .tool_callback_token
        .as_deref()
        .context("manager tools require a scoped callback token")?;
    ensure!(
        (32..=4096).contains(&token.len()) && !token.chars().any(char::is_control),
        "invalid manager callback token"
    );
    Ok(())
}

async fn call_tool(config: &ManagerConfig, message: &Value) -> Result<Value> {
    let name = message["name"].as_str().context("tool name missing")?;
    let call = message["call_id"]
        .as_str()
        .context("tool call ID missing")?;
    vibeface_protocol::validate_id(call).map_err(anyhow::Error::msg)?;
    ensure!(
        config.custom_tools.contains_key(name)
            && message["arguments"].is_object()
            && message.as_object().is_some_and(|object| object
                .keys()
                .all(|key| matches!(key.as_str(), "type" | "name" | "call_id" | "arguments"))),
        "manager tool is outside its fixed catalog"
    );
    validate_callback(config)?;
    let url = reqwest::Url::parse(
        config
            .tool_callback_url
            .as_deref()
            .context("manager callback missing")?,
    )?
    .join("api/tools")?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    let mut response = client
        .post(url)
        .bearer_auth(config.tool_callback_token.as_deref().unwrap())
        .json(&json!({"name":name,"arguments":message["arguments"],"call_id":call}))
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "scoped Node callback rejected manager tool"
    );
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len() + chunk.len() <= MAX_LINE,
            "manager tool result exceeded its limit"
        );
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).context("Node callback returned invalid JSON")
}

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
    #[serde(default)]
    pub custom_tools: BTreeMap<String, CustomToolConfig>,
    pub tool_callback_url: Option<String>,
    #[serde(skip)]
    pub tool_callback_token: Option<String>,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            kind: ManagerKind::CodexSdk,
            workspace: ".".into(),
            model: "auto".into(),
            timeout_secs: 600,
            custom_tools: BTreeMap::new(),
            tool_callback_url: None,
            tool_callback_token: None,
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

pub async fn run_manager_authorized<F, P, PF>(
    engine: &Engine,
    context: &ExecutionContext,
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
    ensure!(
        config.kind == ManagerKind::CodexSdk,
        "this management SDK has no verified isolated model and tool transport"
    );
    ensure!(config.timeout_secs > 0, "manager timeout must be positive");
    ensure!(
        !prompt.trim().is_empty() && prompt.len() <= 65_536,
        "management prompt must be nonempty and at most 65536 bytes"
    );
    if let Some(id) = previous_session_id {
        vibeface_protocol::validate_id(id).map_err(anyhow::Error::msg)?;
    }
    validate_callback(config)?;
    let isolated = engine.prepare_manager(context, &config.workspace).await?;
    let mut command = isolated.command("node");
    command.arg("/opt/vibeface/sdk/codex-manager.mjs");
    let result = run_codex(
        config,
        command,
        "/project",
        "/usr/local/bin/codex",
        prompt,
        previous_session_id,
        persist_session,
        progress,
    )
    .await;
    match &result {
        Ok(_) => {
            if let Err(error) = engine.complete_manager(context).await {
                return Err(ManagerError {
                    uncertain: true,
                    session_id: result
                        .as_ref()
                        .ok()
                        .and_then(|result| result.session_id.clone()),
                    message: format!("manager process stop was not confirmed: {error}"),
                }
                .into());
            }
        }
        Err(_) => {
            let _ = engine.cancel_manager(context).await;
        }
    }
    result
}

async fn run_codex<F, P, PF>(
    config: &ManagerConfig,
    mut command: Command,
    cwd: &str,
    codex: &str,
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
    command
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
        write_message(&mut stdin,&json!({"type":"init","prompt":prompt,"session_id":native,"cwd":cwd,"model":config.model,"codex_path":codex,
            "tools":config.custom_tools.keys().collect::<Vec<_>>()})).await?;
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
                Some("tool_call") => {
                    let reply = call_tool(config, &message).await;
                    let result = match reply {
                        Ok(value) => value,
                        Err(_) => json!({"error":"Node tool rejected or unavailable"}),
                    };
                    write_message(
                        &mut stdin,
                        &json!({"type":"tool_result","call_id":message["call_id"],"result":result}),
                    )
                    .await?;
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
    let mut values = Vec::new();
    if let Some(token) = &config.tool_callback_token {
        values.push(token.clone());
    }
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
            timeout_secs: 5,
            ..ManagerConfig::default()
        };
        let marker = directory.join("persisted");
        let mut command = Command::new("/bin/sh");
        command.arg(&script);
        let result = run_codex(
            &config,
            command,
            directory.to_str().unwrap(),
            "/usr/bin/true",
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
        let mut command = Command::new("/bin/sh");
        command.arg(&script);
        let error = run_codex(
            &config,
            command,
            directory.to_str().unwrap(),
            "/usr/bin/true",
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
            ..ManagerConfig::default()
        };
        let encoded = serde_json::to_string(&config).unwrap();
        assert!(!encoded.contains("secret-token"));
        assert_eq!(
            redact(json!({"nested":["secret-token"]}), &secret_values(&config)),
            json!({"nested":["[redacted]"]})
        );
    }

    #[tokio::test]
    async fn callbacks_require_fixed_loopback_origin_and_the_exact_catalog() {
        let mut config = ManagerConfig {
            custom_tools: BTreeMap::from([(
                "nodes".into(),
                CustomToolConfig {
                    description: None,
                    input_schema: json!({"type":"object"}),
                    output_schema: None,
                },
            )]),
            tool_callback_token: Some("fixture-scoped-callback-token-1234567890".into()),
            ..ManagerConfig::default()
        };
        for url in [
            "http://example.com:1234/",
            "http://127.0.0.1:1234/private",
            "http://127.0.0.1:1234/?next=x",
            "http://user@127.0.0.1:1234/",
            "https://127.0.0.1:1234/",
        ] {
            config.tool_callback_url = Some(url.into());
            assert!(validate_callback(&config).is_err());
        }
        for url in ["http://127.0.0.1:1234/", "http://[::1]:1234/"] {
            config.tool_callback_url = Some(url.into());
            assert!(validate_callback(&config).is_ok());
        }
        assert!(
            call_tool(
                &config,
                &json!({"type":"tool_call","call_id":"fixture","name":"forbidden","arguments":{}})
            )
            .await
            .is_err()
        );
        assert!(call_tool(&config, &json!({"type":"tool_call","call_id":"fixture","name":"nodes","arguments":{},"url":"http://example.com"})).await.is_err());
    }

    #[tokio::test]
    #[ignore = "requires the Manager fixture image with the pinned real SDK and local Docker"]
    async fn isolated_manager_tools_and_process_lifecycle_are_scoped() -> Result<()> {
        use crate::engine::{ExecutionIsolationConfig, ExecutorConfig};
        use axum::{Json, Router, routing::post};
        use sha2::{Digest, Sha256};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use vibeface_secure::authorization::{ResourceId, SpaceId};
        let root = PathBuf::from(std::env::var("VIBEFACE_ISOLATION_TEST_ROOT")?)
            .join(uuid::Uuid::new_v4().to_string());
        let data = root.join("node");
        let project = root.join("project");
        crate::config::private_dir(&data)?;
        std::fs::create_dir(&project)?;
        std::fs::write(project.join("visible"), "visible")?;
        let secret = root.join("host-secret");
        std::fs::write(&secret, "fixture-host-secret")?;
        std::os::unix::fs::symlink(&secret, project.join("escape"))?;
        let token = "fixture-scoped-callback-token-1234567890";
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let router = Router::new().route(
            "/api/tools",
            post(
                move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
                    let observed = observed.clone();
                    async move {
                        assert_eq!(headers["authorization"], format!("Bearer {token}"));
                        assert_eq!(body["name"], "nodes");
                        assert_eq!(body["arguments"], json!({}));
                        assert!(
                            body["call_id"]
                                .as_str()
                                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
                        );
                        observed.fetch_add(1, Ordering::SeqCst);
                        Json(json!({"scope":"fixture-manager-scope"}))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let callback = format!("http://{}/", listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let engine = Engine::new(
            ExecutorConfig {
                execution_isolation: Some(ExecutionIsolationConfig {
                    image: std::env::var("VIBEFACE_ISOLATION_TEST_IMAGE")?,
                    docker_bin: std::env::var("VIBEFACE_ISOLATION_TEST_DOCKER")?.into(),
                    egress: None,
                }),
                data_dir: Some(data.clone()),
                ..ExecutorConfig::default()
            },
            events,
        );
        let config = ManagerConfig {
            kind: ManagerKind::CodexSdk,
            workspace: project.clone(),
            model: "fixture-model".into(),
            timeout_secs: 20,
            custom_tools: BTreeMap::from([(
                "nodes".into(),
                CustomToolConfig {
                    description: None,
                    input_schema: json!({"type":"object"}),
                    output_schema: None,
                },
            )]),
            tool_callback_url: Some(callback.clone()),
            tool_callback_token: Some(token.into()),
        };
        let mut contexts = Vec::new();
        for (index, operation) in ["manager-complete", "manager-cancel", "manager-expire"]
            .into_iter()
            .enumerate()
        {
            contexts.push(ExecutionContext {
                space_id: SpaceId::parse(&format!("spc2_{}", "a".repeat(64)))?,
                resource_id: ResourceId::parse(&format!(
                    "res2_{}",
                    (index + 1).to_string().repeat(64)
                ))?,
                operation_id: operation.into(),
                shared: true,
                lease_expires_at_ms: vibeface_protocol::now_ms() + 30_000,
            });
        }
        let home = |context: &ExecutionContext| {
            let key = hex::encode(Sha256::digest(format!(
                "manager\0{}\0{}",
                context.space_id, context.resource_id
            )));
            data.join("execution").join(key).join("home")
        };
        let outcome = async {
            let prompt = format!("manager_fixture={}", json!({"secret":secret,"scope":"fixture-manager-scope","callback":callback}));
            let result = run_manager_authorized(&engine, &contexts[0], &config, &prompt, None, |id| async move { ensure!(id == "fixture-manager-native", "unexpected native session"); Ok(()) }, |_| async { Ok(()) }).await?;
            let proof: Value = serde_json::from_str(&result.output)?;
            ensure!(proof.as_object().is_some_and(|p| p.len() == 11 && p.values().all(|v| v == true)), "Manager isolation proof failed: {proof}");
            ensure!(calls.load(Ordering::SeqCst) == 1, "unexpected Node tool calls");
            tokio::time::sleep(Duration::from_millis(3300)).await;
            ensure!(!home(&contexts[0]).join("background-write").exists(), "terminal Manager left a background process");
            ensure!(!project.join("changed").exists(), "Manager changed its read-only project");
            let mut negative = Vec::new();
            for (index, cancel) in [(1, true), (2, false)] {
                let mut context = contexts[index].clone();
                context.lease_expires_at_ms = vibeface_protocol::now_ms() + if cancel { 30_000 } else { 5000 };
                let process = engine.prepare_manager(&context, &project).await?;
                let waiting = home(&context).join("waiting");
                let prompt = "manager_fixture={\"wait\":true}";
                let run = run_manager_authorized(&engine, &context, &config, prompt, None, |_| async { Ok(()) }, |_| async { Ok(()) });
                let stop = async {
                    tokio::time::timeout(Duration::from_secs(4), async {
                        while !waiting.exists() { tokio::time::sleep(Duration::from_millis(20)).await; }
                    }).await?;
                    if cancel { engine.cancel_manager(&context).await?; }
                    Ok::<_, anyhow::Error>(())
                };
                let (result, stopped) = tokio::join!(run, stop);
                stopped?;
                let error = result.err().context("stopped Manager unexpectedly succeeded")?;
                ensure!(error.downcast_ref::<ManagerError>().is_some_and(|e| e.uncertain), "stopped Manager did not preserve Unknown: {error}");
                let status = process.command("true").status().await?;
                ensure!(!status.success(), "stopped Manager container still accepts execution");
                negative.push(json!({"cause":if cancel {"cancel"} else {"lease_expiry"},"unknown":true,"container_stopped":true}));
            }
            println!("manager-proof={}", json!({"sdk":"@openai/codex-sdk 0.163.0","cli":"deterministic fixture, no real model","isolation":proof,"callback_calls":calls.load(Ordering::SeqCst),"background_process_removed":true,"stop_cases":negative}));
            Ok::<_, anyhow::Error>(())
        }.await;
        for context in &contexts {
            let _ = engine.cancel_manager(context).await;
            let _ = engine.complete_manager(context).await;
        }
        server.abort();
        std::fs::remove_dir_all(root)?;
        outcome
    }
}
