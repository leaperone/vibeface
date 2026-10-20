//! The coordination runtime's App Server: the process, its socket and the Node's own client.
//! A managed App Server runs in its own process group recorded in a pidfile, so a crash of
//! this Node never leaves an unmanaged one behind: the next start kills the recorded group.
use super::*;
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
    /// Threads the client re-subscribes after every reconnect; requests on them are answered
    /// by the method table.
    pub(crate) fn observe(&self, threads: Vec<String>) -> Result<(), CodexError> {
        self.client.set_observed_threads(threads)
    }
    pub(crate) async fn snapshot(&self) -> Value {
        self.client.snapshot().await
    }
    pub(crate) async fn capability(&self) -> Value {
        json!({"connected":self.client.snapshot().await["connection"]["state"] == "connected"})
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
            Err(error) if error.uncertain => return Err(error),
            Err(_) => return Ok(Probe::Missing),
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
/// its own connection to the same socket.
pub(crate) struct CodexServer {
    client: CodexClient,
    /// The App Server socket; the coordination Agent connects to it directly.
    pub socket: PathBuf,
    managed: Option<Arc<Managed>>,
}

struct Managed {
    socket: PathBuf,
    pidfile: PathBuf,
    codex_bin: PathBuf,
    group: std::sync::Mutex<Option<i32>>,
    stopping: std::sync::atomic::AtomicBool,
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

impl Managed {
    /// A group recorded by an earlier run is killed, not adopted: its requests belong to a
    /// dead epoch and its turns are settled by probes on the new App Server. This runs before
    /// the client exists, so nothing talks to the orphan.
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
        if command.contains("app-server") && command.contains(&self.socket.display().to_string()) {
            tracing::info!(group, "stopping App Server left by an earlier run");
            kill_group(group);
        }
        let _ = std::fs::remove_file(&self.pidfile);
    }

    fn spawn(&self) -> Result<tokio::process::Child> {
        if self.socket.exists() {
            use std::os::unix::fs::FileTypeExt;
            ensure!(
                std::fs::symlink_metadata(&self.socket)?
                    .file_type()
                    .is_socket(),
                "private Codex socket path is occupied by another file"
            );
            std::fs::remove_file(&self.socket)?;
        }
        let mut command = std::process::Command::new(&self.codex_bin);
        command
            .args(["app-server", "--listen"])
            .arg(format!("unix://{}", self.socket.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        let child = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()?;
        let group = child.id().context("App Server exited at once")? as i32;
        config::private_write(&self.pidfile, group.to_string().as_bytes())?;
        *self.group.lock().expect("group lock") = Some(group);
        Ok(child)
    }

    /// Restarts the App Server with backoff until the Node stops.
    async fn supervise(self: Arc<Self>) {
        let mut backoff = 1;
        while !self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            let started = Instant::now();
            match self.spawn() {
                Ok(mut child) => {
                    let status = child.wait().await;
                    if let Some(group) = self.group.lock().expect("group lock").take() {
                        // Tools the App Server started share its group and die with it.
                        signal_group(group, libc::SIGKILL);
                    }
                    let _ = std::fs::remove_file(&self.pidfile);
                    tracing::warn!(?status, "App Server exited");
                }
                Err(error) => tracing::warn!(%error, "App Server could not start"),
            }
            if started.elapsed() > Duration::from_secs(60) {
                backoff = 1;
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    }
}

impl CodexServer {
    pub(super) fn open(
        config: &Config,
        identity: &LocalIdentity,
        data_dir: &Path,
        events: mpsc::UnboundedSender<Value>,
    ) -> Result<Self> {
        let (socket, managed) = match &config.codex_socket {
            // A configured socket belongs to someone else (tests, a user's own App Server).
            Some(socket) => (socket.clone(), None),
            None => {
                let socket = socket_dir(identity, data_dir)?.join("codex.sock");
                let managed = Managed {
                    socket: socket.clone(),
                    pidfile: data_dir.join("codex-app-server.pid"),
                    codex_bin: config.codex_bin.clone(),
                    group: std::sync::Mutex::new(None),
                    stopping: std::sync::atomic::AtomicBool::new(false),
                };
                managed.kill_orphan();
                (socket, Some(Arc::new(managed)))
            }
        };
        Ok(Self {
            client: CodexClient::start(socket.clone(), events),
            socket,
            managed,
        })
    }

    pub(super) fn start(&self) {
        if let Some(managed) = &self.managed {
            tokio::spawn(managed.clone().supervise());
        }
    }

    /// Stops the managed App Server group; called on SIGTERM before the Node exits.
    pub(crate) async fn stop(&self) {
        let Some(managed) = &self.managed else {
            return;
        };
        managed
            .stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let group = managed.group.lock().expect("group lock").take();
        if let Some(group) = group {
            kill_group(group);
        }
        let _ = std::fs::remove_file(&managed.pidfile);
    }
}
