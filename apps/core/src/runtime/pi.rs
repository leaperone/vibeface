//! pi runs bot turns and dispatched work through `pi --mode rpc`, one process per turn: no
//! process idles between turns, and each turn starts with its own environment and instructions.
//! Before a prompt is sent the Node keeps where pi writes the session, so after a restart it
//! reads the turn back from that file and never sends it again.
use super::claude::{Guide, Processes};
use super::executor::{Executor, SteerAck, SteerInput, SteerTrace};
use super::provider_adapter::bounded;
use super::*;
use crate::manager::{CodexTurn, ManagerConfig, ManagerError, ManagerFailure};
use crate::scheduling::{SteerOutcome, SteerTarget};
use crate::work::{Probe, Signal, WorkRecord, WorkSpec};
use base64::Engine as _;
use futures_util::future::BoxFuture;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, watch};
use vibeface_bridge::agent::{self, AgentResult};
use vibeface_bridge::failure::ProviderStage;
use vibeface_bridge::failure::{ExecFailure, FailureKind};
use vibeface_bridge::platform::tree;
use vibeface_bridge::turn::TurnStatus;
use vibeface_bridge::transport;

/// Process groups of the running pi processes, with the program each one runs.
const PIDFILE: &str = "pi.pid";
/// Names a session's process tree in this account's job object namespace.
const TREE_KEY: &str = "pi-session";
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
    /// Session pointers and private guides whose processes still read them.
    dir: PathBuf,
    sessions: PathBuf,
    processes: Arc<Processes>,
    live: std::sync::Mutex<HashMap<String, Live>>,
    preparing: std::sync::Mutex<HashMap<String, watch::Sender<bool>>>,
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

/// A work's cancellation entry stays registered until its process ends.
struct Preparing(Arc<Pi>, String);

impl Drop for Preparing {
    fn drop(&mut self) {
        lock(&self.0.preparing).remove(&self.1);
    }
}

struct WorkFirstInput {
    node: Arc<Runtime>,
    network: String,
    work: WorkSpec,
    session: String,
}

struct PromptWrite<'a> {
    work: Option<&'a WorkFirstInput>,
    cancel: &'a watch::Receiver<bool>,
    writing: &'a AtomicBool,
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

struct LiveTurn<'a> {
    owner: &'a Pi,
    session: String,
    turn: String,
}

impl Drop for LiveTurn<'_> {
    fn drop(&mut self) {
        let mut live = lock(&self.owner.live);
        if live
            .get(&self.session)
            .is_some_and(|live| live.turn == self.turn)
        {
            live.remove(&self.session);
        }
    }
}

/// Where pi writes a session, kept before a prompt is sent into it.
#[derive(serde::Serialize, serde::Deserialize)]
struct Pointer {
    file: PathBuf,
    /// The directory pi runs the session in; a later work in the session runs there too.
    cwd: PathBuf,
    /// The process tree of the session's latest pi; while this Node run tracks it, the turn
    /// may go on.
    tree: String,
    /// The session's last entry before that prompt; `None` when it had none.
    after: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ReplyPointer {
    session: String,
    file: PathBuf,
    after: Option<String>,
    prompt_hash: String,
}

type PendingResponses =
    std::sync::Mutex<Option<HashMap<String, oneshot::Sender<transport::HeldJson>>>>;

/// One pi process's commands: each goes out with an ID and its response comes back by it.
struct Rpc {
    stdin: Mutex<Option<ChildStdin>>,
    /// `None` once pi closed its output.
    waiting: PendingResponses,
    responses: transport::JsonBudget,
    commands: transport::JsonBudget,
    next: AtomicU64,
}

struct PendingReply<'a> { rpc: &'a Rpc, id: String }
impl Drop for PendingReply<'_> {
    fn drop(&mut self) {
        if let Some(waiting) = lock(&self.rpc.waiting).as_mut() { waiting.remove(&self.id); }
    }
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
    guide: Option<Arc<Guide>>,
}

struct Process {
    child: Child,
    tree: tree::Tree,
    reaped: bool,
    processes: Arc<Processes>,
    rpc: Arc<Rpc>,
    /// Everything pi writes but the responses.
    events: transport::JsonReceiver,
    output: tokio::task::JoinHandle<()>,
    /// The end of what pi wrote to stderr, once it closed it.
    stderr: Option<tokio::task::JoinHandle<String>>,
    _guide: Option<Arc<Guide>>,
}

impl Drop for Process {
    fn drop(&mut self) {
        self.output.abort();
        if !self.reaped && (cfg!(windows) || self.child.id() == Some(self.tree.root())) {
            self.tree.kill();
        }
        if let Some(stderr) = self.stderr.take() {
            stderr.abort();
        }
    }
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

// Only these signals are produced by run_work. New report variants must supply a byte
// estimate before entering this queue; an unknown variant forces reconciliation.
fn report_bytes(report: &Report) -> usize {
    let payload = match report {
        Report::Probe => 0,
        Report::Progress(value) => transport::value_bytes(value),
        Report::Signal(signal) => match signal {
            Signal::TurnStarted { turn_id, .. } => turn_id.capacity(),
            Signal::StartRejected(error) => error.detail.capacity(),
            Signal::Progress { summary, .. } => summary.capacity(),
            Signal::Text(text) => text.capacity(),
            Signal::TurnCompleted { turn_id, text, error, .. } => turn_id.capacity()
                .saturating_add(text.capacity())
                .saturating_add(error.as_ref().map_or(0, |error| error.detail.capacity())),
            _ => usize::MAX,
        },
    };
    std::mem::size_of::<Report>().saturating_add(payload)
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
            "auto_compaction_end" if event["aborted"] != true && event["error"].is_null() => {
                vec![json!({"type":"context_compacted"})]
            }
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

/// A frame owns stdin until all its bytes and flush complete. Cancelling `write_all` may
/// leave a prefix on the pipe, so an interrupted writer must never return to the input slot.
struct InFlightFrame<'a, W> {
    pipe: Option<W>,
    slot: &'a mut Option<W>,
    waiting: &'a PendingResponses,
}

impl<W> InFlightFrame<'_, W> {
    fn complete(mut self) {
        *self.slot = self.pipe.take();
    }
}

impl<W> Drop for InFlightFrame<'_, W> {
    fn drop(&mut self) {
        if self.pipe.is_some() {
            *lock(self.waiting) = None;
            tracing::warn!("pi input frame interrupted; closing stdin and reconciling native session");
        }
        // An incomplete frame's pipe closes here. Its native process is left for readback.
    }
}

async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    stdin: &mut Option<W>,
    waiting: &PendingResponses,
    bytes: &[u8],
    started: &AtomicBool,
    prompt_started: Option<&AtomicBool>,
) -> Result<(), Reply> {
    let Some(pipe) = stdin.take() else { return Err(Reply::Unsent); };
    let mut frame = InFlightFrame { pipe: Some(pipe), slot: stdin, waiting };
    started.store(true, Relaxed);
    if let Some(prompt_started) = prompt_started { prompt_started.store(true, Relaxed); }
    let pipe = frame.pipe.as_mut().expect("in-flight frame owns stdin");
    pipe.write_all(bytes).await.map_err(|_| Reply::Lost)?;
    pipe.flush().await.map_err(|_| Reply::Lost)?;
    frame.complete();
    Ok(())
}

async fn write_command<W: tokio::io::AsyncWrite + Unpin>(
    stdin: &Mutex<Option<W>>,
    waiting: &PendingResponses,
    line: &Value,
    started: &AtomicBool,
) -> Result<(), Reply> {
    let mut stdin = stdin.lock().await;
    let mut bytes = transport::json(line, transport::FRAME_BYTES)
        .map_err(|error| Reply::Refused(error.to_string()))?;
    bytes.push(b'\n');
    write_frame(&mut stdin, waiting, &bytes, started, None).await
}

impl Rpc {
    async fn call(&self, command: Value, within: Duration) -> Reply {
        self.call_with_prompt(command, within, None).await
    }

    async fn call_with_prompt(
        &self,
        mut command: Value,
        within: Duration,
        prompt: Option<PromptWrite<'_>>,
    ) -> Reply {
        let id = format!("vf-{}", self.next.fetch_add(1, Relaxed));
        command["id"] = json!(id);
        let command = match self.commands.try_hold(command) {
            Ok(command) => command,
            Err(error) => return Reply::Refused(error.to_string()),
        };
        let (sender, answer) = oneshot::channel();
        match lock(&self.waiting).as_mut() {
            Some(waiting) if waiting.len() < transport::QUEUE_ITEMS => waiting.insert(id.clone(), sender),
            Some(_) => return Reply::Refused("pi has too many pending commands".into()),
            None => return Reply::Unsent,
        };
        let _pending = PendingReply { rpc: self, id };
        let write_started = AtomicBool::new(false);
        let written = tokio::time::timeout(within, async { match prompt {
            Some(prompt) => self.write_prompt(&command.value, prompt, &write_started).await,
            None => write_command(&self.stdin, &self.waiting, &command.value, &write_started).await,
        }}).await.unwrap_or_else(|_| Err(if write_started.load(Relaxed) {
            Reply::Lost
        } else {
            Reply::Unsent
        }));
        if let Err(reply) = written { return reply; }
        let answer = tokio::time::timeout(within, answer).await;
        match answer {
            Ok(Ok(response)) if response.value["success"] == true => {
                Reply::Done(response.value["data"].clone())
            }
            Ok(Ok(response)) => Reply::Refused(
                response.value["error"]
                    .as_str()
                    .unwrap_or("pi refused the command")
                    .into(),
            ),
            Ok(Err(_)) | Err(_) => Reply::Lost,
        }
    }

    async fn write_prompt(
        &self,
        line: &Value,
        prompt: PromptWrite<'_>,
        started: &AtomicBool,
    ) -> Result<(), Reply> {
        let mut stdin = self.stdin.lock().await;
        let mut bytes = transport::json(line, transport::FRAME_BYTES)
            .map_err(|error| Reply::Refused(error.to_string()))?;
        bytes.push(b'\n');
        if *prompt.cancel.borrow() {
            return Err(Reply::Refused("stopped before pi took the prompt".into()));
        }
        if stdin.is_none() { return Err(Reply::Unsent); }
        let _lease = match prompt.work {
            Some(work) => Some(
                work.node
                    .work_first_input_lease(&work.network, &work.work, &work.session)
                    .await
                    .map_err(|error| Reply::Refused(format!("{error:#}")))?,
            ),
            None => None,
        };
        if *prompt.cancel.borrow() {
            return Err(Reply::Refused("stopped before pi took the prompt".into()));
        }
        write_frame(&mut stdin, &self.waiting, &bytes, started, Some(prompt.writing)).await
    }

    /// One JSON line on pi's input; false when pi no longer reads it.
    async fn write(&self, line: &Value) -> bool {
        let started = AtomicBool::new(false);
        write_command(&self.stdin, &self.waiting, line, &started).await.is_ok()
    }
}

/// Routes pi's output: responses to their callers, the rest as events. With no extensions
/// loaded, nothing in it waits for an answer.
async fn read_output(
    stdout: impl tokio::io::AsyncRead + Unpin,
    rpc: Arc<Rpc>,
    events: transport::JsonSender,
) {
    let mut lines = transport::Lines::new(tokio::io::BufReader::new(stdout), transport::FRAME_BYTES);
    let result = async {
        while let Some(line) = lines.next_line().await? {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message["type"] != "response" {
                // Waiting here can strand the response to the prompt/get_entries/abort RPC
                // behind progress on the same pipe. Pressure makes the outcome unknown.
                events.try_send(message)?;
                continue;
            }
            let waiting = message["id"]
                .as_str()
                .and_then(|id| lock(&rpc.waiting).as_mut()?.remove(id));
            if let Some(waiting) = waiting {
                let _ = waiting.send(rpc.responses.try_hold(message)?);
            }
        }
        std::io::Result::Ok(())
    }.await;
    if let Err(error) = result {
        tracing::warn!(%error, "pi output transport stopped; reconcile native session");
        events.fail(error);
    }
    *lock(&rpc.waiting) = None;
}

impl Process {
    fn spawn(
        launch: &Launch,
        processes: &Arc<Processes>,
        sessions: &Path,
    ) -> Result<Self, ExecFailure> {
        if !launch.cwd.is_dir() {
            return Err(ExecFailure::new(
                FailureKind::Rejected,
                format!("{} is not a directory", launch.cwd.display()),
            ));
        }
        let mut command = std::process::Command::new(&launch.bin);
        config::private_dir(sessions)
            .map_err(|error| ExecFailure::new(FailureKind::Rejected, error.to_string()))?;
        command
            .args(["--mode", "rpc", "--no-extensions"])
            .args(&launch.args)
            .args(["--session-dir".as_ref(), sessions.as_os_str()])
            .current_dir(&launch.cwd)
            .envs(&launch.env)
            .env("PI_CODING_AGENT_SESSION_DIR", sessions)
            .env(RUN_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (tree, mut child) = tree::Tree::spawn(command, TREE_KEY, true).map_err(|error| {
            let kind = match error
                .downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind)
            {
                Some(std::io::ErrorKind::NotFound) => FailureKind::MissingBinary,
                _ => FailureKind::Unknown,
            };
            ExecFailure::new(
                kind,
                format!("could not start pi at {}: {error:#}", launch.bin.display()),
            )
        })?;
        if let Some(guide) = &launch.guide {
            guide.started(tree.token());
        }
        processes
            .track(tree.token(), &launch.bin)
            .map_err(|error| ExecFailure::new(FailureKind::Unknown, format!("{error:#}")))?;
        let (Some(stdin), Some(stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(ExecFailure::new(FailureKind::Unknown, "pi has no pipes"));
        };
        let rpc = Arc::new(Rpc {
            stdin: Mutex::new(Some(stdin)),
            waiting: std::sync::Mutex::new(Some(HashMap::new())),
            responses: transport::JsonBudget::new(transport::QUEUE_BYTES),
            commands: transport::JsonBudget::new(transport::QUEUE_BYTES),
            next: AtomicU64::new(1),
        });
        let (sender, events) = transport::json_channel(transport::QUEUE_ITEMS, transport::QUEUE_BYTES);
        let output = tokio::spawn(read_output(stdout, rpc.clone(), sender));
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
            tree,
            reaped: false,
            processes: processes.clone(),
            rpc,
            events,
            output,
            stderr: Some(stderr),
            _guide: launch.guide.clone(),
        })
    }

    /// Ends the process: closing its input shuts pi down, and one that does not exit in time is
    /// ended with its tree. Returns the end of its stderr.
    async fn end(mut self) -> String {
        self.rpc.stdin.lock().await.take();
        let _ =
            tokio::time::timeout(Duration::from_secs(5), tree::observe_exit(&mut self.child)).await;
        self.reaped = agent::terminate_process_tree(&mut self.child, &mut self.tree).await;
        if self.reaped {
            self.processes.forget(self.tree.token());
        }
        let Some(stderr) = self.stderr.take() else {
            return String::new();
        };
        match tokio::time::timeout(Duration::from_secs(1), stderr).await {
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
                return self.events.failure().is_none();
            }
        }
        false
    }

    /// Sends the prompt and waits until pi keeps its user message, whose entry ID names the
    /// turn. Once the write may have started, cancellation leaves its outcome for a probe.
    async fn begin(
        &mut self,
        after: Option<&str>,
        prompt: Value,
        within: Duration,
        cancel: &mut watch::Receiver<bool>,
        first_input: Option<&WorkFirstInput>,
    ) -> Begin {
        let stopped =
            || ExecFailure::new(FailureKind::Rejected, "stopped before pi took the prompt");
        if *cancel.borrow() {
            return Begin::Refused(stopped());
        }
        let writing = AtomicBool::new(false);
        let write_cancel = cancel.clone();
        let begin = self.take_prompt(
            after,
            prompt,
            within,
            PromptWrite {
                work: first_input,
                cancel: &write_cancel,
                writing: &writing,
            },
        );
        tokio::select! {
            biased;
            _ = agent::wait_for_cancel(cancel) => {
                if writing.load(Relaxed) {
                    Begin::Unknown(ExecFailure::new(
                        FailureKind::Unknown,
                        "stopped while pi may have taken the prompt",
                    ))
                } else {
                    Begin::Refused(stopped())
                }
            }
            result = begin => result,
        }
    }

    async fn take_prompt(
        &mut self,
        after: Option<&str>,
        prompt: Value,
        within: Duration,
        write: PromptWrite<'_>,
    ) -> Begin {
        let rpc = self.rpc.clone();
        let reply = rpc.call_with_prompt(prompt, within, Some(write)).await;
        match reply {
            Reply::Done(data) if data["disposition"] == "handled" => {
                return Begin::Refused(ExecFailure::new(
                    FailureKind::Rejected,
                    "pi took the prompt as a command; nothing ran",
                ));
            }
            Reply::Done(_) => {}
            Reply::Refused(error) => {
                return Begin::Refused(failure(error, FailureKind::Rejected));
            }
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
                        self.events.failure().unwrap_or_else(|| "pi stopped before the turn started".into()),
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
    pub(super) async fn native_directory(&self, session: &str) -> Result<PathBuf> {
        let cwd = self
            .pointer(session)?
            .context("the pi session has no local pointer")?
            .cwd;
        ensure!(
            cwd.is_absolute(),
            "the pi session directory is not absolute"
        );
        Ok(cwd)
    }

    /// Ends the pi processes an earlier Node run left, so their turns are read back as they are.
    pub(crate) async fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("pi"),
            sessions: ProviderRoots::new(data_dir).pi_sessions,
            processes: Arc::new(Processes::open(data_dir.join(PIDFILE)).await),
            live: Default::default(),
            preparing: Default::default(),
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

    fn reply_pointer_path(&self, client_id: &str) -> Result<PathBuf> {
        vibeface_protocol::validate_id(client_id).map_err(anyhow::Error::msg)?;
        Ok(self.dir.join("replies").join(format!(
            "{}.json",
            crate::crypto::digest(client_id.as_bytes())
        )))
    }

    fn record_reply(&self, session: &str, client_id: &str, prompt: &str) -> Result<()> {
        let pointer = self
            .pointer(session)?
            .context("pi session has no local pointer")?;
        let reply = ReplyPointer {
            session: session.into(),
            file: pointer.file,
            after: pointer.after,
            prompt_hash: crate::crypto::digest(prompt.as_bytes()),
        };
        let path = self.reply_pointer_path(client_id)?;
        config::private_dir(path.parent().context("pi reply locator has no parent")?)?;
        config::private_write(&path, &serde_json::to_vec(&reply)?)
    }

    pub(crate) async fn probe_reply(
        &self,
        session: &str,
        native_turn: Option<&str>,
        client_id: &str,
    ) -> Result<Probe> {
        vibeface_protocol::validate_id(session).map_err(anyhow::Error::msg)?;
        let pointer: ReplyPointer = match std::fs::read(self.reply_pointer_path(client_id)?) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Probe::Missing);
            }
            Err(error) => return Err(error.into()),
        };
        if pointer.session != session {
            return Ok(Probe::Missing);
        }
        let entries = entries(&pointer.file)?;
        let start = match &pointer.after {
            Some(after) => match entries.iter().position(|entry| entry["id"] == *after) {
                Some(at) => at + 1,
                None => return Ok(Probe::Missing),
            },
            None => 0,
        };
        let Some(turn) = entries[start..]
            .iter()
            .position(|entry| is_message(entry, "user"))
            .map(|at| start + at)
        else {
            return Ok(Probe::Missing);
        };
        let Some(turn_id) = entries[turn]["id"].as_str() else {
            return Ok(Probe::Missing);
        };
        if native_turn.is_some_and(|native| native != turn_id)
            || crate::crypto::digest(text(&entries[turn]["message"]).as_bytes())
                != pointer.prompt_hash
        {
            return Ok(Probe::Missing);
        }
        if self.live_turn(session, turn_id).is_some() {
            return Ok(Probe::Running {
                turn_id: turn_id.into(),
                epoch: 0,
            });
        }
        let last = run(&entries, turn)
            .iter()
            .rev()
            .find(|entry| is_message(entry, "assistant"))
            .map(|entry| &entry["message"]);
        let Some((status, error)) = last.and_then(ending) else {
            return Ok(Probe::Missing);
        };
        Ok(Probe::Terminal {
            turn_id: turn_id.into(),
            status,
            text: last.map(text).unwrap_or_default(),
            error,
        })
    }

    /// Keeps where pi writes `session`, the directory it runs in and the session's last entry
    /// so far.
    fn keep(&self, session: &str, file: &Path, cwd: &Path, tree: &str) -> Result<Option<String>> {
        let after = entries(file)?
            .last()
            .and_then(|entry| entry["id"].as_str())
            .map(str::to_owned);
        let pointer = Pointer {
            file: file.to_owned(),
            cwd: cwd.to_owned(),
            tree: tree.to_owned(),
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
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<(Process, Option<String>), ManagerError> {
        let before_start = |stage, outcome, failure| ManagerError {
            stage,
            outcome,
            start: crate::manager::StartEvidence::NotStarted,
            session_id: None,
            failure,
        };
        let stopped = || {
            ExecFailure::new(
                FailureKind::Rejected,
                "stopped before pi opened the session",
            )
        };
        if *cancel.borrow() {
            return Err(before_start(
                ProviderStage::SessionStart,
                ManagerFailure::Cancelled,
                stopped(),
            ));
        }
        let process =
            Process::spawn(launch, &self.processes, &self.sessions).map_err(|failure| {
                before_start(ProviderStage::Launch, ManagerFailure::Failed, failure)
            })?;
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
            self.keep(session, &file, &launch.cwd, process.tree.token())
                .map_err(|error| own(format!("keep where pi writes the session: {error:#}")))
        };
        let opened = tokio::select! {
            biased;
            _ = agent::wait_for_cancel(cancel) => Err(stopped()),
            result = opened => result,
        };
        match opened {
            Ok(after) => Ok((process, after)),
            Err(failure) => {
                let tail = process.end().await;
                Err(before_start(
                    ProviderStage::SessionStart,
                    if *cancel.borrow() {
                        ManagerFailure::Cancelled
                    } else {
                        ManagerFailure::Failed
                    },
                    with_tail(failure, &tail),
                ))
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
        let start = std::sync::Mutex::new(crate::manager::StartEvidence::NotStarted);
        let stage = vibeface_bridge::failure::ProviderTrace::new("pi", ProviderStage::SessionStart);
        let error = ManagerError::reporter(&stage, &start, &secrets);
        let own = |detail: String| ExecFailure::new(FailureKind::Unknown, detail);
        let stopped = |session: Option<&str>| {
            error(
                ManagerFailure::Cancelled,
                session,
                own("stopped before the pi turn started".into()),
            )
        };
        let resumed = match previous {
            Some(id) => self
                .pointer(id)
                .map_err(|error| ManagerError::not_started_at(ProviderStage::Record, error))?
                .filter(|pointer| pointer.file.is_file()),
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
        let guide = if guide.is_empty() {
            None
        } else {
            let guide = Guide::write(&self.dir.join("guides"), &guide)
                .map_err(|error| ManagerError::not_started_at(ProviderStage::Record, error))?;
            args.extend([
                "--append-system-prompt".into(),
                guide.path.display().to_string(),
            ]);
            Some(guide)
        };
        if !config.model.is_empty() {
            args.extend(["--model".into(), config.model.clone()]);
        }
        if let Some(effort) = &config.reasoning_effort {
            args.extend(["--thinking".into(), effort.clone()]);
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
            guide,
        };
        if *cancel.borrow() {
            return Err(stopped(previous));
        }
        let (mut process, after) =
            self.open(&launch, &session, &mut cancel)
                .await
                .map_err(|failure| {
                    stage.enter(failure.stage);
                    error(failure.outcome, previous, failure.failure)
                })?;
        let native = Some(session.as_str());
        stage.enter(ProviderStage::Record);
        if let Some(turn) = &config.codex_turn
            && let Err(e) = self.record_reply(&session, &turn.client_id, prompt)
        {
            process.end().await;
            return Err(error(
                ManagerFailure::Failed,
                native,
                own(format!("record the pi reply locator: {e}")),
            ));
        }
        if let Some(started) = &config.process_started
            && let Err(e) = started(process.tree.root()).await
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
        stage.enter(ProviderStage::TurnStart);
        *start.lock().unwrap() = crate::manager::StartEvidence::Unknown;
        let turn = match process
            .begin(after.as_deref(), prompt, within, &mut cancel, None)
            .await
        {
            Begin::Started(turn) => turn,
            Begin::Refused(failure) => {
                *start.lock().unwrap() = crate::manager::StartEvidence::NotStarted;
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
        *start.lock().unwrap() = crate::manager::StartEvidence::Started;
        let mut seen = Seen::default();
        lock(&self.live).insert(
            session.clone(),
            Live {
                turn: turn.clone(),
                rpc: process.rpc.clone(),
                stopping: Default::default(),
            },
        );
        let live = LiveTurn {
            owner: self,
            session: session.clone(),
            turn: turn.clone(),
        };
        stage.enter(ProviderStage::Record);
        let recorded = match &config.codex_turn {
            Some(record) => {
                async {
                    (record.record)(session.clone(), Some(turn.clone())).await?;
                    if let Some(started) = &record.started {
                        started(turn.clone()).await?;
                    }
                    anyhow::Ok(())
                }
                .await
            }
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
            Ok(()) => {
                stage.enter(ProviderStage::Run);
                loop {
                    let event = tokio::select! {
                        biased;
                        _ = agent::wait_for_cancel(&mut cancel) => {
                            stage.enter(ProviderStage::Interrupt);
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
                            ExecFailure::new(FailureKind::Lost, process.events.failure().unwrap_or_else(|| "pi exited during the turn".into())),
                        ));
                    };
                    let mut shown = Ok(());
                    for update in seen.apply(&event) {
                        shown = transport::deliver(progress(crate::manager::redact(update, &secrets)), &mut cancel, deadline).await;
                        if shown.is_err() {
                            break;
                        }
                    }
                    if let Err(e) = shown {
                        break Err(error(ManagerFailure::Unknown, native, own(e.to_string())));
                    }
                    if event["type"] == "agent_settled" {
                        if let Some(failure) = process.events.failure() {
                            break Err(error(ManagerFailure::Unknown, native, own(failure)));
                        }
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
                }
            }
        };
        drop(live);
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
        first_input: WorkFirstInput,
        prompt: Value,
        report: transport::Sender<Report>,
        mut cancel: watch::Receiver<bool>,
    ) {
        let session = first_input.session.clone();
        let send = async |item| {
            if let Err(error) = report.send(item).await {
                tracing::warn!(%error, "pi work report unavailable; reconcile native session");
                // Probe is tiny and reliable even when the original terminal report exceeded
                // the byte budget. A closed consumer cannot accept either message.
                let _ = report.send(Report::Probe).await;
            }
        };
        let (mut process, after) = match self.open(&launch, &session, &mut cancel).await {
            Ok(opened) => opened,
            Err(error) => return send(Report::Signal(Signal::StartRejected(error.failure))).await,
        };
        let turn = match process
            .begin(
                after.as_deref(),
                prompt,
                PREFLIGHT,
                &mut cancel,
                Some(&first_input),
            )
            .await
        {
            Begin::Started(turn) => turn,
            Begin::Refused(failure) => {
                let tail = process.end().await;
                return send(Report::Signal(Signal::StartRejected(with_tail(
                    failure, &tail,
                )))).await;
            }
            Begin::Unknown(_) => {
                process.end().await;
                return send(Report::Probe).await;
            }
        };
        // Keep the turn interruptible before reporting its native handle.
        let stopping = Arc::new(AtomicBool::new(false));
        lock(&self.live).insert(
            session.clone(),
            Live {
                turn: turn.clone(),
                rpc: process.rpc.clone(),
                stopping: stopping.clone(),
            },
        );
        let live = LiveTurn {
            owner: &self,
            session: session.clone(),
            turn: turn.clone(),
        };
        send(Report::Signal(Signal::TurnStarted {
            turn_id: turn.clone(),
            epoch: 0,
        })).await;
        let mut seen = Seen::default();
        let settled = 'events: loop {
            let event = tokio::select! {
                biased;
                _ = agent::wait_for_cancel(&mut cancel), if !stopping.load(Relaxed) => {
                    stopping.store(true, Relaxed);
                    if !process.rpc.write(&json!({"type":"abort"})).await {
                        break false;
                    }
                    continue;
                }
                event = process.events.recv() => match event {
                    Some(event) => event,
                    None => {
                        if let Some(error) = process.events.failure() {
                            tracing::warn!(%error, "pi work output unavailable; reconcile native session");
                        }
                        break false;
                    },
                },
            };
            for update in seen.apply(&event) {
                let delivered = tokio::select! {
                    biased;
                    _ = agent::wait_for_cancel(&mut cancel) => false,
                    result = report.send(Report::Progress(update)) => result.is_ok(),
                };
                if !delivered {
                    stopping.store(*cancel.borrow(), Relaxed);
                    break 'events false;
                }
            }
            let message = &event["message"];
            if event["type"] == "message_end" && message["role"] == "assistant" {
                let text = text(message);
                if !text.is_empty() {
                    let signal = Report::Signal(match message["stopReason"].as_str() {
                        // Text before tool calls is the turn's commentary.
                        Some("toolUse") => Signal::Progress {
                            summary: text,
                            at_ms: now_ms(),
                        },
                        _ => Signal::Text(text),
                    });
                    let delivered = tokio::select! {
                        biased;
                        _ = agent::wait_for_cancel(&mut cancel) => false,
                        result = report.send(signal) => result.is_ok(),
                    };
                    if !delivered { break 'events false; }
                }
            }
            if event["type"] == "agent_settled" {
                break process.events.failure().is_none();
            }
        };
        drop(live);
        process.end().await;
        if !settled {
            return send(Report::Probe).await;
        }
        let (status, error) = seen.ending(stopping.load(Relaxed));
        send(Report::Signal(Signal::TurnCompleted {
            turn_id: turn,
            status,
            text: seen.text(),
            error,
        })).await;
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
        /// The directory the session runs in.
        cwd: PathBuf,
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
    fn start_or_resume<'a>(
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
                let pointer = |session: &str, gone: &'static str| -> Result<Pointer> {
                    self.pointer(session)?
                        .filter(|pointer| pointer.file.is_file())
                        .context(gone)
                };
                let origin = match (work.thread.as_deref(), thread) {
                    (Some(session), _) => {
                        let Pointer {
                            file, cwd, tree, ..
                        } = pointer(session, "the pi session of this thread is gone")?;
                        if !work.cwd.is_empty() && Path::new(&work.cwd) != cwd {
                            let failure = ExecFailure::new(
                                FailureKind::Rejected,
                                format!(
                                    "this pi session runs in {}, not {}; leave the directory out to go on in it",
                                    cwd.display(),
                                    work.cwd
                                ),
                            );
                            return Err(failure.into());
                        }
                        ensure!(
                            !self.processes.tracked(&tree),
                            "a pi started earlier still runs in this session"
                        );
                        // A restart before pi takes the prompt reads the session from here on.
                        self.keep(session, &file, &cwd, &tree)?;
                        Origin::Resume {
                            session: session.to_owned(),
                            file,
                            cwd,
                        }
                    }
                    (None, Some(original)) => Origin::Fork(
                        pointer(
                            original,
                            "the pi session of the work this one continues is gone",
                        )?
                        .file,
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
                Origin::Resume { session, file, .. } => (
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
            if let Some(model) = work.execution_model().filter(|model| !model.is_empty()) {
                launch_args.extend(["--model".into(), model.into()]);
            }
            if let Some(effort) = args["effort"].as_str() {
                let level = if effort == "none" { "off" } else { effort };
                launch_args.extend(["--thinking".into(), level.into()]);
            }
            let launch = Launch {
                bin: node.providers().bin(Provider::Pi),
                cwd: match &origin {
                    Origin::Resume { cwd, .. } => cwd.clone(),
                    Origin::New | Origin::Fork(_) => PathBuf::from(&work.cwd),
                },
                env: node.provider_env(Provider::Pi),
                args: launch_args,
                guide: None,
            };
            let (sender, mut reports) = transport::channel(transport::QUEUE_ITEMS, transport::QUEUE_BYTES, report_bytes);
            let (stop, cancel) = watch::channel(false);
            {
                let mut preparing = lock(&self.preparing);
                ensure!(
                    !preparing.contains_key(&work.work_id),
                    "pi is already preparing this work"
                );
                preparing.insert(work.work_id.clone(), stop);
            }
            let preparing = Preparing(node.pi.clone(), work.work_id.clone());
            let first_input = WorkFirstInput {
                node: node.clone(),
                network: network.to_owned(),
                work: work.clone(),
                session,
            };
            let run = node
                .pi
                .clone()
                .run_work(launch, first_input, prompt, sender, cancel);
            tokio::spawn(async move {
                run.await;
                drop(preparing);
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
                                None if self.processes.tracked(&pointer.tree) => {
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
            if self.processes.tracked(&pointer.tree) {
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
            if work.state.handle().is_none()
                && let Some(stop) = lock(&self.preparing).get(&work.work_id)
            {
                let _ = stop.send(true);
                return Ok(());
            }
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

    fn send_turn<'a>(
        &'a self,
        into: &'a SteerTarget,
        message_id: &'a str,
        input: &'a SteerInput,
    ) -> BoxFuture<'a, SteerAck> {
        let message = format!("{STEER_LEAD}\n\n{}", input.message(message_id));
        Box::pin(async move {
            let Some(live) = self.live_turn(&into.thread_id, &into.turn_id) else {
                return SteerAck::Refused("the pi turn is not running".into());
            };
            let images: Vec<Value> = input
                .files
                .iter()
                .filter(|file| file.is_image())
                .map(|file| json!({"type":"image","data":file.base64(),"mimeType":file.mime_type}))
                .collect();
            if input
                .files
                .iter()
                .filter(|file| file.is_image())
                .any(|file| file.bytes.len() as u64 > MAX_IMAGE)
            {
                return SteerAck::Refused("pi images must be at most 10 MiB".into());
            }
            match live
                .rpc
                .call(
                    json!({"type":"steer","message":message,"images":images}),
                    ANSWER,
                )
                .await
            {
                Reply::Done(_) => SteerAck::Accepted { turn_id: None },
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
            let entries = entries(&pointer.file).ok()?;
            let Some(turn) = entries.iter().position(|entry| entry["id"] == into.turn_id) else {
                return Some(SteerTrace::TargetUnseen);
            };
            let marker = format!("\"message_id\":{}", json!(message_id));
            let Some(at) = entries.iter().position(|entry| {
                is_message(entry, "user") && text(&entry["message"]).contains(&marker)
            }) else {
                return Some(
                    if self.live_turn(&into.thread_id, &into.turn_id).is_some()
                        || self.processes.tracked(&pointer.tree)
                    {
                        SteerTrace::Pending
                    } else {
                        SteerTrace::NotArrived
                    },
                );
            };
            let actual = entries[..=at]
                .iter()
                .rposition(|entry| {
                    is_message(entry, "user") && !text(&entry["message"]).starts_with(STEER_LEAD)
                })
                .unwrap_or(turn);
            let turn_id = entries[actual]["id"].as_str()?.to_owned();
            if self.live_turn(&into.thread_id, &turn_id).is_some() {
                return Some(SteerTrace::Received { turn_id });
            }
            let actual_run = run(&entries, actual);
            let answered = actual_run[at - actual..]
                .iter()
                .rev()
                .find(|entry| is_message(entry, "assistant"))
                .is_some_and(|entry| entry["message"]["stopReason"] == "stop");
            Some(SteerTrace::Ended {
                turn_id,
                outcome: match answered {
                    true => SteerOutcome::Answered,
                    false => SteerOutcome::Unfinished,
                },
            })
        })
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use tokio::io::AsyncReadExt as _;

    fn rpc() -> Arc<Rpc> {
        Arc::new(Rpc {
            stdin: Mutex::new(None),
            waiting: std::sync::Mutex::new(Some(HashMap::new())),
            responses: transport::JsonBudget::new(4096),
            commands: transport::JsonBudget::new(4096),
            next: AtomicU64::new(1),
        })
    }

    fn pending_response() -> (PendingResponses, oneshot::Receiver<transport::HeldJson>) {
        let (reply, answer) = oneshot::channel();
        (std::sync::Mutex::new(Some(HashMap::from([("pending".into(), reply)]))), answer)
    }

    #[tokio::test]
    async fn timed_out_partial_frame_closes_stdin_and_never_appends_another_command() {
        let (pipe, mut reader) = tokio::io::duplex(8);
        let stdin = Mutex::new(Some(pipe));
        let (waiting, answer) = pending_response();
        let started = AtomicBool::new(false);
        let command = json!({"type":"steer", "text":"a prompt longer than the pipe"});
        let result = tokio::time::timeout(Duration::from_millis(5),
            write_command(&stdin, &waiting, &command, &started)).await;
        assert!(result.is_err());
        assert!(started.load(Relaxed));
        assert!(stdin.lock().await.is_none());
        assert!(lock(&waiting).is_none());
        assert!(answer.await.is_err());
        let next = AtomicBool::new(false);
        assert!(matches!(write_command(&stdin, &waiting, &json!({"type":"abort"}), &next).await,
            Err(Reply::Unsent)));
        assert!(!next.load(Relaxed));
        let mut received = Vec::new();
        reader.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, transport::json(&command, transport::FRAME_BYTES).unwrap()[..8]);
        assert!(!received.contains(&b'\n'), "the partial frame never gained a second command");
    }

    #[tokio::test]
    async fn cancelling_partial_prompt_closes_stdin_and_releases_other_rpc_waiters() {
        let (pipe, mut reader) = tokio::io::duplex(8);
        let mut stdin = Some(pipe);
        let (waiting, answer) = pending_response();
        let started = AtomicBool::new(false);
        let prompt_started = AtomicBool::new(false);
        let (stop, mut cancel) = watch::channel(false);
        let bytes = b"{\"type\":\"prompt\",\"text\":\"long partial prompt\"}\n";
        {
            let writing = write_frame(&mut stdin, &waiting, bytes, &started, Some(&prompt_started));
            tokio::pin!(writing);
            assert!(futures_util::poll!(writing.as_mut()).is_pending());
            stop.send(true).unwrap();
            tokio::select! {
                biased;
                _ = agent::wait_for_cancel(&mut cancel) => {},
                _ = &mut writing => panic!("the undrained pipe cannot finish this frame"),
            }
            // Dropping the selected-away write future is the same cancellation used by begin.
        }
        assert!(started.load(Relaxed) && prompt_started.load(Relaxed));
        assert!(stdin.is_none());
        assert!(lock(&waiting).is_none());
        assert!(answer.await.is_err());
        let next = AtomicBool::new(false);
        assert!(matches!(write_frame(&mut stdin, &waiting, b"next\n", &next, None).await,
            Err(Reply::Unsent)));
        let mut received = Vec::new();
        reader.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, bytes[..8]);
        assert!(!next.load(Relaxed));
    }

    #[tokio::test]
    async fn timeout_before_writing_keeps_stdin_and_does_not_poison_other_waiters() {
        let (pipe, mut reader) = tokio::io::duplex(128);
        let stdin = Mutex::new(Some(pipe));
        let held = stdin.lock().await;
        let (waiting, _answer) = pending_response();
        let started = AtomicBool::new(false);
        assert!(tokio::time::timeout(Duration::from_millis(5),
            write_command(&stdin, &waiting, &json!({"type":"abort"}), &started)).await.is_err());
        assert!(!started.load(Relaxed));
        assert!(held.is_some());
        assert!(lock(&waiting).as_ref().unwrap().contains_key("pending"));
        drop(held);
        let command = json!({"type":"get_state"});
        assert!(write_command(&stdin, &waiting, &command, &started).await.is_ok());
        assert!(stdin.lock().await.take().is_some());
        let mut received = Vec::new();
        reader.read_to_end(&mut received).await.unwrap();
        let mut expected = transport::json(&command, transport::FRAME_BYTES).unwrap();
        expected.push(b'\n');
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn cancellation_before_writing_keeps_stdin_and_pending_waiters() {
        let (pipe, mut reader) = tokio::io::duplex(128);
        let stdin = Mutex::new(Some(pipe));
        let held = stdin.lock().await;
        let (waiting, _answer) = pending_response();
        let started = AtomicBool::new(false);
        let command = json!({"type":"abort"});
        {
            let writing = write_command(&stdin, &waiting, &command, &started);
            tokio::pin!(writing);
            assert!(futures_util::poll!(writing.as_mut()).is_pending());
        }
        assert!(!started.load(Relaxed));
        assert!(held.is_some());
        assert!(lock(&waiting).as_ref().unwrap().contains_key("pending"));
        drop(held);
        assert!(stdin.lock().await.take().is_some());
        let mut received = Vec::new();
        reader.read_to_end(&mut received).await.unwrap();
        assert!(received.is_empty());
    }

    #[tokio::test]
    async fn cancellation_after_complete_frame_keeps_stdin_for_readback_commands() {
        let (pipe, mut reader) = tokio::io::duplex(128);
        let stdin = Mutex::new(Some(pipe));
        let (waiting, _answer) = pending_response();
        let started = AtomicBool::new(false);
        let command = json!({"type":"get_state"});
        {
            let call = async {
                assert!(write_command(&stdin, &waiting, &command, &started).await.is_ok());
                std::future::pending::<()>().await;
            };
            tokio::pin!(call);
            assert!(futures_util::poll!(call.as_mut()).is_pending());
        }
        assert!(started.load(Relaxed));
        assert!(lock(&waiting).as_ref().unwrap().contains_key("pending"));
        assert!(stdin.lock().await.take().is_some());
        let mut received = Vec::new();
        reader.read_to_end(&mut received).await.unwrap();
        let mut expected = transport::json(&command, transport::FRAME_BYTES).unwrap();
        expected.push(b'\n');
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn response_bypasses_progress_and_terminal_remains_ordered() {
        let rpc = rpc();
        let (reply, answer) = oneshot::channel();
        lock(&rpc.waiting).as_mut().unwrap().insert("vf-1".into(), reply);
        let (sender, mut events) = transport::json_channel(2, 4096);
        let input = concat!(
            "{\"type\":\"message_update\"}\n",
            "{\"type\":\"response\",\"id\":\"vf-1\",\"success\":true,\"data\":{\"ok\":true}}\n",
            "{\"type\":\"agent_settled\"}\n",
        );
        read_output(input.as_bytes(), rpc.clone(), sender).await;
        assert_eq!(answer.await.unwrap().value["data"], json!({"ok":true}));
        assert_eq!(events.recv().await.unwrap()["type"], "message_update");
        assert_eq!(events.recv().await.unwrap()["type"], "agent_settled");
        assert!(events.recv().await.is_none());
        assert!(events.failure().is_none());
        assert!(lock(&rpc.waiting).is_none());
    }

    #[tokio::test]
    async fn progress_pressure_closes_intake_and_releases_response_waiter() {
        let rpc = rpc();
        let (reply, answer) = oneshot::channel();
        lock(&rpc.waiting).as_mut().unwrap().insert("vf-1".into(), reply);
        let (sender, mut events) = transport::json_channel(1, 4096);
        let input = concat!(
            "{\"type\":\"message_update\"}\n",
            "{\"type\":\"message_update\"}\n",
            "{\"type\":\"response\",\"id\":\"vf-1\",\"success\":true}\n",
            "{\"type\":\"agent_settled\"}\n",
        );
        tokio::time::timeout(Duration::from_secs(1), read_output(input.as_bytes(), rpc.clone(), sender))
            .await.unwrap();
        assert!(answer.await.is_err(), "RPC must become unknown, never hang behind progress");
        assert_eq!(events.recv().await.unwrap()["type"], "message_update");
        assert!(events.recv().await.is_none());
        assert!(events.failure().unwrap().contains("queue"));
        assert!(lock(&rpc.waiting).is_none());
    }

    #[tokio::test]
    async fn cancelling_a_pending_rpc_releases_its_waiter_entry() {
        let rpc = rpc();
        let (reply, answer) = oneshot::channel();
        lock(&rpc.waiting).as_mut().unwrap().insert("cancelled".into(), reply);
        let pending = PendingReply { rpc: &rpc, id: "cancelled".into() };
        drop(pending);
        assert!(answer.await.is_err());
        assert!(lock(&rpc.waiting).as_ref().unwrap().is_empty());
    }

    #[tokio::test]
    async fn report_queue_pressure_keeps_the_terminal_signal() {
        let (sender, mut reports) = transport::channel(1, 4096, report_bytes);
        sender.try_send(Report::Progress(json!({"type":"text_delta", "text":"hello"}))).unwrap();
        let terminal = sender.send(Report::Signal(Signal::TurnCompleted {
            turn_id: "turn".into(), status: TurnStatus::Completed, text: "done".into(), error: None,
        }));
        tokio::pin!(terminal);
        assert!(tokio::time::timeout(Duration::from_millis(5), &mut terminal).await.is_err());
        assert!(matches!(reports.recv().await, Some(Report::Progress(_))));
        terminal.await.unwrap();
        assert!(matches!(reports.recv().await, Some(Report::Signal(Signal::TurnCompleted { text, .. })) if text == "done"));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::manager::{CodexTurn, Sdk, run_manager};
    use crate::runtime::test_support::{
        assert_work_outcome, join_network, open_node, provider_work,
    };
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
                started: None,
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
            reasoning_effort: None,
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
            started: None,
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

    async fn concurrent_pi_turns(
        dir: &Path,
        pi: &Arc<Pi>,
        first: Option<ManagerFailure>,
    ) -> Result<()> {
        let mut tasks = Vec::with_capacity(16);
        let mut stops = Vec::with_capacity(16);
        for index in 0..16 {
            let config = gated_bot(
                dir,
                pi,
                if index == 0 && first == Some(ManagerFailure::Failed) {
                    "fail"
                } else {
                    "reply"
                },
                &format!("client-{index}"),
            );
            let prompt = format!("marker-{index}");
            let (stop, cancel) = tokio::sync::watch::channel(false);
            stops.push(stop);
            tasks.push(tokio::spawn(async move {
                crate::manager::run_manager(
                    &config,
                    &prompt,
                    None,
                    |_| async { Ok(()) },
                    cancel,
                    |_| async { Ok(()) },
                )
                .await
            }));
        }
        let gated = async {
            wait_for_gate(dir).await?;
            if first == Some(ManagerFailure::Cancelled) {
                stops[0].send(true)?;
                wait_for_abort(dir).await?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        release(dir);
        let mut results = Vec::with_capacity(16);
        for task in tasks {
            results.push(task.await);
        }
        gated?;
        let mut sessions = std::collections::HashSet::new();
        for (index, result) in results.into_iter().enumerate() {
            let result = result?;
            if index == 0 && first.is_some() {
                let error = result.err().context("first pi turn")?;
                let error = error
                    .downcast_ref::<ManagerError>()
                    .context("manager error")?;
                if first == Some(ManagerFailure::Cancelled) {
                    assert!(error.cancelled());
                } else {
                    assert_eq!(error.outcome, ManagerFailure::Failed);
                }
            } else {
                let result = result?;
                if first.is_none() {
                    sessions.insert(result.session_id.context("pi session")?);
                }
                assert_eq!(result.output, format!("done: marker-{index}"));
            }
        }
        if first.is_none() {
            assert_eq!(sessions.len(), 16);
        }
        if first == Some(ManagerFailure::Cancelled) {
            assert_eq!(
                log(dir)
                    .iter()
                    .filter(|entry| entry["abort"] == true)
                    .count(),
                1
            );
        }
        Ok(())
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
            guide: None,
        }
    }

    fn spec(work_id: &str, target: &str, cwd: &Path) -> WorkSpec {
        WorkSpec {
            work_id: work_id.into(),
            call_ref: work_id.into(),
            ..provider_work(Provider::Pi, target, cwd, "do it")
        }
    }

    /// A Node in a network of its own whose pi is the fake, and that network.
    async fn node(dir: &Path, mode: &str) -> Result<(Arc<Runtime>, String)> {
        let data = dir.join("node");
        let mut configuration = Config::local("127.0.0.1:0".parse()?, vec![dir.to_owned()], false);
        configuration.providers.execution = [Provider::Pi].into();
        let node = open_node(&data, &configuration).await?;
        let (admin, network) = join_network(&node).await?;
        super::super::claude::tests::conversation(&node, &admin, &network, "c", "m1").await?;
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
        {
            let identity = node.identity.to_owned();
            let network = network.to_owned();
            let spec = spec.clone();
            node.store.call("pi.accept_work_fixture", move |store| store.accept_work_fixture(&identity, &network, spec, now_ms())).await
        }?;
        let pi = node.pi.clone();
        let sends = (node.start_work(pi.as_ref(), network, &spec, None, &Value::Null)).await?;
        ensure!(!sends, "the Node sends a pi work itself");
        Ok(())
    }

    fn gate_state(node: &Runtime, dir: &Path) -> Result<()> {
        node.providers.lock().unwrap().env.apply(
            Provider::Pi,
            BTreeMap::from([(
                "FAKE_PI_STATE_GATE".into(),
                Some(dir.join("state-release").display().to_string()),
            )]),
        )
    }

    async fn state_waiting(dir: &Path) -> Result<u32> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(pid) = log(dir)
                    .iter()
                    .find(|entry| entry["state_waiting"].is_string())
                    .and_then(|entry| entry["pid"].as_u64())
                {
                    return pid as u32;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("pi reached the get_state gate")
    }

    /// The work `w-1` once its state is `done`.
    async fn work(node: &Runtime, done: impl Fn(&WorkState) -> bool) -> Result<WorkRecord> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let record = node.store.call("pi.work_record", move |store| store.work_record("w-1")).await?;
            let record = record.context("work")?;
            if done(&record.state) || Instant::now() > deadline {
                return Ok(record);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn two_nodes_force_private_pi_sessions_without_changing_auth_config() -> Result<()> {
        let dir = scratch("private-sessions");
        let user = dir.join("user");
        std::fs::create_dir(&user)?;
        for name in ["node-a", "node-b"] {
            let data = dir.join(name);
            let pi = Arc::new(Pi::new(&data).await);
            let mut launch = launch(&dir, "reply", name);
            launch.env.insert(
                "PI_CODING_AGENT_SESSION_DIR".into(),
                user.display().to_string(),
            );
            launch
                .env
                .insert("PI_CODING_AGENT_DIR".into(), user.display().to_string());
            let (_cancel, mut cancel) = watch::channel(false);
            let (mut process, _) = pi.open(&launch, name, &mut cancel).await?;
            assert!(matches!(
                process
                    .begin(
                        None,
                        json!({"type":"prompt", "message":"hello"}),
                        ANSWER,
                        &mut cancel,
                        None,
                    )
                    .await,
                Begin::Started(_)
            ));
            let _ = process.end().await;
            let pointer = pi.pointer(name)?.context("session pointer")?;
            assert!(
                pointer
                    .file
                    .starts_with(ProviderRoots::new(&data).pi_sessions)
            );
            assert!(pointer.file.is_file());
        }
        assert_eq!(std::fs::read_dir(&user)?.count(), 0);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn bot_readback_uses_its_frozen_input_and_requires_an_ending() -> Result<()> {
        let dir = scratch("bot-readback");
        let pi = Pi::new(&dir).await;
        let file = dir.join("session.jsonl");
        std::fs::write(&file, "")?;
        pi.keep("s-1", &file, &dir, "")?;
        pi.record_reply("s-1", "attempt-a", "first prompt")?;
        let user = |id: &str, prompt: &str| json!({"type":"message","id":id,"message":{"role":"user","content":prompt}});
        let assistant = |id: &str, text: &str, stop: Value| json!({"type":"message","id":id,"message":{"role":"assistant","content":[{"type":"text","text":text}],"stopReason":stop}});
        let write = |records: Vec<Value>| -> Result<()> {
            std::fs::write(
                &file,
                records
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )?;
            Ok(())
        };
        assert_eq!(
            pi.probe_reply("s-1", None, "attempt-a").await?,
            Probe::Missing
        );
        write(vec![
            user("user-a", "first prompt"),
            assistant("reply-a", "unfinished", json!("toolUse")),
            user("user-b", "later prompt"),
            assistant("reply-b", "later result", json!("stop")),
        ])?;
        assert_eq!(
            pi.probe_reply("s-1", None, "attempt-a").await?,
            Probe::Missing
        );
        write(vec![
            user("user-a", "first prompt"),
            assistant("reply-a", "first result", json!("stop")),
            user("user-b", "later prompt"),
            assistant("reply-b", "later result", json!("stop")),
        ])?;
        pi.keep("s-1", &file, &dir, "")?;
        assert_eq!(
            pi.probe_reply("s-1", None, "attempt-a").await?,
            Probe::Terminal {
                turn_id: "user-a".into(),
                status: TurnStatus::Completed,
                text: "first result".into(),
                error: None,
            }
        );
        assert_eq!(
            pi.probe_reply("s-1", Some("user-b"), "attempt-a").await?,
            Probe::Missing
        );
        assert_eq!(
            pi.probe_reply("s-1", None, "attempt-b").await?,
            Probe::Missing
        );
        assert_eq!(
            pi.probe_reply("different-session", None, "attempt-a")
                .await?,
            Probe::Missing
        );
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_bot_turn_streams_its_reply_and_the_next_turn_goes_on_in_its_session() -> Result<()> {
        let dir = scratch("bot");
        let pi = Arc::new(Pi::new(&dir).await);
        let mut config = bot(&dir, &pi, "reply", None);
        config.reasoning_effort = Some("high".into());
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
        for args in [&first, &second] {
            assert!(args.windows(2).any(|pair| pair == ["--thinking", "high"]));
        }
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
        assert_eq!(starts[0]["guide"], "Answer briefly.");
        assert_eq!(starts[0]["guide_private"], true);
        assert!(
            !Path::new(guide).exists(),
            "an ended process releases its guide"
        );
        assert_eq!(second[3], "--session");
        assert!(second[4].ends_with(&format!("_{session}.jsonl")));
        assert_eq!(starts[0]["run"], "1");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_message_steered_into_the_running_reply_is_answered_in_it() -> Result<()> {
        let dir = scratch("steer");
        let pi = Arc::new(Pi::new(&dir).await);
        let turns = Turns::default();
        let mut config = bot(&dir, &pi, "wait", Some(&turns));
        let native_starts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded_starts = native_starts.clone();
        let start_ready = Arc::new(tokio::sync::Notify::new());
        let marked = start_ready.clone();
        config.codex_turn.as_mut().unwrap().started = Some(Arc::new(move |turn| {
            recorded_starts.lock().unwrap().push(turn);
            marked.notify_one();
            Box::pin(async { Ok(()) })
        }));
        let bytes = b"\x89PNG\r\n\x1a\nfull-fixture-image".to_vec();
        let image_path = dir.join("image.png");
        std::fs::write(&image_path, &bytes)?;
        let input = SteerInput {
            text: "also this".into(),
            files: vec![super::super::executor::PreparedAttachment {
                attachment: Attachment {
                    hash: crate::crypto::digest(&bytes),
                    size: bytes.len() as u64,
                    name: "image.png".into(),
                },
                local_path: image_path,
                mime_type: "image/png".into(),
                bytes: bytes.clone(),
            }],
        };
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
            tokio::time::timeout(Duration::from_secs(5), start_ready.notified())
                .await
                .expect("the held turn reports its actual native start");
            let (thread_id, turn_id) = started(&turns).await;
            assert_eq!(*native_starts.lock().unwrap(), vec![turn_id.clone()]);
            let into = SteerTarget {
                provider: Provider::Pi,
                attempt_id: "a".into(),
                thread_id,
                turn_id,
            };
            assert_eq!(
                pi.send_turn(&into, "m-1", &input).await,
                SteerAck::Accepted { turn_id: None }
            );
            into
        };
        let (reply, into) = tokio::join!(reply, steer);
        assert_eq!(reply?.output, "Got it");
        let pointer = pi.pointer(&into.thread_id)?.context("pi session")?;
        let entries = entries(&pointer.file)?;
        let content = entries
            .iter()
            .find(|entry| text(&entry["message"]).contains("\"message_id\":\"m-1\""))
            .context("steered input")?["message"]["content"]
            .clone();
        assert_eq!(content[1]["type"], "image");
        assert_eq!(
            content[1]["data"],
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        assert_eq!(
            pi.read_back(&into, "m-1").await,
            Some(SteerTrace::Ended {
                turn_id: into.turn_id.clone(),
                outcome: SteerOutcome::Answered
            })
        );
        assert_eq!(
            pi.read_back(&into, "m-2").await,
            Some(SteerTrace::NotArrived)
        );
        assert_eq!(
            pi.send_turn(
                &into,
                "m-3",
                &SteerInput {
                    text: "late".into(),
                    files: vec![]
                }
            )
            .await,
            SteerAck::Refused("the pi turn is not running".into())
        );
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn stopping_a_bot_turn_aborts_it_in_pi() -> Result<()> {
        let dir = scratch("stop");
        let pi = Arc::new(Pi::new(&dir).await);
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
        let pi = Arc::new(Pi::new(&dir).await);
        concurrent_pi_turns(&dir, &pi, None).await?;
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn aborting_a_bot_future_clears_only_its_live_turn() -> Result<()> {
        let dir = scratch("future-abort");
        let pi = Arc::new(Pi::new(&dir).await);
        let mut tasks = Vec::new();
        let mut turns = Vec::new();
        let mut stops = Vec::new();
        for index in 0..2 {
            let recorded = Turns::default();
            let mut config = bot(&dir, &pi, "wait", Some(&recorded));
            config.codex_turn.as_mut().unwrap().client_id = format!("client-{index}");
            let (stop, cancel) = watch::channel(false);
            stops.push(stop);
            turns.push(recorded);
            tasks.push(tokio::spawn(async move {
                run_manager(
                    &config,
                    "wait",
                    None,
                    |_| async { anyhow::Ok(()) },
                    cancel,
                    |_| async { anyhow::Ok(()) },
                )
                .await
            }));
        }
        let first = started(&turns[0]).await;
        let second = started(&turns[1]).await;
        assert!(pi.live_turn(&first.0, &first.1).is_some());
        assert_eq!(std::fs::read_dir(pi.dir.join("guides"))?.count(), 2);
        drop(LiveTurn {
            owner: &pi,
            session: second.0.clone(),
            turn: "superseded-turn".into(),
        });
        assert!(pi.live_turn(&second.0, &second.1).is_some());
        let aborted = tasks.remove(0);
        aborted.abort();
        assert!(aborted.await.err().context("aborted task")?.is_cancelled());
        assert!(pi.live_turn(&first.0, &first.1).is_none());
        assert!(pi.live_turn(&second.0, &second.1).is_some());
        assert_eq!(
            pi.probe_reply(&first.0, Some(&first.1), "client-0").await?,
            Probe::Missing
        );
        stops[1].send(true)?;
        assert!(tasks.remove(0).await?.is_err());
        assert!(lock(&pi.live).is_empty());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_one_of_sixteen_pi_turns_leaves_the_others_running() -> Result<()> {
        let dir = scratch("concurrent-cancel");
        let pi = Arc::new(Pi::new(&dir).await);
        concurrent_pi_turns(&dir, &pi, Some(ManagerFailure::Cancelled)).await?;
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn one_failed_pi_turn_does_not_stop_the_other_fifteen() -> Result<()> {
        let dir = scratch("concurrent-failure");
        let pi = Arc::new(Pi::new(&dir).await);
        concurrent_pi_turns(&dir, &pi, Some(ManagerFailure::Failed)).await?;
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_dispatched_work_runs_in_a_new_session_and_finishes_with_its_reply() -> Result<()> {
        let dir = scratch("work");
        let (node, network) = node(&dir, "reply").await?;
        dispatch(&node, &network, &dir).await?;
        let record = work(&node, WorkState::finished).await?;
        assert_work_outcome(&record, WorkOutcome::Completed);
        assert_eq!(record.text, "Hello");
        assert_eq!(prompts(&dir), ["do it"]);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_preparing_work_rechecks_execution_and_roots_before_its_first_prompt() -> Result<()> {
        for revoked in ["unchanged", "execution", "roots"] {
            let dir = scratch(revoked);
            let (node, network) = node(&dir, "reply").await?;
            gate_state(&node, &dir)?;
            dispatch(&node, &network, &dir).await?;
            let pid = state_waiting(&dir).await?;
            assert!(prompts(&dir).is_empty());
            match revoked {
                "execution" => {
                    node.providers.lock().unwrap().switches.execution.clear();
                }
                "roots" => {
                    node.settings.lock().unwrap().roots.clear();
                }
                _ => {}
            }
            std::fs::write(dir.join("state-release"), "")?;
            let record = work(&node, WorkState::finished).await?;
            assert_eq!(
                record.state,
                WorkState::Finished {
                    outcome: if revoked == "unchanged" {
                        WorkOutcome::Completed
                    } else {
                        WorkOutcome::Rejected
                    },
                },
                "{revoked}"
            );
            assert_eq!(
                prompts(&dir),
                if revoked == "unchanged" {
                    vec![json!("do it")]
                } else {
                    vec![]
                },
                "{revoked}"
            );
            assert!(ended(&pid.to_string()).await, "pi ended after {revoked}");
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_preparing_work_can_be_stopped_without_a_turn_or_get_state_reply() -> Result<()> {
        let dir = scratch("preparing-stop");
        let (node, network) = node(&dir, "reply").await?;
        gate_state(&node, &dir)?;
        dispatch(&node, &network, &dir).await?;
        let pid = state_waiting(&dir).await?;
        let preparing = node.store.call("pi.work_record", move |store| store.work_record("w-1")).await?
            .context("work")?;
        assert!(preparing.state.handle().is_none());
        tokio::time::timeout(Duration::from_secs(2), node.stop_work(&network, "c", "w-1"))
            .await??;
        node.reconcile_works(&network).await?;
        assert_eq!(
            work(&node, WorkState::finished).await?.state,
            WorkState::Finished {
                outcome: WorkOutcome::Rejected
            }
        );
        assert!(
            ended(&pid.to_string()).await,
            "cancelling get_state ended pi"
        );
        std::fs::write(dir.join("state-release"), "")?;
        assert!(prompts(&dir).is_empty());
        assert!(!lock(&node.pi.preparing).contains_key("w-1"));
        assert!(node.store.call("pi.stop_requests", move |store| store.stop_requests()).await?.is_empty());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_normal_end_preserves_eof_and_grace_for_a_cooperative_child() -> Result<()> {
        let dir = scratch("normal-cooperative-end");
        let processes = Arc::new(Processes::new(dir.join(PIDFILE)));
        let process = Process::spawn(
            &Launch {
                bin: super::super::claude::tests::cooperative_cli(&dir)?,
                cwd: dir.clone(),
                env: BTreeMap::from([("HOME".into(), dir.display().to_string())]),
                args: Vec::new(),
                guide: None,
            },
            &processes,
            &dir.join("sessions"),
        )?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dir.join("member-ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        process.end().await;
        let natural = std::fs::read_to_string(dir.join("natural-close"))?;
        let completed = std::fs::read_to_string(dir.join("member-completed")).unwrap_or_default();
        std::fs::remove_dir_all(dir)?;
        assert_eq!(natural, "persisted");
        assert_eq!(
            completed, "term-finished",
            "a normal pi end gives its live child TERM and time to finish"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_cancelled_prompt_never_wins_a_ready_or_queued_stdin_write() -> Result<()> {
        for queued in [false, true] {
            let dir = scratch("cancel-before-write");
            let pi = Pi::new(&dir).await;
            let (stop, mut cancel) = watch::channel(false);
            let (mut process, after) = pi
                .open(&launch(&dir, "reply", "s-1"), "s-1", &mut cancel)
                .await?;
            let rpc = process.rpc.clone();
            let stdin = if queued {
                Some(rpc.stdin.lock().await)
            } else {
                None
            };
            if !queued {
                stop.send(true)?;
            }
            let result = {
                let begin = process.begin(
                    after.as_deref(),
                    json!({"type":"prompt", "message":"do it"}),
                    ANSWER,
                    &mut cancel,
                    None,
                );
                tokio::pin!(begin);
                if queued {
                    tokio::select! {
                        result = &mut begin => panic!("the prompt wrote through a held stdin lock: {}", matches!(result, Begin::Started(_))),
                        _ = tokio::task::yield_now() => {}
                    }
                    stop.send(true)?;
                    drop(stdin);
                }
                begin.await
            };
            assert!(
                matches!(result, Begin::Refused(_)),
                "cancelled before writing"
            );
            process.end().await;
            assert!(prompts(&dir).is_empty());
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_after_the_prompt_write_keeps_an_unknown_outcome() -> Result<()> {
        let dir = scratch("cancel-after-write");
        let pi = Pi::new(&dir).await;
        let (stop, mut cancel) = watch::channel(false);
        let (mut process, after) = pi
            .open(&launch(&dir, "prompt-pending", "s-1"), "s-1", &mut cancel)
            .await?;
        let stopping = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while prompts(&dir).is_empty() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            stop.send(true)?;
            anyhow::Ok(())
        };
        let (result, cancelled) = tokio::join!(
            process.begin(
                after.as_deref(),
                json!({"type":"prompt", "message":"do it"}),
                ANSWER,
                &mut cancel,
                None
            ),
            stopping,
        );
        cancelled?;
        assert!(matches!(result, Begin::Unknown(_)));
        process.end().await;
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
        let session = ({
            let network = network.to_owned();
            node.store.call("pi.works", move |store| store.works(&network, "c")).await
        }?)
            .into_iter()
            .find_map(|work| work.thread_id)
            .context("a session")?;
        assert_eq!(node.pi.native_directory(&session).await?, dir);
        assert!(node.pi.native_directory("missing-session").await.is_err());
        let file = node.pi.pointer(&session)?.context("pointer")?.file;
        let pi = node.pi.clone();
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir(&elsewhere)?;
        let moved = WorkSpec {
            thread: Some(session.clone()),
            ..spec("w-3", &node.identity.device_id()?, &elsewhere)
        };
        {
            let identity = node.identity.to_owned();
            let network = network.to_owned();
            let moved = moved.clone();
            node.store.call("pi.accept_work_fixture", move |store| store.accept_work_fixture(&identity, &network, moved, now_ms())).await
        }?;
        let refused = node.start_work(pi.as_ref(), &network, &moved, Some(&session), &Value::Null);
        let refused = refused
            .await
            .err()
            .context("another directory is refused")?;
        assert!(
            format!("{refused:#}")
                .contains("declared work directory differs from the native session"),
            "{refused:#}"
        );
        // A continuation uses the session's verified directory.
        let spec = WorkSpec {
            prompt: "again".into(),
            thread: Some(session.clone()),
            ..spec("w-2", &node.identity.device_id()?, &dir)
        };
        {
            let identity = node.identity.to_owned();
            let network = network.to_owned();
            let spec = spec.clone();
            node.store.call("pi.accept_work_fixture", move |store| store.accept_work_fixture(&identity, &network, spec, now_ms())).await
        }?;
        (node.start_work(pi.as_ref(), &network, &spec, Some(&session), &Value::Null)).await?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let record = loop {
            let record = node.store.call("pi.work_record", move |store| store.work_record("w-2")).await?.context("w-2")?;
            if record.state.finished() || Instant::now() > deadline {
                break record;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_work_outcome(&record, WorkOutcome::Completed);
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
        assert_work_outcome(&record, WorkOutcome::Interrupted);
        assert_eq!(record.text, "Working");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_running_pi_work_retries_native_abort_until_it_settles() -> Result<()> {
        let dir = scratch("abort-retry");
        let (node, network) = node(&dir, "retry-abort").await?;
        dispatch(&node, &network, &dir).await?;
        let running = work(&node, |state| state.handle().is_some()).await?;
        let handle = running.state.handle().context("native handle")?;
        let group = node.pi.pointer(&handle.thread_id)?.context("pointer")?.tree;
        let aborts = || {
            log(&dir)
                .iter()
                .filter(|entry| entry["abort"] == true)
                .count()
        };
        let mut absent_handle = handle.clone();
        absent_handle.turn_id.push_str("-missing");
        let absent = WorkRecord {
            state: WorkState::Running {
                handle: absent_handle,
            },
            ..running.clone()
        };
        let missing = node
            .pi
            .interrupt(&absent)
            .await
            .expect_err("an unknown native handle must not cancel preparation");
        assert!(format!("{missing:#}").contains("pi is not running this work"));
        assert_eq!(aborts(), 0);
        node.stop_work(&network, "c", "w-1").await?;
        wait_for_abort(&dir).await?;
        assert_eq!(aborts(), 1, "the first native abort reached pi");
        let first = node.store.call("pi.work_record", move |store| store.work_record("w-1")).await?
            .context("work")?;
        assert!(matches!(first.state, WorkState::Interrupting { .. }));
        assert_eq!(
            node.pi.probe(&first).await?,
            Probe::Running {
                turn_id: handle.turn_id.clone(),
                epoch: 0
            }
        );
        let mut held = node.store.call("pi.stop_requests", move |store| store.stop_requests()).await?
            .into_iter()
            .find(|request| request.work_id == "w-1")
            .context("the unsettled turn keeps its stop request")?;
        held.deadline_ms = now_ms() - 1;
        {
            let held = held.to_owned();
            node.store.call("pi.save_stop_request", move |store| store.save_stop_request(&held)).await
        }?;
        node.reconcile_works(&network).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while aborts() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("the stop retry sent another native abort")?;
        let finished = work(&node, WorkState::finished).await?;
        assert_work_outcome(&finished, WorkOutcome::Interrupted);
        assert_eq!(
            node.pi.probe(&running).await?,
            Probe::Terminal {
                turn_id: handle.turn_id.clone(),
                status: TurnStatus::Interrupted,
                text: "Working".into(),
                error: None,
            }
        );
        assert!(ended(&group).await, "settled pi ended its process");
        assert!(node.store.call("pi.stop_requests", move |store| store.stop_requests()).await?.is_empty());
        assert!(!lock(&node.pi.preparing).contains_key("w-1"));
        assert!(
            node.pi
                .live_turn(&handle.thread_id, &handle.turn_id)
                .is_none()
        );
        node.reconcile_works(&network).await?;
        assert_eq!(aborts(), 2, "a settled turn receives no further abort");
        assert_eq!(prompts(&dir), ["do it"], "the prompt was never resent");
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
        assert_work_outcome(&record, WorkOutcome::Interrupted);
        assert_eq!(prompts(&dir), ["do it"]);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    async fn ended(token: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while tree::token_is_running(token) {
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
        let (node, network) = node(&dir, "wait").await?;
        dispatch(&node, &network, &dir).await?;
        let running = work(&node, |state| state.handle().is_some()).await?;
        let session = running.state.thread().context("thread")?;
        let left = node.pi.pointer(session)?.context("pointer")?.tree;
        let mut stranger = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()?;
        let data = dir.join("node");
        let pidfile = data.join(PIDFILE);
        let mut recorded = std::fs::read_to_string(&pidfile).unwrap_or_default();
        recorded.push_str(&format!("{} {}\n", stranger.id(), fake().display()));
        std::fs::write(&pidfile, recorded)?;
        let restarted = Pi::new(&data).await;
        assert!(ended(&left).await, "the restart ended the pi it left");
        assert!(
            std::fs::read_to_string(&pidfile)?.contains(&format!("{} ", stranger.id())),
            "an unconfirmed record is retained"
        );
        assert!(
            stranger.try_wait()?.is_none(),
            "a process the Node did not start is left alone"
        );
        let mut pointer = restarted.pointer(session)?.context("pointer")?;
        pointer.tree = stranger.id().to_string();
        config::private_write(
            &restarted.pointer_path(session)?,
            &serde_json::to_vec(&pointer)?,
        )?;
        let probe = restarted.probe(&running).await?;
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
        let group = node.pi.pointer(session)?.context("pointer")?.tree;
        node.shutdown().await;
        assert!(ended(&group).await, "the shutdown ended the running pi");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_session_without_the_prompt_reads_as_not_started() -> Result<()> {
        let dir = scratch("unsent");
        let pi = Pi::new(&dir).await;
        let (mut work, _) = crate::work::accept(spec("w-1", "t", &dir), 0);
        crate::work::transition(&mut work, Signal::ThreadCreated("s-1".into()));
        assert_eq!(pi.probe(&work).await?, Probe::NotStarted);
        let (process, _) = pi
            .open(
                &launch(&dir, "reply", "s-1"),
                "s-1",
                &mut watch::channel(false).1,
            )
            .await?;
        assert!(pi.probe(&work).await.is_err());
        process.end().await;
        assert!(pi.pointer("s-1")?.is_some());
        assert_eq!(pi.probe(&work).await?, Probe::NotStarted);
        assert!(prompts(&dir).is_empty());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn pi_management_failures_refund_only_proven_not_started_triage() -> Result<()> {
        use crate::manager::StartEvidence::{NotStarted, Started, Unknown};
        use crate::store::triage::TriageAdmission;

        for (mode, expected_start, expected_kind, charged, pending) in [
            ("refuse", NotStarted, FailureKind::MissingKey, 0, 1),
            ("refuse-model", NotStarted, FailureKind::NoModel, 0, 1),
            ("fail-key", Started, FailureKind::MissingKey, 1, 0),
            ("prompt-lost", Unknown, FailureKind::Lost, 1, 0),
            ("crash", Started, FailureKind::Lost, 1, 0),
            ("instructions-error", NotStarted, FailureKind::Unknown, 0, 1),
            (
                "instructions-write-error",
                NotStarted,
                FailureKind::Unknown,
                0,
                1,
            ),
            ("pointer-error", NotStarted, FailureKind::Unknown, 0, 1),
        ] {
            let dir = scratch(mode);
            let pi = Arc::new(Pi::new(&dir).await);
            let mut config = bot(&dir, &pi, mode, None);
            let previous = match mode {
                "instructions-error" => {
                    std::fs::write(&pi.dir, "not a directory")?;
                    None
                }
                "instructions-write-error" => {
                    let file = dir.join("session.jsonl");
                    std::fs::write(&file, "")?;
                    pi.keep("s-1", &file, &dir, "")?;
                    std::fs::write(pi.dir.join("guides"), "not a directory")?;
                    Some("s-1")
                }
                "pointer-error" => {
                    config::private_dir(&pi.dir)?;
                    std::fs::write(pi.pointer_path("s-1")?, "not json")?;
                    Some("s-1")
                }
                _ => None,
            };
            let mut store = crate::store::Store::open(":memory:")?;
            let now = now_ms();
            store.queue_triage_event(
                crate::triage::AgentEvent {
                    event_id: "event-1".into(),
                    ts_ms: now,
                    ..Default::default()
                },
                now,
            )?;
            let TriageAdmission::Claimed(attempt) =
                store.admit_triage("network", "boot", 1, 1, now)?
            else {
                panic!("the first event must reserve one unit");
            };
            let result = run_manager(
                &config,
                "review event-1",
                previous,
                |_| async { anyhow::Ok(()) },
                watch::channel(false).1,
                |_| async { anyhow::Ok(()) },
            )
            .await
            .map(|_| ());
            let error = result.as_ref().expect_err("controlled pi failure");
            let evidence = error.downcast_ref::<ManagerError>();
            let actual = evidence.map(|error| (error.start, error.failure.kind));
            store.finish_triage(attempt, &result)?;
            let status = store.triage_status("network", "boot", None, 1, now)?;
            assert_eq!(
                (
                    actual,
                    status.budget.used,
                    status.pending_events,
                    status.unknown_events
                ),
                (
                    Some((expected_start, expected_kind)),
                    charged,
                    pending,
                    1 - pending
                ),
                "{mode}"
            );
            if pending == 1 {
                if mode == "instructions-error" {
                    std::fs::remove_file(&pi.dir)?;
                }
                if mode == "instructions-write-error" {
                    std::fs::remove_file(pi.dir.join("guides"))?;
                }
                config.provider_env = env(&dir, "reply");
                let TriageAdmission::Claimed(retry) =
                    store.admit_triage("network", "boot", 1, 1, now)?
                else {
                    panic!("a call proven not started must be retryable");
                };
                let result = run_manager(
                    &config,
                    "review event-1",
                    None,
                    |_| async { anyhow::Ok(()) },
                    watch::channel(false).1,
                    |_| async { anyhow::Ok(()) },
                )
                .await?;
                assert_eq!(result.output, "Hello");
                store.finish_triage(retry, &Ok(()))?;
                let status = store.triage_status("network", "boot", None, 1, now)?;
                assert_eq!(
                    (
                        status.budget.used,
                        status.pending_events,
                        status.unknown_events
                    ),
                    (1, 0, 0),
                    "{mode} recovers without losing the event"
                );
            }
            assert!(matches!(
                store.admit_triage("network", "boot", 1, 1, now)?,
                TriageAdmission::Empty
            ));
            drop(store);
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_prompt_pi_refuses_rejects_the_work() -> Result<()> {
        let dir = scratch("refuse");
        let (node, network) = node(&dir, "refuse").await?;
        dispatch(&node, &network, &dir).await?;
        assert_eq!(
            work(&node, WorkState::finished).await?.state,
            WorkState::Finished {
                outcome: WorkOutcome::Rejected
            }
        );
        assert_eq!(
            {
                let network = network.to_owned();
                node.store.call("pi.work", move |store| store.work(&network, "w-1")).await
            }?
                .context("work")?
                .failure,
            Some(FailureKind::MissingKey)
        );
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
