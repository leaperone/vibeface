use super::{
    DIAL_TIMEOUT, DISCOVERY,
    bootstrap::{self, Issuer, SignedCatalog},
    device_id, endpoint_id,
};
use anyhow::{Context, Result, bail, ensure};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, TransportAddr,
    endpoint::{Connection, RecvStream, SendStream},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    sync::{Notify, Semaphore, mpsc},
    task::{JoinHandle, JoinSet},
};

/// A request names at most a relay origin; a reply may carry the signed catalogue.
const MAX_REQUEST: usize = 1024;
const MAX_REPLY: usize = bootstrap::MAX_SIGNED_CATALOG_BYTES + 512;
const MAX_SESSIONS: usize = 512;
const MAX_PER_IP: usize = 8;
const MAX_REQUESTS: usize = 60;
const LEASE: Duration = Duration::from_secs(60);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(3);
const NOTICE_INTERVAL: Duration = Duration::from_millis(250);

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
    Register {},
    Lookup { device_id: EndpointId },
    Probe {},
    /// The signed service catalogue, when this service publishes one.
    Catalog {},
    /// A relay grant for the connecting device, which the QUIC handshake proved.
    Grant { relay_url: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Reply {
    Registered {
        address: SocketAddr,
    },
    Peer {
        device_id: EndpointId,
        address: SocketAddr,
    },
    Notice {
        device_id: EndpointId,
        address: SocketAddr,
    },
    Missing {},
    Probed {},
    Catalog {
        catalog: SignedCatalog,
    },
    Granted {
        grant: String,
    },
    Refused {
        reason: String,
    },
}

struct Registration {
    generation: usize,
    address: SocketAddr,
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

/// Catalogue and grant requests on the authenticated registration session with one server.
#[derive(Clone)]
pub(super) struct DiscoveryClient(Arc<Client>);

impl std::fmt::Debug for DiscoveryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DiscoveryClient")
    }
}

impl DiscoveryClient {
    async fn call(&self, request: &Request, wait: Duration) -> Result<Reply> {
        let connection = self
            .0
            .connection_or_wait(wait)
            .await
            .context("discovery server is not connected")?;
        tokio::time::timeout(CONTROL_TIMEOUT, async {
            let (mut send, mut recv) = connection.open_bi().await?;
            write(&mut send, request, MAX_REQUEST).await?;
            send.finish()?;
            read::<Reply>(&mut recv, MAX_REPLY).await
        })
        .await
        .context("discovery request timed out")?
    }

    pub async fn catalog(&self, wait: Duration) -> Result<SignedCatalog> {
        match self.call(&Request::Catalog {}, wait).await? {
            Reply::Catalog { catalog } => Ok(catalog),
            Reply::Refused { reason } => bail!("{reason}"),
            _ => bail!("invalid discovery catalogue reply"),
        }
    }

    pub async fn grant(&self, relay_url: &str, wait: Duration) -> Result<String> {
        let request = Request::Grant {
            relay_url: relay_url.to_owned(),
        };
        match self.call(&request, wait).await? {
            Reply::Granted { grant } => Ok(grant),
            Reply::Refused { reason } => bail!("{reason}"),
            _ => bail!("invalid discovery grant reply"),
        }
    }
}

pub(super) struct Discovery {
    server: bool,
    /// Signs the catalogue and grants when this service publishes a catalogue.
    issuer: Option<Arc<Issuer>>,
    registry: Arc<Mutex<Registry>>,
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
    pub fn new(
        endpoint: &Endpoint,
        servers: &[String],
        server: bool,
        issuer: Option<Arc<Issuer>>,
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
            tasks.push(tokio::spawn(async move {
                while !endpoint.is_closed() {
                    if let Err(error) =
                        register(&endpoint, &server, &client, &probes, &observations).await
                    {
                        tracing::debug!(%error, "discovery registration ended");
                    }
                    *client.connection.lock().expect("discovery connection") = None;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }));
        }
        Ok(Self {
            server,
            issuer,
            registry: Default::default(),
            sessions: Semaphore::new(MAX_SESSIONS),
            clients,
            tasks,
        })
    }

    /// The session with the server `value` names, if this endpoint registers there.
    pub fn client_for(&self, value: &str) -> Option<DiscoveryClient> {
        let wanted = Server::parse(value).ok()?;
        self.clients
            .iter()
            .find(|(server, _)| *server == wanted)
            .map(|(_, client)| DiscoveryClient(client.clone()))
    }

    pub async fn resolve(&self, device_id: EndpointId) -> Option<SocketAddr> {
        let queries = self.clients.iter().map(|(_, client)| async move {
            let connection = client.connection_or_wait(Duration::from_secs(1)).await?;
            tokio::time::timeout(CONTROL_TIMEOUT, async {
                let (mut send, mut recv) = connection.open_bi().await?;
                write(&mut send, &Request::Lookup { device_id }, MAX_REQUEST).await?;
                send.finish()?;
                match read::<Reply>(&mut recv, MAX_REPLY).await? {
                    Reply::Peer {
                        device_id: found,
                        address,
                    } if found == device_id => {
                        validate_address(address)?;
                        Ok::<_, anyhow::Error>(Some(address))
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
        futures_util::future::join_all(queries)
            .await
            .into_iter()
            .flatten()
            .next()
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
            async {
                for _ in 0..MAX_REQUESTS {
                    let streams = tokio::select! {
                        _ = connection.closed() => break,
                        streams = connection.accept_bi() => streams?,
                    };
                    let (mut send, mut recv) = streams;
                    let request: Request =
                        tokio::time::timeout(CONTROL_TIMEOUT, read(&mut recv, MAX_REQUEST))
                            .await
                            .context("discovery request timed out")??;
                    match request {
                        Request::Probe {} => {
                            write(&mut send, &Reply::Probed {}, MAX_REPLY).await?;
                            send.finish()?;
                        }
                        Request::Catalog {} if self.server => {
                            let reply = match &self.issuer {
                                Some(issuer) => match issuer.catalog(bootstrap::now_ms()) {
                                    Ok(catalog) => Reply::Catalog { catalog },
                                    Err(error) => Reply::Refused {
                                        reason: error.to_string(),
                                    },
                                },
                                None => Reply::Refused {
                                    reason: "this discovery service publishes no catalogue".into(),
                                },
                            };
                            write(&mut send, &reply, MAX_REPLY).await?;
                            send.finish()?;
                        }
                        Request::Grant { relay_url } if self.server => {
                            // The QUIC handshake proved the device; the source address is the
                            // observed one.
                            let device = device_id(&connection.remote_id());
                            let reply = match &self.issuer {
                                Some(issuer) => match issuer.grant(
                                    &device,
                                    &relay_url,
                                    address.ip(),
                                    bootstrap::now_ms(),
                                ) {
                                    Ok(grant) => Reply::Granted {
                                        grant: grant.as_str().to_owned(),
                                    },
                                    Err(error) => Reply::Refused {
                                        reason: error.to_string(),
                                    },
                                },
                                None => Reply::Refused {
                                    reason: "this discovery service issues no grants".into(),
                                },
                            };
                            write(&mut send, &reply, MAX_REPLY).await?;
                            send.finish()?;
                        }
                        Request::Register {} if self.server && !registered => {
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
                                    notices,
                                    last_notice: None,
                                });
                            let guard = RegistrationGuard {
                                registry: self.registry.clone(),
                                device_id,
                                generation,
                            };
                            work.spawn(async move {
                                let _guard = guard;
                                write(&mut send, &Reply::Registered { address }, MAX_REPLY).await?;
                                while let Some((device_id, address)) = incoming.recv().await {
                                    write(&mut send, &Reply::Notice { device_id, address }, MAX_REPLY)
                                        .await?;
                                }
                                Ok::<_, anyhow::Error>(())
                            });
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
                                        entry.address
                                    })
                            };
                            let reply = match found {
                                Some(address) => Reply::Peer { device_id, address },
                                None => Reply::Missing {},
                            };
                            write(&mut send, &reply, MAX_REPLY).await?;
                            send.finish()?;
                        }
                        _ => bail!("this endpoint only accepts discovery probes"),
                    }
                    while work.try_join_next().is_some() {}
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
) -> Result<()> {
    let connection = server.connect(endpoint).await?;
    let (mut send, mut recv) = connection.open_bi().await?;
    write(&mut send, &Request::Register {}, MAX_REQUEST).await?;
    send.finish()?;
    let Reply::Registered { address } =
        tokio::time::timeout(CONTROL_TIMEOUT, read(&mut recv, MAX_REPLY)).await??
    else {
        bail!("discovery server did not register this endpoint");
    };
    validate_address(address)?;
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
    let result = async {
        loop {
            let reply: Reply = tokio::select! {
                _ = connection.closed() => break,
                reply = read(&mut recv, MAX_REPLY) => reply?,
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
                            matches!(
                                read::<Reply>(&mut recv, MAX_REPLY).await?,
                                Reply::Probed {}
                            ),
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
        net::{Net, NetworkConfig, PathKind, RelayServiceMode, SYNC},
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
                        "test endpoint bind {:?}, mode {:?}",
                        config.bind.or(config.relay_listen),
                        config.relay_mode
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
                json!({"type":"register","address":"127.0.0.1:1234"})
            )
            .is_err()
        );
        Ok(())
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
            relay_mode: RelayServiceMode::DiscoveryOnly,
            relay_listen: Some("127.0.0.1:0".parse()?),
            ..Default::default()
        };
        let service = net(&identity, &config).await?;
        let service_router = service.discovery_router();
        let service_address = *service
            .addr()
            .ip_addrs()
            .next()
            .context("service IP missing")?;
        config.relay_listen = Some(service_address);
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
                relay_mode: RelayServiceMode::DiscoveryOnly,
                relay_listen: Some("0.0.0.0:0".parse()?),
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
        let mut a_config = NetworkConfig {
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
        )
        .await?;
        let relay_url = format!(
            "http://{}",
            relay.http_addr().context("relay listener missing")?
        );
        a_config.relays = vec![relay_url.clone()];
        b_config.relays = vec![relay_url];
        let a = net(&a_id, &a_config).await?;
        let b = net(&b_id, &b_config).await?;
        let a_router = router(&a);
        let b_router = router(&b);
        registered(&service, 2).await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            while b.addr().relay_urls().next().is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        a.learn(&b.addr()).await;
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
        relay.shutdown().await?;
        Ok(())
    }
}
