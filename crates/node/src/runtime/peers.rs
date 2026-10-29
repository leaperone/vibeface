//! The Node side of the iroh transport: membership-gated handlers, version-vector sync, the
//! signed address book, scheduler delivery and resumable attachment downloads.
//!
//! Sync only stores events. It never creates scheduler jobs or work, so pulling history again
//! after a restart or from another Node replays no side effect.
use super::*;
use crate::net::{self, AddressRecord, Net, SignedAddress};
use iroh::{
    endpoint::{Connection, ConnectionError},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use std::sync::Weak;

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
        node.accept_connection(self.alpn, connection)
            .await
            .map_err(|error| AcceptError::from_err(std::io::Error::other(format!("{error:#}"))))
    }
}

/// A member Node's relay admits member Nodes and joiners holding an open invite; a public
/// entry's relay admits Nodes holding its operator token.
#[derive(Clone)]
struct RelayAccess(Weak<Runtime>);
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
        let allowed = match self.0.upgrade() {
            Some(node) => node.relay_allows(request).await,
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

    pub(super) async fn start_network(self: &Arc<Self>) -> Result<()> {
        let relays = match (&self.config.network.entry, &self.entry_token) {
            (Some(entry), Some(token)) => {
                vec![(net::relay_url(&entry.relay)?, Some(token.clone()))]
            }
            _ => vec![],
        };
        let net = Arc::new(
            Net::bind(
                &self.identity.secret_key_hex,
                &self.config.network,
                self.config.listen.port(),
                relays,
            )
            .await?,
        );
        ensure!(self.net.set(net.clone()).is_ok(), "network already started");
        let weak = Arc::downgrade(self);
        let mut router = Router::builder(net.endpoint.clone());
        for alpn in net::alpns(&self.config.network) {
            router = router.accept(
                alpn,
                Inbound {
                    node: weak.clone(),
                    alpn,
                },
            );
        }
        *self.router.lock().await = Some(router.spawn());
        if let Some(listen) = self
            .config
            .network
            .relay_listen
            .filter(|_| self.config.network.relay_mode == net::RelayServiceMode::Forward)
        {
            let relay = net::spawn_relay(listen, Arc::new(RelayAccess(weak))).await?;
            tracing::info!(event = "relay.started", %listen, "relay started");
            *self.relay.lock().await = Some(relay);
        }
        self.delivery.attach(Arc::downgrade(self))?;
        if self.config.public_entry {
            return Ok(());
        }
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
            for signed in self.store.lock().await.address_book()? {
                if let Ok(record) = signed.open(&network)
                    && view.node(&record.device_id).is_ok()
                {
                    net.learn(&record.endpoint).await;
                }
            }
        }
        let node = self.clone();
        tokio::spawn(async move { node.sync_loop().await });
        let node = self.clone();
        tokio::spawn(async move { node.have_loop().await });
        let node = self.clone();
        tokio::spawn(async move { node.outbox_loop().await });
        Ok(())
    }

    pub(super) async fn stop_network(&self) {
        if let Ok(net) = self.net() {
            net.stop_lan().await;
        }
        if let Some(router) = self.router.lock().await.take() {
            let _ = router.shutdown().await;
        }
        if let Some(relay) = self.relay.lock().await.take() {
            let _ = relay.shutdown().await;
        }
    }

    async fn relay_allows(&self, request: &iroh_relay::server::ClientRequest) -> bool {
        let device = net::device_id(&request.endpoint_id());
        if self.config.public_entry {
            return self.identity.device_id().is_ok_and(|own| own == device)
                || request
                    .auth_token()
                    .zip(self.entry_token.as_deref())
                    .is_some_and(|(token, own)| net::same_token(&token, own));
        }
        if self.member_node(&device).await.is_ok() {
            return true;
        }
        match request.auth_token() {
            Some(token) => self
                .store
                .lock()
                .await
                .relay_token_open(&token)
                .unwrap_or(false),
            None => false,
        }
    }

    async fn removed_here(&self, device: &str) -> bool {
        self.membership_state()
            .await
            .and_then(|state| state.project())
            .is_ok_and(|view| view.revocation(device).is_some())
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
        self.store.lock().await.authorize_node(&network, device)?;
        Ok(network)
    }

    async fn accept_connection(
        self: Arc<Self>,
        alpn: &'static [u8],
        connection: Connection,
    ) -> Result<()> {
        if alpn == net::DISCOVERY {
            return self.net()?.serve_discovery(connection).await;
        }
        let remote = net::device_id(&connection.remote_id());
        if alpn == net::SERVICE {
            return self.delivery.serve(self.clone(), connection).await;
        }
        if self.config.public_entry {
            connection.close(1u32.into(), b"a public entry accepts no connections");
            bail!("a public entry accepts no connections");
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
                let node = Arc::downgrade(&self);
                net::serve_streams(connection, move |request: Value| {
                    let (node, network) = (node.clone(), network.clone());
                    async move {
                        ensure!(
                            request["network_id"].as_str() == Some(network.as_str()),
                            "wrong personal network"
                        );
                        let node = node.upgrade().context("Node stopped")?;
                        if request["type"] == "membership" {
                            let incoming: NetworkState =
                                serde_json::from_value(request["state"].clone())
                                    .context("invalid membership")?;
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
                        }
                        let state = node.membership_state().await?;
                        Ok(json!({"type":"revoked","network_id":network,"state":state}))
                    }
                })
                .await;
                return Ok(());
            }
            Err(error) => {
                tracing::debug!(%remote, "refused a connection from a non-member");
                connection.close(1u32.into(), b"not a member Node of this personal network");
                return Err(error);
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
                node.store.lock().await.authorize_node(&network, &remote)?;
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
        tokio::spawn(async move {
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
        });
    }

    /// A request to another member Node over `vibeface/rpc/1`.
    pub(super) async fn peer_request(
        &self,
        network: &str,
        target: &str,
        mut payload: Value,
    ) -> Result<Value> {
        let local = self.identity.device_id()?;
        ensure!(
            target != local,
            "this Node answers itself without the network"
        );
        {
            let store = self.store.lock().await;
            store.authorize_node(network, &local)?;
            store.authorize_node(network, target)?;
        }
        payload["network_id"] = json!(network);
        self.net()?
            .request(target, net::RPC, &payload, PEER_TIMEOUT)
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
            "capabilities" | "progress_read" | "addresses" | "scheduler_jobs" | "join_changed"
        ))
        .then(|| self.maintenance.enter())
        .transpose()?;
        match kind {
            "capabilities" => Ok(self.capabilities().await),
            "addresses" => {
                let records: Vec<SignedAddress> =
                    serde_json::from_value(input["records"].clone()).unwrap_or_default();
                self.merge_addresses(network, records).await?;
                Ok(json!({"records":self.store.lock().await.address_book()?}))
            }
            "deliver" => self.accept_delivery(network, origin, input).await,
            "scheduler_jobs" => {
                self.owner_jobs(network, field(input, "conversation_id")?)
                    .await
            }
            "progress_read" => self.read_progress(network, origin, input).await,
            "dispatch_work" => self.accept_remote_dispatch(network, origin, input).await,
            "join_changed" => {
                self.publish_reread(network);
                Ok(json!({}))
            }
            "pending_joins" => Ok(
                json!({"requests":self.store.lock().await.pending_joins(field(input, "admin")?)?}),
            ),
            "approve_join" => {
                crate::client::approve_join(self, field(input, "admin")?, &input["params"]).await
            }
            "interrupt_work" => {
                let scope = field(input, "conversation_id")?;
                let work_id = field(input, "work_id")?;
                let record = self
                    .store
                    .lock()
                    .await
                    .work_record(work_id)?
                    .context("work not found on this device")?;
                ensure!(
                    record.conversation == scope,
                    "work belongs to another conversation"
                );
                self.interrupt_work(network, work_id).await
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
                self.answer_request(field(input, "request_id")?, Some(Ok(result)), "agent")
                    .await
            }
            "hand_over_session" => {
                self.hand_over_session(
                    network,
                    field(input, "from")?,
                    &self.identity.device_id()?,
                    field(input, "session_id")?,
                    field(input, "version")?,
                    input["conversation_id"].as_str(),
                )
                .await
            }
            "cancel_manager" | "resolve" => {
                ensure!(
                    input["human"] == true,
                    "management control requires the user"
                );
                let scope = field(input, "conversation_id")?;
                let access = Access {
                    network_id: network.into(),
                    human: true,
                    conversation_id: Some(scope.into()),
                    manager: None,
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

    async fn hello(&self, network: &str) -> Result<Value> {
        let state = self.membership_state().await?;
        let (heads, forks) = {
            let store = self.store.lock().await;
            (store.heads(network)?, store.fork_digest(network)?)
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
                    let store = self.store.lock().await;
                    theirs.values().try_fold(false, |behind, (_, hash)| {
                        anyhow::Ok(behind || store.event(network, hash)?.is_none())
                    })?
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
                Ok(json!({"state":self.membership_state().await?}))
            }
            "forks" => {
                let after: Option<(String, u64, String)> =
                    serde_json::from_value(input["after"].clone())
                        .context("invalid fork cursor")?;
                let events = self
                    .store
                    .lock()
                    .await
                    .fork_events(network, after.as_ref())?;
                Ok(json!({"events":events}))
            }
            "pull" => {
                let author = field(input, "author")?;
                let from = input["from_seq"].as_u64().context("from_seq is required")?;
                let limit = input["limit"]
                    .as_u64()
                    .unwrap_or(crate::store::MAX_PULL_EVENTS as u64)
                    as usize;
                let events =
                    self.store
                        .lock()
                        .await
                        .events_from(network, author, from.max(1), limit)?;
                Ok(json!({"author":author,"events":events}))
            }
            "have" => {
                let author = field(input, "author")?;
                let seq = input["seq"].as_u64().context("seq is required")?;
                let local = self.store.lock().await.heads(network)?;
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
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5)) => {},
                _ = self.sync_wakeup.notified() => {},
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
                tokio::spawn(async move {
                    let result = node.sync_peer(&network, &peer).await;
                    let (rerun, presence_changed) = {
                        let mut states = node.sync_peers.lock().expect("sync peers");
                        let entry = states.entry(peer.clone()).or_default();
                        entry.running = false;
                        let was_failing = entry.failures > 0;
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
                    if result.is_err() {
                        node.align(&network).await;
                    }
                    if rerun {
                        node.sync_wakeup.notify_one();
                    }
                });
            }
        }
    }

    /// One `Hello` round with `peer`: align membership, swap address books, then pull every
    /// author range the peer has beyond ours. Returns whether the round made progress and left
    /// history the next round can still pull.
    pub(super) async fn sync_peer(self: &Arc<Self>, network: &str, peer: &str) -> Result<bool> {
        let net = self.net()?.clone();
        let ours = self.hello(network).await?;
        let theirs = net.request(peer, net::SYNC, &ours, PEER_TIMEOUT).await?;
        if theirs["type"] == "revoked" {
            let removed = self.accept_removal(network, &theirs).await;
            if removed.is_err() {
                // Still a member here: a late veto may have put back a directory the peer has
                // not seen, so it gets this history to roll back too.
                let state = self.membership_state().await?;
                net.request(
                    peer,
                    net::SYNC,
                    &json!({"type":"membership","network_id":network,"state":state}),
                    PEER_TIMEOUT,
                )
                .await?;
                // The peer decided how to serve this connection when it took it; the next
                // round dials again.
                net.peer_restarted(peer);
            }
            return removed.map(|()| false);
        }
        ensure!(
            theirs["network_id"].as_str() == Some(network),
            "peer answered for another personal network"
        );
        if theirs["membership"] != ours["membership"] {
            let state = self.membership_state().await?;
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
        let before = self.store.lock().await.heads(network)?;
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
                |hash| async move { Ok(self.store.lock().await.event(network, &hash)?.is_some()) },
                |from, limit| self.pull(network, peer, author, from, limit),
                |events| async move { self.ingest(network, &events).await },
            )
            .await?;
        }
        let forks_left = theirs["forks"] != json!(self.store.lock().await.fork_digest(network)?)
            && self.pull_forks(network, peer).await?;
        let heads = self.store.lock().await.heads(network)?;
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
            return self
                .store
                .lock()
                .await
                .peer_capabilities(peer)
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
            && let Err(error) = self
                .store
                .lock()
                .await
                .keep_peer_capabilities(peer, &answer)
        {
            tracing::debug!(%error, "peer capabilities kept in memory only");
        }
        answer
    }

    /// Whether the last sync with `peer` failed.
    pub(crate) fn sync_failing(&self, peer: &str) -> bool {
        let states = self.sync_peers.lock().expect("sync peers");
        states.get(peer).is_some_and(|state| state.failures > 0)
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
            self.store.lock().await.record_write_file()?;
            return Ok(true);
        }
        let (network, peers) = self.member_peers().await?;
        let (forked, same_file) = {
            let store = self.store.lock().await;
            (
                store.has_conflicts_by(&network, &self.identity.device_id()?)?,
                store.wrote_from_this_file()?,
            )
        };
        let state = match (forked, same_file || peers.is_empty()) {
            (true, _) => WriteState::Forked,
            (false, true) => WriteState::Writable,
            (false, false) => WriteState::Aligning,
        };
        if state == WriteState::Writable {
            self.store.lock().await.record_write_file()?;
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
                *seq <= local
                    && self
                        .store
                        .lock()
                        .await
                        .hash_at(network, &own, *seq)?
                        .as_ref()
                        == Some(hash)
            }
        };
        if self.store.lock().await.has_conflicts_by(network, &own)? {
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
        let mut store = self.store.lock().await;
        let current = *self.write_state.borrow();
        if current == to || !from.contains(&current) {
            return false;
        }
        let recorded = match to {
            WriteState::Writable => store.record_write_file(),
            _ => Ok(()),
        };
        if let Err(error) = recorded.and_then(|()| store.set_write_state(to)) {
            tracing::warn!(event = "sync.write_state_pending", %error, "write state change pending");
            return false;
        }
        self.write_state.send_replace(to);
        drop(store);
        tracing::info!(event = "sync.write_state", state = ?to, "write state changed");
        let _ = self.events.send(Update {
            network_id: network.into(),
            payload: json!({"type":"write_state","state":to}),
        });
        true
    }

    /// The write state, and the conflicts kept aside here, for the web.
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
        let conflicts = self.store.lock().await.conflicts(network)?.len();
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
        let secrets: Vec<String> = self.entry_token.iter().cloned().collect();
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
                "public_entry":self.config.public_entry,"project_roots":settings.roots.len(),
                "manager_enabled":settings.manager.is_some(),"execution":self.providers().switches.execution,
                "session_scanning":self.triage.config().scan_codex_sessions},
            "write_state":*self.write_state.borrow(), "conflicts":conflicts,
            "peers":peers,
            "range":{"since_ms":since_ms,"until_ms":until_ms},
            "logs":{"node.log":records,"service.stderr.log":stderr},
        }))
    }

    pub(crate) async fn write_status(&self, network: &str) -> Result<Value> {
        let state = *self.write_state.borrow();
        let conflicts = self.store.lock().await.conflicts(network)?;
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
            self.store.lock().await.connection.execute(
                "INSERT INTO identity_meta(key,value) VALUES('write-confirmed',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [json!({"device":device,"at_ms":now_ms()}).to_string()],
            )?;
            tracing::info!(
                event = "sync.write_confirmed",
                device,
                "write confirmed without a backup check"
            );
        }
        Ok(())
    }

    async fn local_seq(&self, network: &str, author: &str) -> Result<u64> {
        Ok(self
            .store
            .lock()
            .await
            .heads(network)?
            .get(author)
            .map(|head| head.0)
            .unwrap_or(0))
    }

    async fn ingest(self: &Arc<Self>, network: &str, events: &[SignedEvent]) -> Result<()> {
        let ingested = {
            let mut store = self.store.lock().await;
            let ingested = store.ingest_events(network, events)?;
            store.settle_local_works(network)?;
            ingested
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
            let head = self
                .store
                .lock()
                .await
                .heads(&view.network_id)
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
                tokio::spawn(async move {
                    if let Err(error) = net
                        .request(&peer, net::SYNC, &have, Duration::from_secs(10))
                        .await
                    {
                        tracing::debug!(%peer, %error, "have notice not delivered; Hello covers it");
                    }
                });
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
        tokio::spawn(async move {
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
        });
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
            let _ = self.store.lock().await.forget_address(&revoked.device_id);
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
        let removed = signed_removals(&state, &view);
        if removed.is_empty() {
            return;
        }
        let result = self.store.lock().await.unverify_removed_works(
            &self.identity,
            network,
            &removed,
            now_ms(),
        );
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
        match self
            .store
            .lock()
            .await
            .drop_requests_to(network, &local, &removed)
        {
            Ok(0) => {}
            Ok(_) => self.schedule_wakeup.notify_one(),
            Err(error) => {
                tracing::warn!(event = "devices.removed_requests_pending", %error, "dropping requests to removed devices is pending")
            }
        }
    }

    /// Where browsers reach this Node: its public entry, else its own URL.
    pub(crate) fn gateway_url(&self) -> Option<String> {
        self.config
            .network
            .entry
            .as_ref()
            .map(|entry| entry.url.clone())
            .or_else(|| {
                self.config
                    .web_enabled
                    .then(|| self.config.public_url.clone())
            })
    }

    /// Re-signs this Node's record when its paths or Gateway changed; returns whether it did.
    async fn refresh_own_address(&self, network: &str) -> Result<bool> {
        let net = self.net()?;
        let device_id = self.identity.device_id()?;
        let endpoint = net.addr();
        let gateway_url = self.gateway_url();
        let mut store = self.store.lock().await;
        let previous = store
            .address_book()?
            .into_iter()
            .filter_map(|signed| signed.open(network).ok())
            .find(|record| record.device_id == device_id);
        if previous
            .as_ref()
            .is_some_and(|record| record.endpoint == endpoint && record.gateway_url == gateway_url)
        {
            return Ok(false);
        }
        let record = AddressRecord {
            version: 1,
            network_id: network.into(),
            device_id,
            seq: previous
                .map(|record| record.seq + 1)
                .unwrap_or(0)
                .max(now_ms() as u64),
            endpoint,
            gateway_url,
        };
        let payload = serde_json::to_string(&record)?;
        let signed = SignedAddress {
            signature: self
                .identity
                .sign_payload(net::ADDRESS_DOMAIN, payload.as_bytes())?,
            payload,
        };
        store.save_address(network, &record, &signed)?;
        Ok(true)
    }

    async fn exchange_addresses(&self, network: &str, peer: &str) -> Result<()> {
        let records = self.store.lock().await.address_book()?;
        let reply = self
            .net()?
            .request(
                peer,
                net::RPC,
                &json!({"type":"addresses","network_id":network,"records":records}),
                PEER_TIMEOUT,
            )
            .await?;
        let records: Vec<SignedAddress> = serde_json::from_value(reply["records"].clone())?;
        self.merge_addresses(network, records).await
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
            let changed = self
                .store
                .lock()
                .await
                .save_address(network, &record, &signed)?;
            if changed && let Ok(net) = self.net() {
                net.learn(&record.endpoint).await;
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
            .lock()
            .await
            .address_book()?
            .into_iter()
            .filter(|signed| {
                signed
                    .open(network)
                    .is_ok_and(|record| view.node(&record.device_id).is_ok())
            })
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

    async fn outbox_loop(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(3)) => {},
                _ = self.outbox_wakeup.notified() => {},
            }
            let Ok(state) = self.membership_state().await else {
                continue;
            };
            let network = &state.genesis.network_id;
            let (outbox, works) =
                tokio::join!(self.deliver_outbox(), self.deliver_work_requests(network));
            if let Err(error) = outbox {
                tracing::debug!(%error, "scheduler delivery pending");
            }
            if let Err(error) = works {
                tracing::debug!(%error, "work requests wait for their devices");
            }
            if let Err(error) = self.reconcile_works(network).await {
                tracing::debug!(%error, "work reconciliation pending");
            }
            if let Err(error) = self.reconcile_checks(network).await {
                tracing::debug!(%error, "pending check reconciliation pending");
            }
        }
    }

    /// Hands each waiting message or result to the conversation's current owner. The owner admits
    /// by trigger, so a retry after an unknown outcome is harmless.
    async fn deliver_outbox(self: &Arc<Self>) -> Result<()> {
        let Some(network) = self.store.lock().await.network()? else {
            return Ok(());
        };
        let entries = self.store.lock().await.scheduler_outbox()?;
        let mut owned = Vec::new();
        for entry in entries {
            let owner = self
                .store
                .lock()
                .await
                .conversation(&network, &entry.conversation_id)?
                .and_then(|conversation| conversation.owner)
                .context("conversation has no scheduling Node")?;
            owned.push((owner, entry));
        }
        let (local, network) = (&self.identity.device_id()?, &network);
        each_device(owned, |owner, entry| async move {
            let delivered = if owner == *local {
                self.store
                    .lock()
                    .await
                    .admit_delivery(local, network, &entry.conversation_id, &entry.trigger)
                    .map(|receipt| json!({ "receipt": receipt }))
            } else {
                let request = json!({"type":"deliver","conversation_id":entry.conversation_id,
                        "trigger":entry.trigger});
                self.peer_request(network, &owner, request).await
            }?;
            ensure!(
                delivered["receipt"].is_object(),
                "delivery answer had no receipt"
            );
            self.store
                .lock()
                .await
                .remove_outbox(&entry.conversation_id, &entry.trigger)?;
            self.publish_changed(
                network,
                &entry.conversation_id,
                Changed::Job(&entry.trigger),
            );
            self.schedule_wakeup.notify_one();
            Ok(())
        })
        .await;
        Ok(())
    }

    async fn accept_delivery(
        self: &Arc<Self>,
        network: &str,
        origin: &str,
        input: &Value,
    ) -> Result<Value> {
        let scope = id(input, "conversation_id")?;
        let trigger = field(input, "trigger")?;
        let local = self.identity.device_id()?;
        let mut receipt = self
            .store
            .lock()
            .await
            .admit_delivery(&local, network, &scope, trigger)?;
        if receipt.is_none() {
            self.sync_peer(network, origin).await?;
            receipt = self
                .store
                .lock()
                .await
                .admit_delivery(&local, network, &scope, trigger)?;
        }
        let receipt = receipt.context("the message has not reached the scheduling Node yet")?;
        self.schedule_wakeup.notify_one();
        self.publish_changed(network, &scope, Changed::Job(trigger));
        Ok(json!({"receipt":receipt}))
    }

    async fn serve_blobs(self: Arc<Self>, connection: Connection, network: String, remote: String) {
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            let (node, network, remote) = (self.clone(), network.clone(), remote.clone());
            tokio::spawn(async move {
                if let Err(error) = node
                    .send_blob(&network, &remote, &mut send, &mut recv)
                    .await
                {
                    let _ =
                        net::write_frame(&mut send, &json!({"error":format!("{error:#}")})).await;
                }
                send.finish().ok();
                let _ = send.stopped().await;
            });
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
            let store = self.store.lock().await;
            store.authorize_node(network, remote)?;
            let size = store
                .referenced_blob(network, hash)?
                .context("no stored event references this attachment")?;
            ensure!(
                request["size"].as_u64() == Some(size) && offset <= size,
                "attachment size mismatch"
            );
            store.blob_file(hash)?
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
        let mut wanted: Vec<(String, BlobRef)> = Vec::new();
        {
            let store = self.store.lock().await;
            for event in events {
                for blob in &event.event.blobs {
                    if !wanted.iter().any(|(_, seen)| seen.hash == blob.hash)
                        && !store.has_blob(blob).unwrap_or(false)
                    {
                        wanted.push((event.event.author.clone(), blob.clone()));
                    }
                }
            }
        }
        if wanted.is_empty() {
            return Default::default();
        }
        let fetch = async {
            let _download = self.blob_fetch.lock().await;
            for (author, blob) in &wanted {
                if local.as_ref() == Some(author)
                    || self.store.lock().await.has_blob(blob).unwrap_or(false)
                {
                    continue;
                }
                if let Err(error) = self.fetch_blob(network, author, blob).await {
                    tracing::debug!(%error, blob = blob.hash, "attachment for a turn is still pending");
                }
            }
        };
        let _ = tokio::time::timeout(ATTACHMENT_WAIT, fetch).await;
        let store = self.store.lock().await;
        wanted
            .into_iter()
            .filter(|(_, blob)| !store.has_blob(blob).unwrap_or(false))
            .map(|(_, blob)| blob.hash)
            .collect()
    }

    /// Downloads from `peer` every attachment stored events reference that is not here yet. Each
    /// sync round runs it, so one a turn left pending arrives once a member holding it is reached.
    async fn fetch_blobs(&self, network: &str, peer: &str) -> Result<()> {
        let _download = self.blob_fetch.lock().await;
        let missing = self.store.lock().await.missing_blobs(network)?;
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
        let (part, target, committed) = {
            let store = self.store.lock().await;
            (
                store.partial_blob_file(&blob.hash)?,
                store.blob_file(&blob.hash)?,
                store.committed_offset(&blob.hash)?,
            )
        };
        let dir = part.parent().context("partial directory")?;
        tokio::fs::create_dir_all(dir).await?;
        if !part.exists() {
            staging_room(dir, blob.size)?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&part)
            .await?;
        file.set_len(committed).await?;
        file.seek(std::io::SeekFrom::Start(committed)).await?;
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
                self.store
                    .lock()
                    .await
                    .commit_offset(&blob.hash, Some(offset))?;
                unsynced = 0;
            }
        }
        file.sync_all().await?;
        self.store
            .lock()
            .await
            .commit_offset(&blob.hash, Some(offset))?;
        ensure!(offset == blob.size, "attachment stream ended early");
        drop(file);
        let bytes = tokio::fs::read(&part).await?;
        if crate::crypto::digest(&bytes) != blob.hash {
            let _ = tokio::fs::remove_file(&part).await;
            self.store.lock().await.commit_offset(&blob.hash, None)?;
            bail!("downloaded attachment failed its hash check");
        }
        tokio::fs::create_dir_all(target.parent().context("blob directory")?).await?;
        tokio::fs::rename(&part, &target).await?;
        self.store.lock().await.commit_offset(&blob.hash, None)?;
        Ok(())
    }
}

/// Sends each device its requests in order, all devices at once. A device that does not answer
/// keeps the rest of its requests for the next round, so it holds up nobody else's.
pub(super) async fn each_device<T, F, Fut>(requests: Vec<(String, T)>, send: F)
where
    F: Fn(String, T) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut queues: BTreeMap<String, Vec<T>> = BTreeMap::new();
    for (device, request) in requests {
        queues.entry(device).or_default().push(request);
    }
    let send = &send;
    futures_util::future::join_all(queues.into_iter().map(|(device, requests)| async move {
        for request in requests {
            if let Err(error) = send(device.clone(), request).await {
                tracing::debug!(%device, %error, "request waits for its device");
                if net::Unanswered::is(&error) {
                    break;
                }
            }
        }
    }))
    .await;
}

/// Devices out of `view` by a manager's signed revocation in an epoch in effect, not only
/// because a recovery left them out.
fn signed_removals(state: &NetworkState, view: &AuthorizationView) -> Vec<String> {
    let stopped: BTreeSet<u64> = state
        .checkpoints
        .iter()
        .filter(|checkpoint| {
            checkpoint
                .hash()
                .is_ok_and(|hash| view.stopped.contains(&hash))
        })
        .map(|checkpoint| checkpoint.epoch)
        .collect();
    view.revoked
        .iter()
        .filter(|revoked| {
            state.events.iter().any(|event| {
                event.epoch <= view.epoch
                    && !stopped.contains(&event.epoch)
                    && matches!(&event.action, GovernanceAction::Revoke(revocation) if revocation.device_id == revoked.device_id)
            })
        })
        .map(|revoked| revoked.device_id.clone())
        .collect()
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
mod tests {
    use super::*;

    use crate::{config, crypto::LocalIdentity};

    async fn node(identity: &LocalIdentity) -> Result<(Arc<Runtime>, PathBuf)> {
        node_with(
            identity,
            net::NetworkConfig {
                bind: Some("127.0.0.1:0".parse()?),
                ..Default::default()
            },
        )
        .await
    }

    async fn node_with(
        identity: &LocalIdentity,
        network: net::NetworkConfig,
    ) -> Result<(Arc<Runtime>, PathBuf)> {
        let dir = std::env::temp_dir().join(format!("vibeface-peers-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        let mut configuration = Config::local("127.0.0.1:0".parse()?, vec![], false);
        configuration.network = network;
        configuration.providers.codex_app_server.socket = Some(dir.join("unavailable.sock"));
        configuration.save(&dir)?;
        config::private_write(
            &dir.join("data-epoch.json"),
            &crate::config::data_epoch_marker(),
        )?;
        config::private_write(&dir.join("identity.json"), &serde_json::to_vec(identity)?)?;
        Ok((Arc::new(Runtime::open(&dir).await?), dir))
    }

    fn user(identity: &LocalIdentity, network: &str, id: &str) -> Result<Attestation> {
        user_with(identity, network, id, vec![])
    }

    fn user_with(
        identity: &LocalIdentity,
        network: &str,
        id: &str,
        attachments: Vec<Attachment>,
    ) -> Result<Attestation> {
        identity
            .fixture_admin()
            .attest(
                network,
                "c",
                EventBody::MessagePosted {
                    message_id: id.into(),
                    role: Role::User,
                    text: id.into(),
                    attachments,
                    report_results: Some(true),
                    in_reply_to: None,
                    status: None,
                    answers: None,
                    attestation: None,
                    failure: None,
                },
            )?
            .attestation()
            .cloned()
            .context("signed message")
    }

    #[tokio::test]
    async fn diverged_logs_walk_back_to_the_fork_and_converge() -> Result<()> {
        use std::sync::Mutex;
        let (ia, ix) = (LocalIdentity::generate(), LocalIdentity::generate());
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ix.device_id()?])?;
        let network = ia.fixture_network_id()?;
        let author = ix.device_id()?;
        let replica = || -> Result<Store> {
            let mut store = Store::open(":memory:")?;
            store.install_trust_bundle(&network, &state)?;
            Ok(store)
        };
        let message = |id: &str| -> Result<EventBody> {
            Ok(EventBody::MessagePosted {
                message_id: id.into(),
                role: Role::User,
                text: id.into(),
                attachments: vec![],
                report_results: Some(true),
                in_reply_to: None,
                status: None,
                answers: None,
                attestation: Some(user(&ia, &network, id)?),
                failure: None,
            })
        };
        let (mut p, mut q) = (replica()?, replica()?);
        let created = EventBody::ConversationCreated { title: "t".into() };
        let shared = vec![
            p.append_event(&ix, &network, "c", created, vec![], 1)?,
            p.append_event(&ix, &network, "c", message("x2")?, vec![], 2)?,
        ];
        for id in ["a3", "a4", "a5"] {
            p.append_event(&ix, &network, "c", message(id)?, vec![], 3)?;
        }
        q.ingest_events(&network, &shared)?;
        for id in ["b3", "b4", "b5", "b6"] {
            q.append_event(&ix, &network, "c", message(id)?, vec![], 3)?;
        }
        let (p, q) = (Mutex::new(p), Mutex::new(q));
        let pulls = Mutex::new(0);
        let network = network.as_str();
        let fetch = async |to: &Mutex<Store>, from: &Mutex<Store>| -> Result<bool> {
            let their = from.lock().unwrap().heads(network)?[&author].clone();
            let our_max = to.lock().unwrap().heads(network)?[&author].0;
            reconcile_log(
                our_max,
                (their.0, &their.1),
                |hash| async move { Ok(to.lock().unwrap().event(network, &hash)?.is_some()) },
                |seq, limit| {
                    *pulls.lock().unwrap() += 1;
                    let events = from
                        .lock()
                        .unwrap()
                        .events_from(network, &author, seq, limit);
                    async move { events }
                },
                |events| async move {
                    to.lock().unwrap().ingest_events(network, &events)?;
                    Ok(())
                },
            )
            .await
        };
        assert!(fetch(&q, &p).await?);
        assert!(
            *pulls.lock().unwrap() <= 4,
            "walk-back probes in doubling steps"
        );
        assert!(fetch(&p, &q).await?);
        assert!(!fetch(&q, &p).await? && !fetch(&p, &q).await?);
        let evidence = |store: &Mutex<Store>| -> Result<(Vec<String>, Vec<String>, String)> {
            let store = store.lock().unwrap();
            let hashes = |sql: &str| -> Result<Vec<String>> {
                Ok(store
                    .connection
                    .prepare(sql)?
                    .query_map([network], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?)
            };
            Ok((
                hashes("SELECT hash FROM events WHERE network_id=? ORDER BY hash")?,
                hashes("SELECT hash FROM events WHERE network_id=? AND effective=1 ORDER BY hash")?,
                store.fork_digest(network)?,
            ))
        };
        let (stored, effective, forks) = evidence(&p)?;
        assert_eq!((stored.len(), effective.len()), (9, 2));
        assert!(!forks.is_empty());
        assert_eq!(evidence(&q)?, (stored, effective, forks));
        Ok(())
    }

    /// A recovery by `ia`'s current root keeping `members`, signed at `issued`, on `state`.
    fn recovery_on(
        ia: &LocalIdentity,
        state: &NetworkState,
        members: Vec<crate::identity::Member>,
        retired: Vec<crate::identity::Revocation>,
        issued: u64,
    ) -> Result<(NetworkState, crate::identity::RootCheckpoint, String)> {
        let next = LocalIdentity::generate().device_id()?;
        let checkpoint = crate::identity::RootCheckpoint::recovery(
            state,
            &ia.fixture_root_at(2),
            &next,
            members,
            retired,
            issued,
        )?;
        let mut waiting = state.clone();
        waiting.recover(checkpoint.clone())?;
        Ok((waiting, checkpoint, next))
    }

    /// `waiting` with the fixture manager's veto of `checkpoint`, signed at `at`.
    fn vetoed(
        ia: &LocalIdentity,
        waiting: &NetworkState,
        checkpoint: &crate::identity::RootCheckpoint,
        at: u64,
    ) -> Result<NetworkState> {
        use crate::identity::{GovernanceAction, GovernanceEvent};
        let veto = GovernanceEvent::unsigned(
            &waiting.project_at(checkpoint.issued_at_ms)?,
            &ia.fixture_admin().device_id()?,
            GovernanceAction::Veto {
                checkpoint: checkpoint.hash()?,
                at_ms: at,
            },
        )?
        .sign(&ia.fixture_admin())?;
        let mut state = waiting.clone();
        state.append(veto)?;
        Ok(state)
    }

    /// Long enough ago to be due by the wall clock.
    fn overdue() -> u64 {
        now_ms() as u64 - crate::identity::RECOVERY_DELAY_MS - 1_000
    }

    async fn stop_nodes(nodes: Vec<(Arc<Runtime>, PathBuf)>) {
        for (node, _) in &nodes {
            node.stop_network().await;
        }
        for (node, dir) in nodes {
            drop(node);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// A Node that slept through the wait puts the recovery in effect by the wall clock; the
    /// veto it missed, signed in time, puts the old directory back when it arrives. Sync goes on.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_that_flipped_early_rolls_back_on_a_late_veto_and_keeps_syncing() -> Result<()> {
        let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
        let network = ia.fixture_network_id()?;
        let (a, dir_a) = node(&ia).await?;
        let (b, dir_b) = node(&ib).await?;
        let result = async {
            for node in [&a, &b] {
                node.install_membership(state.clone()).await?;
            }
            let attacker = LocalIdentity::generate();
            let mut members = state.project()?.members;
            members.push(crate::identity::Member::client(
                &attacker, "attacker", true,
            )?);
            let issued = overdue();
            let (waiting, checkpoint, next) = recovery_on(&ia, &state, members, vec![], issued)?;
            let flipped = a.install_membership(waiting.clone()).await?;
            assert_eq!(flipped.epoch, 3);
            assert!(flipped.client(&attacker.device_id()?).is_ok());
            assert!(a.recovery_since_ms(&next).await?.is_some());
            b.install_membership(vetoed(&ia, &waiting, &checkpoint, issued + 1)?)
                .await?;
            let owner = ia.owner_fixture(&network, "c", &ib.device_id()?)?;
            b.store
                .lock()
                .await
                .create_conversation(&ib, &network, "c", "t", owner, 1)?;
            for node in [&a, &b] {
                node.start_network().await?;
            }
            a.net()?.learn(&b.net()?.addr()).await;
            a.sync_peer(&network, &ib.device_id()?).await?;
            let view = a.membership_state().await?.project()?;
            assert_eq!(view.epoch, 2);
            assert_eq!(view.stopped, vec![checkpoint.hash()?]);
            assert!(view.client(&attacker.device_id()?).is_err());
            assert_eq!(a.membership_state().await?, b.membership_state().await?);
            assert!(
                a.store.lock().await.conversation(&network, "c")?.is_some(),
                "the data log synced too"
            );
            anyhow::Ok(())
        }
        .await;
        stop_nodes(vec![(a, dir_a), (b, dir_b)]).await;
        result
    }

    /// Two Nodes that both slept through the veto agree with each other first, then with the
    /// Node that holds the veto; all three end on the old directory.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_nodes_that_slept_through_a_veto_converge_with_the_one_that_holds_it() -> Result<()>
    {
        let ids = [
            LocalIdentity::generate(),
            LocalIdentity::generate(),
            LocalIdentity::generate(),
        ];
        let ia = &ids[0];
        let devices = ids
            .iter()
            .map(|id| id.device_id())
            .collect::<Result<Vec<_>>>()?;
        let state = ia.sign_network_fixture(1, devices.clone())?;
        let network = ia.fixture_network_id()?;
        let mut nodes = vec![];
        for id in &ids {
            nodes.push(node(id).await?);
        }
        let result = async {
            let [(a, _), (b, _), (c, _)] = &nodes[..] else {
                unreachable!("three nodes")
            };
            let attacker = LocalIdentity::generate();
            let mut members = state.project()?.members;
            members.push(crate::identity::Member::client(
                &attacker, "attacker", true,
            )?);
            let issued = overdue();
            let (waiting, checkpoint, _) = recovery_on(ia, &state, members, vec![], issued)?;
            for node in [a, b] {
                node.install_membership(waiting.clone()).await?;
                assert_eq!(node.membership_state().await?.project()?.epoch, 3);
            }
            c.install_membership(vetoed(ia, &waiting, &checkpoint, issued + 1)?)
                .await?;
            for node in [a, b] {
                node.start_network().await?;
            }
            a.net()?.learn(&b.net()?.addr()).await;
            a.sync_peer(&network, &devices[1]).await?;
            for node in [a, b] {
                assert_eq!(node.membership_state().await?.project()?.epoch, 3);
            }
            c.start_network().await?;
            for (from, to) in [(a, c), (b, a), (c, a), (c, b)] {
                from.net()?.learn(&to.net()?.addr()).await;
            }
            a.sync_peer(&network, &devices[2]).await?;
            b.sync_peer(&network, &devices[0]).await?;
            let expected = c.membership_state().await?;
            for node in [a, b, c] {
                assert_eq!(node.membership_state().await?, expected);
                let view = node.membership_state().await?.project()?;
                assert_eq!((view.epoch, &view.stopped), (2, &vec![checkpoint.hash()?]));
                assert!(view.client(&attacker.device_id()?).is_err());
            }
            anyhow::Ok(())
        }
        .await;
        stop_nodes(nodes).await;
        result
    }

    /// A Node that put in effect a recovery leaving out a peer refuses that peer; the peer, which
    /// holds the veto, hands it over anyway, and both end on the old directory.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_refusing_a_peer_it_saw_removed_takes_its_veto() -> Result<()> {
        let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
        let network = ia.fixture_network_id()?;
        let (a, dir_a) = node(&ia).await?;
        let (b, dir_b) = node(&ib).await?;
        let result = async {
            for node in [&a, &b] {
                node.install_membership(state.clone()).await?;
            }
            let view = state.project()?;
            let kept: Vec<_> = view
                .members
                .iter()
                .filter(|member| member.device_id != ib.device_id().unwrap_or_default())
                .cloned()
                .collect();
            let retired = view.retire(&kept, &a.store.lock().await.heads(&network)?)?;
            let issued = overdue();
            let (waiting, checkpoint, _) = recovery_on(&ia, &state, kept, retired, issued)?;
            let flipped = a.install_membership(waiting.clone()).await?;
            assert!(flipped.revocation(&ib.device_id()?).is_some());
            b.install_membership(vetoed(&ia, &waiting, &checkpoint, issued + 1)?)
                .await?;
            for node in [&a, &b] {
                node.start_network().await?;
            }
            b.net()?.learn(&a.net()?.addr()).await;
            // Refused this round (unless a background round already handed it over).
            let _ = b.sync_peer(&network, &ia.device_id()?).await;
            assert_eq!(a.membership_state().await?, b.membership_state().await?);
            assert_eq!(a.membership_state().await?.project()?.epoch, 2);
            b.sync_peer(&network, &ia.device_id()?).await?;
            anyhow::Ok(())
        }
        .await;
        stop_nodes(vec![(a, dir_a), (b, dir_b)]).await;
        result
    }

    /// A stolen, removed Node with the recovery file signs a backdated recovery and pushes it:
    /// the checkpoint stays out, so nothing skips the wait or the veto.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_removed_node_cannot_push_a_backdated_recovery() -> Result<()> {
        use crate::identity::{GovernanceEvent, Revocation};
        let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
        let network = ia.fixture_network_id()?;
        let (a, dir_a) = node(&ia).await?;
        let (b, dir_b) = node(&ib).await?;
        let result = async {
            let mut removed = state.clone();
            removed.append(GovernanceEvent::revoke(
                &ia.fixture_admin(),
                &state.project()?,
                Revocation::new(&ib.device_id()?, None)?,
            )?)?;
            a.install_membership(removed.clone()).await?;
            let before = a.membership_state().await?;
            let attacker = LocalIdentity::generate();
            let mut members = removed.project()?.members;
            members.push(crate::identity::Member::client(
                &attacker, "attacker", true,
            )?);
            let (forged, _, _) = recovery_on(&ia, &removed, members, vec![], overdue())?;
            assert!(forged.project()?.client(&attacker.device_id()?).is_ok());
            for node in [&a, &b] {
                node.start_network().await?;
            }
            b.net()?.learn(&a.net()?.addr()).await;
            let reply = b
                .net()?
                .request(
                    &ia.device_id()?,
                    net::SYNC,
                    &json!({"type":"membership","network_id":network,"state":forged}),
                    PEER_TIMEOUT,
                )
                .await?;
            assert_eq!(reply["type"], "revoked");
            assert_eq!(a.membership_state().await?, before);
            let view = a.membership_state().await?.project()?;
            assert!(view.client(&attacker.device_id()?).is_err());
            anyhow::Ok(())
        }
        .await;
        stop_nodes(vec![(a, dir_a), (b, dir_b)]).await;
        result
    }

    /// The lost device was itself a Node and never comes back: the recovery that leaves it out
    /// takes effect when the wall clock reaches its time.
    #[tokio::test]
    async fn a_recovery_leaving_out_a_lost_node_takes_effect_on_time() -> Result<()> {
        let (ia, lost) = (LocalIdentity::generate(), LocalIdentity::generate());
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?, lost.device_id()?])?;
        let network = ia.fixture_network_id()?;
        let (a, dir) = node(&ia).await?;
        let result = async {
            a.install_membership(state.clone()).await?;
            let view = state.project()?;
            let kept: Vec<_> = view
                .members
                .iter()
                .filter(|member| member.device_id != lost.device_id().unwrap_or_default())
                .cloned()
                .collect();
            let retired = view.retire(&kept, &a.store.lock().await.heads(&network)?)?;
            let issued = now_ms() as u64 + 1_000 - crate::identity::RECOVERY_DELAY_MS;
            let (waiting, _, next) = recovery_on(&ia, &state, kept, retired, issued)?;
            assert_eq!(a.install_membership(waiting).await?.epoch, 2, "waiting");
            a.start_recovery_timer();
            let deadline = Instant::now() + Duration::from_secs(10);
            while a.recovery_since_ms(&next).await?.is_none() {
                ensure!(Instant::now() < deadline, "the recovery never took effect");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let view = a.membership_state().await?.project()?;
            assert_eq!(view.epoch, 3);
            assert!(view.revocation(&lost.device_id()?).is_some());
            anyhow::Ok(())
        }
        .await;
        drop(a);
        std::fs::remove_dir_all(dir)?;
        result
    }

    /// A recovery leaving out a Node records nothing about its work: a late veto can put the
    /// Node back, and a record in the shared log could not be taken back. A manager's signed
    /// removal leaves its work unverified.
    #[tokio::test]
    async fn a_recovery_undone_by_a_late_veto_adds_nothing_to_the_shared_log() -> Result<()> {
        use crate::{
            identity::{GovernanceEvent, Revocation},
            model::{EventBody, Role, WorkKind, WorkOutcome},
            work::WorkSpec,
        };
        let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
        let network = ia.fixture_network_id()?;
        let (a, dir) = node(&ia).await?;
        let result = async {
            a.install_membership(state.clone()).await?;
            let trigger = crate::scheduling::message_trigger("m1");
            let work = WorkSpec {
                work_id: WorkSpec::id("c", &trigger, "remote"),
                conversation: "c".into(),
                trigger: trigger.clone(),
                target: ib.device_id()?,
                dispatcher: ia.device_id()?,
                call_ref: "remote".into(),
                kind: WorkKind::Agent(crate::providers::Provider::Codex),
                cwd: "/tmp".into(),
                title: "t".into(),
                prompt: "p".into(),
                continues: None,
                parent: None,
                thread: None,
            };
            {
                let mut store = a.store.lock().await;
                let owner = ia.owner_fixture(&network, "c", &ia.device_id()?)?;
                store.create_conversation(&ia, &network, "c", "t", owner, 1)?;
                let posted = ia.fixture_admin().attest(
                    &network,
                    "c",
                    EventBody::MessagePosted {
                        message_id: "m1".into(),
                        role: Role::User,
                        text: "go".into(),
                        attachments: vec![],
                        report_results: Some(true),
                        in_reply_to: None,
                        status: None,
                        answers: None,
                        attestation: None,
                        failure: None,
                    },
                )?;
                let signature = posted.attestation().cloned().context("signed message")?;
                store.accept_message(&ia, &network, "c", "m1", "go", vec![], true, signature, 2)?;
                store.accept_work(&ib, &network, work.clone(), 3)?;
            }
            let shared = async || -> Result<(i64, Option<WorkOutcome>)> {
                let store = a.store.lock().await;
                let count = store.connection.query_row(
                    "SELECT count(*) FROM events WHERE network_id=?",
                    [&network],
                    |r| r.get(0),
                )?;
                let outcome = store
                    .works(&network, "c")?
                    .into_iter()
                    .find(|w| w.work_id == work.work_id)
                    .context("work")?
                    .outcome;
                Ok((count, outcome))
            };
            let before = shared().await?;
            assert_eq!(before.1, None);

            let view = state.project()?;
            let kept: Vec<_> = view
                .members
                .iter()
                .filter(|member| member.device_id != work.target)
                .cloned()
                .collect();
            let retired = view.retire(&kept, &a.store.lock().await.heads(&network)?)?;
            let issued = overdue();
            let (waiting, checkpoint, _) = recovery_on(&ia, &state, kept, retired, issued)?;
            let flipped = a.install_membership(waiting.clone()).await?;
            assert!(flipped.revocation(&work.target).is_some());
            a.unverify_revoked_works(&network).await;
            assert_eq!(shared().await?, before, "a recovery records nothing");

            let back = a
                .install_membership(vetoed(&ia, &waiting, &checkpoint, issued + 1)?)
                .await?;
            assert_eq!(back.epoch, 2);
            assert!(back.node(&work.target).is_ok());
            assert_eq!(shared().await?, before, "nor does rolling it back");

            let heads = a.store.lock().await.heads(&network)?;
            let (seq, hash) = &heads[&work.target];
            let removal = GovernanceEvent::revoke(
                &ia.fixture_admin(),
                &back,
                Revocation::new(&work.target, Some((*seq, hash.as_str())))?,
            )?;
            let mut removed = a.membership_state().await?;
            removed.append(removal)?;
            a.install_membership(removed).await?;
            assert_eq!(shared().await?.1, Some(WorkOutcome::Unverified));
            anyhow::Ok(())
        }
        .await;
        drop(a);
        std::fs::remove_dir_all(dir)?;
        result
    }

    /// Nothing new arrives when a recovery's wait ends, so the Node installs the history again
    /// itself, once.
    #[tokio::test]
    async fn a_waiting_recovery_is_installed_once_when_due() -> Result<()> {
        use crate::identity::{RECOVERY_DELAY_MS, RootCheckpoint};
        let ia = LocalIdentity::generate();
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?])?;
        let (a, dir) = node(&ia).await?;
        let result = async {
            a.install_membership(state.clone()).await?;
            let next = LocalIdentity::generate().device_id()?;
            let members = state.project()?.members;
            let issued = now_ms() as u64 + 1_000 - RECOVERY_DELAY_MS;
            let mut waiting = state.clone();
            waiting.recover(RootCheckpoint::recovery(
                &state,
                &ia.fixture_root_at(2),
                &next,
                members,
                vec![],
                issued,
            )?)?;
            a.install_membership(waiting).await?;
            a.start_recovery_timer();
            let installs = *a.membership.borrow();
            let deadline = Instant::now() + Duration::from_secs(10);
            while a.recovery_since_ms(&next).await?.is_none() {
                ensure!(
                    Instant::now() < deadline,
                    "the recovery was not installed when due"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(*a.membership.borrow(), installs + 1, "installed once");
            anyhow::Ok(())
        }
        .await;
        drop(a);
        std::fs::remove_dir_all(dir)?;
        result
    }

    #[tokio::test]
    async fn a_restart_from_the_same_file_writes_alone_and_a_copy_waits() -> Result<()> {
        let ia = LocalIdentity::generate();
        let others = [LocalIdentity::generate(), LocalIdentity::generate()];
        let state = ia.sign_network_fixture(
            1,
            vec![
                ia.device_id()?,
                others[0].device_id()?,
                others[1].device_id()?,
            ],
        )?;
        let network = ia.fixture_network_id()?;
        let (a, dir) = node(&ia).await?;
        let result = async {
            a.install_membership(state).await?;
            assert!(
                !a.align_on_start().await?,
                "no record of writing from this file yet"
            );
            a.confirm_writable(&network, "test").await?;

            a.set_write_state(&network, &[WriteState::Writable], WriteState::Aligning)
                .await;
            assert!(
                a.align_on_start().await?,
                "the file it last wrote from holds all it wrote; no other Node needs to answer"
            );
            assert_eq!(*a.write_state.borrow(), WriteState::Writable);

            let copy = dir.canonicalize()?.join("copy.sqlite3");
            a.store
                .lock()
                .await
                .connection
                .execute("VACUUM INTO ?", [copy.to_str().context("path")?])?;
            assert!(
                !Store::open(&copy)?.wrote_from_this_file()?,
                "a copied or restored database waits for the other Nodes or the user"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        drop(a);
        let _ = std::fs::remove_dir_all(dir);
        result
    }

    #[tokio::test]
    async fn alignment_waits_for_every_member_node_to_confirm() -> Result<()> {
        use crate::identity::{GovernanceEvent, Revocation};
        let (ia, ib, ic) = (
            LocalIdentity::generate(),
            LocalIdentity::generate(),
            LocalIdentity::generate(),
        );
        let (b, c) = (ib.device_id()?, ic.device_id()?);
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?, b.clone(), c.clone()])?;
        let network = ia.fixture_network_id()?;
        let (a, dir) = node(&ia).await?;
        let result = async {
            a.install_membership(state.clone()).await?;
            assert!(!a.align_on_start().await?);
            let mark = |peer: &str, confirmed: bool| {
                let mut states = a.sync_peers.lock().expect("sync peers");
                let entry = states.entry(peer.into()).or_default();
                entry.confirmed = confirmed;
                entry.failures = u32::from(!confirmed);
            };
            let now = || *a.write_state.borrow();
            mark(&b, true);
            a.align(&network).await;
            assert_eq!(now(), WriteState::Aligning, "c has not answered yet");
            mark(&c, false);
            a.align(&network).await;
            assert_eq!(now(), WriteState::ReadOnly);
            mark(&c, true);
            a.align(&network).await;
            assert_eq!(now(), WriteState::Writable);

            a.set_write_state(&network, &[WriteState::Writable], WriteState::Aligning)
                .await;
            mark(&c, false);
            let mut next = state.clone();
            next.append(GovernanceEvent::revoke(
                &ia.fixture_admin(),
                &state.project()?,
                Revocation::new(&c, None)?,
            )?)?;
            a.install_membership(next).await?;
            assert_eq!(
                now(),
                WriteState::Writable,
                "a revoked Node no longer counts"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        drop(a);
        let _ = std::fs::remove_dir_all(dir);
        result
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_removed_node_learns_its_removal_only_from_signed_history() -> Result<()> {
        use crate::identity::{GovernanceAction, GovernanceEvent, Revocation};
        let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
        let (member, removed) = (ia.device_id()?, ib.device_id()?);
        let state = ia.sign_network_fixture(1, vec![member.clone(), removed.clone()])?;
        let network = ia.fixture_network_id()?;
        let (a, dir_a) = node(&ia).await?;
        let (b, dir_b) = node(&ib).await?;
        let result = async {
            for node in [&a, &b] {
                node.install_membership(state.clone()).await?;
            }
            let revocation = Revocation::new(&removed, None)?;
            let admin = ia.fixture_admin().device_id()?;
            let action = GovernanceAction::Revoke(revocation.clone());
            let unsigned = GovernanceEvent::unsigned(&state.project()?, &admin, action.clone())?;
            let impostor = GovernanceEvent::unsigned(&state.project()?, &admin, action)?
                .sign(&LocalIdentity::generate().fixture_admin())?;
            for event in [unsigned, impostor] {
                let mut forged = state.clone();
                forged.events.push(event);
                let reply = json!({"type":"revoked","network_id":network,"state":forged});
                assert!(b.accept_removal(&network, &reply).await.is_err());
            }
            let bare = json!({"type":"revoked","network_id":network});
            assert!(b.accept_removal(&network, &bare).await.is_err());
            let held = b.membership_state().await?.project()?;
            assert_eq!(
                held.revocation(&removed),
                None,
                "a forged removal changes nothing"
            );

            let mut next = state.clone();
            next.append(GovernanceEvent::revoke(
                &ia.fixture_admin(),
                &state.project()?,
                revocation.clone(),
            )?)?;
            a.install_membership(next).await?;
            for node in [&a, &b] {
                node.start_network().await?;
            }
            b.net()?.learn(&a.net()?.addr()).await;
            b.sync_peer(&network, &member).await?;
            let held = b.membership_state().await?.project()?;
            assert_eq!(held.revocation(&removed), Some(&revocation));
            Ok::<_, anyhow::Error>(())
        }
        .await;
        for node in [&a, &b] {
            node.stop_network().await;
        }
        drop((a, b));
        for dir in [dir_a, dir_b] {
            let _ = std::fs::remove_dir_all(dir);
        }
        result
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_fetches_a_missing_attachment_from_its_author_or_marks_it_pending() -> Result<()>
    {
        let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
        let network = ia.fixture_network_id()?;
        let (a, dir_a) = node(&ia).await?;
        let (b, dir_b) = node(&ib).await?;
        let result = async {
            for node in [&a, &b] {
                node.install_membership(state.clone()).await?;
                node.start_network().await?;
            }
            let content = b"meeting notes".to_vec();
            let blob = BlobRef {
                hash: crate::crypto::digest(&content),
                size: content.len() as u64,
            };
            let attached = Attachment {
                hash: blob.hash.clone(),
                size: blob.size,
                name: "notes.txt".into(),
            };
            {
                let mut store = a.store.lock().await;
                let owner = ia.owner_fixture(&network, "c", &ia.device_id()?)?;
                store.create_conversation(&ia, &network, "c", "Files", owner, now_ms())?;
                store.put_blob(&blob, &content)?;
                store.accept_message(
                    &ia,
                    &network,
                    "c",
                    "m1",
                    "m1",
                    vec![attached.clone()],
                    true,
                    user_with(&ia, &network, "m1", vec![attached])?,
                    now_ms(),
                )?;
            }
            b.net()?.learn(&a.net()?.addr()).await;
            b.refresh_own_address(&network).await?;
            b.sync_peer(&network, &ia.device_id()?).await?;
            let events = b.store.lock().await.context(&network, "c", None)?.events;
            let file = b.store.lock().await.blob_file(&blob.hash)?;
            std::fs::remove_file(&file)?;
            assert!(
                b.fetch_turn_attachments(&network, events.iter())
                    .await
                    .is_empty()
            );
            assert!(
                b.store.lock().await.has_blob(&blob)?,
                "fetched from the message's author"
            );
            a.stop_network().await;
            std::fs::remove_file(&file)?;
            assert_eq!(
                b.fetch_turn_attachments(&network, events.iter()).await,
                [blob.hash.clone()].into(),
                "an author out of reach leaves it pending"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        for node in [&a, &b] {
            node.stop_network().await;
        }
        drop((a, b));
        for dir in [dir_a, dir_b] {
            let _ = std::fs::remove_dir_all(dir);
        }
        result
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sync_fetches_attachments_past_one_the_peer_lacks_and_catches_up_later() -> Result<()> {
        let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
        let network = ia.fixture_network_id()?;
        let (a, dir_a) = node(&ia).await?;
        let (b, dir_b) = node(&ib).await?;
        let result = async {
            for node in [&a, &b] {
                node.install_membership(state.clone()).await?;
                node.start_network().await?;
            }
            let mut files: Vec<(Vec<u8>, BlobRef)> = ["first notes", "second notes"]
                .map(|text| {
                    let content = text.as_bytes().to_vec();
                    let blob = BlobRef {
                        hash: crate::crypto::digest(&content),
                        size: content.len() as u64,
                    };
                    (content, blob)
                })
                .into();
            files.sort_by(|x, y| x.1.hash.cmp(&y.1.hash));
            let attached: Vec<Attachment> = files
                .iter()
                .map(|(_, blob)| Attachment {
                    hash: blob.hash.clone(),
                    size: blob.size,
                    name: "notes.txt".into(),
                })
                .collect();
            let (lacked, held) = (&files[0], &files[1]);
            {
                let mut store = a.store.lock().await;
                let owner = ia.owner_fixture(&network, "c", &ia.device_id()?)?;
                store.create_conversation(&ia, &network, "c", "Files", owner, now_ms())?;
                for (content, blob) in &files {
                    store.put_blob(blob, content)?;
                }
                store.accept_message(
                    &ia,
                    &network,
                    "c",
                    "m1",
                    "m1",
                    attached.clone(),
                    true,
                    user_with(&ia, &network, "m1", attached)?,
                    now_ms(),
                )?;
                std::fs::remove_file(store.blob_file(&lacked.1.hash)?)?;
            }
            b.net()?.learn(&a.net()?.addr()).await;
            b.refresh_own_address(&network).await?;
            b.sync_peer(&network, &ia.device_id()?).await?;
            assert!(
                b.store.lock().await.has_blob(&held.1)?,
                "an attachment the peer lacks does not hold back the next"
            );
            assert_eq!(
                b.store.lock().await.missing_blobs(&network)?,
                std::slice::from_ref(&lacked.1)
            );
            a.store.lock().await.put_blob(&lacked.1, &lacked.0)?;
            b.sync_peer(&network, &ia.device_id()?).await?;
            assert!(
                b.store.lock().await.missing_blobs(&network)?.is_empty(),
                "the next sync round fetches what was pending"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        for node in [&a, &b] {
            node.stop_network().await;
        }
        drop((a, b));
        for dir in [dir_a, dir_b] {
            let _ = std::fs::remove_dir_all(dir);
        }
        result
    }

    /// A bot computer that goes offline is announced to clients and keeps its last capabilities,
    /// so a client tells "its device is offline" from "no device runs a bot".
    #[tokio::test(flavor = "multi_thread")]
    async fn an_offline_node_is_announced_and_listed_with_its_last_capabilities() -> Result<()> {
        let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
        let network = ia.fixture_network_id()?;
        let (a, dir_a) = node(&ia).await?;
        let (b, dir_b) = node(&ib).await?;
        let result = async {
            for node in [&a, &b] {
                node.install_membership(state.clone()).await?;
                node.start_network().await?;
            }
            b.settings.lock().expect("settings").manager = Some(config::ManagerSettings {
                provider: crate::providers::Provider::Cursor,
                workspace: dir_b.join("manager-workspace"),
                model: String::new(),
            });
            a.net()?.learn(&b.net()?.addr()).await;
            let peer = ib.device_id()?;
            let access = Access {
                network_id: network.clone(),
                human: true,
                conversation_id: None,
                manager: None,
            };
            let listed = |nodes: Value| {
                nodes["nodes"]
                    .as_array()
                    .and_then(|nodes| nodes.iter().find(|node| node["device_id"] == *peer))
                    .cloned()
                    .context("the peer is listed")
            };
            a.wake_sync(&peer);
            tokio::time::timeout(Duration::from_secs(30), async {
                while !a
                    .sync_peers
                    .lock()
                    .expect("sync peers")
                    .get(&peer)
                    .is_some_and(|state| state.synced_ms.is_some() && state.failures == 0)
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .context("first sync")?;
            let online = listed(a.rpc(access.clone(), json!({"method":"nodes"})).await?)?;
            assert_eq!(online["online"], true);
            assert_eq!(online["capabilities"]["manager"], "cursor");
            let mut events = a.events.subscribe();
            b.stop_network().await;
            a.wake_sync(&peer);
            tokio::time::timeout(Duration::from_secs(40), async {
                loop {
                    let update = events.recv().await?;
                    if update.network_id == network && update.payload == json!({"type":"changed"}) {
                        return anyhow::Ok(());
                    }
                }
            })
            .await
            .context("clients are told to reread once the peer stops answering")??;

            let offline = listed(a.rpc(access, json!({"method":"nodes"})).await?)?;
            assert_eq!(offline["online"], false);
            assert_eq!(offline["capabilities"], online["capabilities"]);
            anyhow::Ok(())
        }
        .await;
        stop_nodes(vec![(a, dir_a), (b, dir_b)]).await;
        result
    }

    async fn first_sync(node: &Runtime, peer: &str) -> Result<()> {
        node.wake_sync(peer);
        tokio::time::timeout(Duration::from_secs(30), async {
            while !node
                .sync_peers
                .lock()
                .expect("sync peers")
                .get(peer)
                .is_some_and(|state| state.synced_ms.is_some() && state.failures == 0)
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("first sync")
    }

    /// A join waits on the Node whose invite it used. A manager connected to another member Node
    /// is told to reread at once, and lists the request from there.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_join_held_by_another_node_reaches_managers_on_every_node() -> Result<()> {
        use crate::identity::local::Join;
        use vibeface_secure::join::{INVITE_DOMAIN, InviteRecord, invite_hash, join_commit};

        let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
        let network = ia.fixture_network_id()?;
        let admin = ia.fixture_admin();
        let manager = admin.device_id()?;
        let (a, dir_a) = node(&ia).await?;
        let (b, dir_b) = node(&ib).await?;
        let result = async {
            for node in [&a, &b] {
                node.install_membership(state.clone()).await?;
                node.start_network().await?;
            }
            a.net()?.learn(&b.net()?.addr()).await;
            b.net()?.learn(&a.net()?.addr()).await;
            first_sync(&a, &ib.device_id()?).await?;
            first_sync(&b, &ia.device_id()?).await?;

            let pushed = || {
                let mut events = a.events.subscribe();
                let network = network.clone();
                async move {
                    tokio::time::timeout(Duration::from_secs(5), async {
                        loop {
                            let update = events.recv().await?;
                            if update.network_id == network
                                && update.payload == json!({"type":"changed"})
                            {
                                return anyhow::Ok(());
                            }
                        }
                    })
                    .await
                    .context("a manager on the other Node is told to reread")?
                }
            };
            let secret = [7u8; 32];
            let mut record = InviteRecord {
                network_id: network.clone(),
                invite_hash: invite_hash(&secret),
                issuer: manager.clone(),
                expires_at_ms: crate::runtime::now_ms() + 60_000,
                signature: String::new(),
            };
            record.signature = admin.sign_payload(INVITE_DOMAIN, &record.signing_payload()?)?;
            let join_id = b.store.lock().await.register_invite(&record, &manager)?;
            let phone = LocalIdentity::generate().device_id()?;
            b.store.lock().await.join(
                &Join {
                    secret: hex::encode(secret),
                    public_key: phone.clone(),
                    kind: crate::identity::IdentityKind::Client,
                    name: "phone".into(),
                    commit: join_commit(&phone, &"11".repeat(32))?,
                },
                &phone,
            )?;
            let listed = a.remote_joins(&manager).await;
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0]["join_id"], join_id, "A lists the join B holds");

            let waiting = pushed();
            let nonce = json!({"join_id":join_id,"admin_nonce":"22".repeat(32)});
            crate::client::approve_join(&b, &manager, &nonce).await?;
            waiting.await?;
            let waiting = pushed();
            crate::client::approve_join(&b, &manager, &json!({"join_id":join_id,"reject":true}))
                .await?;
            waiting.await?;
            assert!(
                a.remote_joins(&manager).await.is_empty(),
                "a rejected join is gone"
            );
            anyhow::Ok(())
        }
        .await;
        stop_nodes(vec![(a, dir_a), (b, dir_b)]).await;
        result
    }

    /// The last capabilities of a member Node survive this Node's restart, so a bot computer that
    /// is off still shows as offline instead of as missing.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_offline_node_keeps_its_last_capabilities_across_a_restart() -> Result<()> {
        let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
        let peer = ib.device_id()?;
        let access = Access {
            network_id: ia.fixture_network_id()?,
            human: true,
            conversation_id: None,
            manager: None,
        };
        let listed = |nodes: Value| {
            nodes["nodes"]
                .as_array()
                .and_then(|nodes| nodes.iter().find(|node| node["device_id"] == *peer))
                .cloned()
                .context("the peer is listed")
        };
        let (a, dir_a) = node(&ia).await?;
        let (b, dir_b) = node(&ib).await?;
        let online = async {
            for node in [&a, &b] {
                node.install_membership(state.clone()).await?;
                node.start_network().await?;
            }
            b.settings.lock().expect("settings").manager = Some(config::ManagerSettings {
                provider: crate::providers::Provider::Cursor,
                workspace: dir_b.join("manager-workspace"),
                model: String::new(),
            });
            a.net()?.learn(&b.net()?.addr()).await;
            first_sync(&a, &peer).await?;
            listed(a.rpc(access.clone(), json!({"method":"nodes"})).await?)
        }
        .await;
        a.shutdown().await;
        b.stop_network().await;
        // Background tasks still hold the stopped Runtime and its directory lock; a copy of the
        // directory is what the next process would open.
        let copy = dir_a.with_extension("restarted");
        let result = async {
            let online = online?;
            assert_eq!(online["capabilities"]["manager"], "cursor");
            std::process::Command::new("cp")
                .arg("-R")
                .args([&dir_a, &copy])
                .status()?
                .success()
                .then_some(())
                .context("copy the data directory")?;
            std::fs::remove_file(copy.join("node.lock"))?;
            let again = Arc::new(Runtime::open(&copy).await?);
            again.start_network().await?;
            let nodes = again.rpc(access.clone(), json!({"method":"nodes"})).await;
            again.stop_network().await;
            let offline = listed(nodes?)?;
            assert_eq!(offline["online"], false);
            assert_eq!(offline["capabilities"], online["capabilities"]);
            anyhow::Ok(())
        }
        .await;
        let _ = std::fs::remove_dir_all(copy);
        stop_nodes(vec![(a, dir_a), (b, dir_b)]).await;
        result
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn member_nodes_sync_ranges_and_attachments_without_jobs_and_refuse_outsiders()
    -> Result<()> {
        let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
        let network = ia.fixture_network_id()?;
        let (a, dir_a) = node(&ia).await?;
        let (b, dir_b) = node(&ib).await?;
        let (outsider, dir_c) = node(&LocalIdentity::generate()).await?;
        let result = async {
            for node in [&a, &b] {
                node.install_membership(state.clone()).await?;
            }
            for node in [&a, &b, &outsider] {
                node.start_network().await?;
            }
            let content: Vec<u8> = (0..3 * 1024 * 1024 + 17).map(|i| (i % 251) as u8).collect();
            let blob = BlobRef {
                hash: crate::crypto::digest(&content),
                size: content.len() as u64,
            };
            let attached = Attachment { hash: blob.hash.clone(), size: blob.size, name: "notes.bin".into() };
            {
                let mut store = a.store.lock().await;
                let owner = ia.owner_fixture(&network, "c", &ia.device_id()?)?;
                store.create_conversation(&ia, &network, "c", "Sync", owner, now_ms())?;
                store.put_blob(&blob, &content)?;
                store.accept_message(&ia, &network, "c", "m1", "m1", vec![attached.clone()], true,
                    user_with(&ia, &network, "m1", vec![attached.clone()])?, now_ms())?;
            }
            b.net()?.learn(&a.net()?.addr()).await;
            b.refresh_own_address(&network).await?;
            let peer_a = ia.device_id()?;
            let first = {
                let (b, network, peer) = (b.clone(), network.clone(), peer_a.clone());
                tokio::spawn(async move { b.sync_peer(&network, &peer).await })
            };
            while b.store.lock().await.committed_offset(&blob.hash)? == 0
                && !first.is_finished()
            {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            b.fetch_blobs(&network, &peer_a).await?;
            first.await??;
            let heads = a.store.lock().await.heads(&network)?;
            assert_eq!(b.store.lock().await.heads(&network)?, heads);
            assert!(b.store.lock().await.missing_blobs(&network)?.is_empty());
            let synced = b.store.lock().await.message(&network, "c", "m1")?.context("m1 synced")?;
            assert_eq!(synced.attachments, vec![attached], "the file name syncs with the message");
            assert!(b.store.lock().await.scheduler_jobs("c")?.is_empty());
            assert!(b.store.lock().await.scheduler_outbox()?.is_empty());

            for id in ["m2", "m3"] {
                a.store.lock().await.accept_message(&ia, &network, "c", id, id, vec![], true,
                    user(&ia, &network, id)?, now_ms())?;
            }
            let mut updates = b.events.subscribe();
            b.sync_peer(&network, &ia.device_id()?).await?;
            b.sync_peer(&network, &ia.device_id()?).await?;
            let mut pushed = Vec::new();
            while let Ok(update) = updates.try_recv() {
                let records = b.changed_records(&network, "browser", &update.payload).await?
                    .context("an ingested message is pushed as records")?;
                assert_eq!((&records["type"], &records["conversation_id"]), (&json!("records"), &json!("c")));
                for message in records["messages"].as_array().context("messages")? {
                    assert!(message["rank"].is_string(), "clients order pushed messages by rank");
                    pushed.push(message["message_id"].clone());
                }
            }
            pushed.sort_by_key(|id| id.to_string());
            assert_eq!(pushed, [json!("m2"), json!("m3")], "each ingested message is pushed once");
            assert_eq!(b.store.lock().await.heads(&network)?, a.store.lock().await.heads(&network)?);
            assert_eq!(b.store.lock().await.messages(&network, "c")?.len(), 3);
            assert!(b.store.lock().await.scheduler_jobs("c")?.is_empty());
            assert!(
                a.store.lock().await.address_book()?.iter().any(|signed| signed
                    .open(&network)
                    .is_ok_and(|record| record.device_id == ib.device_id().unwrap_or_default())),
                "the pulled Node learned the puller's signed address"
            );
            a.sync_peer(&network, &ib.device_id()?).await?;

            b.store.lock().await.accept_message(&ib, &network, "c", "m4", "m4", vec![], true,
                user(&ia, &network, "m4")?, now_ms())?;
            assert_eq!(b.store.lock().await.scheduler_outbox()?.len(), 1);
            b.deliver_outbox().await?;
            assert!(b.store.lock().await.scheduler_outbox()?.is_empty());
            let trigger = crate::scheduling::message_trigger("m4");
            a.store.lock().await.scheduler_job("c", &trigger)?
                .context("the owner admitted the delivered message")?;
            let again = b.peer_request(&network, &ia.device_id()?,
                json!({"type":"deliver","conversation_id":"c","trigger":trigger})).await?;
            assert_eq!(again["receipt"]["trigger"], trigger.as_str());
            assert_eq!(
                a.store.lock().await.scheduler_jobs("c")?.iter().filter(|job| job.trigger == trigger).count(),
                1,
                "a repeated delivery returns the original admission"
            );

            let target = b.store.lock().await.blob_file(&blob.hash)?;
            std::fs::remove_file(&target)?;
            let part = b.store.lock().await.partial_blob_file(&blob.hash)?;
            let mut seeded = content[..1024 * 1024].to_vec();
            seeded[0] ^= 1;
            seeded.extend_from_slice(b"bytes after the last synced offset");
            std::fs::write(&part, &seeded)?;
            b.store.lock().await.commit_offset(&blob.hash, Some(1024 * 1024))?;
            let corrupted = b.fetch_blob(&network, &ia.device_id()?, &blob).await;
            assert!(corrupted.is_err() && !part.exists(), "resumed bytes keep the synced prefix and fail the hash check");
            std::fs::write(&part, &content[..1024 * 1024])?;
            b.store.lock().await.commit_offset(&blob.hash, Some(1024 * 1024))?;
            b.fetch_blob(&network, &ia.device_id()?, &blob).await?;
            assert_eq!(std::fs::read(&target)?, content);
            assert!(!part.exists());

            outsider.net()?.learn(&a.net()?.addr()).await;
            let hello = json!({"type":"hello","network_id":network,"vector":{},"membership_epoch":0,"membership":""});
            assert!(outsider.net()?.request(&ia.device_id()?, net::SYNC, &hello, Duration::from_secs(5)).await.is_err());
            assert!(outsider.net()?.request(&ia.device_id()?, net::RPC, &json!({"type":"capabilities","network_id":network}), Duration::from_secs(5)).await.is_err());
            Ok::<_, anyhow::Error>(())
        }
        .await;
        for node in [&a, &b, &outsider] {
            node.stop_network().await;
        }
        drop((a, b, outsider));
        for dir in [dir_a, dir_b, dir_c] {
            let _ = std::fs::remove_dir_all(dir);
        }
        result
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn events_that_wait_for_an_ancestor_are_pushed_and_acted_on_once_stored() -> Result<()> {
        let [il, ih, ic] = [
            LocalIdentity::generate(),
            LocalIdentity::generate(),
            LocalIdentity::generate(),
        ];
        let admin = il.fixture_admin();
        let device = |id: &LocalIdentity| id.device_id().unwrap();
        let state = il.sign_network_fixture(1, vec![device(&il), device(&ih), device(&ic)])?;
        let network = il.fixture_network_id()?;
        let (l, dir_l) = node(&il).await?;
        let (h, dir_h) = node(&ih).await?;
        let (c, dir_c) = node(&ic).await?;
        let message = |scope: &str, id: &str| -> Result<Attestation> {
            admin
                .attest(
                    &network,
                    scope,
                    EventBody::MessagePosted {
                        message_id: id.into(),
                        role: Role::User,
                        text: id.into(),
                        attachments: vec![],
                        report_results: Some(true),
                        in_reply_to: None,
                        status: None,
                        answers: None,
                        attestation: None,
                        failure: None,
                    },
                )?
                .attestation()
                .cloned()
                .context("signed message")
        };
        let result = async {
            for node in [&l, &h, &c] {
                node.install_membership(state.clone()).await?;
            }
            let trigger = {
                let mut store = c.store.lock().await;
                let owner = il.owner_fixture(&network, "bot", &device(&ic))?;
                store.create_conversation(&ic, &network, "bot", "Bot", owner, now_ms())?;
                let bot = admin.attest(
                    &network,
                    "bot",
                    EventBody::BotCreated {
                        bot_id: "bot".into(),
                        name: "Bot".into(),
                        instructions: String::new(),
                        project: None,
                        attestation: None,
                        cause: None,
                    },
                )?;
                store.append_event(&ic, &network, "bot", bot, vec![], now_ms())?;
                store.accept_message(
                    &ic,
                    &network,
                    "bot",
                    "m1",
                    "m1",
                    vec![],
                    true,
                    message("bot", "m1")?,
                    now_ms(),
                )?;
                let claimed = store
                    .claim_next(&ic, &network, "bot", "boot", "cfg")?
                    .context("job")?;
                let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
                store.mark_started("bot", &claimed.job.trigger, &attempt)?;
                claimed.job.trigger
            };
            let log_of = async |node: &Runtime, id: &LocalIdentity| {
                node.store
                    .lock()
                    .await
                    .events_from(&network, &device(id), 1, 1024)
            };
            l.store
                .lock()
                .await
                .ingest_events(&network, &log_of(&c, &ic).await?)?;
            let owner = il.owner_fixture(&network, "s", &device(&il))?;
            h.store
                .lock()
                .await
                .create_conversation(&ih, &network, "s", "S", owner, now_ms())?;
            let from_h = log_of(&h, &ih).await?;
            l.store.lock().await.ingest_events(&network, &from_h)?;
            let deleted = admin
                .attest(
                    &network,
                    "bot",
                    EventBody::BotDeleted {
                        attestation: None,
                        cause: None,
                    },
                )?
                .attestation()
                .cloned()
                .context("signed deletion")?;
            let (posted, deletion) = {
                let mut store = l.store.lock().await;
                let (posted, _) = store.accept_message(
                    &il,
                    &network,
                    "s",
                    "l1",
                    "l1",
                    vec![],
                    true,
                    message("s", "l1")?,
                    now_ms(),
                )?;
                let deletion = store
                    .delete_bot(&il, &network, "bot", deleted, now_ms())?
                    .context("deleted")?;
                (posted.event_hash, deletion.hash)
            };

            let mut updates = c.events.subscribe();
            let pushed = |updates: &mut broadcast::Receiver<Update>| {
                std::iter::from_fn(|| updates.try_recv().ok())
                    .filter_map(|update| {
                        Some((
                            update.payload["conversation_id"].as_str()?.to_owned(),
                            update.payload["event_hash"].as_str()?.to_owned(),
                        ))
                    })
                    .collect::<Vec<_>>()
            };
            c.ingest(&network, &log_of(&l, &il).await?).await?;
            assert_eq!(
                c.store.lock().await.event(&network, &posted)?,
                None,
                "L's events wait for H's"
            );
            assert!(
                pushed(&mut updates).is_empty(),
                "nothing is pushed for an event still waiting"
            );

            c.ingest(&network, &from_h).await?;
            let after = pushed(&mut updates);
            assert!(
                after.contains(&("s".into(), posted.clone())),
                "the message stored with its ancestor is pushed: {after:?}"
            );
            assert!(
                after.contains(&("bot".into(), deletion.clone())),
                "the deletion stored with its ancestor is pushed"
            );
            let stopped = async {
                loop {
                    let job = c
                        .store
                        .lock()
                        .await
                        .scheduler_job("bot", &trigger)?
                        .context("job")?;
                    if !matches!(job.state, crate::scheduling::JobState::Running { .. }) {
                        return Ok::<_, anyhow::Error>(job.state);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            };
            let state = tokio::time::timeout(Duration::from_secs(10), stopped)
                .await
                .context("the deleted bot's running turn is stopped")??;
            assert!(
                matches!(
                    state,
                    crate::scheduling::JobState::Cancelling { .. }
                        | crate::scheduling::JobState::Cancelled { .. }
                ),
                "{state:?}"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        drop((l, h, c));
        for dir in [dir_l, dir_h, dir_c] {
            let _ = std::fs::remove_dir_all(dir);
        }
        result
    }

    #[tokio::test]
    async fn every_local_append_wakes_the_have_notice() -> Result<()> {
        let identity = LocalIdentity::generate();
        let state = identity.sign_network_fixture(1, vec![identity.device_id()?])?;
        let network = identity.fixture_network_id()?;
        let (node, dir) = node(&identity).await?;
        let result = async {
            node.install_membership(state).await?;
            let owner = identity.owner_fixture(&network, "c", &identity.device_id()?)?;
            let woken = node.have_wakeup.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            node.store.lock().await.create_conversation(
                &identity,
                &network,
                "c",
                "C",
                owner,
                now_ms(),
            )?;
            tokio::time::timeout(Duration::from_secs(1), woken)
                .await
                .context("an append that publishes nothing still announces the new head")?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        drop(node);
        let _ = std::fs::remove_dir_all(dir);
        result
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nodes_with_disjoint_ip_families_sync_through_a_member_relay() -> Result<()> {
        let port = std::net::TcpListener::bind("127.0.0.1:0")?
            .local_addr()?
            .port();
        let relay_url = format!("http://127.0.0.1:{port}");
        let ids = [
            LocalIdentity::generate(),
            LocalIdentity::generate(),
            LocalIdentity::generate(),
        ];
        let state = ids[0].sign_network_fixture(
            1,
            ids.iter()
                .map(LocalIdentity::device_id)
                .collect::<Result<_>>()?,
        )?;
        let network = ids[0].fixture_network_id()?;
        let (relay, dir_r) = node_with(
            &ids[0],
            net::NetworkConfig {
                bind: Some("127.0.0.1:0".parse()?),
                relay_listen: Some(format!("127.0.0.1:{port}").parse()?),
                ..Default::default()
            },
        )
        .await?;
        let (a, dir_a) = node_with(
            &ids[1],
            net::NetworkConfig {
                bind: Some("127.0.0.1:0".parse()?),
                relays: vec![relay_url.clone()],
                ..Default::default()
            },
        )
        .await?;
        let (b, dir_b) = node_with(
            &ids[2],
            net::NetworkConfig {
                bind: Some("[::1]:0".parse()?),
                relays: vec![relay_url.clone()],
                ..Default::default()
            },
        )
        .await?;
        let result = async {
            for node in [&relay, &a, &b] {
                node.install_membership(state.clone()).await?;
                node.start_network().await?;
            }
            a.store.lock().await.create_conversation(
                &ids[1],
                &network,
                "c",
                "Relayed",
                ids[0].owner_fixture(&network, "c", &ids[1].device_id()?)?,
                now_ms(),
            )?;
            let addr = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let addr = a.net()?.addr();
                    if addr.relay_urls().next().is_some() {
                        return Ok::<_, anyhow::Error>(addr);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .context("the hidden Node never reached its relay")??;
            assert!(addr.ip_addrs().all(|address| address.is_ipv4()));
            assert!(b.net()?.addr().ip_addrs().all(|address| address.is_ipv6()));
            b.net()?.learn(&addr).await;
            b.sync_peer(&network, &ids[1].device_id()?).await?;
            assert_eq!(
                b.store.lock().await.heads(&network)?,
                a.store.lock().await.heads(&network)?
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        for node in [&relay, &a, &b] {
            node.stop_network().await;
        }
        drop((relay, a, b));
        for dir in [dir_r, dir_a, dir_b] {
            let _ = std::fs::remove_dir_all(dir);
        }
        result
    }

    /// A member Node that is off costs only its own messages a wait: the others' are handed over
    /// in the same round.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_offline_owner_does_not_hold_up_delivery_to_online_ones() -> Result<()> {
        let [ia, ib, ix] = [
            LocalIdentity::generate(),
            LocalIdentity::generate(),
            LocalIdentity::generate(),
        ];
        let device = |id: &LocalIdentity| id.device_id().unwrap();
        let state = ia.sign_network_fixture(1, vec![device(&ia), device(&ib), device(&ix)])?;
        let network = ia.fixture_network_id()?;
        let (a, dir_a) = node(&ia).await?;
        let (b, dir_b) = node(&ib).await?;
        let (x, dir_x) = node(&ix).await?;
        let result = async {
            for node in [&a, &b, &x] {
                node.install_membership(state.clone()).await?;
                node.start_network().await?;
            }
            a.net()?.learn(&b.net()?.addr()).await;
            a.net()?.learn(&x.net()?.addr()).await;
            b.net()?.learn(&a.net()?.addr()).await;
            x.stop_network().await;
            let message = |scope: &str, id: &str| -> Result<Attestation> {
                posted(&ia, &network, scope, id)?
                    .attestation()
                    .cloned()
                    .context("signed message")
            };
            {
                let mut store = a.store.lock().await;
                for (scope, owner) in [("a1", &ix), ("a2", &ix), ("c", &ib)] {
                    let owner = ia.owner_fixture(&network, scope, &device(owner))?;
                    store.create_conversation(&ia, &network, scope, scope, owner, now_ms())?;
                }
            }
            first_sync(&b, &device(&ia)).await?;
            {
                let mut store = a.store.lock().await;
                for (scope, id) in [("a1", "x1"), ("a2", "x2"), ("c", "b1")] {
                    store.accept_message(
                        &ia,
                        &network,
                        scope,
                        id,
                        id,
                        vec![],
                        true,
                        message(scope, id)?,
                        now_ms(),
                    )?;
                }
                assert_eq!(store.scheduler_outbox()?.len(), 3);
            }
            a.outbox_wakeup.notify_one();
            let trigger = crate::scheduling::message_trigger("b1");
            tokio::time::timeout(Duration::from_secs(8), async {
                // B stores the job before A reads its receipt and drops the entry.
                while b.store.lock().await.scheduler_job("c", &trigger)?.is_none()
                    || a.store.lock().await.scheduler_outbox()?.len() != 2
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                anyhow::Ok(())
            })
            .await
            .context("the online owner gets its message while the offline one is still dialed")??;
            anyhow::Ok(())
        }
        .await;
        stop_nodes(vec![(a, dir_a), (b, dir_b), (x, dir_x)]).await;
        result
    }

    fn posted(identity: &LocalIdentity, network: &str, scope: &str, id: &str) -> Result<EventBody> {
        identity.fixture_admin().attest(
            network,
            scope,
            EventBody::MessagePosted {
                message_id: id.into(),
                role: Role::User,
                text: id.into(),
                attachments: vec![],
                report_results: Some(true),
                in_reply_to: None,
                status: None,
                answers: None,
                attestation: None,
                failure: None,
            },
        )
    }

    /// A Node that missed a join notice while its sync still looked fine is told once more, so
    /// its managers see the request without waiting for another change.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_missed_join_notice_is_sent_once_more() -> Result<()> {
        let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
        let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
        let network = ia.fixture_network_id()?;
        let (a, dir_a) = node(&ia).await?;
        let (b, dir_b) = node(&ib).await?;
        let result = async {
            // Sync stays out of the way: only the join notice tells B's clients to reread.
            for (node, peer) in [(&a, &ib), (&b, &ia)] {
                node.install_membership(state.clone()).await?;
                node.sync_peers
                    .lock()
                    .expect("sync peers")
                    .entry(peer.device_id()?)
                    .or_default()
                    .running = true;
            }
            a.start_network().await?;
            let mut updates = b.events.subscribe();
            a.join_changed(&network);
            tokio::time::sleep(Duration::from_millis(200)).await;
            b.start_network().await?;
            a.net()?.learn(&b.net()?.addr()).await;
            assert!(
                !a.sync_failing(&ib.device_id()?),
                "A's sync with B looks fine"
            );
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let update = updates.recv().await?;
                    if update.payload == json!({"type":"changed"}) {
                        return anyhow::Ok(());
                    }
                }
            })
            .await
            .context("B's managers are told to reread")??;
            anyhow::Ok(())
        }
        .await;
        stop_nodes(vec![(a, dir_a), (b, dir_b)]).await;
        result
    }

    /// Fork evidence longer than one round's pages arrives over the following rounds, which run
    /// right away.
    #[tokio::test(flavor = "multi_thread")]
    async fn fork_evidence_past_one_rounds_pages_arrives_in_the_next_rounds() -> Result<()> {
        let [il, ic] = [LocalIdentity::generate(), LocalIdentity::generate()];
        let mut forked = [LocalIdentity::generate(), LocalIdentity::generate()];
        forked.sort_by_key(|identity| identity.device_id().unwrap());
        let [f1, f2] = &forked;
        let device = |id: &LocalIdentity| id.device_id().unwrap();
        let state =
            il.sign_network_fixture(1, vec![device(&il), device(&ic), device(f1), device(f2)])?;
        let network = il.fixture_network_id()?;
        let (l, dir_l) = node(&il).await?;
        let (c, dir_c) = node(&ic).await?;
        let scratch_dir = dir_l.with_extension("other-branch");
        let result = async {
            for node in [&l, &c] {
                node.install_membership(state.clone()).await?;
            }
            let mut other = crate::store::Store::open(scratch_dir.join("db.sqlite"))?;
            other.install_trust_bundle(&network, &state)?;
            let long = MAX_FORK_PAGES * crate::store::MAX_PULL_EVENTS + 100;
            {
                let mut store = l.store.lock().await;
                for i in 0..long {
                    store.append_event(
                        f1,
                        &network,
                        "c",
                        posted(&il, &network, "c", &format!("f{i}"))?,
                        vec![],
                        i as i64,
                    )?;
                }
                store.append_event(
                    f2,
                    &network,
                    "c",
                    posted(&il, &network, "c", "g")?,
                    vec![],
                    0,
                )?;
                for (author, id) in [(f1, "f-other"), (f2, "g-other")] {
                    let branch = other.append_event(
                        author,
                        &network,
                        "c",
                        posted(&il, &network, "c", id)?,
                        vec![],
                        0,
                    )?;
                    store.ingest_events(&network, &[branch])?;
                }
            }
            let digest = l.store.lock().await.fork_digest(&network)?;
            assert!(!digest.is_empty(), "L holds both forks");
            for node in [&l, &c] {
                node.start_network().await?;
            }
            c.net()?.learn(&l.net()?.addr()).await;
            c.wake_sync(&il.device_id()?);
            tokio::time::timeout(Duration::from_secs(240), async {
                while c.store.lock().await.fork_digest(&network)? != digest {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                anyhow::Ok(())
            })
            .await
            .context("C holds all of L's fork evidence")??;
            anyhow::Ok(())
        }
        .await;
        let _ = std::fs::remove_dir_all(&scratch_dir);
        stop_nodes(vec![(l, dir_l), (c, dir_c)]).await;
        result
    }

    /// A round that leaves events waiting on other authors past the pending limit is followed
    /// by another one right away instead of the next Hello a minute later.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_round_left_behind_pulls_again_without_waiting_for_hello() -> Result<()> {
        let [il, ic, ih, ij] = [
            LocalIdentity::generate(),
            LocalIdentity::generate(),
            LocalIdentity::generate(),
            LocalIdentity::generate(),
        ];
        let device = |id: &LocalIdentity| id.device_id().unwrap();
        let state =
            il.sign_network_fixture(1, vec![device(&il), device(&ic), device(&ih), device(&ij)])?;
        let network = il.fixture_network_id()?;
        let (l, dir_l) = node(&il).await?;
        let (c, dir_c) = node(&ic).await?;
        let result = async {
            for node in [&l, &c] {
                node.install_membership(state.clone()).await?;
            }
            {
                let mut store = l.store.lock().await;
                for i in 0..40_000 {
                    let author = if i % 2 == 0 { &ih } else { &ij };
                    store.append_event(
                        author,
                        &network,
                        "c",
                        posted(&il, &network, "c", &format!("m{i}"))?,
                        vec![],
                        i,
                    )?;
                }
            }
            let target = l.store.lock().await.heads(&network)?;
            let peer = device(&il);
            // Anything that wakes C during its round would also start the next one at once: L's
            // own Hello, or an address record C has not seen. Only C's schedule decides here.
            l.sync_peers
                .lock()
                .expect("sync peers")
                .entry(device(&ic))
                .or_default()
                .running = true;
            c.sync_peers
                .lock()
                .expect("sync peers")
                .entry(peer.clone())
                .or_default()
                .running = true;
            for node in [&l, &c] {
                node.start_network().await?;
            }
            c.net()?.learn(&l.net()?.addr()).await;
            let book = async |node: &Runtime| -> Result<Vec<(String, u64)>> {
                Ok(node
                    .store
                    .lock()
                    .await
                    .address_book()?
                    .iter()
                    .filter_map(|signed| signed.open(&network).ok())
                    .map(|record| (record.device_id, record.seq))
                    .collect())
            };
            let mut settled = Vec::new();
            loop {
                let now = vec![book(&l).await?, book(&c).await?];
                if now.iter().all(|records| records.len() == 1) && now == settled {
                    break;
                }
                settled = now;
                tokio::time::sleep(Duration::from_secs(6)).await;
            }
            let records = l.store.lock().await.address_book()?;
            c.merge_addresses(&network, records).await?;
            c.sync_peers
                .lock()
                .expect("sync peers")
                .entry(peer.clone())
                .or_default()
                .running = false;
            c.wake_sync(&peer);
            let synced = || {
                c.sync_peers
                    .lock()
                    .expect("sync peers")
                    .get(&peer)
                    .and_then(|s| s.synced_ms)
            };
            tokio::time::timeout(Duration::from_secs(300), async {
                while synced().is_none() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .context("the first round ends")?;
            let first = c.store.lock().await.heads(&network)?;
            assert_ne!(first, target, "one round stops at the pending limit");
            tokio::time::timeout(Duration::from_secs(10), async {
                while c.store.lock().await.heads(&network)? == first {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                anyhow::Ok(())
            })
            .await
            .context("C pulls again right after a round that left it behind")??;
            tokio::time::timeout(Duration::from_secs(300), async {
                while c.store.lock().await.heads(&network)? != target {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                anyhow::Ok(())
            })
            .await
            .context("C catches up")??;
            anyhow::Ok(())
        }
        .await;
        stop_nodes(vec![(l, dir_l), (c, dir_c)]).await;
        result
    }
}
