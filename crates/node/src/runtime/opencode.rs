//! opencode on this device: one `opencode serve` on loopback that the Node starts when a bot turn
//! or a work first needs it, and starts again when it died or its program or variables changed.
//! Sessions live in opencode's own store, so a work is read back from there after a restart and
//! never sent again. v2 keeps a shell environment per session; each turn replaces it before
//! prompting, blanks model keys and injects only that turn's Node tool credentials.
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
/// The user's opencode hook plugin.
pub(crate) const PLUGIN: &str = include_str!("../../extensions/opencode.ts");
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
    /// Uses of the serve in progress; while any lasts, a changed launch waits.
    users: Arc<std::sync::atomic::AtomicUsize>,
    /// Sessions a work is being sent into now.
    starting: std::sync::Mutex<BTreeSet<String>>,
    /// Works whose start runs in this process and has yet to claim its session.
    sending: std::sync::Mutex<BTreeSet<String>>,
    cancelled_inputs: std::sync::Mutex<BTreeSet<String>>,
}

/// One entry of a claim set, released when dropped.
struct Claim<'a>(&'a std::sync::Mutex<BTreeSet<String>>, String);

impl<'a> Claim<'a> {
    fn take(set: &'a std::sync::Mutex<BTreeSet<String>>, key: &str) -> Option<Self> {
        let mut held = set
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.insert(key.to_owned())
            .then(|| Self(set, key.to_owned()))
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        (self.0.lock())
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.1);
    }
}

/// One use of the serve, from taking its connection until the turn or call is over.
struct Use(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for Use {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
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
        tracing::info!(
            event = "opencode.leftover_stopped",
            group,
            "stopping the opencode serve left by an earlier run"
        );
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
    events: tokio::task::JoinHandle<()>,
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        self.events.abort();
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
            users: Default::default(),
            starting: Default::default(),
            sending: Default::default(),
            cancelled_inputs: Default::default(),
        })
    }

    /// The program and variables of the next serve; one running with others is replaced when
    /// next used while nothing else uses it.
    pub(crate) fn set_launch(&self, bin: PathBuf, env: BTreeMap<String, String>) {
        *self.launch.lock().expect("launch") = Launch { bin, env };
    }

    /// Stops the serve; the Node calls this when it shuts down.
    pub(crate) async fn stop(&self) {
        if let Some(serve) = self.serve.lock().await.take() {
            serve.stop().await;
        }
    }

    /// The running serve and a use of it. A serve is started first when there is none or it
    /// died, and also when its launch changed and nothing else uses it: runs on it go on.
    async fn conn(&self) -> Result<(Conn, Use)> {
        use std::sync::atomic::Ordering::SeqCst;
        let launch = self.launch.lock().expect("launch").clone();
        let mut serve = self.serve.lock().await;
        if let Some(running) = serve.as_mut()
            && (running.launch == launch || self.users.load(SeqCst) > 0)
            && running
                .child
                .try_wait()
                .is_ok_and(|status| status.is_none())
        {
            self.users.fetch_add(1, SeqCst);
            return Ok((running.conn.clone(), Use(self.users.clone())));
        }
        if let Some(old) = serve.take() {
            old.stop().await;
        }
        let started = self.spawn(launch).await?;
        self.users.fetch_add(1, SeqCst);
        Ok((serve.insert(started).conn.clone(), Use(self.users.clone())))
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
        let password = crate::crypto::random_hex(32);
        crate::logging::add_secret(&password);
        // ponytail: the port may be taken before the serve binds it; the next use starts again.
        let port = std::net::TcpListener::bind("127.0.0.1:0")?
            .local_addr()?
            .port();
        let permission = json!({"*":"allow","question":"deny"});
        let settings = json!({"$schema":"https://opencode.ai/config.json","autoupdate":false,
            "share":"disabled","permission":permission,"agent":{"build":{"permission":permission}}});
        let mut command = std::process::Command::new(&launch.bin);
        command
            .envs(&launch.env)
            .env("OPENCODE_SERVER_USERNAME", "opencode")
            .env("OPENCODE_SERVER_PASSWORD", &password)
            .env("OPENCODE_CONFIG_CONTENT", settings.to_string())
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
            .call(conn, Method::GET, &format!("/api/session/{id}"), &[], None)
            .await?
        {
            (200, envelope) => envelope
                .get("data")
                .cloned()
                .filter(|data| !data.is_null())
                .context("opencode sent a session response without data")
                .map(Some),
            (404, _) => Ok(None),
            (status, body) => bail!("opencode answered {status} reading session {id}: {body}"),
        }
    }

    async fn message(&self, conn: &Conn, session: &str, id: &str) -> Result<Option<Value>> {
        let path = format!("/api/session/{session}/message/{id}");
        match self.call(conn, Method::GET, &path, &[], None).await? {
            (200, envelope) => envelope
                .get("data")
                .cloned()
                .filter(|data| !data.is_null())
                .context("opencode sent a message response without data")
                .map(Some),
            (404, _) => Ok(None),
            (status, body) => bail!("opencode answered {status} reading message {id}: {body}"),
        }
    }

    /// Whether a run goes on in the session, as v2's active-session map reports it.
    async fn busy(&self, conn: &Conn, session: &str) -> Result<bool> {
        let (status, body) = self
            .call(conn, Method::GET, "/api/session/active", &[], None)
            .await?;
        ensure!(
            status == 200,
            "opencode answered {status} reading active sessions"
        );
        Ok(body["data"]
            .as_object()
            .context("opencode sent no active session map")?
            .contains_key(session))
    }

    /// The session's messages from `first` on, oldest first; `None` when it has no `first`.
    async fn tail(&self, conn: &Conn, session: &str, first: &str) -> Result<Option<Vec<Value>>> {
        let path = format!("/api/session/{session}/message");
        for limit in [64, 512, 4096] {
            let query = [("limit", limit.to_string()), ("order", "desc".to_owned())];
            let query = query.each_ref().map(|(key, value)| (*key, value.as_str()));
            let (status, body) = self.call(conn, Method::GET, &path, &query, None).await?;
            ensure!(
                status == 200,
                "opencode answered {status} reading session {session}"
            );
            let mut messages = body
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .context("opencode sent no message list")?;
            messages.reverse();
            if let Some(at) = messages.iter().position(|m| m["id"] == first) {
                return Ok(Some(messages.split_off(at)));
            }
            if messages.len() < limit as usize {
                return Ok(None);
            }
        }
        // ponytail: a message 4096 back reads as gone; turns and works read recent ones.
        Ok(None)
    }

    async fn inbox(&self, conn: &Conn, session: &str) -> Result<Vec<Value>> {
        let (status, body) = self
            .call(
                conn,
                Method::GET,
                &format!("/api/session/{session}/inbox"),
                &[],
                None,
            )
            .await?;
        ensure!(
            status == 200,
            "opencode answered {status} reading session inbox"
        );
        Ok(body["data"]
            .as_array()
            .context("opencode sent no inbox list")?
            .to_owned())
    }

    async fn inbox_contains(&self, conn: &Conn, session: &str, id: &str) -> Result<bool> {
        Ok(self
            .inbox(conn, session)
            .await?
            .iter()
            .any(|item| item["id"] == id))
    }

    async fn cancel_inbox_message(&self, conn: &Conn, session: &str, id: &str) -> Result<bool> {
        if !self.inbox_contains(conn, session, id).await? {
            return Ok(false);
        }
        let (status, body) = self
            .call(
                conn,
                Method::DELETE,
                &format!("/api/session/{session}/inbox/{id}"),
                &[],
                None,
            )
            .await?;
        ensure!(
            status == 204,
            "opencode refused the inbox cancellation ({status}): {body}"
        );
        Ok(!self.inbox_contains(conn, session, id).await?
            && self.message(conn, session, id).await?.is_none())
    }

    async fn received(&self, conn: &Conn, session: &str, id: &str) -> Result<bool> {
        if self.message(conn, session, id).await?.is_some() {
            return Ok(true);
        }
        self.inbox_contains(conn, session, id).await
    }

    async fn switch_model(&self, conn: &Conn, session: &str, model: &Value) -> Result<()> {
        let (status, body) = self
            .call(
                conn,
                Method::POST,
                &format!("/api/session/{session}/model"),
                &[],
                Some(&json!({"model":model})),
            )
            .await?;
        ensure!(
            status == 204,
            "opencode refused the model switch ({status}): {body}"
        );
        Ok(())
    }

    /// v2 replaces the complete shell environment, so no key from the serve reaches a tool.
    async fn environment(
        &self,
        conn: &Conn,
        session: &str,
        tool_env: &BTreeMap<String, String>,
    ) -> Result<()> {
        let launch = self.launch.lock().expect("launch").clone();
        let mut variables: BTreeMap<String, String> = std::env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
            .chain(launch.env)
            .map(|(name, value)| {
                let value = if secret_name(&name)
                    || matches!(
                        name.as_str(),
                        "OPENCODE_SERVER_USERNAME" | "OPENCODE_CONFIG_CONTENT"
                    ) {
                    String::new()
                } else {
                    value
                };
                (name, value)
            })
            .collect();
        variables.insert("OPENCODE_SERVER_PASSWORD".into(), String::new());
        variables.extend(tool_env.clone());
        let (status, body) = self
            .call(
                conn,
                Method::PUT,
                &format!("/api/session/{session}/environment"),
                &[],
                Some(&json!({"variables":variables})),
            )
            .await?;
        ensure!(
            status == 204,
            "opencode refused the tool environment ({status}): {body}"
        );
        Ok(())
    }

    /// Creates a session in `directory`; its ID. The session's own rules allow every tool but
    /// questions, which nobody answers: they come after the agent's and the user's configured
    /// rules, and the last that matches wins. A subagent's session does not take them.
    async fn create(
        &self,
        conn: &Conn,
        directory: &str,
        model: Option<&Value>,
    ) -> Result<String, ExecFailure> {
        let mut body = json!({
            "agent":"build",
            "location":{"directory":directory},
            "permissions":[
                {"action":"*","resource":"*","effect":"allow"},
                {"action":"question","resource":"*","effect":"deny"}
            ]
        });
        if let Some(model) = model {
            body["model"] = model.clone();
        }
        match self
            .call(conn, Method::POST, "/api/session", &[], Some(&body))
            .await
        {
            Ok((200..=299, envelope)) => match envelope["data"]["id"].as_str() {
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
    async fn send(&self, conn: &Conn, session: &str, body: &Value) -> Sent {
        let path = format!("/api/session/{session}/prompt");
        match self.call(conn, Method::POST, &path, &[], Some(body)).await {
            Ok((200, response))
                if response["data"]["id"] == body["id"]
                    && response["data"]["sessionID"] == session =>
            {
                Sent::Taken
            }
            Ok((status @ 400..=499, body)) => Sent::Refused(http_failure(status, &body)),
            Ok((status, body)) => Sent::Unknown(format!("opencode answered {status}: {body}")),
            Err(error) => Sent::Unknown(format!("opencode: {error:#}")),
        }
    }

    async fn abort(&self, conn: &Conn, session: &str) -> Result<(u16, Value)> {
        let path = format!("/api/session/{session}/interrupt");
        let query = [("resume", "false")];
        self.call(conn, Method::POST, &path, &query, None).await
    }

    /// An interrupt leaves queued input intact, so remove this turn's queued messages first.
    async fn stop_run(&self, conn: &Conn, session: &str, last: &str) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        let (mut sent, _) = self.posted(session);
        if !sent.iter().any(|id| id == last) {
            sent.push(last.to_owned());
        }
        let mut cancelled = BTreeSet::new();
        loop {
            for id in &sent {
                if matches!(self.cancel_inbox_message(conn, session, id).await, Ok(true)) {
                    cancelled.insert(id.clone());
                }
            }
            let _ = self.abort(conn, session).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let mut ended = matches!(self.busy(conn, session).await, Ok(false));
            for id in &sent {
                ended &= cancelled.contains(id)
                    || matches!(self.message(conn, session, id).await, Ok(Some(_)));
            }
            if ended {
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
        errored: bool,
    ) -> Result<Option<Vec<Value>>> {
        let (sent, last_post) = self.posted(session);
        let Some(first) = sent.first() else {
            return Ok(None);
        };
        if self.busy(conn, session).await? {
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

    /// Whether no run or queued input remains in a reply's session; a running turn is aborted. Runs
    /// end with their serve, and the Node stops the serve of an earlier run when it opens.
    pub(crate) async fn run_ended(&self, session: &str) -> bool {
        let Some(conn) = self.running().await else {
            return true;
        };
        match self.session(&conn, session).await {
            Ok(Some(_)) => {}
            Ok(None) => return true,
            Err(_) => return false,
        }
        let pending = match self.inbox(&conn, session).await {
            Ok(items) => !items.is_empty(),
            Err(_) => return false,
        };
        match self.busy(&conn, session).await {
            Ok(false) => !pending,
            Ok(true) => {
                let _ = self.abort(&conn, session).await;
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
        let start = std::sync::Mutex::new(manager::StartEvidence::NotStarted);
        let error = |outcome, session: Option<&str>, failure: ExecFailure| -> anyhow::Error {
            ManagerError {
                outcome,
                start: *start
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
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
        let (conn, _serving) =
            (self.conn().await).map_err(|e| failed(previous, launch_failure(&e)))?;
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
                let busy = self
                    .busy(&conn, id)
                    .await
                    .map_err(|e| failed(previous, own(&format!("read opencode status: {e}"))))?;
                let pending = self
                    .inbox(&conn, id)
                    .await
                    .map_err(|e| failed(previous, own(&format!("read opencode inbox: {e}"))))?;
                if busy || !pending.is_empty() {
                    return Err(failed(
                        previous,
                        ExecFailure::new(
                            FailureKind::WriterBusy,
                            "opencode still has a run or pending input in this session",
                        ),
                    ));
                }
                let directory = found["location"]["directory"].as_str().unwrap_or(workspace);
                if let Some(model) = model.as_ref()
                    && found["model"] != *model
                {
                    self.switch_model(&conn, id, model).await.map_err(|e| {
                        failed(
                            previous,
                            ExecFailure::new(FailureKind::Transport, format!("opencode: {e:#}")),
                        )
                    })?;
                }
                (id.to_owned(), directory.to_owned(), prompt)
            }
            _ => {
                let id = (self.create(&conn, workspace, model.as_ref()).await)
                    .map_err(|f| failed(previous, f))?;
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
        self.environment(&conn, &session, &env)
            .await
            .map_err(|e| failed(native, own(&format!("set the tool environment: {e}"))))?;
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let (up, ready) = tokio::sync::oneshot::channel();
        let follow = follow_events(
            self.http.clone(),
            conn.clone(),
            session.clone(),
            events_tx,
            up,
        );
        let _guard = TurnGuard {
            owner: self,
            session: session.clone(),
            events: tokio::spawn(follow),
        };
        // Progress is best effort: the turn goes on without the stream after a while.
        let _ = tokio::time::timeout(Duration::from_secs(5), ready).await;
        let instructions = manager::bot_instructions(
            config.tool_cli.as_deref(),
            &config.custom_tools,
            &config.instructions,
        );
        let body = json!({"id":message,"text":if instructions.is_empty() {
            prompt.to_owned()
        } else {
            format!("{instructions}\n\n{prompt}")
        }});
        let first = body.clone();
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
        *start.lock().unwrap() = manager::StartEvidence::Unknown;
        match self.send(&conn, &session, &first).await {
            Sent::Taken => {}
            Sent::Refused(failure) => {
                *start.lock().unwrap() = manager::StartEvidence::NotStarted;
                return Err(failed(native, failure));
            }
            Sent::Unknown(detail) => match self.received(&conn, &session, &message).await {
                Ok(true) => {}
                Ok(false) | Err(_) => return Err(unknown(native, &detail)),
            },
        }
        *start.lock().unwrap() = manager::StartEvidence::Started;
        self.touch(&session);
        if let Some(turn) = &config.codex_turn
            && let Err(e) = (turn.record)(session.clone(), Some(message.clone())).await
        {
            let kind = if self.stop_run(&conn, &session, &message).await {
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
                    break if self.stop_run(&conn, &session, &last).await {
                        Err(error(ManagerFailure::Cancelled, native, own("opencode run stopped")))
                    } else {
                        Err(unknown(native, "opencode did not confirm the stop"))
                    };
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let last = self.close(&session);
                    let kind = if self.stop_run(&conn, &session, &last).await {
                        ManagerFailure::Failed
                    } else {
                        ManagerFailure::Unknown
                    };
                    break Err(error(kind, native, ExecFailure::new(FailureKind::TimedOut, "opencode management timed out")));
                }
                event = events.recv(), if streaming => match event {
                    Some(event) => {
                        let event = manager::redact(event, &secrets);
                        if let Some(update) = seen.apply(&event)
                            && let Err(e) = progress(update).await
                        {
                            // Not knowing what was shown, the run must not go on unseen.
                            let last = self.close(&session);
                            self.stop_run(&conn, &session, &last).await;
                            break Err(unknown(native, &e.to_string()));
                        }
                        matches!(
                            event["type"].as_str(),
                            Some(
                                "session.idle"
                                    | "session.execution.succeeded"
                                    | "session.execution.failed"
                                    | "session.execution.interrupted"
                            )
                        )
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
            if let Ok(Some(messages)) = self.settled(&conn, &session, errored).await {
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
        if let Some(failure) = seen.error.take() {
            return Err(failed(native, failure));
        }
        let reply = match &last.reply {
            None => {
                let failure = seen.error.take().unwrap_or_else(|| {
                    own("opencode ended the run without an answer; check that the model exists and its provider is signed in")
                });
                return Err(failed(native, failure));
            }
            Some(reply) if reply["error"]["type"] == "aborted" => {
                let (outcome, failure) = manager::interrupted(&cancel, Provider::Opencode);
                return Err(error(outcome, native, failure));
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
            reply["model"]["providerID"].as_str().unwrap_or("?"),
            reply["model"]["id"].as_str().unwrap_or("?")
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
    session: String,
    events: mpsc::UnboundedSender<Value>,
    up: tokio::sync::oneshot::Sender<()>,
) {
    let response = http
        .get(format!("{}/api/event", conn.base))
        .basic_auth("opencode", Some(&conn.password))
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
    let mut data = Vec::new();
    let mut dispatch = |bytes: &[u8]| {
        if bytes.is_empty() {
            return;
        }
        let Ok(event) = serde_json::from_slice::<Value>(bytes) else {
            return;
        };
        if let Some(up) = up.take() {
            let _ = up.send(());
        }
        if session_of(&event) == Some(session.as_str()) {
            let _ = events.send(event);
        }
    };
    while let Ok(Some(chunk)) = response.chunk().await {
        buffer.extend_from_slice(&chunk);
        while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            let line = line.strip_suffix(b"\n").unwrap_or(&line);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                dispatch(&data);
                data.clear();
            } else if let Some(value) = line.strip_prefix(b"data:") {
                let value = value.strip_prefix(b" ").unwrap_or(value);
                data.extend_from_slice(value);
            }
        }
        // ponytail: a line this long is no event the turn needs.
        if buffer.len() > 1 << 24 {
            return;
        }
    }
    dispatch(&data);
}

fn session_of(event: &Value) -> Option<&str> {
    event["data"]["sessionID"].as_str()
}

/// What the event stream showed of a turn: its answers as neutral progress, and the first error
/// opencode reported.
#[derive(Default)]
struct Seen {
    texts: BTreeSet<String>,
    tools: HashMap<String, &'static str>,
    tool_names: HashMap<String, &'static str>,
    error: Option<ExecFailure>,
}

impl Seen {
    /// The progress an event shows; the user's own messages, reasoning and tool details stay out.
    fn apply(&mut self, event: &Value) -> Option<Value> {
        match event["type"].as_str()? {
            "session.text.delta" => {
                let data = &event["data"];
                let id = format!(
                    "{}:{}",
                    data["assistantMessageID"].as_str()?,
                    data["ordinal"].as_u64().unwrap_or_default()
                );
                self.texts.insert(id.clone());
                Some(
                    json!({"type":"text_delta","item_id":bounded(&id, 128),"text":bounded(data["delta"].as_str()?, 8000)}),
                )
            }
            "session.text.ended" => {
                let data = &event["data"];
                let id = format!(
                    "{}:{}",
                    data["assistantMessageID"].as_str()?,
                    data["ordinal"].as_u64().unwrap_or_default()
                );
                Some(
                    json!({"type":"text_snapshot","item_id":bounded(&id, 128),"text":bounded(data["text"].as_str()?, 8000)}),
                )
            }
            "session.tool.input.started" => {
                let data = &event["data"];
                let call = data["id"].as_str()?;
                let name = data["name"].as_str().unwrap_or("tool");
                let name = tool_name(name);
                self.tool_names.insert(call.to_owned(), name);
                (self.tools.insert(call.to_owned(), "running") != Some("running")).then(|| {
                    json!({"type":"tool","item_id":bounded(call, 128),"name":name,"status":"running"})
                })
            }
            "session.tool.success" | "session.tool.failed" => {
                let data = &event["data"];
                let call = data["id"].as_str()?;
                let name = self.tool_names.get(call).copied().unwrap_or("tool");
                (self.tools.insert(call.to_owned(), "completed") != Some("completed")).then(|| {
                    json!({"type":"tool","item_id":bounded(call, 128),"name":name,"status":"completed"})
                })
            }
            "session.execution.failed" if self.error.is_none() => {
                self.error = Some(failure_of(&event["data"]["error"]));
                None
            }
            "session.execution.interrupted" if self.error.is_none() => {
                self.error = Some(ExecFailure::new(
                    FailureKind::Unknown,
                    "opencode execution interrupted",
                ));
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

/// Each v2 user message owns the assistant messages until the next user message.
fn chain(messages: &[Value], id: &str) -> Chain {
    let user_at = messages.iter().position(|message| message["id"] == id);
    let user = user_at.map(|at| &messages[at]);
    let replies: Vec<&Value> = user_at
        .into_iter()
        .flat_map(|at| messages.iter().skip(at + 1))
        .take_while(|message| message["type"] != "user")
        .filter(|message| message["type"] == "assistant")
        .collect();
    Chain {
        visible: user.is_some(),
        created_ms: (user.and_then(|message| message["time"]["created"].as_i64()))
            .unwrap_or_default(),
        reply: replies.last().map(|message| (*message).clone()),
        text: (replies.iter().rev().map(|message| text_of(message)))
            .find(|text| !text.trim().is_empty())
            .unwrap_or_default(),
    }
}

/// The text an assistant message shows: its own text parts, not the ones opencode adds.
fn text_of(message: &Value) -> String {
    (message["content"].as_array().into_iter().flatten())
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// What an error opencode recorded means.
fn failure_of(error: &Value) -> ExecFailure {
    let name = error["type"]
        .as_str()
        .or_else(|| error["_tag"].as_str())
        .unwrap_or("opencode error");
    let detail = error["message"]
        .as_str()
        .map_or_else(|| format!("opencode: {name}"), str::to_owned);
    match (name, error["status"].as_u64()) {
        (_, Some(401 | 403)) | ("provider.auth", _) => {
            ExecFailure::new(FailureKind::AuthFailed, detail)
        }
        (_, Some(429)) => ExecFailure::new(FailureKind::Quota, detail),
        (_, Some(500..=599)) => ExecFailure::new(FailureKind::Transport, detail),
        (_, Some(400..=499)) => ExecFailure::upstream(detail, FailureKind::Rejected),
        _ => ExecFailure::upstream(detail, FailureKind::Unknown),
    }
}

fn http_failure(status: u16, body: &Value) -> ExecFailure {
    let detail = body["message"]
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
            Ok(Some(json!({"providerID":provider,"id":id})))
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
    match extension.as_deref() {
        Some("png" | "jpg" | "jpeg" | "gif" | "webp") => {}
        _ => return Err(refuse("it takes png, jpeg, gif and webp files")),
    }
    if !(path.is_absolute() && path.is_file()) {
        return Err(refuse("not an existing absolute path"));
    }
    let url = reqwest::Url::from_file_path(path).map_err(|()| refuse("not a file path"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image");
    Ok(json!({"uri":url.as_str(),"name":name}))
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
            let sending = Claim::take(&self.sending, id);
            let reject = |failure: ExecFailure| -> Result<bool> { Err(failure.into()) };
            let mut files = Vec::new();
            for image in args["images"].as_array().into_iter().flatten() {
                match image_part(image) {
                    Ok(file) => files.push(file),
                    Err(failure) => return reject(failure),
                }
            }
            // ponytail: opencode takes no effort per message; a model variant would carry it.
            let model = match parse_model(args["model"].as_str().unwrap_or_default()) {
                Ok(model) => model,
                Err(failure) => return reject(failure),
            };
            let (conn, serving) = match self.conn().await {
                Ok(conn) => conn,
                Err(error) => return reject(launch_failure(&error)),
            };
            let (session, model_change) = match thread {
                Some(session) => match self.session(&conn, session).await? {
                    Some(found) => {
                        let change = model
                            .as_ref()
                            .filter(|selected| &found["model"] != *selected)
                            .cloned();
                        (session.to_owned(), change)
                    }
                    None => {
                        let failure = ExecFailure::new(
                            FailureKind::Rejected,
                            format!("opencode has no session {session}"),
                        );
                        return reject(failure);
                    }
                },
                None => {
                    let cwd = Path::new(&work.cwd);
                    if !(cwd.is_absolute() && cwd.is_dir()) {
                        let failure = ExecFailure::new(
                            FailureKind::Rejected,
                            "the work directory must be an existing absolute path",
                        );
                        return reject(failure);
                    }
                    match self.create(&conn, &work.cwd, model.as_ref()).await {
                        Ok(session) => (session, None),
                        Err(failure) => return reject(failure),
                    }
                }
            };
            // Two runs in one session would end each other's turns. A work not known to be sent
            // holds the session too, unless its own start has yet to try for the session here.
            let _claim = match thread {
                Some(_) => {
                    let claim = Claim::take(&self.starting, &session);
                    drop(sending);
                    let runs = node.store.lock().await.work_records_on_thread(&session)?;
                    let holder = runs.into_iter().find(|run| {
                        run.work_id != id
                            && !run.state.finished()
                            && !(matches!(run.state, WorkState::Starting { .. })
                                && (self.sending.lock())
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .contains(&run.work_id))
                    });
                    let busy = match (&claim, &holder) {
                        (None, _) => {
                            Some("opencode is starting another work in this session".to_owned())
                        }
                        (_, Some(run)) => Some(format!(
                            "work {} still holds this opencode session ({}); it ends when that work finishes or the user settles it",
                            run.work_id,
                            run.state.name()
                        )),
                        _ if !self.inbox(&conn, &session).await?.is_empty() => {
                            Some("opencode still has pending input in this session".to_owned())
                        }
                        _ if self.busy(&conn, &session).await? => {
                            Some("opencode still runs a turn in this session".to_owned())
                        }
                        _ => None,
                    };
                    if let Some(busy) = busy {
                        return reject(ExecFailure::new(FailureKind::WriterBusy, busy));
                    }
                    claim
                }
                None => {
                    drop(sending);
                    None
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
            if let Some(model) = model_change {
                self.switch_model(&conn, &session, &model).await?;
            }
            let message = work_message(id);
            let mut body = json!({"id":message,"text":work.prompt});
            if !files.is_empty() {
                body["files"] = json!(files);
            }
            if let Err(error) = self.environment(&conn, &session, &BTreeMap::new()).await {
                return reject(ExecFailure::new(
                    FailureKind::Transport,
                    format!("opencode: {error:#}"),
                ));
            }
            match self.send(&conn, &session, &body).await {
                Sent::Taken => {
                    let epoch = conn.started_ms as u64;
                    let signal = Signal::TurnStarted {
                        turn_id: message,
                        epoch,
                    };
                    // The work runs now; its follower reads the turn back.
                    if let Err(error) = node.signal_work(network, id, signal).await {
                        tracing::warn!(event = "opencode.work_update_pending", %error, work_id = id, "the opencode work waits for its probe");
                    }
                }
                Sent::Refused(failure) => return reject(failure),
                Sent::Unknown(detail) => {
                    tracing::warn!(
                        event = "opencode.send_unknown",
                        work_id = id,
                        detail,
                        "unknown whether opencode took the work"
                    );
                }
            }
            let follower = follow(Arc::downgrade(node), network.to_owned(), id.to_owned());
            tokio::spawn(async move {
                follower.await;
                drop(serving);
            });
            Ok(false)
        })
    }

    fn probe<'a>(&'a self, work: &'a WorkRecord) -> BoxFuture<'a, Result<Probe>> {
        Box::pin(async move {
            let id = work_message(&work.work_id);
            if self
                .cancelled_inputs
                .lock()
                .expect("cancelled inputs")
                .remove(&id)
            {
                return Ok(Probe::Terminal {
                    turn_id: id,
                    status: TurnStatus::Interrupted,
                    text: String::new(),
                    error: None,
                });
            }
            let session = work.state.thread().context("the work has no session")?;
            let conn = (self.running().await)
                .context("no opencode serve runs; the next one reads this work back")?;
            let Some(found) = self.session(&conn, session).await? else {
                return Ok(Probe::Missing);
            };
            let epoch = conn.started_ms as u64;
            let Some(messages) = self.tail(&conn, session, &id).await? else {
                if self.inbox_contains(&conn, session, &id).await? {
                    return Ok(Probe::Running { turn_id: id, epoch });
                }
                return Ok(Probe::NotStarted);
            };
            found["location"]["directory"]
                .as_str()
                .context("opencode sent a session without its directory")?;
            let later = messages.iter().skip(1).any(|m| m["type"] == "user");
            let busy = self.busy(&conn, session).await?;
            if !later && busy {
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
                Some(reply) if reply["error"]["type"] == "aborted" => {
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
                _ if busy || now_ms() - chain.created_ms < QUIET_MS => Probe::Running {
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
            let (conn, _serving) = self.conn().await?;
            (self.session(&conn, session).await?).context("opencode no longer has the session")?;
            let id = work_message(&work.work_id);
            if self.cancel_inbox_message(&conn, session, &id).await? {
                self.cancelled_inputs
                    .lock()
                    .expect("cancelled inputs")
                    .insert(id);
                return Ok(());
            }
            let (status, body) = self.abort(&conn, session).await?;
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
            let Some((conn, _directory, mut body)) = joined else {
                return SteerAck::Refused("the reply already ended".into());
            };
            let message = json!({"message_id":message_id,"text":text});
            body["id"] = json!(id);
            body["text"] = json!(format!(
                "The user sent another message in this conversation while you were replying. Take it into account and answer it in this same reply.\n\n{message}"
            ));
            body["delivery"] = json!("steer");
            match self.send(&conn, &into.thread_id, &body).await {
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
            let (conn, _serving) = self.conn().await.ok()?;
            if self.session(&conn, &into.thread_id).await.ok()?.is_none() {
                return Some(SteerTrace::TargetUnseen);
            }
            Some(
                match self.received(&conn, &into.thread_id, &id).await.ok()? {
                    false => SteerTrace::NotArrived,
                    true => SteerTrace::Ended(SteerOutcome::Unfinished),
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

    #[derive(Default)]
    struct Fake {
        /// A new password is a new serve: the runs of the one before died with it.
        password: String,
        sessions: BTreeMap<String, FakeSession>,
        /// Every message sent: its session, body and the tool environment its shells get.
        prompts: Vec<Value>,
        aborts: usize,
        held: BTreeMap<String, String>,
        peak_active: usize,
        interrupted: Vec<String>,
        environment_failure: bool,
        ids: usize,
    }

    #[derive(Default)]
    struct FakeSession {
        directory: String,
        /// The session's own permission rules.
        permission: Value,
        model: Value,
        environment: Value,
        busy: bool,
        messages: Vec<Value>,
        inbox: Vec<Value>,
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

        fn event(&self, event_type: &str, data: Value) -> Value {
            json!({"id":format!("evt_{}", uuid::Uuid::new_v4().simple()),"created":now_ms(),
                "type":event_type,"data":data})
        }

        fn begin(&self, fake: &mut Fake, session: &str) -> String {
            let id = fake.id("msg");
            let model = fake.sessions[session].model.clone();
            let info = json!({"id":id,"type":"assistant","agent":"build",
                "model":model,"time":{"created":now_ms()},"content":[]});
            self.emit(self.event(
                "session.step.started",
                json!({"sessionID":session,
                "assistantMessageID":id,"agent":"build","model":{"providerID":"fake","id":"m"}}),
            ));
            fake.messages(session).push(info);
            id
        }

        fn finish(&self, fake: &mut Fake, session: &str, id: &str, text: &str) {
            let call = fake.id("call");
            let at = now_ms();
            self.emit(self.event(
                "session.tool.input.started",
                json!({"sessionID":session,
                "assistantMessageID":id,"id":call,"name":"bash"}),
            ));
            self.emit(self.event(
                "session.tool.called",
                json!({"sessionID":session,
                "assistantMessageID":id,"id":call,"input":{},"executed":true}),
            ));
            self.emit(self.event(
                "session.tool.success",
                json!({"sessionID":session,
                "assistantMessageID":id,"id":call,"content":[{"type":"text","text":"done"}],"metadata":{},"executed":true}),
            ));
            self.emit(self.event(
                "session.text.started",
                json!({"sessionID":session,
                "assistantMessageID":id,"ordinal":0}),
            ));
            self.emit(self.event(
                "session.text.delta",
                json!({"sessionID":session,
                "assistantMessageID":id,"ordinal":0,"delta":text}),
            ));
            self.emit(self.event(
                "session.text.ended",
                json!({"sessionID":session,
                "assistantMessageID":id,"ordinal":0,"text":text}),
            ));
            let tool = json!({"type":"tool","id":call,"name":"bash","executed":true,
                "state":{"status":"completed","input":{},"metadata":{},"content":[{"type":"text","text":"done"}]},
                "time":{"created":at,"ran":at,"completed":now_ms()}});
            let part = json!({"type":"text","text":text});
            let reply = (fake.messages(session).iter_mut())
                .find(|message| message["id"] == id)
                .expect("reply");
            reply["time"]["completed"] = json!(now_ms());
            reply["finish"] = json!("stop");
            reply["content"] = json!([tool, part]);
            self.emit(self.event(
                "session.step.ended",
                json!({"sessionID":session,
                "assistantMessageID":id,"finish":"stop"}),
            ));
        }

        fn idle(&self, fake: &mut Fake, session: &str) {
            fake.sessions.get_mut(session).expect("session").busy = false;
            self.emit(self.event("session.execution.succeeded", json!({"sessionID":session})));
            self.emit(self.event("session.idle", json!({"sessionID":session})));
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
                fake.held.clear();
                fake.sessions
                    .values_mut()
                    .for_each(|session| session.busy = false);
            }
        }
        next.run(request).await
    }

    fn missing() -> Response {
        let body =
            json!({"_tag":"SessionNotFoundError","sessionID":"ses_missing","message":"not found"});
        (StatusCode::NOT_FOUND, Json(body)).into_response()
    }

    async fn create(State(api): State<Api>, Json(body): Json<Value>) -> Json<Value> {
        let directory = body["location"]["directory"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let mut fake = api.fake.lock().unwrap();
        let id = format!("ses_fake{}", fake.sessions.len() + 1);
        let session = FakeSession {
            directory,
            permission: body["permissions"].clone(),
            model: body["model"].clone(),
            ..Default::default()
        };
        let created = json!({"id":id,"projectID":"project_fake","cost":0,
            "tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}},
            "time":{"created":now_ms(),"updated":now_ms()},"agent":"build",
            "model":session.model,"permissions":session.permission,
            "location":{"directory":session.directory}});
        fake.sessions.insert(id, session);
        Json(json!({"data":created}))
    }

    async fn active(State(api): State<Api>) -> Json<Value> {
        let fake = api.fake.lock().unwrap();
        let busy = (fake.sessions.iter())
            .filter(|(_, session)| session.busy)
            .map(|(id, _)| (id.clone(), json!({"type":"running"})));
        Json(json!({"data":Value::Object(busy.collect())}))
    }

    async fn session(State(api): State<Api>, UrlPath(id): UrlPath<String>) -> Response {
        let fake = api.fake.lock().unwrap();
        match fake.sessions.get(&id) {
            Some(found) => Json(json!({"data":{"id":id,"projectID":"project_fake","cost":0,
                    "tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}},
                    "time":{"created":0,"updated":0},"model":found.model,
                    "location":{"directory":found.directory}}}))
            .into_response(),
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
        let mut messages = found.messages.clone();
        if query.get("order").is_some_and(|order| order == "desc") {
            messages.reverse();
        }
        messages.truncate(limit);
        Json(json!({"data":messages,"cursor":{}})).into_response()
    }

    async fn message(
        State(api): State<Api>,
        UrlPath((id, message)): UrlPath<(String, String)>,
    ) -> Response {
        let fake = api.fake.lock().unwrap();
        let found = (fake.sessions.get(&id))
            .and_then(|session| (session.messages.iter()).find(|m| m["id"] == message.as_str()));
        found.map_or_else(missing, |found| Json(json!({"data":found})).into_response())
    }

    async fn inbox(State(api): State<Api>, UrlPath(id): UrlPath<String>) -> Response {
        let fake = api.fake.lock().unwrap();
        let Some(session) = fake.sessions.get(&id) else {
            return missing();
        };
        Json(json!({"data":session.inbox})).into_response()
    }

    async fn cancel_inbox(
        State(api): State<Api>,
        UrlPath((session, id)): UrlPath<(String, String)>,
    ) -> StatusCode {
        let mut fake = api.fake.lock().unwrap();
        let Some(session) = fake.sessions.get_mut(&session) else {
            return StatusCode::NOT_FOUND;
        };
        session.inbox.retain(|item| item["id"] != id);
        StatusCode::NO_CONTENT
    }

    async fn switch_model(
        State(api): State<Api>,
        UrlPath(id): UrlPath<String>,
        Json(body): Json<Value>,
    ) -> StatusCode {
        let mut fake = api.fake.lock().unwrap();
        let Some(session) = fake.sessions.get_mut(&id) else {
            return StatusCode::NOT_FOUND;
        };
        session.model = body["model"].clone();
        StatusCode::NO_CONTENT
    }

    async fn environment(
        State(api): State<Api>,
        UrlPath(id): UrlPath<String>,
        Json(body): Json<Value>,
    ) -> StatusCode {
        let mut fake = api.fake.lock().unwrap();
        if fake.environment_failure {
            return StatusCode::SERVICE_UNAVAILABLE;
        }
        let Some(session) = fake.sessions.get_mut(&id) else {
            return StatusCode::NOT_FOUND;
        };
        session.environment = body["variables"].clone();
        StatusCode::NO_CONTENT
    }

    async fn prompt(
        State(api): State<Api>,
        UrlPath(session): UrlPath<String>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let id = body["id"].as_str().unwrap_or_default().to_owned();
        let text = body["text"].as_str().unwrap_or_default().to_owned();
        let text = text
            .strip_prefix("Answer briefly.\n\n")
            .unwrap_or(&text)
            .to_owned();
        let mut fake = api.fake.lock().unwrap();
        let env = fake
            .sessions
            .get(&session)
            .map(|session| session.environment.clone())
            .unwrap_or_default();
        fake.prompts
            .push(json!({"session":session,"body":body,"env":env}));
        let Some(found) = fake.sessions.get_mut(&session) else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"_tag":"SessionNotFoundError","message":"not found"})),
            );
        };
        found
            .messages
            .push(json!({"id":id,"type":"user","time":{"created":now_ms()},"text":text}));
        found.inbox.retain(|item| item["id"] != id);
        found.busy = true;
        let active = fake
            .sessions
            .values()
            .filter(|session| session.busy)
            .count();
        fake.peak_active = fake.peak_active.max(active);
        if text == "INBOX" {
            let accepted = json!({"id":id,"sessionID":session,"time":{"created":now_ms()},
                "type":"user","payload":{"text":text},"delivery":"queue"});
            let session = fake.sessions.get_mut(&session).unwrap();
            session.messages.retain(|message| message["id"] != id);
            session.inbox.push(accepted.clone());
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"_tag":"UnknownError","message":"accepted then lost response"})),
            );
        }
        if text == "HOLD" {
            let reply = api.begin(&mut fake, &session);
            fake.held.insert(session.clone(), reply);
            return (
                StatusCode::OK,
                Json(json!({"data":{"id":id,"sessionID":session,
                "time":{"created":now_ms()},"type":"user","payload":{"text":text},"delivery":"steer"}})),
            );
        }
        let accepted = json!({"id":id,"sessionID":session,"time":{"created":now_ms()},
            "type":"user","payload":{"text":text},"delivery":"steer"});
        fake.sessions
            .get_mut(&session)
            .unwrap()
            .inbox
            .push(accepted.clone());
        drop(fake);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut fake = api.fake.lock().unwrap();
            let queued = &mut fake.sessions.get_mut(&session).unwrap().inbox;
            if !queued.iter().any(|input| input["id"] == id) {
                api.idle(&mut fake, &session);
                return;
            }
            queued.retain(|input| input["id"] != id);
            if let Some(held) = fake.held.remove(&session) {
                api.finish(&mut fake, &session, &held, "first");
            }
            let reply = api.begin(&mut fake, &session);
            if text == "FAIL" {
                let error = json!({"type":"provider.auth","message":"fixture denied","status":401});
                let message = fake
                    .messages(&session)
                    .iter_mut()
                    .find(|message| message["id"] == reply)
                    .unwrap();
                message["time"]["completed"] = json!(now_ms());
                message["error"] = error.clone();
                fake.sessions.get_mut(&session).unwrap().busy = false;
                api.emit(api.event(
                    "session.execution.failed",
                    json!({"sessionID":session,"error":error}),
                ));
                return;
            }
            api.finish(&mut fake, &session, &reply, &format!("re:{text}"));
            api.idle(&mut fake, &session);
        });
        (StatusCode::OK, Json(json!({"data":accepted})))
    }

    async fn abort(State(api): State<Api>, UrlPath(session): UrlPath<String>) -> Json<Value> {
        let mut fake = api.fake.lock().unwrap();
        fake.aborts += 1;
        fake.interrupted.push(session.clone());
        if let Some(held) = fake.held.remove(&session) {
            let reply = (fake.messages(&session).iter_mut())
                .find(|message| message["id"] == held.as_str())
                .expect("reply");
            reply["error"] = json!({"type":"aborted","message":"The operation was aborted."});
            reply["time"]["completed"] = json!(now_ms());
            api.emit(api.event(
                "session.execution.interrupted",
                json!({"sessionID":session,"reason":"user"}),
            ));
            api.idle(&mut fake, &session);
        }
        Json(json!({"interrupted":true}))
    }

    async fn events(
        State(api): State<Api>,
    ) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
        let connected = json!({"id":"evt_connected","type":"server.connected","data":{}});
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
                .route("/api/session", post(create))
                .route("/api/session/active", get(active))
                .route("/api/session/{id}", get(session))
                .route("/api/session/{id}/message", get(messages))
                .route("/api/session/{id}/message/{message}", get(message))
                .route("/api/session/{id}/inbox", get(inbox))
                .route(
                    "/api/session/{id}/inbox/{input}",
                    axum::routing::delete(cancel_inbox),
                )
                .route("/api/session/{id}/model", post(switch_model))
                .route(
                    "/api/session/{id}/environment",
                    axum::routing::put(environment),
                )
                .route("/api/session/{id}/prompt", post(prompt))
                .route("/api/session/{id}/interrupt", post(abort))
                .route("/api/event", get(events))
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
        let opencode = node.opencode.clone();
        let sends = (node.start_work(opencode.as_ref(), network, spec, None, &args)).await?;
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
                json!({"type":"text_delta","item_id":"msg_1:0","text":"re:hello"}),
                json!({"type":"text_snapshot","item_id":"msg_1:0","text":"re:hello"}),
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
            json!([{"action":"*","resource":"*","effect":"allow"},
                {"action":"question","resource":"*","effect":"deny"}]),
            "the session's own rules win over the user's opencode permission"
        );
        let sent = &place.prompts()[0];
        assert_eq!(sent["session"], session.as_str());
        assert_eq!(
            sent["body"],
            json!({"id":message,"text":"Answer briefly.\n\nhello"})
        );
        assert_eq!(sent["env"]["VIBEFACE_NODE_URL"], "http://127.0.0.1:9");
        assert_eq!(sent["env"]["VIBEFACE_TOOL_TOKEN"], "tool-token-fixture");
        assert_eq!(sent["env"]["FAKE_API_KEY"], "");
        assert_eq!(sent["env"]["OPENCODE_SERVER_PASSWORD"], "");
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
        assert!(
            settings["plugin"].is_null(),
            "session environments do not use a plugin"
        );

        let mut config = bot(&place, &server, "attempt-2", &turns);
        config.model = "fake/next".into();
        let (next, saved, _) = turn(&place, &config, "more", Some(&session), never).await;
        assert_eq!(next?.output, "re:more");
        assert_eq!(
            place.fake.lock().unwrap().sessions[&session].model,
            json!({"providerID":"fake","id":"next"})
        );
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
    async fn a_bot_turn_aborted_by_opencode_is_not_a_requested_stop() -> Result<()> {
        let place = Place::new().await?;
        let server = place.opencode()?;
        let turns = Turns::default();
        let config = bot(&place, &server, "attempt-1", &turns);
        let interrupted = async {
            let (session, _) = started(&turns).await;
            let mut fake = place.fake.lock().unwrap();
            let held = fake.held.remove(&session).context("held reply")?;
            let reply = fake
                .messages(&session)
                .iter_mut()
                .find(|reply| reply["id"] == held)
                .context("reply")?;
            reply["error"] = json!({"type":"aborted","message":"The operation was aborted."});
            reply["time"]["completed"] = json!(now_ms());
            fake.sessions.get_mut(&session).unwrap().busy = false;
            anyhow::Ok(())
        };
        let ((result, _, _), ended) = tokio::join!(
            turn(&place, &config, "HOLD", None, watch::channel(false).1),
            interrupted
        );
        ended?;
        let error = result.err().context("an aborted turn has no reply")?;
        let error = error
            .downcast_ref::<ManagerError>()
            .context("manager error")?;
        assert_eq!(error.outcome, ManagerFailure::Failed, "{error}");
        assert!(error.to_string().contains("without being asked"), "{error}");
        assert_eq!(place.fake.lock().unwrap().aborts, 0);
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
    async fn a_bot_turn_whose_progress_fails_stops_its_run_before_reading_unknown() -> Result<()> {
        let place = Place::new().await?;
        let server = place.opencode()?;
        let turns = Turns::default();
        let config = bot(&place, &server, "attempt-1", &turns);
        let result = run_manager(
            &config,
            "hello",
            None,
            |_| async { anyhow::Ok(()) },
            watch::channel(false).1,
            |_| async { Err(anyhow::anyhow!("the progress could not be written")) },
        )
        .await;
        let Err(error) = result else {
            bail!("a turn whose progress failed has no reply");
        };
        let error = error
            .downcast_ref::<ManagerError>()
            .context("a manager error")?;
        assert_eq!(error.outcome, ManagerFailure::Unknown, "{error}");
        assert!(place.fake.lock().unwrap().aborts >= 1);
        Ok(())
    }

    #[tokio::test]
    async fn sixteen_bot_sessions_keep_outputs_environments_and_cancellation_apart() -> Result<()> {
        let place = Place::new().await?;
        let server = place.opencode()?;
        let turns = (0..16).map(|_| Turns::default()).collect::<Vec<_>>();
        let channels = (0..16).map(|_| watch::channel(false)).collect::<Vec<_>>();
        let configs = (0..16)
            .map(|index| {
                let mut config = bot(&place, &server, &format!("attempt-{index}"), &turns[index]);
                config.tool_token = Some(format!("token-{index}"));
                config
            })
            .collect::<Vec<_>>();
        let running =
            futures_util::future::join_all(configs.iter().enumerate().map(|(index, config)| {
                turn(&place, config, "HOLD", None, channels[index].1.clone())
            }));
        let control = async {
            let started = futures_util::future::join_all(turns.iter().map(started)).await;
            channels[0].0.send(true).unwrap();
            for (index, (session, turn)) in started.iter().enumerate().skip(1) {
                let target = SteerTarget {
                    provider: Provider::Opencode,
                    attempt_id: format!("attempt-{index}"),
                    thread_id: session.clone(),
                    turn_id: turn.clone(),
                };
                assert_eq!(
                    server
                        .steer(&target, &format!("m-{index}"), &format!("only-{index}"))
                        .await,
                    SteerAck::Accepted
                );
            }
            started
        };
        let (results, started) = tokio::join!(running, control);
        assert!(
            results[0]
                .0
                .as_ref()
                .err()
                .expect("failed turn")
                .downcast_ref::<ManagerError>()
                .unwrap()
                .cancelled()
        );
        for (index, (result, saved, _)) in results.into_iter().enumerate().skip(1) {
            let result = result?;
            assert_eq!(
                result.session_id.as_deref(),
                Some(started[index].0.as_str())
            );
            assert!(
                result.output.contains(&format!("only-{index}")),
                "{}",
                result.output
            );
            for other in 1..16 {
                if other != index {
                    assert!(
                        !result
                            .output
                            .contains(&format!("\"text\":\"only-{other}\"")),
                        "{}",
                        result.output
                    );
                }
            }
            assert_eq!(saved.unwrap().0, started[index].0);
        }
        let fake = place.fake.lock().unwrap();
        assert_eq!(fake.sessions.len(), 16);
        assert_eq!(fake.peak_active, 16);
        assert!(
            fake.interrupted
                .iter()
                .all(|session| session == &started[0].0)
        );
        for prompt in &fake.prompts {
            let session = prompt["session"].as_str().unwrap();
            let index = started.iter().position(|(id, _)| id == session).unwrap();
            assert_eq!(
                prompt["env"]["VIBEFACE_TOOL_TOKEN"],
                format!("token-{index}")
            );
            assert_eq!(prompt["env"]["FAKE_API_KEY"], "");
            assert_eq!(prompt["env"]["OPENCODE_SERVER_PASSWORD"], "");
        }
        Ok(())
    }

    #[tokio::test]
    async fn one_failed_bot_session_does_not_fail_the_others() -> Result<()> {
        let place = Place::new().await?;
        let server = place.opencode()?;
        let turns = (0..4).map(|_| Turns::default()).collect::<Vec<_>>();
        let configs = (0..4)
            .map(|index| bot(&place, &server, &format!("attempt-{index}"), &turns[index]))
            .collect::<Vec<_>>();
        let (_, never) = watch::channel(false);
        let results =
            futures_util::future::join_all(configs.iter().enumerate().map(|(index, config)| {
                turn(
                    &place,
                    config,
                    if index == 0 { "FAIL" } else { "hello" },
                    None,
                    never.clone(),
                )
            }))
            .await;
        assert_eq!(
            results[0]
                .0
                .as_ref()
                .err()
                .expect("failed turn")
                .downcast_ref::<ManagerError>()
                .unwrap()
                .failure
                .kind,
            FailureKind::AuthFailed
        );
        for (result, _, _) in results.into_iter().skip(1) {
            assert_eq!(result?.output, "re:hello");
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_refused_tool_environment_never_starts_the_prompt() -> Result<()> {
        let place = Place::new().await?;
        let server = place.opencode()?;
        place.fake.lock().unwrap().environment_failure = true;
        let config = bot(&place, &server, "attempt-1", &Turns::default());
        let (_, never) = watch::channel(false);
        let (result, _, _) = turn(&place, &config, "hello", None, never).await;
        assert!(result.is_err());
        assert!(place.prompts().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn accepted_inbox_input_is_not_reported_as_not_started_or_replayed() -> Result<()> {
        let place = Place::new().await?;
        let server = place.opencode()?;
        let (conn, _serving) = server.conn().await?;
        let cwd = place.dir.join("work").display().to_string();
        let session = server
            .create(&conn, &cwd, None)
            .await
            .map_err(|failure| anyhow::anyhow!(failure.detail))?;
        let id = work_message("w-1");
        assert!(matches!(
            server
                .send(&conn, &session, &json!({"id":id,"text":"INBOX"}))
                .await,
            Sent::Unknown(_)
        ));
        assert!(server.message(&conn, &session, &id).await?.is_none());
        assert!(server.received(&conn, &session, &id).await?);
        let (mut work, _) = crate::work::accept(spec("t", &place, "INBOX"), 0);
        crate::work::transition(&mut work, Signal::ThreadCreated(session));
        assert!(matches!(server.probe(&work).await?, Probe::Running { .. }));
        assert_eq!(place.prompts().len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_an_idle_inbox_removes_input_before_another_turn_can_run() -> Result<()> {
        let place = Place::new().await?;
        let server = place.opencode()?;
        let (conn, _serving) = server.conn().await?;
        let session = server
            .create(&conn, &place.dir.join("work").to_string_lossy(), None)
            .await
            .map_err(|failure| anyhow::anyhow!(failure.detail))?;
        let id = work_message("queued-work");
        server
            .send(&conn, &session, &json!({"id":id,"text":"INBOX"}))
            .await;
        place
            .fake
            .lock()
            .unwrap()
            .sessions
            .get_mut(&session)
            .unwrap()
            .busy = false;
        assert!(!server.run_ended(&session).await);
        let recorded = Turns::default();
        let config = bot(&place, &server, "next", &recorded);
        let (blocked, _, _) = turn(
            &place,
            &config,
            "next",
            Some(&session),
            watch::channel(false).1,
        )
        .await;
        assert_eq!(
            blocked
                .err()
                .expect("pending input blocks a new turn")
                .downcast_ref::<ManagerError>()
                .unwrap()
                .failure
                .kind,
            FailureKind::WriterBusy
        );
        assert_eq!(place.prompts().len(), 1);
        assert!(server.stop_run(&conn, &session, &id).await);
        assert!(!server.received(&conn, &session, &id).await?);
        assert!(server.run_ended(&session).await);
        let (result, _, _) = turn(
            &place,
            &config,
            "next",
            Some(&session),
            watch::channel(false).1,
        )
        .await;
        assert_eq!(result?.output, "re:next");
        assert_eq!(place.prompts().len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn stopping_a_queued_work_removes_its_input_and_finishes_the_work() -> Result<()> {
        let place = Place::new().await?;
        let (node, network) = node(&place).await?;
        let work = spec(&node.identity.device_id()?, &place, "INBOX");
        dispatch(&node, &network, &work).await?;
        let running = until(&node, |state| matches!(state, WorkState::Running { .. })).await?;
        let session = running.state.thread().unwrap().to_owned();
        place
            .fake
            .lock()
            .unwrap()
            .sessions
            .get_mut(&session)
            .unwrap()
            .busy = false;
        node.interrupt_work(&network, &work.work_id).await?;
        let stopped = until(&node, WorkState::finished).await?;
        assert_eq!(
            stopped.state,
            WorkState::Finished {
                outcome: WorkOutcome::Interrupted
            }
        );
        assert!(
            place.fake.lock().unwrap().sessions[&session]
                .inbox
                .is_empty()
        );
        assert_eq!(place.prompts().len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn pending_input_does_not_prevent_reconciliation_from_stopping_a_running_session()
    -> Result<()> {
        let place = Place::new().await?;
        let server = place.opencode()?;
        let (conn, _serving) = server.conn().await?;
        let session = server
            .create(&conn, &place.dir.join("work").to_string_lossy(), None)
            .await
            .map_err(|failure| anyhow::anyhow!(failure.detail))?;
        server
            .send(&conn, &session, &json!({"id":"msg_active","text":"HOLD"}))
            .await;
        server
            .send(&conn, &session, &json!({"id":"msg_pending","text":"INBOX"}))
            .await;
        assert!(!server.run_ended(&session).await);
        assert!(!place.fake.lock().unwrap().sessions[&session].busy);
        assert!(
            server
                .inbox_contains(&conn, &session, "msg_pending")
                .await?
        );
        assert_eq!(place.fake.lock().unwrap().aborts, 1);
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
            json!({"action":"*","resource":"*","effect":"allow"})
        );
        assert_eq!(prompts[0]["body"]["id"], work_message("w-1").as_str());
        assert_eq!(
            place.fake.lock().unwrap().sessions[session].model,
            json!({"providerID":"fake","id":"m"})
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
        assert!(node.probe_work(&network, "w-1", true).await.is_err());
        assert_eq!(
            place.recorded("argv").lines().count(),
            1,
            "a probe started no serve"
        );
        drop(node.opencode.conn().await?);
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
        assert!(server.probe(&on("ses_gone")).await.is_err());
        assert!(
            place.recorded("argv").is_empty(),
            "a probe started no serve"
        );
        let (conn, _serving) = server.conn().await?;
        assert_eq!(server.probe(&on("ses_gone")).await?, Probe::Missing);
        let cwd = place.dir.join("work").display().to_string();
        let created = server.create(&conn, &cwd, None).await;
        let session = created.map_err(|failure| anyhow::anyhow!(failure.detail))?;
        assert_eq!(server.probe(&on(&session)).await?, Probe::NotStarted);
        assert!(place.prompts().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn a_second_work_into_a_running_opencode_session_is_refused_and_never_sent() -> Result<()>
    {
        let place = Place::new().await?;
        let (node, network) = node(&place).await?;
        let first = spec(&node.identity.device_id()?, &place, "HOLD");
        dispatch(&node, &network, &first).await?;
        let running = until(&node, |state| matches!(state, WorkState::Running { .. })).await?;
        let session = running
            .state
            .thread()
            .context("the work's session")?
            .to_owned();
        let second = WorkSpec {
            work_id: "w-2".into(),
            call_ref: "w-2".into(),
            prompt: "again".into(),
            thread: Some(session.clone()),
            ..first
        };
        (node.store.lock().await).accept_work(
            &node.identity,
            &network,
            second.clone(),
            now_ms(),
        )?;
        let args = json!({"model":"fake/other"});
        let opencode = node.opencode.clone();
        let busy = node
            .start_work(opencode.as_ref(), &network, &second, Some(&session), &args)
            .await
            .expect_err("the session runs another work");
        assert!(
            format!("{busy:#}").contains("still holds this opencode session"),
            "the dispatch says why: {busy:#}"
        );
        assert_eq!(
            place.fake.lock().unwrap().sessions[&session].model,
            json!({"providerID":"fake","id":"m"}),
            "a refused work must not change the running session's model"
        );
        let refused = (node.store.lock().await.work_record("w-2")?).context("recorded")?;
        assert_eq!(
            refused.state,
            WorkState::Finished {
                outcome: WorkOutcome::Rejected
            }
        );
        assert_eq!(
            place.prompts().len(),
            1,
            "the running session got nothing more"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_work_not_known_to_be_sent_keeps_its_opencode_session() -> Result<()> {
        let place = Place::new().await?;
        let (node, network) = node(&place).await?;
        let (conn, _serving) = node.opencode.conn().await?;
        let cwd = place.dir.join("work").display().to_string();
        let created = node.opencode.create(&conn, &cwd, None).await;
        let session = created.map_err(|failure| anyhow::anyhow!(failure.detail))?;
        let unsure = WorkSpec {
            thread: Some(session.clone()),
            ..spec(&node.identity.device_id()?, &place, "do it")
        };
        let second = WorkSpec {
            work_id: "w-2".into(),
            call_ref: "w-2".into(),
            ..unsure.clone()
        };
        for work in [&unsure, &second] {
            let mut store = node.store.lock().await;
            store.accept_work(&node.identity, &network, work.clone(), now_ms())?;
        }
        let args = json!({"model":"fake/m"});
        let opencode = node.opencode.clone();
        let busy = node
            .start_work(opencode.as_ref(), &network, &second, Some(&session), &args)
            .await
            .expect_err("the unsure work holds the session");
        assert!(format!("{busy:#}").contains("still holds"), "{busy:#}");
        let refused = (node.store.lock().await.work_record("w-2")?).context("recorded")?;
        assert_eq!(
            refused.state,
            WorkState::Finished {
                outcome: WorkOutcome::Rejected
            }
        );
        assert!(place.prompts().is_empty(), "nothing went into the session");
        Ok(())
    }

    #[tokio::test]
    async fn of_two_works_started_at_once_into_one_opencode_session_one_is_sent() -> Result<()> {
        let place = Place::new().await?;
        let (node, network) = node(&place).await?;
        let (conn, _serving) = node.opencode.conn().await?;
        let cwd = place.dir.join("work").display().to_string();
        let created = node.opencode.create(&conn, &cwd, None).await;
        let session = created.map_err(|failure| anyhow::anyhow!(failure.detail))?;
        let first = WorkSpec {
            thread: Some(session.clone()),
            ..spec(&node.identity.device_id()?, &place, "HOLD")
        };
        let second = WorkSpec {
            work_id: "w-2".into(),
            call_ref: "w-2".into(),
            ..first.clone()
        };
        for work in [&first, &second] {
            let mut store = node.store.lock().await;
            store.accept_work(&node.identity, &network, work.clone(), now_ms())?;
        }
        let args = json!({"model":"fake/m"});
        let opencode = node.opencode.clone();
        let (one, two) = tokio::join!(
            node.start_work(opencode.as_ref(), &network, &first, Some(&session), &args),
            node.start_work(opencode.as_ref(), &network, &second, Some(&session), &args)
        );
        assert_eq!(
            [one, two].iter().filter(|start| start.is_err()).count(),
            1,
            "the refused start says so"
        );
        assert_eq!(
            place.prompts().len(),
            1,
            "exactly one went into the session"
        );
        let mut rejected = 0;
        for id in ["w-1", "w-2"] {
            let run = (node.store.lock().await.work_record(id)?).context("recorded")?;
            rejected += usize::from(
                run.state
                    == WorkState::Finished {
                        outcome: WorkOutcome::Rejected,
                    },
            );
        }
        assert_eq!(rejected, 1, "the other was refused");
        Ok(())
    }

    #[tokio::test]
    async fn a_run_a_later_message_joined_reads_as_running_while_its_session_is_busy() -> Result<()>
    {
        let place = Place::new().await?;
        let server = place.opencode()?;
        let (conn, _serving) = server.conn().await?;
        let cwd = place.dir.join("work").display().to_string();
        let created = server.create(&conn, &cwd, None).await;
        let session = created.map_err(|failure| anyhow::anyhow!(failure.detail))?;
        (server.serve.lock().await.as_mut())
            .context("the serve")?
            .conn
            .started_ms -= 3 * QUIET_MS;
        let id = work_message("w-1");
        let sent = now_ms() - 2 * QUIET_MS;
        {
            let mut fake = place.fake.lock().unwrap();
            let found = fake.sessions.get_mut(&session).context("session")?;
            found.busy = true;
            found.messages = vec![
                json!({"id":id,"type":"user","time":{"created":sent}}),
                json!({"id":"msg_1","type":"assistant","time":{"created":sent},"content":[]}),
                json!({"id":"msg_2","type":"user","time":{"created":sent+1}}),
            ];
        }
        let (mut work, _) = crate::work::accept(spec("t", &place, "do it"), 0);
        crate::work::transition(&mut work, Signal::ThreadCreated(session));
        let probe = server.probe(&work).await?;
        assert!(matches!(probe, Probe::Running { .. }), "{probe:?}");
        Ok(())
    }

    #[tokio::test]
    async fn a_changed_launch_waits_until_nothing_runs_on_the_serve() -> Result<()> {
        let place = Place::new().await?;
        let server = place.opencode()?;
        let (running, serving) = server.conn().await?;
        let Launch { bin, mut env } = server.launch.lock().expect("launch").clone();
        env.insert("VIBEFACE_FAKE_LAUNCH".into(), "2".into());
        server.set_launch(bin, env);
        let (during, _) = server.conn().await?;
        assert_eq!(
            (during.base, during.started_ms),
            (running.base.clone(), running.started_ms),
            "a run in progress keeps its serve"
        );
        drop(serving);
        let (after, _) = server.conn().await?;
        assert_ne!(
            (after.base, after.started_ms),
            (running.base, running.started_ms),
            "an idle serve takes the new launch"
        );
        server.stop().await;
        Ok(())
    }

    #[test]
    fn opencode_errors_and_model_names_read_as_failures() {
        let kind = |error: Value| failure_of(&error).kind;
        let auth = json!({"type":"provider.auth","message":"sign in"});
        assert_eq!(kind(auth), FailureKind::AuthFailed);
        let quota = json!({"type":"api","message":"slow","status":429});
        assert_eq!(kind(quota), FailureKind::Quota);
        let busy = json!({"type":"api","message":"busy","status":503});
        assert_eq!(kind(busy), FailureKind::Transport);
        let other = json!({"type":"unknown","message":"boom"});
        assert_eq!(kind(other), FailureKind::Unknown);
        assert_eq!(parse_model(" auto "), Ok(None));
        assert_eq!(
            parse_model("deepseek/deepseek-flash"),
            Ok(Some(json!({"providerID":"deepseek","id":"deepseek-flash"})))
        );
        assert_eq!(
            parse_model("flash").map_err(|failure| failure.kind),
            Err(FailureKind::NoModel)
        );
    }
}
