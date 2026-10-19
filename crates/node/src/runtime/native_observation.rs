use super::*;
use crate::scheduling::{JobOutcome, JobState, TriggerKind};
use serde::{Deserialize, Serialize};

const INDEX: &str = "native-observation:v1:index";
const MAX_NATIVE_WORK: usize = 256;

/// App Server methods an Agent may send and the parameters it may set.
/// The Node owns approval, reviewer and sandbox fields; Agents cannot answer server requests.
pub(crate) const AGENT_NATIVE_METHODS: &[(&str, &[&str])] = &[
    (
        "thread/start",
        &[
            "cwd",
            "model",
            "modelProvider",
            "personality",
            "serviceTier",
            "ephemeral",
        ],
    ),
    (
        "thread/resume",
        &[
            "threadId",
            "cwd",
            "model",
            "modelProvider",
            "personality",
            "serviceTier",
            "excludeTurns",
        ],
    ),
    (
        "turn/start",
        &[
            "threadId",
            "input",
            "cwd",
            "model",
            "effort",
            "summary",
            "personality",
            "outputSchema",
            "serviceTier",
        ],
    ),
    ("turn/interrupt", &["threadId", "turnId"]),
    ("thread/read", &["threadId", "includeTurns"]),
    (
        "thread/turns/list",
        &["threadId", "cursor", "limit", "sortDirection", "itemsView"],
    ),
    ("thread/loaded/list", &["cursor", "limit"]),
];

/// Rejects methods and fields outside the Agent allowlist, and pins every work-starting
/// request to human approval inside an authorized root.
pub(super) fn confine_agent_request(
    method: &str,
    params: &Value,
    allowed_path: impl Fn(&str) -> Result<PathBuf>,
) -> Result<Value> {
    let (_, fields) = AGENT_NATIVE_METHODS
        .iter()
        .find(|(name, _)| *name == method)
        .with_context(|| format!("native method {method} is not available to Agents"))?;
    let mut params = params
        .as_object()
        .context("native params must be a JSON object")?
        .clone();
    if let Some(field) = params.keys().find(|key| !fields.contains(&key.as_str())) {
        bail!(
            "native field {field} is not available to Agents; the Node sets approval and sandbox"
        );
    }
    if fields.contains(&"cwd") {
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .context("native cwd is required and must be inside an allowed root")?;
        let cwd = allowed_path(cwd)?
            .to_str()
            .context("native cwd must be UTF-8")?
            .to_owned();
        params.insert("approvalPolicy".into(), json!("on-request"));
        params.insert("approvalsReviewer".into(), json!("user"));
        if method == "turn/start" {
            params.insert(
                "sandboxPolicy".into(),
                json!({"type":"workspaceWrite","writableRoots":[cwd],"networkAccess":false}),
            );
        } else {
            params.insert("sandbox".into(), json!("workspace-write"));
        }
        params.insert("cwd".into(), json!(cwd));
    }
    Ok(Value::Object(params))
}

pub(super) struct NativeRuntime {
    socket: PathBuf,
    client: vibeface_bridge::codex::CodexClient,
    _process: Option<tokio::process::Child>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeState {
    Running,
    Waiting,
    Interrupting,
    Completed,
    Failed,
    Interrupted,
    Unknown,
}

impl NativeState {
    pub(super) fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Interrupted)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct NativeWork {
    pub work_id: String,
    pub conversation_id: String,
    pub task_ref: Option<String>,
    pub origin_trigger: String,
    pub attempt_id: String,
    pub target_node: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub state: NativeState,
    pub detail: String,
    #[serde(default)]
    pub sequence: u64,
}

fn key(work_id: &str) -> String {
    format!("native-observation:v1:work:{work_id}")
}

pub(super) fn sessions_from(store: &Store, network: &str) -> Result<Vec<NativeWork>> {
    let ids: Vec<String> =
        serde_json::from_value(store.local_value(network, INDEX)?.unwrap_or(json!([])))?;
    ids.into_iter()
        .map(|id| {
            serde_json::from_value(
                store
                    .local_value(network, &key(&id))?
                    .context("native observation is missing")?,
            )
            .map_err(Into::into)
        })
        .collect()
}

fn save_work(store: &mut Store, network: &str, work: &NativeWork) -> Result<()> {
    store.local_set(network, &key(&work.work_id), &serde_json::to_value(work)?)
}

fn event_identity(event: &Value) -> (Option<&str>, Option<&str>) {
    let params = &event["params"];
    (
        params["threadId"]
            .as_str()
            .or_else(|| params["thread"]["id"].as_str()),
        params["turnId"]
            .as_str()
            .or_else(|| params["turn"]["id"].as_str()),
    )
}

fn native_input_request(event: &Value) -> bool {
    serde_json::from_value::<vibeface_protocol::RpcId>(event["id"].clone()).is_ok()
        && event["method"].as_str().is_some_and(|method| {
            method.ends_with("requestApproval") || method == "item/tool/requestUserInput"
        })
}

fn pending_native_request<'a>(snapshot: &'a Value, work: &NativeWork) -> Option<&'a Value> {
    snapshot["pending_requests"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|request| {
            native_input_request(request)
                && event_identity(request)
                    == (Some(work.thread_id.as_str()), work.turn_id.as_deref())
        })
}

fn terminal_state(status: &str) -> Option<NativeState> {
    match status {
        "completed" => Some(NativeState::Completed),
        "failed" => Some(NativeState::Failed),
        "interrupted" => Some(NativeState::Interrupted),
        _ => None,
    }
}

fn apply_event(work: &mut NativeWork, event: &Value) -> bool {
    let (thread, turn) = event_identity(event);
    if work.state.terminal()
        || thread != Some(work.thread_id.as_str())
        || turn.is_none()
        || turn != work.turn_id.as_deref()
    {
        return false;
    }
    let params = &event["params"];
    match event["method"].as_str() {
        Some("turn/completed") => {
            let Some(state) = params["turn"]["status"].as_str().and_then(terminal_state) else {
                return false;
            };
            work.state = state;
            if let Some(error) = params["turn"]["error"]["message"].as_str() {
                work.detail = error.chars().take(65536).collect();
            }
        }
        Some("item/agentMessage/delta") => {
            if work.state != NativeState::Interrupting {
                work.state = NativeState::Running;
            }
            if let Some(text) = params["delta"].as_str() {
                work.detail = format!("{}{text}", work.detail)
                    .chars()
                    .take(65536)
                    .collect();
            }
        }
        Some("item/completed") if params["item"]["type"] == "agentMessage" => {
            if work.state != NativeState::Interrupting {
                work.state = NativeState::Running;
            }
            if let Some(text) = params["item"]["text"].as_str() {
                work.detail = text.chars().take(65536).collect();
            }
        }
        Some(method)
            if method.ends_with("requestApproval") || method == "item/tool/requestUserInput" =>
        {
            work.state = NativeState::Waiting;
        }
        Some("turn/started") => work.state = NativeState::Running,
        _ => {}
    }
    true
}

fn loaded_thread(thread: &Value) -> bool {
    matches!(
        thread["status"]["type"].as_str(),
        Some("idle" | "active" | "systemError")
    )
}

async fn read_original_turn(
    client: &vibeface_bridge::codex::CodexClient,
    work: &NativeWork,
) -> Result<Value> {
    let turn_id = work
        .turn_id
        .as_deref()
        .context("original native turn is unknown")?;
    if let Ok(reply) = client
        .request(
            "thread/read",
            json!({"threadId":work.thread_id,"includeTurns":true}),
        )
        .await
    {
        ensure!(
            reply["thread"]["id"] == work.thread_id && loaded_thread(&reply["thread"]),
            "the original native thread is not loaded on this Socket"
        );
        if let Some(turn) = reply["thread"]["turns"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|turn| turn["id"] == turn_id)
        {
            return Ok(turn.clone());
        }
    }
    let mut cursor: Option<String> = None;
    let mut cursors = BTreeSet::new();
    loop {
        let reply = client
            .request(
                "thread/turns/list",
                json!({"threadId":work.thread_id,"cursor":cursor,
            "limit":100,"itemsView":"full","sortDirection":"desc"}),
            )
            .await?;
        let turns = reply["data"]
            .as_array()
            .context("native turn history is unavailable")?;
        ensure!(
            turns.len() <= 100,
            "native turn history exceeded its requested page"
        );
        if let Some(turn) = turns.iter().find(|turn| turn["id"] == turn_id) {
            return Ok(turn.clone());
        }
        let next = reply["nextCursor"]
            .as_str()
            .context("the original turn was not found in native history")?;
        ensure!(
            !next.is_empty() && cursors.insert(next.to_owned()),
            "native turn history cursor did not advance"
        );
        cursor = Some(next.to_owned());
    }
}

fn reconcile_work(work: &mut NativeWork, turn: Result<Value>, assistant: Option<&str>) -> bool {
    if work.state.terminal() {
        return false;
    }
    let turn = match turn {
        Ok(turn) if work.turn_id.is_some() && turn["id"].as_str() == work.turn_id.as_deref() => {
            turn
        }
        _ => {
            let detail = "The original native turn cannot be verified on this Socket. Its outcome is unknown; reconcile it on the owning device before retrying.";
            let changed = work.state != NativeState::Unknown || work.detail != detail;
            work.state = NativeState::Unknown;
            work.detail = detail.into();
            return changed;
        }
    };
    let state = match turn["status"].as_str() {
        Some("inProgress") => match work.state {
            NativeState::Waiting | NativeState::Interrupting => work.state,
            _ => NativeState::Running,
        },
        Some(status) => match terminal_state(status) {
            Some(state) => state,
            None => return false,
        },
        None => return false,
    };
    let detail = turn["error"]["message"]
        .as_str()
        .or_else(|| {
            turn["items"]
                .as_array()
                .into_iter()
                .flatten()
                .rev()
                .find(|item| item["type"] == "agentMessage")
                .and_then(|item| item["text"].as_str())
        })
        .or(assistant)
        .or_else(|| (work.state == NativeState::Unknown).then_some(""));
    let changed = work.state != state || detail.is_some_and(|detail| work.detail != detail);
    work.state = state;
    if let Some(detail) = detail {
        work.detail = detail.chars().take(65536).collect();
    }
    changed
}

pub(crate) fn outcome(works: &[NativeWork]) -> Option<JobOutcome> {
    if works.is_empty() {
        return Some(JobOutcome::Completed);
    }
    if works.iter().any(|work| !work.state.terminal()) {
        return None;
    }
    if let Some(work) = works.iter().find(|work| work.state == NativeState::Failed) {
        return Some(JobOutcome::Failed(work.detail.clone()));
    }
    if works
        .iter()
        .any(|work| work.state == NativeState::Interrupted)
    {
        return Some(JobOutcome::Cancelled(
            "the native Socket confirmed the original turn was interrupted".into(),
        ));
    }
    Some(JobOutcome::Completed)
}

impl Runtime {
    pub(super) async fn ensure_native_client(&self) -> Result<()> {
        self.native
            .get_or_try_init(|| async {
                ensure!(
                    self.config.roles.contains(&NodeRole::Scheduler),
                    "native Codex requires the Scheduler role"
                );
                let managed = self.config.codex_socket.is_none();
                let socket = if let Some(socket) = &self.config.codex_socket {
                    socket.clone()
                } else {
                    let directory = PathBuf::from("/tmp")
                        .join(format!("vibeface-{}", &self.identity.peer_id()?[..32]));
                    match std::fs::create_dir(&directory) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error.into()),
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::{MetadataExt, PermissionsExt};
                        let metadata = std::fs::symlink_metadata(&directory)?;
                        ensure!(
                            metadata.is_dir()
                                && !metadata.file_type().is_symlink()
                                && metadata.uid() == std::fs::metadata(&self.data_dir)?.uid(),
                            "private socket directory has unexpected ownership"
                        );
                        std::fs::set_permissions(
                            &directory,
                            std::fs::Permissions::from_mode(0o700),
                        )?;
                    }
                    directory.join("codex.sock")
                };
                let mut process = None;
                if managed && tokio::net::UnixStream::connect(&socket).await.is_err() {
                    if socket.exists() {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::FileTypeExt;
                            ensure!(
                                std::fs::symlink_metadata(&socket)?.file_type().is_socket(),
                                "private Codex socket path is occupied by another file"
                            );
                        }
                        std::fs::remove_file(&socket)?;
                    }
                    process = Some(
                        tokio::process::Command::new(&self.config.codex_bin)
                            .args(["app-server", "--listen"])
                            .arg(format!("unix://{}", socket.display()))
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .kill_on_drop(true)
                            .spawn()?,
                    );
                }
                Ok::<_, anyhow::Error>(NativeRuntime {
                    client: vibeface_bridge::codex::CodexClient::start(
                        socket.clone(),
                        self.native_events.clone(),
                    ),
                    socket,
                    _process: process,
                })
            })
            .await?;
        Ok(())
    }

    pub(crate) fn native_socket(&self) -> Option<&Path> {
        self.native.get().map(|native| native.socket.as_path())
    }

    pub(super) fn native_client(&self) -> Result<&vibeface_bridge::codex::CodexClient> {
        self.native
            .get()
            .map(|native| &native.client)
            .context("this Node has no native Codex Socket configured")
    }

    pub(super) async fn observe_session(
        &self,
        access: &Access,
        args: &Value,
        call_id: &str,
    ) -> Result<Value> {
        vibeface_protocol::validate_id(call_id).map_err(anyhow::Error::msg)?;
        let manager = access
            .manager
            .as_ref()
            .context("native observation requires a live Agent invocation")?;
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing conversation")?;
        let thread_id = id(args, "thread_id")?;
        let turn_id = id(args, "turn_id")?;
        let work = {
            let mut store = self.store.lock().await;
            Self::check_manager_permission(&store, access, true)?;
            let job = store
                .scheduler_job(
                    &access.network_id,
                    scope,
                    &manager.trigger,
                    &manager.target_node,
                )?
                .context("Agent invocation is missing")?;
            let task_ref = (job.kind == TriggerKind::AgentTask).then(|| job.trigger.clone());
            let works = sessions_from(&store, &access.network_id)?;
            if let Some(previous) = works.iter().find(|work| {
                work.thread_id == thread_id && work.turn_id.as_deref() == Some(&turn_id)
            }) {
                ensure!(
                    previous.conversation_id == scope
                        && previous.origin_trigger == manager.trigger
                        && previous.attempt_id == manager.attempt_id,
                    "this native turn already belongs to another task or invocation"
                );
                if previous.state.terminal() {
                    return Ok(json!(previous));
                }
                previous.clone()
            } else {
                ensure!(
                    works.iter().filter(|work| !work.state.terminal()).count() < MAX_NATIVE_WORK,
                    "native observation capacity is full"
                );
                let work = NativeWork {
                    work_id: crypto::digest(&serde_json::to_vec(&(
                        scope,
                        &manager.attempt_id,
                        &thread_id,
                        &turn_id,
                    ))?),
                    conversation_id: scope.into(),
                    task_ref,
                    origin_trigger: manager.trigger.clone(),
                    attempt_id: manager.attempt_id.clone(),
                    target_node: self.identity.peer_id()?,
                    thread_id,
                    turn_id: Some(turn_id),
                    state: NativeState::Unknown,
                    detail: "The original native turn has been recorded but is not yet verified on this Socket. Its outcome is unknown.".into(),
                    sequence: 0,
                };
                save_work(&mut store, &access.network_id, &work)?;
                let mut ids = works
                    .into_iter()
                    .map(|work| work.work_id)
                    .collect::<Vec<_>>();
                ids.push(work.work_id.clone());
                store.local_set(&access.network_id, INDEX, &json!(ids))?;
                work
            }
        };
        self.publish_changed(&access.network_id, scope, &work.origin_trigger);
        let observation = tokio::time::timeout(Duration::from_secs(10), async {
            let client = self.native_client()?;
            client.set_observed_threads(
                self.native_observed_work()
                    .await?
                    .into_iter()
                    .map(|(_, work)| work.thread_id)
                    .collect(),
            )?;
            let lookup = async {
                let mut snapshot = client.snapshot().await;
                let thread = &snapshot["threads"][&work.thread_id];
                let observed_here = snapshot["connection"]["state"] == "connected"
                    && loaded_thread(thread)
                    && thread["last_turn"]["id"].as_str() == work.turn_id.as_deref();
                if !observed_here {
                    let read = client
                        .request(
                            "thread/read",
                            json!({"threadId":work.thread_id,"includeTurns":false}),
                        )
                        .await?;
                    ensure!(
                        read["thread"]["id"] == work.thread_id && loaded_thread(&read["thread"]),
                        "the original native thread is not loaded on this Socket"
                    );
                    snapshot = client.snapshot().await;
                }
                let cached = &snapshot["threads"][&work.thread_id]["last_turn"];
                let turn = if cached["id"].as_str() == work.turn_id.as_deref() {
                    cached.clone()
                } else {
                    read_original_turn(client, &work).await?
                };
                Ok::<_, anyhow::Error>((Some(turn), snapshot))
            }
            .await;
            if lookup.is_ok() {
                return lookup;
            }
            loop {
                let snapshot = client.snapshot().await;
                let thread = &snapshot["threads"][&work.thread_id];
                if snapshot["connection"]["state"] == "connected" {
                    if loaded_thread(thread)
                        && thread["last_turn"]["id"].as_str() == work.turn_id.as_deref()
                    {
                        return Ok((Some(thread["last_turn"].clone()), snapshot));
                    }
                    if pending_native_request(&snapshot, &work).is_some() {
                        return Ok((None, snapshot));
                    }
                }
                let current: NativeWork = serde_json::from_value(
                    self.store
                        .lock()
                        .await
                        .local_value(&access.network_id, &key(&work.work_id))?
                        .context("native work is missing")?,
                )?;
                if current.state.terminal()
                    || (current.sequence > work.sequence && current.state != NativeState::Unknown)
                {
                    return Ok((None, snapshot));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("native observation lookup timed out"))
        .and_then(|result| result);
        let (turn, snapshot) = match observation {
            Ok((turn, snapshot)) => (turn.map(Ok), Some(snapshot)),
            Err(error) => (Some(Err(error)), None),
        };
        {
            let mut store = self.store.lock().await;
            let mut current: NativeWork = serde_json::from_value(
                store
                    .local_value(&access.network_id, &key(&work.work_id))?
                    .context("native work is missing")?,
            )?;
            let assistant = snapshot.as_ref().and_then(|snapshot| {
                let thread = &snapshot["threads"][&work.thread_id];
                (thread["last_turn"]["id"].as_str() == work.turn_id.as_deref())
                    .then(|| thread["last_assistant"].as_str())
                    .flatten()
            });
            if let Some(turn) = turn
                && (turn.is_ok() || current.sequence == work.sequence)
                && reconcile_work(&mut current, turn, assistant)
            {
                save_work(&mut store, &access.network_id, &current)?;
            }
        }
        if let Some(snapshot) = snapshot {
            for request in snapshot["pending_requests"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if event_identity(request)
                    == (Some(work.thread_id.as_str()), work.turn_id.as_deref())
                {
                    self.observe_native(request.clone()).await?;
                }
            }
        }
        self.publish_changed(&access.network_id, scope, &work.origin_trigger);
        self.store
            .lock()
            .await
            .local_value(&access.network_id, &key(&work.work_id))?
            .context("native observation was not saved")
    }

    pub(super) fn start_native_observation(self: &Arc<Self>) {
        let node = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tick.tick().await;
                let _work = node.maintenance.enter_when_open().await;
                if node.native.get().is_some()
                    && let Err(error) = node.refresh_native_subscriptions().await
                {
                    tracing::debug!(%error, "native observation recovery is pending");
                }
                if let Err(error) = crate::scheduler::settle_interrupted_jobs(node.clone()).await {
                    tracing::debug!(%error, "native result finalization is pending");
                }
            }
        });
    }

    async fn native_observed_work(&self) -> Result<Vec<(String, NativeWork)>> {
        let networks = self.store.lock().await.networks()?;
        let mut all = Vec::new();
        for network in networks {
            let store = self.store.lock().await;
            if store
                .authorize_node(
                    &network,
                    &self.identity.peer_id()?,
                    Some(NodeRole::Scheduler),
                )
                .is_ok()
            {
                all.extend(
                    sessions_from(&store, &network)?
                        .into_iter()
                        .filter(|work| !work.state.terminal())
                        .map(|work| (network.clone(), work)),
                );
            }
        }
        Ok(all)
    }

    async fn refresh_native_subscriptions(&self) -> Result<()> {
        let client = self.native_client()?;
        let all = self.native_observed_work().await?;
        client
            .set_observed_threads(all.iter().map(|(_, work)| work.thread_id.clone()).collect())?;
        let snapshot = client.snapshot().await;
        if snapshot["connection"]["state"] != "connected" {
            return Ok(());
        }
        for (network, work) in all {
            if let Some(request) = pending_native_request(&snapshot, &work) {
                if !matches!(work.state, NativeState::Waiting | NativeState::Interrupting) {
                    self.observe_native(request.clone()).await?;
                }
                continue;
            }
            let thread = &snapshot["threads"][&work.thread_id];
            let (turn, assistant) = if !loaded_thread(thread) {
                (
                    Err(anyhow::anyhow!(
                        "the original native thread is not loaded on this Socket"
                    )),
                    None,
                )
            } else if work.turn_id.is_some()
                && thread["last_turn"]["id"].as_str() == work.turn_id.as_deref()
            {
                (
                    Ok(thread["last_turn"].clone()),
                    thread["last_assistant"].as_str(),
                )
            } else {
                let turn = tokio::time::timeout(
                    Duration::from_secs(10),
                    read_original_turn(client, &work),
                )
                .await
                .map_err(|_| anyhow::anyhow!("original native turn history lookup timed out"))
                .and_then(|result| result);
                (turn, None)
            };
            let mut store = self.store.lock().await;
            let mut current: NativeWork = serde_json::from_value(
                store
                    .local_value(&network, &key(&work.work_id))?
                    .context("native work is missing")?,
            )?;
            let changed = (turn.is_ok() || current.sequence == work.sequence)
                && reconcile_work(&mut current, turn, assistant);
            if changed {
                save_work(&mut store, &network, &current)?;
            }
            drop(store);
            if changed {
                self.publish_changed(&network, &current.conversation_id, &current.origin_trigger);
            }
        }
        Ok(())
    }

    pub(super) async fn observe_native(&self, event: Value) -> Result<()> {
        let _work = self.maintenance.enter_when_open().await;
        let networks = self.store.lock().await.networks()?;
        for network in networks {
            let mut store = self.store.lock().await;
            if store
                .authorize_node(
                    &network,
                    &self.identity.peer_id()?,
                    Some(NodeRole::Scheduler),
                )
                .is_err()
            {
                continue;
            }
            let works = sessions_from(&store, &network)?;
            for mut work in works {
                let mut changed_hash = work.origin_trigger.clone();
                if event["method"] == "bridge.codex_disconnected" && !work.state.terminal() {
                    work.state = NativeState::Unknown;
                    save_work(&mut store, &network, &work)?;
                } else if apply_event(&mut work, &event) {
                    work.sequence = work
                        .sequence
                        .checked_add(1)
                        .context("native progress sequence overflow")?;
                    save_work(&mut store, &network, &work)?;
                    let sequence = work.sequence;
                    if native_input_request(&event)
                        && let Some(notification) = store.raise_notification_once(
                            &self.identity,
                            &network,
                            Notification {
                                notification_id: format!(
                                    "native-input-{}",
                                    crypto::digest(&serde_json::to_vec(&(
                                        &work.work_id,
                                        &event["id"]
                                    ))?)
                                ),
                                conversation_id: work.conversation_id.clone(),
                                kind: "needs_input".into(),
                                text: "后台工作需要你的确认。".into(),
                                event_hash: String::new(),
                            },
                            now_ms(),
                        )?
                    {
                        changed_hash = notification.hash;
                    }
                    self.publish_progress(&network, json!({"type":"native_progress","conversation_id":work.conversation_id,
                        "work_id":work.work_id,"target_node":work.target_node,"source_node":work.target_node,
                        "thread_id":work.thread_id,"turn_id":work.turn_id,"task_ref":work.task_ref,
                        "sequence":sequence,"progress":observation::native_progress(&event)}));
                } else {
                    continue;
                }
                self.publish_changed(&network, &work.conversation_id, &changed_hash);
            }
        }
        Ok(())
    }

    pub(crate) async fn wait_native_work(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
        attempt: &str,
    ) -> Result<Vec<NativeWork>> {
        loop {
            let works = self
                .native_work_for_attempt(network, scope, trigger, attempt)
                .await?;
            let target = self.identity.peer_id()?;
            let job = self
                .store
                .lock()
                .await
                .scheduler_job(network, scope, trigger, &target)?
                .context("task invocation is missing")?;
            let children = if job.kind == TriggerKind::AgentTask {
                self.child_agent_tasks(network, scope, trigger).await?
            } else {
                Vec::new()
            };
            if outcome(&works).is_some()
                && children.iter().all(|task| {
                    matches!(
                        task["state"].as_str(),
                        Some("completed" | "failed" | "interrupted" | "cancelled")
                    )
                })
            {
                return Ok(works);
            }
            // Stopping the reply is the caller's race; native turns are observed independently of it.
            if works.iter().any(|work| work.state == NativeState::Unknown)
                || children.iter().any(|task| task["state"] == "pending_check")
                || matches!(job.state, JobState::Interrupted { .. })
            {
                return Err(crate::manager::ManagerError {
                    kind: crate::manager::ManagerFailure::Unknown,
                    session_id: None,
                    message: "started work awaits reconciliation; nothing was replayed".into(),
                }
                .into());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub(crate) async fn child_agent_tasks(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
    ) -> Result<Vec<Value>> {
        let local = self.identity.peer_id()?;
        let store = self.store.lock().await;
        let context = store.context(network, scope, &[])?;
        context
            .events
            .iter()
            .filter_map(|event| {
                if event.scope_id != scope || event.author != local {
                    return None;
                }
                let EventBody::AgentMessage { message } = &event.body else {
                    return None;
                };
                (message.kind == AgentMessageKind::Task && message.origin_trigger == trigger).then(
                    || {
                        store.agent_task_status(
                            network,
                            scope,
                            &crypto::digest(&serde_json::to_vec(event)?),
                        )
                    },
                )
            })
            .collect()
    }

    pub(crate) async fn native_work_for_attempt(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
        attempt: &str,
    ) -> Result<Vec<NativeWork>> {
        Ok(sessions_from(&*self.store.lock().await, network)?
            .into_iter()
            .filter(|work| {
                work.conversation_id == scope
                    && work.origin_trigger == trigger
                    && work.attempt_id == attempt
            })
            .collect())
    }

    pub(super) async fn native_status(&self, network: &str, scope: &str) -> Result<Value> {
        let snapshot = match self.native.get() {
            Some(native) => native.client.snapshot().await,
            None => Value::Null,
        };
        let store = self.store.lock().await;
        store.authorize_node(network, &self.identity.peer_id()?, None)?;
        let sessions = sessions_from(&store, network)?
            .into_iter()
            .filter(|work| work.conversation_id == scope)
            .map(|work| {
                let mut value = json!(work);
                let thread = &snapshot["threads"][&work.thread_id];
                value["freshness"] = json!("observed");
                value["can_control"] = json!(
                    snapshot["connection"]["state"] == "connected"
                        && loaded_thread(thread)
                        && work.turn_id.is_some()
                        && thread["last_turn"]["id"].as_str() == work.turn_id.as_deref()
                        && matches!(work.state, NativeState::Running | NativeState::Waiting)
                );
                value["pending_requests"] = json!(super::chat_controls::pending_requests(
                    &snapshot,
                    Some(&work.thread_id),
                    work.turn_id.as_deref()
                ));
                value
            })
            .collect::<Vec<_>>();
        Ok(json!({"native_sessions":sessions}))
    }

    pub(super) async fn native_control(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        ensure!(access.human, "native controls require the user");
        let scope = field(input, "conversation_id")?;
        let target = field(input, "target_node")?;
        self.require_scope(access, scope).await?;
        let method = field(input, "method")?;
        if target != self.identity.peer_id()? {
            self.require_role(&access.network_id, NodeRole::Gateway)
                .await?;
            let mut request = input.clone();
            request["type"] = json!(method);
            request["human"] = json!(true);
            return self.peer_request(&access.network_id, target, request).await;
        }
        self.native_control_local(&access.network_id, input, method)
            .await
    }

    pub(super) async fn native_control_local(
        &self,
        network: &str,
        input: &Value,
        method: &str,
    ) -> Result<Value> {
        let scope = field(input, "conversation_id")?;
        let work_id = field(input, "work_id")?;
        ensure!(
            field(input, "target_node")? == self.identity.peer_id()?,
            "native control target mismatch"
        );
        let work: NativeWork = {
            let store = self.store.lock().await;
            store.authorize_node(
                network,
                &self.identity.peer_id()?,
                Some(NodeRole::Scheduler),
            )?;
            serde_json::from_value(
                store
                    .local_value(network, &key(work_id))?
                    .context("native work not found")?,
            )?
        };
        ensure!(
            work.conversation_id == scope && work.target_node == self.identity.peer_id()?,
            "native control scope mismatch"
        );
        ensure!(!work.state.terminal(), "native turn is already terminal");
        if method == "native_interrupt" {
            return self.interrupt_native_work(network, work).await;
        }
        ensure!(method == "native_respond", "unknown native control");
        let request_id = input
            .get("request_id")
            .context("native request ID is required")?;
        serde_json::from_value::<vibeface_protocol::RpcId>(request_id.clone())?;
        let snapshot = self.native_client()?.snapshot().await;
        let request = snapshot["pending_requests"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|request| {
                &request["id"] == request_id
                    && event_identity(request)
                        == (Some(work.thread_id.as_str()), work.turn_id.as_deref())
            })
            .context("the native request no longer belongs to the original turn")?;
        validate_response(request, &input["response"])?;
        self.native_client()?
            .respond(request_id.clone(), input["response"].clone())
            .await?;
        Ok(json!({"work_id":work_id,"responded":true}))
    }

    async fn interrupt_native_work(&self, network: &str, mut work: NativeWork) -> Result<Value> {
        let turn = work
            .turn_id
            .as_deref()
            .context("native turn is unknown; verify it on the owning device")?;
        let client = self.native_client()?;
        let snapshot = client.snapshot().await;
        ensure!(
            snapshot["connection"]["state"] == "connected"
                && loaded_thread(&snapshot["threads"][&work.thread_id]),
            "native turn cannot be controlled on this Socket; its outcome is unknown"
        );
        ensure!(
            snapshot["threads"][&work.thread_id]["last_turn"]["id"].as_str() == Some(turn),
            "native thread now points at a different turn; refresh its original work before interrupting"
        );
        client
            .request(
                "turn/interrupt",
                json!({"threadId":work.thread_id,"turnId":turn}),
            )
            .await?;
        work.state = NativeState::Interrupting;
        let mut store = self.store.lock().await;
        let current: NativeWork = serde_json::from_value(
            store
                .local_value(network, &key(&work.work_id))?
                .context("native work missing")?,
        )?;
        if !current.state.terminal() {
            save_work(&mut store, network, &work)?;
        }
        drop(store);
        self.publish_changed(network, &work.conversation_id, &work.origin_trigger);
        Ok(json!({"work_id":work.work_id,"interrupt_requested":true,"confirmed":false}))
    }

    fn local_native_work(
        store: &Store,
        network: &str,
        scope: &str,
        work_id: &str,
    ) -> Result<NativeWork> {
        let work: NativeWork = serde_json::from_value(
            store
                .local_value(network, &key(work_id))?
                .context("native work not found")?,
        )?;
        ensure!(
            work.conversation_id == scope,
            "native work belongs to another conversation"
        );
        Ok(work)
    }

    /// Reads the original turn again; it never starts or replays native work.
    pub(super) async fn reconcile_native_work(
        &self,
        network: &str,
        scope: &str,
        work_id: &str,
    ) -> Result<NativeWork> {
        let work = Self::local_native_work(&*self.store.lock().await, network, scope, work_id)?;
        if work.state.terminal() {
            return Ok(work);
        }
        let client = self.native_client()?;
        let turn = tokio::time::timeout(Duration::from_secs(10), read_original_turn(client, &work))
            .await
            .map_err(|_| anyhow::anyhow!("original native turn history lookup timed out"))
            .and_then(|result| result);
        let mut store = self.store.lock().await;
        let mut current = Self::local_native_work(&store, network, scope, work_id)?;
        if (turn.is_ok() || current.sequence == work.sequence)
            && reconcile_work(&mut current, turn, None)
        {
            save_work(&mut store, network, &current)?;
        }
        Ok(current)
    }

    /// Records the user's evidence for a native turn this Node can no longer observe.
    pub(super) async fn resolve_native_work(
        &self,
        network: &str,
        scope: &str,
        work_id: &str,
        state: NativeState,
        evidence: &str,
    ) -> Result<NativeWork> {
        ensure!(
            state.terminal(),
            "a confirmed native outcome must be terminal"
        );
        let mut store = self.store.lock().await;
        let mut work = Self::local_native_work(&store, network, scope, work_id)?;
        ensure!(
            work.state == NativeState::Unknown,
            "only an unknown native turn needs confirmation"
        );
        work.state = state;
        work.detail = format!("用户核对确认：{evidence}")
            .chars()
            .take(65536)
            .collect();
        work.sequence = work
            .sequence
            .checked_add(1)
            .context("native progress sequence overflow")?;
        save_work(&mut store, network, &work)?;
        Ok(work)
    }

    pub(super) async fn interrupt_native_task(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
    ) -> Result<()> {
        let works = sessions_from(&*self.store.lock().await, network)?
            .into_iter()
            .filter(|work| {
                work.conversation_id == scope
                    && work.origin_trigger == trigger
                    && !work.state.terminal()
            })
            .collect::<Vec<_>>();
        for work in works {
            if let Err(error) = self.interrupt_native_work(network, work).await {
                tracing::debug!(%error, "native task interruption is unconfirmed");
            }
        }
        Ok(())
    }
}

fn validate_response(request: &Value, response: &Value) -> Result<()> {
    match request["method"].as_str() {
        Some("item/commandExecution/requestApproval" | "item/fileChange/requestApproval") => {
            let decision = response["decision"]
                .as_str()
                .context("approval decision is required")?;
            ensure!(
                matches!(decision, "accept" | "decline" | "cancel"),
                "unsupported approval decision"
            );
            ensure!(
                request["params"]["availableDecisions"]
                    .as_array()
                    .is_none_or(|allowed| allowed
                        .iter()
                        .any(|allowed| allowed.as_str() == Some(decision))),
                "the native request does not offer that decision"
            );
        }
        Some("item/tool/requestUserInput") => {
            let answers = response["answers"]
                .as_object()
                .context("answers are required")?;
            let questions = request["params"]["questions"]
                .as_array()
                .context("native input questions are missing")?;
            ensure!(
                answers.len() == questions.len(),
                "answer every native question exactly once"
            );
            for question in questions {
                let id = question["id"]
                    .as_str()
                    .context("native question ID is missing")?;
                let values = answers
                    .get(id)
                    .and_then(|answer| answer["answers"].as_array())
                    .context("native answer is missing")?;
                ensure!(
                    !values.is_empty() && values.iter().all(Value::is_string),
                    "native answers must be text"
                );
            }
        }
        _ => bail!("this native request must be answered on the owning device"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work() -> NativeWork {
        NativeWork {
            work_id: "work".into(),
            conversation_id: "chat".into(),
            task_ref: Some("task".into()),
            origin_trigger: "task".into(),
            attempt_id: "attempt".into(),
            target_node: "node".into(),
            thread_id: "thread".into(),
            turn_id: Some("turn".into()),
            state: NativeState::Running,
            detail: String::new(),
            sequence: 0,
        }
    }

    async fn manager_access(node: &Runtime, network: &str, message_id: &str) -> Result<Access> {
        let local = node.identity.peer_id()?;
        let mut store = node.store.lock().await;
        let (event, _) = store.accept_message(
            &node.identity,
            network,
            "chat",
            EventBody::MessagePosted {
                message_id: message_id.into(),
                role: "user".into(),
                text: "observe the original native turn".into(),
                scheduler_node: Some(local.clone()),
            },
            vec![],
            2,
            &local,
            &local,
            false,
        )?;
        let job = store
            .claim_next(network, "chat", &local, "native-observation-test", "sdk")?
            .context("native observation fixture was not claimed")?
            .job;
        let attempt = job
            .state
            .attempt()
            .context("native observation fixture has no attempt")?;
        store.mark_started(network, "chat", &local, &event.hash, &attempt.id)?;
        Ok(Access {
            network_id: network.into(),
            human: false,
            conversation_id: Some("chat".into()),
            manager: Some(ManagerAccess {
                attempt_id: attempt.id.clone(),
                trigger: event.hash,
                target_node: local,
                read_only: false,
                followup_version: None,
            }),
        })
    }

    #[test]
    fn terminal_events_require_the_original_thread_and_turn_and_are_idempotent() {
        let mut work = work();
        for (thread, turn) in [("other", "turn"), ("thread", "other")] {
            assert!(!apply_event(
                &mut work,
                &json!({"method":"turn/completed","params":{"threadId":thread,"turn":{"id":turn,"status":"completed"}}})
            ));
            assert_eq!(work.state, NativeState::Running);
        }
        assert!(!apply_event(
            &mut work,
            &json!({"method":"thread/status/changed","params":{"threadId":"thread","status":{"type":"idle"}}})
        ));
        let completed = json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"turn","status":"completed"}}});
        assert!(apply_event(&mut work, &completed));
        assert!(!apply_event(&mut work, &completed));
        assert!(matches!(outcome(&[work]), Some(JobOutcome::Completed)));
    }

    #[test]
    fn agent_native_requests_are_allowlisted_and_pinned_to_human_approval_in_roots() {
        let roots = |path: &str| {
            ensure!(
                path.starts_with("/root/project"),
                "outside configured roots"
            );
            Ok(PathBuf::from(path))
        };
        for (method, params) in [
            ("config/batchWrite", json!({})),
            ("command/exec", json!({"command":["true"]})),
            (
                "item/commandExecution/requestApproval",
                json!({"decision":"accept"}),
            ),
            (
                "thread/start",
                json!({"cwd":"/root/project","approvalPolicy":"never"}),
            ),
            (
                "thread/start",
                json!({"cwd":"/root/project","sandbox":"danger-full-access"}),
            ),
            (
                "thread/start",
                json!({"cwd":"/root/project","config":{"approval_policy":"never"}}),
            ),
            (
                "thread/resume",
                json!({"threadId":"t","cwd":"/root/project","approvalsReviewer":"auto_review"}),
            ),
            (
                "turn/start",
                json!({"threadId":"t","cwd":"/root/project","input":[],"sandboxPolicy":{"type":"dangerFullAccess"}}),
            ),
            ("thread/start", json!({"cwd":"/etc"})),
            ("turn/start", json!({"threadId":"t","input":[]})),
        ] {
            assert!(
                confine_agent_request(method, &params, roots).is_err(),
                "{method} {params}"
            );
        }
        assert_eq!(
            confine_agent_request(
                "thread/start",
                &json!({"cwd":"/root/project","model":"m"}),
                roots
            )
            .unwrap(),
            json!({"cwd":"/root/project","model":"m","approvalPolicy":"on-request",
                "approvalsReviewer":"user","sandbox":"workspace-write"})
        );
        assert_eq!(
            confine_agent_request(
                "turn/start",
                &json!({"threadId":"t","cwd":"/root/project","input":[]}),
                roots
            )
            .unwrap(),
            json!({"threadId":"t","cwd":"/root/project","input":[],"approvalPolicy":"on-request",
                "approvalsReviewer":"user","sandboxPolicy":{"type":"workspaceWrite",
                "writableRoots":["/root/project"],"networkAccess":false}})
        );
        assert_eq!(
            confine_agent_request(
                "turn/interrupt",
                &json!({"threadId":"t","turnId":"u"}),
                roots
            )
            .unwrap(),
            json!({"threadId":"t","turnId":"u"})
        );
    }

    #[tokio::test]
    async fn agent_codex_tool_rejects_approval_bypass_before_reaching_the_socket() -> Result<()> {
        let dir = crate::runtime::tests::fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = crate::runtime::tests::conversation(&node).await?;
        let access = manager_access(&node, &network, "native-bypass").await?;
        let manager = access.manager.clone().context("manager access missing")?;
        node.register_manager_attempt(
            "native-bypass-token".into(),
            access.clone(),
            &manager.attempt_id,
            &manager.trigger,
            false,
        )
        .await?;
        for args in [
            json!({"method":"thread/start","params":{"cwd":"/tmp","approvalPolicy":"never"}}),
            json!({"method":"item/commandExecution/requestApproval","params":{"decision":"accept"}}),
        ] {
            let error = node
                .agent_tool(access.clone(), "codex", args, "bypass-call")
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("not available to Agents"), "{error}");
        }
        assert!(node.native.get().is_none());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn native_approval_notification_is_signed_deduplicated_and_syncs_without_request_data()
    -> Result<()> {
        let dir = crate::runtime::tests::fixture()?;
        let node = Runtime::open(&dir).await?;
        let network = crate::runtime::tests::conversation(&node).await?;
        let mut observed = work();
        observed.target_node = node.identity.peer_id()?;
        {
            let mut store = node.store.lock().await;
            save_work(&mut store, &network, &observed)?;
            store.local_set(&network, INDEX, &json!([observed.work_id]))?;
        }
        let mut updates = node.events.subscribe();
        let approval = json!({"id":42,"method":"item/commandExecution/requestApproval",
            "params":{"threadId":"thread","turnId":"turn",
            "command":"private-command","secret":"private-key","availableDecisions":["decline"]}});
        let mut wrong_turn = approval.clone();
        wrong_turn["params"]["turnId"] = json!("another-turn");
        node.observe_native(wrong_turn).await?;
        assert!(
            node.store
                .lock()
                .await
                .pending_notifications(&network, "phone")?
                .is_empty()
        );
        node.observe_native(approval.clone()).await?;
        node.observe_native(approval).await?;
        let (notification, bundle, snapshot) = {
            let store = node.store.lock().await;
            let notices = store.pending_notifications(&network, "phone")?;
            assert_eq!(notices.len(), 1);
            let notice = notices[0].clone();
            assert_eq!(notice.kind, "needs_input");
            assert_eq!(notice.text, "后台工作需要你的确认。");
            let event = store
                .event(&network, &notice.event_hash)?
                .context("native approval notification is not a signed event")?;
            let body: Event = serde_json::from_str(&event.canonical_body)?;
            assert!(matches!(body.body, EventBody::NotificationRaised { .. }));
            assert!(!serde_json::to_string(&event)?.contains("private-"));
            assert_eq!(
                sessions_from(&store, &network)?[0].state,
                NativeState::Waiting
            );
            (
                notice,
                store.trust_bundle(&network)?.unwrap(),
                store.export_snapshot(&network, "chat")?,
            )
        };
        assert!(
            std::iter::from_fn(|| updates.try_recv().ok()).any(|update| {
                update.payload["type"] == "changed"
                    && update.payload["event_hash"] == notification.event_hash
            })
        );
        let replica_path = dir.join("replica.sqlite3");
        let mut replica = Store::open(&replica_path)?;
        replica.install_trust_bundle(&network, &bundle)?;
        replica.import_snapshot(&network, &snapshot)?;
        drop(replica);
        let replica = Store::open(&replica_path)?;
        assert_eq!(
            replica.pending_notifications(&network, "phone")?,
            vec![notification]
        );
        assert!(
            replica
                .scheduler_jobs(&network, "chat", &node.identity.peer_id()?)?
                .is_empty()
        );
        assert!(replica.scheduler_outbox_all(&network)?.is_empty());
        drop(replica);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn refresh_checks_exact_turn_history_and_reports_unsupported_history_as_unknown()
    -> Result<()> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        for history_available in [true, false] {
            let dir = crate::runtime::tests::fixture()?;
            let socket_dir =
                PathBuf::from("/tmp").join(format!("vf-native-recovery-{}", uuid::Uuid::new_v4()));
            config::private_dir(&socket_dir)?;
            let socket_path = socket_dir.join("native.sock");
            let listener = tokio::net::UnixListener::bind(&socket_path)?;
            let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
            let captured = requests.clone();
            let delayed_lookup = Arc::new(tokio::sync::Notify::new());
            let delayed_approval = Arc::new(tokio::sync::Notify::new());
            let lookup_rejected = delayed_lookup.clone();
            let release_approval = delayed_approval.clone();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                let mut socket = tokio_tungstenite::accept_async(stream).await?;
                let mut delayed_sent = false;
                while let Some(message) = socket.next().await {
                    let message = message?;
                    if !message.is_text() {
                        continue;
                    }
                    let request: Value = serde_json::from_str(message.to_text()?)?;
                    captured.lock().await.push(request.clone());
                    if request["method"] == "initialized" {
                        socket
                            .send(Message::Text(
                                json!({"method":"turn/completed","params":{"threadId":"thread",
                            "turn":{"id":"later-turn","status":"completed"}}})
                                .to_string()
                                .into(),
                            ))
                            .await?;
                        continue;
                    }
                    let result = match request["method"].as_str() {
                        Some("initialize") => Some(json!({"userAgent":"native-recovery-test"})),
                        Some("thread/turns/list") if history_available => {
                            if request["params"]["cursor"] == "older" {
                                Some(
                                    json!({"data":[{"id":"turn","status":"completed","items":[{"type":"agentMessage","text":"original result"}]}],"nextCursor":null}),
                                )
                            } else {
                                Some(
                                    json!({"data":[{"id":"later-turn","status":"completed","items":[{"type":"agentMessage","text":"later result"}]}],"nextCursor":"older"}),
                                )
                            }
                        }
                        _ => None,
                    };
                    let reply = match result {
                        Some(result) => json!({"id":request["id"],"result":result}),
                        None => {
                            json!({"id":request["id"],"error":{"code":-32601,"message":"list_turns is not supported yet"}})
                        }
                    };
                    socket.send(Message::Text(reply.to_string().into())).await?;
                    if !delayed_sent
                        && request["method"] == "thread/read"
                        && request["params"]["threadId"] == "delayed-thread"
                    {
                        delayed_sent = true;
                        lookup_rejected.notify_one();
                        release_approval.notified().await;
                        socket
                            .send(Message::Text(
                                json!({"id":7,"method":"item/commandExecution/requestApproval",
                                    "params":{"threadId":"delayed-thread","turnId":"delayed-turn",
                                    "availableDecisions":["decline"]}})
                                .to_string()
                                .into(),
                            ))
                            .await?;
                    }
                }
                Ok::<_, anyhow::Error>(())
            });
            let node = Runtime::open(&dir).await?;
            let (events, _receiver) = mpsc::unbounded_channel();
            assert!(
                node.native
                    .set(NativeRuntime {
                        client: vibeface_bridge::codex::CodexClient::start(
                            socket_path.clone(),
                            events
                        ),
                        socket: socket_path,
                        _process: None,
                    })
                    .is_ok()
            );
            let node = Arc::new(node);
            let network = crate::runtime::tests::conversation(&node).await?;
            let observed = work();
            {
                let mut store = node.store.lock().await;
                save_work(&mut store, &network, &observed)?;
                store.local_set(&network, INDEX, &json!([observed.work_id]))?;
            }
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let snapshot = node.native_client()?.snapshot().await;
                    if snapshot["connection"]["state"] == "connected"
                        && snapshot["threads"]["thread"]["last_turn"]["id"] == "later-turn"
                    {
                        return Ok::<_, anyhow::Error>(());
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await??;
            node.refresh_native_subscriptions().await?;
            let restored = sessions_from(&*node.store.lock().await, &network)?;
            assert_eq!(
                restored[0].state,
                if history_available {
                    NativeState::Completed
                } else {
                    NativeState::Unknown
                }
            );
            assert!(!restored[0].detail.contains("later result"));
            if history_available {
                assert_eq!(restored[0].detail, "original result");
                assert!(
                    requests
                        .lock()
                        .await
                        .iter()
                        .any(|request| request["method"] == "thread/turns/list"
                            && request["params"]["cursor"] == "older")
                );
            } else {
                assert!(
                    requests
                        .lock()
                        .await
                        .iter()
                        .any(|request| request["method"] == "thread/turns/list")
                );
            }
            let access = manager_access(&node, &network, "observed-native-turn").await?;
            let args = json!({"thread_id":"thread","turn_id":"later-turn"});
            assert!(node.observe_session(&access, &args, "").await.is_err());
            let mut stale = access.clone();
            stale.manager.as_mut().unwrap().attempt_id = "stale-attempt".into();
            assert!(
                node.observe_session(&stale, &args, "stale-call")
                    .await
                    .is_err()
            );
            assert_eq!(sessions_from(&*node.store.lock().await, &network)?.len(), 1);
            let attached = node
                .observe_session(&access, &args, "observed-call")
                .await?;
            assert_eq!(attached["state"], "completed");
            assert_eq!(attached["turn_id"], "later-turn");
            let delayed_node = node.clone();
            let delayed_access = access.clone();
            let mut delayed = tokio::spawn(async move {
                delayed_node
                    .observe_session(
                        &delayed_access,
                        &json!({"thread_id":"delayed-thread","turn_id":"delayed-turn"}),
                        "delayed-call",
                    )
                    .await
            });
            tokio::time::timeout(Duration::from_secs(3), delayed_lookup.notified()).await?;
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut delayed)
                    .await
                    .is_err()
            );
            assert_eq!(
                sessions_from(&*node.store.lock().await, &network)?
                    .iter()
                    .find(|work| work.turn_id.as_deref() == Some("delayed-turn"))
                    .unwrap()
                    .state,
                NativeState::Unknown
            );
            delayed_approval.notify_one();
            let resumed = tokio::time::timeout(Duration::from_secs(3), delayed).await???;
            assert_eq!(resumed["state"], "waiting");
            node.refresh_native_subscriptions().await?;
            let manager = access.manager.as_ref().unwrap();
            assert!(matches!(
                node.store
                    .lock()
                    .await
                    .scheduler_job(&network, "chat", &manager.trigger, &manager.target_node)?
                    .unwrap()
                    .state,
                JobState::Running { .. }
            ));
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(50),
                    node.wait_native_work(&network, "chat", &manager.trigger, &manager.attempt_id),
                )
                .await
                .is_err()
            );
            node.observe_native(json!({"method":"turn/completed","params":{
                "threadId":"delayed-thread","turn":{"id":"delayed-turn","status":"completed"}
            }}))
            .await?;
            assert!(matches!(
                outcome(
                    &node
                        .wait_native_work(&network, "chat", &manager.trigger, &manager.attempt_id)
                        .await?
                ),
                Some(JobOutcome::Completed)
            ));
            let unverified = node
                .observe_session(
                    &access,
                    &json!({"thread_id":"thread","turn_id":"unverified-turn"}),
                    "unverified-call",
                )
                .await?;
            assert_eq!(unverified["state"], "unknown");
            assert_eq!(unverified["turn_id"], "unverified-turn");
            let error = node
                .wait_native_work(&network, "chat", &manager.trigger, &manager.attempt_id)
                .await
                .unwrap_err();
            assert!(
                error
                    .downcast_ref::<crate::manager::ManagerError>()
                    .is_some_and(|error| error.uncertain())
            );
            assert!(requests.lock().await.iter().all(|request| {
                !matches!(
                    request["method"].as_str(),
                    Some("thread/start" | "thread/resume" | "turn/start" | "turn/interrupt")
                )
            }));
            server.abort();
            let _ = server.await;
            drop(node);
            std::fs::remove_dir_all(socket_dir)?;
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    #[test]
    fn recovery_uses_the_original_turn_and_preserves_its_persisted_terminal() {
        let mut observed = work();
        assert!(reconcile_work(
            &mut observed,
            Ok(json!({"id":"later-turn","status":"completed"})),
            Some("later output")
        ));
        assert_eq!(observed.state, NativeState::Unknown);
        assert!(!observed.detail.contains("later output"));
        assert!(outcome(std::slice::from_ref(&observed)).is_none());
        assert!(reconcile_work(
            &mut observed,
            Ok(json!({"id":"turn","status":"inProgress"})),
            None
        ));
        assert_eq!(observed.state, NativeState::Running);
        assert!(observed.detail.is_empty());
        assert!(reconcile_work(
            &mut observed,
            Err(anyhow::anyhow!("history temporarily unavailable")),
            None
        ));
        assert_eq!(observed.state, NativeState::Unknown);
        assert!(reconcile_work(
            &mut observed,
            Ok(json!({"id":"turn","status":"completed",
            "items":[{"type":"agentMessage","text":"original result"}]})),
            None
        ));
        assert_eq!(observed.state, NativeState::Completed);
        assert_eq!(observed.detail, "original result");
        assert!(!reconcile_work(
            &mut observed,
            Err(anyhow::anyhow!("history unsupported")),
            None
        ));
        assert_eq!(observed.state, NativeState::Completed);
        assert_eq!(observed.detail, "original result");
    }

    #[tokio::test]
    async fn unavailable_native_history_exits_the_running_task_wait_as_unknown() -> Result<()> {
        let dir = crate::runtime::tests::fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = crate::runtime::tests::conversation(&node).await?;
        let local = node.identity.peer_id()?;
        let (trigger, attempt) = {
            let mut store = node.store.lock().await;
            let (event, _) = store.accept_message(
                &node.identity,
                &network,
                "chat",
                EventBody::MessagePosted {
                    message_id: "native-recovery".into(),
                    role: "user".into(),
                    text: "wait for the original native turn".into(),
                    scheduler_node: Some(local.clone()),
                },
                vec![],
                2,
                &local,
                &local,
                false,
            )?;
            let job = store
                .claim_next(&network, "chat", &local, "native-recovery-test", "sdk")?
                .context("native recovery fixture was not claimed")?
                .job;
            let attempt = job
                .state
                .attempt()
                .context("native recovery fixture has no attempt")?
                .id
                .clone();
            store.mark_started(&network, "chat", &local, &event.hash, &attempt)?;
            (event.hash, attempt)
        };
        let mut observed = work();
        observed.target_node = local.clone();
        observed.origin_trigger = trigger.clone();
        observed.attempt_id = attempt.clone();
        assert!(reconcile_work(
            &mut observed,
            Err(anyhow::anyhow!("list_turns is not supported yet")),
            None
        ));
        assert_eq!(observed.state, NativeState::Unknown);
        {
            let mut store = node.store.lock().await;
            save_work(&mut store, &network, &observed)?;
            store.local_set(&network, INDEX, &json!([observed.work_id]))?;
        }
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            node.wait_native_work(&network, "chat", &trigger, &attempt),
        )
        .await
        .context("unknown native history must not leave the task waiting")?
        .unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::manager::ManagerError>()
                .is_some_and(|error| error.uncertain())
        );
        let store = node.store.lock().await;
        assert!(matches!(
            store
                .scheduler_job(&network, "chat", &trigger, &local)?
                .unwrap()
                .state,
            JobState::Running { .. }
        ));
        assert_eq!(
            sessions_from(&store, &network)?[0].state,
            NativeState::Unknown
        );
        drop(store);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn native_progress_sequence_survives_store_reopen() -> Result<()> {
        let dir = crate::runtime::tests::fixture()?;
        let identity = LocalIdentity::generate();
        let network = identity.fixture_network_id()?;
        let mut store = Store::open(dir.join("node.sqlite3"))?;
        store.install_trust_bundle(
            &network,
            &identity.sign_network_fixture(
                1,
                vec![identity.certificate(vec![NodeRole::Replica, NodeRole::Scheduler])?],
            )?,
        )?;
        let mut observed = work();
        observed.sequence = 41;
        save_work(&mut store, &network, &observed)?;
        store.local_set(&network, INDEX, &json!([observed.work_id]))?;
        drop(store);
        let store = Store::open(dir.join("node.sqlite3"))?;
        let restored = sessions_from(&store, &network)?;
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].sequence, 41);
        assert_eq!(restored[0].thread_id, "thread");
        assert_eq!(restored[0].turn_id.as_deref(), Some("turn"));
        drop(store);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn cancellation_and_approval_require_real_native_evidence() -> Result<()> {
        let mut interrupted = work();
        interrupted.state = NativeState::Interrupting;
        assert!(outcome(&[interrupted.clone()]).is_none());
        interrupted.state = NativeState::Interrupted;
        assert!(matches!(
            outcome(&[interrupted]),
            Some(JobOutcome::Cancelled(_))
        ));
        let approval = json!({"method":"item/commandExecution/requestApproval","params":{"availableDecisions":["decline"]}});
        assert!(validate_response(&approval, &json!({"decision":"accept"})).is_err());
        validate_response(&approval, &json!({"decision":"decline"}))?;
        assert!(!loaded_thread(
            &json!({"status":{"type":"notLoaded"},"turns":[{"id":"turn","status":"completed"}]})
        ));
        Ok(())
    }
}
