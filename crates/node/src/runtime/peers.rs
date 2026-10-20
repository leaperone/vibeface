//! The Node side of the iroh transport: membership-gated handlers, version-vector sync, the
//! signed address book, scheduler delivery and resumable attachment downloads.
//!
//! Sync only stores events. It never creates scheduler jobs or work, so pulling history again
//! after a restart or from another Node replays no side effect.
use super::*;
use crate::net::{self, AddressRecord, Net, SignedAddress};
use iroh::{
    endpoint::Connection,
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

#[derive(Default)]
pub(super) struct PeerSync {
    due: Option<Instant>,
    failures: u32,
    running: bool,
    rerun: bool,
    boot: Option<String>,
    synced_ms: Option<i64>,
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

fn membership_digest(state: &NetworkState) -> Result<String> {
    let mut hashes = state
        .events
        .iter()
        .map(|event| event.hash().map_err(anyhow::Error::msg))
        .collect::<Result<Vec<_>>>()?;
    hashes.sort();
    Ok(crate::crypto::digest(
        serde_json::to_string(&(state.epoch(), &state.checkpoint, hashes))?.as_bytes(),
    ))
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
        for alpn in net::ALPNS {
            router = router.accept(
                alpn,
                Inbound {
                    node: weak.clone(),
                    alpn,
                },
            );
        }
        *self.router.lock().await = Some(router.spawn());
        if let Some(listen) = self.config.network.relay_listen {
            let relay = net::spawn_relay(listen, Arc::new(RelayAccess(weak))).await?;
            tracing::info!(%listen, "relay started");
            *self.relay.lock().await = Some(relay);
        }
        if self.config.public_entry {
            return Ok(());
        }
        if self.has_membership().await? {
            let network = self.membership_state().await?.genesis.network_id;
            let view = self.membership_state().await?.project()?;
            for signed in self.store.lock().await.address_book(&network)? {
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
        let remote = net::device_id(&connection.remote_id());
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
        let network = match self.member_node(&remote).await {
            Ok(network) => network,
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
        let watched = connection.clone();
        net::serve_streams(connection, move |request: Value| {
            let (node, connection, network, remote) = (
                self.clone(),
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
        let _work = (!matches!(kind, "capabilities" | "progress_read" | "addresses"))
            .then(|| self.maintenance.enter())
            .transpose()?;
        match kind {
            "capabilities" => Ok(self.capabilities().await),
            "addresses" => {
                let records: Vec<SignedAddress> =
                    serde_json::from_value(input["records"].clone()).unwrap_or_default();
                self.merge_addresses(network, records).await?;
                Ok(json!({"records":self.store.lock().await.address_book(network)?}))
            }
            "deliver" => self.accept_delivery(network, origin, input).await,
            "manager_status" => {
                self.manager_status(
                    network,
                    field(input, "conversation_id")?,
                    field(input, "trigger")?,
                )
                .await
            }
            "progress_read" => self.read_progress(network, origin, input).await,
            "dispatch_work" => self.accept_remote_dispatch(network, origin, input).await,
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
                    .work_record(network, work_id)?
                    .context("work not found on this device")?;
                ensure!(
                    record.spec.conversation == scope,
                    "work belongs to another conversation"
                );
                self.interrupt_work(network, work_id).await
            }
            "approval_forms" => self.approval_forms(network, field(input, "work_id")?).await,
            "answer_approval" => {
                self.answer_approval(
                    network,
                    field(input, "conversation_id")?,
                    field(input, "work_id")?,
                    field(input, "request_id")?,
                    input.get("response").context("response is required")?,
                    serde_json::from_value(input["attestation"].clone())
                        .context("the user's signature is required")?,
                )
                .await
            }
            "cancel_manager" | "reconcile" | "resolve" => {
                ensure!(
                    input["human"] == true,
                    "management control requires the user"
                );
                let access = Access {
                    network_id: network.into(),
                    human: true,
                    conversation_id: Some(field(input, "conversation_id")?.into()),
                    manager: None,
                };
                match kind {
                    "cancel_manager" => self.cancel_manager(&access, input).await,
                    _ => self.pending_check_control(&access, input, kind).await,
                }
            }
            _ => bail!("unknown peer request"),
        }
    }

    async fn hello(&self, network: &str) -> Result<Value> {
        let state = self.membership_state().await?;
        let heads = self.store.lock().await.heads(network)?;
        Ok(
            json!({"type":"hello","network_id":network,"vector":heads,"boot":self.boot,
            "membership_epoch":state.epoch(),"membership":membership_digest(&state)?}),
        )
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
                let local: BTreeMap<String, (u64, String)> =
                    serde_json::from_value(ours["vector"].clone())?;
                let behind = theirs.iter().any(|(author, (seq, _))| {
                    local.get(author).map(|head| head.0).unwrap_or(0) < *seq
                });
                if behind || input["membership"] != ours["membership"] {
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
                if incoming.epoch() >= self.membership_state().await?.epoch() {
                    self.install_membership(incoming).await?;
                }
                Ok(json!({"state":self.membership_state().await?}))
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
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5)) => {},
                _ = self.sync_wakeup.notified() => {},
            }
            let Ok(state) = self.membership_state().await else {
                continue;
            };
            let Ok(view) = state.project() else { continue };
            let network = view.network_id.clone();
            // A new path (say, the relay came up) reaches peers now, not at the next Hello.
            match self.refresh_own_address(&network).await {
                Ok(true) => self.wake_sync_all(),
                Ok(false) => {}
                Err(error) => tracing::debug!(%error, "address record refresh pending"),
            }
            let local = self.identity.device_id().unwrap_or_default();
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
            let members = Arc::new(peers.clone());
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
                let members = members.clone();
                tokio::spawn(async move {
                    let result = node.sync_peer(&network, &peer).await;
                    if result.is_err() && node.unreachable(&members, &peer) {
                        node.set_write_state(
                            &network,
                            &[WriteState::Aligning],
                            WriteState::ReadOnly,
                        )
                        .await;
                    }
                    let mut states = node.sync_peers.lock().expect("sync peers");
                    let entry = states.entry(peer.clone()).or_default();
                    entry.running = false;
                    match &result {
                        Ok(()) => {
                            entry.failures = 0;
                            entry.synced_ms = Some(now_ms());
                            entry.due = Some(Instant::now() + HELLO_INTERVAL);
                        }
                        Err(error) => {
                            entry.failures = entry.failures.saturating_add(1);
                            let wait = Duration::from_secs(1 << entry.failures.min(5));
                            entry.due = Some(Instant::now() + wait.min(RETRY_LIMIT));
                            tracing::debug!(%peer, %error, "sync with peer pending");
                        }
                    }
                    if std::mem::take(&mut entry.rerun) {
                        entry.due = Some(Instant::now());
                        drop(states);
                        node.sync_wakeup.notify_one();
                    }
                });
            }
        }
    }

    /// One `Hello` round with `peer`: align membership, swap address books, then pull every
    /// author range the peer has beyond ours.
    pub(super) async fn sync_peer(self: &Arc<Self>, network: &str, peer: &str) -> Result<()> {
        let net = self.net()?.clone();
        let ours = self.hello(network).await?;
        let theirs = net.request(peer, net::SYNC, &ours, PEER_TIMEOUT).await?;
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
            if incoming.epoch() >= state.epoch() {
                self.install_membership(incoming).await?;
            }
        }
        if let Err(error) = self.exchange_addresses(network, peer).await {
            tracing::debug!(%peer, %error, "address book exchange pending");
        }
        let vector: BTreeMap<String, (u64, String)> =
            serde_json::from_value(theirs["vector"].clone()).context("invalid peer vector")?;
        for _pass in 0..4 {
            let mut progressed = false;
            for (author, (seq, _)) in &vector {
                loop {
                    let local = self.local_seq(network, author).await?;
                    if local >= *seq {
                        break;
                    }
                    let reply = net
                        .request(
                            peer,
                            net::SYNC,
                            &json!({"type":"pull","network_id":network,"author":author,
                                "from_seq":local + 1,"limit":crate::store::MAX_PULL_EVENTS}),
                            PEER_TIMEOUT,
                        )
                        .await?;
                    let events: Vec<SignedEvent> = serde_json::from_value(reply["events"].clone())?;
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
        self.check_own_log(network, peer, &vector).await?;
        if let Err(error) = self.fetch_blobs(network, peer).await {
            tracing::debug!(%peer, %error, "attachment download pending");
        }
        Ok(())
    }

    /// Whether the last sync with `peer` failed.
    pub(crate) fn sync_failing(&self, peer: &str) -> bool {
        let states = self.sync_peers.lock().expect("sync peers");
        states.get(peer).is_some_and(|state| state.failures > 0)
    }

    /// Whether every other member Node has failed since boot, counting this failure of `peer`.
    fn unreachable(&self, members: &[String], peer: &str) -> bool {
        let states = self.sync_peers.lock().expect("sync peers");
        members.iter().all(|member| {
            member == peer || states.get(member).is_some_and(|state| state.failures > 0)
        })
    }

    /// Starting up with other member Nodes, this one waits to learn it holds every event it
    /// wrote before (a restored backup may not); with none, it writes right away.
    pub(super) async fn align_on_start(&self) -> Result<bool> {
        if !self.has_membership().await? {
            return Ok(true);
        }
        let (network, peers) = self.member_peers().await?;
        let forked = self
            .store
            .lock()
            .await
            .has_conflicts_by(&network, &self.identity.device_id()?)?;
        let state = match (forked, peers.is_empty()) {
            (true, _) => WriteState::Forked,
            (false, true) => WriteState::Writable,
            (false, false) => WriteState::Aligning,
        };
        self.set_write_state(&network, &[WriteState::Writable], state)
            .await;
        Ok(state == WriteState::Writable)
    }

    /// Compares what `peer` holds of this device's log with ours. A different event at the
    /// same sequence means another copy of this data directory wrote it: that copy is kept
    /// aside beside ours, and this Node stops writing.
    async fn check_own_log(
        &self,
        network: &str,
        peer: &str,
        vector: &BTreeMap<String, (u64, String)>,
    ) -> Result<()> {
        let own = self.identity.device_id()?;
        let theirs = vector.get(&own);
        let local = self.local_seq(network, &own).await?;
        let ours = match theirs {
            Some((seq, _)) if *seq <= local => {
                self.store.lock().await.hash_at(network, &own, *seq)?
            }
            _ => None,
        };
        if let Some((seq, hash)) = theirs
            && *seq <= local
            && ours.as_deref() != Some(hash)
        {
            let reply = self
                .net()?
                .request(
                    peer,
                    net::SYNC,
                    &json!({"type":"pull","network_id":network,"author":own,"from_seq":seq,"limit":1}),
                    PEER_TIMEOUT,
                )
                .await?;
            let events: Vec<SignedEvent> = serde_json::from_value(reply["events"].clone())?;
            self.ingest(network, &events).await?;
        }
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
                tracing::warn!(%peer, "another copy of this device wrote different events; writes stopped");
            }
        } else if theirs.is_none_or(|(seq, _)| *seq <= local) {
            let from = [WriteState::Aligning, WriteState::ReadOnly];
            self.set_write_state(network, &from, WriteState::Writable)
                .await;
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
        if let Err(error) = store.set_write_state(to) {
            tracing::warn!(%error, "write state change pending");
            return false;
        }
        self.write_state.send_replace(to);
        drop(store);
        tracing::info!(state = ?to, "write state changed");
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
                }
            })
            .collect()
    }

    /// The troubleshooting export: versions, configuration (it holds no secrets), write and peer
    /// state, and the end of this Node's logs with invites and the entry token blanked.
    pub(crate) async fn diagnostics(&self, network: &str) -> Result<Value> {
        let conflicts = self.store.lock().await.conflicts(network)?.len();
        let logs: serde_json::Map<String, Value> = [("node.log", 400), ("service.stderr.log", 50)]
            .into_iter()
            .filter_map(|(name, keep)| {
                let text = std::fs::read_to_string(self.data_dir.join(name)).ok()?;
                let lines: Vec<&str> = text.lines().collect();
                let tail = lines[lines.len().saturating_sub(keep)..]
                    .iter()
                    .map(|line| redact(line, self.entry_token.as_deref()))
                    .collect();
                Some((name.to_owned(), Value::Array(tail)))
            })
            .collect();
        Ok(json!({
            "generated_at_ms":now_ms(), "version":env!("CARGO_PKG_VERSION"),
            "os":std::env::consts::OS, "arch":std::env::consts::ARCH,
            "device_id":self.identity.device_id()?, "config":self.config,
            "write_state":*self.write_state.borrow(), "conflicts":conflicts,
            "peers":self.peer_health().await, "logs":logs,
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
            self.store.lock().await.local_set(
                network,
                "write-confirmed",
                &json!({"device":device,"at_ms":now_ms()}),
            )?;
            tracing::info!(device, "write confirmed without a backup check");
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

    async fn ingest(&self, network: &str, events: &[SignedEvent]) -> Result<()> {
        let statuses = {
            let mut store = self.store.lock().await;
            let statuses = store.ingest_events(network, events)?;
            // The user's choice on an unverified result may have arrived from another entry.
            store.settle_local_works(network)?;
            statuses
        };
        let mut scopes = BTreeSet::new();
        for (event, status) in events.iter().zip(&statuses) {
            match status {
                IngestStatus::Accepted | IngestStatus::Pending => {
                    if let Ok(body) = serde_json::from_str::<Event>(&event.canonical_body) {
                        scopes.insert((body.scope_id, event.hash.clone()));
                    }
                }
                IngestStatus::Rejected(reason) => {
                    tracing::debug!(hash = %event.hash, reason, "peer event rejected")
                }
                IngestStatus::Conflict => {
                    tracing::warn!(hash = %event.hash, "conflicting event kept aside")
                }
                IngestStatus::Duplicate => {}
            }
        }
        for (scope, hash) in scopes {
            let _ = self.events.send(Update {
                network_id: network.into(),
                payload: json!({"type":"changed","conversation_id":scope,"event_hash":hash}),
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
            let _ = self
                .store
                .lock()
                .await
                .forget_address(&view.network_id, &revoked.device_id);
        }
        self.abandon_revoked_works(&view.network_id).await;
    }

    /// Work on removed devices that never reported is recorded as abandoned by the owner, and
    /// requests still waiting for those devices are dropped.
    pub(super) async fn abandon_revoked_works(&self, network: &str) {
        let Ok(view) = self
            .membership_state()
            .await
            .and_then(|state| state.project())
        else {
            return;
        };
        let removed: Vec<String> = view
            .revoked
            .iter()
            .map(|revoked| revoked.device_id.clone())
            .collect();
        if removed.is_empty() {
            return;
        }
        let result =
            self.store
                .lock()
                .await
                .abandon_works(&self.identity, network, &removed, now_ms());
        match result {
            Ok(abandoned) => {
                for (scope, work_id) in &abandoned {
                    self.publish_changed(network, scope, work_id);
                }
                if !abandoned.is_empty() {
                    self.schedule_wakeup.notify_one();
                    self.outbox_wakeup.notify_one();
                }
            }
            Err(error) => tracing::warn!(%error, "abandoning work on removed devices is pending"),
        }
        if let Err(error) = self.store.lock().await.drop_requests_to(network, &removed) {
            tracing::warn!(%error, "dropping requests to removed devices is pending");
        }
    }

    /// Where browsers reach this Node: its public entry, else its own Gateway.
    pub(crate) fn gateway_url(&self) -> Option<String> {
        match &self.config.network.entry {
            Some(entry) => Some(entry.url.clone()),
            None => self
                .config
                .roles
                .contains(&NodeRole::Gateway)
                .then(|| self.config.public_url.clone()),
        }
    }

    /// Re-signs this Node's record when its paths or Gateway changed; returns whether it did.
    async fn refresh_own_address(&self, network: &str) -> Result<bool> {
        let net = self.net()?;
        let device_id = self.identity.device_id()?;
        let endpoint = net.addr();
        let gateway_url = self.gateway_url();
        let mut store = self.store.lock().await;
        let previous = store
            .address_book(network)?
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
        let records = self.store.lock().await.address_book(network)?;
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
        // Pass news on so a Node that only reaches this one hears it before the next Hello.
        if learned {
            self.wake_sync_all();
        }
        Ok(())
    }

    /// Signed records of current member Nodes that serve browsers.
    pub(crate) async fn entrances(&self, network: &str) -> Result<Vec<SignedAddress>> {
        let view = self.membership_state().await?.project()?;
        Ok(self
            .store
            .lock()
            .await
            .address_book(network)?
            .into_iter()
            .filter(|signed| {
                signed.open(network).is_ok_and(|record| {
                    record.gateway_url.is_some() && view.node(&record.device_id).is_ok()
                })
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
            if let Err(error) = self.deliver_outbox().await {
                tracing::debug!(%error, "scheduler delivery pending");
            }
            if let Ok(state) = self.membership_state().await
                && let Err(error) = self.deliver_work_requests(&state.genesis.network_id).await
            {
                tracing::debug!(%error, "work requests wait for their devices");
            }
        }
    }

    /// Hands each waiting message or result to the conversation's current owner. The owner admits
    /// by trigger, so a retry after an unknown outcome is harmless.
    async fn deliver_outbox(self: &Arc<Self>) -> Result<()> {
        let networks = self.store.lock().await.networks()?;
        let local = self.identity.device_id()?;
        for network in networks {
            let entries = self.store.lock().await.scheduler_outbox(&network)?;
            for entry in entries {
                let owner = self
                    .store
                    .lock()
                    .await
                    .conversation(&network, &entry.conversation_id)?
                    .and_then(|conversation| conversation.owner)
                    .context("conversation has no scheduling Node")?;
                let delivered = if owner == local {
                    self.store
                        .lock()
                        .await
                        .admit_delivery(&local, &network, &entry.conversation_id, &entry.trigger)
                        .map(|receipt| json!({ "receipt": receipt }))
                } else {
                    let request = json!({"type":"deliver","conversation_id":entry.conversation_id,
                        "trigger":entry.trigger});
                    self.peer_request(&network, &owner, request).await
                };
                match delivered {
                    Ok(reply) if reply["receipt"].is_object() => {
                        self.store.lock().await.remove_outbox(
                            &network,
                            &entry.conversation_id,
                            &entry.trigger,
                        )?;
                        self.publish_changed(&network, &entry.conversation_id, &entry.trigger);
                        self.schedule_wakeup.notify_one();
                    }
                    Ok(_) => {
                        tracing::debug!(%owner, "delivery answer had no receipt")
                    }
                    Err(error) => {
                        tracing::debug!(%owner, %error, "scheduler delivery pending")
                    }
                }
            }
        }
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
        self.publish_changed(network, &scope, trigger);
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

    async fn fetch_blobs(&self, network: &str, peer: &str) -> Result<()> {
        let _download = self.blob_fetch.lock().await;
        let missing = self.store.lock().await.missing_blobs(network)?;
        for blob in missing {
            self.fetch_blob(network, peer, &blob).await?;
        }
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
                store.committed_offset(network, &blob.hash)?,
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
                    .commit_offset(network, &blob.hash, Some(offset))?;
                unsynced = 0;
            }
        }
        file.sync_all().await?;
        self.store
            .lock()
            .await
            .commit_offset(network, &blob.hash, Some(offset))?;
        ensure!(offset == blob.size, "attachment stream ended early");
        drop(file);
        let bytes = tokio::fs::read(&part).await?;
        if crate::crypto::digest(&bytes) != blob.hash {
            let _ = tokio::fs::remove_file(&part).await;
            self.store
                .lock()
                .await
                .commit_offset(network, &blob.hash, None)?;
            bail!("downloaded attachment failed its hash check");
        }
        tokio::fs::create_dir_all(target.parent().context("blob directory")?).await?;
        tokio::fs::rename(&part, &target).await?;
        self.store
            .lock()
            .await
            .commit_offset(network, &blob.hash, None)?;
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

/// Blanks the entry token and anything shaped like an invite ticket.
fn redact(line: &str, token: Option<&str>) -> Value {
    let mut line = match token {
        Some(token) => line.replace(token, "[redacted]"),
        None => line.to_owned(),
    };
    while let Some(start) = line.find("vibeface1") {
        let end = line[start..]
            .find(|c: char| !c.is_ascii_alphanumeric())
            .map_or(line.len(), |length| start + length);
        line.replace_range(start..end, "[invite]");
    }
    Value::String(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_blank_tokens_and_invites() {
        let token = "ab".repeat(32);
        let line = format!("join https://e.example/join#vibeface1qq2ww3 failed; token {token}.");
        assert_eq!(
            redact(&line, Some(&token)),
            "join https://e.example/join#[invite] failed; token [redacted]."
        );
    }
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
        configuration.codex_socket = Some(dir.join("unavailable.sock"));
        configuration.save(&dir)?;
        config::private_write(&dir.join("data-epoch.json"), br#"{"data_epoch":7}"#)?;
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
        attachments: Vec<BlobRef>,
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
                    attestation: None,
                },
            )?
            .attestation()
            .cloned()
            .context("signed message")
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
            {
                let mut store = a.store.lock().await;
                let owner = ia.owner_fixture(&network, "c", &ia.device_id()?)?;
                store.create_conversation(&ia, &network, "c", "Sync", owner, now_ms())?;
                store.put_blob(&blob, &content)?;
                store.accept_message(&ia, &network, "c", "m1", "m1", vec![blob.clone()], true,
                    user_with(&ia, &network, "m1", vec![blob.clone()])?, now_ms())?;
            }
            b.net()?.learn(&a.net()?.addr()).await;
            b.refresh_own_address(&network).await?;
            let peer_a = ia.device_id()?;
            let first = {
                let (b, network, peer) = (b.clone(), network.clone(), peer_a.clone());
                tokio::spawn(async move { b.sync_peer(&network, &peer).await })
            };
            while b.store.lock().await.committed_offset(&network, &blob.hash)? == 0
                && !first.is_finished()
            {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            // A second download of the same attachment waits instead of sharing its staging file.
            b.fetch_blobs(&network, &peer_a).await?;
            first.await??;
            let heads = a.store.lock().await.heads(&network)?;
            assert_eq!(b.store.lock().await.heads(&network)?, heads);
            assert!(b.store.lock().await.missing_blobs(&network)?.is_empty());
            assert!(b.store.lock().await.scheduler_jobs(&network, "c")?.is_empty());
            assert!(b.store.lock().await.scheduler_outbox(&network)?.is_empty());

            for id in ["m2", "m3"] {
                a.store.lock().await.accept_message(&ia, &network, "c", id, id, vec![], true,
                    user(&ia, &network, id)?, now_ms())?;
            }
            b.sync_peer(&network, &ia.device_id()?).await?;
            b.sync_peer(&network, &ia.device_id()?).await?;
            assert_eq!(b.store.lock().await.heads(&network)?, a.store.lock().await.heads(&network)?);
            assert_eq!(b.store.lock().await.messages(&network, "c")?.len(), 3);
            assert!(b.store.lock().await.scheduler_jobs(&network, "c")?.is_empty());
            assert!(
                a.store.lock().await.address_book(&network)?.iter().any(|signed| signed
                    .open(&network)
                    .is_ok_and(|record| record.device_id == ib.device_id().unwrap_or_default())),
                "the pulled Node learned the puller's signed address"
            );
            a.sync_peer(&network, &ib.device_id()?).await?;

            b.store.lock().await.accept_message(&ib, &network, "c", "m4", "m4", vec![], true,
                user(&ia, &network, "m4")?, now_ms())?;
            assert_eq!(b.store.lock().await.scheduler_outbox(&network)?.len(), 1);
            b.deliver_outbox().await?;
            assert!(b.store.lock().await.scheduler_outbox(&network)?.is_empty());
            let trigger = crate::scheduling::message_trigger("m4");
            a.store.lock().await.scheduler_job(&network, "c", &trigger)?
                .context("the owner admitted the delivered message")?;
            let again = b.peer_request(&network, &ia.device_id()?,
                json!({"type":"deliver","conversation_id":"c","trigger":trigger})).await?;
            assert_eq!(again["receipt"]["trigger"], trigger.as_str());
            assert_eq!(
                a.store.lock().await.scheduler_jobs(&network, "c")?.iter().filter(|job| job.trigger == trigger).count(),
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
            b.store.lock().await.commit_offset(&network, &blob.hash, Some(1024 * 1024))?;
            let corrupted = b.fetch_blob(&network, &ia.device_id()?, &blob).await;
            assert!(corrupted.is_err() && !part.exists(), "resumed bytes keep the synced prefix and fail the hash check");
            std::fs::write(&part, &content[..1024 * 1024])?;
            b.store.lock().await.commit_offset(&network, &blob.hash, Some(1024 * 1024))?;
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
    async fn nodes_without_direct_paths_sync_through_a_member_relay() -> Result<()> {
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
        let hidden = net::NetworkConfig {
            relays: vec![relay_url.clone()],
            relay_paths_only: true,
            ..Default::default()
        };
        let (a, dir_a) = node_with(&ids[1], hidden.clone()).await?;
        let (b, dir_b) = node_with(&ids[2], hidden).await?;
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
            assert_eq!(
                addr.ip_addrs().count(),
                0,
                "relay_paths_only advertises no IP path"
            );
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
}
