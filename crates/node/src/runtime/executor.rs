//! The executor host owns the Codex App Server process and the only client to it.
//! A managed App Server runs in its own process group recorded in a pidfile, so a crash of
//! this Node never leaves an unmanaged one behind: the next start kills the recorded group.
use super::*;
use crate::work::Probe;
use std::os::unix::process::CommandExt as _;
use vibeface_bridge::codex::{CodexClient, CodexError};

/// One executor kind. `start` is split so the thread ID is journaled before `turn/start`.
pub(crate) trait Executor {
    fn capability(&self) -> impl Future<Output = Value> + Send;
    fn start_thread(&self, cwd: &str) -> impl Future<Output = Result<String, CodexError>> + Send;
    fn start_turn(
        &self,
        thread: &str,
        cwd: &str,
        prompt: &str,
    ) -> impl Future<Output = Result<String, CodexError>> + Send;
    fn interrupt(
        &self,
        thread: &str,
        turn: &str,
    ) -> impl Future<Output = Result<(), CodexError>> + Send;
    fn probe(
        &self,
        thread: &str,
        turn: Option<&str>,
        epoch: u64,
    ) -> impl Future<Output = Result<Probe, CodexError>> + Send;
    fn answer(
        &self,
        rpc_id: Value,
        response: Value,
    ) -> impl Future<Output = Result<(), CodexError>> + Send;
}

pub(crate) struct CodexExecutor {
    client: CodexClient,
}

fn confined(cwd: &str) -> Value {
    json!({"cwd":cwd,"approvalPolicy":"on-request","approvalsReviewer":"user"})
}

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

impl CodexExecutor {
    /// Threads the client re-subscribes after every reconnect.
    pub(crate) fn observe(&self, threads: Vec<String>) -> Result<(), CodexError> {
        self.client.set_observed_threads(threads)
    }
    pub(crate) async fn snapshot(&self) -> Value {
        self.client.snapshot().await
    }
}

impl Executor for CodexExecutor {
    async fn capability(&self) -> Value {
        json!({"executor":"codex","connected":self.client.snapshot().await["connection"]["state"] == "connected"})
    }
    async fn start_thread(&self, cwd: &str) -> Result<String, CodexError> {
        let mut params = confined(cwd);
        params["sandbox"] = json!("workspace-write");
        let reply = self.client.request("thread/start", params).await?;
        reply["thread"]["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| CodexError {
                uncertain: true,
                message: "thread/start returned no thread ID".into(),
            })
    }
    async fn start_turn(
        &self,
        thread: &str,
        cwd: &str,
        prompt: &str,
    ) -> Result<String, CodexError> {
        let mut params = confined(cwd);
        params["threadId"] = json!(thread);
        params["input"] = json!([{"type":"text","text":prompt}]);
        params["sandboxPolicy"] =
            json!({"type":"workspaceWrite","writableRoots":[cwd],"networkAccess":false});
        let reply = self.client.request("turn/start", params).await?;
        reply["turn"]["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| CodexError {
                uncertain: true,
                message: "turn/start returned no turn ID".into(),
            })
    }
    async fn interrupt(&self, thread: &str, turn: &str) -> Result<(), CodexError> {
        self.client
            .request("turn/interrupt", json!({"threadId":thread,"turnId":turn}))
            .await
            .map(|_| ())
    }
    /// Reads the journaled thread; it never starts or replays work.
    async fn probe(
        &self,
        thread: &str,
        turn: Option<&str>,
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
            None if turns.is_empty() => return Ok(Probe::NotStarted),
            None => turns.last(),
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
    async fn answer(&self, rpc_id: Value, response: Value) -> Result<(), CodexError> {
        self.client.respond(rpc_id, response).await
    }
}

/// Checks a user's answer against what the request offers.
pub(crate) fn validate_response(
    method: &str,
    params: &Value,
    response: &Value,
) -> Result<ApprovalDecision> {
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            let decision = response["decision"]
                .as_str()
                .context("approval decision is required")?;
            ensure!(
                params["availableDecisions"]
                    .as_array()
                    .is_none_or(|allowed| allowed.iter().any(|a| a.as_str() == Some(decision))),
                "the request does not offer that decision"
            );
            Ok(match decision {
                "accept" => ApprovalDecision::Accept,
                "decline" => ApprovalDecision::Decline,
                "cancel" => ApprovalDecision::Cancel,
                _ => bail!("unsupported approval decision"),
            })
        }
        "item/tool/requestUserInput" => {
            let answers = response["answers"]
                .as_object()
                .context("answers are required")?;
            let questions = params["questions"]
                .as_array()
                .context("input questions are missing")?;
            ensure!(
                answers.len() == questions.len(),
                "answer every question exactly once"
            );
            for question in questions {
                let id = question["id"].as_str().context("question ID is missing")?;
                let values = answers
                    .get(id)
                    .and_then(|answer| answer["answers"].as_array())
                    .context("answer is missing")?;
                ensure!(
                    !values.is_empty() && values.iter().all(Value::is_string),
                    "answers must be text"
                );
            }
            Ok(ApprovalDecision::Answered)
        }
        _ => bail!("this request must be answered on the executing device"),
    }
}

/// The text shown for a request; it never includes model credentials.
pub(crate) fn request_text(method: &str, params: &Value) -> String {
    let mut lines = vec![match method {
        "item/commandExecution/requestApproval" => "执行器请求运行命令。".to_owned(),
        "item/fileChange/requestApproval" => "执行器请求修改文件。".to_owned(),
        "item/tool/requestUserInput" => "执行器需要你补充信息。".to_owned(),
        _ => return "执行器发来暂不支持的请求，请到执行设备处理。".into(),
    }];
    if let Some(reason) = params["reason"].as_str().filter(|r| !r.trim().is_empty()) {
        lines.push(reason.into());
    }
    if let Some(command) = params["command"].as_str() {
        lines.push(format!("命令：{command}"));
    } else if let Some(command) = params["command"].as_array() {
        let parts: Vec<_> = command.iter().filter_map(Value::as_str).collect();
        lines.push(format!("命令：{}", parts.join(" ")));
    }
    if let Some(cwd) = params["cwd"].as_str() {
        lines.push(format!("目录：{cwd}"));
    }
    if let Some(root) = params["grantRoot"].as_str() {
        lines.push(format!("允许修改的目录：{root}"));
    }
    if let Some(changes) = params["changes"].as_object() {
        lines.push(format!(
            "文件：{}",
            changes.keys().cloned().collect::<Vec<_>>().join("、")
        ));
    }
    lines.join("\n")
}

/// Answer choices and questions for the UI.
pub(crate) fn request_form(method: &str, params: &Value) -> Value {
    let mut form = json!({});
    if method.ends_with("requestApproval") {
        let offered = params["availableDecisions"].as_array();
        form["choices"] = json!(
            [
                ("批准一次", "accept"),
                ("拒绝", "decline"),
                ("取消本轮", "cancel")
            ]
            .into_iter()
            .filter(|(_, d)| offered.is_none_or(|a| a.iter().any(|v| v.as_str() == Some(*d))))
            .map(|(label, d)| json!({"label":label,"response":{"decision":d}}))
            .collect::<Vec<_>>()
        );
    }
    // Structured copies so the web can lay out the command, place and files.
    if let Some(reason) = params["reason"].as_str().filter(|r| !r.trim().is_empty()) {
        form["reason"] = json!(reason);
    }
    let command = params["command"].as_str().map(str::to_owned).or_else(|| {
        params["command"].as_array().map(|c| {
            c.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
    });
    if let Some(command) = command {
        form["command"] = json!(command);
    }
    if let Some(cwd) = params["cwd"].as_str().or(params["grantRoot"].as_str()) {
        form["cwd"] = json!(cwd);
    }
    if let Some(changes) = params["changes"].as_object() {
        form["files"] = json!(changes.keys().collect::<Vec<_>>());
    }
    if let Some(questions) = params["questions"].as_array() {
        form["questions"] = json!(
            questions
                .iter()
                .filter(|q| q["id"].is_string() && q["question"].is_string())
                .map(|q| json!({"id":q["id"],"header":q["header"],"question":q["question"],
                    "is_secret":q["isSecret"].as_bool().unwrap_or(false),
                    "options":q["options"].as_array().into_iter().flatten().filter(|o| o["label"].is_string())
                        .map(|o| json!({"label":o["label"],"description":o["description"].as_str()})).collect::<Vec<_>>()}))
                .collect::<Vec<_>>()
        );
    }
    form
}

pub(crate) struct ExecutorHost {
    pub executor: CodexExecutor,
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

impl ExecutorHost {
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
            executor: CodexExecutor {
                client: CodexClient::start(socket, events),
            },
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn approval_forms_carry_command_place_and_files() {
        let form = super::request_form(
            "item/commandExecution/requestApproval",
            &json!({"command":["npm","test"],"cwd":"/p","reason":"run tests","availableDecisions":["accept","decline"]}),
        );
        assert_eq!(
            (
                form["command"].as_str(),
                form["cwd"].as_str(),
                form["reason"].as_str()
            ),
            (Some("npm test"), Some("/p"), Some("run tests"))
        );
        assert_eq!(form["choices"].as_array().map(Vec::len), Some(2));
        let form = super::request_form(
            "item/fileChange/requestApproval",
            &json!({"grantRoot":"/p","changes":{"a.ts":{}}}),
        );
        assert_eq!(
            (form["cwd"].as_str(), &form["files"]),
            (Some("/p"), &json!(["a.ts"]))
        );
    }
}
