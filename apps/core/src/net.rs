//! Node transport, typed address rendezvous, and an optional business relay on one iroh Endpoint.
//!
//! Every request is one QUIC bidirectional stream carrying length-prefixed JSON frames. The
//! remote `EndpointId` is the peer's `DeviceId`, proven by the QUIC TLS handshake.
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use futures_util::{FutureExt as _, StreamExt as _, stream::FuturesUnordered};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey,
    address_lookup::memory::MemoryLookup,
    endpoint::{
        BindOpts, Connection, PortmapperConfig, ReadExactError, RecvStream, SendStream, presets,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    num::{NonZeroU32, NonZeroUsize},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

pub mod bootstrap;
mod discovery;
mod lan;
pub mod pkarr;

pub const DISCOVERY: &[u8] = b"vibeface/discovery/1";
pub const SYNC: &[u8] = b"vibeface/sync/1";
pub const BLOB: &[u8] = b"vibeface/blob/1";
pub const RPC: &[u8] = b"vibeface/rpc/1";
pub const CLIENT: &[u8] = b"vibeface/client/1";
pub const JOIN: &[u8] = b"vibeface/join/1";
/// A decision sent to a session through another device; the target checks its token.
pub const DECISION: &[u8] = b"vibeface/decision/1";
pub const SERVICE: &[u8] = b"vibeface/service/1";
/// Every Node answers discovery probes; registrations and lookups only with `serve_discovery`.
pub const ALPNS: [&[u8]; 8] = [SYNC, BLOB, RPC, CLIENT, JOIN, DECISION, SERVICE, DISCOVERY];
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_RELAYS: usize = 16;
const RELAY_TOKEN_DOMAIN: &[u8] = b"vibeface:relay:v1";

/// A peer that was not reached in time or did not answer before the timeout. A request it may
/// have received is reconciled before it is sent again.
#[derive(Debug)]
pub struct Unanswered(String);

impl Unanswered {
    pub fn error(message: String) -> anyhow::Error {
        anyhow::Error::new(Self(message))
    }

    pub fn is(error: &anyhow::Error) -> bool {
        error.downcast_ref::<Self>().is_some()
    }
}

impl std::fmt::Display for Unanswered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unanswered {}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    #[serde(default)]
    pub pkarr: pkarr::PkarrConfig,
    /// UDP address of the iroh endpoint; defaults to all interfaces on the `listen` port.
    #[serde(default)]
    pub bind: Option<SocketAddr>,
    /// Extra iroh relays besides the ones the personal network's Nodes run.
    #[serde(default)]
    pub relays: Vec<Relay>,
    /// Also use n0's public relays.
    #[serde(default)]
    pub n0_relays: bool,
    /// Infrastructure this Node uses for address discovery and simultaneous direct probes.
    #[serde(default)]
    pub discovery: Vec<String>,
    /// Answer other devices' discovery registrations and lookups on this Node's UDP endpoint.
    #[serde(default)]
    pub serve_discovery: bool,
    /// Run an iroh relay on this address.
    #[serde(default)]
    pub relay_listen: Option<SocketAddr>,
    /// The relay's URL as other Nodes reach it; defaults to `http://<relay_listen>`.
    #[serde(default)]
    pub relay_url: Option<String>,
    /// A token that admits devices outside the personal network to the relay.
    #[serde(default)]
    pub relay_operator_token: Option<String>,
    /// The relay admits any device, within `relay_rate` and `relay_max_clients`.
    #[serde(default)]
    pub relay_open: bool,
    /// Bytes a second the relay reads from each connection; unlimited when unset.
    #[serde(default)]
    pub relay_rate: Option<NonZeroU32>,
    /// Connections from outside the personal network the relay holds at once; unlimited when
    /// unset.
    #[serde(default)]
    pub relay_max_clients: Option<NonZeroUsize>,
    /// Connections the relay takes from one client address; unlimited when unset.
    #[serde(default)]
    pub relay_max_clients_per_ip: Option<NonZeroUsize>,
    /// Device IDs the relay refuses.
    #[serde(default)]
    pub relay_blocked: Vec<String>,
    /// How this Node finds public infrastructure. Off in `default()`, so fixtures stay offline;
    /// initialization turns the public service on for ordinary Nodes.
    #[serde(default)]
    pub bootstrap: bootstrap::Bootstrap,
}

/// A relay this Node uses, with the token it requires of devices outside its personal network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relay {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    Direct,
    Relay,
}

/// A relay token file holds 64 hex characters.
pub fn read_token_file(path: &std::path::Path) -> Result<String> {
    let token = std::fs::read_to_string(path).context("the relay token file is unreadable")?;
    let token = token.trim();
    valid_token(token)?;
    Ok(token.to_owned())
}

fn valid_token(token: &str) -> Result<()> {
    ensure!(
        token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()),
        "a relay token is 64 hex characters"
    );
    Ok(())
}

/// Compares in time independent of where the inputs differ.
pub fn same_token(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |acc, (x, y)| acc | (x ^ y))
            == 0
}
impl NetworkConfig {
    pub fn validate(&self) -> Result<()> {
        self.pkarr.validate()?;
        ensure!(self.relays.len() <= MAX_RELAYS, "too many relays");
        ensure!(self.discovery.len() <= 4, "too many discovery servers");
        for server in &self.discovery {
            discovery::validate_server(server)?;
        }
        self.bootstrap.validate()?;
        let mut urls = HashSet::new();
        for relay in &self.relays {
            ensure!(
                urls.insert(relay_url(&relay.url)?),
                "a relay is listed once"
            );
            if let Some(token) = &relay.token {
                valid_token(token)?;
            }
        }
        if let Some(url) = &self.relay_url {
            relay_url(url)?;
        }
        ensure!(
            self.relay_listen.is_some()
                || (self.relay_url.is_none()
                    && self.relay_operator_token.is_none()
                    && !self.relay_open
                    && self.relay_rate.is_none()
                    && self.relay_max_clients.is_none()
                    && self.relay_max_clients_per_ip.is_none()
                    && self.relay_blocked.is_empty()),
            "relay_url and the relay's admission and limits require relay_listen"
        );
        // Anyone may connect to an open relay, so it is bounded.
        ensure!(
            !self.relay_open
                || (self.relay_operator_token.is_none()
                    && self.relay_rate.is_some()
                    && self.relay_max_clients.is_some()),
            "an open relay takes no operator token and needs relay_rate and relay_max_clients"
        );
        if let Some(token) = &self.relay_operator_token {
            valid_token(token)?;
        }
        for device in &self.relay_blocked {
            vibeface_secure::validate_device_id(device)?;
        }
        if let Some(listen) = self.relay_listen {
            ensure!(
                self.relay_url.is_some() || !listen.ip().is_unspecified(),
                "a relay on all interfaces needs relay_url"
            );
        }
        Ok(())
    }
    /// The URL other Nodes use for this Node's relay.
    pub fn own_relay(&self) -> Result<Option<RelayUrl>> {
        match (&self.relay_url, self.relay_listen) {
            (Some(url), _) => relay_url(url).map(Some),
            (None, Some(listen)) => relay_url(&format!("http://{listen}")).map(Some),
            (None, None) => Ok(None),
        }
    }

    /// Every discovery service this Node registers with: the configured ones and the bootstrap
    /// one, once each.
    pub fn discovery_servers(&self) -> Result<Vec<String>> {
        let mut servers = self.discovery.clone();
        if let Some(discovery) = self.bootstrap.discovery()?
            && !servers.contains(&discovery)
        {
            servers.push(discovery);
        }
        Ok(servers)
    }
}

pub fn relay_url(value: &str) -> Result<RelayUrl> {
    let url = reqwest::Url::parse(value).context("invalid relay URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "relay URL must be an HTTP(S) URL without credentials"
    );
    value
        .parse()
        .map_err(|error| anyhow!("invalid relay URL: {error}"))
}

/// A joiner's relay credential: only someone holding the invite can derive it.
pub fn relay_token(invite_hash: &str) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(RELAY_TOKEN_DOMAIN);
    hasher.update(invite_hash.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn endpoint_id(device_id: &str) -> Result<EndpointId> {
    let bytes: [u8; 32] = hex::decode(device_id)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .context("invalid device ID")?;
    EndpointId::from_bytes(&bytes).map_err(|error| anyhow!("invalid device ID: {error}"))
}

pub fn device_id(id: &EndpointId) -> String {
    hex::encode(id.as_bytes())
}

/// A Node's signed statement of where it can be reached, exchanged only inside the network.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AddressRecord {
    pub version: u8,
    pub network_id: String,
    pub device_id: String,
    pub seq: u64,
    pub endpoint: EndpointAddr,
    pub gateway_url: Option<String>,
}

/// `payload` is the record's JSON text exactly as signed, so browsers verify the same bytes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SignedAddress {
    pub payload: String,
    pub signature: String,
}

pub const ADDRESS_DOMAIN: &str = "address";

impl SignedAddress {
    /// The record, if `device_id` signed it for `network_id`; membership is the caller's check.
    pub fn open(&self, network_id: &str) -> Result<AddressRecord> {
        ensure!(self.payload.len() <= 8192, "address record too large");
        let record: AddressRecord = serde_json::from_str(&self.payload)?;
        ensure!(
            record.version == 1 && record.network_id == network_id,
            "address record belongs to another network"
        );
        ensure!(
            record.endpoint.id == endpoint_id(&record.device_id)?,
            "address record names another endpoint"
        );
        if let Some(url) = &record.gateway_url {
            let url = reqwest::Url::parse(url)?;
            ensure!(
                matches!(url.scheme(), "http" | "https") && url.path() == "/",
                "invalid gateway URL"
            );
        }
        vibeface_secure::verify(
            &record.device_id,
            ADDRESS_DOMAIN,
            self.payload.as_bytes(),
            &self.signature,
        )?;
        Ok(record)
    }
}

pub struct Net {
    pub endpoint: Endpoint,
    lookup: MemoryLookup,
    discovery: discovery::Discovery,
    lan: OnceLock<lan::Lan>,
    relays: Arc<tokio::sync::Mutex<HashSet<RelayUrl>>>,
    connections: Mutex<HashMap<(EndpointId, &'static [u8]), Connection>>,
}
impl std::fmt::Debug for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Net")
    }
}

/// Relay configurations by URL. The first configuration of a URL stays unless it carried no
/// credential, so a bare copy of an authenticated relay never strips its token.
fn merge_relays(relays: Vec<(RelayUrl, Option<String>)>) -> Vec<(RelayUrl, Option<String>)> {
    let mut merged: Vec<(RelayUrl, Option<String>)> = Vec::new();
    for (url, token) in relays {
        match merged.iter_mut().find(|(known, _)| *known == url) {
            Some((_, existing)) => {
                if existing.is_none() {
                    *existing = token;
                }
            }
            None => merged.push((url, token)),
        }
    }
    merged
}

impl Net {
    /// Binds the endpoint. Addresses stay in tickets and the personal network unless the
    /// operator configures a discovery service or public bootstrap is on.
    pub async fn bind(
        secret_key_hex: &str,
        config: &NetworkConfig,
        listen_port: u16,
        relays: Vec<(RelayUrl, Option<String>)>,
    ) -> Result<Self> {
        let seed: [u8; 32] = hex::decode(secret_key_hex)?
            .try_into()
            .map_err(|_| anyhow!("invalid device key"))?;
        let map = RelayMap::empty();
        let mut urls: Vec<(RelayUrl, Option<String>)> = relays;
        for relay in &config.relays {
            urls.push((relay_url(&relay.url)?, relay.token.clone()));
        }
        if let Some(own) = config.own_relay()? {
            urls.push((own, None));
        }
        if config.n0_relays {
            urls.extend(
                iroh::defaults::prod::default_relay_map()
                    .urls::<Vec<_>>()
                    .into_iter()
                    .map(|url| (url, None)),
            );
        }
        let mut known = HashSet::new();
        for (url, token) in merge_relays(urls) {
            known.insert(url.clone());
            map.insert(url.clone(), Arc::new(relay_config(url, token)));
        }
        let known = Arc::new(tokio::sync::Mutex::new(known));
        let lookup = MemoryLookup::new();
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from_bytes(&seed))
            .alpns(ALPNS.iter().map(|alpn| alpn.to_vec()).collect())
            .relay_mode(RelayMode::Custom(map))
            .address_lookup(lookup.clone());
        for url in &config.pkarr.urls {
            let url = reqwest::Url::parse(url)?;
            if config.pkarr.publish {
                let publisher = iroh::address_lookup::PkarrPublisher::builder(url.clone());
                let publisher = if config.pkarr.publish_direct {
                    publisher.addr_filter(iroh::address_lookup::AddrFilter::unfiltered())
                } else {
                    publisher
                };
                builder = builder.address_lookup(publisher);
            }
            if config.pkarr.resolve {
                builder = builder.address_lookup(pkarr::FreshResolver::new(url)?);
            }
        }
        // iroh starts with wildcard sockets on random ports; replace them so a configured
        // address is the only one, and the default uses the listen port.
        builder = builder.clear_ip_transports();
        // A router mapping would publish an address the user did not configure.
        if config.bind.is_some() {
            builder = builder.portmapper_config(PortmapperConfig::Disabled);
        }
        let invalid = |error| anyhow!("invalid QUIC bind address: {error}");
        builder = match config.bind {
            Some(bind) => builder.bind_addr(bind).map_err(invalid)?,
            None => builder
                .bind_addr(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), listen_port))
                .map_err(invalid)?
                .bind_addr_with_opts(
                    SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), listen_port),
                    BindOpts::default().set_is_required(false),
                )
                .map_err(invalid)?,
        };
        let endpoint = builder
            .bind()
            .await
            .context("QUIC endpoint failed to start")?;
        // Registrants may make an open relay their home relay; any other is for those it admits.
        let offered = if config.relay_open {
            config.own_relay()?
        } else {
            None
        };
        // Registration runs in the background: the local API, maintenance and LAN start
        // whether or not the public infrastructure answers.
        let discovery = discovery::Discovery::new(
            &endpoint,
            &config.discovery_servers()?,
            config.serve_discovery,
            offered,
            config.own_relay()?,
            known.clone(),
        )?;
        Ok(Self {
            endpoint,
            lookup,
            discovery,
            lan: OnceLock::new(),
            relays: known,
            connections: Mutex::new(HashMap::new()),
        })
    }

    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// Remembers where a peer is and adds relays it uses that this Node does not know yet.
    pub async fn learn(&self, addr: &EndpointAddr) {
        if addr.id == self.endpoint.id() {
            return;
        }
        self.lookup.set_endpoint_info(addr.clone());
        for url in addr.relay_urls() {
            let mut relays = self.relays.lock().await;
            if relays.insert(url.clone()) {
                self.endpoint
                    .insert_relay(url.clone(), Arc::new(relay_config(url.clone(), None)))
                    .await;
            }
        }
    }

    /// Adds `url` with `token`. iroh reads a relay's credential when it first connects to the
    /// relay, so this comes before the first dial through it.
    pub async fn insert_relay(&self, url: RelayUrl, token: Option<String>) {
        self.relays.lock().await.insert(url.clone());
        self.endpoint
            .insert_relay(url.clone(), Arc::new(relay_config(url, token)))
            .await;
    }

    /// Advertises this Node on the LAN and finds the other member Nodes there; `wake` hears
    /// which one appeared. Without mDNS, dialing keeps the other addresses.
    pub fn start_lan(&self, wake: impl Fn(&str) + Send + Sync + 'static) {
        let sockets = self.endpoint.bound_sockets();
        let Some(port) = sockets.first().map(SocketAddr::port) else {
            return;
        };
        // An address family listening on another port, or not at all, would be advertised
        // with a port that does not answer.
        let listens = |v6: bool| {
            sockets
                .iter()
                .any(|s| s.is_ipv6() == v6 && s.port() == port)
        };
        let off = [
            (false, mdns_sd::IfKind::IPv4),
            (true, mdns_sd::IfKind::IPv6),
        ]
        .into_iter()
        .filter(|(v6, _)| !listens(*v6))
        .map(|(_, kind)| kind)
        .collect();
        match lan::Lan::start(device_id(&self.endpoint.id()), port, off, Box::new(wake)) {
            Ok(lan) => {
                let _ = self.lan.set(lan);
            }
            Err(error) => tracing::warn!(event = "lan.off", %error, "LAN discovery is off"),
        }
    }

    /// Withdraws this Node's LAN advert before it exits.
    pub async fn stop_lan(&self) {
        if let Some(lan) = self.lan.get() {
            lan.stop().await;
        }
    }

    /// `members` are the other member Nodes of `network`; `None` stops advertising.
    pub fn refresh_lan(&self, network: Option<&str>, members: &[String]) {
        if let Some(lan) = self.lan.get() {
            lan.refresh(network, members);
        }
    }

    pub fn forget(&self, device_id: &str) {
        if let Ok(id) = endpoint_id(device_id) {
            self.lookup.remove_endpoint_info(id);
            self.close_connections(id, b"revoked");
        }
    }

    /// Drops connections to a peer that restarted. QUIC cannot tell they are dead until its
    /// idle timeout, so requests on them would hang that long.
    pub fn peer_restarted(&self, device_id: &str) {
        if let Ok(id) = endpoint_id(device_id) {
            self.close_connections(id, b"peer restarted");
        }
    }

    fn close_connections(&self, id: EndpointId, reason: &[u8]) {
        let mut connections = self.connections.lock().expect("connection cache");
        connections.retain(|(peer, _), connection| {
            let keep = *peer != id;
            if !keep {
                connection.close(0u32.into(), reason);
            }
            keep
        });
    }

    pub fn connected(&self, device_id: &str) -> bool {
        let Ok(id) = endpoint_id(device_id) else {
            return false;
        };
        self.connections
            .lock()
            .expect("connection cache")
            .iter()
            .any(|((peer, _), connection)| *peer == id && connection.close_reason().is_none())
    }

    /// Reuses an open connection; otherwise dials through the address book within `DIAL_TIMEOUT`.
    pub async fn connect(&self, device_id: &str, alpn: &'static [u8]) -> Result<Connection> {
        let id = endpoint_id(device_id)?;
        ensure!(id != self.endpoint.id(), "a Node does not dial itself");
        if let Some(connection) = self
            .connections
            .lock()
            .expect("connection cache")
            .get(&(id, alpn))
            .filter(|connection| connection.close_reason().is_none())
        {
            return Ok(connection.clone());
        }
        let connection = tokio::time::timeout(DIAL_TIMEOUT, async {
            let hint = self.discovery.resolve(id, Duration::from_secs(1)).await;
            let mut addresses = self
                .lookup
                .get_endpoint_info(id)
                .map(|info| info.addrs().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            // Dialed as given, not added to the relay map: a relay there could become this
            // Node's home relay and refuse it.
            if let Some(found) = hint {
                addresses.push(iroh::TransportAddr::Ip(found.address));
                addresses.extend(found.relays.into_iter().map(iroh::TransportAddr::Relay));
            }
            let lan = self.lan.get().map(|lan| lan.hints(device_id));
            addresses.extend(lan.into_iter().flatten().map(iroh::TransportAddr::Ip));
            self.endpoint
                .connect(EndpointAddr::from_parts(id, addresses), alpn)
                .await
        })
        .await
        .map_err(|_| Unanswered::error("peer did not answer in time".into()))?
        .map_err(|error| Unanswered::error(format!("peer is unreachable: {error}")))?;
        self.connections
            .lock()
            .expect("connection cache")
            .insert((id, alpn), connection.clone());
        Ok(connection)
    }

    /// The relays Discovery has registered for `device_id`, waiting up to `wait` for this
    /// endpoint's registration sessions to come up.
    pub async fn discovered_relays(
        &self,
        device_id: &str,
        wait: Duration,
    ) -> Result<Vec<RelayUrl>> {
        let found = self.discovery.resolve(endpoint_id(device_id)?, wait).await;
        Ok(found.map(|found| found.relays).unwrap_or_default())
    }

    pub(crate) fn discovery_router(self: &Arc<Self>) -> iroh::protocol::Router {
        iroh::protocol::Router::builder(self.endpoint.clone())
            .accept(DISCOVERY, discovery::Handler(Arc::downgrade(self)))
            .spawn()
    }

    pub async fn serve_discovery(&self, connection: Connection) -> Result<()> {
        self.discovery.serve(connection).await
    }

    pub fn path_kind(&self, device_id: &str) -> Option<PathKind> {
        let id = endpoint_id(device_id).ok()?;
        let connections = self.connections.lock().expect("connection cache");
        connections
            .iter()
            .filter(|((peer, _), connection)| *peer == id && connection.close_reason().is_none())
            .find_map(|(_, connection)| {
                connection
                    .paths()
                    .iter()
                    .find(|path| path.is_selected())
                    .and_then(|path| {
                        if path.is_ip() {
                            Some(PathKind::Direct)
                        } else if path.is_relay() {
                            Some(PathKind::Relay)
                        } else {
                            None
                        }
                    })
            })
    }

    /// One request on its own stream; the response is the stream's only frame.
    pub async fn request(
        &self,
        device_id: &str,
        alpn: &'static [u8],
        request: &Value,
        timeout: Duration,
    ) -> Result<Value> {
        let connection = self.connect(device_id, alpn).await?;
        let result = tokio::time::timeout(timeout, call(&connection, request))
            .await
            .map_err(|_| {
                Unanswered::error("peer response timed out; outcome may be unknown".into())
            })
            .flatten();
        if result.is_err() && connection.close_reason().is_none() {
            // The peer may have restarted behind a connection QUIC still thinks is open.
            let id = connection.remote_id();
            let mut connections = self.connections.lock().expect("connection cache");
            if connections
                .get(&(id, alpn))
                .is_some_and(|cached| cached.stable_id() == connection.stable_id())
            {
                connections.remove(&(id, alpn));
            }
        }
        result
    }

    pub async fn close(&self) {
        self.endpoint.close().await;
    }
}

fn relay_config(url: RelayUrl, token: Option<String>) -> iroh::RelayConfig {
    let config = iroh::RelayConfig::new(url, None);
    match token {
        Some(token) => config.with_auth_token(token),
        None => config,
    }
}

pub async fn call(connection: &Connection, request: &Value) -> Result<Value> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|error| anyhow!("peer connection closed: {error}"))?;
    write_frame(&mut send, request).await?;
    send.finish().ok();
    let response = read_frame(&mut recv)
        .await?
        .context("peer closed the stream without answering")?;
    if let Some(error) = response.get("error").and_then(Value::as_str) {
        bail!("{error}");
    }
    Ok(response)
}

pub async fn write_frame(send: &mut SendStream, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_FRAME, "frame too large");
    let mut frame = (bytes.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&bytes);
    send.write_all(&frame)
        .await
        .map_err(|error| anyhow!("peer stream failed: {error}"))
}

/// `None` when the peer finished the stream cleanly between frames.
pub async fn read_frame(recv: &mut RecvStream) -> Result<Option<Value>> {
    let mut length = [0u8; 4];
    match recv.read_exact(&mut length).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(error) => bail!("peer stream failed: {error}"),
    }
    let length = u32::from_be_bytes(length) as usize;
    ensure!(length <= MAX_FRAME, "frame too large");
    let mut bytes = vec![0; length];
    recv.read_exact(&mut bytes)
        .await
        .map_err(|error| anyhow!("peer stream failed: {error}"))?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

/// Answers each stream of `connection` with `handle`, one stream at a time per task.
pub async fn serve_streams<F, Fut>(connection: Connection, handle: F)
where
    F: Fn(Value) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = Result<Value>> + Send,
{
    let mut replies = FuturesUnordered::new();
    loop {
        tokio::select! {
            stream = connection.accept_bi() => {
                let Ok((mut send, mut recv)) = stream else { break };
                let handle = handle.clone();
                replies.push(std::panic::AssertUnwindSafe(async move {
                    let response = match read_frame(&mut recv).await {
                        Ok(Some(request)) => handle(request)
                            .await
                            .unwrap_or_else(|error| serde_json::json!({"error":format!("{error:#}")})),
                        Ok(None) => return,
                        Err(error) => serde_json::json!({"error":format!("{error:#}")}),
                    };
                    if write_frame(&mut send, &response).await.is_ok() {
                        send.finish().ok();
                        let _ = send.stopped().await;
                    }
                }).catch_unwind());
            }
            Some(result) = replies.next(), if !replies.is_empty() => {
                if result.is_err() {
                    tracing::warn!(event = "net.request_panicked", "peer request handler panicked");
                }
            }
        }
    }
}

/// A relay this Node runs, behind a front on `listen` when it limits connections per address.
pub struct RelayServer {
    server: iroh_relay::server::Server,
    /// Where devices reach the relay.
    pub addr: SocketAddr,
    front: Option<Front>,
}

struct Front(tokio::task::JoinHandle<()>);
impl Drop for Front {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl RelayServer {
    pub async fn shutdown(self) {
        drop(self.front);
        let _ = self.server.shutdown().await;
    }
}

/// The relay this Node runs; `access` decides who it admits, `rate` caps the bytes a second it
/// reads from each connection and `per_ip` the connections one client address holds.
pub async fn spawn_relay(
    listen: SocketAddr,
    access: Arc<dyn iroh_relay::server::DynAccessControl>,
    rate: Option<NonZeroU32>,
    per_ip: Option<NonZeroUsize>,
) -> Result<RelayServer> {
    let refused =
        |error: &dyn std::fmt::Display| anyhow!("relay failed to start on {listen}: {error}");
    // The relay never sees client addresses, so a front counts them and passes the bytes on to
    // the relay on loopback.
    let front = match per_ip {
        Some(per_ip) => Some((
            tokio::net::TcpListener::bind(listen)
                .await
                .map_err(|error| refused(&error))?,
            per_ip,
        )),
        None => None,
    };
    let mut relay = iroh_relay::server::RelayConfig::new(match front {
        Some(_) => SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        None => listen,
    });
    relay.access = access;
    relay.limits.client_rx = rate.map(iroh_relay::server::ClientRateLimit::new);
    let mut config = iroh_relay::server::ServerConfig::default();
    config.relay = Some(relay);
    let server = iroh_relay::server::Server::spawn(config)
        .await
        .map_err(|error| refused(&error))?;
    let relay = server.http_addr().context("relay listener missing")?;
    let (addr, front) = match front {
        Some((listener, per_ip)) => (
            listener.local_addr()?,
            Some(Front(tokio::spawn(relay_front(listener, relay, per_ip)))),
        ),
        None => (relay, None),
    };
    Ok(RelayServer {
        server,
        addr,
        front,
    })
}

/// Passes each connection on to the relay unless its client address already holds `per_ip`.
async fn relay_front(listener: tokio::net::TcpListener, relay: SocketAddr, per_ip: NonZeroUsize) {
    let shares = Shares::default();
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            Some(_) = connections.join_next() => {}
            accepted = listener.accept() => match accepted {
                Ok((mut client, peer)) => {
                    // A refused connection closes as it drops.
                    let Some(share) = shares.take(peer.ip(), per_ip) else { continue };
                    connections.spawn(async move {
                        let _share = share;
                        // The relay sends small QUIC datagrams that must not wait to coalesce.
                        let _ = client.set_nodelay(true);
                        // A client gone without closing would otherwise keep its share.
                        let keepalive = socket2::TcpKeepalive::new()
                            .with_time(Duration::from_secs(60))
                            .with_interval(Duration::from_secs(15));
                        let _ = socket2::SockRef::from(&client).set_tcp_keepalive(&keepalive);
                        if let Ok(mut relay) = tokio::net::TcpStream::connect(relay).await {
                            let _ = relay.set_nodelay(true);
                            let (mut from_client, mut to_client) = client.split();
                            let (mut from_relay, mut to_relay) = relay.split();
                            // Either side ending closes both, so a half-closed peer keeps no share.
                            tokio::select! {
                                _ = tokio::io::copy(&mut from_client, &mut to_relay) => {}
                                _ = tokio::io::copy(&mut from_relay, &mut to_client) => {}
                            }
                        }
                    });
                }
                // Out of descriptors, say; retrying at once would spin.
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            },
        }
    }
}

/// What each client address holds of a per-address limit.
#[derive(Clone, Default)]
pub struct Shares(Arc<Mutex<HashMap<IpAddr, usize>>>);

impl Shares {
    /// One more for `ip`'s client address, unless it already holds `limit`.
    pub fn take(&self, ip: IpAddr, limit: NonZeroUsize) -> Option<Share> {
        let client = client_address(ip);
        let mut held = self.0.lock().expect("address shares");
        let count = held.entry(client).or_default();
        (*count < limit.get()).then(|| {
            *count += 1;
            Share(self.clone(), client)
        })
    }
}

/// One share of a client address, given back when dropped.
pub struct Share(Shares, IpAddr);
impl Drop for Share {
    fn drop(&mut self) {
        let mut held = (self.0).0.lock().expect("address shares");
        if let Some(count) = held.get_mut(&self.1) {
            *count -= 1;
            if *count == 0 {
                held.remove(&self.1);
            }
        }
    }
}

/// The address one client controls: an IPv4 address, or the /56 of an IPv6 one, which ISPs
/// commonly delegate to a single subscriber.
pub fn client_address(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & (u128::MAX << 72))),
        v4 => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_settings_are_validated() {
        let relay = |url: &str, token: Option<&str>| Relay {
            url: url.into(),
            token: token.map(Into::into),
        };
        let token = "ab".repeat(32);
        let mut config = NetworkConfig {
            relays: vec![
                relay("https://relay.example", None),
                relay("https://private.example", Some(&token)),
            ],
            ..Default::default()
        };
        config.validate().unwrap();
        config.relays = vec![relay("https://user:secret@relay.example", None)];
        assert!(config.validate().is_err());
        config.relays = vec![relay("https://relay.example", Some("short"))];
        assert!(config.validate().is_err(), "tokens are 64 hex characters");
        config.relays = vec![
            relay("https://relay.example", None),
            relay("https://relay.example/", Some(&token)),
        ];
        assert!(config.validate().is_err(), "a relay is listed once");
        config.relays.clear();
        config.relay_url = Some("https://relay.example".into());
        assert!(config.validate().is_err(), "relay_url alone runs nothing");
        config.relay_listen = Some("0.0.0.0:3340".parse().unwrap());
        config.validate().unwrap();
        config.relay_url = None;
        assert!(config.validate().is_err());
        config.relay_listen = Some("127.0.0.1:3340".parse().unwrap());
        assert_eq!(
            config.own_relay().unwrap().unwrap().to_string(),
            "http://127.0.0.1:3340/"
        );
        config.relay_operator_token = Some(token.clone());
        config.relay_rate = NonZeroU32::new(65536);
        config.relay_max_clients = NonZeroUsize::new(512);
        config.relay_max_clients_per_ip = NonZeroUsize::new(16);
        config.relay_blocked = vec![
            crate::crypto::LocalIdentity::generate()
                .device_id()
                .unwrap(),
        ];
        config.validate().unwrap();
        config.relay_open = true;
        assert!(config.validate().is_err(), "an open relay asks no token");
        config.relay_operator_token = None;
        config.validate().unwrap();
        config.relay_max_clients = None;
        assert!(config.validate().is_err(), "an open relay is bounded");
        config.relay_max_clients = NonZeroUsize::new(512);
        config.relay_rate = None;
        assert!(config.validate().is_err(), "an open relay is paced");
        config.relay_rate = NonZeroU32::new(65536);
        config.relay_blocked = vec!["not-a-device".into()];
        assert!(config.validate().is_err());
        config.relay_blocked.clear();
        config.relay_listen = None;
        assert!(
            config.validate().is_err(),
            "admission and limits belong to a running relay"
        );
    }

    #[test]
    fn a_client_address_is_an_ipv4_address_or_an_ipv6_slash_56() {
        let address = |ip: &str| client_address(ip.parse().unwrap()).to_string();
        assert_eq!(address("203.0.113.7"), "203.0.113.7");
        assert_eq!(address("::ffff:203.0.113.7"), "203.0.113.7");
        assert_eq!(address("2001:db8:1:2ff:3:4:5:6"), "2001:db8:1:200::");
        let shares = Shares::default();
        let two = NonZeroUsize::new(2).unwrap();
        let first = shares.take("2001:db8::1".parse().unwrap(), two).unwrap();
        let _second = shares
            .take("2001:db8:0:1::2".parse().unwrap(), two)
            .unwrap();
        assert!(
            shares
                .take("2001:db8:0:ff::3".parse().unwrap(), two)
                .is_none(),
            "one /56 holds two"
        );
        assert!(
            shares
                .take("2001:db8:0:100::3".parse().unwrap(), two)
                .is_some(),
            "the next /56 has its own"
        );
        drop(first);
        assert!(
            shares.take("2001:db8::3".parse().unwrap(), two).is_some(),
            "a dropped share is given back"
        );
    }

    /// A relay limited per address refuses the connection past the limit and admits it again
    /// once one closes from either end, with relay traffic passing through the front.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_relay_front_holds_each_address_to_its_share() -> Result<()> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let relay = spawn_relay(
            "127.0.0.1:0".parse()?,
            Arc::new(iroh_relay::server::AllowAll),
            None,
            NonZeroUsize::new(1),
        )
        .await?;
        let addr = relay.addr;
        let probe = move |headers: &'static str| async move {
            let mut stream = tokio::net::TcpStream::connect(addr).await?;
            let request = format!("GET /generate_204 HTTP/1.1\r\nHost: relay\r\n{headers}\r\n");
            stream.write_all(request.as_bytes()).await?;
            let mut head = [0; 12];
            let read =
                tokio::time::timeout(Duration::from_secs(5), stream.read(&mut head)).await??;
            Ok::<_, anyhow::Error>((stream, head[..read].to_vec()))
        };
        // The first connection the relay answers once the address's share is back, left open.
        let admitted = move |headers: &'static str| async move {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match probe(headers).await {
                        Ok((stream, head)) if head == b"HTTP/1.1 204" => return stream,
                        _ => tokio::time::sleep(Duration::from_millis(50)).await,
                    }
                }
            })
            .await
        };
        let (held, head) = probe("").await?;
        assert_eq!(head, b"HTTP/1.1 204", "the relay answers through the front");
        // The front closes it unread, which the peer may see as a reset.
        let refused = probe("").await.map(|(_, head)| head);
        assert!(
            refused.as_ref().map_or(true, Vec::is_empty),
            "a second connection from the address is closed: {refused:?}"
        );
        drop(held);
        let _ended = admitted("Connection: close\r\n")
            .await
            .context("the closed connection's share never came back")?;
        // The relay closes `_ended` after answering, so the front ends it while the client
        // still holds its end open.
        admitted("")
            .await
            .context("a connection the relay closed kept its share")?;
        relay.shutdown().await;
        Ok(())
    }

    #[test]
    fn relay_tokens_depend_on_the_invite() {
        assert_ne!(relay_token(&"a".repeat(64)), relay_token(&"b".repeat(64)));
        assert_eq!(relay_token(&"a".repeat(64)).len(), 64);
    }

    #[test]
    fn bootstrap_settings_are_validated() -> Result<()> {
        let mut config = NetworkConfig {
            bootstrap: bootstrap::Bootstrap::Public,
            ..Default::default()
        };
        config.validate()?;
        let servers = config.discovery_servers()?;
        assert_eq!(servers.len(), 1);
        assert!(servers[0].starts_with("vibeface://"));
        config.discovery = servers.clone();
        assert_eq!(
            config.discovery_servers()?.len(),
            1,
            "the bootstrap Discovery is not registered twice"
        );
        assert!(NetworkConfig::default().discovery_servers()?.is_empty());
        // The public node: Discovery and an open relay on one Node.
        let public = NetworkConfig {
            serve_discovery: true,
            relay_listen: Some("0.0.0.0:3340".parse()?),
            relay_url: Some("http://203.0.113.7:3340".into()),
            relay_open: true,
            relay_rate: NonZeroU32::new(65536),
            relay_max_clients: NonZeroUsize::new(512),
            relay_max_clients_per_ip: NonZeroUsize::new(16),
            ..Default::default()
        };
        public.validate()?;
        Ok(())
    }

    #[test]
    fn duplicate_relay_urls_keep_the_credential() -> Result<()> {
        let url: RelayUrl = "https://relay.example".parse().unwrap();
        let other: RelayUrl = "https://other.example".parse().unwrap();
        let merged = merge_relays(vec![
            (url.clone(), Some("grant".into())),
            (url.clone(), None),
            (other.clone(), None),
            (other.clone(), Some("token".into())),
            (url.clone(), Some("later".into())),
        ]);
        assert_eq!(
            merged,
            vec![(url, Some("grant".into())), (other, Some("token".into()))]
        );
        Ok(())
    }

    /// Admits only the one token the test hands out.
    #[derive(Debug)]
    struct TokenOnly {
        token: String,
        attempts: tokio::sync::mpsc::UnboundedSender<bool>,
    }
    impl iroh_relay::server::AccessControl for TokenOnly {
        async fn on_connect(
            &self,
            request: &iroh_relay::server::ClientRequest,
        ) -> iroh_relay::server::Access {
            let allowed = request
                .auth_token()
                .is_some_and(|token| same_token(&token, &self.token));
            let _ = self.attempts.send(allowed);
            if allowed {
                iroh_relay::server::Access::Allow
            } else {
                iroh_relay::server::Access::Deny {
                    reason: Some("token required".into()),
                }
            }
        }
    }

    /// A relay named both with its credential and bare in `relays` is used with the credential:
    /// checked against a real relay that admits only that credential.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_bare_copy_of_an_authenticated_relay_does_not_strip_its_token() -> Result<()> {
        use iroh::Watcher as _;

        let token = crate::crypto::random_hex(32);
        let (attempts, mut received) = tokio::sync::mpsc::unbounded_channel();
        let policy = TokenOnly {
            token: token.clone(),
            attempts,
        };
        let relay = spawn_relay("127.0.0.1:0".parse()?, Arc::new(policy), None, None).await?;
        let url = format!("http://{}", relay.addr);
        let config = NetworkConfig {
            bind: Some("127.0.0.1:0".parse()?),
            relays: vec![Relay {
                url: url.clone(),
                token: None,
            }],
            ..Default::default()
        };
        let identity = crate::crypto::LocalIdentity::generate();
        let net = Net::bind(
            &identity.secret_key_hex,
            &config,
            0,
            vec![(relay_url(&url)?, Some(token))],
        )
        .await?;
        let expected = relay_url(&url)?;
        let mut status = net.endpoint.home_relay_status();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if status
                    .get()
                    .iter()
                    .any(|relay| relay.url() == &expected && relay.is_connected())
                {
                    return Ok::<(), anyhow::Error>(());
                }
                status.updated().await.context("endpoint closed")?;
            }
        })
        .await
        .context("the relay never admitted the endpoint with its credential")??;
        let bare = Net::bind(
            &crate::crypto::LocalIdentity::generate().secret_key_hex,
            &config,
            0,
            vec![],
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(15), async {
            while let Some(allowed) = received.recv().await {
                if !allowed {
                    return Ok::<(), anyhow::Error>(());
                }
            }
            Err(anyhow!("relay closed before rejecting the bare endpoint"))
        })
        .await
        .context("the bare endpoint never attempted relay admission")??;
        assert!(
            !bare
                .endpoint
                .home_relay_status()
                .get()
                .iter()
                .any(|relay| relay.is_connected()),
            "the bare endpoint was rejected while the authenticated endpoint connected"
        );
        net.close().await;
        bare.close().await;
        relay.shutdown().await;
        Ok(())
    }
}
