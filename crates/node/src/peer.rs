//! Authenticated, network-scoped peer transport; business requests remain opaque.

use crate::{
    crypto::{LocalIdentity, digest, verify_payload},
    model::{NetworkId, NodeRole, PeerId, WIRE_VERSION},
    network::{AuthenticationFailure, NetworkHub, NetworkSocket, Service},
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use axum::extract::ws::{Message as AxumMessage, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
#[cfg(test)]
use tokio_tungstenite::{connect_async_with_config, tungstenite::protocol::WebSocketConfig};
use uuid::Uuid;
use vibeface_secure::{Channel, Handshake};

pub const MAX_PACKET_BYTES: usize = 2 * 1024 * 1024;
const MAX_HANDSHAKE_BYTES: usize = 16 * 1024;
const MAX_HOPS: u8 = 16;
const MAX_CONNECTIONS: usize = 128;
const MAX_NETWORK_CONNECTIONS: usize = 32;
const MAX_PENDING: usize = 256;
const MAX_SEEN: usize = 8192;
const QUEUE_CAPACITY: usize = 64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT: Duration = Duration::from_secs(20);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_AGE_MS: i64 = 120_000;
const MAX_FUTURE_MS: i64 = 30_000;
const PRESENCE_LEASE_MS: i64 = 90_000;
const MAX_EXPLORATION_NEIGHBORS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestFailure {
    NoRoute,
    Busy,
    Unknown,
}
impl std::fmt::Display for RequestFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoRoute => "no peer route available; request was not queued",
            Self::Busy => "peer queue busy; request was not queued",
            Self::Unknown => "peer response unavailable; execution outcome may be unknown",
        })
    }
}
impl std::error::Error for RequestFailure {}

#[derive(Debug, Clone)]
pub(crate) struct AuthorizationDenied(Arc<anyhow::Error>);
impl std::fmt::Display for AuthorizationDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "peer authorization denied: {}", self.0)
    }
}
impl std::error::Error for AuthorizationDenied {}

#[derive(Debug, PartialEq, Eq)]
enum SendOutcome {
    NoRoute,
    Busy,
    Queued(usize),
}
impl SendOutcome {
    fn observed(self, packet: &Packet) -> Self {
        tracing::debug!(network=%packet.body.network_id, target=?packet.body.target,
            request_id=%packet.body.request_id, kind=?packet.body.kind, outcome=?self,
            "peer local send outcome");
        self
    }
}

#[derive(Debug, Clone)]
pub struct Inbound {
    pub network_id: NetworkId,
    pub origin: PeerId,
    pub request_id: String,
    pub payload: Value,
    pub broadcast: bool,
}

pub struct PeerHub {
    identity: LocalIdentity,
    store: Arc<Mutex<Store>>,
    relay_enabled: bool,
    state: Mutex<State>,
    inbound: mpsc::Sender<Inbound>,
    sockets: Arc<Semaphore>,
    queued_bytes: Arc<Semaphore>,
}

#[derive(Default)]
struct State {
    links: HashMap<(NetworkId, PeerId), Link>,
    pending: HashMap<(NetworkId, String), Pending>,
    seen: HashMap<PacketId, SeenPacket>,
    routes: HashMap<(NetworkId, PeerId, PeerId), PresenceRoute>,
    last_presence: HashMap<NetworkId, Instant>,
}
struct Link {
    id: String,
    outbound: bool,
    send: mpsc::Sender<Arc<Queued>>,
}
struct PresenceRoute {
    link_id: String,
    signed_at_ms: i64,
    expires_at: Instant,
}
struct SeenPacket {
    hash: String,
    observed_at: Instant,
    presence_expires_at: Option<Instant>,
}
struct Queued {
    text: String,
    _permit: OwnedSemaphorePermit,
}
struct Pending {
    origin: PeerId,
    created: Instant,
    reply: oneshot::Sender<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    version: u32,
    network_id: NetworkId,

    peer_id: PeerId,
}
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Request,
    Response,
    Broadcast,
    Presence,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    version: u32,
    network_id: NetworkId,
    origin: PeerId,
    target: Option<PeerId>,
    request_id: String,
    created_at_ms: i64,
    kind: Kind,
    payload: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Packet {
    body: Body,
    signature: String,
    hops_remaining: u8,
}
type PacketId = (NetworkId, PeerId, String, Kind);

fn packet_id(body: &Body) -> PacketId {
    (
        body.network_id.clone(),
        body.origin.clone(),
        body.request_id.clone(),
        body.kind,
    )
}

impl State {
    fn forget_link(&mut self, network: &str, peer: &str, id: &str) {
        let key = (network.to_owned(), peer.to_owned());
        if self.links.get(&key).is_some_and(|link| link.id == id) {
            self.links.remove(&key);
        }
        self.routes.retain(|(n, _, neighbor), route| {
            n != network || neighbor != peer || route.link_id != id
        });
    }

    fn remember(&mut self, packet: &Packet, wall_now: i64, now: Instant) -> Result<bool> {
        let id = packet_id(&packet.body);
        let hash = digest(&serde_json::to_vec(&packet.body)?);
        self.seen.retain(|_, entry| {
            now.duration_since(entry.observed_at)
                < Duration::from_millis((MAX_AGE_MS + MAX_FUTURE_MS) as u64)
        });
        if let Some(previous) = self.seen.get(&id) {
            ensure!(previous.hash == hash, "conflicting peer packet ID");
            return Ok(false);
        }
        ensure!(self.seen.len() < MAX_SEEN, RequestFailure::Busy);
        let remaining = packet
            .body
            .created_at_ms
            .saturating_add(PRESENCE_LEASE_MS)
            .saturating_sub(wall_now)
            .clamp(0, PRESENCE_LEASE_MS);
        self.seen.insert(
            id,
            SeenPacket {
                hash,
                observed_at: now,
                presence_expires_at: (packet.body.kind == Kind::Presence)
                    .then_some(now + Duration::from_millis(remaining as u64)),
            },
        );
        Ok(true)
    }

    fn observe_presence(
        &mut self,
        neighbor: &str,
        link_id: &str,
        body: &Body,
        wall_now: i64,
        now: Instant,
    ) {
        let key = (
            body.network_id.clone(),
            body.origin.clone(),
            neighbor.to_owned(),
        );
        if self.routes.get(&key).is_some_and(|route| {
            route.link_id == link_id && route.signed_at_ms >= body.created_at_ms
        }) {
            return;
        }
        let remaining = body
            .created_at_ms
            .saturating_add(PRESENCE_LEASE_MS)
            .saturating_sub(wall_now)
            .clamp(0, PRESENCE_LEASE_MS);
        if remaining == 0 {
            return;
        }
        let expires_at = self
            .seen
            .get(&packet_id(body))
            .and_then(|entry| entry.presence_expires_at)
            .unwrap_or(now)
            .min(now + Duration::from_millis(remaining as u64));
        if expires_at <= now {
            return;
        }
        self.routes.retain(|(network, _, neighbor), route| {
            route.expires_at > now
                && self
                    .links
                    .get(&(network.clone(), neighbor.clone()))
                    .is_some_and(|link| link.id == route.link_id)
        });
        if !self.routes.contains_key(&key)
            && self.routes.len() >= MAX_SEEN
            && let Some(oldest) = self
                .routes
                .iter()
                .min_by_key(|(_, route)| route.expires_at)
                .map(|(key, _)| key.clone())
        {
            self.routes.remove(&oldest);
        }
        self.routes.insert(
            key,
            PresenceRoute {
                link_id: link_id.to_owned(),
                signed_at_ms: body.created_at_ms,
                expires_at,
            },
        );
        tracing::debug!(network=%body.network_id, target=%body.origin, neighbor, link_id,
            signed_at_ms=body.created_at_ms, remaining_ms=expires_at.saturating_duration_since(now).as_millis() as u64,
            "peer presence route observed");
    }
}

impl PeerHub {
    #[cfg(test)]
    pub(crate) async fn reserve_replay_entries(
        &self,
        network: &str,
        remaining: usize,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        for index in state.seen.len()..MAX_SEEN.saturating_sub(remaining) {
            state.seen.insert(
                (
                    network.to_owned(),
                    self.identity.peer_id()?,
                    Uuid::from_u128(index as u128).to_string(),
                    Kind::Presence,
                ),
                SeenPacket {
                    hash: String::new(),
                    observed_at: Instant::now(),
                    presence_expires_at: None,
                },
            );
        }
        Ok(())
    }

    pub fn new(
        identity: LocalIdentity,
        store: Arc<Mutex<Store>>,
        enabled_roles: &[NodeRole],
    ) -> (Arc<Self>, mpsc::Receiver<Inbound>) {
        let (inbound, receive) = mpsc::channel(256);
        (
            Arc::new(Self {
                identity,
                store,
                relay_enabled: enabled_roles.contains(&NodeRole::Relay),
                state: Mutex::new(State::default()),
                inbound,
                sockets: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
                queued_bytes: Arc::new(Semaphore::new(32 * 1024 * 1024)),
            }),
            receive,
        )
    }

    /// The upgrade handler must also set its frame and message limits to MAX_PACKET_BYTES.
    pub async fn accept(self: Arc<Self>, socket: WebSocket) -> Result<()> {
        self.accept_transport(NetworkSocket::server(socket)).await
    }

    pub async fn accept_transport(self: Arc<Self>, mut socket: NetworkSocket) -> Result<()> {
        let _permit = self
            .sockets
            .clone()
            .try_acquire_owned()
            .context("too many peer sockets")?;
        let (network, peer, channel) =
            tokio::time::timeout(HANDSHAKE_TIMEOUT, self.server_handshake(&mut socket))
                .await
                .context("peer handshake timed out")??;
        self.connected(socket, network, peer, false, channel, None)
            .await
    }

    /// Runs until disconnected. Reconnection policy belongs to the runtime.
    #[cfg(test)]
    pub async fn connect(self: Arc<Self>, network_id: String, url: String) -> Result<()> {
        let _permit = self
            .sockets
            .clone()
            .try_acquire_owned()
            .context("too many peer sockets")?;
        self.authorize(&network_id, &self.identity.peer_id()?)
            .await?;
        ensure!(
            url.starts_with("ws://") || url.starts_with("wss://"),
            "invalid peer URL scheme"
        );
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_PACKET_BYTES))
            .max_frame_size(Some(MAX_PACKET_BYTES));
        let (socket, _) = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            connect_async_with_config(&url, Some(config), false),
        )
        .await
        .context("peer connection timed out")?
        .map_err(|_| anyhow::anyhow!("peer connection failed"))?;
        let mut socket = NetworkSocket::Client(Box::new(socket));
        let (peer, channel) = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            self.client_handshake(&mut socket, &network_id, None),
        )
        .await
        .context("peer handshake timed out")??;
        self.connected(socket, network_id, peer, true, channel, None)
            .await
    }

    pub async fn request(&self, network: &str, target: &str, payload: Value) -> Result<Value> {
        let packet = self.packet(
            network,
            Some(target.to_owned()),
            Kind::Request,
            payload,
            None,
        )?;
        let key = (network.to_owned(), packet.body.request_id.clone());
        let blob_range = (packet.body.payload["type"] == "blob").then(|| {
            (
                packet.body.payload["hash"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                packet.body.payload["offset"].as_u64(),
                packet.body.payload["limit"].as_u64(),
            )
        });
        let (reply, receive) = oneshot::channel();
        {
            let mut state = self.state.lock().await;
            state
                .pending
                .retain(|_, p| !p.reply.is_closed() && p.created.elapsed() < REQUEST_TIMEOUT);
            ensure!(state.pending.len() < MAX_PENDING, RequestFailure::Busy);
            state.pending.insert(
                key.clone(),
                Pending {
                    origin: target.to_owned(),
                    created: Instant::now(),
                    reply,
                },
            );
        }
        let result = match self.emit(packet).await {
            Ok(()) => {
                if let Some((hash, offset, limit)) = blob_range {
                    tracing::trace!(network, target, request_id=%key.1, hash, offset, limit,
                        "peer blob request queued");
                }
                match tokio::time::timeout(REQUEST_TIMEOUT, receive).await {
                    Ok(Ok(value)) => Ok(value),
                    Ok(Err(_)) | Err(_) => {
                        tracing::debug!(network, target, request_id=%key.1, "peer request outcome unknown");
                        Err(RequestFailure::Unknown.into())
                    }
                }
            }
            Err(error) => Err(error),
        };
        self.state.lock().await.pending.remove(&key);
        result
    }

    /// Sends to reachable peers; durable delivery and offline replay belong to the ledger.
    pub async fn broadcast(&self, network: &str, payload: Value) -> Result<()> {
        self.emit(self.packet(network, None, Kind::Broadcast, payload, None)?)
            .await
    }

    pub async fn respond(&self, inbound: &Inbound, payload: Value) -> Result<()> {
        ensure!(!inbound.broadcast, "cannot respond to a peer broadcast");
        let packet = self.packet(
            &inbound.network_id,
            Some(inbound.origin.clone()),
            Kind::Response,
            payload,
            Some(inbound.request_id.clone()),
        )?;
        self.emit(packet).await
    }

    /// Presence is soft state; an online entry does not guarantee a request will succeed.
    pub async fn online(&self, network: &str) -> Vec<PeerId> {
        let (direct, indirect) = {
            let state = self.state.lock().await;
            let direct: BTreeSet<_> = state
                .links
                .iter()
                .filter(|((n, _), link)| n == network && !link.send.is_closed())
                .map(|((_, n), _)| n.clone())
                .collect();
            let indirect: Vec<_> = state
                .routes
                .iter()
                .filter(|((n, _, neighbor), route)| {
                    n == network
                        && route.expires_at > Instant::now()
                        && state
                            .links
                            .get(&(n.clone(), neighbor.clone()))
                            .is_some_and(|link| link.id == route.link_id && !link.send.is_closed())
                })
                .map(|((_, target, neighbor), _)| (target.clone(), neighbor.clone()))
                .collect();
            (direct, indirect)
        };
        let (direct, indirect) = {
            let store = self.store.lock().await;
            let direct = direct
                .into_iter()
                .filter(|node| store.authorize_node(network, node, None).is_ok())
                .collect::<Vec<_>>();
            let indirect = indirect
                .into_iter()
                .filter(|(target, neighbor)| {
                    store.authorize_node(network, target, None).is_ok()
                        && store
                            .authorize_node(
                                network,
                                neighbor,
                                (target != neighbor).then_some(NodeRole::Relay),
                            )
                            .is_ok()
                })
                .collect::<Vec<_>>();
            (direct, indirect)
        };
        let state = self.state.lock().await;
        direct
            .into_iter()
            .filter(|node| {
                state
                    .links
                    .get(&(network.to_owned(), node.clone()))
                    .is_some_and(|link| !link.send.is_closed())
            })
            .chain(indirect.into_iter().filter_map(|(target, neighbor)| {
                state
                    .routes
                    .get(&(network.to_owned(), target.clone(), neighbor.clone()))
                    .is_some_and(|route| {
                        route.expires_at > Instant::now()
                            && state
                                .links
                                .get(&(network.to_owned(), neighbor))
                                .is_some_and(|link| {
                                    link.id == route.link_id && !link.send.is_closed()
                                })
                    })
                    .then_some(target)
            }))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(crate) async fn has_route(&self, network: &str, target: &str) -> bool {
        self.online(network).await.iter().any(|node| node == target)
    }

    pub(crate) async fn link_matches(&self, network: &str, target: &str, id: &str) -> bool {
        self.state
            .lock()
            .await
            .links
            .get(&(network.to_owned(), target.to_owned()))
            .is_some_and(|link| link.id == id && !link.send.is_closed())
    }

    pub(crate) async fn authorize(&self, network: &str, node: &str) -> Result<String> {
        Ok(self
            .store
            .lock()
            .await
            .authorize_node(network, node, None)
            .map_err(|error| AuthorizationDenied(Arc::new(error)))?
            .public_key)
    }

    fn hello(&self, network: &str) -> Result<Hello> {
        Ok(Hello {
            version: WIRE_VERSION,
            network_id: network.to_owned(),
            peer_id: self.identity.peer_id()?,
        })
    }

    async fn check_hello(&self, hello: &Hello, network: Option<&str>) -> Result<String> {
        ensure!(hello.version == WIRE_VERSION, "unsupported peer version");
        check_hex_id(&hello.network_id)?;
        check_hex_id(&hello.peer_id)?;
        ensure!(
            network.is_none_or(|n| n == hello.network_id),
            "peer belongs to another network"
        );
        let local = self.identity.peer_id()?;
        ensure!(hello.peer_id != local, "cannot connect a node to itself");
        self.authorize(&hello.network_id, &local).await?;
        self.authorize(&hello.network_id, &hello.peer_id).await
    }

    async fn server_handshake(
        &self,
        socket: &mut NetworkSocket,
    ) -> Result<(String, String, Channel)> {
        let mut handshake = secure(Handshake::new(
            false,
            &self.identity.secret_key_hex,
            None,
            b"vibeface:peer:v1",
        ))?;
        let first = receive_binary(socket, MAX_HANDSHAKE_BYTES).await?;
        ensure!(
            secure(handshake.read(&first))?.is_empty(),
            "unexpected first peer payload"
        );
        let response = secure(handshake.write(&[]))?;
        send_binary(socket, response).await?;
        let third = receive_binary(socket, MAX_HANDSHAKE_BYTES).await?;
        let plaintext = secure(handshake.read(&third))?;
        let initiator: Hello = serde_json::from_slice(&plaintext)?;
        self.check_hello(&initiator, None).await?;
        ensure!(
            handshake.remote_id().as_deref() == Some(&initiator.peer_id),
            "peer Noise identity mismatch"
        );
        let mut channel = secure(handshake.finish())?;
        let responder = self.hello(&initiator.network_id)?;
        send_secure(socket, &mut channel, &serde_json::to_vec(&responder)?).await?;
        Ok((initiator.network_id, initiator.peer_id, channel))
    }

    async fn client_handshake(
        &self,
        socket: &mut NetworkSocket,
        network: &str,
        target: Option<String>,
    ) -> Result<(PeerId, Channel)> {
        let mut handshake = secure(Handshake::new(
            true,
            &self.identity.secret_key_hex,
            target,
            b"vibeface:peer:v1",
        ))?;
        send_binary(socket, secure(handshake.write(&[]))?).await?;
        let second = receive_binary(socket, MAX_HANDSHAKE_BYTES).await?;
        ensure!(
            secure(handshake.read(&second))?.is_empty(),
            "unexpected peer responder payload"
        );
        let remote = handshake.remote_id().context("peer omitted its identity")?;
        self.authorize(network, &remote).await?;
        let hello = self.hello(network)?;
        send_binary(
            socket,
            secure(handshake.write(&serde_json::to_vec(&hello)?))?,
        )
        .await?;
        let mut channel = secure(handshake.finish())?;
        let responder: Hello = serde_json::from_slice(
            &receive_secure(socket, &mut channel, MAX_HANDSHAKE_BYTES).await?,
        )?;
        ensure!(remote == responder.peer_id, "peer Noise identity mismatch");
        self.check_hello(&responder, Some(network)).await?;
        Ok((responder.peer_id, channel))
    }

    pub async fn connect_target(
        self: Arc<Self>,
        network: String,
        target: String,
        transport: Arc<NetworkHub>,
        ready: Option<oneshot::Sender<String>>,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let preparation = async {
            self.authorize(&network, &self.identity.peer_id()?).await?;
            self.authorize(&network, &target).await?;
            let winner = transport
                .dial_authenticated(&target, Service::Peer, None, |mut socket| {
                    let hub = self.clone();
                    let network = network.clone();
                    let target = target.clone();
                    async move {
                        let permit = hub.sockets.clone().try_acquire_owned().map_err(|_| {
                            AuthenticationFailure::Busy(RequestFailure::Busy.into())
                        })?;
                        let result = tokio::time::timeout(
                            HANDSHAKE_TIMEOUT,
                            hub.client_handshake(&mut socket, &network, Some(target)),
                        )
                        .await
                        .context("peer handshake timed out")
                        .and_then(|result| result);
                        match result {
                            Ok((peer, channel)) => Ok((socket, (peer, channel, permit))),
                            Err(error) if error.downcast_ref::<AuthorizationDenied>().is_some() => {
                                Err(AuthenticationFailure::Terminal(error))
                            }
                            Err(error) => Err(AuthenticationFailure::Path(error)),
                        }
                    }
                })
                .await?;
            self.authorize(&network, &self.identity.peer_id()?).await?;
            self.authorize(&network, &target).await?;
            Ok::<_, anyhow::Error>(winner)
        };
        let (socket, (peer, channel, _permit)) = tokio::time::timeout_at(deadline, preparation)
            .await
            .context("peer route preparation deadline exceeded")??;
        self.connected(socket, network, peer, true, channel, ready)
            .await
    }

    async fn connected(
        self: Arc<Self>,
        socket: NetworkSocket,
        network: String,
        peer: String,
        outbound: bool,
        channel: Channel,
        ready: Option<oneshot::Sender<String>>,
    ) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        let (send, receive) = mpsc::channel(QUEUE_CAPACITY);
        let link_key = (network.clone(), peer.clone());
        {
            let local = self.identity.peer_id()?;
            let mut state = self.state.lock().await;
            if let Some(previous) = state.links.get(&link_key) {
                // Both ends choose the same direction if they connect simultaneously.
                let preferred = local < peer;
                ensure!(
                    outbound == preferred && previous.outbound != preferred,
                    "peer is already connected"
                );
            } else {
                ensure!(
                    state.links.len() < MAX_CONNECTIONS,
                    "too many peer connections"
                );
                ensure!(
                    state.links.keys().filter(|(n, _)| n == &network).count()
                        < MAX_NETWORK_CONNECTIONS,
                    "too many network peer connections"
                );
            }
            state.links.insert(
                link_key.clone(),
                Link {
                    id: id.clone(),
                    outbound,
                    send,
                },
            );
        }
        tracing::debug!(network, peer, link_id=%id, outbound, "peer link ready");
        if let Some(ready) = ready {
            let _ = ready.send(id.clone());
        }
        let result = self
            .drive(socket, &network, &peer, &id, receive, channel)
            .await;
        let mut state = self.state.lock().await;
        let current = state.links.get(&link_key).is_some_and(|link| link.id == id);
        let routes_before = state.routes.len();
        state.forget_link(&network, &peer, &id);
        tracing::debug!(network, peer, link_id=%id, current, removed_routes=routes_before-state.routes.len(),
            "peer link removed");
        result
    }

    async fn drive(
        &self,
        mut socket: NetworkSocket,
        network: &str,
        peer: &str,
        link_id: &str,
        mut outbound: mpsc::Receiver<Arc<Queued>>,
        mut channel: Channel,
    ) -> Result<()> {
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut received = Instant::now();
        let mut partial_started: Option<Instant> = None;
        loop {
            tokio::select! {
                frame = socket.next() => {
                    let frame = frame.context("peer disconnected")??;
                    self.authorize(network, peer).await?;
                    self.authorize(network, &self.identity.peer_id()?).await?;
                    received = Instant::now();
                    match frame {
                        AxumMessage::Binary(bytes) => {
                            if let Some(plaintext) = secure(channel.open_record(&bytes))? {
                                partial_started = None;
                                ensure!(plaintext.len() <= MAX_PACKET_BYTES, "peer packet too large");
                                let packet: Packet = serde_json::from_slice(&plaintext).context("invalid peer packet")?;
                                self.receive_packet(network, peer, link_id, packet).await?;
                            } else {
                                partial_started.get_or_insert_with(Instant::now);
                            }
                        }
                        AxumMessage::Ping(_) | AxumMessage::Pong(_) => {},
                        _ => bail!("peer accepts only encrypted binary frames"),
                    }
                }
                queued = outbound.recv() => {
                    let text = queued.context("peer connection replaced")?;
                    self.authorize(network, peer).await?;
                    self.authorize(network, &self.identity.peer_id()?).await?;
                    send_secure(&mut socket, &mut channel, text.text.as_bytes()).await?;
                }
                _ = heartbeat.tick() => {
                    ensure!(received.elapsed() < IDLE_TIMEOUT, "peer heartbeat timed out");
                    ensure!(partial_started.is_none_or(|start| start.elapsed() < IDLE_TIMEOUT), "peer fragmented message timed out");
                    self.authorize(network, peer).await?;
                    self.authorize(network, &self.identity.peer_id()?).await?;
                    tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.send(AxumMessage::Ping(Vec::new().into()))).await.context("peer send timed out")??;
                    if let Err(error) = self.presence(network).await {
                        if error.downcast_ref::<RequestFailure>().is_none() {
                            return Err(error);
                        }
                        tracing::debug!(network, peer, %error, "peer presence rejected locally");
                    }
                }
            }
        }
    }

    fn packet(
        &self,
        network: &str,
        target: Option<String>,
        kind: Kind,
        payload: Value,
        request_id: Option<String>,
    ) -> Result<Packet> {
        let body = Body {
            version: WIRE_VERSION,
            network_id: network.to_owned(),
            origin: self.identity.peer_id()?,
            target,
            request_id: request_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
            created_at_ms: now_ms()?,
            kind,
            payload,
        };
        validate_body(&body, now_ms()?)?;
        let signature = self
            .identity
            .sign_payload("peer-packet", &serde_json::to_vec(&body)?)?;
        let packet = Packet {
            body,
            signature,
            hops_remaining: MAX_HOPS,
        };
        ensure!(
            serde_json::to_vec(&packet)?.len() <= MAX_PACKET_BYTES,
            "peer packet too large"
        );
        Ok(packet)
    }

    async fn presence(&self, network: &str) -> Result<()> {
        {
            let mut state = self.state.lock().await;
            if state
                .last_presence
                .get(network)
                .is_some_and(|time| time.elapsed() < HEARTBEAT)
            {
                return Ok(());
            }
            state
                .last_presence
                .insert(network.to_owned(), Instant::now());
        }
        self.emit(self.packet(network, None, Kind::Presence, Value::Null, None)?)
            .await
    }

    async fn emit(&self, packet: Packet) -> Result<()> {
        self.authorize(&packet.body.network_id, &packet.body.origin)
            .await?;
        if let Some(target) = &packet.body.target {
            self.authorize(&packet.body.network_id, target).await?;
        }
        if !self.remember(&packet).await? {
            return Ok(());
        }
        if packet.body.target.as_deref() == Some(packet.body.origin.as_str()) {
            return self.deliver(&packet).await;
        }
        match self.forward(&packet, None).await? {
            SendOutcome::Queued(_) => Ok(()),
            SendOutcome::NoRoute
                if matches!(packet.body.kind, Kind::Broadcast | Kind::Presence) =>
            {
                Ok(())
            }
            SendOutcome::NoRoute => Err(RequestFailure::NoRoute.into()),
            SendOutcome::Busy => Err(RequestFailure::Busy.into()),
        }
    }

    async fn receive_packet(
        &self,
        network: &str,
        peer: &str,
        link_id: &str,
        packet: Packet,
    ) -> Result<()> {
        ensure!(
            packet.body.network_id == network,
            "cross-network peer packet"
        );
        ensure!(packet.hops_remaining <= MAX_HOPS, "invalid peer hop limit");
        validate_body(&packet.body, now_ms()?)?;
        let key = match self.authorize(network, &packet.body.origin).await {
            Ok(key) => key,
            Err(error) => {
                tracing::debug!(%error, origin=%packet.body.origin, network, "dropping peer packet from an unknown member");
                return Ok(());
            }
        };
        if let Some(target) = &packet.body.target
            && let Err(error) = self.authorize(network, target).await
        {
            tracing::debug!(%error, target, network, "dropping peer packet for an unknown member");
            return Ok(());
        }
        verify_payload(
            &key,
            "peer-packet",
            &serde_json::to_vec(&packet.body)?,
            &packet.signature,
        )?;
        let fresh = {
            let mut state = self.state.lock().await;
            ensure!(
                state
                    .links
                    .get(&(network.to_owned(), peer.to_owned()))
                    .is_some_and(|link| link.id == link_id),
                "stale peer link"
            );
            let now = Instant::now();
            let wall_now = now_ms()?;
            let fresh = match state.remember(&packet, wall_now, now) {
                Ok(fresh) => fresh,
                Err(error)
                    if error.downcast_ref::<RequestFailure>() == Some(&RequestFailure::Busy) =>
                {
                    tracing::debug!(network, peer, "peer replay cache rejected busy");
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            if packet.body.kind == Kind::Presence {
                state.observe_presence(peer, link_id, &packet.body, wall_now, now);
            }
            fresh
        };
        if !fresh {
            return Ok(());
        }
        let local = self.identity.peer_id()?;
        if packet.body.target.as_deref() == Some(local.as_str()) {
            return match self.deliver(&packet).await {
                Err(error)
                    if error.downcast_ref::<RequestFailure>() == Some(&RequestFailure::Busy) =>
                {
                    tracing::debug!(network, peer, request_id=%packet.body.request_id, "peer inbound rejected busy");
                    Ok(())
                }
                result => result,
            };
        }
        if packet.body.kind == Kind::Broadcast
            && let Err(error) = self.deliver(&packet).await
        {
            if error.downcast_ref::<RequestFailure>() != Some(&RequestFailure::Busy) {
                return Err(error);
            }
            tracing::debug!(network, peer, "peer broadcast inbound rejected busy");
        }
        if self.relay_enabled
            && packet.hops_remaining > 0
            && self
                .store
                .lock()
                .await
                .authorize_node(network, &local, Some(NodeRole::Relay))
                .is_ok()
        {
            let mut forwarded = packet;
            forwarded.hops_remaining -= 1;
            let outcome = self.forward(&forwarded, Some(peer)).await?;
            if !matches!(outcome, SendOutcome::Queued(_)) {
                tracing::debug!(network, peer, ?outcome, "peer forward rejected locally");
            }
        }
        Ok(())
    }

    async fn remember(&self, packet: &Packet) -> Result<bool> {
        self.state
            .lock()
            .await
            .remember(packet, now_ms()?, Instant::now())
    }

    async fn deliver(&self, packet: &Packet) -> Result<()> {
        if packet.body.kind == Kind::Response {
            let key = (
                packet.body.network_id.clone(),
                packet.body.request_id.clone(),
            );
            let mut state = self.state.lock().await;
            if state
                .pending
                .get(&key)
                .is_some_and(|p| p.origin == packet.body.origin)
                && let Some(pending) = state.pending.remove(&key)
            {
                let delivered = pending.reply.send(packet.body.payload.clone()).is_ok();
                tracing::debug!(network=%packet.body.network_id, origin=%packet.body.origin,
                    request_id=%packet.body.request_id, delivered, "peer response matched");
            }
        } else {
            self.inbound
                .try_send(Inbound {
                    network_id: packet.body.network_id.clone(),
                    origin: packet.body.origin.clone(),
                    request_id: packet.body.request_id.clone(),
                    payload: packet.body.payload.clone(),
                    broadcast: packet.body.kind == Kind::Broadcast,
                })
                .map_err(|_| RequestFailure::Busy)?;
        }
        Ok(())
    }

    async fn forward(&self, packet: &Packet, except: Option<&str>) -> Result<SendOutcome> {
        let network = &packet.body.network_id;
        let target = packet.body.target.as_deref();
        let links = {
            let mut state = self.state.lock().await;
            let closed = state
                .links
                .iter()
                .filter(|((n, _), link)| n == network && link.send.is_closed())
                .map(|((n, peer), link)| (n.clone(), peer.clone(), link.id.clone()))
                .collect::<Vec<_>>();
            for (n, peer, id) in closed {
                state.forget_link(&n, &peer, &id);
            }
            state
                .links
                .iter()
                .filter(|((n, peer), link)| {
                    n == network && except != Some(peer.as_str()) && !link.send.is_closed()
                })
                .map(|((_, peer), link)| (peer.clone(), link.id.clone(), link.send.clone()))
                .collect::<Vec<_>>()
        };
        let mut links = {
            let store = self.store.lock().await;
            links
                .into_iter()
                .filter(|(peer, _, _)| {
                    store
                        .authorize_node(
                            network,
                            peer,
                            (target.is_some() && target != Some(peer.as_str()))
                                .then_some(NodeRole::Relay),
                        )
                        .is_ok()
                })
                .collect::<Vec<_>>()
        };
        let mut state = self.state.lock().await;
        links.retain(|(peer, id, send)| {
            !send.is_closed()
                && state
                    .links
                    .get(&(network.clone(), peer.clone()))
                    .is_some_and(|link| link.id == *id)
        });
        let routes = state
            .routes
            .iter()
            .filter(|((n, t, neighbor), route)| {
                n == network
                    && Some(t.as_str()) == target
                    && route.expires_at > Instant::now()
                    && state
                        .links
                        .get(&(n.clone(), neighbor.clone()))
                        .is_some_and(|link| link.id == route.link_id && !link.send.is_closed())
            })
            .map(|((_, _, neighbor), _)| neighbor.clone())
            .collect::<BTreeSet<_>>();
        links.sort_by_key(|(peer, _, _)| {
            (
                if target == Some(peer.as_str()) {
                    0
                } else if routes.contains(peer) {
                    1
                } else {
                    2
                },
                peer.clone(),
            )
        });
        if links.is_empty() {
            return Ok(SendOutcome::NoRoute.observed(packet));
        }
        let text = serde_json::to_string(packet)?;
        ensure!(text.len() <= MAX_PACKET_BYTES, "peer packet too large");
        let Ok(permit) = self
            .queued_bytes
            .clone()
            .try_acquire_many_owned(text.len() as u32)
        else {
            return Ok(SendOutcome::Busy.observed(packet));
        };
        let queued = Arc::new(Queued {
            _permit: permit,
            text,
        });
        let mut busy = false;
        let mut sent = 0;
        let mut explored = 0;
        for (peer, id, send) in links {
            if !state
                .links
                .get(&(network.clone(), peer.clone()))
                .is_some_and(|link| link.id == id)
            {
                continue;
            }
            let exploring =
                target.is_some() && target != Some(peer.as_str()) && !routes.contains(&peer);
            if exploring {
                if explored >= MAX_EXPLORATION_NEIGHBORS {
                    continue;
                }
                explored += 1;
            }
            match send.try_send(queued.clone()) {
                Ok(()) => {
                    sent += 1;
                    let route_kind = if target.is_none() {
                        "broadcast"
                    } else if target == Some(peer.as_str()) {
                        "direct"
                    } else if exploring {
                        "exploration"
                    } else {
                        "presence"
                    };
                    tracing::debug!(network, target, request_id=%packet.body.request_id,
                        kind=?packet.body.kind, next_hop=%peer, link_id=%id, route_kind,
                        "peer packet queued");
                    if target.is_some() && !exploring {
                        break;
                    }
                }
                Err(mpsc::error::TrySendError::Full(_)) => busy = true,
                Err(mpsc::error::TrySendError::Closed(_)) => state.forget_link(network, &peer, &id),
            }
        }
        Ok((if sent > 0 {
            SendOutcome::Queued(sent)
        } else if busy {
            SendOutcome::Busy
        } else {
            SendOutcome::NoRoute
        })
        .observed(packet))
    }
}

fn now_ms() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}
fn check_hex_id(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid peer identifier"
    );
    Ok(())
}
fn validate_body(body: &Body, now: i64) -> Result<()> {
    ensure!(
        body.version == WIRE_VERSION,
        "unsupported peer packet version"
    );
    check_hex_id(&body.network_id)?;
    check_hex_id(&body.origin)?;
    if let Some(target) = &body.target {
        check_hex_id(target)?;
    }
    ensure!(
        Uuid::parse_str(&body.request_id)?.to_string() == body.request_id,
        "invalid peer request ID"
    );
    ensure!(
        body.created_at_ms >= now.saturating_sub(MAX_AGE_MS)
            && body.created_at_ms <= now.saturating_add(MAX_FUTURE_MS),
        "expired or future peer packet"
    );
    ensure!(
        matches!(body.kind, Kind::Request | Kind::Response) == body.target.is_some(),
        "invalid peer packet target"
    );
    ensure!(
        body.kind != Kind::Presence || body.payload.is_null(),
        "invalid peer presence payload"
    );
    Ok(())
}

fn secure<T>(result: std::result::Result<T, String>) -> Result<T> {
    result.map_err(anyhow::Error::msg)
}
async fn send_binary(socket: &mut NetworkSocket, bytes: Vec<u8>) -> Result<()> {
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        socket.send(AxumMessage::Binary(bytes.into())),
    )
    .await
    .context("peer send timed out")?
}
async fn receive_binary(socket: &mut NetworkSocket, limit: usize) -> Result<Vec<u8>> {
    while let Some(frame) = socket.next().await {
        match frame? {
            AxumMessage::Binary(bytes) => {
                ensure!(bytes.len() <= limit, "peer frame too large");
                return Ok(bytes.to_vec());
            }
            AxumMessage::Ping(_) | AxumMessage::Pong(_) => {}
            _ => bail!("peer accepts only encrypted binary frames"),
        }
    }
    bail!("peer disconnected")
}
async fn send_secure(
    socket: &mut NetworkSocket,
    channel: &mut Channel,
    value: &[u8],
) -> Result<()> {
    for record in secure(channel.seal_message(value))? {
        send_binary(socket, record).await?;
    }
    Ok(())
}
async fn receive_secure(
    socket: &mut NetworkSocket,
    channel: &mut Channel,
    limit: usize,
) -> Result<Vec<u8>> {
    loop {
        if let Some(value) =
            secure(channel.open_record(&receive_binary(socket, MAX_PACKET_BYTES).await?))?
        {
            ensure!(value.len() <= limit, "peer message too large");
            return Ok(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        extract::{State as AxumState, WebSocketUpgrade},
        routing::get,
    };
    use serde_json::json;
    use tokio::{net::TcpListener, task::JoinHandle};

    #[test]
    fn peer_id_hello_uses_only_the_current_field() -> Result<()> {
        let wire = r#"{"version":1,"network_id":"network","peer_id":"device"}"#;
        let hello: Hello = serde_json::from_str(wire)?;
        assert_eq!(hello.peer_id, "device");
        assert_eq!(serde_json::to_string(&hello)?, wire);
        assert!(serde_json::from_str::<Hello>(&wire.replace("peer_id", "node_id")).is_err());
        Ok(())
    }

    fn roles() -> Vec<NodeRole> {
        vec![
            NodeRole::Gateway,
            NodeRole::Relay,
            NodeRole::Replica,
            NodeRole::Scheduler,
            NodeRole::Executor,
        ]
    }
    fn hub(
        owner: &LocalIdentity,
        identity: &LocalIdentity,
        nodes: &[&LocalIdentity],
    ) -> Result<(Arc<PeerHub>, mpsc::Receiver<Inbound>)> {
        let network = owner.fixture_network_id()?;
        let mut store = Store::open(":memory:")?;
        let certificates = nodes
            .iter()
            .map(|node| node.certificate(roles()))
            .collect::<Result<Vec<_>>>()?;
        store.install_trust_bundle(&network, &owner.sign_network_fixture(1, certificates)?)?;
        Ok(PeerHub::new(
            identity.clone(),
            Arc::new(Mutex::new(store)),
            &roles(),
        ))
    }

    async fn test_link(
        hub: &PeerHub,
        network: &str,
        peer: &str,
        id: &str,
    ) -> mpsc::Receiver<Arc<Queued>> {
        let (send, receive) = mpsc::channel(QUEUE_CAPACITY);
        hub.state.lock().await.links.insert(
            (network.to_owned(), peer.to_owned()),
            Link {
                id: id.to_owned(),
                outbound: true,
                send,
            },
        );
        receive
    }

    #[tokio::test]
    async fn m4_presence_keeps_duplicate_neighbor_sources_and_drops_only_the_lost_route()
    -> Result<()> {
        let owner = LocalIdentity::generate();
        let target = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let c = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let nodes = [&owner, &target, &b, &c];
        let (receiver, _) = hub(&owner, &owner, &nodes)?;
        let (sender, _) = hub(&owner, &target, &nodes)?;
        let _b = test_link(&receiver, &network, &b.peer_id()?, "b").await;
        let _c = test_link(&receiver, &network, &c.peer_id()?, "c").await;
        let packet = sender.packet(&network, None, Kind::Presence, Value::Null, None)?;
        receiver
            .receive_packet(&network, &b.peer_id()?, "b", packet.clone())
            .await?;
        let first_deadline = receiver.state.lock().await.routes
            [&(network.clone(), target.peer_id()?, b.peer_id()?)]
            .expires_at;
        receiver
            .receive_packet(&network, &c.peer_id()?, "c", packet.clone())
            .await?;
        receiver
            .receive_packet(&network, &b.peer_id()?, "b", packet)
            .await?;
        {
            let mut state = receiver.state.lock().await;
            assert_eq!(state.routes.len(), 2);
            assert_eq!(
                state.routes[&(network.clone(), target.peer_id()?, b.peer_id()?)].expires_at,
                first_deadline
            );
            state.forget_link(&network, &b.peer_id()?, "b");
        }
        assert!(receiver.online(&network).await.contains(&target.peer_id()?));
        receiver
            .state
            .lock()
            .await
            .forget_link(&network, &c.peer_id()?, "c");
        let _unrelated = test_link(&receiver, &network, &b.peer_id()?, "unrelated").await;
        assert!(!receiver.online(&network).await.contains(&target.peer_id()?));
        assert!(receiver.online(&network).await.contains(&b.peer_id()?));
        Ok(())
    }

    #[tokio::test]
    async fn m4_presence_signed_deadline_survives_replay_clock_rollback_and_eviction() -> Result<()>
    {
        let owner = LocalIdentity::generate();
        let target = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let c = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let (receiver, _) = hub(&owner, &owner, &[&owner, &target, &b, &c])?;
        let (sender, _) = hub(&owner, &target, &[&owner, &target, &b, &c])?;
        let _b = test_link(&receiver, &network, &b.peer_id()?, "b").await;
        let _c = test_link(&receiver, &network, &c.peer_id()?, "c").await;
        let mut packet = sender.packet(&network, None, Kind::Presence, Value::Null, None)?;
        packet.body.created_at_ms = 1_000_000;
        let now = Instant::now();
        let mut state = receiver.state.lock().await;
        assert!(state.remember(&packet, 1_080_000, now)?);
        state.observe_presence(&b.peer_id()?, "b", &packet.body, 1_080_000, now);
        let key = (network.clone(), target.peer_id()?, b.peer_id()?);
        assert_eq!(state.routes[&key].expires_at, now + Duration::from_secs(10));
        assert!(!state.remember(&packet, 900_000, now + Duration::from_secs(5))?);
        state.observe_presence(
            &b.peer_id()?,
            "b",
            &packet.body,
            900_000,
            now + Duration::from_secs(5),
        );
        state.observe_presence(
            &c.peer_id()?,
            "c",
            &packet.body,
            900_000,
            now + Duration::from_secs(5),
        );
        assert!(
            state
                .routes
                .values()
                .all(|route| route.expires_at == now + Duration::from_secs(10))
        );
        state.routes.clear();
        state.observe_presence(
            &b.peer_id()?,
            "b",
            &packet.body,
            900_000,
            now + Duration::from_secs(11),
        );
        assert!(state.routes.is_empty());
        let mut future = packet.clone();
        future.body.request_id = Uuid::new_v4().to_string();
        future.body.created_at_ms = 2_030_000;
        assert!(state.remember(&future, 2_000_000, now)?);
        state.observe_presence(&b.peer_id()?, "b", &future.body, 2_000_000, now);
        assert_eq!(state.routes[&key].expires_at, now + Duration::from_secs(90));
        let mut conflicting = future.clone();
        conflicting.body.created_at_ms += 1;
        assert!(state.remember(&conflicting, 2_000_000, now).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn m4_presence_old_link_tail_and_cleanup_cannot_change_new_generation() -> Result<()> {
        let owner = LocalIdentity::generate();
        let target = LocalIdentity::generate();
        let neighbor = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let (receiver, _) = hub(&owner, &owner, &[&owner, &target, &neighbor])?;
        let (sender, _) = hub(&owner, &target, &[&owner, &target, &neighbor])?;
        let _old = test_link(&receiver, &network, &neighbor.peer_id()?, "old").await;
        let packet = sender.packet(&network, None, Kind::Presence, Value::Null, None)?;
        receiver
            .receive_packet(&network, &neighbor.peer_id()?, "old", packet.clone())
            .await?;
        let _new = test_link(&receiver, &network, &neighbor.peer_id()?, "new").await;
        receiver
            .receive_packet(&network, &neighbor.peer_id()?, "new", packet.clone())
            .await?;
        assert!(
            receiver
                .receive_packet(&network, &neighbor.peer_id()?, "old", packet)
                .await
                .unwrap_err()
                .to_string()
                .contains("stale peer link")
        );
        receiver
            .state
            .lock()
            .await
            .forget_link(&network, &neighbor.peer_id()?, "old");
        let key = (network.clone(), target.peer_id()?, neighbor.peer_id()?);
        assert_eq!(receiver.state.lock().await.routes[&key].link_id, "new");
        assert!(receiver.online(&network).await.contains(&target.peer_id()?));
        let mut restricted = neighbor.certificate(roles())?;
        restricted.roles.retain(|role| role != &NodeRole::Relay);
        receiver.store.lock().await.install_trust_bundle(
            &network,
            &owner.sign_network_fixture(
                2,
                vec![
                    owner.certificate(roles())?,
                    target.certificate(roles())?,
                    restricted,
                ],
            )?,
        )?;
        assert!(!receiver.online(&network).await.contains(&target.peer_id()?));
        assert!(
            receiver
                .online(&network)
                .await
                .contains(&neighbor.peer_id()?)
        );
        Ok(())
    }

    #[tokio::test]
    async fn m4_directed_packets_choose_direct_then_presence_then_two_explorers() -> Result<()> {
        let owner = LocalIdentity::generate();
        let target = LocalIdentity::generate();
        let neighbors = (0..4)
            .map(|_| LocalIdentity::generate())
            .collect::<Vec<_>>();
        let mut nodes = vec![&owner, &target];
        nodes.extend(neighbors.iter());
        let network = owner.fixture_network_id()?;
        let (sender, _) = hub(&owner, &owner, &nodes)?;
        let (remote, _) = hub(&owner, &target, &nodes)?;
        let mut receivers = Vec::new();
        for node in &neighbors {
            receivers.push(test_link(&sender, &network, &node.peer_id()?, "neighbor").await);
        }
        let mut direct = test_link(&sender, &network, &target.peer_id()?, "direct").await;
        let presence = remote.packet(&network, None, Kind::Presence, Value::Null, None)?;
        sender
            .receive_packet(&network, &neighbors[0].peer_id()?, "neighbor", presence)
            .await?;
        while direct.try_recv().is_ok() {}
        for receive in &mut receivers {
            while receive.try_recv().is_ok() {}
        }
        let packet = sender.packet(
            &network,
            Some(target.peer_id()?),
            Kind::Request,
            json!({"write":"once"}),
            None,
        )?;
        assert_eq!(sender.forward(&packet, None).await?, SendOutcome::Queued(1));
        let queued: Packet = serde_json::from_str(&direct.try_recv()?.text)?;
        assert_eq!(
            serde_json::to_value(&queued.body)?,
            serde_json::to_value(&packet.body)?
        );
        assert!(
            receivers
                .iter_mut()
                .all(|receive| receive.try_recv().is_err())
        );
        sender
            .state
            .lock()
            .await
            .forget_link(&network, &target.peer_id()?, "direct");
        assert_eq!(sender.forward(&packet, None).await?, SendOutcome::Queued(1));
        assert!(receivers[0].try_recv().is_ok());
        assert!(
            receivers[1..]
                .iter_mut()
                .all(|receive| receive.try_recv().is_err())
        );
        sender.state.lock().await.routes.clear();
        assert_eq!(
            sender
                .forward(&packet, Some(&neighbors[0].peer_id()?))
                .await?,
            SendOutcome::Queued(2)
        );
        assert!(receivers[0].try_recv().is_err());
        assert_eq!(
            receivers[1..]
                .iter_mut()
                .map(|receive| usize::from(receive.try_recv().is_ok()))
                .sum::<usize>(),
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn m4_request_distinguishes_no_route_busy_and_queued_unknown_without_resending()
    -> Result<()> {
        let owner = LocalIdentity::generate();
        let target = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let (sender, _) = hub(&owner, &owner, &[&owner, &target])?;
        let budget = sender
            .queued_bytes
            .clone()
            .acquire_many_owned(32 * 1024 * 1024)
            .await?;
        let no_route = sender
            .request(&network, &target.peer_id()?, json!({"id":"once"}))
            .await
            .unwrap_err();
        assert_eq!(
            no_route.downcast_ref::<RequestFailure>(),
            Some(&RequestFailure::NoRoute)
        );
        let mut receive = test_link(&sender, &network, &target.peer_id()?, "direct").await;
        let busy = sender
            .request(&network, &target.peer_id()?, json!({"id":"once"}))
            .await
            .unwrap_err();
        assert_eq!(
            busy.downcast_ref::<RequestFailure>(),
            Some(&RequestFailure::Busy)
        );
        assert!(receive.try_recv().is_err());
        assert!(sender.state.lock().await.pending.is_empty());
        drop(budget);
        let peer = sender.clone();
        let scope = network.clone();
        let destination = target.peer_id()?;
        let request = tokio::spawn(async move {
            peer.request(&scope, &destination, json!({"id":"once"}))
                .await
        });
        let queued = tokio::time::timeout(Duration::from_secs(2), receive.recv())
            .await?
            .context("request not queued")?;
        let packet: Packet = serde_json::from_str(&queued.text)?;
        assert_eq!(packet.body.payload, json!({"id":"once"}));
        sender.state.lock().await.pending.clear();
        let unknown = request.await?.unwrap_err();
        assert_eq!(
            unknown.downcast_ref::<RequestFailure>(),
            Some(&RequestFailure::Unknown)
        );
        assert!(receive.try_recv().is_err());
        assert!(sender.state.lock().await.pending.is_empty());
        drop(queued);
        let packet = sender.packet(
            &network,
            Some(target.peer_id()?),
            Kind::Request,
            Value::Null,
            None,
        )?;
        for _ in 0..QUEUE_CAPACITY {
            assert_eq!(sender.forward(&packet, None).await?, SendOutcome::Queued(1));
        }
        assert_eq!(sender.forward(&packet, None).await?, SendOutcome::Busy);
        drop(receive);
        let _budget = sender
            .queued_bytes
            .clone()
            .acquire_many_owned(32 * 1024 * 1024)
            .await?;
        assert_eq!(sender.forward(&packet, None).await?, SendOutcome::NoRoute);
        assert!(
            !sender
                .state
                .lock()
                .await
                .links
                .contains_key(&(network, target.peer_id()?))
        );
        Ok(())
    }

    #[tokio::test]
    async fn m4_presence_expiring_while_authorization_waits_cannot_suppress_exploration()
    -> Result<()> {
        let owner = LocalIdentity::generate();
        let target = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let c = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let nodes = [&owner, &target, &b, &c];
        let (sender, _) = hub(&owner, &owner, &nodes)?;
        let (remote, _) = hub(&owner, &target, &nodes)?;
        let mut via_b = test_link(&sender, &network, &b.peer_id()?, "b").await;
        let mut via_c = test_link(&sender, &network, &c.peer_id()?, "c").await;
        let presence = remote.packet(&network, None, Kind::Presence, Value::Null, None)?;
        sender
            .receive_packet(&network, &b.peer_id()?, "b", presence)
            .await?;
        while via_c.try_recv().is_ok() {}
        let packet = sender.packet(
            &network,
            Some(target.peer_id()?),
            Kind::Request,
            json!("once"),
            None,
        )?;
        let authorization = sender.store.lock().await;
        let mut forwarding = Box::pin(sender.forward(&packet, None));
        assert!(futures_util::poll!(forwarding.as_mut()).is_pending());
        sender
            .state
            .lock()
            .await
            .routes
            .get_mut(&(network.clone(), target.peer_id()?, b.peer_id()?))
            .unwrap()
            .expires_at = Instant::now();
        drop(authorization);
        assert_eq!(forwarding.await?, SendOutcome::Queued(2));
        assert!(via_b.try_recv().is_ok());
        assert!(via_c.try_recv().is_ok());
        let route_key = (network.clone(), target.peer_id()?, b.peer_id()?);
        sender
            .state
            .lock()
            .await
            .routes
            .get_mut(&route_key)
            .unwrap()
            .expires_at = Instant::now() + Duration::from_secs(90);
        let authorization = sender.store.lock().await;
        let mut online = Box::pin(sender.online(&network));
        assert!(futures_util::poll!(online.as_mut()).is_pending());
        sender
            .state
            .lock()
            .await
            .routes
            .get_mut(&route_key)
            .unwrap()
            .expires_at = Instant::now();
        drop(authorization);
        let online = online.await;
        assert!(!online.contains(&target.peer_id()?));
        assert!(online.contains(&b.peer_id()?));
        Ok(())
    }

    #[tokio::test]
    async fn m4_local_backpressure_keeps_trusted_link_and_replay_capacity_is_bounded() -> Result<()>
    {
        let owner = LocalIdentity::generate();
        let sender_id = LocalIdentity::generate();
        let target = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let nodes = [&owner, &sender_id, &target];
        let (receiver, mut inbound) = hub(&owner, &owner, &nodes)?;
        let (sender, _) = hub(&owner, &sender_id, &nodes)?;
        let _input = test_link(&receiver, &network, &sender_id.peer_id()?, "input").await;
        let _target = test_link(&receiver, &network, &target.peer_id()?, "target").await;
        let _budget = receiver
            .queued_bytes
            .clone()
            .acquire_many_owned(32 * 1024 * 1024)
            .await?;
        let packet = sender.packet(
            &network,
            Some(target.peer_id()?),
            Kind::Request,
            json!("forward busy"),
            None,
        )?;
        receiver
            .receive_packet(&network, &sender_id.peer_id()?, "input", packet)
            .await?;
        for _ in 0..256 {
            let packet = sender.packet(
                &network,
                Some(owner.peer_id()?),
                Kind::Request,
                json!("fill"),
                None,
            )?;
            receiver
                .receive_packet(&network, &sender_id.peer_id()?, "input", packet)
                .await?;
        }
        let packet = sender.packet(
            &network,
            Some(owner.peer_id()?),
            Kind::Request,
            json!("inbound busy"),
            None,
        )?;
        receiver
            .receive_packet(&network, &sender_id.peer_id()?, "input", packet)
            .await?;
        assert_eq!(inbound.len(), 256);
        receiver.reserve_replay_entries(&network, 0).await?;
        let packet = sender.packet(
            &network,
            Some(owner.peer_id()?),
            Kind::Request,
            json!("replay busy"),
            None,
        )?;
        receiver
            .receive_packet(&network, &sender_id.peer_id()?, "input", packet)
            .await?;
        assert_eq!(receiver.state.lock().await.seen.len(), MAX_SEEN);
        assert!(
            receiver
                .state
                .lock()
                .await
                .links
                .contains_key(&(network.clone(), sender_id.peer_id()?))
        );
        assert_eq!(inbound.try_recv()?.payload, json!("fill"));
        Ok(())
    }

    #[tokio::test]
    async fn forwarding_requires_both_enabled_and_certified_relay_role() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let c = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let (sender, _) = hub(&a, &a, &[&a, &b, &c])?;
        let (enabled, _) = hub(&a, &b, &[&a, &b, &c])?;
        let (disabled, mut inbound) =
            PeerHub::new(b.clone(), enabled.store.clone(), &[NodeRole::Replica]);
        let (send, mut receive) = mpsc::channel(4);
        let mut input_receivers = Vec::new();
        for hub in [&enabled, &disabled] {
            input_receivers.push(test_link(hub, &network, &a.peer_id()?, "input").await);
            hub.state.lock().await.links.insert(
                (network.clone(), c.peer_id()?),
                Link {
                    id: "test".into(),
                    outbound: true,
                    send: send.clone(),
                },
            );
        }
        let packet = sender.packet(
            &network,
            Some(c.peer_id()?),
            Kind::Request,
            json!("relay"),
            None,
        )?;
        disabled
            .receive_packet(&network, &a.peer_id()?, "input", packet.clone())
            .await?;
        assert!(receive.try_recv().is_err());
        enabled
            .receive_packet(&network, &a.peer_id()?, "input", packet)
            .await?;
        let forwarded: Packet = serde_json::from_str(&receive.try_recv()?.text)?;
        assert_eq!(forwarded.hops_remaining, MAX_HOPS - 1);
        let packet = sender.packet(
            &network,
            Some(b.peer_id()?),
            Kind::Request,
            json!("local"),
            None,
        )?;
        disabled
            .receive_packet(&network, &a.peer_id()?, "input", packet)
            .await?;
        assert_eq!(inbound.try_recv()?.payload, json!("local"));
        let mut restricted = b.certificate(roles())?;
        restricted.roles.retain(|r| r != &NodeRole::Relay);
        enabled.store.lock().await.install_trust_bundle(
            &network,
            &a.sign_network_fixture(
                2,
                vec![a.certificate(roles())?, restricted, c.certificate(roles())?],
            )?,
        )?;
        let packet = sender.packet(
            &network,
            Some(c.peer_id()?),
            Kind::Request,
            json!("no certificate"),
            None,
        )?;
        enabled
            .receive_packet(&network, &a.peer_id()?, "input", packet)
            .await?;
        assert!(receive.try_recv().is_err());
        Ok(())
    }

    #[tokio::test]
    async fn signed_packets_are_network_bound_deduped_and_revocation_checked() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let (sender, _) = hub(&a, &a, &[&a, &b])?;
        let (receiver, mut inbound) = hub(&a, &b, &[&a, &b])?;
        let _input = test_link(&receiver, &network, &a.peer_id()?, "input").await;
        let _wrong_network_input =
            test_link(&receiver, &b.fixture_network_id()?, &a.peer_id()?, "input").await;
        let packet = sender.packet(
            &network,
            Some(b.peer_id()?),
            Kind::Request,
            json!({"operation_id":"once"}),
            None,
        )?;
        let mut tampered = packet.clone();
        tampered.body.payload = json!({"operation_id":"changed"});
        assert!(
            receiver
                .receive_packet(&network, &a.peer_id()?, "input", tampered)
                .await
                .is_err()
        );
        assert!(inbound.try_recv().is_err());
        assert!(
            receiver
                .receive_packet(
                    &b.fixture_network_id()?,
                    &a.peer_id()?,
                    "input",
                    packet.clone()
                )
                .await
                .is_err()
        );
        receiver
            .receive_packet(&network, &a.peer_id()?, "input", packet.clone())
            .await?;
        receiver
            .receive_packet(&network, &a.peer_id()?, "input", packet)
            .await?;
        let received = inbound.try_recv()?;
        assert_eq!(received.origin, a.peer_id()?);
        assert_eq!(received.payload, json!({"operation_id":"once"}));
        assert!(inbound.try_recv().is_err());
        let mut revoked = a.certificate(roles())?;
        revoked.revoked = true;
        receiver.store.lock().await.install_trust_bundle(
            &network,
            &a.sign_network_fixture(2, vec![revoked, b.certificate(roles())?])?,
        )?;
        let packet = sender.packet(
            &network,
            Some(b.peer_id()?),
            Kind::Request,
            Value::Null,
            None,
        )?;
        receiver
            .receive_packet(&network, &a.peer_id()?, "input", packet)
            .await?;
        assert!(inbound.try_recv().is_err());
        Ok(())
    }

    #[tokio::test]
    async fn unknown_member_packet_is_dropped_without_closing_trusted_link() -> Result<()> {
        let owner = LocalIdentity::generate();
        let known = LocalIdentity::generate();
        let unknown = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let (receiver, mut inbound) = hub(&owner, &known, &[&owner, &known])?;
        let (sender, _) = hub(&owner, &unknown, &[&owner, &known, &unknown])?;
        let (send, _) = mpsc::channel(1);
        receiver.state.lock().await.links.insert(
            (network.clone(), owner.peer_id()?),
            Link {
                id: "trusted".into(),
                outbound: true,
                send,
            },
        );
        let packet = sender.packet(
            &network,
            Some(known.peer_id()?),
            Kind::Request,
            json!({"unknown":true}),
            None,
        )?;
        receiver
            .receive_packet(&network, &owner.peer_id()?, "trusted", packet)
            .await?;
        assert!(inbound.try_recv().is_err());
        assert!(
            receiver
                .state
                .lock()
                .await
                .links
                .contains_key(&(network, owner.peer_id()?))
        );
        Ok(())
    }

    #[tokio::test]
    async fn replies_must_match_expected_origin_and_network() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let c = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let (requester, _) = hub(&a, &a, &[&a, &b, &c])?;
        let (expected, _) = hub(&a, &b, &[&a, &b, &c])?;
        let (other, _) = hub(&a, &c, &[&a, &b, &c])?;
        let _other_input = test_link(&requester, &network, &c.peer_id()?, "other").await;
        let _expected_input = test_link(&requester, &network, &b.peer_id()?, "expected").await;
        let request_id = Uuid::new_v4().to_string();
        let (reply, mut receive) = oneshot::channel();
        requester.state.lock().await.pending.insert(
            (network.clone(), request_id.clone()),
            Pending {
                origin: b.peer_id()?,
                created: Instant::now(),
                reply,
            },
        );
        let wrong = other.packet(
            &network,
            Some(a.peer_id()?),
            Kind::Response,
            json!("wrong"),
            Some(request_id.clone()),
        )?;
        requester
            .receive_packet(&network, &c.peer_id()?, "other", wrong)
            .await?;
        assert!(matches!(
            receive.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let correct = expected.packet(
            &network,
            Some(a.peer_id()?),
            Kind::Response,
            json!("correct"),
            Some(request_id),
        )?;
        requester
            .receive_packet(&network, &b.peer_id()?, "expected", correct)
            .await?;
        assert_eq!(receive.await?, json!("correct"));
        Ok(())
    }

    #[test]
    fn packet_shape_rejects_stale_unbounded_and_unsigned_changes() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let (hub, _) = hub(&a, &a, &[&a, &b])?;
        let packet = hub.packet(
            &a.fixture_network_id()?,
            Some(b.peer_id()?),
            Kind::Request,
            json!("ok"),
            None,
        )?;
        let now = now_ms()?;
        let mut body = packet.body.clone();
        body.created_at_ms = now - MAX_AGE_MS - 1;
        assert!(validate_body(&body, now).is_err());
        body.created_at_ms = now + MAX_FUTURE_MS + 1;
        assert!(validate_body(&body, now).is_err());
        body = packet.body.clone();
        body.target = None;
        assert!(validate_body(&body, now).is_err());
        body = packet.body.clone();
        body.target = Some(a.peer_id()?);
        assert!(
            verify_payload(
                &a.public_key_hex()?,
                "peer-packet",
                &serde_json::to_vec(&body)?,
                &packet.signature
            )
            .is_err()
        );
        assert!(
            hub.packet(
                &a.fixture_network_id()?,
                Some(b.peer_id()?),
                Kind::Request,
                json!("x".repeat(MAX_PACKET_BYTES)),
                None
            )
            .is_err()
        );
        let payload = json!("x".repeat(MAX_PACKET_BYTES - 1024 - 2));
        assert_eq!(serde_json::to_vec(&payload)?.len(), MAX_PACKET_BYTES - 1024);
        let response = hub.packet(
            &a.fixture_network_id()?,
            Some(b.peer_id()?),
            Kind::Response,
            payload,
            None,
        )?;
        assert!(serde_json::to_vec(&response)?.len() <= MAX_PACKET_BYTES);
        Ok(())
    }

    struct Tasks(Vec<JoinHandle<()>>);
    impl Drop for Tasks {
        fn drop(&mut self) {
            for task in &self.0 {
                task.abort();
            }
        }
    }
    async fn serve(hub: Arc<PeerHub>, tasks: &mut Tasks) -> Result<String> {
        async fn upgrade(
            AxumState(hub): AxumState<Arc<PeerHub>>,
            ws: WebSocketUpgrade,
        ) -> impl axum::response::IntoResponse {
            ws.max_message_size(MAX_PACKET_BYTES)
                .max_frame_size(MAX_PACKET_BYTES)
                .on_upgrade(|socket| async move {
                    let _ = hub.accept(socket).await;
                })
        }
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}/peer", listener.local_addr()?);
        let router = Router::new().route("/peer", get(upgrade)).with_state(hub);
        tasks.0.push(tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        }));
        Ok(url)
    }
    async fn wait_link(hub: &PeerHub, network: &str, target: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if hub
                    .state
                    .lock()
                    .await
                    .links
                    .contains_key(&(network.to_owned(), target.to_owned()))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("test peer did not connect")
    }

    #[tokio::test]
    async fn websocket_handshake_rejects_an_untrusted_node() -> Result<()> {
        let owner = LocalIdentity::generate();
        let outsider = LocalIdentity::generate();
        let (server, _) = hub(&owner, &owner, &[&owner])?;
        let (client, _) = hub(&owner, &outsider, &[&owner, &outsider])?;
        let mut tasks = Tasks(Vec::new());
        let url = serve(server.clone(), &mut tasks).await?;
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            client.connect(owner.fixture_network_id()?, url),
        )
        .await?;
        assert!(result.is_err());
        assert!(server.online(&owner.fixture_network_id()?).await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn lost_last_link_stops_reporting_cached_presence_and_allows_reconnect() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let target = b.peer_id()?;
        let (server, _) = hub(&a, &a, &[&a, &b])?;
        let (client, _) = hub(&a, &b, &[&a, &b])?;
        let mut tasks = Tasks(Vec::new());
        let url = serve(server.clone(), &mut tasks).await?;
        let peer = client.clone();
        let scope = network.clone();
        let endpoint = url.clone();
        let connection = tokio::spawn(async move {
            let _ = peer.connect(scope, endpoint).await;
        });
        wait_link(&server, &network, &target).await?;
        tokio::time::timeout(Duration::from_secs(3), async {
            while !server.state.lock().await.routes.contains_key(&(
                network.clone(),
                target.clone(),
                target.clone(),
            )) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert!(server.online(&network).await.contains(&target));
        connection.abort();
        let _ = connection.await;
        tokio::time::timeout(Duration::from_secs(3), async {
            while !server.state.lock().await.links.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let unrelated = "f".repeat(64);
        let (send, _receive) = mpsc::channel(1);
        server.state.lock().await.links.insert(
            (unrelated.clone(), target.clone()),
            Link {
                id: "another-network".into(),
                outbound: true,
                send,
            },
        );
        assert!(server.online(&network).await.is_empty());
        server
            .state
            .lock()
            .await
            .links
            .remove(&(unrelated, target.clone()));

        let (restarted, mut inbound) = hub(&a, &b, &[&a, &b])?;
        let peer = restarted.clone();
        let scope = network.clone();
        tasks.0.push(tokio::spawn(async move {
            let _ = peer.connect(scope, url).await;
        }));
        wait_link(&server, &network, &target).await?;
        let peer = server.clone();
        let scope = network.clone();
        let request = tokio::spawn(async move {
            peer.request(&scope, &target, json!({"query":"after restart"}))
                .await
        });
        let received = tokio::time::timeout(Duration::from_secs(3), inbound.recv())
            .await?
            .context("missing request after reconnect")?;
        restarted
            .respond(&received, json!({"reconnected":true}))
            .await?;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), request).await???,
            json!({"reconnected":true})
        );
        Ok(())
    }

    #[tokio::test]
    async fn actual_reverse_websockets_route_requests_and_deduplicate_cyclic_broadcasts()
    -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let c = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let (ha, mut ra) = hub(&a, &a, &[&a, &b, &c])?;
        let (hb, mut rb) = hub(&a, &b, &[&a, &b, &c])?;
        let (hc, mut rc) = hub(&a, &c, &[&a, &b, &c])?;
        let mut tasks = Tasks(Vec::new());
        let url_a = serve(ha.clone(), &mut tasks).await?;
        let url_b = serve(hb.clone(), &mut tasks).await?;
        let h = hb.clone();
        let n = network.clone();
        let url = url_a.clone();
        tasks.0.push(tokio::spawn(async move {
            let _ = h.connect(n, url).await;
        }));
        wait_link(&ha, &network, &b.peer_id()?).await?;
        let h = hc.clone();
        let n = network.clone();
        tasks.0.push(tokio::spawn(async move {
            let _ = h.connect(n, url_b).await;
        }));
        wait_link(&hb, &network, &c.peer_id()?).await?;
        let h = ha.clone();
        let n = network.clone();
        let target = c.peer_id()?;
        let request =
            tokio::spawn(async move { h.request(&n, &target, json!({"query":"status"})).await });
        // C has no listening socket: its outbound connection carries requests back in.
        let received = tokio::time::timeout(Duration::from_secs(5), rc.recv())
            .await?
            .context("missing relayed request")?;
        assert_eq!(received.origin, a.peer_id()?);
        assert_eq!(received.payload, json!({"query":"status"}));
        hc.respond(&received, json!({"status":"running"})).await?;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), request).await???,
            json!({"status":"running"})
        );
        assert!(rb.try_recv().is_err());
        assert!(ra.try_recv().is_err());
        // Add a second route and a cycle; each destination sees the broadcast once.
        let h = hc.clone();
        let n = network.clone();
        tasks.0.push(tokio::spawn(async move {
            let _ = h.connect(n, url_a).await;
        }));
        wait_link(&ha, &network, &c.peer_id()?).await?;
        hc.broadcast(&network, json!({"event":"one"})).await?;
        let at_a = tokio::time::timeout(Duration::from_secs(5), ra.recv())
            .await?
            .context("missing A broadcast")?;
        let at_b = tokio::time::timeout(Duration::from_secs(5), rb.recv())
            .await?
            .context("missing B broadcast")?;
        assert!(at_a.broadcast && at_b.broadcast);
        assert_eq!(at_a.origin, c.peer_id()?);
        assert_eq!(at_a.request_id, at_b.request_id);
        assert_eq!(at_a.payload, json!({"event":"one"}));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), ra.recv())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), rb.recv())
                .await
                .is_err()
        );
        assert!(rc.try_recv().is_err());
        Ok(())
    }
}
