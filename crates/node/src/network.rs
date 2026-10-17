//! Public address discovery and bounded opaque tunnels. Discovery never grants membership.

use crate::crypto::{LocalIdentity, fingerprint, verify_payload};
use anyhow::{Context, Result, bail, ensure};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::TcpStream,
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message as NativeMessage, protocol::WebSocketConfig},
};

pub const MAX_FRAME_BYTES: usize = 65_535;
const MAX_CONTROL_BYTES: usize = 16 * 1024;
const MAX_SOCKETS: usize = 128;
const MAX_RECORDS: usize = 2048;
const MAX_RESERVATIONS: usize = 64;
const MAX_CIRCUITS: usize = 32;
const MAX_URLS: usize = 8;
const RECORD_TTL_SECS: i64 = 180;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT: Duration = Duration::from_secs(20);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const RESERVATION_LIFETIME: Duration = Duration::from_secs(3600);
const CIRCUIT_LIFETIME: Duration = Duration::from_secs(1800);
const CIRCUIT_BYTE_LIMIT: usize = 64 * 1024 * 1024;
const CIRCUIT_BYTES_PER_SECOND: usize = 2 * 1024 * 1024;
const SERVICE_TYPE: &str = "_vibeface._tcp.local.";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    #[serde(default)]
    pub bootstrap_urls: Vec<String>,
    #[serde(default)]
    pub advertise_urls: Vec<String>,
    #[serde(default)]
    pub mdns: bool,
    #[serde(default)]
    pub relay_enabled: bool,
}
impl NetworkConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.bootstrap_urls.len() <= MAX_URLS && self.advertise_urls.len() <= MAX_URLS,
            "too many network URLs"
        );
        for url in self.bootstrap_urls.iter().chain(&self.advertise_urls) {
            validate_url(url)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    Peer,
    Client,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reachability {
    pub node_id: String,
    pub public_key: String,
    pub direct_urls: Vec<String>,
    pub relay_urls: Vec<String>,
    pub issued_at: i64,
    pub expires_at: i64,
    pub signature: String,
}
impl Reachability {
    fn unsigned(&self) -> Result<Vec<u8>> {
        let mut record = self.clone();
        record.signature.clear();
        Ok(serde_json::to_vec(&record)?)
    }
    pub fn verify(&self, now: i64) -> Result<()> {
        ensure!(
            self.node_id == fingerprint(&self.public_key)?,
            "reachability identity mismatch"
        );
        ensure!(
            self.direct_urls.len() <= MAX_URLS && self.relay_urls.len() <= MAX_URLS,
            "too many reachability URLs"
        );
        ensure!(
            self.issued_at <= now + 30
                && self.expires_at > now
                && self.expires_at > self.issued_at
                && self.expires_at <= self.issued_at + RECORD_TTL_SECS,
            "expired or invalid reachability lifetime"
        );
        for url in self.direct_urls.iter().chain(&self.relay_urls) {
            validate_url(url)?;
        }
        verify_payload(
            &self.public_key,
            "reachability",
            &self.unsigned()?,
            &self.signature,
        )
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Control {
    Welcome {
        nonce: String,
    },
    Direct {
        service: Service,
    },
    Publish {
        record: Reachability,
    },
    Lookup {
        target: String,
    },
    Record {
        record: Option<Reachability>,
    },
    Reserve {
        node_id: String,
        public_key: String,
        signature: String,
    },
    Reserved,
    Dial {
        target: String,
        service: Service,
    },
    Incoming {
        token: String,
        service: Service,
    },
    Attach {
        token: String,
        signature: String,
    },
    Ready,
    Published,
}

#[derive(Serialize)]
struct ReservationProof<'a> {
    nonce: &'a str,
    node_id: &'a str,
}
#[derive(Serialize)]
struct AttachmentProof<'a> {
    nonce: &'a str,
    token: &'a str,
}

pub struct IncomingSocket {
    pub service: Service,
    pub socket: NetworkSocket,
}

pub enum NetworkSocket {
    Server(WebSocket),
    Limited(Box<NetworkSocket>, OwnedSemaphorePermit),
    Routed(Box<NetworkSocket>, String),
    Client(Box<WebSocketStream<MaybeTlsStream<TcpStream>>>),
}
impl NetworkSocket {
    pub fn server(socket: WebSocket) -> Self {
        Self::Server(socket)
    }
    fn route(&self) -> Option<&str> {
        match self {
            Self::Routed(_, route) => Some(route),
            Self::Limited(socket, _) => socket.route(),
            _ => None,
        }
    }
}
impl Stream for NetworkSocket {
    type Item = Result<Message>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        match self.get_mut() {
            Self::Limited(socket, _) | Self::Routed(socket, _) => {
                Pin::new(&mut **socket).poll_next(cx)
            }
            Self::Server(socket) => Pin::new(socket)
                .poll_next(cx)
                .map(|item| item.map(|result| result.map_err(Into::into))),
            Self::Client(socket) => loop {
                match Pin::new(&mut **socket).poll_next(cx) {
                    Poll::Ready(Some(Ok(frame))) => {
                        let frame = match frame {
                            NativeMessage::Text(value) => {
                                Message::Text(value.as_str().to_owned().into())
                            }
                            NativeMessage::Binary(value) => Message::Binary(value),
                            NativeMessage::Ping(value) => Message::Ping(value),
                            NativeMessage::Pong(value) => Message::Pong(value),
                            NativeMessage::Close(_) => Message::Close(None),
                            NativeMessage::Frame(_) => continue,
                        };
                        return Poll::Ready(Some(Ok(frame)));
                    }
                    Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(error.into()))),
                    Poll::Ready(None) => return Poll::Ready(None),
                    Poll::Pending => return Poll::Pending,
                }
            },
        }
    }
}
impl Sink<Message> for NetworkSocket {
    type Error = anyhow::Error;
    fn poll_ready(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<()>> {
        match self.get_mut() {
            Self::Limited(socket, _) | Self::Routed(socket, _) => {
                Pin::new(&mut **socket).poll_ready(cx)
            }
            Self::Server(socket) => Pin::new(socket).poll_ready(cx).map_err(Into::into),
            Self::Client(socket) => Pin::new(&mut **socket).poll_ready(cx).map_err(Into::into),
        }
    }
    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<()> {
        match self.get_mut() {
            Self::Limited(socket, _) | Self::Routed(socket, _) => {
                Pin::new(&mut **socket).start_send(item)
            }
            Self::Server(socket) => Pin::new(socket).start_send(item).map_err(Into::into),
            Self::Client(socket) => Pin::new(&mut **socket)
                .start_send(match item {
                    Message::Text(value) => NativeMessage::Text(value.as_str().to_owned().into()),
                    Message::Binary(value) => NativeMessage::Binary(value),
                    Message::Ping(value) => NativeMessage::Ping(value),
                    Message::Pong(value) => NativeMessage::Pong(value),
                    Message::Close(_) => NativeMessage::Close(None),
                })
                .map_err(Into::into),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<()>> {
        match self.get_mut() {
            Self::Limited(socket, _) | Self::Routed(socket, _) => {
                Pin::new(&mut **socket).poll_flush(cx)
            }
            Self::Server(socket) => Pin::new(socket).poll_flush(cx).map_err(Into::into),
            Self::Client(socket) => Pin::new(&mut **socket).poll_flush(cx).map_err(Into::into),
        }
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<()>> {
        match self.get_mut() {
            Self::Limited(socket, _) | Self::Routed(socket, _) => {
                Pin::new(&mut **socket).poll_close(cx)
            }
            Self::Server(socket) => Pin::new(socket).poll_close(cx).map_err(Into::into),
            Self::Client(socket) => Pin::new(&mut **socket).poll_close(cx).map_err(Into::into),
        }
    }
}

pub struct NetworkHub {
    identity: LocalIdentity,
    config: NetworkConfig,
    state: Mutex<State>,
    incoming: mpsc::Sender<IncomingSocket>,
    sockets: Arc<Semaphore>,
    circuits: Arc<Semaphore>,
}
#[derive(Default)]
struct State {
    records: HashMap<String, Reachability>,
    candidates: HashMap<String, Vec<String>>,
    reservations: HashMap<String, Reservation>,
    attachments: HashMap<String, Attachment>,
    active_relays: HashSet<String>,
    failed_routes: HashMap<(String, String), Instant>,
    incoming_rate: ControlRate,
}
struct Reservation {
    id: String,
    public_key: String,
    send: mpsc::Sender<Control>,
}
struct Attachment {
    public_key: String,
    created: Instant,
    socket: oneshot::Sender<NetworkSocket>,
}

impl NetworkHub {
    pub fn new(
        identity: LocalIdentity,
        config: NetworkConfig,
    ) -> (Arc<Self>, mpsc::Receiver<IncomingSocket>) {
        let (incoming, receive) = mpsc::channel(32);
        (
            Arc::new(Self {
                identity,
                config,
                state: Mutex::new(State::default()),
                incoming,
                sockets: Arc::new(Semaphore::new(MAX_SOCKETS)),
                circuits: Arc::new(Semaphore::new(MAX_CIRCUITS)),
            }),
            receive,
        )
    }
    pub async fn accept(self: Arc<Self>, socket: WebSocket) -> Result<()> {
        self.state.lock().await.incoming_rate.charge(600)?;
        let permit = self
            .sockets
            .clone()
            .try_acquire_owned()
            .context("public socket limit reached")?;
        let mut socket = NetworkSocket::Limited(Box::new(NetworkSocket::server(socket)), permit);
        let nonce = random_token();
        send_control(
            &mut socket,
            &Control::Welcome {
                nonce: nonce.clone(),
            },
        )
        .await?;
        let control = tokio::time::timeout(CONNECT_TIMEOUT, receive_control(&mut socket))
            .await
            .context("network control timed out")??;
        match control {
            Control::Direct { service } => {
                send_control(&mut socket, &Control::Ready).await?;
                self.incoming
                    .try_send(IncomingSocket { service, socket })
                    .map_err(|_| anyhow::anyhow!("network service queue full"))?;
                Ok(())
            }
            Control::Publish { record } => {
                self.publish(record).await?;
                send_control(&mut socket, &Control::Published).await
            }
            Control::Lookup { target } => {
                check_id(&target)?;
                let record = self.lookup_local(&target).await?;
                send_control(&mut socket, &Control::Record { record }).await
            }
            Control::Reserve {
                node_id,
                public_key,
                signature,
            } => {
                ensure!(self.config.relay_enabled, "public relay is disabled");
                ensure!(
                    node_id == fingerprint(&public_key)?,
                    "reservation identity mismatch"
                );
                verify_payload(
                    &public_key,
                    "relay-reserve",
                    &serde_json::to_vec(&ReservationProof {
                        nonce: &nonce,
                        node_id: &node_id,
                    })?,
                    &signature,
                )?;
                self.reservation(socket, node_id, public_key).await
            }
            Control::Dial { target, service } => {
                ensure!(self.config.relay_enabled, "public relay is disabled");
                check_id(&target)?;
                self.relay(socket, target, service).await
            }
            Control::Attach { token, signature } => {
                let mut state = self.state.lock().await;
                let attachment = state
                    .attachments
                    .get(&token)
                    .context("unknown relay attachment")?;
                ensure!(
                    attachment.created.elapsed() < CONNECT_TIMEOUT,
                    "expired relay attachment"
                );
                verify_payload(
                    &attachment.public_key,
                    "relay-attach",
                    &serde_json::to_vec(&AttachmentProof {
                        nonce: &nonce,
                        token: &token,
                    })?,
                    &signature,
                )?;
                let attachment = state
                    .attachments
                    .remove(&token)
                    .context("relay attachment already used")?;
                attachment
                    .socket
                    .send(socket)
                    .map_err(|_| anyhow::anyhow!("relay caller disconnected"))
            }
            _ => bail!("unexpected initial network control"),
        }
    }

    async fn lookup_local(&self, target: &str) -> Result<Option<Reachability>> {
        if target == self.identity.node_id()? {
            return Ok(Some(self.local_record().await?));
        }
        let mut state = self.state.lock().await;
        let now = now_secs()?;
        state.records.retain(|_, r| r.expires_at > now);
        Ok(state.records.get(target).cloned())
    }
    async fn publish(&self, record: Reachability) -> Result<()> {
        let now = now_secs()?;
        record.verify(now)?;
        let mut state = self.state.lock().await;
        state.records.retain(|_, r| r.expires_at > now);
        if let Some(old) = state.records.get(&record.node_id) {
            ensure!(
                record.issued_at >= old.issued_at,
                "older reachability record"
            );
        } else {
            ensure!(
                state.records.len() < MAX_RECORDS,
                "public address directory full"
            );
        }
        state.records.insert(record.node_id.clone(), record);
        Ok(())
    }
    async fn local_record(&self) -> Result<Reachability> {
        let now = now_secs()?;
        let mut relay_urls: Vec<_> = self
            .state
            .lock()
            .await
            .active_relays
            .iter()
            .cloned()
            .collect();
        relay_urls.sort();
        let mut record = Reachability {
            node_id: self.identity.node_id()?,
            public_key: self.identity.public_key_hex()?,
            direct_urls: self.config.advertise_urls.clone(),
            relay_urls,
            issued_at: now,
            expires_at: now + RECORD_TTL_SECS,
            signature: String::new(),
        };
        record.signature = self
            .identity
            .sign_payload("reachability", &record.unsigned()?)?;
        Ok(record)
    }
    pub async fn resolve(&self, target: &str) -> Result<Vec<Reachability>> {
        check_id(target)?;
        let mut records = Vec::new();
        if let Some(record) = self.lookup_local(target).await? {
            records.push(record);
        }
        let queries = self.config.bootstrap_urls.iter().map(|url| async move {
            let mut socket = open_control(url).await?.0;
            send_control(
                &mut socket,
                &Control::Lookup {
                    target: target.to_owned(),
                },
            )
            .await?;
            let Control::Record { record } = receive_control(&mut socket).await? else {
                bail!("invalid directory reply");
            };
            if let Some(record) = &record {
                record.verify(now_secs()?)?;
                ensure!(record.node_id == target, "directory returned another node");
            }
            Ok::<_, anyhow::Error>(record)
        });
        for result in futures_util::future::join_all(queries).await {
            if let Ok(Some(record)) = result {
                self.publish(record.clone()).await?;
                records.push(record);
            }
        }
        records.sort_by_key(|record| std::cmp::Reverse(record.issued_at));
        records.dedup_by(|a, b| a.signature == b.signature);
        Ok(records)
    }
    pub async fn dial(&self, target: &str, service: Service) -> Result<NetworkSocket> {
        check_id(target)?;
        let now = now_secs()?;
        let (cached, failures) = {
            let mut state = self.state.lock().await;
            state
                .failed_routes
                .retain(|_, at| at.elapsed() < Duration::from_secs(30));
            let mut urls = state.candidates.get(target).cloned().unwrap_or_default();
            if let Some(record) = state
                .records
                .get(target)
                .filter(|record| record.expires_at > now)
            {
                urls.extend(record.direct_urls.clone());
            }
            (urls, state.failed_routes.clone())
        };
        let mut seen = HashSet::new();
        if let Some(socket) = dial_direct(cached, target, service, &failures, &mut seen).await {
            return Ok(socket);
        }
        let records = self.resolve(target).await?;
        let direct = records
            .iter()
            .flat_map(|record| record.direct_urls.clone())
            .collect();
        if let Some(socket) = dial_direct(direct, target, service, &failures, &mut seen).await {
            return Ok(socket);
        }
        let mut seen = HashSet::new();
        for url in records
            .iter()
            .flat_map(|record| &record.relay_urls)
            .filter(|url| seen.insert((*url).clone()))
            .take(MAX_URLS)
        {
            let route = format!("relay:{url}");
            if failures.contains_key(&(target.to_owned(), route.clone())) {
                continue;
            }
            if let Ok(socket) = connect_service(
                url,
                Control::Dial {
                    target: target.to_owned(),
                    service,
                },
            )
            .await
            {
                return Ok(NetworkSocket::Routed(Box::new(socket), route));
            }
        }
        bail!("node has no reachable direct or relay path")
    }
    pub async fn report_failure(&self, target: &str, socket: &NetworkSocket) {
        if let Some(route) = socket.route() {
            let mut state = self.state.lock().await;
            state
                .failed_routes
                .retain(|_, at| at.elapsed() < Duration::from_secs(30));
            if state.failed_routes.len() < 1024 {
                state
                    .failed_routes
                    .insert((target.to_owned(), route.to_owned()), Instant::now());
            }
        }
    }

    async fn reservation(
        self: Arc<Self>,
        mut socket: NetworkSocket,
        node_id: String,
        public_key: String,
    ) -> Result<()> {
        let id = random_token();
        let (send, mut receive) = mpsc::channel(16);
        {
            let mut state = self.state.lock().await;
            ensure!(
                !state.reservations.contains_key(&node_id),
                "node is already reserved"
            );
            ensure!(
                state.reservations.len() < MAX_RESERVATIONS,
                "relay reservation capacity reached"
            );
            state.reservations.insert(
                node_id.clone(),
                Reservation {
                    id: id.clone(),
                    public_key,
                    send,
                },
            );
        }
        let result = async {
            send_control(&mut socket, &Control::Reserved).await?;
            let deadline = tokio::time::sleep(RESERVATION_LIFETIME);
            tokio::pin!(deadline);
            let mut heartbeat = tokio::time::interval(HEARTBEAT);
            let mut last_received = Instant::now();
            let mut rate = ControlRate::default();
            loop {
                tokio::select! {
                    _ = &mut deadline => bail!("relay reservation expired"),
                    _ = heartbeat.tick() => {
                        ensure!(last_received.elapsed() < IDLE_TIMEOUT, "relay reservation idle");
                        send_frame(&mut socket, Message::Ping(Vec::new().into())).await?;
                    }
                    command = receive.recv() => send_control(&mut socket, &command.context("reservation replaced")?).await?,
                    frame = socket.next() => {
                        let frame = frame.context("reservation closed")??;
                        last_received = Instant::now();
                        match frame {
                            Message::Text(text) => {
                                rate.charge(30)?;
                                ensure!(text.len() <= MAX_CONTROL_BYTES, "network control too large");
                                let Control::Publish { record } = serde_json::from_str(&text)? else { bail!("invalid reservation control"); };
                                ensure!(record.node_id == node_id, "reservation may publish only its own record");
                                self.publish(record).await?;
                            }
                            Message::Ping(_) | Message::Pong(_) => {},
                            _ => bail!("invalid reservation frame"),
                        }
                    }
                }
            }
        }.await;
        let mut state = self.state.lock().await;
        if state.reservations.get(&node_id).is_some_and(|r| r.id == id) {
            state.reservations.remove(&node_id);
        }
        result
    }
    async fn relay(
        &self,
        mut caller: NetworkSocket,
        target: String,
        service: Service,
    ) -> Result<()> {
        let _permit = self
            .circuits
            .clone()
            .try_acquire_owned()
            .context("relay circuit capacity reached")?;
        let token = random_token();
        let (send, receive) = oneshot::channel();
        {
            let mut state = self.state.lock().await;
            state
                .attachments
                .retain(|_, attachment| attachment.created.elapsed() < CONNECT_TIMEOUT);
            let reservation = state
                .reservations
                .get(&target)
                .context("target has no active relay reservation")?;
            reservation
                .send
                .try_send(Control::Incoming {
                    token: token.clone(),
                    service,
                })
                .context("target relay queue full")?;
            let public_key = reservation.public_key.clone();
            state.attachments.insert(
                token.clone(),
                Attachment {
                    public_key,
                    created: Instant::now(),
                    socket: send,
                },
            );
        }
        let attached = tokio::time::timeout(CONNECT_TIMEOUT, receive).await;
        self.state.lock().await.attachments.remove(&token);
        let mut target = attached
            .context("target relay connection timed out")?
            .context("target relay connection cancelled")?;
        send_control(&mut caller, &Control::Ready).await?;
        send_control(&mut target, &Control::Ready).await?;
        tokio::time::timeout(
            CIRCUIT_LIFETIME,
            forward_ciphertext(&mut caller, &mut target),
        )
        .await
        .context("relay circuit lifetime reached")?
    }

    pub async fn run(self: Arc<Self>, listen: SocketAddr) -> Result<()> {
        self.config.validate()?;
        let mut tasks = tokio::task::JoinSet::new();
        for url in &self.config.bootstrap_urls {
            let hub = self.clone();
            let url = url.clone();
            tasks.spawn(async move {
                loop {
                    if let Err(error) = hub.clone().maintain_relay(&url).await {
                        tracing::debug!(%error, "rendezvous reconnect pending");
                    }
                    hub.state.lock().await.active_relays.remove(&url);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            });
        }
        if self.config.mdns {
            let hub = self.clone();
            tasks.spawn(async move {
                if let Err(error) = hub.mdns(listen).await {
                    tracing::warn!(%error, "LAN discovery unavailable");
                }
            });
        }
        futures_util::future::pending().await
    }

    async fn maintain_relay(self: Arc<Self>, url: &str) -> Result<()> {
        // Publication works even when the chosen rendezvous declines public relay service.
        self.publish_at(url).await?;
        let (mut socket, nonce) = open_control(url).await?;
        let node_id = self.identity.node_id()?;
        let signature = self.identity.sign_payload(
            "relay-reserve",
            &serde_json::to_vec(&ReservationProof {
                nonce: &nonce,
                node_id: &node_id,
            })?,
        )?;
        send_control(
            &mut socket,
            &Control::Reserve {
                node_id,
                public_key: self.identity.public_key_hex()?,
                signature,
            },
        )
        .await?;
        ensure!(
            matches!(receive_control(&mut socket).await?, Control::Reserved),
            "relay rejected reservation"
        );
        self.state.lock().await.active_relays.insert(url.to_owned());
        let result = async {
            let mut publish = tokio::time::interval(Duration::from_secs(60));
            publish.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut heartbeat = tokio::time::interval(HEARTBEAT);
            let mut last_received = Instant::now();
            loop {
                tokio::select! {
                    _ = publish.tick() => {
                        let record = self.local_record().await?;
                        send_control(&mut socket, &Control::Publish { record }).await?;
                        // Refresh every configured directory, so alternate bootstrap entries remain useful.
                        for entry in &self.config.bootstrap_urls {
                            if entry != url { let _ = self.publish_at(entry).await; }
                        }
                    }
                    _ = heartbeat.tick() => {
                        ensure!(last_received.elapsed() < IDLE_TIMEOUT, "relay keepalive timed out");
                        send_frame(&mut socket, Message::Ping(Vec::new().into())).await?;
                    }
                    frame = socket.next() => {
                        last_received = Instant::now();
                        match frame.context("relay disconnected")?? {
                            Message::Ping(_) | Message::Pong(_) => {},
                            Message::Text(text) => {
                                ensure!(text.len() <= MAX_CONTROL_BYTES, "relay control too large");
                                let Control::Incoming { token, service } = serde_json::from_str(&text)? else { bail!("invalid relay control"); };
                                check_id(&token)?;
                                let permit = self.sockets.clone().try_acquire_owned().context("outbound relay socket limit reached")?;
                                let hub = self.clone();
                                let url = url.to_owned();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    if let Err(error) = hub.attach(&url, token, service).await { tracing::debug!(%error, "relay attachment failed"); }
                                });
                            }
                            _ => bail!("invalid relay reservation message"),
                        }
                    }
                }
            }
        }.await;
        self.state.lock().await.active_relays.remove(url);
        result
    }
    async fn publish_at(&self, url: &str) -> Result<()> {
        let (mut socket, _) = open_control(url).await?;
        send_control(
            &mut socket,
            &Control::Publish {
                record: self.local_record().await?,
            },
        )
        .await?;
        ensure!(
            matches!(receive_control(&mut socket).await?, Control::Published),
            "directory publication failed"
        );
        Ok(())
    }
    async fn attach(&self, url: &str, token: String, service: Service) -> Result<()> {
        let (mut socket, nonce) = open_control(url).await?;
        let signature = self.identity.sign_payload(
            "relay-attach",
            &serde_json::to_vec(&AttachmentProof {
                nonce: &nonce,
                token: &token,
            })?,
        )?;
        send_control(&mut socket, &Control::Attach { token, signature }).await?;
        ensure!(
            matches!(receive_control(&mut socket).await?, Control::Ready),
            "relay attachment was not ready"
        );
        self.incoming
            .try_send(IncomingSocket { service, socket })
            .map_err(|_| anyhow::anyhow!("network service queue full"))
    }
    async fn mdns(&self, listen: SocketAddr) -> Result<()> {
        let daemon = ServiceDaemon::new()?;
        let node_id = self.identity.node_id()?;
        let properties = [("node_id", node_id.as_str()), ("path", "/api/network/ws")];
        if !listen.ip().is_loopback() {
            let service = ServiceInfo::new(
                SERVICE_TYPE,
                &node_id,
                &format!("vf-{}.local.", &node_id[..16]),
                "",
                listen.port(),
                &properties[..],
            )?
            .enable_addr_auto();
            daemon.register(service)?;
        }
        let events = daemon.browse(SERVICE_TYPE)?;
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            while let Ok(event) = events.try_recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let Some(target) = info.get_property_val_str("node_id") else {
                            continue;
                        };
                        if check_id(target).is_err() || target == node_id {
                            continue;
                        }
                        let urls: Vec<_> = info
                            .get_addresses()
                            .iter()
                            .filter(|ip| !ip.is_unspecified())
                            .take(MAX_URLS)
                            .map(|ip| {
                                format!(
                                    "ws://{}/api/network/ws",
                                    SocketAddr::new(*ip, info.get_port())
                                )
                            })
                            .collect();
                        let mut state = self.state.lock().await;
                        if state.candidates.len() < 256 || state.candidates.contains_key(target) {
                            state.candidates.insert(target.to_owned(), urls);
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, name) => {
                        if let Some(target) = name.strip_suffix(&format!(".{SERVICE_TYPE}")) {
                            self.state.lock().await.candidates.remove(target);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[derive(Default)]
struct ControlRate {
    start: Option<Instant>,
    count: usize,
}
impl ControlRate {
    fn charge(&mut self, limit: usize) -> Result<()> {
        if self
            .start
            .is_none_or(|start| start.elapsed() >= Duration::from_secs(60))
        {
            self.start = Some(Instant::now());
            self.count = 0;
        }
        self.count += 1;
        ensure!(self.count <= limit, "reservation control rate exceeded");
        Ok(())
    }
}
async fn forward_ciphertext(a: &mut NetworkSocket, b: &mut NetworkSocket) -> Result<()> {
    let mut bytes = 0usize;
    let mut window_bytes = 0usize;
    let mut window = Instant::now();
    let mut last_received = Instant::now();
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    let mut frame_rate = ControlRate::default();
    loop {
        tokio::select! {
            frame = a.next() => {
                let frame = frame.context("relay caller disconnected")??;
                frame_rate.charge(30_720)?;
                relay_frame(frame, b, &mut bytes, &mut window_bytes, &mut window).await?;
                last_received = Instant::now();
            }
            frame = b.next() => {
                let frame = frame.context("relay target disconnected")??;
                frame_rate.charge(30_720)?;
                relay_frame(frame, a, &mut bytes, &mut window_bytes, &mut window).await?;
                last_received = Instant::now();
            }
            _ = heartbeat.tick() => {
                ensure!(last_received.elapsed() < IDLE_TIMEOUT, "relay circuit idle");
                send_frame(a, Message::Ping(Vec::new().into())).await?;
                send_frame(b, Message::Ping(Vec::new().into())).await?;
            }
        }
    }
}
async fn relay_frame(
    frame: Message,
    destination: &mut NetworkSocket,
    bytes: &mut usize,
    window_bytes: &mut usize,
    window: &mut Instant,
) -> Result<()> {
    match frame {
        Message::Binary(data) => {
            ensure!(data.len() <= MAX_FRAME_BYTES, "relay frame too large");
            if window.elapsed() >= Duration::from_secs(1) {
                *window = Instant::now();
                *window_bytes = 0;
            }
            if *window_bytes + data.len() > CIRCUIT_BYTES_PER_SECOND {
                tokio::time::sleep(Duration::from_secs(1).saturating_sub(window.elapsed())).await;
                *window = Instant::now();
                *window_bytes = 0;
            }
            *bytes += data.len();
            *window_bytes += data.len();
            ensure!(*bytes <= CIRCUIT_BYTE_LIMIT, "relay byte budget exceeded");
            send_frame(destination, Message::Binary(data)).await
        }
        Message::Ping(_) | Message::Pong(_) => Ok(()),
        _ => bail!("relay accepts only encrypted binary frames"),
    }
}
async fn dial_direct(
    urls: Vec<String>,
    target: &str,
    service: Service,
    failures: &HashMap<(String, String), Instant>,
    seen: &mut HashSet<String>,
) -> Option<NetworkSocket> {
    for url in urls
        .into_iter()
        .filter(|url| seen.insert(url.clone()))
        .take(MAX_URLS * 2)
    {
        let route = format!("direct:{url}");
        if failures.contains_key(&(target.to_owned(), route.clone())) {
            continue;
        }
        if let Ok(socket) = connect_service(&url, Control::Direct { service }).await {
            return Some(NetworkSocket::Routed(Box::new(socket), route));
        }
    }
    None
}

pub async fn connect_direct(url: &str, service: Service) -> Result<NetworkSocket> {
    connect_service(url, Control::Direct { service }).await
}

async fn connect_service(url: &str, control: Control) -> Result<NetworkSocket> {
    let (mut socket, _) = open_control(url).await?;
    send_control(&mut socket, &control).await?;
    ensure!(
        matches!(receive_control(&mut socket).await?, Control::Ready),
        "network path was not ready"
    );
    Ok(socket)
}
async fn open_control(url: &str) -> Result<(NetworkSocket, String)> {
    validate_url(url)?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES));
    let (socket, _) = tokio::time::timeout(
        CONNECT_TIMEOUT,
        connect_async_with_config(url, Some(config), false),
    )
    .await
    .context("network connection timed out")?
    .map_err(|_| anyhow::anyhow!("network connection failed"))?;
    let mut socket = NetworkSocket::Client(Box::new(socket));
    let Control::Welcome { nonce } = receive_control(&mut socket).await? else {
        bail!("network endpoint omitted welcome");
    };
    check_id(&nonce)?;
    Ok((socket, nonce))
}
async fn send_frame(socket: &mut NetworkSocket, frame: Message) -> Result<()> {
    tokio::time::timeout(CONNECT_TIMEOUT, socket.send(frame))
        .await
        .context("network send timed out")?
}
async fn send_control(socket: &mut NetworkSocket, control: &Control) -> Result<()> {
    let text = serde_json::to_string(control)?;
    ensure!(text.len() <= MAX_CONTROL_BYTES, "network control too large");
    send_frame(socket, Message::Text(text.into())).await
}
async fn receive_control(socket: &mut NetworkSocket) -> Result<Control> {
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        while let Some(frame) = socket.next().await {
            match frame? {
                Message::Text(text) => {
                    ensure!(text.len() <= MAX_CONTROL_BYTES, "network control too large");
                    return serde_json::from_str(&text).context("invalid network control");
                }
                Message::Ping(_) | Message::Pong(_) => {}
                _ => bail!("expected network control message"),
            }
        }
        bail!("network connection closed")
    })
    .await
    .context("network control timed out")?
}
fn validate_url(value: &str) -> Result<()> {
    ensure!(value.len() <= 2048, "network URL too long");
    let url = reqwest::Url::parse(value)?;
    ensure!(
        matches!(url.scheme(), "ws" | "wss")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.query().is_none(),
        "invalid network URL"
    );
    Ok(())
}
fn check_id(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid network identifier"
    );
    Ok(())
}
fn now_secs() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .try_into()?)
}
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        extract::{State as AxumState, WebSocketUpgrade},
        routing::get,
    };
    use tokio::{net::TcpListener, task::JoinSet};
    use vibeface_secure::Handshake;

    async fn serve(hub: Arc<NetworkHub>, tasks: &mut JoinSet<()>) -> Result<String> {
        async fn upgrade(
            AxumState(hub): AxumState<Arc<NetworkHub>>,
            ws: WebSocketUpgrade,
        ) -> impl axum::response::IntoResponse {
            ws.max_message_size(MAX_FRAME_BYTES)
                .max_frame_size(MAX_FRAME_BYTES)
                .on_upgrade(|socket| async move {
                    let _ = hub.accept(socket).await;
                })
        }
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}/api/network/ws", listener.local_addr()?);
        let router = Router::new()
            .route("/api/network/ws", get(upgrade))
            .with_state(hub);
        tasks.spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(url)
    }

    async fn next_binary(socket: &mut NetworkSocket) -> Result<Vec<u8>> {
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            while let Some(frame) = socket.next().await {
                match frame? {
                    Message::Binary(bytes) => return Ok(bytes.to_vec()),
                    Message::Ping(_) | Message::Pong(_) => {}
                    _ => bail!("expected encrypted binary frame"),
                }
            }
            bail!("missing encrypted binary frame")
        })
        .await
        .context("encrypted frame timed out")?
    }

    #[tokio::test]
    async fn signed_records_are_targeted_expiring_and_not_membership() -> Result<()> {
        let owner = LocalIdentity::generate();
        let (hub, _) = NetworkHub::new(owner.clone(), NetworkConfig::default());
        let record = hub.local_record().await?;
        record.verify(now_secs()?)?;
        let outsider = LocalIdentity::generate();
        let (directory, _) = NetworkHub::new(outsider, NetworkConfig::default());
        directory.publish(record.clone()).await?;
        assert_eq!(
            directory
                .lookup_local(&owner.node_id()?)
                .await?
                .unwrap()
                .signature,
            record.signature
        );
        assert!(directory.lookup_local(&"f".repeat(64)).await?.is_none());
        assert!(record.verify(record.expires_at).is_err());
        let mut changed = record.clone();
        changed
            .direct_urls
            .push("ws://127.0.0.1:9/api/network/ws".to_owned());
        assert!(changed.verify(now_secs()?).is_err());
        changed = record.clone();
        changed.node_id = "a".repeat(64);
        assert!(changed.verify(now_secs()?).is_err());
        assert!(validate_url("ws://user:password@example.com/api/network/ws").is_err());
        assert!(validate_url("file:///tmp/socket").is_err());
        assert!(validate_url("wss://relay.example/api/network/ws").is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn unrelated_relay_carries_native_noise_after_direct_failure() -> Result<()> {
        let mut tasks = JoinSet::new();
        let relay_identity = LocalIdentity::generate();
        let target_identity = LocalIdentity::generate();
        let caller_identity = LocalIdentity::generate();
        let (relay, _) = NetworkHub::new(
            relay_identity.clone(),
            NetworkConfig {
                relay_enabled: true,
                ..Default::default()
            },
        );
        let relay_url = serve(relay.clone(), &mut tasks).await?;
        let (target, mut target_incoming) = NetworkHub::new(
            target_identity.clone(),
            NetworkConfig {
                bootstrap_urls: vec![relay_url.clone()],
                advertise_urls: vec!["ws://127.0.0.1:1/api/network/ws".to_owned()],
                ..Default::default()
            },
        );
        let target_id = target_identity.node_id()?;
        let target_run = target.clone();
        tasks.spawn(async move {
            let _ = target_run.run("127.0.0.1:0".parse().unwrap()).await;
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if relay
                    .lookup_local(&target_id)
                    .await
                    .unwrap()
                    .is_some_and(|record| !record.relay_urls.is_empty())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let (caller, _) = NetworkHub::new(
            caller_identity.clone(),
            NetworkConfig {
                bootstrap_urls: vec![relay_url],
                ..Default::default()
            },
        );
        let mut caller_socket = caller.dial(&target_id, Service::Client).await?;
        let mut incoming = target_incoming
            .recv()
            .await
            .context("missing reverse relay socket")?;
        assert_eq!(incoming.service, Service::Client);
        assert_ne!(relay_identity.node_id()?, target_id);
        assert_ne!(relay_identity.node_id()?, caller_identity.node_id()?);

        let mut client = Handshake::new(
            true,
            &caller_identity.secret_key_hex,
            Some(target_id),
            b"test:relay",
        )
        .map_err(anyhow::Error::msg)?;
        let mut server = Handshake::new(
            false,
            &target_identity.secret_key_hex,
            Some(caller_identity.node_id()?),
            b"test:relay",
        )
        .map_err(anyhow::Error::msg)?;
        let first = client.write(&[]).map_err(anyhow::Error::msg)?;
        send_frame(&mut caller_socket, Message::Binary(first.into())).await?;
        let first = next_binary(&mut incoming.socket).await?;
        server.read(&first).map_err(anyhow::Error::msg)?;
        let second = server.write(&[]).map_err(anyhow::Error::msg)?;
        send_frame(&mut incoming.socket, Message::Binary(second.into())).await?;
        let second = next_binary(&mut caller_socket).await?;
        assert_eq!(client.read(&second).map_err(anyhow::Error::msg)?, b"");
        let third = client
            .write(b"authenticated caller")
            .map_err(anyhow::Error::msg)?;
        send_frame(&mut caller_socket, Message::Binary(third.into())).await?;
        let third = next_binary(&mut incoming.socket).await?;
        assert_eq!(
            server.read(&third).map_err(anyhow::Error::msg)?,
            b"authenticated caller"
        );
        let mut client = client.finish().map_err(anyhow::Error::msg)?;
        let mut server = server.finish().map_err(anyhow::Error::msg)?;
        let canary = b"private context canary across an unrelated relay";
        for encrypted in client.seal_message(canary).map_err(anyhow::Error::msg)? {
            assert!(
                !encrypted
                    .windows(canary.len())
                    .any(|window| window == canary)
            );
            send_frame(&mut caller_socket, Message::Binary(encrypted.into())).await?;
        }
        let encrypted = next_binary(&mut incoming.socket).await?;
        assert_eq!(
            server
                .open_record(&encrypted)
                .map_err(anyhow::Error::msg)?
                .unwrap(),
            canary
        );
        for encrypted in server
            .seal_message(b"complete")
            .map_err(anyhow::Error::msg)?
        {
            send_frame(&mut incoming.socket, Message::Binary(encrypted.into())).await?;
        }
        let encrypted = next_binary(&mut caller_socket).await?;
        assert_eq!(
            client
                .open_record(&encrypted)
                .map_err(anyhow::Error::msg)?
                .unwrap(),
            b"complete"
        );
        Ok(())
    }

    #[tokio::test]
    async fn direct_path_is_preferred_and_public_relay_is_opt_in() -> Result<()> {
        let mut tasks = JoinSet::new();
        let target_identity = LocalIdentity::generate();
        let (target, mut incoming) =
            NetworkHub::new(target_identity.clone(), NetworkConfig::default());
        let target_url = serve(target.clone(), &mut tasks).await?;
        let (caller, _) = NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        caller
            .state
            .lock()
            .await
            .candidates
            .insert(target_identity.node_id()?, vec![target_url.clone()]);
        let _socket = caller
            .dial(&target_identity.node_id()?, Service::Peer)
            .await?;
        assert_eq!(
            incoming
                .recv()
                .await
                .context("missing direct socket")?
                .service,
            Service::Peer
        );
        let (mut socket, nonce) = open_control(&target_url).await?;
        let identity = LocalIdentity::generate();
        let node_id = identity.node_id()?;
        let signature = identity.sign_payload(
            "relay-reserve",
            &serde_json::to_vec(&ReservationProof {
                nonce: &nonce,
                node_id: &node_id,
            })?,
        )?;
        send_control(
            &mut socket,
            &Control::Reserve {
                node_id,
                public_key: identity.public_key_hex()?,
                signature,
            },
        )
        .await?;
        assert!(receive_control(&mut socket).await.is_err());
        assert!(target.state.lock().await.reservations.is_empty());
        let mut rate = ControlRate::default();
        for _ in 0..30 {
            rate.charge(30)?;
        }
        assert!(rate.charge(30).is_err());
        Ok(())
    }
}
