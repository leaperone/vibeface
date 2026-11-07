//! Public bootstrap against real loopback infrastructure: a discovery-only entry publishing a
//! signed catalogue over UDP and HTTP, a public entry whose relay admits by Discovery grants,
//! and fresh Nodes that find them, lose them, or never had them.

use super::{Runtime, now_ms, peers::RelayAccess};
use crate::{
    api,
    config::{self, Config},
    crypto::LocalIdentity,
    net::{
        self, NetworkConfig, RelayAdmission, RelayServiceMode,
        bootstrap::{self, Bootstrap, BootstrapState, Transport},
    },
};
use anyhow::{Context as _, Result, anyhow, ensure};
use iroh::{Watcher as _, endpoint::RelayStatus};
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::net::TcpListener;

fn free_tcp_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?
        .local_addr()?
        .port())
}

fn free_udp_port() -> Result<u16> {
    Ok(std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?
        .local_addr()?
        .port())
}

async fn runtime(
    identity: &LocalIdentity,
    listen: SocketAddr,
    public_entry: bool,
    network: NetworkConfig,
) -> Result<(Arc<Runtime>, PathBuf)> {
    let dir = std::env::temp_dir().join(format!("vibeface-bootstrap-{}", uuid::Uuid::new_v4()));
    config::private_dir(&dir)?;
    let mut configuration = Config::local(listen, vec![], false);
    configuration.public_entry = public_entry;
    configuration.web_enabled = !public_entry;
    configuration.network = network;
    configuration.providers.codex_app_server.socket = Some(dir.join("unavailable.sock"));
    configuration.save(&dir)?;
    config::private_write(
        &dir.join("data-epoch.json"),
        &crate::config::data_epoch_marker(),
    )?;
    config::private_write(&dir.join("identity.json"), &serde_json::to_vec(identity)?)?;
    Ok((Arc::new(Runtime::open(&dir).await?), dir))
}

fn serve_api(node: Arc<Runtime>, listener: TcpListener) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            api::router(node).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    })
}

/// A discovery-only entry and a grant-admitting public entry, both on loopback.
struct Infra {
    discovery: Arc<Runtime>,
    entry: Arc<Runtime>,
    issuer: String,
    udp_port: u16,
    https: String,
    gateway_url: String,
    relay_url: String,
    dirs: Vec<PathBuf>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Infra {
    async fn start(grant_ttl_ms: i64) -> Result<Self> {
        let entry_api = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let entry_addr = entry_api.local_addr()?;
        let discovery_api = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let discovery_addr = discovery_api.local_addr()?;
        let relay_port = free_tcp_port()?;
        let udp_port = free_udp_port()?;
        let gateway_url = format!("http://{entry_addr}");
        let relay_url = format!("http://127.0.0.1:{relay_port}");
        let discovery_identity = LocalIdentity::generate();
        let issuer = discovery_identity.device_id()?;
        let (entry, entry_dir) = runtime(
            &LocalIdentity::generate(),
            entry_addr,
            true,
            NetworkConfig {
                bind: Some(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)),
                relay_listen: Some(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), relay_port)),
                relay_url: Some(relay_url.clone()),
                relay_admission: RelayAdmission::Grants {
                    issuer: issuer.clone(),
                },
                ..Default::default()
            },
        )
        .await?;
        let (discovery, discovery_dir) = runtime(
            &discovery_identity,
            discovery_addr,
            true,
            NetworkConfig {
                relay_mode: RelayServiceMode::DiscoveryOnly,
                relay_listen: Some(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), udp_port)),
                catalog: Some(bootstrap::CatalogConfig {
                    entries: vec![bootstrap::CatalogEntry {
                        gateway_url: gateway_url.clone(),
                        relay_url: relay_url.clone(),
                    }],
                    grant_ttl_ms,
                }),
                ..Default::default()
            },
        )
        .await?;
        entry.start().await?;
        discovery.start().await?;
        let tasks = vec![
            serve_api(entry.clone(), entry_api),
            serve_api(discovery.clone(), discovery_api),
        ];
        Ok(Self {
            discovery,
            entry,
            issuer,
            udp_port,
            https: format!("http://{discovery_addr}/bootstrap"),
            gateway_url,
            relay_url,
            dirs: vec![entry_dir, discovery_dir],
            tasks,
        })
    }

    fn discovery_url(&self, issuer: &str, udp_port: u16) -> String {
        format!("vibeface://{issuer}@127.0.0.1:{udp_port}")
    }

    /// The source a Node pins: this Discovery over UDP, with HTTP as the second path.
    fn source(&self, https: bool) -> Bootstrap {
        Bootstrap::Custom {
            discovery: self.discovery_url(&self.issuer, self.udp_port),
            https: https.then(|| self.https.clone()),
        }
    }

    fn relay(&self) -> Result<iroh::RelayUrl> {
        net::relay_url(&self.relay_url)
    }

    async fn stop(self) -> Result<()> {
        let Self {
            discovery,
            entry,
            dirs,
            tasks,
            ..
        } = self;
        for task in tasks {
            task.abort();
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                return Err(error).context("bootstrap test API task failed to stop");
            }
        }
        entry.shutdown().await;
        discovery.shutdown().await;
        drop(entry);
        drop(discovery);
        for dir in dirs {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("remove bootstrap fixture {}", dir.display()))?;
        }
        Ok(())
    }
}

impl Runtime {
    async fn stop_entry_relay(self: &Arc<Self>) -> Result<()> {
        let relay = self
            .relay
            .lock()
            .await
            .take()
            .context("relay not running")?;
        relay
            .shutdown()
            .await
            .map_err(|error| anyhow!("relay shutdown failed: {error}"))?;
        Ok(())
    }

    async fn start_entry_relay(self: &Arc<Self>) -> Result<()> {
        let listen = self
            .config
            .network
            .relay_listen
            .context("this Node runs no relay")?;
        let access = Arc::new(RelayAccess(Arc::downgrade(self)));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match net::spawn_relay(listen, access.clone()).await {
                Ok(relay) => {
                    *self.relay.lock().await = Some(relay);
                    return Ok(());
                }
                Err(error) if tokio::time::Instant::now() < deadline => {
                    tracing::debug!(%error, "relay port still closing");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Drops every relay connection the way a relay deployment does: the server restarts on the
    /// same address and every Node reconnects with the token its relay actor still holds.
    async fn restart_relay(self: &Arc<Self>) -> Result<()> {
        self.stop_entry_relay().await?;
        self.start_entry_relay().await
    }
}

async fn fresh_node(bootstrap: Bootstrap) -> Result<(Arc<Runtime>, PathBuf)> {
    let (node, dir) = runtime(
        &LocalIdentity::generate(),
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
        false,
        NetworkConfig {
            bind: Some(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)),
            bootstrap,
            ..Default::default()
        },
    )
    .await?;
    node.start_network().await?;
    Ok((node, dir))
}

async fn wait_admitted(node: &Runtime, timeout: Duration) -> Result<bootstrap::Admitted> {
    node.net()?.bootstrap().wait_admitted(timeout).await
}

fn connected(statuses: &[RelayStatus], relay: &iroh::RelayUrl) -> bool {
    statuses
        .iter()
        .any(|status| status.url() == relay && status.is_connected())
}

/// Waits until the endpoint reports a connection to `relay`. With `current`, a connection that
/// already exists counts; without it, only a status change after the watcher was taken does,
/// which is how a reconnection is told apart from a connection that never dropped.
async fn wait_connected<W>(status: &mut W, relay: &iroh::RelayUrl, current: bool) -> Result<()>
where
    W: iroh::Watcher<Value = Vec<RelayStatus>>,
{
    tokio::time::timeout(Duration::from_secs(45), async {
        if current && connected(&status.get(), relay) {
            return Ok::<(), anyhow::Error>(());
        }
        loop {
            let value = status
                .updated()
                .await
                .map_err(|_| anyhow!("endpoint closed"))?;
            if connected(&value, relay) {
                return Ok(());
            }
        }
    })
    .await
    .context("the relay never admitted this endpoint")?
}

async fn wait_relay_connection(node: &Runtime, relay: &iroh::RelayUrl) -> Result<()> {
    let mut status = node.net()?.endpoint.home_relay_status();
    wait_connected(&mut status, relay, true).await
}

async fn stop_node(node: Arc<Runtime>, dir: PathBuf) -> Result<()> {
    node.shutdown().await;
    drop(node);
    std::fs::remove_dir_all(&dir)
        .with_context(|| format!("remove bootstrap fixture {}", dir.display()))?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission_waits_for_the_public_relay_listener_before_reporting_ready() -> Result<()> {
    let infra = Infra::start(bootstrap::DEFAULT_GRANT_TTL_MS).await?;
    infra.entry.stop_entry_relay().await?;
    let relay = infra.relay()?;
    let started = tokio::time::Instant::now();
    let (node, dir) = tokio::time::timeout(Duration::from_secs(5), fresh_node(infra.source(true)))
        .await
        .context("network start must not wait for a cold public relay")??;
    assert!(started.elapsed() < Duration::from_secs(5));
    let endpoint_id = node.net()?.endpoint.id();
    let admission = infra
        .entry
        .net()?
        .grant_admission()
        .context("the entry admits by grants")?;
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            if admission.admitted_until(&endpoint_id).is_some() {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("the entry never registered a grant for this endpoint")??;
    assert!(
        wait_admitted(&node, Duration::from_secs(1)).await.is_err(),
        "bootstrap must not admit while the relay listener is still down"
    );
    assert_eq!(
        node.gateway_url().as_deref(),
        Some(node.config.public_url.as_str()),
        "without a live relay this Node keeps advertising itself"
    );
    assert!(
        !connected(&node.net()?.endpoint.home_relay_status().get(), &relay),
        "grant registration alone must not look like a connected relay"
    );
    infra.entry.start_entry_relay().await?;
    let admitted = wait_admitted(&node, Duration::from_secs(40)).await?;
    assert!(
        connected(&node.net()?.endpoint.home_relay_status().get(), &relay),
        "admission must align with relay connectivity, not only grant registration"
    );
    assert_eq!(admitted.gateway_url, infra.gateway_url);
    assert_eq!(admitted.relay_url, infra.relay_url);
    assert_eq!(
        node.gateway_url().as_deref(),
        Some(infra.gateway_url.as_str()),
        "after admission the catalogue gateway is advertised"
    );
    stop_node(node, dir).await?;
    infra.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_node_is_admitted_over_udp_and_relays_through_the_public_entry() -> Result<()> {
    let infra = Infra::start(bootstrap::DEFAULT_GRANT_TTL_MS).await?;
    let (node, dir) = fresh_node(infra.source(true)).await?;
    let admitted = wait_admitted(&node, Duration::from_secs(40)).await?;
    assert_eq!(
        admitted.via,
        Transport::Udp,
        "UDP is the first path when it answers"
    );
    assert_eq!(admitted.gateway_url, infra.gateway_url);
    assert_eq!(admitted.relay_url, infra.relay_url);
    assert_eq!(
        node.gateway_url().as_deref(),
        Some(infra.gateway_url.as_str()),
        "the admitted public entry is the Gateway this Node advertises"
    );
    let relay = infra.relay()?;
    wait_relay_connection(&node, &relay).await?;
    assert!(
        node.net()?.addr().relay_urls().any(|url| *url == relay),
        "the granted relay is part of this Node's address"
    );
    let state = serde_json::to_value(node.bootstrap_state().context("bootstrap is on")?)?;
    assert_eq!(state["state"], "admitted");
    assert_eq!(state["via"], "udp");
    let registered = infra
        .entry
        .net()?
        .grant_admission()
        .context("the entry admits by grants")?
        .admitted_until(&node.net()?.endpoint.id());
    assert_eq!(
        registered,
        Some(admitted.grant_until_ms),
        "the entry learnt the grant"
    );
    stop_node(node, dir).await?;
    infra.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_whose_udp_is_blocked_is_admitted_over_http() -> Result<()> {
    let infra = Infra::start(bootstrap::DEFAULT_GRANT_TTL_MS).await?;
    let closed = free_udp_port()?;
    let (node, dir) = fresh_node(Bootstrap::Custom {
        discovery: infra.discovery_url(&infra.issuer, closed),
        https: Some(infra.https.clone()),
    })
    .await?;
    let admitted = wait_admitted(&node, Duration::from_secs(60)).await?;
    assert_eq!(admitted.via, Transport::Https);
    assert_eq!(admitted.relay_url, infra.relay_url);
    wait_relay_connection(&node, &infra.relay()?).await?;
    stop_node(node, dir).await?;
    infra.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grants_are_renewed_registered_and_survive_a_relay_restart() -> Result<()> {
    let infra = Infra::start(6_000).await?;
    let (node, dir) = fresh_node(infra.source(true)).await?;
    let first = wait_admitted(&node, Duration::from_secs(40)).await?;
    let relay = infra.relay()?;
    wait_relay_connection(&node, &relay).await?;
    let endpoint_id = node.net()?.endpoint.id();
    let admission = infra
        .entry
        .net()?
        .grant_admission()
        .context("the entry admits by grants")?
        .clone();
    // The grant is renewed about halfway through its life and the entry learns the renewal.
    let renewed = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(current) = node.net()?.bootstrap().admitted()
                && current.grant_until_ms > first.grant_until_ms
            {
                return Ok::<_, anyhow::Error>(current);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("the grant was never renewed")??;
    assert!(
        admission
            .admitted_until(&endpoint_id)
            .is_some_and(|until| until >= renewed.grant_until_ms),
        "the entry learnt the renewal before the Node reported it"
    );
    // The relay actor still presents the first grant. Once that has expired, a relay restart
    // makes every Node reconnect with it; the registered renewal is what admits this one.
    let wait = (first.grant_until_ms - now_ms()).max(0) as u64 + 200;
    tokio::time::sleep(Duration::from_millis(wait)).await;
    let mut status = node.net()?.endpoint.home_relay_status();
    infra.entry.restart_relay().await?;
    wait_connected(&mut status, &relay, false).await?;
    assert!(
        node.net()?.bootstrap().admitted().is_some(),
        "the Node stays admitted across the restart"
    );
    // A relay that lost its memory refuses the stale grant on reconnect; the Node notices the
    // denial or simply renews on schedule, registers, and is admitted on the next retry. Either
    // way it is back without hammering the relay, and its registration is current again.
    let mut status = node.net()?.endpoint.home_relay_status();
    admission.forget_all();
    infra.entry.restart_relay().await?;
    wait_connected(&mut status, &relay, false).await?;
    assert!(
        admission
            .admitted_until(&endpoint_id)
            .is_some_and(|until| until > now_ms()),
        "the Node re-registered a fresh grant"
    );
    stop_node(node, dir).await?;
    infra.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_without_public_infrastructure_starts_locally_and_keeps_seeking() -> Result<()> {
    let closed_udp = free_udp_port()?;
    let closed_tcp = free_tcp_port()?;
    let issuer = LocalIdentity::generate().device_id()?;
    let started = tokio::time::Instant::now();
    let (node, dir) = tokio::time::timeout(
        Duration::from_secs(5),
        fresh_node(Bootstrap::Custom {
            discovery: format!("vibeface://{issuer}@127.0.0.1:{closed_udp}"),
            https: Some(format!("http://127.0.0.1:{closed_tcp}/bootstrap")),
        }),
    )
    .await
    .context("network start must not wait for the public infrastructure")??;
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        node.gateway_url(),
        Some(node.config.public_url.clone()),
        "without an admitted entry this Node advertises itself"
    );
    // Startup can finish while discovery is still awaiting its bounded HTTP fallback.
    let mut states = node.net()?.bootstrap().subscribe();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let state = states.borrow_and_update().clone();
            match state {
                BootstrapState::Seeking {
                    reason: Some(_), ..
                } => return Ok::<(), anyhow::Error>(()),
                BootstrapState::Seeking { reason: None, .. } => {
                    states.changed().await.context("bootstrap stopped")?;
                }
                other => anyhow::bail!("unexpected bootstrap state: {other:?}"),
            }
        }
    })
    .await
    .context("the failure must be reported after discovery and HTTP have failed")??;
    assert!(node.net()?.bootstrap().admitted().is_none());
    stop_node(node, dir).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_catalogue_from_another_issuer_never_admits() -> Result<()> {
    let infra = Infra::start(bootstrap::DEFAULT_GRANT_TTL_MS).await?;
    let other = LocalIdentity::generate().device_id()?;
    let (node, dir) = fresh_node(Bootstrap::Custom {
        discovery: infra.discovery_url(&other, infra.udp_port),
        https: Some(infra.https.clone()),
    })
    .await?;
    assert!(
        wait_admitted(&node, Duration::from_secs(8)).await.is_err(),
        "a catalogue signed by another key is not trusted"
    );
    match node.bootstrap_state().context("bootstrap is on")? {
        BootstrapState::Seeking {
            reason: Some(reason),
            ..
        } => {
            assert!(
                reason.contains("issuer") || reason.contains("catalogue"),
                "{reason}"
            );
        }
        other => panic!("unexpected bootstrap state: {other:?}"),
    }
    stop_node(node, dir).await?;
    infra.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap_http_serves_signed_resources_within_the_contract_limits() -> Result<()> {
    let infra = Infra::start(bootstrap::DEFAULT_GRANT_TTL_MS).await?;
    let http = reqwest::Client::builder().no_proxy().build()?;
    let response = http
        .get(format!("{}/catalog", infra.https))
        .header("Origin", "https://elsewhere.example")
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("*"),
        "any client origin may read the catalogue"
    );
    let body = response.bytes().await?;
    ensure!(body.len() <= bootstrap::MAX_SIGNED_CATALOG_BYTES);
    let signed: bootstrap::SignedCatalog = serde_json::from_slice(&body)?;
    let catalog = signed.open(&infra.issuer, now_ms())?;
    assert_eq!(catalog.entries[0].gateway_url, infra.gateway_url);
    assert!(
        signed
            .open(&LocalIdentity::generate().device_id()?, now_ms())
            .is_err(),
        "the catalogue is bound to its issuer"
    );
    let device = LocalIdentity::generate();
    let request = bootstrap::SignedRequest::sign(&device, &infra.relay_url, now_ms())?;
    let granted = http
        .post(format!("{}/grant", infra.https))
        .json(&request)
        .send()
        .await?;
    assert_eq!(granted.status(), reqwest::StatusCode::OK);
    let token = granted.json::<serde_json::Value>().await?["grant"]
        .as_str()
        .context("grant token")?
        .to_owned();
    ensure!(token.len() <= bootstrap::MAX_GRANT_BYTES);
    let grant = bootstrap::SignedGrant::parse(&token)?.verify(
        &infra.issuer,
        &net::endpoint_id(&device.device_id()?)?,
        &infra.relay()?,
        now_ms(),
    )?;
    assert_eq!(grant.device_id, device.device_id()?);
    let mut forged = request.clone();
    forged.signature =
        bootstrap::SignedRequest::sign(&LocalIdentity::generate(), &infra.relay_url, now_ms())?
            .signature;
    let refused = http
        .post(format!("{}/grant", infra.https))
        .json(&forged)
        .send()
        .await?;
    assert_eq!(refused.status(), reqwest::StatusCode::FORBIDDEN);
    let elsewhere = bootstrap::SignedRequest::sign(&device, "https://other.example", now_ms())?;
    let refused = http
        .post(format!("{}/grant", infra.https))
        .json(&elsewhere)
        .send()
        .await?;
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::FORBIDDEN,
        "grants are only issued for relays the catalogue lists"
    );
    let oversized = http
        .post(format!("{}/grant", infra.https))
        .header("content-type", "application/json")
        .body(format!(
            "{{\"payload\":\"{}\",\"signature\":\"\"}}",
            "x".repeat(2000)
        ))
        .send()
        .await?;
    assert_eq!(oversized.status(), reqwest::StatusCode::BAD_REQUEST);
    let registered = http
        .post(format!("{}/api/relay/grant", infra.gateway_url))
        .json(&serde_json::json!({"grant": token}))
        .send()
        .await?;
    assert_eq!(
        registered.status(),
        reqwest::StatusCode::OK,
        "the entry accepts its issuer's grants"
    );
    let foreign = http
        .post(format!("{}/api/relay/grant", infra.gateway_url))
        .json(&serde_json::json!({"grant": "bm90LWEtZ3JhbnQ.00"}))
        .send()
        .await?;
    assert_eq!(foreign.status(), reqwest::StatusCode::FORBIDDEN);
    assert!(
        infra.entry.bootstrap_state().is_none() && infra.discovery.bootstrap_state().is_none(),
        "public entries report no bootstrap of their own"
    );
    infra.stop().await?;
    Ok(())
}
