//! pi runs bot turns and dispatched work through `pi --mode rpc`, one process per turn: no
//! process idles between turns, and each turn starts with its own environment and instructions.
//! Before a prompt is sent the Node keeps where pi writes the session, so after a restart it
//! reads the turn back from that file and never sends it again.
use super::claude::Processes;
use super::executor::{Executor, SteerAck, SteerTrace};
use super::*;
use crate::manager::{CodexTurn, ManagerConfig, ManagerError, ManagerFailure};
use crate::scheduling::{SteerOutcome, SteerTarget};
use crate::work::{Probe, Signal, WorkRecord, WorkSpec};
use base64::Engine as _;
use futures_util::future::BoxFuture;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, watch};
use vibeface_bridge::agent::{self, AgentResult};
use vibeface_bridge::failure::{ExecFailure, FailureKind};
use vibeface_bridge::turn::TurnStatus;

/// Process groups of the running pi processes, with the program each one runs.
const PIDFILE: &str = "pi.pid";
/// Set in every pi process the Node starts; the vibeface extension stays out of those.
const RUN_ENV: &str = "VIBEFACE_PI_RUN";
/// How long pi may take to start, or to answer a command.
const ANSWER: Duration = Duration::from_secs(60);
/// How long pi may take to take a prompt: it may compact the session first.
const PREFLIGHT: Duration = Duration::from_secs(600);
/// How long a stopped turn may take to end.
const SETTLE: Duration = Duration::from_secs(6);
/// A dispatched image larger than this is refused.
const MAX_IMAGE: u64 = 10 << 20;
/// The first words of a message steered into a running reply.
const STEER_LEAD: &str =
    "The user sent another message in this conversation while you were replying.";

/// The pi turns of this Node: where each session is written, and the turns running now.
pub struct Pi {
    /// Per session: its pointer (`<session>.json`) and the bot's instructions (`<session>.md`).
    dir: PathBuf,
    processes: Arc<Processes>,
    live: std::sync::Mutex<HashMap<String, Live>>,
    /// Sessions a work resumes, from its start until its pi has ended.
    resuming: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// A session claimed for a work that resumes it; released when that work's pi is done.
struct Claim(Arc<Pi>, String);

impl Drop for Claim {
    fn drop(&mut self) {
        lock(&self.0.resuming).remove(&self.1);
    }
}

/// A turn running in this process, by its session.
#[derive(Clone)]
struct Live {
    /// The entry ID of the turn's prompt.
    turn: String,
    rpc: Arc<Rpc>,
    /// The Node asked the turn to stop.
    stopping: Arc<AtomicBool>,
}

/// Where pi writes a session, kept before a prompt is sent into it.
#[derive(serde::Serialize, serde::Deserialize)]
struct Pointer {
    file: PathBuf,
    /// The process group of the session's latest pi; while this Node run tracks it, the turn
    /// may go on.
    group: u32,
    /// The session's last entry before that prompt; `None` when it had none.
    after: Option<String>,
}

/// One pi process's commands: each goes out with an ID and its response comes back by it.
struct Rpc {
    stdin: Mutex<Option<ChildStdin>>,
    /// `None` once pi closed its output.
    waiting: std::sync::Mutex<Option<HashMap<String, oneshot::Sender<Value>>>>,
    next: AtomicU64,
}

enum Reply {
    Done(Value),
    /// pi answered with an error; the command did nothing.
    Refused(String),
    /// The command never reached pi.
    Unsent,
    /// pi may have taken it, but no answer came.
    Lost,
}

/// What a pi process is started with.
struct Launch {
    bin: PathBuf,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    args: Vec<String>,
}

struct Process {
    child: Child,
    group: u32,
    _tracked: Tracked,
    rpc: Arc<Rpc>,
    /// Everything pi writes but the responses.
    events: mpsc::UnboundedReceiver<Value>,
    /// The end of what pi wrote to stderr, once it closed it.
    stderr: tokio::task::JoinHandle<String>,
}

/// How a prompt went in.
enum Begin {
    /// pi runs it; its user message is the entry with this ID.
    Started(String),
    /// Nothing ran.
    Refused(ExecFailure),
    /// It may run; only the session file tells.
    Unknown(ExecFailure),
}

/// What a work's run tells the Node, in order.
enum Report {
    Signal(Signal),
    /// Progress for the work's live view.
    Progress(Value),
    /// The run ended without seeing the turn end; the session file tells.
    Probe,
}

/// What the events of one turn showed.
#[derive(Default)]
struct Seen {
    /// Assistant messages started so far; each one's text blocks are items of their own.
    messages: u32,
    /// The last assistant message that ended.
    last: Option<Value>,
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn bounded(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|character| !character.is_control() || matches!(*character, '\n' | '\t'))
        .take(limit)
        .collect()
}

/// pi's own wording first, then what the model provider said.
fn failure(detail: impl Into<String>, otherwise: FailureKind) -> ExecFailure {
    let detail = detail.into();
    let lower = detail.to_lowercase();
    let otherwise = if lower.contains("no api key") {
        FailureKind::MissingKey
    } else if lower.contains("no model") || lower.contains("--list-models") {
        FailureKind::NoModel
    } else {
        otherwise
    };
    ExecFailure::upstream(detail, otherwise)
}

/// The failure with what pi wrote to stderr, which may say better why.
fn with_tail(failed: ExecFailure, tail: &str) -> ExecFailure {
    match tail.is_empty() {
        true => failed,
        false => failure(format!("{}: {tail}", failed.detail), failed.kind),
    }
}

/// A message's text: a user message's content may be one string.
fn text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        content => content
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

fn is_message(entry: &Value, role: &str) -> bool {
    entry["type"] == "message" && entry["message"]["role"] == role
}

/// The entries of a session file in the order pi wrote them; a line still being written is left
/// out, and so is the header.
fn entries(file: &Path) -> Result<Vec<Value>> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error.into()),
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["type"] != "session")
        .collect())
}

/// The entries of the turn whose prompt is entry `turn`, up to the next prompt: messages steered
/// into it are part of it.
fn run(entries: &[Value], turn: usize) -> &[Value] {
    let rest = &entries[turn + 1..];
    let end = rest
        .iter()
        .position(|entry| {
            is_message(entry, "user") && !text(&entry["message"]).starts_with(STEER_LEAD)
        })
        .unwrap_or(rest.len());
    &rest[..end]
}

/// How an assistant message ended its turn; `None` while it waits for its tools.
fn ending(message: &Value) -> Option<(TurnStatus, Option<ExecFailure>)> {
    Some(match message["stopReason"].as_str()? {
        "stop" => (TurnStatus::Completed, None),
        "aborted" => (TurnStatus::Interrupted, None),
        "length" => (
            TurnStatus::Failed,
            Some(ExecFailure::new(
                FailureKind::Unknown,
                "pi stopped at the model's output limit",
            )),
        ),
        "error" => (
            TurnStatus::Failed,
            Some(failure(
                message["errorMessage"]
                    .as_str()
                    .unwrap_or("pi reported an error"),
                FailureKind::Unknown,
            )),
        ),
        _ => return None,
    })
}

impl Seen {
    /// The progress an event shows: reply text and tools. Reasoning, tool arguments and outputs
    /// stay out.
    fn apply(&mut self, event: &Value) -> Vec<Value> {
        let message = &event["message"];
        let item = |messages: u32, index: &Value| {
            format!("pi-{messages}.{}", index.as_u64().unwrap_or_default())
        };
        match event["type"].as_str().unwrap_or_default() {
            "message_start" if message["role"] == "assistant" => {
                self.messages += 1;
                vec![]
            }
            "message_update" => {
                let update = &event["assistantMessageEvent"];
                match (update["type"].as_str(), update["delta"].as_str()) {
                    (Some("text_delta"), Some(delta)) => vec![json!({"type":"text_delta",
                        "item_id":item(self.messages, &update["contentIndex"]),"text":bounded(delta, 8000)})],
                    _ => vec![],
                }
            }
            "message_end" if message["role"] == "assistant" => {
                self.last = Some(message.clone());
                message["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .filter(|(_, block)| block["type"] == "text")
                    .map(|(index, block)| {
                        json!({"type":"text_snapshot","item_id":item(self.messages, &json!(index)),
                        "text":bounded(block["text"].as_str().unwrap_or_default(), 8000)})
                    })
                    .collect()
            }
            kind @ ("tool_execution_start" | "tool_execution_end") => {
                let name = match event["toolName"].as_str() {
                    Some("bash") => "command",
                    Some("edit" | "write") => "file change",
                    _ => "tool",
                };
                let status = match kind {
                    "tool_execution_end" => "completed",
                    _ => "running",
                };
                vec![
                    json!({"type":"tool","item_id":bounded(event["toolCallId"].as_str().unwrap_or("tool"), 128),
                    "name":name,"status":status}),
                ]
            }
            _ => vec![],
        }
    }

    /// How the turn ended once pi settled: by its last reply, or as stopped when the Node asked.
    fn ending(&self, stopping: bool) -> (TurnStatus, Option<ExecFailure>) {
        match self.last.as_ref().and_then(ending) {
            Some(end) => end,
            None if stopping => (TurnStatus::Interrupted, None),
            None => (
                TurnStatus::Failed,
                Some(ExecFailure::new(
                    FailureKind::Unknown,
                    "pi ended the turn without a final reply",
                )),
            ),
        }
    }

    fn text(&self) -> String {
        self.last.as_ref().map(text).unwrap_or_default()
    }
}

impl Rpc {
    async fn call(&self, mut command: Value, within: Duration) -> Reply {
        let id = format!("vf-{}", self.next.fetch_add(1, Relaxed));
        command["id"] = json!(id);
        let (sender, answer) = oneshot::channel();
        match lock(&self.waiting).as_mut() {
            Some(waiting) => waiting.insert(id.clone(), sender),
            None => return Reply::Unsent,
        };
        if !self.write(&command).await {
            if let Some(waiting) = lock(&self.waiting).as_mut() {
                waiting.remove(&id);
            }
            return Reply::Unsent;
        }
        let answer = tokio::time::timeout(within, answer).await;
        if let Some(waiting) = lock(&self.waiting).as_mut() {
            waiting.remove(&id);
        }
        match answer {
            Ok(Ok(response)) if response["success"] == true => {
                Reply::Done(response["data"].clone())
            }
            Ok(Ok(response)) => Reply::Refused(
                response["error"]
                    .as_str()
                    .unwrap_or("pi refused the command")
                    .into(),
            ),
            Ok(Err(_)) | Err(_) => Reply::Lost,
        }
    }

    /// One JSON line on pi's input; false when pi no longer reads it.
    async fn write(&self, line: &Value) -> bool {
        let mut bytes = line.to_string().into_bytes();
        bytes.push(b'\n');
        match self.stdin.lock().await.as_mut() {
            Some(pipe) => pipe.write_all(&bytes).await.is_ok() && pipe.flush().await.is_ok(),
            None => false,
        }
    }
}

/// Routes pi's output: responses to their callers, the rest as events. With no extensions
/// loaded, nothing in it waits for an answer.
async fn read_output(stdout: ChildStdout, rpc: Arc<Rpc>, events: mpsc::UnboundedSender<Value>) {
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message["type"] != "response" {
            let _ = events.send(message);
            continue;
        }
        let waiting = message["id"]
            .as_str()
            .and_then(|id| lock(&rpc.waiting).as_mut()?.remove(id));
        if let Some(waiting) = waiting {
            let _ = waiting.send(message);
        }
    }
    *lock(&rpc.waiting) = None;
}

/// A pi process group recorded in the pidfile until its process is done.
struct Tracked(Arc<Processes>, u32);

impl Drop for Tracked {
    fn drop(&mut self) {
        self.0.forget(self.1);
    }
}

impl Process {
    fn spawn(launch: &Launch, processes: &Arc<Processes>) -> Result<Self, ExecFailure> {
        if !launch.cwd.is_dir() {
            return Err(ExecFailure::new(
                FailureKind::Rejected,
                format!("{} is not a directory", launch.cwd.display()),
            ));
        }
        let mut command = Command::new(&launch.bin);
        command
            .args(["--mode", "rpc", "--no-extensions"])
            .args(&launch.args)
            .current_dir(&launch.cwd)
            .envs(&launch.env)
            .env(RUN_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            let kind = match error.kind() {
                std::io::ErrorKind::NotFound => FailureKind::MissingBinary,
                _ => FailureKind::Unknown,
            };
            ExecFailure::new(
                kind,
                format!("could not start pi at {}: {error}", launch.bin.display()),
            )
        })?;
        let group = child
            .id()
            .ok_or_else(|| ExecFailure::new(FailureKind::Lost, "pi exited as it started"))?;
        processes
            .track(group, &launch.bin)
            .map_err(|error| ExecFailure::new(FailureKind::Unknown, format!("{error:#}")))?;
        let tracked = Tracked(processes.clone(), group);
        let (Some(stdin), Some(stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(ExecFailure::new(FailureKind::Unknown, "pi has no pipes"));
        };
        let rpc = Arc::new(Rpc {
            stdin: Mutex::new(Some(stdin)),
            waiting: std::sync::Mutex::new(Some(HashMap::new())),
            next: AtomicU64::new(1),
        });
        let (sender, events) = mpsc::unbounded_channel();
        tokio::spawn(read_output(stdout, rpc.clone(), sender));
        let stderr = tokio::spawn(async move {
            let (mut tail, mut buffer) = (Vec::new(), [0; 4096]);
            while let Ok(read @ 1..) = stderr.read(&mut buffer).await {
                tail.extend_from_slice(&buffer[..read]);
                let over = tail.len().saturating_sub(2000);
                tail.drain(..over);
            }
            String::from_utf8_lossy(&tail).trim().to_owned()
        });
        Ok(Self {
            child,
            group,
            _tracked: tracked,
            rpc,
            events,
            stderr,
        })
    }

    /// Ends the process: closing its input shuts pi down, and one that does not exit in time is
    /// killed with its group. Returns the end of its stderr.
    async fn end(mut self) -> String {
        self.rpc.stdin.lock().await.take();
        let exited = matches!(
            tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await,
            Ok(Ok(_))
        );
        if !exited {
            agent::terminate_process_group(&mut self.child, self.group).await;
        }
        match tokio::time::timeout(Duration::from_secs(1), self.stderr).await {
            Ok(Ok(tail)) => tail,
            _ => String::new(),
        }
    }

    /// Asks pi to abort the turn and waits for it to settle; true once it did.
    async fn stop(&mut self, seen: &mut Seen) -> bool {
        if !self.rpc.write(&json!({"type":"abort"})).await {
            return false;
        }
        let deadline = tokio::time::Instant::now() + SETTLE;
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, self.events.recv()).await {
            seen.apply(&event);
            if event["type"] == "agent_settled" {
                return true;
            }
        }
        false
    }

    /// Sends the prompt and waits until pi keeps its user message, whose entry ID names the
    /// turn. A stop before pi took it aborts what pi does first, as compacting.
    async fn begin(
        &mut self,
        after: Option<&str>,
        prompt: Value,
        within: Duration,
        cancel: &mut watch::Receiver<bool>,
    ) -> Begin {
        let rpc = self.rpc.clone();
        let sent = rpc.call(prompt, within);
        tokio::pin!(sent);
        let mut aborted = false;
        let reply = loop {
            tokio::select! {
                reply = &mut sent => break reply,
                _ = agent::wait_for_cancel(cancel), if !aborted => {
                    aborted = true;
                    rpc.write(&json!({"type":"abort"})).await;
                }
            }
        };
        match reply {
            Reply::Done(data) if data["disposition"] == "handled" => {
                return Begin::Refused(ExecFailure::new(
                    FailureKind::Rejected,
                    "pi took the prompt as a command; nothing ran",
                ));
            }
            Reply::Done(_) => {}
            Reply::Refused(error) => return Begin::Refused(failure(error, FailureKind::Rejected)),
            Reply::Unsent => {
                return Begin::Refused(ExecFailure::new(
                    FailureKind::Lost,
                    "pi exited before it took the prompt",
                ));
            }
            Reply::Lost => {
                return Begin::Unknown(ExecFailure::new(
                    FailureKind::Lost,
                    "pi did not answer the prompt",
                ));
            }
        }
        loop {
            match tokio::time::timeout(ANSWER, self.events.recv()).await {
                Ok(Some(event))
                    if event["type"] == "message_end" && event["message"]["role"] == "user" =>
                {
                    break;
                }
                // The run ended before it kept the prompt: pi sent nothing to the model.
                Ok(Some(event)) if event["type"] == "agent_settled" => {
                    return Begin::Refused(ExecFailure::new(
                        FailureKind::Rejected,
                        "pi ended the run before it took the prompt",
                    ));
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    return Begin::Unknown(ExecFailure::new(
                        FailureKind::Lost,
                        "pi stopped before the turn started",
                    ));
                }
            }
        }
        let mut command = json!({"type":"get_entries"});
        if let Some(after) = after {
            command["since"] = json!(after);
        }
        let turn = match rpc.call(command, ANSWER).await {
            Reply::Done(data) => data["entries"]
                .as_array()
                .and_then(|entries| entries.iter().find(|entry| is_message(entry, "user")))
                .and_then(|entry| entry["id"].as_str())
                .map(str::to_owned),
            _ => None,
        };
        match turn {
            Some(turn) => Begin::Started(turn),
            None => Begin::Unknown(ExecFailure::new(
                FailureKind::Unknown,
                "pi did not show the prompt it took",
            )),
        }
    }
}

impl Pi {
    /// Ends the pi processes an earlier Node run left, so their turns are read back as they are.
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("pi"),
            processes: Arc::new(Processes::open(data_dir.join(PIDFILE))),
            live: Default::default(),
            resuming: Default::default(),
        }
    }

    /// Ends every running pi; the Node calls this before it exits.
    pub(super) async fn stop(&self) {
        self.processes.stop().await;
    }

    fn pointer_path(&self, session: &str) -> Result<PathBuf> {
        vibeface_protocol::validate_id(session).map_err(anyhow::Error::msg)?;
        Ok(self.dir.join(format!("{session}.json")))
    }

    fn pointer(&self, session: &str) -> Result<Option<Pointer>> {
        match std::fs::read(self.pointer_path(session)?) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Keeps where pi writes `session` and the session's last entry so far.
    fn keep(&self, session: &str, file: &Path, group: u32) -> Result<Option<String>> {
        let after = entries(file)?
            .last()
            .and_then(|entry| entry["id"].as_str())
            .map(str::to_owned);
        let pointer = Pointer {
            file: file.to_owned(),
            group,
            after: after.clone(),
        };
        config::private_dir(&self.dir)?;
        config::private_write(&self.pointer_path(session)?, &serde_json::to_vec(&pointer)?)?;
        Ok(after)
    }

    fn live_turn(&self, session: &str, turn: &str) -> Option<Live> {
        lock(&self.live)
            .get(session)
            .filter(|live| live.turn == turn)
            .cloned()
    }

    /// Starts pi on `session` and keeps where it writes it, with the session's last entry so
    /// far, before anything is sent.
    async fn open(
        &self,
        launch: &Launch,
        session: &str,
    ) -> Result<(Process, Option<String>), ExecFailure> {
        let process = Process::spawn(launch, &self.processes)?;
        let opened = async {
            let state = match process.rpc.call(json!({"type":"get_state"}), ANSWER).await {
                Reply::Done(state) => state,
                Reply::Refused(error) => return Err(failure(error, FailureKind::Rejected)),
                Reply::Unsent | Reply::Lost => {
                    return Err(failure("pi did not start", FailureKind::Lost));
                }
            };
            let own = |detail: String| ExecFailure::new(FailureKind::Unknown, detail);
            if state["sessionId"] != session {
                return Err(own(format!("pi opened session {}", state["sessionId"])));
            }
            let file = PathBuf::from(state["sessionFile"].as_str().unwrap_or_default());
            self.keep(session, &file, process.group)
                .map_err(|error| own(format!("keep where pi writes the session: {error:#}")))
        }
        .await;
        match opened {
            Ok(after) => Ok((process, after)),
            Err(failure) => {
                let tail = process.end().await;
                Err(with_tail(failure, &tail))
            }
        }
    }

    /// One bot turn. A session pi still has goes on; one whose file is gone is replaced by a new
    /// one, which gets the whole prompt.
    #[expect(
        clippy::too_many_arguments,
        reason = "run_manager's turn, with the pi program"
    )]
    pub(crate) async fn run_bot<F, P, PF>(
        &self,
        config: &ManagerConfig,
        bin: &Path,
        prompt: &str,
        previous: Option<&str>,
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
        let secrets = crate::manager::secret_values(config);
        let error = |outcome, session: Option<&str>, failure: ExecFailure| -> anyhow::Error {
            ManagerError {
                outcome,
                session_id: session.map(str::to_owned),
                failure: ExecFailure {
                    detail: crate::manager::redact_string(failure.detail, &secrets),
                    ..failure
                },
            }
            .into()
        };
        let own = |detail: String| ExecFailure::new(FailureKind::Unknown, detail);
        let stopped = |session: Option<&str>| {
            error(
                ManagerFailure::Cancelled,
                session,
                own("stopped before the pi turn started".into()),
            )
        };
        let resumed = match previous {
            Some(id) => self.pointer(id)?.filter(|pointer| pointer.file.is_file()),
            None => None,
        };
        let session = match (&resumed, previous) {
            (Some(_), Some(id)) => id.to_owned(),
            _ => uuid::Uuid::new_v4().to_string(),
        };
        let prompt = match (&resumed, previous, &config.codex_turn) {
            (
                None,
                Some(_),
                Some(CodexTurn {
                    fresh_prompt: Some(fresh),
                    ..
                }),
            ) => fresh.as_str(),
            _ => prompt,
        };
        let mut args = match &resumed {
            Some(pointer) => vec!["--session".into(), pointer.file.display().to_string()],
            None => vec!["--session-id".into(), session.clone()],
        };
        // The instructions go in a file: with the tool catalog they can outgrow one argument.
        let guide = crate::manager::bot_instructions(
            config.tool_cli.as_deref(),
            &config.custom_tools,
            &config.instructions,
        );
        if !guide.is_empty() {
            let path = self.dir.join(format!("{session}.md"));
            config::private_dir(&self.dir)?;
            config::private_write(&path, guide.as_bytes())?;
            args.extend(["--append-system-prompt".into(), path.display().to_string()]);
        }
        if !config.model.is_empty() {
            args.extend(["--model".into(), config.model.clone()]);
        }
        let mut env = config.provider_env.clone();
        env.extend(config.tool_env.clone());
        if let Some(token) = &config.tool_token {
            env.insert("VIBEFACE_TOOL_TOKEN".into(), token.clone());
        }
        let launch = Launch {
            bin: bin.to_owned(),
            cwd: config.workspace.clone(),
            env,
            args,
        };
        if *cancel.borrow() {
            return Err(stopped(previous));
        }
        let (mut process, after) = self
            .open(&launch, &session)
            .await
            .map_err(|failure| error(ManagerFailure::Failed, previous, failure))?;
        let native = Some(session.as_str());
        if let Some(started) = &config.process_started
            && let Err(e) = started(process.group).await
        {
            process.end().await;
            return Err(error(
                ManagerFailure::Failed,
                previous,
                own(format!("record the pi process: {e}")),
            ));
        }
        if let Err(e) = persist_session(session.clone()).await {
            process.end().await;
            // A stop fences the attempt, so saving its session fails then.
            return Err(if *cancel.borrow() {
                stopped(native)
            } else {
                error(
                    ManagerFailure::Failed,
                    native,
                    own(format!("persist the pi session: {e}")),
                )
            });
        }
        if let Some(turn) = &config.codex_turn
            && let Err(e) = (turn.record)(session.clone(), None).await
        {
            process.end().await;
            return Err(error(
                ManagerFailure::Failed,
                native,
                own(format!("record the pi turn: {e}")),
            ));
        }
        if *cancel.borrow() {
            process.end().await;
            return Err(stopped(native));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(config.timeout_secs);
        let within = PREFLIGHT.min(Duration::from_secs(config.timeout_secs));
        let prompt = json!({"type":"prompt","message":prompt});
        let turn = match process
            .begin(after.as_deref(), prompt, within, &mut cancel)
            .await
        {
            Begin::Started(turn) => turn,
            Begin::Refused(failure) => {
                let tail = process.end().await;
                return Err(if *cancel.borrow() {
                    stopped(native)
                } else {
                    error(ManagerFailure::Failed, native, with_tail(failure, &tail))
                });
            }
            Begin::Unknown(failure) => {
                let tail = process.end().await;
                return Err(error(
                    ManagerFailure::Unknown,
                    native,
                    with_tail(failure, &tail),
                ));
            }
        };
        let mut seen = Seen::default();
        lock(&self.live).insert(
            session.clone(),
            Live {
                turn: turn.clone(),
                rpc: process.rpc.clone(),
                stopping: Default::default(),
            },
        );
        let recorded = match &config.codex_turn {
            Some(record) => (record.record)(session.clone(), Some(turn.clone())).await,
            None => Ok(()),
        };
        let outcome = match recorded {
            Err(e) => {
                let kind = match process.stop(&mut seen).await {
                    true => ManagerFailure::Failed,
                    false => ManagerFailure::Unknown,
                };
                Err(error(kind, native, own(format!("record the pi turn: {e}"))))
            }
            Ok(()) => loop {
                let event = tokio::select! {
                    biased;
                    _ = agent::wait_for_cancel(&mut cancel) => {
                        break match process.stop(&mut seen).await {
                            true => Err(error(ManagerFailure::Cancelled, native, own("pi turn stopped".into()))),
                            false => Err(error(ManagerFailure::Unknown, native, own("pi did not confirm the stop".into()))),
                        };
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        let kind = match process.stop(&mut seen).await {
                            true => ManagerFailure::Failed,
                            false => ManagerFailure::Unknown,
                        };
                        break Err(error(kind, native, ExecFailure::new(FailureKind::TimedOut, "pi management timed out")));
                    }
                    event = process.events.recv() => event,
                };
                let Some(event) = event else {
                    break Err(error(
                        ManagerFailure::Unknown,
                        native,
                        ExecFailure::new(FailureKind::Lost, "pi exited during the turn"),
                    ));
                };
                let mut shown = Ok(());
                for update in seen.apply(&event) {
                    shown = progress(crate::manager::redact(update, &secrets)).await;
                    if shown.is_err() {
                        break;
                    }
                }
                if let Err(e) = shown {
                    break Err(error(ManagerFailure::Unknown, native, own(e.to_string())));
                }
                if event["type"] == "agent_settled" {
                    break match seen.ending(false) {
                        (TurnStatus::Completed, _) => Ok(seen.text()),
                        (TurnStatus::Interrupted, _) => {
                            let (outcome, failure) =
                                crate::manager::interrupted(&cancel, Provider::Pi);
                            Err(error(outcome, native, failure))
                        }
                        (_, failure) => Err(error(
                            ManagerFailure::Failed,
                            native,
                            failure.unwrap_or_else(|| own("pi turn failed".into())),
                        )),
                    };
                }
            },
        };
        lock(&self.live).remove(&session);
        process.end().await;
        Ok(AgentResult {
            session_id: Some(session),
            output: crate::manager::redact_string(outcome?, &secrets),
            usage: None,
        })
    }

    /// Runs a work's turn in its session and reports to the Node in order.
    async fn run_work(
        self: Arc<Self>,
        launch: Launch,
        session: String,
        prompt: Value,
        report: mpsc::UnboundedSender<Report>,
    ) {
        let send = |item| {
            let _ = report.send(item);
        };
        let (mut process, after) = match self.open(&launch, &session).await {
            Ok(opened) => opened,
            Err(failure) => return send(Report::Signal(Signal::StartRejected(failure))),
        };
        // A work is stopped through `interrupt`, once its turn runs.
        let mut unstoppable = watch::channel(false).1;
        let turn = match process
            .begin(after.as_deref(), prompt, PREFLIGHT, &mut unstoppable)
            .await
        {
            Begin::Started(turn) => turn,
            Begin::Refused(failure) => {
                let tail = process.end().await;
                return send(Report::Signal(Signal::StartRejected(with_tail(
                    failure, &tail,
                ))));
            }
            Begin::Unknown(_) => {
                process.end().await;
                return send(Report::Probe);
            }
        };
        // Live before it runs, so a stop finds it.
        let stopping = Arc::new(AtomicBool::new(false));
        lock(&self.live).insert(
            session.clone(),
            Live {
                turn: turn.clone(),
                rpc: process.rpc.clone(),
                stopping: stopping.clone(),
            },
        );
        send(Report::Signal(Signal::TurnStarted {
            turn_id: turn.clone(),
            epoch: 0,
        }));
        let mut seen = Seen::default();
        let settled = loop {
            let Some(event) = process.events.recv().await else {
                break false;
            };
            for update in seen.apply(&event) {
                send(Report::Progress(update));
            }
            let message = &event["message"];
            if event["type"] == "message_end" && message["role"] == "assistant" {
                let text = text(message);
                if !text.is_empty() {
                    send(Report::Signal(match message["stopReason"].as_str() {
                        // Text before tool calls is the turn's commentary.
                        Some("toolUse") => Signal::Progress {
                            summary: text,
                            at_ms: now_ms(),
                        },
                        _ => Signal::Text(text),
                    }));
                }
            }
            if event["type"] == "agent_settled" {
                break true;
            }
        };
        lock(&self.live).remove(&session);
        process.end().await;
        if !settled {
            return send(Report::Probe);
        }
        let (status, error) = seen.ending(stopping.load(Relaxed));
        send(Report::Signal(Signal::TurnCompleted {
            turn_id: turn,
            status,
            text: seen.text(),
            error,
        }));
    }
}

/// Which session a work's pi opens.
enum Origin {
    New,
    /// A new session forked from this file's.
    Fork(PathBuf),
    /// The session in this file goes on.
    Resume {
        session: String,
        file: PathBuf,
    },
}

/// A dispatched image as pi takes it.
async fn image(path: &str) -> Result<Value> {
    let mime = match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => bail!("pi takes PNG, JPEG, GIF or WebP images: {path}"),
    };
    let size = tokio::fs::metadata(path).await?.len();
    ensure!(size <= MAX_IMAGE, "{path} is larger than 10 MiB");
    let data = base64::engine::general_purpose::STANDARD.encode(tokio::fs::read(path).await?);
    Ok(json!({"type":"image","data":data,"mimeType":mime}))
}

impl Runtime {
    async fn pi_report(
        self: &Arc<Self>,
        network: &str,
        conversation: &str,
        work_id: &str,
        report: Report,
    ) -> Result<()> {
        match report {
            Report::Signal(signal) => self.signal_work(network, work_id, signal).await.map(drop),
            Report::Progress(progress) => {
                let sequence = self.progress_sequence.fetch_add(1, Relaxed) + 1;
                self.publish_progress(
                    network,
                    json!({"type":"native_progress","conversation_id":conversation,"work_id":work_id,
                        "target_node":self.identity.device_id()?,"sequence":sequence,"progress":progress}),
                );
                Ok(())
            }
            Report::Probe => self.probe_work(network, work_id, true).await,
        }
    }
}

/// A pi work runs in a new session the Node starts and journals before anything is sent; a
/// work that continues another one runs in a fork of that one's session, and one given a
/// thread goes on in that session, which one of this device's works ran in.
impl Executor for Pi {
    fn start<'a>(
        &'a self,
        node: &'a Arc<Runtime>,
        network: &'a str,
        work: &'a WorkSpec,
        thread: Option<&'a str>,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let claim = match work.thread.as_deref() {
                Some(session) if lock(&self.resuming).insert(session.to_owned()) => {
                    Some(Claim(node.pi.clone(), session.to_owned()))
                }
                Some(_) => {
                    let failure = ExecFailure::new(
                        FailureKind::WriterBusy,
                        "pi still runs another work in this session",
                    );
                    return Err(failure.into());
                }
                None => None,
            };
            let prepared = async {
                let file = |session: &str, gone: &'static str| -> Result<(PathBuf, u32)> {
                    self.pointer(session)?
                        .filter(|pointer| pointer.file.is_file())
                        .map(|pointer| (pointer.file, pointer.group))
                        .context(gone)
                };
                let origin = match (work.thread.as_deref(), thread) {
                    (Some(session), _) => {
                        let (file, group) = file(session, "the pi session of this thread is gone")?;
                        ensure!(
                            !self.processes.tracked(group),
                            "a pi started earlier still runs in this session"
                        );
                        // A restart before pi takes the prompt reads the session from here on.
                        self.keep(session, &file, group)?;
                        Origin::Resume {
                            session: session.to_owned(),
                            file,
                        }
                    }
                    (None, Some(original)) => Origin::Fork(
                        file(
                            original,
                            "the pi session of the work this one continues is gone",
                        )?
                        .0,
                    ),
                    (None, None) => Origin::New,
                };
                let mut prompt = json!({"type":"prompt","message":work.prompt});
                if let Some(paths) = args["images"].as_array() {
                    let mut images = Vec::new();
                    for path in paths {
                        images.push(image(path.as_str().context("an image is a path")?).await?);
                    }
                    prompt["images"] = json!(images);
                }
                anyhow::Ok((origin, prompt))
            };
            let (origin, prompt) = prepared.await?;
            let (session, mut launch_args) = match &origin {
                Origin::Resume { session, file } => (
                    session.clone(),
                    vec!["--session".to_owned(), file.display().to_string()],
                ),
                Origin::New | Origin::Fork(_) => {
                    let session = uuid::Uuid::new_v4().to_string();
                    node.signal_work(
                        network,
                        &work.work_id,
                        Signal::ThreadCreated(session.clone()),
                    )
                    .await?;
                    (session.clone(), vec!["--session-id".to_owned(), session])
                }
            };
            if let Origin::Fork(file) = &origin {
                launch_args.extend(["--fork".into(), file.display().to_string()]);
            }
            if let Some(model) = args["model"].as_str().filter(|model| !model.is_empty()) {
                launch_args.extend(["--model".into(), model.into()]);
            }
            if let Some(effort) = args["effort"].as_str() {
                let level = if effort == "none" { "off" } else { effort };
                launch_args.extend(["--thinking".into(), level.into()]);
            }
            let launch = Launch {
                bin: node.providers().bin(Provider::Pi),
                cwd: PathBuf::from(&work.cwd),
                env: node.provider_env(Provider::Pi),
                args: launch_args,
            };
            let (sender, mut reports) = mpsc::unbounded_channel();
            let run = node.pi.clone().run_work(launch, session, prompt, sender);
            tokio::spawn(async move {
                run.await;
                drop(claim);
            });
            let (node, network, conversation, work_id) = (
                node.clone(),
                network.to_owned(),
                work.conversation.clone(),
                work.work_id.clone(),
            );
            tokio::spawn(async move {
                while let Some(report) = reports.recv().await {
                    if let Err(error) = node
                        .pi_report(&network, &conversation, &work_id, report)
                        .await
                    {
                        tracing::warn!(event = "pi.work_report_pending", %error, work_id, "a pi work report waits for the next probe");
                    }
                }
            });
            Ok(false)
        })
    }

    fn probe<'a>(&'a self, work: &'a WorkRecord) -> BoxFuture<'a, Result<Probe>> {
        Box::pin(async move {
            let session = work.state.thread().context("the work has no thread")?;
            let Some(pointer) = self.pointer(session)? else {
                return Ok(Probe::NotStarted);
            };
            let entries = entries(&pointer.file)?;
            let turn = match work.state.handle() {
                Some(handle) => entries
                    .iter()
                    .position(|entry| entry["id"] == handle.turn_id),
                None => {
                    let start = match &pointer.after {
                        None => Some(0),
                        Some(after) => entries
                            .iter()
                            .position(|entry| entry["id"] == *after)
                            .map(|at| at + 1),
                    };
                    match start {
                        Some(start) => {
                            match entries[start..]
                                .iter()
                                .position(|entry| is_message(entry, "user"))
                            {
                                Some(at) => Some(start + at),
                                None if self.processes.tracked(pointer.group) => {
                                    bail!("pi may still take the prompt")
                                }
                                None => return Ok(Probe::NotStarted),
                            }
                        }
                        None => None,
                    }
                }
            };
            let Some(turn) = turn else {
                return Ok(Probe::Missing);
            };
            let turn_id = entries[turn]["id"].as_str().unwrap_or_default().to_owned();
            if self.processes.tracked(pointer.group) {
                return Ok(Probe::Running { turn_id, epoch: 0 });
            }
            let last = run(&entries, turn)
                .iter()
                .rev()
                .find(|entry| is_message(entry, "assistant"))
                .map(|entry| &entry["message"]);
            let (status, error) = last.and_then(ending).unwrap_or_else(|| {
                (
                    TurnStatus::Interrupted,
                    Some(ExecFailure::new(
                        FailureKind::Lost,
                        "pi exited before the turn finished",
                    )),
                )
            });
            Ok(Probe::Terminal {
                turn_id,
                status,
                text: last.map(text).unwrap_or_default(),
                error,
            })
        })
    }

    fn interrupt<'a>(&'a self, work: &'a WorkRecord) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let handle = work.state.handle().context("the work has no turn")?;
            let live = self
                .live_turn(&handle.thread_id, &handle.turn_id)
                .context("pi is not running this work on this device")?;
            live.stopping.store(true, Relaxed);
            ensure!(
                live.rpc.write(&json!({"type":"abort"})).await,
                "pi did not take the stop"
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
        let message = format!(
            "{STEER_LEAD} Take it into account and answer it in this same reply.\n\n{}",
            json!({"message_id":message_id,"text":text})
        );
        Box::pin(async move {
            let Some(live) = self.live_turn(&into.thread_id, &into.turn_id) else {
                return SteerAck::Refused("the pi turn is not running".into());
            };
            match live
                .rpc
                .call(json!({"type":"steer","message":message}), ANSWER)
                .await
            {
                Reply::Done(_) => SteerAck::Accepted,
                Reply::Refused(error) => SteerAck::Refused(error),
                Reply::Unsent => SteerAck::Refused("the pi turn ended".into()),
                Reply::Lost => SteerAck::Unknown,
            }
        })
    }

    fn read_back<'a>(
        &'a self,
        into: &'a SteerTarget,
        message_id: &'a str,
    ) -> BoxFuture<'a, Option<SteerTrace>> {
        Box::pin(async move {
            let pointer = self.pointer(&into.thread_id).ok()??;
            if self.live_turn(&into.thread_id, &into.turn_id).is_some()
                || self.processes.tracked(pointer.group)
            {
                return Some(SteerTrace::Pending);
            }
            let entries = entries(&pointer.file).ok()?;
            let Some(turn) = entries.iter().position(|entry| entry["id"] == into.turn_id) else {
                return Some(SteerTrace::TargetUnseen);
            };
            let run = run(&entries, turn);
            let marker = format!("\"message_id\":{}", json!(message_id));
            let Some(at) = run.iter().position(|entry| {
                is_message(entry, "user") && text(&entry["message"]).contains(&marker)
            }) else {
                return Some(SteerTrace::NotArrived);
            };
            let answered = run[at..]
                .iter()
                .rev()
                .find(|entry| is_message(entry, "assistant"))
                .is_some_and(|entry| entry["message"]["stopReason"] == "stop");
            Some(SteerTrace::Ended(match answered {
                true => SteerOutcome::Answered,
                false => SteerOutcome::Unfinished,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::manager_control::group_alive;
    use super::*;
    use crate::crypto::LocalIdentity;
    use crate::identity::{Genesis, Member, NetworkState};
    use crate::manager::{CodexTurn, Sdk, run_manager};
    use crate::work::WorkState;

    type Turns = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

    fn fake() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-pi")
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vibeface-pi-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// The fake's sessions and log live in `dir`; `mode` picks its turn.
    fn env(dir: &Path, mode: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "PI_CODING_AGENT_SESSION_DIR".into(),
                dir.join("sessions").display().to_string(),
            ),
            ("FAKE_PI_LOG".into(), dir.join("log").display().to_string()),
            ("FAKE_PI_MODE".into(), mode.into()),
        ])
    }

    fn gated_env(dir: &Path, mode: &str) -> BTreeMap<String, String> {
        let mut variables = env(dir, mode);
        variables.insert(
            "FAKE_PI_GATE_DIR".into(),
            dir.join("concurrent-gate").display().to_string(),
        );
        variables.insert(
            "FAKE_PI_GATE_RELEASE".into(),
            dir.join("concurrent-release").display().to_string(),
        );
        variables
    }

    fn log(dir: &Path) -> Vec<Value> {
        std::fs::read_to_string(dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn prompts(dir: &Path) -> Vec<Value> {
        log(dir)
            .into_iter()
            .filter_map(|line| line.get("prompt").cloned())
            .collect()
    }

    fn bot(dir: &Path, pi: &Arc<Pi>, mode: &str, turns: Option<&Turns>) -> ManagerConfig {
        let codex_turn = turns.map(|turns| {
            let turns = turns.clone();
            CodexTurn {
                client_id: "client".into(),
                record: Arc::new(move |session, turn| {
                    turns.lock().unwrap().push((session, turn));
                    Box::pin(async { anyhow::Ok(()) })
                }),
                fresh_prompt: None,
            }
        });
        ManagerConfig {
            sdk: Sdk::Pi {
                bin: fake(),
                pi: pi.clone(),
            },
            workspace: dir.to_owned(),
            model: String::new(),
            timeout_secs: 30,
            custom_tools: BTreeMap::new(),
            tool_cli: None,
            instructions: "Answer briefly.".into(),
            tool_env: BTreeMap::new(),
            tool_token: Some("tool-token-fixture".into()),
            provider_env: env(dir, mode),
            process_started: None,
            codex_turn,
        }
    }

    fn gated_bot(dir: &Path, pi: &Arc<Pi>, mode: &str, client: &str) -> ManagerConfig {
        let mut config = bot(dir, pi, mode, None);
        config.timeout_secs = 30;
        config.provider_env = gated_env(dir, mode);
        config.codex_turn = Some(CodexTurn {
            client_id: client.into(),
            record: Arc::new(|_, _| Box::pin(async { anyhow::Ok(()) })),
            fresh_prompt: None,
        });
        config
    }

    async fn wait_for_gate(dir: &Path) -> Result<()> {
        let gate = dir.join("concurrent-gate");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let count = std::fs::read_dir(&gate)
                    .map(|entries| entries.filter_map(Result::ok).count())
                    .unwrap_or_default();
                if count >= 16 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("all pi turns reached the fixture gate")
    }

    async fn wait_for_abort(dir: &Path) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let aborted = std::fs::read_to_string(dir.join("log"))
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .any(|entry| entry["abort"] == true);
                if aborted {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("the cancelled pi turn received an abort")
    }

    fn release(dir: &Path) {
        std::fs::File::create(dir.join("concurrent-release")).unwrap();
    }

    fn concurrent_pi_turns(
        dir: &Path,
        pi: &Arc<Pi>,
        modes: impl IntoIterator<Item = String>,
    ) -> Vec<tokio::task::JoinHandle<Result<AgentResult>>> {
        modes
            .into_iter()
            .enumerate()
            .map(|(index, mode)| {
                let config = gated_bot(dir, pi, &mode, &format!("client-{index}"));
                let prompt = format!("marker-{index}");
                tokio::spawn(async move {
                    run_manager(
                        &config,
                        &prompt,
                        None,
                        |_| async { anyhow::Ok(()) },
                        tokio::sync::watch::channel(false).1,
                        |_| async { anyhow::Ok(()) },
                    )
                    .await
                })
            })
            .collect()
    }

    /// The turn the bot recorded once pi took its prompt.
    async fn started(turns: &Turns) -> (String, String) {
        loop {
            if let Some((session, Some(turn))) = turns.lock().unwrap().last().cloned() {
                return (session, turn);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn launch(dir: &Path, mode: &str, session: &str) -> Launch {
        Launch {
            bin: fake(),
            cwd: dir.to_owned(),
            env: env(dir, mode),
            args: vec!["--session-id".into(), session.into()],
        }
    }

    fn spec(work_id: &str, target: &str, cwd: &Path) -> WorkSpec {
        WorkSpec {
            work_id: work_id.into(),
            conversation: "c".into(),
            trigger: "message:m1".into(),
            target: target.into(),
            dispatcher: target.into(),
            call_ref: work_id.into(),
            kind: WorkKind::Agent(Provider::Pi),
            cwd: cwd.display().to_string(),
            title: "t".into(),
            prompt: "do it".into(),
            continues: None,
            parent: None,
            thread: None,
        }
    }

    /// A Node in a network of its own whose pi is the fake, and that network.
    async fn node(dir: &Path, mode: &str) -> Result<(Arc<Runtime>, String)> {
        let data = dir.join("node");
        config::private_dir(&data)?;
        let mut configuration = Config::local("127.0.0.1:0".parse()?, vec![], false);
        configuration.providers.execution = [Provider::Pi].into();
        configuration.save(&data)?;
        config::private_write(&data.join("data-epoch.json"), &config::data_epoch_marker())?;
        let identity = LocalIdentity::generate();
        config::private_write(&data.join("identity.json"), &serde_json::to_vec(&identity)?)?;
        let node = Arc::new(Runtime::open(&data).await?);
        let state = NetworkState::new(Genesis::create(
            &LocalIdentity::generate(),
            Member::client(&LocalIdentity::generate(), "admin", true)?,
            Member::node(&node.identity, "node")?,
        )?)?;
        let network = state.genesis.network_id.clone();
        node.install_membership(state).await?;
        let mut local = node.providers.lock().unwrap();
        local.bins.insert(Provider::Pi, fake());
        let variables = env(dir, mode).into_iter().map(|(k, v)| (k, Some(v)));
        local.env.apply(Provider::Pi, variables.collect())?;
        drop(local);
        Ok((node, network))
    }

    /// Dispatches the pi work `w-1` on `node`; the Node sends it itself.
    async fn dispatch(node: &Arc<Runtime>, network: &str, dir: &Path) -> Result<()> {
        let spec = spec("w-1", &node.identity.device_id()?, dir);
        node.store
            .lock()
            .await
            .accept_work(&node.identity, network, spec.clone(), now_ms())?;
        let pi = node.pi.clone();
        let sends = (node.start_work(pi.as_ref(), network, &spec, None, &Value::Null)).await?;
        ensure!(!sends, "the Node sends a pi work itself");
        Ok(())
    }

    /// The work `w-1` once its state is `done`.
    async fn work(node: &Runtime, done: impl Fn(&WorkState) -> bool) -> Result<WorkRecord> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let record = node.store.lock().await.work_record("w-1")?;
            let record = record.context("work")?;
            if done(&record.state) || Instant::now() > deadline {
                return Ok(record);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn a_bot_turn_streams_its_reply_and_the_next_turn_goes_on_in_its_session() -> Result<()> {
        let dir = scratch("bot");
        let pi = Arc::new(Pi::new(&dir));
        let config = bot(&dir, &pi, "reply", None);
        let shown = std::sync::Mutex::new(Vec::new());
        let progress = |update| {
            shown.lock().unwrap().push(update);
            async { anyhow::Ok(()) }
        };
        let (_stop, cancel) = watch::channel(false);
        let persist = |_| async { anyhow::Ok(()) };
        let first = run_manager(&config, "hi", None, persist, cancel.clone(), progress).await?;
        assert_eq!(first.output, "Hello");
        assert_eq!(
            shown.lock().unwrap().clone(),
            [
                json!({"type":"text_delta","item_id":"pi-1.0","text":"Looking"}),
                json!({"type":"text_snapshot","item_id":"pi-1.0","text":"Looking"}),
                json!({"type":"tool","item_id":"call-1","name":"command","status":"running"}),
                json!({"type":"tool","item_id":"call-1","name":"command","status":"completed"}),
                json!({"type":"text_delta","item_id":"pi-2.0","text":"Hel"}),
                json!({"type":"text_delta","item_id":"pi-2.0","text":"lo"}),
                json!({"type":"text_snapshot","item_id":"pi-2.0","text":"Hello"}),
            ]
        );
        let session = first.session_id.context("session")?;
        let next = run_manager(
            &config,
            "more",
            Some(&session),
            persist,
            cancel,
            |_| async { anyhow::Ok(()) },
        )
        .await?;
        assert_eq!(next.session_id.as_deref(), Some(session.as_str()));
        assert_eq!(prompts(&dir), ["hi", "more"]);
        let starts: Vec<_> = log(&dir)
            .into_iter()
            .filter(|line| line["argv"].is_array())
            .collect();
        let argv = |at: usize| serde_json::from_value::<Vec<String>>(starts[at]["argv"].clone());
        let (first, second) = (argv(0)?, argv(1)?);
        assert_eq!(
            first[..5],
            [
                "--mode",
                "rpc",
                "--no-extensions",
                "--session-id",
                session.as_str()
            ]
        );
        let guide = &first[first
            .iter()
            .position(|a| a == "--append-system-prompt")
            .context("guide")?
            + 1];
        assert_eq!(std::fs::read_to_string(guide)?, "Answer briefly.");
        assert_eq!(second[3], "--session");
        assert!(second[4].ends_with(&format!("_{session}.jsonl")));
        assert_eq!(starts[0]["run"], "1");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_message_steered_into_the_running_reply_is_answered_in_it() -> Result<()> {
        let dir = scratch("steer");
        let pi = Arc::new(Pi::new(&dir));
        let turns = Turns::default();
        let config = bot(&dir, &pi, "wait", Some(&turns));
        let (_stop, cancel) = watch::channel(false);
        let reply = run_manager(
            &config,
            "hi",
            None,
            |_| async { anyhow::Ok(()) },
            cancel,
            |_| async { anyhow::Ok(()) },
        );
        let steer = async {
            let (thread_id, turn_id) = started(&turns).await;
            let into = SteerTarget {
                provider: Provider::Pi,
                attempt_id: "a".into(),
                thread_id,
                turn_id,
            };
            assert_eq!(
                pi.steer(&into, "m-1", "also this").await,
                SteerAck::Accepted
            );
            into
        };
        let (reply, into) = tokio::join!(reply, steer);
        assert_eq!(reply?.output, "Got it");
        assert_eq!(
            pi.read_back(&into, "m-1").await,
            Some(SteerTrace::Ended(SteerOutcome::Answered))
        );
        assert_eq!(
            pi.read_back(&into, "m-2").await,
            Some(SteerTrace::NotArrived)
        );
        assert_eq!(
            pi.steer(&into, "m-3", "late").await,
            SteerAck::Refused("the pi turn is not running".into())
        );
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn stopping_a_bot_turn_aborts_it_in_pi() -> Result<()> {
        let dir = scratch("stop");
        let pi = Arc::new(Pi::new(&dir));
        let turns = Turns::default();
        let config = bot(&dir, &pi, "wait", Some(&turns));
        let (stop, cancel) = watch::channel(false);
        let reply = run_manager(
            &config,
            "hi",
            None,
            |_| async { anyhow::Ok(()) },
            cancel,
            |_| async { anyhow::Ok(()) },
        );
        let stopping = async {
            started(&turns).await;
            stop.send(true).unwrap();
        };
        let (reply, ()) = tokio::join!(reply, stopping);
        let Err(error) = reply else {
            panic!("a stopped turn has no reply");
        };
        assert!(
            error
                .downcast_ref::<ManagerError>()
                .context("manager error")?
                .cancelled()
        );
        let session = turns.lock().unwrap()[0].0.clone();
        let file = pi.pointer(&session)?.context("pointer")?.file;
        let last = entries(&file)?.pop().context("entry")?;
        assert_eq!(last["message"]["stopReason"], "aborted");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn sixteen_pi_turns_overlap_and_keep_their_outputs() -> Result<()> {
        let dir = scratch("concurrent");
        let pi = Arc::new(Pi::new(&dir));
        let tasks = concurrent_pi_turns(&dir, &pi, (0..16).map(|_| String::from("reply")));
        wait_for_gate(&dir).await?;
        release(&dir);
        let mut sessions = std::collections::HashSet::new();
        for (index, task) in tasks.into_iter().enumerate() {
            let result = task.await??;
            sessions.insert(result.session_id.context("pi session")?);
            assert_eq!(result.output, format!("done: marker-{index}"));
        }
        assert_eq!(sessions.len(), 16);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_one_of_sixteen_pi_turns_leaves_the_others_running() -> Result<()> {
        let dir = scratch("concurrent-cancel");
        let pi = Arc::new(Pi::new(&dir));
        let mut tasks = Vec::with_capacity(16);
        let mut stops = Vec::with_capacity(16);
        for index in 0..16 {
            let config = gated_bot(&dir, &pi, "reply", &format!("client-{index}"));
            let prompt = format!("marker-{index}");
            let (stop, cancel) = tokio::sync::watch::channel(false);
            stops.push(stop);
            tasks.push(tokio::spawn(async move {
                run_manager(
                    &config,
                    &prompt,
                    None,
                    |_| async { anyhow::Ok(()) },
                    cancel,
                    |_| async { anyhow::Ok(()) },
                )
                .await
            }));
        }
        wait_for_gate(&dir).await?;
        stops[0].send(true)?;
        wait_for_abort(&dir).await?;
        release(&dir);
        for (index, task) in tasks.into_iter().enumerate() {
            let result = task.await?;
            if index == 0 {
                let error = result.err().context("cancelled pi turn")?;
                assert!(
                    error
                        .downcast_ref::<ManagerError>()
                        .context("manager error")?
                        .cancelled()
                );
            } else {
                assert_eq!(result?.output, format!("done: marker-{index}"));
            }
        }
        assert_eq!(
            log(&dir)
                .iter()
                .filter(|entry| entry["abort"] == true)
                .count(),
            1
        );
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn one_failed_pi_turn_does_not_stop_the_other_fifteen() -> Result<()> {
        let dir = scratch("concurrent-failure");
        let pi = Arc::new(Pi::new(&dir));
        let modes = (0..16).map(|index| {
            if index == 0 {
                String::from("fail")
            } else {
                String::from("reply")
            }
        });
        let tasks = concurrent_pi_turns(&dir, &pi, modes);
        wait_for_gate(&dir).await?;
        release(&dir);
        for (index, task) in tasks.into_iter().enumerate() {
            let result = task.await?;
            if index == 0 {
                let error = result.err().context("failed pi turn")?;
                assert_eq!(
                    error
                        .downcast_ref::<ManagerError>()
                        .context("manager error")?
                        .outcome,
                    ManagerFailure::Failed
                );
            } else {
                assert_eq!(result?.output, format!("done: marker-{index}"));
            }
        }
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_dispatched_work_runs_in_a_new_session_and_finishes_with_its_reply() -> Result<()> {
        let dir = scratch("work");
        let (node, network) = node(&dir, "reply").await?;
        dispatch(&node, &network, &dir).await?;
        let record = work(&node, WorkState::finished).await?;
        assert_eq!(
            record.state,
            WorkState::Finished {
                outcome: WorkOutcome::Completed
            }
        );
        assert_eq!(record.text, "Hello");
        assert_eq!(prompts(&dir), ["do it"]);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_work_given_a_thread_goes_on_in_that_session() -> Result<()> {
        let dir = scratch("resume");
        let (node, network) = node(&dir, "reply").await?;
        dispatch(&node, &network, &dir).await?;
        work(&node, WorkState::finished).await?;
        let session = (node.store.lock().await.works(&network, "c")?)
            .into_iter()
            .find_map(|work| work.thread_id)
            .context("a session")?;
        let file = node.pi.pointer(&session)?.context("pointer")?.file;
        let spec = WorkSpec {
            prompt: "again".into(),
            thread: Some(session.clone()),
            ..spec("w-2", &node.identity.device_id()?, &dir)
        };
        node.store
            .lock()
            .await
            .accept_work(&node.identity, &network, spec.clone(), now_ms())?;
        let pi = node.pi.clone();
        (node.start_work(pi.as_ref(), &network, &spec, Some(&session), &Value::Null)).await?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let record = loop {
            let record = node.store.lock().await.work_record("w-2")?.context("w-2")?;
            if record.state.finished() || Instant::now() > deadline {
                break record;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(
            record.state,
            WorkState::Finished {
                outcome: WorkOutcome::Completed
            }
        );
        assert_eq!(prompts(&dir), ["do it", "again"]);
        assert_eq!(node.pi.pointer(&session)?.context("pointer")?.file, file);
        let users = entries(&file)?
            .iter()
            .filter(|entry| is_message(entry, "user"))
            .count();
        assert_eq!(users, 2, "both prompts are in the one session");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn interrupting_a_work_aborts_its_turn_in_pi() -> Result<()> {
        let dir = scratch("interrupt");
        let (node, network) = node(&dir, "wait").await?;
        dispatch(&node, &network, &dir).await?;
        let running = work(&node, |state| state.handle().is_some()).await?;
        node.pi.interrupt(&running).await?;
        let record = work(&node, WorkState::finished).await?;
        assert_eq!(
            record.state,
            WorkState::Finished {
                outcome: WorkOutcome::Interrupted
            }
        );
        assert_eq!(record.text, "Working");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_work_whose_pi_died_is_read_back_from_its_session_and_never_sent_again() -> Result<()>
    {
        let dir = scratch("crash");
        let (node, network) = node(&dir, "crash").await?;
        dispatch(&node, &network, &dir).await?;
        let record = work(&node, WorkState::finished).await?;
        assert_eq!(
            record.state,
            WorkState::Finished {
                outcome: WorkOutcome::Interrupted
            }
        );
        assert_eq!(prompts(&dir), ["do it"]);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    async fn ended(group: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while group_alive(group) {
            if Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        true
    }

    #[tokio::test]
    async fn a_restart_ends_the_pi_its_last_run_left_and_reads_the_work_back() -> Result<()> {
        use std::os::unix::process::CommandExt as _;
        let dir = scratch("restart");
        let pi = Arc::new(Pi::new(&dir));
        let (sender, mut reports) = mpsc::unbounded_channel();
        let prompt = json!({"type":"prompt","message":"do it"});
        tokio::spawn(pi.clone().run_work(
            launch(&dir, "wait", "s-1"),
            "s-1".into(),
            prompt,
            sender,
        ));
        while !matches!(
            reports.recv().await,
            Some(Report::Signal(Signal::TurnStarted { .. })) | None
        ) {}
        let left = pi.pointer("s-1")?.context("pointer")?.group;
        let mut stranger = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()?;
        let pidfile = dir.join(PIDFILE);
        let mut recorded = std::fs::read_to_string(&pidfile).unwrap_or_default();
        recorded.push_str(&format!("{} {}\n", stranger.id(), fake().display()));
        std::fs::write(&pidfile, recorded)?;
        let restarted = Pi::new(&dir);
        assert!(ended(left).await, "the restart ended the pi it left");
        assert!(!pidfile.exists());
        assert!(
            stranger.try_wait()?.is_none(),
            "a process the Node did not start is left alone"
        );
        let mut pointer = restarted.pointer("s-1")?.context("pointer")?;
        pointer.group = stranger.id();
        config::private_write(
            &restarted.pointer_path("s-1")?,
            &serde_json::to_vec(&pointer)?,
        )?;
        let (mut work, _) = crate::work::accept(spec("w-1", "t", &dir), 0);
        crate::work::transition(&mut work, Signal::ThreadCreated("s-1".into()));
        let probe = restarted.probe(&work).await?;
        assert!(
            matches!(probe, Probe::Terminal { .. }),
            "a reused group is not the work's pi: {probe:?}"
        );
        assert_eq!(prompts(&dir), ["do it"]);
        stranger.kill()?;
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn shutting_the_node_down_ends_its_running_pi() -> Result<()> {
        let dir = scratch("shutdown");
        let (node, network) = node(&dir, "wait").await?;
        dispatch(&node, &network, &dir).await?;
        let running = work(&node, |state| state.handle().is_some()).await?;
        let session = running.state.thread().context("thread")?;
        let group = node.pi.pointer(session)?.context("pointer")?.group;
        node.shutdown().await;
        assert!(ended(group).await, "the shutdown ended the running pi");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_session_without_the_prompt_reads_as_not_started() -> Result<()> {
        let dir = scratch("unsent");
        let pi = Pi::new(&dir);
        let (mut work, _) = crate::work::accept(spec("w-1", "t", &dir), 0);
        crate::work::transition(&mut work, Signal::ThreadCreated("s-1".into()));
        assert_eq!(pi.probe(&work).await?, Probe::NotStarted);
        let (process, _) = pi.open(&launch(&dir, "reply", "s-1"), "s-1").await?;
        assert!(pi.probe(&work).await.is_err());
        process.end().await;
        assert!(pi.pointer("s-1")?.is_some());
        assert_eq!(pi.probe(&work).await?, Probe::NotStarted);
        assert!(prompts(&dir).is_empty());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_prompt_pi_refuses_rejects_the_work() -> Result<()> {
        let dir = scratch("refuse");
        let pi = Arc::new(Pi::new(&dir));
        let (sender, mut reports) = mpsc::unbounded_channel();
        let prompt = json!({"type":"prompt","message":"do it"});
        pi.run_work(launch(&dir, "refuse", "s-1"), "s-1".into(), prompt, sender)
            .await;
        let Some(Report::Signal(Signal::StartRejected(failure))) = reports.recv().await else {
            panic!("the work was not rejected");
        };
        assert_eq!(failure.kind, FailureKind::MissingKey);
        assert!(reports.recv().await.is_none());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn the_extension_stays_out_of_the_runs_the_node_starts() {
        let Some(crate::providers::Watch::Extension { source, .. }) = Provider::Pi.watch() else {
            panic!("pi is watched through its extension");
        };
        assert!(source.contains(&format!("if (process.env.{RUN_ENV}) return;")));
    }
}
