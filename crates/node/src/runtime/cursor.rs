use super::*;
use crate::manager::CursorInstance;
use crate::runtime::executor::{Executor, SteerAck, SteerTrace};
use crate::work::{Probe, Signal, WorkRecord, WorkSpec, WorkState};
use futures_util::future::BoxFuture;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Mutex as StdMutex, PoisonError, Weak, atomic::Ordering};
use tokio::sync::{Mutex, watch};
use vibeface_bridge::{
    agent::{
        AgentFailure, AgentRunError, NativeRun, NativeStatus, RunStarted, SdkBridge, SdkTurn,
        StartEvidence, ToolCallbackConfig,
    },
    failure::{ExecFailure, FailureKind},
};

fn native_probe(native: NativeRun) -> Probe {
    match native.status {
        NativeStatus::Running => Probe::Running {
            turn_id: native.run_id,
            epoch: 0,
        },
        NativeStatus::Completed => Probe::Terminal {
            turn_id: native.run_id,
            status: vibeface_bridge::turn::TurnStatus::Completed,
            text: native.output.unwrap_or_default(),
            error: None,
        },
        NativeStatus::Cancelled => Probe::Terminal {
            turn_id: native.run_id,
            status: vibeface_bridge::turn::TurnStatus::Interrupted,
            text: native.output.unwrap_or_default(),
            error: None,
        },
        NativeStatus::Failed { detail } => Probe::Terminal {
            turn_id: native.run_id,
            status: vibeface_bridge::turn::TurnStatus::Failed,
            text: native.output.unwrap_or_default(),
            error: Some(ExecFailure::upstream(detail, FailureKind::Unknown)),
        },
        NativeStatus::Unknown => Probe::Missing,
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyLocator {
    agent: String,
    owner: String,
    workspace: PathBuf,
    executable: PathBuf,
    local_store_root: PathBuf,
    provider_identity: String,
}

fn provider_identity(
    api_key: &str,
    env: &BTreeMap<String, String>,
    configured_key: bool,
) -> Result<String> {
    Ok(crate::crypto::digest(&serde_json::to_vec(&(
        api_key,
        env,
        configured_key,
    ))?))
}

fn reply_locator_path(cursor_dir: &Path, client_id: &str) -> Result<PathBuf> {
    vibeface_protocol::validate_id(client_id).map_err(anyhow::Error::msg)?;
    Ok(cursor_dir.join("replies").join(format!(
        "{}.json",
        crate::crypto::digest(client_id.as_bytes())
    )))
}

pub(crate) fn record_reply(
    turn: &SdkTurn,
    configured_env: &BTreeMap<String, String>,
    client_id: &str,
    agent: &str,
) -> Result<()> {
    vibeface_protocol::validate_id(agent).map_err(anyhow::Error::msg)?;
    let instances = turn
        .local_store_root
        .parent()
        .context("Cursor native store has no parent")?;
    ensure!(
        instances
            .file_name()
            .is_some_and(|name| name == "instances"),
        "Cursor native store has no stable instance root"
    );
    let cursor_dir = instances
        .parent()
        .context("Cursor instance root has no parent")?;
    ensure!(
        cursor_dir.file_name().is_some_and(|name| name == "cursor"),
        "Cursor instance root is outside its private data directory"
    );
    ensure!(
        turn.local_store_root == instances.join(crate::crypto::digest(turn.owner.as_bytes())),
        "Cursor native store does not match its owner"
    );
    let locator = ReplyLocator {
        agent: agent.into(),
        owner: turn.owner.clone(),
        workspace: turn.workspace.clone(),
        executable: turn.executable.clone(),
        local_store_root: turn.local_store_root.clone(),
        provider_identity: provider_identity(
            &turn.api_key,
            &turn.env,
            configured_env.contains_key("CURSOR_API_KEY"),
        )?,
    };
    let path = reply_locator_path(cursor_dir, client_id)?;
    config::private_dir(
        path.parent()
            .context("Cursor reply locator has no parent")?,
    )?;
    config::private_write(&path, &serde_json::to_vec(&locator)?)
}

pub(crate) async fn probe_recorded_reply(
    node: &Runtime,
    client_id: &str,
    agent_id: &str,
    run_id: &str,
) -> Result<Probe> {
    let locator: ReplyLocator = match std::fs::read(reply_locator_path(
        &node.data_dir.join("cursor"),
        client_id,
    )?) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(locator) => locator,
            Err(_) => return Ok(Probe::Missing),
        },
        Err(_) => return Ok(Probe::Missing),
    };
    let expected_root = node
        .data_dir
        .join("cursor")
        .join("instances")
        .join(crate::crypto::digest(locator.owner.as_bytes()));
    if locator.agent != agent_id
        || locator.local_store_root != expected_root
        || !locator.workspace.is_absolute()
        || !locator.workspace.is_dir()
    {
        return Ok(Probe::Missing);
    }
    let configured_env = node.provider_env(Provider::Cursor);
    let (Some(api_key), env) = crate::providers::cursor_launch(&configured_env) else {
        return Ok(Probe::Missing);
    };
    if api_key.is_empty()
        || provider_identity(
            &api_key,
            &env,
            configured_env.contains_key("CURSOR_API_KEY"),
        )? != locator.provider_identity
    {
        return Ok(Probe::Missing);
    }
    let Some(bridge) = node.cursor.get() else {
        return Ok(Probe::Missing);
    };
    let turn = SdkTurn {
        owner: locator.owner,
        executable: locator.executable,
        workspace: locator.workspace,
        local_store_root: locator.local_store_root,
        api_key,
        env,
        model: String::new(),
        timeout_secs: node.cursor_executor.timeout_secs,
        custom_tools: BTreeMap::new(),
        callback_token: None,
        process_started: None,
        run_started: None,
    };
    Ok(match bridge.probe(&turn, agent_id, run_id).await {
        Ok(native) => native_probe(native),
        Err(_) => Probe::Missing,
    })
}

/// Process groups of the running bridges, with the program each one runs.
pub(super) const PIDFILE: &str = "cursor-sdk-bridge.pid";
const WORK_OWNERS: &str = "cursor/work-owners.json";

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct WorkOwner {
    network: String,
    conversation: String,
    root_work: String,
    workspace: PathBuf,
    agent_id: Option<String>,
}

#[derive(Clone, Default, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct OwnerJournal {
    works: BTreeMap<String, WorkOwner>,
    agents: BTreeMap<String, WorkOwner>,
}

struct LiveWork {
    owner: CursorInstance,
    turn: SdkTurn,
    cancel: watch::Sender<bool>,
}

#[derive(Clone)]
pub(super) struct CursorExecutor {
    bridge: Arc<std::sync::OnceLock<Arc<SdkBridge>>>,
    providers: Arc<StdMutex<crate::providers::Local>>,
    data_dir: PathBuf,
    timeout_secs: u64,
    owners: Arc<StdMutex<OwnerJournal>>,
    owner_file: PathBuf,
    live: Arc<Mutex<HashMap<String, LiveWork>>>,
    starting: Arc<Mutex<HashSet<String>>>,
}

impl CursorExecutor {
    pub(super) fn new(
        bridge: Arc<std::sync::OnceLock<Arc<SdkBridge>>>,
        providers: Arc<StdMutex<crate::providers::Local>>,
        data_dir: PathBuf,
        timeout_secs: u64,
    ) -> Result<Self> {
        let owner_file = data_dir.join(WORK_OWNERS);
        let owners = match std::fs::read(&owner_file) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("Cursor task owner journal is invalid")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => OwnerJournal::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            bridge,
            providers,
            data_dir,
            timeout_secs,
            owners: Arc::new(StdMutex::new(owners)),
            owner_file,
            live: Arc::new(Mutex::new(HashMap::new())),
            starting: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    fn owner_file(&self) -> &Path {
        &self.owner_file
    }

    fn save_owner(&self, work_id: &str, owner: WorkOwner) -> Result<()> {
        crate::config::private_dir(
            self.owner_file()
                .parent()
                .context("Cursor owner file has no parent")?,
        )?;
        let mut owners = self.owners.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = owners.clone();
        next.works.insert(work_id.into(), owner);
        let bytes = serde_json::to_vec(&next)?;
        crate::config::private_write(self.owner_file(), &bytes)?;
        *owners = next;
        Ok(())
    }

    fn remember_agent(&self, work_id: &str, agent_id: &str) -> Result<()> {
        let mut owners = self.owners.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = owners.clone();
        let owner = next
            .works
            .get_mut(work_id)
            .context("Cursor task owner is not journaled")?;
        owner.agent_id = Some(agent_id.into());
        let owner = owner.clone();
        next.agents.insert(agent_id.into(), owner);
        let bytes = serde_json::to_vec(&next)?;
        crate::config::private_write(self.owner_file(), &bytes)?;
        *owners = next;
        Ok(())
    }

    fn owner_for_work(&self, work_id: &str) -> Result<WorkOwner> {
        self.owners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .works
            .get(work_id)
            .cloned()
            .context("Cursor task owner is not journaled; waiting for reconciliation")
    }

    fn owner_for_agent(&self, agent_id: &str) -> Option<WorkOwner> {
        self.owners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .agents
            .get(agent_id)
            .cloned()
    }

    fn instance(owner: &WorkOwner) -> CursorInstance {
        CursorInstance::Work {
            network: owner.network.clone(),
            conversation: owner.conversation.clone(),
            root_work: owner.root_work.clone(),
        }
    }

    fn bridge(&self) -> Result<Arc<SdkBridge>> {
        self.bridge
            .get()
            .cloned()
            .context("Cursor SDK bridge is not ready; waiting for reconciliation")
    }

    fn turn(
        &self,
        owner: &CursorInstance,
        workspace: PathBuf,
        model: String,
        callback_token: Option<String>,
        run_started: Option<RunStarted>,
    ) -> Result<SdkTurn> {
        ensure!(
            workspace.is_absolute() && workspace.is_dir(),
            "Cursor work directory must be an existing absolute directory"
        );
        let local = self
            .providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let (api_key, env) = crate::providers::cursor_launch(local.env.get(Provider::Cursor));
        let api_key = api_key.context("CURSOR_API_KEY is required for Cursor execution")?;
        Ok(SdkTurn {
            owner: owner.key(),
            executable: local.bin(Provider::Cursor),
            workspace,
            local_store_root: self
                .data_dir
                .join("cursor")
                .join("instances")
                .join(crate::crypto::digest(owner.key().as_bytes())),
            api_key,
            env,
            model: Provider::Cursor.model(&model),
            timeout_secs: self.timeout_secs,
            custom_tools: BTreeMap::new(),
            callback_token,
            process_started: None,
            run_started,
        })
    }

    async fn lineage(
        &self,
        node: &Arc<Runtime>,
        network: &str,
        work: &WorkSpec,
    ) -> Result<WorkOwner> {
        if let Some(thread) = work.thread.as_deref()
            && let Some(owner) = self.owner_for_agent(thread)
        {
            ensure!(
                owner.network == network && owner.conversation == work.conversation,
                "Cursor thread belongs to another task scope"
            );
            return Ok(owner);
        }
        let projections = node.store.lock().await.works(network, &work.conversation)?;
        let mut by_id: HashMap<_, _> = projections
            .into_iter()
            .map(|projection| (projection.work_id.clone(), projection))
            .collect();
        let mut root = work.work_id.clone();
        let mut next = work.continues.clone();
        let mut seen = HashSet::new();
        while let Some(previous) = next {
            ensure!(
                seen.insert(previous.clone()),
                "Cursor continuation lineage contains a cycle"
            );
            let projection = by_id
                .remove(&previous)
                .context("continued Cursor work is not on this device")?;
            root = projection.work_id.clone();
            next = projection.continues;
        }
        let workspace = self
            .owner_for_work(&root)
            .ok()
            .map(|owner| owner.workspace)
            .or_else(|| {
                if root == work.work_id {
                    Some(PathBuf::from(&work.cwd))
                } else {
                    by_id
                        .get(&root)
                        .map(|projection| PathBuf::from(&projection.cwd))
                }
            })
            .context("Cursor task workspace is not available")?;
        Ok(WorkOwner {
            network: network.into(),
            conversation: work.conversation.clone(),
            root_work: root,
            workspace,
            agent_id: None,
        })
    }

    async fn finish(
        &self,
        node: &Arc<Runtime>,
        network: &str,
        work_id: &str,
        result: std::result::Result<vibeface_bridge::agent::AgentResult, AgentRunError>,
    ) {
        let record = match node.store.lock().await.work_record(work_id) {
            Ok(Some(record)) => record,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(event = "cursor.work_end_pending", %error, work_id, "Cursor work state could not be read");
                return;
            }
        };
        let id = record.state.handle().map(|handle| handle.turn_id.clone());
        let signal = match result {
            Ok(result) => match id {
                Some(turn_id) => Some(Signal::TurnCompleted {
                    turn_id,
                    status: vibeface_bridge::turn::TurnStatus::Completed,
                    text: result.output,
                    error: None,
                }),
                None => Some(Signal::Probed(Probe::Missing)),
            },
            Err(error) if error.start == StartEvidence::NotStarted => {
                Some(Signal::StartRejected(error.failure))
            }
            Err(error) if error.outcome == AgentFailure::Cancelled => match id {
                Some(turn_id) => Some(Signal::TurnCompleted {
                    turn_id,
                    status: vibeface_bridge::turn::TurnStatus::Interrupted,
                    text: String::new(),
                    error: None,
                }),
                None => Some(Signal::Probed(Probe::Missing)),
            },
            Err(_) if id.is_some() => Some(Signal::Disconnected),
            Err(_) => Some(Signal::Probed(Probe::Missing)),
        };
        if let Some(error) = signal.as_ref().and_then(|signal| match signal {
            Signal::StartRejected(failure) => Some(&failure.detail),
            _ => None,
        }) {
            tracing::warn!(event = "cursor.work_rejected", work_id, reason = %error, "Cursor task was rejected");
        }
        if let Some(signal) = signal
            && let Err(error) = node.signal_work(network, work_id, signal).await
        {
            tracing::warn!(event = "cursor.work_end_pending", %error, work_id, "Cursor work result waits for reconciliation");
        }
    }

    async fn follow(
        self,
        node: Weak<Runtime>,
        network: String,
        work: WorkSpec,
        turn: SdkTurn,
        previous: Option<String>,
        cancel: watch::Receiver<bool>,
    ) {
        let WorkSpec {
            work_id, prompt, ..
        } = work;
        let bridge = match self.bridge() {
            Ok(bridge) => bridge,
            Err(error) => {
                if let Some(node) = node.upgrade()
                    && let Err(signal_error) = node
                        .signal_work(&network, &work_id, Signal::Probed(Probe::Missing))
                        .await
                {
                    tracing::warn!(event = "cursor.work_bridge_pending", %signal_error, work_id, "Cursor bridge readiness waits for reconciliation");
                }
                tracing::warn!(event = "cursor.work_bridge_unavailable", %error, work_id, "Cursor task waits for bridge reconciliation");
                self.live.lock().await.remove(&work_id);
                return;
            }
        };
        let persist_node = node.clone();
        let persist_work = work_id.clone();
        let persist_network = network.clone();
        let owner_store = self.clone();
        let persist = move |id: String| {
            let persist_node = persist_node.clone();
            let persist_work = persist_work.clone();
            let persist_network = persist_network.clone();
            let owner_store = owner_store.clone();
            async move {
                owner_store.remember_agent(&persist_work, &id)?;
                let node = persist_node
                    .upgrade()
                    .context("Node closed while recording Cursor agent")?;
                let stopped = node
                    .store
                    .lock()
                    .await
                    .stop_requests()?
                    .into_iter()
                    .any(|request| request.work_id == persist_work);
                ensure!(!stopped, "Cursor work was stopped before Send");
                let record = node
                    .signal_work(
                        &persist_network,
                        &persist_work,
                        Signal::ThreadCreated(id.clone()),
                    )
                    .await?;
                ensure!(
                    matches!(record.state, WorkState::Starting { thread: Some(ref thread) } if thread == &id),
                    "Cursor work no longer accepts Send"
                );
                Ok(())
            }
        };
        let started_node = node.clone();
        let started_work = work_id.clone();
        let started_network = network.clone();
        let run_started: RunStarted = Arc::new(move |id| {
            let node = started_node.clone();
            let work = started_work.clone();
            let network = started_network.clone();
            Box::pin(async move {
                let node = node
                    .upgrade()
                    .context("Node closed while recording Cursor run")?;
                let record = node
                    .signal_work(
                        &network,
                        &work,
                        Signal::TurnStarted {
                            turn_id: id.clone(),
                            epoch: 0,
                        },
                    )
                    .await?;
                ensure!(
                    record
                        .state
                        .handle()
                        .is_some_and(|handle| handle.turn_id == id),
                    "Cursor work no longer tracks this native run"
                );
                Ok(())
            })
        });
        let mut turn = turn;
        turn.run_started = Some(run_started);
        let progress_node = node.clone();
        let progress_network = network.clone();
        let progress_work = work_id.clone();
        let text = Arc::new(StdMutex::new(String::new()));
        let progress_text = text.clone();
        let mut progress = move |update: Value| {
            let node = progress_node.clone();
            let network = progress_network.clone();
            let work_id = progress_work.clone();
            let text = progress_text.clone();
            async move {
                let Some(node) = node.upgrade() else {
                    return Ok(());
                };
                let kind = update["type"].as_str().unwrap_or_default();
                if let Some(value) = update["text"].as_str() {
                    let answer = {
                        let mut answer = text.lock().unwrap_or_else(PoisonError::into_inner);
                        match kind {
                            "text_snapshot" => *answer = value.into(),
                            "text_delta" => answer.push_str(value),
                            _ => {}
                        }
                        answer.clone()
                    };
                    node.signal_work(&network, &work_id, Signal::Text(answer))
                        .await?;
                }
                let summary = update["text"]
                    .as_str()
                    .or_else(|| update["status"].as_str())
                    .or_else(|| update["name"].as_str())
                    .unwrap_or("Cursor 正在执行");
                if !summary.trim().is_empty() {
                    node.signal_work(
                        &network,
                        &work_id,
                        Signal::Progress {
                            summary: summary.into(),
                            at_ms: now_ms(),
                        },
                    )
                    .await?;
                }
                let target_node = node.identity.device_id()?;
                node.publish_progress(
                    &network,
                    json!({"type":"native_progress","conversation_id":node.store.lock().await.work_record(&work_id)?.map(|r| r.conversation).unwrap_or_default(),"work_id":work_id,"target_node":target_node,"sequence":node.progress_sequence.fetch_add(1, Ordering::Relaxed)+1,"progress":update}),
                );
                Ok(())
            }
        };
        let result = bridge
            .run(
                &turn,
                &prompt,
                previous.as_deref(),
                persist,
                cancel,
                &mut progress,
            )
            .await;
        if let Some(node) = node.upgrade() {
            self.finish(&node, &network, &work_id, result).await;
        }
        self.live.lock().await.remove(&work_id);
    }
}

impl Executor for CursorExecutor {
    fn start<'a>(
        &'a self,
        node: &'a Arc<Runtime>,
        network: &'a str,
        work: &'a WorkSpec,
        thread: Option<&'a str>,
        _args: &'a Value,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            ensure!(
                thread.is_none_or(|thread| !thread.trim().is_empty()),
                "invalid Cursor task thread"
            );
            let _bridge = self.bridge()?;
            let owner = self.lineage(node, network, work).await?;
            let instance = Self::instance(&owner);
            let key = instance.key();
            {
                let mut starting = self.starting.lock().await;
                let live = self.live.lock().await;
                ensure!(
                    !live.values().any(|entry| entry.owner.key() == key)
                        && starting.insert(key.clone()),
                    "Cursor SDK still runs another work in this session"
                );
            }
            if let Err(error) = self.save_owner(&work.work_id, owner.clone()) {
                self.starting.lock().await.remove(&key);
                return Err(error);
            }
            let (cancel, cancel_rx) = watch::channel(false);
            let turn = match self.turn(
                &instance,
                owner.workspace.clone(),
                work.execution_model().unwrap_or_default().into(),
                None,
                None,
            ) {
                Ok(turn) => turn,
                Err(error) => {
                    self.starting.lock().await.remove(&key);
                    return Err(error);
                }
            };
            self.live.lock().await.insert(
                work.work_id.clone(),
                LiveWork {
                    owner: instance,
                    turn: turn.clone(),
                    cancel,
                },
            );
            self.starting.lock().await.remove(&key);
            let runner = self.clone();
            let node = Arc::downgrade(node);
            let network = network.to_owned();
            let work = work.clone();
            let previous = thread.map(str::to_owned);
            tokio::spawn(async move {
                runner
                    .follow(node, network, work, turn, previous, cancel_rx)
                    .await;
            });
            Ok(false)
        })
    }

    fn probe<'a>(&'a self, work: &'a WorkRecord) -> BoxFuture<'a, Result<Probe>> {
        Box::pin(async move {
            let agent_id = work
                .state
                .thread()
                .context("Cursor work has no native agent ID")?;
            let Some(run_id) = work.state.handle().map(|handle| handle.turn_id.as_str()) else {
                return Ok(Probe::Missing);
            };
            let live_turn = self
                .live
                .lock()
                .await
                .get(&work.work_id)
                .map(|live| (live.owner.clone(), live.turn.clone()));
            let turn = match live_turn {
                Some((_instance, turn)) => turn,
                None => {
                    let owner = self.owner_for_work(&work.work_id)?;
                    let instance = Self::instance(&owner);
                    self.turn(&instance, owner.workspace, String::new(), None, None)?
                }
            };
            let bridge = self.bridge()?;
            let native = match bridge.probe(&turn, agent_id, run_id).await {
                Ok(native) => native,
                Err(error) => {
                    tracing::warn!(event = "cursor.work_probe_unknown", %error, work_id = work.work_id, "Cursor run readback is unavailable");
                    return Ok(Probe::Missing);
                }
            };
            Ok(native_probe(native))
        })
    }

    fn interrupt<'a>(&'a self, work: &'a WorkRecord) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(live) = self.live.lock().await.get(&work.work_id) {
                live.cancel
                    .send(true)
                    .map_err(|_| anyhow::anyhow!("Cursor task is no longer running"))?;
                return Ok(());
            }
            let owner = self.owner_for_work(&work.work_id)?;
            let instance = Self::instance(&owner);
            let agent_id = work
                .state
                .thread()
                .context("Cursor work has no native agent ID")?;
            let run_id = work
                .state
                .handle()
                .map(|handle| handle.turn_id.as_str())
                .context("Cursor work has no native run ID")?;
            let turn = self.turn(&instance, owner.workspace, String::new(), None, None)?;
            self.bridge()?.cancel(&turn, agent_id, run_id).await
        })
    }

    fn steer<'a>(
        &'a self,
        _: &'a crate::scheduling::SteerTarget,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, SteerAck> {
        Box::pin(async { SteerAck::Refused("Cursor SDK has no native steer".into()) })
    }

    fn read_back<'a>(
        &'a self,
        _: &'a crate::scheduling::SteerTarget,
        _: &'a str,
    ) -> BoxFuture<'a, Option<SteerTrace>> {
        Box::pin(async { None })
    }
}

impl Runtime {
    pub(crate) fn cursor_store_root(&self, owner: &CursorInstance) -> PathBuf {
        self.data_dir
            .join("cursor")
            .join("instances")
            .join(crate::crypto::digest(owner.key().as_bytes()))
    }

    /// What every coordination Agent shell gets, whatever the turn.
    pub(crate) fn agent_env(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "VIBEFACE_DATA_DIR".into(),
                self.data_dir.display().to_string(),
            ),
            (
                TRIAGE_URL_ENV.into(),
                format!("http://{}", self.config.local_listen),
            ),
            (
                "VIBEFACE_CODEX_SOCKET".into(),
                self.codex.socket().display().to_string(),
            ),
        ])
    }

    /// The tool listener is up, so the bridge can call back and turns may start.
    pub(crate) fn tools_ready(&self, url: String) -> Result<()> {
        let bridge = SdkBridge::new(
            self.agent_env(),
            ToolCallbackConfig {
                url: url.clone(),
                auth_token: self.cursor_callback_token.clone(),
            },
            Some(self.data_dir.join(PIDFILE)),
        )?;
        // The bridge goes first: a turn that sees the tool URL must find the bridge too.
        self.cursor
            .set(Arc::new(bridge))
            .map_err(|_| anyhow::anyhow!("Cursor SDK bridge is already initialized"))?;
        self.tool_url
            .set(url)
            .map_err(|_| anyhow::anyhow!("tool listener is already initialized"))?;
        self.refresh_cursor();
        Ok(())
    }

    pub(super) fn refresh_cursor(&self) {
        let Some(bridge) = self.cursor.get().cloned() else {
            return;
        };
        let launch = self
            .manager()
            .filter(|manager| manager.provider == Provider::Cursor)
            .and_then(|manager| {
                let (key, env) =
                    crate::providers::cursor_launch(&self.provider_env(manager.provider));
                Some((manager, key?, env))
            });
        let bin = self.providers().bin(Provider::Cursor);
        let owner = CursorInstance::Coordination;
        let owner_key = owner.key();
        let store_root = self.cursor_store_root(&owner);
        tokio::spawn(async move {
            if let Some((manager, api_key, env)) = launch
                && let Err(error) = bridge
                    .prewarm(
                        &owner_key,
                        &bin,
                        &manager.workspace,
                        &store_root,
                        &api_key,
                        &env,
                    )
                    .await
            {
                tracing::warn!(event = "cursor.prewarm_failed", %error, "Cursor SDK bridge did not start ahead of a reply");
            }
        });
    }

    /// Access for a tool call. Cursor installs the active attempt bearer on its owner bridge;
    /// the existing authorization and fence checks then decide whether the call is still valid.
    pub(crate) async fn tool_access(&self, bearer: &str, _agent_id: &str) -> Option<Access> {
        self.authorize_bearer(bearer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{self, Config},
        crypto::LocalIdentity,
        identity::{Genesis, Member, NetworkState},
    };
    use std::time::Instant;

    fn access(conversation: &str) -> Access {
        Access {
            network_id: "network".into(),
            human: false,
            conversation_id: Some(conversation.into()),
            manager: None,
        }
    }

    #[tokio::test]
    async fn cursor_callbacks_use_the_attempt_bearer() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("vibeface-cursor-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        let mut configuration = Config::local("127.0.0.1:0".parse()?, vec![], false);
        configuration.providers.codex_app_server.socket = Some(dir.join("unavailable.sock"));
        configuration.save(&dir)?;
        config::private_write(&dir.join("data-epoch.json"), &config::data_epoch_marker())?;
        config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&LocalIdentity::generate())?,
        )?;
        let node = Runtime::open(&dir).await?;
        for (token, conversation) in [
            ("attempt-a", "conversation-a"),
            ("attempt-b", "conversation-b"),
        ] {
            node.agent_tokens
                .lock()
                .await
                .insert(token.into(), access(conversation));
        }
        let conversation =
            |access: Option<Access>| access.and_then(|access| access.conversation_id);

        assert_eq!(
            conversation(node.tool_access("attempt-a", "agent-a").await).as_deref(),
            Some("conversation-a")
        );
        assert_eq!(
            conversation(node.tool_access("attempt-b", "agent-b").await).as_deref(),
            Some("conversation-b")
        );
        assert!(node.tool_access("unknown", "agent-a").await.is_none());
        assert!(
            node.tool_access("attempt-a", "agent-b")
                .await
                .is_some_and(|access| access.conversation_id.as_deref() == Some("conversation-a"))
        );

        node.revoke_tool_token("attempt-a").await;
        assert!(node.tool_access("attempt-a", "agent-a").await.is_none());
        assert!(node.tool_access("attempt-b", "agent-b").await.is_some());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn bot_native_run_is_journaled_and_read_back_without_another_send() -> Result<()> {
        let (node, dir, network, workspace) = cursor_node().await?;
        let owner = CursorInstance::Bot {
            network,
            conversation: "bot-readback".into(),
        };
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let recorded = calls.clone();
        node.providers.lock().unwrap().env.apply(
            Provider::Cursor,
            BTreeMap::from([
                (
                    "FAKE_BRIDGE_GET_RUN_STATUS".into(),
                    Some("RUN_LIFECYCLE_STATUS_UNSPECIFIED".into()),
                ),
                (
                    "FAKE_BRIDGE_LOG".into(),
                    Some(dir.join("bridge-starts.log").display().to_string()),
                ),
            ]),
        )?;
        let variables = node.provider_env(Provider::Cursor);
        let config = crate::manager::ManagerConfig {
            sdk: crate::manager::Sdk::Cursor {
                bridge: node.cursor.get().cloned(),
                sdk_bin: fake_bridge(),
                owner: None,
                local_store_root: None,
            },
            workspace: workspace.clone(),
            model: "fixture-model".into(),
            reasoning_effort: None,
            timeout_secs: 10,
            custom_tools: BTreeMap::new(),
            tool_cli: None,
            instructions: String::new(),
            tool_env: BTreeMap::new(),
            tool_token: None,
            provider_env: variables,
            process_started: None,
            codex_turn: Some(crate::manager::CodexTurn {
                client_id: "bot-attempt".into(),
                record: Arc::new(move |agent, run| {
                    recorded
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push((agent, run));
                    Box::pin(async { Ok(()) })
                }),
                fresh_prompt: None,
            }),
        }
        .for_cursor_instance(owner.clone(), node.cursor_store_root(&owner));
        let result = crate::manager::run_manager(
            &config,
            "read it back",
            None,
            |_| async { Ok(()) },
            watch::channel(false).1,
            |_| async { Ok(()) },
        )
        .await?;
        let agent = result.session_id.context("agent ID")?;
        let journal = calls.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert_eq!(journal.len(), 2);
        assert_eq!(journal[0], (agent.clone(), None));
        let run = journal[1].1.as_deref().context("native run ID journaled")?;
        assert_eq!(
            probe_recorded_reply(&node, "bot-attempt", &agent, run).await?,
            Probe::Terminal {
                turn_id: run.into(),
                status: vibeface_bridge::turn::TurnStatus::Completed,
                text: "done: read it back".into(),
                error: None,
            }
        );
        assert_eq!(
            probe_recorded_reply(&node, "bot-attempt", &agent, "not-the-run").await?,
            Probe::Missing
        );
        let new_workspace = dir.join("new-workspace");
        config::private_dir(&new_workspace)?;
        node.settings.lock().unwrap().manager = Some(config::ManagerSettings {
            provider: Provider::Cursor,
            workspace: new_workspace,
            model: "different-model".into(),
            reasoning_effort: None,
            backup: Default::default(),
        });
        let expected = Probe::Terminal {
            turn_id: run.into(),
            status: vibeface_bridge::turn::TurnStatus::Completed,
            text: "done: read it back".into(),
            error: None,
        };
        assert_eq!(
            probe_recorded_reply(&node, "bot-attempt", &agent, run).await?,
            expected
        );
        node.settings.lock().unwrap().manager = None;
        node.providers
            .lock()
            .unwrap()
            .bins
            .insert(Provider::Cursor, dir.join("no-new-binary"));
        assert_eq!(
            probe_recorded_reply(&node, "bot-attempt", &agent, run).await?,
            expected
        );
        let locator_path = reply_locator_path(&node.data_dir.join("cursor"), "bot-attempt")?;
        let locator_bytes = std::fs::read(&locator_path)?;
        let saved = std::str::from_utf8(&locator_bytes)?;
        assert!(!saved.contains("fixture-api-key") && !saved.contains("CURSOR_API_KEY"));
        let launches = std::fs::read_to_string(dir.join("bridge-starts.log"))?;
        node.providers.lock().unwrap().env.apply(
            Provider::Cursor,
            BTreeMap::from([("CURSOR_API_KEY".into(), Some("rotated-key".into()))]),
        )?;
        assert_eq!(
            probe_recorded_reply(&node, "bot-attempt", &agent, run).await?,
            Probe::Missing
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("bridge-starts.log"))?,
            launches,
            "changed authentication never starts another native bridge"
        );
        node.providers.lock().unwrap().env.apply(
            Provider::Cursor,
            BTreeMap::from([("CURSOR_API_KEY".into(), Some("fixture-api-key".into()))]),
        )?;
        config::private_write(&locator_path, b"not json")?;
        assert_eq!(
            probe_recorded_reply(&node, "bot-attempt", &agent, run).await?,
            Probe::Missing
        );
        config::private_write(&locator_path, &locator_bytes)?;
        node.providers.lock().unwrap().env.apply(
            Provider::Cursor,
            BTreeMap::from([("CURSOR_API_KEY".into(), None)]),
        )?;
        assert_eq!(
            probe_recorded_reply(&node, "bot-attempt", &agent, run).await?,
            Probe::Missing
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("bridge-starts.log"))?,
            launches
        );
        node.providers.lock().unwrap().env.apply(
            Provider::Cursor,
            BTreeMap::from([("CURSOR_API_KEY".into(), Some("fixture-api-key".into()))]),
        )?;
        let blocked = reply_locator_path(&node.data_dir.join("cursor"), "blocked-attempt")?;
        config::private_dir(&blocked)?;
        let mut blocked_config = config.clone();
        blocked_config
            .codex_turn
            .as_mut()
            .context("turn recorder")?
            .client_id = "blocked-attempt".into();
        let result = crate::manager::run_manager(
            &blocked_config,
            "must not send",
            None,
            |_| async { Ok(()) },
            watch::channel(false).1,
            |_| async { Ok(()) },
        )
        .await;
        let error = result.err().context("expected locator failure")?;
        let error = error
            .downcast_ref::<crate::manager::ManagerError>()
            .context("structured failure")?;
        assert_eq!(error.stage, vibeface_bridge::failure::ProviderStage::Record);
        assert_eq!(error.start, StartEvidence::NotStarted);
        assert_eq!(
            std::fs::read_to_string(dir.join("sends.log"))?
                .lines()
                .count(),
            1
        );
        node.shutdown().await;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    fn fake_bridge() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../bridge/tests/fixtures/fake-cursor-sdk-bridge")
    }

    async fn cursor_node() -> Result<(Arc<Runtime>, PathBuf, String, PathBuf)> {
        let dir =
            std::env::temp_dir().join(format!("vibeface-cursor-work-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        let workspace = dir.join("workspace");
        config::private_dir(&workspace)?;
        let send_log = dir.join("sends.log");
        let mut configuration =
            Config::local("127.0.0.1:0".parse()?, vec![workspace.clone()], false);
        configuration.providers.execution = [Provider::Cursor].into();
        configuration.save(&dir)?;
        config::private_write(&dir.join("data-epoch.json"), &config::data_epoch_marker())?;
        config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&LocalIdentity::generate())?,
        )?;
        let node = Arc::new(Runtime::open(&dir).await?);
        {
            let mut local = node.providers.lock().unwrap();
            local.bins.insert(Provider::Cursor, fake_bridge());
            local.env.apply(
                Provider::Cursor,
                BTreeMap::from([
                    ("CURSOR_API_KEY".into(), Some("fixture-api-key".into())),
                    (
                        "FAKE_BRIDGE_SEND_LOG".into(),
                        Some(send_log.display().to_string()),
                    ),
                ]),
            )?;
        }
        node.tools_ready("http://127.0.0.1:1".into())?;
        let admin = LocalIdentity::generate();
        let state = NetworkState::new(Genesis::create(
            &LocalIdentity::generate(),
            Member::client(&admin, "admin", true)?,
            Member::node(&node.identity, "node")?,
        )?)?;
        let network = state.genesis.network_id.clone();
        node.install_membership(state).await?;
        Ok((node, dir, network, workspace))
    }

    fn work(node: &Runtime, workspace: &Path, call_ref: &str, prompt: &str) -> Result<WorkSpec> {
        let device = node.identity.device_id()?;
        Ok(WorkSpec {
            work_id: WorkSpec::id("conversation", "message:prompt", call_ref),
            conversation: "conversation".into(),
            trigger: "message:prompt".into(),
            target: device.clone(),
            dispatcher: device,
            call_ref: call_ref.into(),
            kind: WorkKind::Agent(Provider::Cursor),
            model: None,
            resolved: crate::model::ResolvedModel {
                provider: Provider::Cursor,
                model: Some("fixture-model".into()),
            },
            cwd: workspace.display().to_string(),
            title: "cursor work".into(),
            prompt: prompt.into(),
            continues: None,
            parent: None,
            thread: None,
        })
    }

    async fn until_finished(node: &Runtime, work_id: &str) -> Result<WorkRecord> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let record = node
                .store
                .lock()
                .await
                .work_record(work_id)?
                .context("Cursor work record")?;
            if record.state.finished() || Instant::now() >= deadline {
                return Ok(record);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn cursor_start_rejection_requires_explicit_not_started_evidence() -> Result<()> {
        let (node, dir, network, workspace) = cursor_node().await?;
        for (call_ref, start, outcome) in [
            (
                "not-started",
                StartEvidence::NotStarted,
                AgentFailure::Failed,
            ),
            ("started", StartEvidence::Started, AgentFailure::Failed),
            ("unknown", StartEvidence::Unknown, AgentFailure::Failed),
            (
                "cancelled-after-start",
                StartEvidence::Started,
                AgentFailure::Cancelled,
            ),
        ] {
            let spec = work(&node, &workspace, call_ref, "fixture prompt")?;
            node.store.lock().await.accept_work_fixture(
                &node.identity,
                &network,
                spec.clone(),
                now_ms(),
            )?;
            node.signal_work(
                &network,
                &spec.work_id,
                Signal::ThreadCreated("fixture-agent".into()),
            )
            .await?;
            node.cursor_executor
                .finish(
                    &node,
                    &network,
                    &spec.work_id,
                    Err(AgentRunError {
                        stage: vibeface_bridge::failure::ProviderStage::TurnStart,
                        outcome,
                        start,
                        session_id: Some("fixture-agent".into()),
                        failure: ExecFailure::new(
                            FailureKind::Lost,
                            "fixture run handle was not recorded",
                        ),
                    }),
                )
                .await;
            let record = node
                .store
                .lock()
                .await
                .work_record(&spec.work_id)?
                .context("finished Cursor classification")?;
            if start == StartEvidence::NotStarted {
                assert_eq!(
                    record.state,
                    WorkState::Finished {
                        outcome: WorkOutcome::Rejected
                    }
                );
            } else {
                assert!(
                    matches!(record.state, WorkState::Unverified { handle: None, thread: Some(ref thread), .. } if thread == "fixture-agent"),
                    "a missing run handle cannot prove the model never started: {record:?}"
                );
            }
        }
        node.shutdown().await;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn cursor_work_dispatches_once_and_returns_result() -> Result<()> {
        let (node, dir, network, workspace) = cursor_node().await?;
        let spec = work(&node, &workspace, "once", "build it")?;
        node.store.lock().await.accept_work_fixture(
            &node.identity,
            &network,
            spec.clone(),
            now_ms(),
        )?;
        assert!(
            !node
                .start_work(
                    node.executor(Provider::Cursor)?,
                    &network,
                    &spec,
                    None,
                    &json!({"model":"fixture-model"})
                )
                .await?
        );
        let finished = until_finished(&node, &spec.work_id).await?;
        assert_eq!(
            finished.state,
            WorkState::Finished {
                outcome: WorkOutcome::Completed
            },
            "{finished:?}"
        );
        assert_eq!(finished.text, "done: build it");
        let (_, created) = node.store.lock().await.accept_work_fixture(
            &node.identity,
            &network,
            spec,
            now_ms(),
        )?;
        assert!(
            !created,
            "retrying the same call only reads the existing work"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sends.log"))?
                .lines()
                .count(),
            1
        );
        node.shutdown().await;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn cursor_interrupt_waits_for_provider_cancelled_terminal() -> Result<()> {
        let (node, dir, network, workspace) = cursor_node().await?;
        let spec = work(&node, &workspace, "cancel", "hang around")?;
        node.store.lock().await.accept_work_fixture(
            &node.identity,
            &network,
            spec.clone(),
            now_ms(),
        )?;
        node.start_work(
            node.executor(Provider::Cursor)?,
            &network,
            &spec,
            None,
            &json!({"model":"fixture-model"}),
        )
        .await?;
        let running = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let record = node
                    .store
                    .lock()
                    .await
                    .work_record(&spec.work_id)?
                    .context("running Cursor work")?;
                if matches!(record.state, WorkState::Running { .. }) {
                    break Ok::<_, anyhow::Error>(record);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("Cursor work did not expose a native run ID")??;
        node.interrupt_work(&network, &spec.work_id).await?;
        let finished = until_finished(&node, &spec.work_id).await?;
        assert_eq!(
            finished.state,
            WorkState::Finished {
                outcome: WorkOutcome::Interrupted
            }
        );
        assert!(running.state.handle().is_some());
        node.shutdown().await;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
