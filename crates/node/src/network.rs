//! Public address discovery and bounded opaque tunnels. Discovery never grants membership.

mod admission;
use admission::{Admission, CircuitAdmission, SocketClass};
pub use admission::{Denied, SocketAdmission};

use crate::crypto::{LocalIdentity, fingerprint, verify_payload};
use anyhow::{Context, Result, bail, ensure};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{
    Sink, SinkExt, Stream, StreamExt,
    stream::{self, BoxStream, FuturesUnordered},
};
use mdns_sd::{DaemonEvent, DaemonStatus, IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{Arc, Mutex as StdMutex, MutexGuard},
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::TcpStream,
    sync::{Mutex, mpsc, oneshot, watch},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message as NativeMessage, protocol::WebSocketConfig},
};
use tracing::Instrument;

pub const MAX_FRAME_BYTES: usize = 65_535;
const MAX_CONTROL_BYTES: usize = 16 * 1024;
const MAX_ANNOUNCEMENT_BYTES: usize = 8 * 1024;
const MAX_CLUSTER_RECORDS: usize = 8;
const MAX_RECORDS: usize = 2048;
const MAX_RESOLVED: usize = 256;
const MAX_RESERVATIONS: usize = 64;
const MAX_GENERATION_HISTORY: usize = 1024;
const MAX_GENERATIONS_PER_MINUTE: usize = 6;
const GENERATION_INTERVAL: Duration = Duration::from_secs(1);
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
const MAX_MDNS_SOURCES: usize = 256;
const MDNS_VERIFY_INTERVAL: Duration = Duration::from_secs(1);
const MDNS_VERIFY_TIMEOUT: Duration = Duration::from_secs(2);
const MDNS_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const MDNS_COMMAND_RETRY: Duration = Duration::from_millis(25);
pub(crate) const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
const DIRECT_HEAD_START: Duration = Duration::from_millis(250);
const MAX_DIAL_ATTEMPTS: usize = 2;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    Peer,
    Client,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterAdvertisement {
    pub cluster_id: String,
    pub services: Vec<Service>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reachability {
    pub peer_id: String,
    pub public_key: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub cluster: Option<ClusterAdvertisement>,
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
        crate::identity::canonical_json(&record)
    }
    pub fn verify(&self, now: i64) -> Result<()> {
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_ANNOUNCEMENT_BYTES,
            "reachability announcement too large"
        );
        ensure!(
            self.peer_id == fingerprint(&self.public_key)?,
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
        if let Some(cluster) = &self.cluster {
            check_id(&cluster.cluster_id)?;
            ensure!(
                !cluster.services.is_empty()
                    && cluster.services.len() <= 2
                    && cluster.services.windows(2).all(|pair| pair[0] < pair[1]),
                "invalid cluster services"
            );
        }
        verify_payload(
            &self.public_key,
            "reachability-v2",
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
    LookupCluster {
        cluster_id: String,
        service: Service,
    },
    ClusterRecords {
        records: Vec<Reachability>,
        truncated: bool,
    },
    Reserve {
        peer_id: String,
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

    peer_id: &'a str,
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
    Server(Box<WebSocket>),
    Limited(Box<NetworkSocket>, SocketAdmission),
    Routed(Box<NetworkSocket>, String),
    Client(Box<WebSocketStream<MaybeTlsStream<TcpStream>>>),
}
impl NetworkSocket {
    pub fn server(socket: WebSocket) -> Self {
        Self::Server(Box::new(socket))
    }
    pub fn route(&self) -> Option<String> {
        match self {
            Self::Routed(_, route) => Some(route.clone()),
            Self::Limited(socket, _) => socket.route(),
            _ => None,
        }
    }
    fn promote(&mut self, class: SocketClass) -> Result<()> {
        let admission = self.admitted()?;
        admission.promote(class).inspect_err(|error| {
                tracing::debug!(source = ?admission.source(), ?class, reason = error.code(), "network admission rejected");
            }).map_err(Into::into)
    }

    fn admitted(&mut self) -> Result<&mut SocketAdmission> {
        match self {
            Self::Limited(_, admission) => Ok(admission),
            Self::Routed(socket, _) => socket.admitted(),
            _ => bail!("network socket has no admission"),
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
            Self::Server(socket) => Pin::new(&mut **socket)
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
            Self::Server(socket) => Pin::new(&mut **socket).poll_ready(cx).map_err(Into::into),
            Self::Client(socket) => Pin::new(&mut **socket).poll_ready(cx).map_err(Into::into),
        }
    }
    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<()> {
        match self.get_mut() {
            Self::Limited(socket, _) | Self::Routed(socket, _) => {
                Pin::new(&mut **socket).start_send(item)
            }
            Self::Server(socket) => Pin::new(&mut **socket).start_send(item).map_err(Into::into),
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
            Self::Server(socket) => Pin::new(&mut **socket).poll_flush(cx).map_err(Into::into),
            Self::Client(socket) => Pin::new(&mut **socket).poll_flush(cx).map_err(Into::into),
        }
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<()>> {
        match self.get_mut() {
            Self::Limited(socket, _) | Self::Routed(socket, _) => {
                Pin::new(&mut **socket).poll_close(cx)
            }
            Self::Server(socket) => Pin::new(&mut **socket).poll_close(cx).map_err(Into::into),
            Self::Client(socket) => Pin::new(&mut **socket).poll_close(cx).map_err(Into::into),
        }
    }
}

pub struct NetworkHub {
    identity: LocalIdentity,
    config: NetworkConfig,
    state: Mutex<State>,
    relay: Arc<StdMutex<RelayState>>,
    mdns: Arc<StdMutex<MdnsSources>>,
    incoming: mpsc::Sender<IncomingSocket>,
    admission: Arc<Admission>,
    relay_changes: watch::Sender<u64>,
    #[cfg(test)]
    test_hooks: StdMutex<TestHooks>,
}

#[cfg(test)]
#[derive(Default)]
struct TestHooks {
    publish_started: Option<oneshot::Sender<()>>,
    before_attachment_receive: Option<TestPause>,
    forward: Option<ForwardFixture>,
    mdns_started: Option<oneshot::Sender<(u64, ServiceDaemon)>>,
    mdns_fail: Option<oneshot::Receiver<()>>,
    mdns_closed: Option<oneshot::Sender<()>>,
    mdns_dropped: Option<oneshot::Sender<Result<DaemonStatus>>>,
}

#[cfg(test)]
struct TestPause {
    entered: oneshot::Sender<()>,
    resume: oneshot::Receiver<()>,
}

#[cfg(test)]
async fn test_pause(pause: Option<TestPause>) {
    if let Some(pause) = pause {
        let _ = pause.entered.send(());
        let _ = pause.resume.await;
    }
}
#[derive(Default)]
struct State {
    records: HashMap<String, Reachability>,
    resolved: HashMap<String, Reachability>,
    active_relays: HashSet<String>,
    paths: HashMap<(String, String), PathHistory>,
    local_cluster: Option<ClusterAdvertisement>,
    cluster_cursor: usize,
}

#[derive(Clone)]
struct MdnsSource {
    fullname: String,
    peer_id: String,
    urls: Vec<String>,
}

#[derive(Default)]
struct MdnsSources {
    epoch: u64,
    sources: HashMap<String, MdnsSource>,
    verify: VecDeque<String>,
}

fn mdns_lock(state: &StdMutex<MdnsSources>) -> MutexGuard<'_, MdnsSources> {
    state.lock().unwrap_or_else(|error| error.into_inner())
}

struct MdnsLease {
    state: Arc<StdMutex<MdnsSources>>,
    epoch: u64,
}

impl Drop for MdnsLease {
    fn drop(&mut self) {
        let mut sources = mdns_lock(&self.state);
        if sources.epoch == self.epoch {
            sources.reset("owner_stopped");
        }
    }
}

impl MdnsSources {
    fn reset(&mut self, reason: &str) -> u64 {
        self.epoch = self.epoch.wrapping_add(1);
        for (_, source) in self.sources.drain() {
            tracing::debug!(epoch = self.epoch, fullname = ?source.fullname, target = %source.peer_id, urls = ?source.urls, event = "removed", reason, "mdns source changed");
        }
        self.verify.clear();
        tracing::debug!(epoch = self.epoch, reason, "mdns daemon reset");
        self.epoch
    }

    fn resolved(&mut self, epoch: u64, info: &ServiceInfo, local_id: &str) -> bool {
        if epoch != self.epoch {
            return false;
        }
        let fullname = info.get_fullname();
        let key = fullname.to_ascii_lowercase();
        let target = info.get_property_val_str("peer_id").unwrap_or_default();
        let mut urls: Vec<_> = info
            .get_addresses()
            .iter()
            .filter(|ip| {
                !ip.is_unspecified()
                    && !ip.is_multicast()
                    && !matches!(ip, IpAddr::V6(ip) if ip.is_unicast_link_local())
            })
            .map(|ip| {
                format!(
                    "ws://{}/api/network/ws",
                    SocketAddr::new(*ip, info.get_port())
                )
            })
            .collect();
        urls.sort();
        urls.dedup();
        urls.truncate(MAX_URLS);
        if check_id(target).is_err()
            || target == local_id
            || info.get_port() == 0
            || urls.is_empty()
        {
            self.removed(epoch, fullname, "invalid_update");
            return false;
        }
        if !self.sources.contains_key(&key) {
            if self.sources.len() >= MAX_MDNS_SOURCES {
                tracing::debug!(
                    epoch,
                    ?fullname,
                    target,
                    reason = "source_capacity",
                    "mdns source rejected"
                );
                return false;
            }
            self.verify.push_back(key.clone());
        }
        tracing::debug!(
            epoch,
            ?fullname,
            target,
            ?urls,
            event = "resolved",
            reason = "dns",
            "mdns source changed"
        );
        self.sources.insert(
            key,
            MdnsSource {
                fullname: fullname.to_owned(),
                peer_id: target.to_owned(),
                urls,
            },
        );
        true
    }

    fn removed(&mut self, epoch: u64, fullname: &str, reason: &str) -> bool {
        if epoch != self.epoch {
            return false;
        }
        let key = fullname.to_ascii_lowercase();
        self.verify.retain(|entry| entry != &key);
        if let Some(source) = self.sources.remove(&key) {
            tracing::debug!(epoch, fullname = ?source.fullname, target = %source.peer_id, urls = ?source.urls, event = "removed", reason, "mdns source changed");
            true
        } else {
            false
        }
    }

    fn urls(&self, target: &str) -> Vec<String> {
        let mut urls: Vec<_> = self
            .sources
            .values()
            .filter(|source| source.peer_id == target)
            .flat_map(|source| source.urls.iter().cloned())
            .collect();
        urls.sort();
        urls.dedup();
        urls
    }

    fn next_verify(&mut self) -> Option<MdnsSource> {
        let key = self.verify.pop_front()?;
        let source = self.sources.get(&key)?.clone();
        self.verify.push_back(key);
        Some(source)
    }
}
struct Reservation {
    id: String,
    public_key: String,
    send: mpsc::Sender<Control>,
    source: IpAddr,
    authenticated_at: Instant,
    slot: u64,
    ready: bool,
    cancel: watch::Sender<bool>,
}
struct Attachment {
    public_key: String,
    created: Instant,
    socket: oneshot::Sender<NetworkSocket>,
    target: String,
    reservation_id: String,
}

#[derive(Default)]
struct RelayState {
    reservations: HashMap<String, Reservation>,
    attachments: HashMap<String, Attachment>,
    generations: HashMap<String, GenerationWindow>,
}

#[derive(Clone, Copy)]
struct GenerationWindow {
    start: Instant,
    last: Instant,
    count: usize,
}

impl RelayState {
    fn next_generation(&mut self, peer_id: &str, now: Instant) -> Result<GenerationWindow> {
        let active = &self.reservations;
        self.generations.retain(|id, history| {
            active.contains_key(id) || now.duration_since(history.last) < Duration::from_secs(60)
        });
        if let Some(history) = self.generations.get(peer_id) {
            ensure!(
                now.duration_since(history.last) >= GENERATION_INTERVAL,
                "reservation_generation_rate"
            );
            let mut next = *history;
            if now.duration_since(next.start) >= Duration::from_secs(60) {
                next.start = now;
                next.count = 0;
            }
            ensure!(
                next.count < MAX_GENERATIONS_PER_MINUTE,
                "reservation_generation_rate"
            );
            next.last = now;
            next.count += 1;
            Ok(next)
        } else {
            ensure!(
                self.generations.len() < MAX_GENERATION_HISTORY,
                "reservation_generation_history_full"
            );
            Ok(GenerationWindow {
                start: now,
                last: now,
                count: 1,
            })
        }
    }
}

fn relay_lock(state: &StdMutex<RelayState>) -> MutexGuard<'_, RelayState> {
    state.lock().unwrap_or_else(|error| error.into_inner())
}

fn generation_tag(id: &str) -> String {
    crate::crypto::digest(id.as_bytes())[..12].to_owned()
}

struct ReservationLease {
    state: Arc<StdMutex<RelayState>>,
    peer_id: String,
    id: String,
}

impl Drop for ReservationLease {
    fn drop(&mut self) {
        let mut state = relay_lock(&self.state);
        let current = state
            .reservations
            .get(&self.peer_id)
            .is_some_and(|record| record.id == self.id);
        if current {
            state.reservations.remove(&self.peer_id);
        }
        let before = state.attachments.len();
        state.attachments.retain(|_, attachment| {
            attachment.target != self.peer_id || attachment.reservation_id != self.id
        });
        let removed = before - state.attachments.len();
        drop(state);
        tracing::debug!(target = %self.peer_id, generation = %generation_tag(&self.id), current, removed_pending = removed, "relay reservation removed");
    }
}

struct PendingAttachment {
    state: Arc<StdMutex<RelayState>>,
    token: String,
    reservation_id: String,
}

impl Drop for PendingAttachment {
    fn drop(&mut self) {
        let mut state = relay_lock(&self.state);
        if state
            .attachments
            .get(&self.token)
            .is_some_and(|attachment| attachment.reservation_id == self.reservation_id)
        {
            state.attachments.remove(&self.token);
        }
    }
}

struct RelayCircuit {
    caller: NetworkSocket,
    target: Option<NetworkSocket>,
    pending: Option<PendingAttachment>,
    _admission: CircuitAdmission,
}
#[derive(Debug, PartialEq, Eq)]
enum PublishOutcome {
    Accepted,
    Full,
}
#[derive(Clone)]
struct PathHistory {
    authenticated_at: Option<Instant>,
    failed_until: Option<Instant>,
    updated_at: Instant,
}

pub(crate) enum AuthenticationFailure {
    Path(anyhow::Error),
    Terminal(anyhow::Error),
    Busy(anyhow::Error),
    SourceGone,
    Cancelled,
}

struct ActivePath {
    relay: bool,
    started_at: Instant,
    cancel: Option<oneshot::Sender<()>>,
}

#[derive(Clone)]
struct PathCandidate {
    url: String,
    relay: bool,
    explicit: bool,
    mdns: bool,
    record_expires_at: Option<i64>,
}
impl PathCandidate {
    fn route(&self) -> String {
        format!(
            "{}:{}",
            if self.relay { "relay" } else { "direct" },
            self.url
        )
    }
    fn live(&self, hub: &NetworkHub, target: &str) -> bool {
        self.explicit
            || self
                .record_expires_at
                .is_some_and(|expires| now_secs().is_ok_and(|now| expires > now))
            || (self.mdns && mdns_lock(&hub.mdns).urls(target).contains(&self.url))
    }
}

#[derive(Default)]
struct PathQueue {
    pending: Vec<PathCandidate>,
    tried: HashSet<String>,
    direct: usize,
    relay: usize,
}
impl PathQueue {
    fn insert(&mut self, candidate: PathCandidate) {
        let route = candidate.route();
        if self.tried.contains(&route) {
            return;
        }
        if let Some(existing) = self.pending.iter_mut().find(|other| other.route() == route) {
            existing.explicit |= candidate.explicit;
            existing.mdns |= candidate.mdns;
            existing.record_expires_at =
                existing.record_expires_at.max(candidate.record_expires_at);
            return;
        }
        let (used, max) = if candidate.relay {
            (&mut self.relay, MAX_URLS)
        } else {
            (&mut self.direct, MAX_URLS * 2)
        };
        if *used >= max {
            return;
        }
        *used += 1;
        self.pending.push(candidate);
    }
    fn record(&mut self, record: Reachability) {
        for (relay, urls) in [(false, record.direct_urls), (true, record.relay_urls)] {
            for url in urls {
                self.insert(PathCandidate {
                    url,
                    relay,
                    explicit: false,
                    mdns: false,
                    record_expires_at: Some(record.expires_at),
                });
            }
        }
    }
    fn pop(
        &mut self,
        history: &HashMap<String, PathHistory>,
        prefer_relay: bool,
    ) -> Option<PathCandidate> {
        let now = Instant::now();
        let index = self
            .pending
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                !history
                    .get(&candidate.route())
                    .is_some_and(|record| record.failed_until.is_some_and(|until| until > now))
            })
            .min_by_key(|(_, candidate)| {
                (
                    candidate.relay != prefer_relay,
                    std::cmp::Reverse(
                        history
                            .get(&candidate.route())
                            .and_then(|record| record.authenticated_at),
                    ),
                    std::cmp::Reverse(candidate.record_expires_at),
                    candidate.route(),
                )
            })
            .map(|(index, _)| index)?;
        let candidate = self.pending.remove(index);
        Some(candidate)
    }
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
                relay: Arc::new(StdMutex::new(RelayState::default())),
                mdns: Arc::new(StdMutex::new(MdnsSources::default())),
                incoming,
                admission: Arc::new(Admission::default()),
                relay_changes: watch::channel(0).0,
                #[cfg(test)]
                test_hooks: StdMutex::new(TestHooks::default()),
            }),
            receive,
        )
    }
    pub fn admit(&self, source: IpAddr) -> std::result::Result<SocketAdmission, Denied> {
        self.admission
            .socket(Some(source), SocketClass::Initial)
            .inspect_err(|error| {
                tracing::debug!(%source, reason = error.code(), "network admission rejected");
            })
    }

    fn outbound(&self, class: SocketClass) -> Result<SocketAdmission> {
        self.admission
            .socket(None, class)
            .inspect_err(|error| {
                tracing::debug!(
                    source = "local",
                    ?class,
                    reason = error.code(),
                    "network admission rejected"
                );
            })
            .map_err(Into::into)
    }

    pub async fn accept(
        self: Arc<Self>,
        socket: WebSocket,
        admission: SocketAdmission,
    ) -> Result<()> {
        let source = admission
            .source()
            .context("network socket has no TCP source")?;
        let mut socket = NetworkSocket::Limited(Box::new(NetworkSocket::server(socket)), admission);
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
                socket.promote(SocketClass::Data)?;
                send_control(&mut socket, &Control::Ready).await?;
                self.incoming
                    .try_send(IncomingSocket { service, socket })
                    .map_err(|_| anyhow::anyhow!("network service queue full"))?;
                Ok(())
            }
            Control::Publish { record } => {
                socket.promote(SocketClass::Control)?;
                ensure!(
                    self.publish(record).await? == PublishOutcome::Accepted,
                    "public address directory full"
                );
                send_control(&mut socket, &Control::Published).await
            }
            Control::Lookup { target } => {
                socket.promote(SocketClass::Control)?;
                check_id(&target)?;
                let record = self.lookup_local(&target).await?;
                send_control(&mut socket, &Control::Record { record }).await
            }
            Control::LookupCluster {
                cluster_id,
                service,
            } => {
                socket.promote(SocketClass::Control)?;
                check_id(&cluster_id)?;
                let response = self.lookup_cluster_local(&cluster_id, service).await?;
                send_control(&mut socket, &response).await
            }
            Control::Reserve {
                peer_id,
                public_key,
                signature,
            } => {
                ensure!(self.config.relay_enabled, "public relay is disabled");
                ensure!(
                    peer_id == fingerprint(&public_key)?,
                    "reservation identity mismatch"
                );
                verify_payload(
                    &public_key,
                    "relay-reserve",
                    &serde_json::to_vec(&ReservationProof {
                        nonce: &nonce,
                        peer_id: &peer_id,
                    })?,
                    &signature,
                )?;
                self.reservation(socket, peer_id, public_key).await
            }
            Control::Dial { target, service } => {
                check_id(&target)?;
                socket.promote(SocketClass::Data)?;
                if service == Service::Client && target == self.identity.peer_id()? {
                    send_control(&mut socket, &Control::Ready).await?;
                    return self
                        .incoming
                        .try_send(IncomingSocket { service, socket })
                        .map_err(|_| anyhow::anyhow!("network service queue full"));
                }
                ensure!(self.config.relay_enabled, "public relay is disabled");
                self.relay(socket, source, target, service).await
            }
            Control::Attach { token, signature } => {
                let (public_key, target, reservation_id) = {
                    let state = relay_lock(&self.relay);
                    let attachment = state
                        .attachments
                        .get(&token)
                        .context("unknown relay attachment")?;
                    ensure!(
                        attachment.created.elapsed() < CONNECT_TIMEOUT,
                        "expired relay attachment"
                    );
                    (
                        attachment.public_key.clone(),
                        attachment.target.clone(),
                        attachment.reservation_id.clone(),
                    )
                };
                verify_payload(
                    &public_key,
                    "relay-attach",
                    &serde_json::to_vec(&AttachmentProof {
                        nonce: &nonce,
                        token: &token,
                    })?,
                    &signature,
                )?;
                let mut state = relay_lock(&self.relay);
                let attachment = state
                    .attachments
                    .get(&token)
                    .context("unknown relay attachment")?;
                ensure!(
                    attachment.created.elapsed() < CONNECT_TIMEOUT,
                    "expired relay attachment"
                );
                if attachment.reservation_id != reservation_id
                    || !state.reservations.get(&target).is_some_and(|reservation| {
                        reservation.ready && reservation.id == reservation_id
                    })
                {
                    tracing::debug!(%target, generation = %generation_tag(&reservation_id), reason = "stale_generation", "relay attachment rejected");
                    bail!("stale relay attachment");
                }
                socket.promote(SocketClass::Data)?;
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

    async fn open(&self, url: &str, class: SocketClass) -> Result<(NetworkSocket, String)> {
        open_control_admitted(url, Some(self.outbound(class)?)).await
    }
    async fn connect_path(&self, url: &str, control: Control) -> Result<NetworkSocket> {
        let (mut socket, _) = self.open(url, SocketClass::Data).await?;
        send_control(&mut socket, &control).await?;
        ensure!(
            matches!(receive_control(&mut socket).await?, Control::Ready),
            "network path was not ready"
        );
        Ok(socket)
    }

    async fn lookup_local(&self, target: &str) -> Result<Option<Reachability>> {
        if target == self.identity.peer_id()? {
            return Ok(Some(self.local_record().await?));
        }
        let mut state = self.state.lock().await;
        let now = now_secs()?;
        state.records.retain(|_, r| r.expires_at > now);
        Ok(state.records.get(target).cloned())
    }
    pub async fn set_local_cluster(&self, cluster: Option<ClusterAdvertisement>) {
        let mut state = self.state.lock().await;
        if state.local_cluster != cluster {
            state.local_cluster = cluster;
            self.relay_changes
                .send_modify(|version| *version = version.wrapping_add(1));
        }
    }
    async fn lookup_cluster_local(&self, cluster_id: &str, service: Service) -> Result<Control> {
        let local = self.local_record().await?;
        let now = now_secs()?;
        let mut state = self.state.lock().await;
        state.records.retain(|_, record| record.expires_at > now);
        let matches = |record: &&Reachability| {
            record.cluster.as_ref().is_some_and(|cluster| {
                cluster.cluster_id == cluster_id && cluster.services.contains(&service)
            })
        };
        let mut candidates: Vec<_> = state
            .records
            .values()
            .filter(|record| record.peer_id != local.peer_id)
            .chain(std::iter::once(&local))
            .filter(matches)
            .cloned()
            .collect();
        candidates.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));
        if !candidates.is_empty() {
            let offset = state.cluster_cursor % candidates.len();
            candidates.rotate_left(offset);
            state.cluster_cursor = state.cluster_cursor.wrapping_add(1);
        }
        let mut response = Control::ClusterRecords {
            records: vec![],
            truncated: false,
        };
        for candidate in candidates {
            let Control::ClusterRecords { records, truncated } = &mut response else {
                unreachable!()
            };
            if records.len() == MAX_CLUSTER_RECORDS {
                *truncated = true;
                break;
            }
            records.push(candidate);
            if serde_json::to_vec(&response)?.len() > MAX_CONTROL_BYTES {
                let Control::ClusterRecords { records, truncated } = &mut response else {
                    unreachable!()
                };
                records.pop();
                *truncated = true;
                break;
            }
        }
        Ok(response)
    }
    async fn publish(&self, record: Reachability) -> Result<PublishOutcome> {
        record.verify(now_secs()?)?;
        #[cfg(test)]
        if let Some(started) = self.test_hooks.lock().unwrap().publish_started.take() {
            let _ = started.send(());
        }
        let mut state = self.state.lock().await;
        let now = now_secs()?;
        ensure!(
            record.expires_at > now,
            "reachability expired while waiting"
        );
        state.records.retain(|_, r| r.expires_at > now);
        if let Some(old) = state.records.get(&record.peer_id) {
            if record.issued_at < old.issued_at {
                return Ok(PublishOutcome::Accepted);
            }
        } else if state.records.len() >= MAX_RECORDS {
            return Ok(PublishOutcome::Full);
        }
        state.records.insert(record.peer_id.clone(), record);
        Ok(PublishOutcome::Accepted)
    }
    async fn remember(&self, record: Reachability) -> Result<bool> {
        record.verify(now_secs()?)?;
        let mut state = self.state.lock().await;
        let now = now_secs()?;
        ensure!(
            record.expires_at > now,
            "reachability expired while waiting"
        );
        state.resolved.retain(|_, r| r.expires_at > now);
        if let Some(old) = state.resolved.get(&record.peer_id) {
            if record.issued_at < old.issued_at {
                return Ok(false);
            }
        } else if state.resolved.len() >= MAX_RESOLVED {
            return Ok(false);
        }
        state.resolved.insert(record.peer_id.clone(), record);
        Ok(true)
    }
    async fn known_records(&self, target: &str) -> Result<Vec<Reachability>> {
        let mut records: Vec<_> = self.lookup_local(target).await?.into_iter().collect();
        let mut state = self.state.lock().await;
        let now = now_secs()?;
        state.resolved.retain(|_, r| r.expires_at > now);
        if let Some(record) = state.resolved.get(target) {
            records.push(record.clone());
        }
        records.retain(|r| r.expires_at > now);
        records.sort_by_key(|record| std::cmp::Reverse(record.issued_at));
        records.dedup_by(|a, b| a.signature == b.signature);
        Ok(records)
    }
    async fn local_record(&self) -> Result<Reachability> {
        let now = now_secs()?;
        let state = self.state.lock().await;
        let mut relay_urls: Vec<_> = state.active_relays.iter().cloned().collect();
        relay_urls.sort();
        let mut record = Reachability {
            peer_id: self.identity.peer_id()?,
            public_key: self.identity.public_key_hex()?,
            cluster: state.local_cluster.clone(),
            direct_urls: self.config.advertise_urls.clone(),
            relay_urls,
            issued_at: now,
            expires_at: now + RECORD_TTL_SECS,
            signature: String::new(),
        };
        record.signature = self
            .identity
            .sign_payload("reachability-v2", &record.unsigned()?)?;
        record.verify(now)?;
        Ok(record)
    }
    pub async fn resolve(&self, target: &str) -> Result<BoxStream<'_, Reachability>> {
        check_id(target)?;
        let known = self.known_records(target).await?;
        let mut sources = HashSet::new();
        let queries: FuturesUnordered<_> = self
            .config
            .bootstrap_urls
            .iter()
            .filter(|url| sources.insert((*url).clone()))
            .take(MAX_URLS)
            .map(|url| {
                let target = target.to_owned();
                async move {
                    let result = tokio::time::timeout(CONNECT_TIMEOUT, async {
                        let mut socket = self.open(url, SocketClass::Control).await?.0;
                        send_control(
                            &mut socket,
                            &Control::Lookup {
                                target: target.clone(),
                            },
                        )
                        .await?;
                        let Control::Record { record } = receive_control(&mut socket).await? else {
                            bail!("invalid directory reply");
                        };
                        if let Some(record) = &record {
                            ensure!(record.peer_id == target, "directory returned another node");
                            record.verify(now_secs()?)?;
                        }
                        Ok::<_, anyhow::Error>(record)
                    })
                    .await
                    .context("directory query timed out")
                    .and_then(|result| result);
                    (url, result)
                }
            })
            .collect();
        let remote = stream::unfold(
            (queries, target.to_owned()),
            move |(mut queries, target)| async move {
                while let Some((directory, result)) = queries.next().await {
                    match result {
                        Ok(Some(record)) => match self.remember(record.clone()).await {
                            Ok(cached) => {
                                tracing::debug!(directory, %target, outcome = "record", cached, "directory query completed");
                                return Some((record, (queries, target)));
                            }
                            Err(error) => {
                                tracing::debug!(directory, %target, outcome = "rejected", reason = %error, "directory query completed")
                            }
                        },
                        Ok(None) => {
                            tracing::debug!(directory, %target, outcome = "empty", "directory query completed")
                        }
                        Err(error) => {
                            tracing::debug!(directory, %target, outcome = "rejected", reason = %error, "directory query completed")
                        }
                    }
                }
                None
            },
        );
        let mut signatures = HashSet::new();
        Ok(stream::iter(known)
            .chain(remote)
            .filter_map(move |record| {
                futures_util::future::ready(
                    (now_secs().is_ok_and(|now| record.expires_at > now)
                        && signatures.insert(record.signature.clone()))
                    .then_some(record),
                )
            })
            .boxed())
    }
    pub(crate) async fn dial_authenticated<T, F, Fut>(
        &self,
        target: &str,
        service: Service,
        entry: Option<&str>,
        authenticate: F,
    ) -> Result<(NetworkSocket, T)>
    where
        F: Fn(NetworkSocket) -> Fut,
        Fut: Future<Output = std::result::Result<(NetworkSocket, T), AuthenticationFailure>>,
    {
        tokio::time::timeout(
            DIAL_TIMEOUT,
            self.race_paths(target, service, entry, authenticate),
        )
        .await
        .context("authenticated node dial deadline exceeded")?
    }

    async fn race_paths<T, F, Fut>(
        &self,
        target: &str,
        service: Service,
        entry: Option<&str>,
        authenticate: F,
    ) -> Result<(NetworkSocket, T)>
    where
        F: Fn(NetworkSocket) -> Fut,
        Fut: Future<Output = std::result::Result<(NetworkSocket, T), AuthenticationFailure>>,
    {
        check_id(target)?;
        let mut queue = PathQueue::default();
        if let Some(url) = entry {
            validate_url(url)?;
            queue.insert(PathCandidate {
                url: url.to_owned(),
                relay: true,
                explicit: true,
                mdns: false,
                record_expires_at: None,
            });
        }
        for url in mdns_lock(&self.mdns).urls(target) {
            queue.insert(PathCandidate {
                url,
                relay: false,
                explicit: false,
                mdns: true,
                record_expires_at: None,
            });
        }
        for mut record in self.known_records(target).await? {
            record.direct_urls.retain(|url| Some(url.as_str()) != entry);
            queue.record(record);
        }
        let history = {
            let mut state = self.state.lock().await;
            state
                .paths
                .retain(|_, record| record.updated_at.elapsed() < Duration::from_secs(300));
            state
                .paths
                .iter()
                .filter(|((node, _), _)| node == target)
                .map(|((_, route), record)| (route.clone(), record.clone()))
                .collect::<HashMap<_, _>>()
        };
        let mut records = self.resolve(target).await?;
        let mut records_done = false;
        let mut active = FuturesUnordered::new();
        let mut active_direct = 0;
        let mut active_relay = 0;
        let mut running: HashMap<String, ActivePath> = HashMap::new();
        let mut lead_until = None;
        let mut first_attempt = true;
        let mut last_busy = None;
        loop {
            let relay_waiting = queue.pending.iter().any(|candidate| {
                candidate.relay
                    && candidate.live(self, target)
                    && !history.get(&candidate.route()).is_some_and(|record| {
                        record
                            .failed_until
                            .is_some_and(|until| until > Instant::now())
                    })
            });
            if active.len() == MAX_DIAL_ATTEMPTS
                && active_relay == 0
                && relay_waiting
                && !running.values().any(|attempt| attempt.cancel.is_none())
                && let Some((route, attempt)) = running
                    .iter_mut()
                    .filter(|(_, attempt)| !attempt.relay)
                    .min_by_key(|(_, attempt)| attempt.started_at)
                && let Some(cancel) = attempt.cancel.take()
            {
                let _ = cancel.send(());
                tracing::debug!(
                    target,
                    route,
                    reason = "relay_slot",
                    "authenticated path cancellation requested"
                );
            }
            while active.len() < MAX_DIAL_ATTEMPTS
                && (active.is_empty()
                    || lead_until.is_none_or(|until| until <= tokio::time::Instant::now()))
            {
                let Some(candidate) = queue.pop(&history, active_direct > 0 && active_relay == 0)
                else {
                    break;
                };
                if !candidate.live(self, target) {
                    if candidate.relay {
                        queue.relay -= 1;
                    } else {
                        queue.direct -= 1;
                    }
                    continue;
                }
                queue.tried.insert(candidate.route());
                if first_attempt
                    && !candidate.relay
                    && history
                        .get(&candidate.route())
                        .is_some_and(|record| record.authenticated_at.is_some())
                {
                    lead_until = Some(tokio::time::Instant::now() + DIRECT_HEAD_START);
                }
                first_attempt = false;
                if candidate.relay {
                    active_relay += 1;
                } else {
                    active_direct += 1;
                }
                tracing::debug!(target, route=%candidate.route(), active=active.len() + 1, "authenticated path attempt started");
                let (cancel, cancelled) = oneshot::channel();
                running.insert(
                    candidate.route(),
                    ActivePath {
                        relay: candidate.relay,
                        started_at: Instant::now(),
                        cancel: Some(cancel),
                    },
                );
                let cancelled_candidate = candidate.clone();
                let authenticate = &authenticate;
                active.push(async move {
                    tokio::select! {
                        biased;
                        result = self.attempt_path(candidate, target, service, authenticate) => result,
                        _ = cancelled => (cancelled_candidate, Err(AuthenticationFailure::Cancelled)),
                    }
                });
            }
            if active.is_empty() && records_done {
                break;
            }
            tokio::select! {
                biased;
                result = active.next(), if !active.is_empty() => {
                    let (candidate, result) = result.context("path attempt missing")?;
                    running.remove(&candidate.route());
                    if candidate.relay { active_relay -= 1; } else { active_direct -= 1; }
                    if active.is_empty() { lead_until = None; }
                    match result {
                        Ok(winner) => {
                            self.record_path(target, &candidate.route(), true).await;
                            tracing::debug!(target, route=%candidate.route(), cancelled=active.len(), "authenticated path selected");
                            return Ok(winner);
                        }
                        Err(AuthenticationFailure::Terminal(error)) => return Err(error),
                        Err(AuthenticationFailure::Busy(error)) => last_busy = Some(error),
                        Err(AuthenticationFailure::SourceGone) => {
                            queue.tried.remove(&candidate.route());
                            if candidate.relay { queue.relay -= 1; } else { queue.direct -= 1; }
                        }
                        Err(AuthenticationFailure::Cancelled) => {
                            tracing::debug!(target, route=%candidate.route(), reason="relay_slot", "authenticated path cancelled");
                        }
                        Err(AuthenticationFailure::Path(error)) => {
                            self.record_path(target, &candidate.route(), false).await;
                            tracing::debug!(target, route=%candidate.route(), %error, "authenticated path rejected");
                            if candidate.explicit && candidate.relay {
                                queue.insert(PathCandidate { relay: false, ..candidate });
                            }
                        }
                    }
                }
                record = records.next(), if !records_done => {
                    match record {
                        Some(mut record) => {
                            record.direct_urls.retain(|url| Some(url.as_str()) != entry);
                            queue.record(record);
                        }
                        None => records_done = true
                    }
                }
                _ = tokio::time::sleep_until(lead_until.unwrap_or_else(tokio::time::Instant::now)), if lead_until.is_some() => {
                    lead_until = None;
                }
            }
        }
        if let Some(error) = last_busy {
            return Err(error);
        }
        bail!("node has no authenticated direct or relay path")
    }

    async fn attempt_path<T, F, Fut>(
        &self,
        candidate: PathCandidate,
        target: &str,
        service: Service,
        authenticate: &F,
    ) -> (
        PathCandidate,
        std::result::Result<(NetworkSocket, T), AuthenticationFailure>,
    )
    where
        F: Fn(NetworkSocket) -> Fut,
        Fut: Future<Output = std::result::Result<(NetworkSocket, T), AuthenticationFailure>>,
    {
        if !candidate.live(self, target) {
            return (candidate, Err(AuthenticationFailure::SourceGone));
        }
        let control = if candidate.relay {
            Control::Dial {
                target: target.to_owned(),
                service,
            }
        } else {
            Control::Direct { service }
        };
        let result = match self.connect_path(&candidate.url, control).await {
            Ok(socket) => {
                authenticate(NetworkSocket::Routed(Box::new(socket), candidate.route())).await
            }
            Err(error) if error.downcast_ref::<Denied>().is_some() => {
                Err(AuthenticationFailure::Busy(error))
            }
            Err(error) => Err(AuthenticationFailure::Path(error)),
        };
        (candidate, result)
    }

    async fn record_path(&self, target: &str, route: &str, authenticated: bool) {
        let now = Instant::now();
        let mut state = self.state.lock().await;
        let key = (target.to_owned(), route.to_owned());
        if !state.paths.contains_key(&key)
            && state.paths.len() >= 1024
            && let Some(oldest) = state
                .paths
                .iter()
                .min_by_key(|(_, record)| record.updated_at)
                .map(|(key, _)| key.clone())
        {
            state.paths.remove(&oldest);
        }
        let record = state.paths.entry(key).or_insert(PathHistory {
            authenticated_at: None,
            failed_until: None,
            updated_at: now,
        });
        record.updated_at = now;
        if authenticated {
            record.authenticated_at = Some(now);
            record.failed_until = None;
        } else {
            record.failed_until = Some(now + Duration::from_secs(30));
        }
    }

    async fn reservation(
        self: Arc<Self>,
        mut socket: NetworkSocket,
        peer_id: String,
        public_key: String,
    ) -> Result<()> {
        let id = random_token();
        let source = socket
            .admitted()?
            .source()
            .context("reservation has no TCP source")?;
        let slot = socket.admitted()?.id();
        let (send, mut receive) = mpsc::channel(16);
        let (cancel, mut cancelled) = watch::channel(false);
        let (mut pending, previous, old_cancel, _lease) = {
            let mut state = relay_lock(&self.relay);
            let previous = state.reservations.get(&peer_id).map(|old| {
                (
                    old.id.clone(),
                    old.slot,
                    old.ready,
                    old.cancel.clone(),
                    old.source,
                    old.authenticated_at,
                )
            });
            if let Some((_, _, ready, _, _, _)) = &previous {
                ensure!(*ready, "reservation_handoff_pending");
            } else {
                ensure!(
                    state.reservations.len() < MAX_RESERVATIONS,
                    "relay reservation capacity reached"
                );
            }
            let now = Instant::now();
            let history = state.next_generation(&peer_id, now)?;
            let pending = if let Some((_, old_slot, _, _, _, _)) = &previous {
                Some(socket.admitted()?.handoff(*old_slot)?)
            } else {
                socket.promote(SocketClass::Reservation)?;
                None
            };
            if let Some((old_id, _, _, _, _, _)) = &previous {
                state.attachments.retain(|_, attachment| {
                    attachment.target != peer_id || attachment.reservation_id != *old_id
                });
            }
            state.reservations.insert(
                peer_id.clone(),
                Reservation {
                    id: id.clone(),
                    public_key,
                    send,
                    source,
                    authenticated_at: now,
                    slot,
                    ready: false,
                    cancel,
                },
            );
            state.generations.insert(peer_id.clone(), history);
            let lease = ReservationLease {
                state: self.relay.clone(),
                peer_id: peer_id.clone(),
                id: id.clone(),
            };
            let old_cancel = previous.as_ref().map(|old| old.3.clone());
            (pending, previous, old_cancel, lease)
        };
        tracing::debug!(target = %peer_id, %source, generation = %generation_tag(&id), previous = ?previous.as_ref().map(|old| generation_tag(&old.0)), previous_source = ?previous.as_ref().map(|old| old.4), previous_authenticated_secs = ?previous.as_ref().map(|old| old.5.elapsed().as_secs()), "relay reservation installed");
        if let Some(cancel) = old_cancel {
            cancel.send_replace(true);
        }
        let serving = async {
            if let Some(ticket) = &mut pending {
                tokio::time::timeout(CONNECT_TIMEOUT, ticket.closed())
                    .await
                    .context("reservation handoff close timed out")??;
                socket.admitted()?.finish_handoff(ticket)?;
                tracing::debug!(target = %peer_id, generation = %generation_tag(&id), previous = ?previous.as_ref().map(|old| generation_tag(&old.0)), "relay reservation previous socket closed");
            }
            drop(pending.take());
            send_control(&mut socket, &Control::Reserved).await?;
            {
                let mut state = relay_lock(&self.relay);
                let reservation = state
                    .reservations
                    .get_mut(&peer_id)
                    .filter(|reservation| reservation.id == id)
                    .context("reservation replaced before ready")?;
                reservation.ready = true;
            }
            tracing::debug!(target = %peer_id, %source, generation = %generation_tag(&id), "relay reservation ready");
            let deadline = tokio::time::sleep(RESERVATION_LIFETIME);
            tokio::pin!(deadline);
            let mut heartbeat = tokio::time::interval(HEARTBEAT);
            let mut last_received = Instant::now();
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
                            Message::Ping(_) | Message::Pong(_) => {},
                            _ => bail!("invalid reservation frame"),
                        }
                    }
                }
            }
        };
        tokio::pin!(serving);
        tokio::select! {
            biased;
            _ = cancelled.changed() => bail!("reservation replaced"),
            result = &mut serving => result,
        }
    }

    async fn relay(
        &self,
        caller: NetworkSocket,
        source: IpAddr,
        target: String,
        service: Service,
    ) -> Result<()> {
        let admission = self.admission.circuit(source, &target).inspect_err(|error| {
            tracing::debug!(%source, %target, reason = error.code(), "network admission rejected");
        })?;
        let mut circuit = RelayCircuit {
            caller,
            target: None,
            pending: None,
            _admission: admission,
        };
        let token = random_token();
        let circuit_tag = generation_tag(&token);
        let (send, receive) = oneshot::channel();
        let generation = {
            let mut state = relay_lock(&self.relay);
            let reservation = state
                .reservations
                .get(&target)
                .filter(|reservation| reservation.ready)
                .context("target has no active relay reservation")?;
            let generation = reservation.id.clone();
            let public_key = reservation.public_key.clone();
            let commands = reservation.send.clone();
            state.attachments.insert(
                token.clone(),
                Attachment {
                    public_key,
                    created: Instant::now(),
                    socket: send,
                    target: target.clone(),
                    reservation_id: generation.clone(),
                },
            );
            circuit.pending = Some(PendingAttachment {
                state: self.relay.clone(),
                token: token.clone(),
                reservation_id: generation.clone(),
            });
            if commands
                .try_send(Control::Incoming { token, service })
                .is_err()
            {
                drop(state);
                bail!("target relay queue full");
            }
            generation
        };
        tracing::debug!(%target, %source, generation = %generation_tag(&generation), "relay incoming queued");
        #[cfg(test)]
        {
            let pause = self
                .test_hooks
                .lock()
                .unwrap()
                .before_attachment_receive
                .take();
            test_pause(pause).await;
        }
        circuit.target = Some(
            tokio::time::timeout(CONNECT_TIMEOUT, receive)
                .await
                .context("target relay connection timed out")?
                .context("target relay connection cancelled")?,
        );
        drop(circuit.pending.take());
        let RelayCircuit {
            caller,
            target: attached,
            ..
        } = &mut circuit;
        let attached = attached.as_mut().context("target relay socket missing")?;
        send_control(caller, &Control::Ready).await?;
        send_control(attached, &Control::Ready).await?;
        tracing::debug!(%target, %source, generation = %generation_tag(&generation), "relay circuit ready");
        let forward = RelayForward::default();
        #[cfg(test)]
        let forward = match self.test_hooks.lock().unwrap().forward.take() {
            Some(fixture) => RelayForward::fixture(fixture),
            None => forward,
        };
        let span = tracing::debug_span!(
            "relay_forward",
            circuit = %circuit_tag,
            target = %target,
            source = %source,
            generation = %generation_tag(&generation),
        );
        tokio::time::timeout(
            CIRCUIT_LIFETIME,
            forward_ciphertext(caller, attached, forward).instrument(span),
        )
        .await
        .context("relay circuit lifetime reached")?
    }

    pub async fn run(self: Arc<Self>, listen: SocketAddr) -> Result<()> {
        self.config.validate()?;
        let mut tasks = tokio::task::JoinSet::new();
        let mut entries = HashSet::new();
        for url in self
            .config
            .bootstrap_urls
            .iter()
            .filter(|url| entries.insert((*url).clone()))
        {
            let hub = self.clone();
            let publish_url = url.clone();
            tasks.spawn(async move {
                hub.maintain_publication(&publish_url).await;
            });
            let hub = self.clone();
            let url = url.clone();
            tasks.spawn(async move {
                loop {
                    if let Err(error) = hub.clone().maintain_relay(&url).await {
                        tracing::debug!(directory = %url, %error, "rendezvous reconnect pending");
                    }
                    hub.set_relay_active(&url, false).await;
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

    async fn set_relay_active(&self, url: &str, active: bool) {
        let changed = {
            let mut state = self.state.lock().await;
            if active {
                state.active_relays.insert(url.to_owned())
            } else {
                state.active_relays.remove(url)
            }
        };
        if changed {
            self.relay_changes
                .send_modify(|version| *version = version.wrapping_add(1));
        }
    }

    async fn maintain_publication(self: Arc<Self>, url: &str) {
        let mut changes = self.relay_changes.subscribe();
        let mut refresh = tokio::time::interval(Duration::from_secs(60));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = refresh.tick() => {},
                changed = changes.changed() => if changed.is_err() { return; },
            }
            let _version = *changes.borrow_and_update();
            match self.publish_at(url).await {
                Ok(()) => tracing::debug!(
                    directory = url,
                    outcome = "published",
                    "directory publication completed"
                ),
                Err(error) => {
                    tracing::debug!(directory = url, outcome = "rejected", reason = %error, "directory publication completed")
                }
            }
        }
    }

    async fn maintain_relay(self: Arc<Self>, url: &str) -> Result<()> {
        let (mut socket, nonce) = self.open(url, SocketClass::Reservation).await?;
        let peer_id = self.identity.peer_id()?;
        let signature = self.identity.sign_payload(
            "relay-reserve",
            &serde_json::to_vec(&ReservationProof {
                nonce: &nonce,
                peer_id: &peer_id,
            })?,
        )?;
        send_control(
            &mut socket,
            &Control::Reserve {
                peer_id,
                public_key: self.identity.public_key_hex()?,
                signature,
            },
        )
        .await?;
        ensure!(
            matches!(receive_control(&mut socket).await?, Control::Reserved),
            "relay rejected reservation"
        );
        self.set_relay_active(url, true).await;
        let result = async {
            let mut heartbeat = tokio::time::interval(HEARTBEAT);
            let mut last_received = Instant::now();
            loop {
                tokio::select! {
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
                                let admission = match self.outbound(SocketClass::Data) {
                                    Ok(admission) => admission,
                                    Err(error) => { tracing::debug!(directory = url, reason = %error, "relay incoming rejected"); continue; }
                                };
                                let hub = self.clone();
                                let url = url.to_owned();
                                tokio::spawn(async move {
                                    if let Err(error) = hub.attach(&url, token, service, admission).await { tracing::debug!(%error, "relay attachment failed"); }
                                });
                            }
                            _ => bail!("invalid relay reservation message"),
                        }
                    }
                }
            }
        }.await;
        self.set_relay_active(url, false).await;
        result
    }
    async fn publish_at(&self, url: &str) -> Result<()> {
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            let record = self.local_record().await?;
            let (mut socket, _) = self.open(url, SocketClass::Control).await?;
            send_control(&mut socket, &Control::Publish { record }).await?;
            ensure!(
                matches!(receive_control(&mut socket).await?, Control::Published),
                "directory publication failed"
            );
            Ok(())
        })
        .await
        .context("directory publication timed out")?
    }
    async fn attach(
        &self,
        url: &str,
        token: String,
        service: Service,
        admission: SocketAdmission,
    ) -> Result<()> {
        let (mut socket, nonce) = open_control_admitted(url, Some(admission)).await?;
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
        let peer_id = self.identity.peer_id()?;
        loop {
            let epoch = mdns_lock(&self.mdns).reset("start");
            let _lease = MdnsLease {
                state: self.mdns.clone(),
                epoch,
            };
            let daemon = match ServiceDaemon::new() {
                Ok(daemon) => MdnsGuard::new(daemon),
                Err(error) => {
                    tracing::debug!(epoch, %error, "mdns restart pending");
                    tokio::time::sleep(MDNS_RETRY_INTERVAL).await;
                    continue;
                }
            };
            #[cfg(test)]
            let mut daemon = daemon;
            #[cfg(test)]
            {
                daemon.completed = self.test_hooks.lock().unwrap().mdns_dropped.take();
            }
            let outcome = async {
                if listen.ip().is_unspecified() {
                    daemon.get().disable_interface(if listen.is_ipv4() { IfKind::IPv6 } else { IfKind::IPv4 })?;
                }
                let monitor = daemon.get().monitor()?;
                let events = daemon.get().browse(SERVICE_TYPE)?;
                if let Some(service) = mdns_service(&peer_id, listen)? {
                    tracing::debug!(epoch, %listen, fullname = ?service.get_fullname(), addr_auto = service.is_addr_auto(), addresses = ?service.get_addresses(), "mdns service registered");
                    daemon.get().register(service)?;
                }
                #[cfg(test)]
                if let Some(started) = self.test_hooks.lock().unwrap().mdns_started.take() {
                    let _ = started.send((epoch, daemon.get().clone()));
                }
                #[cfg(test)]
                let failure = self.test_hooks.lock().unwrap().mdns_fail.take();
                #[cfg(test)]
                let failure = async {
                    match failure {
                        Some(failure) => { let _ = failure.await; }
                        None => futures_util::future::pending().await,
                    }
                };
                #[cfg(not(test))]
                let failure = futures_util::future::pending::<()>();
                tokio::pin!(failure);
                let mut verify = tokio::time::interval(MDNS_VERIFY_INTERVAL);
                verify.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = &mut failure => bail!("mdns injected generation error"),
                        event = events.recv_async() => match event.context("mdns browse stopped")? {
                            ServiceEvent::ServiceResolved(info) => {
                                mdns_lock(&self.mdns).resolved(epoch, &info, &peer_id);
                            }
                            ServiceEvent::ServiceRemoved(_, fullname) => {
                                mdns_lock(&self.mdns).removed(epoch, &fullname, "dns");
                            }
                            _ => {}
                        },
                        event = monitor.recv_async() => match event.context("mdns monitor stopped")? {
                            DaemonEvent::IpAdd(ip) | DaemonEvent::IpDel(ip) => {
                                tracing::debug!(epoch, %ip, %listen, "mdns interface changed");
                                return Ok::<_, anyhow::Error>("interface_reset");
                            }
                            DaemonEvent::NameChange(change) => {
                                tracing::debug!(epoch, original = ?change.original, fullname = ?change.new_name, interface = ?change.intf_name, rr_type = %change.rr_type, "mdns name changed");
                            }
                            DaemonEvent::Error(error) => return Err(error.into()),
                            _ => {}
                        },
                        _ = verify.tick() => {
                            let source = mdns_lock(&self.mdns).next_verify();
                            if let Some(source) = source {
                                daemon.get().verify(source.fullname.clone(), MDNS_VERIFY_TIMEOUT)?;
                                tracing::debug!(epoch, fullname = ?source.fullname, target = %source.peer_id, urls = ?source.urls, timeout_ms = MDNS_VERIFY_TIMEOUT.as_millis(), "mdns source verify");
                            }
                        }
                    }
                }
            }.await;
            mdns_lock(&self.mdns).reset(outcome.as_ref().copied().unwrap_or("daemon_error"));
            daemon.shutdown().await?;
            #[cfg(test)]
            if let Some(closed) = self.test_hooks.lock().unwrap().mdns_closed.take() {
                let _ = closed.send(());
            }
            if let Err(error) = outcome {
                tracing::debug!(epoch, %error, "mdns restart pending");
                tokio::time::sleep(MDNS_RETRY_INTERVAL).await;
            }
        }
    }
}

struct MdnsGuard {
    daemon: Option<ServiceDaemon>,
    #[cfg(test)]
    force_again: usize,
    #[cfg(test)]
    completed: Option<oneshot::Sender<Result<DaemonStatus>>>,
}
impl MdnsGuard {
    fn new(daemon: ServiceDaemon) -> Self {
        Self {
            daemon: Some(daemon),
            #[cfg(test)]
            force_again: 0,
            #[cfg(test)]
            completed: None,
        }
    }

    fn get(&self) -> &ServiceDaemon {
        self.daemon.as_ref().expect("mdns daemon already shut down")
    }

    fn request_shutdown(&mut self) -> mdns_sd::Result<mdns_sd::Receiver<DaemonStatus>> {
        #[cfg(test)]
        if self.force_again > 0 {
            self.force_again -= 1;
            return Err(mdns_sd::Error::Again);
        }
        self.get().shutdown()
    }

    async fn shutdown(mut self) -> Result<()> {
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            loop {
                match self.request_shutdown() {
                    Ok(closed) => {
                        ensure!(
                            closed.recv_async().await? == DaemonStatus::Shutdown,
                            "mdns daemon did not shut down"
                        );
                        return Ok::<_, anyhow::Error>(());
                    }
                    Err(mdns_sd::Error::Again) => tokio::time::sleep(MDNS_COMMAND_RETRY).await,
                    Err(error) => return Err(error.into()),
                }
            }
        })
        .await
        .context("mdns shutdown timed out")??;
        self.daemon.take();
        tracing::debug!(mode = "owner", status = "shutdown", "mdns daemon cleanup");
        Ok(())
    }
}
impl Drop for MdnsGuard {
    fn drop(&mut self) {
        if self.daemon.is_none() {
            return;
        }
        let shutdown = self.request_shutdown();
        let daemon = self.daemon.take().expect("checked mdns daemon missing");
        #[cfg(test)]
        let completed = self.completed.take();
        match shutdown {
            Ok(closed) => {
                tracing::debug!(mode = "drop", status = "exit_queued", "mdns daemon cleanup");
                #[cfg(test)]
                if let Some(completed) = completed {
                    std::thread::spawn(move || {
                        let _ = completed
                            .send(closed.recv_timeout(CONNECT_TIMEOUT).map_err(Into::into));
                    });
                }
                #[cfg(not(test))]
                drop(closed);
            }
            Err(mdns_sd::Error::Again) => {
                let reaper = std::thread::Builder::new().name("mdns-cleanup".into()).spawn(move || {
                    let deadline = Instant::now() + CONNECT_TIMEOUT;
                    while Instant::now() < deadline {
                        match daemon.shutdown() {
                            Ok(closed) => {
                                let status = closed.recv_timeout(deadline.saturating_duration_since(Instant::now()));
                                if matches!(status, Ok(DaemonStatus::Shutdown)) {
                                    tracing::debug!(mode = "reaper", status = "shutdown", "mdns daemon cleanup");
                                } else {
                                    tracing::warn!(mode = "reaper", status = ?status, deadline_seconds = CONNECT_TIMEOUT.as_secs(), "mdns daemon exit unconfirmed");
                                }
                                #[cfg(test)]
                                if let Some(completed) = completed {
                                    let _ = completed.send(status.map_err(Into::into));
                                }
                                return;
                            }
                            Err(mdns_sd::Error::Again) => std::thread::sleep(MDNS_COMMAND_RETRY),
                            Err(error) => {
                                tracing::warn!(mode = "reaper", %error, "mdns daemon cleanup failed");
                                return;
                            }
                        }
                    }
                    tracing::warn!(mode = "reaper", "mdns daemon cleanup timed out");
                });
                if let Err(error) = reaper {
                    tracing::warn!(%error, "mdns cleanup thread could not start");
                }
            }
            Err(error) => tracing::warn!(mode = "drop", %error, "mdns daemon exit unconfirmed"),
        }
    }
}

fn mdns_service(peer_id: &str, listen: SocketAddr) -> Result<Option<ServiceInfo>> {
    check_id(peer_id)?;
    if listen.ip().is_loopback() {
        return Ok(None);
    }
    let properties = [("peer_id", peer_id), ("path", "/api/network/ws")];
    let address = if listen.ip().is_unspecified() {
        String::new()
    } else {
        listen.ip().to_string()
    };
    let service = ServiceInfo::new(
        SERVICE_TYPE,
        &peer_id[..32],
        &format!("vf-{}.local.", &peer_id[..16]),
        address.as_str(),
        listen.port(),
        &properties[..],
    )?;
    Ok(Some(if listen.ip().is_unspecified() {
        service.enable_addr_auto()
    } else {
        service
    }))
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
const RELAY_QUEUE_FRAMES: usize = 4;
const RELAY_FRAME_RATE: usize = 30_720;

#[derive(Clone, Copy, Debug, Default)]
struct BufferedFrames {
    frames: usize,
    bytes: usize,
}

#[derive(Clone, Copy)]
enum ForwardStage {
    Reader,
    Queue,
    Writer,
}

struct ForwardState {
    bytes: usize,
    window: Instant,
    window_bytes: usize,
    frame_rate: ControlRate,
    last_received: [Instant; 2],
    reader: [BufferedFrames; 2],
    queue: [BufferedFrames; 2],
    writer: [BufferedFrames; 2],
}

impl ForwardState {
    fn new(now: Instant) -> Self {
        Self {
            bytes: 0,
            window: now,
            window_bytes: 0,
            frame_rate: ControlRate::default(),
            last_received: [now; 2],
            reader: [BufferedFrames::default(); 2],
            queue: [BufferedFrames::default(); 2],
            writer: [BufferedFrames::default(); 2],
        }
    }

    fn receive(&mut self, direction: usize, frame: &Message, now: Instant) -> Result<()> {
        self.frame_rate.charge(RELAY_FRAME_RATE)?;
        match frame {
            Message::Binary(data) => {
                ensure!(data.len() <= MAX_FRAME_BYTES, "relay frame too large");
                let total = self
                    .bytes
                    .checked_add(data.len())
                    .context("relay byte overflow")?;
                ensure!(total <= CIRCUIT_BYTE_LIMIT, "relay byte budget exceeded");
                self.bytes = total;
            }
            Message::Ping(_) | Message::Pong(_) => {}
            _ => bail!("relay accepts only encrypted binary frames"),
        }
        self.last_received[direction] = now;
        Ok(())
    }

    fn rate(&mut self, bytes: usize, now: Instant) -> Option<Duration> {
        let elapsed = now.duration_since(self.window);
        if elapsed >= Duration::from_secs(1) {
            self.window = now;
            self.window_bytes = 0;
        }
        if self.window_bytes + bytes > CIRCUIT_BYTES_PER_SECOND {
            Some(Duration::from_secs(1).saturating_sub(elapsed))
        } else {
            self.window_bytes += bytes;
            None
        }
    }

    fn idle(&self, now: Instant, timeout: Duration) -> Option<usize> {
        self.last_received
            .iter()
            .position(|last| now.duration_since(*last) >= timeout)
    }

    fn buffered(&mut self, stage: ForwardStage, direction: usize) -> &mut BufferedFrames {
        match stage {
            ForwardStage::Reader => &mut self.reader[direction],
            ForwardStage::Queue => &mut self.queue[direction],
            ForwardStage::Writer => &mut self.writer[direction],
        }
    }
}

#[cfg(test)]
struct ForwardFixture {
    heartbeat: Duration,
    idle_timeout: Duration,
    send_timeout: Duration,
    observed: mpsc::UnboundedSender<ForwardObservation>,
}

#[cfg(test)]
#[derive(Debug)]
struct ForwardObservation {
    event: &'static str,
    direction: usize,
    reader: [BufferedFrames; 2],
    queue: [BufferedFrames; 2],
    writer: [BufferedFrames; 2],
    accepted_bytes: usize,
    window_bytes: usize,
}

struct RelayForward {
    state: StdMutex<ForwardState>,
    heartbeat: Duration,
    idle_timeout: Duration,
    send_timeout: Duration,
    #[cfg(test)]
    observed: Option<mpsc::UnboundedSender<ForwardObservation>>,
}

impl Default for RelayForward {
    fn default() -> Self {
        Self {
            state: StdMutex::new(ForwardState::new(Instant::now())),
            heartbeat: HEARTBEAT,
            idle_timeout: IDLE_TIMEOUT,
            send_timeout: CONNECT_TIMEOUT,
            #[cfg(test)]
            observed: None,
        }
    }
}

impl RelayForward {
    fn lock(&self) -> MutexGuard<'_, ForwardState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    #[cfg(test)]
    fn fixture(fixture: ForwardFixture) -> Self {
        Self {
            heartbeat: fixture.heartbeat,
            idle_timeout: fixture.idle_timeout,
            send_timeout: fixture.send_timeout,
            observed: Some(fixture.observed),
            ..Self::default()
        }
    }

    fn observe(&self, event: &'static str, direction: usize) {
        let (reader, queue, writer, accepted_bytes, window_bytes) = {
            let state = self.lock();
            (
                state.reader,
                state.queue,
                state.writer,
                state.bytes,
                state.window_bytes,
            )
        };
        tracing::debug!(
            event,
            direction = if direction == 0 {
                "caller_to_target"
            } else {
                "target_to_caller"
            },
            queued_frames = queue[direction].frames,
            queued_bytes = queue[direction].bytes,
            reader_held_bytes = reader[direction].bytes,
            writer_held_bytes = writer[direction].bytes,
            accepted_bytes,
            window_bytes,
            "relay flow"
        );
        #[cfg(test)]
        if let Some(observed) = &self.observed {
            let _ = observed.send(ForwardObservation {
                event,
                direction,
                reader,
                queue,
                writer,
                accepted_bytes,
                window_bytes,
            });
        }
    }
}

struct ForwardFrame<'a> {
    frame: Message,
    bytes: usize,
    direction: usize,
    stage: ForwardStage,
    forward: &'a RelayForward,
}

impl<'a> ForwardFrame<'a> {
    fn new(
        frame: Message,
        direction: usize,
        stage: ForwardStage,
        forward: &'a RelayForward,
    ) -> Self {
        let bytes = match &frame {
            Message::Binary(data) => data.len(),
            _ => 0,
        };
        let held = &mut *forward.lock();
        let buffered = held.buffered(stage, direction);
        buffered.frames += 1;
        buffered.bytes += bytes;
        Self {
            frame,
            bytes,
            direction,
            stage,
            forward,
        }
    }

    fn stage(&mut self, stage: ForwardStage) {
        let mut state = self.forward.lock();
        let previous = state.buffered(self.stage, self.direction);
        previous.frames -= 1;
        previous.bytes -= self.bytes;
        let next = state.buffered(stage, self.direction);
        next.frames += 1;
        next.bytes += self.bytes;
        self.stage = stage;
    }
}

impl Drop for ForwardFrame<'_> {
    fn drop(&mut self) {
        let mut state = self.forward.lock();
        let buffered = state.buffered(self.stage, self.direction);
        buffered.frames -= 1;
        buffered.bytes -= self.bytes;
    }
}

async fn forward_reader<'a>(
    mut source: impl Stream<Item = Result<Message>> + Unpin,
    destination: mpsc::Sender<ForwardFrame<'a>>,
    forward: &'a RelayForward,
    direction: usize,
) -> Result<()> {
    loop {
        let frame = source
            .next()
            .await
            .context("relay endpoint disconnected")??;
        forward.lock().receive(direction, &frame, Instant::now())?;
        if !matches!(frame, Message::Binary(_)) {
            continue;
        }
        let mut held = ForwardFrame::new(frame, direction, ForwardStage::Reader, forward);
        let permit = match destination.try_reserve() {
            Ok(permit) => permit,
            Err(mpsc::error::TrySendError::Full(_)) => {
                forward.observe("queue_full", direction);
                tokio::time::timeout(forward.send_timeout, destination.reserve())
                    .await
                    .context("relay queue backpressure timed out")?
                    .context("relay writer stopped")?
            }
            Err(mpsc::error::TrySendError::Closed(_)) => bail!("relay writer stopped"),
        };
        held.stage(ForwardStage::Queue);
        permit.send(held);
    }
}

async fn forward_writer(
    mut destination: impl Sink<Message, Error = anyhow::Error> + Unpin,
    mut receive: mpsc::Receiver<ForwardFrame<'_>>,
    forward: &RelayForward,
    direction: usize,
) -> Result<()> {
    loop {
        let mut frame = receive.recv().await.context("relay reader stopped")?;
        frame.stage(ForwardStage::Writer);
        let mut rate_waiting = false;
        loop {
            let wait = forward.lock().rate(frame.bytes, Instant::now());
            match wait {
                Some(wait) => {
                    rate_waiting = true;
                    forward.observe("rate_wait", direction);
                    tokio::time::sleep(wait).await;
                }
                None => break,
            }
        }
        if rate_waiting {
            forward.observe("rate_resumed", direction);
        }
        let message = std::mem::replace(&mut frame.frame, Message::Ping(Vec::new().into()));
        let sending = destination.send(message);
        tokio::pin!(sending);
        let mut blocked = false;
        tokio::time::timeout(
            forward.send_timeout,
            std::future::poll_fn(|cx| {
                let poll = std::future::Future::poll(sending.as_mut(), cx);
                if poll.is_pending() && !blocked {
                    blocked = true;
                    forward.observe("write_pending", direction);
                }
                poll
            }),
        )
        .await
        .context("relay write timed out")??;
        if blocked {
            forward.observe("write_resumed", direction);
        }
    }
}

async fn forward_ciphertext(
    a: &mut NetworkSocket,
    b: &mut NetworkSocket,
    forward: RelayForward,
) -> Result<()> {
    let (a_write, a_read) = a.split();
    let (b_write, b_read) = b.split();
    let (to_b, from_a) = mpsc::channel(RELAY_QUEUE_FRAMES);
    let (to_a, from_b) = mpsc::channel(RELAY_QUEUE_FRAMES);
    let monitor = async {
        let mut heartbeat = tokio::time::interval(forward.heartbeat);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            heartbeat.tick().await;
            let idle = forward.lock().idle(Instant::now(), forward.idle_timeout);
            if let Some(direction) = idle {
                forward.observe("idle", direction);
                bail!("relay endpoint idle");
            }
            for (direction, send) in [(0, &to_b), (1, &to_a)] {
                if let Ok(permit) = send.try_reserve() {
                    permit.send(ForwardFrame::new(
                        Message::Ping(Vec::new().into()),
                        direction,
                        ForwardStage::Queue,
                        &forward,
                    ));
                }
            }
        }
    };
    let result = tokio::select! {
        result = forward_reader(a_read, to_b.clone(), &forward, 0) => result,
        result = forward_reader(b_read, to_a.clone(), &forward, 1) => result,
        result = forward_writer(a_write, from_b, &forward, 1) => result,
        result = forward_writer(b_write, from_a, &forward, 0) => result,
        result = monitor => result,
    };
    forward.observe("closed", 0);
    forward.observe("closed", 1);
    result
}

pub async fn connect_direct(url: &str, service: Service) -> Result<NetworkSocket> {
    connect_service(url, Control::Direct { service }).await
}

pub async fn connect_relay(url: &str, target: &str, service: Service) -> Result<NetworkSocket> {
    check_id(target)?;
    connect_service(
        url,
        Control::Dial {
            target: target.to_owned(),
            service,
        },
    )
    .await
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
    open_control_admitted(url, None).await
}

async fn open_control_admitted(
    url: &str,
    admission: Option<SocketAdmission>,
) -> Result<(NetworkSocket, String)> {
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
    if let Some(admission) = admission {
        socket = NetworkSocket::Limited(Box::new(socket), admission);
    }
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
    use super::admission::{MAX_ESTABLISHED, MAX_SOCKETS};
    use super::*;
    use axum::{
        Router,
        extract::{ConnectInfo, State as AxumState, WebSocketUpgrade},
        http::StatusCode,
        response::{IntoResponse, Response},
        routing::get,
    };
    use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet};
    use vibeface_secure::Handshake;

    enum DirectoryObservation {
        Request(Control),
        Closed,
    }

    #[tokio::test]
    async fn client_target_connects_locally_without_enabling_public_relay() -> Result<()> {
        let identity = LocalIdentity::generate();
        let (target, mut incoming) = NetworkHub::new(identity.clone(), NetworkConfig::default());
        let mut tasks = JoinSet::new();
        let url = serve(target.clone(), &mut tasks).await?;
        let socket = connect_relay(&url, &identity.peer_id()?, Service::Client).await?;
        let accepted = incoming
            .recv()
            .await
            .context("local client was not delivered")?;
        assert_eq!(accepted.service, Service::Client);
        assert!(relay_lock(&target.relay).reservations.is_empty());
        assert!(relay_lock(&target.relay).attachments.is_empty());
        assert!(
            connect_relay(&url, &LocalIdentity::generate().peer_id()?, Service::Client)
                .await
                .is_err()
        );
        assert!(
            connect_relay(&url, &identity.peer_id()?, Service::Peer)
                .await
                .is_err()
        );
        drop(socket);
        drop(accepted);
        Ok(())
    }

    struct DataConnections {
        clients: Vec<NetworkSocket>,
        servers: Vec<NetworkSocket>,
        urls: Vec<String>,
    }

    fn seed_mdns_urls(hub: &NetworkHub, target: &str, urls: Vec<String>) {
        let mut sources = mdns_lock(&hub.mdns);
        for (index, chunk) in urls.chunks(MAX_URLS).enumerate() {
            let fullname = format!("fixture-{index}.{SERVICE_TYPE}");
            let key = fullname.to_ascii_lowercase();
            if !sources.sources.contains_key(&key) {
                sources.verify.push_back(key.clone());
            }
            sources.sources.insert(
                key,
                MdnsSource {
                    fullname,
                    peer_id: target.to_owned(),
                    urls: chunk.to_vec(),
                },
            );
        }
    }

    struct AcceptedTask {
        abort: tokio::task::AbortHandle,
        done: oneshot::Receiver<std::result::Result<(), String>>,
    }

    async fn hold_outbound_data(
        hub: &NetworkHub,
        count: usize,
        tasks: &mut JoinSet<()>,
    ) -> Result<DataConnections> {
        let mut held = DataConnections {
            clients: Vec::new(),
            servers: Vec::new(),
            urls: Vec::new(),
        };
        while held.clients.len() < count {
            let (sink, mut incoming) =
                NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
            let url = serve(sink, tasks).await?;
            for _ in 0..16.min(count - held.clients.len()) {
                let client = hub
                    .connect_path(
                        &url,
                        Control::Direct {
                            service: Service::Client,
                        },
                    )
                    .await?;
                let server = incoming
                    .recv()
                    .await
                    .context("sink did not accept data socket")?;
                held.clients.push(client);
                held.servers.push(server.socket);
            }
            held.urls.push(url);
        }
        Ok(held)
    }

    async fn serve(hub: Arc<NetworkHub>, tasks: &mut JoinSet<()>) -> Result<String> {
        Ok(serve_at(hub, "127.0.0.1:0", tasks).await?.0)
    }

    async fn serve_at(
        hub: Arc<NetworkHub>,
        address: &str,
        tasks: &mut JoinSet<()>,
    ) -> Result<(String, mpsc::UnboundedReceiver<Denied>)> {
        serve_observed(hub, address, tasks, None).await
    }

    async fn serve_observed(
        hub: Arc<NetworkHub>,
        address: &str,
        tasks: &mut JoinSet<()>,
        observed: Option<mpsc::UnboundedSender<AcceptedTask>>,
    ) -> Result<(String, mpsc::UnboundedReceiver<Denied>)> {
        type Ingress = (
            Arc<NetworkHub>,
            mpsc::UnboundedSender<Denied>,
            Option<mpsc::UnboundedSender<AcceptedTask>>,
        );
        async fn upgrade(
            AxumState((hub, denied, observed)): AxumState<Ingress>,
            ConnectInfo(peer): ConnectInfo<SocketAddr>,
            ws: WebSocketUpgrade,
        ) -> Response {
            let admission = match hub.admit(peer.ip()) {
                Ok(admission) => admission,
                Err(error) => {
                    let _ = denied.send(error);
                    return (
                        if error.rate() {
                            StatusCode::TOO_MANY_REQUESTS
                        } else {
                            StatusCode::SERVICE_UNAVAILABLE
                        },
                        error.code(),
                    )
                        .into_response();
                }
            };
            ws.max_message_size(MAX_FRAME_BYTES)
                .max_frame_size(MAX_FRAME_BYTES)
                .on_upgrade(|socket| async move {
                    let accept = tokio::spawn(hub.accept(socket, admission));
                    let (done, completed) = oneshot::channel();
                    if let Some(observed) = observed {
                        let _ = observed.send(AcceptedTask {
                            abort: accept.abort_handle(),
                            done: completed,
                        });
                    }
                    let result = accept.await;
                    if let Ok(Err(error)) = &result
                        && let Some(reason) = error.downcast_ref::<Denied>()
                    {
                        let _ = denied.send(*reason);
                    }
                    let _ = done.send(match result {
                        Ok(result) => result.map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    });
                })
        }
        let (denied, receive) = mpsc::unbounded_channel();
        let listener = TcpListener::bind(address).await?;
        let url = format!("ws://{}/api/network/ws", listener.local_addr()?);
        let router = Router::new()
            .route("/api/network/ws", get(upgrade))
            .with_state((hub, denied, observed));
        tasks.spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        Ok((url, receive))
    }

    async fn serve_directory(
        record: Option<Reachability>,
        gate: Option<Arc<Semaphore>>,
        tasks: &mut JoinSet<()>,
    ) -> Result<(String, mpsc::UnboundedReceiver<DirectoryObservation>)> {
        #[derive(Clone)]
        struct Directory {
            record: Option<Reachability>,
            gate: Option<Arc<Semaphore>>,
            observed: mpsc::UnboundedSender<DirectoryObservation>,
        }
        async fn upgrade(
            AxumState(directory): AxumState<Directory>,
            ws: WebSocketUpgrade,
        ) -> impl axum::response::IntoResponse {
            ws.on_upgrade(|socket| async move {
                let _ = async {
                    let mut socket = NetworkSocket::server(socket);
                    send_control(
                        &mut socket,
                        &Control::Welcome {
                            nonce: random_token(),
                        },
                    )
                    .await?;
                    let control = receive_control(&mut socket).await?;
                    let _ = directory.observed.send(DirectoryObservation::Request(control));
                    let _held = match &directory.gate {
                        Some(gate) => Some(loop {
                            tokio::select! {
                                permit = gate.acquire() => break permit?,
                                frame = socket.next() => match frame {
                                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => {
                                        let _ = directory.observed.send(DirectoryObservation::Closed);
                                        return Ok::<_, anyhow::Error>(());
                                    }
                                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {},
                                    _ => bail!("unexpected gated directory frame"),
                                }
                            }
                        }),
                        None => None,
                    };
                    send_control(
                        &mut socket,
                        &Control::Record {
                            record: directory.record,
                        },
                    )
                    .await
                }
                .await;
            })
        }
        let (observed, receive) = mpsc::unbounded_channel();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}/api/network/ws", listener.local_addr()?);
        let router = Router::new()
            .route("/api/network/ws", get(upgrade))
            .route("/api/network/ws/{*variant}", get(upgrade))
            .with_state(Directory {
                record,
                gate,
                observed,
            });
        tasks.spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok((url, receive))
    }

    async fn serve_raw_sockets(
        hub: Arc<NetworkHub>,
        tasks: &mut JoinSet<()>,
    ) -> Result<(String, mpsc::UnboundedReceiver<NetworkSocket>)> {
        async fn upgrade(
            AxumState((hub, sockets)): AxumState<(
                Arc<NetworkHub>,
                mpsc::UnboundedSender<NetworkSocket>,
            )>,
            ConnectInfo(peer): ConnectInfo<SocketAddr>,
            ws: WebSocketUpgrade,
        ) -> Response {
            let admission = hub.admit(peer.ip()).unwrap();
            ws.on_upgrade(|socket| async move {
                let _ = sockets.send(NetworkSocket::Limited(
                    Box::new(NetworkSocket::server(socket)),
                    admission,
                ));
            })
        }
        let (send, receive) = mpsc::unbounded_channel();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}/api/network/ws", listener.local_addr()?);
        let router = Router::new()
            .route("/api/network/ws", get(upgrade))
            .with_state((hub, send));
        tasks.spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        Ok((url, receive))
    }

    fn signed_record(identity: &LocalIdentity, issued_at: i64) -> Result<Reachability> {
        let mut record = Reachability {
            peer_id: identity.peer_id()?,
            public_key: identity.public_key_hex()?,
            cluster: None,
            direct_urls: Vec::new(),
            relay_urls: Vec::new(),
            issued_at,
            expires_at: issued_at + RECORD_TTL_SECS,
            signature: String::new(),
        };
        record.signature = identity.sign_payload("reachability-v2", &record.unsigned()?)?;
        Ok(record)
    }

    #[tokio::test]
    async fn cluster_directory_rotates_and_bounds_signed_candidates() -> Result<()> {
        let (hub, _) = NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        let cluster_id = "ab".repeat(32);
        let advertisement = ClusterAdvertisement {
            cluster_id: cluster_id.clone(),
            services: vec![Service::Peer, Service::Client],
        };
        hub.set_local_cluster(Some(advertisement.clone())).await;
        assert_eq!(
            hub.local_record().await?.cluster,
            Some(advertisement.clone())
        );
        hub.set_local_cluster(None).await;
        for _ in 0..10 {
            let identity = LocalIdentity::generate();
            let mut record = signed_record(&identity, now_secs()?)?;
            record.cluster = Some(advertisement.clone());
            record.signature = identity.sign_payload("reachability-v2", &record.unsigned()?)?;
            assert_eq!(hub.publish(record).await?, PublishOutcome::Accepted);
        }
        let Control::ClusterRecords {
            records: first,
            truncated,
        } = hub
            .lookup_cluster_local(&cluster_id, Service::Client)
            .await?
        else {
            unreachable!()
        };
        assert!(truncated);
        assert_eq!(first.len(), MAX_CLUSTER_RECORDS);
        let Control::ClusterRecords { records: next, .. } = hub
            .lookup_cluster_local(&cluster_id, Service::Client)
            .await?
        else {
            unreachable!()
        };
        assert_ne!(first[0].peer_id, next[0].peer_id);
        assert!(
            matches!(hub.lookup_cluster_local(&"cd".repeat(32), Service::Client).await?, Control::ClusterRecords { records, truncated: false } if records.is_empty())
        );
        hub.state.lock().await.records.clear();
        for _ in 0..3 {
            let identity = LocalIdentity::generate();
            let mut record = signed_record(&identity, now_secs()?)?;
            record.cluster = Some(advertisement.clone());
            record.direct_urls = vec![format!("wss://example.test/{}", "x".repeat(1750)); 4];
            record.signature = identity.sign_payload("reachability-v2", &record.unsigned()?)?;
            assert_eq!(hub.publish(record).await?, PublishOutcome::Accepted);
        }
        let response = hub
            .lookup_cluster_local(&cluster_id, Service::Client)
            .await?;
        assert!(serde_json::to_vec(&response)?.len() <= MAX_CONTROL_BYTES);
        assert!(
            matches!(response, Control::ClusterRecords { records, truncated: true } if records.len() == 2)
        );
        let identity = LocalIdentity::generate();
        let mut oversized = signed_record(&identity, now_secs()?)?;
        oversized.direct_urls = vec![format!("wss://example.test/{}", "x".repeat(1750)); 8];
        oversized.signature = identity.sign_payload("reachability-v2", &oversized.unsigned()?)?;
        assert!(hub.publish(oversized).await.is_err());
        let mut forged = first[0].clone();
        forged.cluster.as_mut().unwrap().cluster_id = "ef".repeat(32);
        assert!(forged.verify(now_secs()?).is_err());
        Ok(())
    }

    async fn serve_publication_probe(
        gate: Arc<Semaphore>,
        tasks: &mut JoinSet<()>,
    ) -> Result<(String, mpsc::UnboundedReceiver<Reachability>)> {
        #[derive(Clone)]
        struct Probe {
            gate: Arc<Semaphore>,
            observed: mpsc::UnboundedSender<Reachability>,
        }
        async fn upgrade(
            AxumState(probe): AxumState<Probe>,
            ws: WebSocketUpgrade,
        ) -> impl axum::response::IntoResponse {
            ws.on_upgrade(|socket| async move {
                let _ = async {
                    let mut socket = NetworkSocket::server(socket);
                    send_control(
                        &mut socket,
                        &Control::Welcome {
                            nonce: random_token(),
                        },
                    )
                    .await?;
                    match receive_control(&mut socket).await? {
                        Control::Publish { record } => {
                            record.verify(now_secs()?)?;
                            let _ = probe.observed.send(record);
                            let _permit = probe.gate.acquire().await?;
                            send_control(&mut socket, &Control::Published).await
                        }
                        Control::Reserve { .. } => Ok(()),
                        Control::Lookup { .. } => {
                            send_control(&mut socket, &Control::Record { record: None }).await
                        }
                        _ => bail!("unexpected publication probe control"),
                    }
                }
                .await;
            })
        }
        let (observed, receive) = mpsc::unbounded_channel();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}/api/network/ws", listener.local_addr()?);
        let router = Router::new()
            .route("/api/network/ws", get(upgrade))
            .with_state(Probe { gate, observed });
        tasks.spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok((url, receive))
    }

    async fn reserve_at(url: &str, identity: &LocalIdentity) -> Result<NetworkSocket> {
        let (mut socket, nonce) = open_control(url).await?;
        send_reserve(&mut socket, &nonce, identity).await?;
        ensure!(
            matches!(receive_control(&mut socket).await?, Control::Reserved),
            "reservation rejected"
        );
        Ok(socket)
    }

    async fn send_reserve(
        socket: &mut NetworkSocket,
        nonce: &str,
        identity: &LocalIdentity,
    ) -> Result<()> {
        let peer_id = identity.peer_id()?;
        let signature = identity.sign_payload(
            "relay-reserve",
            &serde_json::to_vec(&ReservationProof {
                nonce,
                peer_id: &peer_id,
            })?,
        )?;
        send_control(
            socket,
            &Control::Reserve {
                peer_id,
                public_key: identity.public_key_hex()?,
                signature,
            },
        )
        .await
    }

    async fn open_from(url: &str, source: IpAddr) -> Result<(NetworkSocket, String)> {
        open_with_receive_buffer(url, source, None).await
    }

    async fn open_with_receive_buffer(
        url: &str,
        source: IpAddr,
        receive_bytes: Option<u32>,
    ) -> Result<(NetworkSocket, String)> {
        let parsed = reqwest::Url::parse(url)?;
        let address = SocketAddr::new(
            parsed
                .host_str()
                .context("test URL missing IP")?
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse()?,
            parsed.port().context("test URL missing port")?,
        );
        let tcp = if source.is_ipv4() {
            tokio::net::TcpSocket::new_v4()?
        } else {
            tokio::net::TcpSocket::new_v6()?
        };
        tcp.bind(SocketAddr::new(source, 0))?;
        if let Some(bytes) = receive_bytes {
            tcp.set_recv_buffer_size(bytes)?;
        }
        let stream = tcp.connect(address).await?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES));
        let (socket, _) = tokio_tungstenite::client_async_with_config(
            url,
            MaybeTlsStream::Plain(stream),
            Some(config),
        )
        .await?;
        let mut socket = NetworkSocket::Client(Box::new(socket));
        let Control::Welcome { nonce } = receive_control(&mut socket).await? else {
            bail!("test endpoint omitted welcome");
        };
        Ok((socket, nonce))
    }

    async fn assert_socket_closed(socket: &mut NetworkSocket) -> Result<()> {
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            while let Some(frame) = socket.next().await {
                match frame {
                    Err(_) | Ok(Message::Close(_)) => return Ok(()),
                    Ok(Message::Ping(_) | Message::Pong(_)) => {}
                    _ => bail!("closed socket delivered application data"),
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    fn assert_admission_empty(hub: &NetworkHub) {
        let state = hub.admission.snapshot();
        assert_eq!(
            (
                state.total,
                state.short,
                state.long,
                state.held_long,
                state.promotions,
                state.slots,
                state.circuits,
            ),
            (0, 0, 0, 0, 0, 0, 0)
        );
        assert!(state.targets.is_empty());
    }

    async fn dial_pending(url: &str, target: &str) -> Result<NetworkSocket> {
        let (mut socket, _) = open_control(url).await?;
        send_control(
            &mut socket,
            &Control::Dial {
                target: target.to_owned(),
                service: Service::Client,
            },
        )
        .await?;
        Ok(socket)
    }

    async fn incoming_token(reservation: &mut NetworkSocket) -> Result<String> {
        let Control::Incoming { token, .. } = receive_control(reservation).await? else {
            bail!("reservation did not deliver Incoming");
        };
        Ok(token)
    }

    async fn attach_pending(
        url: &str,
        identity: &LocalIdentity,
        token: &str,
    ) -> Result<NetworkSocket> {
        let (mut socket, nonce) = open_control(url).await?;
        send_attach(&mut socket, &nonce, identity, token).await?;
        Ok(socket)
    }

    async fn send_attach(
        socket: &mut NetworkSocket,
        nonce: &str,
        identity: &LocalIdentity,
        token: &str,
    ) -> Result<()> {
        let signature = identity.sign_payload(
            "relay-attach",
            &serde_json::to_vec(&AttachmentProof { nonce, token })?,
        )?;
        send_control(
            socket,
            &Control::Attach {
                token: token.to_owned(),
                signature,
            },
        )
        .await
    }

    #[test]
    fn a2_generation_history_preserves_the_interval_across_a_disconnected_window_boundary() {
        let start = Instant::now();
        let last = start + Duration::from_millis(59_900);
        let mut state = RelayState::default();
        state.generations.insert(
            "disconnected".into(),
            GenerationWindow {
                start,
                last,
                count: MAX_GENERATIONS_PER_MINUTE,
            },
        );
        assert!(
            state
                .next_generation("disconnected", start + Duration::from_secs(60))
                .is_err()
        );
        assert_eq!(state.generations["disconnected"].last, last);
        let resumed = state
            .next_generation("disconnected", last + GENERATION_INTERVAL)
            .unwrap();
        assert_eq!(resumed.count, 1);
        assert_eq!(resumed.start, last + GENERATION_INTERVAL);
        state.generations.insert("disconnected".into(), resumed);
        assert!(
            state
                .next_generation("disconnected", resumed.last + Duration::from_secs(59))
                .is_ok()
        );
        let _ = state
            .next_generation("unrelated", resumed.last + Duration::from_secs(60))
            .unwrap();
        assert!(!state.generations.contains_key("disconnected"));
    }

    fn fixture_reservation(id: String, slot: u64, now: Instant) -> Reservation {
        Reservation {
            id,
            public_key: String::new(),
            send: mpsc::channel(1).0,
            source: IpAddr::from([127, 0, 0, 1]),
            authenticated_at: now,
            slot,
            ready: true,
            cancel: watch::channel(false).0,
        }
    }

    #[test]
    fn a2_generation_history_is_bounded_without_excluding_an_active_identity() {
        let start = Instant::now();
        let mut state = RelayState::default();
        for index in 0..MAX_GENERATION_HISTORY {
            state.generations.insert(
                index.to_string(),
                GenerationWindow {
                    start,
                    last: start,
                    count: 1,
                },
            );
        }
        state.reservations.insert(
            "0".into(),
            fixture_reservation("generation".into(), 0, start),
        );
        assert!(
            state
                .next_generation("new", start + GENERATION_INTERVAL)
                .is_err()
        );
        assert_eq!(state.generations.len(), MAX_GENERATION_HISTORY);
        assert_eq!(
            state
                .next_generation("0", start + GENERATION_INTERVAL)
                .unwrap()
                .count,
            2
        );
        let now = start + Duration::from_secs(61);
        assert_eq!(state.next_generation("0", now).unwrap().count, 1);
        assert_eq!(state.generations.len(), 1);
        state.reservations.clear();
        assert!(state.next_generation("new", now).is_ok());
        assert!(state.generations.is_empty());
        state.generations.insert(
            "limited".into(),
            GenerationWindow {
                start,
                last: start + Duration::from_secs(5),
                count: MAX_GENERATIONS_PER_MINUTE,
            },
        );
        assert!(
            state
                .next_generation("limited", start + Duration::from_secs(6))
                .is_err()
        );
    }

    #[tokio::test]
    async fn a2_new_challenge_preserves_ready_circuit_during_blocked_directory_publication()
    -> Result<()> {
        let mut tasks = JoinSet::new();
        let (relay, _) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                relay_enabled: true,
                ..Default::default()
            },
        );
        let (observed, mut accepts) = mpsc::unbounded_channel();
        let (url, _) =
            serve_observed(relay.clone(), "127.0.0.1:0", &mut tasks, Some(observed)).await?;
        let identity = LocalIdentity::generate();
        let target = identity.peer_id()?;
        let mut old = reserve_at(&url, &identity).await?;
        let old_task = accepts.recv().await.context("missing old accept task")?;
        let old_id = relay_lock(&relay.relay).reservations[&target].id.clone();
        let mut ready_caller = dial_pending(&url, &target).await?;
        let ready_task = accepts.recv().await.context("missing circuit task")?;
        let token = incoming_token(&mut old).await?;
        let mut ready_target = attach_pending(&url, &identity, &token).await?;
        let attach_task = accepts.recv().await.context("missing attach task")?;
        assert!(attach_task.done.await?.is_ok());
        assert!(matches!(
            receive_control(&mut ready_caller).await?,
            Control::Ready
        ));
        assert!(matches!(
            receive_control(&mut ready_target).await?,
            Control::Ready
        ));
        let mut pending_caller = dial_pending(&url, &target).await?;
        let pending_task = accepts.recv().await.context("missing pending task")?;
        let old_token = incoming_token(&mut old).await?;
        let (started, publish_started) = oneshot::channel();
        relay.test_hooks.lock().unwrap().publish_started = Some(started);
        let held_directory = relay.state.lock().await;
        let (mut publication, _) = open_control(&url).await?;
        let publication_task = accepts.recv().await.context("missing publication task")?;
        send_control(
            &mut publication,
            &Control::Publish {
                record: signed_record(&identity, now_secs()?)?,
            },
        )
        .await?;
        tokio::time::timeout(CONNECT_TIMEOUT, publish_started).await??;
        tokio::time::sleep(GENERATION_INTERVAL).await;
        let checkpoint = Instant::now();
        let mut current = reserve_at(&url, &identity).await?;
        let current_task = accepts.recv().await.context("missing new accept task")?;
        assert!(old_task.done.await?.unwrap_err().contains("replaced"));
        assert_socket_closed(&mut old).await?;
        assert!(checkpoint.elapsed() < CONNECT_TIMEOUT);
        let current_id = {
            let state = relay_lock(&relay.relay);
            let current = &state.reservations[&target];
            assert!(current.ready);
            assert_ne!(current.id, old_id);
            assert!(state.attachments.is_empty());
            assert_eq!(state.generations[&target].count, 2);
            current.id.clone()
        };
        assert!(pending_task.done.await?.is_err());
        assert_socket_closed(&mut pending_caller).await?;
        let mut stale = attach_pending(&url, &identity, &old_token).await?;
        let stale_task = accepts.recv().await.context("missing stale attach task")?;
        assert!(stale_task.done.await?.is_err());
        assert_socket_closed(&mut stale).await?;
        assert!(!held_directory.records.contains_key(&target));
        drop(held_directory);
        assert!(matches!(
            receive_control(&mut publication).await?,
            Control::Published
        ));
        assert!(publication_task.done.await?.is_ok());
        let caller_identity = LocalIdentity::generate();
        noise_round_trip(
            &caller_identity,
            &identity,
            &mut ready_caller,
            &mut ready_target,
        )
        .await?;
        let mut new_caller = dial_pending(&url, &target).await?;
        let new_circuit_task = accepts.recv().await.context("missing new circuit task")?;
        let token = incoming_token(&mut current).await?;
        let mut new_target = attach_pending(&url, &identity, &token).await?;
        let new_attach_task = accepts.recv().await.context("missing new attach task")?;
        assert!(new_attach_task.done.await?.is_ok());
        assert!(matches!(
            receive_control(&mut new_caller).await?,
            Control::Ready
        ));
        assert!(matches!(
            receive_control(&mut new_target).await?,
            Control::Ready
        ));
        noise_round_trip(
            &caller_identity,
            &identity,
            &mut new_caller,
            &mut new_target,
        )
        .await?;
        assert_eq!(
            relay_lock(&relay.relay).reservations[&target].id,
            current_id
        );
        current_task.abort.abort();
        assert!(current_task.done.await?.unwrap_err().contains("cancelled"));
        ready_task.abort.abort();
        new_circuit_task.abort.abort();
        assert!(ready_task.done.await?.is_err());
        assert!(new_circuit_task.done.await?.is_err());
        assert_admission_empty(&relay);
        assert!(relay_lock(&relay.relay).reservations.is_empty());
        eprintln!(
            "NET1_A2 independent_publish_completed=true old_pending_rejected=true old_ready_noise=true new_generation_noise=true all_active=0"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a2_outer_abort_drops_an_attached_socket_waiting_in_the_receiver() -> Result<()> {
        let mut tasks = JoinSet::new();
        let (relay, _) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                relay_enabled: true,
                ..Default::default()
            },
        );
        let (observed, mut accepts) = mpsc::unbounded_channel();
        let (url, _) =
            serve_observed(relay.clone(), "127.0.0.1:0", &mut tasks, Some(observed)).await?;
        let identity = LocalIdentity::generate();
        let target = identity.peer_id()?;
        let mut reservation = reserve_at(&url, &identity).await?;
        let reservation_task = accepts.recv().await.context("missing reservation task")?;
        let (entered, paused) = oneshot::channel();
        let (resume, proceed) = oneshot::channel();
        relay.test_hooks.lock().unwrap().before_attachment_receive = Some(TestPause {
            entered,
            resume: proceed,
        });
        let mut caller = dial_pending(&url, &target).await?;
        let caller_task = accepts.recv().await.context("missing caller task")?;
        tokio::time::timeout(CONNECT_TIMEOUT, paused).await??;
        let token = incoming_token(&mut reservation).await?;
        let mut attached = attach_pending(&url, &identity, &token).await?;
        let attached_task = accepts.recv().await.context("missing attached task")?;
        assert!(attached_task.done.await?.is_ok());
        assert!(!resume.is_closed(), "relay already consumed the attachment");
        assert_eq!(relay.admission.snapshot().circuits, 1);
        assert_eq!(relay.admission.snapshot().long, 3);
        caller_task.abort.abort();
        assert!(caller_task.done.await?.unwrap_err().contains("cancelled"));
        assert!(resume.is_closed());
        assert_socket_closed(&mut caller).await?;
        assert_socket_closed(&mut attached).await?;
        assert!(relay_lock(&relay.relay).attachments.is_empty());
        let snapshot = relay.admission.snapshot();
        assert_eq!(
            (snapshot.total, snapshot.long, snapshot.circuits),
            (1, 1, 0)
        );
        assert!(snapshot.targets.is_empty());
        let mut pending = dial_pending(&url, &target).await?;
        let pending_task = accepts.recv().await.context("missing pending task")?;
        let _ = incoming_token(&mut reservation).await?;
        assert_eq!(relay_lock(&relay.relay).attachments.len(), 1);
        reservation_task.abort.abort();
        assert!(
            reservation_task
                .done
                .await?
                .unwrap_err()
                .contains("cancelled")
        );
        assert!(pending_task.done.await?.is_err());
        assert_socket_closed(&mut reservation).await?;
        assert_socket_closed(&mut pending).await?;
        assert!(relay_lock(&relay.relay).reservations.is_empty());
        assert!(relay_lock(&relay.relay).attachments.is_empty());
        assert_admission_empty(&relay);
        eprintln!(
            "NET1_A2 abort_stage=attach_in_oneshot_before_receive caller_eof=true target_eof=true circuit_released=true reservation_abort_pending_clean=true all_active=0"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a2_reserved_send_failure_drops_the_lease_and_handoff_ticket() -> Result<()> {
        let mut tasks = JoinSet::new();
        let (relay, _) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                relay_enabled: true,
                ..Default::default()
            },
        );
        let (observed, mut accepts) = mpsc::unbounded_channel();
        let (url, _) =
            serve_observed(relay.clone(), "127.0.0.1:0", &mut tasks, Some(observed)).await?;
        let identity = LocalIdentity::generate();
        let target = identity.peer_id()?;
        let mut old = reserve_at(&url, &identity).await?;
        let old_task = accepts.recv().await.context("missing old task")?;
        let (raw_url, mut receive) = serve_raw_sockets(relay.clone(), &mut tasks).await?;
        let (client, _) = tokio_tungstenite::connect_async(&raw_url).await?;
        let mut client = NetworkSocket::Client(Box::new(client));
        let mut server = receive.recv().await.context("raw upgrade missing")?;
        send_frame(&mut server, Message::Close(None)).await?;
        tokio::time::sleep(GENERATION_INTERVAL).await;
        let error = relay
            .clone()
            .reservation(server, target.clone(), identity.public_key_hex()?)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("Sending after closing"),
            "{error:?}"
        );
        assert!(old_task.done.await?.is_err());
        assert_socket_closed(&mut old).await?;
        assert_socket_closed(&mut client).await?;
        assert!(relay_lock(&relay.relay).reservations.is_empty());
        assert!(relay_lock(&relay.relay).attachments.is_empty());
        assert_admission_empty(&relay);
        let mut healthy = reserve_at(&url, &LocalIdentity::generate()).await?;
        let healthy_task = accepts.recv().await.context("missing healthy task")?;
        assert_eq!(relay.admission.snapshot().long, 1);
        healthy_task.abort.abort();
        assert!(healthy_task.done.await?.is_err());
        assert_socket_closed(&mut healthy).await?;
        assert_admission_empty(&relay);
        eprintln!(
            "NET1_A2 local_send_failure=SendAfterClosing entry=reservation_after_authentication handoff_lease_ticket_released=true capacity_reused=true"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a2_real_full_source_and_global_handoff_uses_a_full_logical_table_fixture() -> Result<()>
    {
        let mut tasks = JoinSet::new();
        let (relay, mut incoming) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                relay_enabled: true,
                ..Default::default()
            },
        );
        let (observed, mut accepts) = mpsc::unbounded_channel();
        let (url, mut denied) =
            serve_observed(relay.clone(), "127.0.0.1:0", &mut tasks, Some(observed)).await?;
        let identity = LocalIdentity::generate();
        let target = identity.peer_id()?;
        let mut old = reserve_at(&url, &identity).await?;
        let old_task = accepts.recv().await.context("missing old task")?;
        let mut clients = Vec::new();
        let mut applications = Vec::new();
        for _ in 0..15 {
            clients.push(connect_direct(&url, Service::Client).await?);
            applications.push(
                incoming
                    .recv()
                    .await
                    .context("missing application socket")?,
            );
            assert!(
                accepts
                    .recv()
                    .await
                    .context("missing direct task")?
                    .done
                    .await?
                    .is_ok()
            );
        }
        let held = hold_outbound_data(&relay, MAX_ESTABLISHED - 16, &mut tasks).await?;
        {
            let mut state = relay_lock(&relay.relay);
            for index in 1..MAX_RESERVATIONS {
                state.reservations.insert(
                    format!("logical-fixture-{index}"),
                    fixture_reservation(format!("fixture-generation-{index}"), 0, Instant::now()),
                );
            }
            assert_eq!(state.reservations.len(), MAX_RESERVATIONS);
        }
        assert_eq!(relay.admission.snapshot().long, MAX_ESTABLISHED);
        assert_eq!(
            relay.admission.snapshot().sources[&IpAddr::from([127, 0, 0, 1])].long,
            16
        );
        assert!(reserve_at(&url, &LocalIdentity::generate()).await.is_err());
        let full_task = accepts.recv().await.context("missing logical full task")?;
        assert!(
            full_task
                .done
                .await?
                .unwrap_err()
                .contains("reservation capacity")
        );
        let mut controls = Vec::new();
        for index in 0..16 {
            controls.push(
                relay
                    .open(&held.urls[index / 4], SocketClass::Control)
                    .await?
                    .0,
            );
        }
        assert_eq!(relay.admission.snapshot().total, MAX_SOCKETS);
        assert!(open_control(&url).await.is_err());
        assert_eq!(denied.recv().await, Some(Denied::GlobalSocket));
        drop(controls.pop());
        let (mut replacement, nonce) = open_control(&url).await?;
        let replacement_task = accepts.recv().await.context("missing replacement task")?;
        assert_eq!(relay.admission.snapshot().total, MAX_SOCKETS);
        tokio::time::sleep(GENERATION_INTERVAL).await;
        send_reserve(&mut replacement, &nonce, &identity).await?;
        assert!(matches!(
            receive_control(&mut replacement).await?,
            Control::Reserved
        ));
        assert!(old_task.done.await?.is_err());
        assert_socket_closed(&mut old).await?;
        let snapshot = relay.admission.snapshot();
        assert_eq!(
            (
                snapshot.total,
                snapshot.long,
                snapshot.held_long,
                snapshot.promotions
            ),
            (127, 112, 0, 0)
        );
        assert_eq!(snapshot.sources[&IpAddr::from([127, 0, 0, 1])].long, 16);
        assert_eq!(
            relay_lock(&relay.relay).reservations.len(),
            MAX_RESERVATIONS
        );
        assert!(relay_lock(&relay.relay).reservations[&target].ready);
        replacement_task.abort.abort();
        assert!(replacement_task.done.await?.is_err());
        drop(controls);
        drop(held);
        drop(clients);
        drop(applications);
        assert_admission_empty(&relay);
        relay_lock(&relay.relay).reservations.clear();
        eprintln!(
            "NET1_A2 real_established=112 real_source_established=16 actual_peak_sockets=128 logical_table_fixture=64 handoff_recovered=true all_active=0"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a2_cross_ip_handoff_preserves_the_old_generation_when_the_new_source_is_full()
    -> Result<()> {
        let mut tasks = JoinSet::new();
        let (relay, mut incoming) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                relay_enabled: true,
                ..Default::default()
            },
        );
        let (observed, mut accepts) = mpsc::unbounded_channel();
        let (v4, _) = serve_observed(
            relay.clone(),
            "127.0.0.1:0",
            &mut tasks,
            Some(observed.clone()),
        )
        .await?;
        let (v6, _) = serve_observed(relay.clone(), "[::1]:0", &mut tasks, Some(observed)).await?;
        let identity = LocalIdentity::generate();
        let target = identity.peer_id()?;
        let mut old = reserve_at(&v4, &identity).await?;
        let old_task = accepts.recv().await.context("missing old task")?;
        let old_id = relay_lock(&relay.relay).reservations[&target].id.clone();
        let source: IpAddr = "::1".parse()?;
        let mut clients = Vec::new();
        let mut applications = Vec::new();
        for _ in 0..16 {
            let (mut socket, _) = open_from(&v6, source).await?;
            send_control(
                &mut socket,
                &Control::Direct {
                    service: Service::Client,
                },
            )
            .await?;
            assert!(matches!(
                receive_control(&mut socket).await?,
                Control::Ready
            ));
            clients.push(socket);
            applications.push(incoming.recv().await.context("missing IPv6 application")?);
            assert!(
                accepts
                    .recv()
                    .await
                    .context("missing direct task")?
                    .done
                    .await?
                    .is_ok()
            );
        }
        tokio::time::sleep(GENERATION_INTERVAL).await;
        let (mut refused, nonce) = open_from(&v6, source).await?;
        send_reserve(&mut refused, &nonce, &identity).await?;
        let refused_task = accepts.recv().await.context("missing refused task")?;
        assert!(
            refused_task
                .done
                .await?
                .unwrap_err()
                .contains("source_established_socket")
        );
        assert_socket_closed(&mut refused).await?;
        assert_eq!(relay_lock(&relay.relay).reservations[&target].id, old_id);
        assert_eq!(relay_lock(&relay.relay).generations[&target].count, 1);
        assert!(!old_task.abort.is_finished());
        let (mut invalid, _) = open_control(&v4).await?;
        send_control(
            &mut invalid,
            &Control::Reserve {
                peer_id: target.clone(),
                public_key: identity.public_key_hex()?,
                signature: "00".into(),
            },
        )
        .await?;
        assert!(
            accepts
                .recv()
                .await
                .context("missing invalid task")?
                .done
                .await?
                .is_err()
        );
        assert_socket_closed(&mut invalid).await?;
        assert_eq!(relay_lock(&relay.relay).generations[&target].count, 1);
        let unrelated_identity = LocalIdentity::generate();
        let mut unrelated = reserve_at(&v4, &unrelated_identity).await?;
        let unrelated_task = accepts.recv().await.context("missing unrelated task")?;
        assert_eq!(relay_lock(&relay.relay).generations[&target].count, 1);
        assert_eq!(
            relay_lock(&relay.relay).generations[&unrelated_identity.peer_id()?].count,
            1
        );
        drop(clients.pop());
        drop(applications.pop());
        let (mut current, nonce) = open_from(&v6, source).await?;
        send_reserve(&mut current, &nonce, &identity).await?;
        let current_task = accepts.recv().await.context("missing current task")?;
        assert!(matches!(
            receive_control(&mut current).await?,
            Control::Reserved
        ));
        assert!(old_task.done.await?.is_err());
        assert_socket_closed(&mut old).await?;
        let current_id = relay_lock(&relay.relay).reservations[&target].id.clone();
        assert_ne!(current_id, old_id);
        assert_eq!(
            relay_lock(&relay.relay).reservations[&target].source,
            source
        );
        assert_eq!(relay.admission.snapshot().sources[&source].long, 16);
        assert_eq!(relay_lock(&relay.relay).generations[&target].count, 2);
        let (mut rapid, nonce) = open_control(&v4).await?;
        send_reserve(&mut rapid, &nonce, &identity).await?;
        let rapid_task = accepts.recv().await.context("missing rapid task")?;
        assert!(
            rapid_task
                .done
                .await?
                .unwrap_err()
                .contains("reservation_generation_rate")
        );
        assert_socket_closed(&mut rapid).await?;
        assert_eq!(relay_lock(&relay.relay).generations[&target].count, 2);
        assert_eq!(
            relay_lock(&relay.relay).reservations[&target].id,
            current_id
        );
        current_task.abort.abort();
        unrelated_task.abort.abort();
        assert!(current_task.done.await?.is_err());
        assert!(unrelated_task.done.await?.is_err());
        assert_socket_closed(&mut current).await?;
        assert_socket_closed(&mut unrelated).await?;
        drop(clients);
        drop(applications);
        assert_admission_empty(&relay);
        eprintln!(
            "NET1_A2 cross_ip_full_preserves_old=true released_new_source_handoff=true failed_challenge_not_charged=true unrelated_identity_independent=true rapid_generation_rejected=true all_active=0"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a2_pending_generation_cannot_back_a_third_with_a_held_old_owner_fixture() -> Result<()>
    {
        for finish in [true, false] {
            let mut tasks = JoinSet::new();
            let (relay, _) = NetworkHub::new(
                LocalIdentity::generate(),
                NetworkConfig {
                    relay_enabled: true,
                    ..Default::default()
                },
            );
            let (observed, mut accepts) = mpsc::unbounded_channel();
            let (url, _) =
                serve_observed(relay.clone(), "127.0.0.1:0", &mut tasks, Some(observed)).await?;
            let identity = LocalIdentity::generate();
            let target = identity.peer_id()?;
            let (raw_url, mut sockets) = serve_raw_sockets(relay.clone(), &mut tasks).await?;
            let (client, _) = tokio_tungstenite::connect_async(&raw_url).await?;
            let mut old_client = NetworkSocket::Client(Box::new(client));
            let mut old_owner = sockets.recv().await.context("missing held old socket")?;
            old_owner.promote(SocketClass::Reservation)?;
            let old_slot = old_owner.admitted()?.id();
            let old_id = "held-old-generation".to_owned();
            let authenticated = Instant::now() - GENERATION_INTERVAL;
            let (cancel, cancelled) = watch::channel(false);
            let (send, _commands) = mpsc::channel(16);
            {
                let mut state = relay_lock(&relay.relay);
                state.reservations.insert(
                    target.clone(),
                    Reservation {
                        id: old_id.clone(),
                        public_key: identity.public_key_hex()?,
                        send,
                        source: IpAddr::from([127, 0, 0, 1]),
                        authenticated_at: authenticated,
                        slot: old_slot,
                        ready: true,
                        cancel,
                    },
                );
                state.generations.insert(
                    target.clone(),
                    GenerationWindow {
                        start: authenticated,
                        last: authenticated,
                        count: 1,
                    },
                );
            }
            let (mut next, nonce) = open_control(&url).await?;
            let next_task = accepts.recv().await.context("missing next task")?;
            send_reserve(&mut next, &nonce, &identity).await?;
            tokio::time::timeout(CONNECT_TIMEOUT, async {
                loop {
                    let waiting = relay_lock(&relay.relay)
                        .reservations
                        .get(&target)
                        .is_some_and(|reservation| reservation.id != old_id && !reservation.ready);
                    if waiting {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            assert!(*cancelled.borrow());
            let pending_id = relay_lock(&relay.relay).reservations[&target].id.clone();
            assert_eq!(relay.admission.snapshot().promotions, 1);
            assert_eq!(relay_lock(&relay.relay).generations[&target].count, 2);
            let (mut third, nonce) = open_control(&url).await?;
            let third_task = accepts.recv().await.context("missing third task")?;
            send_reserve(&mut third, &nonce, &identity).await?;
            assert!(
                third_task
                    .done
                    .await?
                    .unwrap_err()
                    .contains("reservation_handoff_pending")
            );
            assert_socket_closed(&mut third).await?;
            assert_eq!(relay_lock(&relay.relay).generations[&target].count, 2);
            assert_eq!(
                relay_lock(&relay.relay).reservations[&target].id,
                pending_id
            );
            assert_eq!(relay.admission.snapshot().promotions, 1);
            if finish {
                drop(old_owner);
                assert!(matches!(
                    receive_control(&mut next).await?,
                    Control::Reserved
                ));
                assert!(relay_lock(&relay.relay).reservations[&target].ready);
                assert_eq!(relay.admission.snapshot().promotions, 0);
                next_task.abort.abort();
                assert!(next_task.done.await?.is_err());
            } else {
                next_task.abort.abort();
                assert!(next_task.done.await?.unwrap_err().contains("cancelled"));
                assert!(relay_lock(&relay.relay).reservations.is_empty());
                let snapshot = relay.admission.snapshot();
                assert_eq!(
                    (
                        snapshot.total,
                        snapshot.long,
                        snapshot.held_long,
                        snapshot.promotions
                    ),
                    (1, 1, 0, 0)
                );
                drop(old_owner);
            }
            assert_socket_closed(&mut old_client).await?;
            assert_socket_closed(&mut next).await?;
            assert_admission_empty(&relay);
            assert!(relay_lock(&relay.relay).reservations.is_empty());
        }
        eprintln!(
            "NET1_A2 old_owner=held_real_ws_fixture pending_third_rejected=true third_not_generation_charged=true pending_abort_released=true old_late_drop_safe=true all_active=0"
        );
        Ok(())
    }

    #[test]
    fn x1_shared_production_byte_rate_frame_and_liveness_boundaries() -> Result<()> {
        let start = Instant::now();
        let mut state = ForwardState::new(start);
        for direction in 0..2 {
            let mut remaining = CIRCUIT_BYTE_LIMIT / 2;
            while remaining > 0 {
                let length = remaining.min(MAX_FRAME_BYTES);
                state.receive(direction, &Message::Binary(vec![0; length].into()), start)?;
                remaining -= length;
            }
        }
        assert_eq!(state.bytes, CIRCUIT_BYTE_LIMIT);
        assert!(
            state
                .receive(1, &Message::Binary(vec![0].into()), start)
                .is_err()
        );
        assert_eq!(state.bytes, CIRCUIT_BYTE_LIMIT);
        let mut rate = ForwardState::new(start);
        let mut left = CIRCUIT_BYTES_PER_SECOND;
        while left > 0 {
            let bytes = left.min(MAX_FRAME_BYTES);
            assert_eq!(rate.rate(bytes, start), None);
            left -= bytes;
        }
        assert_eq!(rate.rate(1, start), Some(Duration::from_secs(1)));
        let next_window = start + Duration::from_secs(1);
        assert_eq!(rate.rate(MAX_FRAME_BYTES, next_window), None);
        assert_eq!(rate.rate(MAX_FRAME_BYTES, next_window), None);
        assert_eq!(rate.window_bytes, 2 * MAX_FRAME_BYTES);
        let mut control = ForwardState::new(start);
        for direction in (0..2).cycle().take(RELAY_FRAME_RATE) {
            let frame = if direction == 0 {
                Message::Ping(Vec::new().into())
            } else {
                Message::Pong(Vec::new().into())
            };
            control.receive(direction, &frame, start)?;
        }
        assert!(
            control
                .receive(1, &Message::Pong(Vec::new().into()), start)
                .is_err()
        );
        let mut live = ForwardState::new(start);
        let deadline = start + IDLE_TIMEOUT;
        live.last_received[0] = deadline;
        assert_eq!(live.idle(deadline, IDLE_TIMEOUT), Some(1));
        live.last_received = [start, deadline];
        assert_eq!(live.idle(deadline, IDLE_TIMEOUT), Some(0));
        live.last_received = [deadline; 2];
        assert_eq!(live.idle(deadline, IDLE_TIMEOUT), None);
        assert!(
            live.receive(0, &Message::Text("plaintext".into()), deadline)
                .is_err()
        );
        assert!(
            live.receive(
                0,
                &Message::Binary(vec![0; MAX_FRAME_BYTES + 1].into()),
                deadline
            )
            .is_err()
        );
        eprintln!(
            "NET1_X1 production_total_bytes=67108864 shared_rate_bytes_per_second=2097152 shared_frames_per_minute=30720 each_side_idle_seconds=90 boundaries=passed"
        );
        Ok(())
    }

    async fn duplex_pump(socket: NetworkSocket, marker: u8) -> Result<(usize, usize)> {
        let (mut write, mut read) = socket.split();
        let writer = async {
            let mut sent = 0;
            for sequence in 0..1025u64 {
                let mut payload = vec![marker; 32_768];
                payload[..8].copy_from_slice(&sequence.to_le_bytes());
                if send_frame_sink(&mut write, Message::Binary(payload.into()))
                    .await
                    .is_err()
                {
                    break;
                }
                sent += 1;
            }
            sent
        };
        let reader = async {
            let mut received = 0u64;
            while let Some(frame) = read.next().await {
                match frame {
                    Ok(Message::Binary(bytes)) => {
                        ensure!(bytes.len() == 32_768, "duplex frame size changed");
                        ensure!(bytes[..8] == received.to_le_bytes(), "duplex order changed");
                        ensure!(
                            bytes[8..].iter().all(|byte| *byte == 3 - marker),
                            "duplex bytes changed"
                        );
                        received += 1;
                    }
                    Ok(Message::Ping(_) | Message::Pong(_)) => {}
                    Err(_) | Ok(Message::Close(_)) => break,
                    _ => bail!("unexpected duplex application frame"),
                }
            }
            Ok::<_, anyhow::Error>(received as usize)
        };
        let (sent, received) = tokio::join!(writer, reader);
        Ok((sent, received?))
    }

    #[tokio::test]
    async fn x1_two_real_writers_share_the_production_rate_and_total_byte_limit() -> Result<()> {
        let mut tasks = JoinSet::new();
        let (relay, _) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                relay_enabled: true,
                ..Default::default()
            },
        );
        let (observed, mut accepts) = mpsc::unbounded_channel();
        let (url, _) =
            serve_observed(relay.clone(), "127.0.0.1:0", &mut tasks, Some(observed)).await?;
        let identity = LocalIdentity::generate();
        let mut reservation = reserve_at(&url, &identity).await?;
        let reservation_task = accepts.recv().await.context("missing reservation task")?;
        let (flow, mut flows) = mpsc::unbounded_channel();
        relay.test_hooks.lock().unwrap().forward = Some(ForwardFixture {
            heartbeat: HEARTBEAT,
            idle_timeout: IDLE_TIMEOUT,
            send_timeout: CONNECT_TIMEOUT,
            observed: flow,
        });
        let (caller, target, mut circuit) =
            ready_circuit(&url, &identity, &mut reservation, &mut accepts).await?;
        let caller_pump = tokio::spawn(duplex_pump(caller, 1));
        let target_pump = tokio::spawn(duplex_pump(target, 2));
        let mut waits = [false; 2];
        let mut accepted = 0;
        let mut lookup_done = false;
        let error = tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                tokio::select! {
                    result = &mut circuit.done => return Ok::<_, anyhow::Error>(result?.unwrap_err()),
                    observation = flows.recv() => {
                        let Some(observation) = observation else {
                            return Ok((&mut circuit.done).await?.unwrap_err());
                        };
                        assert_forward_bounds(&observation);
                        accepted = accepted.max(observation.accepted_bytes);
                        if observation.event == "rate_wait" {
                            waits[observation.direction] = true;
                        }
                        if waits == [true; 2] && !lookup_done {
                            let (mut lookup, _) = open_control(&url).await?;
                            send_control(&mut lookup, &Control::Lookup { target: identity.peer_id()? }).await?;
                            assert!(matches!(receive_control(&mut lookup).await?, Control::Record { .. }));
                            assert!(accepts.recv().await.context("missing lookup task")?.done.await?.is_ok());
                            lookup_done = true;
                        }
                    }
                }
            }
        }).await?;
        let error = error?;
        assert!(error.contains("relay byte budget exceeded"), "{error}");
        let caller = caller_pump.await??;
        let target = target_pump.await??;
        while let Ok(observation) = flows.try_recv() {
            assert_forward_bounds(&observation);
            accepted = accepted.max(observation.accepted_bytes);
            if observation.event == "rate_wait" {
                waits[observation.direction] = true;
            }
        }
        assert_eq!(accepted, CIRCUIT_BYTE_LIMIT);
        assert_eq!(waits, [true; 2]);
        assert!(lookup_done);
        for (sent, received) in [caller, target] {
            assert!(sent * 32_768 < CIRCUIT_BYTE_LIMIT);
            assert!(received * 32_768 > CIRCUIT_BYTE_LIMIT / 4);
        }
        assert_eq!(relay.admission.snapshot().circuits, 0);
        reservation_task.abort.abort();
        assert!(reservation_task.done.await?.is_err());
        assert_admission_empty(&relay);
        eprintln!(
            "NET1_X1 real_duplex_accepted_bytes=67108864 per_endpoint_attempt_bytes=33587200 both_writers_rate_wait=true default_rate_bytes_per_second=2097152 concurrent_lookup=true ordered_opaque_frames=true timers=production_defaults all_active=0"
        );
        Ok(())
    }

    async fn ready_circuit(
        url: &str,
        identity: &LocalIdentity,
        reservation: &mut NetworkSocket,
        accepts: &mut mpsc::UnboundedReceiver<AcceptedTask>,
    ) -> Result<(NetworkSocket, NetworkSocket, AcceptedTask)> {
        ready_circuit_with_buffer(url, identity, reservation, accepts, None).await
    }

    async fn ready_circuit_with_buffer(
        url: &str,
        identity: &LocalIdentity,
        reservation: &mut NetworkSocket,
        accepts: &mut mpsc::UnboundedReceiver<AcceptedTask>,
        receive_bytes: Option<u32>,
    ) -> Result<(NetworkSocket, NetworkSocket, AcceptedTask)> {
        let mut caller = dial_pending(url, &identity.peer_id()?).await?;
        let task = accepts.recv().await.context("missing circuit task")?;
        let token = incoming_token(reservation).await?;
        let mut target = match receive_bytes {
            Some(bytes) => {
                let (mut socket, nonce) =
                    open_with_receive_buffer(url, IpAddr::from([127, 0, 0, 1]), Some(bytes))
                        .await?;
                send_attach(&mut socket, &nonce, identity, &token).await?;
                socket
            }
            None => attach_pending(url, identity, &token).await?,
        };
        assert!(
            accepts
                .recv()
                .await
                .context("missing attach task")?
                .done
                .await?
                .is_ok()
        );
        assert!(matches!(
            receive_control(&mut caller).await?,
            Control::Ready
        ));
        assert!(matches!(
            receive_control(&mut target).await?,
            Control::Ready
        ));
        Ok((caller, target, task))
    }

    #[tokio::test]
    async fn x1_abort_and_timeout_during_rate_or_slow_write_release_io_and_allow_reuse()
    -> Result<()> {
        for stage in ["rate_abort", "write_abort", "write_timeout"] {
            let mut tasks = JoinSet::new();
            let (relay, _) = NetworkHub::new(
                LocalIdentity::generate(),
                NetworkConfig {
                    relay_enabled: true,
                    ..Default::default()
                },
            );
            let (observed, mut accepts) = mpsc::unbounded_channel();
            let (url, _) =
                serve_observed(relay.clone(), "127.0.0.1:0", &mut tasks, Some(observed)).await?;
            let identity = LocalIdentity::generate();
            let mut reservation = reserve_at(&url, &identity).await?;
            let reservation_task = accepts.recv().await.context("missing reservation task")?;
            let (flow, mut flows) = mpsc::unbounded_channel();
            relay.test_hooks.lock().unwrap().forward = Some(ForwardFixture {
                heartbeat: Duration::from_millis(100),
                idle_timeout: IDLE_TIMEOUT,
                send_timeout: if stage == "write_timeout" {
                    Duration::from_millis(1200)
                } else {
                    CONNECT_TIMEOUT
                },
                observed: flow,
            });
            let (caller, target, mut circuit) = ready_circuit_with_buffer(
                &url,
                &identity,
                &mut reservation,
                &mut accepts,
                Some(8192),
            )
            .await?;
            let (mut caller_write, mut caller_read) = caller.split();
            let (_target_write, mut target_read) = target.split();
            let flood = tokio::spawn(async move {
                for _ in 0..200 {
                    if send_frame_sink(&mut caller_write, Message::Binary(vec![9; 60_000].into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                caller_write
            });
            if stage == "rate_abort" {
                tokio::time::timeout(CONNECT_TIMEOUT, async {
                    loop {
                        tokio::select! {
                            observation = flows.recv() => {
                                let observation = observation.context("rate observations ended")?;
                                assert_forward_bounds(&observation);
                                if observation.event == "rate_wait" && observation.direction == 0 {
                                    break;
                                }
                            }
                            frame = target_read.next() => ensure!(matches!(frame, Some(Ok(_))), "rate fixture target closed"),
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                }).await??;
            } else {
                wait_continuous_backpressure(&mut flows).await?;
            }
            if stage != "write_timeout" {
                circuit.abort.abort();
            }
            let error = tokio::time::timeout(Duration::from_secs(3), &mut circuit.done)
                .await??
                .unwrap_err();
            if stage == "write_timeout" {
                assert!(error.contains("timed out"), "{error}");
            } else {
                assert!(error.contains("cancelled"), "{error}");
            }
            flood.abort();
            match flood.await {
                Err(error) => assert!(error.is_cancelled()),
                Ok(write) => drop(write),
            }
            drain_to_eof(&mut caller_read).await?;
            drain_to_eof(&mut target_read).await?;
            while let Ok(observation) = flows.try_recv() {
                assert_forward_bounds(&observation);
                if stage == "rate_abort" {
                    assert!(
                        observation.direction != 0 || observation.event != "rate_resumed",
                        "rate sleep ended before cancellation"
                    );
                } else {
                    assert!(
                        observation.direction != 0 || observation.event != "write_resumed",
                        "slow write ended before cancellation/timeout"
                    );
                }
                if observation.event == "closed" {
                    assert!(
                        observation
                            .reader
                            .iter()
                            .chain(&observation.queue)
                            .chain(&observation.writer)
                            .all(|held| held.frames == 0 && held.bytes == 0)
                    );
                }
            }
            let snapshot = relay.admission.snapshot();
            assert_eq!(
                (snapshot.total, snapshot.long, snapshot.circuits),
                (1, 1, 0)
            );
            assert!(snapshot.targets.is_empty());
            assert!(relay_lock(&relay.relay).attachments.is_empty());
            let (mut fresh_caller, mut fresh_target, fresh_task) =
                ready_circuit(&url, &identity, &mut reservation, &mut accepts).await?;
            noise_round_trip(
                &LocalIdentity::generate(),
                &identity,
                &mut fresh_caller,
                &mut fresh_target,
            )
            .await?;
            fresh_task.abort.abort();
            assert!(fresh_task.done.await?.is_err());
            assert_socket_closed(&mut fresh_caller).await?;
            assert_socket_closed(&mut fresh_target).await?;
            reservation_task.abort.abort();
            assert!(reservation_task.done.await?.is_err());
            assert_admission_empty(&relay);
            eprintln!(
                "NET1_X1 wait_stage={stage} both_peer_eof=true circuit_pending_released=true same_source_target_noise_reused=true heartbeat_fixture_ms=100 timeout_fixture_ms={} all_active=0",
                if stage == "write_timeout" {
                    1200
                } else {
                    10_000
                }
            );
        }
        Ok(())
    }

    async fn drain_to_eof(
        socket: &mut (impl Stream<Item = Result<Message>> + Unpin),
    ) -> Result<()> {
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            while let Some(frame) = socket.next().await {
                match frame {
                    Err(_) | Ok(Message::Close(_)) => return Ok(()),
                    Ok(_) => {}
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    #[tokio::test]
    async fn x1_each_silent_endpoint_expires_despite_continuous_opposite_traffic() -> Result<()> {
        for silent in 0..2 {
            let mut tasks = JoinSet::new();
            let (relay, _) = NetworkHub::new(
                LocalIdentity::generate(),
                NetworkConfig {
                    relay_enabled: true,
                    ..Default::default()
                },
            );
            let (observed, mut accepts) = mpsc::unbounded_channel();
            let (url, _) =
                serve_observed(relay.clone(), "127.0.0.1:0", &mut tasks, Some(observed)).await?;
            let identity = LocalIdentity::generate();
            let mut reservation = reserve_at(&url, &identity).await?;
            let reservation_task = accepts.recv().await.context("missing reservation task")?;
            let (flow, mut flows) = mpsc::unbounded_channel();
            relay.test_hooks.lock().unwrap().forward = Some(ForwardFixture {
                heartbeat: Duration::from_millis(20),
                idle_timeout: Duration::from_millis(180),
                send_timeout: CONNECT_TIMEOUT,
                observed: flow,
            });
            let (caller, target, mut circuit) =
                ready_circuit(&url, &identity, &mut reservation, &mut accepts).await?;
            let (mut quiet, active) = if silent == 0 {
                (caller, target)
            } else {
                (target, caller)
            };
            let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let sent_count = sent.clone();
            let traffic = tokio::spawn(async move {
                let (mut write, mut read) = active.split();
                let mut interval = tokio::time::interval(Duration::from_millis(20));
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if send_frame_sink(&mut write, Message::Binary(vec![1].into())).await.is_err() {
                                break;
                            }
                            sent_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        frame = read.next() => match frame {
                            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                            _ => {}
                        }
                    }
                }
            });
            let error = tokio::time::timeout(Duration::from_secs(2), &mut circuit.done)
                .await??
                .unwrap_err();
            assert!(error.contains("relay endpoint idle"), "{error}");
            assert!(sent.load(std::sync::atomic::Ordering::Relaxed) >= 3);
            assert!(
                tokio::time::timeout(Duration::from_secs(1), traffic)
                    .await?
                    .is_ok()
            );
            drain_to_eof(&mut quiet).await?;
            let mut exact_idle = false;
            while let Ok(observation) = flows.try_recv() {
                assert_forward_bounds(&observation);
                if observation.event == "idle" {
                    assert_eq!(observation.direction, silent);
                    exact_idle = true;
                }
                if observation.event == "closed" {
                    assert!(
                        observation
                            .reader
                            .iter()
                            .chain(&observation.queue)
                            .chain(&observation.writer)
                            .all(|held| held.frames == 0 && held.bytes == 0)
                    );
                }
            }
            assert!(exact_idle);
            assert_eq!(relay.admission.snapshot().circuits, 0);
            reservation_task.abort.abort();
            assert!(reservation_task.done.await?.is_err());
            assert_admission_empty(&relay);
        }
        eprintln!(
            "NET1_X1 silent_caller_expires=true silent_target_expires=true active_other_side_continues=true heartbeat_fixture_ms=20 idle_fixture_ms=180 production_idle_boundary_seconds=90 all_active=0"
        );
        Ok(())
    }

    async fn wait_continuous_backpressure(
        flows: &mut mpsc::UnboundedReceiver<ForwardObservation>,
    ) -> Result<()> {
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            let mut write_pending = false;
            let mut stable_since = None;
            loop {
                let deadline = stable_since.map(|since| since + Duration::from_millis(500));
                let observation = tokio::select! {
                    observation = flows.recv() => observation.context("flow observation ended")?,
                    _ = async {
                        match deadline {
                            Some(deadline) => tokio::time::sleep_until(deadline).await,
                            None => futures_util::future::pending().await,
                        }
                    } => break,
                };
                assert_forward_bounds(&observation);
                if observation.direction == 0 {
                    match observation.event {
                        "write_pending" => write_pending = true,
                        "write_resumed" => {
                            write_pending = false;
                            stable_since = None;
                        }
                        "queue_full" if write_pending && stable_since.is_none() => {
                            stable_since = Some(tokio::time::Instant::now());
                        }
                        _ => {}
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    fn assert_forward_bounds(observed: &ForwardObservation) {
        assert!(observed.direction < 2);
        assert!(observed.accepted_bytes <= CIRCUIT_BYTE_LIMIT);
        assert!(observed.window_bytes <= CIRCUIT_BYTES_PER_SECOND);
        let mut bytes = 0;
        for side in 0..2 {
            assert!(observed.reader[side].frames <= 1);
            assert!(observed.queue[side].frames <= RELAY_QUEUE_FRAMES);
            assert!(observed.writer[side].frames <= 1);
            for stage in [
                observed.reader[side],
                observed.queue[side],
                observed.writer[side],
            ] {
                assert!(stage.bytes <= stage.frames * MAX_FRAME_BYTES);
                bytes += stage.bytes;
            }
        }
        assert!(bytes <= 12 * MAX_FRAME_BYTES);
    }

    #[tokio::test]
    async fn x1_stopped_tcp_reader_keeps_reverse_noise_and_other_target_progress() -> Result<()> {
        let mut tasks = JoinSet::new();
        let (relay, _) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                relay_enabled: true,
                ..Default::default()
            },
        );
        let (observed, mut accepts) = mpsc::unbounded_channel();
        let (url, _) =
            serve_observed(relay.clone(), "127.0.0.1:0", &mut tasks, Some(observed)).await?;
        let identity = LocalIdentity::generate();
        let caller_identity = LocalIdentity::generate();
        let target_id = identity.peer_id()?;
        let mut reservation = reserve_at(&url, &identity).await?;
        let reservation_task = accepts.recv().await.context("missing reservation task")?;
        let (flow, mut flows) = mpsc::unbounded_channel();
        relay.test_hooks.lock().unwrap().forward = Some(ForwardFixture {
            heartbeat: Duration::from_millis(100),
            idle_timeout: IDLE_TIMEOUT,
            send_timeout: CONNECT_TIMEOUT,
            observed: flow,
        });
        let mut caller = dial_pending(&url, &target_id).await?;
        let circuit_task = accepts.recv().await.context("missing circuit task")?;
        let token = incoming_token(&mut reservation).await?;
        let (mut target, nonce) =
            open_with_receive_buffer(&url, IpAddr::from([127, 0, 0, 1]), Some(8192)).await?;
        send_attach(&mut target, &nonce, &identity, &token).await?;
        assert!(
            accepts
                .recv()
                .await
                .context("missing attach task")?
                .done
                .await?
                .is_ok()
        );
        assert!(matches!(
            receive_control(&mut caller).await?,
            Control::Ready
        ));
        assert!(matches!(
            receive_control(&mut target).await?,
            Control::Ready
        ));
        let (mut client_crypto, mut target_crypto) =
            noise_channels(&caller_identity, &identity, &mut caller, &mut target).await?;
        let (mut caller_write, mut caller_read) = caller.split();
        let (mut target_write, mut stopped_read) = target.split();
        let messages = 160u64;
        let mut encrypted = Vec::new();
        for sequence in 0..messages {
            let mut payload = vec![sequence as u8; 60_000];
            payload[..8].copy_from_slice(&sequence.to_le_bytes());
            encrypted.extend(
                client_crypto
                    .seal_message(&payload)
                    .map_err(anyhow::Error::msg)?,
            );
        }
        let flood = tokio::spawn(async move {
            for record in encrypted {
                send_frame_sink(&mut caller_write, Message::Binary(record.into())).await?;
            }
            Ok::<_, anyhow::Error>(caller_write)
        });
        wait_continuous_backpressure(&mut flows).await?;
        let reverse_started = Instant::now();
        for record in target_crypto
            .seal_message(b"reverse while TCP reader paused")
            .map_err(anyhow::Error::msg)?
        {
            send_frame_sink(&mut target_write, Message::Binary(record.into())).await?;
        }
        let reverse =
            tokio::time::timeout(Duration::from_secs(2), next_binary(&mut caller_read)).await??;
        assert_eq!(
            client_crypto
                .open_record(&reverse)
                .map_err(anyhow::Error::msg)?
                .unwrap(),
            b"reverse while TCP reader paused"
        );
        assert!(reverse_started.elapsed() < CONNECT_TIMEOUT);
        let (mut lookup, _) = open_control(&url).await?;
        send_control(
            &mut lookup,
            &Control::Lookup {
                target: target_id.clone(),
            },
        )
        .await?;
        assert!(matches!(
            receive_control(&mut lookup).await?,
            Control::Record { .. }
        ));
        assert!(
            accepts
                .recv()
                .await
                .context("missing lookup task")?
                .done
                .await?
                .is_ok()
        );
        let second_identity = LocalIdentity::generate();
        let mut second_reservation = reserve_at(&url, &second_identity).await?;
        let second_reservation_task = accepts.recv().await.context("missing second reservation")?;
        let mut second_caller = dial_pending(&url, &second_identity.peer_id()?).await?;
        let second_circuit_task = accepts.recv().await.context("missing second circuit")?;
        let second_token = incoming_token(&mut second_reservation).await?;
        let mut second_target = attach_pending(&url, &second_identity, &second_token).await?;
        assert!(
            accepts
                .recv()
                .await
                .context("missing second attach")?
                .done
                .await?
                .is_ok()
        );
        assert!(matches!(
            receive_control(&mut second_caller).await?,
            Control::Ready
        ));
        assert!(matches!(
            receive_control(&mut second_target).await?,
            Control::Ready
        ));
        noise_round_trip(
            &caller_identity,
            &second_identity,
            &mut second_caller,
            &mut second_target,
        )
        .await?;
        assert!(
            !circuit_task.abort.is_finished(),
            "slow circuit expired before reverse/other-target checks"
        );
        while let Ok(observation) = flows.try_recv() {
            assert_forward_bounds(&observation);
            assert!(
                observation.direction != 0 || observation.event != "write_resumed",
                "slow write resumed before the reverse/lookup/other-target interval completed"
            );
        }
        for sequence in 0..messages {
            let received = loop {
                let record = next_binary(&mut stopped_read).await?;
                if let Some(message) = target_crypto
                    .open_record(&record)
                    .map_err(anyhow::Error::msg)?
                {
                    break message;
                }
            };
            let mut expected = vec![sequence as u8; 60_000];
            expected[..8].copy_from_slice(&sequence.to_le_bytes());
            assert_eq!(received, expected);
        }
        let _caller_write = flood.await??;
        circuit_task.abort.abort();
        second_circuit_task.abort.abort();
        reservation_task.abort.abort();
        second_reservation_task.abort.abort();
        assert!(circuit_task.done.await?.is_err());
        assert!(second_circuit_task.done.await?.is_err());
        assert!(reservation_task.done.await?.is_err());
        assert!(second_reservation_task.done.await?.is_err());
        assert_admission_empty(&relay);
        while let Ok(observation) = flows.try_recv() {
            assert_forward_bounds(&observation);
        }
        eprintln!(
            "NET1_X1 actual_tcp_reader=paused receive_buffer=8192 write_pending_and_queue_full=true stable_pending_ms=500 write_pending_continuous_during_checks=true reverse_noise=true lookup=true other_target_noise=true ordered_decrypted_messages=160 queue_frames_per_direction=4 application_payload_bound_bytes=786420 heartbeat_fixture_ms=100 send_timeout_seconds=10 all_active=0"
        );
        Ok(())
    }

    async fn send_frame_sink(
        socket: &mut (impl Sink<Message, Error = anyhow::Error> + Unpin),
        message: Message,
    ) -> Result<()> {
        tokio::time::timeout(CONNECT_TIMEOUT, socket.send(message)).await?
    }

    async fn noise_channels(
        caller_identity: &LocalIdentity,
        target_identity: &LocalIdentity,
        caller: &mut NetworkSocket,
        target: &mut NetworkSocket,
    ) -> Result<(vibeface_secure::Channel, vibeface_secure::Channel)> {
        let mut client = Handshake::new(
            true,
            &caller_identity.secret_key_hex,
            Some(target_identity.peer_id()?),
            b"test:directory",
        )
        .map_err(anyhow::Error::msg)?;
        let mut server = Handshake::new(
            false,
            &target_identity.secret_key_hex,
            Some(caller_identity.peer_id()?),
            b"test:directory",
        )
        .map_err(anyhow::Error::msg)?;
        send_frame(
            caller,
            Message::Binary(client.write(&[]).map_err(anyhow::Error::msg)?.into()),
        )
        .await?;
        server
            .read(&next_binary(target).await?)
            .map_err(anyhow::Error::msg)?;
        send_frame(
            target,
            Message::Binary(server.write(&[]).map_err(anyhow::Error::msg)?.into()),
        )
        .await?;
        client
            .read(&next_binary(caller).await?)
            .map_err(anyhow::Error::msg)?;
        send_frame(
            caller,
            Message::Binary(client.write(&[]).map_err(anyhow::Error::msg)?.into()),
        )
        .await?;
        server
            .read(&next_binary(target).await?)
            .map_err(anyhow::Error::msg)?;
        Ok((
            client.finish().map_err(anyhow::Error::msg)?,
            server.finish().map_err(anyhow::Error::msg)?,
        ))
    }

    async fn noise_round_trip(
        caller_identity: &LocalIdentity,
        target_identity: &LocalIdentity,
        caller: &mut NetworkSocket,
        target: &mut NetworkSocket,
    ) -> Result<()> {
        let (mut client, mut server) =
            noise_channels(caller_identity, target_identity, caller, target).await?;
        encrypted_round_trip(&mut client, &mut server, caller, target).await
    }

    async fn encrypted_round_trip(
        client: &mut vibeface_secure::Channel,
        server: &mut vibeface_secure::Channel,
        caller: &mut NetworkSocket,
        target: &mut NetworkSocket,
    ) -> Result<()> {
        for encrypted in client
            .seal_message(b"fixed-target canary")
            .map_err(anyhow::Error::msg)?
        {
            send_frame(caller, Message::Binary(encrypted.into())).await?;
        }
        assert_eq!(
            server
                .open_record(&next_binary(target).await?)
                .map_err(anyhow::Error::msg)?
                .unwrap(),
            b"fixed-target canary"
        );
        for encrypted in server
            .seal_message(b"complete")
            .map_err(anyhow::Error::msg)?
        {
            send_frame(target, Message::Binary(encrypted.into())).await?;
        }
        assert_eq!(
            client
                .open_record(&next_binary(caller).await?)
                .map_err(anyhow::Error::msg)?
                .unwrap(),
            b"complete"
        );
        Ok(())
    }

    async fn dial_test_noise(
        hub: &NetworkHub,
        caller_identity: &LocalIdentity,
        target_identity: &LocalIdentity,
        service: Service,
        incoming: &mut mpsc::Receiver<IncomingSocket>,
        domain: &'static [u8],
    ) -> Result<(
        NetworkSocket,
        vibeface_secure::Channel,
        IncomingSocket,
        vibeface_secure::Channel,
    )> {
        let target_id = target_identity.peer_id()?;
        let dial = hub.dial_authenticated(&target_id, service, None, |mut socket| async move {
            let result = async {
                let mut handshake = Handshake::new(
                    true,
                    &caller_identity.secret_key_hex,
                    Some(target_identity.peer_id()?),
                    domain,
                )
                .map_err(anyhow::Error::msg)?;
                send_frame(
                    &mut socket,
                    Message::Binary(handshake.write(&[]).map_err(anyhow::Error::msg)?.into()),
                )
                .await?;
                assert!(
                    handshake
                        .read(&next_binary(&mut socket).await?)
                        .map_err(anyhow::Error::msg)?
                        .is_empty()
                );
                send_frame(
                    &mut socket,
                    Message::Binary(
                        handshake
                            .write(b"authenticated caller")
                            .map_err(anyhow::Error::msg)?
                            .into(),
                    ),
                )
                .await?;
                Ok::<_, anyhow::Error>((socket, handshake.finish().map_err(anyhow::Error::msg)?))
            }
            .await;
            result.map_err(AuthenticationFailure::Path)
        });
        let accept = async {
            let mut accepted = incoming
                .recv()
                .await
                .context("missing authenticated target socket")?;
            assert_eq!(accepted.service, service);
            let mut handshake = Handshake::new(
                false,
                &target_identity.secret_key_hex,
                Some(caller_identity.peer_id()?),
                domain,
            )
            .map_err(anyhow::Error::msg)?;
            assert!(
                handshake
                    .read(&next_binary(&mut accepted.socket).await?)
                    .map_err(anyhow::Error::msg)?
                    .is_empty()
            );
            send_frame(
                &mut accepted.socket,
                Message::Binary(handshake.write(&[]).map_err(anyhow::Error::msg)?.into()),
            )
            .await?;
            assert_eq!(
                handshake
                    .read(&next_binary(&mut accepted.socket).await?)
                    .map_err(anyhow::Error::msg)?,
                b"authenticated caller"
            );
            Ok::<_, anyhow::Error>((accepted, handshake.finish().map_err(anyhow::Error::msg)?))
        };
        let ((socket, client), (accepted, server)) = tokio::try_join!(dial, accept)?;
        Ok((socket, client, accepted, server))
    }

    async fn next_binary(
        socket: &mut (impl Stream<Item = Result<Message>> + Unpin),
    ) -> Result<Vec<u8>> {
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
    async fn real_sources_share_initial_limits_despite_forwarded_headers_and_ipv6_is_independent()
    -> Result<()> {
        use tokio_tungstenite::tungstenite::{Error as WsError, client::IntoClientRequest};
        let mut tasks = JoinSet::new();
        let (hub, _) = NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        let (v4, mut denials) = serve_at(hub.clone(), "127.0.0.1:0", &mut tasks).await?;
        let (v6, _) = serve_at(hub.clone(), "[::1]:0", &mut tasks).await?;
        let mut initial = Vec::new();
        for _ in 0..4 {
            initial.push(open_control(&v4).await?.0);
        }
        let mut request = v4.clone().into_client_request()?;
        request
            .headers_mut()
            .insert("x-forwarded-for", "203.0.113.55".parse()?);
        request
            .headers_mut()
            .insert("forwarded", "for=198.51.100.8".parse()?);
        let error = tokio_tungstenite::connect_async(request)
            .await
            .err()
            .context("forged header bypassed source cap")?;
        let WsError::Http(response) = error else {
            bail!("source rejection was not before Upgrade");
        };
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(denials.recv().await, Some(Denied::SourceInitialSocket));
        assert_eq!(
            (
                hub.admission.snapshot().short,
                hub.admission.snapshot().inbound_requests
            ),
            (4, 4)
        );
        let (mut other, _) = open_control(&v6).await?;
        send_control(
            &mut other,
            &Control::Lookup {
                target: "f".repeat(64),
            },
        )
        .await?;
        assert!(matches!(
            receive_control(&mut other).await?,
            Control::Record { .. }
        ));
        drop(other);
        drop(initial);
        tokio::time::timeout(Duration::from_secs(5), async {
            while hub.admission.snapshot().total != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let snapshot = hub.admission.snapshot();
        assert_eq!(
            snapshot.sources[&"127.0.0.1".parse::<IpAddr>()?].requests,
            4
        );
        assert_eq!(snapshot.sources[&"::1".parse::<IpAddr>()?].requests, 1);
        eprintln!(
            "NET1_A1 ipv4_initial=4 extra_initial_http=503 forwarded_headers_ignored=true ipv6_lookup=passed all_active=0"
        );
        Ok(())
    }

    #[tokio::test]
    async fn real_source_request_rate_survives_socket_drop_and_another_source_can_lookup()
    -> Result<()> {
        use tokio_tungstenite::tungstenite::{Error as WsError, client::IntoClientRequest};
        let mut tasks = JoinSet::new();
        let (hub, _) = NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        let (v4, mut denied) = serve_at(hub.clone(), "127.0.0.1:0", &mut tasks).await?;
        let (v6, _) = serve_at(hub.clone(), "[::1]:0", &mut tasks).await?;
        for _ in 0..120 {
            let (mut socket, _) = open_control(&v4).await?;
            send_control(
                &mut socket,
                &Control::Lookup {
                    target: "f".repeat(64),
                },
            )
            .await?;
            assert!(matches!(
                receive_control(&mut socket).await?,
                Control::Record { .. }
            ));
        }
        let mut request = v4.into_client_request()?;
        request
            .headers_mut()
            .insert("x-forwarded-for", "203.0.113.55".parse()?);
        let WsError::Http(response) = tokio_tungstenite::connect_async(request)
            .await
            .err()
            .context("121st source request accepted")?
        else {
            bail!("source rate was not rejected before Upgrade");
        };
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(denied.recv().await, Some(Denied::SourceRate));
        assert_eq!(hub.admission.snapshot().inbound_requests, 120);
        let (mut other, _) = open_control(&v6).await?;
        send_control(
            &mut other,
            &Control::Lookup {
                target: "f".repeat(64),
            },
        )
        .await?;
        assert!(matches!(
            receive_control(&mut other).await?,
            Control::Record { .. }
        ));
        drop(other);
        tokio::time::timeout(Duration::from_secs(5), async {
            while hub.admission.snapshot().total != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(hub.admission.snapshot().inbound_requests, 121);
        eprintln!(
            "NET1_A1 ipv4_requests=120 extra_request_http=429 global_after_source_denial=120 ipv6_lookup=passed global_after_ipv6=121"
        );
        Ok(())
    }

    #[tokio::test]
    async fn waiting_attach_circuits_bound_real_sources_and_targets_and_leave_lookup_capacity()
    -> Result<()> {
        let mut tasks = JoinSet::new();
        let (hub, _) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                relay_enabled: true,
                ..Default::default()
            },
        );
        let (v4, mut v4_denied) = serve_at(hub.clone(), "127.0.0.1:0", &mut tasks).await?;
        let (v6, mut v6_denied) = serve_at(hub.clone(), "[::1]:0", &mut tasks).await?;
        let target_a = LocalIdentity::generate();
        let target_b = LocalIdentity::generate();
        let _reservation_a = reserve_at(&v4, &target_a).await?;
        let _reservation_b = reserve_at(&v4, &target_b).await?;
        let mut callers = Vec::new();
        let target = target_a.peer_id()?;
        for _ in 0..4 {
            let (mut socket, _) = open_control(&v4).await?;
            send_control(
                &mut socket,
                &Control::Dial {
                    target: target.clone(),
                    service: Service::Client,
                },
            )
            .await?;
            callers.push(socket);
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while hub.admission.snapshot().targets.get(&target) != Some(&4) {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(hub.admission.snapshot().circuits, 4);
        let (mut source_extra, _) = open_control(&v4).await?;
        send_control(
            &mut source_extra,
            &Control::Dial {
                target: target_b.peer_id()?,
                service: Service::Client,
            },
        )
        .await?;
        assert!(receive_control(&mut source_extra).await.is_err());
        assert_eq!(v4_denied.recv().await, Some(Denied::SourceCircuit));
        let (mut target_extra, _) = open_control(&v6).await?;
        send_control(
            &mut target_extra,
            &Control::Dial {
                target: target_a.peer_id()?,
                service: Service::Client,
            },
        )
        .await?;
        assert!(receive_control(&mut target_extra).await.is_err());
        assert_eq!(v6_denied.recv().await, Some(Denied::TargetCircuit));
        for _ in 0..4 {
            let (mut socket, _) = open_control(&v6).await?;
            send_control(
                &mut socket,
                &Control::Dial {
                    target: target_b.peer_id()?,
                    service: Service::Client,
                },
            )
            .await?;
            callers.push(socket);
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while hub.admission.snapshot().circuits != 8 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        for url in [&v4, &v6] {
            let (mut socket, _) = open_control(url).await?;
            send_control(
                &mut socket,
                &Control::Lookup {
                    target: target_a.peer_id()?,
                },
            )
            .await?;
            assert!(matches!(
                receive_control(&mut socket).await?,
                Control::Record { .. }
            ));
        }
        for mut caller in callers {
            assert!(receive_control(&mut caller).await.is_err());
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while hub.admission.snapshot().circuits != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(hub.admission.snapshot().targets.is_empty());
        assert!(relay_lock(&hub.relay).attachments.is_empty());
        eprintln!(
            "NET1_A1 real_waiting_attach_peak=8 source_circuit_rejected=true target_circuit_rejected=true ipv6_other_target_progress=true lookup_progress=true timeout_circuit_counts=0"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_http_upgrade_releases_an_already_admitted_initial_socket() -> Result<()> {
        use tokio::io::AsyncWriteExt;
        #[derive(Clone)]
        struct Gate {
            hub: Arc<NetworkHub>,
            release: Arc<Semaphore>,
            admitted: mpsc::UnboundedSender<()>,
        }
        async fn upgrade(
            AxumState(gate): AxumState<Gate>,
            ConnectInfo(peer): ConnectInfo<SocketAddr>,
            ws: WebSocketUpgrade,
        ) -> Response {
            let admission = gate
                .hub
                .admit(peer.ip())
                .expect("empty fixture rejected source");
            let _ = gate.admitted.send(());
            let _held = gate.release.acquire().await.expect("fixture gate closed");
            ws.on_upgrade(move |socket| async move {
                let _ = gate.hub.accept(socket, admission).await;
            })
        }
        let (hub, _) = NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        let release = Arc::new(Semaphore::new(0));
        let (admitted, mut observed) = mpsc::unbounded_channel();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = Router::new()
            .route("/api/network/ws", get(upgrade))
            .with_state(Gate {
                hub: hub.clone(),
                release: release.clone(),
                admitted,
            });
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        let mut socket = TcpStream::connect(address).await?;
        socket.write_all(format!("GET /api/network/ws HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").as_bytes()).await?;
        tokio::time::timeout(Duration::from_secs(5), observed.recv())
            .await?
            .context("HTTP request did not reach admission")?;
        assert_eq!(
            (
                hub.admission.snapshot().total,
                hub.admission.snapshot().short
            ),
            (1, 1)
        );
        socket.shutdown().await?;
        drop(socket);
        release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(5), async {
            while hub.admission.snapshot().total != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        eprintln!("NET1_A1 admitted_http_initial=1 cancelled_transport_active=0");
        Ok(())
    }

    #[test]
    fn peer_id_uses_canonical_signed_bytes() -> Result<()> {
        let identity = LocalIdentity {
            secret_key_hex: "01".repeat(32),
        };
        let peer_id = identity.peer_id()?;
        let public_key = identity.public_key_hex()?;
        let unsigned = format!(
            r#"{{"cluster":null,"direct_urls":[],"expires_at":1180,"issued_at":1000,"peer_id":"{peer_id}","public_key":"{public_key}","relay_urls":[],"signature":""}}"#
        );
        let mut record: Reachability = serde_json::from_str(&unsigned)?;
        assert_eq!(record.unsigned()?, unsigned.as_bytes());
        record.signature = identity.sign_payload("reachability-v2", unsigned.as_bytes())?;
        record.verify(1001)?;
        let wire = serde_json::to_string(&record)?;
        assert!(
            serde_json::from_str::<Reachability>(&wire.replace("\"cluster\":null,", "")).is_err()
        );
        let signature = record.signature.clone();
        record.signature = identity.sign_payload("reachability", &record.unsigned()?)?;
        assert!(record.verify(1001).is_err());
        record.signature = signature;
        assert!(serde_json::from_str::<Reachability>(&wire.replace("peer_id", "node_id")).is_err());
        assert_eq!(
            serde_json::to_string(&ReservationProof {
                nonce: "ticket",
                peer_id: &peer_id
            })?,
            format!(r#"{{"nonce":"ticket","peer_id":"{peer_id}"}}"#)
        );
        let input = format!(
            r#"{{"type":"reserve","peer_id":"{peer_id}","public_key":"{public_key}","signature":"signature"}}"#
        );
        let control: Control = serde_json::from_str(&input)?;
        assert_eq!(serde_json::to_value(control)?["peer_id"], peer_id);
        assert!(serde_json::from_str::<Control>(&input.replace("peer_id", "node_id")).is_err());
        Ok(())
    }

    #[test]
    fn mdns_uses_valid_dns_labels_and_retains_the_full_identity() -> Result<()> {
        let peer_id = LocalIdentity::generate().peer_id()?;
        let service = mdns_service(&peer_id, "0.0.0.0:4790".parse()?)?
            .context("wildcard registration missing")?;
        assert!(
            service
                .get_fullname()
                .split('.')
                .all(|label| label.len() <= 63)
        );
        assert_eq!(
            service.get_property_val_str("peer_id"),
            Some(peer_id.as_str())
        );
        assert!(service.get_property_val_str("node_id").is_none());
        assert_eq!(service.get_port(), 4790);
        Ok(())
    }

    fn mdns_info(instance: &str, peer_id: &str, addresses: &str) -> Result<ServiceInfo> {
        let properties = [("peer_id", peer_id), ("path", "/api/network/ws")];
        Ok(ServiceInfo::new(
            SERVICE_TYPE,
            instance,
            "fixture.local.",
            addresses,
            4790,
            &properties[..],
        )?)
    }

    #[test]
    fn m4_mdns_fullname_sources_survive_exact_removal_and_replace_invalid_updates() -> Result<()> {
        let target = format!("{}{}", "a".repeat(32), "b".repeat(32));
        let neighbor = format!("{}{}", "a".repeat(32), "c".repeat(32));
        let local = "0".repeat(64);
        let mut sources = MdnsSources::default();
        let epoch = sources.reset("fixture");
        let first = mdns_info("Device", &target, "192.0.2.1")?;
        let renamed = mdns_info("Device (2)", &target, "192.0.2.2")?;
        let same_url = mdns_info("Device (3)", &target, "192.0.2.1")?;
        let shared_prefix = mdns_info("Other", &neighbor, "192.0.2.3")?;
        for info in [&first, &renamed, &same_url, &shared_prefix] {
            assert!(sources.resolved(epoch, info, &local));
        }
        assert_eq!(sources.urls(&target).len(), 2);
        assert!(sources.removed(epoch, &renamed.get_fullname().to_ascii_uppercase(), "dns"));
        assert_eq!(
            sources.urls(&target),
            vec!["ws://192.0.2.1:4790/api/network/ws"]
        );
        assert!(sources.removed(epoch, first.get_fullname(), "dns"));
        assert_eq!(sources.urls(&target).len(), 1);
        assert!(sources.removed(epoch, same_url.get_fullname(), "dns"));
        assert!(sources.urls(&target).is_empty());
        assert_eq!(
            sources.urls(&neighbor),
            vec!["ws://192.0.2.3:4790/api/network/ws"]
        );
        let updated = mdns_info("Other", &target, "192.0.2.4")?;
        assert!(sources.resolved(epoch, &updated, &local));
        assert!(sources.urls(&neighbor).is_empty());
        assert_eq!(
            sources.urls(&target),
            vec!["ws://192.0.2.4:4790/api/network/ws"]
        );
        assert!(!sources.resolved(epoch, &mdns_info("Other", "invalid", "192.0.2.4")?, &local));
        assert!(sources.urls(&target).is_empty());
        assert!(sources.verify.is_empty());
        assert!(sources.resolved(epoch, &updated, &local));
        assert!(!sources.resolved(epoch, &mdns_info("Other", &target, "fe80::1")?, &local));
        assert!(sources.urls(&target).is_empty());
        assert!(sources.resolved(epoch, &updated, &local));
        assert!(!sources.resolved(epoch, &mdns_info("Other", &local, "192.0.2.4")?, &local));
        assert!(sources.sources.is_empty());
        Ok(())
    }

    #[test]
    fn m4_mdns_source_capacity_verify_rotation_and_old_epoch_cleanup_are_bounded() -> Result<()> {
        let target = "1".repeat(64);
        let local = "0".repeat(64);
        let state = Arc::new(StdMutex::new(MdnsSources::default()));
        let epoch = mdns_lock(&state).reset("fixture");
        let old_lease = MdnsLease {
            state: state.clone(),
            epoch,
        };
        for index in 0..MAX_MDNS_SOURCES {
            assert!(mdns_lock(&state).resolved(
                epoch,
                &mdns_info(&format!("Source-{index}"), &target, "192.0.2.1")?,
                &local
            ));
        }
        assert!(!mdns_lock(&state).resolved(
            epoch,
            &mdns_info("Overflow", &target, "192.0.2.2")?,
            &local
        ));
        assert!(mdns_lock(&state).resolved(
            epoch,
            &mdns_info("Source-0", &target, "192.0.2.3")?,
            &local
        ));
        let mut verified = HashSet::new();
        for _ in 0..MAX_MDNS_SOURCES {
            verified.insert(mdns_lock(&state).next_verify().unwrap().fullname);
        }
        assert_eq!(verified.len(), MAX_MDNS_SOURCES);
        assert!(
            verified
                .iter()
                .any(|fullname| fullname.starts_with("Source-0."))
        );
        assert_eq!(mdns_lock(&state).verify.len(), MAX_MDNS_SOURCES);
        let current = mdns_lock(&state).reset("interface_reset");
        assert!(!mdns_lock(&state).resolved(
            epoch,
            &mdns_info("Stale", &target, "192.0.2.1")?,
            &local
        ));
        assert!(mdns_lock(&state).resolved(
            current,
            &mdns_info("Current", &target, "192.0.2.4")?,
            &local
        ));
        drop(old_lease);
        assert_eq!(
            mdns_lock(&state).urls(&target),
            vec!["ws://192.0.2.4:4790/api/network/ws"]
        );
        let current_lease = MdnsLease {
            state: state.clone(),
            epoch: current,
        };
        drop(current_lease);
        assert!(mdns_lock(&state).urls(&target).is_empty());
        assert!(mdns_lock(&state).sources.is_empty());
        assert!(mdns_lock(&state).verify.is_empty());
        Ok(())
    }

    #[test]
    fn m4_mdns_registration_matches_fixed_wildcard_and_loopback_listeners() -> Result<()> {
        let peer_id = LocalIdentity::generate().peer_id()?;
        for address in ["192.0.2.7:4790", "[2001:db8::7]:4790"] {
            let listen: SocketAddr = address.parse()?;
            let service = mdns_service(&peer_id, listen)?.unwrap();
            assert!(!service.is_addr_auto());
            assert_eq!(
                service
                    .get_addresses()
                    .iter()
                    .copied()
                    .collect::<HashSet<_>>(),
                HashSet::from([listen.ip()])
            );
        }
        for address in ["0.0.0.0:4790", "[::]:4790"] {
            let service = mdns_service(&peer_id, address.parse()?)?.unwrap();
            assert!(service.is_addr_auto());
            assert!(service.get_addresses().is_empty());
        }
        for address in ["127.0.0.1:4790", "[::1]:4790"] {
            assert!(mdns_service(&peer_id, address.parse()?)?.is_none());
        }
        Ok(())
    }

    #[tokio::test]
    async fn m4_mdns_cleanup_waits_for_real_exit_ack_after_drop_and_again_fixture() -> Result<()> {
        for force_again in [0, 1] {
            let mut normal = MdnsGuard::new(ServiceDaemon::new()?);
            normal.force_again = force_again;
            normal.shutdown().await?;
            let (completed, closed) = oneshot::channel();
            let mut cancelled = MdnsGuard::new(ServiceDaemon::new()?);
            cancelled.force_again = force_again;
            cancelled.completed = Some(completed);
            drop(cancelled);
            assert_eq!(
                tokio::time::timeout(CONNECT_TIMEOUT, closed).await???,
                DaemonStatus::Shutdown
            );
        }
        eprintln!(
            "NET1_M4 mdns_cleanup_owner_actual_ack=true drop_actual_ack=true again_fixture=one_local_command_rejection native_reaper_actual_ack=true"
        );
        Ok(())
    }

    #[tokio::test]
    async fn m4_mdns_owner_restarts_only_after_exit_ack_and_abort_withdraws_sources() -> Result<()>
    {
        let (hub, _) = NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        let (first_started, first) = oneshot::channel();
        let (fail, failure) = oneshot::channel();
        hub.test_hooks.lock().unwrap().mdns_started = Some(first_started);
        hub.test_hooks.lock().unwrap().mdns_fail = Some(failure);
        let owner = tokio::spawn({
            let hub = hub.clone();
            async move { hub.mdns("127.0.0.1:4790".parse()?).await }
        });
        let (old_epoch, _old_daemon) = tokio::time::timeout(CONNECT_TIMEOUT, first).await??;
        let target = "1".repeat(64);
        assert!(mdns_lock(&hub.mdns).resolved(
            old_epoch,
            &mdns_info("Old", &target, "192.0.2.1")?,
            &hub.identity.peer_id()?
        ));
        let (closed, old_closed) = oneshot::channel();
        let (next_started, next) = oneshot::channel();
        let (dropped, current_closed) = oneshot::channel();
        hub.test_hooks.lock().unwrap().mdns_closed = Some(closed);
        hub.test_hooks.lock().unwrap().mdns_started = Some(next_started);
        hub.test_hooks.lock().unwrap().mdns_dropped = Some(dropped);
        fail.send(())
            .map_err(|_| anyhow::anyhow!("old generation already stopped"))?;
        tokio::time::timeout(CONNECT_TIMEOUT, old_closed).await??;
        let (current_epoch, _current_daemon) =
            tokio::time::timeout(CONNECT_TIMEOUT, next).await??;
        assert!(current_epoch > old_epoch);
        assert!(mdns_lock(&hub.mdns).urls(&target).is_empty());
        assert!(!mdns_lock(&hub.mdns).resolved(
            old_epoch,
            &mdns_info("Late", &target, "192.0.2.1")?,
            &hub.identity.peer_id()?
        ));
        assert!(mdns_lock(&hub.mdns).resolved(
            current_epoch,
            &mdns_info("Current", &target, "192.0.2.2")?,
            &hub.identity.peer_id()?
        ));
        owner.abort();
        assert!(owner.await.unwrap_err().is_cancelled());
        assert!(mdns_lock(&hub.mdns).urls(&target).is_empty());
        assert_eq!(
            tokio::time::timeout(CONNECT_TIMEOUT, current_closed).await???,
            DaemonStatus::Shutdown
        );
        eprintln!(
            "NET1_M4 injected_generation_error=true old_actual_exit_ack_before_new=true old_epoch_rejected=true owner_abort_sources=0 current_native_actual_exit_ack=true"
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires local multicast IPv4; run explicitly for NET-1"]
    async fn m4_mdns_healthy_unchanged_service_survives_real_verify_refresh() -> Result<()> {
        let publisher = MdnsGuard::new(ServiceDaemon::new()?);
        let browser = MdnsGuard::new(ServiceDaemon::new()?);
        publisher.get().disable_interface(IfKind::IPv6)?;
        browser.get().disable_interface(IfKind::IPv6)?;
        let target = LocalIdentity::generate().peer_id()?;
        let service =
            mdns_info(&format!("NET1-{}", &target[..12]), &target, "")?.enable_addr_auto();
        let expected = service.get_fullname().to_owned();
        let events = browser.get().browse(SERVICE_TYPE)?;
        publisher.get().register(service)?;
        let mut sources = MdnsSources::default();
        let epoch = sources.reset("real_fixture");
        let mut resolved = 0;
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            loop {
                if let ServiceEvent::ServiceResolved(info) = events.recv_async().await?
                    && info.get_fullname() == expected
                {
                    assert!(sources.resolved(epoch, &info, &"0".repeat(64)));
                    resolved += 1;
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        let settle = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(settle);
        loop {
            tokio::select! {
                _ = &mut settle => break,
                event = events.recv_async() => if let ServiceEvent::ServiceResolved(info) = event?
                    && info.get_fullname() == expected
                {
                    assert!(sources.resolved(epoch, &info, &"0".repeat(64)));
                    resolved += 1;
                },
            }
        }
        let initial = resolved;
        let mut refresh_resolved = 0;
        for _ in 0..3 {
            browser
                .get()
                .verify(expected.clone(), MDNS_VERIFY_TIMEOUT)?;
            let waited = tokio::time::sleep(MDNS_VERIFY_TIMEOUT + Duration::from_millis(400));
            tokio::pin!(waited);
            loop {
                tokio::select! {
                    _ = &mut waited => break,
                    event = events.recv_async() => match event? {
                        ServiceEvent::ServiceRemoved(_, fullname) if fullname.eq_ignore_ascii_case(&expected) => bail!("healthy unchanged service expired during actual verify refresh"),
                        ServiceEvent::ServiceResolved(info) if info.get_fullname() == expected => {
                            assert!(sources.resolved(epoch, &info, &"0".repeat(64)));
                            refresh_resolved += 1;
                        }
                        _ => {}
                    }
                }
            }
            assert!(!sources.urls(&target).is_empty());
        }
        assert_eq!(
            refresh_resolved, 0,
            "fixture changed while checking unchanged refresh"
        );
        publisher.shutdown().await?;
        browser
            .get()
            .verify(expected.clone(), MDNS_VERIFY_TIMEOUT)?;
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            loop {
                if let ServiceEvent::ServiceRemoved(_, fullname) = events.recv_async().await?
                    && fullname.eq_ignore_ascii_case(&expected)
                {
                    assert!(sources.removed(epoch, &fullname, "dns"));
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        assert!(sources.urls(&target).is_empty());
        drop(events);
        browser.shutdown().await?;
        eprintln!(
            "NET1_M4 real_mdns_initial_resolved={initial} verify_rounds=3 verify_timeout_ms=2000 refresh_resolved=0 healthy_source_retained=true publisher_stop_then_verify_exact_removed=true actual_native_exit_ack=true"
        );
        Ok(())
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
                .lookup_local(&owner.peer_id()?)
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
        changed.peer_id = "a".repeat(64);
        assert!(changed.verify(now_secs()?).is_err());
        assert!(validate_url("ws://user:password@example.com/api/network/ws").is_err());
        assert!(validate_url("file:///tmp/socket").is_err());
        assert!(validate_url("wss://relay.example/api/network/ws").is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn resolve_keeps_newest_cache_when_bootstraps_return_different_revisions() -> Result<()> {
        let target = LocalIdentity::generate();
        let target_id = target.peer_id()?;
        let (owner, _) = NetworkHub::new(target.clone(), NetworkConfig::default());
        let template = owner.local_record().await?;
        let now = now_secs()?;
        let make_record = |age: i64, port: u16| -> Result<Reachability> {
            let mut record = template.clone();
            record.issued_at = now - age;
            record.expires_at = record.issued_at + RECORD_TTL_SECS;
            record.direct_urls = vec![format!("ws://127.0.0.1:{port}/api/network/ws")];
            record.signature = target.sign_payload("reachability-v2", &record.unsigned()?)?;
            record.verify(now)?;
            Ok(record)
        };
        let stale = make_record(60, 4701)?;
        let fresh = make_record(40, 4702)?;
        let newest = make_record(20, 4703)?;
        let (stale_directory, _) =
            NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        let (fresh_directory, _) =
            NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        stale_directory.publish(stale.clone()).await?;
        fresh_directory.publish(fresh.clone()).await?;
        let mut tasks = JoinSet::new();
        let stale_url = serve(stale_directory, &mut tasks).await?;
        let fresh_url = serve(fresh_directory, &mut tasks).await?;
        for bootstrap_urls in [
            vec![fresh_url.clone(), stale_url.clone()],
            vec![stale_url, fresh_url],
        ] {
            for cached in [None, Some(newest.clone())] {
                let (resolver, _) = NetworkHub::new(
                    LocalIdentity::generate(),
                    NetworkConfig {
                        bootstrap_urls: bootstrap_urls.clone(),
                        ..Default::default()
                    },
                );
                if let Some(record) = &cached {
                    assert!(resolver.remember(record.clone()).await?);
                }
                let records: Vec<_> = resolver.resolve(&target_id).await?.collect().await;
                let expected = cached.as_ref().unwrap_or(&fresh);
                if cached.is_some() {
                    assert_eq!(
                        records.first().map(|record| &record.signature),
                        Some(&expected.signature)
                    );
                }
                assert!(
                    records
                        .iter()
                        .any(|record| record.signature == fresh.signature)
                );
                assert!(
                    records
                        .iter()
                        .any(|record| record.signature == stale.signature)
                );
                assert_eq!(
                    resolver
                        .state
                        .lock()
                        .await
                        .resolved
                        .get(&target_id)
                        .context("resolved record was not cached")?
                        .signature,
                    expected.signature
                );
                assert!(resolver.lookup_local(&target_id).await?.is_none());
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn net1_expiring_directory_record_does_not_abort_healthy_slow_directory() -> Result<()> {
        let identity = LocalIdentity::generate();
        let target_id = identity.peer_id()?;
        let (owner, _) = NetworkHub::new(identity.clone(), NetworkConfig::default());
        let healthy = owner.local_record().await?;
        let mut expiring = healthy.clone();
        expiring.expires_at = now_secs()? + 2;
        expiring.signature = identity.sign_payload("reachability-v2", &expiring.unsigned()?)?;
        let (fast, _) = NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        let (slow, _) = NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        fast.publish(expiring.clone()).await?;
        slow.publish(healthy.clone()).await?;
        let mut tasks = JoinSet::new();
        let fast_url = serve(fast.clone(), &mut tasks).await?;
        let slow_url = serve(slow.clone(), &mut tasks).await?;
        let (resolver, _) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                bootstrap_urls: vec![fast_url, slow_url],
                ..Default::default()
            },
        );
        let delayed = slow.state.lock().await;
        let (delivered, first) = oneshot::channel();
        let resolve = tokio::spawn({
            let resolver = resolver.clone();
            let target_id = target_id.clone();
            async move {
                let mut records = resolver.resolve(&target_id).await?;
                let first = records
                    .next()
                    .await
                    .context("fast record was not delivered")?;
                delivered
                    .send(first)
                    .map_err(|_| anyhow::anyhow!("observer closed"))?;
                Ok::<Vec<_>, anyhow::Error>(records.collect().await)
            }
        });
        let first = tokio::time::timeout(Duration::from_secs(5), first).await??;
        assert_eq!(first.signature, expiring.signature);
        assert!(now_secs()? < expiring.expires_at);
        while now_secs()? < expiring.expires_at {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(delayed);
        let records = resolve.await??;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].signature, healthy.signature);
        assert_eq!(
            slow.lookup_local(&target_id)
                .await?
                .context("healthy directory lost its record")?
                .signature,
            healthy.signature
        );
        assert!(resolver.lookup_local(&target_id).await?.is_none());
        assert_eq!(
            resolver
                .state
                .lock()
                .await
                .resolved
                .get(&target_id)
                .unwrap()
                .signature,
            healthy.signature
        );
        assert_eq!(resolver.admission.snapshot().total, 0);
        Ok(())
    }

    #[tokio::test]
    async fn directory_stream_delivers_before_slow_reply_and_drop_releases_sockets() -> Result<()> {
        let target = LocalIdentity::generate();
        let record = signed_record(&target, now_secs()?)?;
        let mut tasks = JoinSet::new();
        let slow_gate = Arc::new(Semaphore::new(0));
        let fast_gate = Arc::new(Semaphore::new(0));
        let (slow_url, mut slow_requests) =
            serve_directory(None, Some(slow_gate.clone()), &mut tasks).await?;
        let (fast_url, _) =
            serve_directory(Some(record.clone()), Some(fast_gate.clone()), &mut tasks).await?;
        let (resolver, _) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                bootstrap_urls: vec![slow_url, fast_url],
                ..Default::default()
            },
        );
        let mut records = resolver.resolve(&target.peer_id()?).await?;
        let (observed, received) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                async {
                    let request = slow_requests
                        .recv()
                        .await
                        .context("slow source was never queried")?;
                    ensure!(
                        matches!(
                            request,
                            DirectoryObservation::Request(Control::Lookup { .. })
                        ),
                        "expected lookup"
                    );
                    fast_gate.add_permits(1);
                    Ok::<_, anyhow::Error>(())
                },
                records.next()
            )
        })
        .await?;
        observed?;
        assert_eq!(
            received.context("healthy source was blocked")?.signature,
            record.signature
        );
        assert_eq!(slow_gate.available_permits(), 0);
        assert_eq!(
            (
                resolver.admission.snapshot().total,
                resolver.admission.snapshot().short
            ),
            (1, 1)
        );
        drop(records);
        assert_eq!(resolver.admission.snapshot().total, 0);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), slow_requests.recv()).await?,
            Some(DirectoryObservation::Closed)
        ));
        slow_gate.add_permits(1);
        Ok(())
    }

    #[tokio::test]
    async fn resolved_cache_is_private_bounded_and_full_cache_still_connects() -> Result<()> {
        let target_identity = LocalIdentity::generate();
        let caller_identity = LocalIdentity::generate();
        let (target, mut incoming) =
            NetworkHub::new(target_identity.clone(), NetworkConfig::default());
        let mut tasks = JoinSet::new();
        let target_url = serve(target, &mut tasks).await?;
        let mut record = signed_record(&target_identity, now_secs()?)?;
        record.direct_urls.push(target_url);
        record.signature = target_identity.sign_payload("reachability-v2", &record.unsigned()?)?;
        let (directory_url, _) = serve_directory(Some(record.clone()), None, &mut tasks).await?;
        let (resolver, _) = NetworkHub::new(
            caller_identity.clone(),
            NetworkConfig {
                bootstrap_urls: vec![directory_url],
                ..Default::default()
            },
        );
        let mut first = None;
        for index in 0..MAX_RECORDS {
            let identity = LocalIdentity::generate();
            let record = signed_record(&identity, now_secs()?)?;
            if index == 0 {
                first = Some((identity, record.clone()));
            }
            assert_eq!(
                resolver.publish(record.clone()).await?,
                PublishOutcome::Accepted
            );
            if index < MAX_RESOLVED {
                assert!(resolver.remember(record).await?);
            }
        }
        let (identity, mut updated) = first.context("missing full-table fixture")?;
        updated.issued_at += 1;
        updated.expires_at += 1;
        updated.signature = identity.sign_payload("reachability-v2", &updated.unsigned()?)?;
        assert_eq!(
            resolver.publish(updated.clone()).await?,
            PublishOutcome::Accepted
        );
        assert!(resolver.remember(updated.clone()).await?);
        assert_eq!(
            resolver
                .state
                .lock()
                .await
                .resolved
                .get(&updated.peer_id)
                .unwrap()
                .signature,
            updated.signature
        );
        assert_eq!(
            resolver.publish(record.clone()).await?,
            PublishOutcome::Full
        );
        assert!(!resolver.remember(record.clone()).await?);
        let records: Vec<_> = resolver.resolve(&record.peer_id).await?.collect().await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].signature, record.signature);
        assert!(resolver.lookup_local(&record.peer_id).await?.is_none());
        let (mut socket, mut client, mut accepted, mut server) = dial_test_noise(
            &resolver,
            &caller_identity,
            &target_identity,
            Service::Client,
            &mut incoming,
            b"test:directory",
        )
        .await?;
        encrypted_round_trip(&mut client, &mut server, &mut socket, &mut accepted.socket).await?;
        let state = resolver.state.lock().await;
        assert_eq!(state.records.len(), MAX_RECORDS);
        assert_eq!(state.resolved.len(), MAX_RESOLVED);
        assert!(!state.resolved.contains_key(&record.peer_id));
        eprintln!(
            "NET1_D1 cache_full public={} private={} cached=false noise=fixed_target",
            state.records.len(),
            state.resolved.len()
        );
        drop(state);
        drop(socket);
        assert_eq!(resolver.admission.snapshot().total, 0);
        Ok(())
    }

    #[tokio::test]
    async fn directory_rejects_wrong_identity_and_signature_without_losing_healthy_source()
    -> Result<()> {
        let identity = LocalIdentity::generate();
        let healthy = signed_record(&identity, now_secs()?)?;
        let wrong = signed_record(&LocalIdentity::generate(), now_secs()?)?;
        let mut invalid = healthy.clone();
        invalid.signature = "0".repeat(invalid.signature.len());
        let mut tasks = JoinSet::new();
        let mut urls = Vec::new();
        let mut requests = Vec::new();
        for record in [wrong.clone(), invalid, healthy.clone()] {
            let (url, observed) = serve_directory(Some(record), None, &mut tasks).await?;
            urls.push(url);
            requests.push(observed);
        }
        let (resolver, _) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                bootstrap_urls: urls,
                ..Default::default()
            },
        );
        let records: Vec<_> = resolver
            .resolve(&identity.peer_id()?)
            .await?
            .collect()
            .await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].signature, healthy.signature);
        for observed in &mut requests {
            assert!(matches!(
                observed.recv().await,
                Some(DirectoryObservation::Request(Control::Lookup { .. }))
            ));
        }
        let state = resolver.state.lock().await;
        assert_eq!(state.resolved.len(), 1);
        assert!(!state.resolved.contains_key(&wrong.peer_id));
        assert!(state.records.is_empty());
        assert_eq!(resolver.admission.snapshot().total, 0);
        Ok(())
    }

    #[tokio::test]
    async fn records_expiring_while_waiting_for_state_are_not_stored_or_delivered() -> Result<()> {
        let identity = LocalIdentity::generate();
        let mut record = signed_record(&identity, now_secs()?)?;
        record.expires_at = now_secs()? + 2;
        record.signature = identity.sign_payload("reachability-v2", &record.unsigned()?)?;
        let (hub, _) = NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        let held = hub.state.lock().await;
        let mut publish = Box::pin(hub.publish(record.clone()));
        let mut remember = Box::pin(hub.remember(record.clone()));
        assert!(futures_util::poll!(publish.as_mut()).is_pending());
        assert!(futures_util::poll!(remember.as_mut()).is_pending());
        while now_secs()? < record.expires_at {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(held);
        assert_eq!(
            publish.await.unwrap_err().to_string(),
            "reachability expired while waiting"
        );
        assert_eq!(
            remember.await.unwrap_err().to_string(),
            "reachability expired while waiting"
        );
        assert!(hub.state.lock().await.records.is_empty());
        assert!(hub.state.lock().await.resolved.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn delayed_local_candidate_expires_and_both_tables_reclaim_it() -> Result<()> {
        let identity = LocalIdentity::generate();
        let mut record = signed_record(&identity, now_secs()?)?;
        record.expires_at = now_secs()? + 2;
        record.signature = identity.sign_payload("reachability-v2", &record.unsigned()?)?;
        let (hub, _) = NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        assert_eq!(hub.publish(record.clone()).await?, PublishOutcome::Accepted);
        assert!(hub.remember(record.clone()).await?);
        let mut stream = hub.resolve(&record.peer_id).await?;
        while now_secs()? < record.expires_at {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(stream.next().await.is_none());
        let replacement = signed_record(&LocalIdentity::generate(), now_secs()?)?;
        assert_eq!(
            hub.publish(replacement.clone()).await?,
            PublishOutcome::Accepted
        );
        assert!(hub.remember(replacement).await?);
        let state = hub.state.lock().await;
        assert_eq!(state.records.len(), 1);
        assert_eq!(state.resolved.len(), 1);
        assert!(!state.records.contains_key(&record.peer_id));
        assert!(!state.resolved.contains_key(&record.peer_id));
        Ok(())
    }

    #[tokio::test]
    async fn authenticated_dial_budgets_are_shared_across_all_candidate_sources() -> Result<()> {
        let identity = LocalIdentity::generate();
        let target = identity.peer_id()?;
        let mut tasks = JoinSet::new();
        let (reject_url, mut attempts) = serve_directory(None, None, &mut tasks).await?;
        for relay_only in [false, true] {
            let mut directories = Vec::new();
            for group in 0..3 {
                let mut record = signed_record(&identity, now_secs()? - group)?;
                let urls = (0..8)
                    .map(|index| format!("{reject_url}/group-{group}-{index}"))
                    .collect::<Vec<_>>();
                if relay_only {
                    record.relay_urls = urls;
                } else {
                    record.direct_urls = urls;
                }
                record.signature = identity.sign_payload("reachability-v2", &record.unsigned()?)?;
                directories.push(serve_directory(Some(record), None, &mut tasks).await?.0);
            }
            let (resolver, _) = NetworkHub::new(
                LocalIdentity::generate(),
                NetworkConfig {
                    bootstrap_urls: directories,
                    ..Default::default()
                },
            );
            if !relay_only {
                seed_mdns_urls(
                    &resolver,
                    &target,
                    (0..16)
                        .map(|index| format!("{reject_url}/known-{index}"))
                        .collect(),
                );
            }
            let authenticated = std::sync::atomic::AtomicUsize::new(0);
            assert!(
                resolver
                    .dial_authenticated::<(), _, _>(&target, Service::Peer, None, |_| {
                        authenticated.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        async {
                            Err(AuthenticationFailure::Path(anyhow::anyhow!(
                                "control rejection must not authenticate"
                            )))
                        }
                    })
                    .await
                    .is_err()
            );
            assert_eq!(authenticated.load(std::sync::atomic::Ordering::Relaxed), 0);
            let mut direct = 0;
            let mut relays = 0;
            while let Ok(control) = attempts.try_recv() {
                match control {
                    DirectoryObservation::Request(Control::Direct { .. }) => direct += 1,
                    DirectoryObservation::Request(Control::Dial { .. }) => relays += 1,
                    _ => bail!("unexpected path attempt"),
                }
            }
            if relay_only {
                assert_eq!(
                    (direct, relays),
                    (0, 8),
                    "one relay budget shared by all records"
                );
            } else {
                assert_eq!(
                    (direct, relays),
                    (16, 0),
                    "one direct budget shared across mDNS and all directories"
                );
            }
            assert_eq!(resolver.admission.snapshot().total, 0);
            eprintln!(
                "NET1_M4 D1 relay_only={relay_only} direct_attempts={direct} relay_attempts={relays} control_only=true noise_calls=0"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn known_direct_and_relay_compete_with_one_shared_candidate_budget() -> Result<()> {
        let target_identity = LocalIdentity::generate();
        let caller_identity = LocalIdentity::generate();
        let (target, mut incoming) =
            NetworkHub::new(target_identity.clone(), NetworkConfig::default());
        let mut tasks = JoinSet::new();
        let target_url = serve(target, &mut tasks).await?;
        let (reject_url, mut attempts) = serve_directory(None, None, &mut tasks).await?;
        let (caller, _) = NetworkHub::new(caller_identity.clone(), NetworkConfig::default());
        let mut public = signed_record(&target_identity, now_secs()?)?;
        public.direct_urls = (0..4)
            .map(|index| format!("{reject_url}/public-{index}"))
            .collect();
        public.relay_urls.push(format!("{reject_url}/relay"));
        public.signature = target_identity.sign_payload("reachability-v2", &public.unsigned()?)?;
        let mut private = signed_record(&target_identity, now_secs()? - 1)?;
        private.direct_urls.push(target_url.clone());
        private.signature =
            target_identity.sign_payload("reachability-v2", &private.unsigned()?)?;
        assert_eq!(caller.publish(public).await?, PublishOutcome::Accepted);
        assert!(caller.remember(private.clone()).await?);
        seed_mdns_urls(
            &caller,
            &private.peer_id,
            (0..8)
                .map(|index| format!("{reject_url}/mdns-{index}"))
                .collect(),
        );
        let (mut socket, mut client, mut accepted, mut server) = dial_test_noise(
            &caller,
            &caller_identity,
            &target_identity,
            Service::Client,
            &mut incoming,
            b"test:directory",
        )
        .await?;
        assert_eq!(socket.route(), Some(format!("direct:{target_url}")));
        let mut direct_attempts = 0;
        let mut relay_attempts = 0;
        while let Ok(control) = attempts.try_recv() {
            match control {
                DirectoryObservation::Request(Control::Direct { .. }) => direct_attempts += 1,
                DirectoryObservation::Request(Control::Dial { .. }) => relay_attempts += 1,
                _ => bail!("unexpected candidate control"),
            }
        }
        assert!(direct_attempts <= 12);
        assert!(relay_attempts <= 1);
        encrypted_round_trip(&mut client, &mut server, &mut socket, &mut accepted.socket).await?;
        Ok(())
    }

    #[tokio::test]
    async fn directory_operations_have_one_deadline_across_welcome_and_reply() -> Result<()> {
        #[derive(Clone)]
        struct DelayedDirectory {
            record: Reachability,
            queried: mpsc::UnboundedSender<&'static str>,
        }
        async fn upgrade(
            AxumState(directory): AxumState<DelayedDirectory>,
            ws: WebSocketUpgrade,
        ) -> impl axum::response::IntoResponse {
            ws.on_upgrade(|socket| async move {
                let _ = async {
                    let mut socket = NetworkSocket::server(socket);
                    tokio::time::sleep(Duration::from_secs(6)).await;
                    send_control(
                        &mut socket,
                        &Control::Welcome {
                            nonce: random_token(),
                        },
                    )
                    .await?;
                    let (operation, reply) = match receive_control(&mut socket).await? {
                        Control::Lookup { .. } => (
                            "lookup",
                            Control::Record {
                                record: Some(directory.record),
                            },
                        ),
                        Control::Publish { record } => {
                            record.verify(now_secs()?)?;
                            ("publish", Control::Published)
                        }
                        _ => bail!("unexpected delayed directory operation"),
                    };
                    let _ = directory.queried.send(operation);
                    tokio::time::sleep(Duration::from_secs(6)).await;
                    send_control(&mut socket, &reply).await
                }
                .await;
            })
        }
        let identity = LocalIdentity::generate();
        let record = signed_record(&identity, now_secs()?)?;
        let (queried, mut observed) = mpsc::unbounded_channel();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}/api/network/ws", listener.local_addr()?);
        let router = Router::new()
            .route("/api/network/ws", get(upgrade))
            .with_state(DelayedDirectory { record, queried });
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let (resolver, _) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                bootstrap_urls: vec![url.clone()],
                ..Default::default()
            },
        );
        let records = resolver.resolve(&identity.peer_id()?).await?;
        let (records, publication) =
            tokio::time::timeout(CONNECT_TIMEOUT + Duration::from_secs(1), async {
                tokio::join!(records.collect::<Vec<_>>(), resolver.publish_at(&url))
            })
            .await?;
        assert!(records.is_empty());
        assert_eq!(
            publication.unwrap_err().to_string(),
            "directory publication timed out"
        );
        let mut operations = vec![observed.try_recv()?, observed.try_recv()?];
        operations.sort();
        assert_eq!(operations, vec!["lookup", "publish"]);
        assert_eq!(resolver.admission.snapshot().total, 0);
        eprintln!(
            "NET1_D2 query_and_publish_deadline=10s welcome_delay=6s reply_delay=6s result=rejected permits=reclaimed"
        );
        Ok(())
    }

    #[tokio::test]
    async fn publication_preserves_relay_changes_during_an_inflight_publish() -> Result<()> {
        let mut tasks = JoinSet::new();
        let gate = Arc::new(Semaphore::new(0));
        let (url, mut published) = serve_publication_probe(gate.clone(), &mut tasks).await?;
        let (hub, _) = NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        let publication = tokio::spawn({
            let hub = hub.clone();
            async move { hub.maintain_publication(&url).await }
        });
        let first = tokio::time::timeout(Duration::from_secs(5), published.recv())
            .await?
            .context("initial publish missing")?;
        assert!(first.relay_urls.is_empty());
        let relay = "ws://127.0.0.1:9/api/network/ws";
        let transient = "ws://127.0.0.1:10/api/network/ws";
        hub.set_relay_active(transient, true).await;
        hub.set_relay_active(relay, true).await;
        hub.set_relay_active(transient, false).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), published.recv())
                .await
                .is_err(),
            "more than one publish was in flight"
        );
        gate.add_permits(1);
        let second = tokio::time::timeout(Duration::from_secs(5), published.recv())
            .await?
            .context("relay change during publish was lost")?;
        assert_eq!(second.relay_urls, vec![relay]);
        gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(5), async {
            while hub.admission.snapshot().short != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), published.recv())
                .await
                .is_err(),
            "coalesced changes triggered an extra publish"
        );
        publication.abort();
        assert!(publication.await.unwrap_err().is_cancelled());
        assert_eq!(hub.admission.snapshot().total, 0);
        eprintln!("NET1_D2 publication_inflight=1 relay_change_during_publish=observed");
        Ok(())
    }

    #[tokio::test]
    async fn relay_refusal_does_not_republish_and_duplicate_entries_do_not_duplicate_tasks()
    -> Result<()> {
        let mut tasks = JoinSet::new();
        let (directory, _) = NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        let url = serve(directory.clone(), &mut tasks).await?;
        let (hub, _) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                bootstrap_urls: vec![url.clone(), url.clone()],
                ..Default::default()
            },
        );
        let peer_id = hub.identity.peer_id()?;
        let run = tokio::spawn({
            let hub = hub.clone();
            async move { hub.run("127.0.0.1:0".parse()?).await }
        });
        tokio::time::sleep(Duration::from_secs(11)).await;
        assert_eq!(
            directory.admission.snapshot().inbound_requests,
            4,
            "one Publish plus three refused Reserve requests"
        );
        assert!(directory.lookup_local(&peer_id).await?.is_some());
        assert!(relay_lock(&directory.relay).reservations.is_empty());
        assert!(hub.state.lock().await.active_relays.is_empty());
        run.abort();
        assert!(run.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), async {
            while hub.admission.snapshot().total != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        eprintln!(
            "NET1_D2 duplicate_urls=2 unique_entries=1 observed_requests=4 control_classification=not_instrumented"
        );
        Ok(())
    }

    #[tokio::test]
    async fn reservation_rejects_publish_and_public_full_publish_is_not_acknowledged() -> Result<()>
    {
        let mut tasks = JoinSet::new();
        let (relay, _) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                relay_enabled: true,
                ..Default::default()
            },
        );
        for _ in 0..MAX_RECORDS {
            let record = signed_record(&LocalIdentity::generate(), now_secs()?)?;
            assert_eq!(relay.publish(record).await?, PublishOutcome::Accepted);
        }
        let url = serve(relay.clone(), &mut tasks).await?;
        let identity = LocalIdentity::generate();
        let mut socket = reserve_at(&url, &identity).await?;
        let record = signed_record(&identity, now_secs()?)?;
        send_control(
            &mut socket,
            &Control::Publish {
                record: record.clone(),
            },
        )
        .await?;
        let (mut public, _) = open_control(&url).await?;
        send_control(
            &mut public,
            &Control::Publish {
                record: record.clone(),
            },
        )
        .await?;
        assert!(
            receive_control(&mut public).await.is_err(),
            "full public Publish must not ACK success"
        );
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            while let Some(frame) = socket.next().await {
                match frame {
                    Err(_) | Ok(Message::Close(_)) => break,
                    Ok(Message::Ping(_) | Message::Pong(_)) => {}
                    _ => bail!("unexpected reservation response"),
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        assert!(
            !relay_lock(&relay.relay)
                .reservations
                .contains_key(&record.peer_id)
        );
        eprintln!("NET1_D2 public_full_ack=false reservation_publish_rejected=true");
        Ok(())
    }

    #[tokio::test]
    async fn full_data_capacity_refuses_only_the_incoming_and_recovers_on_the_same_reservation()
    -> Result<()> {
        let mut tasks = JoinSet::new();
        let (relay, _) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                relay_enabled: true,
                ..Default::default()
            },
        );
        let url = serve(relay.clone(), &mut tasks).await?;
        let identity = LocalIdentity::generate();
        let target_id = identity.peer_id()?;
        let (target, mut incoming) = NetworkHub::new(identity.clone(), NetworkConfig::default());
        let maintenance = tokio::spawn({
            let target = target.clone();
            let url = url.clone();
            async move { target.maintain_relay(&url).await }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !target.state.lock().await.active_relays.contains(&url) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let generation = relay_lock(&relay.relay)
            .reservations
            .get(&target_id)
            .context("reservation missing")?
            .id
            .clone();
        let mut held = hold_outbound_data(&target, MAX_ESTABLISHED - 1, &mut tasks).await?;
        assert_eq!(target.admission.snapshot().long, MAX_ESTABLISHED);
        let mut controls = Vec::new();
        for index in 0..16 {
            controls.push(
                target
                    .open(&held.urls[index / 4], SocketClass::Control)
                    .await?
                    .0,
            );
        }
        assert_eq!(target.admission.snapshot().total, MAX_SOCKETS);
        let error = target
            .open(&held.urls[4], SocketClass::Control)
            .await
            .err()
            .context("129th socket was admitted")?;
        assert_eq!(error.downcast_ref::<Denied>(), Some(&Denied::GlobalSocket));
        drop(controls.pop());
        let error = target
            .open(&held.urls[4], SocketClass::Data)
            .await
            .err()
            .context("113th established socket was admitted")?;
        assert_eq!(
            error.downcast_ref::<Denied>(),
            Some(&Denied::EstablishedSocket)
        );
        drop(target.open(&held.urls[4], SocketClass::Control).await?);
        drop(controls);
        assert_eq!(
            (
                target.admission.snapshot().short,
                target.admission.snapshot().long
            ),
            (0, MAX_ESTABLISHED)
        );
        let (mut control, _) = target.open(&url, SocketClass::Control).await?;
        send_control(
            &mut control,
            &Control::Lookup {
                target: target_id.clone(),
            },
        )
        .await?;
        assert!(matches!(
            receive_control(&mut control).await?,
            Control::Record { .. }
        ));
        drop(control);
        let (mut caller, _) = open_control(&url).await?;
        send_control(
            &mut caller,
            &Control::Dial {
                target: target_id.clone(),
                service: Service::Client,
            },
        )
        .await?;
        assert!(
            tokio::time::timeout(
                CONNECT_TIMEOUT + Duration::from_secs(1),
                receive_control(&mut caller)
            )
            .await?
            .is_err()
        );
        assert!(!maintenance.is_finished());
        assert!(target.state.lock().await.active_relays.contains(&url));
        assert_eq!(
            relay_lock(&relay.relay)
                .reservations
                .get(&target_id)
                .context("full Data tore down reservation")?
                .id,
            generation
        );
        drop(held.clients.pop());
        drop(held.servers.pop());
        let caller_identity = LocalIdentity::generate();
        let mut caller = connect_relay(&url, &target_id, Service::Client).await?;
        let mut accepted = incoming
            .recv()
            .await
            .context("released Data slot did not recover Incoming")?;
        noise_round_trip(
            &caller_identity,
            &identity,
            &mut caller,
            &mut accepted.socket,
        )
        .await?;
        assert_eq!(
            relay_lock(&relay.relay)
                .reservations
                .get(&target_id)
                .unwrap()
                .id,
            generation
        );
        drop(caller);
        drop(accepted);
        drop(held);
        maintenance.abort();
        assert!(maintenance.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), async {
            while target.admission.snapshot().total != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        eprintln!(
            "NET1_A1 actual_peak_sockets=128 established=112 sink_hubs=7 incoming_busy_keeps_generation=true released_slot_noise=true controls_progress=true"
        );
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
        let publication_gate = Arc::new(Semaphore::new(0));
        let (slow_directory, mut slow_publications) =
            serve_publication_probe(publication_gate.clone(), &mut tasks).await?;
        let (target, mut target_incoming) = NetworkHub::new(
            target_identity.clone(),
            NetworkConfig {
                bootstrap_urls: vec![relay_url.clone(), slow_directory],
                advertise_urls: vec!["ws://127.0.0.1:1/api/network/ws".to_owned()],
                ..Default::default()
            },
        );
        let target_id = target_identity.peer_id()?;
        let run_started = Instant::now();
        let target_run = target.clone();
        tasks.spawn(async move {
            let _ = target_run.run("127.0.0.1:0".parse().unwrap()).await;
        });
        assert!(
            tokio::time::timeout(Duration::from_secs(5), slow_publications.recv())
                .await?
                .is_some()
        );
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
        let published = relay
            .lookup_local(&target_id)
            .await?
            .context("target reachability record missing")?;
        assert!(published.relay_urls.iter().any(|url| url == &relay_url));
        let (caller, _) = NetworkHub::new(
            caller_identity.clone(),
            NetworkConfig {
                bootstrap_urls: vec![relay_url],
                ..Default::default()
            },
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = target.admission.snapshot();
                if (snapshot.short, snapshot.long) == (1, 1) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let (mut caller_socket, mut client, mut incoming, mut server) = dial_test_noise(
            &caller,
            &caller_identity,
            &target_identity,
            Service::Client,
            &mut target_incoming,
            b"test:relay",
        )
        .await?;
        tokio::task::yield_now().await;
        assert_eq!(
            (
                target.admission.snapshot().short,
                target.admission.snapshot().long
            ),
            (1, 2)
        );
        assert_eq!(
            (
                caller.admission.snapshot().short,
                caller.admission.snapshot().long
            ),
            (0, 1)
        );
        assert_eq!(incoming.service, Service::Client);
        assert_ne!(relay_identity.peer_id()?, target_id);
        assert_ne!(relay_identity.peer_id()?, caller_identity.peer_id()?);

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
        assert_eq!(
            publication_gate.available_permits(),
            0,
            "slow Publish was not the reservation's progress dependency"
        );
        assert!(
            run_started.elapsed() < CONNECT_TIMEOUT,
            "Noise completed after the slow publication deadline"
        );
        assert_eq!(
            (
                target.admission.snapshot().short,
                target.admission.snapshot().long
            ),
            (1, 2)
        );
        publication_gate.add_permits(1);
        Ok(())
    }

    #[tokio::test]
    async fn m4_wrong_noise_path_loses_and_healthy_relay_beats_a_stalled_direct() -> Result<()> {
        let mut tasks = JoinSet::new();
        let target_identity = LocalIdentity::generate();
        let caller_identity = LocalIdentity::generate();
        let wrong_identity = LocalIdentity::generate();
        let (wrong, mut wrong_incoming) =
            NetworkHub::new(wrong_identity.clone(), NetworkConfig::default());
        let wrong_url = serve(wrong, &mut tasks).await?;
        let (wrong_done, wrong_observed) = oneshot::channel();
        tasks.spawn(async move {
            let result = async {
                let mut accepted = wrong_incoming
                    .recv()
                    .await
                    .context("wrong endpoint not attempted")?;
                let mut handshake = Handshake::new(
                    false,
                    &wrong_identity.secret_key_hex,
                    None,
                    b"test:directory",
                )
                .map_err(anyhow::Error::msg)?;
                handshake
                    .read(&next_binary(&mut accepted.socket).await?)
                    .map_err(anyhow::Error::msg)?;
                send_frame(
                    &mut accepted.socket,
                    Message::Binary(handshake.write(&[]).map_err(anyhow::Error::msg)?.into()),
                )
                .await?;
                let _ = wrong_done.send(());
                Ok::<_, anyhow::Error>(())
            }
            .await;
            assert!(result.is_ok(), "{result:?}");
        });
        let (slow, mut slow_incoming) =
            NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        let slow_url = serve(slow, &mut tasks).await?;
        let (relay, _) = NetworkHub::new(
            LocalIdentity::generate(),
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
                advertise_urls: vec![wrong_url.clone(), slow_url.clone()],
                ..Default::default()
            },
        );
        let runner = target.clone();
        tasks.spawn(async move {
            let _ = runner.run("127.0.0.1:0".parse().unwrap()).await;
        });
        let target_id = target_identity.peer_id()?;
        let record = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(record) = relay.lookup_local(&target_id).await?
                    && !record.relay_urls.is_empty()
                {
                    break Ok::<_, anyhow::Error>(record);
                }
                tokio::task::yield_now().await;
            }
        })
        .await??;
        let (caller, _) = NetworkHub::new(caller_identity.clone(), NetworkConfig::default());
        assert!(caller.remember(record).await?);
        // Seed only ordering history: the actual wrong endpoint must still pass
        // target-pinned Noise before it can win, and is rejected below.
        caller
            .record_path(&target_id, &format!("direct:{wrong_url}"), true)
            .await;
        let (forward, mut accepted) = mpsc::channel(1);
        let (slow_seen, slow_observed) = oneshot::channel();
        tasks.spawn(async move {
            let stalled = slow_incoming
                .recv()
                .await
                .expect("slow direct was not started");
            let _ = slow_seen.send(stalled);
            let target = target_incoming.recv().await.expect("relay target missing");
            forward.send(target).await.expect("relay responder dropped");
        });
        let started = Instant::now();
        let (mut socket, mut client, mut incoming, mut server) = tokio::time::timeout(
            Duration::from_secs(5),
            dial_test_noise(
                &caller,
                &caller_identity,
                &target_identity,
                Service::Client,
                &mut accepted,
                b"test:directory",
            ),
        )
        .await??;
        wrong_observed.await?;
        let stalled = slow_observed.await?;
        assert_eq!(socket.route(), Some(format!("relay:{relay_url}")));
        assert!(started.elapsed() < CONNECT_TIMEOUT);
        assert_eq!(
            caller.admission.snapshot().total,
            1,
            "losing local socket permits must drop before winner returns"
        );
        let state = caller.state.lock().await;
        assert!(
            state.paths[&(target_id.clone(), format!("direct:{wrong_url}"))]
                .failed_until
                .is_some()
        );
        assert!(
            !state
                .paths
                .contains_key(&(target_id.clone(), format!("direct:{slow_url}"))),
            "cancelled loser is not a failed route"
        );
        assert!(
            state.paths[&(target_id, format!("relay:{relay_url}"))]
                .authenticated_at
                .is_some()
        );
        drop(state);
        encrypted_round_trip(&mut client, &mut server, &mut socket, &mut incoming.socket).await?;
        drop(socket);
        assert_eq!(caller.admission.snapshot().total, 0);
        drop(stalled);
        eprintln!(
            "NET1_M4 wrong_noise_identity_rejected=true slow_direct_started=true healthy_relay_authenticated=true local_loser_permits_released=true cancelled_loser_not_penalized=true"
        );
        Ok(())
    }

    #[tokio::test]
    async fn m4_peer_revocation_during_target_noise_is_terminal_and_does_not_penalize_path()
    -> Result<()> {
        let owner = LocalIdentity::generate();
        let target_identity = LocalIdentity::generate();
        let cluster = owner.fixture_cluster_id()?;
        let roles = vec![
            crate::model::NodeRole::Replica,
            crate::model::NodeRole::Relay,
        ];
        let mut store = crate::store::Store::open(":memory:")?;
        store.install_trust_bundle(
            &cluster,
            &owner.sign_cluster_fixture(
                1,
                vec![
                    owner.certificate(roles.clone())?,
                    target_identity.certificate(roles.clone())?,
                ],
            )?,
        )?;
        let store = Arc::new(Mutex::new(store));
        let (peer, _) = crate::peer::PeerHub::new(owner.clone(), store.clone(), &roles);
        let (target, mut incoming) =
            NetworkHub::new(target_identity.clone(), NetworkConfig::default());
        let mut tasks = JoinSet::new();
        let url = serve(target, &mut tasks).await?;
        let (transport, _) = NetworkHub::new(owner.clone(), NetworkConfig::default());
        let mut record = signed_record(&target_identity, now_secs()?)?;
        record.direct_urls.push(url);
        record.signature = target_identity.sign_payload("reachability-v2", &record.unsigned()?)?;
        assert!(transport.remember(record).await?);
        let (entered, observed) = oneshot::channel();
        let (resume, gate) = oneshot::channel();
        let (closed, closed_observed) = oneshot::channel();
        let server_owner = owner.clone();
        let server_identity = target_identity.clone();
        tasks.spawn(async move {
            let result = async {
                let mut accepted = incoming.recv().await.context("target path missing")?;
                let mut handshake = Handshake::new(
                    false,
                    &server_identity.secret_key_hex,
                    Some(server_owner.peer_id()?),
                    b"vibeface:peer:v2",
                )
                .map_err(anyhow::Error::msg)?;
                assert!(
                    handshake
                        .read(&next_binary(&mut accepted.socket).await?)
                        .map_err(anyhow::Error::msg)?
                        .is_empty()
                );
                let _ = entered.send(());
                gate.await?;
                send_frame(
                    &mut accepted.socket,
                    Message::Binary(handshake.write(&[]).map_err(anyhow::Error::msg)?.into()),
                )
                .await?;
                let rejected_before_third = next_binary(&mut accepted.socket).await.is_err();
                let _ = closed.send(rejected_before_third);
                Ok::<_, anyhow::Error>(())
            }
            .await;
            assert!(result.is_ok(), "{result:?}");
        });
        let driver_peer = peer.clone();
        let driver_cluster = cluster.clone();
        let driver_target = target_identity.peer_id()?;
        let driver_transport = transport.clone();
        let driver = tokio::spawn(async move {
            driver_peer
                .connect_target(
                    driver_cluster,
                    driver_target,
                    driver_transport,
                    None,
                    tokio::time::Instant::now() + DIAL_TIMEOUT,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), observed).await??;
        let mut revoked = target_identity.certificate(roles.clone())?;
        revoked.revoked = true;
        store.lock().await.install_trust_bundle(
            &cluster,
            &owner.sign_cluster_fixture(2, vec![owner.certificate(roles)?, revoked])?,
        )?;
        resume
            .send(())
            .map_err(|_| anyhow::anyhow!("target handshake gate dropped"))?;
        let error = tokio::time::timeout(Duration::from_secs(2), driver)
            .await??
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::peer::AuthorizationDenied>()
                .is_some(),
            "{error:#}"
        );
        assert!(tokio::time::timeout(Duration::from_secs(2), closed_observed).await??);
        assert!(peer.online(&cluster).await.is_empty());
        assert_eq!(transport.admission.snapshot().total, 0);
        assert!(
            transport.state.lock().await.paths.is_empty(),
            "current membership denial is terminal, not a route penalty"
        );
        eprintln!(
            "NET1_M4 real_peer_noise_started=true store_target_revoked_before_response=true typed_authorization_terminal=true no_link_installed=true local_admission_released=true no_path_penalty=true"
        );
        Ok(())
    }

    #[tokio::test]
    async fn m4_total_dial_deadline_cancels_a_stalled_callback_and_releases_socket() -> Result<()> {
        let target_identity = LocalIdentity::generate();
        let (target, mut incoming) =
            NetworkHub::new(target_identity.clone(), NetworkConfig::default());
        let mut tasks = JoinSet::new();
        let url = serve(target, &mut tasks).await?;
        let (caller, _) = NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        seed_mdns_urls(&caller, &target_identity.peer_id()?, vec![url]);
        let (entered, mut entered_rx) = mpsc::unbounded_channel();
        let started = Instant::now();
        let target_id = target_identity.peer_id()?;
        let mut dial = Box::pin(caller.dial_authenticated::<(), _, _>(
            &target_id,
            Service::Client,
            None,
            |socket| {
                let entered = entered.clone();
                async move {
                    let _ = entered.send(());
                    std::future::pending::<()>().await;
                    Ok((socket, ()))
                }
            },
        ));
        tokio::select! {
            signal = entered_rx.recv() => assert!(signal.is_some()),
            _ = &mut dial => panic!("dial completed before stalled callback"),
        }
        let accepted = incoming
            .recv()
            .await
            .context("stalled callback socket missing")?;
        assert_eq!(caller.admission.snapshot().total, 1);
        let error = dial.await.err().expect("stalled callback unexpectedly won");
        assert!(error.to_string().contains("deadline exceeded"));
        assert!(started.elapsed() >= DIAL_TIMEOUT);
        assert_eq!(caller.admission.snapshot().total, 0);
        assert!(
            caller.state.lock().await.paths.is_empty(),
            "deadline cancellation is not a path failure"
        );
        drop(accepted);
        eprintln!(
            "NET1_M4 deadline_seconds=15 fixture_callback_stalled=true local_socket_released=true cancelled_path_not_penalized=true"
        );
        Ok(())
    }

    #[tokio::test]
    async fn m4_withdrawn_source_is_rechecked_when_the_attempt_is_first_polled() -> Result<()> {
        let identity = LocalIdentity::generate();
        let target = identity.peer_id()?;
        let (hub, _) = NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        let url = "ws://127.0.0.1:1/api/network/ws".to_owned();
        seed_mdns_urls(&hub, &target, vec![url.clone()]);
        let candidate = PathCandidate {
            explicit: false,
            url,
            relay: false,
            mdns: true,
            record_expires_at: None,
        };
        assert!(candidate.live(&hub, &target));
        async fn authenticate(
            _: NetworkSocket,
        ) -> std::result::Result<(NetworkSocket, ()), AuthenticationFailure> {
            panic!("withdrawn source must not open a socket or authenticate");
        }
        let attempt = hub.attempt_path(candidate, &target, Service::Peer, &authenticate);
        mdns_lock(&hub.mdns).sources.clear();
        assert!(matches!(
            attempt.await.1,
            Err(AuthenticationFailure::SourceGone)
        ));
        assert_eq!(hub.admission.snapshot().total, 0);
        assert!(hub.state.lock().await.paths.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn m4_fast_directory_direct_authenticates_while_known_noise_and_other_directory_stall()
    -> Result<()> {
        let mut tasks = JoinSet::new();
        let target_identity = LocalIdentity::generate();
        let caller_identity = LocalIdentity::generate();
        let (target, mut incoming) =
            NetworkHub::new(target_identity.clone(), NetworkConfig::default());
        let url = serve(target, &mut tasks).await?;
        let (stalled, mut stalled_incoming) =
            NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        let stalled_url = serve(stalled, &mut tasks).await?;
        let mut record = signed_record(&target_identity, now_secs()?)?;
        record.direct_urls.push(url.clone());
        record.signature = target_identity.sign_payload("reachability-v2", &record.unsigned()?)?;
        let fast_gate = Arc::new(Semaphore::new(0));
        let slow_gate = Arc::new(Semaphore::new(0));
        let (fast_url, mut fast_requests) =
            serve_directory(Some(record), Some(fast_gate.clone()), &mut tasks).await?;
        let (slow_url, mut slow_requests) =
            serve_directory(None, Some(slow_gate.clone()), &mut tasks).await?;
        let (caller, _) = NetworkHub::new(
            caller_identity.clone(),
            NetworkConfig {
                bootstrap_urls: vec![fast_url, slow_url],
                ..Default::default()
            },
        );
        let target_id = target_identity.peer_id()?;
        seed_mdns_urls(&caller, &target_id, vec![stalled_url.clone()]);
        let release_fast = async {
            let mut stalled = stalled_incoming
                .recv()
                .await
                .context("known direct missing")?;
            let _actual_noise_first = next_binary(&mut stalled.socket).await?;
            assert!(matches!(
                fast_requests.recv().await,
                Some(DirectoryObservation::Request(Control::Lookup { .. }))
            ));
            assert!(matches!(
                slow_requests.recv().await,
                Some(DirectoryObservation::Request(Control::Lookup { .. }))
            ));
            fast_gate.add_permits(1);
            Ok::<_, anyhow::Error>(stalled)
        };
        let dial = dial_test_noise(
            &caller,
            &caller_identity,
            &target_identity,
            Service::Client,
            &mut incoming,
            b"test:directory",
        );
        let ((mut socket, mut client, mut accepted, mut server), stalled) =
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::try_join!(dial, release_fast)
            })
            .await??;
        assert_eq!(socket.route(), Some(format!("direct:{url}")));
        assert_eq!(
            slow_gate.available_permits(),
            0,
            "slow directory never opened"
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), slow_requests.recv()).await?,
            Some(DirectoryObservation::Closed)
        ));
        assert_eq!(caller.admission.snapshot().total, 1);
        assert!(
            !caller
                .state
                .lock()
                .await
                .paths
                .contains_key(&(target_id, format!("direct:{stalled_url}")))
        );
        encrypted_round_trip(&mut client, &mut server, &mut socket, &mut accepted.socket).await?;
        drop(stalled);
        eprintln!(
            "NET1_M4 known_direct_noise_stalled=true slow_directory_gate_closed=true fast_directory_healthy_direct_noise_complete=true did_not_wait_for_slow_directory=true"
        );
        Ok(())
    }

    #[tokio::test]
    async fn m4_late_relay_preempts_one_of_two_stalled_direct_attempts() -> Result<()> {
        let mut tasks = JoinSet::new();
        let target_identity = LocalIdentity::generate();
        let caller_identity = LocalIdentity::generate();
        let mut stalled_receivers = Vec::new();
        let mut stalled_urls = Vec::new();
        for _ in 0..2 {
            let (hub, receive) =
                NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
            stalled_urls.push(serve(hub, &mut tasks).await?);
            stalled_receivers.push(receive);
        }
        let (relay, _) = NetworkHub::new(
            LocalIdentity::generate(),
            NetworkConfig {
                relay_enabled: true,
                ..Default::default()
            },
        );
        let relay_url = serve(relay.clone(), &mut tasks).await?;
        let (target, mut incoming) = NetworkHub::new(
            target_identity.clone(),
            NetworkConfig {
                bootstrap_urls: vec![relay_url.clone()],
                advertise_urls: stalled_urls.clone(),
                ..Default::default()
            },
        );
        let runner = target.clone();
        tasks.spawn(async move {
            let _ = runner.run("127.0.0.1:0".parse().unwrap()).await;
        });
        let target_id = target_identity.peer_id()?;
        let record = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(record) = relay.lookup_local(&target_id).await?
                    && !record.relay_urls.is_empty()
                {
                    break Ok::<_, anyhow::Error>(record);
                }
                tokio::task::yield_now().await;
            }
        })
        .await??;
        let gate = Arc::new(Semaphore::new(0));
        let (directory, mut requests) =
            serve_directory(Some(record), Some(gate.clone()), &mut tasks).await?;
        let (caller, _) = NetworkHub::new(
            caller_identity.clone(),
            NetworkConfig {
                bootstrap_urls: vec![directory],
                ..Default::default()
            },
        );
        let held_capacity = (0..MAX_ESTABLISHED - 2)
            .map(|_| caller.outbound(SocketClass::Data))
            .collect::<Result<Vec<_>>>()?;
        seed_mdns_urls(&caller, &target_id, stalled_urls.clone());
        let release_relay = async {
            let mut stalled = Vec::new();
            for receive in &mut stalled_receivers {
                let mut accepted = receive
                    .recv()
                    .await
                    .context("second slow direct never started")?;
                let _actual_noise_first = next_binary(&mut accepted.socket).await?;
                stalled.push(accepted);
            }
            assert_eq!(
                caller.admission.snapshot().long,
                MAX_ESTABLISHED,
                "two real data attempts occupy both slots before relay arrival"
            );
            assert!(matches!(
                requests.recv().await,
                Some(DirectoryObservation::Request(Control::Lookup { .. }))
            ));
            gate.add_permits(1);
            Ok::<_, anyhow::Error>(stalled)
        };
        let dial = dial_test_noise(
            &caller,
            &caller_identity,
            &target_identity,
            Service::Client,
            &mut incoming,
            b"test:directory",
        );
        let ((mut socket, mut client, mut accepted, mut server), stalled) =
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::try_join!(dial, release_relay)
            })
            .await??;
        assert_eq!(socket.route(), Some(format!("relay:{relay_url}")));
        assert_eq!(caller.admission.snapshot().total, MAX_ESTABLISHED - 1);
        drop(held_capacity);
        assert_eq!(caller.admission.snapshot().total, 1);
        let state = caller.state.lock().await;
        for url in &stalled_urls {
            assert!(
                !state
                    .paths
                    .contains_key(&(target_id.clone(), format!("direct:{url}"))),
                "preempted/cancelled directs cannot be penalized"
            );
        }
        drop(state);
        encrypted_round_trip(&mut client, &mut server, &mut socket, &mut accepted.socket).await?;
        drop(stalled);
        eprintln!(
            "NET1_M4 two_direct_noise_attempts_held=true late_directory_relay_arrived=true one_direct_cancelled_before_relay_slot=true healthy_relay_noise_complete=true local_losers_released=true logical_capacity_fixture=110 real_attempt_slots=2 relay_could_not_acquire_before_cancelled_socket_drop=true"
        );
        Ok(())
    }

    #[tokio::test]
    async fn m4_target_noise_with_wrong_hello_claim_is_path_failure_and_healthy_target_still_wins()
    -> Result<()> {
        let mut tasks = JoinSet::new();
        let owner = LocalIdentity::generate();
        let target_identity = LocalIdentity::generate();
        let untrusted = LocalIdentity::generate();
        let cluster = owner.fixture_cluster_id()?;
        let roles = vec![
            crate::model::NodeRole::Replica,
            crate::model::NodeRole::Relay,
        ];
        let mut store = crate::store::Store::open(":memory:")?;
        store.install_trust_bundle(
            &cluster,
            &owner.sign_cluster_fixture(
                1,
                vec![
                    owner.certificate(roles.clone())?,
                    target_identity.certificate(roles.clone())?,
                ],
            )?,
        )?;
        let store = Arc::new(Mutex::new(store));
        let (peer, _) = crate::peer::PeerHub::new(owner.clone(), store.clone(), &roles);
        let (healthy_peer, mut messages) =
            crate::peer::PeerHub::new(target_identity.clone(), store, &roles);
        let (bad, mut bad_incoming) =
            NetworkHub::new(target_identity.clone(), NetworkConfig::default());
        let bad_url = serve(bad, &mut tasks).await?;
        let (healthy, mut healthy_incoming) =
            NetworkHub::new(target_identity.clone(), NetworkConfig::default());
        let healthy_url = serve(healthy, &mut tasks).await?;
        let (bad_rejected, rejected) = oneshot::channel();
        let bad_identity = target_identity.clone();
        let bad_owner = owner.clone();
        let bad_cluster = cluster.clone();
        tasks.spawn(async move {
            let result = async {
                let mut accepted = bad_incoming.recv().await.context("malformed Hello path missing")?;
                let mut handshake = Handshake::new(false, &bad_identity.secret_key_hex,
                    Some(bad_owner.peer_id()?), b"vibeface:peer:v2").map_err(anyhow::Error::msg)?;
                assert!(handshake.read(&next_binary(&mut accepted.socket).await?).map_err(anyhow::Error::msg)?.is_empty());
                send_frame(&mut accepted.socket, Message::Binary(handshake.write(&[]).map_err(anyhow::Error::msg)?.into())).await?;
                let hello: serde_json::Value = serde_json::from_slice(&handshake.read(&next_binary(&mut accepted.socket).await?).map_err(anyhow::Error::msg)?)?;
                assert_eq!(hello["peer_id"], bad_owner.peer_id()?);
                let mut channel = handshake.finish().map_err(anyhow::Error::msg)?;
                let malformed = serde_json::to_vec(&serde_json::json!({
                    "version":crate::model::WIRE_VERSION,"cluster_id":bad_cluster,"peer_id":untrusted.peer_id()?
                }))?;
                for record in channel.seal_message(&malformed).map_err(anyhow::Error::msg)? {
                    send_frame(&mut accepted.socket, Message::Binary(record.into())).await?;
                }
                ensure!(next_binary(&mut accepted.socket).await.is_err(), "wrong Hello path was not rejected");
                let _ = bad_rejected.send(());
                Ok::<_, anyhow::Error>(())
            }.await;
            assert!(result.is_ok(), "{result:?}");
        });
        let stop = Arc::new(tokio::sync::Notify::new());
        let server_stop = stop.clone();
        let server_peer = healthy_peer.clone();
        tasks.spawn(async move {
            rejected.await.expect("malformed path did not close");
            let accepted = healthy_incoming
                .recv()
                .await
                .expect("healthy target path missing");
            tokio::select! {
                _ = server_peer.accept_transport(accepted.socket) => {},
                _ = server_stop.notified() => {},
            }
        });
        let response_peer = healthy_peer.clone();
        tasks.spawn(async move {
            let request = messages.recv().await.expect("healthy target query missing");
            assert_eq!(
                request.payload,
                serde_json::json!({"type":"classification_probe"})
            );
            response_peer
                .respond(&request, serde_json::json!({"ok":true}))
                .await
                .expect("healthy target response failed");
        });
        let (transport, _) = NetworkHub::new(owner.clone(), NetworkConfig::default());
        let target_id = target_identity.peer_id()?;
        let mut record = signed_record(&target_identity, now_secs()?)?;
        record.direct_urls = vec![bad_url.clone(), healthy_url.clone()];
        record.signature = target_identity.sign_payload("reachability-v2", &record.unsigned()?)?;
        assert!(transport.remember(record).await?);
        transport
            .record_path(&target_id, &format!("direct:{bad_url}"), true)
            .await;
        let driver_peer = peer.clone();
        let driver_cluster = cluster.clone();
        let driver_target = target_id.clone();
        let driver_transport = transport.clone();
        let mut driver = tokio::spawn(async move {
            driver_peer
                .connect_target(
                    driver_cluster,
                    driver_target,
                    driver_transport,
                    None,
                    tokio::time::Instant::now() + DIAL_TIMEOUT,
                )
                .await
        });
        tokio::select! {
            result = &mut driver => bail!("malformed Hello terminated the flight: {result:?}"),
            ready = tokio::time::timeout(Duration::from_secs(5), async {
                while !peer.online(&cluster).await.contains(&target_id) { tokio::task::yield_now().await; }
            }) => ready?,
        }
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(2),
                peer.request(
                    &cluster,
                    &target_id,
                    serde_json::json!({"type":"classification_probe"})
                )
            )
            .await??,
            serde_json::json!({"ok":true})
        );
        let state = transport.state.lock().await;
        assert!(
            state.paths[&(target_id.clone(), format!("direct:{bad_url}"))]
                .failed_until
                .is_some()
        );
        assert!(
            state.paths[&(target_id.clone(), format!("direct:{healthy_url}"))]
                .authenticated_at
                .is_some()
        );
        drop(state);
        stop.notify_one();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), driver)
                .await??
                .is_err()
        );
        assert!(peer.online(&cluster).await.is_empty());
        assert_eq!(transport.admission.snapshot().total, 0);
        eprintln!(
            "NET1_M4 actual_target_noise_complete=true malformed_hello_claim_untrusted=true classified_path_failure=true healthy_same_target_noise_hello_authorization_wins=true encrypted_query_response=true"
        );
        Ok(())
    }

    #[tokio::test]
    async fn public_socket_capacity_is_held_until_the_application_channel_drops() -> Result<()> {
        let mut tasks = JoinSet::new();
        let (hub, mut incoming) =
            NetworkHub::new(LocalIdentity::generate(), NetworkConfig::default());
        let url = serve(hub.clone(), &mut tasks).await?;
        let mut clients = Vec::new();
        let mut applications = Vec::new();
        for _ in 0..16 {
            clients.push(connect_direct(&url, Service::Client).await?);
            applications.push(incoming.recv().await.context("missing accepted channel")?);
        }
        assert_eq!(
            (
                hub.admission.snapshot().total,
                hub.admission.snapshot().long
            ),
            (16, 16)
        );
        clients.clear();
        assert_eq!(hub.admission.snapshot().long, 16);
        assert!(connect_direct(&url, Service::Client).await.is_err());
        drop(applications.pop());
        assert_eq!(hub.admission.snapshot().long, 15);
        let _next = connect_direct(&url, Service::Client).await?;
        applications.push(
            incoming
                .recv()
                .await
                .context("released application slot was not reused")?,
        );
        assert_eq!(hub.admission.snapshot().long, 16);
        drop(applications);
        assert_eq!(hub.admission.snapshot().total, 0);
        Ok(())
    }

    #[tokio::test]
    async fn direct_path_is_preferred_and_public_relay_is_opt_in() -> Result<()> {
        let mut tasks = JoinSet::new();
        let target_identity = LocalIdentity::generate();
        let (target, mut incoming) =
            NetworkHub::new(target_identity.clone(), NetworkConfig::default());
        let target_url = serve(target.clone(), &mut tasks).await?;
        let caller_identity = LocalIdentity::generate();
        let (caller, _) = NetworkHub::new(caller_identity.clone(), NetworkConfig::default());
        seed_mdns_urls(
            &caller,
            &target_identity.peer_id()?,
            vec![target_url.clone()],
        );
        let (mut socket, mut client, mut accepted, mut server) = dial_test_noise(
            &caller,
            &caller_identity,
            &target_identity,
            Service::Peer,
            &mut incoming,
            b"test:directory",
        )
        .await?;
        encrypted_round_trip(&mut client, &mut server, &mut socket, &mut accepted.socket).await?;
        let (mut socket, nonce) = open_control(&target_url).await?;
        let identity = LocalIdentity::generate();
        let peer_id = identity.peer_id()?;
        let signature = identity.sign_payload(
            "relay-reserve",
            &serde_json::to_vec(&ReservationProof {
                nonce: &nonce,
                peer_id: &peer_id,
            })?,
        )?;
        send_control(
            &mut socket,
            &Control::Reserve {
                peer_id,
                public_key: identity.public_key_hex()?,
                signature,
            },
        )
        .await?;
        assert!(receive_control(&mut socket).await.is_err());
        assert!(relay_lock(&target.relay).reservations.is_empty());
        let mut rate = ControlRate::default();
        for _ in 0..30 {
            rate.charge(30)?;
        }
        assert!(rate.charge(30).is_err());
        Ok(())
    }
}
