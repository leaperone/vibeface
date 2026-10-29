//! Node transport, typed address rendezvous, and an optional business relay on one iroh Endpoint.
//!
//! Every request is one QUIC bidirectional stream carrying length-prefixed JSON frames. The
//! remote `EndpointId` is the peer's `DeviceId`, proven by the QUIC TLS handshake.
use anyhow::{Context as _, Result, anyhow, bail, ensure};
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
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

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
pub const ALPNS: [&[u8]; 7] = [SYNC, BLOB, RPC, CLIENT, JOIN, DECISION, SERVICE];
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
    pub relays: Vec<String>,
    /// Also use n0's public relays.
    #[serde(default)]
    pub n0_relays: bool,
    /// Infrastructure this Node uses for address discovery and simultaneous direct probes.
    #[serde(default)]
    pub discovery: Vec<String>,
    #[serde(default)]
    pub relay_mode: RelayServiceMode,
    /// Run an iroh relay or a UDP discovery service on this address.
    #[serde(default)]
    pub relay_listen: Option<SocketAddr>,
    /// The relay's URL as other Nodes reach it; defaults to `http://<relay_listen>`.
    #[serde(default)]
    pub relay_url: Option<String>,
    /// A public entry outside the personal network that browsers come through. Its operator
    /// token is the data directory's `entry-token` file.
    #[serde(default)]
    pub entry: Option<PublicEntry>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum RelayServiceMode {
    #[default]
    Forward,
    DiscoveryOnly,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    Direct,
    Relay,
}

pub fn alpns(config: &NetworkConfig) -> Vec<&'static [u8]> {
    if config.relay_mode == RelayServiceMode::DiscoveryOnly {
        vec![DISCOVERY]
    } else {
        ALPNS
            .into_iter()
            .chain(std::iter::once(DISCOVERY))
            .collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicEntry {
    /// Web origin browsers open; this Node's signed address names it as its Gateway.
    pub url: String,
    /// The entry's relay; the same origin when a reverse proxy routes `/relay` to it.
    pub relay: String,
}

/// File holding the operator token of a public entry's relay, on the entry and on its Nodes.
pub const ENTRY_TOKEN_FILE: &str = "entry-token";

pub fn read_token_file(path: &std::path::Path) -> Result<String> {
    let token =
        std::fs::read_to_string(path).context("the public entry token file is unreadable")?;
    let token = token.trim();
    ensure!(
        token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()),
        "the public entry token file is invalid"
    );
    Ok(token.to_owned())
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
        if self.relay_mode == RelayServiceMode::DiscoveryOnly {
            ensure!(
                self.relay_listen.is_some_and(|listen| listen.port() != 0)
                    && self.relay_url.is_none()
                    && self.entry.is_none()
                    && self.relays.is_empty()
                    && !self.n0_relays
                    && self.bind.is_none()
                    && self.discovery.is_empty(),
                "discovery-only needs relay_listen and cannot configure data relay paths or another bind address"
            );
        }
        for url in self.relays.iter().chain(&self.relay_url) {
            relay_url(url)?;
        }
        if let Some(entry) = &self.entry {
            relay_url(&entry.relay)?;
            let url = reqwest::Url::parse(&entry.url).context("invalid public entry URL")?;
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.path() == "/"
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "the public entry URL must be an HTTP(S) origin"
            );
        }
        ensure!(
            self.relay_url.is_none() || self.relay_listen.is_some(),
            "relay_url requires relay_listen"
        );
        if let Some(listen) = self
            .relay_listen
            .filter(|_| self.relay_mode == RelayServiceMode::Forward)
        {
            ensure!(
                self.relay_url.is_some() || !listen.ip().is_unspecified(),
                "a relay on all interfaces needs relay_url"
            );
        }
        Ok(())
    }
    /// The URL other Nodes use for this Node's relay.
    pub fn own_relay(&self) -> Result<Option<RelayUrl>> {
        if self.relay_mode == RelayServiceMode::DiscoveryOnly {
            return Ok(None);
        }
        match (&self.relay_url, self.relay_listen) {
            (Some(url), _) => relay_url(url).map(Some),
            (None, Some(listen)) => relay_url(&format!("http://{listen}")).map(Some),
            (None, None) => Ok(None),
        }
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
    relays: Mutex<HashSet<RelayUrl>>,
    connections: Mutex<HashMap<(EndpointId, &'static [u8]), Connection>>,
}
impl std::fmt::Debug for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Net")
    }
}

impl Net {
    /// Binds the endpoint. Addresses stay in tickets and the personal network unless the
    /// operator explicitly configures a discovery service.
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
        for url in &config.relays {
            urls.push((relay_url(url)?, None));
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
        for (url, token) in urls {
            known.insert(url.clone());
            map.insert(url.clone(), Arc::new(relay_config(url, token)));
        }
        let lookup = MemoryLookup::new();
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from_bytes(&seed))
            .alpns(
                alpns(config)
                    .into_iter()
                    .map(|alpn| alpn.to_vec())
                    .collect(),
            )
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
        let bind = if config.relay_mode == RelayServiceMode::DiscoveryOnly {
            config.relay_listen
        } else {
            config.bind
        };
        if bind.is_some() {
            builder = builder.portmapper_config(PortmapperConfig::Disabled);
        }
        let invalid = |error| anyhow!("invalid QUIC bind address: {error}");
        builder = match bind {
            Some(bind)
                if config.relay_mode == RelayServiceMode::DiscoveryOnly
                    && bind.ip().is_unspecified() =>
            {
                builder
                    .bind_addr(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), bind.port()))
                    .map_err(invalid)?
                    .bind_addr_with_opts(
                        SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), bind.port()),
                        BindOpts::default().set_is_required(false),
                    )
                    .map_err(invalid)?
            }
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
        let discovery = discovery::Discovery::new(
            &endpoint,
            &config.discovery,
            config.relay_mode == RelayServiceMode::DiscoveryOnly,
        )?;
        Ok(Self {
            endpoint,
            lookup,
            discovery,
            lan: OnceLock::new(),
            relays: Mutex::new(known),
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
            if self.relays.lock().expect("relay set").insert(url.clone()) {
                self.endpoint
                    .insert_relay(url.clone(), Arc::new(relay_config(url.clone(), None)))
                    .await;
            }
        }
    }

    /// Advertises this Node on the LAN and finds the other member Nodes there; `wake` hears
    /// which one appeared. Without mDNS, dialing keeps the other addresses.
    pub fn start_lan(&self, wake: impl Fn(&str) + Send + Sync + 'static) {
        let Some(port) = self.endpoint.bound_sockets().first().map(SocketAddr::port) else {
            return;
        };
        match lan::Lan::start(device_id(&self.endpoint.id()), port, Box::new(wake)) {
            Ok(lan) => {
                let _ = self.lan.set(lan);
            }
            Err(error) => tracing::warn!(%error, "LAN discovery is off"),
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
            let hint = self.discovery.resolve(id).await;
            let mut addresses = self
                .lookup
                .get_endpoint_info(id)
                .map(|info| info.addrs().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            if let Some(address) = hint {
                addresses.push(iroh::TransportAddr::Ip(address));
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
    while let Ok((mut send, mut recv)) = connection.accept_bi().await {
        let handle = handle.clone();
        tokio::spawn(async move {
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
        });
    }
}

/// The relay this Node runs for its personal network. It admits current member Nodes and
/// joiners whose token matches an open invite registered here.
pub async fn spawn_relay(
    listen: SocketAddr,
    access: Arc<dyn iroh_relay::server::DynAccessControl>,
) -> Result<iroh_relay::server::Server> {
    let mut relay = iroh_relay::server::RelayConfig::new(listen);
    relay.access = access;
    let mut config = iroh_relay::server::ServerConfig::default();
    config.relay = Some(relay);
    iroh_relay::server::Server::spawn(config)
        .await
        .map_err(|error| anyhow!("relay failed to start on {listen}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_settings_are_validated() {
        let mut config = NetworkConfig {
            relays: vec!["https://relay.example".into()],
            ..Default::default()
        };
        config.validate().unwrap();
        config.relays = vec!["https://user:secret@relay.example".into()];
        assert!(config.validate().is_err());
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
    }

    #[test]
    fn relay_tokens_depend_on_the_invite() {
        assert_ne!(relay_token(&"a".repeat(64)), relay_token(&"b".repeat(64)));
        assert_eq!(relay_token(&"a".repeat(64)).len(), 64);
    }
}
