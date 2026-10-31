//! The coordination runtime's App Server: the process, its socket and the Node's own client.
//! A managed App Server runs in its own process group recorded in a pidfile, so a crash of
//! this Node never leaves an unmanaged one behind: the next start kills the recorded group.
use super::executor::{Executor, SteerAck, SteerTrace};
use super::*;
use crate::providers::{
    Authentication, CatalogCoverage, CodexLaunch, CodexTarget, EffortOptions, ModelCatalog,
    ModelChoice,
};
use crate::scheduling::{SteerOutcome, SteerTarget};
use crate::work::{Probe, Signal, WorkRecord, WorkSpec};
use futures_util::future::BoxFuture;
use std::os::unix::process::CommandExt as _;
use vibeface_bridge::{
    codex::{CodexClient, CodexError, ExecEvent},
    turn::{TurnStatus, turn_snapshot},
};

fn last_answer(turn: &Value) -> String {
    turn["items"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .find(|item| item["type"] == "agentMessage")
        .and_then(|item| item["text"].as_str())
        .unwrap_or_default()
        .into()
}

fn missing_thread(error: &CodexError) -> bool {
    if !error.answered {
        return false;
    }
    let missing = |message: &str| message.to_ascii_lowercase().starts_with("thread not found");
    if let Some(body) = error.message.strip_prefix("Codex rejected request: ") {
        let Ok(body) = serde_json::from_str::<Value>(body) else {
            return false;
        };
        body["message"].as_str().is_some_and(missing)
    } else {
        missing(&error.message)
    }
}

/// Whether a turn carries the user message the caller sent with this `clientUserMessageId`.
pub(crate) fn sent_by(turn: &Value, client_id: &str) -> bool {
    turn["items"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| item["type"] == "userMessage" && item["clientId"] == client_id)
}

/// A plan as one line per step, the current step marked, for the persisted progress.
fn plan_summary(params: &Value) -> String {
    params["plan"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|step| {
            let mark = match step["status"].as_str() {
                Some("completed") => "✓",
                Some("inProgress") => "→",
                _ => "·",
            };
            format!("{mark} {}", step["step"].as_str().unwrap_or_default())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn steer_trace(turns: &[Value], message_id: &str, target: &str) -> SteerTrace {
    let Some(turn) = turns.iter().find(|turn| sent_by(turn, message_id)) else {
        return match turns.iter().find(|turn| turn["id"] == target) {
            None => SteerTrace::TargetUnseen,
            Some(turn) if turn_snapshot(turn).status == Some(TurnStatus::InProgress) => {
                SteerTrace::Pending
            }
            Some(_) => SteerTrace::NotArrived,
        };
    };
    match turn_snapshot(turn).status {
        Some(TurnStatus::InProgress) => SteerTrace::Pending,
        Some(TurnStatus::Completed) if turn["id"] == target => {
            SteerTrace::Ended(SteerOutcome::Answered)
        }
        _ => SteerTrace::Ended(SteerOutcome::Unfinished),
    }
}

/// What an App Server notification says about a turn's work.
pub(crate) struct TurnEvent<'a> {
    pub thread: Option<&'a str>,
    pub turn: Option<&'a str>,
    /// The turn started or ended.
    pub boundary: bool,
    pub signal: Option<Signal>,
}

pub(crate) fn turn_event(event: &Value) -> TurnEvent<'_> {
    let method = event["method"].as_str().unwrap_or_default();
    let params = &event["params"];
    let turn = params["turnId"]
        .as_str()
        .or_else(|| params["turn"]["id"].as_str());
    let item = &params["item"];
    let signal = match method {
        "item/completed" if item["type"] == "agentMessage" && item["phase"] == "commentary" => {
            Some(Signal::Progress {
                summary: item["text"].as_str().unwrap_or_default().into(),
                at_ms: now_ms(),
            })
        }
        "item/completed"
            if item["type"] == "agentMessage"
                && (item["phase"].is_null() || item["phase"] == "final_answer") =>
        {
            Some(Signal::Text(
                item["text"].as_str().unwrap_or_default().into(),
            ))
        }
        "turn/plan/updated" => Some(Signal::Progress {
            summary: plan_summary(params),
            at_ms: now_ms(),
        }),
        "turn/completed" => {
            let ended = turn_snapshot(&params["turn"]);
            Some(Signal::TurnCompleted {
                turn_id: turn.unwrap_or_default().into(),
                status: ended
                    .status
                    .unwrap_or_else(|| TurnStatus::Other("unknown".into())),
                text: String::new(),
                error: ended.error,
            })
        }
        _ => None,
    };
    TurnEvent {
        thread: params["threadId"].as_str(),
        turn,
        boundary: matches!(method, "turn/started" | "turn/completed"),
        signal,
    }
}

impl CodexServer {
    pub(crate) fn client(&self) -> &CodexClient {
        &self.client
    }
    /// Threads the client re-subscribes after every reconnect; requests on them come to the
    /// Node, which hands them to the coordination Agent.
    pub(crate) fn observe(&self, threads: Vec<String>) -> Result<(), CodexError> {
        self.client.set_observed_threads(threads)
    }
    pub(crate) async fn status(&self) -> Value {
        self.client.status().await
    }
    pub(crate) async fn capability(&self) -> Value {
        json!({"connected":self.client.status().await["connection"]["state"] == "connected"})
    }
    pub(crate) async fn interrupt(&self, thread: &str, turn: &str) -> Result<(), CodexError> {
        self.client
            .request("turn/interrupt", json!({"threadId":thread,"turnId":turn}))
            .await
            .map(|_| ())
    }

    /// The connection the turns the Node reads now run under; see `Handle::epoch`.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::SeqCst)
    }
    /// Reads the journaled thread; it never starts or replays work. Without a known turn, the
    /// turn is the one whose user message carries `client_id`.
    pub(crate) async fn probe(
        &self,
        thread: &str,
        turn: Option<&str>,
        client_id: &str,
    ) -> Result<Probe, CodexError> {
        let epoch = self.epoch();
        let reply = match self
            .client
            .request(
                "thread/read",
                json!({"threadId":thread,"includeTurns":true}),
            )
            .await
        {
            Ok(reply) => reply,
            // Only the App Server saying it has no such thread makes the turn missing.
            Err(error) if missing_thread(&error) => {
                self.inner.release(&format!("manager:{client_id}"));
                return Ok(Probe::Missing);
            }
            Err(error) => return Err(error),
        };
        let turns = reply["thread"]["turns"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let found = match turn {
            Some(turn) => turns.iter().find(|t| t["id"] == turn),
            None => match turns.iter().find(|t| sent_by(t, client_id)) {
                Some(found) => Some(found),
                None => {
                    self.inner.release(&format!("manager:{client_id}"));
                    return Ok(Probe::NotStarted);
                }
            },
        };
        let Some(found) = found else {
            self.inner.release(&format!("manager:{client_id}"));
            return Ok(Probe::Missing);
        };
        let snapshot = turn_snapshot(found);
        let turn_id = snapshot.id.unwrap_or_default();
        let probe = match snapshot.status {
            Some(TurnStatus::InProgress) => Probe::Running { turn_id, epoch },
            Some(
                status @ (TurnStatus::Completed | TurnStatus::Interrupted | TurnStatus::Failed),
            ) => Probe::Terminal {
                turn_id,
                status,
                text: last_answer(found),
                error: snapshot.error,
            },
            _ => {
                return Err(CodexError {
                    uncertain: false,
                    answered: true,
                    message: "Codex did not report a known turn status".into(),
                });
            }
        };
        if !matches!(probe, Probe::Running { .. }) {
            self.inner.release(&format!("manager:{client_id}"));
        }
        Ok(probe)
    }
}

/// A Codex work is sent by the coordination Agent on its own App Server connection; the Node's
/// client reads, stops and steers turns.
impl Executor for CodexServer {
    fn start_or_resume<'a>(
        &'a self,
        node: &'a Arc<Runtime>,
        _: &'a str,
        work: &'a WorkSpec,
        _: Option<&'a str>,
        _: &'a Value,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let expected = node.providers().codex_target(
                node.manager()
                    .is_some_and(|manager| manager.provider.on_app_server()),
            );
            self.inner
                .hold(format!("work:{}", work.work_id), &expected)?;
            Ok(true)
        })
    }

    fn probe<'a>(&'a self, work: &'a WorkRecord) -> BoxFuture<'a, Result<Probe>> {
        Box::pin(async move {
            let thread = work.state.thread().context("the work has no thread")?;
            let turn = work.state.handle().map(|handle| handle.turn_id.as_str());
            CodexServer::probe(self, thread, turn, &work.work_id)
                .await
                .map_err(|error| anyhow::anyhow!("App Server probe failed: {error}"))
        })
    }

    fn interrupt<'a>(&'a self, work: &'a WorkRecord) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let handle = work.state.handle().context("the work has no turn")?;
            CodexServer::interrupt(self, &handle.thread_id, &handle.turn_id)
                .await
                .map_err(|error| anyhow::anyhow!("interrupt was not confirmed: {error}"))
        })
    }

    fn send_turn<'a>(
        &'a self,
        into: &'a SteerTarget,
        message_id: &'a str,
        text: &'a str,
    ) -> BoxFuture<'a, SteerAck> {
        let message = json!({"message_id":message_id,"text":text});
        let text = format!(
            "The user sent another message in this conversation while you were replying. Take it into account and answer it in this same reply.\n\n{message}"
        );
        let params = json!({"threadId":into.thread_id,"expectedTurnId":into.turn_id,
            "clientUserMessageId":message_id,
            "input":[{"type":"text","text":text,"text_elements":[]}]});
        Box::pin(async move {
            match self.client.request("turn/steer", params).await {
                Ok(_) => SteerAck::Accepted,
                Err(error) if !error.uncertain => SteerAck::Refused(error.message),
                Err(_) => SteerAck::Unknown,
            }
        })
    }

    fn read_back<'a>(
        &'a self,
        into: &'a SteerTarget,
        message_id: &'a str,
    ) -> BoxFuture<'a, Option<SteerTrace>> {
        Box::pin(async move {
            let reply = self
                .client
                .request(
                    "thread/read",
                    json!({"threadId":into.thread_id,"includeTurns":true}),
                )
                .await
                .ok()?;
            let turns = reply["thread"]["turns"].as_array()?;
            Some(steer_trace(turns, message_id, &into.turn_id))
        })
    }
}

/// The Node's own client observes, probes and interrupts; the coordination Agent sends turns on
/// its own connection to the same App Server. That App Server is the one already running on this
/// computer, whose sessions the owner shares, or one the Node starts itself.
pub struct CodexServer {
    client: CodexClient,
    inner: Arc<Inner>,
    /// Wakes sends waiting for the client to connect.
    connected: tokio::sync::Notify,
    /// Numbers the client's connections; the Node persists it, so it only grows.
    epoch: std::sync::atomic::AtomicU64,
}

/// How long a send waits for the App Server, as right after the Node starts, before it is refused.
const READY_WAIT: Duration = Duration::from_secs(20);
/// How long a started App Server may stay unconnected before the settings page asks the user.
const START_LIMIT: Duration = Duration::from_secs(60);

struct Inner {
    client: CodexClient,
    /// The socket the client connects to; the supervisor moves it.
    endpoint: tokio::sync::watch::Sender<Option<PathBuf>>,
    target: std::sync::Mutex<TargetState>,
    /// Wakes the supervisor for a new target.
    changed: tokio::sync::Notify,
    /// Where an App Server the Node starts listens.
    spawn_socket: PathBuf,
    /// A link to the socket in use. The coordination Agent's tools keep this one path while the
    /// App Server moves.
    agent_socket: PathBuf,
    pidfile: PathBuf,
    link: std::sync::Mutex<Link>,
    group: std::sync::Mutex<Option<i32>>,
    stopping: std::sync::atomic::AtomicBool,
}

struct TargetState {
    current: CodexTarget,
    applied: Option<CodexTarget>,
    pending: Option<CodexTarget>,
    leases: BTreeSet<String>,
}

/// An unknown turn keeps its launch until read-back proves it ended.
pub struct CodexLease {
    inner: Arc<Inner>,
    id: String,
    retained: bool,
    target: CodexTarget,
}

impl CodexLease {
    pub(crate) fn target(&self) -> &CodexTarget {
        &self.target
    }

    pub(crate) fn client_id(&self) -> &str {
        self.id.strip_prefix("manager:").expect("manager lease")
    }

    pub(crate) fn bind_turn(&mut self, client_id: &str) -> Result<()> {
        ensure!(
            !self.retained,
            "read back the unknown Codex turn before reusing its launch"
        );
        let id = format!("manager:{client_id}");
        if id != self.id {
            let mut target = self.inner.target.lock().expect("target lock");
            ensure!(
                !target.leases.contains(&id),
                "Codex turn already holds its launch"
            );
            ensure!(target.leases.remove(&self.id), "Codex launch lease ended");
            target.leases.insert(id.clone());
            self.id = id;
        }
        Ok(())
    }

    pub(crate) fn retain_until_read_back(&mut self) {
        self.retained = true;
    }
}

impl Drop for CodexLease {
    fn drop(&mut self) {
        if !self.retained {
            self.inner.release(&self.id);
        }
    }
}

/// The App Server as the settings page shows it.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Link {
    Off,
    /// Looking for the system App Server, or starting the Node's own.
    Starting,
    Connected {
        socket: PathBuf,
        user_agent: String,
    },
    /// No live App Server at any of these sockets.
    NotFound {
        candidates: Vec<PathBuf>,
    },
    /// A socket answered but the `initialize` handshake did not complete.
    HandshakeFailed {
        socket: PathBuf,
        reason: String,
    },
    /// The App Server the Node starts could not start or exited.
    LaunchFailed {
        reason: String,
    },
}

impl Link {
    pub(crate) fn sanitized(mut self) -> Self {
        let text = match &mut self {
            Self::LaunchFailed { reason } => {
                *reason = "Codex App Server could not start; check the local Node log".into();
                None
            }
            Self::HandshakeFailed { reason, .. } => {
                *reason = "Codex App Server handshake failed; check the local Node log".into();
                None
            }
            Self::Connected { user_agent, .. } => Some(user_agent),
            _ => None,
        };
        if let Some(text) = text {
            *text = crate::logging::scrub(text)
                .chars()
                .filter(|c| !c.is_control())
                .take(512)
                .collect();
        }
        self
    }
}

/// What the system App Server's control socket answered.
#[derive(Debug, PartialEq)]
pub(crate) enum Discovery {
    Live { socket: PathBuf, user_agent: String },
    NotFound { candidates: Vec<PathBuf> },
    HandshakeFailed { socket: PathBuf, reason: String },
}

/// Nothing listens there: no file, a socket file or link a stopped App Server left, or a path
/// too long for any socket.
fn stale(error: &str) -> bool {
    [
        "No such file",
        "Connection refused",
        "Not a directory",
        "SUN_LEN",
    ]
    .iter()
    .any(|cause| error.contains(cause))
}

/// The first candidate that completes an `initialize` handshake. Stale sockets are skipped.
pub(crate) async fn discover(candidates: &[PathBuf]) -> Discovery {
    let mut failed = None;
    for socket in candidates {
        match vibeface_bridge::codex::DirectSession::connect(socket).await {
            Ok(session) => {
                let user_agent = session.user_agent.clone();
                session.close().await;
                return Discovery::Live {
                    socket: socket.clone(),
                    user_agent,
                };
            }
            Err(error) if stale(&error.message) => {}
            Err(error) => {
                failed.get_or_insert(Discovery::HandshakeFailed {
                    socket: socket.clone(),
                    reason: error.message,
                });
            }
        }
    }
    failed.unwrap_or_else(|| Discovery::NotFound {
        candidates: candidates.to_vec(),
    })
}

fn signal_group(group: i32, signal: i32) -> bool {
    // SAFETY: kill(2) with a negative pid signals one process group and touches no memory.
    unsafe { libc::kill(-group, signal) == 0 }
}

fn group_alive(group: i32) -> bool {
    signal_group(group, 0)
}

/// Terminates a group: SIGTERM, then SIGKILL if it outlives the grace period.
/// ponytail: blocks up to 3s; it runs only at startup and shutdown.
fn kill_group(group: i32) {
    if !signal_group(group, libc::SIGTERM) {
        return;
    }
    for _ in 0..30 {
        if !group_alive(group) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    signal_group(group, libc::SIGKILL);
}

fn socket_dir(identity: &LocalIdentity, data_dir: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let directory =
        PathBuf::from("/tmp").join(format!("vibeface-{}", &identity.device_id()?[..32]));
    match std::fs::create_dir(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = std::fs::symlink_metadata(&directory)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == std::fs::metadata(data_dir)?.uid(),
        "private socket directory has unexpected ownership"
    );
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    Ok(directory)
}

impl Inner {
    fn hold(&self, id: String, expected: &CodexTarget) -> Result<()> {
        let mut target = self.target.lock().expect("target lock");
        ensure!(
            *expected != CodexTarget::Off,
            "Codex App Server is not configured"
        );
        ensure!(
            &target.current == expected
                && target.pending.is_none()
                && target
                    .applied
                    .as_ref()
                    .is_none_or(|applied| applied == expected),
            "Codex launch changed; wait for active turns and capture its configuration again"
        );
        ensure!(
            target.leases.insert(id),
            "Codex turn already holds its launch; read it back before retrying"
        );
        Ok(())
    }

    fn release(&self, id: &str) {
        let mut target = self.target.lock().expect("target lock");
        if target.leases.remove(id)
            && target.leases.is_empty()
            && let Some(pending) = target.pending.take()
        {
            target.current = pending;
            self.set_link(Link::Starting);
            self.changed.notify_one();
        }
    }

    fn stopping(&self) -> bool {
        self.stopping.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn set_link(&self, link: Link) {
        *self.link.lock().expect("link lock") = link;
    }

    /// Points the client and the Agent's link at `socket`. Pointing again at the same socket
    /// cuts the client's reconnect backoff short.
    fn point(&self, socket: Option<PathBuf>) {
        if let Some(socket) = &socket {
            let next = self.agent_socket.with_extension("next");
            let _ = std::fs::remove_file(&next);
            if let Err(error) = std::os::unix::fs::symlink(socket, &next)
                .and_then(|()| std::fs::rename(&next, &self.agent_socket))
            {
                tracing::warn!(event = "app_server.link_failed", %error, "the Agent's App Server link was not updated");
            }
        } else {
            let _ = std::fs::remove_file(&self.agent_socket);
        }
        self.endpoint.send_replace(socket);
    }

    /// Sleeps `seconds` unless the target changes first; `true` means it changed.
    async fn wait(&self, seconds: u64) -> bool {
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(seconds)) => false,
            () = self.changed.notified() => true,
        }
    }

    /// A group recorded by an earlier run is killed, not adopted: its requests belong to a
    /// dead epoch and its turns are settled by probes on the new App Server. Only a group whose
    /// command names the Node's own socket is touched, never the system App Server.
    fn kill_orphan(&self) {
        let Some(group) = std::fs::read_to_string(&self.pidfile)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
            .filter(|group| *group > 1)
        else {
            return;
        };
        let command = std::process::Command::new("/bin/ps")
            .args(["-o", "command=", "-p", &group.to_string()])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        if command.contains("app-server")
            && command.contains(&self.spawn_socket.display().to_string())
        {
            tracing::info!(
                event = "app_server.leftover_stopped",
                group,
                "stopping App Server left by an earlier run"
            );
            kill_group(group);
        }
        let _ = std::fs::remove_file(&self.pidfile);
    }

    fn spawn(&self, launch: &CodexLaunch) -> Result<tokio::process::Child> {
        if self.spawn_socket.exists() {
            use std::os::unix::fs::FileTypeExt;
            ensure!(
                std::fs::symlink_metadata(&self.spawn_socket)?
                    .file_type()
                    .is_socket(),
                "private Codex socket path is occupied by another file"
            );
            std::fs::remove_file(&self.spawn_socket)?;
        }
        let mut command = std::process::Command::new(&launch.bin);
        command
            .envs(&launch.env)
            .args(["app-server", "--listen"])
            .arg(format!("unix://{}", self.spawn_socket.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        let mut child = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(crate::logging::forward_lines(
                stderr,
                usize::MAX >> 1,
                |line| {
                    let thread = crate::logging::thread_hint(line);
                    crate::logging::child_line(
                        crate::logging::Source::Codex,
                        "codex.app_server.stderr",
                        line,
                        thread.as_deref(),
                    )
                },
            ));
        }
        let group = child.id().context("App Server exited at once")? as i32;
        config::private_write(&self.pidfile, group.to_string().as_bytes())?;
        *self.group.lock().expect("group lock") = Some(group);
        Ok(child)
    }

    /// Runs the Node's own App Server once. `true` when a new target ended it; its turns are
    /// settled by probes on the next App Server, as after a crash.
    async fn run_spawned(&self, launch: &CodexLaunch) -> bool {
        self.point(Some(self.spawn_socket.clone()));
        let mut child = match self.spawn(launch) {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(event = "app_server.start_failed", %error, "App Server could not start");
                self.set_link(Link::LaunchFailed {
                    reason: format!("App Server could not start: {error}"),
                });
                return false;
            }
        };
        self.target.lock().expect("target lock").applied = Some(CodexTarget::Spawn(launch.clone()));
        self.set_link(Link::Starting);
        let limit = tokio::time::sleep(START_LIMIT);
        tokio::pin!(limit);
        let mut late = false;
        let status = loop {
            tokio::select! {
                status = child.wait() => break Some(status),
                () = self.changed.notified() => break None,
                () = &mut limit, if !late => {
                    late = true;
                    self.set_link(Link::HandshakeFailed {
                        socket: self.spawn_socket.clone(),
                        reason: format!("Codex 启动 {} 秒后仍连不上", START_LIMIT.as_secs()),
                    });
                }
            }
        };
        let group = self.group.lock().expect("group lock").take();
        let replaced = match status {
            Some(status) => {
                if let Some(group) = group {
                    // Tools the App Server started share its group and die with it.
                    signal_group(group, libc::SIGKILL);
                }
                tracing::warn!(event = "app_server.exited", ?status, "App Server exited");
                if !self.stopping() {
                    self.set_link(Link::LaunchFailed {
                        reason: match status {
                            Ok(status) => format!("App Server exited: {status}"),
                            Err(error) => format!("App Server exited: {error}"),
                        },
                    });
                }
                false
            }
            None => {
                if let Some(group) = group {
                    signal_group(group, libc::SIGTERM);
                    let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
                    signal_group(group, libc::SIGKILL);
                }
                let _ = child.wait().await;
                tracing::info!(
                    event = "app_server.restarted",
                    "App Server restarts with new provider settings"
                );
                true
            }
        };
        let _ = std::fs::remove_file(&self.pidfile);
        self.target.lock().expect("target lock").applied = None;
        replaced
    }

    async fn use_system(&self, socket: &Path) -> bool {
        let socket = socket.to_owned();
        if !socket.exists() {
            self.set_link(Link::NotFound {
                candidates: vec![socket.clone()],
            });
        }
        self.point(Some(socket.clone()));
        self.target.lock().expect("target lock").applied =
            Some(CodexTarget::System(socket.clone()));
        let mut away = 0;
        loop {
            if self.wait(1).await {
                return true;
            }
            let connection = self.client.status().await["connection"].clone();
            if connection["state"] == "connected" {
                away = 0;
                continue;
            }
            away += 1;
            if away < 5 {
                continue;
            }
            match connection["error"].as_str() {
                Some(reason) if !stale(reason) => self.set_link(Link::HandshakeFailed {
                    socket,
                    reason: reason.into(),
                }),
                _ => self.set_link(Link::NotFound {
                    candidates: vec![socket],
                }),
            }
            return false;
        }
    }

    /// Follows the target until the Node stops, retrying with backoff.
    async fn supervise(self: Arc<Self>) {
        let mut backoff = 1;
        while !self.stopping() {
            let target = self.target.lock().expect("target lock").current.clone();
            let started = Instant::now();
            let replaced = match &target {
                CodexTarget::Off => {
                    self.point(None);
                    self.set_link(Link::Off);
                    self.changed.notified().await;
                    true
                }
                CodexTarget::Spawn(launch) => self.run_spawned(launch).await,
                CodexTarget::System(socket) => self.use_system(socket).await,
            };
            if replaced || started.elapsed() > Duration::from_secs(60) {
                backoff = 1;
            }
            if replaced {
                continue;
            }
            if self.wait(backoff).await {
                backoff = 1;
            } else {
                backoff = (backoff * 2).min(30);
            }
        }
    }
}

impl CodexServer {
    pub(super) fn open(
        target: CodexTarget,
        identity: &LocalIdentity,
        data_dir: &Path,
        events: mpsc::UnboundedSender<ExecEvent>,
    ) -> Result<Self> {
        let directory = socket_dir(identity, data_dir)?;
        let (endpoint, receiver) = tokio::sync::watch::channel(None);
        let client = CodexClient::start(receiver, events);
        let named = match &target {
            CodexTarget::System(socket) => Some(socket.clone()),
            _ => None,
        };
        let applied = named.as_ref().map(|_| target.clone());
        let inner = Inner {
            client: client.clone(),
            endpoint,
            target: std::sync::Mutex::new(TargetState {
                current: target,
                applied,
                pending: None,
                leases: BTreeSet::new(),
            }),
            changed: tokio::sync::Notify::new(),
            spawn_socket: directory.join("codex.sock"),
            agent_socket: directory.join("app-server.sock"),
            pidfile: data_dir.join("codex-app-server.pid"),
            link: std::sync::Mutex::new(Link::Starting),
            group: std::sync::Mutex::new(None),
            stopping: std::sync::atomic::AtomicBool::new(false),
        };
        inner.kill_orphan();
        inner.point(named);
        Ok(Self {
            client,
            inner: Arc::new(inner),
            connected: tokio::sync::Notify::new(),
            epoch: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub(super) fn start(&self) {
        tokio::spawn(self.inner.clone().supervise());
    }

    /// A new connection, numbered `epoch`; requests and turns of earlier ones are read again.
    pub(super) fn connected(&self, epoch: u64) {
        self.epoch.store(epoch, std::sync::atomic::Ordering::SeqCst);
        self.connected.notify_waiters();
    }

    /// Whether the client is connected, waiting up to `READY_WAIT` while the supervisor and the
    /// client's backoff bring it up. With Codex turned off it does not wait.
    pub(crate) async fn ready(&self) -> bool {
        if self.inner.target.lock().expect("target lock").current == CodexTarget::Off {
            return false;
        }
        let up = async {
            loop {
                let connected = self.connected.notified();
                if self.client.status().await["connection"]["state"] == "connected" {
                    return;
                }
                connected.await;
            }
        };
        tokio::time::timeout(READY_WAIT, up).await.is_ok()
    }

    /// The App Server the current target uses, as `CodexTarget::server` names it.
    pub(crate) fn server(&self) -> String {
        self.inner
            .target
            .lock()
            .expect("target lock")
            .current
            .server()
    }

    /// The socket the coordination Agent connects to; it follows the App Server in use.
    pub(crate) fn socket(&self) -> PathBuf {
        self.inner.agent_socket.clone()
    }

    /// Switches after active turns end. The system App Server is always left running.
    pub(crate) fn retarget(&self, target: CodexTarget) {
        let mut state = self.inner.target.lock().expect("target lock");
        if state.current == target {
            state.pending = None;
            return;
        }
        if !state.leases.is_empty() {
            state.pending = Some(target);
            return;
        }
        state.current = target;
        state.pending = None;
        self.inner.set_link(Link::Starting);
        self.inner.changed.notify_one();
    }

    pub(crate) fn lease(&self, id: String, expected: &CodexTarget) -> Result<CodexLease> {
        self.inner.hold(id.clone(), expected)?;
        Ok(CodexLease {
            inner: self.inner.clone(),
            id,
            retained: false,
            target: expected.clone(),
        })
    }

    pub(crate) fn release_work(&self, work_id: &str) {
        self.inner.release(&format!("work:{work_id}"));
    }

    pub(crate) fn release_manager(&self, attempt_id: &str) {
        self.inner.release(&format!("manager:{attempt_id}"));
    }

    pub(crate) fn configuration_pending(&self, expected: &CodexTarget) -> bool {
        let target = self.inner.target.lock().expect("target lock");
        target.current != *expected
            || target.pending.is_some()
            || target
                .applied
                .as_ref()
                .is_some_and(|applied| applied != expected)
    }

    pub(crate) async fn link(&self) -> Link {
        let status = self.client.status().await;
        if status["connection"]["state"] == "connected"
            && let Some(socket) = self.inner.endpoint.borrow().clone()
        {
            return Link::Connected {
                socket,
                user_agent: status["connection"]["user_agent"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
            };
        }
        self.inner.link.lock().expect("link lock").clone()
    }

    /// A fresh handshake with the App Server the current target uses, beside the client's own
    /// connection. A live one the client is backing off from is retried now.
    pub(crate) async fn test(&self) -> Link {
        let target = self
            .inner
            .target
            .lock()
            .expect("target lock")
            .current
            .clone();
        let candidates = match &target {
            CodexTarget::Off => return Link::Off,
            CodexTarget::Spawn(_) => vec![self.inner.spawn_socket.clone()],
            CodexTarget::System(socket) => vec![socket.clone()],
        };
        let system = matches!(target, CodexTarget::System(_));
        match discover(&candidates).await {
            Discovery::Live { socket, user_agent } => {
                if self.inner.endpoint.borrow().as_ref() == Some(&socket) {
                    self.inner.point(Some(socket.clone()));
                } else if system {
                    self.inner.changed.notify_one();
                }
                Link::Connected { socket, user_agent }
            }
            Discovery::NotFound { candidates } => Link::NotFound { candidates },
            Discovery::HandshakeFailed { socket, reason } => {
                Link::HandshakeFailed { socket, reason }
            }
        }
    }

    pub(crate) async fn provider_observation(&self) -> (Authentication, ModelCatalog) {
        let account = tokio::time::timeout(
            Duration::from_secs(5),
            self.client
                .request("account/read", json!({"refreshToken":false})),
        )
        .await;
        let authentication = match account {
            Ok(Ok(account)) => account_authentication(&account),
            _ => Authentication::Unverified,
        };
        let catalog = tokio::time::timeout(Duration::from_secs(5), self.models())
            .await
            .unwrap_or_default();
        (authentication, catalog)
    }

    async fn models(&self) -> ModelCatalog {
        if self.client.status().await["connection"]["state"] != "connected" {
            return ModelCatalog::default();
        }
        let mut models = vec![];
        let mut params = json!({});
        let mut coverage = CatalogCoverage::Partial;
        let mut hidden = false;
        for _ in 0..MODEL_PAGES {
            let Ok(page) = self.client.request("model/list", params).await else {
                return ModelCatalog::default();
            };
            let Some(data) = page["data"].as_array() else {
                return ModelCatalog::default();
            };
            hidden |= data.iter().any(|model| model["hidden"] == true);
            models.extend(model_choices(&page));
            match page.get("nextCursor") {
                Some(Value::String(cursor)) if !cursor.is_empty() => {
                    params = json!({"cursor":cursor})
                }
                None | Some(Value::Null) | Some(Value::String(_)) => {
                    if !hidden {
                        coverage = CatalogCoverage::Complete;
                    }
                    break;
                }
                _ => return ModelCatalog::default(),
            }
        }
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models.dedup_by(|a, b| a.id == b.id);
        ModelCatalog::Available { models, coverage }
    }

    /// Stops the App Server the Node started; called on SIGTERM before the Node exits. The
    /// system App Server is never stopped.
    pub(crate) async fn stop(&self) {
        self.inner
            .stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let group = self.inner.group.lock().expect("group lock").take();
        if let Some(group) = group {
            kill_group(group);
        }
        let _ = std::fs::remove_file(&self.inner.pidfile);
    }
}

const MODEL_PAGES: usize = 5;

fn account_authentication(account: &Value) -> Authentication {
    match (
        account["requiresOpenaiAuth"].as_bool(),
        account.get("account"),
    ) {
        (Some(true), Some(Value::Null)) => Authentication::SignInRequired,
        (Some(true), Some(value)) if value["type"] == "chatgpt" => Authentication::Authenticated,
        _ => Authentication::Unverified,
    }
}

/// One `model/list` page as choices, without the models the App Server hides.
fn model_choices(page: &Value) -> Vec<ModelChoice> {
    page["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|model| model["hidden"] != true)
        .filter_map(|model| {
            let id = model["model"]
                .as_str()
                .or(model["id"].as_str())
                .filter(|id| safe_model_text(id))?;
            let options: Vec<String> = model["supportedReasoningEfforts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|effort| effort["reasoningEffort"].as_str())
                .filter(|effort| {
                    Provider::Codex
                        .validate_reasoning_effort(Some(effort))
                        .is_ok()
                })
                .map(String::from)
                .collect();
            let effort = if !options.is_empty() {
                let default = model["defaultReasoningEffort"]
                    .as_str()
                    .filter(|default| options.iter().any(|option| option == default))
                    .map(String::from);
                EffortOptions::Selectable { options, default }
            } else if model["reasoningEffortConfigurable"] == false {
                EffortOptions::ProviderControlled
            } else {
                EffortOptions::Unknown
            };
            Some(ModelChoice {
                id: id.into(),
                name: model["displayName"]
                    .as_str()
                    .filter(|name| safe_model_text(name))
                    .unwrap_or(id)
                    .into(),
                default: model["isDefault"] == true,
                effort,
            })
        })
        .collect()
}

fn safe_model_text(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 256
        && !value.chars().any(char::is_control)
        && !crate::model::looks_secret(value)
}

impl Runtime {
    /// The web's view of Codex on this device, or on the device `node` names. The web never
    /// calls App Server methods.
    pub(super) async fn codex_state(&self, network: &str, input: &Value) -> Result<Value> {
        if let Some(node) = input["node"].as_str()
            && node != self.identity.device_id()?
        {
            return self
                .peer_request(network, node, json!({"type":"codex_state","human":true}))
                .await;
        }
        Ok(self.codex.status().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_refusal_does_not_prove_an_unknown_turn_ended() {
        let error = |message: &str, answered| CodexError {
            uncertain: false,
            answered,
            message: message.into(),
        };
        assert!(missing_thread(&error("thread not found: fixture", true)));
        assert!(!missing_thread(&error("authentication required", true)));
        assert!(!missing_thread(&error("server request rejected", true)));
        assert!(!missing_thread(&error("thread not found", false)));
        let wrapped =
            r#"Codex rejected request: {"code":-32602,"message":"thread not found: fixture"}"#;
        assert!(missing_thread(&error(wrapped, true)));
        assert!(!missing_thread(&error(wrapped, false)));
        assert!(!missing_thread(&error(
            r#"Codex rejected request: {"message":"authentication failed while reading thread not found"}"#,
            true
        )));
        assert!(!missing_thread(&error(
            "Codex rejected request: not-json",
            true
        )));
    }

    #[test]
    fn model_choices_take_the_name_thread_start_uses_and_skip_hidden_ones() {
        let page = json!({"data":[
            {"id":"a","model":"gpt-a","displayName":"GPT A","isDefault":true},
            {"id":"b","model":"gpt-b","displayName":"","hidden":false},
            {"id":"c","model":"gpt-c","displayName":"GPT C","hidden":true},
            {"displayName":"no id"}
        ],"nextCursor":null});
        assert_eq!(
            model_choices(&page),
            vec![
                ModelChoice {
                    id: "gpt-a".into(),
                    name: "GPT A".into(),
                    default: true,
                    effort: EffortOptions::Unknown,
                },
                ModelChoice {
                    id: "gpt-b".into(),
                    name: "gpt-b".into(),
                    default: false,
                    effort: EffortOptions::Unknown,
                },
            ]
        );
        assert!(model_choices(&json!({})).is_empty());
    }

    #[test]
    fn codex_observations_project_authentication_and_per_model_effort() {
        assert_eq!(
            account_authentication(&json!({"requiresOpenaiAuth":true,"account":null})),
            Authentication::SignInRequired
        );
        assert_eq!(
            account_authentication(
                &json!({"requiresOpenaiAuth":true,"account":{"type":"chatgpt","email":"private@example.invalid"}})
            ),
            Authentication::Authenticated
        );
        assert_eq!(
            account_authentication(
                &json!({"requiresOpenaiAuth":false,"account":{"type":"apiKey"}})
            ),
            Authentication::Unverified
        );
        assert_eq!(
            account_authentication(&json!({})),
            Authentication::Unverified
        );
        let choices = model_choices(
            &json!({"data":[{"model":"model","supportedReasoningEfforts":[{"reasoningEffort":"low"},{"reasoningEffort":"high"},{"reasoningEffort":"untrusted"}],"defaultReasoningEffort":"high"}]}),
        );
        assert_eq!(
            choices[0].effort,
            EffortOptions::Selectable {
                options: vec!["low".into(), "high".into()],
                default: Some("high".into())
            }
        );
        assert!(model_choices(&json!({"data":[{"model":"bad\nmodel"}]})).is_empty());
    }

    #[test]
    fn app_server_reasons_are_scrubbed_bounded_and_control_free() {
        let link = Link::LaunchFailed {
            reason: format!("Authorization: Bearer fixture-secret\n{}", "x".repeat(800)),
        }
        .sanitized();
        let Link::LaunchFailed { reason } = link else {
            panic!("wrong link state")
        };
        assert!(!reason.contains("fixture-secret"));
        assert!(!reason.chars().any(char::is_control));
        assert!(reason.chars().count() <= 512);
    }

    #[test]
    fn a_steered_message_is_traced_only_from_what_its_thread_shows() {
        let turn = |id: &str, status: &str, sent: bool| {
            let items = if sent {
                json!([{"type":"userMessage","clientId":"m"}])
            } else {
                json!([])
            };
            json!({"id":id,"status":status,"items":items})
        };
        let trace = |turns: &[Value]| steer_trace(turns, "m", "r");
        assert_eq!(trace(&[turn("r", "inProgress", true)]), SteerTrace::Pending);
        assert_eq!(
            trace(&[turn("r", "inProgress", false)]),
            SteerTrace::Pending
        );
        assert_eq!(
            trace(&[turn("r", "completed", true)]),
            SteerTrace::Ended(SteerOutcome::Answered)
        );
        assert_eq!(
            trace(&[turn("r", "interrupted", true)]),
            SteerTrace::Ended(SteerOutcome::Unfinished)
        );
        assert_eq!(
            trace(&[turn("r", "completed", false), turn("x", "completed", true)]),
            SteerTrace::Ended(SteerOutcome::Unfinished)
        );
        assert_eq!(
            trace(&[turn("r", "interrupted", false)]),
            SteerTrace::NotArrived
        );
        assert_eq!(trace(&[]), SteerTrace::TargetUnseen);
        assert_eq!(
            trace(&[turn("x", "completed", false)]),
            SteerTrace::TargetUnseen
        );
    }
    use crate::providers::{AppServerMode, Local, Provider, ProviderEnv};
    use futures_util::{SinkExt as _, StreamExt as _};
    use std::os::unix::fs::PermissionsExt;

    async fn lines(path: &Path, count: usize) -> Vec<String> {
        for _ in 0..100 {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            let lines: Vec<String> = text.lines().map(str::to_owned).collect();
            if lines.len() >= count {
                return lines;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("App Server fixture did not start {count} times");
    }

    async fn ended(group: i32) -> bool {
        for _ in 0..100 {
            if !group_alive(group) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    fn artifacts(name: &str) -> Result<PathBuf> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("{name}-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        Ok(dir)
    }

    /// Unix socket paths stay short, so fixture sockets live under `/tmp`.
    fn sockets() -> Result<PathBuf> {
        let dir = PathBuf::from(format!(
            "/tmp/vf-app-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..10]
        ));
        config::private_dir(&dir)?;
        Ok(dir)
    }

    fn recording_codex(dir: &Path) -> Result<(PathBuf, PathBuf)> {
        let (program, out) = (dir.join("fake-codex"), dir.join("ran"));
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\necho \"$@\" >> '{}'\nexec sleep 60\n",
                out.display()
            ),
        )?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
        Ok((program, out))
    }

    fn system_app_server(
        socket: &Path,
        user_agent: &'static str,
        turn: Option<Arc<std::sync::Mutex<Value>>>,
    ) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::UnixListener::bind(socket).expect("fixture socket");
        tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((stream, _)) = listener.accept().await {
                let turn = turn.clone();
                connections.spawn(async move {
                    let Ok(mut server) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    while let Some(Ok(message)) = server.next().await {
                        let Ok(request) = serde_json::from_slice::<Value>(&message.into_data())
                        else {
                            continue;
                        };
                        if request["method"] == "initialize" {
                            let reply =
                                json!({"id":request["id"],"result":{"userAgent":user_agent}});
                            let _ = server.send(reply.to_string().into()).await;
                        } else if request["method"] == "thread/read" && let Some(turn) = &turn {
                            let turn = turn.lock().unwrap().clone();
                            let reply = if turn.is_null() {
                                json!({"id":request["id"],"error":{"code":-32000,"message":"fixture unavailable"}})
                            } else {
                                json!({"id":request["id"],"result":{"thread":{"turns":[turn]}}})
                            };
                            let _ = server.send(reply.to_string().into()).await;
                        }
                    }
                });
            }
        })
    }

    fn stale_socket(path: &Path) -> Result<()> {
        drop(std::os::unix::net::UnixListener::bind(path)?);
        Ok(())
    }

    #[tokio::test]
    async fn manual_manager_confirmation_releases_codex_hold_but_refuses_a_live_turn() -> Result<()>
    {
        for completed in [true, false] {
            let (node, admin, data) = super::super::triage::tests::node().await?;
            let sockets = sockets()?;
            let socket = sockets.join("manager.sock");
            let turn = Arc::new(std::sync::Mutex::new(
                json!({"id":"turn","status":"inProgress","items":[]}),
            ));
            let fixture = system_app_server(&socket, "codex/manual-check", Some(turn.clone()));
            let target = CodexTarget::System(socket);
            node.codex.retarget(target.clone());
            node.codex.start();
            link_becomes(&node.codex, |link| matches!(link, Link::Connected { .. })).await;
            let (access, check) = super::super::manager_control::tests::pending_manager(
                &node, &admin, "system", "thread",
            )
            .await?;
            let mut lease = node
                .codex
                .lease(format!("manager:{}", check.attempt_id), &target)?;
            lease.retain_until_read_back();
            drop(lease);
            node.codex.retarget(CodexTarget::Off);
            let input = json!({"conversation_id":"c","check_id":check.check_id});
            let mut agent = access.clone();
            agent.human = false;
            assert!(node.resolve_check(&agent, &input).await.is_err());
            assert!(node.resolve_check(&access, &input).await.is_err());
            assert_eq!(node.store.lock().await.pending_checks()?.len(), 1);
            assert!(
                node.codex
                    .inner
                    .target
                    .lock()
                    .unwrap()
                    .leases
                    .contains(&format!("manager:{}", check.attempt_id))
            );
            *turn.lock().unwrap() = if completed {
                json!({"id":"turn","status":"completed","items":[]})
            } else {
                Value::Null
            };
            assert_eq!(node.resolve_check(&access, &input).await?["resolved"], true);
            assert!(node.store.lock().await.pending_checks()?.is_empty());
            assert!(node.codex.inner.target.lock().unwrap().leases.is_empty());
            node.codex.release_manager(&check.attempt_id);
            node.shutdown().await;
            fixture.abort();
            drop(node);
            std::fs::remove_dir_all(data)?;
            std::fs::remove_dir_all(sockets)?;
        }
        Ok(())
    }

    async fn link_becomes(server: &CodexServer, wanted: impl Fn(&Link) -> bool) -> Link {
        for _ in 0..200 {
            let link = server.link().await;
            if wanted(&link) {
                return link;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("App Server link stayed {:?}", server.link().await);
    }

    #[tokio::test]
    async fn the_app_server_runs_with_only_codex_variables_and_restarts_on_a_new_launch()
    -> Result<()> {
        let dir = artifacts("codex-launch")?;
        let out = dir.join("starts");
        let fixture = dir.join("fake-codex");
        std::fs::write(
            &fixture,
            "#!/bin/sh\necho \"$$ ${CPA_API_KEY:-none} ${FIXTURE_CURSOR_ONLY:-none}\" >> \"$FIXTURE_OUT\"\nexec sleep 60\n",
        )?;
        std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700))?;
        let mut env = ProviderEnv::default();
        env.apply(
            Provider::Codex,
            BTreeMap::from([
                ("CPA_API_KEY".into(), Some("fixture-codex-key".into())),
                ("FIXTURE_OUT".into(), Some(out.display().to_string())),
            ]),
        )?;
        env.apply(
            Provider::Cursor,
            BTreeMap::from([("FIXTURE_CURSOR_ONLY".into(), Some("leaked".into()))]),
        )?;
        let mut local = Local {
            switches: Default::default(),
            bins: BTreeMap::from([(Provider::Codex, fixture.clone())]),
            env,
        };
        local.switches.execution.insert(Provider::Codex);
        local.switches.codex_app_server.mode = AppServerMode::Spawn;
        let identity = LocalIdentity::generate();
        let (events, _events) = mpsc::unbounded_channel();
        let server = CodexServer::open(local.codex_target(false), &identity, &dir, events)?;
        server.start();
        let first = lines(&out, 1).await;
        let (pid, rest) = first[0].split_once(' ').unwrap();
        assert_eq!(
            rest, "fixture-codex-key none",
            "only Codex variables reach it"
        );

        let expected = local.codex_target(false);
        let mut bot = server.lease("manager:capture-fixture".into(), &expected)?;
        server.inner.hold("work:fixture-work".into(), &expected)?;

        local.env.apply(
            Provider::Codex,
            BTreeMap::from([("CPA_API_KEY".into(), Some("fixture-replaced-key".into()))]),
        )?;
        server.retarget(local.codex_target(false));
        bot.bind_turn("fixture-bot")?;
        assert!(bot.target() == &expected);
        assert_eq!(bot.client_id(), "fixture-bot");
        assert!(group_alive(pid.parse()?));
        assert!(server.inner.target.lock().unwrap().pending.is_some());
        assert!(
            server
                .lease("manager:new-generation".into(), &local.codex_target(false))
                .is_err()
        );
        bot.retain_until_read_back();
        assert!(bot.bind_turn("another-turn").is_err());
        drop(bot);
        server.inner.release("manager:fixture-bot");
        assert!(
            group_alive(pid.parse()?),
            "the dispatched work still uses the old launch"
        );
        assert!(server.inner.target.lock().unwrap().pending.is_some());
        server.release_work("fixture-work");
        let second = lines(&out, 2).await;
        assert!(second[1].ends_with(" fixture-replaced-key none"));
        assert!(ended(pid.parse()?).await, "the old App Server ends");

        local.switches.execution.clear();
        server.retarget(local.codex_target(false));
        let second_group: i32 = second[1].split_once(' ').unwrap().0.parse()?;
        assert!(
            ended(second_group).await,
            "turning execution off stops the App Server"
        );
        link_becomes(&server, |link| *link == Link::Off).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            lines(&out, 2).await.len(),
            2,
            "nothing starts while it is off"
        );
        server.stop().await;
        std::fs::remove_dir_all(server.socket().parent().unwrap())?;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn discovery_skips_stale_sockets_and_names_a_failed_handshake() -> Result<()> {
        let dir = sockets()?;
        let (missing, stale, live, mute) = (
            dir.join("missing.sock"),
            dir.join("stale.sock"),
            dir.join("live.sock"),
            dir.join("mute.sock"),
        );
        stale_socket(&stale)?;
        let fixture = system_app_server(&live, "codex/fixture", None);
        let found = discover(&[missing.clone(), stale.clone(), live.clone()]).await;
        assert_eq!(
            found,
            Discovery::Live {
                socket: live.clone(),
                user_agent: "codex/fixture".into()
            },
            "stale and missing sockets are skipped"
        );
        assert_eq!(
            discover(&[missing.clone(), stale.clone()]).await,
            Discovery::NotFound {
                candidates: vec![missing.clone(), stale.clone()]
            }
        );
        let listener = tokio::net::UnixListener::bind(&mute)?;
        let closer = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        assert!(matches!(
            discover(&[stale, mute.clone()]).await,
            Discovery::HandshakeFailed { socket, .. } if socket == mute
        ));
        fixture.abort();
        closer.abort();
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn system_mode_never_starts_or_stops_the_app_server_and_follows_its_restart() -> Result<()>
    {
        let dir = artifacts("codex-system")?;
        let (program, ran) = recording_codex(&dir)?;
        let sockets = sockets()?;
        let control = sockets.join("control.sock");
        let (stale, first, second) = (
            sockets.join("stale.sock"),
            sockets.join("first.sock"),
            sockets.join("second.sock"),
        );
        stale_socket(&stale)?;
        let fixture = system_app_server(&first, "codex/first", None);
        std::os::unix::fs::symlink(&first, &control)?;
        let local = Local {
            switches: crate::providers::Providers {
                execution: [Provider::Codex].into(),
                execution_models: BTreeMap::new(),
                codex_app_server: crate::providers::AppServer {
                    mode: AppServerMode::System,
                    socket: Some(control.clone()),
                },
            },
            bins: BTreeMap::from([(Provider::Codex, program)]),
            env: ProviderEnv::default(),
        };
        assert_eq!(
            local.switches.codex_app_server.mode,
            AppServerMode::System,
            "an explicit system App Server remains usable"
        );
        let identity = LocalIdentity::generate();
        let (events, _events) = mpsc::unbounded_channel();
        let server = CodexServer::open(local.codex_target(false), &identity, &dir, events)?;
        server.start();
        let connected = link_becomes(&server, |link| matches!(link, Link::Connected { .. })).await;
        assert_eq!(
            connected,
            Link::Connected {
                socket: control.clone(),
                user_agent: "codex/first".into()
            }
        );
        assert_eq!(
            std::fs::read_link(server.socket())?,
            control,
            "the Agent's link names the system App Server"
        );

        fixture.abort();
        let _ = fixture.await;
        std::fs::remove_file(&first)?;
        link_becomes(&server, |link| !matches!(link, Link::Connected { .. })).await;
        let restarted = system_app_server(&second, "codex/second", None);
        std::fs::remove_file(&control)?;
        std::os::unix::fs::symlink(&second, &control)?;
        link_becomes(&server, |link| {
            matches!(link, Link::Connected { user_agent, .. } if user_agent == "codex/second")
        })
        .await;
        assert!(matches!(server.test().await, Link::Connected { socket, .. } if socket == control));

        server.retarget(CodexTarget::Off);
        link_becomes(&server, |link| *link == Link::Off).await;
        server.stop().await;
        std::fs::remove_dir_all(server.socket().parent().unwrap())?;
        assert!(
            vibeface_bridge::codex::DirectSession::connect(&control)
                .await
                .is_ok(),
            "the system App Server outlives the Node"
        );
        assert!(!ran.exists(), "the Node never started an App Server");
        assert!(!dir.join("codex-app-server.pid").exists());
        restarted.abort();
        std::fs::remove_dir_all(sockets)?;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn without_a_system_app_server_the_link_says_not_found() -> Result<()> {
        let dir = artifacts("codex-none")?;
        let sockets = sockets()?;
        let candidate = sockets.join("control.sock");
        let local = Local {
            switches: crate::providers::Providers {
                execution: [Provider::Codex].into(),
                execution_models: BTreeMap::new(),
                codex_app_server: crate::providers::AppServer {
                    mode: AppServerMode::System,
                    socket: Some(candidate.clone()),
                },
            },
            bins: BTreeMap::new(),
            env: ProviderEnv::default(),
        };
        let identity = LocalIdentity::generate();
        let (events, _events) = mpsc::unbounded_channel();
        let server = CodexServer::open(local.codex_target(false), &identity, &dir, events)?;
        server.start();
        let link = link_becomes(&server, |link| matches!(link, Link::NotFound { .. })).await;
        assert_eq!(
            link,
            Link::NotFound {
                candidates: vec![candidate]
            }
        );
        server.stop().await;
        std::fs::remove_dir_all(server.socket().parent().unwrap())?;
        std::fs::remove_dir_all(sockets)?;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
