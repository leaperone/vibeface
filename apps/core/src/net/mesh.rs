//! The public mesh. Every Node holds the signed records of the Nodes it has heard of, spread by
//! gossip and pulled from neighbors, and finds devices, gateways and delivery providers there.
//! A Node first joins through its bootstrap nodes, which are ordinary Nodes too.
pub mod record;
pub mod table;

use super::{MemoryLookup, Net, call, device_id, endpoint_id, serve_streams_within};
use anyhow::{Context as _, Result, ensure};
use futures_util::{StreamExt as _, stream::BoxStream, stream::FuturesUnordered};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayUrl, TransportAddr,
    address_lookup::{self, AddressLookup, EndpointData, EndpointInfo, Item},
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, RouterBuilder},
};
use iroh_gossip::{
    Gossip, TopicId,
    api::{Event, GossipReceiver, GossipSender},
};
use record::{Access, MeshRecord, Reachable, Role, SignedMeshRecord};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest as _;
use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::Path,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};
use table::{Entry, Store, Table, Write};
use tokio::sync::{Notify, watch};
use vibeface_secure::link::{Addr, NodeLink};

pub const DEFAULT_NETWORK: &str = "vibeface";
const MAX_BOOTSTRAP: usize = 16;
const MAX_MESSAGE: usize = 4096;
const ANNOUNCE_EVERY: Duration = Duration::from_secs(600);
/// Changes are announced at most this often; tests wait less, their records still dated 30 s
/// apart.
const ANNOUNCE_GAP: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(30)
};
const JOIN_RETRY: Duration = Duration::from_secs(30);
const JOIN_RETRY_MAX: Duration = Duration::from_secs(300);
const PAGE_RECORDS: usize = 2000;
const PAGE_BYTES: usize = 4 << 20;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const ANSWERING: usize = 16;
const LOOKUP_BUDGET: Duration = Duration::from_secs(8);
const LOOKUP_NEIGHBORS: usize = 3;
/// Remembered Nodes tried when joining again, besides the bootstrap nodes.
const CANDIDATES: usize = 8;
const GATEWAYS: usize = 16;
/// Only the most recently seen gateways are checked.
const GATEWAY_CHECKS: usize = 32;
const GATEWAY_OK_FOR: Duration = Duration::from_secs(1800);
const GATEWAY_FAILED_FOR: Duration = Duration::from_secs(300);
/// A gateway check resolves, connects and reads `/api/info` within this, at most 16 KiB of it.
const GATEWAY_TIMEOUT: Duration = Duration::from_secs(5);
const GATEWAY_INFO_BYTES: usize = 16 * 1024;
const DIGEST_EVERY: Duration = Duration::from_secs(10);
const LOOKUPS_PER_SECOND: u32 = 5;
/// A device's relay probes are answered once a minute; tests wait less.
const PROBE_EVERY: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(60)
};
/// Records of devices not held yet that one neighbor's deliveries may add, per second and at
/// once. The personal network's Nodes and the bootstrap nodes are not limited.
const NEW_RECORDS_PER_SECOND: f64 = 20.0;
const NEW_RECORDS_BURST: f64 = 200.0;
/// Changes to the table are written to `mesh.sqlite3` together, this long after the first.
const WRITE_WINDOW: Duration = Duration::from_millis(200);
/// The largest exchange request; a digest's filter takes most of it.
const MAX_REQUEST: usize = 96 * 1024;
const MAX_FILTER: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshConfig {
    /// Off in `default()`, so fixtures stay offline; `init` turns it on.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_network")]
    pub network: String,
    /// The Nodes this one joins through first, each a node link
    /// `vibeface://node/<device-id>?addr=<host>:<UDP-port>&relay=<URL>` naming an address, a
    /// relay or both.
    #[serde(default)]
    pub bootstrap: Vec<String>,
    /// Use gateways and relays on private and loopback addresses, as a LAN group or test does.
    #[serde(default)]
    pub allow_private: bool,
}

fn default_network() -> String {
    DEFAULT_NETWORK.into()
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            network: default_network(),
            bootstrap: vec![],
            allow_private: false,
        }
    }
}

impl MeshConfig {
    pub fn validate(&self) -> Result<()> {
        record::valid_network(&self.network)?;
        ensure!(
            self.bootstrap.len() <= MAX_BOOTSTRAP,
            "at most {MAX_BOOTSTRAP} bootstrap nodes"
        );
        let mut seen = HashSet::new();
        for value in &self.bootstrap {
            ensure!(
                seen.insert(bootstrap_link(value)?.id),
                "a bootstrap node is listed once"
            );
        }
        Ok(())
    }

    /// The public mesh through the default bootstrap node, as `init` sets up.
    pub fn public() -> Result<Self> {
        Ok(Self {
            enabled: true,
            bootstrap: vec![default_bootstrap()?],
            ..Self::default()
        })
    }

    /// Takes out `remove`, each a node link or a device ID, and adds `add` in place of any
    /// listed node of the same device; the result is checked as `init` checks it.
    pub fn edit_bootstrap(&mut self, add: &[String], remove: &[String]) -> Result<()> {
        let listed = |list: &[String], id: [u8; 32]| {
            list.iter()
                .position(|value| bootstrap_link(value).is_ok_and(|node| node.id == id))
        };
        for value in remove {
            let id = match value.starts_with("vibeface://") {
                true => bootstrap_link(value)?.id,
                false => *endpoint_id(value)?.as_bytes(),
            };
            let at = listed(&self.bootstrap, id)
                .with_context(|| format!("{value} is not a bootstrap node"))?;
            self.bootstrap.remove(at);
        }
        for value in add {
            let id = bootstrap_link(value)?.id;
            if let Some(at) = listed(&self.bootstrap, id) {
                self.bootstrap.remove(at);
            }
            self.bootstrap.push(value.clone());
        }
        self.validate()
    }
}

#[derive(Deserialize)]
struct Anchor {
    issuer: String,
    udp_host: String,
    udp_port: u16,
    relay_url: String,
}

fn anchor() -> Result<Anchor> {
    Ok(serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../config/public-bootstrap.json"
    )))?)
}

/// The default bootstrap node: the official public Node `config/public-bootstrap.json` pins.
pub fn default_bootstrap() -> Result<String> {
    let anchor = anchor()?;
    let host = match anchor.udp_host.parse::<std::net::Ipv6Addr>() {
        Ok(_) => format!("[{}]", anchor.udp_host),
        Err(_) => anchor.udp_host,
    };
    let link = NodeLink::new(
        &anchor.issuer,
        vec![Addr::parse(&format!("{host}:{}", anchor.udp_port))?],
        Some(anchor.relay_url),
    )?
    .to_string();
    bootstrap_link(&link).context("invalid config/public-bootstrap.json")?;
    Ok(link)
}

/// The default bootstrap node's device ID.
pub fn default_bootstrap_id() -> Result<String> {
    Ok(anchor()?.issuer)
}

/// A bootstrap node: a node link naming where to dial it. Its relay stays in the relay map, as
/// the members' relays do.
pub fn bootstrap_link(value: &str) -> Result<NodeLink> {
    let link = NodeLink::parse(value)?;
    ensure!(
        link.has_hints(),
        "a bootstrap node link needs an addr or a relay"
    );
    if let Some(relay) = &link.relay {
        super::relay_url(relay)?;
    }
    Ok(link)
}

pub fn gossip_alpn(network: &str) -> Vec<u8> {
    format!("vibeface/mesh-gossip/{network}/1").into_bytes()
}

pub fn exchange_alpn(network: &str) -> Vec<u8> {
    format!("vibeface/mesh/{network}/1").into_bytes()
}

fn topic(network: &str) -> TopicId {
    let mut hash = sha2::Sha256::new();
    hash.update(b"vibeface:mesh:v1:");
    hash.update(network.as_bytes());
    TopicId::from_bytes(hash.finalize().into())
}

fn now_ms() -> u64 {
    crate::runtime::now_ms() as u64
}

/// Whether `url` names a host outside private and loopback networks, judging names by their
/// form alone.
pub fn public_host(url: &reqwest::Url) -> bool {
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => crate::fetch::public(ip),
        Err(_) => !host.is_empty() && host != "localhost" && !host.ends_with(".localhost"),
    }
}

/// Where `record` says its device is. A relay on a private host is left out unless allowed.
pub fn record_addr(record: &MeshRecord, allow_private: bool) -> Result<EndpointAddr> {
    let relay = record
        .relay_url
        .as_deref()
        .and_then(|url| super::relay_url(url).ok())
        .filter(|url| allow_private || public_host(url));
    Ok(EndpointAddr::from_parts(
        endpoint_id(&record.device_id)?,
        record
            .addrs
            .iter()
            .copied()
            .map(TransportAddr::Ip)
            .chain(relay.map(TransportAddr::Relay)),
    ))
}

/// The remembered Nodes worth joining again: publicly reachable ones, or any with an address on
/// a private mesh, most recently seen first.
fn recent<'a>(table: &'a Table, own: &str, allow_private: bool) -> Vec<&'a Entry> {
    let mut known: Vec<&Entry> = table
        .entries()
        .filter(|entry| {
            entry.record.device_id != own
                && (entry.record.reachable == Reachable::Public
                    || (allow_private && !entry.record.addrs.is_empty()))
        })
        .collect();
    known.sort_by_key(|entry| Reverse(entry.last_seen_ms));
    known.truncate(CANDIDATES);
    known
}

/// Whether a current record in `entries` offers `url` as a relay open to any device.
pub fn open_relay<'a>(entries: impl IntoIterator<Item = &'a Entry>, url: &RelayUrl) -> bool {
    let now = now_ms();
    entries.into_iter().any(|entry| {
        entry.online(now)
            && entry.record.roles.iter().any(|role| {
                matches!(role, Role::Relay { url: offered, access: Access::Open, .. }
                    if super::relay_url(offered).is_ok_and(|offered| offered == *url))
            })
    })
}

/// An exchange request, read straight from a frame of at most [`MAX_REQUEST`] bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// The records held here that the asker's filter lacks, a page at a time.
    Digest { have: Have },
    /// A device's current record.
    Lookup { device_id: String },
    /// Check a relay from here, as a device elsewhere would use it.
    ProbeRelay { url: String },
}

/// A Bloom filter of the records a Node holds, each a device and its `seq`, so a digest
/// request stays small however many it holds. A record the filter wrongly claims is missed in
/// this pull; the next one, with another seed, or the device's next announcement brings it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Have {
    seed: u64,
    hashes: u8,
    /// The filter's bits, base64url.
    bits: String,
}

impl Have {
    /// A filter of `held` with about 16 bits a record, at most [`MAX_FILTER`] bytes.
    pub fn new(held: &[(String, u64)]) -> Self {
        let bytes = (held.len() * 2).clamp(8, MAX_FILTER);
        let bits_per_record = (bytes * 8) as f64 / held.len().max(1) as f64;
        let hashes = (bits_per_record * std::f64::consts::LN_2)
            .round()
            .clamp(1.0, 8.0) as u8;
        let mut have = Self {
            seed: rand::random(),
            hashes,
            bits: String::new(),
        };
        let mut bits = vec![0u8; bytes];
        for (device, seq) in held {
            for index in have.indexes(bits.len() * 8, device, *seq) {
                bits[index / 8] |= 1 << (index % 8);
            }
        }
        have.bits = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bits);
        have
    }

    fn indexes(&self, len: usize, device: &str, seq: u64) -> impl Iterator<Item = usize> {
        let mut hash = sha2::Sha256::new();
        hash.update(self.seed.to_le_bytes());
        hash.update(device.as_bytes());
        hash.update(seq.to_le_bytes());
        let hash = hash.finalize();
        let first = u64::from_le_bytes(hash[..8].try_into().expect("8 bytes"));
        let step = u64::from_le_bytes(hash[8..16].try_into().expect("8 bytes")) | 1;
        (0..u64::from(self.hashes))
            .map(move |i| (first.wrapping_add(i.wrapping_mul(step)) % len as u64) as usize)
    }

    /// Whether the asker holds `seq` of `device`, as the filter says.
    fn contains(&self, bits: &[u8], device: &str, seq: u64) -> bool {
        !bits.is_empty()
            && self
                .indexes(bits.len() * 8, device, seq)
                .all(|index| bits[index / 8] & (1 << (index % 8)) != 0)
    }

    fn open(&self) -> Result<Vec<u8>> {
        ensure!(
            (1..=16).contains(&self.hashes),
            "a mesh digest filter takes 1 to 16 hashes"
        );
        let bits = base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            &self.bits,
        )
        .context("invalid mesh digest filter")?;
        ensure!(
            bits.len() <= MAX_FILTER,
            "the mesh digest filter is too large"
        );
        Ok(bits)
    }
}

/// One exchange request over a connection of its own.
async fn exchange(
    endpoint: &Endpoint,
    addr: impl Into<EndpointAddr>,
    alpn: &[u8],
    request: &impl Serialize,
) -> Result<Value> {
    let request = serde_json::to_value(request)?;
    tokio::time::timeout(EXCHANGE_TIMEOUT, async {
        let connection = endpoint.connect(addr, alpn).await?;
        let reply = call(&connection, &request).await;
        connection.close(0u32.into(), b"done");
        reply
    })
    .await
    .context("mesh peer did not answer in time")?
}

/// Opens a `lookup` answer for `device`.
fn found(reply: &Value, network: &str, device: &str) -> Option<SignedMeshRecord> {
    let signed: SignedMeshRecord = serde_json::from_value(reply["record"].clone()).ok()?;
    let record = signed.open(network, now_ms()).ok()?;
    (record.device_id == device).then_some(signed)
}

#[derive(Default)]
struct Limit {
    digest_at: Option<Instant>,
    /// The last digest answer had more pages; the next page is not limited.
    more: bool,
    lookups: (Option<Instant>, u32),
    probe_at: Option<Instant>,
}

/// How many records of devices not held yet one neighbor may still deliver.
struct Bucket {
    tokens: f64,
    at: Instant,
    /// When dropping this neighbor's records was last logged.
    logged_at: Option<Instant>,
}

impl Bucket {
    fn new() -> Self {
        Self {
            tokens: NEW_RECORDS_BURST,
            at: Instant::now(),
            logged_at: None,
        }
    }

    fn take(&mut self) -> bool {
        let now = Instant::now();
        self.tokens = (self.tokens
            + now.duration_since(self.at).as_secs_f64() * NEW_RECORDS_PER_SECOND)
            .min(NEW_RECORDS_BURST);
        self.at = now;
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

pub struct Mesh {
    pub network: String,
    device_id: String,
    allow_private: bool,
    endpoint: Endpoint,
    /// The Net's address book, where bootstrap nodes' addresses go.
    lookup: MemoryLookup,
    gossip: Gossip,
    sender: OnceLock<GossipSender>,
    gossip_alpn: Vec<u8>,
    exchange_alpn: Vec<u8>,
    table: Mutex<Table>,
    /// Writes the table's changes, which wait in `pending` until the writer takes them.
    store: Mutex<Store>,
    pending: Mutex<Vec<Write>>,
    written: Notify,
    changes: watch::Sender<u64>,
    roles: Mutex<Vec<Role>>,
    bootstrap: Mutex<Vec<String>>,
    /// The personal network's other Nodes.
    members: Mutex<Vec<String>>,
    neighbors: Mutex<HashSet<EndpointId>>,
    buckets: Mutex<HashMap<EndpointId, Bucket>>,
    /// Asks the announcer to check this Node's record.
    wake: Notify,
    limits: Mutex<HashMap<EndpointId, Limit>>,
    /// Exchange requests answered at once from every device together, each within 30 s, so
    /// pages nobody reads hold at most these permits' worth of memory.
    answering: Arc<tokio::sync::Semaphore>,
    gateway_checks: Mutex<HashMap<(String, String), (bool, Instant)>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for Mesh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Mesh({})", self.network)
    }
}

impl Mesh {
    /// Joins `config.network` on `net`'s endpoint with the records `db` holds. The caller's
    /// router accepts the mesh protocols through [`Mesh::accept`].
    pub async fn start(net: &Net, config: &MeshConfig, db: &Path) -> Result<Arc<Self>> {
        config.validate()?;
        let endpoint = net.endpoint.clone();
        let own = device_id(&endpoint.id());
        let (table, store) = Table::open(db, &own, &config.network, now_ms())?;
        let gossip_alpn = gossip_alpn(&config.network);
        let gossip = Gossip::builder()
            .max_message_size(MAX_MESSAGE)
            .alpn(&gossip_alpn)
            .spawn(endpoint.clone());
        let mesh = Arc::new(Self {
            network: config.network.clone(),
            device_id: own,
            allow_private: config.allow_private,
            endpoint: endpoint.clone(),
            lookup: net.lookup.clone(),
            gossip,
            sender: OnceLock::new(),
            gossip_alpn,
            exchange_alpn: exchange_alpn(&config.network),
            table: Mutex::new(table),
            store: Mutex::new(store),
            pending: Mutex::new(vec![]),
            written: Notify::new(),
            changes: watch::Sender::new(0),
            roles: Mutex::new(vec![]),
            bootstrap: Mutex::new(config.bootstrap.clone()),
            members: Mutex::new(vec![]),
            neighbors: Mutex::new(HashSet::new()),
            buckets: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            limits: Mutex::new(HashMap::new()),
            answering: Arc::new(tokio::sync::Semaphore::new(ANSWERING)),
            gateway_checks: Mutex::new(HashMap::new()),
            tasks: Mutex::new(vec![]),
        });
        mesh.protect();
        endpoint
            .address_lookup()?
            .add(Lookup(Arc::downgrade(&mesh)));
        let candidates = mesh.candidates().await;
        let (sender, receiver) = mesh
            .gossip
            .subscribe(topic(&mesh.network), candidates)
            .await?
            .split();
        let _ = mesh.sender.set(sender);
        let tasks = [
            tokio::spawn(mesh.clone().events(receiver)),
            tokio::spawn(mesh.clone().announcer()),
            tokio::spawn(mesh.clone().rejoin()),
            tokio::spawn(mesh.clone().writer()),
        ];
        mesh.tasks.lock().expect("mesh tasks").extend(tasks);
        Ok(mesh)
    }

    /// Adds the gossip and exchange protocols to `router`.
    pub fn accept(self: &Arc<Self>, router: RouterBuilder) -> RouterBuilder {
        router
            .accept(&self.gossip_alpn, self.gossip.clone())
            .accept(&self.exchange_alpn, Exchange(Arc::downgrade(self)))
    }

    /// Stops gossip and the mesh's tasks, which hold it until then, and writes what is pending.
    pub async fn close(&self) {
        for task in self.tasks.lock().expect("mesh tasks").drain(..) {
            task.abort();
        }
        let _ = self.gossip.shutdown().await;
        self.flush();
    }

    /// Queues `writes` for the writer.
    fn queue(&self, writes: Vec<Write>) {
        if !writes.is_empty() {
            self.pending.lock().expect("mesh writes").extend(writes);
            self.written.notify_one();
        }
    }

    /// Writes the pending changes in one transaction, without holding the table.
    fn flush(&self) {
        let writes = std::mem::take(&mut *self.pending.lock().expect("mesh writes"));
        if let Err(error) = self.store.lock().expect("mesh store").apply(writes) {
            tracing::warn!(event = "mesh.table_failed", %error);
        }
    }

    /// Writes the table's changes a short while after the first of them.
    async fn writer(self: Arc<Self>) {
        loop {
            self.written.notified().await;
            tokio::time::sleep(WRITE_WINDOW).await;
            self.flush();
        }
    }

    /// Protects the records of the personal network's Nodes and the bootstrap nodes from being
    /// dropped by a full table.
    fn protect(&self) {
        let mut devices: HashSet<String> = self
            .members
            .lock()
            .expect("mesh members")
            .iter()
            .cloned()
            .collect();
        devices.extend(
            self.bootstrap
                .lock()
                .expect("mesh bootstrap")
                .iter()
                .filter_map(|value| bootstrap_link(value).ok())
                .map(|link| link.device_id()),
        );
        self.table
            .lock()
            .expect("mesh table")
            .set_protected(devices);
    }

    /// The relays of the bootstrap nodes: each link's relay, and the open relays a bootstrap
    /// node's record announces. They stay in the relay map like the members' relays.
    pub fn bootstrap_relays(&self) -> Vec<RelayUrl> {
        let links: Vec<NodeLink> = self
            .bootstrap
            .lock()
            .expect("mesh bootstrap")
            .iter()
            .filter_map(|value| bootstrap_link(value).ok())
            .collect();
        let mut relays: Vec<RelayUrl> = links
            .iter()
            .filter_map(|link| super::relay_url(link.relay.as_deref()?).ok())
            .collect();
        let now = now_ms();
        let table = self.table.lock().expect("mesh table");
        for link in &links {
            let Some(entry) = table
                .get(&link.device_id())
                .filter(|entry| entry.online(now))
            else {
                continue;
            };
            relays.extend(entry.record.roles.iter().filter_map(|role| {
                match role {
                    Role::Relay {
                        url,
                        access: Access::Open,
                        ..
                    } => super::relay_url(url)
                        .ok()
                        .filter(|url| self.allow_private || public_host(url)),
                    _ => None,
                }
            }));
        }
        relays.sort();
        relays.dedup();
        relays
    }

    /// Counts up each time the records change.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// The records that have not expired, this Node's own included.
    pub fn records(&self) -> Vec<Entry> {
        let now = now_ms();
        self.table
            .lock()
            .expect("mesh table")
            .online(now)
            .cloned()
            .collect()
    }

    /// The records held, expired ones too.
    pub fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        self.table.lock().expect("mesh table")
    }

    pub fn neighbors(&self) -> Vec<String> {
        self.neighbors
            .lock()
            .expect("mesh neighbors")
            .iter()
            .map(device_id)
            .collect()
    }

    /// The records that have not expired and the neighbors.
    pub fn counts(&self) -> (usize, usize) {
        let records = self
            .table
            .lock()
            .expect("mesh table")
            .online(now_ms())
            .count();
        (
            records,
            self.neighbors.lock().expect("mesh neighbors").len(),
        )
    }

    /// The roles this Node announces; a change goes out within 30 s.
    pub fn set_roles(&self, roles: Vec<Role>) {
        let mut current = self.roles.lock().expect("mesh roles");
        if *current != roles {
            *current = roles;
            self.wake.notify_one();
        }
    }

    pub async fn set_bootstrap(&self, bootstrap: Vec<String>) {
        *self.bootstrap.lock().expect("mesh bootstrap") = bootstrap;
        self.protect();
        if self.neighbors.lock().expect("mesh neighbors").is_empty() {
            let peers = self.candidates().await;
            self.join(peers).await;
        }
    }

    /// Joins through the personal network's other Nodes, `device_ids`, whose records a full
    /// table keeps and whose deliveries are not limited.
    pub async fn join_peers(&self, device_ids: &[String]) {
        *self.members.lock().expect("mesh members") = device_ids.to_vec();
        self.protect();
        let peers = device_ids
            .iter()
            .filter_map(|device| endpoint_id(device).ok())
            .collect();
        self.join(peers).await;
    }

    async fn join(&self, peers: Vec<EndpointId>) {
        if let Some(sender) = self.sender.get()
            && !peers.is_empty()
            && let Err(error) = sender.join_peers(peers).await
        {
            tracing::debug!(event = "mesh.join_failed", %error);
        }
    }

    /// One exchange request to `device`, a neighbor or any Node this endpoint reaches.
    pub async fn request(&self, device: &str, request: &impl Serialize) -> Result<Value> {
        exchange(
            &self.endpoint,
            endpoint_id(device)?,
            &self.exchange_alpn,
            request,
        )
        .await
    }

    /// `device`'s current record: the one held, else one a neighbor holds, within 8 s.
    pub async fn find(&self, device: &str) -> Option<MeshRecord> {
        let now = now_ms();
        if let Some(entry) = self
            .table
            .lock()
            .expect("mesh table")
            .get(device)
            .filter(|entry| entry.online(now))
        {
            return Some(entry.record.clone());
        }
        self.ask_neighbors(device).await
    }

    /// Verified gateways of devices other than this Node and `exclude`, most recently seen
    /// first. Each answers `/api/info` as its device; results are kept 30 min, failures 5 min.
    pub async fn gateways(&self, exclude: &[String]) -> Vec<(String, String)> {
        let now = now_ms();
        let mut candidates: Vec<(u64, String, String)> = self
            .table
            .lock()
            .expect("mesh table")
            .online(now)
            .filter(|entry| {
                entry.record.device_id != self.device_id
                    && !exclude.contains(&entry.record.device_id)
            })
            .flat_map(|entry| {
                entry.record.roles.iter().filter_map(|role| match role {
                    Role::Gateway { url } => Some((
                        entry.last_seen_ms,
                        entry.record.device_id.clone(),
                        url.clone(),
                    )),
                    _ => None,
                })
            })
            .collect();
        candidates.sort_by_key(|(seen, _, _)| Reverse(*seen));
        candidates.truncate(GATEWAY_CHECKS);
        let checks = futures_util::future::join_all(
            candidates
                .iter()
                .map(|(_, device, url)| self.gateway_ok(device, url)),
        )
        .await;
        candidates
            .into_iter()
            .zip(checks)
            .filter(|(_, ok)| *ok)
            .map(|((_, device, url), _)| (device, url))
            .take(GATEWAYS)
            .collect()
    }

    /// `device`'s gateway, when its online record names one that answers as it.
    pub async fn gateway_of(&self, device: &str) -> Option<String> {
        let url = self
            .table
            .lock()
            .expect("mesh table")
            .online(now_ms())
            .filter(|entry| entry.record.device_id == device)
            .find_map(|entry| {
                entry.record.roles.iter().find_map(|role| match role {
                    Role::Gateway { url } => Some(url.clone()),
                    _ => None,
                })
            })?;
        self.gateway_ok(device, &url).await.then_some(url)
    }

    async fn gateway_ok(&self, device: &str, url: &str) -> bool {
        let key = (device.to_owned(), url.to_owned());
        if let Some((ok, at)) = self
            .gateway_checks
            .lock()
            .expect("gateway checks")
            .get(&key)
            && at.elapsed()
                < if *ok {
                    GATEWAY_OK_FOR
                } else {
                    GATEWAY_FAILED_FOR
                }
        {
            return *ok;
        }
        let ok = match verify_gateway(url, device, self.allow_private).await {
            Ok(()) => true,
            Err(error) => {
                tracing::debug!(event = "mesh.gateway_unverified", %url, %error);
                false
            }
        };
        let mut checks = self.gateway_checks.lock().expect("gateway checks");
        if checks.len() >= 4 * GATEWAY_CHECKS {
            checks.retain(|_, (_, at)| at.elapsed() < GATEWAY_OK_FOR);
        }
        checks.insert(key, (ok, Instant::now()));
        ok
    }

    /// Takes `records` that `from` delivered, or that this Node asked for when `None`. A
    /// neighbor outside the personal network and the bootstrap nodes adds records of devices
    /// not held yet at most 20 a second, 200 at once; returns the records accepted and whether
    /// any were dropped for that.
    fn accept_records(
        &self,
        records: Vec<SignedMeshRecord>,
        from: Option<EndpointId>,
    ) -> (Vec<MeshRecord>, bool) {
        let mut limited = false;
        let (accepted, writes) = {
            let mut table = self.table.lock().expect("mesh table");
            let from = from.filter(|peer| !table.is_protected(&device_id(peer)));
            let mut buckets = self.buckets.lock().expect("mesh buckets");
            if buckets.len() >= 4096 {
                buckets.retain(|_, bucket| bucket.at.elapsed() < Duration::from_secs(60));
            }
            let mut bucket = from.map(|peer| buckets.entry(peer).or_insert_with(Bucket::new));
            let admit = |_: &MeshRecord| {
                let admitted = bucket.as_mut().is_none_or(|bucket| bucket.take());
                limited |= !admitted;
                admitted
            };
            let inserted = table.insert(records, now_ms(), admit);
            if limited
                && let Some(bucket) = bucket
                && bucket
                    .logged_at
                    .is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
            {
                bucket.logged_at = Some(Instant::now());
                tracing::debug!(
                    event = "mesh.records_limited",
                    peer = %from.map(|peer| peer.fmt_short().to_string()).unwrap_or_default(),
                    "dropped new records past a neighbor's limit"
                );
            }
            inserted
        };
        self.queue(writes);
        if !accepted.is_empty() {
            self.changes.send_modify(|generation| *generation += 1);
        }
        (accepted, limited)
    }

    /// The remembered Nodes and the bootstrap nodes, whose addresses go to the address book.
    async fn candidates(&self) -> Vec<EndpointId> {
        let mut peers: Vec<EndpointId> = {
            let table = self.table.lock().expect("mesh table");
            recent(&table, &self.device_id, self.allow_private)
                .into_iter()
                .filter_map(|entry| endpoint_id(&entry.record.device_id).ok())
                .collect()
        };
        let bootstrap = self.bootstrap.lock().expect("mesh bootstrap").clone();
        for value in bootstrap {
            let Ok(link) = bootstrap_link(&value) else {
                continue;
            };
            let Ok(addr) = super::link_addr(&link).await else {
                continue;
            };
            if addr.id == self.endpoint.id() || peers.contains(&addr.id) {
                continue;
            }
            peers.push(addr.id);
            self.lookup.add_endpoint_info(addr);
        }
        peers
    }

    async fn events(self: Arc<Self>, mut receiver: GossipReceiver) {
        while let Some(event) = receiver.next().await {
            match event {
                Ok(Event::NeighborUp(peer)) => {
                    let first = {
                        let mut neighbors = self.neighbors.lock().expect("mesh neighbors");
                        neighbors.insert(peer);
                        neighbors.len() == 1
                    };
                    tracing::debug!(event = "mesh.neighbor_up", peer = %peer.fmt_short());
                    // What was announced with no neighbor reached nobody.
                    if first {
                        let own = self
                            .table
                            .lock()
                            .expect("mesh table")
                            .get(&self.device_id)
                            .map(|entry| entry.signed.clone());
                        if let Some(own) = own {
                            self.broadcast(&own).await;
                        }
                    }
                    self.spawn_pull(peer);
                }
                Ok(Event::NeighborDown(peer)) => {
                    self.neighbors.lock().expect("mesh neighbors").remove(&peer);
                }
                Ok(Event::Received(message)) => {
                    if let Ok(signed) = serde_json::from_slice(&message.content) {
                        self.accept_records(vec![signed], Some(message.delivered_from));
                    }
                }
                Ok(Event::Lagged) => {
                    tracing::warn!(event = "mesh.lagged", "mesh events were dropped");
                    let peer = self
                        .neighbors
                        .lock()
                        .expect("mesh neighbors")
                        .iter()
                        .next()
                        .copied();
                    if let Some(peer) = peer {
                        self.spawn_pull(peer);
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(event = "mesh.gossip_closed", %error);
                    return;
                }
            }
        }
    }

    fn spawn_pull(self: &Arc<Self>, peer: EndpointId) {
        let mesh = self.clone();
        tokio::spawn(async move {
            if let Err(error) = mesh.pull(peer).await {
                tracing::debug!(event = "mesh.pull_failed", peer = %peer.fmt_short(), %error);
            }
        });
    }

    /// Takes the records `peer` holds that this Node lacks, a page at a time, until `peer`
    /// delivers more new devices than its limit lets in.
    async fn pull(&self, peer: EndpointId) -> Result<()> {
        let mut have: Vec<(String, u64)> = self
            .table
            .lock()
            .expect("mesh table")
            .entries()
            .map(|entry| (entry.record.device_id.clone(), entry.record.seq))
            .collect();
        for _ in 0..=table::CAPACITY / PAGE_RECORDS {
            let reply = exchange(
                &self.endpoint,
                peer,
                &self.exchange_alpn,
                &Request::Digest {
                    have: Have::new(&have),
                },
            )
            .await?;
            let records: Vec<SignedMeshRecord> = serde_json::from_value(reply["records"].clone())?;
            ensure!(records.len() <= PAGE_RECORDS, "mesh page is too long");
            have.extend(records.iter().filter_map(|signed| {
                let record = MeshRecord::parse(&signed.payload).ok()?;
                Some((record.device_id, record.seq))
            }));
            let (_, limited) = self.accept_records(records, Some(peer));
            if limited || reply["more"] != true {
                break;
            }
        }
        Ok(())
    }

    async fn broadcast(&self, signed: &SignedMeshRecord) {
        let Some(sender) = self.sender.get() else {
            return;
        };
        let message = match serde_json::to_vec(signed) {
            Ok(message) => message,
            Err(_) => return,
        };
        if let Err(error) = sender.broadcast(message.into()).await {
            tracing::debug!(event = "mesh.broadcast_failed", %error);
        }
    }

    /// Announces at start, on a change at most every 30 s, and every 10 minutes.
    async fn announcer(self: Arc<Self>) {
        let mut last: Option<tokio::time::Instant> = None;
        loop {
            if let Some(at) = last {
                tokio::select! {
                    _ = self.wake.notified() => {}
                    _ = tokio::time::sleep_until(at + ANNOUNCE_EVERY) => {}
                }
                tokio::time::sleep_until(at + ANNOUNCE_GAP).await;
            }
            let due = last.is_none_or(|at| at.elapsed() >= ANNOUNCE_EVERY);
            match self.announce(due).await {
                Ok(true) => last = Some(tokio::time::Instant::now()),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(event = "mesh.announce_failed", %error);
                    last = Some(tokio::time::Instant::now());
                }
            }
        }
    }

    /// Signs, stores and broadcasts this Node's record when it changed or `due`.
    async fn announce(&self, due: bool) -> Result<bool> {
        let now = now_ms();
        let mut record = self.own_record(now);
        let held = self
            .table
            .lock()
            .expect("mesh table")
            .get(&self.device_id)
            .map(|entry| entry.record.clone());
        if let Some(held) = held {
            if !due
                && (MeshRecord {
                    seq: held.seq,
                    ..record.clone()
                }) == held
            {
                return Ok(false);
            }
            // Peers take a device's records 30 s apart by their dates.
            record.seq = record.seq.max(held.seq + table::MIN_INTERVAL_MS);
        }
        // Local addresses, listed last, give way until the record fits.
        while serde_json::to_string(&record)?.len() > record::MAX_PAYLOAD
            && record.addrs.pop().is_some()
        {}
        let signed = SignedMeshRecord::sign(&record, |payload| self.sign(payload))?;
        let writes = self
            .table
            .lock()
            .expect("mesh table")
            .insert_own(signed.clone(), record, now);
        self.queue(writes);
        self.changes.send_modify(|generation| *generation += 1);
        self.broadcast(&signed).await;
        Ok(true)
    }

    fn sign(&self, payload: &[u8]) -> Result<String> {
        let secret = zeroize::Zeroizing::new(hex::encode(self.endpoint.secret_key().to_bytes()));
        Ok(vibeface_secure::Identity::from_secret_hex(&secret)?.sign(record::DOMAIN, payload)?)
    }

    /// Public addresses first, then local ones. Reachable from anywhere when a public address
    /// keeps a port this endpoint is bound to: the host has it, or its NAT maps one to one.
    fn own_record(&self, now: u64) -> MeshRecord {
        let addr = self.endpoint.addr();
        let ports: Vec<u16> = self
            .endpoint
            .bound_sockets()
            .iter()
            .map(SocketAddr::port)
            .collect();
        let public = |address: &&SocketAddr| crate::fetch::public(address.ip());
        let mut addrs: Vec<SocketAddr> = addr.ip_addrs().copied().collect();
        addrs.sort_by_key(|address| !crate::fetch::public(address.ip()));
        addrs.truncate(8);
        let reachable = if addrs
            .iter()
            .filter(public)
            .any(|address| ports.contains(&address.port()))
        {
            Reachable::Public
        } else if addrs.iter().any(|address| public(&address)) {
            Reachable::Nat
        } else {
            Reachable::Unknown
        };
        MeshRecord {
            v: 1,
            network: self.network.clone(),
            device_id: self.device_id.clone(),
            seq: now,
            ttl_s: record::DEFAULT_TTL_S,
            addrs,
            relay_url: addr.relay_urls().next().map(ToString::to_string),
            reachable,
            roles: self.roles.lock().expect("mesh roles").clone(),
        }
    }

    /// Joins again through the remembered and bootstrap Nodes while this Node has no neighbor,
    /// backing off from 30 s to 5 minutes.
    async fn rejoin(self: Arc<Self>) {
        let mut wait = JOIN_RETRY;
        loop {
            tokio::time::sleep(wait).await;
            if self.neighbors.lock().expect("mesh neighbors").is_empty() {
                let peers = self.candidates().await;
                tracing::debug!(event = "mesh.rejoin", peers = peers.len());
                self.join(peers).await;
                wait = (wait * 2).min(JOIN_RETRY_MAX);
            } else {
                wait = JOIN_RETRY;
            }
        }
    }

    /// Asks up to three neighbors for `device`'s record, within 8 s altogether.
    async fn ask_neighbors(&self, device: &str) -> Option<MeshRecord> {
        let neighbors: Vec<EndpointId> = self
            .neighbors
            .lock()
            .expect("mesh neighbors")
            .iter()
            .filter(|peer| device_id(peer) != device)
            .take(LOOKUP_NEIGHBORS)
            .copied()
            .collect();
        let request = Request::Lookup {
            device_id: device.to_owned(),
        };
        let mut asks: FuturesUnordered<_> = neighbors
            .into_iter()
            .map(|peer| exchange(&self.endpoint, peer, &self.exchange_alpn, &request))
            .collect();
        let signed = tokio::time::timeout(LOOKUP_BUDGET, async {
            while let Some(reply) = asks.next().await {
                if let Some(signed) = reply
                    .ok()
                    .and_then(|reply| found(&reply, &self.network, device))
                {
                    return Some(signed);
                }
            }
            None
        })
        .await
        .ok()
        .flatten()?;
        let record = MeshRecord::parse(&signed.payload).ok();
        self.accept_records(vec![signed], None);
        record
    }

    async fn serve(self: Arc<Self>, connection: Connection) {
        let remote = connection.remote_id();
        let limit = (self.answering.clone(), EXCHANGE_TIMEOUT);
        let mesh = Arc::downgrade(&self);
        drop(self);
        serve_streams_within(
            connection,
            Some(limit),
            MAX_REQUEST,
            move |request: Request| {
                let mesh = mesh.clone();
                async move {
                    mesh.upgrade()
                        .context("the mesh stopped")?
                        .answer(remote, request)
                        .await
                }
            },
        )
        .await;
    }

    /// Answers one exchange request from `remote`.
    pub async fn answer(&self, remote: EndpointId, request: Request) -> Result<Value> {
        match request {
            Request::Digest { have } => {
                self.admit(remote, true)?;
                let (records, more) = self.page(&have)?;
                if let Some(limit) = self.limits.lock().expect("mesh limits").get_mut(&remote) {
                    limit.more = more;
                }
                Ok(json!({"records":records,"more":more}))
            }
            Request::Lookup { device_id } => {
                self.admit(remote, false)?;
                let now = now_ms();
                let record = self
                    .table
                    .lock()
                    .expect("mesh table")
                    .get(&device_id)
                    .filter(|entry| entry.online(now))
                    .map(|entry| entry.signed.clone());
                Ok(json!({"record":record}))
            }
            Request::ProbeRelay { url } => {
                {
                    let mut limits = self.limits.lock().expect("mesh limits");
                    let limit = limits.entry(remote).or_default();
                    ensure!(
                        limit.probe_at.is_none_or(|at| at.elapsed() >= PROBE_EVERY),
                        "a device asks for a relay probe at most once a minute"
                    );
                    limit.probe_at = Some(Instant::now());
                }
                // The asker learns only that the check failed, not what this Node saw.
                let (latency, _) = super::relay::check_relay(&url, self.allow_private)
                    .await
                    .map_err(|error| {
                        tracing::debug!(event = "mesh.probe_failed", %url, error = %format!("{error:#}"));
                        anyhow::anyhow!("the relay check failed")
                    })?;
                Ok(json!({"ok":true,"latency_ms":latency.as_millis() as u64}))
            }
        }
    }

    fn admit(&self, remote: EndpointId, digest: bool) -> Result<()> {
        let mut limits = self.limits.lock().expect("mesh limits");
        if limits.len() >= 4096 {
            limits.retain(|_, limit| {
                [limit.digest_at, limit.lookups.0, limit.probe_at]
                    .into_iter()
                    .flatten()
                    .any(|at| at.elapsed() < Duration::from_secs(60))
            });
        }
        let limit = limits.entry(remote).or_default();
        if digest {
            ensure!(
                limit.more
                    || limit
                        .digest_at
                        .is_none_or(|at| at.elapsed() >= DIGEST_EVERY),
                "a device asks for a mesh digest at most every 10 seconds"
            );
            limit.digest_at = Some(Instant::now());
            limit.more = false;
        } else {
            match &mut limit.lookups {
                (Some(start), count) if start.elapsed() < Duration::from_secs(1) => {
                    ensure!(
                        *count < LOOKUPS_PER_SECOND,
                        "a device makes at most 5 mesh lookups a second"
                    );
                    *count += 1;
                }
                lookups => *lookups = (Some(Instant::now()), 1),
            }
        }
        Ok(())
    }

    /// The records held here that `have` lacks, at most 2000 and about 4 MiB.
    fn page(&self, have: &Have) -> Result<(Vec<SignedMeshRecord>, bool)> {
        let bits = have.open()?;
        let now = now_ms();
        let table = self.table.lock().expect("mesh table");
        let mut records = Vec::new();
        let mut bytes = 0;
        for entry in table.online(now) {
            if have.contains(&bits, &entry.record.device_id, entry.record.seq) {
                continue;
            }
            // Escaped inside the reply, the payload can take twice its length.
            let size = 2 * entry.signed.payload.len() + entry.signed.signature.len() + 32;
            if records.len() == PAGE_RECORDS || bytes + size > PAGE_BYTES {
                return Ok((records, true));
            }
            bytes += size;
            records.push(entry.signed.clone());
        }
        Ok((records, false))
    }
}

/// Looks `device` up through `net`, which runs no mesh of its own, as a stopped Node's join
/// does: in `db`'s records, then asking the Nodes they name and the bootstrap nodes, within 8 s.
pub async fn find_through(
    net: &Net,
    config: &MeshConfig,
    db: &Path,
    device: &str,
) -> Option<MeshRecord> {
    let own = device_id(&net.endpoint.id());
    let now = now_ms();
    let mut candidates = Vec::new();
    match Table::open(db, &own, &config.network, now) {
        Ok((table, _)) => {
            if let Some(entry) = table.get(device).filter(|entry| entry.online(now)) {
                return Some(entry.record.clone());
            }
            candidates.extend(
                recent(&table, &own, config.allow_private)
                    .into_iter()
                    .filter_map(|entry| record_addr(&entry.record, config.allow_private).ok()),
            );
        }
        Err(error) => tracing::debug!(event = "mesh.table_unavailable", %error),
    }
    for value in &config.bootstrap {
        if let Ok(link) = bootstrap_link(value)
            && let Ok(addr) = super::link_addr(&link).await
        {
            candidates.push(addr);
        }
    }
    let alpn = exchange_alpn(&config.network);
    let request = Request::Lookup {
        device_id: device.to_owned(),
    };
    let mut asks: FuturesUnordered<_> = candidates
        .into_iter()
        .filter(|addr| addr.id != net.endpoint.id())
        .map(|addr| exchange(&net.endpoint, addr, &alpn, &request))
        .collect();
    let signed = tokio::time::timeout(LOOKUP_BUDGET, async {
        while let Some(reply) = asks.next().await {
            if let Some(signed) = reply
                .ok()
                .and_then(|reply| found(&reply, &config.network, device))
            {
                return Some(signed);
            }
        }
        None
    })
    .await
    .ok()
    .flatten()?;
    MeshRecord::parse(&signed.payload).ok()
}

/// `url` answers `/api/info` as `device`, over HTTPS to an address checked to be public unless
/// `allow_private`.
async fn verify_gateway(url: &str, device: &str, allow_private: bool) -> Result<()> {
    let url = crate::config::http_origin(url)?;
    ensure!(url.scheme() == "https", "a gateway is an HTTPS origin");
    let host = url.host_str().context("the gateway has no host")?;
    let port = url
        .port_or_known_default()
        .context("the gateway has no port")?;
    let addresses: Vec<SocketAddr> = tokio::time::timeout(
        GATEWAY_TIMEOUT,
        tokio::net::lookup_host((host.trim_matches(['[', ']']), port)),
    )
    .await
    .context("the gateway did not resolve in time")??
    .collect();
    let address = *addresses.first().context("the gateway does not resolve")?;
    ensure!(
        allow_private
            || addresses
                .iter()
                .all(|address| crate::fetch::public(address.ip())),
        "the gateway is on a private network"
    );
    let mut client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(GATEWAY_TIMEOUT)
        .timeout(GATEWAY_TIMEOUT);
    if let Some(domain) = url.domain() {
        // Connect to the address checked above, not to a second lookup's answer.
        client = client.resolve(domain, address);
    }
    let mut response = client
        .build()?
        .get(url.join("/api/info")?)
        .send()
        .await?
        .error_for_status()?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len() + chunk.len() <= GATEWAY_INFO_BYTES,
            "the gateway's /api/info is too large"
        );
        body.extend_from_slice(&chunk);
    }
    let info: Value = serde_json::from_slice(&body)?;
    ensure!(
        info["device_id"] == device,
        "the gateway answers for another device"
    );
    Ok(())
}

#[derive(Debug, Clone)]
struct Exchange(Weak<Mesh>);

impl ProtocolHandler for Exchange {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if let Some(mesh) = self.0.upgrade() {
            mesh.serve(connection).await;
        }
        Ok(())
    }
}

/// Finds devices for this endpoint's dials in the mesh records: an expired record still names
/// where its device was, and a neighbor may know where it is now.
#[derive(Debug)]
struct Lookup(Weak<Mesh>);

impl AddressLookup for Lookup {
    fn publish(&self, _data: &EndpointData) {
        if let Some(mesh) = self.0.upgrade() {
            mesh.wake.notify_one();
        }
    }

    fn resolve(
        &self,
        id: EndpointId,
    ) -> Option<BoxStream<'static, Result<Item, address_lookup::Error>>> {
        let mesh = self.0.upgrade()?;
        let device = device_id(&id);
        if device == mesh.device_id {
            return None;
        }
        let held = mesh.table.lock().expect("mesh table").get(&device).cloned();
        let allow_private = mesh.allow_private;
        let item = move |record: &MeshRecord, seen_ms: u64| {
            let addr = record_addr(record, allow_private).ok()?;
            Some(Ok(Item::new(
                EndpointInfo::from_parts(id, EndpointData::new(addr.addrs.into_iter().collect())),
                "mesh",
                Some(seen_ms * 1000),
            )))
        };
        let now = now_ms();
        let held_item = held
            .as_ref()
            .and_then(|entry| item(&entry.record, entry.last_seen_ms));
        if held.as_ref().is_some_and(|entry| entry.online(now)) {
            return Some(futures_util::stream::iter(held_item).boxed());
        }
        let fresh = futures_util::stream::once(async move {
            let record = mesh.ask_neighbors(&device).await?;
            item(&record, now_ms())
        })
        .filter_map(std::future::ready);
        Some(futures_util::stream::iter(held_item).chain(fresh).boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bootstrap_nodes_are_node_links_that_say_where_to_reach_them() {
        let id = "eb963306d731fa49648858f7a5489bd45aa1b96be18adb30e9909b9f8dd4b2dc";
        let full = bootstrap_link(&format!(
            "vibeface://node/{id}?addr=127.0.0.1:4792&relay=http://node.example:3340"
        ))
        .unwrap();
        assert_eq!(full.device_id(), id);
        let addr = super::super::link_addr(&full).await.unwrap();
        assert_eq!(device_id(&addr.id), id);
        assert_eq!(
            addr.ip_addrs().copied().collect::<Vec<_>>(),
            ["127.0.0.1:4792".parse::<SocketAddr>().unwrap()]
        );
        assert_eq!(
            addr.relay_urls().next().unwrap().to_string(),
            "http://node.example:3340/"
        );
        let relay_only =
            bootstrap_link(&format!("vibeface://node/{id}?relay=https://relay.example")).unwrap();
        assert!(relay_only.addrs.is_empty());
        for bad in [
            format!("vibeface://node/{id}"),
            format!("vibeface://{id}@node.example:4792"),
            format!("https://node/{id}?addr=node.example:4792"),
            format!("vibeface://node/{id}?addr=node.example:4792&relay=http://user@x"),
            format!("vibeface://node/{id}?addr=node.example:4792&other=1"),
            "vibeface://node/zz?addr=node.example:4792".into(),
        ] {
            assert!(bootstrap_link(&bad).is_err(), "{bad}");
        }
        let public = MeshConfig::public().unwrap();
        assert!(public.enabled && public.network == DEFAULT_NETWORK);
        assert_eq!(
            bootstrap_link(&public.bootstrap[0]).unwrap().device_id(),
            default_bootstrap_id().unwrap()
        );
        assert!(
            MeshConfig {
                network: "Bad".into(),
                ..MeshConfig::default()
            }
            .validate()
            .is_err()
        );
        let twice = MeshConfig {
            bootstrap: vec![public.bootstrap[0].clone(); 2],
            ..MeshConfig::default()
        };
        assert!(twice.validate().is_err());
    }

    struct Node {
        net: Arc<Net>,
        mesh: Arc<Mesh>,
        router: iroh::protocol::Router,
    }

    impl Node {
        async fn start(secret: &str, db: &Path, bootstrap: Vec<String>) -> Result<Self> {
            let config = crate::net::NetworkConfig {
                bind: Some("127.0.0.1:0".parse()?),
                ..Default::default()
            };
            let net = Arc::new(Net::bind(secret, &config, 0, vec![]).await?);
            let mesh = MeshConfig {
                enabled: true,
                network: "loopback".into(),
                bootstrap,
                allow_private: true,
            };
            net.start_mesh(&mesh, db).await?;
            let mesh = net.mesh().context("the mesh did not start")?;
            let router = mesh
                .accept(iroh::protocol::Router::builder(net.endpoint.clone()))
                .spawn();
            Ok(Self { net, mesh, router })
        }

        fn bootstrap(&self) -> String {
            let port = self.net.endpoint.bound_sockets()[0].port();
            format!(
                "vibeface://node/{}?addr=127.0.0.1:{port}",
                self.mesh.device_id
            )
        }

        /// Waits until this Node holds `count` records that name an address.
        async fn holds(&self, count: usize) -> Result<()> {
            let held = || {
                self.mesh
                    .records()
                    .iter()
                    .filter(|entry| !entry.record.addrs.is_empty())
                    .count()
            };
            tokio::time::timeout(Duration::from_secs(20), async {
                while held() < count {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .with_context(|| format!("a Node holds {} records, not {count}", held()))
        }

        async fn stop(self) {
            let _ = self.router.shutdown().await;
            self.net.close().await;
        }
    }

    fn signed_record(identity: &vibeface_secure::Identity) -> SignedMeshRecord {
        let record = MeshRecord {
            v: 1,
            network: "loopback".into(),
            device_id: identity.device_id(),
            seq: now_ms(),
            ttl_s: record::DEFAULT_TTL_S,
            addrs: vec![],
            relay_url: None,
            reachable: Reachable::Unknown,
            roles: vec![],
        };
        SignedMeshRecord::sign(&record, |payload| {
            Ok(identity.sign(record::DOMAIN, payload)?)
        })
        .unwrap()
    }

    /// A neighbor delivers ten thousand records of new devices into a table that holds at
    /// most 100. It adds about 200 of them, no more than its limit lets through, and none of
    /// the 50 records known before is dropped; a member's deliveries are not limited.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_flood_of_new_records_neither_passes_the_limit_nor_drops_known_ones() -> Result<()> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("mesh-flood-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let secret = crate::crypto::LocalIdentity::generate()
            .secret_key_hex
            .clone();
        let node = Node::start(&secret, &dir.join("mesh.sqlite3"), vec![]).await?;
        let result = async {
            node.mesh.table().set_capacity(100);
            let known: Vec<vibeface_secure::Identity> = (0..50)
                .map(|_| vibeface_secure::Identity::generate().unwrap())
                .collect();
            let (accepted, _) = node
                .mesh
                .accept_records(known.iter().map(signed_record).collect(), None);
            assert_eq!(accepted.len(), 50);

            let flood: Vec<SignedMeshRecord> = (0..10_000)
                .map(|_| signed_record(&vibeface_secure::Identity::generate().unwrap()))
                .collect();
            let neighbor = endpoint_id(&crate::crypto::LocalIdentity::generate().device_id()?)?;
            let started = Instant::now();
            let mut added = 0;
            let mut limited = false;
            for signed in flood {
                let (accepted, dropped) = node.mesh.accept_records(vec![signed], Some(neighbor));
                added += accepted.len();
                limited |= dropped;
            }
            let allowed =
                NEW_RECORDS_BURST + started.elapsed().as_secs_f64() * NEW_RECORDS_PER_SECOND + 1.0;
            assert!(limited && added as f64 <= allowed, "{added} > {allowed}");
            {
                let table = node.mesh.table();
                assert!(table.entries().count() <= 100);
                assert!(
                    known
                        .iter()
                        .all(|identity| table.get(&identity.device_id()).is_some()),
                    "a record known before the flood was dropped"
                );
            }

            let member = vibeface_secure::Identity::generate()?;
            node.mesh.join_peers(&[member.device_id()]).await;
            let from_member: Vec<SignedMeshRecord> = (0..300)
                .map(|_| signed_record(&vibeface_secure::Identity::generate().unwrap()))
                .collect();
            let (_, limited) = node
                .mesh
                .accept_records(from_member, Some(endpoint_id(&member.device_id())?));
            assert!(!limited, "a member's deliveries are not limited");
            Ok::<_, anyhow::Error>(())
        }
        .await;
        node.stop().await;
        let _ = std::fs::remove_dir_all(dir);
        result
    }

    /// A digest's filter of a full table fits in a request, and claims few records it lacks.
    #[test]
    fn a_full_tables_digest_filter_fits_in_a_request() {
        let held: Vec<(String, u64)> = (0..table::CAPACITY as u64)
            .map(|n| (format!("{n:064x}"), n))
            .collect();
        let have = Have::new(&held);
        let request = serde_json::to_vec(&Request::Digest { have: have.clone() }).unwrap();
        assert!(request.len() <= MAX_REQUEST, "{}", request.len());
        let bits = have.open().unwrap();
        assert!(
            held.iter()
                .all(|(device, seq)| have.contains(&bits, device, *seq))
        );
        let wrong = (0..10_000u64)
            .filter(|n| have.contains(&bits, &format!("{n:064x}"), n + 1))
            .count();
        assert!(wrong < 500, "{wrong} of 10000 claimed wrongly");
        let empty = Have::new(&[]);
        assert!(!empty.contains(&empty.open().unwrap(), &held[0].0, 0));
    }

    /// D joins through B alone and learns every Node; restarted with no bootstrap node, it joins
    /// again through the Nodes its table remembers.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_learns_the_mesh_through_one_bootstrap_node_and_rejoins_from_its_table()
    -> Result<()> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("mesh-loopback-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let secrets: Vec<String> = (0..4)
            .map(|_| {
                crate::crypto::LocalIdentity::generate()
                    .secret_key_hex
                    .clone()
            })
            .collect();
        let mut nodes = Vec::new();
        let result = async {
            let a = Node::start(&secrets[0], &dir.join("a.sqlite3"), vec![]).await?;
            let a_bootstrap = a.bootstrap();
            nodes.push(a);
            for (secret, db) in [(&secrets[1], "b.sqlite3"), (&secrets[2], "c.sqlite3")] {
                nodes.push(Node::start(secret, &dir.join(db), vec![a_bootstrap.clone()]).await?);
            }
            for node in &nodes {
                node.holds(3).await?;
            }
            let d = Node::start(
                &secrets[3],
                &dir.join("d.sqlite3"),
                vec![nodes[1].bootstrap()],
            )
            .await?;
            d.holds(4).await?;
            assert_eq!(d.mesh.counts().0, 4);

            let (a_id, c_id) = (
                nodes[0].mesh.device_id.clone(),
                nodes[2].mesh.device_id.clone(),
            );
            let reply = d
                .mesh
                .request(&a_id, &json!({"type":"lookup","device_id":c_id}))
                .await?;
            assert!(found(&reply, "loopback", &c_id).is_some());
            let unknown = d.mesh.request(&a_id, &json!({"type":"relay_grant"})).await;
            assert!(unknown.unwrap_err().to_string().contains("invalid request"));
            let large = json!({"type":"lookup","device_id":"a".repeat(MAX_REQUEST)});
            let large = d.mesh.request(&a_id, &large).await.unwrap_err();
            assert!(large.to_string().contains("frame too large"), "{large}");
            let stranger = endpoint_id(&crate::crypto::LocalIdentity::generate().device_id()?)?;
            let digest = |have: &[(String, u64)]| Request::Digest {
                have: Have::new(have),
            };
            let page = nodes[0].mesh.answer(stranger, digest(&[])).await?;
            assert!(
                page["records"]
                    .as_array()
                    .is_some_and(|records| records.len() >= 3)
            );
            assert!(
                nodes[0].mesh.answer(stranger, digest(&[])).await.is_err(),
                "one digest every 10 s"
            );
            // The filter leaves out the records the asker holds.
            let held: Vec<(String, u64)> = nodes[0]
                .mesh
                .records()
                .iter()
                .map(|entry| (entry.record.device_id.clone(), entry.record.seq))
                .collect();
            let other = endpoint_id(&crate::crypto::LocalIdentity::generate().device_id()?)?;
            let page = nodes[0].mesh.answer(other, digest(&held)).await?;
            assert_eq!(page["records"], json!([]));
            assert!(Have::new(&held).bits.len() < MAX_REQUEST);

            d.stop().await;
            let d = Node::start(&secrets[3], &dir.join("d.sqlite3"), vec![]).await?;
            assert!(d.mesh.records().len() >= 4, "the table outlives a restart");
            let rejoined = tokio::time::timeout(Duration::from_secs(20), async {
                while d.mesh.neighbors().is_empty() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
            nodes.push(d);
            rejoined.context("the restarted Node did not rejoin from its table")
        }
        .await;
        for node in nodes {
            node.stop().await;
        }
        let _ = std::fs::remove_dir_all(dir);
        result
    }
}
