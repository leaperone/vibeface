use crate::failure::{ExecFailure, FailureKind};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    hash::{BuildHasher, RandomState},
    io::Write,
    net::IpAddr,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, Notify, Semaphore, oneshot, watch},
    task::JoinHandle,
    time::Instant,
};

const MAX_FRAME_BYTES: usize = 1_048_576;
const MAX_STREAM_BYTES: usize = 16_777_216;
const MAX_RESULT_BYTES: usize = 65_536;
const MAX_PROMPT_BYTES: usize = 86_016;
const AGENT_SERVICE: &str = "SdkAgentService";
const CONTROL_SERVICE: &str = "SdkBridgeControlService";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const PING_TIMEOUT: Duration = Duration::from_secs(5);

/// Durably records an SDK process group before the agent can act.
pub type ProcessStarted = std::sync::Arc<
    dyn Fn(u32) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync,
>;

/// One Cursor SDK turn on the Node's resident bridge.
pub struct SdkTurn {
    pub executable: PathBuf,
    pub workspace: PathBuf,
    pub api_key: String,
    /// The engine's own variables for the bridge process; a different set starts a new bridge.
    pub env: BTreeMap<String, String>,
    pub model: String,
    pub timeout_secs: u64,
    pub custom_tools: BTreeMap<String, CustomToolConfig>,
    pub process_started: Option<ProcessStarted>,
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

#[derive(Clone)]
pub struct ToolCallbackConfig {
    pub url: String,
    pub auth_token: String,
}

pub struct AgentResult {
    pub session_id: Option<String>,
    pub output: String,
    pub usage: Option<Value>,
}

/// How certain the end of a failed turn is; `ExecFailure` says why it failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentFailure {
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Debug)]
pub struct AgentRunError {
    pub outcome: AgentFailure,
    pub session_id: Option<String>,
    pub failure: ExecFailure,
}

impl std::fmt::Display for AgentRunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.failure.fmt(formatter)
    }
}

impl std::error::Error for AgentRunError {}

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

/// A local process reap is not a provider terminal state. Cancellation stays
/// `Unknown` until the provider reports `Cancelled`, `Failed`, or another finish.
fn provider_cancel_failure(native_terminal: Option<AgentFailure>) -> AgentFailure {
    native_terminal.unwrap_or(AgentFailure::Unknown)
}

/// Reaps the local process group only; already accepted external work is unaffected.
pub async fn terminate_process_group(child: &mut Child, group: u32) -> bool {
    async fn signal(group: u32, signal: &str) -> Option<bool> {
        Command::new("/bin/kill")
            .arg(signal)
            // procps kill parses a bare negative PID as an option; `--` makes it a group.
            .arg("--")
            .arg(format!("-{group}"))
            .output()
            .await
            .ok()
            .map(|output| output.status.success())
    }
    let _ = signal(group, "-TERM").await;
    let reaped = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    if !matches!(reaped, Ok(Ok(_))) {
        let _ = signal(group, "-KILL").await;
        if !matches!(
            tokio::time::timeout(Duration::from_secs(2), child.wait()).await,
            Ok(Ok(_))
        ) {
            return false;
        }
    }
    for _ in 0..20 {
        match signal(group, "-0").await {
            Some(false) => return true,
            None => return false,
            Some(true) => {
                let _ = signal(group, "-KILL").await;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn signal_group_now(group: u32, signal: &str) -> bool {
    std::process::Command::new("/bin/kill")
        .args([signal, "--", &format!("-{group}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The Node's one resident `cursor-sdk-bridge`. It starts on first use and serves every
/// turn; a dead or unresponsive bridge is replaced with backoff, and a bridge left holding
/// an agent of unknown state is retired so its process group ends.
pub struct SdkBridge {
    env: BTreeMap<String, String>,
    callback: ToolCallbackConfig,
    hasher: RandomState,
    slot: Mutex<Slot>,
    processes: Arc<Processes>,
}

#[derive(Default)]
struct Slot {
    live: Option<Arc<Live>>,
    failures: u32,
    retry_at: Option<Instant>,
}

impl Slot {
    /// Each failure doubles the wait before the next start, up to 30s; a bridge that ran for
    /// a minute clears the earlier failures.
    fn failed(&mut self, ran: Duration) {
        if ran > Duration::from_secs(60) {
            self.failures = 0;
        }
        self.failures += 1;
        let delay = (1u64 << (self.failures - 1).min(5)).min(30);
        self.retry_at = Some(Instant::now() + Duration::from_secs(delay));
    }
}

struct Live {
    key: u64,
    group: u32,
    rpc: Rpc,
    agents: Option<Arc<Semaphore>>,
    started: Instant,
    turns: AtomicUsize,
    retired: AtomicBool,
    kill: Arc<Notify>,
    exited: watch::Receiver<bool>,
}

impl Live {
    fn exited(&self) -> bool {
        *self.exited.borrow()
    }

    fn admit(self: &Arc<Self>) -> Admission {
        self.turns.fetch_add(1, SeqCst);
        Admission(self.clone())
    }

    /// Admits no new turn; the process ends with its last running turn.
    fn retire(&self) {
        self.retired.store(true, SeqCst);
        if self.turns.load(SeqCst) == 0 {
            self.kill.notify_one();
        }
    }

    async fn healthy(&self) -> bool {
        !self.exited()
            && matches!(
                tokio::time::timeout(
                    PING_TIMEOUT,
                    self.rpc.unary(CONTROL_SERVICE, "Ping", json!({}))
                )
                .await,
                Ok(Ok(_))
            )
    }
}

struct Admission(Arc<Live>);

impl Drop for Admission {
    fn drop(&mut self) {
        if self.0.turns.fetch_sub(1, SeqCst) == 1 && self.0.retired.load(SeqCst) {
            self.0.kill.notify_one();
        }
    }
}

/// Kills a bridge whose startup did not finish, also when the waiting turn is cancelled.
struct Abandon(Option<Arc<Notify>>);

impl Drop for Abandon {
    fn drop(&mut self) {
        if let Some(kill) = self.0.take() {
            kill.notify_one();
        }
    }
}

/// Bridge process groups still running, mirrored to the pidfile for `kill_orphan`.
struct Processes {
    pidfile: Option<PathBuf>,
    running: std::sync::Mutex<BTreeMap<u32, Tracked>>,
}

struct Tracked {
    executable: PathBuf,
    kill: Arc<Notify>,
    exited: watch::Receiver<bool>,
}

impl Processes {
    fn track(&self, group: u32, tracked: Tracked) -> Result<()> {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        running.insert(group, tracked);
        self.save(&running)
            .context("record the Cursor SDK bridge process group")
    }

    fn forget(&self, group: u32) {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        running.remove(&group);
        let _ = self.save(&running);
    }

    fn save(&self, running: &BTreeMap<u32, Tracked>) -> std::io::Result<()> {
        let Some(pidfile) = &self.pidfile else {
            return Ok(());
        };
        if running.is_empty() {
            return match std::fs::remove_file(pidfile) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
                _ => Ok(()),
            };
        }
        let lines: String = running
            .iter()
            .map(|(group, tracked)| format!("{group} {}\n", tracked.executable.display()))
            .collect();
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(pidfile)?
            .write_all(lines.as_bytes())
    }
}

impl SdkBridge {
    /// `env` reaches every agent shell of the bridge, and the bridge calls `tool_callback`
    /// for the custom tools of every turn.
    pub fn new(
        env: BTreeMap<String, String>,
        tool_callback: ToolCallbackConfig,
        pidfile: Option<PathBuf>,
    ) -> Result<Self> {
        validate_env(&env)?;
        validate_callback(&tool_callback)?;
        Ok(Self {
            env,
            callback: tool_callback,
            hasher: RandomState::new(),
            slot: Mutex::default(),
            processes: Arc::new(Processes {
                pidfile,
                running: Default::default(),
            }),
        })
    }

    /// Starts the bridge before the first turn needs it.
    pub async fn prewarm(
        &self,
        executable: &Path,
        workspace: &Path,
        api_key: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<()> {
        validate_api_key(api_key)?;
        validate_launch_env(env)?;
        self.admit(executable, workspace, api_key, env)
            .await
            .map(drop)
            .map_err(|error| {
                anyhow!(redact_text(
                    format!("{error:#}"),
                    &self.secrets(api_key, env, "")
                ))
            })
    }

    /// Lets running turns finish on the current bridge, then ends it; the next turn starts anew.
    pub async fn retire(&self) {
        if let Some(live) = self.slot.lock().await.live.take() {
            live.retire();
        }
    }

    /// Kills every bridge process group this Node started and removes the pidfile.
    pub async fn stop(&self) {
        let running: Vec<_> = self
            .processes
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|tracked| (tracked.kill.clone(), tracked.exited.clone()))
            .collect();
        for (kill, mut exited) in running {
            kill.notify_one();
            let _ = exited.wait_for(|exited| *exited).await;
        }
        if let Some(pidfile) = &self.processes.pidfile {
            let _ = std::fs::remove_file(pidfile);
        }
    }

    /// Kills the bridge groups an earlier Node run recorded in `pidfile`, when the group leader
    /// still runs the recorded program; a reused process id is left alone.
    pub fn kill_orphan(pidfile: &Path) {
        let Ok(recorded) = std::fs::read_to_string(pidfile) else {
            return;
        };
        for line in recorded.lines() {
            let Some((group, executable)) = line.split_once(' ') else {
                continue;
            };
            let Some(group) = group.parse::<u32>().ok().filter(|group| *group > 1) else {
                continue;
            };
            let command = std::process::Command::new("/bin/ps")
                .args(["-o", "command=", "-p", &group.to_string()])
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                .unwrap_or_default();
            if executable.is_empty() || !command.contains(executable) {
                continue;
            }
            if signal_group_now(group, "-TERM") {
                for _ in 0..30 {
                    if !signal_group_now(group, "-0") {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                signal_group_now(group, "-KILL");
            }
        }
        let _ = std::fs::remove_file(pidfile);
    }

    /// Runs the prompt with the turn's tools on the resident bridge.
    /// The callback commits the native ID before the first mutating Send request.
    pub async fn run<F, P, PF>(
        &self,
        turn: &SdkTurn,
        prompt: &str,
        previous_session_id: Option<&str>,
        persist_session: impl FnOnce(String) -> F,
        mut cancel: watch::Receiver<bool>,
        mut progress: P,
    ) -> Result<AgentResult, AgentRunError>
    where
        F: Future<Output = Result<()>>,
        P: FnMut(Value) -> PF,
        PF: Future<Output = Result<()>>,
    {
        let not_sent = |outcome, failure: ExecFailure| AgentRunError {
            outcome,
            session_id: previous_session_id.map(str::to_owned),
            failure: ExecFailure {
                detail: redact_text(failure.detail, &self.secrets(&turn.api_key, &turn.env, "")),
                ..failure
            },
        };
        let cancelled_before_send = || {
            not_sent(
                AgentFailure::Cancelled,
                ExecFailure::new(
                    FailureKind::Unknown,
                    "Cursor SDK management cancelled before Send",
                ),
            )
        };
        if let Err(failure) = validate_turn(turn, prompt, previous_session_id) {
            return Err(not_sent(AgentFailure::Failed, failure));
        }
        if *cancel.borrow() {
            return Err(cancelled_before_send());
        }
        let admission = tokio::select! {
            admitted = self.admit(&turn.executable, &turn.workspace, &turn.api_key, &turn.env) => {
                admitted.map_err(|error| not_sent(AgentFailure::Failed, start_failure(&error)))?
            }
            _ = wait_for_cancel(&mut cancel) => return Err(cancelled_before_send()),
        };
        let live = &admission.0;
        let _agent_slot = match live.agents.clone() {
            Some(agents) => tokio::select! {
                permit = agents.acquire_owned() => permit.ok(),
                _ = wait_for_cancel(&mut cancel) => return Err(cancelled_before_send()),
            },
            None => None,
        };
        if let Some(record) = &turn.process_started
            && let Err(error) = record(live.group).await
        {
            return Err(not_sent(
                AgentFailure::Failed,
                ExecFailure::new(
                    FailureKind::Unknown,
                    format!("record Cursor SDK process group before sending: {error:#}"),
                ),
            ));
        }
        let rpc = &live.rpc;
        let secrets = self.secrets(&turn.api_key, &turn.env, &rpc.bearer);
        let mut agent_id = previous_session_id.map(str::to_owned);
        let mut state = RunState::default();
        let mut send_started = false;
        let operation = async {
            let options = sdk_options(turn)?;
            let reply = if let Some(id) = previous_session_id {
                rpc.unary(
                    AGENT_SERVICE,
                    "ResumeAgent",
                    json!({"agentId": id, "options": options}),
                )
                .await?
            } else {
                rpc.unary(AGENT_SERVICE, "CreateAgent", json!({"options": options}))
                    .await?
            };
            let id = reply
                .get("agentId")
                .and_then(Value::as_str)
                .context("Cursor SDK returned no agent id")?;
            validate_sdk_id(id)?;
            if let Some(previous) = previous_session_id {
                ensure!(previous == id, "Cursor SDK resumed a different agent");
            }
            agent_id = Some(id.to_owned());
            persist_session(id.to_owned())
                .await
                .context("persist Cursor SDK session before sending management prompt")?;
            send_started = true;
            let mut pending_text = BTreeMap::new();
            let mut emit_progress =
                |value| progress(redact_stream_progress(value, &secrets, &mut pending_text));
            rpc.send(id, prompt, &mut state, &mut emit_progress).await?;
            let mut result = state.finish(id)?;
            result.output = redact_text(result.output, &secrets);
            result.usage = result.usage.map(|usage| redact_progress(usage, &secrets));
            Ok(result)
        };
        let mut cancellation_requested = false;
        let mut exited = live.exited.clone();
        let outcome = tokio::select! {
            biased;
            result = tokio::time::timeout(Duration::from_secs(turn.timeout_secs), operation) => match result {
                Ok(result) => result.map_err(|error| ExecFailure::upstream(format!("{error:#}"), FailureKind::Unknown)),
                Err(_) => Err(ExecFailure::new(FailureKind::TimedOut, "Cursor SDK manager timed out; completion is unknown")),
            },
            _ = exited.wait_for(|exited| *exited) => {
                Err(ExecFailure::new(FailureKind::Lost, "Cursor SDK bridge exited unexpectedly; completion is unknown"))
            }
            _ = wait_for_cancel(&mut cancel) => {
                cancellation_requested = true;
                Err(ExecFailure::new(FailureKind::Unknown, "Cursor SDK management cancellation requested"))
            }
        };
        let mut native_terminal = None;
        if outcome.is_err()
            && let Some(run_id) = &state.run_id
        {
            let _ = tokio::time::timeout(
                Duration::from_secs(3),
                rpc.unary(
                    AGENT_SERVICE,
                    "CancelRun",
                    json!({"runId": run_id, "agentId": agent_id}),
                ),
            )
            .await;
            if cancellation_requested && let Some(agent_id) = &agent_id {
                native_terminal = tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        let reply = rpc
                            .unary(AGENT_SERVICE, "GetRun", json!({"runId":run_id}))
                            .await?;
                        let snapshot = reply
                            .get("run")
                            .context("Cursor SDK returned no run snapshot")?;
                        let mut identity = RunState {
                            run_id: Some(run_id.clone()),
                            ..RunState::default()
                        };
                        identity.identity(snapshot, agent_id, true)?;
                        if cancelled(snapshot.get("status")) {
                            return Ok::<AgentFailure, anyhow::Error>(AgentFailure::Cancelled);
                        }
                        if finished(snapshot.get("status")) {
                            return Ok(AgentFailure::Unknown);
                        }
                        if failed(snapshot.get("status")) {
                            return Ok(AgentFailure::Failed);
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                })
                .await
                .ok()
                .and_then(Result::ok);
            }
        }
        if let Some(id) = &agent_id {
            let _ = tokio::time::timeout(
                Duration::from_secs(3),
                rpc.unary(AGENT_SERVICE, "CloseAgent", json!({"agentId": id})),
            )
            .await;
        }
        outcome.map_err(|failure| {
            let outcome = if cancellation_requested {
                provider_cancel_failure(native_terminal)
            } else if send_started {
                AgentFailure::Unknown
            } else {
                AgentFailure::Failed
            };
            // The agent may still act; its pending check closes only once the bridge group ends.
            if outcome == AgentFailure::Unknown && !live.exited() {
                live.retire();
            }
            AgentRunError {
                outcome,
                session_id: agent_id,
                failure: ExecFailure {
                    detail: redact_text(failure.detail, &secrets),
                    ..failure
                },
            }
        })
    }

    /// The live bridge for this program, workspace and key, started if none answers.
    async fn admit(
        &self,
        executable: &Path,
        workspace: &Path,
        api_key: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<Admission> {
        let key = self.hasher.hash_one((executable, workspace, api_key, env));
        let mut slot = self.slot.lock().await;
        if let Some(live) = slot.live.clone() {
            // Counted before the retired check, so a concurrent retire cannot end it under this turn.
            let admission = live.admit();
            let current = live.key == key && !live.retired.load(SeqCst);
            if current && live.healthy().await {
                return Ok(admission);
            }
            slot.live = None;
            if current {
                slot.failed(live.started.elapsed());
                live.kill.notify_one();
            }
            live.retire();
        }
        if let Some(retry_at) = slot.retry_at {
            tokio::time::sleep_until(retry_at).await;
        }
        match self.start(executable, workspace, api_key, env, key).await {
            Ok(live) => {
                let admission = live.admit();
                slot.live = Some(live);
                Ok(admission)
            }
            Err(error) => {
                slot.failed(Duration::ZERO);
                Err(error)
            }
        }
    }

    async fn start(
        &self,
        executable: &Path,
        workspace: &Path,
        api_key: &str,
        env: &BTreeMap<String, String>,
        key: u64,
    ) -> Result<Arc<Live>> {
        prepare_sdk_workspace(workspace).await?;
        let mut command = Command::new(executable);
        command
            .arg("--workspace")
            .arg(workspace)
            .arg("--state-root")
            .arg(workspace.join(".sdk"))
            .args(["--host", "127.0.0.1", "--port", "0"])
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("VIBEFACE_")
                || name.starts_with("CURSOR_SDK_")
                || matches!(
                    name.as_ref(),
                    "CURSOR_API_KEY"
                        | "HTTP_PROXY"
                        | "HTTPS_PROXY"
                        | "ALL_PROXY"
                        | "NO_PROXY"
                        | "http_proxy"
                        | "https_proxy"
                        | "all_proxy"
                        | "no_proxy"
                )
            {
                command.env_remove(key);
            }
        }
        command
            .envs(env)
            .envs(&self.env)
            .env("CURSOR_API_KEY", api_key)
            .env("CURSOR_SDK_CLIENT_LANGUAGE", "rust");
        let mut child = command.spawn().map_err(|error| {
            let kind = if error.kind() == std::io::ErrorKind::NotFound {
                FailureKind::MissingBinary
            } else {
                FailureKind::Unknown
            };
            ExecFailure::new(kind, format!("could not start cursor-sdk-bridge: {error}"))
        })?;
        let group = child.id().context("Cursor SDK process has no group")?;
        let stdout = child
            .stdout
            .take()
            .context("Cursor SDK stdout unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("Cursor SDK stderr unavailable")?;
        let (ready_tx, ready_rx) = oneshot::channel();
        let process = SdkProcess {
            child,
            group: Some(group),
            stdout: tokio::spawn(drain(stdout)),
            stderr: tokio::spawn(read_discovery(stderr, ready_tx)),
        };
        let kill = Arc::new(Notify::new());
        let (exited_tx, exited) = watch::channel(false);
        let tracked = self.processes.track(
            group,
            Tracked {
                executable: executable.to_owned(),
                kill: kill.clone(),
                exited: exited.clone(),
            },
        );
        tokio::spawn(supervise(
            process,
            kill.clone(),
            exited_tx,
            self.processes.clone(),
        ));
        let mut abandon = Abandon(Some(kill.clone()));
        tracked?;
        let mut died = exited.clone();
        let (rpc, agents) = tokio::select! {
            ready = tokio::time::timeout(STARTUP_TIMEOUT, self.connect(ready_rx)) => ready.map_err(|_| {
                ExecFailure::new(
                    FailureKind::TimedOut,
                    format!(
                        "Cursor SDK bridge did not start within {}s; the reply did not start",
                        STARTUP_TIMEOUT.as_secs()
                    ),
                )
            })??,
            _ = died.wait_for(|exited| *exited) => bail!("Cursor SDK bridge exited during startup"),
        };
        abandon.0 = None;
        Ok(Arc::new(Live {
            key,
            group,
            rpc,
            agents,
            started: Instant::now(),
            turns: AtomicUsize::new(0),
            retired: AtomicBool::new(false),
            kill,
            exited,
        }))
    }

    /// Reads the discovery line, then sets the Node's tool callback once for the bridge's life.
    async fn connect(
        &self,
        ready: oneshot::Receiver<Result<Discovery>>,
    ) -> Result<(Rpc, Option<Arc<Semaphore>>)> {
        let discovery = ready
            .await
            .context("Cursor SDK stopped before discovery")??;
        let endpoint = discovery.endpoint()?;
        let metadata = tokio::fs::symlink_metadata(&discovery.auth_token_file)
            .await
            .context("Cursor SDK token file unavailable")?;
        ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.permissions().mode() & 0o777 == 0o600
                && metadata.len() <= 4096,
            "Cursor SDK token file must be a private 0600 file"
        );
        let bearer = tokio::fs::read_to_string(&discovery.auth_token_file)
            .await
            .map_err(|_| anyhow!("could not read Cursor SDK token file"))?;
        let bearer = bearer.trim().to_owned();
        ensure!(
            !bearer.is_empty() && bearer.len() <= 4096,
            "invalid Cursor SDK bridge authentication token"
        );
        let client = reqwest::Client::builder()
            .http1_only()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .build()
            .map_err(|_| anyhow!("could not create Cursor SDK HTTP client"))?;
        let rpc = Rpc {
            endpoint,
            bearer,
            client,
        };
        let version = rpc.unary(CONTROL_SERVICE, "GetVersion", json!({})).await?;
        ensure!(
            version.get("protocolVersion").and_then(Value::as_str) == Some("sdk.v1"),
            "unsupported Cursor SDK protocol version"
        );
        rpc.unary(
            CONTROL_SERVICE,
            "SetToolCallback",
            json!({"url": self.callback.url, "authToken": self.callback.auth_token}),
        )
        .await?;
        let agents = discovery
            .max_concurrent_agents
            .filter(|limit| *limit > 0)
            .map(|limit| Arc::new(Semaphore::new(limit.min(Semaphore::MAX_PERMITS))));
        Ok((rpc, agents))
    }

    fn secrets<'a>(
        &'a self,
        api_key: &'a str,
        env: &'a BTreeMap<String, String>,
        bearer: &'a str,
    ) -> Vec<&'a str> {
        let mut secrets = vec![api_key, bearer, self.callback.auth_token.as_str()];
        secrets.extend(self.env.values().map(String::as_str));
        secrets.extend(env.values().map(String::as_str));
        secrets.retain(|secret| !secret.is_empty());
        secrets
    }
}

/// Owns the bridge process: exit, closed output or a kill request ends its whole group.
async fn supervise(
    mut process: SdkProcess,
    kill: Arc<Notify>,
    exited: watch::Sender<bool>,
    processes: Arc<Processes>,
) {
    tokio::select! {
        _ = &mut process.stdout => {}
        _ = &mut process.stderr => {}
        _ = process.child.wait() => {}
        _ = kill.notified() => {}
    }
    if let Some(group) = process.group {
        if terminate_process_group(&mut process.child, group).await {
            process.group = None;
        }
        processes.forget(group);
    }
    let _ = exited.send(true);
}

async fn prepare_sdk_workspace(workspace: &Path) -> Result<()> {
    for directory in [workspace, &workspace.join(".sdk")] {
        tokio::fs::create_dir_all(directory)
            .await
            .context("create Cursor SDK workspace")?;
        let metadata = tokio::fs::symlink_metadata(directory).await?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Cursor SDK workspace must be a real directory"
        );
        tokio::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

fn validate_turn(
    turn: &SdkTurn,
    prompt: &str,
    previous_session_id: Option<&str>,
) -> Result<(), ExecFailure> {
    if turn.model.trim().is_empty() {
        return Err(ExecFailure::new(
            FailureKind::NoModel,
            "Cursor SDK requires an explicit model",
        ));
    }
    validate_api_key(&turn.api_key)
        .map_err(|error| ExecFailure::new(FailureKind::MissingKey, error.to_string()))?;
    (|| {
        ensure!(turn.timeout_secs > 0, "Cursor SDK timeout must be positive");
        ensure!(
            !prompt.trim().is_empty() && prompt.len() <= MAX_PROMPT_BYTES,
            "invalid management prompt"
        );
        validate_launch_env(&turn.env)?;
        validate_tools(&turn.custom_tools)?;
        if let Some(id) = previous_session_id {
            validate_sdk_id(id)?;
        }
        Ok(())
    })()
    .map_err(|error: anyhow::Error| ExecFailure::new(FailureKind::Unknown, format!("{error:#}")))
}

/// A bridge that did not start: its own kind when it named one, else by the SDK's words.
fn start_failure(error: &anyhow::Error) -> ExecFailure {
    match error.downcast_ref::<ExecFailure>() {
        Some(failure) => failure.clone(),
        None => ExecFailure::upstream(format!("{error:#}"), FailureKind::Unknown),
    }
}

fn validate_api_key(api_key: &str) -> Result<()> {
    ensure!(
        !api_key.trim().is_empty() && api_key.len() <= 4096,
        "CURSOR_API_KEY is missing or invalid"
    );
    Ok(())
}

/// The engine's variables may name anything but what the bridge itself sets.
fn validate_launch_env(env: &BTreeMap<String, String>) -> Result<()> {
    ensure!(env.len() <= 64, "too many Cursor environment entries");
    for (key, value) in env {
        ensure!(
            !key.is_empty()
                && key.len() <= 256
                && key.bytes().enumerate().all(|(index, byte)| {
                    byte == b'_'
                        || byte.is_ascii_alphabetic()
                        || (index > 0 && byte.is_ascii_digit())
                })
                && key != "CURSOR_API_KEY"
                && !key.starts_with("CURSOR_SDK_")
                && !key.starts_with("VIBEFACE_")
                && value.len() <= 4096
                && !value.contains('\0'),
            "invalid or reserved Cursor environment entry"
        );
    }
    Ok(())
}

fn validate_env(env: &BTreeMap<String, String>) -> Result<()> {
    ensure!(env.len() <= 64, "too many tool environment entries");
    for (key, value) in env {
        ensure!(
            !key.is_empty()
                && key.len() <= 256
                && key.bytes().enumerate().all(|(index, byte)| {
                    byte == b'_'
                        || byte.is_ascii_alphabetic()
                        || (index > 0 && byte.is_ascii_digit())
                })
                && !matches!(
                    key.as_str(),
                    "CODEX_HOME"
                        | "CODEX_API_KEY"
                        | "CURSOR_API_KEY"
                        | "HOME"
                        | "PATH"
                        | "OPENAI_API_KEY"
                        | "OPENAI_BASE_URL"
                )
                && value.len() <= 4096
                && !value.contains('\0'),
            "invalid or reserved tool environment entry"
        );
    }
    Ok(())
}

fn validate_callback(callback: &ToolCallbackConfig) -> Result<()> {
    let url = reqwest::Url::parse(&callback.url).context("invalid custom tool callback URL")?;
    let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
    let ip: IpAddr = host
        .parse()
        .map_err(|_| anyhow!("custom tool callback must use a loopback IP"))?;
    ensure!(
        url.scheme() == "http"
            && ip.is_loopback()
            && url.port().is_some_and(|port| port > 0)
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none(),
        "custom tool callback must be a local HTTP origin"
    );
    ensure!(
        (32..=4096).contains(&callback.auth_token.len())
            && !callback.auth_token.chars().any(char::is_control),
        "invalid custom tool callback token"
    );
    Ok(())
}

fn validate_tools(tools: &BTreeMap<String, CustomToolConfig>) -> Result<()> {
    ensure!(tools.len() <= 32, "too many custom tools");
    for (name, tool) in tools {
        ensure!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'),
            "invalid custom tool name"
        );
        ensure!(
            tool.input_schema.is_object()
                && tool.output_schema.as_ref().is_none_or(Value::is_object),
            "custom tool schemas must be objects"
        );
        ensure!(
            serde_json::to_vec(tool)?.len() <= 32_768,
            "custom tool definition is too large"
        );
    }
    Ok(())
}

fn sdk_options(turn: &SdkTurn) -> Result<Value> {
    let workspace = turn
        .workspace
        .to_str()
        .context("Cursor SDK workspace path must be UTF-8")?;
    Ok(json!({"apiKey":turn.api_key,"model":{"id":turn.model},
        "local":{"cwd":[workspace],"settingSources":[],"customTools":turn.custom_tools},"mcpServers":{}}))
}

fn validate_sdk_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control),
        "invalid Cursor SDK id"
    );
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Discovery {
    schema_version: u32,
    transport: String,
    protocol: String,
    url: String,
    auth_token_file: PathBuf,
    /// Present only when the bridge was launched with `--max-concurrent-agents`.
    max_concurrent_agents: Option<usize>,
}

impl Discovery {
    fn endpoint(&self) -> Result<reqwest::Url> {
        ensure!(
            self.schema_version == 1 && self.transport == "tcp" && self.protocol == "connect",
            "unsupported Cursor SDK discovery protocol"
        );
        let url =
            reqwest::Url::parse(&self.url).map_err(|_| anyhow!("invalid Cursor SDK endpoint"))?;
        let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
        let ip: IpAddr = host
            .parse()
            .map_err(|_| anyhow!("Cursor SDK endpoint must use a loopback IP"))?;
        ensure!(
            url.scheme() == "http"
                && ip.is_loopback()
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_some_and(|port| port > 0)
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "Cursor SDK endpoint must be a local HTTP origin"
        );
        Ok(url)
    }
}

struct SdkProcess {
    child: Child,
    group: Option<u32>,
    stdout: JoinHandle<Result<()>>,
    stderr: JoinHandle<Result<()>>,
}
impl Drop for SdkProcess {
    fn drop(&mut self) {
        self.stdout.abort();
        self.stderr.abort();
        if let Some(group) = self.group.take() {
            signal_group_now(group, "-KILL");
        }
    }
}

async fn read_discovery(
    stderr: impl AsyncRead + Unpin,
    ready: oneshot::Sender<Result<Discovery>>,
) -> Result<()> {
    let mut reader = BufReader::new(stderr);
    loop {
        let line = bounded_line(&mut reader, 16_384).await?;
        if line.is_empty() {
            bail!("Cursor SDK closed stderr before discovery");
        }
        if let Some(payload) = line.strip_prefix(b"cursor-sdk-bridge ready ") {
            let discovery = serde_json::from_slice(payload)
                .map_err(|_| anyhow!("Cursor SDK emitted invalid discovery JSON"));
            let _ = ready.send(discovery);
            return forward(reader).await;
        }
    }
}

/// The bridge's diagnostics after discovery, one log record per line.
async fn forward(mut reader: impl AsyncBufRead + Unpin) -> Result<()> {
    loop {
        let mut line = Vec::new();
        if (&mut reader)
            .take(16_384)
            .read_until(b'\n', &mut line)
            .await?
            == 0
        {
            return Ok(());
        }
        let line = String::from_utf8_lossy(&line);
        let line = line.trim_end();
        if !line.is_empty() {
            tracing::info!(source = "cursor", event = "cursor.stderr", "{line}");
        }
    }
}

async fn drain(mut output: impl AsyncRead + Unpin) -> Result<()> {
    let mut buffer = [0; 4096];
    while output.read(&mut buffer).await? != 0 {}
    Ok(())
}

struct Rpc {
    endpoint: reqwest::Url,
    bearer: String,
    client: reqwest::Client,
}

impl Rpc {
    fn request(&self, service: &str, method: &str) -> Result<reqwest::RequestBuilder> {
        let url = self
            .endpoint
            .join(&format!("sdk.v1.{service}/{method}"))
            .map_err(|_| anyhow!("invalid Cursor SDK RPC endpoint"))?;
        Ok(self
            .client
            .post(url)
            .bearer_auth(&self.bearer)
            .header("Connect-Protocol-Version", "1"))
    }

    async fn unary(&self, service: &str, method: &str, body: Value) -> Result<Value> {
        let mut response = self
            .request(service, method)?
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|error| anyhow!("Cursor SDK {method} transport failed: {error}"))?;
        ensure!(
            response.status().is_success(),
            "Cursor SDK {method} failed with HTTP {}",
            response.status().as_u16()
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!("Cursor SDK {method} response failed"))?
        {
            ensure!(
                bytes.len() + chunk.len() <= MAX_FRAME_BYTES,
                "Cursor SDK unary response is too large"
            );
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("Cursor SDK {method} returned invalid JSON"))
    }

    async fn send<P, PF>(
        &self,
        agent_id: &str,
        text: &str,
        state: &mut RunState,
        progress: &mut P,
    ) -> Result<()>
    where
        P: FnMut(Value) -> PF,
        PF: Future<Output = Result<()>>,
    {
        let body = encode_frame(
            0,
            &json!({"agentId": agent_id, "message": {"text": text}, "options":{"enableDeltas":true}}),
        )?;
        let mut response = self
            .request(AGENT_SERVICE, "Send")?
            .header("Content-Type", "application/connect+json")
            .header("Connect-Accept-Encoding", "identity")
            .body(body)
            .send()
            .await
            .map_err(|_| anyhow!("Cursor SDK Send transport failed; completion is unknown"))?;
        ensure!(
            response.status().is_success(),
            "Cursor SDK Send failed with HTTP {}",
            response.status().as_u16()
        );
        let mut decoder = FrameDecoder::default();
        let mut total = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!("Cursor SDK run stream disconnected; completion is unknown"))?
        {
            total += chunk.len();
            ensure!(
                total <= MAX_STREAM_BYTES,
                "Cursor SDK run stream limit exceeded"
            );
            for (flags, message) in decoder.push(&chunk)? {
                state.accept(flags, message, agent_id, progress).await?;
            }
        }
        decoder.finish()?;
        ensure!(
            state.ended,
            "Cursor SDK stream ended without an EndStream trailer; completion is unknown"
        );
        Ok(())
    }
}

fn encode_frame(flags: u8, value: &Value) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(value)?;
    ensure!(
        payload.len() <= MAX_FRAME_BYTES,
        "Cursor SDK frame is too large"
    );
    let mut frame = Vec::with_capacity(payload.len() + 5);
    frame.push(flags);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

#[derive(Default)]
struct FrameDecoder {
    pending: Vec<u8>,
}
impl FrameDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<(u8, Value)>> {
        ensure!(
            self.pending.len() + bytes.len() <= MAX_STREAM_BYTES,
            "Cursor SDK frame buffer limit exceeded"
        );
        self.pending.extend_from_slice(bytes);
        let mut frames = Vec::new();
        let mut consumed = 0;
        while self.pending.len() - consumed >= 5 {
            let flags = self.pending[consumed];
            ensure!(
                matches!(flags, 0 | 2),
                "unsupported Cursor SDK Connect frame flags"
            );
            let size =
                u32::from_be_bytes(self.pending[consumed + 1..consumed + 5].try_into().unwrap())
                    as usize;
            ensure!(size <= MAX_FRAME_BYTES, "Cursor SDK frame is too large");
            if self.pending.len() - consumed < size + 5 {
                break;
            }
            let value = serde_json::from_slice(&self.pending[consumed + 5..consumed + 5 + size])
                .map_err(|_| anyhow!("Cursor SDK frame contains invalid JSON"))?;
            frames.push((flags, value));
            consumed += size + 5;
        }
        self.pending.drain(..consumed);
        Ok(frames)
    }
    fn finish(&self) -> Result<()> {
        ensure!(
            self.pending.is_empty(),
            "Cursor SDK stream ended within a frame; completion is unknown"
        );
        Ok(())
    }
}

#[derive(Default)]
struct RunState {
    run_id: Option<String>,
    output: Option<String>,
    usage: Option<Value>,
    done: bool,
    ended: bool,
    text_deltas: bool,
    assistant_text: String,
}
impl RunState {
    fn identity(&mut self, message: &Value, expected_agent: &str, required: bool) -> Result<()> {
        if let Some(agent_id) = message
            .get("agentId")
            .or_else(|| message.get("agent_id"))
            .and_then(Value::as_str)
        {
            ensure!(
                agent_id == expected_agent,
                "Cursor SDK stream contains a different agent"
            );
        } else {
            ensure!(!required, "Cursor SDK terminal envelope has no agent id");
        }
        if let Some(run_id) = message
            .get("runId")
            .or_else(|| message.get("run_id"))
            .and_then(Value::as_str)
        {
            validate_sdk_id(run_id)?;
            if let Some(previous) = &self.run_id {
                ensure!(
                    previous == run_id,
                    "Cursor SDK stream contains a different run"
                );
            }
            self.run_id = Some(run_id.into());
        } else {
            ensure!(!required, "Cursor SDK terminal envelope has no run id");
        }
        Ok(())
    }

    async fn accept<P, PF>(
        &mut self,
        flags: u8,
        message: Value,
        expected_agent: &str,
        progress: &mut P,
    ) -> Result<()>
    where
        P: FnMut(Value) -> PF,
        PF: Future<Output = Result<()>>,
    {
        ensure!(!self.ended, "Cursor SDK emitted data after EndStream");
        if flags == 2 {
            if let Some(error) = message.get("error").filter(|value| !value.is_null()) {
                let code = error
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let detail: String = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("stream failed")
                    .chars()
                    .take(1024)
                    .collect();
                bail!("Cursor SDK Connect error {code}: {detail}");
            }
            ensure!(
                message.is_object() && message.get("error").is_none_or(Value::is_null),
                "Cursor SDK Connect stream failed"
            );
            ensure!(
                self.output.is_some() && self.done,
                "Cursor SDK stream closed before result and done"
            );
            self.ended = true;
            progress(json!({"type":"status","status":"completed"})).await?;
            return Ok(());
        }
        ensure!(!self.done, "Cursor SDK emitted data after done");
        if let Some(result) = message.get("result") {
            ensure!(
                self.output.is_none(),
                "Cursor SDK emitted duplicate terminal results"
            );
            self.identity(result, expected_agent, true)?;
            ensure!(
                finished(result.get("status")),
                "Cursor SDK run did not finish successfully"
            );
            ensure!(
                result
                    .get("errorCode")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty),
                "Cursor SDK run reported a terminal error"
            );
            let result = result
                .get("result")
                .context("Cursor SDK terminal envelope has no result")?;
            self.identity(result, expected_agent, true)?;
            ensure!(
                finished(result.get("status")),
                "Cursor SDK result did not finish successfully"
            );
            let text = result
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or_default();
            ensure!(
                text.len() <= MAX_RESULT_BYTES,
                "Cursor SDK result is too large"
            );
            self.output = Some(text.into());
            progress(json!({"type":"text_snapshot","item_id":"assistant-0","text":safe_text(&json!(text)).unwrap_or_default()})).await?;
            self.usage = result
                .get("usage")
                .filter(|value| !value.is_null())
                .and_then(safe_usage);
            let mut event = Map::new();
            event.insert("type".into(), Value::String("status".into()));
            event.insert("status".into(), Value::String("finishing".into()));
            if let Some(run_id) = &self.run_id {
                event.insert("run_id".into(), Value::String(run_id.clone()));
            }
            if let Some(usage) = &self.usage {
                event.insert("usage".into(), usage.clone());
            }
            progress(Value::Object(event)).await?;
        } else if let Some(done) = message.get("done") {
            self.identity(done, expected_agent, true)?;
            ensure!(
                self.output.is_some(),
                "Cursor SDK emitted done before a successful result"
            );
            self.done = true;
        } else if let Some(envelope) = message.get("sdkMessage") {
            let Some(event) = envelope.get("message") else {
                return Ok(());
            };
            self.identity(event, expected_agent, false)?;
            if let Some(status) = event.get("status").and_then(Value::as_str) {
                ensure!(
                    !matches!(
                        status.to_ascii_uppercase().as_str(),
                        "ERROR"
                            | "CANCELLED"
                            | "EXPIRED"
                            | "RUN_LIFECYCLE_STATUS_ERROR"
                            | "RUN_LIFECYCLE_STATUS_CANCELLED"
                            | "RUN_LIFECYCLE_STATUS_EXPIRED"
                    ),
                    "Cursor SDK run reported failure or cancellation"
                );
            }
            let kind = envelope
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if kind == "assistant" {
                // The bridge can mirror each text fragment in both envelopes.
                if !self.text_deltas
                    && let Some(mut progress_event) = safe_progress_event(kind, event)
                {
                    self.assistant_text
                        .push_str(progress_event["text"].as_str().unwrap_or_default());
                    self.assistant_text = self.assistant_text.chars().take(16_384).collect();
                    progress_event["item_id"] = json!("assistant-0");
                    progress_event["text"] = json!(self.assistant_text);
                    progress(progress_event).await?;
                }
            } else if let Some(progress_event) = safe_progress_event(kind, event) {
                progress(progress_event).await?;
            }
        } else if let Some(envelope) = message.get("interactionUpdate") {
            let kind = envelope
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if kind == "text-delta"
                && let Some(event) = envelope.get("update")
            {
                self.identity(event, expected_agent, false)?;
                if let Some(text) = event.get("text").and_then(safe_text) {
                    if !self.text_deltas {
                        self.text_deltas = true;
                        if !self.assistant_text.is_empty() {
                            progress(
                                json!({"type":"text_snapshot","item_id":"assistant-0","text":""}),
                            )
                            .await?;
                            self.assistant_text.clear();
                        }
                    }
                    progress(json!({"type":"text_delta","item_id":"assistant-0","text":text}))
                        .await?;
                }
            }
        }
        Ok(())
    }

    fn finish(&self, agent_id: &str) -> Result<AgentResult> {
        ensure!(
            self.done && self.ended,
            "Cursor SDK run has no successful stream completion"
        );
        Ok(AgentResult {
            session_id: Some(agent_id.into()),
            usage: self.usage.clone(),
            output: self
                .output
                .clone()
                .context("Cursor SDK run has no result")?,
        })
    }
}

fn safe_label(value: &Value) -> Option<Value> {
    match value {
        Value::String(text)
            if !text.is_empty() && text.len() <= 128 && !text.chars().any(char::is_control) =>
        {
            Some(Value::String(text.clone()))
        }
        _ => None,
    }
}

fn safe_usage(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    let mut output = Map::new();
    for key in [
        "inputTokens",
        "outputTokens",
        "cacheReadTokens",
        "totalTokens",
        "costCents",
    ] {
        let Some(value) = object.get(key) else {
            continue;
        };
        let valid = match value {
            Value::Number(_) => true,
            Value::String(text) => {
                text.len() <= 64
                    && !text.is_empty()
                    && text
                        .chars()
                        .all(|ch| ch.is_ascii_digit() || ".-".contains(ch))
            }
            _ => false,
        };
        if valid {
            output.insert(key.into(), value.clone());
        }
    }
    (!output.is_empty()).then_some(Value::Object(output))
}

fn safe_text(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    Some(
        text.chars()
            .filter(|ch| !ch.is_control() || matches!(*ch, '\n' | '\r' | '\t'))
            .take(16_384)
            .collect(),
    )
}

fn safe_progress_event(kind: &str, event: &Value) -> Option<Value> {
    let object = event.as_object()?;
    let mut output = Map::new();
    match kind {
        "assistant" => {
            let blocks = object.get("message")?.get("content")?.as_array()?;
            let text: String = blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block.get("text").and_then(safe_text))
                .collect();
            output.insert("type".into(), json!("text_snapshot"));
            output.insert(
                "text".into(),
                json!(text.chars().take(16_384).collect::<String>()),
            );
        }
        "tool_call" => {
            output.insert("type".into(), json!("tool"));
            for key in ["name", "status"] {
                if let Some(value) = object.get(key).and_then(safe_label) {
                    output.insert(key.into(), value);
                }
            }
            if !output.contains_key("name") && !output.contains_key("status") {
                return None;
            }
            if let Some(id) = object
                .get("call_id")
                .or_else(|| object.get("id"))
                .and_then(safe_label)
            {
                output.insert("item_id".into(), id);
            }
        }
        "system" | "status" => {
            output.insert("type".into(), json!("status"));
            if let Some(value) = object
                .get("status")
                .or_else(|| object.get("subtype"))
                .and_then(safe_label)
            {
                output.insert("status".into(), value);
            }
            if kind == "status"
                && let Some(text) = object.get("message").and_then(safe_text)
                && !text.is_empty()
            {
                output.insert("text".into(), json!(text));
            }
            if !output.contains_key("status") && !output.contains_key("text") {
                return None;
            }
        }
        _ => return None,
    }
    if let Some(usage) = object.get("usage").and_then(safe_usage) {
        output.insert("usage".into(), usage);
    }
    (output.len() > 1).then_some(Value::Object(output))
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

fn finished(status: Option<&Value>) -> bool {
    status.is_some_and(|status| {
        status.as_str() == Some("RUN_LIFECYCLE_STATUS_FINISHED") || status.as_u64() == Some(3)
    })
}

fn cancelled(status: Option<&Value>) -> bool {
    status.is_some_and(|status| {
        status.as_str() == Some("RUN_LIFECYCLE_STATUS_CANCELLED") || status.as_u64() == Some(5)
    })
}

fn failed(status: Option<&Value>) -> bool {
    status.is_some_and(|status| {
        matches!(
            status.as_str(),
            Some("RUN_LIFECYCLE_STATUS_ERROR" | "RUN_LIFECYCLE_STATUS_EXPIRED")
        ) || matches!(status.as_u64(), Some(4 | 6))
    })
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
mod tests {
    use super::*;
    fn result(status: Value) -> Value {
        json!({"result": {"agentId": "a", "runId": "r", "status": status,
            "result": {"agentId": "a", "runId": "r", "status": status, "result": "Needs approval", "usage": {"inputTokens": "42", "outputTokens": "7"}}}})
    }

    #[tokio::test]
    async fn fragmented_connect_stream_requires_result_done_and_trailer() {
        let messages = [
            (0, result(json!("RUN_LIFECYCLE_STATUS_FINISHED"))),
            (0, json!({"done": {"agentId": "a", "runId": "r"}})),
            (2, json!({})),
        ];
        let bytes: Vec<_> = messages
            .iter()
            .flat_map(|(flags, value)| encode_frame(*flags, value).unwrap())
            .collect();
        let mut decoder = FrameDecoder::default();
        let mut state = RunState::default();
        let mut progress = |_| async { Ok(()) };
        for byte in bytes {
            for (flags, message) in decoder.push(&[byte]).unwrap() {
                state
                    .accept(flags, message, "a", &mut progress)
                    .await
                    .unwrap();
            }
        }
        decoder.finish().unwrap();
        let output = state.finish("a").unwrap();
        assert_eq!(output.session_id.as_deref(), Some("a"));
        assert_eq!(output.output, "Needs approval");
        assert_eq!(
            output.usage,
            Some(json!({"inputTokens":"42","outputTokens":"7"}))
        );
        let mut missing_trailer = RunState::default();
        let mut progress = |_| async { Ok(()) };
        missing_trailer
            .accept(0, result(json!(3)), "a", &mut progress)
            .await
            .unwrap();
        missing_trailer
            .accept(
                0,
                json!({"done": {"agentId": "a", "runId": "r"}}),
                "a",
                &mut progress,
            )
            .await
            .unwrap();
        assert!(missing_trailer.finish("a").is_err());
    }

    #[tokio::test]
    async fn connect_errors_and_unsuccessful_runs_never_complete() {
        for status in [
            "RUN_LIFECYCLE_STATUS_ERROR",
            "RUN_LIFECYCLE_STATUS_CANCELLED",
            "RUN_LIFECYCLE_STATUS_EXPIRED",
        ] {
            let mut state = RunState::default();
            let mut progress = |_| async { Ok(()) };
            assert!(
                state
                    .accept(0, result(json!(status)), "a", &mut progress)
                    .await
                    .is_err()
            );
        }
        let mut state = RunState::default();
        let mut progress = |_| async { Ok(()) };
        state
            .accept(0, result(json!(3)), "a", &mut progress)
            .await
            .unwrap();
        state
            .accept(
                0,
                json!({"done": {"agentId": "a", "runId": "r"}}),
                "a",
                &mut progress,
            )
            .await
            .unwrap();
        assert!(
            state
                .accept(
                    2,
                    json!({"error": {"code": "internal"}}),
                    "a",
                    &mut progress
                )
                .await
                .is_err()
        );
        let mut decoder = FrameDecoder::default();
        decoder.push(&[0, 0, 0]).unwrap();
        assert!(decoder.finish().is_err());
        assert!(
            FrameDecoder::default()
                .push(&[0, 0xff, 0xff, 0xff, 0xff])
                .is_err()
        );
    }

    #[test]
    fn native_custom_tools_need_a_private_local_callback_and_no_mcp_server() {
        let turn = SdkTurn {
            executable: "/sdk".into(),
            workspace: "/manager".into(),
            api_key: "fixture-key".into(),
            env: BTreeMap::new(),
            timeout_secs: 5,
            model: "fixture-model".into(),
            process_started: None,
            custom_tools: BTreeMap::from([(
                "context".into(),
                CustomToolConfig {
                    description: Some("Read visible business context".into()),
                    input_schema: json!({"type":"object"}),
                    output_schema: None,
                },
            )]),
        };
        let options = sdk_options(&turn).unwrap();
        assert_eq!(options["mcpServers"], json!({}));
        assert!(options.get("tools").is_none());
        assert_eq!(
            options["local"]["customTools"]["context"]["inputSchema"],
            json!({"type":"object"})
        );
        let callback = ToolCallbackConfig {
            url: "http://127.0.0.1:1234".into(),
            auth_token: "x".repeat(32),
        };
        assert!(SdkBridge::new(BTreeMap::new(), callback.clone(), None).is_ok());
        let reserved = BTreeMap::from([("CURSOR_API_KEY".into(), "override".into())]);
        assert!(SdkBridge::new(reserved, callback.clone(), None).is_err());
        let remote = ToolCallbackConfig {
            url: "http://192.0.2.1:1234".into(),
            ..callback
        };
        assert!(SdkBridge::new(BTreeMap::new(), remote, None).is_err());
    }

    #[tokio::test]
    async fn progress_callback_only_receives_bounded_whitelisted_fields() {
        let mut state = RunState::default();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = captured.clone();
        let mut progress = move |value| {
            sink.lock().unwrap().push(value);
            async { Ok(()) }
        };
        state
            .accept(
                0,
                json!({"sdkMessage":{"type":"assistant","message":{"message":{"content":[{"type":"text","text":"x".repeat(20000)}]},"secret":"do-not-forward"}}}),
                "a",
                &mut progress,
            )
            .await
            .unwrap();
        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "text_snapshot");
        assert!(events[0]["text"].as_str().unwrap().len() <= 16_384);
        assert!(events[0].get("secret").is_none());
    }

    #[tokio::test]
    async fn cursor_text_deltas_ignore_mirrored_assistant_content_and_keep_newlines() {
        let mut state = RunState::default();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = captured.clone();
        let mut progress = move |value| {
            sink.lock().unwrap().push(value);
            async { Ok(()) }
        };
        state.accept(0,json!({"interactionUpdate":{"type":"text-delta","update":{"type":"text-delta","text":"first\n\tsecond\u{0000}"}}}),"a",&mut progress).await.unwrap();
        state.accept(0,json!({"sdkMessage":{"type":"assistant","message":{"message":{"content":[{"type":"text","text":"first\n\tsecond"},{"type":"tool_use","input":{"secret":"hidden"}}]}}}}),"a",&mut progress).await.unwrap();
        state
            .accept(
                0,
                json!({"interactionUpdate":{"type":"thinking-delta","update":{"text":"private"}}}),
                "a",
                &mut progress,
            )
            .await
            .unwrap();
        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            json!({"type":"text_delta","item_id":"assistant-0","text":"first\n\tsecond"})
        );
        assert!(
            !events
                .iter()
                .any(|event| event.to_string().contains("hidden"))
        );
        assert!(cancelled(Some(&json!(5))));
        assert!(!cancelled(Some(&json!(3))));
    }

    #[tokio::test]
    async fn cursor_unkeyed_assistant_fragments_and_deltas_render_exactly_once() {
        for deltas in [false, true] {
            for assistant_first in [false, true] {
                let mut state = RunState::default();
                let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let sink = captured.clone();
                let mut progress = move |value| {
                    sink.lock().unwrap().push(value);
                    async { Ok(()) }
                };
                for text in ["first\n", "\tsecond", "!"] {
                    let assistant = json!({"sdkMessage":{"type":"assistant","message":{"type":"assistant","agent_id":"a","run_id":"r","message":{"role":"assistant","content":[{"type":"text","text":text}]}}}});
                    let delta = json!({"interactionUpdate":{"type":"text-delta","update":{"type":"text-delta","text":text}}});
                    let messages = if !deltas {
                        vec![assistant]
                    } else if assistant_first {
                        vec![assistant, delta]
                    } else {
                        vec![delta, assistant]
                    };
                    for message in messages {
                        state.accept(0, message, "a", &mut progress).await.unwrap();
                    }
                }
                let mut preview = String::new();
                for event in captured.lock().unwrap().iter() {
                    assert_eq!(event["item_id"], "assistant-0");
                    let text = event["text"].as_str().unwrap();
                    match event["type"].as_str().unwrap() {
                        "text_snapshot" => preview = text.to_owned(),
                        "text_delta" => preview.push_str(text),
                        kind => panic!("unexpected text event {kind}"),
                    }
                }
                assert_eq!(
                    preview, "first\n\tsecond!",
                    "deltas={deltas},assistant_first={assistant_first}"
                );
            }
        }
    }

    #[tokio::test]
    async fn cursor_final_snapshot_restores_safe_secret_prefix_tail() {
        for (text, expected) in [("yes", "yes"), ("sk-secret", "[redacted]")] {
            let mut state = RunState::default();
            let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = captured.clone();
            let mut pending = BTreeMap::new();
            let mut progress = move |value| {
                sink.lock().unwrap().push(redact_stream_progress(
                    value,
                    &["sk-secret"],
                    &mut pending,
                ));
                async { Ok(()) }
            };
            state
                .accept(
                    0,
                    json!({"interactionUpdate":{"type":"text-delta","update":{"text":text}}}),
                    "a",
                    &mut progress,
                )
                .await
                .unwrap();
            let mut terminal = result(json!(3));
            terminal["result"]["result"]["result"] = json!(text);
            state.accept(0, terminal, "a", &mut progress).await.unwrap();
            let mut preview = String::new();
            for event in captured.lock().unwrap().iter() {
                match event["type"].as_str().unwrap() {
                    "text_snapshot" => preview = event["text"].as_str().unwrap().to_owned(),
                    "text_delta" => preview.push_str(event["text"].as_str().unwrap()),
                    _ => {}
                }
            }
            assert_eq!(preview, expected);
        }
    }

    #[test]
    fn progress_redaction_covers_nested_values() {
        let redacted = redact_progress(
            json!({"text": "prefix api-key bearer-token", "items": ["api-key"]}),
            &["api-key", "bearer-token"],
        );
        assert_eq!(
            redacted,
            json!({"text": "prefix [redacted] [redacted]", "items": ["[redacted]"]})
        );
    }

    #[test]
    fn cursor_status_and_tool_fields_match_the_live_progress_contract() {
        assert_eq!(
            safe_progress_event("system", &json!({"subtype":"init"})),
            Some(json!({"type":"status","status":"init"}))
        );
        assert_eq!(
            safe_progress_event("status", &json!({"status":"RUNNING","message":""})),
            Some(json!({"type":"status","status":"RUNNING"}))
        );
        assert_eq!(
            safe_progress_event(
                "tool_call",
                &json!({"call_id":"call-1","name":"context","status":"completed","args":{"secret":"hidden"},"result":{"secret":"hidden"}})
            ),
            Some(json!({"type":"tool","item_id":"call-1","name":"context","status":"completed"}))
        );
        assert!(
            safe_progress_event(
                "tool_call",
                &json!({"name":"","status":3,"call_id":"call-1"})
            )
            .is_none()
        );
        assert!(safe_progress_event("status", &json!({"status":3,"message":""})).is_none());
        assert!(safe_progress_event("usage", &json!({"usage":{"inputTokens":3}})).is_none());
    }

    #[tokio::test]
    async fn cursor_terminal_progress_reports_canonical_status_without_native_numbers() {
        let mut state = RunState::default();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = captured.clone();
        let mut progress = move |value| {
            sink.lock().unwrap().push(value);
            async { Ok(()) }
        };
        for (flags, message) in [
            (0, result(json!(3))),
            (0, json!({"done":{"agentId":"a","runId":"r"}})),
            (2, json!({})),
        ] {
            state
                .accept(flags, message, "a", &mut progress)
                .await
                .unwrap();
        }
        let events = captured.lock().unwrap();
        let statuses: Vec<_> = events
            .iter()
            .filter(|value| value["type"] == "status")
            .cloned()
            .collect();
        assert_eq!(
            statuses,
            vec![
                json!({"type":"status","status":"finishing","run_id":"r","usage":{"inputTokens":"42","outputTokens":"7"}}),
                json!({"type":"status","status":"completed"})
            ]
        );
    }

    #[test]
    fn local_reap_without_provider_terminal_stays_unknown() {
        assert_eq!(provider_cancel_failure(None), AgentFailure::Unknown);
        assert_eq!(
            provider_cancel_failure(Some(AgentFailure::Cancelled)),
            AgentFailure::Cancelled
        );
        assert_eq!(
            provider_cancel_failure(Some(AgentFailure::Failed)),
            AgentFailure::Failed
        );
    }

    #[test]
    fn text_delta_redaction_holds_partial_secrets_across_frames() {
        for (secret, fragments) in [
            ("sk-secret", vec!["prefix sk-", "sec", "ret suffix"]),
            ("aba", vec!["a", "b", "a"]),
            ("密钥", vec!["密", "钥"]),
        ] {
            let mut pending = BTreeMap::new();
            let output: String = fragments
                .into_iter()
                .map(|text| {
                    redact_stream_progress(
                        json!({"type":"text_delta","item_id":"a","text":text}),
                        &[secret],
                        &mut pending,
                    )["text"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect();
            assert!(!output.contains(secret));
            assert!(output.contains("[redacted]"));
        }
    }

    #[test]
    fn discovery_rejects_nonlocal_and_redirectable_endpoints() {
        let mut ready = Discovery {
            schema_version: 1,
            transport: "tcp".into(),
            protocol: "connect".into(),
            url: "http://127.0.0.1:1234".into(),
            auth_token_file: "/token".into(),
            max_concurrent_agents: None,
        };
        assert!(ready.endpoint().is_ok());
        for url in [
            "https://127.0.0.1:1234",
            "http://example.com:1234",
            "http://192.0.2.1:1234",
            "http://secret@127.0.0.1:1234",
            "http://127.0.0.1:1234/path",
        ] {
            ready.url = url.into();
            assert!(ready.endpoint().is_err());
        }
    }
}
