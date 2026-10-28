//! opencode on this device: one `opencode serve` on loopback that the Node starts when a bot turn
//! or a work first needs it, and starts again when it died or its program or variables changed.
//! Sessions live in opencode's own store, so a work is read back from there after a restart and
//! never sent again. opencode keeps no shell environment per session, so the serve loads the
//! Node's plugin (`extensions/opencode.ts`): it gives each session's tool shells the tool
//! environment of its turn from a private file and blanks what looks like a key.
use super::executor::{Executor, SteerAck, SteerTrace};
use super::*;
use crate::manager::{self, ManagerConfig, ManagerError, ManagerFailure};
use crate::scheduling::{SteerOutcome, SteerTarget};
use crate::work::{Probe, Signal, WorkRecord, WorkSpec, WorkState};
use futures_util::future::BoxFuture;
use reqwest::Method;
use std::os::unix::process::CommandExt as _;
use tokio::io::AsyncBufReadExt as _;
use vibeface_bridge::{
    agent::{self, AgentResult},
    failure::{ExecFailure, FailureKind},
    turn::TurnStatus,
};

/// The server reply turns record for opencode: its store keeps the sessions across serves.
pub(crate) const SERVER: &str = "opencode";
/// The plugin; the Node's serve loads a copy without the hook.
pub(crate) const PLUGIN: &str = include_str!("../../extensions/opencode.ts");
/// Set only in the Node's serve: the directory of the tool environment files.
const ENV_MARKER: &str = "VIBEFACE_OPENCODE_ENV";
const PIDFILE: &str = "serve.pid";
const READY_WAIT: Duration = Duration::from_secs(30);
const REQUEST_WAIT: Duration = Duration::from_secs(30);
/// How often a turn reads whether its run ended; the event stream may miss the end.
const TICK: Duration = Duration::from_secs(2);
const WORK_TICK: Duration = Duration::from_millis(if cfg!(test) { 200 } else { 5_000 });
/// Our message shows, the session is idle and nothing answered it this long after the last send:
/// the run ended without an answer, as with a model opencode does not have.
const QUIET_MS: i64 = 10_000;
/// A message opencode has not shown this long after it was sent never arrived.
const LOST_MS: i64 = 60_000;
const MAX_OUTPUT: usize = 65_536;

/// The Node's opencode serve and the bot turns running on it.
pub struct Opencode {
    dir: PathBuf,
    launch: std::sync::Mutex<Launch>,
    serve: Mutex<Option<Serve>>,
    http: reqwest::Client,
    /// Bot turns in progress by session: what a steer joins.
    live: std::sync::Mutex<HashMap<String, Live>>,
    /// Steered messages a completed turn answered.
    /// ponytail: in memory and cleared past 1024; one forgotten reads back as unfinished.
    answered: std::sync::Mutex<BTreeSet<String>>,
}

#[derive(Clone, PartialEq)]
struct Launch {
    bin: PathBuf,
    env: BTreeMap<String, String>,
}

/// How to reach one serve.
#[derive(Clone)]
struct Conn {
    base: String,
    password: String,
    /// A message created before this ran on a serve that is gone.
    started_ms: i64,
}

struct Serve {
    launch: Launch,
    conn: Conn,
    child: tokio::process::Child,
    group: i32,
    pidfile: PathBuf,
}

impl Serve {
    /// SIGTERM, then the drop's SIGKILL after a grace period.
    async fn stop(mut self) {
        signal_group(self.group, libc::SIGTERM);
        let _ = tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await;
    }
}

impl Drop for Serve {
    /// The tools the serve started share its group and end with it.
    fn drop(&mut self) {
        signal_group(self.group, libc::SIGKILL);
        let _ = std::fs::remove_file(&self.pidfile);
    }
}

fn signal_group(group: i32, signal: i32) {
    // SAFETY: kill(2) with a negative pid signals one process group and touches no memory.
    unsafe { libc::kill(-group, signal) };
}

/// A serve an earlier run of the Node left is killed, not adopted: its runs died with that Node
/// and probes read them back from the store. Only a group whose command is such a serve is
/// touched.
fn kill_orphan(pidfile: &Path) {
    let Some(group) = std::fs::read_to_string(pidfile)
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
    if command.contains(" serve --hostname 127.0.0.1 --port ") {
        tracing::info!(group, "stopping the opencode serve left by an earlier run");
        signal_group(group, libc::SIGKILL);
    }
    let _ = std::fs::remove_file(pidfile);
}

/// A bot turn in progress.
struct Live {
    /// Its first message, which steers name as the turn.
    turn: String,
    conn: Conn,
    directory: String,
    /// What every message of the turn carries besides its own: agent, system and model.
    body: Value,
    /// Our messages in the order sent: the turn's, then the steered ones.
    sent: Vec<String>,
    last_post_ms: i64,
    /// Steers join until the turn is settled.
    open: bool,
}

/// Ends a turn's events, tool environment and steering however the turn ends.
struct TurnGuard<'a> {
    owner: &'a Opencode,
    session: String,
    env_file: PathBuf,
    events: tokio::task::JoinHandle<()>,
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        self.events.abort();
        let _ = std::fs::remove_file(&self.env_file);
        self.owner.live.lock().expect("live").remove(&self.session);
    }
}

/// What became of a message sent to the serve.
enum Sent {
    Taken,
    /// Nothing ran.
    Refused(ExecFailure),
    Unknown(String),
}

impl Opencode {
    pub(crate) fn open(
        data_dir: &Path,
        bin: PathBuf,
        env: BTreeMap<String, String>,
    ) -> Result<Self> {
        let dir = data_dir.join("opencode");
        kill_orphan(&dir.join(PIDFILE));
        // The tool environments of an earlier run's turns hold tokens that act no more.
        let _ = std::fs::remove_dir_all(dir.join("env"));
        Ok(Self {
            dir,
            launch: std::sync::Mutex::new(Launch { bin, env }),
            serve: Mutex::new(None),
            http: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_secs(5))
                .build()?,
            live: Default::default(),
            answered: Default::default(),
        })
    }

    /// The program and variables of the next serve; one running with others is replaced when
    /// next used.
    pub(crate) fn set_launch(&self, bin: PathBuf, env: BTreeMap<String, String>) {
        *self.launch.lock().expect("launch") = Launch { bin, env };
    }

    /// Stops the serve; the Node calls this when it shuts down.
    pub(crate) async fn stop(&self) {
        if let Some(serve) = self.serve.lock().await.take() {
            serve.stop().await;
        }
    }

    /// The running serve, started first when there is none, it died or its launch changed.
    async fn conn(&self) -> Result<Conn> {
        let launch = self.launch.lock().expect("launch").clone();
        let mut serve = self.serve.lock().await;
        if let Some(running) = serve.as_mut()
            && running.launch == launch
            && running
                .child
                .try_wait()
                .is_ok_and(|status| status.is_none())
        {
            return Ok(running.conn.clone());
        }
        if let Some(old) = serve.take() {
            old.stop().await;
        }
        let started = self.spawn(launch).await?;
        Ok(serve.insert(started).conn.clone())
    }

    /// The serve running now, never started here.
    async fn running(&self) -> Option<Conn> {
        let mut serve = self.serve.lock().await;
        let running = serve.as_mut()?;
        (running.child.try_wait())
            .is_ok_and(|status| status.is_none())
            .then(|| running.conn.clone())
    }

    async fn spawn(&self, launch: Launch) -> Result<Serve> {
        config::private_dir(&self.dir)?;
        let dir = self.dir.canonicalize()?;
        let env_dir = dir.join("env");
        config::private_dir(&env_dir)?;
        // Not named like the copy `vibeface hooks install` puts where opencode finds it as well.
        let plugin = dir.join("vibeface-serve.ts");
        config::private_write(
            &plugin,
            PLUGIN.replace("__VIBEFACE_HOOK__", "null").as_bytes(),
        )?;
        // Tool shells inherit the serve's variables: keys and the serve's own are blanked.
        let blank: BTreeMap<String, String> = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .chain(launch.env.keys().cloned())
            .filter(|name| secret_name(name))
            .chain(
                [
                    "OPENCODE_SERVER_USERNAME",
                    "OPENCODE_SERVER_PASSWORD",
                    "OPENCODE_CONFIG_CONTENT",
                    ENV_MARKER,
                ]
                .map(String::from),
            )
            .map(|name| (name, String::new()))
            .collect();
        config::private_write(&env_dir.join("default.json"), &serde_json::to_vec(&blank)?)?;
        let password = crate::crypto::random_hex(32);
        crate::logging::add_secret(&password);
        // ponytail: the port may be taken before the serve binds it; the next use starts again.
        let port = std::net::TcpListener::bind("127.0.0.1:0")?
            .local_addr()?
            .port();
        let plugin = reqwest::Url::from_file_path(&plugin)
            .map_err(|()| anyhow::anyhow!("the opencode plugin path is not absolute"))?;
        let permission = json!({"*":"allow","question":"deny"});
        let settings = json!({"$schema":"https://opencode.ai/config.json","autoupdate":false,
            "share":"disabled","permission":permission,"agent":{"build":{"permission":permission}},
            "plugin":[plugin.as_str()]});
        let mut command = std::process::Command::new(&launch.bin);
        command
            .envs(&launch.env)
            .env("OPENCODE_SERVER_USERNAME", "opencode")
            .env("OPENCODE_SERVER_PASSWORD", &password)
            .env("OPENCODE_CONFIG_CONTENT", settings.to_string())
            .env(ENV_MARKER, &env_dir)
            .args([
                "serve",
                "--hostname",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
            .current_dir(&dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        let mut child = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()?;
        let group = child.id().context("the opencode serve exited at once")? as i32;
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let mut serve = Serve {
            launch,
            conn: Conn {
                base: String::new(),
                password,
                started_ms: now_ms(),
            },
            child,
            group,
            pidfile: dir.join(PIDFILE),
        };
        config::private_write(&serve.pidfile, group.to_string().as_bytes())?;
        if let Some(stderr) = stderr {
            tokio::spawn(crate::logging::forward_lines(
                stderr,
                usize::MAX >> 1,
                |line| {
                    crate::logging::child_line(
                        crate::logging::Source::Agent,
                        "opencode.serve.stderr",
                        line,
                        None,
                    )
                },
            ));
        }
        let mut lines =
            tokio::io::BufReader::new(stdout.context("the opencode serve has no output")?).lines();
        let listening = async {
            while let Some(line) = lines.next_line().await? {
                if let Some(base) = line.split("listening on ").nth(1) {
                    return anyhow::Ok(base.trim().trim_end_matches('/').to_owned());
                }
            }
            bail!("the opencode serve exited before it listened")
        };
        serve.conn.base = tokio::time::timeout(READY_WAIT, listening)
            .await
            .context("the opencode serve did not listen in time")??;
        // The password goes only to the loopback address the Node asked for.
        ensure!(
            serve.conn.base.starts_with("http://127.0.0.1:"),
            "the opencode serve listens on {}",
            serve.conn.base
        );
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                crate::logging::child_line(
                    crate::logging::Source::Agent,
                    "opencode.serve.stdout",
                    &line,
                    None,
                );
            }
        });
        tracing::info!(
            event = "opencode.serve.started",
            group,
            "opencode serve started"
        );
        Ok(serve)
    }

    /// One request to the serve: its status and JSON body, `null` when empty and the text when
    /// not JSON. An `Err` is a request whose fate is unknown.
    async fn call(
        &self,
        conn: &Conn,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<(u16, Value)> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", conn.base))
            .basic_auth("opencode", Some(&conn.password))
            .query(query)
            .timeout(REQUEST_WAIT);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await?;
        let body = match serde_json::from_slice(&bytes) {
            Ok(body) => body,
            Err(_) if bytes.is_empty() => Value::Null,
            Err(_) => Value::String(String::from_utf8_lossy(&bytes).chars().take(500).collect()),
        };
        Ok((status, body))
    }

    /// The session as opencode keeps it; `None` when it has none by that ID.
    async fn session(&self, conn: &Conn, id: &str) -> Result<Option<Value>> {
        match self
            .call(conn, Method::GET, &format!("/session/{id}"), &[], None)
            .await?
        {
            (200, session) => Ok(Some(session)),
            (404, _) => Ok(None),
            (status, body) => bail!("opencode answered {status} reading session {id}: {body}"),
        }
    }

    async fn message(&self, conn: &Conn, session: &str, id: &str) -> Result<Option<Value>> {
        let path = format!("/session/{session}/message/{id}");
        match self.call(conn, Method::GET, &path, &[], None).await? {
            (200, message) => Ok(Some(message)),
            (404, _) => Ok(None),
            (status, body) => bail!("opencode answered {status} reading message {id}: {body}"),
        }
    }

    /// Whether a run goes on in the session; opencode keeps the status by directory.
    async fn busy(&self, conn: &Conn, session: &str, directory: &str) -> Result<bool> {
        let query = [("directory", directory)];
        let (status, body) = self
            .call(conn, Method::GET, "/session/status", &query, None)
            .await?;
        ensure!(
            status == 200,
            "opencode answered {status} reading the session status"
        );
        Ok(body[session]["type"]
            .as_str()
            .is_some_and(|kind| kind != "idle"))
    }

    /// The session's messages from `first` on, oldest first; `None` when it has no `first`.
    async fn tail(&self, conn: &Conn, session: &str, first: &str) -> Result<Option<Vec<Value>>> {
        let path = format!("/session/{session}/message");
        for limit in [64, 512, 4096] {
            let query = [("limit", limit.to_string())];
            let query = query.each_ref().map(|(key, value)| (*key, value.as_str()));
            let (status, body) = self.call(conn, Method::GET, &path, &query, None).await?;
            ensure!(
                status == 200,
                "opencode answered {status} reading session {session}"
            );
            let Value::Array(mut messages) = body else {
                bail!("opencode sent no message list")
            };
            if let Some(at) = messages.iter().position(|m| m["info"]["id"] == first) {
                return Ok(Some(messages.split_off(at)));
            }
            if messages.len() < limit {
                return Ok(None);
            }
        }
        // ponytail: a message 4096 back reads as gone; turns and works read recent ones.
        Ok(None)
    }

    /// Creates a session in `directory`; its ID. The session's own rules allow every tool but
    /// questions, which nobody answers: they come after the agent's and the user's configured
    /// rules, and the last that matches wins. A subagent's session does not take them.
    async fn create(&self, conn: &Conn, directory: &str) -> Result<String, ExecFailure> {
        let body = json!({"permission":[{"permission":"*","pattern":"*","action":"allow"},
            {"permission":"question","pattern":"*","action":"deny"}]});
        let query = [("directory", directory)];
        match self
            .call(conn, Method::POST, "/session", &query, Some(&body))
            .await
        {
            Ok((200..=299, session)) => match session["id"].as_str() {
                Some(id) if vibeface_protocol::validate_id(id).is_ok() => Ok(id.to_owned()),
                _ => Err(ExecFailure::new(
                    FailureKind::Unknown,
                    "opencode created a session without an ID",
                )),
            },
            Ok((status, body)) => Err(http_failure(status, &body)),
            Err(error) => Err(ExecFailure::new(
                FailureKind::Transport,
                format!("opencode: {error:#}"),
            )),
        }
    }

    /// Sends a message; opencode runs it, or takes it into the run going on.
    async fn send(&self, conn: &Conn, session: &str, directory: &str, body: &Value) -> Sent {
        let path = format!("/session/{session}/prompt_async");
        let query = [("directory", directory)];
        match self
            .call(conn, Method::POST, &path, &query, Some(body))
            .await
        {
            Ok((200..=299, _)) => Sent::Taken,
            Ok((status @ 400..=499, body)) => Sent::Refused(http_failure(status, &body)),
            Ok((status, body)) => Sent::Unknown(format!("opencode answered {status}: {body}")),
            Err(error) => Sent::Unknown(format!("opencode: {error:#}")),
        }
    }

    async fn abort(&self, conn: &Conn, session: &str, directory: &str) -> Result<(u16, Value)> {
        let path = format!("/session/{session}/abort");
        let query = [("directory", directory)];
        self.call(conn, Method::POST, &path, &query, None).await
    }

    /// Aborts the session's run; `true` once opencode shows `last`, our last message, and no
    /// run. The abort repeats, as one that came before the run started stops nothing.
    async fn stop_run(&self, conn: &Conn, session: &str, directory: &str, last: &str) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        loop {
            let _ = self.abort(conn, session, directory).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            if let (Ok(Some(_)), Ok(false)) = (
                self.message(conn, session, last).await,
                self.busy(conn, session, directory).await,
            ) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
        }
    }

    /// What the turn in `session` sent, in order, and when it last sent.
    fn posted(&self, session: &str) -> (Vec<String>, i64) {
        let live = self.live.lock().expect("live");
        live.get(session)
            .map(|turn| (turn.sent.clone(), turn.last_post_ms))
            .unwrap_or_default()
    }

    fn touch(&self, session: &str) {
        if let Some(turn) = self.live.lock().expect("live").get_mut(session) {
            turn.last_post_ms = now_ms();
        }
    }

    /// Closes the turn in `session` to steers; the last message it sent.
    fn close(&self, session: &str) -> String {
        let mut live = self.live.lock().expect("live");
        live.get_mut(session)
            .map(|turn| {
                turn.open = false;
                turn.sent.last().cloned().unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// The session's messages from the turn's first once its run is over, `None` while it runs.
    /// It is over once the session is idle and every message we sent is answered, shows
    /// unanswered (`QUIET_MS`) or not at all (`LOST_MS`), or opencode reported an error. That
    /// closes the turn to steers, unless one came in meanwhile.
    async fn settled(
        &self,
        conn: &Conn,
        session: &str,
        directory: &str,
        errored: bool,
    ) -> Result<Option<Vec<Value>>> {
        let (sent, last_post) = self.posted(session);
        let Some(first) = sent.first() else {
            return Ok(None);
        };
        if self.busy(conn, session, directory).await? {
            return Ok(None);
        }
        let messages = self.tail(conn, session, first).await?.unwrap_or_default();
        let chains: Vec<Chain> = sent.iter().map(|id| chain(&messages, id)).collect();
        let quiet = now_ms() - last_post;
        let over = errored
            || chains.iter().all(Chain::terminal)
            || (quiet > QUIET_MS && chains.iter().all(|chain| chain.visible))
            || quiet > LOST_MS;
        if !over {
            return Ok(None);
        }
        let mut live = self.live.lock().expect("live");
        if let Some(turn) = live.get_mut(session) {
            if turn.sent.len() != sent.len() {
                return Ok(None);
            }
            turn.open = false;
        }
        Ok(Some(messages))
    }

    /// Whether no run goes on in a reply's session any more; one still going is aborted. Runs
    /// end with their serve, and the Node stops the serve of an earlier run when it opens.
    pub(crate) async fn run_ended(&self, session: &str) -> bool {
        let Some(conn) = self.running().await else {
            return true;
        };
        let directory = match self.session(&conn, session).await {
            Ok(Some(found)) => found["directory"].as_str().unwrap_or_default().to_owned(),
            Ok(None) => return true,
            Err(_) => return false,
        };
        match self.busy(&conn, session, &directory).await {
            Ok(false) => true,
            Ok(true) => {
                let _ = self.abort(&conn, session, &directory).await;
                false
            }
            Err(_) => false,
        }
    }

    /// One bot turn in its session on the serve. The session is created and saved before
    /// anything is sent; the message ID comes from the attempt, so a send whose fate is unknown
    /// is read back by it and never sent twice.
    pub(crate) async fn run_turn<F, P, PF>(
        &self,
        config: &ManagerConfig,
        prompt: &str,
        previous: Option<&str>,
        persist_session: impl FnOnce(String) -> F,
        mut cancel: tokio::sync::watch::Receiver<bool>,
        mut progress: P,
    ) -> Result<AgentResult>
    where
        F: Future<Output = Result<()>>,
        P: FnMut(Value) -> PF,
        PF: Future<Output = Result<()>>,
    {
        let workspace = (config.workspace)
            .to_str()
            .filter(|_| config.workspace.is_absolute() && config.workspace.is_dir())
            .context("manager workspace must be an existing absolute directory")?;
        let secrets = manager::secret_values(config);
        let error = |outcome, session: Option<&str>, failure: ExecFailure| -> anyhow::Error {
            ManagerError {
                outcome,
                session_id: session.map(str::to_owned),
                failure: ExecFailure {
                    detail: manager::redact_string(failure.detail, &secrets),
                    ..failure
                },
            }
            .into()
        };
        let failed =
            |session: Option<&str>, failure| error(ManagerFailure::Failed, session, failure);
        // Faults of this Node's own bookkeeping, not of opencode.
        let own = |detail: &str| ExecFailure::new(FailureKind::Unknown, detail);
        let unknown = |session: Option<&str>, detail: &str| {
            error(ManagerFailure::Unknown, session, own(detail))
        };
        let stopped = |session: Option<&str>| {
            error(
                ManagerFailure::Cancelled,
                session,
                own("stopped before the opencode run started"),
            )
        };
        if *cancel.borrow() {
            return Err(stopped(previous));
        }
        let model = parse_model(&config.model).map_err(|failure| failed(previous, failure))?;
        let conn = (self.conn().await).map_err(|e| failed(previous, launch_failure(&e)))?;
        let resumed = match previous {
            Some(id) => self.session(&conn, id).await.map_err(|e| {
                failed(
                    previous,
                    ExecFailure::new(FailureKind::Transport, format!("opencode: {e:#}")),
                )
            })?,
            None => None,
        };
        let (session, directory, prompt) = match (resumed, previous) {
            (Some(found), Some(id)) => {
                let directory = found["directory"].as_str().unwrap_or(workspace);
                (id.to_owned(), directory.to_owned(), prompt)
            }
            _ => {
                let id = (self.create(&conn, workspace).await).map_err(|f| failed(previous, f))?;
                // A session opencode no longer has is replaced by a new one, which needs it all.
                let fresh = previous
                    .and(config.codex_turn.as_ref())
                    .and_then(|turn| turn.fresh_prompt.as_deref());
                (id, workspace.to_owned(), fresh.unwrap_or(prompt))
            }
        };
        let native = Some(session.as_str());
        if let Err(e) = persist_session(session.clone()).await {
            // A stop fences the attempt, so saving its session fails then.
            return Err(if *cancel.borrow() {
                stopped(native)
            } else {
                failed(native, own(&format!("persist the opencode session: {e}")))
            });
        }
        let message = match &config.codex_turn {
            Some(turn) => {
                (turn.record)(session.clone(), None)
                    .await
                    .map_err(|e| failed(native, own(&format!("record the opencode turn: {e}"))))?;
                hashed("msg_", &format!("vibeface-turn:{}", turn.client_id), 24)
            }
            None => format!("msg_{}", crate::crypto::random_hex(12)),
        };
        let mut env = config.tool_env.clone();
        if let Some(token) = &config.tool_token {
            env.insert("VIBEFACE_TOOL_TOKEN".into(), token.clone());
        }
        let env_file = self.dir.join("env").join(format!("{session}.json"));
        config::private_write(&env_file, &serde_json::to_vec(&env)?)
            .map_err(|e| failed(native, own(&format!("write the tool environment: {e}"))))?;
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let (up, ready) = tokio::sync::oneshot::channel();
        let follow = follow_events(
            self.http.clone(),
            conn.clone(),
            directory.clone(),
            session.clone(),
            events_tx,
            up,
        );
        let _guard = TurnGuard {
            owner: self,
            session: session.clone(),
            env_file,
            events: tokio::spawn(follow),
        };
        // Progress is best effort: the turn goes on without the stream after a while.
        let _ = tokio::time::timeout(Duration::from_secs(5), ready).await;
        let instructions = manager::bot_instructions(
            config.tool_cli.as_deref(),
            &config.custom_tools,
            &config.instructions,
        );
        let mut body = json!({"agent":"build"});
        if !instructions.is_empty() {
            body["system"] = json!(instructions);
        }
        if let Some(model) = model {
            body["model"] = model;
        }
        let mut first = body.clone();
        first["messageID"] = json!(message);
        first["parts"] = json!([{"type":"text","text":prompt}]);
        let live = Live {
            turn: message.clone(),
            conn: conn.clone(),
            directory: directory.clone(),
            body,
            sent: vec![message.clone()],
            last_post_ms: now_ms(),
            open: true,
        };
        self.live
            .lock()
            .expect("live")
            .insert(session.clone(), live);
        if *cancel.borrow() {
            return Err(stopped(native));
        }
        match self.send(&conn, &session, &directory, &first).await {
            Sent::Taken => {}
            Sent::Refused(failure) => return Err(failed(native, failure)),
            Sent::Unknown(detail) => {
                if !matches!(self.message(&conn, &session, &message).await, Ok(Some(_))) {
                    return Err(unknown(native, &detail));
                }
            }
        }
        self.touch(&session);
        if let Some(turn) = &config.codex_turn
            && let Err(e) = (turn.record)(session.clone(), Some(message.clone())).await
        {
            let kind = if self.stop_run(&conn, &session, &directory, &message).await {
                ManagerFailure::Failed
            } else {
                ManagerFailure::Unknown
            };
            return Err(error(
                kind,
                native,
                own(&format!("record the opencode turn: {e}")),
            ));
        }
        let mut seen = Seen::default();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(config.timeout_secs);
        let mut ticks = tokio::time::interval(TICK);
        let mut streaming = true;
        let messages = loop {
            let check = tokio::select! {
                biased;
                _ = agent::wait_for_cancel(&mut cancel) => {
                    let last = self.close(&session);
                    break if self.stop_run(&conn, &session, &directory, &last).await {
                        Err(error(ManagerFailure::Cancelled, native, own("opencode run stopped")))
                    } else {
                        Err(unknown(native, "opencode did not confirm the stop"))
                    };
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let last = self.close(&session);
                    let kind = if self.stop_run(&conn, &session, &directory, &last).await {
                        ManagerFailure::Failed
                    } else {
                        ManagerFailure::Unknown
                    };
                    break Err(error(kind, native, ExecFailure::new(FailureKind::TimedOut, "opencode management timed out")));
                }
                event = events.recv(), if streaming => match event {
                    Some(event) => {
                        let event = manager::redact(event, &secrets);
                        if let Some(update) = seen.apply(&event) {
                            progress(update).await.map_err(|e| unknown(native, &e.to_string()))?;
                        }
                        event["type"] == "session.idle"
                    }
                    None => {
                        streaming = false;
                        false
                    }
                },
                _ = ticks.tick() => true,
            };
            if !check {
                continue;
            }
            if (self.running().await).is_none_or(|now| now.started_ms != conn.started_ms) {
                self.close(&session);
                let lost = ExecFailure::new(
                    FailureKind::Lost,
                    "the opencode serve stopped during the run",
                );
                break Err(failed(native, lost));
            }
            let errored = seen.error.is_some();
            if let Ok(Some(messages)) = self.settled(&conn, &session, &directory, errored).await {
                break Ok(messages);
            }
        }?;
        let sent = self.posted(&session).0;
        let chains: Vec<Chain> = sent.iter().map(|id| chain(&messages, id)).collect();
        // The run ends with the last message that shows.
        let Some(last) = chains.iter().rev().find(|chain| chain.visible) else {
            return Err(match seen.error.take() {
                Some(failure) => failed(native, failure),
                None => unknown(native, "opencode never showed the message it was sent"),
            });
        };
        let reply = match &last.reply {
            None => {
                let failure = seen.error.take().unwrap_or_else(|| {
                    own("opencode ended the run without an answer; check that the model exists and its provider is signed in")
                });
                return Err(failed(native, failure));
            }
            Some(reply) if reply["error"]["name"] == "MessageAbortedError" => {
                return Err(error(
                    ManagerFailure::Cancelled,
                    native,
                    own("the opencode run was aborted"),
                ));
            }
            Some(reply) if !reply["error"].is_null() => {
                return Err(failed(native, failure_of(&reply["error"])));
            }
            Some(reply) => reply,
        };
        let mut answered = self.answered.lock().expect("answered");
        if answered.len() > 1024 {
            answered.clear();
        }
        for (id, chain) in sent.iter().zip(&chains).skip(1) {
            if chain.terminal() && chain.reply.as_ref().is_some_and(|r| r["error"].is_null()) {
                answered.insert(id.clone());
            }
        }
        drop(answered);
        let text = chains
            .iter()
            .map(|chain| chain.text.as_str())
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let output = manager::redact_string(text, &secrets);
        let output = crate::scheduler::prefix(&output, MAX_OUTPUT).to_owned();
        // An empty model is opencode's choice, which the turn records.
        let model = format!(
            "{}/{}",
            reply["providerID"].as_str().unwrap_or("?"),
            reply["modelID"].as_str().unwrap_or("?")
        );
        tracing::info!(event = "agent.turn.model", session_id = %session, %model, "opencode answered");
        Ok(AgentResult {
            session_id: Some(session),
            output,
            usage: Some(json!({"model":model})),
        })
    }
}

/// Forwards the serve's events of `session` until the stream ends or the turn goes; `up` fires
/// with the first event, `server.connected`.
async fn follow_events(
    http: reqwest::Client,
    conn: Conn,
    directory: String,
    session: String,
    events: mpsc::UnboundedSender<Value>,
    up: tokio::sync::oneshot::Sender<()>,
) {
    let response = http
        .get(format!("{}/event", conn.base))
        .basic_auth("opencode", Some(&conn.password))
        .query(&[("directory", &directory)])
        .send()
        .await;
    let Ok(mut response) = response else {
        return;
    };
    if !response.status().is_success() {
        return;
    }
    let mut up = Some(up);
    let mut buffer = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        buffer.extend_from_slice(&chunk);
        while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            let Some(data) = line.strip_prefix(b"data:") else {
                continue;
            };
            let Ok(event) = serde_json::from_slice::<Value>(data.trim_ascii()) else {
                continue;
            };
            if let Some(up) = up.take() {
                let _ = up.send(());
            }
            if session_of(&event) == Some(session.as_str()) && events.send(event).is_err() {
                return;
            }
        }
        // ponytail: a line this long is no event the turn needs.
        if buffer.len() > 1 << 24 {
            return;
        }
    }
}

fn session_of(event: &Value) -> Option<&str> {
    let properties = &event["properties"];
    (properties["sessionID"].as_str())
        .or_else(|| properties["info"]["sessionID"].as_str())
        .or_else(|| properties["part"]["sessionID"].as_str())
}

/// What the event stream showed of a turn: its answers as neutral progress, and the first error
/// opencode reported.
#[derive(Default)]
struct Seen {
    replies: BTreeSet<String>,
    texts: BTreeSet<String>,
    tools: HashMap<String, &'static str>,
    error: Option<ExecFailure>,
}

impl Seen {
    /// The progress an event shows; the user's own messages, reasoning and tool details stay out.
    fn apply(&mut self, event: &Value) -> Option<Value> {
        let properties = &event["properties"];
        let part = &properties["part"];
        match event["type"].as_str()? {
            "message.updated" if properties["info"]["role"] == "assistant" => {
                self.replies
                    .insert(properties["info"]["id"].as_str()?.to_owned());
                None
            }
            "message.part.updated" if self.replies.contains(part["messageID"].as_str()?) => {
                match part["type"].as_str()? {
                    "text" if part["synthetic"] != true => {
                        let id = part["id"].as_str()?;
                        self.texts.insert(id.to_owned());
                        let text = part["text"].as_str()?;
                        (part["time"]["end"].is_number()).then(|| {
                            json!({"type":"text_snapshot","item_id":bounded(id, 128),"text":bounded(text, 8000)})
                        })
                    }
                    "tool" => {
                        let call = part["callID"].as_str()?;
                        let status = match part["state"]["status"].as_str()? {
                            "completed" | "error" => "completed",
                            _ => "running",
                        };
                        let name = tool_name(part["tool"].as_str().unwrap_or_default());
                        (self.tools.insert(call.to_owned(), status) != Some(status)).then(|| {
                            json!({"type":"tool","item_id":bounded(call, 128),"name":name,"status":status})
                        })
                    }
                    _ => None,
                }
            }
            "message.part.delta"
                if properties["field"] == "text"
                    && self.texts.contains(properties["partID"].as_str()?) =>
            {
                let id = properties["partID"].as_str()?;
                let delta = properties["delta"].as_str()?;
                Some(
                    json!({"type":"text_delta","item_id":bounded(id, 128),"text":bounded(delta, 8000)}),
                )
            }
            "session.error" if self.error.is_none() => {
                self.error = Some(failure_of(&properties["error"]));
                None
            }
            _ => None,
        }
    }
}

fn tool_name(tool: &str) -> &'static str {
    match tool {
        "bash" => "command",
        "edit" | "write" | "patch" | "multiedit" | "apply_patch" => "file change",
        "webfetch" | "websearch" => "web search",
        _ => "tool",
    }
}

/// At most `limit` characters, without control characters but line breaks and tabs.
fn bounded(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .take(limit)
        .collect()
}

/// One message we sent and what answered it.
struct Chain {
    visible: bool,
    created_ms: i64,
    /// The last assistant message answering it.
    reply: Option<Value>,
    /// The last text its answers gave.
    text: String,
}

impl Chain {
    fn terminal(&self) -> bool {
        (self.reply.as_ref())
            .is_some_and(|reply| !reply["time"]["completed"].is_null() || !reply["error"].is_null())
    }
}

/// Each message opencode answers gets its own assistant messages, one per step.
fn chain(messages: &[Value], id: &str) -> Chain {
    let user = messages.iter().find(|message| message["info"]["id"] == id);
    let replies: Vec<&Value> = messages
        .iter()
        .filter(|m| m["info"]["role"] == "assistant" && m["info"]["parentID"] == id)
        .collect();
    Chain {
        visible: user.is_some(),
        created_ms: (user.and_then(|message| message["info"]["time"]["created"].as_i64()))
            .unwrap_or_default(),
        reply: replies.last().map(|message| message["info"].clone()),
        text: (replies.iter().rev().map(|message| text_of(message)))
            .find(|text| !text.trim().is_empty())
            .unwrap_or_default(),
    }
}

/// The text an assistant message shows: its own text parts, not the ones opencode adds.
fn text_of(message: &Value) -> String {
    (message["parts"].as_array().into_iter().flatten())
        .filter(|part| part["type"] == "text" && part["synthetic"] != true)
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// What an error opencode recorded means.
fn failure_of(error: &Value) -> ExecFailure {
    let name = error["name"].as_str().unwrap_or("opencode error");
    let detail = (error["data"]["message"].as_str())
        .map_or_else(|| format!("opencode: {name}"), str::to_owned);
    match (name, error["data"]["statusCode"].as_u64()) {
        ("ProviderAuthError", _) | (_, Some(401 | 403)) => {
            ExecFailure::new(FailureKind::AuthFailed, detail)
        }
        (_, Some(429)) => ExecFailure::new(FailureKind::Quota, detail),
        (_, Some(500..=599)) => ExecFailure::new(FailureKind::Transport, detail),
        (_, Some(400..=499)) => ExecFailure::upstream(detail, FailureKind::Rejected),
        _ => ExecFailure::upstream(detail, FailureKind::Unknown),
    }
}

fn http_failure(status: u16, body: &Value) -> ExecFailure {
    let detail = body["data"]["message"]
        .as_str()
        .map_or_else(|| body.to_string(), str::to_owned);
    ExecFailure::upstream(
        format!("opencode refused ({status}): {detail}"),
        FailureKind::Rejected,
    )
}

fn launch_failure(error: &anyhow::Error) -> ExecFailure {
    let missing = (error.downcast_ref::<std::io::Error>())
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound);
    let kind = if missing {
        FailureKind::MissingBinary
    } else {
        FailureKind::Unknown
    };
    ExecFailure::new(kind, format!("opencode could not start: {error:#}"))
}

/// opencode names a model `provider/model`; empty or "auto" leaves the choice to it.
fn parse_model(model: &str) -> Result<Option<Value>, ExecFailure> {
    let model = model.trim();
    if model.is_empty() || model == "auto" {
        return Ok(None);
    }
    match model.split_once('/') {
        Some((provider, id)) if !provider.is_empty() && !id.is_empty() => {
            Ok(Some(json!({"providerID":provider,"modelID":id})))
        }
        _ => Err(ExecFailure::new(
            FailureKind::NoModel,
            format!("opencode names models provider/model, not {model:?}"),
        )),
    }
}

/// Whether a variable looks like it holds a key, by the words Codex's shell policy reads.
fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    ["KEY", "SECRET", "TOKEN", "PASSWORD"]
        .iter()
        .any(|word| name.contains(word))
}

/// An ID opencode takes, the same for the same seed.
fn hashed(prefix: &str, seed: &str, len: usize) -> String {
    format!("{prefix}{}", &crate::crypto::digest(seed.as_bytes())[..len])
}

/// The message a work is sent as: a restart reads the run back by it.
fn work_message(work_id: &str) -> String {
    hashed("msg_", &format!("vibeface-work:{work_id}"), 24)
}

fn steer_message(message_id: &str) -> String {
    hashed("msg_", &format!("vibeface-steer:{message_id}"), 24)
}

/// An image of a work as opencode takes it: an existing local file.
fn image_part(image: &Value) -> Result<Value, ExecFailure> {
    let refuse = |why: &str| {
        ExecFailure::new(
            FailureKind::Rejected,
            format!("opencode cannot take image {image}: {why}"),
        )
    };
    let path = Path::new(image.as_str().ok_or_else(|| refuse("not a path"))?);
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let mime = match extension.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => return Err(refuse("it takes png, jpeg, gif and webp files")),
    };
    if !(path.is_absolute() && path.is_file()) {
        return Err(refuse("not an existing absolute path"));
    }
    let url = reqwest::Url::from_file_path(path).map_err(|()| refuse("not a file path"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image");
    Ok(json!({"type":"file","mime":mime,"filename":name,"url":url.as_str()}))
}

/// Follows a started work until it ends. Probes only read, so nothing is ever sent again; the
/// reconcile follows the works of an earlier run.
async fn follow(node: std::sync::Weak<Runtime>, network: String, work_id: String) {
    loop {
        tokio::time::sleep(WORK_TICK).await;
        let Some(node) = node.upgrade() else {
            return;
        };
        let Ok(Some(record)) = node.store.lock().await.work_record(&work_id) else {
            return;
        };
        let starting = matches!(record.state, WorkState::Starting { .. });
        if !starting
            && !matches!(
                record.state,
                WorkState::Running { .. } | WorkState::Interrupting { .. }
            )
        {
            return;
        }
        let signal = match Executor::probe(node.opencode.as_ref(), &record).await {
            Ok(Probe::Running { turn_id, epoch }) if starting => {
                Signal::TurnStarted { turn_id, epoch }
            }
            Ok(probe @ Probe::Terminal { .. }) => Signal::Probed(probe),
            _ => continue,
        };
        if let Err(error) = node.signal_work(&network, &work_id, signal).await {
            tracing::debug!(%error, work_id, "the opencode work waits for its next probe");
        }
    }
}

/// opencode works run on the Node's serve: the Node creates the session, journals it and sends
/// the work itself under a message ID made from the work ID.
impl Executor for Opencode {
    fn start<'a>(
        &'a self,
        node: &'a Arc<Runtime>,
        network: &'a str,
        work: &'a WorkSpec,
        thread: Option<&'a str>,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let id = work.work_id.as_str();
            let reject = |failure: ExecFailure| async move {
                (node
                    .signal_work(network, id, Signal::StartRejected(failure))
                    .await)
                    .map(|_| false)
            };
            let mut parts = vec![json!({"type":"text","text":work.prompt})];
            for image in args["images"].as_array().into_iter().flatten() {
                match image_part(image) {
                    Ok(part) => parts.push(part),
                    Err(failure) => return reject(failure).await,
                }
            }
            // ponytail: opencode takes no effort per message; a model variant would carry it.
            let model = match parse_model(args["model"].as_str().unwrap_or_default()) {
                Ok(model) => model,
                Err(failure) => return reject(failure).await,
            };
            let conn = match self.conn().await {
                Ok(conn) => conn,
                Err(error) => return reject(launch_failure(&error)).await,
            };
            let (session, directory) = match thread {
                Some(session) => match self.session(&conn, session).await? {
                    Some(found) => {
                        let directory = found["directory"].as_str().unwrap_or(&work.cwd);
                        (session.to_owned(), directory.to_owned())
                    }
                    None => {
                        let failure = ExecFailure::new(
                            FailureKind::Rejected,
                            format!("opencode has no session {session}"),
                        );
                        return reject(failure).await;
                    }
                },
                None => {
                    let cwd = Path::new(&work.cwd);
                    if !(cwd.is_absolute() && cwd.is_dir()) {
                        let failure = ExecFailure::new(
                            FailureKind::Rejected,
                            "the work directory must be an existing absolute path",
                        );
                        return reject(failure).await;
                    }
                    match self.create(&conn, &work.cwd).await {
                        Ok(session) => (session, work.cwd.clone()),
                        Err(failure) => return reject(failure).await,
                    }
                }
            };
            // Journaled before the send: a restart reads the run back by the work's message.
            let signal = Signal::ThreadCreated(session.clone());
            let record = node.signal_work(network, id, signal).await?;
            if record.state
                != (WorkState::Starting {
                    thread: Some(session.clone()),
                })
            {
                return Ok(false);
            }
            let message = work_message(id);
            let mut body = json!({"messageID":message,"agent":"build","parts":parts});
            if let Some(model) = model {
                body["model"] = model;
            }
            match self.send(&conn, &session, &directory, &body).await {
                Sent::Taken => {
                    let epoch = conn.started_ms as u64;
                    let signal = Signal::TurnStarted {
                        turn_id: message,
                        epoch,
                    };
                    node.signal_work(network, id, signal).await?;
                }
                Sent::Refused(failure) => return reject(failure).await,
                Sent::Unknown(detail) => {
                    tracing::warn!(
                        work_id = id,
                        detail,
                        "unknown whether opencode took the work"
                    );
                }
            }
            let follower = follow(Arc::downgrade(node), network.to_owned(), id.to_owned());
            tokio::spawn(follower);
            Ok(false)
        })
    }

    fn probe<'a>(&'a self, work: &'a WorkRecord) -> BoxFuture<'a, Result<Probe>> {
        Box::pin(async move {
            let session = work.state.thread().context("the work has no session")?;
            let id = work_message(&work.work_id);
            let conn = self.conn().await?;
            let Some(found) = self.session(&conn, session).await? else {
                return Ok(Probe::Missing);
            };
            let Some(messages) = self.tail(&conn, session, &id).await? else {
                return Ok(Probe::NotStarted);
            };
            let directory = found["directory"]
                .as_str()
                .context("opencode sent a session without its directory")?;
            let epoch = conn.started_ms as u64;
            // A later message took the run over, so ours has ended.
            let later = messages.iter().skip(1).any(|m| m["info"]["role"] == "user");
            if !later && self.busy(&conn, session, directory).await? {
                return Ok(Probe::Running { turn_id: id, epoch });
            }
            let chain = chain(&messages, &id);
            let done = |status, error| Probe::Terminal {
                turn_id: id.clone(),
                status,
                text: chain.text.clone(),
                error,
            };
            // A run cut between steps leaves its last step finished for tools.
            let finished = |reply: &Value| {
                !reply["time"]["completed"].is_null()
                    && (later
                        || !matches!(reply["finish"].as_str(), Some("tool-calls" | "unknown")))
            };
            Ok(match &chain.reply {
                Some(reply) if reply["error"]["name"] == "MessageAbortedError" => {
                    done(TurnStatus::Interrupted, None)
                }
                Some(reply) if !reply["error"].is_null() => {
                    done(TurnStatus::Failed, Some(failure_of(&reply["error"])))
                }
                Some(reply) if finished(reply) => done(TurnStatus::Completed, None),
                _ if chain.created_ms < conn.started_ms => {
                    let failure = ExecFailure::new(
                        FailureKind::Restarted,
                        "the opencode serve running this work stopped",
                    );
                    done(TurnStatus::Interrupted, Some(failure))
                }
                _ if now_ms() - chain.created_ms < QUIET_MS => Probe::Running {
                    turn_id: id.clone(),
                    epoch,
                },
                _ => {
                    let failure = ExecFailure::new(
                        FailureKind::Unknown,
                        "opencode ended this run without an answer; check that the model exists and its provider is signed in",
                    );
                    done(TurnStatus::Failed, Some(failure))
                }
            })
        })
    }

    fn interrupt<'a>(&'a self, work: &'a WorkRecord) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let session = work.state.thread().context("the work has no session")?;
            let conn = self.conn().await?;
            let found = (self.session(&conn, session).await?)
                .context("opencode no longer has the session")?;
            let directory = found["directory"].as_str().unwrap_or_default();
            let (status, body) = self.abort(&conn, session, directory).await?;
            ensure!(
                (200..300).contains(&status),
                "opencode refused the stop ({status}): {body}"
            );
            Ok(())
        })
    }

    fn steer<'a>(
        &'a self,
        into: &'a SteerTarget,
        message_id: &'a str,
        text: &'a str,
    ) -> BoxFuture<'a, SteerAck> {
        Box::pin(async move {
            let id = steer_message(message_id);
            let joined = {
                let mut live = self.live.lock().expect("live");
                (live.get_mut(&into.thread_id))
                    .filter(|turn| turn.open && turn.turn == into.turn_id)
                    .map(|turn| {
                        turn.sent.push(id.clone());
                        turn.last_post_ms = now_ms();
                        (turn.conn.clone(), turn.directory.clone(), turn.body.clone())
                    })
            };
            let Some((conn, directory, mut body)) = joined else {
                return SteerAck::Refused("the reply already ended".into());
            };
            let message = json!({"message_id":message_id,"text":text});
            body["messageID"] = json!(id);
            body["parts"] = json!([{"type":"text","text":format!(
                "The user sent another message in this conversation while you were replying. Take it into account and answer it in this same reply.\n\n{message}"
            )}]);
            match self.send(&conn, &into.thread_id, &directory, &body).await {
                Sent::Taken => {
                    self.touch(&into.thread_id);
                    SteerAck::Accepted
                }
                Sent::Refused(failure) => {
                    if let Some(turn) = self.live.lock().expect("live").get_mut(&into.thread_id) {
                        turn.sent.retain(|sent| *sent != id);
                    }
                    SteerAck::Refused(failure.detail)
                }
                Sent::Unknown(_) => SteerAck::Unknown,
            }
        })
    }

    fn read_back<'a>(
        &'a self,
        into: &'a SteerTarget,
        message_id: &'a str,
    ) -> BoxFuture<'a, Option<SteerTrace>> {
        Box::pin(async move {
            let id = steer_message(message_id);
            if self.answered.lock().expect("answered").contains(&id) {
                return Some(SteerTrace::Ended(SteerOutcome::Answered));
            }
            let conn = self.conn().await.ok()?;
            if self.session(&conn, &into.thread_id).await.ok()?.is_none() {
                return Some(SteerTrace::TargetUnseen);
            }
            Some(
                match self.message(&conn, &into.thread_id, &id).await.ok()? {
                    None => SteerTrace::NotArrived,
                    Some(_) => SteerTrace::Ended(SteerOutcome::Unfinished),
                },
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::LocalIdentity;
    use crate::identity::{Genesis, Member, NetworkState};
    use crate::manager::{CodexTurn, Sdk, run_manager};
    use axum::extract::{Path as UrlPath, Query, Request, State};
    use axum::http::{StatusCode, header};
    use axum::response::sse::{Event, Sse};
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use base64::Engine as _;
    use tokio::sync::watch;

    type Turns = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-opencode")
    }

    /// opencode's HTTP API as the Node uses it. A message is answered 50 ms after it is sent with
    /// a command and "re:<text>"; "HOLD" starts a reply that goes on until it is aborted or the
    /// next message ends it with "first".
    #[derive(Default)]
    struct Fake {
        /// A new password is a new serve: the runs of the one before died with it.
        password: String,
        sessions: BTreeMap<String, FakeSession>,
        /// Every message sent: its session, body and the tool environment its shells get.
        prompts: Vec<Value>,
        aborts: usize,
        /// The reply "HOLD" started: its session and message.
        held: Option<(String, String)>,
        ids: usize,
    }

    #[derive(Default)]
    struct FakeSession {
        directory: String,
        /// The session's own permission rules.
        permission: Value,
        busy: bool,
        messages: Vec<Value>,
    }

    impl Fake {
        fn id(&mut self, prefix: &str) -> String {
            self.ids += 1;
            format!("{prefix}_{}", self.ids)
        }

        fn messages(&mut self, session: &str) -> &mut Vec<Value> {
            &mut self.sessions.get_mut(session).expect("session").messages
        }
    }

    #[derive(Clone)]
    struct Api {
        /// Where the fixture records how the serve was started.
        dir: PathBuf,
        fake: Arc<std::sync::Mutex<Fake>>,
        events: tokio::sync::broadcast::Sender<Value>,
    }

    impl Api {
        fn emit(&self, event: Value) {
            let _ = self.events.send(event);
        }

        /// Starts an assistant reply to `parent`.
        fn begin(&self, fake: &mut Fake, session: &str, parent: &str) -> String {
            let id = fake.id("msg");
            let info = json!({"id":id,"sessionID":session,"role":"assistant","parentID":parent,
                "providerID":"fake","modelID":"m","time":{"created":now_ms()}});
            self.emit(json!({"type":"message.updated","properties":{"info":info}}));
            fake.messages(session).push(json!({"info":info,"parts":[]}));
            id
        }

        /// Ends reply `id` with a command and `text`, streamed as opencode streams it.
        fn finish(&self, fake: &mut Fake, session: &str, id: &str, text: &str) {
            let (call, words) = (fake.id("call"), fake.id("prt"));
            let updated =
                |part: &Value| json!({"type":"message.part.updated","properties":{"part":part}});
            let mut tool = json!({"id":call,"sessionID":session,"messageID":id,"type":"tool",
                "callID":call,"tool":"bash","state":{"status":"running"}});
            self.emit(updated(&tool));
            tool["state"]["status"] = json!("completed");
            self.emit(updated(&tool));
            let mut part = json!({"id":words,"sessionID":session,"messageID":id,"type":"text",
                "text":"","time":{"start":now_ms()}});
            self.emit(updated(&part));
            self.emit(
                json!({"type":"message.part.delta","properties":{"sessionID":session,
                "messageID":id,"partID":words,"field":"text","delta":text}}),
            );
            part["text"] = json!(text);
            part["time"]["end"] = json!(now_ms());
            self.emit(updated(&part));
            let reply = (fake.messages(session).iter_mut())
                .find(|message| message["info"]["id"] == id)
                .expect("reply");
            reply["info"]["time"]["completed"] = json!(now_ms());
            reply["info"]["finish"] = json!("stop");
            reply["parts"] = json!([tool, part]);
            self.emit(json!({"type":"message.updated","properties":{"info":reply["info"]}}));
        }

        fn idle(&self, fake: &mut Fake, session: &str) {
            fake.sessions.get_mut(session).expect("session").busy = false;
            self.emit(json!({"type":"session.idle","properties":{"sessionID":session}}));
        }
    }

    /// Every request carries the serve's password as opencode's basic auth.
    async fn auth(
        State(api): State<Api>,
        request: Request,
        next: axum::middleware::Next,
    ) -> Response {
        let password = std::fs::read_to_string(api.dir.join("password")).unwrap_or_default();
        let expected =
            base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"));
        let given =
            (request.headers().get(header::AUTHORIZATION)).and_then(|value| value.to_str().ok());
        if password.is_empty() || given != Some(format!("Basic {expected}").as_str()) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        {
            let mut fake = api.fake.lock().unwrap();
            if fake.password != password {
                fake.password = password;
                fake.held = None;
                fake.sessions
                    .values_mut()
                    .for_each(|session| session.busy = false);
            }
        }
        next.run(request).await
    }

    fn missing() -> Response {
        let body = json!({"name":"NotFoundError","data":{"message":"not found"}});
        (StatusCode::NOT_FOUND, Json(body)).into_response()
    }

    async fn create(
        State(api): State<Api>,
        Query(query): Query<HashMap<String, String>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let directory = query.get("directory").cloned().unwrap_or_default();
        let mut fake = api.fake.lock().unwrap();
        let id = format!("ses_fake{}", fake.sessions.len() + 1);
        let session = FakeSession {
            directory,
            permission: body["permission"].clone(),
            ..Default::default()
        };
        let created =
            json!({"id":id,"directory":session.directory,"permission":session.permission});
        fake.sessions.insert(id, session);
        Json(created)
    }

    async fn status(
        State(api): State<Api>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        let fake = api.fake.lock().unwrap();
        let directory = query.get("directory").map(String::as_str);
        let busy = (fake.sessions.iter())
            .filter(|(_, session)| session.busy && Some(session.directory.as_str()) == directory)
            .map(|(id, _)| (id.clone(), json!({"type":"busy"})));
        Json(Value::Object(busy.collect()))
    }

    async fn session(State(api): State<Api>, UrlPath(id): UrlPath<String>) -> Response {
        let fake = api.fake.lock().unwrap();
        match fake.sessions.get(&id) {
            Some(found) => Json(json!({"id":id,"directory":found.directory})).into_response(),
            None => missing(),
        }
    }

    async fn messages(
        State(api): State<Api>,
        UrlPath(id): UrlPath<String>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Response {
        let fake = api.fake.lock().unwrap();
        let Some(found) = fake.sessions.get(&id) else {
            return missing();
        };
        let limit = (query.get("limit"))
            .and_then(|limit| limit.parse().ok())
            .unwrap_or(usize::MAX);
        let from = found.messages.len().saturating_sub(limit);
        Json(json!(found.messages[from..])).into_response()
    }

    async fn message(
        State(api): State<Api>,
        UrlPath((id, message)): UrlPath<(String, String)>,
    ) -> Response {
        let fake = api.fake.lock().unwrap();
        let found = (fake.sessions.get(&id)).and_then(|session| {
            (session.messages.iter()).find(|m| m["info"]["id"] == message.as_str())
        });
        found.map_or_else(missing, |found| Json(found.clone()).into_response())
    }

    async fn prompt(
        State(api): State<Api>,
        UrlPath(session): UrlPath<String>,
        Json(body): Json<Value>,
    ) -> StatusCode {
        // What the serve's plugin gives this session's tool shells.
        let env_dir = std::fs::read_to_string(api.dir.join("env-dir")).unwrap_or_default();
        let env = std::fs::read(Path::new(&env_dir).join(format!("{session}.json")))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null);
        let id = body["messageID"].as_str().unwrap_or_default().to_owned();
        let text = body["parts"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let mut fake = api.fake.lock().unwrap();
        fake.prompts
            .push(json!({"session":session,"body":body,"env":env}));
        let Some(found) = fake.sessions.get_mut(&session) else {
            return StatusCode::NOT_FOUND;
        };
        found
            .messages
            .push(json!({"info":{"id":id,"sessionID":session,"role":"user",
            "time":{"created":now_ms()}},"parts":body["parts"]}));
        found.busy = true;
        if text == "HOLD" {
            let reply = api.begin(&mut fake, &session, &id);
            fake.held = Some((session, reply));
            return StatusCode::NO_CONTENT;
        }
        drop(fake);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut fake = api.fake.lock().unwrap();
            if let Some((_, held)) = fake.held.take_if(|(held, _)| *held == session) {
                api.finish(&mut fake, &session, &held, "first");
            }
            let reply = api.begin(&mut fake, &session, &id);
            api.finish(&mut fake, &session, &reply, &format!("re:{text}"));
            api.idle(&mut fake, &session);
        });
        StatusCode::NO_CONTENT
    }

    async fn abort(State(api): State<Api>, UrlPath(session): UrlPath<String>) -> Json<Value> {
        let mut fake = api.fake.lock().unwrap();
        fake.aborts += 1;
        if let Some((_, held)) = fake.held.take_if(|(held, _)| *held == session) {
            let reply = (fake.messages(&session).iter_mut())
                .find(|message| message["info"]["id"] == held.as_str())
                .expect("reply");
            reply["info"]["error"] = json!({"name":"MessageAbortedError",
                "data":{"message":"The operation was aborted."}});
            reply["info"]["time"]["completed"] = json!(now_ms());
            let info = reply["info"].clone();
            api.emit(json!({"type":"message.updated","properties":{"info":info}}));
            api.idle(&mut fake, &session);
        }
        Json(json!(true))
    }

    async fn events(
        State(api): State<Api>,
    ) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
        let connected = json!({"type":"server.connected","properties":{}});
        let start = (Some(connected), api.events.subscribe());
        Sse::new(futures_util::stream::unfold(
            start,
            |(first, mut events)| async move {
                let event = match first {
                    Some(event) => event,
                    None => events.recv().await.ok()?,
                };
                Some((Ok(Event::default().data(event.to_string())), (None, events)))
            },
        ))
    }

    /// A scratch directory and the fake API the fixture names as its serve's address.
    struct Place {
        dir: PathBuf,
        port: u16,
        fake: Arc<std::sync::Mutex<Fake>>,
        api: tokio::task::JoinHandle<()>,
    }

    impl Place {
        async fn new() -> Result<Self> {
            let dir =
                std::env::temp_dir().join(format!("vibeface-opencode-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(dir.join("work"))?;
            let dir = dir.canonicalize()?;
            let fake = Arc::<std::sync::Mutex<Fake>>::default();
            let api = Api {
                dir: dir.clone(),
                fake: fake.clone(),
                events: tokio::sync::broadcast::channel(1024).0,
            };
            let app = Router::new()
                .route("/session", post(create))
                .route("/session/status", get(status))
                .route("/session/{id}", get(session))
                .route("/session/{id}/message", get(messages))
                .route("/session/{id}/message/{message}", get(message))
                .route("/session/{id}/prompt_async", post(prompt))
                .route("/session/{id}/abort", post(abort))
                .route("/event", get(events))
                .layer(axum::middleware::from_fn_with_state(api.clone(), auth))
                .with_state(api);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let port = listener.local_addr()?.port();
            let api = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Ok(Self {
                dir,
                port,
                fake,
                api,
            })
        }

        /// The fixture's variables: where it records and where the fake API listens.
        fn env(&self) -> BTreeMap<String, String> {
            BTreeMap::from([
                ("FAKE_OPENCODE_DIR".into(), self.dir.display().to_string()),
                ("FAKE_OPENCODE_PORT".into(), self.port.to_string()),
                ("FAKE_API_KEY".into(), "fixture-key".into()),
            ])
        }

        fn opencode(&self) -> Result<Arc<Opencode>> {
            let server = Opencode::open(&self.dir.join("node"), fixture(), self.env())?;
            Ok(Arc::new(server))
        }

        fn prompts(&self) -> Vec<Value> {
            self.fake.lock().unwrap().prompts.clone()
        }

        /// What the fixture recorded of the serves it stood in for.
        fn recorded(&self, name: &str) -> String {
            std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
        }
    }

    impl Drop for Place {
        fn drop(&mut self) {
            self.api.abort();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn bot(place: &Place, server: &Arc<Opencode>, client: &str, turns: &Turns) -> ManagerConfig {
        let turns = turns.clone();
        ManagerConfig {
            sdk: Sdk::Opencode {
                server: server.clone(),
            },
            workspace: place.dir.join("work"),
            model: "fake/m".into(),
            timeout_secs: 30,
            custom_tools: BTreeMap::new(),
            tool_cli: None,
            instructions: "Answer briefly.".into(),
            tool_env: BTreeMap::from([("VIBEFACE_NODE_URL".into(), "http://127.0.0.1:9".into())]),
            tool_token: Some("tool-token-fixture".into()),
            provider_env: BTreeMap::new(),
            process_started: None,
            codex_turn: Some(CodexTurn {
                client_id: client.into(),
                record: Arc::new(move |session, turn| {
                    turns.lock().unwrap().push((session, turn));
                    Box::pin(async { anyhow::Ok(()) })
                }),
                fresh_prompt: None,
            }),
        }
    }

    /// One bot turn: its result, the session saved with how many messages opencode had been
    /// sent by then, and the progress shown.
    async fn turn(
        place: &Place,
        config: &ManagerConfig,
        prompt: &str,
        previous: Option<&str>,
        cancel: watch::Receiver<bool>,
    ) -> (Result<AgentResult>, Option<(String, usize)>, Vec<Value>) {
        let saved = std::sync::Mutex::new(None);
        let shown = std::sync::Mutex::new(Vec::new());
        let result = run_manager(
            config,
            prompt,
            previous,
            |session| {
                *saved.lock().unwrap() = Some((session, place.prompts().len()));
                async { anyhow::Ok(()) }
            },
            cancel,
            |update| {
                shown.lock().unwrap().push(update);
                async { anyhow::Ok(()) }
            },
        )
        .await;
        let (saved, shown) = (saved.into_inner().unwrap(), shown.into_inner().unwrap());
        (result, saved, shown)
    }

    /// The session and message of the bot turn once opencode took it.
    async fn started(turns: &Turns) -> (String, String) {
        loop {
            if let Some((session, Some(turn))) = turns.lock().unwrap().last().cloned() {
                return (session, turn);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn spec(target: &str, place: &Place, prompt: &str) -> WorkSpec {
        WorkSpec {
            work_id: "w-1".into(),
            conversation: "c".into(),
            trigger: "message:m1".into(),
            target: target.into(),
            dispatcher: target.into(),
            call_ref: "w-1".into(),
            kind: WorkKind::Agent(Provider::Opencode),
            cwd: place.dir.join("work").display().to_string(),
            title: "t".into(),
            prompt: prompt.into(),
            continues: None,
            parent: None,
            thread: None,
        }
    }

    /// A Node in a network of its own whose opencode is the fixture, and that network.
    async fn node(place: &Place) -> Result<(Arc<Runtime>, String)> {
        let data = place.dir.join("node");
        config::private_dir(&data)?;
        let mut configuration = Config::local("127.0.0.1:0".parse()?, vec![], false);
        configuration.providers.execution = [Provider::Opencode].into();
        configuration.bins.insert(Provider::Opencode, fixture());
        configuration.save(&data)?;
        config::private_write(&data.join("data-epoch.json"), &config::data_epoch_marker())?;
        let identity = LocalIdentity::generate();
        config::private_write(&data.join("identity.json"), &serde_json::to_vec(&identity)?)?;
        let mut env = crate::providers::ProviderEnv::default();
        let variables = place
            .env()
            .into_iter()
            .map(|(name, value)| (name, Some(value)));
        env.apply(Provider::Opencode, variables.collect())?;
        env.save(&data)?;
        let node = Arc::new(Runtime::open(&data).await?);
        let state = NetworkState::new(Genesis::create(
            &LocalIdentity::generate(),
            Member::client(&LocalIdentity::generate(), "admin", true)?,
            Member::node(&node.identity, "node")?,
        )?)?;
        let network = state.genesis.network_id.clone();
        node.install_membership(state).await?;
        Ok((node, network))
    }

    /// Registers the work `w-1` and has the Node start it on opencode.
    async fn dispatch(node: &Arc<Runtime>, network: &str, spec: &WorkSpec) -> Result<()> {
        let at = now_ms();
        (node.store.lock().await).accept_work(&node.identity, network, spec.clone(), at)?;
        let args = json!({"model":"fake/m"});
        let sends = node
            .opencode
            .start(node, network, spec, None, &args)
            .await?;
        ensure!(!sends, "the Node sends an opencode work itself");
        Ok(())
    }

    async fn until(node: &Runtime, done: impl Fn(&WorkState) -> bool) -> Result<WorkRecord> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let record = node.store.lock().await.work_record("w-1")?;
            let record = record.context("the work is recorded")?;
            if done(&record.state) {
                return Ok(record);
            }
            ensure!(
                Instant::now() < deadline,
                "the work stayed {:?}",
                record.state
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn a_bot_turn_streams_its_reply_and_the_next_turn_goes_on_in_its_session() -> Result<()> {
        let place = Place::new().await?;
        let server = place.opencode()?;
        let turns = Turns::default();
        let (_, never) = watch::channel(false);
        let config = bot(&place, &server, "attempt-1", &turns);
        let (result, saved, shown) = turn(&place, &config, "hello", None, never.clone()).await;
        let result = result?;
        let session = result.session_id.clone().context("session")?;
        assert_eq!(
            saved,
            Some((session.clone(), 0)),
            "the session is saved before the send"
        );
        assert_eq!(result.output, "re:hello");
        assert_eq!(result.usage, Some(json!({"model":"fake/m"})));
        assert_eq!(
            shown,
            [
                json!({"type":"tool","item_id":"call_2","name":"command","status":"running"}),
                json!({"type":"tool","item_id":"call_2","name":"command","status":"completed"}),
                json!({"type":"text_delta","item_id":"prt_3","text":"re:hello"}),
                json!({"type":"text_snapshot","item_id":"prt_3","text":"re:hello"}),
            ]
        );
        let message = hashed("msg_", "vibeface-turn:attempt-1", 24);
        assert_eq!(
            *turns.lock().unwrap(),
            [
                (session.clone(), None),
                (session.clone(), Some(message.clone()))
            ]
        );
        assert_eq!(
            place.fake.lock().unwrap().sessions[&session].permission,
            json!([{"permission":"*","pattern":"*","action":"allow"},
                {"permission":"question","pattern":"*","action":"deny"}]),
            "the session's own rules win over the user's opencode permission"
        );
        let sent = &place.prompts()[0];
        assert_eq!(sent["session"], session.as_str());
        assert_eq!(
            sent["body"],
            json!({"agent":"build","system":"Answer briefly.",
                "model":{"providerID":"fake","modelID":"m"},"messageID":message,
                "parts":[{"type":"text","text":"hello"}]})
        );
        assert_eq!(
            sent["env"],
            json!({"VIBEFACE_NODE_URL":"http://127.0.0.1:9",
                "VIBEFACE_TOOL_TOKEN":"tool-token-fixture"})
        );
        let env_dir = PathBuf::from(place.recorded("env-dir"));
        assert!(
            !env_dir.join(format!("{session}.json")).exists(),
            "the tool environment ends with the turn"
        );
        let blank: Value = serde_json::from_slice(&std::fs::read(env_dir.join("default.json"))?)?;
        assert_eq!(blank["FAKE_API_KEY"], "");
        assert_eq!(blank["OPENCODE_SERVER_PASSWORD"], "");
        assert!(
            place
                .recorded("argv")
                .starts_with("serve --hostname 127.0.0.1 --port ")
        );
        let settings: Value = serde_json::from_str(&place.recorded("config.json"))?;
        assert_eq!(
            settings["permission"],
            json!({"*":"allow","question":"deny"})
        );
        let plugin = reqwest::Url::parse(settings["plugin"][0].as_str().context("plugin")?)?;
        let plugin = (plugin.to_file_path()).map_err(|()| anyhow::anyhow!("a plugin file"))?;
        assert!(std::fs::read_to_string(plugin)?.contains("const HOOK = null;"));

        let config = bot(&place, &server, "attempt-2", &turns);
        let (next, saved, _) = turn(&place, &config, "more", Some(&session), never).await;
        assert_eq!(next?.output, "re:more");
        assert_eq!(saved.map(|(saved, _)| saved), Some(session.clone()));
        assert_eq!(place.prompts()[1]["session"], session.as_str());
        assert_eq!(
            place.recorded("argv").lines().count(),
            1,
            "one serve runs both turns"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_message_steered_into_the_running_reply_is_answered_in_it() -> Result<()> {
        let place = Place::new().await?;
        let server = place.opencode()?;
        let turns = Turns::default();
        let config = bot(&place, &server, "attempt-1", &turns);
        let (_, never) = watch::channel(false);
        let steer = async {
            let (thread_id, turn_id) = started(&turns).await;
            let into = SteerTarget {
                provider: Provider::Opencode,
                attempt_id: "attempt-1".into(),
                thread_id,
                turn_id,
            };
            assert_eq!(
                server.steer(&into, "m-1", "also this").await,
                SteerAck::Accepted
            );
            into
        };
        let ((reply, _, _), into) = tokio::join!(turn(&place, &config, "HOLD", None, never), steer);
        let output = reply?.output;
        assert!(
            output.starts_with("first\n\nre:") && output.contains("also this"),
            "{output}"
        );
        assert_eq!(
            server.read_back(&into, "m-1").await,
            Some(SteerTrace::Ended(SteerOutcome::Answered))
        );
        assert_eq!(
            server.read_back(&into, "m-2").await,
            Some(SteerTrace::NotArrived)
        );
        assert_eq!(
            server.steer(&into, "m-3", "late").await,
            SteerAck::Refused("the reply already ended".into())
        );
        Ok(())
    }

    #[tokio::test]
    async fn stopping_a_bot_turn_aborts_its_opencode_run() -> Result<()> {
        let place = Place::new().await?;
        let server = place.opencode()?;
        let turns = Turns::default();
        let config = bot(&place, &server, "attempt-1", &turns);
        let (stop, cancel) = watch::channel(false);
        let stopping = async {
            started(&turns).await;
            stop.send(true).unwrap();
        };
        let ((result, _, _), ()) =
            tokio::join!(turn(&place, &config, "HOLD", None, cancel), stopping);
        let Err(error) = result else {
            bail!("a stopped turn has no reply");
        };
        let error = error
            .downcast_ref::<ManagerError>()
            .context("a manager error")?;
        assert!(error.cancelled(), "{error}");
        assert!(place.fake.lock().unwrap().aborts >= 1);
        Ok(())
    }

    #[tokio::test]
    async fn a_dispatched_opencode_work_runs_in_its_own_session_and_ends_with_its_reply()
    -> Result<()> {
        let place = Place::new().await?;
        let (node, network) = node(&place).await?;
        let spec = spec(&node.identity.device_id()?, &place, "do it");
        dispatch(&node, &network, &spec).await?;
        let record = until(&node, WorkState::finished).await?;
        assert_eq!(
            record.state,
            WorkState::Finished {
                outcome: WorkOutcome::Completed
            }
        );
        assert_eq!(record.text, "re:do it");
        let prompts = place.prompts();
        assert_eq!(prompts.len(), 1);
        let session = prompts[0]["session"].as_str().context("session")?;
        let rules = place.fake.lock().unwrap().sessions[session]
            .permission
            .clone();
        assert_eq!(
            rules[0],
            json!({"permission":"*","pattern":"*","action":"allow"})
        );
        assert_eq!(
            prompts[0]["body"]["messageID"],
            work_message("w-1").as_str()
        );
        assert_eq!(
            prompts[0]["body"]["model"],
            json!({"providerID":"fake","modelID":"m"})
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_opencode_work_is_read_back_after_a_restart_and_never_sent_again() -> Result<()> {
        let place = Place::new().await?;
        let (node, network) = node(&place).await?;
        let spec = spec(&node.identity.device_id()?, &place, "HOLD");
        dispatch(&node, &network, &spec).await?;
        until(&node, |state| matches!(state, WorkState::Running { .. })).await?;
        drop(node);
        let node = Arc::new(Runtime::reopen(&place.dir.join("node")).await?);
        node.probe_work(&network, "w-1", true).await?;
        let record = until(&node, WorkState::finished).await?;
        assert_eq!(
            record.state,
            WorkState::Finished {
                outcome: WorkOutcome::Interrupted
            }
        );
        assert_eq!(place.prompts().len(), 1, "the work was sent once");
        assert_eq!(
            place.recorded("argv").lines().count(),
            2,
            "the restart started another serve"
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_opencode_session_without_the_work_reads_as_not_started() -> Result<()> {
        let place = Place::new().await?;
        let server = place.opencode()?;
        let (accepted, _) = crate::work::accept(spec("t", &place, "do it"), 0);
        let on = |session: &str| {
            let mut work = accepted.clone();
            crate::work::transition(&mut work, Signal::ThreadCreated(session.into()));
            work
        };
        assert_eq!(server.probe(&on("ses_gone")).await?, Probe::Missing);
        let conn = server.conn().await?;
        let cwd = place.dir.join("work").display().to_string();
        let created = server.create(&conn, &cwd).await;
        let session = created.map_err(|failure| anyhow::anyhow!(failure.detail))?;
        assert_eq!(server.probe(&on(&session)).await?, Probe::NotStarted);
        assert!(place.prompts().is_empty());
        Ok(())
    }

    #[test]
    fn opencode_errors_and_model_names_read_as_failures() {
        let kind = |error: Value| failure_of(&error).kind;
        let auth = json!({"name":"ProviderAuthError","data":{"message":"sign in"}});
        assert_eq!(kind(auth), FailureKind::AuthFailed);
        let quota = json!({"name":"APIError","data":{"message":"slow","statusCode":429}});
        assert_eq!(kind(quota), FailureKind::Quota);
        let busy = json!({"name":"APIError","data":{"message":"busy","statusCode":503}});
        assert_eq!(kind(busy), FailureKind::Transport);
        let other = json!({"name":"UnknownError","data":{"message":"boom"}});
        assert_eq!(kind(other), FailureKind::Unknown);
        assert_eq!(parse_model(" auto "), Ok(None));
        assert_eq!(
            parse_model("deepseek/deepseek-flash"),
            Ok(Some(
                json!({"providerID":"deepseek","modelID":"deepseek-flash"})
            ))
        );
        assert_eq!(
            parse_model("flash").map_err(|failure| failure.kind),
            Err(FailureKind::NoModel)
        );
    }

    #[test]
    fn the_installed_opencode_plugin_watches_nothing_in_the_nodes_serve() {
        let Some(crate::providers::Watch::Extension { source, .. }) = Provider::Opencode.watch()
        else {
            panic!("opencode is watched through its plugin");
        };
        assert!(source.contains(&format!("const ENV_DIR = process.env.{ENV_MARKER};")));
        assert!(source.contains("if (ENV_DIR) return HOOK ? {} : { \"shell.env\": shellEnv };"));
    }
}
