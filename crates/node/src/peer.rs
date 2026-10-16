//! Authenticated, network-scoped peer transport; business requests remain opaque.

use crate::{
    crypto::{LocalIdentity, digest, verify_payload},
    model::{NetworkId, NodeId, NodeRole, WIRE_VERSION},
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use axum::extract::ws::{Message as AxumMessage, WebSocket};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    error::Error,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Message as ClientMessage, protocol::WebSocketConfig},
};
use uuid::Uuid;

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

#[derive(Debug, Clone)]
pub struct Inbound {
    pub network_id: NetworkId,
    pub origin: NodeId,
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
    links: HashMap<(NetworkId, NodeId), Link>,
    pending: HashMap<(NetworkId, String), Pending>,
    seen: HashMap<PacketId, (String, Instant)>,
    reachable: HashMap<(NetworkId, NodeId), Instant>,
    last_presence: HashMap<NetworkId, Instant>,
}
struct Link {
    id: String,
    outbound: bool,
    send: mpsc::Sender<Arc<Queued>>,
}
struct Queued {
    text: String,
    _permit: OwnedSemaphorePermit,
}
struct Pending {
    origin: NodeId,
    created: Instant,
    reply: oneshot::Sender<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    version: u32,
    network_id: NetworkId,
    node_id: NodeId,
    nonce: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Challenge {
    hello: Hello,
    signature: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    signature: String,
}
#[derive(Serialize)]
struct Transcript<'a> {
    initiator: &'a Hello,
    responder: &'a Hello,
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
    origin: NodeId,
    target: Option<NodeId>,
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
type PacketId = (NetworkId, NodeId, String, Kind);

impl PeerHub {
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
    pub async fn accept(self: Arc<Self>, mut socket: WebSocket) -> Result<()> {
        let _permit = self
            .sockets
            .clone()
            .try_acquire_owned()
            .context("too many peer sockets")?;
        let (network, peer) =
            tokio::time::timeout(HANDSHAKE_TIMEOUT, self.server_handshake(&mut socket))
                .await
                .context("peer handshake timed out")??;
        self.connected(socket, network, peer, false).await
    }

    /// Runs until disconnected. Reconnection policy belongs to the runtime.
    pub async fn connect(self: Arc<Self>, network_id: String, url: String) -> Result<()> {
        let _permit = self
            .sockets
            .clone()
            .try_acquire_owned()
            .context("too many peer sockets")?;
        self.authorize(&network_id, &self.identity.node_id()?)
            .await?;
        ensure!(
            url.starts_with("ws://") || url.starts_with("wss://"),
            "invalid peer URL scheme"
        );
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_PACKET_BYTES))
            .max_frame_size(Some(MAX_PACKET_BYTES));
        let (mut socket, _) = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            connect_async_with_config(&url, Some(config), false),
        )
        .await
        .context("peer connection timed out")?
        .map_err(|_| anyhow::anyhow!("peer connection failed"))?;
        let peer = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            self.client_handshake(&mut socket, &network_id),
        )
        .await
        .context("peer handshake timed out")??;
        self.connected(socket, network_id, peer, true).await
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
        let (reply, receive) = oneshot::channel();
        {
            let mut state = self.state.lock().await;
            state
                .pending
                .retain(|_, p| !p.reply.is_closed() && p.created.elapsed() < REQUEST_TIMEOUT);
            ensure!(
                state.pending.len() < MAX_PENDING,
                "too many pending peer requests"
            );
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
            Ok(()) => match tokio::time::timeout(REQUEST_TIMEOUT, receive).await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(_)) => Err(anyhow::anyhow!(
                    "peer request cancelled; execution outcome may be unknown"
                )),
                Err(_) => Err(anyhow::anyhow!(
                    "peer request timed out; execution outcome may be unknown"
                )),
            },
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
    pub async fn online(&self, network: &str) -> Vec<NodeId> {
        let candidates: BTreeSet<_> = {
            let state = self.state.lock().await;
            state
                .links
                .keys()
                .filter(|(n, _)| n == network)
                .map(|(_, n)| n.clone())
                .chain(
                    state
                        .reachable
                        .iter()
                        .filter(|((n, _), time)| n == network && time.elapsed() < IDLE_TIMEOUT)
                        .map(|((_, n), _)| n.clone()),
                )
                .collect()
        };
        let store = self.store.lock().await;
        candidates
            .into_iter()
            .filter(|node| store.authorize_node(network, node, None).is_ok())
            .collect()
    }

    async fn authorize(&self, network: &str, node: &str) -> Result<String> {
        Ok(self
            .store
            .lock()
            .await
            .authorize_node(network, node, None)?
            .public_key)
    }

    fn hello(&self, network: &str) -> Result<Hello> {
        let mut nonce = [0u8; 32];
        OsRng.fill_bytes(&mut nonce);
        Ok(Hello {
            version: WIRE_VERSION,
            network_id: network.to_owned(),
            node_id: self.identity.node_id()?,
            nonce: hex::encode(nonce),
        })
    }

    async fn check_hello(&self, hello: &Hello, network: Option<&str>) -> Result<String> {
        ensure!(hello.version == WIRE_VERSION, "unsupported peer version");
        check_hex_id(&hello.network_id)?;
        check_hex_id(&hello.node_id)?;
        check_hex_id(&hello.nonce)?;
        ensure!(
            network.is_none_or(|n| n == hello.network_id),
            "peer belongs to another network"
        );
        let local = self.identity.node_id()?;
        ensure!(hello.node_id != local, "cannot connect a node to itself");
        self.authorize(&hello.network_id, &local).await?;
        self.authorize(&hello.network_id, &hello.node_id).await
    }

    async fn server_handshake<S, M, E>(&self, socket: &mut S) -> Result<(String, String)>
    where
        S: Stream<Item = std::result::Result<M, E>> + Sink<M, Error = E> + Unpin,
        M: WireMessage,
        E: Error + Send + Sync + 'static,
    {
        let initiator: Hello = receive_json(socket, MAX_HANDSHAKE_BYTES).await?;
        let key = self.check_hello(&initiator, None).await?;
        let responder = self.hello(&initiator.network_id)?;
        let transcript = serde_json::to_vec(&Transcript {
            initiator: &initiator,
            responder: &responder,
        })?;
        send_json(
            socket,
            &Challenge {
                hello: responder,
                signature: self.identity.sign_payload("peer-server", &transcript)?,
            },
        )
        .await?;
        let proof: Proof = receive_json(socket, MAX_HANDSHAKE_BYTES).await?;
        verify_payload(&key, "peer-client", &transcript, &proof.signature)?;
        self.authorize(&initiator.network_id, &initiator.node_id)
            .await?;
        send_json(
            socket,
            &Proof {
                signature: self.identity.sign_payload("peer-ready", &transcript)?,
            },
        )
        .await?;
        Ok((initiator.network_id, initiator.node_id))
    }

    async fn client_handshake<S, M, E>(&self, socket: &mut S, network: &str) -> Result<NodeId>
    where
        S: Stream<Item = std::result::Result<M, E>> + Sink<M, Error = E> + Unpin,
        M: WireMessage,
        E: Error + Send + Sync + 'static,
    {
        let initiator = self.hello(network)?;
        send_json(socket, &initiator).await?;
        let challenge: Challenge = receive_json(socket, MAX_HANDSHAKE_BYTES).await?;
        let key = self.check_hello(&challenge.hello, Some(network)).await?;
        let transcript = serde_json::to_vec(&Transcript {
            initiator: &initiator,
            responder: &challenge.hello,
        })?;
        verify_payload(&key, "peer-server", &transcript, &challenge.signature)?;
        send_json(
            socket,
            &Proof {
                signature: self.identity.sign_payload("peer-client", &transcript)?,
            },
        )
        .await?;
        let ready: Proof = receive_json(socket, MAX_HANDSHAKE_BYTES).await?;
        verify_payload(&key, "peer-ready", &transcript, &ready.signature)?;
        Ok(challenge.hello.node_id)
    }

    async fn connected<S, M, E>(
        self: Arc<Self>,
        socket: S,
        network: String,
        peer: String,
        outbound: bool,
    ) -> Result<()>
    where
        S: Stream<Item = std::result::Result<M, E>> + Sink<M, Error = E> + Unpin,
        M: WireMessage,
        E: Error + Send + Sync + 'static,
    {
        let id = Uuid::new_v4().to_string();
        let (send, receive) = mpsc::channel(QUEUE_CAPACITY);
        let link_key = (network.clone(), peer.clone());
        {
            let local = self.identity.node_id()?;
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
        let result = self.drive(socket, &network, &peer, receive).await;
        let mut state = self.state.lock().await;
        if state.links.get(&link_key).is_some_and(|link| link.id == id) {
            state.links.remove(&link_key);
        }
        result
    }

    async fn drive<S, M, E>(
        &self,
        mut socket: S,
        network: &str,
        peer: &str,
        mut outbound: mpsc::Receiver<Arc<Queued>>,
    ) -> Result<()>
    where
        S: Stream<Item = std::result::Result<M, E>> + Sink<M, Error = E> + Unpin,
        M: WireMessage,
        E: Error + Send + Sync + 'static,
    {
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut received = Instant::now();
        loop {
            tokio::select! {
                frame = socket.next() => {
                    let frame = frame.context("peer disconnected")??;
                    self.authorize(network, peer).await?;
                    self.authorize(network, &self.identity.node_id()?).await?;
                    received = Instant::now();
                    if let Some(text) = frame.into_text()? {
                        ensure!(text.len() <= MAX_PACKET_BYTES, "peer packet too large");
                        let packet: Packet = serde_json::from_str(&text).context("invalid peer packet")?;
                        self.receive_packet(network, peer, packet).await?;
                    }
                }
                queued = outbound.recv() => {
                    let text = queued.context("peer connection replaced")?;
                    self.authorize(network, peer).await?;
                    self.authorize(network, &self.identity.node_id()?).await?;
                    tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.send(M::text(text.text.clone()))).await.context("peer send timed out")??;
                }
                _ = heartbeat.tick() => {
                    ensure!(received.elapsed() < IDLE_TIMEOUT, "peer heartbeat timed out");
                    self.authorize(network, peer).await?;
                    self.authorize(network, &self.identity.node_id()?).await?;
                    tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.send(M::ping())).await.context("peer send timed out")??;
                    self.presence(network).await?;
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
            origin: self.identity.node_id()?,
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
        let sent = self.forward(&packet, None).await?;
        ensure!(
            packet.body.kind != Kind::Request || sent > 0,
            "no peer route available"
        );
        Ok(())
    }

    async fn receive_packet(&self, network: &str, peer: &str, packet: Packet) -> Result<()> {
        ensure!(
            packet.body.network_id == network,
            "cross-network peer packet"
        );
        ensure!(packet.hops_remaining <= MAX_HOPS, "invalid peer hop limit");
        validate_body(&packet.body, now_ms()?)?;
        let key = self.authorize(network, &packet.body.origin).await?;
        verify_payload(
            &key,
            "peer-packet",
            &serde_json::to_vec(&packet.body)?,
            &packet.signature,
        )?;
        if let Some(target) = &packet.body.target {
            self.authorize(network, target).await?;
        }
        if !self.remember(&packet).await? {
            return Ok(());
        }
        let local = self.identity.node_id()?;
        if packet.body.target.as_deref() == Some(local.as_str()) {
            return self.deliver(&packet).await;
        }
        if packet.body.kind == Kind::Presence {
            let mut state = self.state.lock().await;
            state.reachable.retain(|_, at| at.elapsed() < IDLE_TIMEOUT);
            state.reachable.insert(
                (network.to_owned(), packet.body.origin.clone()),
                Instant::now(),
            );
        } else if packet.body.kind == Kind::Broadcast {
            self.deliver(&packet).await?;
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
            self.forward(&forwarded, Some(peer)).await?;
        }
        Ok(())
    }

    async fn remember(&self, packet: &Packet) -> Result<bool> {
        let id = (
            packet.body.network_id.clone(),
            packet.body.origin.clone(),
            packet.body.request_id.clone(),
            packet.body.kind,
        );
        let hash = digest(&serde_json::to_vec(&packet.body)?);
        let mut state = self.state.lock().await;
        state.seen.retain(|_, (_, at)| {
            at.elapsed() < Duration::from_millis((MAX_AGE_MS + MAX_FUTURE_MS) as u64)
        });
        if let Some((previous, _)) = state.seen.get(&id) {
            ensure!(previous == &hash, "conflicting peer packet ID");
            return Ok(false);
        }
        ensure!(state.seen.len() < MAX_SEEN, "peer replay cache full");
        state.seen.insert(id, (hash, Instant::now()));
        Ok(true)
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
                let _ = pending.reply.send(packet.body.payload.clone());
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
                .context("peer inbound queue unavailable")?;
        }
        Ok(())
    }

    async fn forward(&self, packet: &Packet, except: Option<&str>) -> Result<usize> {
        let text = serde_json::to_string(packet)?;
        ensure!(text.len() <= MAX_PACKET_BYTES, "peer packet too large");
        let queued = Arc::new(Queued {
            _permit: self
                .queued_bytes
                .clone()
                .try_acquire_many_owned(text.len() as u32)
                .context("peer send byte budget full")?,
            text,
        });
        let state = self.state.lock().await;
        Ok(state
            .links
            .iter()
            .filter(|((network, peer), _)| {
                network == &packet.body.network_id && except != Some(peer.as_str())
            })
            .filter(|(_, link)| link.send.try_send(queued.clone()).is_ok())
            .count())
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

trait WireMessage {
    fn text(value: String) -> Self;
    fn ping() -> Self;
    fn into_text(self) -> Result<Option<String>>;
}
impl WireMessage for AxumMessage {
    fn text(value: String) -> Self {
        Self::Text(value.into())
    }
    fn ping() -> Self {
        Self::Ping(Vec::new().into())
    }
    fn into_text(self) -> Result<Option<String>> {
        match self {
            Self::Text(value) => Ok(Some(value.to_string())),
            Self::Ping(_) | Self::Pong(_) => Ok(None),
            Self::Close(_) => bail!("peer closed connection"),
            Self::Binary(_) => bail!("binary peer messages are not supported"),
        }
    }
}
impl WireMessage for ClientMessage {
    fn text(value: String) -> Self {
        Self::Text(value.into())
    }
    fn ping() -> Self {
        Self::Ping(Vec::new().into())
    }
    fn into_text(self) -> Result<Option<String>> {
        match self {
            Self::Text(value) => Ok(Some(value.to_string())),
            Self::Ping(_) | Self::Pong(_) => Ok(None),
            Self::Close(_) => bail!("peer closed connection"),
            _ => bail!("unsupported peer message"),
        }
    }
}
async fn send_json<S, M, E, T>(socket: &mut S, value: &T) -> Result<()>
where
    S: Sink<M, Error = E> + Unpin,
    M: WireMessage,
    E: Error + Send + Sync + 'static,
    T: Serialize,
{
    let text = serde_json::to_string(value)?;
    ensure!(
        text.len() <= MAX_HANDSHAKE_BYTES,
        "peer handshake too large"
    );
    socket.send(M::text(text)).await?;
    Ok(())
}
async fn receive_json<S, M, E, T>(socket: &mut S, limit: usize) -> Result<T>
where
    S: Stream<Item = std::result::Result<M, E>> + Unpin,
    M: WireMessage,
    E: Error + Send + Sync + 'static,
    T: DeserializeOwned,
{
    while let Some(frame) = socket.next().await {
        if let Some(text) = frame?.into_text()? {
            ensure!(text.len() <= limit, "peer handshake too large");
            return serde_json::from_str(&text).context("invalid peer handshake");
        }
    }
    bail!("peer disconnected during handshake")
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
        let network = owner.network_id()?;
        let mut store = Store::open(":memory:")?;
        let certificates = nodes
            .iter()
            .map(|node| node.certificate(roles()))
            .collect::<Result<Vec<_>>>()?;
        store.install_trust_bundle(&network, &owner.sign_trust_bundle(1, certificates)?)?;
        Ok(PeerHub::new(
            identity.clone(),
            Arc::new(Mutex::new(store)),
            &roles(),
        ))
    }

    #[tokio::test]
    async fn forwarding_requires_both_enabled_and_certified_relay_role() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let c = LocalIdentity::generate();
        let network = a.network_id()?;
        let (sender, _) = hub(&a, &a, &[&a, &b, &c])?;
        let (enabled, _) = hub(&a, &b, &[&a, &b, &c])?;
        let (disabled, mut inbound) =
            PeerHub::new(b.clone(), enabled.store.clone(), &[NodeRole::Replica]);
        let (send, mut receive) = mpsc::channel(4);
        for hub in [&enabled, &disabled] {
            hub.state.lock().await.links.insert(
                (network.clone(), c.node_id()?),
                Link {
                    id: "test".into(),
                    outbound: true,
                    send: send.clone(),
                },
            );
        }
        let packet = sender.packet(
            &network,
            Some(c.node_id()?),
            Kind::Request,
            json!("relay"),
            None,
        )?;
        disabled
            .receive_packet(&network, &a.node_id()?, packet.clone())
            .await?;
        assert!(receive.try_recv().is_err());
        enabled
            .receive_packet(&network, &a.node_id()?, packet)
            .await?;
        let forwarded: Packet = serde_json::from_str(&receive.try_recv()?.text)?;
        assert_eq!(forwarded.hops_remaining, MAX_HOPS - 1);
        let packet = sender.packet(
            &network,
            Some(b.node_id()?),
            Kind::Request,
            json!("local"),
            None,
        )?;
        disabled
            .receive_packet(&network, &a.node_id()?, packet)
            .await?;
        assert_eq!(inbound.try_recv()?.payload, json!("local"));
        let mut restricted = b.certificate(roles())?;
        restricted.roles.retain(|r| r != &NodeRole::Relay);
        enabled.store.lock().await.install_trust_bundle(
            &network,
            &a.sign_trust_bundle(
                2,
                vec![a.certificate(roles())?, restricted, c.certificate(roles())?],
            )?,
        )?;
        let packet = sender.packet(
            &network,
            Some(c.node_id()?),
            Kind::Request,
            json!("no certificate"),
            None,
        )?;
        enabled
            .receive_packet(&network, &a.node_id()?, packet)
            .await?;
        assert!(receive.try_recv().is_err());
        Ok(())
    }

    #[tokio::test]
    async fn signed_packets_are_network_bound_deduped_and_revocation_checked() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let network = a.network_id()?;
        let (sender, _) = hub(&a, &a, &[&a, &b])?;
        let (receiver, mut inbound) = hub(&a, &b, &[&a, &b])?;
        let packet = sender.packet(
            &network,
            Some(b.node_id()?),
            Kind::Request,
            json!({"operation_id":"once"}),
            None,
        )?;
        let mut tampered = packet.clone();
        tampered.body.payload = json!({"operation_id":"changed"});
        assert!(
            receiver
                .receive_packet(&network, &a.node_id()?, tampered)
                .await
                .is_err()
        );
        assert!(inbound.try_recv().is_err());
        assert!(
            receiver
                .receive_packet(&b.network_id()?, &a.node_id()?, packet.clone())
                .await
                .is_err()
        );
        receiver
            .receive_packet(&network, &a.node_id()?, packet.clone())
            .await?;
        receiver
            .receive_packet(&network, &a.node_id()?, packet)
            .await?;
        let received = inbound.try_recv()?;
        assert_eq!(received.origin, a.node_id()?);
        assert_eq!(received.payload, json!({"operation_id":"once"}));
        assert!(inbound.try_recv().is_err());
        let mut revoked = a.certificate(roles())?;
        revoked.revoked = true;
        receiver.store.lock().await.install_trust_bundle(
            &network,
            &a.sign_trust_bundle(2, vec![revoked, b.certificate(roles())?])?,
        )?;
        let packet = sender.packet(
            &network,
            Some(b.node_id()?),
            Kind::Request,
            Value::Null,
            None,
        )?;
        assert!(
            receiver
                .receive_packet(&network, &a.node_id()?, packet)
                .await
                .is_err()
        );
        assert!(inbound.try_recv().is_err());
        Ok(())
    }

    #[tokio::test]
    async fn replies_must_match_expected_origin_and_network() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let c = LocalIdentity::generate();
        let network = a.network_id()?;
        let (requester, _) = hub(&a, &a, &[&a, &b, &c])?;
        let (expected, _) = hub(&a, &b, &[&a, &b, &c])?;
        let (other, _) = hub(&a, &c, &[&a, &b, &c])?;
        let request_id = Uuid::new_v4().to_string();
        let (reply, mut receive) = oneshot::channel();
        requester.state.lock().await.pending.insert(
            (network.clone(), request_id.clone()),
            Pending {
                origin: b.node_id()?,
                created: Instant::now(),
                reply,
            },
        );
        let wrong = other.packet(
            &network,
            Some(a.node_id()?),
            Kind::Response,
            json!("wrong"),
            Some(request_id.clone()),
        )?;
        requester
            .receive_packet(&network, &c.node_id()?, wrong)
            .await?;
        assert!(matches!(
            receive.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let correct = expected.packet(
            &network,
            Some(a.node_id()?),
            Kind::Response,
            json!("correct"),
            Some(request_id),
        )?;
        requester
            .receive_packet(&network, &b.node_id()?, correct)
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
            &a.network_id()?,
            Some(b.node_id()?),
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
        body.target = Some(a.node_id()?);
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
                &a.network_id()?,
                Some(b.node_id()?),
                Kind::Request,
                json!("x".repeat(MAX_PACKET_BYTES)),
                None
            )
            .is_err()
        );
        let challenge = hub.hello(&a.network_id()?)?;
        let responder = Hello {
            node_id: b.node_id()?,
            nonce: "f".repeat(64),
            ..challenge.clone()
        };
        let transcript = serde_json::to_vec(&Transcript {
            initiator: &challenge,
            responder: &responder,
        })?;
        let signature = a.sign_payload("peer-client", &transcript)?;
        let changed = Hello {
            nonce: "0".repeat(64),
            ..responder
        };
        let replay = serde_json::to_vec(&Transcript {
            initiator: &challenge,
            responder: &changed,
        })?;
        assert!(verify_payload(&a.public_key_hex()?, "peer-client", &replay, &signature).is_err());
        assert!(
            verify_payload(&a.public_key_hex()?, "peer-server", &transcript, &signature).is_err()
        );
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
            client.connect(owner.network_id()?, url),
        )
        .await?;
        assert!(result.is_err());
        assert!(server.online(&owner.network_id()?).await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn actual_reverse_websockets_route_requests_and_deduplicate_cyclic_broadcasts()
    -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let c = LocalIdentity::generate();
        let network = a.network_id()?;
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
        wait_link(&ha, &network, &b.node_id()?).await?;
        let h = hc.clone();
        let n = network.clone();
        tasks.0.push(tokio::spawn(async move {
            let _ = h.connect(n, url_b).await;
        }));
        wait_link(&hb, &network, &c.node_id()?).await?;
        let h = ha.clone();
        let n = network.clone();
        let target = c.node_id()?;
        let request =
            tokio::spawn(async move { h.request(&n, &target, json!({"query":"status"})).await });
        // C has no listening socket: its outbound connection carries requests back in.
        let received = tokio::time::timeout(Duration::from_secs(5), rc.recv())
            .await?
            .context("missing relayed request")?;
        assert_eq!(received.origin, a.node_id()?);
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
        wait_link(&ha, &network, &c.node_id()?).await?;
        hc.broadcast(&network, json!({"event":"one"})).await?;
        let at_a = tokio::time::timeout(Duration::from_secs(5), ra.recv())
            .await?
            .context("missing A broadcast")?;
        let at_b = tokio::time::timeout(Duration::from_secs(5), rb.recv())
            .await?
            .context("missing B broadcast")?;
        assert!(at_a.broadcast && at_b.broadcast);
        assert_eq!(at_a.origin, c.node_id()?);
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
