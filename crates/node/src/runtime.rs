use crate::{
    config::{self, Config},
    crypto::LocalIdentity,
    identity::{AuthorizationView, GovernanceAction, IdentityKind, NetworkState},
    maintenance::Gate,
    model::*,
    network::{IncomingSocket, NetworkHub, Service},
    peer::{AuthorizationDenied, Inbound, PeerHub, RequestFailure},
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
use tokio::sync::{Mutex, broadcast, mpsc, oneshot, watch};

mod agent_tools;
mod bots;
mod chat_controls;
mod executor;
mod followup;
mod manager_control;
mod observation;
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
const MAX_ROUTE_FLIGHTS: usize = 128;

#[derive(Clone, Debug)]
enum RouteFailure {
    Denied(AuthorizationDenied),
    Busy,
    Unavailable(String),
}
impl RouteFailure {
    fn from_error(error: &anyhow::Error) -> Self {
        if let Some(error) = error.downcast_ref::<AuthorizationDenied>() {
            Self::Denied(error.clone())
        } else if error.downcast_ref::<RequestFailure>() == Some(&RequestFailure::Busy)
            || error.downcast_ref::<crate::network::Denied>().is_some()
        {
            Self::Busy
        } else {
            Self::Unavailable(format!("{error:#}"))
        }
    }
}

#[derive(Clone, Debug)]
enum RouteFlightState {
    Connecting,
    Ready(String),
    Failed(RouteFailure),
}
struct RouteFlight {
    id: String,
    state: watch::Sender<RouteFlightState>,
}

pub struct Runtime {
    pub config: Config,
    pub(crate) data_dir: PathBuf,
    settings: std::sync::Mutex<NodeSettings>,
    pub(crate) config_save: Mutex<()>,
    pub identity: LocalIdentity,
    pub store: Arc<Mutex<Store>>,
    pub network: Arc<NetworkHub>,
    pub hub: Arc<PeerHub>,
    route_flights: Arc<Mutex<HashMap<(String, String), RouteFlight>>>,
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
    peer_rx: Mutex<Option<mpsc::Receiver<Inbound>>>,
    network_rx: Mutex<Option<mpsc::Receiver<IncomingSocket>>>,
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
    serde_json::from_value(v["attestation"].clone())
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
        Self::open_with_bootstrap_urls(dir, &[]).await
    }

    pub async fn open_with_bootstrap_urls(dir: &Path, bootstrap_urls: &[String]) -> Result<Self> {
        let epoch: Value = serde_json::from_slice(&std::fs::read(dir.join("data-epoch.json"))?)?;
        ensure!(
            epoch == json!({"data_epoch":6}),
            "this Node requires an initialized data epoch 6 directory"
        );
        let mut config = Config::load(dir)?;
        if !bootstrap_urls.is_empty() {
            config.network.bootstrap_urls = bootstrap_urls.to_vec();
            config.network.validate()?;
        }
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
        let (hub, peer_rx) = PeerHub::new(identity.clone(), store.clone(), &config.roles);
        let (network, network_rx) = NetworkHub::new(identity.clone(), config.network.clone());
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
            network,
            hub,
            events,
            agent_tokens: Mutex::new(HashMap::new()),
            manager_controls: Mutex::new(HashMap::new()),
            schedule_workers: Mutex::new(BTreeSet::new()),
            route_flights: Arc::new(Mutex::new(HashMap::new())),
            schedule_wakeup: tokio::sync::Notify::new(),
            observation: std::sync::Mutex::new(Default::default()),
            tool_url: std::sync::OnceLock::new(),
            maintenance: Arc::new(Gate::new(dir.join("update-pending.json").try_exists()?)),
            executor,
            executor_epoch: std::sync::atomic::AtomicU64::new(0),
            progress_sequence: std::sync::atomic::AtomicU64::new(0),
            local_access,
            _process_lock: process_lock,
            peer_rx: Mutex::new(Some(peer_rx)),
            network_rx: Mutex::new(Some(network_rx)),
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
        let mut receiver = self
            .peer_rx
            .lock()
            .await
            .take()
            .context("runtime already started")?;
        let node = self.clone();
        tokio::spawn(async move {
            while let Some(message) = receiver.recv().await {
                let n = node.clone();
                tokio::spawn(async move {
                    let response = n.handle_peer(&message).await;
                    let request_type = message.payload["type"].as_str();
                    let operation_id = message.payload["operation_id"]
                        .as_str()
                        .or_else(|| message.payload["request"]["operation_id"].as_str());
                    let trigger = message.payload["trigger"].as_str();
                    tracing::debug!(network=%message.network_id, origin=%message.origin,
                        request_id=%message.request_id, request_type, operation_id, trigger,
                        outcome=if response.is_ok() { "ok" } else { "error" },
                        "peer business request handled");
                    if !message.broadcast {
                        let value = response.unwrap_or_else(|e| json!({"error":format!("{e:#}")}));
                        let _ = n.hub.respond(&message, value).await;
                    }
                });
            }
        });
        let mut incoming = self
            .network_rx
            .lock()
            .await
            .take()
            .context("runtime already started")?;
        let node = self.clone();
        tokio::spawn(async move {
            while let Some(incoming) = incoming.recv().await {
                let node = node.clone();
                tokio::spawn(async move {
                    let result = match incoming.service {
                        Service::Peer => node.hub.clone().accept_transport(incoming.socket).await,
                        Service::Client => {
                            crate::client::serve(node.clone(), incoming.socket).await
                        }
                    };
                    if let Err(error) = result {
                        tracing::debug!(%error,"authenticated channel closed");
                    }
                });
            }
        });
        let transport = self.network.clone();
        let listen = self.config.listen;
        tokio::spawn(async move {
            if let Err(error) = transport.run(listen).await {
                tracing::warn!(%error,"network discovery stopped");
            }
        });
        let node = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            loop {
                tick.tick().await;
                let Ok(state) = node.store.lock().await.public_state() else {
                    continue;
                };
                let Ok(view) = state.project() else { continue };
                let online = node.hub.online(&view.network_id).await;
                for member in view.members.clone().into_iter().filter(|member| {
                    member.kind == IdentityKind::Node && view.cutoff(&member.device_id).is_none()
                }) {
                    if member.device_id == node.identity.device_id().unwrap_or_default()
                        || online.contains(&member.device_id)
                    {
                        continue;
                    }
                    if let Err(error) = node
                        .route_flight(
                            &view.network_id,
                            &member.device_id,
                            "background",
                            None,
                            None,
                        )
                        .await
                    {
                        tracing::debug!(%error, "personal peer reconnect pending");
                    }
                }
            }
        });
        Ok(())
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
    async fn route_flight(
        &self,
        network: &str,
        target: &str,
        trigger: &'static str,
        request_type: Option<&str>,
        operation_id: Option<&str>,
    ) -> Result<watch::Receiver<RouteFlightState>> {
        let key = (network.to_owned(), target.to_owned());
        let mut flights = self.route_flights.lock().await;
        if let Some(existing) = flights.get(&key) {
            let state = existing.state.borrow().clone();
            let current = match &state {
                RouteFlightState::Ready(id) => self.hub.link_matches(network, target, id).await,
                _ => true,
            };
            if current {
                tracing::debug!(network, target, flight_id=%existing.id, trigger, request_type, operation_id,
                    state=?state, "peer route flight joined");
                return Ok(existing.state.subscribe());
            }
            flights.remove(&key);
        }
        ensure!(flights.len() < MAX_ROUTE_FLIGHTS, RequestFailure::Busy);
        let id = uuid::Uuid::new_v4().to_string();
        let deadline = tokio::time::Instant::now() + crate::network::DIAL_TIMEOUT;
        let (state, receive) = watch::channel(RouteFlightState::Connecting);
        flights.insert(
            key.clone(),
            RouteFlight {
                id: id.clone(),
                state: state.clone(),
            },
        );
        tracing::debug!(network, target, flight_id=%id, trigger, request_type, operation_id, "peer route flight started");
        let hub = self.hub.clone();
        let transport = self.network.clone();
        let registry = self.route_flights.clone();
        tokio::spawn(async move {
            let (ready, receive_ready) = oneshot::channel();
            let connection = hub.connect_target(
                key.0.clone(),
                key.1.clone(),
                transport,
                Some(ready),
                deadline,
            );
            tokio::pin!(connection);
            let result = tokio::select! {
                ready = receive_ready => {
                    if let Ok(link_id) = ready {
                        state.send_replace(RouteFlightState::Ready(link_id.clone()));
                        tracing::debug!(network=%key.0, target=%key.1, flight_id=%id, link_id,
                            state="ready", "peer route flight ready");
                    }
                    connection.await
                }
                result = &mut connection => result,
            };
            let failure = match result {
                Err(error) => RouteFailure::from_error(&error),
                Ok(()) => RouteFailure::Unavailable("peer connection closed".to_owned()),
            };
            tracing::debug!(network=%key.0, target=%key.1, flight_id=%id,
                state=?failure, "peer route flight finished");
            state.send_replace(RouteFlightState::Failed(failure));
            let mut flights = registry.lock().await;
            if flights.get(&key).is_some_and(|entry| entry.id == id) {
                flights.remove(&key);
            }
        });
        Ok(receive)
    }

    async fn raw_peer_request(&self, network: &str, target: &str, payload: Value) -> Result<Value> {
        let local = self.identity.device_id()?;
        self.hub.authorize(network, &local).await?;
        self.hub.authorize(network, target).await?;
        if target != local && !self.hub.has_route(network, target).await {
            let request_type = payload["type"].as_str();
            let operation_id = payload["operation_id"]
                .as_str()
                .or_else(|| payload["request"]["operation_id"].as_str());
            let mut receive = self
                .route_flight(network, target, "on_demand", request_type, operation_id)
                .await?;
            loop {
                let state = receive.borrow().clone();
                match state {
                    RouteFlightState::Connecting => receive
                        .changed()
                        .await
                        .map_err(|_| RequestFailure::NoRoute)?,
                    RouteFlightState::Ready(_) => break,
                    RouteFlightState::Failed(RouteFailure::Denied(error)) => {
                        return Err(error.into());
                    }
                    RouteFlightState::Failed(RouteFailure::Busy) => {
                        return Err(RequestFailure::Busy.into());
                    }
                    RouteFlightState::Failed(RouteFailure::Unavailable(error)) => {
                        tracing::debug!(network, target, %error, "peer route unavailable before first send");
                        break;
                    }
                }
            }
        }
        self.hub.authorize(network, &local).await?;
        self.hub.authorize(network, target).await?;
        self.hub.request(network, target, payload).await
    }

    async fn peer_request(&self, network: &str, target: &str, payload: Value) -> Result<Value> {
        let reply = self.raw_peer_request(network, target, payload).await?;
        if let Some(error) = reply["error"].as_str() {
            bail!("{error}");
        }
        Ok(reply)
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
                let online = self.hub.online(network).await;
                let local = self.identity.device_id()?;
                let max_seq = self.store.lock().await.version_vector(network)?;
                let mut nodes = vec![];
                let view = bundle.project()?;
                let display_names = node_display_names(&bundle);
                for member in view
                    .members
                    .into_iter()
                    .filter(|member| member.kind == IdentityKind::Node)
                {
                    let here = member.device_id == local;
                    let live = here || online.contains(&member.device_id);
                    let capabilities = if here {
                        self.capabilities().await
                    } else if live {
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
                    } else {
                        Value::Null
                    };
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
                    now_ms(),
                )?;
                self.publish_changed(network, &scope, &scope);
                Ok(json!(conversation))
            }
            "conversation" => self.conversation_snapshot(&access, &input).await,
            "message" => self.accept_chat_message(&access, &input).await,
            "set_autonomy" => {
                let scope = scope.context("conversation_id is required")?;
                let result = self.set_autonomy(&access, &scope, &input).await?;
                self.schedule_wakeup.notify_one();
                Ok(result)
            }
            "read_artifact" => self.read_artifact(&access, &input).await,
            "answer_approval" | "interrupt_work" => {
                ensure!(access.human, "only the user answers or stops work");
                let scope = scope.context("conversation_id is required")?;
                let work_id = id(&input, "work_id")?;
                if method == "interrupt_work" {
                    ensure!(
                        self.store
                            .lock()
                            .await
                            .works(network, &scope)?
                            .iter()
                            .any(|work| work.work_id == work_id),
                        "work not found in this conversation"
                    );
                    self.interrupt_work(network, &work_id).await
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
    async fn handle_peer(self: &Arc<Self>, message: &Inbound) -> Result<Value> {
        self.membership_state().await?;
        let network = &message.network_id;
        let input = &message.payload;
        self.store
            .lock()
            .await
            .authorize_node(network, &message.origin)?;
        let kind = field(input, "type")?;
        let _work = (!matches!(kind, "capabilities" | "progress_read"))
            .then(|| self.maintenance.enter())
            .transpose()?;
        match kind {
            "capabilities" => Ok(self.capabilities().await),
            "trust" => {
                let state: NetworkState = serde_json::from_value(input["state"].clone())?;
                ensure!(
                    state.genesis.network_id == *network,
                    "wrong membership network"
                );
                self.install_membership(state).await?;
                Ok(json!({"received":true}))
            }
            "manager_status" => {
                self.manager_status(
                    network,
                    field(input, "conversation_id")?,
                    field(input, "trigger")?,
                )
                .await
            }
            "progress_read" => {
                ensure!(
                    !message.broadcast,
                    "progress observation requires a direct request"
                );
                self.read_progress(network, &message.origin, input).await
            }
            "cancel_manager" | "reconcile" | "resolve" => {
                self.store
                    .lock()
                    .await
                    .authorize_node(network, &message.origin)?;
                ensure!(
                    input["human"] == true,
                    "management control requires the user"
                );
                let access = Access {
                    network_id: network.clone(),
                    human: true,
                    conversation_id: Some(field(input, "conversation_id")?.into()),
                    manager: None,
                };
                match kind {
                    "cancel_manager" => self.cancel_manager(&access, input).await,
                    _ => self.pending_check_control(&access, input, kind).await,
                }
            }
            _ => bail!("unknown peer message"),
        }
    }
}
