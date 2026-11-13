//! The coordination runtime's App Server: the process, its endpoint and the Node's own client.
//! A managed App Server runs in its own process tree recorded in a pidfile, so a crash of this
//! Node never leaves an unmanaged one behind: the next start ends the recorded tree, once the
//! record proves the tree is still that very App Server.
use super::executor::{Executor, SteerAck, SteerInput, SteerTrace};
use super::provider_adapter::safe_model_text;
use super::*;
use crate::providers::{
    Authentication, CatalogCoverage, CodexLaunch, CodexTarget, EffortOptions, ModelCatalog,
    ModelChoice,
};
use crate::scheduling::{SteerOutcome, SteerTarget};
use crate::work::{Probe, Signal, WorkRecord, WorkSpec};
use futures_util::future::BoxFuture;
use vibeface_bridge::{
    codex::{CodexClient, CodexError, DirectSession, EventSender},
    platform::{
        ipc::{Alias, Endpoint},
        privatefs, tree,
    },
    turn::{TurnStatus, turn_snapshot},
};
use vibeface_control::agent::ThreadAt;

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
    let Some(turn_id) = turn["id"].as_str() else {
        return SteerTrace::Pending;
    };
    match turn_snapshot(turn).status {
        Some(TurnStatus::InProgress) => SteerTrace::Received {
            turn_id: turn_id.into(),
        },
        Some(status) if status.ended() => SteerTrace::Ended {
            turn_id: turn_id.into(),
            outcome: if status == TurnStatus::Completed {
                SteerOutcome::Answered
            } else {
                SteerOutcome::Unfinished
            },
        },
        _ => SteerTrace::Received {
            turn_id: turn_id.into(),
        },
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
        let params = json!({"threadId":thread,"turnId":turn});
        match self.user_thread(thread) {
            Some(socket) => user_request(&socket, "turn/interrupt", params).await,
            None => self.client.request("turn/interrupt", params).await,
        }
        .map(|_| ())
    }

    /// The user's own Codex App Server while the Node runs its own: `codex_app_server.socket`
    /// in spawn mode. Only later lookups use it; a thread already found stays on its socket.
    pub(crate) fn set_user_socket(&self, socket: Option<PathBuf>) {
        self.user.lock().expect("user codex").socket = socket;
    }

    /// The user's own Codex App Server socket `thread` was found on.
    pub(crate) fn user_thread(&self, thread: &str) -> Option<PathBuf> {
        self.user
            .lock()
            .expect("user codex")
            .threads
            .get(thread)
            .cloned()
    }

    /// Reads `thread` on the App Server that has it: the Node's, else the user's own Codex,
    /// which is then remembered for it. Names the user's socket when it was read there.
    pub(crate) async fn read(
        &self,
        thread: &str,
        params: Value,
    ) -> Result<(Value, Option<PathBuf>), CodexError> {
        if let Some(socket) = self.user_thread(thread) {
            let reply = user_request(&socket, "thread/read", params).await?;
            return Ok((reply, Some(socket)));
        }
        let user = self.user.lock().expect("user codex").socket.clone();
        match (
            self.client.request("thread/read", params.clone()).await,
            user,
        ) {
            (Err(error), Some(socket)) if error.missing_thread() => {
                let reply = user_request(&socket, "thread/read", params).await?;
                self.user
                    .lock()
                    .expect("user codex")
                    .threads
                    .insert(thread.to_owned(), socket.clone());
                Ok((reply, Some(socket)))
            }
            (read, _) => read.map(|reply| (reply, None)),
        }
    }

    /// Which App Server has `thread`, for a send into it.
    pub(crate) async fn locate(&self, thread: &str) -> ThreadAt {
        match self.read(thread, json!({"threadId":thread})).await {
            Ok((_, None)) => ThreadAt::Node,
            Ok((_, Some(socket))) => ThreadAt::User { socket },
            Err(error) if error.missing_thread() => ThreadAt::Missing {
                // The system App Server the Node shares is the user's own Codex.
                user: self.user.lock().expect("user codex").socket.is_some()
                    || matches!(
                        self.inner.target.lock().expect("target lock").current,
                        CodexTarget::System(_)
                    ),
            },
            Err(error) => ThreadAt::Unknown {
                error: error.message,
            },
        }
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
            .read(thread, json!({"threadId":thread,"includeTurns":true}))
            .await
        {
            Ok((reply, _)) => reply,
            // Only the App Server saying it has no such thread makes the turn missing.
            Err(error) if error.missing_thread() => {
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
/// client reads, stops and steers turns, on the user's own Codex for a thread found there.
impl Executor for CodexServer {
    fn start_or_resume<'a>(
        &'a self,
        node: &'a Arc<Runtime>,
        _: &'a str,
        work: &'a WorkSpec,
        thread: Option<&'a str>,
        _: &'a Value,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            // A turn on the user's own Codex does not run on the Node's App Server, so it does
            // not hold that launch.
            if thread.is_some_and(|thread| self.user_thread(thread).is_some()) {
                return Ok(true);
            }
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
        input: &'a SteerInput,
    ) -> BoxFuture<'a, SteerAck> {
        let mut blocks =
            vec![json!({"type":"text","text":input.message(message_id),"text_elements":[]})];
        for file in &input.files {
            blocks.push(if file.is_image() {
                json!({"type":"image","url":file.data_url()})
            } else {
                json!({"type":"mention","name":file.attachment.name,"path":file.local_path})
            });
        }
        let params = json!({"threadId":into.thread_id,"expectedTurnId":into.turn_id,
            "clientUserMessageId":message_id,
            "input":blocks});
        Box::pin(async move {
            let sent = match self.user_thread(&into.thread_id) {
                Some(socket) => user_request(&socket, "turn/steer", params).await,
                None => self.client.request("turn/steer", params).await,
            };
            match sent {
                Ok(reply) => SteerAck::Accepted {
                    turn_id: reply["turnId"].as_str().map(str::to_owned),
                },
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
            let (reply, _) = self
                .read(
                    &into.thread_id,
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
    user: std::sync::Mutex<UserCodex>,
}

/// The user's own Codex App Server while the Node runs its own, and the threads found on it,
/// each with the socket it was found on. The Node sends into and reads those threads there and
/// never starts one there.
#[derive(Default)]
struct UserCodex {
    socket: Option<PathBuf>,
    threads: BTreeMap<String, PathBuf>,
}

/// How long the user's own Codex App Server gets to answer one request.
const USER_WAIT: Duration = Duration::from_secs(10);
/// How long the Node waits for a thread read that may ask its own App Server and then the
/// user's own Codex.
pub(crate) const READ_WAIT: Duration = Duration::from_secs(5 + USER_WAIT.as_secs());

/// One request on its own connection to the user's own Codex App Server; the Node keeps no
/// connection there. Its errors never name the socket: they reach the bot and the chat.
async fn user_request(socket: &Path, method: &str, params: Value) -> Result<Value, CodexError> {
    let request = async {
        let mut session = DirectSession::connect(socket).await?;
        let reply = session.request(method, params).await;
        session.close().await;
        reply
    };
    let unanswered = |message: &str| CodexError {
        uncertain: true,
        answered: false,
        message: format!("the user's own Codex App Server did not answer{message}"),
    };
    match tokio::time::timeout(USER_WAIT, request).await {
        Err(_) => Err(unanswered(" in time")),
        Ok(Err(error)) if !error.answered => {
            tracing::debug!(method, error = error.message, "user Codex request failed");
            Err(unanswered(""))
        }
        Ok(reply) => reply,
    }
}

/// How long a send waits for the App Server, as right after the Node starts, before it is refused.
const READY_WAIT: Duration = Duration::from_secs(20);
/// How long a started App Server may stay unconnected before the settings page asks the user.
const START_LIMIT: Duration = Duration::from_secs(60);
/// Names the managed App Server's process tree in this account's job object namespace.
const TREE_KEY: &str = "codex-app-server";

struct Inner {
    client: CodexClient,
    /// The socket the client connects to; the supervisor moves it.
    endpoint: tokio::sync::watch::Sender<Option<PathBuf>>,
    target: std::sync::Mutex<TargetState>,
    /// Wakes the supervisor for a new target.
    changed: tokio::sync::Notify,
    /// Where an App Server the Node starts listens.
    spawn_endpoint: Endpoint,
    /// An alias for the endpoint in use. The coordination Agent's tools keep this one path
    /// while the App Server moves.
    agent_alias: Alias,
    pidfile: PathBuf,
    link: std::sync::Mutex<Link>,
    tree: tokio::sync::Mutex<Option<tree::Tree>>,
    stopping: std::sync::atomic::AtomicBool,
}

struct TargetState {
    current: CodexTarget,
    applied: Option<CodexTarget>,
    pending: Option<CodexTarget>,
    configuration_reload_pending: bool,
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
        let session = tokio::time::timeout(
            Duration::from_secs(3),
            vibeface_bridge::codex::DirectSession::connect(socket),
        )
        .await
        .unwrap_or_else(|_| {
            Err(CodexError {
                uncertain: false,
                answered: false,
                message: "Codex App Server connection probe timed out".into(),
            })
        });
        match session {
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

fn socket_dir(identity: &LocalIdentity, data_dir: &Path) -> Result<PathBuf> {
    let directory = vibeface_bridge::platform::ipc::runtime_dir(&identity.device_id()?[..32])?;
    match privatefs::create_dir(&directory) {
        Ok(()) => {}
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) => {}
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(&directory)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "the private socket directory must not be a link"
    );
    ensure!(
        privatefs::same_owner(&directory, data_dir)?,
        "private socket directory has unexpected ownership"
    );
    privatefs::restrict_dir(&directory)?;
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
                && !target.configuration_reload_pending
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
            if !matches!(pending, CodexTarget::Spawn(_)) {
                target.configuration_reload_pending = false;
            }
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
        match socket
            .as_deref()
            .map(Endpoint::from_path)
            .transpose()
            .and_then(|endpoint| match endpoint {
                Some(endpoint) => self.agent_alias.point_at(&endpoint),
                None => {
                    self.agent_alias.clear();
                    Ok(())
                }
            }) {
            Ok(()) => {}
            Err(error) => {
                tracing::warn!(event = "app_server.link_failed", %error, "the Agent's App Server link was not updated");
            }
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

    /// A tree recorded by an earlier run is ended, not adopted: its requests belong to a dead
    /// epoch and its turns are settled by probes on the new App Server. Recovery matches the
    /// recorded program against an App Server on the Node's own endpoint, never the system server.
    fn kill_orphan(&self) {
        let Ok(token) = std::fs::read_to_string(&self.pidfile) else {
            return;
        };
        let endpoint = self.spawn_endpoint.to_string();
        match tree::stop_recorded(token.trim(), |member| {
            member.contains("app-server") && member.contains(&endpoint)
        }) {
            Ok(Some(_)) => tracing::info!(
                event = "app_server.leftover_stopped",
                "stopping App Server left by an earlier run"
            ),
            Ok(None) => {}
            Err(error) => tracing::warn!(
                event = "app_server.leftover_left_alone",
                %error,
                "the App Server left by an earlier run was not ended"
            ),
        }
        if matches!(tree::Leftover::find(token.trim()), Ok(None)) {
            let _ = std::fs::remove_file(&self.pidfile);
        }
    }

    async fn spawn(&self, launch: &CodexLaunch) -> Result<tokio::process::Child> {
        let mut owned = self.tree.lock().await;
        ensure!(!self.stopping(), "the App Server is stopping");
        match std::fs::read_to_string(&self.pidfile) {
            Ok(token) => {
                ensure!(
                    matches!(tree::Leftover::find(token.trim()), Ok(None)),
                    "the previous App Server tree has not been confirmed gone"
                );
                std::fs::remove_file(&self.pidfile)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut command = std::process::Command::new(&launch.bin);
        command
            .envs(&launch.env)
            .args(["app-server", "--listen"])
            .arg(self.spawn_endpoint.listen_argument()?)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let (spawned, mut child) = tree::Tree::spawn(command, TREE_KEY, true)?;
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
        config::private_write(&self.pidfile, spawned.token().as_bytes())?;
        *owned = Some(spawned);
        Ok(child)
    }

    async fn end_spawned(&self, child: &mut tokio::process::Child) {
        let mut owned = self.tree.lock().await;
        let cleared = if let Some(mut tree) = owned.take() {
            vibeface_bridge::agent::terminate_process_tree(child, &mut tree).await
        } else {
            // stop() finished all group signals before releasing this lock.
            let reaped = matches!(
                tokio::time::timeout(Duration::from_secs(2), child.wait()).await,
                Ok(Ok(_))
            );
            reaped
                && std::fs::read_to_string(&self.pidfile)
                    .is_ok_and(|token| matches!(tree::Leftover::find(token.trim()), Ok(None)))
        };
        if cleared {
            let _ = std::fs::remove_file(&self.pidfile);
        }
    }

    /// Runs the Node's own App Server once. `true` when a new target ended it; its turns are
    /// settled by probes on the next App Server, as after a crash.
    async fn run_spawned(&self, launch: &CodexLaunch) -> bool {
        if self
            .target
            .lock()
            .expect("target lock")
            .configuration_reload_pending
        {
            self.point(None);
            let disconnected = tokio::time::timeout(READY_WAIT, async {
                while self.client.status().await["connection"]["state"] == "connected" {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            if disconnected.is_err() {
                self.set_link(Link::LaunchFailed {
                    reason: "could not reconnect Codex after its configuration changed".into(),
                });
                return false;
            }
        }
        self.point(
            launch
                .external_auth
                .is_none()
                .then(|| self.spawn_endpoint.path().to_owned()),
        );
        let mut child = match self.spawn(launch).await {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(event = "app_server.start_failed", %error, "App Server could not start");
                self.set_link(Link::LaunchFailed {
                    reason: format!("App Server could not start: {error}"),
                });
                return false;
            }
        };
        if let Some(auth) = &launch.external_auth {
            // The endpoint stays private until external authentication is installed. The
            // target/lease boundary has already kept active turns on their original launch.
            let bootstrap = async {
                let mut session = loop {
                    match vibeface_bridge::codex::DirectSession::connect(self.spawn_endpoint.path())
                        .await
                    {
                        Ok(session) => break session,
                        Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
                    }
                };
                let result=session.request("account/login/start",json!({"type":"chatgptAuthTokens","accessToken":auth.access_token,"chatgptAccountId":auth.account_id,"chatgptPlanType":auth.plan_type})).await;
                session.close().await;
                result
                    .map(|_| ())
                    .map_err(|_| anyhow::anyhow!("Codex rejected the imported access token"))
            };
            let authenticated = tokio::select! {
                result=tokio::time::timeout(START_LIMIT,bootstrap)=>matches!(result,Ok(Ok(()))),
                _=self.changed.notified()=>false,
                _=tree::observe_exit(&mut child)=>false,
            };
            if !authenticated {
                self.end_spawned(&mut child).await;
                self.set_link(Link::LaunchFailed {reason:"Codex external authentication could not be prepared; refresh the source CLI login or sign in for this Node".into()});
                return false;
            }
            self.point(Some(self.spawn_endpoint.path().to_owned()));
        }
        {
            let mut target = self.target.lock().expect("target lock");
            target.applied = Some(CodexTarget::Spawn(launch.clone()));
            target.configuration_reload_pending = false;
        }
        self.set_link(Link::Starting);
        let limit = tokio::time::sleep(START_LIMIT);
        tokio::pin!(limit);
        let mut late = false;
        let status = loop {
            tokio::select! {
                status = tree::observe_exit(&mut child) => break Some(status),
                () = self.changed.notified() => break None,
                () = &mut limit, if !late => {
                    late = true;
                    self.set_link(Link::HandshakeFailed {
                        socket: self.spawn_endpoint.path().to_owned(),
                        reason: format!("Codex 启动 {} 秒后仍连不上", START_LIMIT.as_secs()),
                    });
                }
            }
        };
        self.end_spawned(&mut child).await;
        let replaced = match status {
            Some(status) => {
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
                tracing::info!(
                    event = "app_server.restarted",
                    "App Server restarts with new provider settings"
                );
                true
            }
        };
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
        events: EventSender,
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
                configuration_reload_pending: false,
                leases: BTreeSet::new(),
            }),
            changed: tokio::sync::Notify::new(),
            spawn_endpoint: Endpoint::codex_in_private_dir(&directory)?,
            agent_alias: Alias::new(directory.join(if cfg!(windows) {
                "app-server.endpoint"
            } else {
                "app-server.sock"
            })),
            pidfile: data_dir.join("codex-app-server.pid"),
            link: std::sync::Mutex::new(Link::Starting),
            tree: tokio::sync::Mutex::new(None),
            stopping: std::sync::atomic::AtomicBool::new(false),
        };
        inner.kill_orphan();
        inner.point(named);
        Ok(Self {
            client,
            inner: Arc::new(inner),
            connected: tokio::sync::Notify::new(),
            epoch: std::sync::atomic::AtomicU64::new(0),
            user: std::sync::Mutex::default(),
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
        self.inner.agent_alias.path().to_owned()
    }

    /// Switches after active turns end. The system App Server is always left running.
    pub(crate) fn retarget(&self, target: CodexTarget) {
        let mut state = self.inner.target.lock().expect("target lock");
        if state.current == target && !state.configuration_reload_pending {
            state.pending = None;
            return;
        }
        if !state.leases.is_empty() {
            state.pending = Some(target);
            return;
        }
        if !matches!(target, CodexTarget::Spawn(_)) {
            state.configuration_reload_pending = false;
        }
        state.current = target;
        state.pending = None;
        self.inner.set_link(Link::Starting);
        self.inner.changed.notify_one();
    }

    pub(crate) fn configuration_changed(&self) {
        self.inner
            .target
            .lock()
            .expect("target lock")
            .configuration_reload_pending = true;
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
            || target.configuration_reload_pending
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
            CodexTarget::Spawn(_) => vec![self.inner.spawn_endpoint.path().to_owned()],
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
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut owned = inner.tree.blocking_lock();
            if let Some(mut spawned) = owned.take() {
                let token = spawned.token().to_owned();
                spawned.stop();
                if matches!(tree::Leftover::find(&token), Ok(None)) {
                    let _ = std::fs::remove_file(&inner.pidfile);
                }
            }
        })
        .await
        .ok();
    }
}

const MODEL_PAGES: usize = 5;

fn account_authentication(account: &Value) -> Authentication {
    match (
        account["requiresOpenaiAuth"].as_bool(),
        account.get("account"),
    ) {
        (Some(false), _) => Authentication::NotRequired,
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
            Authentication::NotRequired
        );
        assert_eq!(
            account_authentication(&json!({"requiresOpenaiAuth":false,"account":null})),
            Authentication::NotRequired
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
        let link = Link::Connected {
            socket: PathBuf::from("/fixture.sock"),
            user_agent: format!("Authorization: Bearer fixture-secret\n{}", "x".repeat(800)),
        }
        .sanitized();
        let Link::Connected { user_agent, .. } = link else {
            panic!("wrong link state")
        };
        assert!(!user_agent.contains("fixture-secret"));
        assert!(!user_agent.chars().any(char::is_control));
        assert_eq!(user_agent.chars().count(), 512);
        let failed = Link::LaunchFailed {
            reason: "Authorization: Bearer fixture-secret".into(),
        }
        .sanitized();
        assert_eq!(
            failed,
            Link::LaunchFailed {
                reason: "Codex App Server could not start; check the local Node log".into()
            }
        );
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
        assert_eq!(
            trace(&[turn("r", "inProgress", true)]),
            SteerTrace::Received {
                turn_id: "r".into()
            }
        );
        assert_eq!(
            trace(&[turn("r", "inProgress", false)]),
            SteerTrace::Pending
        );
        assert_eq!(
            trace(&[turn("r", "completed", true)]),
            SteerTrace::Ended {
                turn_id: "r".into(),
                outcome: SteerOutcome::Answered
            }
        );
        assert_eq!(
            trace(&[turn("r", "interrupted", true)]),
            SteerTrace::Ended {
                turn_id: "r".into(),
                outcome: SteerOutcome::Unfinished
            }
        );
        assert_eq!(
            trace(&[turn("r", "completed", false), turn("x", "completed", true)]),
            SteerTrace::Ended {
                turn_id: "x".into(),
                outcome: SteerOutcome::Answered
            }
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
    #[cfg(unix)]
    use crate::providers::{AppServerMode, Local, Provider, ProviderEnv};
    #[cfg(unix)]
    use futures_util::{SinkExt as _, StreamExt as _};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
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

    #[cfg(unix)]
    async fn ended(group: u32) -> bool {
        for _ in 0..100 {
            if !tree::root_is_running(group) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[cfg(unix)]
    fn artifacts(name: &str) -> Result<PathBuf> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("{name}-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        Ok(dir)
    }

    /// Unix socket paths stay short, so fixture sockets live under `/tmp`.
    #[cfg(unix)]
    fn sockets() -> Result<PathBuf> {
        let dir = PathBuf::from(format!(
            "/tmp/vf-app-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..10]
        ));
        config::private_dir(&dir)?;
        Ok(dir)
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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
                        } else if let Some(refusal) = turn.as_str() {
                            // The App Server's own refusal, in its wording.
                            json!({"id":request["id"],"error":{"code":-32600,"message":refusal}})
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

    #[cfg(unix)]
    fn stale_socket(path: &Path) -> Result<()> {
        drop(std::os::unix::net::UnixListener::bind(path)?);
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
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
            assert_eq!(
                node.store
                    .call("codex_server.pending_checks", move |store| store
                        .pending_checks())
                    .await?
                    .len(),
                1
            );
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
            assert!(
                node.store
                    .call("codex_server.pending_checks", move |store| store
                        .pending_checks())
                    .await?
                    .is_empty()
            );
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

    #[cfg(unix)]
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
    #[cfg(unix)]
    async fn cancelling_stop_keeps_tree_ownership_until_group_signals_finish() -> Result<()> {
        use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
        use tokio::io::{AsyncBufReadExt as _, BufReader};
        let dir = artifacts("codex-cancel-stop")?;
        let (program, _) = recording_codex(&dir)?;
        let launch = CodexLaunch {
            bin: program,
            env: BTreeMap::from([("HOME".into(), dir.display().to_string())]),
            external_auth: None,
        };
        let (events, _events) = vibeface_bridge::codex::event_channel();
        let server = Arc::new(CodexServer::open(
            CodexTarget::Spawn(launch.clone()),
            &LocalIdentity::generate(),
            &dir,
            events,
        )?);
        let mut child = server.inner.spawn(&launch).await?;
        let root = child.id().unwrap();
        let mut command = std::process::Command::new("python3");
        command.args(["-c", "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); print('ready', flush=True); time.sleep(120)"])
            .env("HOME", &dir)
            .stdout(std::process::Stdio::piped())
            .process_group(root as i32);
        let mut member = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()?;
        let mut ready = String::new();
        BufReader::new(member.stdout.take().unwrap())
            .read_line(&mut ready)
            .await?;
        assert_eq!(ready, "ready\n");
        let stopping = tokio::spawn({
            let server = server.clone();
            async move { server.stop().await }
        });
        tokio::time::timeout(Duration::from_secs(5), tree::observe_exit(&mut child)).await??;
        assert_eq!(
            child.id(),
            Some(root),
            "the stop still has its owned root anchor"
        );
        assert!(
            tree::try_observe_exit(&mut member)?.is_none(),
            "the controlled member keeps stop in its grace period"
        );
        stopping.abort();
        assert!(stopping.await.unwrap_err().is_cancelled());
        let retained_lock = server.inner.tree.try_lock().is_err();
        let (_, status) = tokio::join!(server.inner.end_spawned(&mut child), member.wait());
        assert_eq!(status?.signal(), Some(libc::SIGKILL));
        assert!(child.id().is_none());
        std::fs::remove_dir_all(server.socket().parent().unwrap())?;
        std::fs::remove_dir_all(dir)?;
        assert!(
            retained_lock,
            "cancelling stop cannot release tree ownership while blocking group signals continue"
        );
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn failed_auth_bootstrap_keeps_an_unconfirmed_tree_record() -> Result<()> {
        use std::os::unix::process::CommandExt as _;
        use tokio::io::{AsyncBufReadExt as _, BufReader};
        let dir = artifacts("codex-bootstrap-cleanup")?;
        let program = dir.join("early-codex");
        std::fs::write(
            &program,
            r#"#!/usr/bin/env python3
import os, pathlib, time
release = pathlib.Path(os.environ['HOME']) / 'release-root'
while not release.exists():
    time.sleep(0.01)
raise SystemExit(7)
"#,
        )?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
        let launch = CodexLaunch {
            bin: program,
            env: BTreeMap::from([("HOME".into(), dir.display().to_string())]),
            external_auth: Some(crate::providers::ExternalCodexLogin {
                access_token: "synthetic-bootstrap-access".into(),
                account_id: "fixture".into(),
                plan_type: None,
            }),
        };
        let (events, _events) = vibeface_bridge::codex::event_channel();
        let server = CodexServer::open(
            CodexTarget::Spawn(launch.clone()),
            &LocalIdentity::generate(),
            &dir,
            events,
        )?;
        let running = tokio::spawn({
            let inner = server.inner.clone();
            let launch = launch.clone();
            async move { inner.run_spawned(&launch).await }
        });
        let token = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(token) = std::fs::read_to_string(&server.inner.pidfile) {
                    break token;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let mut command = std::process::Command::new("python3");
        command.args(["-c", "import pathlib, signal, sys, time; signal.signal(signal.SIGTERM, lambda *_: (pathlib.Path(sys.argv[1]).write_text('term'), sys.exit(0))); print('ready', flush=True); time.sleep(120)"])
            .arg(dir.join("member-term"))
            .stdout(std::process::Stdio::piped())
            .process_group(token.trim().parse()?);
        let mut member = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()?;
        let mut ready = String::new();
        BufReader::new(member.stdout.take().unwrap())
            .read_line(&mut ready)
            .await?;
        assert_eq!(ready, "ready\n");
        std::fs::write(dir.join("release-root"), b"exit")?;
        assert!(!tokio::time::timeout(Duration::from_secs(10), running).await??);
        let recorded = std::fs::read_to_string(&server.inner.pidfile).ok();
        let received_term = dir.join("member-term").exists();
        let retry = server.inner.spawn(&launch).await;
        let refused = retry.is_err();
        if let Ok(mut child) = retry {
            server.inner.end_spawned(&mut child).await;
        }
        let retained = std::fs::read_to_string(&server.inner.pidfile).ok();
        let _ = member.start_kill();
        member.wait().await?;
        let mut resumed = server.inner.spawn(&launch).await?;
        server.inner.end_spawned(&mut resumed).await;
        std::fs::remove_dir_all(server.socket().parent().unwrap())?;
        std::fs::remove_dir_all(&dir)?;
        assert_eq!(
            recorded.as_deref(),
            Some(token.as_str()),
            "unconfirmed bootstrap cleanup retains its exact process record"
        );
        assert!(
            received_term,
            "bootstrap failure asks its owned live member to finish"
        );
        assert!(
            refused,
            "another launch cannot replace an unconfirmed process record"
        );
        assert_eq!(
            retained, recorded,
            "a refused retry preserves the exact record"
        );
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn external_authentication_precedes_endpoint_publication_and_changes_after_leases_end()
    -> Result<()> {
        let dir = artifacts("codex-external-login")?;
        let (program, ran) = recording_codex(&dir)?;
        let first = CodexTarget::Spawn(CodexLaunch {
            bin: program.clone(),
            env: BTreeMap::new(),
            external_auth: Some(crate::providers::ExternalCodexLogin {
                access_token: "synthetic-first-access".into(),
                account_id: "fixture-account".into(),
                plan_type: None,
            }),
        });
        let (events, _events) = vibeface_bridge::codex::event_channel();
        let server = CodexServer::open(first.clone(), &LocalIdentity::generate(), &dir, events)?;
        let listener = tokio::net::UnixListener::bind(server.inner.spawn_endpoint.path())?;
        let captured = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let login_seen = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let calls = captured.clone();
        let seen = login_seen.clone();
        let gate = release.clone();
        let fixture = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((stream, _)) = listener.accept().await {
                let calls = calls.clone();
                let seen = seen.clone();
                let gate = gate.clone();
                connections.spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(frame)) = socket.next().await {
                        let Ok(input) = serde_json::from_slice::<Value>(&frame.into_data()) else {
                            continue;
                        };
                        if input.get("id").is_none() {
                            continue;
                        }
                        let result = if input["method"] == "initialize" {
                            json!({"userAgent":"controlled-external-auth"})
                        } else if input["method"] == "account/login/start" {
                            let first = {
                                let mut calls = calls.lock().unwrap();
                                calls.push(input["params"].clone());
                                calls.len() == 1
                            };
                            if first {
                                seen.notify_one();
                                gate.notified().await;
                            }
                            json!({"type":"chatgptAuthTokens"})
                        } else {
                            json!({"data":[]})
                        };
                        if socket
                            .send(json!({"id":input["id"],"result":result}).to_string().into())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
        });
        server.start();
        tokio::time::timeout(Duration::from_secs(5), login_seen.notified()).await?;
        assert!(
            server.inner.endpoint.borrow().is_none(),
            "no Node client can use the server before authentication"
        );
        release.notify_one();
        link_becomes(&server, |link| matches!(link, Link::Connected { .. })).await;
        assert_eq!(
            captured.lock().unwrap()[0],
            json!({"type":"chatgptAuthTokens","accessToken":"synthetic-first-access","chatgptAccountId":"fixture-account","chatgptPlanType":null})
        );
        lines(&ran, 1).await;
        let lease = server.lease("manager:external-fixture".into(), &first)?;
        let second = CodexTarget::Spawn(CodexLaunch {
            bin: program,
            env: BTreeMap::new(),
            external_auth: Some(crate::providers::ExternalCodexLogin {
                access_token: "synthetic-next-access".into(),
                account_id: "fixture-account".into(),
                plan_type: None,
            }),
        });
        server.retarget(second.clone());
        assert!(server.inner.target.lock().unwrap().current == first);
        assert_eq!(captured.lock().unwrap().len(), 1);
        drop(lease);
        tokio::time::timeout(Duration::from_secs(5), async {
            while captured.lock().unwrap().len() != 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert_eq!(
            captured.lock().unwrap()[1]["accessToken"],
            "synthetic-next-access"
        );
        assert!(
            lines(&ran, 2)
                .await
                .iter()
                .all(|arguments| !arguments.contains("synthetic-")),
            "credentials never enter program arguments"
        );
        server.stop().await;
        fixture.abort();
        let _ = fixture.await;
        std::fs::remove_dir_all(server.socket().parent().unwrap())?;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn new_private_configuration_does_not_block_a_deferred_system_or_off_target() -> Result<()>
    {
        let dir = artifacts("codex-config-retarget")?;
        let (program, ran) = recording_codex(&dir)?;
        let target = CodexTarget::Spawn(CodexLaunch {
            external_auth: None,
            bin: program,
            env: BTreeMap::new(),
        });
        let mut outcomes = Vec::new();
        for next in [
            CodexTarget::System(dir.join("system.sock")),
            CodexTarget::Off,
        ] {
            let (events, _events) = vibeface_bridge::codex::event_channel();
            let server =
                CodexServer::open(target.clone(), &LocalIdentity::generate(), &dir, events)?;
            let mut lease = server.lease("manager:config-fixture".into(), &target)?;
            server.configuration_changed();
            server.retarget(next.clone());
            lease.retain_until_read_back();
            drop(lease);
            assert!(server.inner.target.lock().unwrap().current == target);
            assert!(server.configuration_pending(&next));
            assert!(server.lease("manager:new-fixture".into(), &next).is_err());
            server.release_manager("config-fixture");
            outcomes.push((
                next.server(),
                server.configuration_pending(&next),
                server.lease("manager:new-fixture".into(), &next).is_ok(),
            ));
            server.stop().await;
            std::fs::remove_dir_all(server.socket().parent().unwrap())?;
        }
        assert!(!ran.exists());
        std::fs::remove_dir_all(dir)?;
        assert_eq!(
            outcomes,
            [("system".into(), false, true), ("off".into(), false, false)],
            "applied non-Spawn targets finish configuration work; only system accepts turns"
        );
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn new_private_configuration_reloads_the_same_launch_after_active_leases_end()
    -> Result<()> {
        let dir = artifacts("codex-config-reload")?;
        let out = dir.join("starts");
        let fixture = dir.join("fake-codex");
        std::fs::write(
            &fixture,
            "#!/bin/sh\necho \"$$\" >> \"$FIXTURE_OUT\"\nexec sleep 60\n",
        )?;
        std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700))?;
        let mut local = Local {
            switches: Default::default(),
            bins: [(Provider::Codex, fixture)].into(),
            env: Default::default(),
        };
        local.env.apply(
            Provider::Codex,
            [("FIXTURE_OUT".into(), Some(out.display().to_string()))].into(),
        )?;
        local.switches.execution.insert(Provider::Codex);
        let target = local.codex_target(false);
        let (events, _events) = vibeface_bridge::codex::event_channel();
        let server = CodexServer::open(target.clone(), &LocalIdentity::generate(), &dir, events)?;
        server.start();
        let first = lines(&out, 1).await;
        let pid: u32 = first[0].parse()?;
        let lease = server.lease("manager:config-fixture".into(), &target)?;
        server.configuration_changed();
        for next in [
            CodexTarget::System(dir.join("system.sock")),
            CodexTarget::Off,
        ] {
            server.retarget(next);
            server.retarget(target.clone());
            assert!(
                server.configuration_pending(&target),
                "cancelling a deferred target still reloads the new private configuration"
            );
        }
        server.retarget(target.clone());
        assert!(server.configuration_pending(&target));
        assert!(tree::root_is_running(pid));
        assert!(
            server
                .lease("manager:stale-fixture".into(), &target)
                .is_err()
        );
        drop(lease);
        let second = lines(&out, 2).await;
        assert_ne!(second[0], second[1]);
        assert!(ended(pid).await);
        server.retarget(target);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            lines(&out, 2).await.len(),
            2,
            "an unchanged request does not reload again"
        );
        server.stop().await;
        std::fs::remove_dir_all(server.socket().parent().unwrap())?;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
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
        let (events, _events) = vibeface_bridge::codex::event_channel();
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
        assert!(tree::root_is_running(pid.parse()?));
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
            tree::root_is_running(pid.parse()?),
            "the dispatched work still uses the old launch"
        );
        assert!(server.inner.target.lock().unwrap().pending.is_some());
        server.release_work("fixture-work");
        let second = lines(&out, 2).await;
        assert!(second[1].ends_with(" fixture-replaced-key none"));
        assert!(ended(pid.parse()?).await, "the old App Server ends");

        local.switches.execution.clear();
        server.retarget(local.codex_target(false));
        let second_group: u32 = second[1].split_once(' ').unwrap().0.parse()?;
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
    #[cfg(unix)]
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
        let stalled = dir.join("stalled.sock");
        let listener = tokio::net::UnixListener::bind(&stalled)?;
        let silent = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert!(
                stream.next().await.is_some(),
                "the initialize request reached the fixture"
            );
            std::future::pending::<()>().await;
        });
        let started = tokio::time::Instant::now();
        assert_eq!(
            discover(&[stalled.clone(), live.clone()]).await,
            Discovery::Live {
                socket: live,
                user_agent: "codex/fixture".into(),
            },
            "a silent initialize does not prevent checking the next candidate"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            matches!(discover(std::slice::from_ref(&stalled)).await, Discovery::HandshakeFailed { socket, reason }
            if socket == stalled && reason == "Codex App Server connection probe timed out")
        );
        silent.abort();
        fixture.abort();
        closer.abort();
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
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
                opencode_endpoint: None,
            },
            bins: BTreeMap::from([(Provider::Codex, program)]),
            env: ProviderEnv::default(),
        };
        let identity = LocalIdentity::generate();
        let (events, _events) = vibeface_bridge::codex::event_channel();
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

    /// Codex 0.167 answers `thread/read` with "thread not loaded" for a thread in neither its
    /// Codex folder nor its memory, as after the Node moved to another App Server.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_thread_the_app_server_does_not_have_is_probed_as_missing() -> Result<()> {
        let dir = artifacts("codex-missing-thread")?;
        let sockets = sockets()?;
        let socket = sockets.join("app.sock");
        let refusal = Arc::new(std::sync::Mutex::new(json!("thread not loaded: gone")));
        let fixture = system_app_server(&socket, "codex/fixture", Some(refusal.clone()));
        let local = Local {
            switches: crate::providers::Providers {
                execution: [Provider::Codex].into(),
                execution_models: BTreeMap::new(),
                codex_app_server: crate::providers::AppServer {
                    mode: AppServerMode::System,
                    socket: Some(socket),
                },
                opencode_endpoint: None,
            },
            bins: BTreeMap::new(),
            env: ProviderEnv::default(),
        };
        let identity = LocalIdentity::generate();
        let (events, _events) = vibeface_bridge::codex::event_channel();
        let server = CodexServer::open(local.codex_target(false), &identity, &dir, events)?;
        server.start();
        link_becomes(&server, |link| matches!(link, Link::Connected { .. })).await;
        assert_eq!(
            server.probe("gone", Some("turn"), "work").await?,
            Probe::Missing
        );
        *refusal.lock().unwrap() = json!("authentication required");
        assert!(
            server.probe("gone", Some("turn"), "work").await.is_err(),
            "another refusal leaves the turn unknown"
        );
        server.stop().await;
        fixture.abort();
        std::fs::remove_dir_all(server.socket().parent().unwrap())?;
        std::fs::remove_dir_all(sockets)?;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
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
                opencode_endpoint: None,
            },
            bins: BTreeMap::new(),
            env: ProviderEnv::default(),
        };
        let identity = LocalIdentity::generate();
        let (events, _events) = vibeface_bridge::codex::event_channel();
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
