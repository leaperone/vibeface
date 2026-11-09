//! The relay this Node runs and the ones it adopts. `configure_relay` opens or closes the relay
//! while the Node runs; an open relay is announced on the mesh only after a neighbor reached it
//! from outside. Other Nodes' open relays join the relay map while they rank among the best.
use super::*;
use crate::net::{
    self, NetworkConfig,
    mesh::record::{Access as RelayAccessKind, Role},
    relay::{RelayDb, RelayGrant, Usage},
};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    num::{NonZeroU32, NonZeroUsize},
};

const DEFAULT_LISTEN: &str = "0.0.0.0:3340";
const DEFAULT_RATE: u32 = 16 * 1024;
const DEFAULT_MAX_CLIENTS: usize = 128;
const DEFAULT_MAX_CLIENTS_PER_IP: usize = 8;
/// Waits between outside checks while no neighbor is there to ask.
const NO_NEIGHBOR_RETRY: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(10)
};
const PROBE_RETRY: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(60)
};
const PROBE_RETRY_MAX: Duration = Duration::from_secs(1800);
/// An announced relay is checked from outside again this often.
const PROBE_AGAIN: Duration = Duration::from_secs(1800);
const ADOPT_EVERY: Duration = Duration::from_secs(300);
/// Table changes re-rank the relays at most this often.
const ADOPT_GAP: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RelayStatus {
    Off,
    Checking,
    Announced,
    Unreachable,
}

struct State {
    status: RelayStatus,
    error: Option<String>,
    /// The URL a neighbor reached, which the record announces.
    announced: Option<String>,
}

/// The relay settings in effect, which start from `config.json` and follow `configure_relay`.
pub(crate) struct Volunteer {
    network: std::sync::Mutex<NetworkConfig>,
    state: std::sync::Mutex<State>,
    pub(super) usage: Arc<Usage>,
    wake: tokio::sync::Notify,
}

impl Volunteer {
    pub(super) fn new(network: &NetworkConfig, data_dir: &Path) -> Self {
        let db = match RelayDb::open(&data_dir.join("mesh.sqlite3")) {
            Ok(db) => Some(db),
            Err(error) => {
                tracing::warn!(event = "relay.db_unavailable", %error);
                None
            }
        };
        let status = if volunteering(network) {
            RelayStatus::Checking
        } else {
            RelayStatus::Off
        };
        Self {
            network: std::sync::Mutex::new(network.clone()),
            state: std::sync::Mutex::new(State {
                status,
                error: None,
                announced: None,
            }),
            usage: Usage::new(db),
            wake: tokio::sync::Notify::new(),
        }
    }

    fn set(&self, status: RelayStatus, error: Option<String>, announced: Option<String>) {
        *self.state.lock().expect("relay state") = State {
            status,
            error,
            announced,
        };
    }
}

fn volunteering(network: &NetworkConfig) -> bool {
    network.relay_open && network.relay_listen.is_some()
}

/// `configure_relay`'s parameters.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    volunteer: bool,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    listen: Option<SocketAddr>,
    #[serde(default)]
    rate: Option<NonZeroU32>,
    #[serde(default)]
    max_clients: Option<NonZeroUsize>,
    #[serde(default)]
    max_clients_per_ip: Option<NonZeroUsize>,
}

/// Clears what runs a relay here.
fn close(network: &mut NetworkConfig) {
    network.relay_listen = None;
    network.relay_url = None;
    network.relay_operator_token = None;
    network.relay_open = false;
    network.relay_rate = None;
    network.relay_max_clients = None;
    network.relay_max_clients_per_ip = None;
    network.relay_blocked.clear();
}

impl Runtime {
    /// The relay settings in effect.
    pub(crate) fn relay_config(&self) -> NetworkConfig {
        self.volunteer
            .network
            .lock()
            .expect("relay settings")
            .clone()
    }

    pub(crate) fn relay_settings(&self) -> Result<Value> {
        let network = self.relay_config();
        let state = self.volunteer.state.lock().expect("relay state");
        let volunteer = volunteering(&network);
        Ok(json!({
            "volunteer": volunteer,
            "url": network.own_relay()?.map(|url| url.to_string()),
            "listen": network.relay_listen.map(|listen| listen.to_string()),
            "rate": network.relay_rate.map_or(0, NonZeroU32::get),
            "max_clients": network.relay_max_clients.map_or(0, NonZeroUsize::get),
            "max_clients_per_ip": network.relay_max_clients_per_ip.map_or(0, NonZeroUsize::get),
            "status": if volunteer { state.status } else { RelayStatus::Off },
            "error": state.error,
        }))
    }

    /// Opens this Node's relay to everyone or closes it, saves the settings and restarts the
    /// relay without restarting the endpoint. A relay that cannot start is closed again and the
    /// answer carries why.
    pub(crate) async fn configure_relay(self: &Arc<Self>, params: Value) -> Result<Value> {
        let change: Change = serde_json::from_value(params).context("invalid relay settings")?;
        let _save = self.config_save.lock().await;
        let previous = self.relay_config();
        let mut next = previous.clone();
        if change.volunteer {
            ensure!(
                previous.relay_operator_token.is_none(),
                "this relay admits by operator token; remove it before opening the relay"
            );
            let listen = match change.listen.or(previous.relay_listen) {
                Some(listen) => listen,
                None => DEFAULT_LISTEN.parse()?,
            };
            next.relay_listen = Some(listen);
            next.relay_url = match change.url {
                Some(url) => Some(url),
                None => match &previous.relay_url {
                    Some(url) => Some(url.clone()),
                    None => self.public_relay_url(listen),
                },
            };
            next.relay_open = true;
            next.relay_rate = change
                .rate
                .or(previous.relay_rate)
                .or(NonZeroU32::new(DEFAULT_RATE));
            next.relay_max_clients = change
                .max_clients
                .or(previous.relay_max_clients)
                .or(NonZeroUsize::new(DEFAULT_MAX_CLIENTS));
            next.relay_max_clients_per_ip = change
                .max_clients_per_ip
                .or(previous.relay_max_clients_per_ip)
                .or(NonZeroUsize::new(DEFAULT_MAX_CLIENTS_PER_IP));
            if next.relay_url.is_none() && listen.ip().is_unspecified() {
                bail!("no public address of this Node is known; give the relay's URL");
            }
        } else {
            close(&mut next);
        }
        next.validate()?;
        self.save_relay(&next)?;
        let error = match self.restart_relay(&previous, &next).await {
            Ok(()) => None,
            Err(error) => {
                let message = format!("{error:#}");
                tracing::warn!(event = "relay.start_failed", error = %message);
                let closed = {
                    let mut closed = next.clone();
                    close(&mut closed);
                    closed
                };
                self.save_relay(&closed)?;
                let _ = self.restart_relay(&next, &closed).await;
                Some(message)
            }
        };
        let status = if volunteering(&self.relay_config()) {
            RelayStatus::Checking
        } else {
            RelayStatus::Off
        };
        self.volunteer.set(status, error, None);
        self.volunteer.wake.notify_one();
        if let Some(mesh) = self.net().ok().and_then(|net| net.mesh()) {
            mesh.set_roles(self.mesh_roles());
        }
        self.relay_settings()
    }

    /// Where devices elsewhere reach a relay on `listen`: the public host name of this Node's
    /// web address, else a public IPv4 address the endpoint has seen for itself.
    fn public_relay_url(&self, listen: SocketAddr) -> Option<String> {
        let port = listen.port();
        if let Ok(url) = reqwest::Url::parse(&self.config.public_url)
            && net::mesh::public_host(&url)
            && let Some(host) = url.host_str()
        {
            return Some(format!("http://{host}:{port}"));
        }
        let net = self.net().ok()?;
        net.addr()
            .ip_addrs()
            .find(|addr| addr.is_ipv4() && crate::fetch::public(addr.ip()))
            .map(|addr| format!("http://{}:{port}", addr.ip()))
    }

    /// Writes the relay fields of `network` to `config.json`; the caller holds `config_save`.
    fn save_relay(&self, network: &NetworkConfig) -> Result<()> {
        let mut config = crate::config::Config::load(&self.data_dir)?;
        config.network.relay_listen = network.relay_listen;
        config.network.relay_url.clone_from(&network.relay_url);
        config
            .network
            .relay_operator_token
            .clone_from(&network.relay_operator_token);
        config.network.relay_open = network.relay_open;
        config.network.relay_rate = network.relay_rate;
        config.network.relay_max_clients = network.relay_max_clients;
        config.network.relay_max_clients_per_ip = network.relay_max_clients_per_ip;
        config
            .network
            .relay_blocked
            .clone_from(&network.relay_blocked);
        config.validate()?;
        config.save(&self.data_dir)?;
        *self.volunteer.network.lock().expect("relay settings") = network.clone();
        Ok(())
    }

    /// Replaces the relay `previous` ran with the one `next` runs, and its URL in the relay map.
    async fn restart_relay(
        self: &Arc<Self>,
        previous: &NetworkConfig,
        next: &NetworkConfig,
    ) -> Result<()> {
        let Ok(net) = self.net().cloned() else {
            return Ok(());
        };
        if let Some(relay) = self.relay.lock().await.take() {
            relay.shutdown().await;
        }
        let (old, new) = (previous.own_relay()?, next.own_relay()?);
        if old != new {
            let configured = |url: &iroh::RelayUrl| {
                self.config
                    .network
                    .relays
                    .iter()
                    .any(|relay| net::relay_url(&relay.url).is_ok_and(|known| known == *url))
            };
            if let Some(old) = old.filter(|url| !configured(url)) {
                net.remove_relay(&old).await;
            }
            if let Some(new) = new {
                net.insert_relay(new, None).await;
            }
        }
        self.start_relay().await
    }

    /// Starts the relay the settings in effect describe, if any.
    pub(super) async fn start_relay(self: &Arc<Self>) -> Result<()> {
        let network = self.relay_config();
        let Some(listen) = network.relay_listen else {
            return Ok(());
        };
        let access = peers::RelayAccess::new(Arc::downgrade(self));
        let relay = net::spawn_relay(
            listen,
            Arc::new(access),
            network.relay_rate,
            network.relay_max_clients_per_ip,
            self.volunteer.usage.clone(),
        )
        .await?;
        tracing::info!(event = "relay.started", %listen, "relay started");
        *self.relay.lock().await = Some(relay);
        Ok(())
    }

    /// The relay role this Node announces once a neighbor reached its open relay.
    pub(super) fn relay_role(&self) -> Option<Role> {
        let network = self.relay_config();
        let state = self.volunteer.state.lock().expect("relay state");
        if !volunteering(&network) || state.status != RelayStatus::Announced {
            return None;
        }
        Some(Role::Relay {
            url: state.announced.clone()?,
            access: RelayAccessKind::Open,
            rate: network.relay_rate.map_or(0, NonZeroU32::get),
            max_clients: network
                .relay_max_clients
                .map_or(0, |max| u32::try_from(max.get()).unwrap_or(u32::MAX)),
            offer: None,
        })
    }

    /// Whether `token`, a relay grant, admits `device`: this Node signed it for that device, it
    /// has not expired, and the device has not used up its bytes.
    pub(super) fn grant_admits(&self, token: &str, device: &str) -> bool {
        let Ok(own) = self.identity.device_id() else {
            return false;
        };
        let Ok(grant) = RelayGrant::open(token, &own, device, now_ms() as u64) else {
            return false;
        };
        match grant.max_bytes {
            None => true,
            Some(max) => self
                .volunteer
                .usage
                .db()
                .and_then(|db| db.bytes(device).ok())
                .is_some_and(|used| used < max),
        }
    }

    /// Asks a mesh neighbor to reach the open relay from outside, and announces it once one
    /// did; retries with backoff when none could.
    pub(super) async fn volunteer_loop(self: Arc<Self>) {
        let mut wait: Option<Duration> = Some(Duration::ZERO);
        let mut retry = PROBE_RETRY;
        let mut checked: Option<String> = None;
        loop {
            match wait {
                Some(wait) => tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = self.volunteer.wake.notified() => {}
                },
                None => self.volunteer.wake.notified().await,
            }
            let network = self.relay_config();
            let url = match volunteering(&network).then(|| network.own_relay()) {
                Some(Ok(Some(url))) => url.to_string(),
                _ => {
                    wait = None;
                    checked = None;
                    continue;
                }
            };
            if checked.as_ref() != Some(&url) {
                retry = PROBE_RETRY;
                checked = Some(url.clone());
            }
            let mesh = self.net().ok().and_then(|net| net.mesh());
            let result = match &mesh {
                None => Err(anyhow::anyhow!(
                    "the mesh is off, so no neighbor can reach the relay"
                )),
                Some(mesh) if mesh.neighbors().is_empty() => {
                    wait = Some(NO_NEIGHBOR_RETRY);
                    continue;
                }
                Some(mesh) => net::relay::probe_through(mesh, &url).await,
            };
            // The settings changed while a neighbor was checking; check the new ones.
            if self
                .relay_config()
                .own_relay()
                .ok()
                .flatten()
                .map(|url| url.to_string())
                != Some(url.clone())
                || !volunteering(&self.relay_config())
            {
                wait = Some(Duration::ZERO);
                continue;
            }
            match result {
                Ok(()) => {
                    tracing::info!(event = "relay.announced", %url, "a neighbor reached the relay");
                    self.volunteer.set(RelayStatus::Announced, None, Some(url));
                    wait = Some(PROBE_AGAIN);
                    retry = PROBE_RETRY;
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    tracing::info!(event = "relay.unreachable", %url, error = %message);
                    self.volunteer
                        .set(RelayStatus::Unreachable, Some(message), None);
                    wait = Some(retry);
                    retry = (retry * 2).min(PROBE_RETRY_MAX);
                }
            }
            if let Some(mesh) = mesh {
                mesh.set_roles(self.mesh_roles());
            }
        }
    }

    /// Keeps the best open relays the mesh announces in the relay map, re-ranked every five
    /// minutes and when the records change.
    pub(super) async fn adopt_loop(self: Arc<Self>) {
        let Ok(net) = self.net().cloned() else { return };
        let Some(mesh) = net.mesh() else { return };
        let Some(db) = self.volunteer.usage.db().cloned() else {
            return;
        };
        let allow_private = self.config.network.mesh.allow_private;
        let mut changes = mesh.changes();
        loop {
            let started = Instant::now();
            let own = self.relay_config().own_relay().ok().flatten();
            let own_device = self.identity.device_id().unwrap_or_default();
            let candidates =
                net::relay::candidates(&mesh.records(), &own_device, own.as_ref(), allow_private);
            let check =
                |url: String| async move { net::relay::check_relay(&url, allow_private).await };
            match net::relay::adopt(&net, &db, candidates, check).await {
                Ok(adopted) => tracing::debug!(event = "relay.adopted", count = adopted.len()),
                Err(error) => tracing::warn!(event = "relay.adopt_failed", %error),
            }
            tokio::time::sleep_until((started + ADOPT_GAP).into()).await;
            tokio::select! {
                _ = tokio::time::sleep(ADOPT_EVERY.saturating_sub(ADOPT_GAP)) => {}
                changed = changes.changed() => if changed.is_err() {
                    tokio::time::sleep(ADOPT_EVERY).await;
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn node(network: NetworkConfig) -> Result<(Arc<Runtime>, PathBuf)> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/relay-tests")
            .join(format!("node-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        let mut configuration = Config::local("127.0.0.1:0".parse()?, vec![], false);
        configuration.network = network;
        configuration.providers.codex_app_server.socket = Some(dir.join("unavailable.sock"));
        configuration.save(&dir)?;
        config::private_write(
            &dir.join("data-epoch.json"),
            &crate::config::data_epoch_marker(),
        )?;
        let identity = LocalIdentity::generate();
        config::private_write(&dir.join("identity.json"), &serde_json::to_vec(&identity)?)?;
        let node = Arc::new(Runtime::open(&dir).await?);
        node.start_network().await?;
        Ok((node, dir))
    }

    fn free_port() -> Result<u16> {
        Ok(std::net::TcpListener::bind("127.0.0.1:0")?
            .local_addr()?
            .port())
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .with_context(|| format!("timed out waiting until {what}"))
    }

    /// B opens its relay; a mesh neighbor reaches it from outside before B announces it.
    /// Closing it withdraws the role, stops the relay and clears `config.json`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_volunteer_relay_is_announced_after_a_neighbor_reaches_it() -> Result<()> {
        let mesh = net::mesh::MeshConfig {
            enabled: true,
            network: format!("relay-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
            bootstrap: vec![],
            allow_private: true,
        };
        let config = |mesh| NetworkConfig {
            bind: Some("127.0.0.1:0".parse().unwrap()),
            mesh,
            ..Default::default()
        };
        let (a, dir_a) = node(config(mesh.clone())).await?;
        let a_bootstrap = format!(
            "vibeface://{}@127.0.0.1:{}",
            a.identity.device_id()?,
            a.net()?.endpoint.bound_sockets()[0].port()
        );
        let (b, dir_b) = node(config(net::mesh::MeshConfig {
            bootstrap: vec![a_bootstrap],
            ..mesh
        }))
        .await?;
        let result = async {
            let off = b.relay_settings()?;
            assert_eq!(
                off,
                json!({"volunteer":false,"url":null,"listen":null,"rate":0,"max_clients":0,
                    "max_clients_per_ip":0,"status":"off","error":null})
            );
            let port = free_port()?;
            let listen = format!("127.0.0.1:{port}");
            // A URL no relay answers on is never announced.
            let dead = format!("http://127.0.0.1:{}", free_port()?);
            let unreachable = b
                .configure_relay(json!({"volunteer":true,"listen":listen,"url":dead}))
                .await?;
            assert_eq!(unreachable["listen"], listen, "listen comes back as given");
            assert_eq!(
                [
                    &unreachable["rate"],
                    &unreachable["max_clients"],
                    &unreachable["max_clients_per_ip"]
                ],
                [&json!(16384), &json!(128), &json!(8)]
            );
            until("the neighbor failed to reach the relay", || {
                b.relay_settings().is_ok_and(|settings| {
                    settings["status"] == "unreachable" && settings["error"].is_string()
                })
            })
            .await?;
            assert!(b.relay_role().is_none());
            assert!(
                b.mesh_roles()
                    .iter()
                    .all(|role| !matches!(role, Role::Relay { .. }))
            );

            let url = format!("http://127.0.0.1:{port}");
            let on = b
                .configure_relay(json!({"volunteer":true,"listen":listen,"url":url}))
                .await?;
            assert_eq!(on["volunteer"], true);
            assert_eq!(on["url"], format!("{url}/"));
            assert_eq!(on["rate"], 16384, "limits stay as they were");
            assert!(
                on["status"] == "checking" || on["status"] == "announced",
                "{on}"
            );
            let saved = crate::config::Config::load(&b.data_dir)?.network;
            assert!(saved.relay_open && saved.relay_listen == Some(listen.parse()?));
            until("a neighbor reached the relay", || {
                b.relay_settings()
                    .is_ok_and(|settings| settings["status"] == "announced")
            })
            .await?;
            let announced = format!("{url}/");
            assert!(b.mesh_roles().iter().any(|role| matches!(role,
                Role::Relay { url, access: RelayAccessKind::Open, rate: 16384, max_clients: 128, .. }
                    if *url == announced)));
            let b_id = b.identity.device_id()?;
            let a_mesh = a.net()?.mesh().context("A runs no mesh")?;
            until("A holds B's relay role", || {
                a_mesh.records().iter().any(|entry| {
                    entry.record.device_id == b_id
                        && entry
                            .record
                            .roles
                            .iter()
                            .any(|role| matches!(role, Role::Relay { .. }))
                })
            })
            .await?;

            // A grant B signed admits its device until it has used its bytes.
            let device = LocalIdentity::generate().device_id()?;
            let grant = |max_bytes| {
                RelayGrant {
                    v: 1,
                    relay: b_id.clone(),
                    endpoint: device.clone(),
                    expires_at_ms: now_ms() as u64 + 60_000,
                    max_bytes,
                }
                .token(|payload| b.identity.sign_payload(net::relay::GRANT_DOMAIN, payload))
            };
            assert!(b.grant_admits(&grant(None)?, &device));
            assert!(b.grant_admits(&grant(Some(1))?, &device));
            assert!(!b.grant_admits(&grant(Some(0))?, &device), "used up");

            let closed = b.configure_relay(json!({"volunteer":false})).await?;
            assert_eq!(closed, off);
            assert!(
                b.mesh_roles()
                    .iter()
                    .all(|role| !matches!(role, Role::Relay { .. }))
            );
            assert!(
                tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .is_err(),
                "the relay stopped"
            );
            let saved = crate::config::Config::load(&b.data_dir)?.network;
            assert!(!saved.relay_open && saved.relay_listen.is_none());

            // A port in use leaves the relay closed and says why.
            let taken = std::net::TcpListener::bind("127.0.0.1:0")?;
            let taken = taken.local_addr()?.to_string();
            let refused = b
                .configure_relay(json!({"volunteer":true,"listen":taken,
                    "url":format!("http://{taken}")}))
                .await?;
            assert_eq!(refused["volunteer"], false);
            assert_eq!(refused["status"], "off");
            assert!(
                refused["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("relay failed to start")),
                "{refused}"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        for node in [&a, &b] {
            node.stop_network().await;
            node.shutdown().await;
        }
        drop((a, b));
        for dir in [dir_a, dir_b] {
            let _ = std::fs::remove_dir_all(dir);
        }
        result
    }

    #[test]
    fn closing_clears_every_relay_field() -> Result<()> {
        let mut network = NetworkConfig {
            relay_listen: Some("127.0.0.1:3340".parse()?),
            relay_url: Some("http://203.0.113.7:3340".into()),
            relay_open: true,
            relay_rate: NonZeroU32::new(DEFAULT_RATE),
            relay_max_clients: NonZeroUsize::new(DEFAULT_MAX_CLIENTS),
            relay_max_clients_per_ip: NonZeroUsize::new(DEFAULT_MAX_CLIENTS_PER_IP),
            ..Default::default()
        };
        network.validate()?;
        assert!(volunteering(&network));
        close(&mut network);
        network.validate()?;
        assert!(!volunteering(&network) && network.own_relay()?.is_none());
        Ok(())
    }
}
