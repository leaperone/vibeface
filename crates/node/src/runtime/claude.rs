//! Claude Code on this device. A bot turn and a dispatched work are each one
//! `claude -p --input-format stream-json --output-format stream-json` process in its own group.
//! The Node names the session and the user message before anything is sent, so a turn it lost
//! sight of is read back from Claude Code's own transcript and never sent again. Hooks of the
//! owner's own sessions hold permission requests and questions for the bot.
use super::executor::{Executor, SteerAck, SteerTrace};
use super::*;
use crate::manager::{self, CodexTurn, ManagerConfig, ManagerError, ManagerFailure};
use crate::scheduling::SteerTarget;
use crate::triage::RequestKind;
use crate::work::{Probe, Signal, WorkRecord, WorkSpec};
use futures_util::future::BoxFuture;
use std::collections::HashSet;
use std::io::BufRead as _;
use std::sync::{
    PoisonError, Weak,
    atomic::{AtomicBool, Ordering},
};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use vibeface_bridge::failure::ProviderStage;
use vibeface_bridge::{
    agent::{self, AgentResult},
    failure::{ExecFailure, FailureKind},
    turn::TurnStatus,
};

/// Process groups of the running Claude Code processes, with the program each one runs.
pub(super) const PIDFILE: &str = "claude.pid";
/// How long an interrupted turn has to end before its process group is ended.
const STOP_WAIT: Duration = Duration::from_secs(6);
/// How long a work's turn may outlive the interrupt before its process group is ended.
const INTERRUPT_GRACE: Duration = Duration::from_secs(10);
/// Seeds of the IDs a work's session and user message get from the work ID.
const SESSION_SEED: &str = "claude-session:";
const MESSAGE_SEED: &str = "claude-user:";

/// The process groups of running agent processes, by the program each one runs. The pidfile
/// lists them, so a Node that could not end them ends them when it starts again.
pub struct Processes {
    pidfile: PathBuf,
    running: std::sync::Mutex<BTreeMap<u32, PathBuf>>,
}

impl Processes {
    pub(crate) fn new(pidfile: PathBuf) -> Self {
        Self {
            pidfile,
            running: Default::default(),
        }
    }

    /// Ends the processes an earlier Node run left in `pidfile` before tracking this run's.
    pub(super) fn open(pidfile: PathBuf) -> Self {
        vibeface_bridge::agent::SdkBridge::kill_orphan(&pidfile);
        Self::new(pidfile)
    }

    /// Ends every recorded process; the Node calls this before it exits.
    pub(super) async fn stop(&self) {
        let pidfile = self.pidfile.clone();
        let _ = tokio::task::spawn_blocking(move || {
            vibeface_bridge::agent::SdkBridge::kill_orphan(&pidfile)
        })
        .await;
    }

    pub(super) fn track(&self, group: u32, bin: &Path) -> Result<()> {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        running.insert(group, bin.to_owned());
        self.save(&running)
            .context("record the agent process group")
    }

    pub(super) fn forget(&self, group: u32) {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if running.remove(&group).is_some() {
            let _ = self.save(&running);
        }
    }

    pub(super) fn tracked(&self, group: u32) -> bool {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&group)
    }

    fn save(&self, running: &BTreeMap<u32, PathBuf>) -> Result<()> {
        if running.is_empty() {
            return match std::fs::remove_file(&self.pidfile) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
                _ => Ok(()),
            };
        }
        let lines: String = running
            .iter()
            .map(|(group, bin)| format!("{group} {}\n", bin.display()))
            .collect();
        config::private_write(&self.pidfile, lines.as_bytes())
    }
}

/// One line of the `stream-json` output, as far as a turn needs it.
#[derive(Debug, PartialEq)]
enum Frame {
    Compacted,
    /// `system/init`: the turn runs in this session.
    Started {
        session: String,
    },
    /// A whole text block of the reply.
    Text {
        id: String,
        text: String,
    },
    ToolStarted {
        id: String,
        name: &'static str,
    },
    ToolEnded {
        id: String,
    },
    /// The `result` that ends the turn.
    Ended(Ending),
}

/// How a turn ended.
#[derive(Clone, Debug, PartialEq)]
struct Ending {
    status: TurnStatus,
    text: String,
    error: Option<ExecFailure>,
}

impl Ending {
    /// The `result` frame of a turn.
    fn of(result: &Value) -> Self {
        let text = result["result"].as_str().unwrap_or_default().to_owned();
        if result["terminal_reason"]
            .as_str()
            .is_some_and(|reason| reason.starts_with("aborted"))
        {
            return Self {
                status: TurnStatus::Interrupted,
                text,
                error: None,
            };
        }
        if result["subtype"] == "success" && result["is_error"] != true {
            return Self {
                status: TurnStatus::Completed,
                text,
                error: None,
            };
        }
        let errors = result["errors"]
            .as_array()
            .map(|errors| {
                let errors: Vec<&str> = errors.iter().filter_map(Value::as_str).collect();
                errors.join("; ")
            })
            .unwrap_or_default();
        let detail = [errors, text]
            .into_iter()
            .find(|detail| !detail.trim().is_empty())
            .unwrap_or_else(|| {
                let subtype = result["subtype"].as_str().unwrap_or("an error");
                format!("Claude Code ended the turn with {subtype}")
            });
        Self::failed(ExecFailure::upstream(detail, FailureKind::Unknown))
    }

    fn failed(failure: ExecFailure) -> Self {
        Self {
            status: TurnStatus::Failed,
            text: String::new(),
            error: Some(failure),
        }
    }

    fn probe(self, turn_id: String) -> Probe {
        Probe::Terminal {
            turn_id,
            status: self.status,
            text: self.text,
            error: self.error,
        }
    }
}

/// The frames one output line carries. A sub-agent's frames are not the reply.
fn frames(line: &str) -> Vec<Frame> {
    let Ok(frame) = serde_json::from_str::<Value>(line) else {
        return vec![];
    };
    if frame["parent_tool_use_id"].is_string() {
        return vec![];
    }
    let blocks = || frame["message"]["content"].as_array().into_iter().flatten();
    match frame["type"].as_str() {
        Some("system") if frame["subtype"] == "compact_boundary" => vec![Frame::Compacted],
        Some("system") if frame["subtype"] == "init" => frame["session_id"]
            .as_str()
            .map(|session| Frame::Started {
                session: session.into(),
            })
            .into_iter()
            .collect(),
        Some("assistant") => {
            let uuid = frame["uuid"].as_str().unwrap_or("assistant");
            blocks()
                .enumerate()
                .filter_map(|(index, block)| match block["type"].as_str()? {
                    "text" => Some(Frame::Text {
                        id: match index {
                            0 => uuid.to_owned(),
                            index => format!("{uuid}:{index}"),
                        },
                        text: block["text"].as_str()?.to_owned(),
                    }),
                    "tool_use" => Some(Frame::ToolStarted {
                        id: block["id"].as_str()?.to_owned(),
                        name: tool_kind(block["name"].as_str().unwrap_or_default()),
                    }),
                    _ => None,
                })
                .collect()
        }
        Some("user") => blocks()
            .filter(|block| block["type"] == "tool_result")
            .filter_map(|block| {
                Some(Frame::ToolEnded {
                    id: block["tool_use_id"].as_str()?.to_owned(),
                })
            })
            .collect(),
        Some("result") => vec![Frame::Ended(Ending::of(&frame))],
        _ => vec![],
    }
}

/// How the chat names a Claude Code tool.
fn tool_kind(name: &str) -> &'static str {
    match name {
        "Bash" | "BashOutput" | "KillShell" => "command",
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => "file change",
        "WebSearch" | "WebFetch" => "web search",
        _ => "tool",
    }
}

fn clip(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .take(limit)
        .collect()
}

fn tool_progress(id: &str, name: &str, status: &str) -> Value {
    json!({"type":"tool","item_id":clip(id, 128),"name":name,"status":status})
}

/// What the frames of one turn said so far.
#[derive(Default)]
struct Turn {
    session: Option<String>,
    tools: HashMap<String, &'static str>,
    ending: Option<Ending>,
}

impl Turn {
    /// Takes one frame in; returns what the chat shows of it, shaped as `native_progress`.
    fn apply(&mut self, frame: Frame) -> Option<Value> {
        match frame {
            Frame::Compacted => Some(json!({"type":"context_compacted"})),
            Frame::Started { session } => {
                self.session = Some(session);
                Some(json!({"type":"status","status":"running"}))
            }
            Frame::Text { id, text } => Some(
                json!({"type":"text_snapshot","item_id":clip(&id, 128),"text":clip(&text, 8000)}),
            ),
            Frame::ToolStarted { id, name } => {
                let progress = tool_progress(&id, name, "running");
                self.tools.insert(id, name);
                Some(progress)
            }
            Frame::ToolEnded { id } => {
                let name = self.tools.remove(&id)?;
                Some(tool_progress(&id, name, "completed"))
            }
            Frame::Ended(ending) => {
                let progress = json!({"type":"status","status":ending.status.as_str()});
                self.ending = Some(ending);
                Some(progress)
            }
        }
    }
}

/// Where Claude Code keeps its transcripts for a process with `env` over the Node's own.
fn projects_dir(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    let home = env
        .get("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".claude")))?;
    Some(home.join("projects"))
}

/// The transcript of `session`, in whichever project directory Claude Code filed it.
fn transcript(projects: Option<&Path>, session: &str) -> Option<PathBuf> {
    uuid::Uuid::parse_str(session).ok()?;
    let name = format!("{session}.jsonl");
    std::fs::read_dir(projects?)
        .ok()?
        .flatten()
        .map(|project| project.path().join(&name))
        .find(|path| path.is_file())
}

/// The records of a transcript, in order; a sub-agent's are left out.
fn records(path: &Path) -> impl Iterator<Item = Value> {
    std::fs::File::open(path)
        .ok()
        .map(std::io::BufReader::new)
        .into_iter()
        .flat_map(|file| file.lines().map_while(Result::ok))
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .filter(|record| record["isSidechain"] != true)
}

/// The directory a session ran in.
fn session_cwd(transcript: &Path) -> Option<PathBuf> {
    records(transcript).find_map(|record| record["cwd"].as_str().map(PathBuf::from))
}

/// What a transcript shows of the turn the user message `message` started.
#[derive(Debug, PartialEq)]
enum Seen {
    /// No such message: Claude Code never took it.
    Unsent,
    /// The message is there and its turn has not ended.
    Open,
    Ended(Ending),
}

fn text_of(message: &Value) -> String {
    if let Some(text) = message["content"].as_str() {
        return text.to_owned();
    }
    let texts: Vec<&str> = message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect();
    texts.join("\n")
}

/// Reads the turn back from the transcript of a process that no longer runs. Every block of a
/// message is its own record and carries the message's stop reason, so the turn ends with the
/// message that stopped at `end_turn`, an API error or an interruption; what follows belongs to
/// later turns.
fn seen(transcript: Option<&Path>, message: &str) -> Seen {
    let Some(path) = transcript else {
        return Seen::Unsent;
    };
    // ponytail: reads the whole transcript on every probe; keep an offset if sessions grow large.
    let mut records = records(path);
    if !records
        .by_ref()
        .any(|record| record["type"] == "user" && record["uuid"] == message)
    {
        return Seen::Unsent;
    }
    let (mut reply, mut current, mut done) = (String::new(), Value::Null, false);
    for record in records {
        let body = &record["message"];
        match record["type"].as_str() {
            Some("user") if done => break,
            Some("assistant") if done && body["id"] != current => break,
            Some("assistant") => {
                if record["isApiErrorMessage"] == true {
                    return Seen::Ended(Ending::failed(ExecFailure::upstream(
                        text_of(body),
                        FailureKind::Unknown,
                    )));
                }
                if body["id"] != current {
                    reply.clear();
                    current = body["id"].clone();
                }
                let text = text_of(body);
                if !text.is_empty() {
                    if !reply.is_empty() {
                        reply.push('\n');
                    }
                    reply.push_str(&text);
                }
                done |= body["stop_reason"] == "end_turn";
            }
            Some("user")
                if record["interruptedMessageId"].is_string()
                    || text_of(body).starts_with("[Request interrupted") =>
            {
                return Seen::Ended(Ending {
                    status: TurnStatus::Interrupted,
                    text: reply,
                    error: None,
                });
            }
            Some("user") if !text_of(body).is_empty() => break,
            _ => {}
        }
    }
    match done {
        true => Seen::Ended(Ending {
            status: TurnStatus::Completed,
            text: reply,
            error: None,
        }),
        false => Seen::Open,
    }
}

/// A UUID that `id` always maps to; Claude Code takes only UUIDs as session and message IDs.
fn derived(seed: &str, id: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(format!("{seed}{id}").as_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_random_bytes(bytes)
        .into_uuid()
        .to_string()
}

fn user_frame(prompt: &str, message: &str) -> Value {
    json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":prompt}]},
        "parent_tool_use_id":null,"session_id":"","uuid":message})
}

fn interrupt_frame() -> Value {
    json!({"type":"control_request","request_id":uuid::Uuid::new_v4().to_string(),
        "request":{"subtype":"interrupt"}})
}

/// How one `claude -p` process starts.
struct Launch<'a> {
    bin: &'a Path,
    cwd: &'a Path,
    /// The session to resume, or to start under this ID.
    session: &'a str,
    resume: bool,
    model: &'a str,
    effort: Option<&'a str>,
    instructions: &'a str,
    env: &'a BTreeMap<String, String>,
}

/// A running `claude -p`: its pipes and process group.
struct Process {
    child: Child,
    group: u32,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    lines: Lines<BufReader<ChildStdout>>,
    /// The end of what it wrote to stderr, once it closed it.
    stderr: Option<tokio::task::JoinHandle<String>>,
}

impl Launch<'_> {
    fn spawn(&self) -> Result<Process, ExecFailure> {
        let mut command = Command::new(self.bin);
        command
            .args(["-p", "--input-format", "stream-json", "--output-format"])
            .args([
                "stream-json",
                "--verbose",
                "--permission-mode",
                "bypassPermissions",
            ])
            .arg(if self.resume {
                "--resume"
            } else {
                "--session-id"
            })
            .arg(self.session);
        if !self.model.is_empty() {
            command.args(["--model", self.model]);
        }
        if let Some(effort) = self.effort {
            command.args(["--effort", effort]);
        }
        if !self.instructions.is_empty() {
            command.args(["--append-system-prompt", self.instructions]);
        }
        // The Node's own session variables would make the process a nested or interactive
        // session; each turn gets only its own.
        for (name, _) in std::env::vars_os() {
            let text = name.to_string_lossy();
            if text.starts_with("VIBEFACE_")
                || matches!(text.as_ref(), "CLAUDECODE" | "CLAUDE_CODE_ENTRYPOINT")
            {
                command.env_remove(&name);
            }
        }
        command
            .envs(self.env)
            .current_dir(self.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            let kind = match error.kind() {
                std::io::ErrorKind::NotFound => FailureKind::MissingBinary,
                _ => FailureKind::Unknown,
            };
            ExecFailure::new(kind, format!("could not start Claude Code: {error}"))
        })?;
        let (Some(group), Some(stdin), Some(stdout), Some(stderr)) = (
            child.id(),
            child.stdin.take(),
            child.stdout.take(),
            child.stderr.take(),
        ) else {
            return Err(ExecFailure::new(
                FailureKind::Lost,
                "Claude Code exited as it started",
            ));
        };
        let stderr = tokio::spawn(async move {
            let mut tail = String::new();
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tail.push_str(&line);
                tail.push('\n');
                keep_end(&mut tail);
            }
            tail.trim().to_owned()
        });
        Ok(Process {
            child,
            group,
            stdin: Arc::new(Mutex::new(Some(stdin))),
            lines: BufReader::new(stdout).lines(),
            stderr: Some(stderr),
        })
    }
}

/// Keeps the last 2000 characters of `tail` once it grows past 4000.
fn keep_end(tail: &mut String) {
    let count = tail.chars().count();
    if count > 4000 {
        *tail = tail.chars().skip(count - 2000).collect();
    }
}

async fn send(stdin: &Mutex<Option<ChildStdin>>, frame: &Value) -> Result<()> {
    let mut stdin = stdin.lock().await;
    let pipe = stdin
        .as_mut()
        .context("Claude Code no longer takes input")?;
    pipe.write_all(format!("{frame}\n").as_bytes()).await?;
    pipe.flush().await?;
    Ok(())
}

impl Process {
    /// Interrupts the turn and waits for its end; true once it ended or its group is gone.
    async fn stop(&mut self, turn: &mut Turn) -> bool {
        let _ = send(&self.stdin, &interrupt_frame()).await;
        let ended = tokio::time::timeout(STOP_WAIT, async {
            while let Ok(Some(line)) = self.lines.next_line().await {
                for frame in frames(&line) {
                    turn.apply(frame);
                }
                if turn.ending.is_some() {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        agent::terminate_process_group(&mut self.child, self.group).await || ended
    }

    /// Closes its input, so it exits once its turn is done, and ends its group.
    async fn end(&mut self, processes: &Processes) {
        self.stdin.lock().await.take();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
        if agent::terminate_process_group(&mut self.child, self.group).await {
            processes.forget(self.group);
        }
    }

    /// What it wrote to stderr before it exited.
    async fn stderr(&mut self) -> String {
        match self.stderr.take() {
            Some(tail) => tokio::time::timeout(Duration::from_secs(2), tail)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default(),
            None => String::new(),
        }
    }
}

/// One bot turn. The session is saved before the process starts and the prompt is sent as a
/// user message that carries the turn's ID. A session Claude Code no longer has goes on in a
/// new one with the full prompt.
#[expect(
    clippy::too_many_arguments,
    reason = "run_manager's arguments and the SDK"
)]
pub(crate) async fn run_bot<F, P, PF>(
    config: &ManagerConfig,
    bin: &Path,
    processes: &Processes,
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
    ensure!(
        config.workspace.is_absolute() && config.workspace.is_dir(),
        "manager workspace must be an existing absolute directory"
    );
    let secrets = manager::secret_values(config);
    let start = std::sync::Mutex::new(manager::StartEvidence::NotStarted);
    let stage = vibeface_bridge::failure::ProviderTrace::new("claude", ProviderStage::SessionStart);
    let error = |outcome, session: Option<&str>, failure: ExecFailure| -> anyhow::Error {
        ManagerError {
            stage: stage.current(),
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
    // Faults of this Node's own bookkeeping, not of Claude Code.
    let own = |detail: &str| ExecFailure::new(FailureKind::Unknown, detail);
    let stopped = |session: Option<&str>| {
        error(
            ManagerFailure::Cancelled,
            session,
            own("stopped before the Claude Code turn started"),
        )
    };
    if *cancel.borrow() {
        return Err(stopped(previous));
    }
    let projects = projects_dir(&config.provider_env);
    let resumed = previous.filter(|session| transcript(projects.as_deref(), session).is_some());
    let prompt = match (resumed, previous, &config.codex_turn) {
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
    let session = resumed.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned);
    let native = Some(session.as_str());
    stage.enter(ProviderStage::Record);
    if let Err(e) = persist_session(session.clone()).await {
        // A stop fences the attempt, so saving its session fails then.
        return Err(if *cancel.borrow() {
            stopped(native)
        } else {
            let failure = own(&format!("persist the Claude Code session: {e}"));
            error(ManagerFailure::Failed, native, failure)
        });
    }
    if let Some(turn) = &config.codex_turn {
        (turn.record)(session.clone(), None).await.map_err(|e| {
            error(
                ManagerFailure::Failed,
                native,
                own(&format!("record the Claude reply locator: {e}")),
            )
        })?;
    }
    let mut env = config.provider_env.clone();
    env.extend(config.tool_env.clone());
    if let Some(token) = &config.tool_token {
        env.insert("VIBEFACE_TOOL_TOKEN".into(), token.clone());
    }
    // Claude Code keeps no instructions with a session, so every turn brings them.
    let instructions = manager::bot_instructions(
        config.tool_cli.as_deref(),
        &config.custom_tools,
        &config.instructions,
    );
    let message = config.codex_turn.as_ref().map_or_else(
        || uuid::Uuid::new_v4().to_string(),
        |turn| derived(MESSAGE_SEED, &turn.client_id),
    );
    if *cancel.borrow() {
        return Err(stopped(native));
    }
    let launch = Launch {
        bin,
        cwd: &config.workspace,
        session: &session,
        resume: resumed.is_some(),
        model: &config.model,
        effort: config.reasoning_effort.as_deref(),
        instructions: &instructions,
        env: &env,
    };
    stage.enter(ProviderStage::Launch);
    let mut process = launch
        .spawn()
        .map_err(|failure| error(ManagerFailure::Failed, native, failure))?;
    let sent = async {
        stage.enter(ProviderStage::Record);
        processes.track(process.group, bin)?;
        if let Some(started) = &config.process_started {
            started(process.group).await?;
        }
        stage.enter(ProviderStage::TurnStart);
        *start.lock().unwrap() = manager::StartEvidence::Unknown;
        send(&process.stdin, &user_frame(prompt, &message)).await
    }
    .await;
    if let Err(e) = sent {
        process.end(processes).await;
        let failure = own(&format!("start the Claude Code turn: {e}"));
        return Err(error(ManagerFailure::Failed, native, failure));
    }
    let mut turn = Turn::default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(config.timeout_secs);
    let outcome = 'turn: loop {
        let line = tokio::select! {
            biased;
            _ = agent::wait_for_cancel(&mut cancel) => {
                stage.enter(ProviderStage::Interrupt);
                break if process.stop(&mut turn).await {
                    Err(error(ManagerFailure::Cancelled, native, own("Claude Code turn interrupted")))
                } else {
                    Err(error(ManagerFailure::Unknown, native, own("Claude Code did not confirm the interrupt")))
                };
            }
            _ = tokio::time::sleep_until(deadline) => {
                let kind = match process.stop(&mut turn).await {
                    true => ManagerFailure::Failed,
                    false => ManagerFailure::Unknown,
                };
                break Err(error(kind, native, ExecFailure::new(FailureKind::TimedOut, "Claude Code management timed out")));
            }
            line = process.lines.next_line() => line,
        };
        let Ok(Some(line)) = line else {
            // Its group ends below, so nothing of the turn runs on.
            let failure = match &turn.session {
                Some(_) => ExecFailure::new(
                    FailureKind::Lost,
                    "Claude Code exited before the turn ended",
                ),
                None => {
                    let detail = process.stderr().await;
                    ExecFailure::upstream(
                        if detail.is_empty() {
                            "Claude Code exited before the turn started".into()
                        } else {
                            detail
                        },
                        FailureKind::Lost,
                    )
                }
            };
            break Err(error(ManagerFailure::Failed, native, failure));
        };
        for frame in frames(&line) {
            if let Some(update) = turn.apply(frame)
                && let Err(e) = progress(manager::redact(update, &secrets)).await
            {
                break 'turn Err(error(ManagerFailure::Unknown, native, own(&e.to_string())));
            }
        }
        if turn.session.is_some() {
            stage.enter(ProviderStage::Run);
            *start.lock().unwrap() = manager::StartEvidence::Started;
        }
        if let Some(ending) = turn.ending.take() {
            break Ok(ending);
        }
    };
    process.end(processes).await;
    let ending = outcome?;
    match ending.status {
        TurnStatus::Completed => {}
        TurnStatus::Interrupted => {
            let (outcome, failure) = manager::interrupted(&cancel, Provider::Claude);
            return Err(error(outcome, native, failure));
        }
        _ => {
            let failure = ending
                .error
                .unwrap_or_else(|| own("Claude Code turn failed"));
            return Err(error(ManagerFailure::Failed, native, failure));
        }
    }
    // Only a completed native turn advances the session's covered prompt.
    if let Some(turn) = &config.codex_turn
        && let Err(e) = (turn.record)(session.clone(), Some(message)).await
    {
        tracing::warn!(event = "claude.turn_unrecorded", error = %e, "the next Claude Code turn repeats what this one was shown");
    }
    Ok(AgentResult {
        session_id: Some(session),
        output: manager::redact_string(ending.text, &secrets),
        usage: None,
    })
}

/// Claude Code's side of this device: the process groups of bot turns and works, and the works
/// whose process runs now.
pub(crate) struct Claude {
    processes: Arc<Processes>,
    live: std::sync::Mutex<HashMap<String, Live>>,
    /// Sessions a work is starting in, until its process is live or the start failed.
    starting: std::sync::Mutex<HashSet<String>>,
    /// Where the last started work's Claude Code keeps transcripts; a work started before this
    /// Node did is read back from there too.
    projects: std::sync::Mutex<Option<PathBuf>>,
}

/// Releases a session claimed for a starting work; by then its process is live or never ran.
struct Claim<'a>(&'a Claude, String);

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.0.starting().remove(&self.1);
    }
}

/// A work whose process runs now.
struct Live {
    session: String,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    group: u32,
    interrupted: Arc<AtomicBool>,
}

impl Claude {
    /// Ends the processes an earlier Node run left, so their turns are read back as they are.
    pub(super) fn open(dir: &Path, env: &BTreeMap<String, String>) -> Self {
        Self {
            processes: Arc::new(Processes::open(dir.join(PIDFILE))),
            live: Default::default(),
            starting: Default::default(),
            projects: std::sync::Mutex::new(projects_dir(env)),
        }
    }

    pub(super) fn processes(&self) -> Arc<Processes> {
        self.processes.clone()
    }

    /// Ends every running Claude Code process; the Node calls this before it exits.
    pub(super) async fn stop(&self) {
        self.processes.stop().await;
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<String, Live>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn starting(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.starting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) async fn probe_reply(
        &self,
        session: &str,
        native_turn: Option<&str>,
        client_id: &str,
    ) -> Result<Probe> {
        uuid::Uuid::parse_str(session)?;
        vibeface_protocol::validate_id(client_id).map_err(anyhow::Error::msg)?;
        let message = derived(MESSAGE_SEED, client_id);
        if native_turn.is_some_and(|turn| turn != message) {
            return Ok(Probe::Missing);
        }
        let path = transcript(self.projects().as_deref(), session);
        Ok(
            match path.as_deref().map(|path| seen(Some(path), &message)) {
                Some(Seen::Ended(ending)) => ending.probe(message),
                Some(Seen::Open | Seen::Unsent) | None => Probe::Missing,
            },
        )
    }

    fn projects(&self) -> Option<PathBuf> {
        self.projects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Follows a work's process to its end and reports it. Without the Node it stops at once; the
/// Node's next start ends the process and reads the turn back.
#[expect(clippy::too_many_arguments, reason = "everything one follow needs")]
async fn follow(
    node: Weak<Runtime>,
    network: String,
    work_id: String,
    session: String,
    message: String,
    mut process: Process,
    interrupted: Arc<AtomicBool>,
    projects: Option<PathBuf>,
) {
    let mut turn = Turn::default();
    loop {
        let line = process.lines.next_line().await;
        let Some(node) = node.upgrade() else {
            return;
        };
        let Ok(Some(line)) = line else {
            break;
        };
        for frame in frames(&line) {
            let signal = match &frame {
                Frame::Started { .. } if turn.session.is_none() => Some(Signal::TurnStarted {
                    turn_id: message.clone(),
                    epoch: 0,
                }),
                Frame::Text { text, .. } => Some(Signal::Text(text.clone())),
                _ => None,
            };
            if let Some(progress) = turn.apply(frame)
                && let Ok(target_node) = node.identity.device_id()
                && let Ok(Some(record)) = node.store.lock().await.work_record(&work_id)
            {
                let sequence = node.progress_sequence.fetch_add(1, Ordering::Relaxed) + 1;
                node.publish_progress(
                    &network,
                    json!({"type":"native_progress","conversation_id":record.conversation,
                        "work_id":work_id,"target_node":target_node,"sequence":sequence,
                        "progress":progress}),
                );
            }
            if let Some(signal) = signal
                && let Err(error) = node.signal_work(&network, &work_id, signal).await
            {
                tracing::warn!(event = "claude.work_update_pending", %error, work_id, "a Claude Code work update was not recorded");
            }
        }
        if turn.ending.is_some() {
            break;
        }
    }
    process.stdin.lock().await.take();
    let _ = tokio::time::timeout(Duration::from_secs(5), process.child.wait()).await;
    let stopped = agent::terminate_process_group(&mut process.child, process.group).await;
    let Some(node) = node.upgrade() else {
        return;
    };
    // From here a probe reads the finished transcript.
    node.claude.live().remove(&work_id);
    if stopped {
        node.claude.processes.forget(process.group);
    }
    let signals = match turn.ending {
        Some(ending) => vec![Signal::Probed(ending.probe(message))],
        None => match seen(
            transcript(projects.as_deref(), &session).as_deref(),
            &message,
        ) {
            Seen::Ended(ending) => vec![Signal::Probed(ending.probe(message))],
            Seen::Open => vec![Signal::Probed(Probe::Terminal {
                turn_id: message,
                status: TurnStatus::Interrupted,
                text: String::new(),
                error: (!interrupted.load(Ordering::Relaxed)).then(|| {
                    ExecFailure::new(
                        FailureKind::Lost,
                        "Claude Code exited before the turn ended",
                    )
                }),
            })],
            // Nothing ran: it is refused, or, when its start was already recorded, failed.
            Seen::Unsent => {
                let detail = process.stderr().await;
                let failure = ExecFailure::upstream(
                    if detail.is_empty() {
                        "Claude Code exited before it took the prompt".into()
                    } else {
                        detail
                    },
                    FailureKind::Lost,
                );
                vec![
                    Signal::StartRejected(failure.clone()),
                    Signal::Probed(Ending::failed(failure).probe(message)),
                ]
            }
        },
    };
    for signal in signals {
        match node.signal_work(&network, &work_id, signal).await {
            Ok(record) if record.state.finished() => break,
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(event = "claude.work_end_pending", %error, work_id, "the end of a Claude Code work waits for its probe")
            }
        }
    }
}

/// A work runs in a session the Node names from the work ID, or in the session of the work it
/// continues, with its prompt as a user message whose ID comes from the work ID too. The
/// Node starts the process itself, so the dispatching Agent sends nothing.
impl Executor for Claude {
    fn start<'a>(
        &'a self,
        node: &'a Arc<Runtime>,
        network: &'a str,
        work: &'a WorkSpec,
        thread: Option<&'a str>,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let work_id = work.work_id.as_str();
            let reject = |failure: ExecFailure| -> Result<bool> { Err(failure.into()) };
            let env = node.provider_env(Provider::Claude);
            let projects = projects_dir(&env);
            *self.projects.lock().unwrap_or_else(PoisonError::into_inner) = projects.clone();
            let session = thread.map_or_else(|| derived(SESSION_SEED, work_id), str::to_owned);
            let message = derived(MESSAGE_SEED, work_id);
            // Two processes would interleave their turns in one transcript.
            let claimed = {
                let live = self.live();
                !live.values().any(|live| live.session == session)
                    && self.starting().insert(session.clone())
            };
            if !claimed {
                return reject(ExecFailure::new(
                    FailureKind::WriterBusy,
                    "Claude Code still runs another work in this session",
                ));
            }
            let _claim = Claim(self, session.clone());
            // A session is resumed where it ran; Claude Code files it under that directory.
            let cwd = thread
                .and_then(|session| transcript(projects.as_deref(), session))
                .and_then(|path| session_cwd(&path))
                .unwrap_or_else(|| PathBuf::from(&work.cwd));
            if !cwd.is_dir() {
                return reject(ExecFailure::new(
                    FailureKind::Rejected,
                    format!("the work directory {} does not exist", cwd.display()),
                ));
            }
            if work.thread.is_none() {
                node.signal_work(network, work_id, Signal::ThreadCreated(session.clone()))
                    .await?;
            }
            let bin = node.providers().bin(Provider::Claude);
            let launch = Launch {
                bin: &bin,
                cwd: &cwd,
                session: &session,
                resume: thread.is_some(),
                model: args["model"].as_str().unwrap_or_default(),
                // Claude Code's lowest effort is `low`.
                effort: args["effort"].as_str().map(|effort| match effort {
                    "none" | "minimal" => "low",
                    effort => effort,
                }),
                instructions: "",
                env: &env,
            };
            let mut process = match launch.spawn() {
                Ok(process) => process,
                Err(failure) => return reject(failure),
            };
            let sent = async {
                self.processes.track(process.group, &bin)?;
                send(&process.stdin, &user_frame(&work.prompt, &message)).await
            }
            .await;
            if let Err(e) = sent {
                process.end(&self.processes).await;
                return reject(ExecFailure::new(
                    FailureKind::Lost,
                    format!("Claude Code did not take the prompt: {e}"),
                ));
            }
            let interrupted = Arc::new(AtomicBool::new(false));
            self.live().insert(
                work_id.to_owned(),
                Live {
                    session: session.clone(),
                    stdin: process.stdin.clone(),
                    group: process.group,
                    interrupted: interrupted.clone(),
                },
            );
            tokio::spawn(follow(
                Arc::downgrade(node),
                network.to_owned(),
                work_id.to_owned(),
                session,
                message,
                process,
                interrupted,
                projects,
            ));
            Ok(false)
        })
    }

    fn probe<'a>(&'a self, work: &'a WorkRecord) -> BoxFuture<'a, Result<Probe>> {
        Box::pin(async move {
            let session = work.state.thread().context("the work has no session")?;
            let message = derived(MESSAGE_SEED, &work.work_id);
            // Its process reports the end itself, while the transcript may still be writing it.
            if self.live().contains_key(&work.work_id) {
                return Ok(Probe::Running {
                    turn_id: message,
                    epoch: 0,
                });
            }
            let path = transcript(self.projects().as_deref(), session);
            Ok(match seen(path.as_deref(), &message) {
                Seen::Ended(ending) => ending.probe(message),
                Seen::Unsent => Probe::NotStarted,
                // It ran under an earlier Node run, whose processes this one ended at start.
                Seen::Open => Probe::Terminal {
                    turn_id: message,
                    status: TurnStatus::Interrupted,
                    text: String::new(),
                    error: Some(ExecFailure::new(
                        FailureKind::Restarted,
                        "the device restarted while Claude Code ran this work",
                    )),
                },
            })
        })
    }

    fn interrupt<'a>(&'a self, work: &'a WorkRecord) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let (stdin, group, interrupted) = self
                .live()
                .get(&work.work_id)
                .map(|live| (live.stdin.clone(), live.group, live.interrupted.clone()))
                .context("this work's Claude Code process is not running")?;
            interrupted.store(true, Ordering::Relaxed);
            send(&stdin, &interrupt_frame()).await?;
            // A turn that outlives the interrupt ends with its process group.
            let processes = self.processes.clone();
            tokio::spawn(async move {
                tokio::time::sleep(INTERRUPT_GRACE).await;
                if processes.tracked(group) {
                    // SAFETY: kill(2) with a negative pid signals one process group and touches
                    // no memory; the group is still recorded, so its ID was not reused.
                    unsafe { libc::kill(-(group as i32), libc::SIGKILL) };
                }
            });
            Ok(())
        })
    }

    fn steer<'a>(&'a self, _: &'a SteerTarget, _: &'a str, _: &'a str) -> BoxFuture<'a, SteerAck> {
        Box::pin(async {
            SteerAck::Refused(
                "Claude Code takes no message into a running turn: a user message sent then waits for a turn of its own"
                    .into(),
            )
        })
    }

    fn read_back<'a>(
        &'a self,
        _: &'a SteerTarget,
        _: &'a str,
    ) -> BoxFuture<'a, Option<SteerTrace>> {
        Box::pin(async { None })
    }
}

/// What a `PermissionRequest` of the owner's own session asks, for the bot: the tool and what
/// the model said it is for, never the command or the file contents, and how to answer.
pub(crate) fn describe_request(input: &Value) -> String {
    let tool = input["tool_name"].as_str().unwrap_or("a tool");
    let tool_input = &input["tool_input"];
    if request_kind(input) == RequestKind::Question {
        let questions = crate::triage::question_lines(&tool_input["questions"]);
        return format!(
            "Claude Code asks: {}. Answer with {{\"answers\":{{\"QUESTION\":\"ANSWER\"}}}}.",
            questions.join("; ")
        );
    }
    let what = tool_input["description"]
        .as_str()
        .or_else(|| tool_input["file_path"].as_str())
        .map(|what| format!(": {what}"))
        .unwrap_or_default();
    format!(
        "{tool}{what}. Answer with {{\"behavior\":\"allow\"}} or {{\"behavior\":\"deny\",\"message\":\"WHY\"}}."
    )
}

/// `AskUserQuestion` asks a question; every other tool asks permission.
pub(crate) fn request_kind(input: &Value) -> RequestKind {
    match input["tool_name"] == "AskUserQuestion" {
        true => RequestKind::Question,
        false => RequestKind::Permission,
    }
}

/// What `vibeface hook claude` prints for the answer to its `PermissionRequest`: allow, with
/// the question's answers filled in, or deny. An allow never changes the tool's input. `None`
/// prints nothing, which leaves the decision to the session, as for `{"decision":null}`, an
/// allow that carries `updatedInput`, or an answer that fits neither.
pub fn hook_output(input: &Value, answer: &Value) -> Option<Value> {
    if input["hook_event_name"] != "PermissionRequest" {
        return None;
    }
    let decision = match (&answer["answers"], answer["behavior"].as_str()) {
        (Value::Object(answers), _) if request_kind(input) == RequestKind::Question => {
            if !answers.values().all(Value::is_string) {
                return None;
            }
            let mut updated = input["tool_input"].clone();
            updated.as_object_mut()?;
            updated["answers"] = Value::Object(answers.clone());
            json!({"behavior":"allow","updatedInput":updated})
        }
        // What was approved is what runs: an allow that rewrites the input is not this
        // approval, so it prints nothing and the session asks again.
        (_, Some("allow")) if answer.get("updatedInput").is_some() => return None,
        (_, Some("allow")) => json!({"behavior":"allow"}),
        (_, Some("deny")) => json!({"behavior":"deny",
            "message":answer["message"].as_str().unwrap_or("The owner's bot declined this."),
            "interrupt":answer["interrupt"] == true}),
        _ => return None,
    };
    Some(json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":decision}}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Genesis, Member, NetworkState};
    use crate::work::WorkState;

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude")
    }

    #[test]
    fn a_stderr_tail_of_wide_characters_keeps_its_end() {
        let mut tail = "错".repeat(1399) + "尾";
        keep_end(&mut tail);
        assert_eq!(tail.chars().count(), 1400);
        let mut tail = "错".repeat(4000) + "尾";
        keep_end(&mut tail);
        assert_eq!(tail.chars().count(), 2000);
        assert!(tail.ends_with('尾'));
    }

    /// A private directory with Claude Code's home and the fixture's log inside it.
    struct Place {
        dir: PathBuf,
    }

    impl Place {
        fn new() -> Result<Self> {
            let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../.artifacts")
                .join(format!("claude-{}", uuid::Uuid::new_v4()));
            config::private_dir(&dir)?;
            let dir = dir.canonicalize()?;
            std::fs::create_dir(dir.join("work"))?;
            Ok(Self { dir })
        }
        fn env(&self) -> BTreeMap<String, String> {
            BTreeMap::from([
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    self.dir.join("home").display().to_string(),
                ),
                (
                    "FAKE_CLAUDE_LOG".into(),
                    self.dir.join("fake.log").display().to_string(),
                ),
            ])
        }
        /// What the fixture logged: its arguments and every frame it read.
        fn log(&self) -> Vec<Value> {
            std::fs::read_to_string(self.dir.join("fake.log"))
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        }
        fn user_frames(&self) -> usize {
            self.log()
                .iter()
                .filter(|entry| entry["stdin"] == "user")
                .count()
        }
    }

    impl Drop for Place {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn bot_config(place: &Place, record: manager::TurnRecorder) -> ManagerConfig {
        ManagerConfig {
            sdk: manager::Sdk::Claude {
                bin: fixture(),
                processes: Arc::new(Processes::new(place.dir.join(PIDFILE))),
            },
            workspace: place.dir.join("work"),
            model: "haiku".into(),
            reasoning_effort: None,
            timeout_secs: 30,
            custom_tools: BTreeMap::new(),
            tool_cli: None,
            instructions: "Answer briefly.".into(),
            tool_env: BTreeMap::from([("VIBEFACE_NODE_URL".into(), "http://127.0.0.1:9".into())]),
            tool_token: Some("tool-token-fixture".into()),
            provider_env: place.env(),
            process_started: None,
            codex_turn: Some(CodexTurn {
                client_id: "attempt-1".into(),
                record,
                fresh_prompt: Some("fresh full context".into()),
            }),
        }
    }

    type Calls = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

    fn recorder() -> (manager::TurnRecorder, Calls) {
        let calls = Calls::default();
        let kept = calls.clone();
        let record: manager::TurnRecorder = Arc::new(move |session, turn| {
            kept.lock().unwrap().push((session, turn));
            Box::pin(async { Ok(()) })
        });
        (record, calls)
    }

    fn gated_config(place: &Place, client: &str) -> ManagerConfig {
        let (record, _) = recorder();
        let mut config = bot_config(place, record);
        config.timeout_secs = 30;
        config.codex_turn = Some(CodexTurn {
            client_id: client.into(),
            record: Arc::new(|_, _| Box::pin(async { Ok(()) })),
            fresh_prompt: None,
        });
        let gate = place.dir.join("concurrent-gate");
        std::fs::create_dir_all(&gate).unwrap();
        config
            .provider_env
            .insert("FAKE_CLAUDE_GATE_DIR".into(), gate.display().to_string());
        config.provider_env.insert(
            "FAKE_CLAUDE_GATE_RELEASE".into(),
            place.dir.join("concurrent-release").display().to_string(),
        );
        config
    }

    async fn wait_for_gate(place: &Place) -> Result<()> {
        let gate = place.dir.join("concurrent-gate");
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
        .context("all Claude turns reached the fixture gate")
    }

    async fn wait_for_interrupt(place: &Place) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if place
                    .log()
                    .iter()
                    .any(|entry| entry["subtype"] == "interrupt")
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("the cancelled Claude turn received an interrupt")
    }

    fn release(place: &Place) {
        std::fs::File::create(place.dir.join("concurrent-release")).unwrap();
    }

    fn concurrent_claude_turns(
        place: &Place,
        prompts: Vec<String>,
    ) -> Vec<tokio::task::JoinHandle<Result<AgentResult>>> {
        prompts
            .into_iter()
            .enumerate()
            .map(|(index, prompt)| {
                let config = gated_config(place, &format!("client-{index}"));
                tokio::spawn(async move {
                    manager::run_manager(
                        &config,
                        &prompt,
                        None,
                        |_| async { Ok(()) },
                        tokio::sync::watch::channel(false).1,
                        |_| async { Ok(()) },
                    )
                    .await
                })
            })
            .collect()
    }

    /// One bot turn; returns its result, the session saved first, and the progress shown.
    async fn bot_turn(
        place: &Place,
        config: &ManagerConfig,
        prompt: &str,
        previous: Option<&str>,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> (Result<AgentResult>, Option<(String, usize)>, Vec<Value>) {
        let saved = std::sync::Mutex::new(None);
        let shown = std::sync::Mutex::new(Vec::new());
        let result = manager::run_manager(
            config,
            prompt,
            previous,
            |session| {
                // How many frames the fixture had read when the session was saved.
                *saved.lock().unwrap() = Some((session, place.user_frames()));
                async { Ok(()) }
            },
            cancel,
            |update| {
                shown.lock().unwrap().push(update);
                async { Ok(()) }
            },
        )
        .await;
        (
            result,
            saved.into_inner().unwrap(),
            shown.into_inner().unwrap(),
        )
    }

    #[tokio::test]
    async fn bot_readback_requires_its_exact_message_and_explicit_ending() -> Result<()> {
        let place = Place::new()?;
        let reader = Claude::open(&place.dir, &place.env());
        let session = uuid::Uuid::new_v4().to_string();
        let project = projects_dir(&place.env())
            .context("projects")?
            .join("fixture");
        config::private_dir(&project)?;
        let path = project.join(format!("{session}.jsonl"));
        let message = derived(MESSAGE_SEED, "attempt-a");
        let later = derived(MESSAGE_SEED, "attempt-b");
        let user = |id: &str| json!({"type":"user","uuid":id,"message":{"role":"user","content":"question"}});
        let assistant = |id: &str, text: &str, stop: Value| json!({"type":"assistant","message":{"id":id,"content":[{"type":"text","text":text}],"stop_reason":stop}});
        let write = |records: Vec<Value>| -> Result<()> {
            std::fs::write(
                &path,
                records
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )?;
            Ok(())
        };
        assert_eq!(
            reader.probe_reply(&session, None, "attempt-a").await?,
            Probe::Missing
        );
        write(vec![
            user(&message),
            assistant("reply-a", "unfinished", Value::Null),
            user(&later),
            assistant("reply-b", "later result", json!("end_turn")),
        ])?;
        assert_eq!(
            reader.probe_reply(&session, None, "attempt-a").await?,
            Probe::Missing
        );
        write(vec![
            user(&message),
            assistant("reply-a", "first result", json!("end_turn")),
            user(&later),
            assistant("reply-b", "later result", json!("end_turn")),
        ])?;
        assert_eq!(
            reader.probe_reply(&session, None, "attempt-a").await?,
            Probe::Terminal {
                turn_id: message.clone(),
                status: TurnStatus::Completed,
                text: "first result".into(),
                error: None,
            }
        );
        assert_eq!(
            reader
                .probe_reply(&session, Some(&later), "attempt-a")
                .await?,
            Probe::Missing
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_bot_without_a_durable_locator_never_starts_claude() -> Result<()> {
        let place = Place::new()?;
        let record: manager::TurnRecorder =
            Arc::new(|_, _| Box::pin(async { anyhow::bail!("locator refused") }));
        let config = bot_config(&place, record);
        let (result, _, _) = bot_turn(
            &place,
            &config,
            "hello",
            None,
            tokio::sync::watch::channel(false).1,
        )
        .await;
        let error = result.err().context("expected locator failure")?;
        let error = error
            .downcast_ref::<ManagerError>()
            .context("structured error")?;
        assert_eq!(error.start, manager::StartEvidence::NotStarted);
        assert_eq!(error.stage, ProviderStage::Record);
        assert_eq!(place.user_frames(), 0);
        assert!(place.log().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn a_bot_turn_streams_its_reply_and_keeps_the_session() -> Result<()> {
        let place = Place::new()?;
        let (record, calls) = recorder();
        let mut config = bot_config(&place, record);
        config.reasoning_effort = Some("high".into());
        let (_, never) = tokio::sync::watch::channel(false);
        let (result, saved, shown) = bot_turn(&place, &config, "hello", None, never.clone()).await;
        let result = result?;
        let (session, frames_before) = saved.context("the session is saved")?;
        assert_eq!(
            frames_before, 0,
            "the session is saved before the prompt is sent"
        );
        assert_eq!(result.session_id.as_deref(), Some(session.as_str()));
        assert_eq!(result.output, "done: hello");
        assert!(shown.contains(&json!({"type":"status","status":"running"})));
        assert!(
            shown
                .iter()
                .any(|update| update["type"] == "text_snapshot" && update["text"] == "Looking.")
        );
        assert!(shown.contains(
            &json!({"type":"tool","item_id":"toolu_1","name":"command","status":"running"})
        ));
        assert!(shown.contains(
            &json!({"type":"tool","item_id":"toolu_1","name":"command","status":"completed"})
        ));
        assert_eq!(
            shown.last(),
            Some(&json!({"type":"status","status":"completed"}))
        );
        let message = derived(MESSAGE_SEED, "attempt-1");
        assert_eq!(
            *calls.lock().unwrap(),
            [
                (session.clone(), None),
                (session.clone(), Some(message.clone()))
            ],
            "the locator precedes send and the known turn advances coverage"
        );
        let log = place.log();
        let argv: Vec<&str> = log[0]["argv"]
            .as_array()
            .context("argv")?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for expected in [
            ["--session-id", session.as_str()],
            ["--permission-mode", "bypassPermissions"],
            ["--model", "haiku"],
            ["--effort", "high"],
            ["--append-system-prompt", "Answer briefly."],
        ] {
            assert!(argv.windows(2).any(|pair| pair == expected), "{argv:?}");
        }
        assert_eq!(
            log[0]["env"],
            json!({"VIBEFACE_TOOL_TOKEN":true,"VIBEFACE_NODE_URL":true,"CLAUDECODE":false,
                "CLAUDE_CODE_ENTRYPOINT":false,"FAKE_MARK":false})
        );
        assert_eq!(log[1]["uuid"], message.as_str());
        assert!(
            !place.dir.join(PIDFILE).exists(),
            "an ended turn leaves no process"
        );

        let (again, saved, _) =
            bot_turn(&place, &config, "again", Some(&session), never.clone()).await;
        assert_eq!(again?.output, "done: again");
        assert_eq!(
            saved.context("saved")?.0,
            session,
            "the next turn resumes the session"
        );
        assert!(
            place
                .log()
                .iter()
                .any(|entry| entry["argv"].as_array().is_some_and(|argv| argv
                    .windows(2)
                    .any(|pair| pair == [json!("--resume"), json!(session)])))
        );

        let gone = uuid::Uuid::new_v4().to_string();
        let (fresh, saved, _) = bot_turn(&place, &config, "only new", Some(&gone), never).await;
        let fresh = fresh?;
        assert_ne!(
            saved.context("saved")?.0,
            gone,
            "a lost session goes on in a new one"
        );
        assert_eq!(fresh.output, "done: fresh full context");
        Ok(())
    }

    #[tokio::test]
    async fn a_stopped_bot_turn_is_interrupted_and_its_process_ends() -> Result<()> {
        let place = Place::new()?;
        let (record, calls) = recorder();
        let config = bot_config(&place, record);
        let (stop, cancel) = tokio::sync::watch::channel(false);
        let turn = tokio::spawn({
            let config = config.clone();
            async move {
                manager::run_manager(
                    &config,
                    "hang on",
                    None,
                    |_| async { Ok(()) },
                    cancel,
                    |_| async { Ok(()) },
                )
                .await
            }
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while place.user_frames() == 0 {
            ensure!(Instant::now() < deadline, "the turn did not start");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stop.send(true)?;
        let Err(error) = turn.await? else {
            bail!("a stopped turn has no reply");
        };
        let error = error
            .downcast_ref::<ManagerError>()
            .context("a manager error")?;
        assert!(error.cancelled(), "{error}");
        assert!(place
            .log()
            .iter()
            .any(|entry| entry["stdin"] == "control_request" && entry["subtype"] == "interrupt"));
        let journal = calls.lock().unwrap();
        assert_eq!(journal.len(), 1);
        assert!(
            journal[0].1.is_none(),
            "an interrupted turn keeps its locator without advancing coverage"
        );
        assert!(!place.dir.join(PIDFILE).exists());
        Ok(())
    }

    #[tokio::test]
    async fn a_failed_bot_turn_names_the_failure() -> Result<()> {
        let place = Place::new()?;
        let (record, _) = recorder();
        let (_, never) = tokio::sync::watch::channel(false);
        let (result, _, _) =
            bot_turn(&place, &bot_config(&place, record), "fail now", None, never).await;
        let Err(error) = result else {
            bail!("an API error fails the turn");
        };
        let error = error
            .downcast_ref::<ManagerError>()
            .context("a manager error")?;
        assert_eq!(
            (error.outcome, error.failure.kind),
            (ManagerFailure::Failed, FailureKind::AuthFailed)
        );
        Ok(())
    }

    #[tokio::test]
    async fn sixteen_claude_turns_overlap_and_keep_their_outputs() -> Result<()> {
        let place = Place::new()?;
        let prompts: Vec<_> = (0..16).map(|i| format!("marker-{i}")).collect();
        let tasks = concurrent_claude_turns(&place, prompts.clone());
        wait_for_gate(&place).await?;
        assert_eq!(place.user_frames(), 16);
        release(&place);
        let mut sessions = std::collections::HashSet::new();
        for (index, task) in tasks.into_iter().enumerate() {
            let result = task.await??;
            sessions.insert(result.session_id.context("Claude session")?);
            assert_eq!(result.output, format!("done: marker-{index}"));
        }
        assert_eq!(sessions.len(), 16);
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_one_of_sixteen_claude_turns_leaves_the_others_running() -> Result<()> {
        let place = Place::new()?;
        let mut tasks = Vec::with_capacity(16);
        let mut stops = Vec::with_capacity(16);
        for index in 0..16 {
            let config = gated_config(&place, &format!("client-{index}"));
            let prompt = format!("marker-{index}");
            let (stop, cancel) = tokio::sync::watch::channel(false);
            stops.push(stop);
            tasks.push(tokio::spawn(async move {
                manager::run_manager(
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
        wait_for_gate(&place).await?;
        stops[0].send(true)?;
        wait_for_interrupt(&place).await?;
        release(&place);
        for (index, task) in tasks.into_iter().enumerate() {
            let result = task.await?;
            if index == 0 {
                let error = result.err().context("cancelled Claude turn")?;
                assert!(
                    error
                        .downcast_ref::<manager::ManagerError>()
                        .context("manager error")?
                        .cancelled()
                );
            } else {
                assert_eq!(result?.output, format!("done: marker-{index}"));
            }
        }
        assert_eq!(
            place
                .log()
                .iter()
                .filter(|entry| entry["subtype"] == "interrupt")
                .count(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn one_failed_claude_turn_does_not_stop_the_other_fifteen() -> Result<()> {
        let place = Place::new()?;
        let prompts: Vec<_> = (0..16)
            .map(|i| {
                if i == 0 {
                    "fail marker-0".into()
                } else {
                    format!("marker-{i}")
                }
            })
            .collect();
        let tasks = concurrent_claude_turns(&place, prompts);
        wait_for_gate(&place).await?;
        release(&place);
        for (index, task) in tasks.into_iter().enumerate() {
            let result = task.await?;
            if index == 0 {
                let error = result.err().context("failed Claude turn")?;
                assert_eq!(
                    error
                        .downcast_ref::<manager::ManagerError>()
                        .context("manager error")?
                        .outcome,
                    ManagerFailure::Failed
                );
            } else {
                assert_eq!(result?.output, format!("done: marker-{index}"));
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_bot_turn_claude_code_aborts_on_its_own_fails_instead_of_reading_as_stopped()
    -> Result<()> {
        let place = Place::new()?;
        let (record, _) = recorder();
        let (_, never) = tokio::sync::watch::channel(false);
        let (result, _, _) = bot_turn(
            &place,
            &bot_config(&place, record),
            "abort now",
            None,
            never,
        )
        .await;
        let Err(error) = result else {
            bail!("an aborted turn has no reply");
        };
        let error = error
            .downcast_ref::<ManagerError>()
            .context("a manager error")?;
        assert_eq!(error.outcome, ManagerFailure::Failed, "{error}");
        assert!(error.to_string().contains("without being asked"), "{error}");
        Ok(())
    }

    #[tokio::test]
    async fn a_bot_turn_shows_no_provider_key() -> Result<()> {
        let place = Place::new()?;
        let (record, _) = recorder();
        let mut config = bot_config(&place, record);
        let key = "sk-ant-fixture-secret";
        config
            .provider_env
            .insert("ANTHROPIC_API_KEY".into(), key.into());
        let (_, never) = tokio::sync::watch::channel(false);
        let (result, _, shown) =
            bot_turn(&place, &config, &format!("echo {key}"), None, never).await;
        assert_eq!(result?.output, "done: echo [redacted]");
        assert!(!json!(shown).to_string().contains(key));
        Ok(())
    }

    async fn node(place: &Place) -> Result<(Arc<Runtime>, LocalIdentity, String)> {
        let dir = place.dir.join("node");
        config::private_dir(&dir)?;
        let mut configuration = Config::local("127.0.0.1:0".parse()?, vec![], false);
        // Nothing listens there: the Node never reaches this computer's own App Server.
        let socket = format!(
            "/tmp/vf-{}.sock",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        configuration.providers.codex_app_server.socket = Some(socket.into());
        configuration.providers.execution = [Provider::Claude].into();
        configuration.bins.insert(Provider::Claude, fixture());
        configuration.save(&dir)?;
        config::private_write(&dir.join("data-epoch.json"), &config::data_epoch_marker())?;
        let identity = LocalIdentity::generate();
        config::private_write(&dir.join("identity.json"), &serde_json::to_vec(&identity)?)?;
        let mut env = crate::providers::ProviderEnv::default();
        env.apply(
            Provider::Claude,
            place
                .env()
                .into_iter()
                .map(|(name, value)| (name, Some(value)))
                .collect(),
        )?;
        env.save(&dir)?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let admin = LocalIdentity::generate();
        let state = NetworkState::new(Genesis::create(
            &LocalIdentity::generate(),
            Member::client(&admin, "admin", true)?,
            Member::node(&node.identity, "node")?,
        )?)?;
        node.install_membership(state).await?;
        let network = node.membership_state().await?.genesis.network_id;
        conversation(&node, &admin, &network).await?;
        Ok((node, admin, network))
    }

    /// The bot conversation "c", owned by this Node.
    async fn conversation(node: &Runtime, admin: &LocalIdentity, network: &str) -> Result<()> {
        let local = node.identity.device_id()?;
        let owner = admin.attest(
            network,
            "c",
            EventBody::ConversationOwner {
                scheduler_node: local.clone(),
                epoch: 1,
                attestation: None,
                cause: None,
            },
        )?;
        let owner = crate::model::FirstOwner {
            scheduler_node: local,
            attestation: owner.attestation().cloned().context("signed owner")?,
        };
        let bot = admin.attest(
            network,
            "c",
            EventBody::BotCreated {
                bot_id: "c".into(),
                name: "bot".into(),
                instructions: String::new(),
                project: None,
                attestation: None,
                cause: None,
            },
        )?;
        let mut store = node.store.lock().await;
        store.create_conversation(&node.identity, network, "c", "bot", owner, now_ms())?;
        store.append_event(&node.identity, network, "c", bot, vec![], now_ms())?;
        Ok(())
    }

    fn spec(node: &Runtime, place: &Place, call_ref: &str, prompt: &str) -> Result<WorkSpec> {
        let local = node.identity.device_id()?;
        Ok(WorkSpec {
            work_id: WorkSpec::id("c", "message:m", call_ref),
            conversation: "c".into(),
            trigger: "message:m".into(),
            target: local.clone(),
            dispatcher: local,
            call_ref: call_ref.into(),
            kind: WorkKind::Agent(Provider::Claude),
            cwd: place.dir.join("work").display().to_string(),
            title: "t".into(),
            prompt: prompt.into(),
            continues: None,
            parent: None,
            thread: None,
        })
    }

    /// Registers the work and has Claude Code start it, as `track_work` does.
    async fn dispatch(node: &Arc<Runtime>, network: &str, spec: &WorkSpec) -> Result<bool> {
        node.store
            .lock()
            .await
            .accept_work(&node.identity, network, spec.clone(), now_ms())?;
        let claude = node.executor(Provider::Claude)?;
        node.start_work(claude, network, spec, None, &json!({}))
            .await
    }

    async fn record(node: &Runtime, work_id: &str) -> Result<WorkRecord> {
        node.store
            .lock()
            .await
            .work_record(work_id)?
            .context("the work is recorded")
    }

    async fn until(
        node: &Runtime,
        work_id: &str,
        done: impl Fn(&WorkState) -> bool,
    ) -> Result<WorkRecord> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let record = record(node, work_id).await?;
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
    async fn a_work_runs_to_its_end_and_reads_back_the_same() -> Result<()> {
        let place = Place::new()?;
        let (node, _, network) = node(&place).await?;
        let spec = spec(&node, &place, "r1", "build it")?;
        assert!(
            !dispatch(&node, &network, &spec).await?,
            "the Node sends it, not the Agent"
        );
        let finished = until(&node, &spec.work_id, WorkState::finished).await?;
        assert_eq!(
            finished.state,
            WorkState::Finished {
                outcome: WorkOutcome::Completed
            }
        );
        assert_eq!(finished.text, "done: build it");
        let session = derived(SESSION_SEED, &spec.work_id);
        let unsure = WorkRecord {
            state: WorkState::Starting {
                thread: Some(session),
            },
            ..finished
        };
        assert_eq!(
            node.executor(Provider::Claude)?.probe(&unsure).await?,
            Probe::Terminal {
                turn_id: derived(MESSAGE_SEED, &spec.work_id),
                status: TurnStatus::Completed,
                text: "done: build it".into(),
                error: None,
            }
        );
        assert_eq!(place.user_frames(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn a_work_is_read_back_after_a_restart_and_never_sent_again() -> Result<()> {
        let place = Place::new()?;
        let (node, _, network) = node(&place).await?;
        let spec = spec(&node, &place, "r1", "vanish quietly")?;
        dispatch(&node, &network, &spec).await?;
        until(&node, &spec.work_id, |state| {
            matches!(state, WorkState::Running { .. })
        })
        .await?;
        let pidfile = place.dir.join("node").join(PIDFILE);
        assert!(pidfile.exists(), "a running work's process is recorded");
        drop(node);
        let node = Arc::new(Runtime::reopen(&place.dir.join("node")).await?);
        assert!(!pidfile.exists(), "the restart ended the process it left");
        node.probe_work(&network, &spec.work_id, true).await?;
        let finished = record(&node, &spec.work_id).await?;
        assert_eq!(
            finished.state,
            WorkState::Finished {
                outcome: WorkOutcome::Completed
            }
        );
        assert_eq!(finished.text, "finished while unwatched");
        assert_eq!(place.user_frames(), 1, "the prompt was sent once");
        Ok(())
    }

    #[tokio::test]
    async fn a_work_whose_prompt_never_reached_claude_code_is_not_started() -> Result<()> {
        let place = Place::new()?;
        let (node, _, network) = node(&place).await?;
        let spec = spec(&node, &place, "r1", "never sent")?;
        node.store
            .lock()
            .await
            .accept_work(&node.identity, &network, spec.clone(), now_ms())?;
        let session = derived(SESSION_SEED, &spec.work_id);
        let journaled = node
            .signal_work(&network, &spec.work_id, Signal::ThreadCreated(session))
            .await?;
        let read = node.executor(Provider::Claude)?.probe(&journaled).await?;
        assert_eq!(read, Probe::NotStarted);
        assert_eq!(place.user_frames(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn an_interrupted_work_ends_interrupted() -> Result<()> {
        let place = Place::new()?;
        let (node, _, network) = node(&place).await?;
        let spec = spec(&node, &place, "r1", "hang around")?;
        dispatch(&node, &network, &spec).await?;
        let running = until(&node, &spec.work_id, |state| {
            matches!(state, WorkState::Running { .. })
        })
        .await?;
        node.executor(Provider::Claude)?.interrupt(&running).await?;
        let finished = until(&node, &spec.work_id, WorkState::finished).await?;
        assert_eq!(
            finished.state,
            WorkState::Finished {
                outcome: WorkOutcome::Interrupted
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn two_works_started_at_once_in_one_session_run_one() -> Result<()> {
        let place = Place::new()?;
        let (node, _, network) = node(&place).await?;
        let first = spec(&node, &place, "r1", "build it")?;
        dispatch(&node, &network, &first).await?;
        until(&node, &first.work_id, WorkState::finished).await?;
        let session = derived(SESSION_SEED, &first.work_id);
        let mut specs = Vec::new();
        for call_ref in ["r2", "r3"] {
            let spec = WorkSpec {
                thread: Some(session.clone()),
                ..spec(&node, &place, call_ref, "hang around")?
            };
            node.store.lock().await.accept_work(
                &node.identity,
                &network,
                spec.clone(),
                now_ms(),
            )?;
            specs.push(spec);
        }
        let (claude, args) = (node.executor(Provider::Claude)?, json!({}));
        let (a, b) = tokio::join!(
            node.start_work(claude, &network, &specs[0], Some(&session), &args),
            node.start_work(claude, &network, &specs[1], Some(&session), &args),
        );
        let refused: Vec<_> = [a, b].into_iter().filter_map(Result::err).collect();
        assert_eq!(refused.len(), 1, "the start of the other one says why");
        assert!(
            format!("{:#}", refused[0]).contains("still runs another work in this session"),
            "{:#}",
            refused[0]
        );
        let mut rejected = 0;
        for spec in &specs {
            let state = until(&node, &spec.work_id, |state| {
                matches!(
                    state,
                    WorkState::Running { .. } | WorkState::Finished { .. }
                )
            })
            .await?
            .state;
            if state
                == (WorkState::Finished {
                    outcome: WorkOutcome::Rejected,
                })
            {
                rejected += 1;
            }
        }
        assert_eq!(rejected, 1, "one session runs one work at a time");
        Ok(())
    }

    /// A provider whose start fails before anything runs, as on a store error.
    struct Unstartable;

    impl Executor for Unstartable {
        fn start<'a>(
            &'a self,
            _: &'a Arc<Runtime>,
            _: &'a str,
            _: &'a WorkSpec,
            _: Option<&'a str>,
            _: &'a Value,
        ) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async { bail!("the store is busy") })
        }

        fn probe<'a>(&'a self, _: &'a WorkRecord) -> BoxFuture<'a, Result<Probe>> {
            unreachable!()
        }

        fn interrupt<'a>(&'a self, _: &'a WorkRecord) -> BoxFuture<'a, Result<()>> {
            unreachable!()
        }

        fn steer<'a>(
            &'a self,
            _: &'a SteerTarget,
            _: &'a str,
            _: &'a str,
        ) -> BoxFuture<'a, SteerAck> {
            unreachable!()
        }

        fn read_back<'a>(
            &'a self,
            _: &'a SteerTarget,
            _: &'a str,
        ) -> BoxFuture<'a, Option<SteerTrace>> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn a_work_whose_start_fails_is_rejected_for_a_new_dispatch() -> Result<()> {
        let place = Place::new()?;
        let (node, _, network) = node(&place).await?;
        let spec = spec(&node, &place, "r1", "never run")?;
        node.store
            .lock()
            .await
            .accept_work(&node.identity, &network, spec.clone(), now_ms())?;
        let error = node
            .start_work(&Unstartable, &network, &spec, None, &json!({}))
            .await
            .expect_err("the start failed");
        assert!(format!("{error:#}").contains("new call_ref"), "{error:#}");
        assert_eq!(
            record(&node, &spec.work_id).await?.state,
            WorkState::Finished {
                outcome: WorkOutcome::Rejected
            },
            "a retry under the same call_ref finds it ended instead of waiting forever"
        );
        Ok(())
    }

    fn asks(tool: &str, tool_input: Value) -> Value {
        json!({"hook_event_name":"PermissionRequest","session_id":"s-own","cwd":"/w",
            "transcript_path":"","tool_name":tool,"tool_input":tool_input,"permission_suggestions":[]})
    }

    #[tokio::test]
    async fn a_permission_request_waits_for_the_bot_and_gets_its_answer() -> Result<()> {
        let place = Place::new()?;
        let (node, _, _) = node(&place).await?;
        node.settings.lock().unwrap().manager = Some(crate::config::ManagerSettings {
            provider: Provider::Claude,
            workspace: place.dir.join("work"),
            model: String::new(),
            reasoning_effort: None,
            backup: Default::default(),
        });
        let input = asks(
            "Bash",
            json!({"command":"rm -rf build","description":"Clean the build"}),
        );
        let event = crate::triage::from_claude_hook(&input, 42).context("a hook event")?;
        assert_eq!(
            (event.event, event.hold),
            (
                crate::triage::Kind::NeedsInput,
                Some(RequestKind::Permission)
            )
        );
        assert!(event.detail.contains("Clean the build") && !event.detail.contains("rm -rf"));
        let held = node.ingest_agent_event(event).await?;
        let request_id = held["request_id"]
            .as_str()
            .context("a held request")?
            .to_owned();
        let waiting = tokio::spawn({
            let node = node.clone();
            let request_id = request_id.clone();
            async move { node.wait_request(&request_id).await }
        });
        tokio::task::yield_now().await;
        node.triage_command(json!({"action":"respond","request_id":request_id,
            "result":{"behavior":"allow"}}))
            .await?;
        let answer = waiting.await?;
        assert_eq!(
            hook_output(&input, &answer),
            Some(
                json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest",
                "decision":{"behavior":"allow"}}})
            )
        );
        assert_eq!(
            hook_output(&input, &json!({"behavior":"deny","message":"not now"})),
            Some(
                json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest",
                "decision":{"behavior":"deny","message":"not now","interrupt":false}}})
            )
        );
        // What `wait_request` gives at the deadline: nothing is printed and the session decides.
        assert_eq!(hook_output(&input, &json!({"decision":null})), None);
        Ok(())
    }

    #[test]
    fn an_allow_that_rewrites_the_input_is_not_passed_on() {
        let input = asks(
            "Bash",
            json!({"command":"rm -rf build","description":"Clean the build"}),
        );
        let rewritten = json!({"behavior":"allow","updatedInput":{"command":"rm -rf ~"}});
        assert_eq!(hook_output(&input, &rewritten), None);
    }

    #[test]
    fn a_question_is_answered_with_its_answers_filled_in() {
        let questions = json!([{"question":"Which database?","header":"DB","multiSelect":false,
            "options":[{"label":"Postgres","description":"server"},{"label":"SQLite","description":"file"}]}]);
        let input = asks("AskUserQuestion", json!({"questions":questions}));
        let event = crate::triage::from_claude_hook(&input, 7).expect("a hook event");
        assert_eq!(event.hold, Some(RequestKind::Question));
        assert!(
            event.detail.contains("Which database? [Postgres | SQLite]"),
            "{}",
            event.detail
        );
        assert_eq!(
            hook_output(&input, &json!({"answers":{"Which database?":"SQLite"}})),
            Some(
                json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{
                "behavior":"allow","updatedInput":{"questions":questions,
                "answers":{"Which database?":"SQLite"}}}}})
            )
        );
        assert_eq!(
            hook_output(&input, &json!({"answers":{"Which database?":2}})),
            None
        );
        assert_eq!(hook_output(&input, &json!({"decision":null})), None);
    }

    #[test]
    fn a_stop_hook_carries_the_last_reply() {
        let stop = json!({"hook_event_name":"Stop","session_id":"s-own","cwd":"/w",
            "last_assistant_message":"done"});
        let event = crate::triage::from_claude_hook(&stop, 3).expect("a hook event");
        assert_eq!(event.last_message, "done");
    }

    #[test]
    fn a_transcript_reads_back_how_the_turn_ended() -> Result<()> {
        let place = Place::new()?;
        let path = place.dir.join("t.jsonl");
        let write = |records: &[Value]| {
            let lines: String = records.iter().map(|record| format!("{record}\n")).collect();
            std::fs::write(&path, lines)
        };
        let user = |uuid: &str| json!({"type":"user","uuid":uuid,"message":{"role":"user","content":[{"type":"text","text":"go"}]}});
        let reply = |id: &str, block: Value, stop: &str| json!({"type":"assistant","message":{"id":id,"content":[block],"stop_reason":stop}});
        let text = |text: &str| json!({"type":"text","text":text});
        let ended = |status, text: &str| {
            Seen::Ended(Ending {
                status,
                text: text.into(),
                error: None,
            })
        };

        write(&[
            user("earlier"),
            user("m1"),
            json!({"type":"assistant","isSidechain":true,"message":{"id":"x","content":[text("sub")],"stop_reason":"end_turn"}}),
            reply("a", text("Looking."), "tool_use"),
            reply(
                "a",
                json!({"type":"tool_use","id":"t1","name":"Bash","input":{}}),
                "tool_use",
            ),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":""}]}}),
            reply("b", json!({"type":"thinking","thinking":""}), "end_turn"),
            reply("b", text("the answer"), "end_turn"),
            json!({"type":"system","subtype":"turn_duration"}),
            user("m2"),
            json!({"type":"user","interruptedMessageId":"c","message":{"role":"user","content":"[Request interrupted by user]"}}),
        ])?;
        assert_eq!(
            seen(Some(&path), "m1"),
            ended(TurnStatus::Completed, "the answer")
        );
        assert_eq!(seen(Some(&path), "m2"), ended(TurnStatus::Interrupted, ""));
        assert_eq!(seen(Some(&path), "m3"), Seen::Unsent);
        assert_eq!(seen(None, "m1"), Seen::Unsent);

        write(&[user("m1"), reply("a", text("Looking."), "tool_use")])?;
        assert_eq!(seen(Some(&path), "m1"), Seen::Open);

        write(&[
            user("m1"),
            json!({"type":"assistant","isApiErrorMessage":true,"message":{"id":"e",
                "content":[text("API Error: 401 Invalid API key")],"stop_reason":"stop_sequence"}}),
        ])?;
        let Seen::Ended(Ending {
            status: TurnStatus::Failed,
            error: Some(failure),
            ..
        }) = seen(Some(&path), "m1")
        else {
            bail!("an API error fails the turn");
        };
        assert_eq!(failure.kind, FailureKind::AuthFailed);
        Ok(())
    }
}
