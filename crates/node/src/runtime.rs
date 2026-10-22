use crate::{
    config::{self, Config},
    crypto::LocalIdentity,
    identity::{AuthorizationView, GovernanceAction, IdentityKind, NetworkState},
    maintenance::Gate,
    model::*,
    store::{Store, WriteState},
};
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, broadcast, mpsc};

mod agent_tools;
mod bots;
mod chat_controls;
mod codex_server;
mod cursor;
mod followup;
mod manager_control;
mod observation;
mod peers;
mod schedule_delivery;
mod settings;
mod triage;
mod work;
pub use manager_control::ManagerAccess;
pub(crate) use observation::ObservationGuard;
pub use settings::NodeSettings;
pub use triage::{SPOOL_DIR as HOOK_SPOOL, TRIAGE_URL_ENV};

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
    pub(crate) data_dir: PathBuf,
    settings: std::sync::Mutex<NodeSettings>,
    providers: std::sync::Mutex<crate::providers::Local>,
    pub(crate) config_save: Mutex<()>,
    pub identity: LocalIdentity,
    pub store: Arc<Mutex<Store>>,
    net: std::sync::OnceLock<Arc<crate::net::Net>>,
    router: Mutex<Option<iroh::protocol::Router>>,
    relay: Mutex<Option<iroh_relay::server::Server>>,
    sync_peers: std::sync::Mutex<HashMap<String, peers::PeerSync>>,
    /// Two syncs downloading the same attachment would share one staging file.
    /// ponytail: one download at a time; per-attachment locks if parallel downloads matter.
    blob_fetch: Mutex<()>,
    /// Changes with every process start, so peers can tell a restart from a quiet link.
    boot: String,
    sync_wakeup: tokio::sync::Notify,
    have_wakeup: tokio::sync::Notify,
    outbox_wakeup: tokio::sync::Notify,
    pub events: broadcast::Sender<Update>,
    pub agent_tokens: Mutex<HashMap<String, Access>>,
    manager_controls: Mutex<HashMap<String, Arc<manager_control::ActiveManager>>>,
    schedule_workers: Mutex<BTreeSet<(String, String)>>,
    schedule_wakeup: tokio::sync::Notify,
    observation: std::sync::Mutex<observation::ObservationState>,
    pub tool_url: std::sync::OnceLock<String>,
    /// The resident Cursor SDK bridge, created with the tool listener it calls back to.
    pub(crate) cursor: std::sync::OnceLock<Arc<vibeface_bridge::agent::SdkBridge>>,
    /// Shared by every Cursor turn, so it only proves the bridge called; the agent picks the attempt.
    cursor_callback_token: String,
    /// Cursor agent id → token of the attempt running it.
    pub(crate) cursor_agents: Mutex<HashMap<String, String>>,
    pub maintenance: Arc<Gate>,
    pub(crate) codex: Arc<codex_server::CodexServer>,
    codex_epoch: std::sync::atomic::AtomicU64,
    progress_sequence: std::sync::atomic::AtomicU64,
    pub local_access: Arc<crate::local_access::LocalAccess>,
    /// Operator token of the public entry's relay: this entry's own, or the one it serves.
    pub(crate) entry_token: Option<String>,
    pub(crate) write_state: tokio::sync::watch::Sender<WriteState>,
    /// Bumped on every membership change, so open sessions and connections recheck their peer.
    pub(crate) membership: tokio::sync::watch::Sender<u64>,
    _process_lock: Arc<std::fs::File>,
    codex_rx: Mutex<Option<mpsc::UnboundedReceiver<Value>>>,
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
    ensure!(
        s.len() <= 128
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c)),
        "invalid {key}"
    );
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
        let config = Config::load(dir)?;
        let entry_token = (config.public_entry || config.network.entry.is_some())
            .then(|| crate::net::read_token_file(&dir.join(crate::net::ENTRY_TOKEN_FILE)))
            .transpose()?;
        let process_lock = Arc::new(config::lock_data_dir(dir)?);
        let identity = config::identity(dir)?;
        let mut store = Store::open(dir.join("node.sqlite3"))?;
        store.attach_process_lock(process_lock.clone());
        let store = Arc::new(Mutex::new(store));
        let (events, _) = broadcast::channel(256);
        let (tx, rx) = mpsc::unbounded_channel();
        vibeface_bridge::agent::SdkBridge::kill_orphan(&dir.join(cursor::PIDFILE));
        let providers = crate::providers::Local {
            switches: config.providers.clone(),
            codex_bin: config.codex_bin.clone(),
            env: crate::providers::ProviderEnv::load(dir)?,
        };
        let codex = Arc::new(codex_server::CodexServer::open(
            providers.codex_target(),
            codex_server::system_candidates(),
            &identity,
            dir,
            tx,
        )?);
        let local_access = Arc::new(crate::local_access::LocalAccess::open(
            dir,
            config.local_auto_access,
        )?);
        let triage = triage::TriageState::new(config.triage.clone());
        let node = Self {
            data_dir: dir.to_owned(),
            settings: std::sync::Mutex::new(NodeSettings {
                roots: config.roots.clone(),
                manager: config.manager.clone(),
            }),
            providers: std::sync::Mutex::new(providers),
            config_save: Mutex::new(()),
            config,
            identity,
            store,
            net: std::sync::OnceLock::new(),
            router: Mutex::new(None),
            relay: Mutex::new(None),
            sync_peers: std::sync::Mutex::new(HashMap::new()),
            blob_fetch: Mutex::new(()),
            boot: uuid::Uuid::new_v4().simple().to_string(),
            sync_wakeup: tokio::sync::Notify::new(),
            have_wakeup: tokio::sync::Notify::new(),
            outbox_wakeup: tokio::sync::Notify::new(),
            events,
            agent_tokens: Mutex::new(HashMap::new()),
            manager_controls: Mutex::new(HashMap::new()),
            schedule_workers: Mutex::new(BTreeSet::new()),
            schedule_wakeup: tokio::sync::Notify::new(),
            observation: std::sync::Mutex::new(Default::default()),
            tool_url: std::sync::OnceLock::new(),
            cursor: std::sync::OnceLock::new(),
            cursor_callback_token: hex::encode(rand::random::<[u8; 32]>()),
            cursor_agents: Mutex::new(HashMap::new()),
            maintenance: Arc::new(Gate::new(dir.join("update-pending.json").try_exists()?)),
            codex,
            codex_epoch: std::sync::atomic::AtomicU64::new(0),
            progress_sequence: std::sync::atomic::AtomicU64::new(0),
            local_access,
            entry_token,
            write_state: tokio::sync::watch::Sender::new(WriteState::Writable),
            membership: tokio::sync::watch::Sender::new(0),
            _process_lock: process_lock,
            codex_rx: Mutex::new(Some(rx)),
            triage,
            node_join: std::sync::Mutex::new(None),
            rejoin: tokio::sync::Notify::new(),
            log_intake: Default::default(),
        };
        if let Some(token) = &node.entry_token {
            crate::logging::add_secret(token);
        }
        crate::logging::add_secret(&node.cursor_callback_token);
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
        let mut store = self.store.lock().await;
        let mut state = incoming;
        if store.has_network()? {
            state = store.public_state()?.merge(&state)?;
        }
        let network = state.genesis.network_id.clone();
        let (view, rebuilt) = store.install_state(state)?;
        if rebuilt {
            store.settle_local_works(&network)?;
        }
        drop(store);
        if rebuilt {
            self.publish_rebuilt(&network);
        }
        self.membership.send_modify(|changes| *changes += 1);
        self.forget_revoked(&view).await;
        self.align(&network).await;
        self.wake_sync_all();
        Ok(view)
    }

    /// Asks the serving process to set this removed Node's data aside and start fresh. The short
    /// wait lets the reply reach the local page first.
    pub(crate) fn request_rejoin(self: &Arc<Self>) {
        let node = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            node.rejoin.notify_one();
        });
    }
    pub async fn rejoin_requested(&self) {
        self.rejoin.notified().await
    }

    pub async fn has_membership(&self) -> Result<bool> {
        self.store.lock().await.has_network()
    }

    pub async fn membership_state(&self) -> Result<NetworkState> {
        self.store.lock().await.public_state()
    }
    /// The name this Node had in its network, kept after its removal.
    pub async fn member_name(&self) -> Option<String> {
        let device = self.identity.device_id().ok()?;
        node_display_names(&self.membership_state().await.ok()?).remove(&device)
    }
    pub fn info(&self) -> Value {
        let manager_available = self.assistant().is_some();
        json!({"device_id":self.identity.device_id().ok(),"version":env!("CARGO_PKG_VERSION"),"manager_available":manager_available,"public_entry":self.config.public_entry})
    }
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        let events = self
            .codex_rx
            .lock()
            .await
            .take()
            .context("runtime already started")?;
        if self.config.public_entry {
            return self.start_network().await;
        }
        if self.align_on_start().await? {
            self.start_writers(events);
        } else {
            let node = self.clone();
            tokio::spawn(async move {
                let mut state = node.write_state.subscribe();
                if state
                    .wait_for(|state| *state == WriteState::Writable)
                    .await
                    .is_ok()
                {
                    node.start_writers(events);
                    if let Ok(state) = node.membership_state().await {
                        node.abandon_revoked_works(&state.genesis.network_id).await;
                    }
                }
            });
        }
        self.start_network().await
    }
    /// Work, scheduling and observation append to the log, so they wait for `writable`.
    /// ponytail: a Node that forks later keeps these running and its appends fail; stop them
    /// if a forked device turns out to keep running work.
    fn start_writers(self: &Arc<Self>, events: mpsc::UnboundedReceiver<Value>) {
        self.start_codex(events);
        self.start_schedule_delivery();
        self.start_observation_worker();
        self.start_triage();
    }
    pub(crate) async fn maintenance_idle(&self) -> Result<()> {
        self.store.lock().await.scheduler_maintenance_idle()
    }
    pub async fn authorize_bearer(&self, token: &str) -> Option<Access> {
        self.agent_tokens.lock().await.get(token).cloned()
    }
    /// Stops the managed App Server and Cursor bridge; the Node calls this on SIGTERM before exiting.
    pub async fn shutdown(&self) {
        self.stop_network().await;
        self.codex.stop().await;
        if let Some(bridge) = self.cursor.get() {
            bridge.stop().await;
        }
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
        let event = self.store.lock().await.append_event(
            &self.identity,
            network,
            scope,
            body,
            blobs,
            now_ms(),
        )?;
        self.publish_changed(network, scope, Changed::Event(&event.hash));
        Ok(event)
    }
    /// Which events count changed, so every conversation may read differently now.
    pub(crate) fn publish_rebuilt(&self, network: &str) {
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
            Changed::Job(trigger) => payload["trigger"] = json!(trigger),
            Changed::Reload => {}
        }
        let _ = self.events.send(Update {
            network_id: network.into(),
            payload,
        });
        self.have_wakeup.notify_one();
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
        let waiting = Instant::now();
        let store = self.store.lock().await;
        let waited = waiting.elapsed();
        let held = Instant::now();
        store.authorize_node(&access.network_id, &self.identity.device_id()?)?;
        ensure!(
            store.has_conversation(&access.network_id, scope)?,
            "conversation not found"
        );
        tracing::trace!(network=%access.network_id, scope, stage="scope_access", wait_ms=waited.as_millis() as u64,
            hold_ms=held.elapsed().as_millis() as u64, "history store phase finished");
        Ok(())
    }
    /// What this Node offers its peers right now; work it does not execute is not offered.
    async fn capabilities(&self) -> Value {
        let settings = self.settings_snapshot();
        let manager = self.assistant().map(|m| m.provider);
        let mut codex = self.codex.capability().await;
        codex["enabled"] = json!(self.providers().switches.execution);
        json!({"roots":settings.roots,"relay":self.config.network.relay_listen.is_some(),
            "manager":manager,
            "manager_capabilities":manager.map(|kind|kind.capabilities()),
            "codex":codex})
    }

    pub async fn rpc(self: &Arc<Self>, access: Access, input: Value) -> Result<Value> {
        let _lease = self.manager_tool_lease(&access).await?;
        self.rpc_inner(access, input).await
    }
    async fn rpc_inner(self: &Arc<Self>, access: Access, input: Value) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        let current = self.membership_state().await?;
        let network = &access.network_id;
        ensure!(
            current.genesis.network_id == *network,
            "wrong personal network"
        );
        self.store
            .lock()
            .await
            .authorize_node(network, &self.identity.device_id()?)?;
        let method = field(&input, "method")?;
        if !access.human {
            ensure!(method == "nodes", "method unavailable to Agent tokens");
        }
        let scope = input["conversation_id"].as_str().map(String::from);
        if let Some(scope) = &scope
            && !matches!(method, "create_conversation" | "create_bot")
        {
            self.require_scope(&access, scope).await?;
        }
        match method {
            "nodes" => {
                let bundle = self
                    .store
                    .lock()
                    .await
                    .trust_bundle(network)?
                    .context("unknown network")?;
                let local = self.identity.device_id()?;
                let heads = self.store.lock().await.heads(network)?;
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
                        return self.capabilities().await;
                    }
                    // A Node the sync loop cannot reach would hold this answer for the timeout.
                    if self.sync_failing(&member.device_id) {
                        return Value::Null;
                    }
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
                    .unwrap_or(Value::Null)
                });
                let probes = futures_util::future::join_all(probes).await;
                for (member, capabilities) in members.into_iter().zip(probes) {
                    let here = member.device_id == local;
                    let live = !capabilities.is_null();
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
                Ok(json!({"conversations":self.store.lock().await.conversations(network)?}))
            }
            "create_conversation" => {
                ensure!(access.human, "only the user creates conversations");
                let scope = id(&input, "conversation_id")?;
                let title = field(&input, "title")?;
                ensure!(title.len() <= 512, "title too long");
                let conversation = self.store.lock().await.create_conversation(
                    &self.identity,
                    network,
                    &scope,
                    title,
                    first_owner(&input)?,
                    now_ms(),
                )?;
                self.publish_changed(network, &scope, Changed::Reload);
                Ok(json!(conversation))
            }
            "conversation" => self.conversation_snapshot(&access, &input).await,
            "open_works" => {
                ensure!(access.human, "only clients list open works");
                Ok(json!({"works":self.store.lock().await.open_works(network)?}))
            }
            "message" => self.accept_chat_message(&access, &input).await,
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
            "set_primary_assistant" => self.set_primary_assistant(&access, &input).await,
            "control_schedule" => self.control_schedule(&access, &input).await,
            "cancel_manager" => self.cancel_manager(&access, &input).await,
            "resolve" => self.resolve_check(&access, &input).await,
            "notifications" => Ok(
                json!({"notifications":self.store.lock().await.pending_notifications(network,field(&input,"client_id")?)?}),
            ),
            "ack" => {
                ensure!(access.human, "only clients acknowledge notifications");
                let notification = id(&input, "notification_id")?;
                let client = id(&input, "client_id")?;
                let pending = self
                    .store
                    .lock()
                    .await
                    .pending_notifications(network, &client)?;
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
        ensure!(
            self.settings_snapshot()
                .roots
                .iter()
                .filter_map(|r| std::fs::canonicalize(r).ok())
                .any(|r| path.starts_with(r)),
            "execution path is outside configured roots"
        );
        ensure!(path.is_dir(), "execution path must be a directory");
        Ok(path)
    }
}
