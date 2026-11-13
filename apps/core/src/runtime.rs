pub(crate) use crate::store::actor::{ClientAuthorityState, ClientMembership};
use crate::{
    config::{self, Config},
    crypto::LocalIdentity,
    identity::{AuthorizationView, GovernanceAction, IdentityKind, NetworkState},
    maintenance::Gate,
    model::*,
    providers::{Provider, ProviderRoots},
    store::{Store, StoreActor, WriteState, admission::AdmissionRejected},
};
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, broadcast, mpsc};
use vibeface_protocol::valid_business_id;

pub(crate) mod agent_tools;
mod bots;
mod chat_controls;
pub mod claude;
pub(crate) mod codex_server;
mod cursor;
mod executor;
mod followup;
mod input_receipts;
mod manager_control;
mod observation;
pub mod opencode;
mod peers;
mod permission_fences;
pub mod pi;
mod provider_adapter;
mod provider_probe;
mod relay;
mod schedule_delivery;
mod scheduler_history;
mod settings;
mod triage;
#[cfg(test)]
pub(crate) use triage::tests::{
    bot as test_bot, node as test_node, start_reply as test_start_reply,
};
mod work;
mod work_input;
mod workspace;
pub(crate) use cursor::record_reply as record_cursor_reply;
pub use manager_control::ManagerAccess;
pub(crate) use observation::{ObservationGuard, native_progress};
pub use settings::NodeSettings;
pub use triage::{
    AGENT_ANSWER_WITHIN, HOOK_ANSWER_WITHIN, SPOOL_DIR as HOOK_SPOOL, TRIAGE_URL_ENV,
};

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::{
        identity::{Genesis, Member},
        work::{WorkRecord, WorkSpec, WorkState},
    };

    #[track_caller]
    pub(super) fn assert_work_outcome(record: &WorkRecord, outcome: WorkOutcome) {
        assert_eq!(record.state, WorkState::Finished { outcome });
    }

    pub(super) fn provider_work(
        provider: Provider,
        target: &str,
        cwd: &Path,
        prompt: &str,
    ) -> WorkSpec {
        WorkSpec {
            work_id: "w-1".into(),
            conversation: "c".into(),
            trigger: "message:m1".into(),
            target: target.into(),
            dispatcher: target.into(),
            call_ref: "w-1".into(),
            kind: WorkKind::Agent(provider),
            model: None,
            resolved: ResolvedModel {
                provider,
                model: None,
            },
            cwd: cwd.display().to_string(),
            title: "t".into(),
            prompt: prompt.into(),
            continues: None,
            parent: None,
            thread: None,
        }
    }

    pub(crate) fn initialize_node(
        dir: &Path,
        configuration: &Config,
        identity: &LocalIdentity,
    ) -> Result<()> {
        config::private_dir(dir)?;
        configuration.save(dir)?;
        config::private_write(&dir.join("data-epoch.json"), &config::data_epoch_marker())?;
        config::private_write(&dir.join("identity.json"), &serde_json::to_vec(identity)?)
    }

    pub(super) async fn open_node(dir: &Path, configuration: &Config) -> Result<Arc<Runtime>> {
        initialize_node(dir, configuration, &LocalIdentity::generate())?;
        Ok(Arc::new(Runtime::open(dir).await?))
    }

    pub(crate) fn access(
        network_id: String,
        human: bool,
        conversation_id: Option<String>,
    ) -> Access {
        Access {
            network_id,
            human,
            conversation_id,
            manager: None,
        }
    }

    pub(crate) fn signed_main_input(
        admin: &LocalIdentity,
        network: &str,
        scope: &str,
        id: &str,
        text: &str,
        attachments: Vec<Attachment>,
        sequence: u64,
    ) -> Result<EventBody> {
        admin.attest(
            network,
            scope,
            EventBody::InputPosted {
                message_id: id.into(),
                target: crate::workspace::InputTarget::BotMain {
                    bot_id: scope.into(),
                },
                intent: crate::workspace::InputIntent::Continue,
                sequence,
                expected_revision: None,
                text: text.into(),
                attachments,
                report_results: Some(true),
                attestation: None,
            },
        )
    }

    pub(crate) fn manager_settings(
        provider: Provider,
        workspace: PathBuf,
        model: String,
    ) -> config::ManagerSettings {
        config::ManagerSettings {
            provider,
            workspace,
            model,
            reasoning_effort: None,
            backup: Default::default(),
        }
    }

    pub(super) async fn join_network(node: &Runtime) -> Result<(LocalIdentity, String)> {
        let admin = LocalIdentity::generate();
        let state = NetworkState::new(Genesis::create(
            &LocalIdentity::generate(),
            Member::client(&admin, "admin", true)?,
            Member::node(&node.identity, "node")?,
        )?)?;
        let network = state.genesis.network_id.clone();
        node.install_membership(state).await?;
        Ok((admin, network))
    }
}

fn node_display_names(bundle: &NetworkState) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    for member in bundle.genesis.members().iter().chain(
        bundle
            .checkpoints
            .iter()
            .flat_map(|c| c.directory.iter().flat_map(|d| &d.members)),
    ) {
        if member.kind == IdentityKind::Node {
            names.insert(member.device_id.clone(), member.name.clone());
        }
    }
    for event in &bundle.events {
        if let GovernanceAction::Grant { member } = &event.action
            && member.kind == IdentityKind::Node
        {
            names.insert(member.device_id.clone(), member.name.clone());
        }
    }
    names
}

#[cfg(test)]
mod node_display_names_tests {
    use super::node_display_names;
    use crate::crypto::LocalIdentity;
    use crate::identity::{Genesis, GovernanceEvent, Member, NetworkState, Revocation};
    use anyhow::Result;

    #[test]
    fn revoked_node_keeps_last_granted_name() -> Result<()> {
        let root = LocalIdentity::generate();
        let admin = LocalIdentity::generate();
        let node = LocalIdentity::generate();
        let first = LocalIdentity::generate();
        let mut state = NetworkState::new(Genesis::create(
            &root,
            Member::client(&admin, "admin", true)?,
            Member::node(&first, "first")?,
        )?)?;
        state.append(GovernanceEvent::grant(
            &admin,
            &state.project()?,
            Member::node(&node, "Mac Studio")?,
        )?)?;
        state.append(GovernanceEvent::revoke(
            &admin,
            &state.project()?,
            Revocation::new(&node.device_id()?, None)?,
        )?)?;
        let names = node_display_names(&state);
        assert_eq!(
            names.get(&node.device_id()?).map(String::as_str),
            Some("Mac Studio")
        );
        Ok(())
    }
}

#[derive(Clone)]
pub struct Access {
    pub network_id: String,
    pub human: bool,
    pub conversation_id: Option<String>,
    pub manager: Option<ManagerAccess>,
}

const WORKSPACE_OVERVIEW_SCHEMA: u32 = 1;
const WORKSPACE_OVERVIEW_DEFAULT_LIMIT: usize = 256;
const WORKSPACE_OVERVIEW_MAX_LIMIT: usize = 256;

fn workspace_bounded(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn workspace_bounded_opt(value: Option<&str>, limit: usize) -> Option<String> {
    value
        .filter(|value| !value.is_empty())
        .map(|value| workspace_bounded(value, limit))
}

fn workspace_limit(input: &Value) -> Result<usize> {
    let limit = input
        .get("limit")
        .map(|value| value.as_u64().context("limit must be an integer"))
        .transpose()?
        .unwrap_or(WORKSPACE_OVERVIEW_DEFAULT_LIMIT as u64);
    ensure!(limit > 0, "limit must be greater than zero");
    ensure!(
        limit <= WORKSPACE_OVERVIEW_MAX_LIMIT as u64,
        "limit exceeds the workspace overview maximum"
    );
    Ok(limit as usize)
}

fn workspace_bool(input: &Value, key: &str, default: bool) -> Result<bool> {
    input
        .get(key)
        .map(|value| value.as_bool().context(format!("{key} must be a boolean")))
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn workspace_work_summary(work: &WorkProjection, source: &str) -> Value {
    let mut row = json!({
        "work_id": work.work_id,
        "conversation_id": work.conversation_id,
        "trigger": work.trigger,
        "title": workspace_bounded(&work.title, 256),
        "summary": workspace_bounded(&work.summary, 512),
        "cwd": workspace_bounded(&work.cwd, 512),
        "state": work.state,
        "outcome": work.outcome,
        "kind": work.kind,
        "target_node": work.target_node,
        "dispatcher": work.dispatcher,
        "call_ref": work.call_ref,
        "model": work.model,
        "resolved": work.resolved,
        "parent": work.parent,
        "continues": work.continues,
        "thread": work.thread,
        "thread_id": work.thread_id,
        "turn_id": work.turn_id,
        "phase": work.phase,
        "source": source,
    });
    if let Some(reason) = workspace_bounded_opt(work.failure_reason.as_deref(), 512) {
        row["failure_reason"] = json!(reason);
    }
    if let Some(failure) = work.failure {
        row["failure"] = json!(failure);
    }
    if work.outcome == Some(WorkOutcome::Unverified) {
        row["unknown_reason"] = json!("work_result_unverified");
    }
    row
}

fn workspace_question_summary(question: &Value) -> Value {
    let mut row = json!({});
    for key in [
        "question_id",
        "conversation_id",
        "bot",
        "author",
        "created_at_ms",
        "severity",
        "choices",
        "attention",
    ] {
        if let Some(value) = question.get(key) {
            row[key] = value.clone();
        }
    }
    if row["bot"].is_object() {
        row["bot"]["conversation_id"] = question["conversation_id"].clone();
    }
    if let Some(text) = question.get("text").and_then(Value::as_str) {
        row["text"] = json!(workspace_bounded(text, 1024));
    }
    if let Some(about) = question.get("about").and_then(Value::as_object) {
        let mut safe = json!({});
        for key in ["node", "agent", "session_id", "request_id"] {
            if let Some(value) = about.get(key) {
                safe[key] = value.clone();
            }
        }
        for (key, limit) in [("cwd", 512), ("summary", 512)] {
            if let Some(value) = about.get(key).and_then(Value::as_str) {
                safe[key] = json!(workspace_bounded(value, limit));
            }
        }
        row["about"] = safe;
    }
    row
}

fn workspace_session_summary(session: &Value) -> Value {
    let mut row = json!({});
    for key in [
        "conversation_id",
        "node",
        "session_id",
        "agent",
        "muted",
        "created_at_ms",
        "version",
    ] {
        if let Some(value) = session.get(key) {
            row[key] = value.clone();
        }
    }
    if let Some(cwd) = session.get("cwd").and_then(Value::as_str) {
        row["cwd"] = json!(workspace_bounded(cwd, 512));
    }
    if let Some(summary) = session.get("summary").and_then(Value::as_str) {
        row["summary"] = json!(workspace_bounded(summary, 512));
    }
    row
}

fn workspace_job_summary(
    job: &crate::scheduling::SchedulerJob,
    source_node: &str,
    owner_epoch: u64,
    pending_checks: &BTreeSet<String>,
) -> Value {
    json!({
        "conversation_id": job.conversation_id,
        "trigger": job.trigger,
        "kind": job.kind,
        "lane": job.kind.lane(),
        "sequence": job.sequence,
        "state": job.state.name(),
        "message_id": job.message_id,
        "source_node": source_node,
        "owner_epoch": owner_epoch,
        "runtime": job.runtime,
        "retryable": job.retryable(),
        "unknown_reason": pending_checks.contains(&format!("{}\n{}", job.conversation_id, job.trigger))
            .then_some("pending_check"),
    })
}

#[cfg(test)]
mod workspace_overview_tests {
    use super::*;

    #[test]
    fn question_and_session_summaries_keep_their_conversation() {
        let question = workspace_question_summary(&json!({
            "question_id": "q", "conversation_id": "question-chat",
            "bot": {"bot_id": "b", "name": "Bot"}, "text": "Which format?",
            "severity": "attention", "choices": [], "created_at_ms": 1,
        }));
        assert_eq!(question["conversation_id"], "question-chat");
        assert_eq!(
            question["bot"],
            json!({
                "bot_id": "b", "name": "Bot", "conversation_id": "question-chat",
            })
        );
        let unowned = workspace_question_summary(&json!({"conversation_id": "gone", "bot": null}));
        assert!(unowned["bot"].is_null());
        let session = workspace_session_summary(&json!({
            "conversation_id": "session-chat", "node": "n", "session_id": "s",
        }));
        assert_eq!(session["conversation_id"], "session-chat");
    }

    #[test]
    fn work_summary_does_not_expose_prompt_or_raw_result() {
        let work = WorkProjection {
            work_id: "work-1".into(),
            conversation_id: "conversation-1".into(),
            target_node: "node-1".into(),
            trigger: "task:work-1".into(),
            dispatcher: "node-2".into(),
            kind: WorkKind::Task,
            cwd: "/private/project".into(),
            title: "title".into(),
            prompt: "provider secret".into(),
            call_ref: "call-ref".into(),
            model: None,
            resolved: ResolvedModel {
                provider: Provider::Codex,
                model: None,
            },
            continues: None,
            parent: None,
            thread: None,
            phase: None,
            summary: "summary".into(),
            thread_id: None,
            turn_id: None,
            outcome: Some(WorkOutcome::Completed),
            result_text: "raw provider output".into(),
            failure_reason: None,
            failure: None,
            state: WorkStatus::Finished,
        };
        let value = workspace_work_summary(&work, "dag");
        assert!(value.get("prompt").is_none());
        assert!(value.get("result_text").is_none());
        assert_eq!(value["title"], "title");
        assert_eq!(value["cwd"], "/private/project");
    }
}

type BackgroundFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

enum BackgroundCommand {
    Spawn(BackgroundFuture),
    Stop,
}

/// Owns the Runtime's long-lived tasks and continuously reaps completed jobs.
struct BackgroundTasks {
    commands: std::sync::Mutex<Option<mpsc::UnboundedSender<BackgroundCommand>>>,
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl BackgroundTasks {
    fn new() -> Self {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let supervisor = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            let mut stopping = false;
            loop {
                if stopping && tasks.is_empty() {
                    break;
                }
                tokio::select! {
                    command = receiver.recv(), if !stopping => match command {
                        Some(BackgroundCommand::Spawn(task)) => { tasks.spawn(task); }
                        Some(BackgroundCommand::Stop) | None => {
                            stopping = true;
                            tasks.abort_all();
                        }
                    },
                    result = tasks.join_next(), if !tasks.is_empty() => {
                        if let Some(Err(error)) = result && !error.is_cancelled() {
                            tracing::warn!(event = "runtime.background_task", %error, "background task failed");
                        }
                    },
                }
            }
        });
        Self {
            commands: std::sync::Mutex::new(Some(sender)),
            supervisor: Mutex::new(Some(supervisor)),
        }
    }

    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) -> Result<()> {
        let commands = self
            .commands
            .lock()
            .map_err(|_| anyhow::anyhow!("background task sender lock poisoned"))?;
        let sender = commands
            .as_ref()
            .context("Node background tasks are shutting down")?;
        sender
            .send(BackgroundCommand::Spawn(Box::pin(future)))
            .map_err(|_| anyhow::anyhow!("Node background task supervisor stopped"))
    }

    async fn shutdown(&self) {
        if let Some(sender) = self
            .commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            && sender.send(BackgroundCommand::Stop).is_err()
        {
            tracing::warn!(
                event = "runtime.background_task",
                "background task supervisor stopped before shutdown"
            );
        }
        if let Some(supervisor) = self.supervisor.lock().await.take()
            && let Err(error) = supervisor.await
            && !error.is_cancelled()
        {
            tracing::warn!(event = "runtime.background_task", %error, "background task supervisor failed during shutdown");
        }
    }
}

/// What changed in a conversation, so each client session can push the affected records.
pub(crate) enum Changed<'a> {
    /// A stored event, by hash.
    Event(&'a str),
    /// A work row, or this Node's dispatch and stop requests for it.
    Work(&'a str),
    /// This Node's scheduler job for the trigger, and its reply once written.
    Job(&'a str),
    /// Clients re-read the conversation.
    Reload,
}

#[derive(Clone)]
pub struct Update {
    pub network_id: String,
    pub payload: Value,
}
pub struct Runtime {
    pub config: Config,
    pub delivery: crate::delivery::Delivery,
    pub(crate) data_dir: PathBuf,
    settings: std::sync::Mutex<NodeSettings>,
    providers: Arc<std::sync::Mutex<crate::providers::Local>>,
    /// Where this computer keeps what the providers need, as the Node's environment says.
    pub(crate) host: crate::discovery::Host,
    /// Provider paths a bot proposed, by the question that asks the user; kept until restart.
    pub(crate) provider_proposals: Mutex<HashMap<String, settings::Proposal>>,
    provider_probes: std::sync::Mutex<BTreeMap<Provider, provider_probe::CachedProviderProbe>>,
    provider_probe_locks: BTreeMap<Provider, Mutex<()>>,
    pub(crate) pending_provider_launches: std::sync::atomic::AtomicBool,
    pub(crate) config_save: Mutex<()>,
    pub identity: LocalIdentity,
    pub store: StoreActor,
    net: std::sync::OnceLock<Arc<crate::net::Net>>,
    router: Mutex<Option<iroh::protocol::Router>>,
    relay: Mutex<Option<crate::net::RelayServer>>,
    volunteer: relay::Volunteer,
    /// Runtime-owned workers, cancelled and joined by final shutdown.
    background_tasks: BackgroundTasks,
    sync_peers: std::sync::Mutex<HashMap<String, peers::PeerSync>>,
    peer_delivery_locks: peers::PeerDeliveryLocks,
    removed_sync_limits: peers::RemovedSyncLimits,
    /// Two syncs downloading the same attachment would share one staging file.
    /// ponytail: one download at a time; per-attachment locks if parallel downloads matter.
    blob_fetch: Mutex<()>,
    /// Changes with every process start, so peers can tell a restart from a quiet link.
    boot: String,
    sync_wakeup: tokio::sync::Notify,
    have_wakeup: Arc<tokio::sync::Notify>,
    outbox_wakeup: tokio::sync::Notify,
    pub events: broadcast::Sender<Update>,
    pub agent_tokens: Mutex<HashMap<String, Access>>,
    pub(crate) manager_controls: Mutex<HashMap<String, Arc<manager_control::ActiveManager>>>,
    #[cfg(test)]
    pub(crate) claim_to_start_barrier:
        std::sync::Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
    schedule_workers: Mutex<BTreeSet<(String, crate::scheduling::SessionKey)>>,
    steer_workers: Mutex<BTreeSet<(String, crate::scheduling::SessionKey)>>,
    steer_read_workers: Mutex<BTreeSet<(String, String, String)>>,
    steer_read_slots: Arc<tokio::sync::Semaphore>,
    work_input_locks: Mutex<HashMap<String, std::sync::Weak<tokio::sync::RwLock<()>>>>,
    pub(super) local_permission_epoch: Arc<std::sync::atomic::AtomicU64>,
    schedule_wakeup: tokio::sync::Notify,
    observation: std::sync::Mutex<observation::ObservationState>,
    pub tool_url: std::sync::OnceLock<String>,
    /// The local Cursor ACP controller, created with its scoped tool listener.
    pub(crate) cursor: Arc<std::sync::OnceLock<Arc<vibeface_bridge::agent::CursorAgent>>>,
    cursor_executor: cursor::CursorExecutor,
    pub maintenance: Arc<Gate>,
    pub(crate) codex: Arc<codex_server::CodexServer>,
    pub(crate) claude: claude::Claude,
    pub(crate) pi: Arc<pi::Pi>,
    pub(crate) opencode: Arc<opencode::Opencode>,
    progress_sequence: std::sync::atomic::AtomicU64,
    /// Attempts made and the next one's time for each held stop. Only pacing lives here: the
    /// stop and its deadline are stored, so a restart retries at once.
    stop_retries: std::sync::Mutex<HashMap<String, (u32, i64)>>,
    /// When reconciliation last read every running or unverified work's turn.
    live_probe_at: std::sync::atomic::AtomicI64,
    pub local_access: Arc<crate::local_access::LocalAccess>,
    /// Browser sessions this Node forwards to other devices, and those waiting.
    pub(crate) forwards: crate::client::Forwards,
    pub(crate) write_state: tokio::sync::watch::Sender<WriteState>,
    /// Bumped on every membership change, so open sessions and connections recheck their peer.
    pub(crate) membership: tokio::sync::watch::Sender<u64>,
    client_authority: tokio::sync::watch::Sender<ClientMembership>,
    codex_rx: Mutex<Option<vibeface_bridge::codex::EventReceiver>>,
    pub(crate) triage: triage::TriageState,
    pub(crate) node_join: std::sync::Mutex<Option<crate::client::NodeJoinTask>>,
    rejoin: tokio::sync::Notify,
    /// Budget of records browsers and local processes may add to the log.
    pub(crate) log_intake: crate::logging::RateLimit,
}
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
/// These display requests may use committed WAL snapshots; mutation receipts and controls cannot.
pub(crate) fn snapshot_rpc(method: &str) -> bool {
    matches!(
        method,
        "conversations"
            | "conversation_history"
            | "open_works"
            | "workspace_overview"
            | "workspace_state"
            | "workspace_history"
            | "notifications"
    )
}

fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    let s = v[key]
        .as_str()
        .with_context(|| format!("{key} is required"))?;
    ensure!(!s.trim().is_empty() && s.len() <= 65536, "invalid {key}");
    Ok(s)
}
/// The client's signature over the event body this write produces; checked when the event is signed.
fn attestation(v: &Value) -> Result<Attestation> {
    signed(v, "attestation")
}
/// The Node the user chose to schedule a new conversation, with their signature over it.
fn first_owner(v: &Value) -> Result<FirstOwner> {
    Ok(FirstOwner {
        scheduler_node: id(v, "scheduler_node")?,
        attestation: signed(v, "owner_attestation")?,
    })
}
fn signed(v: &Value, key: &str) -> Result<Attestation> {
    serde_json::from_value(v[key].clone())
        .context("this write needs the signature of your device; reload the page and retry")
}
fn id(v: &Value, key: &str) -> Result<String> {
    let s = field(v, key)?;
    ensure!(valid_business_id(s), "invalid {key}");
    Ok(s.into())
}
impl Runtime {
    pub async fn open(dir: &Path) -> Result<Self> {
        let epoch: Value = serde_json::from_slice(&std::fs::read(dir.join("data-epoch.json"))?)?;
        ensure!(
            epoch == json!({"data_epoch":crate::config::DATA_EPOCH}),
            "this Node requires an initialized data epoch {} directory",
            crate::config::DATA_EPOCH
        );
        let process_lock = Arc::new(config::lock_data_dir(dir)?);
        let host = crate::discovery::Host::current();
        crate::discovery::complete_saved(dir, &host)?;
        let mut config = Config::load(dir)?;
        let provider_roots = ProviderRoots::new(dir);
        for directory in [
            provider_roots.codex_home.as_path(),
            provider_roots.codex_sessions.as_path(),
            provider_roots.claude_config.as_path(),
            provider_roots.claude_projects.as_path(),
            provider_roots.pi_sessions.as_path(),
            provider_roots.opencode.as_path(),
        ] {
            config::private_dir(directory)?;
        }
        let forwards = crate::client::Forwards::new(&config.forwarding);
        let identity = config::identity(dir)?;
        let have_wakeup = Arc::new(tokio::sync::Notify::new());
        let mut store = StoreActor::open(dir.join("node.sqlite3"), process_lock, |store| {
            store.recover_triage()?;
            Ok(())
        })
        .await?;
        let membership = tokio::sync::watch::Sender::new(0);
        let client_authority =
            tokio::sync::watch::Sender::new((0, ClientAuthorityState::Initializing));
        store
            .publish_membership_to(
                client_authority.clone(),
                membership.clone(),
                have_wakeup.clone(),
            )
            .await?;
        let (events, _) = broadcast::channel(256);
        let (tx, rx) = vibeface_bridge::codex::event_channel();
        vibeface_bridge::agent::CursorAgent::kill_orphan(&dir.join(cursor::PIDFILE));
        let mut local = crate::providers::Local {
            switches: config.providers.clone(),
            bins: config.bins.clone(),
            env: config.provider_env.clone(),
        };
        let codex_agent = config
            .manager
            .as_ref()
            .is_some_and(|manager| manager.provider.on_app_server());
        provider_roots.apply(&mut local)?;
        let mut codex_config = if config.codex_config.is_some() {
            config.codex_config.clone()
        } else if local.launches(Provider::Codex) && local.switches.app_server_used(codex_agent) {
            match provider_roots.prepare_codex_config(&host) {
                Ok(projected) => projected,
                Err(error) if error.is::<crate::discovery::LocalSourceError>() => {
                    tracing::warn!(
                        event = "providers.local_source_failed",
                        "could not read local AI service settings; configure the service manually"
                    );
                    None
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        for provider in [Provider::Codex, Provider::Claude, Provider::Cursor] {
            let enabled = if provider == Provider::Codex {
                local.switches.app_server_used(codex_agent)
            } else {
                config
                    .manager
                    .as_ref()
                    .is_some_and(|manager| manager.provider == provider)
                    || local.switches.execution.contains(&provider)
            };
            if !enabled {
                continue;
            }
            match crate::discovery::complete_local(
                &mut local,
                &crate::discovery::LocalConnect {
                    provider,
                    explicit_env_names: BTreeSet::new(),
                },
                codex_config.as_deref(),
                &host,
            )
            .await
            {
                Ok(_) => {}
                Err(error) if error.is::<crate::discovery::LocalSourceError>() => {
                    if provider == Provider::Codex {
                        codex_config = config.codex_config.clone();
                    }
                    tracing::warn!(
                        event = "providers.local_source_failed",
                        provider = provider.name(),
                        "could not read local AI service settings; configure the service manually"
                    );
                }
                Err(error) => return Err(error),
            }
        }
        if local.env != config.provider_env
            || local.bins != config.bins
            || codex_config != config.codex_config
        {
            config.provider_env = local.env.clone();
            config.bins = local.bins.clone();
            config.codex_config = codex_config.clone();
            config.save(dir)?;
        }
        if let Some(projected) = codex_config {
            provider_roots.publish_codex_config(&projected)?;
        }
        {
            let provider = Provider::Codex;
            let enabled = local.switches.execution.contains(&provider)
                || config
                    .manager
                    .as_ref()
                    .is_some_and(|manager| manager.provider == provider);
            if enabled && let Err(error) = provider_roots.prepare_login(provider, &local, &host) {
                if !error.is::<crate::discovery::LocalSourceError>() {
                    return Err(error);
                }
                tracing::warn!(
                    event = "providers.local_login_unavailable",
                    provider = provider.name(),
                    "could not reuse the local CLI login; sign in for this Node"
                );
            }
        }
        if !config.triage.scan_codex_sessions {
            config.triage.codex_sessions = Some(provider_roots.codex_sessions.clone());
        }
        let providers = Arc::new(std::sync::Mutex::new(local));
        let local = providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        local.validate(codex_agent)?;
        let claude_env = local.env.get(Provider::Claude).clone();
        let claude = claude::Claude::open(dir, &claude_env).await;
        let codex_target = local.codex_target(codex_agent);
        let codex = Arc::new(codex_server::CodexServer::open(
            codex_target,
            &identity,
            dir,
            tx,
        )?);
        let opencode = Arc::new(opencode::Opencode::open(
            dir,
            local.bin(Provider::Opencode),
            local.launch_env(Provider::Opencode),
        )?);
        let local_access = Arc::new(crate::local_access::LocalAccess::open(
            dir,
            config.local_auto_access,
        )?);
        let triage = triage::TriageState::new(config.triage.clone());
        let delivery = crate::delivery::Delivery::open(dir, identity.clone())?;
        let cursor = Arc::new(std::sync::OnceLock::new());
        let cursor_executor = cursor::CursorExecutor::new(
            cursor.clone(),
            providers.clone(),
            dir.to_owned(),
            config.timeout_secs,
        )?;
        let volunteer = relay::Volunteer::new(&config.network, dir);
        let node = Self {
            delivery,
            data_dir: dir.to_owned(),
            settings: std::sync::Mutex::new(NodeSettings {
                roots: config.roots.clone(),
                manager: config.manager.clone(),
            }),
            providers: providers.clone(),
            host,
            provider_proposals: Mutex::new(HashMap::new()),
            provider_probes: std::sync::Mutex::new(BTreeMap::new()),
            provider_probe_locks: Provider::ALL
                .into_iter()
                .map(|provider| (provider, Mutex::new(())))
                .collect(),
            pending_provider_launches: std::sync::atomic::AtomicBool::new(false),
            config_save: Mutex::new(()),
            config,
            identity,
            store,
            net: std::sync::OnceLock::new(),
            router: Mutex::new(None),
            relay: Mutex::new(None),
            volunteer,
            background_tasks: BackgroundTasks::new(),
            sync_peers: std::sync::Mutex::new(HashMap::new()),
            peer_delivery_locks: peers::PeerDeliveryLocks::default(),
            removed_sync_limits: peers::RemovedSyncLimits::default(),
            blob_fetch: Mutex::new(()),
            boot: uuid::Uuid::new_v4().simple().to_string(),
            sync_wakeup: tokio::sync::Notify::new(),
            have_wakeup,
            outbox_wakeup: tokio::sync::Notify::new(),
            events,
            agent_tokens: Mutex::new(HashMap::new()),
            manager_controls: Mutex::new(HashMap::new()),
            #[cfg(test)]
            claim_to_start_barrier: std::sync::Mutex::new(None),
            schedule_workers: Mutex::new(BTreeSet::new()),
            steer_workers: Mutex::new(BTreeSet::new()),
            steer_read_workers: Mutex::new(BTreeSet::new()),
            steer_read_slots: Arc::new(tokio::sync::Semaphore::new(8)),
            work_input_locks: Mutex::new(HashMap::new()),
            local_permission_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            schedule_wakeup: tokio::sync::Notify::new(),
            observation: std::sync::Mutex::new(Default::default()),
            tool_url: std::sync::OnceLock::new(),
            cursor,
            cursor_executor,
            maintenance: Arc::new(Gate::new(dir.join("update-pending.json").try_exists()?)),
            codex,
            claude,
            pi: Arc::new(pi::Pi::new(dir).await),
            opencode,
            progress_sequence: std::sync::atomic::AtomicU64::new(0),
            stop_retries: std::sync::Mutex::new(HashMap::new()),
            live_probe_at: std::sync::atomic::AtomicI64::new(now_ms()),
            local_access,
            forwards,
            write_state: tokio::sync::watch::Sender::new(WriteState::Writable),
            membership,
            client_authority,
            codex_rx: Mutex::new(Some(rx)),
            triage,
            node_join: std::sync::Mutex::new(None),
            rejoin: tokio::sync::Notify::new(),
            log_intake: Default::default(),
        };
        node.start_authority_projection_timer()?;
        for token in node.relay_tokens() {
            crate::logging::add_credential(token);
        }
        Ok(node)
    }
    /// Opens a data directory this process has just closed. On Linux a child that another test
    /// thread is spawning keeps a copy of every open descriptor until its exec closes them, so the
    /// released lock can stay held for a moment.
    #[cfg(test)]
    pub(crate) async fn reopen(dir: &Path) -> Result<Self> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match Self::open(dir).await {
                Err(error)
                    if matches!(
                        error.downcast_ref(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) && Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                result => return result,
            }
        }
    }
    pub async fn install_membership(&self, incoming: NetworkState) -> Result<AuthorizationView> {
        let _work = self.maintenance.enter()?;
        let (view, rebuilt, changed, network) = self
            .store
            .call("install_membership", move |store| {
                let previous = if store.has_network()? {
                    Some(store.public_state()?)
                } else {
                    None
                };
                let state = match &previous {
                    Some(previous) => previous.merge(&incoming)?,
                    None => incoming,
                };
                // Clients read the device list on a change, so a join, removal or permission change
                // shows on every open page without a reload.
                let changed = previous.as_ref() != Some(&state);
                let network = state.genesis.network_id.clone();
                let (view, rebuilt) = store.install_state(state)?;
                if rebuilt {
                    store.settle_local_works(&network)?;
                }
                Ok((view, rebuilt, changed, network))
            })
            .await?;
        if rebuilt || changed {
            self.publish_reread(&network);
        }
        self.forget_revoked(&view).await;
        self.align(&network).await;
        self.wake_sync_all();
        Ok(view)
    }

    /// Asks the serving process to set this removed Node's data aside and start fresh. The short
    /// wait lets the reply reach the local page first.
    pub(crate) fn request_rejoin(self: &Arc<Self>) {
        let node = self.clone();
        if let Err(error) = self.background_tasks.spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            node.rejoin.notify_one();
        }) {
            tracing::debug!(%error, "rejoin request skipped while the Node is shutting down");
        }
    }
    pub async fn rejoin_requested(&self) {
        self.rejoin.notified().await
    }

    /// A commit fences authority until its complete history is ready to inspect.
    pub(crate) fn client_membership(&self) -> ClientMembership {
        self.client_authority.borrow().clone()
    }

    pub(crate) fn subscribe_client_membership(
        &self,
    ) -> tokio::sync::watch::Receiver<ClientMembership> {
        self.client_authority.subscribe()
    }

    /// For request workers only; the transport pump observes the state without awaiting it.
    pub(crate) async fn ready_client_membership(&self) -> Result<(u64, Option<Arc<NetworkState>>)> {
        let mut authority = self.subscribe_client_membership();
        loop {
            let (generation, state) = authority.borrow_and_update().clone();
            match state {
                ClientAuthorityState::Ready(state) => return Ok((generation, state)),
                ClientAuthorityState::Initializing | ClientAuthorityState::Updating => {
                    authority
                        .changed()
                        .await
                        .context("client authority channel closed")?;
                }
                ClientAuthorityState::Unavailable => bail!("database authority is unavailable"),
            }
        }
    }

    pub async fn has_membership(&self) -> Result<bool> {
        self.store
            .call("has_network", |store| store.has_network())
            .await
    }

    pub async fn membership_state(&self) -> Result<NetworkState> {
        self.store
            .call("public_state", |store| store.public_state())
            .await
    }
    pub async fn recovery_since_ms(&self, recovery_key: &str) -> Result<Option<i64>> {
        {
            let recovery_key = recovery_key.to_owned();
            self.store
                .call("recovery_since", move |store| {
                    store.recovery_since_ms(&recovery_key)
                })
                .await
        }
    }
    pub fn info(&self) -> Value {
        json!({"device_id":self.identity.device_id().ok(),"version":env!("CARGO_PKG_VERSION")})
    }
    /// The relay tokens in this Node's configuration, kept out of logs and diagnostics.
    pub(crate) fn relay_tokens(&self) -> impl Iterator<Item = &String> {
        self.config
            .network
            .relays
            .iter()
            .filter_map(|relay| relay.token.as_ref())
            .chain(&self.config.network.relay_operator_token)
    }
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        let events = self
            .codex_rx
            .lock()
            .await
            .take()
            .context("runtime already started")?;
        self.start_recovery_timer();
        if self.align_on_start().await? {
            self.start_writers(events);
        } else {
            let node = self.clone();
            self.background_tasks.spawn(async move {
                let mut state = node.write_state.subscribe();
                if state
                    .wait_for(|state| *state == WriteState::Writable)
                    .await
                    .is_ok()
                {
                    node.start_writers(events);
                    if let Ok(state) = node.membership_state().await {
                        node.unverify_revoked_works(&state.genesis.network_id).await;
                    }
                }
            })?;
        }
        self.start_network().await?;
        tracing::info!(
            event = "node.started",
            version = env!("CARGO_PKG_VERSION"),
            data_epoch = crate::config::DATA_EPOCH,
            os = std::env::consts::OS,
            arch = std::env::consts::ARCH,
            web = self.config.web_enabled,
            "Node started"
        );
        Ok(())
    }
    /// Time can change the projected directory before the writer is free to rebuild its rows.
    /// Wake observers from committed history without delaying that permission check on SQLite.
    fn start_authority_projection_timer(&self) -> Result<()> {
        let mut authority = self.client_authority.subscribe();
        let membership = self.membership.clone();
        self.background_tasks.spawn(async move {
            let mut effective = None;
            loop {
                let state = match authority.borrow_and_update().1.clone() {
                    ClientAuthorityState::Ready(state) => state,
                    ClientAuthorityState::Initializing
                    | ClientAuthorityState::Updating
                    | ClientAuthorityState::Unavailable => None,
                };
                let due = match state.map(|state| state.project()).transpose() {
                    Ok(Some(view)) => {
                        let projected = (view.epoch, view.recovery_key);
                        if effective.as_ref() != Some(&projected) {
                            effective = Some(projected);
                            membership.send_modify(|_| {});
                        }
                        view.pending.map(|pending| pending.effective_at_ms)
                    }
                    Ok(None) => {
                        effective = None;
                        None
                    }
                    Err(error) => {
                        tracing::warn!(%error, "committed client authority could not be projected");
                        membership.send_modify(|_| {});
                        None
                    }
                };
                let wait = async {
                    match due {
                        Some(due) => {
                            let left = due.saturating_sub(now_ms() as u64).min(15 * 60_000);
                            tokio::time::sleep(Duration::from_millis(left)).await;
                        }
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    _ = wait => {}
                    change = authority.changed() => if change.is_err() { return; },
                }
            }
        })
    }

    /// A recovery takes effect with time alone and no new record, so this installs the history
    /// again whenever another directory is in effect: at start, after each membership change, and
    /// when a waiting recovery is due. It wakes at least every 15 minutes and checks the wall
    /// clock again, since a monotonic sleep stops while the computer sleeps.
    fn start_recovery_timer(self: &Arc<Self>) {
        let node = Arc::downgrade(self);
        let mut membership = self.membership.subscribe();
        if let Err(error) = self.background_tasks.spawn(async move {
            loop {
                let Some(runtime) = node.upgrade() else {
                    return;
                };
                let due = runtime.settle_recovery().await.unwrap_or_else(|error| {
                    tracing::warn!(event = "recovery.install_pending", %error, "installing the directory in effect is pending");
                    Some(now_ms() as u64 + 60_000)
                });
                drop(runtime);
                let wait = async {
                    match due {
                        Some(due) => {
                            let left = due.saturating_sub(now_ms() as u64).min(15 * 60_000);
                            tokio::time::sleep(Duration::from_millis(left)).await
                        }
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    _ = wait => {}
                    changed = membership.changed() => if changed.is_err() { return },
                }
            }
        }) {
            tracing::debug!(%error, "recovery timer skipped while the Node is shutting down");
        }
    }
    /// Returns when the waiting recovery is due.
    async fn settle_recovery(&self) -> Result<Option<u64>> {
        if !self.has_membership().await? {
            return Ok(None);
        }
        let state = self.membership_state().await?;
        let view = state.project()?;
        if self.recovery_since_ms(&view.recovery_key).await?.is_none() {
            self.install_membership(state).await?;
        }
        Ok(view.pending.map(|pending| pending.effective_at_ms))
    }
    /// Work, scheduling and observation append to the log, so they wait for `writable`.
    /// Later authority loss fences execution while readers and stop delivery remain alive.
    fn start_writers(self: &Arc<Self>, events: vibeface_bridge::codex::EventReceiver) {
        self.start_codex(events);
        self.start_schedule_delivery();
        self.start_observation_worker();
        self.start_triage();
    }
    pub(crate) async fn maintenance_idle(&self) -> Result<()> {
        self.store
            .call("maintenance_idle", |store| {
                store.scheduler_maintenance_idle()
            })
            .await
    }
    pub(crate) fn spawn_background(
        &self,
        task: impl Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        self.background_tasks.spawn(task)
    }
    pub async fn authorize_bearer(&self, token: &str) -> Option<Access> {
        self.agent_tokens.lock().await.get(token).cloned()
    }
    /// Stops the managed App Server, the Claude Code, pi and opencode processes and the Cursor
    /// bridge; the Node calls this on SIGTERM before exiting.
    pub async fn shutdown(&self) {
        self.delivery.shutdown().await;
        self.background_tasks.shutdown().await;
        self.stop_network().await;
        if let Ok(net) = self.net() {
            net.close().await;
        }
        self.codex.stop().await;
        self.claude.stop().await;
        self.pi.stop().await;
        self.opencode.stop().await;
        if let Some(bridge) = self.cursor.get() {
            bridge.stop().await;
        }
        if let Err(error) = self.store.shutdown().await {
            tracing::error!(%error, "database shutdown failed");
        }
        // The counts still held go out first, so the stop is the last line of this run.
        crate::logging::flush();
        tracing::info!(event = "node.stopped", "Node stopped");
    }
    pub(crate) fn writable(&self) -> bool {
        *self.write_state.borrow() == WriteState::Writable
    }
    pub async fn append(&self, network: &str, scope: &str, body: EventBody) -> Result<SignedEvent> {
        self.append_blobs(network, scope, body, vec![]).await
    }
    pub async fn append_blobs(
        &self,
        network: &str,
        scope: &str,
        body: EventBody,
        blobs: Vec<BlobRef>,
    ) -> Result<SignedEvent> {
        let current = self.membership_state().await?;
        ensure!(
            current.genesis.network_id == network,
            "wrong personal network"
        );
        let identity = self.identity.clone();
        let event_network = network.to_owned();
        let event_scope = scope.to_owned();
        let event = self
            .store
            .call("append_event", move |store| {
                store.append_event(
                    &identity,
                    &event_network,
                    &event_scope,
                    body,
                    blobs,
                    now_ms(),
                )
            })
            .await?;
        self.publish_changed(network, scope, Changed::Event(&event.hash));
        Ok(event)
    }
    /// Clients reread everything: which events count changed, the membership changed, or a member
    /// Node came or went.
    pub(crate) fn publish_reread(&self, network: &str) {
        let _ = self.events.send(Update {
            network_id: network.into(),
            payload: json!({"type":"changed"}),
        });
    }
    pub(crate) fn publish_changed(&self, network: &str, scope: &str, change: Changed) {
        let mut payload = json!({"type":"changed","conversation_id":scope});
        match change {
            Changed::Event(hash) => payload["event_hash"] = json!(hash),
            Changed::Work(work_id) => payload["work_id"] = json!(work_id),
            Changed::Job(trigger) => {
                payload["trigger"] = json!(trigger);
                self.hint_job_changed(network, scope);
            }
            Changed::Reload => {}
        }
        let _ = self.events.send(Update {
            network_id: network.into(),
            payload,
        });
    }

    /// A bounded, human-only projection of the facts this Node has locally.
    async fn workspace_overview(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(
            access.human,
            "workspace overview is unavailable to Agent tokens"
        );
        ensure!(
            input.get("cursor").is_none_or(Value::is_null),
            "workspace overview cursor is not supported"
        );
        let limit = workspace_limit(input)?;
        let include_finished = workspace_bool(input, "include_finished", false)?;
        let include_sessions = workspace_bool(input, "include_sessions", true)?;
        let requested_scope = input.get("conversation_id").and_then(Value::as_str);
        if let Some(allowed_scope) = access.conversation_id.as_deref() {
            ensure!(
                requested_scope.is_none_or(|scope| scope == allowed_scope),
                "outside this tool's conversation"
            );
        }
        let scope_filter = access.conversation_id.as_deref().or(requested_scope);
        let network = &access.network_id;
        let local = self.identity.device_id()?;

        let query_network = network.clone();
        let query_scope = scope_filter.map(str::to_owned);
        let projection = self.scheduler_projection()?;
        let (
            owner_epoch,
            active_bots,
            conversations,
            shown_works,
            omitted_finished,
            dispatches,
            open_refusals,
            stops,
            questions,
            checks,
            projected_jobs,
            sessions,
            heads,
        ) = self
            .read_display(access, input, "workspace_overview", move |store| {
                let network = &query_network;
                let owner_epoch = store.public_state()?.project()?.epoch;
                let scope_filter = query_scope.as_deref();
                let active_bots = store.bots(network)?;
                let active_scopes: BTreeSet<_> = active_bots
                    .iter()
                    .map(|bot| bot.conversation_id.as_str())
                    .collect();
                let conversations = store
                    .conversations(network)?
                    .into_iter()
                    .filter(|conversation| {
                        active_scopes.contains(conversation.conversation_id.as_str())
                            && scope_filter
                                .is_none_or(|scope| scope == conversation.conversation_id)
                    })
                    .collect::<Vec<_>>();
                let scope_ids: BTreeSet<_> = conversations
                    .iter()
                    .map(|conversation| conversation.conversation_id.as_str())
                    .collect();
                let query_scopes = scope_ids
                    .iter()
                    .map(|scope| (*scope).to_owned())
                    .collect::<Vec<_>>();
                let mut shown_works = Vec::new();
                let mut omitted_finished = 0;
                for conversation in &conversations {
                    let scope = &conversation.conversation_id;
                    if include_finished {
                        shown_works.extend(store.works(network, scope)?);
                    } else {
                        shown_works.extend(store.open_works(network, Some(scope))?);
                        omitted_finished += store.finished_works(network, scope)?;
                    }
                }
                let dispatches = store
                    .dispatches_for_scopes(&query_scopes)?
                    .into_iter()
                    .map(|dispatch| {
                        let waiting_sync = dispatch.state == crate::work::DispatchState::Accepted
                            && store.work(network, &dispatch.spec.work_id)?.is_none();
                        Ok((dispatch, waiting_sync))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let open_refusals: BTreeSet<_> = store
                    .open_dispatches(network, None)?
                    .into_iter()
                    .map(|dispatch| dispatch.spec.work_id)
                    .collect();
                let stops = store
                    .stop_requests()?
                    .into_iter()
                    .filter(|stop| scope_ids.contains(stop.conversation.as_str()))
                    .map(|stop| {
                        let work = store.work(network, &stop.work_id)?;
                        Ok((stop, work))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let questions = store
                    .unanswered_questions(network)?
                    .into_iter()
                    .filter(|question| {
                        question["conversation_id"]
                            .as_str()
                            .is_some_and(|scope| scope_ids.contains(scope))
                    })
                    .collect::<Vec<_>>();
                let checks = store
                    .pending_checks()?
                    .into_iter()
                    .filter(|check| scope_ids.contains(check.conversation_id.as_str()))
                    .collect::<Vec<_>>();
                let projected_jobs = store
                    .scheduler_jobs_for_scopes(&query_scopes, include_finished)?
                    .into_iter()
                    .map(|job| {
                        let hold = projection.hold_reason(store, network, &job)?;
                        let context = store.scheduler_job_context_projection(&job)?;
                        Ok((job, hold, context))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let sessions = if include_sessions {
                    store
                        .attributed_sessions_all(network)?
                        .into_iter()
                        .filter(|session| {
                            session["conversation_id"]
                                .as_str()
                                .is_some_and(|scope| scope_ids.contains(scope))
                        })
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                let heads = store.heads(network)?;
                Ok((
                    owner_epoch,
                    active_bots,
                    conversations,
                    shown_works,
                    omitted_finished,
                    dispatches,
                    open_refusals,
                    stops,
                    questions,
                    checks,
                    projected_jobs,
                    sessions,
                    heads,
                ))
            })
            .await?;
        let scope_ids: BTreeSet<_> = conversations
            .iter()
            .map(|conversation| conversation.conversation_id.as_str())
            .collect();
        let conversation_by_id: BTreeMap<_, _> = conversations
            .iter()
            .map(|conversation| (conversation.conversation_id.as_str(), conversation))
            .collect();
        let mut tasks = shown_works
            .iter()
            .map(|work| workspace_work_summary(work, "dag"))
            .collect::<Vec<_>>();

        let mut deliveries = Vec::new();
        for (dispatch, waiting_sync) in &dispatches {
            let waiting_sync = *waiting_sync;
            let spec = &dispatch.spec;
            if !include_finished
                && matches!(dispatch.state, crate::work::DispatchState::Rejected { .. })
                && !open_refusals.contains(&spec.work_id)
            {
                continue;
            }
            let state = dispatch.state.name();
            if waiting_sync || dispatch.state != crate::work::DispatchState::Accepted {
                let unknown_reason = if waiting_sync {
                    Some("accepted_waiting_sync")
                } else if matches!(
                    dispatch.state,
                    crate::work::DispatchState::Unverified { .. }
                ) {
                    Some("dispatch_unverified")
                } else {
                    None
                };
                let mut task = json!({
                    "work_id": spec.work_id,
                    "conversation_id": spec.conversation,
                    "trigger": spec.trigger,
                    "title": workspace_bounded(&spec.title, 256),
                    "cwd": workspace_bounded(&spec.cwd, 512),
                    "summary": Value::Null,
                    "state": if waiting_sync { "accepted_waiting_sync" } else { state },
                    "outcome": Value::Null,
                    "kind": spec.kind,
                    "target_node": spec.target,
                    "dispatcher": spec.dispatcher,
                    "parent": spec.parent,
                    "continues": spec.continues,
                    "thread": spec.thread,
                    "source": "local_dispatch",
                    "unknown_reason": unknown_reason,
                });
                if let Some(reason) = dispatch.state.reason() {
                    task["failure_reason"] = json!(workspace_bounded(reason, 512));
                }
                if let Some(failure) = dispatch.state.failure() {
                    task["failure"] = json!(failure);
                }
                tasks.push(task);
                deliveries.push(json!({
                    "work_id": spec.work_id,
                    "conversation_id": spec.conversation,
                    "target_node": spec.target,
                    "state": if waiting_sync { "dispatch_accepted" } else { state },
                    "kind": spec.kind,
                    "title": workspace_bounded(&spec.title, 256),
                    "cwd": workspace_bounded(&spec.cwd, 512),
                    "reason": dispatch.state.reason().map(|reason| workspace_bounded(reason, 512)),
                    "failure": dispatch.state.failure(),
                }));
            }
        }
        for (stop, work) in stops {
            let mut delivery = json!({
                "work_id": stop.work_id,
                "conversation_id": stop.conversation,
                "target_node": stop.target,
                "state": if stop.timed_out { "stop_timed_out" } else { "stop_waiting" },
                "requested_at_ms": stop.requested_at_ms,
                "deadline_ms": stop.deadline_ms,
            });
            if let Some(work) = work {
                delivery["cwd"] = json!(workspace_bounded(&work.cwd, 512));
            }
            deliveries.push(delivery);
        }

        let questions = questions
            .iter()
            .map(workspace_question_summary)
            .collect::<Vec<_>>();
        let pending_jobs: BTreeSet<_> = checks
            .iter()
            .map(|check| format!("{}\n{}", check.conversation_id, check.trigger))
            .collect();
        let jobs = projected_jobs
            .into_iter()
            .map(|(job, hold, context)| {
                let epoch = conversation_by_id
                    .get(job.conversation_id.as_str())
                    .map_or(0, |conversation| conversation.owner_epoch);
                let mut value = workspace_job_summary(&job, &local, epoch, &pending_jobs);
                value["hold_reason"] = json!(hold);
                value["context"] = json!(context);
                value
            })
            .collect::<Vec<_>>();
        for task in &mut tasks {
            let Some(work_id) = task["work_id"].as_str() else {
                continue;
            };
            let trigger = crate::scheduling::task_trigger(work_id);
            task["coordination_job"] = jobs
                .iter()
                .find(|job| job["trigger"].as_str() == Some(trigger.as_str()))
                .cloned()
                .unwrap_or(Value::Null);
        }
        let sessions = sessions
            .iter()
            .map(workspace_session_summary)
            .collect::<Vec<_>>();
        let bots = active_bots
            .into_iter()
            .filter(|bot| {
                scope_ids.contains(bot.conversation_id.as_str())
                    && scope_filter.is_none_or(|scope| scope == bot.conversation_id)
            })
            .map(|bot| {
                json!({
                    "bot_id": bot.bot_id,
                    "conversation_id": bot.conversation_id,
                    "name": bot.name,
                })
            })
            .collect::<Vec<_>>();
        let heads = heads
            .into_iter()
            .map(|(device, (sequence, _))| (device, sequence))
            .collect::<BTreeMap<_, _>>();
        let mut remote_owners = Vec::new();
        for conversation in &conversations {
            match conversation.owner.as_deref() {
                Some(owner) if owner != local => remote_owners.push(json!({
                    "owner": owner,
                    "reachable": Value::Null,
                    "status": "unknown",
                    "owner_epoch": conversation.owner_epoch,
                    "reason": "remote_scheduler_not_queried",
                })),
                None => remote_owners.push(json!({
                    "owner": Value::Null,
                    "reachable": Value::Null,
                    "status": "unknown",
                    "owner_epoch": conversation.owner_epoch,
                    "reason": "owner_unknown",
                })),
                _ => {}
            }
        }
        let conversation_rows = conversations
            .iter()
            .map(|conversation| {
                json!({
                    "conversation_id": conversation.conversation_id,
                    "title": workspace_bounded(&conversation.title, 256),
                    "owner": conversation.owner,
                    "owner_epoch": conversation.owner_epoch,
                })
            })
            .collect::<Vec<_>>();

        let mut relations = Vec::new();
        for work in &shown_works {
            if !include_finished
                && work.outcome.is_some()
                && work.outcome != Some(WorkOutcome::Unverified)
            {
                continue;
            }
            if let Some(parent) = &work.parent {
                relations.push(json!({"from_work":work.work_id,"to_work":parent,"kind":"parent"}));
                relations
                    .push(json!({"from_work":parent,"to_work":work.work_id,"kind":"dispatches"}));
            }
            if let Some(continues) = &work.continues {
                relations
                    .push(json!({"from_work":work.work_id,"to_work":continues,"kind":"continues"}));
            }
        }

        let omitted_tasks = tasks.len().saturating_sub(limit);
        let omitted_questions = questions.len().saturating_sub(limit);
        let omitted_jobs = jobs.len().saturating_sub(limit);
        let omitted_sessions = sessions.len().saturating_sub(limit);
        let omitted_deliveries = deliveries.len().saturating_sub(limit);
        let omitted_relations = relations.len().saturating_sub(limit);
        let truncated = omitted_tasks > 0
            || omitted_questions > 0
            || omitted_jobs > 0
            || omitted_sessions > 0
            || omitted_deliveries > 0
            || omitted_relations > 0
            || bots.len() > limit
            || conversation_rows.len() > limit
            || remote_owners.len() > limit
            || checks.len() > limit;
        tasks.truncate(limit);
        let mut questions = questions;
        questions.truncate(limit);
        let mut jobs = jobs;
        jobs.truncate(limit);
        let mut sessions = sessions;
        sessions.truncate(limit);
        deliveries.truncate(limit);
        relations.truncate(limit);
        let mut bots = bots;
        bots.truncate(limit);
        let mut conversation_rows = conversation_rows;
        conversation_rows.truncate(limit);
        remote_owners.truncate(limit);
        let omitted_checks = checks.len().saturating_sub(limit);
        let mut checks = checks
            .into_iter()
            .map(|check| {
                json!({
                    "check_id": check.check_id,
                    "conversation_id": check.conversation_id,
                    "trigger": check.trigger,
                    "created_at_ms": check.created_at_ms,
                    "unknown_reason": "process_unconfirmed",
                    "codex_turn": check.codex_turn.map(|turn| json!({
                        "server": turn.server,
                        "thread": turn.thread,
                        "turn": turn.turn,
                    })),
                })
            })
            .collect::<Vec<_>>();
        checks.truncate(limit);
        Ok(json!({
            "schema": WORKSPACE_OVERVIEW_SCHEMA,
            "generated_at_ms": now_ms(),
            "network_epoch": owner_epoch,
            "heads": heads,
            "consistency": if remote_owners.is_empty() { "local_snapshot" } else { "partial" },
            "truncated": truncated,
            "conversations": conversation_rows,
            "bots": bots,
            "tasks": tasks,
            "questions": questions,
            "checks": checks,
            "jobs": jobs,
            "sessions": sessions,
            "deliveries": deliveries,
            "relations": relations,
            "coverage": {
                "source": "workspace_overview",
                "complete": false,
                "bounded": true,
                "local_heads": true,
                "remote_owners": remote_owners,
                "omitted_finished": omitted_finished,
                "omitted_sessions": omitted_sessions,
                "omitted_tasks": omitted_tasks,
                "omitted_questions": omitted_questions,
                "omitted_jobs": omitted_jobs,
                "omitted_checks": omitted_checks,
                "omitted_deliveries": omitted_deliveries,
                "omitted_relations": omitted_relations,
            },
        }))
    }
    /// Display authorization and rows share one committed read transaction. The transport
    /// rechecks the current principal before releasing this result to the client.
    async fn read_display<T: Send + 'static>(
        &self,
        access: &Access,
        input: &Value,
        operation: &'static str,
        read: impl FnOnce(&Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        ensure!(access.human, "display snapshot requires the user");
        let (_, authority) = self.ready_client_membership().await?;
        let authority = authority.context("personal network is not installed")?;
        let network = access.network_id.clone();
        let local = self.identity.device_id()?;
        let scope = input["conversation_id"].as_str().map(str::to_owned);
        if let (Some(allowed), Some(scope)) = (&access.conversation_id, &scope) {
            ensure!(allowed == scope, "outside this tool's conversation");
        }
        self.store
            .read(operation, move |store| {
                let committed = store.public_state()?;
                ensure!(
                    &committed == authority.as_ref(),
                    "membership changed while reading the snapshot"
                );
                ensure!(
                    committed.genesis.network_id == network,
                    "wrong personal network"
                );
                committed.project()?.node(&local)?;
                if let Some(scope) = scope {
                    ensure!(
                        store.has_conversation(&network, &scope)?,
                        "conversation not found"
                    );
                }
                read(store)
            })
            .await
    }

    async fn require_scope(&self, access: &Access, scope: &str) -> Result<()> {
        let state = self.membership_state().await?;
        ensure!(
            state.genesis.network_id == access.network_id,
            "wrong personal network"
        );
        if let Some(allowed) = &access.conversation_id {
            ensure!(allowed == scope, "outside this tool's conversation");
        }
        let network = access.network_id.clone();
        let scope = scope.to_owned();
        let local = self.identity.device_id()?;
        self.store
            .call("require_scope", move |store| {
                store.authorize_node(&network, &local)?;
                ensure!(
                    store.has_conversation(&network, &scope)?,
                    "conversation not found"
                );
                Ok(())
            })
            .await
    }
    /// The workspace roots this Node publishes and the state of each capability. A root listed
    /// here is not an execution grant: the submitting side checks execution and roots again.
    async fn capabilities(&self) -> Value {
        let settings = self.settings_snapshot();
        let manager = self.manager().map(|m| m.provider);
        let mut codex = self.codex.capability().await;
        codex["enabled"] = json!(
            self.providers()
                .switches
                .execution
                .contains(&Provider::Codex)
        );
        json!({"roots":settings.roots,"relay":self.relay_config().relay_listen.is_some(),
            "manager":manager,
            "manager_state":self.manager_state().await,
            "manager_readiness":self.manager_readiness().await,
            "manager_capabilities":manager.map(Provider::capabilities),
            "codex":codex})
    }

    pub async fn rpc(self: &Arc<Self>, access: Access, input: Value) -> Result<Value> {
        let _lease = self.manager_tool_lease(&access).await?;
        self.rpc_inner(access, input).await
    }
    async fn rpc_inner(self: &Arc<Self>, access: Access, input: Value) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        let method = field(&input, "method")?;
        if !access.human {
            ensure!(method == "nodes", "method unavailable to Agent tokens");
        }
        let snapshot = access.human && snapshot_rpc(method);
        let network = &access.network_id;
        let node_id = self.identity.device_id()?;
        if snapshot {
            let (_, current) = self.ready_client_membership().await?;
            let current = current.context("personal network is not installed")?;
            ensure!(
                current.genesis.network_id == *network,
                "wrong personal network"
            );
            current.project()?.node(&node_id)?;
        } else {
            let current = self.membership_state().await?;
            ensure!(
                current.genesis.network_id == *network,
                "wrong personal network"
            );
            let authorized_network = network.clone();
            self.store
                .call("authorize_rpc", move |store| {
                    store.authorize_node(&authorized_network, &node_id)
                })
                .await?;
        }
        let scope = input["conversation_id"].as_str().map(String::from);
        if let Some(scope) = &scope
            && !matches!(method, "create_conversation" | "create_bot")
            && !snapshot
        {
            self.require_scope(&access, scope).await?;
        }
        match method {
            "nodes" => {
                let query_network = network.clone();
                let bundle = self
                    .store
                    .call("node_membership", move |store| {
                        store.trust_bundle(&query_network)
                    })
                    .await?
                    .context("unknown network")?;
                let local = self.identity.device_id()?;
                let query_network = network.clone();
                let heads = self
                    .store
                    .call("node_heads", move |store| store.heads(&query_network))
                    .await?;
                let mut nodes = vec![];
                let view = bundle.project()?;
                let display_names = node_display_names(&bundle);
                let members: Vec<_> = view
                    .members
                    .into_iter()
                    .filter(|member| member.kind == IdentityKind::Node)
                    .collect();
                let probes = members.iter().map(|member| async {
                    if member.device_id == local {
                        return (true, self.capabilities().await);
                    }
                    // A Node the sync loop cannot reach would hold this answer for the timeout.
                    let answer = if self.sync_failing(&member.device_id) {
                        None
                    } else {
                        tokio::time::timeout(
                            Duration::from_secs(3),
                            self.peer_request(
                                network,
                                &member.device_id,
                                json!({"type":"capabilities"}),
                            ),
                        )
                        .await
                        .ok()
                        .and_then(Result::ok)
                    };
                    let live = answer.is_some();
                    (
                        live,
                        self.peer_capabilities(&member.device_id, answer).await,
                    )
                });
                let probes = futures_util::future::join_all(probes).await;
                for (member, (live, capabilities)) in members.into_iter().zip(probes) {
                    let here = member.device_id == local;
                    let (seq, hash) = match heads.get(&member.device_id) {
                        Some((seq, hash)) => (*seq, Some(hash)),
                        None => (0, None),
                    };
                    nodes.push(json!({"device_id":member.device_id,"name":member.name,"revoked":false,"local":here,"online":live,"max_seq":seq,"head_hash":hash,"capabilities":capabilities}));
                }
                for revoked in view.revoked {
                    if let Some(name) = display_names.get(&revoked.device_id) {
                        nodes.push(json!({"device_id":revoked.device_id,"name":name,"revoked":true,"local":false,"online":false,"max_seq":revoked.cutoff_seq,"head_hash":revoked.cutoff_hash,"capabilities":Value::Null}));
                    }
                }
                Ok(json!({"nodes":nodes}))
            }
            "conversations" => {
                let network = network.clone();
                let conversations = self
                    .read_display(&access, &input, "conversations", move |store| {
                        store.conversations(&network)
                    })
                    .await?;
                Ok(json!({"conversations":conversations}))
            }
            "create_conversation" => {
                ensure!(access.human, "only the user creates conversations");
                let scope = id(&input, "conversation_id")?;
                let title = field(&input, "title")?;
                ensure!(title.len() <= 512, "title too long");
                let identity = self.identity.clone();
                let query_network = network.clone();
                let query_scope = scope.clone();
                let title = title.to_owned();
                let owner = first_owner(&input)?;
                let conversation = self
                    .store
                    .call("create_conversation", move |store| {
                        store.create_conversation(
                            &identity,
                            &query_network,
                            &query_scope,
                            &title,
                            owner,
                            now_ms(),
                        )
                    })
                    .await?;
                self.publish_changed(network, &scope, Changed::Reload);
                Ok(json!(conversation))
            }
            "conversation" => self.conversation_snapshot(&access, &input).await,
            "conversation_history" => self.conversation_history(&access, &input).await,
            "open_works" => {
                ensure!(access.human, "only clients list open works");
                let network = network.clone();
                let works = self
                    .read_display(&access, &input, "open_works", move |store| {
                        chat_controls::open_works(store, &network, None)
                    })
                    .await?;
                Ok(json!({"works":works}))
            }
            "work_detail" => self.work_detail(&access, &input).await,
            "workspace_overview" => self.workspace_overview(&access, &input).await,
            "workspace_command" => self.workspace_command(&access, &input).await,
            "signed_receipt" => {
                ensure!(
                    access.human && access.conversation_id.is_none(),
                    "signed receipt requires the authenticated user"
                );
                let network = network.clone();
                let client = field(&input, "client_id")?.to_owned();
                let scope = id(&input, "scope_id")?;
                let body = input["body"].clone();
                let attestation = attestation(&input)?;
                self.store
                    .call("signed_receipt", move |store| {
                        store.client_signed_receipt(&network, &client, &scope, body, &attestation)
                    })
                    .await
            }
            "workspace_command_receipt" => {
                ensure!(
                    access.human && access.conversation_id.is_none(),
                    "workspace receipt requires the authenticated user"
                );
                ensure!(
                    input.get("cause").is_none() && input.get("acting_assignment").is_none(),
                    "receipt must identify the original user command"
                );
                let network = network.clone();
                let client = field(&input, "client_id")?.to_owned();
                let command_id = id(&input, "command_id")?;
                let command = serde_json::from_value(input["command"].clone())?;
                let attestation = attestation(&input)?;
                self.store
                    .call("workspace_command_receipt", move |store| {
                        store.client_workspace_receipt(
                            &network,
                            &client,
                            &command_id,
                            &command,
                            &attestation,
                        )
                    })
                    .await
            }
            "workspace_state" => self.workspace_state(&access, &input).await,
            "workspace_history" => self.workspace_history(&access, &input).await,
            "message" => self.accept_chat_message(&access, &input).await,
            "input_receipt" => self.input_receipt(&access, &input).await,
            "scope_context" => self.scope_context(&access, &input).await,
            "set_owner" => {
                let scope = scope.context("conversation_id is required")?;
                self.set_owner(&access, &scope, &input).await
            }
            "read_artifact" => self.read_artifact(&access, &input).await,
            "upload_attachment" => self.upload_attachment(&access, &input).await,
            "interrupt_work" => {
                ensure!(access.human, "only the user stops work");
                let scope = scope.context("conversation_id is required")?;
                self.stop_work(network, &scope, &id(&input, "work_id")?)
                    .await
            }
            "codex_state" => {
                ensure!(access.human, "only the user reaches the App Server");
                self.codex_state(network, &input).await
            }
            "bots" => self.bots(&access).await,
            "questions" => self.questions(&access).await,
            "answer" => self.answer_question(&access, &input).await,
            "create_bot" => self.create_bot(&access, &input).await,
            "delete_bot" => self.delete_bot(&access, &input).await,
            "set_primary_bot" => self.set_primary_bot(&access, &input).await,
            "set_bot_project" => self.set_bot_project(&access, &input).await,
            "set_bot_avatar" => self.set_bot_avatar(&access, &input).await,
            "control_schedule" => self.control_schedule(&access, &input).await,
            "control_memory" => self.control_memory(&access, &input).await,
            "memory_state" => self.memory_state(&access, &input).await,
            "cancel_manager" => self.cancel_manager(&access, &input).await,
            "reply_unanswered" => self.settle_unanswered(&access, &input, false).await,
            "dismiss_unanswered" => self.settle_unanswered(&access, &input, true).await,
            "resolve" => self.resolve_check(&access, &input).await,
            "notifications" => {
                let network = network.clone();
                let client = field(&input, "client_id")?.to_owned();
                let notifications = self
                    .read_display(&access, &input, "notifications", move |store| {
                        store.pending_notifications(&network, &client)
                    })
                    .await?;
                Ok(json!({"notifications":notifications}))
            }
            "dismiss_work" => {
                ensure!(access.human, "only clients close failed works");
                let scope = scope.context("conversation_id is required")?;
                let work_id = id(&input, "work_id")?;
                let query_network = network.clone();
                let query_scope = scope.clone();
                let query_work = work_id.clone();
                let failed = self
                    .store
                    .call("dismiss_work_check", move |store| {
                        if store.closed_in_inbox(&query_network, &query_work)? {
                            return Ok(None);
                        }
                        store
                            .failed_work(&query_network, &query_scope, &query_work)
                            .map(Some)
                    })
                    .await?;
                let Some(failed) = failed else {
                    return Ok(json!({"dismissed":true}));
                };
                ensure!(failed, "这个任务没有失败，不用关掉");
                self.append(
                    network,
                    &scope,
                    EventBody::NotificationAcked {
                        notification_id: work_id,
                        client_id: INBOX_CLIENT.into(),
                        attestation: Some(attestation(&input)?),
                    },
                )
                .await?;
                Ok(json!({"dismissed":true}))
            }
            "ack" => {
                ensure!(access.human, "only clients acknowledge notifications");
                let notification = id(&input, "notification_id")?;
                let client = id(&input, "client_id")?;
                let query_network = network.clone();
                let query_client = client.clone();
                let pending = self
                    .store
                    .call("pending_notifications", move |store| {
                        store.pending_notifications(&query_network, &query_client)
                    })
                    .await?;
                if let Some(n) = pending
                    .into_iter()
                    .find(|n| n.notification_id == notification)
                {
                    self.append(
                        network,
                        &n.conversation_id,
                        EventBody::NotificationAcked {
                            notification_id: notification,
                            client_id: client,
                            attestation: Some(attestation(&input)?),
                        },
                    )
                    .await?;
                }
                Ok(json!({"acknowledged":true}))
            }
            _ => bail!("unknown method"),
        }
    }
    fn allowed_path(&self, path: &str) -> Result<PathBuf> {
        let path = std::fs::canonicalize(path).context("execution path does not exist")?;
        let roots = self
            .settings_snapshot()
            .roots
            .iter()
            .map(std::fs::canonicalize)
            .collect::<Vec<_>>();
        let allowed = roots
            .iter()
            .filter_map(|root| root.as_ref().ok())
            .any(|root| path.starts_with(root));
        if !allowed {
            for root in roots {
                root.context("configured execution root cannot be resolved")?;
            }
        }
        ensure!(
            allowed,
            AdmissionRejected::new(
                "execution path is outside configured roots; ask the user to allow its folder with propose_provider_config roots"
            )
        );
        ensure!(
            std::fs::metadata(&path)?.is_dir(),
            AdmissionRejected::new("execution path must be a directory")
        );
        Ok(path)
    }
}
