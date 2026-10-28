//! Claude Code on this device. A bot turn is one
//! `claude -p --input-format stream-json --output-format stream-json` process in its own group,
//! whose session the Node saves before anything is sent.
use super::*;
use crate::manager::{self, CodexTurn, ManagerConfig, ManagerError, ManagerFailure};
use std::sync::PoisonError;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use vibeface_bridge::{
    agent::{self, AgentResult},
    failure::{ExecFailure, FailureKind},
    turn::TurnStatus,
};

/// Process groups of the running Claude Code processes, with the program each one runs.
pub(super) const PIDFILE: &str = "claude.pid";
/// How long an interrupted turn has to end before its process group is ended.
const STOP_WAIT: Duration = Duration::from_secs(6);
/// Seed of the ID a turn's user message gets from its attempt.
const MESSAGE_SEED: &str = "claude-user:";

/// The process groups of running `claude` processes. The pidfile lists them, so a Node that
/// could not end them ends them when it starts again.
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

    fn track(&self, group: u32, bin: &Path) -> Result<()> {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        running.insert(group, bin.to_owned());
        self.save(&running)
            .context("record the Claude Code process group")
    }

    fn forget(&self, group: u32) {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if running.remove(&group).is_some() {
            let _ = self.save(&running);
        }
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
                if tail.len() > 4000 {
                    tail = tail.chars().skip(tail.chars().count() - 2000).collect();
                }
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
    if let Err(e) = persist_session(session.clone()).await {
        // A stop fences the attempt, so saving its session fails then.
        return Err(if *cancel.borrow() {
            stopped(native)
        } else {
            let failure = own(&format!("persist the Claude Code session: {e}"));
            error(ManagerFailure::Failed, native, failure)
        });
    }
    let mut env = config.provider_env.clone();
    env.extend(config.tool_env.clone());
    if let Some(token) = &config.tool_token {
        env.insert("VIBEFACE_TOOL_TOKEN".into(), token.clone());
    }
    // Claude Code keeps no instructions with a session, so every turn brings them.
    let instructions = manager::codex_instructions(
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
        instructions: &instructions,
        env: &env,
    };
    let mut process = launch
        .spawn()
        .map_err(|failure| error(ManagerFailure::Failed, native, failure))?;
    let sent = async {
        processes.track(process.group, bin)?;
        if let Some(started) = &config.process_started {
            started(process.group).await?;
        }
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
        if let Some(ending) = turn.ending.take() {
            break Ok(ending);
        }
    };
    process.end(processes).await;
    let ending = outcome?;
    match ending.status {
        TurnStatus::Completed => {}
        TurnStatus::Interrupted => {
            let failure = own("Claude Code turn was interrupted");
            return Err(error(ManagerFailure::Cancelled, native, failure));
        }
        _ => {
            let failure = ending
                .error
                .unwrap_or_else(|| own("Claude Code turn failed"));
            return Err(error(ManagerFailure::Failed, native, failure));
        }
    }
    // Only now has the session seen the turn's prompt. Recording it earlier would leave a
    // crashed reply checked against the Codex App Server.
    if let Some(turn) = &config.codex_turn
        && let Err(e) = (turn.record)(session.clone(), Some(message)).await
    {
        tracing::warn!(error = %e, "the next Claude Code turn repeats what this one was shown");
    }
    Ok(AgentResult {
        session_id: Some(session),
        output: manager::redact_string(ending.text, &secrets),
        usage: None,
    })
}

/// Claude Code's side of this device: the process groups of bot turns.
pub(crate) struct Claude {
    processes: Arc<Processes>,
}

impl Claude {
    /// Ends the processes an earlier Node run left.
    pub(super) fn open(dir: &Path) -> Self {
        let pidfile = dir.join(PIDFILE);
        vibeface_bridge::agent::SdkBridge::kill_orphan(&pidfile);
        Self {
            processes: Arc::new(Processes::new(pidfile)),
        }
    }

    pub(super) fn processes(&self) -> Arc<Processes> {
        self.processes.clone()
    }

    /// Ends every running Claude Code process; the Node calls this before it exits.
    pub(super) async fn stop(&self) {
        let pidfile = self.processes.pidfile.clone();
        let _ = tokio::task::spawn_blocking(move || {
            vibeface_bridge::agent::SdkBridge::kill_orphan(&pidfile)
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude")
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
    async fn a_bot_turn_streams_its_reply_and_keeps_the_session() -> Result<()> {
        let place = Place::new()?;
        let (record, calls) = recorder();
        let config = bot_config(&place, record);
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
            [(session.clone(), Some(message.clone()))],
            "the turn is recorded once it ended"
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
        assert!(
            calls.lock().unwrap().is_empty(),
            "an interrupted turn is not recorded"
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
}
