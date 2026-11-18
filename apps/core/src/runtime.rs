use crate::clock::now_ms;
use crate::command::{
    BusinessCommand, BusinessInput, BusinessMethodError, BusinessRejection, ReadConsistency,
};
pub(crate) use crate::store::actor::{ClientAuthorityState, ClientMembership};
use crate::{
    config::{self, Config},
    crypto::LocalIdentity,
    identity::{AuthorizationView, GovernanceAction, IdentityKind, NetworkState},
    maintenance::Gate,
    model::*,
    providers::{Provider, ProviderRoots},
    store::{Store, StoreActor, WriteState},
};
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, broadcast, mpsc};
use vibeface_protocol::valid_business_id;

pub(crate) mod agent_tools;
mod bots;
mod chat_controls;
mod child_pipes;
pub mod claude;
pub(crate) mod codex_server;
mod cursor;
mod execution_directory;
mod executor;
mod followup;
mod input_receipts;
mod manager_control;
mod observation;
pub mod opencode;
mod overview;
mod peers;
mod permission_fences;
pub mod pi;
mod provider_adapter;
mod provider_probe;
mod push;
mod relay;
mod reply_directory;
mod schedule_delivery;
mod scheduler_history;
mod settings;
mod triage;
mod work;
mod work_input;
mod workspace;
pub(crate) use cursor::record_reply as record_cursor_reply;
pub use manager_control::RegisteredInvocation;
pub(crate) use observation::{ObservationGuard, native_progress};
pub use settings::NodeSettings;
pub use triage::{
    AGENT_ANSWER_WITHIN, HOOK_ANSWER_WITHIN, SPOOL_DIR as HOOK_SPOOL, TRIAGE_URL_ENV,
};

#[cfg(test)]
pub(crate) mod test_support;

#[derive(serde::Serialize)]
struct NodeObservation {
    checked_at_ms: i64,
    last_synced_at_ms: Option<i64>,
    path: Option<crate::net::SelectedPath>,
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
pub enum Access {
    User {
        network_id: String,
        scope: UserScope,
    },
    Agent(AgentAccess),
}

#[derive(Clone)]
pub enum UserScope {
    Network,
    Conversation(String),
}

#[derive(Clone)]
pub enum AgentAccess {
    ReadOnly {
        network_id: String,
        conversation_id: String,
    },
    Invocation(RegisteredInvocation),
}

impl Access {
    pub fn network_id(&self) -> &str {
        match self {
            Self::User { network_id, .. } => network_id,
            Self::Agent(agent) => agent.network_id(),
        }
    }

    pub fn conversation_id(&self) -> Option<&str> {
        match self {
            Self::User {
                scope: UserScope::Network,
                ..
            } => None,
            Self::User {
                scope: UserScope::Conversation(id),
                ..
            } => Some(id),
            Self::Agent(agent) => Some(agent.conversation_id()),
        }
    }

    fn is_user(&self) -> bool {
        matches!(self, Self::User { .. })
    }
}

impl AgentAccess {
    pub fn network_id(&self) -> &str {
        match self {
            Self::ReadOnly { network_id, .. } => network_id,
            Self::Invocation(invocation) => invocation.network_id(),
        }
    }

    pub fn conversation_id(&self) -> &str {
        match self {
            Self::ReadOnly {
                conversation_id, ..
            } => conversation_id,
            Self::Invocation(invocation) => invocation.conversation_id(),
        }
    }

    fn invocation(&self) -> Option<&RegisteredInvocation> {
        match self {
            Self::ReadOnly { .. } => None,
            Self::Invocation(invocation) => Some(invocation),
        }
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
    pub agent_tokens: Mutex<HashMap<String, AgentAccess>>,
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
        codex.set_user_socket(local.user_codex_socket());
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
        self.start_push_worker()?;
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
    pub async fn authorize_bearer(&self, token: &str) -> Option<AgentAccess> {
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
            access.is_user(),
            "workspace overview is unavailable to Agent tokens"
        );
        ensure!(
            input.get("cursor").is_none_or(Value::is_null),
            "workspace overview cursor is not supported"
        );
        let limit = overview::workspace_limit(input)?;
        let include_finished = overview::workspace_bool(input, "include_finished", false)?;
        let include_sessions = overview::workspace_bool(input, "include_sessions", true)?;
        let requested_scope = input.get("conversation_id").and_then(Value::as_str);
        if let Some(allowed_scope) = access.conversation_id() {
            ensure!(
                requested_scope.is_none_or(|scope| scope == allowed_scope),
                "outside this tool's conversation"
            );
        }
        let scope_filter = access.conversation_id().or(requested_scope);
        let network = access.network_id();
        let local = self.identity.device_id()?;

        let query_network = network.to_owned();
        let query_scope = scope_filter.map(str::to_owned);
        let projection = self.scheduler_projection()?;
        let snapshot = self
            .read_display(access, input, "workspace_overview", move |store| {
                store.overview_snapshot(
                    crate::store::overview::OverviewQuery {
                        network: &query_network,
                        scope: query_scope.as_deref(),
                        local: &local,
                        limit,
                        include_finished,
                        include_sessions,
                    },
                    overview::job_projector(&projection, &query_network),
                )
            })
            .await?;
        Ok(overview::summary(snapshot))
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
        ensure!(access.is_user(), "display snapshot requires the user");
        let (_, authority) = self.ready_client_membership().await?;
        let authority = authority.context("personal network is not installed")?;
        let network = access.network_id().to_owned();
        let local = self.identity.device_id()?;
        let scope = input["conversation_id"].as_str().map(str::to_owned);
        if let (Some(allowed), Some(scope)) = (access.conversation_id(), &scope) {
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
            state.genesis.network_id == access.network_id(),
            "wrong personal network"
        );
        if let Some(allowed) = access.conversation_id() {
            ensure!(allowed == scope, "outside this tool's conversation");
        }
        self.require_local_conversation(access.network_id(), scope)
            .await
    }

    async fn require_conversation(&self, network: &str, scope: &str) -> Result<()> {
        let state = self.membership_state().await?;
        ensure!(
            state.genesis.network_id == network,
            "wrong personal network"
        );
        self.require_local_conversation(network, scope).await
    }

    async fn require_local_conversation(&self, network: &str, scope: &str) -> Result<()> {
        let network = network.to_owned();
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
            "version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"arch":std::env::consts::ARCH,
            "manager":manager,
            "manager_state":self.manager_state().await,
            "manager_readiness":self.manager_readiness().await,
            "manager_capabilities":manager.map(Provider::capabilities),
            "codex":codex})
    }

    pub(crate) async fn rpc(
        self: &Arc<Self>,
        access: Access,
        input: BusinessInput,
    ) -> Result<Value> {
        let _lease = match &access {
            Access::Agent(AgentAccess::Invocation(invocation)) => {
                Some(self.manager_tool_lease(invocation).await?)
            }
            Access::User { .. } | Access::Agent(AgentAccess::ReadOnly { .. }) => None,
        };
        self.rpc_inner(access, input).await
    }
    async fn rpc_inner(self: &Arc<Self>, access: Access, command: BusinessInput) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        let command = match command {
            Ok(command) => Ok(command),
            Err(BusinessRejection::MethodField { error, .. }) => match error {
                BusinessMethodError::Required => bail!("method is required"),
                BusinessMethodError::Invalid => bail!("invalid method"),
            },
            Err(BusinessRejection::UnsupportedName { params }) => Err(params),
        };
        let input = match &command {
            Ok(command) => command.params(),
            Err(params) => params,
        }
        .value();
        if !access.is_user() {
            ensure!(
                matches!(&command, Ok(BusinessCommand::Nodes(_))),
                "method unavailable to Agent tokens"
            );
        }
        let snapshot = access.is_user()
            && command
                .as_ref()
                .is_ok_and(|command| command.consistency() == ReadConsistency::CommittedDisplay);
        let network = access.network_id();
        let node_id = self.identity.device_id()?;
        if snapshot {
            let (_, current) = self.ready_client_membership().await?;
            let current = current.context("personal network is not installed")?;
            ensure!(
                current.genesis.network_id == network,
                "wrong personal network"
            );
            current.project()?.node(&node_id)?;
        } else {
            let current = self.membership_state().await?;
            ensure!(
                current.genesis.network_id == network,
                "wrong personal network"
            );
            let authorized_network = network.to_owned();
            self.store
                .call("authorize_rpc", move |store| {
                    store.authorize_node(&authorized_network, &node_id)
                })
                .await?;
        }
        let scope = input["conversation_id"].as_str().map(String::from);
        if let Some(scope) = &scope
            && !matches!(
                &command,
                Ok(BusinessCommand::CreateConversation(_) | BusinessCommand::CreateBot(_))
            )
            && !snapshot
        {
            self.require_scope(&access, scope).await?;
        }
        match &command {
            Ok(BusinessCommand::Nodes(_)) => {
                let query_network = network.to_owned();
                let bundle = self
                    .store
                    .call("node_membership", move |store| {
                        store.trust_bundle(&query_network)
                    })
                    .await?
                    .context("unknown network")?;
                let local = self.identity.device_id()?;
                let query_network = network.to_owned();
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
                let include_observation = access.is_user();
                let probes = members.iter().map(|member| async {
                    if member.device_id == local {
                        let capabilities = self.capabilities().await;
                        return (
                            true,
                            capabilities,
                            NodeObservation {
                                checked_at_ms: now_ms(),
                                last_synced_at_ms: None,
                                path: None,
                            },
                        );
                    }
                    // A Node the sync loop cannot reach would hold this answer for the timeout.
                    let answer = if self.sync_failing(&member.device_id) {
                        None
                    } else {
                        tokio::time::timeout(
                            Duration::from_secs(3),
                            self.peer_request_with_connection(
                                network,
                                &member.device_id,
                                json!({"type":"capabilities"}),
                                std::convert::identity,
                            ),
                        )
                        .await
                        .ok()
                        .and_then(Result::ok)
                    };
                    let live = answer.is_some();
                    let path = if include_observation {
                        answer
                            .as_ref()
                            .and_then(|(_, connection)| crate::net::selected_path(connection))
                    } else {
                        None
                    };
                    let (_, last_synced_at_ms) = self.peer_sync_status(&member.device_id);
                    let observation = NodeObservation {
                        checked_at_ms: now_ms(),
                        last_synced_at_ms,
                        path,
                    };
                    (
                        live,
                        self.peer_capabilities(&member.device_id, answer.map(|(answer, _)| answer))
                            .await,
                        observation,
                    )
                });
                let probes = futures_util::future::join_all(probes).await;
                for (member, (live, capabilities, observation)) in members.into_iter().zip(probes) {
                    let here = member.device_id == local;
                    let (seq, hash) = match heads.get(&member.device_id) {
                        Some((seq, hash)) => (*seq, Some(hash)),
                        None => (0, None),
                    };
                    let mut row = json!({"device_id":member.device_id,"name":member.name,"revoked":false,"local":here,"online":live,"max_seq":seq,"head_hash":hash,"capabilities":capabilities});
                    if include_observation {
                        row["observation"] = json!(observation);
                    }
                    nodes.push(row);
                }
                for revoked in view.revoked {
                    if let Some(name) = display_names.get(&revoked.device_id) {
                        nodes.push(json!({"device_id":revoked.device_id,"name":name,"revoked":true,"local":false,"online":false,"max_seq":revoked.cutoff_seq,"head_hash":revoked.cutoff_hash,"capabilities":Value::Null}));
                    }
                }
                Ok(json!({"nodes":nodes}))
            }
            Ok(BusinessCommand::Conversations(_)) => {
                let network = network.to_owned();
                let conversations = self
                    .read_display(&access, input, "conversations", move |store| {
                        store.conversations(&network)
                    })
                    .await?;
                Ok(json!({"conversations":conversations}))
            }
            Ok(BusinessCommand::CreateConversation(_)) => {
                ensure!(access.is_user(), "only the user creates conversations");
                let scope = id(input, "conversation_id")?;
                let title = field(input, "title")?;
                ensure!(title.len() <= 512, "title too long");
                let identity = self.identity.clone();
                let query_network = network.to_owned();
                let query_scope = scope.clone();
                let title = title.to_owned();
                let owner = first_owner(input)?;
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
            Ok(BusinessCommand::Conversation(_)) => {
                self.conversation_snapshot(&access, input).await
            }
            Ok(BusinessCommand::ConversationHistory(_)) => {
                self.conversation_history(&access, input).await
            }
            Ok(BusinessCommand::OpenWorks(_)) => {
                ensure!(access.is_user(), "only clients list open works");
                let network = network.to_owned();
                let works = self
                    .read_display(&access, input, "open_works", move |store| {
                        chat_controls::open_works(store, &network, None)
                    })
                    .await?;
                Ok(json!({"works":works}))
            }
            Ok(BusinessCommand::WorkDetail(_)) => self.work_detail(&access, input).await,
            Ok(BusinessCommand::WorkspaceOverview(_)) => {
                self.workspace_overview(&access, input).await
            }
            Ok(BusinessCommand::WorkspaceCommand(_)) => {
                self.workspace_command(&access, input).await
            }
            Ok(BusinessCommand::SignedReceipt(_)) => {
                ensure!(
                    access.is_user() && access.conversation_id().is_none(),
                    "signed receipt requires the authenticated user"
                );
                let network = network.to_owned();
                let client = field(input, "client_id")?.to_owned();
                let scope = id(input, "scope_id")?;
                let body = input["body"].clone();
                let attestation = attestation(input)?;
                self.store
                    .call("signed_receipt", move |store| {
                        store.client_signed_receipt(&network, &client, &scope, body, &attestation)
                    })
                    .await
            }
            Ok(BusinessCommand::WorkspaceCommandReceipt(_)) => {
                ensure!(
                    access.is_user() && access.conversation_id().is_none(),
                    "workspace receipt requires the authenticated user"
                );
                ensure!(
                    input.get("cause").is_none() && input.get("acting_assignment").is_none(),
                    "receipt must identify the original user command"
                );
                let network = network.to_owned();
                let client = field(input, "client_id")?.to_owned();
                let command_id = id(input, "command_id")?;
                let command = serde_json::from_value(input["command"].clone())?;
                let attestation = attestation(input)?;
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
            Ok(BusinessCommand::WorkspaceState(_)) => self.workspace_state(&access, input).await,
            Ok(BusinessCommand::WorkspaceHistory(_)) => {
                self.workspace_history(&access, input).await
            }
            Ok(BusinessCommand::MessageSearch(_)) => self.message_search(&access, input).await,
            Ok(BusinessCommand::Message(_)) => self.accept_chat_message(&access, input).await,
            Ok(BusinessCommand::InputReceipt(_)) => self.input_receipt(&access, input).await,
            Ok(BusinessCommand::ScopeContext(_)) => self.scope_context(&access, input).await,
            Ok(BusinessCommand::SetOwner(_)) => {
                let scope = scope.context("conversation_id is required")?;
                self.set_owner(&access, &scope, input).await
            }
            Ok(BusinessCommand::ReadArtifact(_)) => self.read_artifact(&access, input).await,
            Ok(BusinessCommand::UploadAttachment(_)) => {
                self.upload_attachment(&access, input).await
            }
            Ok(BusinessCommand::InterruptWork(_)) => {
                ensure!(access.is_user(), "only the user stops work");
                let scope = scope.context("conversation_id is required")?;
                self.stop_work(network, &scope, &id(input, "work_id")?)
                    .await
            }
            Ok(BusinessCommand::CodexState(_)) => {
                ensure!(access.is_user(), "only the user reaches the App Server");
                self.codex_state(network, input).await
            }
            Ok(BusinessCommand::Bots(_)) => self.bots(&access).await,
            Ok(BusinessCommand::Questions(_)) => self.questions(&access).await,
            Ok(BusinessCommand::Answer(_)) => self.answer_question(&access, input).await,
            Ok(BusinessCommand::CreateBot(_)) => self.create_bot(&access, input).await,
            Ok(BusinessCommand::DeleteBot(_)) => self.delete_bot(&access, input).await,
            Ok(BusinessCommand::SetPrimaryBot(_)) => self.set_primary_bot(&access, input).await,
            Ok(BusinessCommand::SetBotProject(_)) => self.set_bot_project(&access, input).await,
            Ok(BusinessCommand::SetBotAvatar(_)) => self.set_bot_avatar(&access, input).await,
            Ok(BusinessCommand::ControlSchedule(_)) => self.control_schedule(&access, input).await,
            Ok(BusinessCommand::ControlMemory(_)) => self.control_memory(&access, input).await,
            Ok(BusinessCommand::MemoryState(_)) => self.memory_state(&access, input).await,
            Ok(BusinessCommand::CancelManager(_)) => self.cancel_manager(&access, input).await,
            Ok(BusinessCommand::ReplyUnanswered(_)) => {
                self.settle_unanswered(&access, input, false).await
            }
            Ok(BusinessCommand::DismissUnanswered(_)) => {
                self.settle_unanswered(&access, input, true).await
            }
            Ok(BusinessCommand::Resolve(_)) => self.resolve_check(&access, input).await,
            Ok(BusinessCommand::RegisterPush(_)) => self.register_push(&access, input).await,
            Ok(BusinessCommand::UnregisterPush(_)) => self.unregister_push(&access, input).await,
            Ok(BusinessCommand::Notifications(_)) => {
                let network = network.to_owned();
                let client = field(input, "client_id")?.to_owned();
                let notifications = self
                    .read_display(&access, input, "notifications", move |store| {
                        store.display_notifications(&network, &client)
                    })
                    .await?;
                Ok(json!({"notifications":notifications}))
            }
            Ok(BusinessCommand::DismissWork(_)) => {
                ensure!(access.is_user(), "only clients close failed works");
                let scope = scope.context("conversation_id is required")?;
                let work_id = id(input, "work_id")?;
                let query_network = network.to_owned();
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
                        attestation: Some(attestation(input)?),
                    },
                )
                .await?;
                Ok(json!({"dismissed":true}))
            }
            Ok(BusinessCommand::Ack(_)) => {
                ensure!(access.is_user(), "only clients acknowledge notifications");
                let notification = id(input, "notification_id")?;
                let client = id(input, "client_id")?;
                let query_network = network.to_owned();
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
                            attestation: Some(attestation(input)?),
                        },
                    )
                    .await?;
                }
                Ok(json!({"acknowledged":true}))
            }
            Err(_params) => bail!("unknown method"),
        }
    }
    fn allowed_path(&self, path: &str) -> Result<PathBuf> {
        let path = execution_directory::CanonicalExecutionPath::resolve(Path::new(path))
            .context("execution path does not exist")?;
        path.authorize(&self.settings_snapshot().roots)
            .map_err(anyhow::Error::from)
    }
}
