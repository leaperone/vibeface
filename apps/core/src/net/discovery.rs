use super::{DIAL_TIMEOUT, DISCOVERY, endpoint_id};
use anyhow::{Context, Result, bail, ensure};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayUrl, TransportAddr, Watcher as _,
    dns::DnsResolver,
    endpoint::{Connection, RecvStream, SendStream},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    sync::{Notify, Semaphore, mpsc},
    task::{JoinHandle, JoinSet},
};

/// A registration and a lookup reply name at most `MAX_HOME_RELAYS` relay URLs.
const MAX_REQUEST: usize = 2048;
const MAX_REPLY: usize = 2048;
const MAX_SESSIONS: usize = 512;
const MAX_PER_IP: usize = 8;
/// Requests other than lookups per session; lookups are rate limited instead, so a dialer that
/// keeps resolving peers does not lose its registration.
const MAX_REQUESTS: usize = 60;
const LOOKUPS_PER_SECOND: usize = 20;
const LEASE: Duration = Duration::from_secs(60);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(3);
const NOTICE_INTERVAL: Duration = Duration::from_millis(250);
const MAX_HOME_RELAYS: usize = 4;
const MAX_RELAY_URL: usize = 256;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(1);
/// Registrations from every session share this, so names that never answer hold at most this many
/// lookups; a registration that finds none free drops the relay.
static RESOLVES: Semaphore = Semaphore::const_new(16);
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Server {
    pub(super) id: EndpointId,
    pub(super) host: String,
    pub(super) port: u16,
}

impl Server {
    pub(super) fn parse(value: &str) -> Result<Self> {
        let url = reqwest::Url::parse(value).context("invalid discovery server URL")?;
        ensure!(
            url.scheme() == "vibeface"
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && matches!(url.path(), "" | "/"),
            "discovery server must be vibeface://<device-id>@<host>:<UDP-port>"
        );
        let id = endpoint_id(url.username())?;
        let host = url
            .host_str()
            .context("discovery server needs a host")?
            .trim_matches(['[', ']'])
            .to_owned();
        let port = url
            .port()
            .filter(|port| *port != 0)
            .context("discovery server needs a UDP port")?;
        Ok(Self { id, host, port })
    }

    async fn connect(&self, endpoint: &Endpoint) -> Result<Connection> {
        let addresses = tokio::net::lookup_host((self.host.as_str(), self.port))
            .await?
            .take(8)
            .map(TransportAddr::Ip)
            .collect::<Vec<_>>();
        ensure!(!addresses.is_empty(), "discovery server has no IP address");
        tokio::time::timeout(
            DIAL_TIMEOUT,
            endpoint.connect(EndpointAddr::from_parts(self.id, addresses), DISCOVERY),
        )
        .await
        .context("discovery server timed out")?
        .map_err(Into::into)
    }
}

pub(super) fn validate_server(value: &str) -> Result<()> {
    Server::parse(value).map(|_| ())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    /// The relays the connecting device is reachable through; the QUIC handshake proved the
    /// device, and a wrong relay only makes it unreachable.
    Register {
        relays: Vec<RelayUrl>,
    },
    Lookup {
        device_id: EndpointId,
    },
    Probe {},
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Reply {
    /// `relay` is the server's own open relay, which the registrant may make its home relay.
    Registered {
        address: SocketAddr,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relay: Option<RelayUrl>,
    },
    Peer {
        device_id: EndpointId,
        address: SocketAddr,
        relays: Vec<RelayUrl>,
    },
    Notice {
        device_id: EndpointId,
        address: SocketAddr,
    },
    Missing {},
    Probed {},
    Refused {
        reason: String,
    },
}

struct Registration {
    generation: usize,
    address: SocketAddr,
    relays: Vec<RelayUrl>,
    notices: mpsc::Sender<(EndpointId, SocketAddr)>,
    last_notice: Option<Instant>,
}

#[derive(Default)]
struct Registry {
    entries: HashMap<EndpointId, Vec<Registration>>,
    by_ip: HashMap<IpAddr, usize>,
}

struct SessionGuard {
    registry: Arc<Mutex<Registry>>,
    ip: IpAddr,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let mut registry = self.registry.lock().expect("discovery registry");
        if let Some(count) = registry.by_ip.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 {
                registry.by_ip.remove(&self.ip);
            }
        }
    }
}

struct RegistrationGuard {
    registry: Arc<Mutex<Registry>>,
    device_id: EndpointId,
    generation: usize,
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        let mut registry = self.registry.lock().expect("discovery registry");
        if let Some(entries) = registry.entries.get_mut(&self.device_id) {
            entries.retain(|entry| entry.generation != self.generation);
            if entries.is_empty() {
                registry.entries.remove(&self.device_id);
            }
        }
    }
}

struct Client {
    connection: Mutex<Option<Connection>>,
    ready: Notify,
}

impl Client {
    /// The registration connection, waiting up to `wait` for the next one to come up.
    async fn connection_or_wait(&self, wait: Duration) -> Option<Connection> {
        let ready = self.ready.notified();
        if let Some(connection) = self
            .connection
            .lock()
            .expect("discovery connection")
            .clone()
        {
            return Some(connection);
        }
        let _ = tokio::time::timeout(wait, ready).await;
        self.connection
            .lock()
            .expect("discovery connection")
            .clone()
    }
}

pub(super) struct Discovery {
    server: bool,
    /// The open relay a serving endpoint offers its registrants.
    relay: Option<RelayUrl>,
    registry: Arc<Mutex<Registry>>,
    dns: DnsResolver,
    sessions: Semaphore,
    clients: Vec<(Server, Arc<Client>)>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Discovery {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Discovery {
    /// `known` is the endpoint's relay map, so a relay a server offers never replaces a
    /// configured one and its credential.
    pub fn new(
        endpoint: &Endpoint,
        servers: &[String],
        server: bool,
        relay: Option<RelayUrl>,
        own_relay: Option<RelayUrl>,
        known: Arc<tokio::sync::Mutex<HashSet<RelayUrl>>>,
    ) -> Result<Self> {
        let mut clients = Vec::new();
        let mut tasks = Vec::new();
        let probes = Arc::new(Semaphore::new(8));
        let observations = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        for value in servers {
            let server = Server::parse(value)?;
            let client = Arc::new(Client {
                connection: Mutex::new(None),
                ready: Notify::new(),
            });
            clients.push((server.clone(), client.clone()));
            let endpoint = endpoint.clone();
            let probes = probes.clone();
            let observations = observations.clone();
            let own_relay = own_relay.clone();
            let known = known.clone();
            tasks.push(tokio::spawn(async move {
                let mut retry = RETRY_MIN;
                while !endpoint.is_closed() {
                    let result = register(
                        &endpoint,
                        &server,
                        &client,
                        &probes,
                        &observations,
                        own_relay.as_ref(),
                        &known,
                    )
                    .await;
                    if client
                        .connection
                        .lock()
                        .expect("discovery connection")
                        .take()
                        .is_some()
                    {
                        retry = RETRY_MIN;
                    }
                    // A session that ended normally (lease, request budget, new home relay)
                    // reconnects at once: until then lookups and relay joiners find nothing.
                    // Failures back off with jitter, so an unreachable server is not hammered
                    // by every device in step.
                    if let Err(error) = result {
                        tracing::debug!(%error, "discovery registration ended");
                        let jitter = rand::random::<f64>() * 0.5 + 0.5;
                        tokio::time::sleep(retry.mul_f64(jitter)).await;
                        retry = (retry * 2).min(RETRY_MAX);
                    }
                }
            }));
        }
        Ok(Self {
            server,
            relay,
            registry: Default::default(),
            dns: endpoint.dns_resolver()?.clone(),
            sessions: Semaphore::new(MAX_SESSIONS),
            clients,
            tasks,
        })
    }

    /// Where `device_id` is registered: here when this endpoint serves Discovery, else with the
    /// servers this endpoint registers with, waiting up to `wait` for its sessions.
    pub async fn resolve(&self, device_id: EndpointId, wait: Duration) -> Option<Located> {
        if self.server {
            let registry = self.registry.lock().expect("discovery registry");
            if let Some(entry) = registry
                .entries
                .get(&device_id)
                .and_then(|entries| entries.last())
            {
                return Some(Located {
                    address: entry.address,
                    relays: entry.relays.clone(),
                });
            }
        }
        let queries = self.clients.iter().map(|(_, client)| async move {
            let connection = client.connection_or_wait(wait).await?;
            tokio::time::timeout(CONTROL_TIMEOUT, async {
                let (mut send, mut recv) = connection.open_bi().await?;
                write(&mut send, &Request::Lookup { device_id }, MAX_REQUEST).await?;
                send.finish()?;
                match read::<Reply>(&mut recv, MAX_REPLY).await? {
                    Reply::Peer {
                        device_id: found,
                        address,
                        relays,
                    } if found == device_id => {
                        validate_address(address)?;
                        let count = relays.len();
                        let relays = usable_relays(relays, local(&connection));
                        if relays.len() < count {
                            tracing::debug!(%device_id, dropped = count - relays.len(), "dropped relays from a discovery record");
                        }
                        Ok::<_, anyhow::Error>(Some(Located { address, relays }))
                    }
                    Reply::Missing {} => Ok(None),
                    _ => bail!("invalid discovery lookup reply"),
                }
            })
            .await
            .ok()?
            .ok()
            .flatten()
        });
        // The first answer wins, so a server that is down costs no more than its own wait.
        let mut queries = queries.collect::<futures_util::stream::FuturesUnordered<_>>();
        while let Some(found) = futures_util::StreamExt::next(&mut queries).await {
            if found.is_some() {
                return found;
            }
        }
        None
    }

    pub async fn serve(&self, connection: Connection) -> Result<()> {
        let result = tokio::time::timeout(LEASE, async {
            let _slot = self.sessions.try_acquire().context("discovery is full")?;
            let address = observed(&connection)?;
            {
                let mut registry = self.registry.lock().expect("discovery registry");
                let count = registry.by_ip.entry(address.ip()).or_default();
                ensure!(
                    *count < MAX_PER_IP,
                    "too many discovery sessions from this address"
                );
                *count += 1;
            }
            let _source = SessionGuard {
                registry: self.registry.clone(),
                ip: address.ip(),
            };
            let mut work = JoinSet::new();
            let mut registered = false;
            let mut requests = 0;
            let mut lookups = (Instant::now(), 0);
            async {
                loop {
                    let streams = tokio::select! {
                        _ = connection.closed() => break,
                        streams = connection.accept_bi() => streams?,
                    };
                    let (mut send, mut recv) = streams;
                    let request: Request =
                        tokio::time::timeout(CONTROL_TIMEOUT, read(&mut recv, MAX_REQUEST))
                            .await
                            .context("discovery request timed out")??;
                    let counted = !matches!(request, Request::Lookup { .. });
                    if !counted {
                        if lookups.0.elapsed() >= Duration::from_secs(1) {
                            lookups = (Instant::now(), 0);
                        }
                        lookups.1 += 1;
                    }
                    match request {
                        Request::Probe {} => {
                            write(&mut send, &Reply::Probed {}, MAX_REPLY).await?;
                            send.finish()?;
                        }
                        Request::Register { relays } if self.server && !registered => {
                            let count = relays.len();
                            let relays = registered_relays(
                                &self.dns,
                                relays,
                                !crate::fetch::public(address.ip()),
                            )
                            .await;
                            if relays.len() < count {
                                tracing::debug!(
                                    dropped = count - relays.len(),
                                    "dropped relays from a discovery registration"
                                );
                            }
                            registered = true;
                            let (notices, mut incoming) = mpsc::channel(8);
                            let device_id = connection.remote_id();
                            let generation = connection.stable_id();
                            self.registry
                                .lock()
                                .expect("discovery registry")
                                .entries
                                .entry(device_id)
                                .or_default()
                                .push(Registration {
                                    generation,
                                    address,
                                    relays,
                                    notices,
                                    last_notice: None,
                                });
                            let guard = RegistrationGuard {
                                registry: self.registry.clone(),
                                device_id,
                                generation,
                            };
                            let registered = Reply::Registered {
                                address,
                                relay: self.relay.clone(),
                            };
                            work.spawn(async move {
                                let _guard = guard;
                                write(&mut send, &registered, MAX_REPLY).await?;
                                while let Some((device_id, address)) = incoming.recv().await {
                                    write(
                                        &mut send,
                                        &Reply::Notice { device_id, address },
                                        MAX_REPLY,
                                    )
                                    .await?;
                                }
                                Ok::<_, anyhow::Error>(())
                            });
                        }
                        Request::Lookup { .. } if self.server && lookups.1 > LOOKUPS_PER_SECOND => {
                            let reply = Reply::Refused {
                                reason: "too many discovery lookups".into(),
                            };
                            write(&mut send, &reply, MAX_REPLY).await?;
                            send.finish()?;
                        }
                        Request::Lookup { device_id } if self.server => {
                            let found = {
                                let mut registry =
                                    self.registry.lock().expect("discovery registry");
                                registry
                                    .entries
                                    .get_mut(&device_id)
                                    .and_then(|entries| entries.last_mut())
                                    .map(|entry| {
                                        if entry
                                            .last_notice
                                            .is_none_or(|last| last.elapsed() >= NOTICE_INTERVAL)
                                            && entry
                                                .notices
                                                .try_send((connection.remote_id(), address))
                                                .is_ok()
                                        {
                                            entry.last_notice = Some(Instant::now());
                                        }
                                        (entry.address, entry.relays.clone())
                                    })
                            };
                            let reply = match found {
                                Some((address, relays)) => Reply::Peer {
                                    device_id,
                                    address,
                                    relays,
                                },
                                None => Reply::Missing {},
                            };
                            write(&mut send, &reply, MAX_REPLY).await?;
                            send.finish()?;
                        }
                        _ => bail!("this endpoint only accepts discovery probes"),
                    }
                    while work.try_join_next().is_some() {}
                    if counted {
                        requests += 1;
                        if requests == MAX_REQUESTS {
                            break;
                        }
                    }
                }
                Ok(())
            }
            .await
        })
        .await
        .context("discovery lease expired");
        connection.close(0u32.into(), b"discovery session ended");
        result?
    }
}

/// Where Discovery last saw a device: its observed UDP address and the relays it registered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Located {
    pub address: SocketAddr,
    pub relays: Vec<RelayUrl>,
}

/// Dialers use registered relays as given, so a record keeps only relays off this host and
/// private networks, unless the registering device or the queried server is itself on one.
/// Dropping a relay leaves the rest of the record usable.
fn usable_relays(relays: Vec<RelayUrl>, local: bool) -> Vec<RelayUrl> {
    relays
        .into_iter()
        .filter(|url| {
            url.as_str().len() <= MAX_RELAY_URL
                && super::relay_url(url.as_str()).is_ok()
                && (local || public_host(url))
        })
        .take(MAX_HOME_RELAYS)
        .collect()
}

fn public_host(url: &RelayUrl) -> bool {
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    match host.parse::<IpAddr>() {
        Ok(ip) => crate::fetch::public(ip),
        Err(_) => !host.is_empty() && host != "localhost" && !host.ends_with(".localhost"),
    }
}

/// The relays a registration may publish: names must also resolve only to public addresses.
async fn registered_relays(dns: &DnsResolver, relays: Vec<RelayUrl>, local: bool) -> Vec<RelayUrl> {
    let relays = usable_relays(relays, local);
    if local {
        return relays;
    }
    let public =
        futures_util::future::join_all(relays.iter().map(|url| resolves_public(dns, url))).await;
    relays
        .into_iter()
        .zip(public)
        .filter_map(|(url, public)| public.then_some(url))
        .collect()
}

/// A name that fails to resolve in time is dropped: a dialer cannot use it either.
/// The lookup is async and cancelled on drop, unlike `getaddrinfo` on the blocking pool.
async fn resolves_public(dns: &DnsResolver, url: &RelayUrl) -> bool {
    if !public_host(url) {
        return false;
    }
    let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    let Ok(_permit) = RESOLVES.try_acquire() else {
        return false;
    };
    let Ok(addresses) = dns.lookup_ipv4_ipv6(host, RESOLVE_TIMEOUT).await else {
        return false;
    };
    let mut any = false;
    for address in addresses {
        if !crate::fetch::public(address) {
            return false;
        }
        any = true;
    }
    any
}

/// Whether the peer of `connection` is on this host or a private network.
fn local(connection: &Connection) -> bool {
    observed(connection).is_ok_and(|address| !crate::fetch::public(address.ip()))
}

fn validate_address(address: SocketAddr) -> Result<()> {
    ensure!(
        address.port() != 0 && !address.ip().is_unspecified() && !address.ip().is_multicast(),
        "invalid observed discovery address"
    );
    Ok(())
}

fn observed(connection: &Connection) -> Result<SocketAddr> {
    let address = connection
        .paths()
        .iter()
        .filter(|path| path.is_selected())
        .find_map(|path| match path.remote_addr() {
            TransportAddr::Ip(address) => Some(*address),
            _ => None,
        })
        .context("discovery requires a direct UDP connection")?;
    validate_address(address)?;
    Ok(address)
}

async fn register(
    endpoint: &Endpoint,
    server: &Server,
    client: &Client,
    probes: &Arc<Semaphore>,
    observations: &tokio::sync::Mutex<HashMap<SocketAddr, usize>>,
    own_relay: Option<&RelayUrl>,
    known: &tokio::sync::Mutex<HashSet<RelayUrl>>,
) -> Result<()> {
    let mut addr = endpoint.watch_addr();
    let connection = server.connect(endpoint).await?;
    // A public server keeps only public relays, so non-public ones never reach it.
    let local = local(&connection);
    let candidates = |addr: &EndpointAddr| {
        let mut relays = addr.relay_urls().cloned().collect::<Vec<_>>();
        if let Some(own) = own_relay
            && !relays.contains(own)
        {
            relays.push(own.clone());
        }
        relays
    };
    let home = |addr: &EndpointAddr| usable_relays(candidates(addr), local);
    let all = candidates(&addr.get());
    let relays = usable_relays(all.clone(), local);
    if relays.len() < all.len() {
        tracing::debug!(
            dropped = all.len() - relays.len(),
            "relays left out of the discovery registration"
        );
    }
    let (mut send, mut recv) = connection.open_bi().await?;
    write(
        &mut send,
        &Request::Register {
            relays: relays.clone(),
        },
        MAX_REQUEST,
    )
    .await?;
    send.finish()?;
    let Reply::Registered { address, relay } =
        tokio::time::timeout(CONTROL_TIMEOUT, read(&mut recv, MAX_REPLY)).await??
    else {
        bail!("discovery server did not register this endpoint");
    };
    validate_address(address)?;
    // The server's open relay becomes a candidate home relay, so devices behind NATs that
    // defeat direct paths still reach each other. Once it is home, this session ends and the
    // next one registers it.
    for url in usable_relays(relay.into_iter().collect(), local) {
        if known.lock().await.insert(url.clone()) {
            endpoint
                .insert_relay(url.clone(), Arc::new(super::relay_config(url, None)))
                .await;
        }
    }
    {
        let mut observations = observations.lock().await;
        let count = observations.entry(address).or_default();
        if *count == 0 {
            endpoint.add_external_addr(address).await;
        }
        *count += 1;
    }
    *client.connection.lock().expect("discovery connection") = Some(connection.clone());
    client.ready.notify_waiters();
    let mut work = JoinSet::new();
    // One stream of frames across loop turns, so an address change never cancels a half-read
    // frame.
    let replies = futures_util::stream::unfold(recv, |mut recv| async move {
        let reply = read::<Reply>(&mut recv, MAX_REPLY).await;
        Some((reply, recv))
    });
    let mut replies = std::pin::pin!(replies);
    let result = async {
        loop {
            let reply: Reply = tokio::select! {
                _ = connection.closed() => break,
                // A new home relay ends this session, so the next one registers it.
                changed = addr.updated() => match changed {
                    Ok(changed) if home(&changed) != relays => break,
                    Ok(_) => continue,
                    Err(_) => break,
                },
                // The server closing the session fails the read as well, and that is a normal end.
                reply = futures_util::StreamExt::next(&mut replies) => {
                    match reply.context("discovery notifications ended")? {
                        Ok(reply) => reply,
                        Err(_) if connection.close_reason().is_some() => break,
                        Err(error) => return Err(error),
                    }
                }
            };
            let Reply::Notice { device_id, address } = reply else {
                bail!("invalid discovery notification");
            };
            validate_address(address)?;
            if device_id == endpoint.id() {
                continue;
            }
            if let Ok(permit) = probes.clone().try_acquire_owned() {
                let endpoint = endpoint.clone();
                work.spawn(async move {
                    let _permit = permit;
                    let _ = tokio::time::timeout(CONTROL_TIMEOUT, async {
                        let peer = endpoint
                            .connect(
                                EndpointAddr::from_parts(device_id, [TransportAddr::Ip(address)]),
                                DISCOVERY,
                            )
                            .await?;
                        let (mut send, mut recv) = peer.open_bi().await?;
                        write(&mut send, &Request::Probe {}, MAX_REQUEST).await?;
                        send.finish()?;
                        ensure!(
                            matches!(read::<Reply>(&mut recv, MAX_REPLY).await?, Reply::Probed {}),
                            "invalid discovery probe reply"
                        );
                        peer.close(0u32.into(), b"discovery probe finished");
                        Ok::<_, anyhow::Error>(())
                    })
                    .await;
                });
            }
            while work.try_join_next().is_some() {}
        }
        Ok(())
    }
    .await;
    {
        let mut observations = observations.lock().await;
        if let Some(count) = observations.get_mut(&address) {
            *count -= 1;
            if *count == 0 {
                endpoint.remove_external_addr(&address).await;
                observations.remove(&address);
            }
        }
    }
    connection.close(0u32.into(), b"discovery registration ended");
    result
}

async fn read<T: DeserializeOwned>(recv: &mut RecvStream, limit: usize) -> Result<T> {
    let mut size = [0; 4];
    recv.read_exact(&mut size).await?;
    let size = u32::from_be_bytes(size) as usize;
    ensure!(size <= limit, "discovery frame is too large");
    let mut bytes = vec![0; size];
    recv.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn write<T: Serialize>(send: &mut SendStream, value: &T, limit: usize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= limit, "discovery frame is too large");
    send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

#[derive(Debug)]
pub(super) struct Handler(pub std::sync::Weak<super::Net>);
impl iroh::protocol::ProtocolHandler for Handler {
    async fn accept(
        &self,
        connection: Connection,
    ) -> std::result::Result<(), iroh::protocol::AcceptError> {
        if let Some(net) = self.0.upgrade() {
            net.serve_discovery(connection).await.map_err(|error| {
                iroh::protocol::AcceptError::from_err(std::io::Error::other(error.to_string()))
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crypto::LocalIdentity,
        net::{Net, NetworkConfig, PathKind, SYNC},
    };
    use iroh::protocol::{AcceptError, ProtocolHandler, Router};
    use serde_json::json;

    #[derive(Debug)]
    struct Echo;
    impl ProtocolHandler for Echo {
        async fn accept(&self, connection: Connection) -> std::result::Result<(), AcceptError> {
            crate::net::serve_streams(connection, |value| async move { Ok(json!({"echo":value})) })
                .await;
            Ok(())
        }
    }

    fn router(net: &Arc<Net>) -> Router {
        Router::builder(net.endpoint.clone())
            .accept(DISCOVERY, Handler(Arc::downgrade(net)))
            .accept(SYNC, Echo)
            .spawn()
    }

    async fn net(identity: &LocalIdentity, config: &NetworkConfig) -> Result<Arc<Net>> {
        Ok(Arc::new(
            Net::bind(&identity.secret_key_hex, config, 0, vec![])
                .await
                .with_context(|| {
                    format!(
                        "test endpoint bind {:?}, serving {}",
                        config.bind, config.serve_discovery
                    )
                })?,
        ))
    }

    async fn registered(service: &Net, count: usize) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if service
                    .discovery
                    .registry
                    .lock()
                    .expect("registry")
                    .entries
                    .len()
                    == count
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("discovery registrations did not settle")
    }

    #[test]
    fn discovery_urls_and_messages_reject_arbitrary_payloads() -> Result<()> {
        let id = LocalIdentity::generate().device_id()?;
        let ipv6 = Server::parse(&format!("vibeface://{id}@[::1]:4790"))?;
        assert_eq!(ipv6.host, "::1");
        assert_eq!(ipv6.port, 4790);
        for value in [
            format!("http://{id}@server:4790"),
            format!("vibeface://{id}:secret@server:4790"),
            format!("vibeface://{id}@server"),
            format!("vibeface://{id}@server:0"),
            format!("vibeface://{id}@server:4790/path"),
            format!("vibeface://{id}@server:4790?q=1"),
            format!("vibeface://{id}@server:4790#fragment"),
            "vibeface://invalid@server:4790".into(),
        ] {
            assert!(Server::parse(&value).is_err(), "{value}");
        }
        assert!(
            serde_json::from_value::<Request>(json!({"type":"probe","payload":"business"}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<Request>(
                json!({"type":"register","relays":[],"address":"127.0.0.1:1234"})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<Request>(json!({"type":"register"})).is_err());
        let relays = |count: usize| -> Vec<RelayUrl> {
            (0..count)
                .map(|i| {
                    format!("https://relay{i}.example/")
                        .parse()
                        .expect("relay URL")
                })
                .collect()
        };
        let parsed = |urls: &[&str]| -> Vec<RelayUrl> {
            urls.iter()
                .map(|url| url.parse().expect("relay URL"))
                .collect()
        };
        assert_eq!(
            usable_relays(relays(MAX_HOME_RELAYS + 1), false),
            relays(MAX_HOME_RELAYS)
        );
        let long: RelayUrl =
            format!("https://relay.example/{}", "a".repeat(MAX_RELAY_URL)).parse()?;
        // A bad relay is dropped, and the rest of the record stays.
        let mut mixed = parsed(&["ftp://relay.example/", "http://127.0.0.1:3340/"]);
        mixed.push(long);
        mixed.extend(relays(1));
        assert_eq!(usable_relays(mixed.clone(), false), relays(1));
        assert_eq!(
            usable_relays(mixed, true),
            [parsed(&["http://127.0.0.1:3340/"]), relays(1)].concat()
        );
        let local = parsed(&[
            "http://127.0.0.1:3340/",
            "http://10.0.0.2/",
            "http://192.168.1.5:3340/",
            "http://169.254.0.1/",
            "http://[::1]:3340/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            "http://localhost:3340/",
            "http://relay.localhost/",
        ]);
        assert!(usable_relays(local.clone(), false).is_empty());
        assert_eq!(usable_relays(local.clone(), true), local[..MAX_HOME_RELAYS]);
        // Four relay URLs at the length limit still fit one registration frame.
        let longest: RelayUrl = format!(
            "https://relay.example/{}",
            "a".repeat(MAX_RELAY_URL - "https://relay.example/".len())
        )
        .parse()?;
        assert_eq!(longest.as_str().len(), MAX_RELAY_URL);
        let register = Request::Register {
            relays: vec![longest; MAX_HOME_RELAYS],
        };
        assert!(serde_json::to_vec(&register)?.len() <= MAX_REQUEST);
        Ok(())
    }

    #[tokio::test]
    async fn registration_drops_relays_that_resolve_to_private_addresses() {
        let parsed = |urls: &[&str]| -> Vec<RelayUrl> {
            urls.iter()
                .map(|url| url.parse().expect("relay URL"))
                .collect()
        };
        // IP literals and `localhost` resolve without the network.
        let dns = DnsResolver::new();
        assert!(resolves_public(&dns, &"https://1.1.1.1/".parse().expect("relay URL")).await);
        for url in parsed(&[
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://localhost:3340/",
        ]) {
            assert!(!resolves_public(&dns, &url).await, "{url}");
        }
        let urls = parsed(&[
            "https://1.1.1.1/",
            "http://10.0.0.2/",
            "https://[2606:4700::1111]/",
        ]);
        assert_eq!(
            registered_relays(&dns, urls.clone(), false).await,
            [urls[0].clone(), urls[2].clone()]
        );
        assert_eq!(registered_relays(&dns, urls.clone(), true).await, urls);
    }

    #[test]
    fn old_registration_cleanup_preserves_a_replacement() -> Result<()> {
        let device_id = endpoint_id(&LocalIdentity::generate().device_id()?)?;
        let registry = Arc::new(Mutex::new(Registry::default()));
        let (notices, _) = mpsc::channel(1);
        registry.lock().expect("registry").entries.insert(
            device_id,
            vec![Registration {
                generation: 2,
                address: "127.0.0.1:1234".parse()?,
                relays: vec![],
                notices,
                last_notice: None,
            }],
        );
        drop(RegistrationGuard {
            registry: registry.clone(),
            device_id,
            generation: 1,
        });
        assert_eq!(
            registry.lock().expect("registry").entries[&device_id][0].generation,
            2
        );
        drop(RegistrationGuard {
            registry: registry.clone(),
            device_id,
            generation: 2,
        });
        assert!(registry.lock().expect("registry").entries.is_empty());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn discovery_connects_unknown_addresses_and_rejects_business() -> Result<()> {
        let identity = LocalIdentity::generate();
        let mut config = NetworkConfig {
            serve_discovery: true,
            bind: Some("127.0.0.1:0".parse()?),
            ..Default::default()
        };
        let service = net(&identity, &config).await?;
        let service_router = service.discovery_router();
        let service_address = *service
            .addr()
            .ip_addrs()
            .next()
            .context("service IP missing")?;
        config.bind = Some(service_address);
        let discovery = format!("vibeface://{}@{service_address}", service.endpoint.id());
        let client_config = NetworkConfig {
            bind: Some("127.0.0.1:0".parse()?),
            discovery: vec![discovery],
            ..Default::default()
        };
        let a = net(&LocalIdentity::generate(), &client_config).await?;
        let b = net(&LocalIdentity::generate(), &client_config).await?;
        let a_router = router(&a);
        let b_router = router(&b);
        registered(&service, 2).await?;
        let peer = b.endpoint.id().to_string();
        assert!(a.lookup.get_endpoint_info(b.endpoint.id()).is_none());
        let value = a
            .request(&peer, SYNC, &json!({"message":"direct"}), CONTROL_TIMEOUT)
            .await?;
        assert_eq!(value, json!({"echo":{"message":"direct"}}));
        assert_eq!(a.path_kind(&peer), Some(PathKind::Direct));
        assert!(
            tokio::time::timeout(CONTROL_TIMEOUT, a.endpoint.connect(service.addr(), SYNC))
                .await?
                .is_err()
        );
        let raw = a.endpoint.connect(service.addr(), DISCOVERY).await?;
        let (mut send, _recv) = raw.open_bi().await?;
        send.write_all(&((MAX_REQUEST + 1) as u32).to_be_bytes())
            .await?;
        send.finish()?;
        tokio::time::timeout(CONTROL_TIMEOUT, raw.closed()).await?;
        drop((send, _recv, raw));
        service_router.shutdown().await?;
        drop(service_router);
        drop(service);
        // Closing QUIC does not await pending stream destructors that still own its UDP socket.
        let restarted = tokio::time::timeout(CONTROL_TIMEOUT, async {
            loop {
                match net(&identity, &config).await {
                    Err(error)
                        if error.chain().any(|cause| {
                            cause
                                .downcast_ref::<std::io::Error>()
                                .is_some_and(|error| error.kind() == std::io::ErrorKind::AddrInUse)
                        }) =>
                    {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                    result => return result,
                }
            }
        })
        .await
        .context("closed discovery UDP socket was not released before restart")??;
        let restarted_router = restarted.discovery_router();
        registered(&restarted, 2).await?;
        assert_eq!(
            a.request(&peer, SYNC, &json!({"after":"restart"}), CONTROL_TIMEOUT)
                .await?,
            json!({"echo":{"after":"restart"}})
        );
        a_router.shutdown().await?;
        b_router.shutdown().await?;
        restarted_router.shutdown().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn discovery_does_not_bridge_blocked_direct_paths_and_relays_still_work() -> Result<()> {
        let service = net(
            &LocalIdentity::generate(),
            &NetworkConfig {
                serve_discovery: true,
                ..Default::default()
            },
        )
        .await?;
        let service_router = service.discovery_router();
        let ipv4_port = service
            .endpoint
            .bound_sockets()
            .into_iter()
            .find(|address| address.is_ipv4())
            .context("service IPv4 missing")?
            .port();
        let ipv6_port = service
            .endpoint
            .bound_sockets()
            .into_iter()
            .find(|address| address.is_ipv6())
            .context("service IPv6 missing")?
            .port();
        let discovery = vec![
            format!("vibeface://{}@127.0.0.1:{ipv4_port}", service.endpoint.id()),
            format!("vibeface://{}@[::1]:{ipv6_port}", service.endpoint.id()),
        ];
        let a_id = LocalIdentity::generate();
        let b_id = LocalIdentity::generate();
        let a_config = NetworkConfig {
            bind: Some("127.0.0.1:0".parse()?),
            discovery: discovery.clone(),
            ..Default::default()
        };
        let mut b_config = NetworkConfig {
            bind: Some("[::1]:0".parse()?),
            discovery,
            ..Default::default()
        };
        let a = net(&a_id, &a_config).await?;
        let b = net(&b_id, &b_config).await?;
        let a_router = router(&a);
        let b_router = router(&b);
        registered(&service, 2).await?;
        assert!(
            a.request(
                &b_id.device_id()?,
                SYNC,
                &json!({"blocked":true}),
                CONTROL_TIMEOUT
            )
            .await
            .is_err()
        );
        a_router.shutdown().await?;
        b_router.shutdown().await?;
        let relay = crate::net::spawn_relay(
            "127.0.0.1:0".parse()?,
            Arc::new(iroh_relay::server::AllowAll),
            None,
            None,
        )
        .await?;
        let relay_url = format!("http://{}", relay.addr);
        // Only B uses the relay: A dials it as Discovery names it, outside its own relay map.
        b_config.relays = vec![crate::net::Relay {
            url: relay_url,
            token: None,
        }];
        let a = net(&a_id, &a_config).await?;
        let b = net(&b_id, &b_config).await?;
        let a_router = router(&a);
        let b_router = router(&b);
        registered(&service, 2).await?;
        let b_endpoint = b.endpoint.id();
        // B registers again once its home relay is up; A learns that relay only from Discovery.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let relays = service
                    .discovery
                    .registry
                    .lock()
                    .expect("registry")
                    .entries
                    .get(&b_endpoint)
                    .and_then(|entries| entries.last())
                    .map(|entry| entry.relays.clone());
                if relays.is_some_and(|relays| {
                    !relays.is_empty() && relays.iter().eq(b.addr().relay_urls())
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("B's home relay was not registered")?;
        assert!(a.lookup.get_endpoint_info(b_endpoint).is_none());
        let peer = b_id.device_id()?;
        assert_eq!(
            a.request(&peer, SYNC, &json!({"fallback":true}), CONTROL_TIMEOUT)
                .await?,
            json!({"echo":{"fallback":true}})
        );
        assert_eq!(a.path_kind(&peer), Some(PathKind::Relay));
        a_router.shutdown().await?;
        b_router.shutdown().await?;
        service_router.shutdown().await?;
        relay.shutdown().await;
        Ok(())
    }
}
