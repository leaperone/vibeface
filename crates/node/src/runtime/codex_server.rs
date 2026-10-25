//! The coordination runtime's App Server: the process, its socket and the Node's own client.
//! A managed App Server runs in its own process group recorded in a pidfile, so a crash of
//! this Node never leaves an unmanaged one behind: the next start kills the recorded group.
use super::*;
use crate::providers::{CodexLaunch, CodexTarget};
use crate::work::Probe;
use std::os::unix::process::CommandExt as _;
use vibeface_bridge::codex::{CodexClient, CodexError};

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
    /// Reads the journaled thread; it never starts or replays work. Without a known turn, the
    /// turn is the one whose user message carries `client_id`.
    pub(crate) async fn probe(
        &self,
        thread: &str,
        turn: Option<&str>,
        client_id: &str,
        epoch: u64,
    ) -> Result<Probe, CodexError> {
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
            Err(error) if error.answered => return Ok(Probe::Missing),
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
                None => return Ok(Probe::NotStarted),
            },
        };
        let Some(found) = found else {
            return Ok(Probe::Missing);
        };
        let turn_id = found["id"].as_str().unwrap_or_default().to_owned();
        Ok(match found["status"].as_str() {
            Some("inProgress") => Probe::Running { turn_id, epoch },
            Some(status) => Probe::Terminal {
                turn_id,
                status: status.into(),
                text: last_answer(found),
                error: found["error"]["message"].as_str().map(str::to_owned),
            },
            None => Probe::Missing,
        })
    }
}

/// The Node's own client observes, probes and interrupts; the coordination Agent sends turns on
/// its own connection to the same App Server. That App Server is the one already running on this
/// computer, whose sessions the owner shares, or one the Node starts itself.
pub(crate) struct CodexServer {
    client: CodexClient,
    inner: Arc<Inner>,
    /// Wakes sends waiting for the client to connect.
    connected: tokio::sync::Notify,
}

/// How long a send waits for the App Server, as right after the Node starts, before it is refused.
const READY_WAIT: Duration = Duration::from_secs(20);

struct Inner {
    client: CodexClient,
    /// The socket the client connects to; the supervisor moves it.
    endpoint: tokio::sync::watch::Sender<Option<PathBuf>>,
    target: std::sync::Mutex<CodexTarget>,
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
                tracing::warn!(%error, "the Agent's App Server link was not updated");
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
            tracing::info!(group, "stopping App Server left by an earlier run");
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
                tracing::warn!(%error, "App Server could not start");
                self.set_link(Link::LaunchFailed {
                    reason: format!("App Server could not start: {error}"),
                });
                return false;
            }
        };
        self.set_link(Link::Starting);
        let status = tokio::select! {
            status = child.wait() => Some(status),
            () = self.changed.notified() => None,
        };
        let group = self.group.lock().expect("group lock").take();
        let replaced = match status {
            Some(status) => {
                if let Some(group) = group {
                    // Tools the App Server started share its group and die with it.
                    signal_group(group, libc::SIGKILL);
                }
                tracing::warn!(?status, "App Server exited");
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
                tracing::info!("App Server restarts with new provider settings");
                true
            }
        };
        let _ = std::fs::remove_file(&self.pidfile);
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
            let target = self.target.lock().expect("target lock").clone();
            let started = Instant::now();
            let replaced = match &target {
                CodexTarget::Off => {
                    self.point(None);
                    self.set_link(Link::Off);
                    self.changed.notified().await;
                    true
                }
                CodexTarget::Spawn(launch) => self.run_spawned(launch).await,
                CodexTarget::System(socket) => self.use_system(&socket).await,
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
        events: mpsc::UnboundedSender<Value>,
    ) -> Result<Self> {
        let directory = socket_dir(identity, data_dir)?;
        let (endpoint, receiver) = tokio::sync::watch::channel(None);
        let client = CodexClient::start(receiver, events);
        let named = match &target {
            CodexTarget::System(socket) => Some(socket.clone()),
            _ => None,
        };
        let inner = Inner {
            client: client.clone(),
            endpoint,
            target: std::sync::Mutex::new(target),
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
        })
    }

    pub(super) fn start(&self) {
        tokio::spawn(self.inner.clone().supervise());
    }

    pub(super) fn connected(&self) {
        self.connected.notify_waiters();
    }

    /// Whether the client is connected, waiting up to `READY_WAIT` while the supervisor and the
    /// client's backoff bring it up. With Codex turned off it does not wait.
    pub(crate) async fn ready(&self) -> bool {
        if *self.inner.target.lock().expect("target lock") == CodexTarget::Off {
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
        self.inner.target.lock().expect("target lock").server()
    }

    /// The socket the coordination Agent connects to; it follows the App Server in use.
    pub(crate) fn socket(&self) -> PathBuf {
        self.inner.agent_socket.clone()
    }

    /// Switches to `target`. A new launch ends the App Server the Node started; the system App
    /// Server is left running whatever the target.
    pub(crate) fn retarget(&self, target: CodexTarget) {
        let mut current = self.inner.target.lock().expect("target lock");
        if *current == target {
            return;
        }
        *current = target;
        self.inner.set_link(Link::Starting);
        self.inner.changed.notify_one();
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
        let target = self.inner.target.lock().expect("target lock").clone();
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

    fn system_app_server(socket: &Path, user_agent: &'static str) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::UnixListener::bind(socket).expect("fixture socket");
        tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((stream, _)) = listener.accept().await {
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
            codex_bin: fixture.clone(),
            env,
        };
        local.switches.execution = true;
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

        local.env.apply(
            Provider::Codex,
            BTreeMap::from([("CPA_API_KEY".into(), Some("fixture-replaced-key".into()))]),
        )?;
        server.retarget(local.codex_target(false));
        let second = lines(&out, 2).await;
        assert!(second[1].ends_with(" fixture-replaced-key none"));
        assert!(ended(pid.parse()?).await, "the old App Server ends");

        local.switches.execution = false;
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
        let fixture = system_app_server(&live, "codex/fixture");
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
        let fixture = system_app_server(&first, "codex/first");
        std::os::unix::fs::symlink(&first, &control)?;
        let local = Local {
            switches: crate::providers::Providers {
                execution: true,
                codex_app_server: crate::providers::AppServer {
                    mode: AppServerMode::System,
                    socket: Some(control.clone()),
                },
            },
            codex_bin: program,
            env: ProviderEnv::default(),
        };
        assert_eq!(
            local.switches.codex_app_server.mode,
            AppServerMode::System,
            "the system App Server is the default"
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
        let restarted = system_app_server(&second, "codex/second");
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
                execution: true,
                codex_app_server: crate::providers::AppServer {
                    mode: AppServerMode::System,
                    socket: Some(candidate.clone()),
                },
            },
            codex_bin: "codex".into(),
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
