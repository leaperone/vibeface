//! Cursor's official local Agent over ACP. Authentication remains with the official CLI;
//! this Node owns private ACP stores and a durable prompt journal. ACP has no native run query,
//! so a submitted prompt without a recorded terminal stays unknown and is never sent again.
use crate::{
    failure::{ExecFailure, FailureKind, ProviderStage},
    platform,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    future::Future,
    net::IpAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Child,
    sync::{Mutex, mpsc, watch},
};

const MAX_FRAME_BYTES: usize = 1_048_576;
const MAX_RESULT_BYTES: usize = 65_536;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(6);
static NEXT_PROCESS: AtomicU64 = AtomicU64::new(0);

pub const CURSOR_AUTH_OVERRIDES: [&str; 3] = [
    "CURSOR_API_KEY",
    "CURSOR_AUTH_TOKEN",
    "AGENT_CLI_CREDENTIAL_STORE",
];

pub fn is_cursor_sdk_bridge(executable: &Path) -> bool {
    executable
        .file_stem()
        .is_some_and(|name| name == "cursor-sdk-bridge")
}

#[derive(Clone, Copy)]
enum CursorCommand {
    Acp,
    Status,
}

pub type ProcessStarted =
    Arc<dyn Fn(u32) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;
/// Called after actual provider activity is observed. Cursor supplies its Node prompt-receipt
/// locator, not a native run ID: ACP has no such ID or durable run-query contract.
pub type RunStarted =
    Arc<dyn Fn(String) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;

#[derive(Clone)]
pub struct CursorTurn {
    pub owner: String,
    pub client_id: String,
    pub executable: PathBuf,
    pub workspace: PathBuf,
    pub local_store_root: PathBuf,
    pub env: BTreeMap<String, String>,
    pub model: String,
    pub timeout_secs: u64,
    pub custom_tools: BTreeMap<String, CustomToolConfig>,
    pub callback_token: Option<String>,
    pub process_started: Option<ProcessStarted>,
    pub run_started: Option<RunStarted>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CustomToolConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

pub struct AgentResult {
    pub session_id: Option<String>,
    pub output: String,
    pub usage: Option<Value>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CursorAuthentication {
    Unknown,
    Authenticated,
    SignInRequired,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorModel {
    pub id: String,
    pub display_name: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CursorCatalog {
    Unavailable,
    Available { models: Vec<CursorModel> },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorObservation {
    pub launchable: bool,
    pub authentication: CursorAuthentication,
    pub catalog: CursorCatalog,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentFailure {
    Failed,
    Cancelled,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartEvidence {
    NotStarted,
    Started,
    Unknown,
}
#[derive(Debug)]
pub struct AgentRunError {
    pub stage: ProviderStage,
    pub outcome: AgentFailure,
    pub start: StartEvidence,
    pub session_id: Option<String>,
    pub failure: ExecFailure,
}
impl std::fmt::Display for AgentRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.failure.fmt(f)
    }
}
impl std::error::Error for AgentRunError {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PromptStatus {
    Submitted,
    Running,
    Completed,
    Cancelled,
    Failed { detail: String },
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorPrompt {
    pub session_id: String,
    pub receipt_id: String,
    pub status: PromptStatus,
    pub output: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptReceipt {
    session: String,
    id: String,
    input_hash: String,
    status: PromptStatus,
    output: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionOwner {
    session: String,
    owner: String,
    workspace: PathBuf,
}

pub struct CursorAgent {
    env: BTreeMap<String, String>,
    tool_origin: String,
    processes: Arc<Processes>,
    owners: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    active: std::sync::Mutex<BTreeMap<(PathBuf, String), watch::Sender<bool>>>,
    stopped: AtomicBool,
}
struct Processes {
    pidfile: Option<PathBuf>,
    running: std::sync::Mutex<BTreeMap<String, Tracked>>,
}
#[derive(Clone)]
struct Tracked {
    executable: PathBuf,
    identity: ProcessIdentity,
    kill: watch::Sender<bool>,
    exited: watch::Receiver<bool>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum ProcessIdentity {
    #[cfg(unix)]
    UnixStart(String),
    #[cfg(windows)]
    WindowsJob,
    Exited,
}
#[derive(Serialize, Deserialize)]
struct ProcessRecord {
    token: String,
    executable: PathBuf,
    identity: ProcessIdentity,
}

#[cfg(unix)]
fn process_start(process: u32) -> Result<String> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "lstart=", "-p", &process.to_string()])
        .env("LC_ALL", "C")
        .output()?;
    ensure!(
        output.status.success() && output.stdout.len() <= 128,
        "could not identify the Cursor process start"
    );
    let value = String::from_utf8(output.stdout)?.trim().to_owned();
    ensure!(!value.is_empty(), "Cursor process already exited");
    Ok(value)
}
impl Processes {
    /// Writes this run's processes plus any earlier record whose tree is not confirmed gone, so
    /// a tree an earlier start could not end is still there to try again after the next start.
    fn save(&self, running: &BTreeMap<String, Tracked>) -> Result<()> {
        let Some(path) = &self.pidfile else {
            return Ok(());
        };
        let mut records: Vec<_> = running
            .iter()
            .map(|(token, tracked)| ProcessRecord {
                token: token.clone(),
                executable: tracked.executable.clone(),
                identity: tracked.identity.clone(),
            })
            .collect();
        if let Ok(bytes) = std::fs::read(path)
            && let Ok(earlier) = serde_json::from_slice::<Vec<ProcessRecord>>(&bytes)
        {
            records.extend(earlier.into_iter().filter(|record| {
                !running.contains_key(&record.token)
                    && !matches!(platform::tree::Leftover::find(&record.token), Ok(None))
            }));
        }
        if records.is_empty() {
            return match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            };
        }
        platform::privatefs::write_atomic(path, &serde_json::to_vec(&records)?)
    }
    fn track(&self, token: String, tracked: Tracked) -> Result<()> {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        running.insert(token, tracked);
        self.save(&running)
    }
    fn forget(&self, token: &str) {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        running.remove(token);
        let _ = self.save(&running);
    }
}
struct Rpc {
    group: u32,
    outgoing: mpsc::Sender<Value>,
    incoming: mpsc::Receiver<Value>,
    kill: watch::Sender<bool>,
    exited: watch::Receiver<bool>,
}
impl Drop for Rpc {
    fn drop(&mut self) {
        let _ = self.kill.send(true);
    }
}
impl Rpc {
    async fn send(&self, value: Value) -> Result<()> {
        self.outgoing
            .send(value)
            .await
            .map_err(|_| anyhow!("Cursor ACP transport closed"))
    }
    async fn close(mut self) {
        let _ = self.kill.send(true);
        let _ = self.exited.wait_for(|v| *v).await;
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn receipt_id(client_id: &str) -> String {
    format!("acp-{}", digest(client_id.as_bytes()))
}
fn receipt_path(root: &Path, id: &str) -> Result<PathBuf> {
    vibeface_protocol::validate_id(id).map_err(anyhow::Error::msg)?;
    Ok(root.join("prompts").join(format!("{id}.json")))
}
fn write_receipt(root: &Path, receipt: &PromptReceipt) -> Result<()> {
    let path = receipt_path(root, &receipt.id)?;
    platform::privatefs::create_dir(path.parent().context("prompt journal has no parent")?)?;
    platform::privatefs::write_atomic(&path, &serde_json::to_vec(receipt)?)
}
fn read_receipt(root: &Path, id: &str) -> Result<Option<PromptReceipt>> {
    let file = match platform::privatefs::open_no_follow(&receipt_path(root, id)?) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("could not read Cursor prompt journal"),
    };
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take((MAX_RESULT_BYTES * 2) as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_RESULT_BYTES * 2,
        "Cursor prompt journal is too large"
    );
    let receipt: PromptReceipt =
        serde_json::from_slice(&bytes).map_err(|_| anyhow!("invalid Cursor prompt journal"))?;
    ensure!(receipt.id == id, "Cursor prompt journal identity changed");
    Ok(Some(receipt))
}
fn owner_path(turn: &CursorTurn, session: &str) -> Result<PathBuf> {
    vibeface_protocol::validate_id(session).map_err(anyhow::Error::msg)?;
    Ok(turn
        .local_store_root
        .join("sessions")
        .join(format!("{session}.json")))
}
fn validate_session_owner(turn: &CursorTurn, session: &str) -> Result<()> {
    let file = platform::privatefs::open_no_follow(&owner_path(turn, session)?)
        .map_err(|_| anyhow!("Cursor session is not owned by this Node instance"))?;
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 8192, "invalid Cursor session owner");
    let owner: SessionOwner =
        serde_json::from_slice(&bytes).map_err(|_| anyhow!("invalid Cursor session owner"))?;
    ensure!(
        owner.session == session && owner.owner == turn.owner && owner.workspace == turn.workspace,
        "Cursor session belongs to a different owner or workspace"
    );
    Ok(())
}
fn record_session_owner(turn: &CursorTurn, session: &str) -> Result<()> {
    let path = owner_path(turn, session)?;
    platform::privatefs::create_dir(path.parent().context("session owner has no parent")?)?;
    platform::privatefs::write_atomic(
        &path,
        &serde_json::to_vec(&SessionOwner {
            session: session.into(),
            owner: turn.owner.clone(),
            workspace: turn.workspace.clone(),
        })?,
    )
}

impl CursorAgent {
    pub fn new(
        env: BTreeMap<String, String>,
        tool_origin: String,
        pidfile: Option<PathBuf>,
    ) -> Result<Self> {
        validate_origin(&tool_origin)?;
        ensure!(
            env.len() <= 64
                && env.iter().all(|(name, value)| name.starts_with("VIBEFACE_")
                    && name != "VIBEFACE_TOOL_TOKEN"
                    && name.len() <= 256
                    && name.bytes().all(|c| c == b'_' || c.is_ascii_alphanumeric())
                    && value.len() <= 4096
                    && !value.contains('\0')),
            "invalid Cursor host tool environment"
        );
        Ok(Self {
            env,
            tool_origin,
            processes: Arc::new(Processes {
                pidfile,
                running: Default::default(),
            }),
            owners: Mutex::default(),
            active: Default::default(),
            stopped: AtomicBool::new(false),
        })
    }
    pub fn kill_orphan(pidfile: &Path) {
        let Ok(bytes) = std::fs::read(pidfile) else {
            return;
        };
        let Ok(records) = serde_json::from_slice::<Vec<ProcessRecord>>(&bytes) else {
            return;
        };
        let mut unconfirmed = Vec::new();
        for record in records {
            let same = match &record.identity {
                #[cfg(unix)]
                ProcessIdentity::UnixStart(expected) => record
                    .token
                    .parse()
                    .ok()
                    .and_then(|pid| process_start(pid).ok())
                    .is_some_and(|current| current == *expected),
                #[cfg(windows)]
                ProcessIdentity::WindowsJob => true,
                ProcessIdentity::Exited => false,
            };
            if same {
                #[cfg(unix)]
                let accepted = |member: &str| {
                    member.contains(&record.executable.to_string_lossy().to_string())
                };
                // The Windows job token already binds the root's creation time and job key.
                #[cfg(windows)]
                let accepted = |_: &str| true;
                let _ = platform::tree::stop_recorded(&record.token, accepted);
            }
            // A tree that cannot be confirmed gone stays on record for the next start.
            if same && !matches!(platform::tree::Leftover::find(&record.token), Ok(None)) {
                unconfirmed.push(record);
            }
        }
        if unconfirmed.is_empty() {
            let _ = std::fs::remove_file(pidfile);
        } else if let Ok(bytes) = serde_json::to_vec(&unconfirmed)
            && let Err(error) = platform::privatefs::write_atomic(pidfile, &bytes)
        {
            tracing::warn!(%error, "could not preserve the unconfirmed process journal");
        }
    }
    pub async fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.retire_all().await;
    }
    pub async fn retire_all(&self) {
        let running: Vec<_> = self
            .processes
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for tracked in &running {
            let _ = tracked.kill.send(true);
        }
        for mut tracked in running {
            let _ = tracked.exited.wait_for(|v| *v).await;
        }
    }
    async fn spawn(
        &self,
        executable: &Path,
        workspace: &Path,
        root: &Path,
        env: &BTreeMap<String, String>,
        model: &str,
        mode: CursorCommand,
    ) -> Result<Rpc> {
        ensure!(
            !self.stopped.load(Ordering::SeqCst),
            "Cursor agent is stopped"
        );
        ensure!(
            !is_cursor_sdk_bridge(executable),
            "Cursor requires the official agent or cursor-agent CLI"
        );
        ensure!(
            workspace.is_absolute() && workspace.is_dir() && root.is_absolute(),
            "Cursor needs an existing absolute workspace and private store"
        );
        for directory in [root.to_path_buf(), root.join("config"), root.join("data")] {
            platform::privatefs::create_dir(&directory)?;
        }
        validate_launch_env(env)?;
        let mut command = std::process::Command::new(executable);
        command
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, _) in std::env::vars_os() {
            let text = name.to_string_lossy();
            if text.starts_with("VIBEFACE_")
                || text.starts_with("CURSOR_")
                || text == "AGENT_CLI_CREDENTIAL_STORE"
            {
                command.env_remove(name);
            }
        }
        command
            .envs(env)
            .envs(&self.env)
            .env("CURSOR_CONFIG_DIR", root.join("config"))
            .env("CURSOR_DATA_DIR", root.join("data"));
        for name in CURSOR_AUTH_OVERRIDES {
            command.env_remove(name);
        }
        if !model.is_empty() {
            command.args(["--model", model]);
        }
        match mode {
            CursorCommand::Acp => {
                command.arg("acp");
            }
            CursorCommand::Status => {
                command.args(["status", "--format", "json"]);
            }
        }
        let key = format!(
            "cursor-acp-{}-{}-{}",
            std::process::id(),
            NEXT_PROCESS.fetch_add(1, Ordering::SeqCst),
            digest(root.to_string_lossy().as_bytes())
        );
        let (mut tree, mut child) = platform::tree::Tree::spawn(command, &key, true)?;
        let group = child.id().context("Cursor ACP process has no ID")?;
        #[cfg(unix)]
        let identity = match process_start(group) {
            Ok(start) => ProcessIdentity::UnixStart(start),
            Err(error) => {
                if child.try_wait()?.is_some() {
                    ProcessIdentity::Exited
                } else {
                    tree.kill();
                    let _ = child.wait().await;
                    return Err(error);
                }
            }
        };
        #[cfg(windows)]
        let identity = ProcessIdentity::WindowsJob;
        let token = tree.token().to_owned();
        let mut stdin = child.stdin.take().context("Cursor ACP stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("Cursor ACP stdout unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("Cursor ACP stderr unavailable")?;
        let (outgoing, mut requests) = mpsc::channel::<Value>(32);
        let (responses, incoming) = mpsc::channel(128);
        let (kill, mut killed) = watch::channel(false);
        let (exit, exited) = watch::channel(false);
        if let Err(error) = self.processes.track(
            token.clone(),
            Tracked {
                executable: executable.into(),
                identity,
                kill: kill.clone(),
                exited: exited.clone(),
            },
        ) {
            tree.kill();
            let _ = child.wait().await;
            return Err(error);
        }
        let processes = self.processes.clone();
        tokio::spawn(async move {
            let mut stdout_joined = false;
            let mut stdout_task = tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                if matches!(mode, CursorCommand::Status) {
                    let mut bytes = Vec::new();
                    reader.take(65_537).read_to_end(&mut bytes).await?;
                    ensure!(bytes.len() <= 65_536, "Cursor status response is too large");
                    let value: Value = serde_json::from_slice(&bytes)
                        .map_err(|_| anyhow!("Cursor status response is invalid"))?;
                    let _ = responses.send(value).await;
                    return Ok::<(), anyhow::Error>(());
                }
                loop {
                    let frame = bounded_line(&mut reader, MAX_FRAME_BYTES).await?;
                    ensure!(!frame.is_empty(), "Cursor ACP stdout closed");
                    let value: Value = serde_json::from_slice(&frame)
                        .map_err(|_| anyhow!("Cursor ACP returned invalid JSON"))?;
                    ensure!(
                        value.is_object() && value["jsonrpc"] == "2.0",
                        "invalid Cursor ACP message"
                    );
                    if responses.send(value).await.is_err() {
                        return Ok::<(), anyhow::Error>(());
                    }
                }
            });
            let stderr_task = tokio::spawn(async move {
                let mut stderr = stderr;
                let mut chunk = [0u8; 4096];
                while matches!(stderr.read(&mut chunk).await, Ok(n) if n > 0) {}
            });
            loop {
                tokio::select! {
                    biased;
                    _ = wait_for_cancel(&mut killed) => break,
                    _ = &mut stdout_task => { stdout_joined = true; break; },
                    value = requests.recv() => {
                        let Some(value) = value else { break; };
                        let Ok(mut bytes) = serde_json::to_vec(&value) else { break; };
                        if bytes.len() > MAX_FRAME_BYTES { break; }
                        bytes.push(b'\n');
                        let written = tokio::select! { result = stdin.write_all(&bytes) => result.is_ok(), _ = wait_for_cancel(&mut killed) => false };
                        if !written { break; }
                    }
                }
            }
            let _ = terminate_process_tree(&mut child, &mut tree).await;
            stdout_task.abort();
            stderr_task.abort();
            if !stdout_joined {
                let _ = stdout_task.await;
            }
            let _ = stderr_task.await;
            drop(stdin);
            drop(child);
            drop(tree);
            processes.forget(&token);
            let _ = exit.send(true);
        });
        Ok(Rpc {
            group,
            outgoing,
            incoming,
            kill,
            exited,
        })
    }
    async fn subscription_authentication(
        &self,
        executable: &Path,
        workspace: &Path,
        root: &Path,
        env: &BTreeMap<String, String>,
    ) -> Result<CursorAuthentication> {
        let mut rpc = self
            .spawn(executable, workspace, root, env, "", CursorCommand::Status)
            .await?;
        let result = tokio::time::timeout(Duration::from_secs(5), rpc.incoming.recv())
            .await
            .ok()
            .flatten();
        rpc.close().await;
        let Some(value) = result else {
            return Ok(CursorAuthentication::Unknown);
        };
        // status exits 0 for authenticated, partial and signed-out states. Discard identity,
        // messages and all other fields; only its explicit local-login schema is evidence.
        let valid = matches!(value["isAuthenticated"], Value::Bool(_))
            && matches!(value["hasAccessToken"], Value::Bool(_))
            && matches!(value["hasRefreshToken"], Value::Bool(_));
        Ok(
            if valid
                && value["status"] == "authenticated"
                && value["isAuthenticated"] == true
                && value["hasAccessToken"] == true
                && value["hasRefreshToken"] == true
            {
                CursorAuthentication::Authenticated
            } else if valid
                && value["isAuthenticated"] == false
                && matches!(
                    value["status"].as_str(),
                    Some("partially-authenticated" | "unauthenticated")
                )
            {
                CursorAuthentication::SignInRequired
            } else {
                CursorAuthentication::Unknown
            },
        )
    }
    async fn initialize(&self, rpc: &mut Rpc) -> Result<Value> {
        rpc.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"vibeface","version":env!("CARGO_PKG_VERSION")},"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}})).await?;
        let response = rpc_response(rpc, 1, None, false, &mut |_| async { Ok(()) }).await?;
        ensure!(
            response["protocolVersion"] == 1,
            "Cursor ACP protocol is unsupported"
        );
        Ok(response)
    }
    fn mcp_servers(&self, capabilities: &Value, turn: &CursorTurn) -> Result<Value> {
        if turn.custom_tools.is_empty() {
            return Ok(json!([]));
        }
        ensure!(
            capabilities["agentCapabilities"]["mcpCapabilities"]["http"] == true,
            "Cursor ACP does not support HTTP MCP tools"
        );
        let token = turn
            .callback_token
            .as_ref()
            .context("Cursor tools need a scoped attempt bearer")?;
        ensure!(
            (32..=4096).contains(&token.len()) && !token.chars().any(char::is_control),
            "invalid Cursor tool bearer"
        );
        Ok(
            json!([{"type":"http","name":"vibeface","url":format!("{}/mcp", self.tool_origin.trim_end_matches('/')),"headers":[{"name":"Authorization","value":format!("Bearer {token}")}]}]),
        )
    }
    pub async fn provider_observation(
        &self,
        executable: &Path,
        workspace: &Path,
        root: &Path,
        env: &BTreeMap<String, String>,
    ) -> Result<CursorObservation> {
        let authentication = self
            .subscription_authentication(executable, workspace, root, env)
            .await?;
        if authentication != CursorAuthentication::Authenticated {
            return Ok(CursorObservation {
                launchable: false,
                authentication,
                catalog: CursorCatalog::Unavailable,
            });
        }
        let mut rpc = self
            .spawn(executable, workspace, root, env, "", CursorCommand::Acp)
            .await?;
        let operation = async {
            self.initialize(&mut rpc).await?;
            // The official CLI checks its existing authentication on session/new. Calling
            // authenticate would start browser login when credentials are missing.
            rpc.send(json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":workspace,"mcpServers":[]}})).await?;
            match rpc_response(&mut rpc, 3, None, false, &mut |_| async { Ok(()) }).await {
                Ok(response) => Ok(CursorObservation {
                    launchable: true,
                    authentication: CursorAuthentication::Authenticated,
                    catalog: cursor_models(&response, env),
                }),
                Err(error)
                    if error
                        .downcast_ref::<AcpError>()
                        .is_some_and(|e| e.code == -32000) =>
                {
                    Ok(CursorObservation {
                        launchable: true,
                        authentication: CursorAuthentication::SignInRequired,
                        catalog: CursorCatalog::Unavailable,
                    })
                }
                Err(_) => Ok(CursorObservation {
                    launchable: true,
                    authentication,
                    catalog: CursorCatalog::Unavailable,
                }),
            }
        };
        let result = tokio::time::timeout(STARTUP_TIMEOUT, operation)
            .await
            .map_err(|_| anyhow!("Cursor authentication check timed out"))?;
        rpc.close().await;
        result
    }
    pub async fn run<F, P, PF>(
        &self,
        turn: &CursorTurn,
        prompt: &str,
        previous: Option<&str>,
        persist_session: impl FnOnce(String) -> F,
        mut cancel: watch::Receiver<bool>,
        mut progress: P,
    ) -> Result<AgentResult, AgentRunError>
    where
        F: Future<Output = Result<()>>,
        P: FnMut(Value) -> PF,
        PF: Future<Output = Result<()>>,
    {
        let mut stage = ProviderStage::Validate;
        let mut session = previous.map(str::to_owned);
        let mut submitted = false;
        let mut observed = false;
        let secrets: Vec<&str> = turn
            .env
            .iter()
            .chain(self.env.iter())
            .filter(|(name, value)| credential_value(name, value))
            .map(|(_, value)| value.as_str())
            .chain(turn.callback_token.as_deref())
            .collect();
        let operation = async {
            validate_turn(turn, prompt)?;
            let id = receipt_id(&turn.client_id);
            let input_hash = digest(&serde_json::to_vec(&(
                prompt,
                &turn.model,
                &turn.workspace,
            ))?);
            let owner = {
                let mut owners = self.owners.lock().await;
                owners.retain(|_, owner| Arc::strong_count(owner) > 1);
                owners
                    .entry(turn.owner.clone())
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone()
            };
            let _owner = tokio::select! { guard = owner.lock() => guard, _ = wait_for_cancel(&mut cancel) => bail!("Cursor cancelled before prompt submission") };
            if let Some(receipt) = read_receipt(&turn.local_store_root, &id)? {
                ensure!(
                    receipt.input_hash == input_hash,
                    "Cursor prompt reference was reused with different input"
                );
                session = Some(receipt.session.clone());
                if receipt.status == PromptStatus::Completed {
                    return Ok(AgentResult {
                        session_id: session.clone(),
                        output: receipt.output,
                        usage: None,
                    });
                }
                submitted = true;
                bail!(
                    "Cursor prompt was already submitted; reconcile its recorded result before retrying"
                );
            }
            if let Some(previous) = previous {
                validate_session_owner(turn, previous)?;
            }
            if *cancel.borrow() {
                bail!("Cursor cancelled before prompt submission");
            }
            stage = ProviderStage::Authentication;
            ensure!(
                self.subscription_authentication(
                    &turn.executable,
                    &turn.workspace,
                    &turn.local_store_root,
                    &turn.env
                )
                .await?
                    == CursorAuthentication::Authenticated,
                "Cursor CLI login is missing or unverified; run agent login explicitly"
            );
            stage = ProviderStage::Launch;
            let mut rpc = self
                .spawn(
                    &turn.executable,
                    &turn.workspace,
                    &turn.local_store_root,
                    &turn.env,
                    &turn.model,
                    CursorCommand::Acp,
                )
                .await?;
            let setup = async {
                if let Some(record) = &turn.process_started {
                    record(rpc.group).await?;
                }
                let capabilities = self.initialize(&mut rpc).await?;
                stage = ProviderStage::SessionStart;
                let servers = self.mcp_servers(&capabilities, turn)?;
                let reply = if let Some(previous) = previous {
                    ensure!(
                        capabilities["agentCapabilities"]["loadSession"] == true,
                        "Cursor ACP does not support loading this session"
                    );
                    rpc.send(json!({"jsonrpc":"2.0","id":3,"method":"session/load","params":{"sessionId":previous,"cwd":turn.workspace,"mcpServers":servers}})).await?;
                    rpc_response(&mut rpc, 3, Some(previous), false, &mut |_| async {
                        Ok(())
                    })
                    .await?;
                    previous.to_owned()
                } else {
                    rpc.send(json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":turn.workspace,"mcpServers":servers}})).await?;
                    let created =
                        rpc_response(&mut rpc, 3, None, false, &mut |_| async { Ok(()) }).await?;
                    let session = created["sessionId"]
                        .as_str()
                        .context("Cursor ACP returned no session ID")?
                        .to_owned();
                    record_session_owner(turn, &session)?;
                    session
                };
                session = Some(reply.clone());
                stage = ProviderStage::Record;
                persist_session(reply.clone()).await?;
                Ok::<_, anyhow::Error>(reply)
            };
            let session_id = match tokio::time::timeout(STARTUP_TIMEOUT, setup).await {
                Ok(result) => result,
                Err(_) => Err(anyhow!("Cursor ACP session setup timed out")),
            }?;
            if *cancel.borrow() {
                rpc.close().await;
                bail!("Cursor cancelled before prompt submission");
            }
            let mut receipt = PromptReceipt {
                session: session_id.clone(),
                id: id.clone(),
                input_hash,
                status: PromptStatus::Submitted,
                output: String::new(),
            };
            write_receipt(&turn.local_store_root, &receipt)?;
            submitted = true;
            stage = ProviderStage::TurnStart;
            let (stop, mut stopped) = watch::channel(false);
            self.active
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert((turn.local_store_root.clone(), id.clone()), stop);
            rpc.send(json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":prompt}]}})).await?;
            let mut pending = BTreeMap::new();
            let mut update = |value: Value| {
                let value = redact_stream_progress(value, &secrets, &mut pending);
                progress(value)
            };
            let result = {
                let running = prompt_response(
                    &mut rpc,
                    &session_id,
                    &id,
                    turn,
                    &mut receipt,
                    &mut observed,
                    &mut update,
                );
                tokio::pin!(running);
                tokio::select! {
                    result = tokio::time::timeout(Duration::from_secs(turn.timeout_secs),&mut running) => result.ok(),
                    _ = wait_for_cancel(&mut cancel) => None,
                    _ = wait_for_cancel(&mut stopped) => None,
                }
            };
            let result = match result {
                Some(result) => result,
                None => {
                    stage = ProviderStage::Interrupt;
                    rpc.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session_id}})).await?;
                    tokio::time::timeout(
                        STOP_TIMEOUT,
                        prompt_response(
                            &mut rpc,
                            &session_id,
                            &id,
                            turn,
                            &mut receipt,
                            &mut observed,
                            &mut update,
                        ),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(anyhow!(
                            "Cursor cancellation has no confirmed terminal; result is unknown"
                        ))
                    })
                }
            };
            self.active
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&(turn.local_store_root.clone(), id.clone()));
            rpc.close().await;
            let stop_reason = result?;
            receipt.output = redact_text(receipt.output, &secrets);
            receipt.status = match stop_reason.as_str() {
                "end_turn" => PromptStatus::Completed,
                "cancelled" => PromptStatus::Cancelled,
                "max_tokens" | "max_turn_requests" => PromptStatus::Failed {
                    detail: "Cursor stopped at its turn limit".into(),
                },
                _ => bail!("Cursor ACP returned an unknown stop reason"),
            };
            // The terminal is committed before callbacks or the caller can observe completion.
            write_receipt(&turn.local_store_root, &receipt)?;
            if receipt.status == PromptStatus::Cancelled {
                bail!("Cursor prompt was cancelled by the provider");
            }
            if let PromptStatus::Failed { detail } = &receipt.status {
                bail!("{detail}");
            }
            progress(
                json!({"type":"text_snapshot","item_id":"cursor-answer","text":receipt.output}),
            )
            .await?;
            Ok(AgentResult {
                session_id: Some(session_id),
                output: receipt.output,
                usage: None,
            })
        };
        let problem = match operation.await {
            Ok(result) => return Ok(result),
            Err(error) => error,
        };
        let id = receipt_id(&turn.client_id);
        self.active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(turn.local_store_root.clone(), id.clone()));
        let terminal = read_receipt(&turn.local_store_root, &id).ok().flatten();
        let outcome = if terminal
            .as_ref()
            .is_some_and(|r| r.status == PromptStatus::Cancelled)
        {
            AgentFailure::Cancelled
        } else if terminal
            .as_ref()
            .is_some_and(|r| matches!(r.status, PromptStatus::Failed { .. }))
        {
            AgentFailure::Failed
        } else if submitted {
            AgentFailure::Unknown
        } else if *cancel.borrow() {
            AgentFailure::Cancelled
        } else {
            AgentFailure::Failed
        };
        let kind = if stage == ProviderStage::Authentication
            || problem
                .downcast_ref::<AcpError>()
                .is_some_and(|error| error.code == -32000)
        {
            FailureKind::AuthFailed
        } else if problem
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            FailureKind::MissingBinary
        } else {
            FailureKind::Unknown
        };
        Err(AgentRunError {
            stage,
            outcome,
            start: if observed {
                StartEvidence::Started
            } else if submitted {
                StartEvidence::Unknown
            } else {
                StartEvidence::NotStarted
            },
            session_id: session,
            failure: ExecFailure::upstream(redact_text(format!("{problem:#}"), &secrets), kind),
        })
    }
    pub async fn probe(&self, turn: &CursorTurn, session: &str, id: &str) -> Result<CursorPrompt> {
        validate_session_owner(turn, session)?;
        let receipt = read_receipt(&turn.local_store_root, id)?
            .context("Cursor prompt receipt is missing")?;
        ensure!(
            receipt.session == session,
            "Cursor prompt belongs to another session"
        );
        let status = if matches!(
            receipt.status,
            PromptStatus::Submitted | PromptStatus::Running
        ) {
            if self
                .active
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(&(turn.local_store_root.clone(), id.into()))
            {
                PromptStatus::Running
            } else {
                PromptStatus::Unknown
            }
        } else {
            receipt.status
        };
        Ok(CursorPrompt {
            session_id: session.into(),
            receipt_id: id.into(),
            status,
            output: Some(receipt.output),
        })
    }
    pub async fn cancel(&self, turn: &CursorTurn, session: &str, id: &str) -> Result<()> {
        validate_session_owner(turn, session)?;
        let receipt = read_receipt(&turn.local_store_root, id)?
            .context("Cursor prompt receipt is missing")?;
        ensure!(
            receipt.session == session,
            "Cursor prompt belongs to another session"
        );
        if let Some(stop) = self
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(turn.local_store_root.clone(), id.into()))
        {
            let _ = stop.send(true);
        }
        Ok(())
    }
}

fn cursor_models(response: &Value, env: &BTreeMap<String, String>) -> CursorCatalog {
    let Some(models) = response["models"]["availableModels"]
        .as_array()
        .filter(|models| models.len() <= 512)
    else {
        return CursorCatalog::Unavailable;
    };
    let mut parsed = Vec::new();
    for model in models {
        let valid = |value: &&str| {
            !value.is_empty()
                && value.len() <= 256
                && !value.chars().any(char::is_control)
                && !env
                    .values()
                    .any(|secret| !secret.is_empty() && value.contains(secret))
        };
        let Some(id) = model["modelId"].as_str().filter(valid) else {
            return CursorCatalog::Unavailable;
        };
        let Some(name) = model["name"].as_str().filter(valid) else {
            return CursorCatalog::Unavailable;
        };
        parsed.push(CursorModel {
            id: id.into(),
            display_name: name.into(),
        });
    }
    CursorCatalog::Available { models: parsed }
}

#[derive(Debug)]
struct AcpError {
    code: i64,
}
impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cursor ACP request rejected ({})", self.code)
    }
}
impl std::error::Error for AcpError {}
async fn rpc_response<P, PF>(
    rpc: &mut Rpc,
    id: u64,
    session: Option<&str>,
    running: bool,
    progress: &mut P,
) -> Result<Value>
where
    P: FnMut(Value) -> PF,
    PF: Future<Output = Result<()>>,
{
    loop {
        let message = rpc
            .incoming
            .recv()
            .await
            .context("Cursor ACP disconnected; result is unknown")?;
        if message.get("method").is_some() {
            handle_message(rpc, &message, session, running, progress).await?;
        } else if message["id"] == id {
            if let Some(error) = message.get("error") {
                return Err(AcpError {
                    code: error["code"].as_i64().unwrap_or(-32603),
                }
                .into());
            }
            return message
                .get("result")
                .filter(|v| v.is_object())
                .cloned()
                .context("Cursor ACP response has no result");
        }
    }
}
async fn prompt_response<P, PF>(
    rpc: &mut Rpc,
    session: &str,
    id: &str,
    turn: &CursorTurn,
    receipt: &mut PromptReceipt,
    observed: &mut bool,
    progress: &mut P,
) -> Result<String>
where
    P: FnMut(Value) -> PF,
    PF: Future<Output = Result<()>>,
{
    loop {
        let message = rpc
            .incoming
            .recv()
            .await
            .context("Cursor ACP disconnected; result is unknown")?;
        let matching_update =
            message["method"] == "session/update" && message["params"]["sessionId"] == session;
        let matching_response = message["id"] == 4 && message.get("method").is_none();
        if (matching_update || matching_response) && !*observed {
            *observed = true;
            receipt.status = PromptStatus::Running;
            write_receipt(&turn.local_store_root, receipt)?;
            if let Some(started) = &turn.run_started {
                started(id.into()).await?;
            }
        }
        if matching_update
            && message["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
            && let Some(text) = message["params"]["update"]["content"]["text"].as_str()
        {
            ensure!(
                receipt.output.len() + text.len() <= MAX_RESULT_BYTES,
                "Cursor reply exceeds the output limit"
            );
            receipt.output.push_str(text);
        }
        if message.get("method").is_some() {
            handle_message(rpc, &message, Some(session), true, progress).await?;
        } else if matching_response {
            if let Some(error) = message.get("error") {
                return Err(AcpError {
                    code: error["code"].as_i64().unwrap_or(-32603),
                }
                .into());
            }
            return message["result"]["stopReason"]
                .as_str()
                .map(str::to_owned)
                .context("Cursor prompt returned no stop reason");
        }
    }
}
async fn handle_message<P, PF>(
    rpc: &Rpc,
    message: &Value,
    session: Option<&str>,
    running: bool,
    progress: &mut P,
) -> Result<()>
where
    P: FnMut(Value) -> PF,
    PF: Future<Output = Result<()>>,
{
    let method = message["method"].as_str().unwrap_or_default();
    let params = &message["params"];
    let current = running && session.is_some_and(|id| params["sessionId"] == id);
    if let Some(id) = message.get("id") {
        let result = match method {
            "session/request_permission" => permission_response(params, current),
            "cursor/ask_question" => json!({"outcome":{"outcome":"cancelled"}}),
            "cursor/create_plan" => {
                json!({"outcome":{"outcome":"rejected","reason":"Continue with the Node task and use its decision tools when user input is required."}})
            }
            _ => {
                rpc.send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Client method is not supported"}})).await?;
                return Ok(());
            }
        };
        rpc.send(json!({"jsonrpc":"2.0","id":id,"result":result}))
            .await?;
        if running
            && session.is_some()
            && matches!(method, "cursor/ask_question" | "cursor/create_plan")
        {
            progress(json!({"type":"status","status":"interaction_declined","text":"Cursor 请求的交互已取消；需要本人决定时请通过节点提问。"})).await?;
        }
    } else if method == "session/update" && current {
        let update = &params["update"];
        match update["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => {
                if let Some(text) = update["content"]["text"].as_str() {
                    progress(json!({"type":"text_delta","item_id":"cursor-answer","text":text}))
                        .await?;
                }
            }
            Some("tool_call" | "tool_call_update") => {
                let mut value = json!({"type":"tool"});
                for (source, target) in [
                    ("title", "name"),
                    ("status", "status"),
                    ("toolCallId", "item_id"),
                ] {
                    if let Some(text) = update[source]
                        .as_str()
                        .filter(|v| v.len() <= 256 && !v.chars().any(char::is_control))
                    {
                        value[target] = json!(text);
                    }
                }
                progress(value).await?;
            }
            _ => {}
        }
    }
    Ok(())
}
fn permission_response(params: &Value, current: bool) -> Value {
    let known = matches!(
        params["toolCall"]["kind"].as_str(),
        Some("read" | "edit" | "delete" | "move" | "search" | "execute" | "fetch" | "think")
    );
    let preferred = if current && known {
        "allow_once"
    } else {
        "reject_once"
    };
    if let Some(option) = params["options"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|option| option["kind"] == preferred)
        && let Some(id) = option["optionId"]
            .as_str()
            .filter(|v| !v.is_empty() && v.len() <= 256)
    {
        json!({"outcome":{"outcome":"selected","optionId":id}})
    } else {
        json!({"outcome":{"outcome":"cancelled"}})
    }
}
fn validate_turn(turn: &CursorTurn, prompt: &str) -> Result<()> {
    ensure!(
        !turn.owner.is_empty() && turn.owner.len() <= 512,
        "invalid Cursor owner"
    );
    vibeface_protocol::validate_id(&turn.client_id).map_err(anyhow::Error::msg)?;
    ensure!(
        !prompt.trim().is_empty() && prompt.len() <= 86_016 && turn.timeout_secs > 0,
        "invalid Cursor prompt or timeout"
    );
    ensure!(
        turn.workspace.is_absolute()
            && turn.workspace.is_dir()
            && turn.local_store_root.is_absolute(),
        "Cursor requires private absolute paths"
    );
    validate_launch_env(&turn.env)
}
fn validate_launch_env(env: &BTreeMap<String, String>) -> Result<()> {
    ensure!(env.len() <= 64, "too many Cursor environment entries");
    for (name, value) in env {
        ensure!(
            !name.is_empty()
                && name.len() <= 256
                && name.bytes().enumerate().all(|(i, c)| c == b'_'
                    || c.is_ascii_alphabetic()
                    || (i > 0 && c.is_ascii_digit()))
                && !name.starts_with("VIBEFACE_")
                && !matches!(name.as_str(), "CURSOR_CONFIG_DIR" | "CURSOR_DATA_DIR")
                && value.len() <= 4096
                && !value.contains('\0'),
            "invalid or reserved Cursor environment entry"
        );
    }
    Ok(())
}
fn validate_origin(origin: &str) -> Result<()> {
    let url = reqwest::Url::parse(origin)?;
    let host: IpAddr = url
        .host_str()
        .unwrap_or_default()
        .trim_matches(['[', ']'])
        .parse()
        .map_err(|_| anyhow!("Cursor tool origin must be a loopback IP"))?;
    ensure!(
        url.scheme() == "http"
            && host.is_loopback()
            && url.port().is_some_and(|p| p > 0)
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none(),
        "Cursor tools require a local HTTP origin"
    );
    Ok(())
}

/// Existing line-based journals used by the Claude and pi process owners. A record stays until
/// its tree is confirmed gone, so an unanswerable or still-running tree is never forgotten.
pub fn kill_recorded_programs(pidfile: &Path) {
    let Ok(recorded) = std::fs::read_to_string(pidfile) else {
        return;
    };
    let mut unconfirmed = String::new();
    for line in recorded.lines() {
        // A journal record never contains a space, so the rest of the line is the whole
        // program path even when a directory in it has one.
        let Some((token, executable)) = line.split_once(' ') else {
            unconfirmed.push_str(&format!("{line}\n"));
            continue;
        };
        if executable.is_empty() {
            unconfirmed.push_str(&format!("{line}\n"));
            continue;
        }
        if let Err(error) =
            platform::tree::stop_recorded(token, |member| member.contains(executable))
        {
            tracing::debug!(
                event = "agent.leftover_left_alone",
                %error,
                "a recorded program was not ended"
            );
        }
        if !matches!(platform::tree::Leftover::find(token), Ok(None)) {
            unconfirmed.push_str(&format!("{line}\n"));
        }
    }
    if unconfirmed.is_empty() {
        let _ = std::fs::remove_file(pidfile);
    } else if let Err(error) = platform::privatefs::write_atomic(pidfile, unconfirmed.as_bytes()) {
        tracing::warn!(%error, "could not preserve the unconfirmed process journal");
    }
}

pub async fn wait_for_cancel(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() {
            return;
        }
        if cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Reaps this bridge's own process tree only; already accepted external work is unaffected.
///
/// `true` means nothing of the tree is left. A `false` answer leaves cleanup unconfirmed.
/// Callers retain the record for later reconciliation.
pub async fn terminate_process_tree(child: &mut Child, tree: &mut platform::tree::Tree) -> bool {
    let token = tree.token().to_owned();
    if cfg!(windows) || child.id() == Some(tree.root()) {
        tree.ask_to_stop();
        let stopped = tokio::time::timeout(platform::tree::GRACE, async {
            // Keep the Unix leader unreaped until the last possible group signal.
            while tree.has_live_members().await.unwrap_or(true) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        if stopped.is_err() {
            tree.kill();
        }
    }
    if !matches!(
        tokio::time::timeout(Duration::from_secs(2), child.wait()).await,
        Ok(Ok(_))
    ) {
        return false;
    }
    for _ in 0..20 {
        // Windows Tree::kill drops its Job handle after terminating it. Query the recorded job
        // by token so this checks the whole tree, including children of a `.cmd` launcher.
        match platform::tree::Leftover::find(&token) {
            Ok(None) => return true,
            Ok(Some(_)) => {}
            Err(error) => {
                tracing::warn!(%error, "could not confirm Cursor process-tree exit");
            }
        }
        #[cfg(windows)]
        tree.kill();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Whether an environment value is a credential worth hiding in shared records. Ordinary values
/// such as `DEBUG=1` or a workspace path must not be replaced, or every "1" in a reply vanishes.
fn credential_value(name: &str, value: &str) -> bool {
    if value.len() < 8 {
        return false;
    }
    if ["_KEY", "_TOKEN", "_SECRET", "_PASSWORD"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
    {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    if [
        "-----begin",
        "bearer ",
        "password=",
        "password:",
        "passwd=",
        "secret=",
        "api_key=",
        "apikey=",
        "token=",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return true;
    }
    value
        .split(|c: char| c.is_whitespace() || "\"'`,;()[]{}<>".contains(c))
        .any(|word| {
            let lower = word.to_ascii_lowercase();
            url_with_password(word)
                || (word.len() >= 20
                    && [
                        "sk-",
                        "sk_",
                        "ghp_",
                        "gho_",
                        "ghs_",
                        "ghu_",
                        "github_pat_",
                        "xoxb-",
                        "xoxp-",
                        "xapp-",
                        "akia",
                        "aiza",
                        "glpat-",
                        "hf_",
                        "npm_",
                    ]
                    .iter()
                    .any(|prefix| lower.starts_with(prefix)))
                || (word.len() >= 32
                    && word
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_+/=".contains(c))
                    && word.chars().any(|c| c.is_ascii_digit())
                    && word.chars().any(|c| c.is_ascii_uppercase())
                    && word.chars().any(|c| c.is_ascii_lowercase()))
        })
}

/// A connection string such as `postgres://user:pass@host/db` carries its password in the
/// authority, so the whole value is a credential whatever its variable is called.
fn url_with_password(word: &str) -> bool {
    let Some((_, rest)) = word.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority.rsplit_once('@').is_some_and(|(userinfo, _)| {
        userinfo
            .split_once(':')
            .is_some_and(|(_, pass)| !pass.is_empty())
    })
}

fn redact_text(mut text: String, secrets: &[&str]) -> String {
    for secret in secrets.iter().copied().filter(|secret| !secret.is_empty()) {
        text = text.replace(secret, "[redacted]");
    }
    text
}

fn redact_progress(value: Value, secrets: &[&str]) -> Value {
    match value {
        Value::String(text) => Value::String(redact_text(text, secrets)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| redact_progress(item, secrets))
                .collect(),
        ),
        Value::Object(items) => Value::Object(
            items
                .into_iter()
                .map(|(key, value)| (key, redact_progress(value, secrets)))
                .collect(),
        ),
        value => value,
    }
}

fn redact_stream_progress(
    mut value: Value,
    secrets: &[&str],
    pending: &mut BTreeMap<String, String>,
) -> Value {
    if let Some(item) = value
        .get("item_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        if value["type"] == "text_snapshot" {
            pending.remove(&item);
        }
        if value["type"] == "text_delta"
            && let Some(text) = value.get("text").and_then(Value::as_str)
        {
            let buffered = pending.entry(item).or_default();
            buffered.push_str(text);
            let Value::String(clean) = redact_progress(json!(buffered), secrets) else {
                unreachable!()
            };
            let max_prefix = secrets
                .iter()
                .map(|secret| secret.len().saturating_sub(1))
                .max()
                .unwrap_or(0);
            let split = clean
                .char_indices()
                .map(|(index, _)| index)
                .filter(|index| clean.len() - index <= max_prefix)
                .find(|index| {
                    secrets.iter().any(|secret| {
                        secret.len() > clean.len() - index && secret.starts_with(&clean[*index..])
                    })
                })
                .unwrap_or(clean.len());
            *buffered = clean[split..].to_owned();
            value["text"] = json!(&clean[..split]);
        }
    }
    redact_progress(value, secrets)
}

pub(crate) async fn bounded_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    limit: usize,
) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut line)
        .await?;
    ensure!(
        line.len() <= limit,
        "line exceeds the configured size limit"
    );
    Ok(line)
}

#[cfg(test)]
mod credential_tests {
    use super::credential_value;

    #[test]
    fn credential_values_are_selected_by_name_or_shape_only() {
        assert!(credential_value("CURSOR_API_KEY", "k!secret1"));
        assert!(!credential_value("FOO_TOKEN", "1"));
        assert!(credential_value(
            "DATABASE_URL",
            "postgres://app:s3cret@db.internal/app"
        ));
        assert!(credential_value(
            "X",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789"
        ));
        assert!(!credential_value("DEBUG", "1"));
        assert!(!credential_value("NODE_ENV", "production"));
        assert!(!credential_value("HOME", "/Users/me/project"));
        assert!(!credential_value("API_BASE", "http://host:8080/path"));
        assert!(!credential_value("ORIGIN", "user@example.com"));
    }
}
