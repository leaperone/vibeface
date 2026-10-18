//! Authenticated, network-scoped peer transport; business requests remain opaque.

use crate::{
    crypto::{LocalIdentity, digest, verify_payload},
    model::{NetworkId, NodeId, NodeRole, WIRE_VERSION},
    network::{NetworkHub, NetworkSocket, Service},
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
use tokio_tungstenite::{connect_async_with_config, tungstenite::protocol::WebSocketConfig};
use uuid::Uuid;
use vibeface_secure::signature_verification::SignatureVerification;
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

#[derive(Debug, Clone)]
pub struct Inbound {
    pub network_id: NetworkId,
    pub origin: NodeId,
    pub request_id: String,
    pub payload: Value,
    pub broadcast: bool,
    pub(crate) verification: SignatureVerification,
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
    #[serde(rename = "space_id")]
    network_id: NetworkId,
    node_id: NodeId,
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
    #[serde(rename = "space_id")]
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
        self.connected(socket, network, peer, false, channel).await
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
        self.connected(socket, network_id, peer, true, channel)
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
            let has_route = state.links.keys().any(|(n, _)| n == network);
            state
                .links
                .keys()
                .filter(|(n, _)| n == network)
                .map(|(_, n)| n.clone())
                .chain(
                    state
                        .reachable
                        .iter()
                        .filter(|((n, _), time)| {
                            has_route && n == network && time.elapsed() < IDLE_TIMEOUT
                        })
                        .map(|((_, n), _)| n.clone()),
                )
                .collect()
        };
        let store = self.store.lock().await;
        candidates
            .into_iter()
            .filter(|node| {
                store
                    .authorize_transport_node(node, crate::runtime::now_ms() as u64)
                    .is_ok()
            })
            .collect()
    }

    async fn authorize(&self, network: &str, node: &str) -> Result<String> {
        self.authorize_with_verification(network, node, &SignatureVerification::new())
            .await
    }

    async fn authorize_with_verification(
        &self,
        _network: &str,
        node: &str,
        verification: &SignatureVerification,
    ) -> Result<String> {
        let store = self.store.lock().await;
        verification.run(|| {
            Ok(store
                .authorize_transport_node(node, crate::runtime::now_ms() as u64)?
                .public_key)
        })
    }

    fn hello(&self, network: &str) -> Result<Hello> {
        Ok(Hello {
            version: WIRE_VERSION,
            network_id: network.to_owned(),
            node_id: self.identity.node_id()?,
        })
    }

    async fn check_hello(&self, hello: &Hello, network: Option<&str>) -> Result<String> {
        ensure!(hello.version == WIRE_VERSION, "unsupported peer version");
        vibeface_secure::authorization::SpaceId::parse(&hello.network_id)?;
        check_hex_id(&hello.node_id)?;
        ensure!(
            network.is_none_or(|n| n == hello.network_id),
            "peer belongs to another network"
        );
        let local = self.identity.node_id()?;
        ensure!(hello.node_id != local, "cannot connect a node to itself");
        self.authorize(&hello.network_id, &local).await?;
        self.authorize(&hello.network_id, &hello.node_id).await
    }

    async fn server_handshake(
        &self,
        socket: &mut NetworkSocket,
    ) -> Result<(String, String, Channel)> {
        let mut handshake = secure(Handshake::new(
            false,
            &self.identity.secret_key_hex,
            None,
            b"vibeface:peer:v2",
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
            handshake.remote_id().as_deref() == Some(&initiator.node_id),
            "peer Noise identity mismatch"
        );
        let mut channel = secure(handshake.finish())?;
        let responder = self.hello(&initiator.network_id)?;
        send_secure(socket, &mut channel, &serde_json::to_vec(&responder)?).await?;
        Ok((initiator.network_id, initiator.node_id, channel))
    }

    async fn client_handshake(
        &self,
        socket: &mut NetworkSocket,
        network: &str,
        target: Option<String>,
    ) -> Result<(NodeId, Channel)> {
        let mut handshake = secure(Handshake::new(
            true,
            &self.identity.secret_key_hex,
            target,
            b"vibeface:peer:v2",
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
        self.check_hello(&responder, Some(network)).await?;
        ensure!(remote == responder.node_id, "peer Noise identity mismatch");
        Ok((responder.node_id, channel))
    }

    pub async fn connect_target(
        self: Arc<Self>,
        network: String,
        target: String,
        transport: Arc<NetworkHub>,
    ) -> Result<()> {
        let _permit = self
            .sockets
            .clone()
            .try_acquire_owned()
            .context("too many peer sockets")?;
        self.authorize(&network, &self.identity.node_id()?).await?;
        self.authorize(&network, &target).await?;
        for _ in 0..24 {
            let mut socket = transport.dial(&target, Service::Peer).await?;
            match tokio::time::timeout(
                HANDSHAKE_TIMEOUT,
                self.client_handshake(&mut socket, &network, Some(target.clone())),
            )
            .await
            {
                Ok(Ok((peer, channel))) => {
                    return self.connected(socket, network, peer, true, channel).await;
                }
                _ => transport.report_failure(&target, socket.route()).await,
            }
        }
        bail!("no authenticated peer path available")
    }

    async fn connected(
        self: Arc<Self>,
        socket: NetworkSocket,
        network: String,
        peer: String,
        outbound: bool,
        channel: Channel,
    ) -> Result<()> {
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
        let result = self.drive(socket, &network, &peer, receive, channel).await;
        let mut state = self.state.lock().await;
        if state.links.get(&link_key).is_some_and(|link| link.id == id) {
            state.links.remove(&link_key);
        }
        result
    }

    async fn drive(
        &self,
        mut socket: NetworkSocket,
        network: &str,
        peer: &str,
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
                    self.authorize(network, &self.identity.node_id()?).await?;
                    received = Instant::now();
                    match frame {
                        AxumMessage::Binary(bytes) => {
                            if let Some(plaintext) = secure(channel.open_record(&bytes))? {
                                partial_started = None;
                                ensure!(plaintext.len() <= MAX_PACKET_BYTES, "peer packet too large");
                                let packet: Packet = serde_json::from_slice(&plaintext).context("invalid peer packet")?;
                                self.receive_packet(network, peer, packet).await?;
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
                    self.authorize(network, &self.identity.node_id()?).await?;
                    send_secure(&mut socket, &mut channel, text.text.as_bytes()).await?;
                }
                _ = heartbeat.tick() => {
                    ensure!(received.elapsed() < IDLE_TIMEOUT, "peer heartbeat timed out");
                    ensure!(partial_started.is_none_or(|start| start.elapsed() < IDLE_TIMEOUT), "peer fragmented message timed out");
                    self.authorize(network, peer).await?;
                    self.authorize(network, &self.identity.node_id()?).await?;
                    tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.send(AxumMessage::Ping(Vec::new().into()))).await.context("peer send timed out")??;
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
        let verification = SignatureVerification::new();
        self.authorize_with_verification(
            &packet.body.network_id,
            &packet.body.origin,
            &verification,
        )
        .await?;
        if let Some(target) = &packet.body.target {
            self.authorize_with_verification(&packet.body.network_id, target, &verification)
                .await?;
        }
        if !self.remember(&packet).await? {
            return Ok(());
        }
        if packet.body.target.as_deref() == Some(packet.body.origin.as_str()) {
            return self.deliver(&packet, verification).await;
        }
        let sent = self.forward(&packet, None).await?;
        ensure!(
            packet.body.kind != Kind::Request || sent > 0,
            "no peer route available"
        );
        Ok(())
    }

    async fn receive_packet(&self, network: &str, peer: &str, packet: Packet) -> Result<()> {
        let verification = SignatureVerification::new();
        ensure!(
            packet.body.network_id == network,
            "cross-network peer packet"
        );
        ensure!(packet.hops_remaining <= MAX_HOPS, "invalid peer hop limit");
        validate_body(&packet.body, now_ms()?)?;
        let key = match self
            .authorize_with_verification(network, &packet.body.origin, &verification)
            .await
        {
            Ok(key) => key,
            Err(error) => {
                tracing::debug!(%error, origin=%packet.body.origin, network, "dropping peer packet from an unknown member");
                return Ok(());
            }
        };
        if let Some(target) = &packet.body.target
            && let Err(error) = self
                .authorize_with_verification(network, target, &verification)
                .await
        {
            tracing::debug!(%error, target, network, "dropping peer packet for an unknown member");
            return Ok(());
        }
        verification.run(|| {
            verify_payload(
                &key,
                "peer-packet",
                &serde_json::to_vec(&packet.body)?,
                &packet.signature,
            )
        })?;
        if !self.remember(&packet).await? {
            return Ok(());
        }
        let local = self.identity.node_id()?;
        if packet.body.target.as_deref() == Some(local.as_str()) {
            return self.deliver(&packet, verification).await;
        }
        if packet.body.kind == Kind::Presence {
            let mut state = self.state.lock().await;
            state.reachable.retain(|_, at| at.elapsed() < IDLE_TIMEOUT);
            state.reachable.insert(
                (network.to_owned(), packet.body.origin.clone()),
                Instant::now(),
            );
        } else if packet.body.kind == Kind::Broadcast {
            self.deliver(&packet, verification).await?;
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

    async fn deliver(&self, packet: &Packet, verification: SignatureVerification) -> Result<()> {
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
                    verification,
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
    vibeface_secure::authorization::SpaceId::parse(&body.network_id)?;
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
    use crate::identity::test_support::{TestNetwork, now};
    use axum::{
        Router,
        extract::{State as AxumState, WebSocketUpgrade},
        routing::get,
    };
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use tokio::{net::TcpListener, task::JoinHandle};
    use vibeface_secure::{
        Identity,
        authorization::{
            Actor, DeviceCertificate, DeviceId, DeviceUse, JournalEntry, Nonce, PolicyDomain,
            PublicKey, ServiceId, Signed,
        },
    };

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
        owner: &TestNetwork,
        identity: &LocalIdentity,
        nodes: &[&LocalIdentity],
    ) -> Result<(Arc<PeerHub>, mpsc::Receiver<Inbound>)> {
        let mut store = owner.memory()?;
        for node in nodes {
            if node.node_id()? == owner.node.node_id()? {
                continue;
            }
            let key = PublicKey::parse(&node.public_key_hex()?)?;
            let device =
                Identity::from_secret_hex(&node.secret_key_hex).map_err(anyhow::Error::msg)?;
            let head = store
                .authorization_state()
                .head(&PolicyDomain::Account(owner.account.clone()))
                .context("account head missing")?
                .clone();
            let mut certificate = DeviceCertificate {
                version: vibeface_secure::authorization::AUTH_VERSION,
                owner: PolicyDomain::Account(owner.account.clone()),
                subject: Actor::Service(ServiceId::from_key(&key)),
                device: DeviceId::from_key(&key),
                public_key: key.clone(),
                uses: vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
                possession: None,
                enrollment_grant: None,
                issued_under: head.clone(),
                nonce: Nonce::from_bytes(Sha256::digest(key.as_str().as_bytes()).into()),
                not_before_ms: now(),
                expires_at_ms: now() + 1_000_000,
            };
            certificate.prove_possession(&device)?;
            let certificate = Signed::sign(certificate, &owner.root)?;
            let mut document = store
                .authorization_state()
                .account(&owner.account)
                .context("account missing")?
                .clone();
            document.previous = Some(head);
            document.sequence += 1;
            document.devices.push(certificate.hash()?);
            document.devices.sort();
            store.install_policy_journal(
                &[
                    JournalEntry::Device(certificate),
                    JournalEntry::Account(Signed::sign(document, &owner.root)?),
                ],
                now(),
            )?;
        }
        store.save_local_device(owner.space.as_str(), identity)?;
        Ok(PeerHub::new(
            identity.clone(),
            Arc::new(Mutex::new(store)),
            &roles(),
        ))
    }

    fn revoke_service(owner: &TestNetwork, store: &mut Store, node: &LocalIdentity) -> Result<()> {
        if node.node_id()? == owner.node.node_id()? {
            return owner.revoke_node(store);
        }
        let device = DeviceId::from_key(&PublicKey::parse(&node.public_key_hex()?)?);
        let mut document = store
            .authorization_state()
            .account(&owner.account)
            .context("account missing")?
            .clone();
        document.previous = store
            .authorization_state()
            .head(&PolicyDomain::Account(owner.account.clone()))
            .cloned();
        document.sequence += 1;
        document.revoked_devices.push(device.clone());
        document.revoked_devices.sort();
        document.devices.retain(|hash| {
            store
                .authorization_state()
                .device(hash)
                .is_none_or(|certificate| certificate.payload.device != device)
        });
        store.install_policy_journal(
            &[JournalEntry::Account(Signed::sign(document, &owner.root)?)],
            now() + 1,
        )
    }

    #[tokio::test]
    async fn forwarding_requires_enabled_relay_and_active_service_membership() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let c = LocalIdentity::generate();
        let owner = TestNetwork::with_node(a.clone())?;
        let network = owner.space.to_string();
        let (sender, _) = hub(&owner, &a, &[&a, &b, &c])?;
        let (enabled, _) = hub(&owner, &b, &[&a, &b, &c])?;
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
        let packet_body = serde_json::to_vec(&packet.body)?;
        let packet_signature = packet.signature.clone();
        disabled
            .receive_packet(&network, &a.node_id()?, packet)
            .await?;
        let local = inbound.try_recv()?;
        assert_eq!(local.payload, json!("local"));
        let before = local.verification.counts();
        assert!(before.actual_verifications > 0 && before.actual_verifications <= 32);
        local.verification.run(|| {
            verify_payload(
                &a.public_key_hex()?,
                "peer-packet",
                &packet_body,
                &packet_signature,
            )
        })?;
        let after = local.verification.counts();
        assert_eq!(after.actual_verifications, before.actual_verifications);
        assert_eq!(after.cache_hits, before.cache_hits + 1);
        revoke_service(&owner, &mut *enabled.store.lock().await, &b)?;
        let packet = sender.packet(
            &network,
            Some(c.node_id()?),
            Kind::Request,
            json!("revoked service"),
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
        let owner = TestNetwork::with_node(a.clone())?;
        let network = owner.space.to_string();
        let (sender, _) = hub(&owner, &a, &[&a, &b])?;
        let (receiver, mut inbound) = hub(&owner, &b, &[&a, &b])?;
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
                .receive_packet(
                    &TestNetwork::new()?.space.to_string(),
                    &a.node_id()?,
                    packet.clone()
                )
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
        revoke_service(&owner, &mut *receiver.store.lock().await, &a)?;
        let packet = sender.packet(
            &network,
            Some(b.node_id()?),
            Kind::Request,
            Value::Null,
            None,
        )?;
        receiver
            .receive_packet(&network, &a.node_id()?, packet)
            .await?;
        assert!(inbound.try_recv().is_err());
        Ok(())
    }

    #[tokio::test]
    async fn unknown_member_packet_is_dropped_without_closing_trusted_link() -> Result<()> {
        let owner = TestNetwork::new()?;
        let known = LocalIdentity::generate();
        let unknown = LocalIdentity::generate();
        let network = owner.space.to_string();
        let (receiver, mut inbound) = hub(&owner, &known, &[&owner.node, &known])?;
        let (sender, _) = hub(&owner, &unknown, &[&owner.node, &known, &unknown])?;
        let (send, _) = mpsc::channel(1);
        receiver.state.lock().await.links.insert(
            (network.clone(), owner.node.node_id()?),
            Link {
                id: "trusted".into(),
                outbound: true,
                send,
            },
        );
        let packet = sender.packet(
            &network,
            Some(known.node_id()?),
            Kind::Request,
            json!({"unknown":true}),
            None,
        )?;
        receiver
            .receive_packet(&network, &owner.node.node_id()?, packet)
            .await?;
        assert!(inbound.try_recv().is_err());
        assert!(
            receiver
                .state
                .lock()
                .await
                .links
                .contains_key(&(network, owner.node.node_id()?))
        );
        Ok(())
    }

    #[tokio::test]
    async fn replies_must_match_expected_origin_and_network() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let c = LocalIdentity::generate();
        let owner = TestNetwork::with_node(a.clone())?;
        let network = owner.space.to_string();
        let (requester, _) = hub(&owner, &a, &[&a, &b, &c])?;
        let (expected, _) = hub(&owner, &b, &[&a, &b, &c])?;
        let (other, _) = hub(&owner, &c, &[&a, &b, &c])?;
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
        let owner = TestNetwork::with_node(a.clone())?;
        let (hub, _) = hub(&owner, &a, &[&a, &b])?;
        let packet = hub.packet(
            owner.space.as_str(),
            Some(b.node_id()?),
            Kind::Request,
            json!("ok"),
            None,
        )?;
        let encoded = serde_json::to_value(&packet.body)?;
        assert_eq!(encoded["space_id"], owner.space.as_str());
        assert!(encoded.get("network_id").is_none());
        let now = now_ms()?;
        let mut body = packet.body.clone();
        body.version = 1;
        assert!(validate_body(&body, now).is_err());
        body = packet.body.clone();
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
                owner.space.as_str(),
                Some(b.node_id()?),
                Kind::Request,
                json!("x".repeat(MAX_PACKET_BYTES)),
                None
            )
            .is_err()
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
        let owner = TestNetwork::new()?;
        let outsider = LocalIdentity::generate();
        let (server, _) = hub(&owner, &owner.node, &[&owner.node])?;
        let (client, _) = hub(&owner, &outsider, &[&owner.node, &outsider])?;
        let mut tasks = Tasks(Vec::new());
        let url = serve(server.clone(), &mut tasks).await?;
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            client.connect(owner.space.to_string(), url),
        )
        .await?;
        assert!(result.is_err());
        assert!(server.online(owner.space.as_str()).await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn lost_last_link_stops_reporting_cached_presence_and_allows_reconnect() -> Result<()> {
        let owner = TestNetwork::new()?;
        let a = &owner.node;
        let b = LocalIdentity::generate();
        let network = owner.space.to_string();
        let target = b.node_id()?;
        let (server, _) = hub(&owner, a, &[a, &b])?;
        let (client, _) = hub(&owner, &b, &[a, &b])?;
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
            while !server
                .state
                .lock()
                .await
                .reachable
                .contains_key(&(network.clone(), target.clone()))
            {
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

        let (restarted, mut inbound) = hub(&owner, &b, &[a, &b])?;
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
        let owner = TestNetwork::with_node(a.clone())?;
        let network = owner.space.to_string();
        let (ha, mut ra) = hub(&owner, &a, &[&a, &b, &c])?;
        let (hb, mut rb) = hub(&owner, &b, &[&a, &b, &c])?;
        let (hc, mut rc) = hub(&owner, &c, &[&a, &b, &c])?;
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
