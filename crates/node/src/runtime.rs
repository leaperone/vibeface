use crate::{
    config::{self, Config},
    crypto::LocalIdentity,
    identity::{AuthorizationView, GovernanceAction, IdentityKind, NetworkState},
    maintenance::Gate,
    model::*,
    store::Store,
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
mod executor;
mod followup;
mod manager_control;
mod observation;
mod peers;
mod schedule_delivery;
mod settings;
mod work;
pub use manager_control::ManagerAccess;
pub(crate) use observation::ObservationGuard;
pub use settings::NodeSettings;

fn node_display_names(bundle: &NetworkState) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    for member in bundle.genesis.members().iter().chain(
        bundle
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.members.as_slice())
            .unwrap_or(&[]),
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
    use crate::identity::{Genesis, GovernanceEvent, Member, NetworkState};
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
            &node.device_id()?,
            0,
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

#[derive(Clone)]
pub struct Update {
    pub network_id: String,
    pub payload: Value,
}
pub struct Runtime {
    pub config: Config,
    pub(crate) data_dir: PathBuf,
    settings: std::sync::Mutex<NodeSettings>,
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
    pub maintenance: Arc<Gate>,
    pub(crate) executor: Arc<executor::ExecutorHost>,
    executor_epoch: std::sync::atomic::AtomicU64,
    progress_sequence: std::sync::atomic::AtomicU64,
    pub local_access: Arc<crate::local_access::LocalAccess>,
    _process_lock: Arc<std::fs::File>,
    executor_rx: Mutex<Option<mpsc::UnboundedReceiver<Value>>>,
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
            epoch == json!({"data_epoch":6}),
            "this Node requires an initialized data epoch 6 directory"
        );
        let config = Config::load(dir)?;
        let process_lock = Arc::new(config::lock_data_dir(dir)?);
        let identity = config::identity(dir)?;
        let mut store = Store::open(dir.join("node.sqlite3"))?;
        store.attach_process_lock(process_lock.clone());
        if store.has_network()? {
            let state = store.public_state()?;
            if state.project()?.node(&identity.device_id()?).is_ok() {
                store.save_local_identity(&state.genesis.network_id, &identity)?;
            }
        }
        let store = Arc::new(Mutex::new(store));
        let (events, _) = broadcast::channel(256);
        let (tx, rx) = mpsc::unbounded_channel();
        let executor = Arc::new(executor::ExecutorHost::open(&config, &identity, dir, tx)?);
        let local_access = Arc::new(crate::local_access::LocalAccess::open(
            dir,
            config.local_auto_access,
        )?);
        let node = Self {
            data_dir: dir.to_owned(),
            settings: std::sync::Mutex::new(NodeSettings {
                roots: config.roots.clone(),
                manager: config.manager.clone(),
            }),
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
            maintenance: Arc::new(Gate::new(dir.join("update-pending.json").try_exists()?)),
            executor,
            executor_epoch: std::sync::atomic::AtomicU64::new(0),
            progress_sequence: std::sync::atomic::AtomicU64::new(0),
            local_access,
            _process_lock: process_lock,
            executor_rx: Mutex::new(Some(rx)),
        };
        Ok(node)
    }
    pub async fn install_membership(&self, incoming: NetworkState) -> Result<AuthorizationView> {
        let _work = self.maintenance.enter()?;
        let mut store = self.store.lock().await;
        let mut state = incoming;
        if store.has_network()? {
            state = store.public_state()?.merge(&state)?;
        }
        let view = state.project()?;
        store.install_state(state)?;
        if view.node(&self.identity.device_id()?).is_ok() {
            store.save_local_identity(&view.network_id, &self.identity)?;
        }
        drop(store);
        self.forget_revoked(&view).await;
        self.wake_sync_all();
        Ok(view)
    }

    pub async fn has_membership(&self) -> Result<bool> {
        self.store.lock().await.has_network()
    }

    pub async fn membership_state(&self) -> Result<NetworkState> {
        self.store.lock().await.public_state()
    }
    pub fn info(&self) -> Value {
        let manager_available = self.config.roles.contains(&NodeRole::Scheduler)
            && self.settings_snapshot().manager.is_some();
        json!({"device_id":self.identity.device_id().ok(),"version":env!("CARGO_PKG_VERSION"),"roles":self.config.roles,"manager_available":manager_available})
    }
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        let events = self
            .executor_rx
            .lock()
            .await
            .take()
            .context("runtime already started")?;
        self.start_executor(events);
        self.start_schedule_delivery();
        self.start_observation_worker();
        self.start_network().await
    }
    pub(crate) async fn maintenance_idle(&self) -> Result<()> {
        let store = self.store.lock().await;
        for network in store.networks()? {
            crate::scheduler::maintenance_idle(&store, &network)?;
        }
        Ok(())
    }
    pub async fn authorize_bearer(&self, token: &str) -> Option<Access> {
        self.agent_tokens.lock().await.get(token).cloned()
    }
    /// Stops the managed App Server; the Node calls this on SIGTERM before exiting.
    pub async fn shutdown(&self) {
        self.stop_network().await;
        self.executor.stop().await;
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
        self.publish_changed(network, scope, &event.hash);
        Ok(event)
    }
    fn publish_changed(&self, network: &str, scope: &str, event_hash: &str) {
        let _ = self.events.send(Update {
            network_id: network.into(),
            payload: json!({"type":"changed","conversation_id":scope,"event_hash":event_hash}),
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
    async fn require_role(&self, network: &str, role: NodeRole) -> Result<()> {
        let state = self.membership_state().await?;
        ensure!(
            state.genesis.network_id == network,
            "wrong personal network"
        );
        ensure!(
            self.config.roles.contains(&role),
            "node capability is disabled"
        );
        self.store
            .lock()
            .await
            .authorize_node(network, &self.identity.device_id()?)?;
        Ok(())
    }
    async fn capabilities(&self) -> Value {
        use executor::Executor as _;
        let settings = self.settings_snapshot();
        let manager =
            settings
                .manager
                .as_ref()
                .and_then(|settings| match settings.provider.as_str() {
                    "cursor_sdk" => Some(crate::manager::ManagerKind::CursorSdk),
                    "codex_sdk" => Some(crate::manager::ManagerKind::CodexSdk),
                    _ => None,
                });
        json!({"roots":settings.roots,"roles":self.config.roles,
            "manager":settings.manager.as_ref().map(|m|m.provider.clone()),
            "manager_capabilities":manager.map(|kind|kind.capabilities()),
            "executor":self.executor.executor.capability().await})
    }

    pub async fn rpc(self: &Arc<Self>, access: Access, input: Value) -> Result<Value> {
        let write = !matches!(
            input["method"].as_str(),
            Some("nodes" | "conversation" | "conversations" | "bots" | "notifications")
        );
        let _lease = self.manager_tool_lease(&access, write).await?;
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
                let max_seq = self.store.lock().await.version_vector(network)?;
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
                    let roles = capabilities.get("roles").cloned().unwrap_or(json!([]));
                    let seq = max_seq.get(&member.device_id).copied().unwrap_or(0);
                    nodes.push(json!({"device_id":member.device_id,"name":member.name,"roles":roles,"revoked":false,"local":here,"online":live,"max_seq":seq,"capabilities":capabilities}));
                }
                for revoked in view.revoked {
                    if let Some(name) = display_names.get(&revoked.device_id) {
                        nodes.push(json!({"device_id":revoked.device_id,"name":name,"roles":[],"revoked":true,"local":false,"online":false,"max_seq":revoked.cutoff_seq,"capabilities":Value::Null}));
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
                self.publish_changed(network, &scope, &scope);
                Ok(json!(conversation))
            }
            "conversation" => self.conversation_snapshot(&access, &input).await,
            "message" => self.accept_chat_message(&access, &input).await,
            "set_owner" => {
                let scope = scope.context("conversation_id is required")?;
                self.set_owner(&access, &scope, &input).await
            }
            "set_autonomy" => {
                let scope = scope.context("conversation_id is required")?;
                let result = self.set_autonomy(&access, &scope, &input).await?;
                self.schedule_wakeup.notify_one();
                Ok(result)
            }
            "read_artifact" => self.read_artifact(&access, &input).await,
            "upload_attachment" => self.upload_attachment(&access, &input).await,
            "answer_approval" | "interrupt_work" => {
                ensure!(access.human, "only the user answers or stops work");
                let scope = scope.context("conversation_id is required")?;
                let work_id = id(&input, "work_id")?;
                if method == "interrupt_work" {
                    self.stop_work(network, &scope, &work_id).await
                } else {
                    self.answer_approval(
                        network,
                        &scope,
                        &work_id,
                        &id(&input, "request_id")?,
                        input.get("response").context("response is required")?,
                        attestation(&input)?,
                    )
                    .await
                }
            }
            "bots" => self.bots(&access).await,
            "create_bot" => self.create_bot(&access, &input).await,
            "cancel_manager" => self.cancel_manager(&access, &input).await,
            "reconcile" | "resolve" => self.pending_check_control(&access, &input, method).await,
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
    pub async fn tool(self: &Arc<Self>, access: Access, name: &str, args: Value) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        ensure!(!access.human, "tool endpoint requires a scoped Agent token");
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing tool scope")?;
        self.require_scope(&access, scope).await?;
        self.agent_tool(access, name, args).await
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
