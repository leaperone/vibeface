//! The Node side of the iroh transport: membership-gated handlers, version-vector sync, the
//! signed address book, scheduler delivery and resumable attachment downloads.
//!
//! Sync only stores events. It never creates scheduler jobs or work, so pulling history again
//! after a restart or from another Node replays no side effect.
use super::*;
use crate::clock::now_ms;
use crate::net::{self, AddressRecord, Net, SignedAddress, relay::RelayGrant};
use crate::store::admission::{
    AdmissionRejected, PeerAdmission, PeerAdmissionState, PeerCommand, PeerEnvelope,
    authorize_peer_node_from,
};
use iroh::{
    endpoint::{Connection, ConnectionError},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use std::sync::Weak;

mod peer_delivery;

const HELLO_INTERVAL: Duration = Duration::from_secs(60);
const RETRY_LIMIT: Duration = Duration::from_secs(30);
const PEER_TIMEOUT: Duration = Duration::from_secs(30);
const BLOB_CHUNK: usize = 64 * 1024;
const BLOB_SYNC_BYTES: u64 = 1024 * 1024;
const MAX_STAGED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_STAGED_OBJECTS: usize = 64;
const MAX_ADDRESS_RECORDS: usize = 256;
const MAX_FORK_PAGES: usize = 4;
/// How long a turn waits for the attachments it shows before it marks them pending.
const ATTACHMENT_WAIT: Duration = Duration::from_secs(15);
const JOIN_NOTICE_WAIT: Duration = Duration::from_secs(3);
const REMOVED_SYNC_BYTES: usize = 256 * 1024;
const REMOVED_SYNC_TIMEOUT: Duration = Duration::from_secs(10);

/// The shared request/reply body; routing fields remain the authenticated caller's concern.
#[derive(serde::Serialize, serde::Deserialize)]
struct AddressExchange {
    records: Vec<SignedAddress>,
}

impl AddressExchange {
    fn decode(input: &Value) -> Result<Self> {
        <Self as serde::Deserialize>::deserialize(input)
            .map_err(|_| anyhow::anyhow!("invalid address exchange records"))
    }

    fn into_request(self, network: &str) -> Result<Value> {
        let mut request = serde_json::to_value(self)?;
        request["type"] = json!("addresses");
        request["network_id"] = json!(network);
        Ok(request)
    }
}

pub(super) struct RemovedSyncLimits {
    open: Arc<tokio::sync::Semaphore>,
    peers: std::sync::Mutex<HashMap<String, Weak<tokio::sync::Semaphore>>>,
}

impl Default for RemovedSyncLimits {
    fn default() -> Self {
        Self {
            open: Arc::new(tokio::sync::Semaphore::new(4)),
            peers: Default::default(),
        }
    }
}

impl RemovedSyncLimits {
    fn for_peer(&self, remote: &str) -> net::StreamLimit {
        let mut peers = self.peers.lock().expect("removed Node sync limits");
        peers.retain(|_, limit| limit.strong_count() > 0);
        let peer = peers
            .get(remote)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let limit = Arc::new(tokio::sync::Semaphore::new(1));
                peers.insert(remote.into(), Arc::downgrade(&limit));
                limit
            });
        (self.open.clone(), peer, REMOVED_SYNC_TIMEOUT)
    }
}

#[derive(Default)]
pub(super) struct PeerDeliveryLocks {
    targets: std::sync::Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

impl PeerDeliveryLocks {
    fn target(&self, target: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut targets = self.targets.lock().expect("peer delivery locks");
        targets.retain(|_, target| target.strong_count() > 0);
        if let Some(target) = targets.get(target).and_then(Weak::upgrade) {
            return target;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        targets.insert(target.into(), Arc::downgrade(&lock));
        lock
    }

    pub(super) async fn lock(&self, target: &str) -> tokio::sync::OwnedMutexGuard<()> {
        self.target(target).lock_owned().await
    }

    pub(super) fn try_lock(&self, target: &str) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        self.target(target).try_lock_owned().ok()
    }
}

/// `(author, seq, hash)` of a fork evidence event, the order the evidence is read in.
type ForkKey = (String, u64, String);

#[derive(Default)]
pub(super) struct PeerSync {
    due: Option<Instant>,
    failures: u32,
    running: bool,
    rerun: bool,
    boot: Option<String>,
    synced_ms: Option<i64>,
    /// The last pair of differing fork digests that woke a sync, so evidence one side cannot
    /// store does not keep both waking each other.
    forks: Option<String>,
    /// Where the next round goes on reading the peer's fork evidence.
    fork_after: Option<ForkKey>,
    /// This boot has seen the peer hold nothing of this device's log beyond ours.
    confirmed: bool,
    /// What the peer last answered to `capabilities`, kept to show while it is offline.
    capabilities: Option<Value>,
}

#[derive(Clone)]
struct Inbound {
    node: Weak<Runtime>,
    alpn: &'static [u8],
}
impl std::fmt::Debug for Inbound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Inbound({})", String::from_utf8_lossy(self.alpn))
    }
}
impl ProtocolHandler for Inbound {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let Some(node) = self.node.upgrade() else {
            return Ok(());
        };
        let peer = net::device_id(&connection.remote_id());
        node.accept_connection(self.alpn, connection)
            .await
            .map_err(|error| {
                let error = format!("{error:#}");
                // iroh repeats this under `iroh::protocol`, which the default filter keeps quiet.
                tracing::warn!(event = "net.accept_failed", alpn = %String::from_utf8_lossy(self.alpn), peer, error, "an incoming connection ended with an error");
                AcceptError::from_err(std::io::Error::other(error))
            })
    }
}

/// The relay admits this device, the personal network's Nodes, joiners holding an open invite,
/// holders of the operator token or of a grant this Node signed and, when open, anyone, but
/// never a blocked device. Only connections from outside the personal network count toward
/// `relay_max_clients`.
pub(super) struct RelayAccess {
    node: Weak<Runtime>,
    counted: std::sync::Mutex<std::collections::HashSet<iroh_relay::server::ConnectionId>>,
}
impl RelayAccess {
    pub(super) fn new(node: Weak<Runtime>) -> Self {
        Self {
            node,
            counted: Default::default(),
        }
    }
}
impl std::fmt::Debug for RelayAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RelayAccess")
    }
}
impl iroh_relay::server::AccessControl for RelayAccess {
    async fn on_connect(
        &self,
        request: &iroh_relay::server::ClientRequest,
    ) -> iroh_relay::server::Access {
        let allowed = match self.node.upgrade() {
            Some(node) => match node.relay_admits(request).await {
                Some(true) => {
                    let max = node.relay_config().relay_max_clients;
                    let mut counted = self.counted.lock().expect("relay clients");
                    max.is_none_or(|max| counted.len() < max.get())
                        && node.volunteer.usage.bind(request)
                        && counted.insert(request.connection_id())
                }
                Some(false) => node.volunteer.usage.bind(request),
                None => false,
            },
            None => false,
        };
        if allowed {
            iroh_relay::server::Access::Allow
        } else {
            tracing::debug!(endpoint = %request.endpoint_id().fmt_short(), "relay refused an endpoint");
            iroh_relay::server::Access::Deny {
                reason: Some("not allowed on this relay".into()),
            }
        }
    }

    fn on_disconnect(
        &self,
        _endpoint: iroh::EndpointId,
        connection: iroh_relay::server::ConnectionId,
    ) {
        self.counted
            .lock()
            .expect("relay clients")
            .remove(&connection);
    }
}

/// Merged states are canonical, so equal histories encode equally.
fn membership_digest(state: &NetworkState) -> Result<String> {
    Ok(crate::crypto::digest(&serde_json::to_vec(state)?))
}

/// Fetches what the peer holds of one author's log past a fork. Range pulls only extend the
/// chain from our head, so a branch that left it earlier never arrives that way. Unless the
/// peer's head is already here, this walks back from the shorter head in doubling steps to the
/// newest of the peer's events held here, then ingests the peer's chain from there. Returns
/// whether it fetched anything.
pub(crate) async fn reconcile_log<H, P, I>(
    our_max: u64,
    (their_max, their_head): (u64, &str),
    mut holds: impl FnMut(String) -> H,
    mut pull: impl FnMut(u64, usize) -> P,
    mut ingest: impl FnMut(Vec<SignedEvent>) -> I,
) -> Result<bool>
where
    H: Future<Output = Result<bool>>,
    P: Future<Output = Result<Vec<SignedEvent>>>,
    I: Future<Output = Result<()>>,
{
    if their_max == 0 || holds(their_head.to_owned()).await? {
        return Ok(false);
    }
    let mut held = our_max.min(their_max);
    if held == their_max {
        held -= 1;
    }
    let mut step = 1;
    while held > 0 {
        if let Some(event) = pull(held, 1).await?.first()
            && holds(event.hash.clone()).await?
        {
            break;
        }
        held = held.saturating_sub(step);
        step = step.saturating_mul(2);
    }
    let mut from = held + 1;
    while from <= their_max {
        let events = pull(from, crate::store::MAX_PULL_EVENTS).await?;
        let Some(last) = events.last() else { break };
        let next = serde_json::from_str::<Event>(&last.canonical_body)?.seq + 1;
        ensure!(next > from, "peer answered a log range out of order");
        ingest(events).await?;
        from = next;
    }
    Ok(true)
}

impl Runtime {
    pub(crate) fn net(&self) -> Result<&Arc<Net>> {
        self.net.get().context("the Node network has not started")
    }

    /// This Node's node link, which bootstraps other Nodes and goes into its invites; only its
    /// device ID until the network starts.
    pub(crate) fn share_link(&self) -> Result<vibeface_secure::link::NodeLink> {
        match self.net() {
            Ok(net) => net.share_link(),
            Err(_) => Ok(vibeface_secure::link::NodeLink::new(
                &self.identity.device_id()?,
                vec![],
                None,
            )?),
        }
    }

    pub(super) async fn start_network(self: &Arc<Self>) -> Result<()> {
        let net = Arc::new(
            Net::bind(
                &self.identity.secret_key_hex,
                &self.config.network,
                self.config.listen.port(),
                vec![],
            )
            .await?,
        );
        ensure!(self.net.set(net.clone()).is_ok(), "network already started");
        let mesh = &self.config.network.mesh;
        if mesh.enabled
            && let Err(error) = net
                .start_mesh(mesh, &self.data_dir.join("mesh.sqlite3"))
                .await
        {
            tracing::warn!(event = "mesh.off", %error, "the mesh is off");
        }
        let weak = Arc::downgrade(self);
        let mut router = Router::builder(net.endpoint.clone());
        for alpn in net::ALPNS {
            router = router.accept(
                alpn,
                Inbound {
                    node: weak.clone(),
                    alpn,
                },
            );
        }
        if let Some(mesh) = net.mesh() {
            router = mesh.accept(router);
        }
        *self.router.lock().await = Some(router.spawn());
        self.start_relay().await?;
        self.delivery.attach(Arc::downgrade(self))?;
        let bind = self.config.network.bind;
        // A Node bound to one address listens nowhere else.
        if bind.is_none_or(|bind| bind.ip().is_unspecified()) {
            let node = Arc::downgrade(self);
            net.start_lan(move |peer| {
                if let Some(node) = node.upgrade() {
                    node.wake_sync(peer);
                }
            });
        }
        if self.has_membership().await? {
            let network = self.membership_state().await?.genesis.network_id;
            let view = self.membership_state().await?.project()?;
            for signed in self
                .store
                .call("peers.address_book", move |store| store.address_book())
                .await?
            {
                if let Ok(record) = signed.open(&network)
                    && view.node(&record.device_id).is_ok()
                {
                    net.learn_member(&record).await;
                }
            }
        }
        let node = self.clone();
        self.background_tasks
            .spawn(async move { node.sync_loop().await })?;
        let node = self.clone();
        self.background_tasks
            .spawn(async move { node.have_loop().await })?;
        let node = self.clone();
        self.background_tasks
            .spawn(async move { node.outbox_loop().await })?;
        let node = self.clone();
        self.background_tasks
            .spawn(async move { node.volunteer_loop().await })?;
        let node = self.clone();
        self.background_tasks
            .spawn(async move { node.adopt_loop().await })?;
        Ok(())
    }

    pub(super) async fn stop_network(&self) {
        if let Ok(net) = self.net() {
            net.stop_lan().await;
            if let Some(mesh) = net.mesh() {
                mesh.close().await;
            }
        }
        if let Some(router) = self.router.lock().await.take() {
            let _ = router.shutdown().await;
        }
        if let Some(relay) = self.relay.lock().await.take() {
            relay.shutdown().await;
        }
    }

    /// `None` when the relay refuses the request's device; otherwise whether its connection
    /// counts toward `relay_max_clients`.
    async fn relay_admits(&self, request: &iroh_relay::server::ClientRequest) -> Option<bool> {
        let device = net::device_id(&request.endpoint_id());
        let relay = self.relay_config();
        if relay.relay_blocked.contains(&device) {
            return None;
        }
        if self.identity.device_id().is_ok_and(|own| own == device)
            || self.member_node(&device).await.is_ok()
        {
            return Some(false);
        }
        let granted = match request.auth_token() {
            Some(token) if RelayGrant::is_grant(&token) => self.grant_admits(&token, &device),
            Some(token) => {
                relay
                    .relay_operator_token
                    .as_ref()
                    .is_some_and(|operator| net::same_token(&token, operator))
                    || {
                        let arg0 = (token).to_owned();
                        self.store
                            .call("peers.relay_token_open", move |store| {
                                store.relay_token_open(&arg0)
                            })
                            .await
                    }
                    .unwrap_or(false)
            }
            None => false,
        };
        (relay.relay_open || granted).then_some(true)
    }

    async fn removed_here(&self, device: &str) -> bool {
        self.membership_state()
            .await
            .and_then(|state| state.project())
            .is_ok_and(|view| view.removed_node(device))
    }

    /// A member Node refusing this one hands back the signed history that removed it. Installing
    /// it checks that history against the one held here, so a forged removal changes nothing.
    pub(super) async fn accept_removal(&self, network: &str, reply: &Value) -> Result<()> {
        let incoming: NetworkState =
            serde_json::from_value(reply["state"].clone()).context("invalid membership")?;
        ensure!(
            incoming.genesis.network_id == network,
            "wrong membership network"
        );
        let view = self.install_membership(incoming).await?;
        ensure!(
            view.revocation(&self.identity.device_id()?).is_some(),
            "the peer refused this Node without a removal"
        );
        tracing::warn!(
            event = "network.removed",
            "this Node has been removed from the personal network"
        );
        Ok(())
    }

    /// The network ID, if `device` is a current member Node.
    async fn member_node(&self, device: &str) -> Result<String> {
        let network = self.membership_state().await?.genesis.network_id;
        {
            let arg0 = (network).to_owned();
            let arg1 = (device).to_owned();
            self.store
                .call("peers.authorize_node", move |store| {
                    store.authorize_node(&arg0, &arg1)
                })
                .await
        }?;
        Ok(network)
    }

    async fn accept_connection(
        self: Arc<Self>,
        alpn: &'static [u8],
        connection: Connection,
    ) -> Result<()> {
        let remote = net::device_id(&connection.remote_id());
        if alpn == net::SERVICE {
            return self.delivery.serve(self.clone(), connection).await;
        }
        if alpn == net::JOIN {
            return crate::client::serve_join(self, connection, remote).await;
        }
        // Any device may forward a client session: it ends here, and Noise proves the browser.
        if alpn == net::CLIENT {
            return crate::client::serve_forwarded(self, connection).await;
        }
        if alpn == net::DECISION {
            return self.serve_decisions(connection).await;
        }
        let network = match self.member_node(&remote).await {
            Ok(network) => network,
            Err(_) if alpn == net::SYNC && self.removed_here(&remote).await => {
                // A removed Node learns why from the signed history; it checks every signature.
                // One that holds a veto undoing that removal hands it here the same way.
                let network = self.membership_state().await?.genesis.network_id;
                let limit = self.removed_sync_limits.for_peer(&remote);
                let node = Arc::downgrade(&self);
                net::serve_streams_within(
                    connection,
                    Some(limit),
                    REMOVED_SYNC_BYTES,
                    move |request: Value| {
                        let (node, network, remote) =
                            (node.clone(), network.clone(), remote.clone());
                        async move {
                            ensure!(
                                request["network_id"].as_str() == Some(network.as_str()),
                                "wrong personal network"
                            );
                            let node = node.upgrade().context("Node stopped")?;
                            ensure!(
                                matches!(request["type"].as_str(), Some("hello" | "membership")),
                                "removed Nodes only exchange membership"
                            );
                            let known = if request["type"] == "membership" {
                                let incoming: NetworkState =
                                    serde_json::from_value(request["state"].clone())
                                        .context("invalid membership")?;
                                let known = incoming.project().ok().map(|_| incoming.clone());
                                // Only its governance events count: a removed Node cannot bring in a
                                // recovery checkpoint, so a backdated one skips no wait or veto. Events
                                // this history cannot check are left out.
                                let local = node.membership_state().await?;
                                let state = NetworkState {
                                    events: incoming.events,
                                    ..local
                                };
                                if let Err(error) = node.install_membership(state).await {
                                    tracing::debug!(%error, "ignored a removed Node's history");
                                }
                                known
                            } else {
                                None
                            };
                            let state = node.membership_state().await?;
                            let state = if state.project()?.node(&remote).is_ok() {
                                state
                            } else {
                                state.removal_state(&remote, known.as_ref())?
                            };
                            ensure!(
                                serde_json::to_vec(&state)?.len() <= REMOVED_SYNC_BYTES,
                                "removal evidence exceeds the recovery sync limit"
                            );
                            Ok(json!({"type":"revoked","network_id":network,"state":state}))
                        }
                    },
                )
                .await;
                return Ok(());
            }
            Err(error) => {
                tracing::debug!(%remote, error = %format!("{error:#}"), "refused a connection from a non-member");
                connection.close(1u32.into(), b"not a member Node of this personal network");
                // Refusing a device outside the personal network is the answer, not a failure.
                let outsider = !self.has_membership().await?
                    || self
                        .membership_state()
                        .await?
                        .project()?
                        .node(&remote)
                        .is_err();
                return if outsider { Ok(()) } else { Err(error) };
            }
        };
        self.close_when_removed(connection.clone(), remote.clone());
        if alpn == net::BLOB {
            self.serve_blobs(connection, network, remote).await;
            return Ok(());
        }
        let (watched, closed) = (connection.clone(), connection.clone());
        let node = self.clone();
        let peer = remote.clone();
        net::serve_streams(connection, move |request: Value| {
            let (node, connection, network, remote) = (
                node.clone(),
                watched.clone(),
                network.clone(),
                remote.clone(),
            );
            async move {
                if request["network_id"].as_str() != Some(network.as_str()) {
                    connection.close(2u32.into(), b"wrong personal network");
                    bail!("wrong personal network");
                }
                {
                    let arg0 = (network).to_owned();
                    let arg1 = (remote).to_owned();
                    node.store
                        .call("peers.authorize_node", move |store| {
                            store.authorize_node(&arg0, &arg1)
                        })
                        .await
                }?;
                if alpn == net::SYNC {
                    node.handle_sync(&network, &remote, &request).await
                } else {
                    node.handle_rpc(&network, &remote, &request).await
                }
            }
        })
        .await;
        // A peer that shut down closed this itself; syncing now tells clients it went offline.
        if let Some(
            ConnectionError::ApplicationClosed(_)
            | ConnectionError::ConnectionClosed(_)
            | ConnectionError::Reset,
        ) = closed.close_reason()
        {
            self.wake_sync(&peer);
        }
        Ok(())
    }

    /// Ends an accepted connection once its Node leaves the directory; each request is checked
    /// anyway, this also stops the streams already open on it.
    fn close_when_removed(self: &Arc<Self>, connection: Connection, remote: String) {
        let node = Arc::downgrade(self);
        let mut membership = self.membership.subscribe();
        if let Err(error) = self.background_tasks.spawn(async move {
            loop {
                tokio::select! {
                    _ = connection.closed() => return,
                    changed = membership.changed() => {
                        let Some(node) = node.upgrade() else { return };
                        if changed.is_err() || node.member_node(&remote).await.is_err() {
                            connection.close(1u32.into(), b"no longer a member Node");
                            return;
                        }
                    }
                }
            }
        }) {
            tracing::debug!(%error, "membership watcher skipped while the Node is shutting down");
        }
    }

    /// A request to another member Node over `vibeface/rpc/1`.
    pub(super) async fn peer_request(
        &self,
        network: &str,
        target: &str,
        payload: Value,
    ) -> Result<Value> {
        self.peer_request_with_send_boundary(network, target, payload, std::convert::identity)
            .await
    }

    pub(super) async fn peer_request_with_send_boundary(
        &self,
        network: &str,
        target: &str,
        payload: Value,
        after_send: fn(anyhow::Error) -> anyhow::Error,
    ) -> Result<Value> {
        let (answer, _) = self
            .peer_request_with_connection(network, target, payload, after_send)
            .await?;
        Ok(answer)
    }

    pub(super) async fn peer_request_with_connection(
        &self,
        network: &str,
        target: &str,
        mut payload: Value,
        after_send: fn(anyhow::Error) -> anyhow::Error,
    ) -> Result<(Value, Connection)> {
        let local = self.identity.device_id()?;
        ensure!(
            target != local,
            "this Node answers itself without the network"
        );
        {
            let network = network.to_owned();
            let local = local.clone();
            let target = target.to_owned();
            self.store
                .call("peers.peer_authorization", move |store| {
                    store.authorize_node(&network, &local)?;
                    store.authorize_node(&network, &target)?;

                    Ok(())
                })
                .await?
        }
        payload["network_id"] = json!(network);
        self.net()?
            .request_with_connection(target, net::RPC, &payload, PEER_TIMEOUT, after_send)
            .await
    }

    async fn handle_rpc(
        self: &Arc<Self>,
        network: &str,
        origin: &str,
        input: &Value,
    ) -> Result<Value> {
        let kind = field(input, "type")?;
        let _work = (!matches!(
            kind,
            "capabilities"
                | "work_evidence"
                | "progress_read"
                | "addresses"
                | "scheduler_jobs"
                | "scheduler_history"
                | "join_changed"
                | "input_receipt"
                | "scope_context"
        ))
        .then(|| self.maintenance.enter())
        .transpose()?;
        match kind {
            "capabilities" => Ok(self.capabilities().await),
            "work_evidence" => {
                let conversation = field(input, "conversation_id")?;
                let work_id = field(input, "work_id")?;
                let vouches = self
                    .local_work_vouches(network, conversation, work_id)
                    .await?;
                Ok(json!({"network_id":network,"conversation_id":conversation,
                    "work_id":work_id,"vouches":vouches}))
            }
            "addresses" => {
                let exchange = AddressExchange::decode(input)?;
                self.merge_addresses(network, exchange.records).await?;
                Ok(serde_json::to_value(AddressExchange {
                    records: self
                        .store
                        .call("peers.address_book", move |store| store.address_book())
                        .await?,
                })?)
            }
            "scheduler_jobs" => {
                self.owner_jobs(network, field(input, "conversation_id")?)
                    .await
            }
            "scheduler_history" => self.owner_history(network, input).await,
            "progress_read" => self.read_progress(network, origin, input).await,
            "deliver" | "dispatch_work" | "interrupt_work" => {
                self.accept_peer_command(network, origin, input).await
            }
            "input_receipt" => self.read_input_receipt(network, input).await,
            "scope_context" => self.read_scope_context(network, input).await,
            "workspace_command" => self.accept_workspace_command(network, input).await,
            "join_changed" => {
                self.publish_reread(network);
                Ok(json!({}))
            }
            "pending_joins" => Ok(
                json!({"requests":({ let admin = field(input, "admin")?.to_owned(); self.store.call("peers.pending_joins", move |store| store.pending_joins(&admin)).await }?)}),
            ),
            "approve_join" => {
                crate::client::approve_join(self, field(input, "admin")?, &input["params"]).await
            }
            "codex_state" => {
                ensure!(
                    input["human"] == true,
                    "the App Server answers the user only"
                );
                self.codex_state(network, input).await
            }
            "respond" => {
                let result = input.get("result").cloned().context("result is required")?;
                self.respond_unclaimed_request(field(input, "request_id")?, result)
                    .await
            }
            "hand_over_session" => self
                .hand_over_session(
                    network,
                    field(input, "from")?,
                    &self.identity.device_id()?,
                    field(input, "session_id")?,
                    field(input, "version")?,
                    input["conversation_id"].as_str(),
                )
                .await
                .map(|outcome| outcome.value),
            "cancel_manager" | "resolve" => {
                ensure!(
                    input["human"] == true,
                    "management control requires the user"
                );
                let scope = field(input, "conversation_id")?;
                let access = Access::User {
                    network_id: network.into(),
                    scope: crate::runtime::UserScope::Conversation(scope.into()),
                };
                if kind == "resolve" {
                    return self.resolve_check(&access, input).await;
                }
                self.require_scope(&access, scope).await?;
                self.cancel_local_manager(network, scope, field(input, "trigger")?)
                    .await?
                    .context("this reply is not scheduled on this device")
            }
            _ => bail!("unknown peer request"),
        }
    }

    async fn sync_membership(&self, network: &str, recipient: &str) -> Result<NetworkState> {
        let state = self.membership_state().await?;
        ensure!(
            state.genesis.network_id == network,
            "wrong personal network"
        );
        // A request may have started before revocation; check the snapshot being sent.
        state.project()?.node(recipient)?;
        Ok(state)
    }

    async fn hello(&self, network: &str) -> Result<Value> {
        let state = self.membership_state().await?;
        let (heads, forks) = {
            let network = network.to_owned();
            self.store
                .call("peers.hello", move |store| {
                    Ok((store.heads(&network)?, store.fork_digest(&network)?))
                })
                .await?
        };
        Ok(
            json!({"type":"hello","network_id":network,"vector":heads,"boot":self.boot,
            "membership_epoch":state.epoch(),"membership":membership_digest(&state)?,"forks":forks}),
        )
    }

    /// Whether differing fork digests are new for `peer`; remembers them if so.
    fn note_forks(&self, peer: &str, theirs: &Value, ours: &Value) -> bool {
        let pair = Some(format!("{theirs}/{ours}"));
        let mut peers = self.sync_peers.lock().expect("sync peers");
        let known = &mut peers.entry(peer.to_owned()).or_default().forks;
        let new = *known != pair;
        *known = pair;
        new
    }

    async fn pull(
        &self,
        network: &str,
        peer: &str,
        author: &str,
        from_seq: u64,
        limit: usize,
    ) -> Result<Vec<SignedEvent>> {
        let reply = self
            .net()?
            .request(
                peer,
                net::SYNC,
                &json!({"type":"pull","network_id":network,"author":author,
                    "from_seq":from_seq,"limit":limit}),
                PEER_TIMEOUT,
            )
            .await?;
        Ok(serde_json::from_value(reply["events"].clone())?)
    }

    async fn handle_sync(&self, network: &str, origin: &str, input: &Value) -> Result<Value> {
        if let Some(boot) = input["boot"].as_str() {
            self.note_boot(origin, boot);
        }
        match field(input, "type")? {
            "hello" => {
                let ours = self.hello(network).await?;
                let theirs: BTreeMap<String, (u64, String)> =
                    serde_json::from_value(input["vector"].clone()).context("invalid vector")?;
                // A head we lack is either further along or on another branch of that log.
                let behind = {
                    let network = network.to_owned();
                    self.store
                        .call("peers.behind", move |store| {
                            theirs.values().try_fold(false, |behind, (_, hash)| {
                                anyhow::Ok(behind || store.event(&network, hash)?.is_none())
                            })
                        })
                        .await?
                };
                let forks = input["forks"] != ours["forks"]
                    && self.note_forks(origin, &input["forks"], &ours["forks"]);
                if behind || forks || input["membership"] != ours["membership"] {
                    self.wake_sync(origin);
                }
                Ok(ours)
            }
            "membership" => {
                let incoming: NetworkState =
                    serde_json::from_value(input["state"].clone()).context("invalid membership")?;
                ensure!(
                    incoming.genesis.network_id == network,
                    "wrong membership network"
                );
                self.install_membership(incoming).await?;
                Ok(json!({"state":self.sync_membership(network, origin).await?}))
            }
            "forks" => {
                let after: Option<(String, u64, String)> =
                    serde_json::from_value(input["after"].clone())
                        .context("invalid fork cursor")?;
                let events = {
                    let network = network.to_owned();
                    self.store
                        .call("peers.fork_events", move |store| {
                            store.fork_events(&network, after.as_ref())
                        })
                        .await
                }?;
                Ok(json!({"events":events}))
            }
            "pull" => {
                let author = field(input, "author")?;
                let from = input["from_seq"].as_u64().context("from_seq is required")?;
                let limit = input["limit"]
                    .as_u64()
                    .unwrap_or(crate::store::MAX_PULL_EVENTS as u64)
                    as usize;
                let events = {
                    let arg0 = (network).to_owned();
                    let arg1 = (author).to_owned();
                    let arg2 = from.max(1);
                    let arg3 = limit;
                    self.store
                        .call("peers.events_from", move |store| {
                            store.events_from(&arg0, &arg1, arg2, arg3)
                        })
                        .await
                }?;
                Ok(json!({"author":author,"events":events}))
            }
            "have" => {
                let author = field(input, "author")?;
                let seq = input["seq"].as_u64().context("seq is required")?;
                let local = {
                    let arg0 = (network).to_owned();
                    self.store
                        .call("peers.heads", move |store| store.heads(&arg0))
                        .await
                }?;
                if local.get(author).map(|head| head.0).unwrap_or(0) < seq {
                    self.wake_sync(origin);
                }
                Ok(json!({}))
            }
            _ => bail!("unknown sync request"),
        }
    }

    /// A peer announcing a new boot has restarted: requests on our old connections to it would
    /// hang until QUIC's idle timeout, so drop them and sync again now.
    fn note_boot(&self, peer: &str, boot: &str) {
        let restarted = {
            let mut peers = self.sync_peers.lock().expect("sync peers");
            let known = &mut peers.entry(peer.to_owned()).or_default().boot;
            let restarted = known.as_deref().is_some_and(|known| known != boot);
            *known = Some(boot.to_owned());
            restarted
        };
        if restarted {
            if let Ok(net) = self.net() {
                net.peer_restarted(peer);
            }
            self.wake_sync(peer);
        }
    }

    /// Asks the sync loop to run with `peer` now, or again right after a run in progress.
    pub(super) fn wake_sync(&self, peer: &str) {
        let mut peers = self.sync_peers.lock().expect("sync peers");
        let entry = peers.entry(peer.to_owned()).or_default();
        entry.due = Some(Instant::now());
        entry.rerun |= entry.running;
        drop(peers);
        self.sync_wakeup.notify_one();
    }

    pub(crate) fn wake_sync_all(&self) {
        let mut peers = self.sync_peers.lock().expect("sync peers");
        for entry in peers.values_mut() {
            entry.due = Some(Instant::now());
            entry.rerun |= entry.running;
        }
        drop(peers);
        self.sync_wakeup.notify_one();
    }

    async fn sync_loop(self: Arc<Self>) {
        let Ok(net) = self.net().cloned() else { return };
        let mut joined = Vec::new();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5)) => {},
                _ = self.sync_wakeup.notified() => {},
            }
            if let Some(mesh) = net.mesh() {
                mesh.set_roles(self.mesh_roles());
            }
            let Ok(state) = self.membership_state().await else {
                continue;
            };
            let Ok(view) = state.project() else { continue };
            let local = self.identity.device_id().unwrap_or_default();
            if view.revocation(&local).is_some() {
                net.refresh_lan(None, &[]);
                continue;
            }
            let network = view.network_id.clone();
            // A new path (say, the relay came up) reaches peers now, not at the next Hello.
            match self.refresh_own_address(&network).await {
                Ok(true) => self.wake_sync_all(),
                Ok(false) => {}
                Err(error) => tracing::debug!(%error, "address record refresh pending"),
            }
            let now = Instant::now();
            let peers: Vec<String> = view
                .members
                .iter()
                .filter(|member| {
                    member.kind == IdentityKind::Node
                        && member.device_id != local
                        && view.cutoff(&member.device_id).is_none()
                })
                .map(|member| member.device_id.clone())
                .collect();
            // The loop also rolls the LAN tag over each hour.
            net.refresh_lan(Some(&network), &peers);
            if let Some(mesh) = net.mesh()
                && joined != peers
            {
                mesh.join_peers(&peers).await;
                joined.clone_from(&peers);
            }
            for peer in peers {
                {
                    let mut states = self.sync_peers.lock().expect("sync peers");
                    let entry = states.entry(peer.clone()).or_default();
                    if entry.running || entry.due.is_some_and(|due| due > now) {
                        continue;
                    }
                    entry.running = true;
                    entry.rerun = false;
                }
                let node = self.clone();
                let network = network.clone();
                let tracked_peer = peer.clone();
                if let Err(error) = self.background_tasks.spawn(async move {
                    let result = node.sync_peer(&network, &peer).await;
                    let (rerun, presence_changed) = {
                        let mut states = node.sync_peers.lock().expect("sync peers");
                        let entry = states.entry(peer.clone()).or_default();
                        entry.running = false;
                        let was_failing = entry.failures > 0;
                        // The first sync since start and every change after it, not every round.
                        match &result {
                            Ok(_) if was_failing || entry.synced_ms.is_none() => {
                                tracing::info!(event = "peer.online", %peer, "a member Node is reachable")
                            }
                            Err(error) if !was_failing => {
                                tracing::info!(event = "peer.offline", %peer, %error, "a member Node is unreachable")
                            }
                            _ => {}
                        }
                        match &result {
                            Ok(more) => {
                                entry.failures = 0;
                                entry.synced_ms = Some(now_ms());
                                entry.due = Some(Instant::now() + HELLO_INTERVAL);
                                entry.rerun |= *more;
                            }
                            Err(error) => {
                                entry.failures = entry.failures.saturating_add(1);
                                let wait = Duration::from_secs(1 << entry.failures.min(5));
                                entry.due = Some(Instant::now() + wait.min(RETRY_LIMIT));
                                tracing::debug!(%peer, %error, "sync with peer pending");
                            }
                        }
                        let rerun = std::mem::take(&mut entry.rerun);
                        if rerun {
                            entry.due = Some(Instant::now());
                        }
                        (rerun, was_failing != (entry.failures > 0))
                    };
                    if presence_changed {
                        node.publish_reread(&network);
                    }
                    if result.is_ok() {
                        node.outbox_wakeup.notify_one();
                    }
                    if result.is_err() {
                        node.align(&network).await;
                    }
                    if rerun {
                        node.sync_wakeup.notify_one();
                    }
                }) {
                    tracing::debug!(%tracked_peer, %error, "peer sync was not started while the Node is shutting down");
                }
            }
        }
    }

    /// One `Hello` round with `peer`: align membership, swap address books, then pull every
    /// author range the peer has beyond ours. Returns whether the round made progress and left
    /// history the next round can still pull.
    pub(super) async fn sync_peer(self: &Arc<Self>, network: &str, peer: &str) -> Result<bool> {
        let net = self.net()?.clone();
        let ours = self.hello(network).await?;
        let theirs = match net.request(peer, net::SYNC, &ours, PEER_TIMEOUT).await {
            Ok(reply) => reply,
            Err(error) => {
                if self
                    .member_node(peer)
                    .await
                    .is_ok_and(|member| member == network)
                {
                    // A private removal proof may be unavailable. A currently authorized peer
                    // can still receive our signed history and learn a late veto.
                    let state = self.sync_membership(network, peer).await?;
                    let reply = net
                        .request(
                            peer,
                            net::SYNC,
                            &json!({"type":"membership","network_id":network,"state":state}),
                            PEER_TIMEOUT,
                        )
                        .await?;
                    let incoming: NetworkState = serde_json::from_value(reply["state"].clone())
                        .context("peer membership exchange had no valid state")?;
                    ensure!(
                        incoming.genesis.network_id == network,
                        "wrong membership network"
                    );
                    self.install_membership(incoming).await?;
                    if reply["type"] == "revoked" {
                        net.forget_sync_connection(peer);
                    }
                }
                return Err(error);
            }
        };
        if theirs["type"] == "revoked" {
            let removed = self.accept_removal(network, &theirs).await;
            if removed.is_err() {
                // Still a member here: a late veto may have put back a directory the peer has
                // not seen, so it gets this history to roll back too.
                let state = self.sync_membership(network, peer).await?;
                net.request(
                    peer,
                    net::SYNC,
                    &json!({"type":"membership","network_id":network,"state":state}),
                    PEER_TIMEOUT,
                )
                .await?;
                // The peer decided how to serve this connection when it took it; the next
                // round dials again.
                net.forget_sync_connection(peer);
            }
            return removed.map(|()| false);
        }
        ensure!(
            theirs["network_id"].as_str() == Some(network),
            "peer answered for another personal network"
        );
        if theirs["membership"] != ours["membership"] {
            let state = self.sync_membership(network, peer).await?;
            let reply = net
                .request(
                    peer,
                    net::SYNC,
                    &json!({"type":"membership","network_id":network,"state":state}),
                    PEER_TIMEOUT,
                )
                .await?;
            let incoming: NetworkState = serde_json::from_value(reply["state"].clone())?;
            self.install_membership(incoming).await?;
        }
        if let Err(error) = self.exchange_addresses(network, peer).await {
            tracing::debug!(%peer, %error, "address book exchange pending");
        }
        let vector: BTreeMap<String, (u64, String)> =
            serde_json::from_value(theirs["vector"].clone()).context("invalid peer vector")?;
        let before = {
            let arg0 = (network).to_owned();
            self.store
                .call("peers.heads", move |store| store.heads(&arg0))
                .await
        }?;
        for _pass in 0..4 {
            let mut progressed = false;
            for (author, (seq, _)) in &vector {
                loop {
                    let local = self.local_seq(network, author).await?;
                    if local >= *seq {
                        break;
                    }
                    let events = self
                        .pull(
                            network,
                            peer,
                            author,
                            local + 1,
                            crate::store::MAX_PULL_EVENTS,
                        )
                        .await?;
                    if events.is_empty() {
                        break;
                    }
                    self.ingest(network, &events).await?;
                    if self.local_seq(network, author).await? == local {
                        break;
                    }
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
        for (author, head) in &vector {
            let our_max = self.local_seq(network, author).await?;
            reconcile_log(
                our_max,
                (head.0, &head.1),
                |hash| async move {
                    Ok({
                        let arg0 = (network).to_owned();
                        let arg1 = (hash).to_owned();
                        self.store
                            .call("peers.event", move |store| store.event(&arg0, &arg1))
                            .await
                    }?
                    .is_some())
                },
                |from, limit| self.pull(network, peer, author, from, limit),
                |events| async move { self.ingest(network, &events).await },
            )
            .await?;
        }
        let forks_left = theirs["forks"]
            != json!({
                let arg0 = (network).to_owned();
                self.store
                    .call("peers.fork_digest", move |store| store.fork_digest(&arg0))
                    .await
            }?)
            && self.pull_forks(network, peer).await?;
        let heads = {
            let arg0 = (network).to_owned();
            self.store
                .call("peers.heads", move |store| store.heads(&arg0))
                .await
        }?;
        let behind = vector
            .iter()
            .any(|(author, (seq, _))| heads.get(author).map_or(0, |head| head.0) < *seq);
        self.check_own_log(network, peer, &vector).await?;
        if let Err(error) = self.fetch_blobs(network, peer).await {
            tracing::debug!(%peer, %error, "attachment download pending");
        }
        Ok(forks_left || (behind && heads != before))
    }

    /// Reads up to `MAX_FORK_PAGES` pages of the peer's fork evidence from where the last round
    /// stopped. Returns whether more is left.
    async fn pull_forks(self: &Arc<Self>, network: &str, peer: &str) -> Result<bool> {
        let cursor = |after: Option<ForkKey>| {
            self.sync_peers
                .lock()
                .expect("sync peers")
                .entry(peer.to_owned())
                .or_default()
                .fork_after = after;
        };
        let mut after = self
            .sync_peers
            .lock()
            .expect("sync peers")
            .get(peer)
            .and_then(|state| state.fork_after.clone());
        for _ in 0..MAX_FORK_PAGES {
            let reply = self
                .net()?
                .request(
                    peer,
                    net::SYNC,
                    &json!({"type":"forks","network_id":network,"after":after}),
                    PEER_TIMEOUT,
                )
                .await?;
            let events: Vec<SignedEvent> = serde_json::from_value(reply["events"].clone())?;
            let Some(last) = events.last() else {
                cursor(None);
                return Ok(false);
            };
            let event: Event = serde_json::from_str(&last.canonical_body)?;
            after = Some((event.author, event.seq, last.hash.clone()));
            self.ingest(network, &events).await?;
            cursor(after.clone());
        }
        Ok(true)
    }

    /// The peer's capabilities: `answer` when it gave one, otherwise the last it gave, kept in
    /// this Node's own store so a restart still shows it while the peer is offline.
    pub(crate) async fn peer_capabilities(&self, peer: &str, answer: Option<Value>) -> Value {
        let Some(answer) = answer else {
            let known = self
                .sync_peers
                .lock()
                .expect("sync peers")
                .get(peer)
                .and_then(|state| state.capabilities.clone());
            if let Some(known) = known {
                return known;
            }
            return {
                let arg0 = (peer).to_owned();
                self.store
                    .call("peers.peer_capabilities", move |store| {
                        store.peer_capabilities(&arg0)
                    })
                    .await
            }
            .ok()
            .flatten()
            .unwrap_or(Value::Null);
        };
        let changed = {
            let mut states = self.sync_peers.lock().expect("sync peers");
            let known = &mut states.entry(peer.to_owned()).or_default().capabilities;
            let changed = known.as_ref() != Some(&answer);
            *known = Some(answer.clone());
            changed
        };
        if changed
            && let Err(error) = {
                let arg0 = (peer).to_owned();
                let arg1 = (answer).to_owned();
                self.store
                    .call("peers.keep_peer_capabilities", move |store| {
                        store.keep_peer_capabilities(&arg0, &arg1)
                    })
                    .await
            }
        {
            tracing::debug!(%error, "peer capabilities kept in memory only");
        }
        answer
    }

    /// Whether the last sync with `peer` failed.
    pub(crate) fn sync_failing(&self, peer: &str) -> bool {
        self.peer_sync_status(peer).0
    }

    /// Whether the last attempt failed and when the last successful sync finished.
    pub(super) fn peer_sync_status(&self, peer: &str) -> (bool, Option<i64>) {
        let states = self.sync_peers.lock().expect("sync peers");
        states
            .get(peer)
            .map_or((false, None), |state| (state.failures > 0, state.synced_ms))
    }

    /// Writable once every other current member Node has confirmed this device's log since
    /// boot; read-only, until the user confirms, once every one that has not is failing.
    pub(super) async fn align(&self, network: &str) {
        let Ok((_, peers)) = self.member_peers().await else {
            return;
        };
        let unconfirmed: Vec<u32> = {
            let states = self.sync_peers.lock().expect("sync peers");
            peers
                .iter()
                .filter_map(|peer| match states.get(peer) {
                    Some(state) if state.confirmed => None,
                    state => Some(state.map_or(0, |state| state.failures)),
                })
                .collect()
        };
        if unconfirmed.is_empty() {
            let from = [WriteState::Aligning, WriteState::ReadOnly];
            self.set_write_state(network, &from, WriteState::Writable)
                .await;
        } else if unconfirmed.iter().all(|failures| *failures > 0) {
            self.set_write_state(network, &[WriteState::Aligning], WriteState::ReadOnly)
                .await;
        }
    }

    /// Started from the database file it last wrote from, this Node holds every event it wrote
    /// and writes right away, as it does with no other member Node. A new file (a copy, or a
    /// restored backup) waits to learn from the others that it holds every event it wrote.
    pub(super) async fn align_on_start(&self) -> Result<bool> {
        if !self.has_membership().await? {
            self.store
                .call("peers.record_write_file", move |store| {
                    store.record_write_file()
                })
                .await?;
            return Ok(true);
        }
        let (network, peers) = self.member_peers().await?;
        let (forked, same_file) = {
            let network = network.clone();
            let local = self.identity.device_id()?;
            self.store
                .call("peers.alignment", move |store| {
                    Ok((
                        store.has_conflicts_by(&network, &local)?,
                        store.wrote_from_this_file()?,
                    ))
                })
                .await?
        };
        let state = match (forked, same_file || peers.is_empty()) {
            (true, _) => WriteState::Forked,
            (false, true) => WriteState::Writable,
            (false, false) => WriteState::Aligning,
        };
        if state == WriteState::Writable {
            self.store
                .call("peers.record_write_file", move |store| {
                    store.record_write_file()
                })
                .await?;
        }
        let from = [
            WriteState::Writable,
            WriteState::Aligning,
            WriteState::ReadOnly,
        ];
        self.set_write_state(&network, &from, state).await;
        Ok(state == WriteState::Writable)
    }

    /// Compares what `peer` holds of this device's log with ours, after the walk-back pulled any
    /// branch it has. A different event at the same sequence means another copy of this data
    /// directory wrote it: that copy is kept aside beside ours, and this Node stops writing.
    async fn check_own_log(
        &self,
        network: &str,
        peer: &str,
        vector: &BTreeMap<String, (u64, String)>,
    ) -> Result<()> {
        let own = self.identity.device_id()?;
        let local = self.local_seq(network, &own).await?;
        let confirmed = match vector.get(&own) {
            None => true,
            Some((seq, hash)) => {
                *seq <= local && {
                    let arg0 = (network).to_owned();
                    let arg1 = (own).to_owned();
                    let arg2 = *seq;
                    self.store
                        .call("peers.hash_at", move |store| {
                            store.hash_at(&arg0, &arg1, arg2)
                        })
                        .await
                }?
                .as_ref()
                    == Some(hash)
            }
        };
        if {
            let arg0 = (network).to_owned();
            let arg1 = (own).to_owned();
            self.store
                .call("peers.has_conflicts_by", move |store| {
                    store.has_conflicts_by(&arg0, &arg1)
                })
                .await
        }? {
            let from = [
                WriteState::Aligning,
                WriteState::ReadOnly,
                WriteState::Writable,
            ];
            if self
                .set_write_state(network, &from, WriteState::Forked)
                .await
            {
                tracing::warn!(event = "sync.forked", %peer, "another copy of this device wrote different events; writes stopped");
            }
        } else if confirmed {
            self.sync_peers
                .lock()
                .expect("sync peers")
                .entry(peer.to_owned())
                .or_default()
                .confirmed = true;
            self.align(network).await;
        }
        Ok(())
    }

    /// Moves to `to` from one of `from`; returns whether it did.
    pub(crate) async fn set_write_state(
        &self,
        network: &str,
        from: &[WriteState],
        to: WriteState,
    ) -> bool {
        // Registration and configuration use controls -> StoreActor. The accepted actor
        // operation must close the gate and cancel invocations even if its caller is dropped.
        let controls = self.manager_controls.lock().await;
        let active: Vec<_> = controls.values().cloned().collect();
        let write_state = self.write_state.clone();
        let epoch = self.local_permission_epoch.clone();
        let identity = self.identity.clone();
        let from = from.to_vec();
        let changed = self.store.call("peers.set_write_state", move |store| {
            let current = *write_state.borrow();
            if current == to || !from.contains(&current) {
                return Ok(false);
            }
            if to == WriteState::Writable
                && epoch.load(std::sync::atomic::Ordering::Acquire) == u64::MAX
            {
                tracing::warn!("execution permission epoch exhausted; write authority remains closed");
                return Ok(false);
            }
            let recorded = match to {
                WriteState::Writable => store.record_write_file(),
                _ => Ok(()),
            };
            if let Err(error) = recorded.and_then(|()| store.set_write_state(to)) {
                tracing::warn!(event = "sync.write_state_pending", %error, "write state change pending");
                return Ok(false);
            }
            write_state.send_replace(to);
            if to != WriteState::Writable {
                for invocation in active {
                    invocation.fenced.store(true, std::sync::atomic::Ordering::Release);
                    invocation.cancel.send_replace(true);
                }
                if current == WriteState::Writable {
                    let mut observed = epoch.load(std::sync::atomic::Ordering::Acquire);
                    while let Some(next) = observed.checked_add(1) {
                        match epoch.compare_exchange_weak(
                            observed,
                            next,
                            std::sync::atomic::Ordering::AcqRel,
                            std::sync::atomic::Ordering::Acquire,
                        ) {
                            Ok(_) => break,
                            Err(actual) => observed = actual,
                        }
                    }
                }
                let held = identity.device_id().and_then(|local| Self::hold_execution_fence_stops(store, &local));
                if let Err(error) = held {
                    tracing::warn!(event = "execution.fence_pending", %error, "local stop records will be retried");
                }
            }
            Ok(true)
        }).await;
        drop(controls);
        match changed {
            Ok(true) => {}
            Ok(false) => return false,
            Err(error) => {
                tracing::warn!(event = "sync.write_state_pending", %error, "write state change pending");
                return false;
            }
        }
        self.schedule_wakeup.notify_one();
        tracing::info!(event = "sync.write_state", state = ?to, "write state changed");
        let _ = self.events.send(Update {
            network_id: network.into(),
            payload: json!({"type":"write_state","state":to}),
        });
        true
    }

    /// Each other member Node with whether the last sync reached it and when one last finished.
    pub(crate) async fn peer_health(&self) -> Vec<crate::maintenance::PeerHealth> {
        let Ok(view) = self
            .membership_state()
            .await
            .and_then(|state| state.project())
        else {
            return Vec::new();
        };
        let local = self.identity.device_id().unwrap_or_default();
        let states = self.sync_peers.lock().expect("sync peers");
        view.members
            .iter()
            .filter(|member| {
                member.kind == IdentityKind::Node
                    && member.device_id != local
                    && view.cutoff(&member.device_id).is_none()
            })
            .map(|member| {
                let state = states.get(&member.device_id);
                crate::maintenance::PeerHealth {
                    device_id: member.device_id.clone(),
                    name: member.name.clone(),
                    reachable: state
                        .filter(|state| state.failures > 0 || state.synced_ms.is_some())
                        .map(|state| state.failures == 0),
                    synced_ms: state.and_then(|state| state.synced_ms),
                    path: self
                        .net
                        .get()
                        .and_then(|net| net.path_kind(&member.device_id)),
                }
            })
            .collect()
    }

    /// The troubleshooting export: versions, configuration summary, write and peer
    /// state, the structured log over a time range (the last day by default), and the end of the
    /// service's stderr, both redacted.
    pub(crate) async fn diagnostics(
        &self,
        network: &str,
        (since_ms, until_ms): (Option<i64>, Option<i64>),
    ) -> Result<Value> {
        let conflicts = {
            let arg0 = (network).to_owned();
            self.store
                .read("peers.conflicts", move |store| store.conflicts(&arg0))
                .await
        }?
        .len();
        let until_ms = until_ms.unwrap_or_else(now_ms);
        let since_ms = since_ms.unwrap_or(until_ms - 86_400_000);
        let filter = crate::logging::Filter {
            since_ms: Some(since_ms),
            until_ms: Some(until_ms),
            limit: Some(crate::logging::MAX_READ),
            ..Default::default()
        };
        let dir = self.data_dir.clone();
        let records =
            tokio::task::spawn_blocking(move || crate::logging::read(&dir, &filter)).await??;
        let secrets: Vec<String> = self.relay_tokens().cloned().collect();
        let stderr: Vec<String> = std::fs::read_to_string(self.data_dir.join("service.stderr.log"))
            .unwrap_or_default()
            .lines()
            .rev()
            .take(50)
            .map(|line| crate::logging::redact(line, &secrets))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let settings = self.settings_snapshot();
        let peers: Vec<Value> = self.peer_health().await.into_iter().map(|peer| {
            json!({"device_id":peer.device_id,"reachable":peer.reachable,"synced_ms":peer.synced_ms})
        }).collect();
        Ok(json!({
            "generated_at_ms":now_ms(), "version":env!("CARGO_PKG_VERSION"),
            "os":std::env::consts::OS, "arch":std::env::consts::ARCH,
            "device_id":self.identity.device_id()?, "config":{
                "project_roots":settings.roots.len(),
                "manager_enabled":settings.manager.is_some(),"execution":self.providers().switches.execution,
                "session_scanning":self.triage.config().scan_codex_sessions},
            "write_state":*self.write_state.borrow(), "conflicts":conflicts,
            "triage":self.triage_status().await?,
            "peers":peers,
            "range":{"since_ms":since_ms,"until_ms":until_ms},
            "logs":{"node.log":records,"service.stderr.log":stderr},
        }))
    }

    /// The write state, and the conflicts kept aside here, for the web.
    pub(crate) async fn write_status(&self, network: &str) -> Result<Value> {
        let state = *self.write_state.borrow();
        let conflicts = {
            let arg0 = (network).to_owned();
            self.store
                .read("peers.conflicts", move |store| store.conflicts(&arg0))
                .await
        }?;
        Ok(json!({"state":state,"conflicts":conflicts}))
    }

    /// The user's word that this data directory was not restored from a backup. Recorded only
    /// here, never in the shared log.
    pub(crate) async fn confirm_writable(&self, network: &str, device: &str) -> Result<()> {
        ensure!(
            *self.write_state.borrow() != WriteState::Forked,
            "this Node's log has forked and it no longer writes"
        );
        let from = [WriteState::Aligning, WriteState::ReadOnly];
        if self
            .set_write_state(network, &from, WriteState::Writable)
            .await
        {
            let device_record = device.to_owned();
            self.store.call("peers.confirm_writable", move |store| {
                store.connection.execute(
                    "INSERT INTO identity_meta(key,value) VALUES('write-confirmed',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    [json!({"device":device_record,"at_ms":now_ms()}).to_string()],
                )?;
                Ok(())
            }).await?;
            tracing::info!(
                event = "sync.write_confirmed",
                device,
                "write confirmed without a backup check"
            );
        }
        Ok(())
    }

    async fn local_seq(&self, network: &str, author: &str) -> Result<u64> {
        Ok({
            let arg0 = (network).to_owned();
            self.store
                .call("peers.heads", move |store| store.heads(&arg0))
                .await
        }?
        .get(author)
        .map(|head| head.0)
        .unwrap_or(0))
    }

    async fn ingest(self: &Arc<Self>, network: &str, events: &[SignedEvent]) -> Result<()> {
        let ingested = {
            let network = network.to_owned();
            let events = events.to_vec();
            self.store
                .call("peers.ingest", move |store| {
                    let ingested = store.ingest_events(&network, &events)?;
                    store.settle_local_works(&network)?;
                    Ok(ingested)
                })
                .await?
        };
        if ingested.rebuilt {
            self.publish_reread(network);
        }
        for (event, status) in events.iter().zip(&ingested.statuses) {
            match status {
                IngestStatus::Rejected(reason) => {
                    tracing::debug!(hash = %event.hash, reason, "peer event rejected")
                }
                IngestStatus::Conflict => {
                    tracing::warn!(event = "sync.fork_evidence", hash = %event.hash, "forked event stored as evidence")
                }
                IngestStatus::Accepted | IngestStatus::Pending | IngestStatus::Duplicate => {}
            }
        }
        for HashedEvent { hash, event } in ingested.stored {
            match event.body {
                EventBody::BotDeleted { .. } => {
                    self.bot_deleted(network, &event.scope_id, &hash);
                    continue;
                }
                // A move stops this Node's turn in that conversation without delay.
                EventBody::ConversationOwner { .. } => self.schedule_wakeup.notify_one(),
                _ => {}
            }
            let _ = self.events.send(Update {
                network_id: network.into(),
                payload: json!({"type":"changed","conversation_id":event.scope_id,"event_hash":hash}),
            });
        }
        Ok(())
    }

    async fn have_loop(self: Arc<Self>) {
        let mut announced = 0;
        loop {
            self.have_wakeup.notified().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let Ok(state) = self.membership_state().await else {
                continue;
            };
            let Ok(view) = state.project() else { continue };
            let Ok(local) = self.identity.device_id() else {
                continue;
            };
            let head = {
                let arg0 = (view.network_id).to_owned();
                self.store
                    .call("peers.heads", move |store| store.heads(&arg0))
                    .await
            }
            .ok()
            .and_then(|heads| heads.get(&local).cloned());
            let Some((seq, hash)) = head else { continue };
            if seq == announced {
                continue;
            }
            announced = seq;
            let Ok(net) = self.net() else { continue };
            let have = json!({"type":"have","network_id":view.network_id,"author":local,"seq":seq,
                "hash":hash,"boot":self.boot});
            for member in view.members.iter().filter(|member| {
                member.kind == IdentityKind::Node
                    && member.device_id != local
                    && view.cutoff(&member.device_id).is_none()
            }) {
                let (net, peer, have) = (net.clone(), member.device_id.clone(), have.clone());
                let tracked_peer = peer.clone();
                if let Err(error) = self.background_tasks.spawn(async move {
                    if let Err(error) = net
                        .request(&peer, net::SYNC, &have, Duration::from_secs(10))
                        .await
                    {
                        tracing::debug!(%peer, %error, "have notice not delivered; Hello covers it");
                    }
                }) {
                    tracing::debug!(%tracked_peer, %error, "have notice skipped while the Node is shutting down");
                }
            }
        }
    }

    /// Other member Nodes this one can ask right now.
    async fn member_peers(&self) -> Result<(String, Vec<String>)> {
        let view = self.membership_state().await?.project()?;
        let local = self.identity.device_id()?;
        let peers = view
            .members
            .iter()
            .filter(|member| {
                member.kind == IdentityKind::Node
                    && member.device_id != local
                    && view.cutoff(&member.device_id).is_none()
            })
            .map(|member| member.device_id.clone())
            .collect();
        Ok((view.network_id, peers))
    }

    /// Join requests waiting on other member Nodes; an unreachable Node simply lists none.
    pub(crate) async fn remote_joins(&self, admin: &str) -> Vec<Value> {
        let Ok((network, peers)) = self.member_peers().await else {
            return vec![];
        };
        let asks = peers.iter().map(|peer| {
            let request = self.peer_request(
                &network,
                peer,
                json!({"type":"pending_joins","admin":admin}),
            );
            async move { tokio::time::timeout(Duration::from_secs(3), request).await }
        });
        futures_util::future::join_all(asks)
            .await
            .into_iter()
            .filter_map(|reply| reply.ok()?.ok())
            .flat_map(|reply| reply["requests"].as_array().cloned().unwrap_or_default())
            .collect()
    }

    /// A join this Node holds changed. Clients here reread, and so do those of reachable member
    /// Nodes, which list it through `pending_joins`; the notice carries nothing of the request.
    pub(crate) fn join_changed(self: &Arc<Self>, network: &str) {
        self.publish_reread(network);
        let (node, network) = (self.clone(), network.to_owned());
        if let Err(error) = self.background_tasks.spawn(async move {
            let Ok((_, peers)) = node.member_peers().await else {
                return;
            };
            let notify = async |peers: Vec<String>, wait| {
                let notices = peers.iter().map(|peer| {
                    let notice = node.peer_request(&network, peer, json!({"type":"join_changed"}));
                    tokio::time::timeout(wait, notice)
                });
                let answers = futures_util::future::join_all(notices).await;
                peers
                    .into_iter()
                    .zip(answers)
                    .filter(|(_, answer)| !matches!(answer, Ok(Ok(_))))
                    .map(|(peer, _)| peer)
                    .collect::<Vec<_>>()
            };
            let reachable = peers.into_iter().filter(|peer| !node.sync_failing(peer));
            let missed = notify(reachable.collect(), JOIN_NOTICE_WAIT).await;
            if !missed.is_empty() {
                // A Node that syncs fine but missed this notice is told once more, given the
                // time a slow path needs.
                tokio::time::sleep(JOIN_NOTICE_WAIT).await;
                notify(missed, PEER_TIMEOUT).await;
            }
        }) {
            tracing::debug!(%error, "join notice skipped while the Node is shutting down");
        }
    }

    /// An approval step for a join held elsewhere goes to the Node that holds it.
    pub(crate) async fn forward_join_approval(&self, admin: &str, params: &Value) -> Result<Value> {
        let (network, peers) = self.member_peers().await?;
        let join_id = field(params, "join_id")?;
        for peer in peers {
            let listed = self
                .peer_request(
                    &network,
                    &peer,
                    json!({"type":"pending_joins","admin":admin}),
                )
                .await;
            let holds = listed.is_ok_and(|reply| {
                reply["requests"]
                    .as_array()
                    .is_some_and(|requests| requests.iter().any(|r| r["join_id"] == join_id))
            });
            if holds {
                return self
                    .peer_request(
                        &network,
                        &peer,
                        json!({"type":"approve_join","admin":admin,"params":params}),
                    )
                    .await;
            }
        }
        bail!("join request not found")
    }

    /// Drops connections and addresses of Nodes the directory has revoked.
    pub(super) async fn forget_revoked(&self, view: &AuthorizationView) {
        for revoked in &view.revoked {
            if let Ok(net) = self.net() {
                net.forget(&revoked.device_id);
            }
            let _ = {
                let arg0 = (revoked.device_id).to_owned();
                self.store
                    .call("peers.forget_address", move |store| {
                        store.forget_address(&arg0)
                    })
                    .await
            };
        }
        self.unverify_revoked_works(&view.network_id).await;
    }

    /// Work on devices a manager removed that never reported is recorded as unverified by the
    /// owner, for the user to settle; requests still waiting for those devices end unknown.
    ///
    /// Only a signed removal counts. A Node a recovery left out is out only until a late veto
    /// puts the earlier directory back, and a record in the shared log cannot be taken back;
    /// its writes past the bound simply stop counting (`log::recompute`) until then.
    pub(super) async fn unverify_revoked_works(&self, network: &str) {
        let Ok(state) = self.membership_state().await else {
            return;
        };
        let Ok(view) = state.project() else {
            return;
        };
        let removed = view.signed_removals().to_vec();
        if removed.is_empty() {
            return;
        }
        let result = {
            let arg0 = (self.identity).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = (removed).to_owned();
            self.store
                .call("peers.unverify_removed_works", move |store| {
                    store.unverify_removed_works(&arg0, &arg1, &arg2, now_ms())
                })
                .await
        };
        match result {
            Ok(unverified) => {
                for (scope, work_id) in &unverified {
                    self.publish_changed(network, scope, Changed::Work(work_id));
                }
                if !unverified.is_empty() {
                    self.outbox_wakeup.notify_one();
                }
            }
            Err(error) => {
                tracing::warn!(event = "devices.removed_work_pending", %error, "recording work on removed devices is pending")
            }
        }
        let Ok(local) = self.identity.device_id() else {
            return;
        };
        let dropped = {
            let arg0 = (network).to_owned();
            let arg1 = (local).to_owned();
            let arg2 = (removed).to_owned();
            self.store
                .call("peers.drop_requests_to", move |store| {
                    store.drop_requests_to(&arg0, &arg1, &arg2)
                })
                .await
        };
        match dropped {
            Ok(0) => {}
            Ok(_) => self.schedule_wakeup.notify_one(),
            Err(error) => {
                tracing::warn!(event = "devices.removed_requests_pending", %error, "dropping requests to removed devices is pending")
            }
        }
    }

    /// Where browsers reach this Node: its own URL, when it serves the web app.
    pub(crate) fn gateway_url(&self) -> Option<String> {
        self.config
            .web_enabled
            .then(|| self.config.public_url.clone())
    }

    /// What this Node offers on the mesh: a gateway when its web app is at a public HTTPS origin
    /// and forwards for any device, and its delivery service.
    pub(crate) fn mesh_roles(&self) -> Vec<net::mesh::record::Role> {
        let url = &self.config.public_url;
        let reachable = reqwest::Url::parse(url).is_ok_and(|parsed| {
            self.config.network.mesh.allow_private || net::mesh::public_host(&parsed)
        });
        let gateway = (self.config.web_enabled
            && self.config.forwarding.any
            && reachable
            && net::mesh::record::gateway_url(url).is_ok())
        .then(|| net::mesh::record::Role::Gateway { url: url.clone() });
        gateway
            .into_iter()
            .chain(self.relay_role())
            .chain(self.delivery.mesh_role())
            .collect()
    }

    /// Saves the bootstrap nodes `vibeface mesh bootstrap` changed and joins through them.
    pub(crate) async fn edit_mesh_bootstrap(
        &self,
        add: &[String],
        remove: &[String],
    ) -> Result<()> {
        let bootstrap = {
            let _save = self.config_save.lock().await;
            let mut config = crate::config::Config::load(&self.data_dir)?;
            config.network.mesh.edit_bootstrap(add, remove)?;
            config.save(&self.data_dir)?;
            config.network.mesh.bootstrap
        };
        if let Some(mesh) = self.net().ok().and_then(|net| net.mesh()) {
            mesh.set_bootstrap(bootstrap).await;
        }
        Ok(())
    }

    /// Re-signs this Node's record when its paths or Gateway changed; returns whether it did.
    async fn refresh_own_address(&self, network: &str) -> Result<bool> {
        let net = self.net()?;
        let device_id = self.identity.device_id()?;
        let endpoint = net.addr();
        let gateway_url = self.gateway_url();
        let relay = self.relay_config().own_relay()?.map(|url| url.to_string());
        let network = network.to_owned();
        let identity = self.identity.clone();
        self.store
            .call("peers.refresh_address", move |store| {
                let previous = store
                    .address_book()?
                    .into_iter()
                    .filter_map(|signed| signed.open(&network).ok())
                    .find(|record| record.device_id == device_id);
                if previous.as_ref().is_some_and(|record| {
                    record.endpoint == endpoint
                        && record.gateway_url == gateway_url
                        && record.relay == relay
                }) {
                    return Ok(false);
                }
                let record = AddressRecord {
                    version: 1,
                    network_id: network,
                    device_id,
                    seq: previous
                        .map(|record| record.seq + 1)
                        .unwrap_or(0)
                        .max(now_ms() as u64),
                    endpoint,
                    gateway_url,
                    relay,
                };
                let payload = serde_json::to_string(&record)?;
                let signed = SignedAddress {
                    signature: identity.sign_payload(net::ADDRESS_DOMAIN, payload.as_bytes())?,
                    payload,
                };
                store.save_address(&record.network_id, &record, &signed)?;
                Ok(true)
            })
            .await
    }

    async fn exchange_addresses(&self, network: &str, peer: &str) -> Result<()> {
        let records = self
            .store
            .call("peers.address_book", move |store| store.address_book())
            .await?;
        let reply = self
            .net()?
            .request(
                peer,
                net::RPC,
                &AddressExchange { records }.into_request(network)?,
                PEER_TIMEOUT,
            )
            .await?;
        let exchange = AddressExchange::decode(&reply)?;
        self.merge_addresses(network, exchange.records).await
    }

    /// Keeps records signed by current member Nodes; this Node's own record is its own to write.
    pub(crate) async fn merge_addresses(
        &self,
        network: &str,
        records: Vec<SignedAddress>,
    ) -> Result<()> {
        let view = self.membership_state().await?.project()?;
        let local = self.identity.device_id()?;
        let mut learned = false;
        for signed in records.into_iter().take(MAX_ADDRESS_RECORDS) {
            let Ok(record) = signed.open(network) else {
                continue;
            };
            if record.device_id == local || view.node(&record.device_id).is_err() {
                continue;
            }
            let changed = {
                let arg0 = (network).to_owned();
                let arg1 = (record).to_owned();
                let arg2 = (signed).to_owned();
                self.store
                    .call("peers.save_address", move |store| {
                        store.save_address(&arg0, &arg1, &arg2)
                    })
                    .await
            }?;
            if changed && let Ok(net) = self.net() {
                net.learn_member(&record).await;
            }
            learned |= changed;
        }
        if learned {
            self.wake_sync_all();
        }
        Ok(())
    }

    /// Includes signed withdrawals so browsers can discard previously cached web entries.
    pub(crate) async fn entrances(&self, network: &str) -> Result<Vec<SignedAddress>> {
        let view = self.membership_state().await?.project()?;
        Ok(self
            .store
            .call("peers.address_book", move |store| store.address_book())
            .await?
            .into_iter()
            .filter(|signed| {
                signed
                    .open(network)
                    .is_ok_and(|record| view.node(&record.device_id).is_ok())
            })
            .collect())
    }

    /// Verified public gateways on the mesh outside the personal network, at most 16.
    pub(crate) async fn mesh_gateways(&self) -> Result<Vec<Value>> {
        let Some(mesh) = self.net().ok().and_then(|net| net.mesh()) else {
            return Ok(vec![]);
        };
        let members: Vec<String> = match self.membership_state().await {
            Ok(state) => state
                .project()?
                .members
                .into_iter()
                .map(|member| member.device_id)
                .collect(),
            Err(_) => vec![],
        };
        Ok(mesh
            .gateways(&members)
            .await
            .into_iter()
            .map(|(device_id, url)| json!({"device_id":device_id,"url":url}))
            .collect())
    }

    /// Whether `origin` is the browser origin of a current member Gateway.
    pub(crate) async fn member_gateway_origin(&self, origin: &str) -> bool {
        let Ok(state) = self.membership_state().await else {
            return false;
        };
        let network = state.genesis.network_id;
        self.entrances(&network).await.is_ok_and(|entrances| {
            entrances.iter().any(|signed| {
                signed
                    .open(&network)
                    .ok()
                    .and_then(|record| record.gateway_url)
                    .and_then(|url| reqwest::Url::parse(&url).ok())
                    .is_some_and(|url| {
                        url.origin()
                            .ascii_serialization()
                            .eq_ignore_ascii_case(origin)
                    })
            })
        })
    }

    async fn serve_blobs(self: Arc<Self>, connection: Connection, network: String, remote: String) {
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            let (node, network, remote) = (self.clone(), network.clone(), remote.clone());
            let tracked_remote = remote.clone();
            if let Err(error) = self.background_tasks.spawn(async move {
                if let Err(error) = node
                    .send_blob(&network, &remote, &mut send, &mut recv)
                    .await
                {
                    let _ =
                        net::write_frame(&mut send, &json!({"error":format!("{error:#}")})).await;
                }
                send.finish().ok();
                let _ = send.stopped().await;
            }) {
                tracing::debug!(%tracked_remote, %error, "blob transfer skipped while the Node is shutting down");
            }
        }
    }

    async fn send_blob(
        &self,
        network: &str,
        remote: &str,
        send: &mut iroh::endpoint::SendStream,
        recv: &mut iroh::endpoint::RecvStream,
    ) -> Result<()> {
        use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
        let request = net::read_frame(recv).await?.context("empty blob request")?;
        ensure!(
            request["network_id"].as_str() == Some(network),
            "wrong personal network"
        );
        let hash = field(&request, "hash")?;
        let offset = request["offset"].as_u64().context("offset is required")?;
        let path = {
            let network = network.to_owned();
            let remote = remote.to_owned();
            let hash = hash.to_owned();
            let request = request.clone();
            self.store
                .call("peers.send_blob", move |store| {
                    store.authorize_node(&network, &remote)?;
                    let size = store
                        .referenced_blob(&network, &hash)?
                        .context("no stored event references this attachment")?;
                    ensure!(
                        request["size"].as_u64() == Some(size) && offset <= size,
                        "attachment size mismatch"
                    );
                    store.blob_file(&hash)
                })
                .await?
        };
        let mut file = tokio::fs::File::open(&path)
            .await
            .context("attachment content is not on this device")?;
        net::write_frame(send, &json!({"offset":offset})).await?;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut buffer = vec![0; BLOB_CHUNK];
        loop {
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                return Ok(());
            }
            send.write_all(&buffer[..read])
                .await
                .map_err(|error| anyhow::anyhow!("peer stream failed: {error}"))?;
        }
    }

    /// Fetches the attachments of `events` that are not on this device from the device that
    /// wrote each event, all within `ATTACHMENT_WAIT`. Returns the hashes still missing, which
    /// the prompt marks `pending_download`; a cut-short download resumes from its last offset.
    pub(crate) async fn fetch_turn_attachments<'a>(
        &self,
        network: &str,
        events: impl Iterator<Item = &'a HashedEvent>,
    ) -> std::collections::BTreeSet<String> {
        let local = self.identity.device_id().ok();
        let mut candidates: Vec<(String, BlobRef)> = Vec::new();
        for event in events {
            for blob in &event.event.blobs {
                if !candidates.iter().any(|(_, seen)| seen.hash == blob.hash) {
                    candidates.push((event.event.author.clone(), blob.clone()));
                }
            }
        }
        let wanted = {
            let candidates = candidates.clone();
            self.store
                .call("peers.turn_missing_blobs", move |store| {
                    Ok(candidates
                        .into_iter()
                        .filter(|(_, blob)| !store.has_blob(blob).unwrap_or(false))
                        .collect::<Vec<_>>())
                })
                .await
        }
        .unwrap_or(candidates);
        if wanted.is_empty() {
            return Default::default();
        }
        let fetch = async {
            for (author, blob) in &wanted {
                if local.as_ref() == Some(author) {
                    continue;
                }
                let present = {
                    let blob = blob.clone();
                    self.store
                        .call("peers.has_blob", move |store| store.has_blob(&blob))
                        .await
                        .unwrap_or(false)
                };
                if present {
                    continue;
                }
                if let Err(error) = self.fetch_blob(network, author, blob).await {
                    tracing::debug!(%error, blob = blob.hash, "attachment for a turn is still pending");
                }
            }
        };
        let _ = tokio::time::timeout(ATTACHMENT_WAIT, fetch).await;
        let pending = wanted.iter().map(|(_, blob)| blob.hash.clone()).collect();
        self.store
            .call("peers.turn_pending_blobs", move |store| {
                Ok(wanted
                    .into_iter()
                    .filter(|(_, blob)| !store.has_blob(blob).unwrap_or(false))
                    .map(|(_, blob)| blob.hash)
                    .collect())
            })
            .await
            .unwrap_or(pending)
    }

    /// Downloads from `peer` every attachment stored events reference that is not here yet. Each
    /// sync round runs it, so one a turn left pending arrives once a member holding it is reached.
    async fn fetch_blobs(&self, network: &str, peer: &str) -> Result<()> {
        let missing = {
            let arg0 = (network).to_owned();
            self.store
                .call("peers.missing_blobs", move |store| {
                    store.missing_blobs(&arg0)
                })
                .await
        }?;
        let mut failed = 0;
        for blob in &missing {
            // One attachment the peer cannot serve must not hold back the rest.
            if let Err(error) = self.fetch_blob(network, peer, blob).await {
                tracing::debug!(%peer, %error, blob = blob.hash, "attachment download pending");
                failed += 1;
            }
        }
        ensure!(failed == 0, "{failed} attachments still pending");
        Ok(())
    }

    /// Resumes from the last offset synced to disk, so a restart re-downloads at most one chunk.
    pub(crate) async fn fetch_blob(&self, network: &str, peer: &str, blob: &BlobRef) -> Result<()> {
        use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};
        ensure!(blob.size as usize <= MAX_BLOB_BYTES, "attachment too large");
        let _download = self.blob_fetch.lock().await;
        let (part, target, committed) = {
            let blob = blob.clone();
            self.store
                .call("peers.blob_resume", move |store| {
                    Ok((
                        store.partial_blob_file(&blob.hash)?,
                        store.blob_file(&blob.hash)?,
                        store.committed_offset(&blob.hash)?,
                    ))
                })
                .await?
        };
        if tokio::fs::metadata(&target)
            .await
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() == blob.size)
        {
            return Ok(());
        }
        let dir = part.parent().context("partial directory")?;
        tokio::fs::create_dir_all(dir).await?;
        if !tokio::fs::try_exists(&part).await? {
            let dir = dir.to_owned();
            let size = blob.size;
            tokio::task::spawn_blocking(move || staging_room(&dir, size)).await??;
        }
        let connection = self.net()?.connect(peer, net::BLOB).await?;
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|error| anyhow::anyhow!("peer connection closed: {error}"))?;
        net::write_frame(
            &mut send,
            &json!({"network_id":network,"hash":blob.hash,"size":blob.size,"offset":committed}),
        )
        .await?;
        send.finish().ok();
        let header = net::read_frame(&mut recv)
            .await?
            .context("source closed the attachment stream")?;
        if let Some(error) = header["error"].as_str() {
            bail!("{error}");
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&part)
            .await?;
        file.set_len(committed).await?;
        file.seek(std::io::SeekFrom::Start(committed)).await?;
        let mut offset = committed;
        let mut unsynced = 0;
        while let Some(chunk) = recv
            .read_chunk(BLOB_CHUNK)
            .await
            .map_err(|error| anyhow::anyhow!("attachment stream failed: {error}"))?
        {
            offset += chunk.len() as u64;
            ensure!(offset <= blob.size, "source sent more than the attachment");
            file.write_all(&chunk).await?;
            unsynced += chunk.len() as u64;
            if unsynced >= BLOB_SYNC_BYTES {
                file.sync_data().await?;
                {
                    let arg0 = (blob.hash).to_owned();
                    let arg1 = Some(offset);
                    self.store
                        .call("peers.commit_offset", move |store| {
                            store.commit_offset(&arg0, arg1)
                        })
                        .await
                }?;
                unsynced = 0;
            }
        }
        file.sync_all().await?;
        {
            let arg0 = (blob.hash).to_owned();
            let arg1 = Some(offset);
            self.store
                .call("peers.commit_offset", move |store| {
                    store.commit_offset(&arg0, arg1)
                })
                .await
        }?;
        ensure!(offset == blob.size, "attachment stream ended early");
        drop(file);
        let hash_part = part.clone();
        let hash = tokio::task::spawn_blocking(move || {
            std::fs::read(hash_part).map(|bytes| crate::crypto::digest(&bytes))
        })
        .await??;
        if hash != blob.hash {
            let _ = tokio::fs::remove_file(&part).await;
            {
                let arg0 = (blob.hash).to_owned();
                let arg1 = None;
                self.store
                    .call("peers.commit_offset", move |store| {
                        store.commit_offset(&arg0, arg1)
                    })
                    .await
            }?;
            bail!("downloaded attachment failed its hash check");
        }
        tokio::fs::create_dir_all(target.parent().context("blob directory")?).await?;
        tokio::fs::rename(&part, &target).await?;
        {
            let arg0 = (blob.hash).to_owned();
            let arg1 = None;
            self.store
                .call("peers.commit_offset", move |store| {
                    store.commit_offset(&arg0, arg1)
                })
                .await
        }?;
        Ok(())
    }
}

/// Refuses a new download once 64 objects or 256 MiB are already staged.
pub(super) fn staging_room(dir: &Path, size: u64) -> Result<()> {
    let mut objects = 0;
    let mut bytes = size;
    for entry in std::fs::read_dir(dir)? {
        let metadata = entry?.metadata()?;
        objects += 1;
        bytes += metadata.len();
    }
    ensure!(
        objects < MAX_STAGED_OBJECTS && bytes <= MAX_STAGED_BYTES,
        "attachment staging is full"
    );
    Ok(())
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

