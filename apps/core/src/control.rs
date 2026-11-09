//! Core-owned one-shot operations used by the sibling CLI.

use crate::{
    config::{self, Config, ManagerSettings},
    crypto::LocalIdentity,
    discovery,
    local_access::Launcher,
    maintenance,
    net::{
        self, NetworkConfig,
        mesh::{self, MeshConfig},
    },
    providers::{AppServer, AppServerMode, Provider, ProviderEnv},
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::{NonZeroU32, NonZeroUsize},
    path::{Path, PathBuf},
    time::Duration,
};
use vibeface_bridge::platform::{clock, privatefs, program};

pub const DEFAULT_LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4790);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InitOptions {
    data_dir: PathBuf,
    #[serde(default = "default_listen")]
    listen: SocketAddr,
    #[serde(default = "default_local_listen")]
    local_listen: SocketAddr,
    public_url: Option<String>,
    #[serde(default)]
    tls: bool,
    #[serde(default)]
    no_web: bool,
    #[serde(default)]
    allowed_origins: Vec<String>,
    #[serde(default)]
    roots: Vec<PathBuf>,
    #[serde(default)]
    no_local_auto_access: bool,
    #[serde(default)]
    allow_insecure_lan: bool,
    #[serde(default)]
    relays: Vec<String>,
    /// `(relay URL, token file)`: the token this Node presents to one of `relays`.
    #[serde(default)]
    relay_tokens: Vec<(String, PathBuf)>,
    relay_listen: Option<SocketAddr>,
    relay_url: Option<String>,
    /// File with the token this Node's relay asks of devices outside its personal network.
    relay_operator_token: Option<PathBuf>,
    #[serde(default)]
    relay_open: bool,
    relay_rate: Option<NonZeroU32>,
    relay_max_clients: Option<NonZeroUsize>,
    relay_max_clients_per_ip: Option<NonZeroUsize>,
    #[serde(default)]
    forward_any: bool,
    forward_sessions: Option<usize>,
    forward_queue: Option<usize>,
    forward_rate: Option<NonZeroU32>,
    forward_per_ip: Option<NonZeroUsize>,
    #[serde(default)]
    trusted_proxies: Vec<IpAddr>,
    manager: Option<Provider>,
    workspace: Option<PathBuf>,
    model: Option<String>,
    #[serde(default)]
    bins: Vec<(Provider, PathBuf)>,
    codex_socket: Option<PathBuf>,
    #[serde(default)]
    no_mesh: bool,
    mesh_network: Option<String>,
    /// Replaces the default bootstrap node.
    #[serde(default)]
    bootstrap: Vec<String>,
}

fn default_listen() -> SocketAddr {
    DEFAULT_LISTEN
}

/// The mesh a fresh Node joins. The default bootstrap node is on the default network only.
fn init_mesh(no_mesh: bool, network: Option<String>, bootstrap: Vec<String>) -> Result<MeshConfig> {
    let mut mesh = MeshConfig {
        enabled: !no_mesh,
        ..MeshConfig::public()?
    };
    if let Some(network) = network {
        if network != mesh.network {
            mesh.bootstrap.clear();
        }
        mesh.network = network;
    }
    if !bootstrap.is_empty() {
        mesh.bootstrap = bootstrap;
    }
    mesh.validate()?;
    Ok(mesh)
}
fn default_local_listen() -> SocketAddr {
    config::DEFAULT_LOCAL_LISTEN
}

pub async fn handle(request: vibeface_control::ControlRequest) -> Result<Value> {
    match request {
        vibeface_control::ControlRequest::Init { options } => {
            let options: InitOptions =
                serde_json::from_value(options).context("invalid init options")?;
            initialize_from_options(options)
        }
        vibeface_control::ControlRequest::Inspect { data_dir } => inspect(&data_dir).await,
        vibeface_control::ControlRequest::DiscoverLocalNodes {
            app_profile_dir,
            remembered_data_dirs,
        } => crate::control_discovery::discover(&app_profile_dir, &remembered_data_dirs).await,
        vibeface_control::ControlRequest::Access { data_dir, json: _ } => access(&data_dir).await,
        vibeface_control::ControlRequest::LocalSetup { data_dir } => local_setup(&data_dir).await,
        vibeface_control::ControlRequest::SetLocalAccess { data_dir, enabled } => {
            set_local_access(&data_dir, enabled)
                .await
                .map(|running| json!({"running":running}))
        }
        vibeface_control::ControlRequest::Join {
            data_dir,
            link,
            name,
        } => join(&data_dir, &link, &name).await,
        vibeface_control::ControlRequest::Wallet {
            data_dir,
            operation,
        } => wallet(&data_dir, operation),
        vibeface_control::ControlRequest::Mesh {
            data_dir,
            operation,
        } => mesh_control(&data_dir, operation).await,
        vibeface_control::ControlRequest::ConfigSet {
            data_dir,
            key,
            value,
        } => config_set(&data_dir, &key, value).await,
    }
}

/// `vibeface config set`: the running Node saves it under its config lock; a stopped one's
/// file is edited here. Either way it takes effect when the Node starts.
async fn config_set(data_dir: &Path, key: &str, value: Value) -> Result<Value> {
    validate_data_dir(data_dir)?;
    let data_dir = fs::canonicalize(data_dir)
        .context("initialized data directory is required; run vibeface init first")?;
    let running = match maintenance::config_set(&data_dir, key, value.clone()).await {
        Ok(()) => true,
        Err(error) if stopped(&error) => {
            Config::set(&data_dir, key, value)?;
            false
        }
        Err(error) => return Err(error),
    };
    Ok(json!({"running":running}))
}

/// The socket is missing or refuses: the Node is not running.
fn stopped(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        )
    })
}

/// `vibeface mesh`: reads the records in `mesh.sqlite3`, which the running Node keeps written
/// through, and edits the bootstrap nodes in the running Node or, stopped, in config.json.
async fn mesh_control(data_dir: &Path, operation: Value) -> Result<Value> {
    validate_data_dir(data_dir)?;
    let data_dir = fs::canonicalize(data_dir)
        .context("initialized data directory is required; run vibeface init first")?;
    let config = Config::load(&data_dir)?.network.mesh;
    let entries = || mesh::table::read(&data_dir.join("mesh.sqlite3"), &config.network);
    let now = crate::runtime::now_ms() as u64;
    match operation["op"].as_str() {
        Some("status") => {
            let own = config::identity(&data_dir)?.device_id()?;
            let entries = entries()?;
            let neighbors = maintenance::probe(&data_dir)
                .await
                .ok()
                .and_then(|health| health.mesh)
                .map(|mesh| mesh.neighbors);
            Ok(json!({
                "enabled": config.enabled,
                "network": config.network,
                "bootstrap": config.bootstrap,
                "records": entries.iter().filter(|entry| entry.online(now)).count(),
                "neighbors": neighbors,
                "own": entries.iter().find(|entry| entry.record.device_id == own).map(|entry| &entry.record),
            }))
        }
        Some("records") => {
            let role = operation["role"].as_str();
            let mut entries = entries()?;
            entries.retain(|entry| {
                role.is_none_or(|role| {
                    entry.record.roles.iter().any(|held| {
                        serde_json::to_value(held).is_ok_and(|held| held["kind"] == role)
                    })
                })
            });
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.last_seen_ms));
            // The answer must fit a control packet.
            let limit = operation["limit"].as_u64().unwrap_or(200).min(500) as usize;
            let records: Vec<Value> = entries
                .iter()
                .take(limit)
                .map(|entry| {
                    json!({"online":entry.online(now),"last_seen_ms":entry.last_seen_ms,
                        "record":entry.record})
                })
                .collect();
            Ok(json!({"records":records,"total":entries.len()}))
        }
        Some("bootstrap") => {
            let list = |key: &str| -> Result<Vec<String>> {
                match &operation[key] {
                    Value::Null => Ok(vec![]),
                    value => {
                        serde_json::from_value(value.clone()).context("invalid bootstrap list")
                    }
                }
            };
            let (add, remove) = (list("add")?, list("remove")?);
            let running =
                match maintenance::mesh_bootstrap(&data_dir, add.clone(), remove.clone()).await {
                    Ok(()) => true,
                    Err(error) if stopped(&error) => {
                        let mut configuration = Config::load(&data_dir)?;
                        configuration.network.mesh.edit_bootstrap(&add, &remove)?;
                        configuration.save(&data_dir)?;
                        false
                    }
                    Err(error) => return Err(error),
                };
            Ok(
                json!({"bootstrap":Config::load(&data_dir)?.network.mesh.bootstrap,"running":running}),
            )
        }
        _ => bail!("unknown mesh operation"),
    }
}

pub fn validate_data_dir(data_dir: &Path) -> Result<()> {
    ensure!(data_dir.is_absolute(), "specify an absolute data directory");
    if let Ok(metadata) = fs::symlink_metadata(data_dir) {
        ensure!(
            !metadata.file_type().is_symlink(),
            "data directory must not be a symbolic link"
        );
    }
    Ok(())
}

fn wallet(data_dir: &Path, operation: Value) -> Result<Value> {
    validate_data_dir(data_dir)?;
    match operation.get("op").and_then(Value::as_str) {
        Some("backup") => {
            let output = operation["output"]
                .as_str()
                .context("wallet backup output is required")?;
            crate::delivery::wallet::backup(data_dir, Path::new(output))?;
            Ok(json!({"status":"backup_saved","output":output}))
        }
        _ => bail!("wallet control is handled by the authenticated maintenance socket"),
    }
}

fn authoritative_core_version(health: Option<&maintenance::Health>) -> &str {
    match health {
        Some(health) => health.version.as_str(),
        None => env!("CARGO_PKG_VERSION"),
    }
}

pub async fn inspect(data_dir: &Path) -> Result<Value> {
    validate_data_dir(data_dir)?;
    let data_dir = fs::canonicalize(data_dir)
        .context("initialized data directory is required; run vibeface init first")?;
    let configuration = Config::load(&data_dir)?;
    let identity = config::identity(&data_dir)?;
    let device_id = identity.device_id()?;
    let socket = maintenance::socket_path(&data_dir)?;
    let health = maintenance::probe(&data_dir).await.and_then(|health| {
        ensure!(
            health.device_id == device_id,
            "running node identity does not match this data directory"
        );
        Ok(health)
    });
    let status = match &health {
        Ok(_) => "running",
        Err(error) if stopped(error) => "stopped",
        Err(_) => "unavailable",
    };
    let running_mesh = health.as_ref().ok().and_then(|health| health.mesh.as_ref());
    let error = health.as_ref().err().map(|error| format!("{error:#}"));
    Ok(json!({
        "status": status,
        "device_id": device_id,
        "data_dir": data_dir,
        "maintenance_socket": socket,
        "listen": configuration.listen,
        "local_listen": configuration.local_listen,
        "public_url": configuration.public_url,
        "web_enabled": health.as_ref().map_or(configuration.web_enabled, |health| health.web_enabled),
        "http_enabled": health.as_ref().is_ok_and(|health| health.http_enabled),
        "version": health.as_ref().ok().map(|health| &health.version),
        "write_state": health.as_ref().ok().and_then(|health| health.write_state),
        "mesh": {
            "enabled": configuration.network.mesh.enabled,
            "network": configuration.network.mesh.network,
            "records": running_mesh.map(|mesh| mesh.records),
            "neighbors": running_mesh.map(|mesh| mesh.neighbors),
        },
        "peers": health.as_ref().ok().map(|health| &health.peers),
        "core_version": authoritative_core_version(health.as_ref().ok()),
        "error": error,
    }))
}

async fn access(data_dir: &Path) -> Result<Value> {
    let info = inspect(data_dir).await?;
    let epoch: Value =
        serde_json::from_slice(&read_bounded(&data_dir.join("data-epoch.json"), 4096)?)?;
    ensure!(
        epoch == json!({"data_epoch": config::DATA_EPOCH}),
        "data directory uses another format; existing data was preserved"
    );
    let configuration = Config::load(data_dir)?;
    ensure!(
        configuration.web_enabled
            && maintenance::probe(data_dir)
                .await
                .map_or(true, |health| health.web_enabled),
        "browser access is disabled; restart without --no-web or enable web_enabled in config.json"
    );
    let identity = config::identity(data_dir)?;
    let device_id = identity.device_id()?;
    let store = Store::open_readonly(data_dir.join("node.sqlite3"))?;
    let initialized = store.has_network()?;
    let capability = if initialized {
        None
    } else {
        let capability: crate::identity::local::BootstrapCapability =
            serde_json::from_slice(&privatefs::read_private_bounded(
                &data_dir.join("bootstrap.json"),
                4096,
                "the bootstrap capability",
            )?)
            .context("initial bootstrap capability is unavailable")?;
        ensure!(
            capability.capability.len() == 64 && hex::decode(&capability.capability).is_ok(),
            "invalid local bootstrap capability"
        );
        Some(capability.capability)
    };
    if initialized && configuration.local_auto_access {
        let launcher = Launcher::read(data_dir)?;
        let mut url = config::http_origin(&launcher.origin)?;
        ensure!(
            launcher.node_id == device_id,
            "local launcher does not match this node"
        );
        let entry: Value = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()?
            .post(url.join("/api/local/launch")?)
            .bearer_auth(launcher.capability)
            .json(&json!({}))
            .send()
            .await
            .context("local node is unavailable; start it before opening browser access")?
            .error_for_status()?
            .json()
            .await?;
        let token = entry["token"]
            .as_str()
            .filter(|token| token.len() == 64 && hex::decode(token).is_ok())
            .context("invalid local browser entry")?;
        ensure!(
            entry["device_id"] == launcher.node_id && entry["url"] == launcher.origin,
            "local browser entry does not match this node"
        );
        url.set_fragment(Some(&format!("local={token}&node={}", launcher.node_id)));
        return Ok(json!({"url":url.to_string(),"device_id":launcher.node_id,"local":true}));
    }
    let mut url = if capability.is_some() {
        let launcher = Launcher::read(data_dir)?;
        ensure!(
            launcher.node_id == device_id,
            "local launcher does not match this node"
        );
        config::http_origin(&launcher.origin)?
    } else {
        config::http_origin(&configuration.public_url)?
    };
    if url.scheme() == "http" && !is_loopback(&url) {
        ensure!(
            configuration.listen.ip().is_unspecified() || configuration.listen.ip().is_loopback(),
            "use a trusted HTTPS frontend; the configured LAN-only listener has no loopback entry"
        );
        let host = if configuration.listen.is_ipv6() {
            "[::1]"
        } else {
            "127.0.0.1"
        };
        url = config::http_origin(&format!("http://{host}:{}", configuration.listen.port()))?;
    }
    let entry = json!({"url":url.origin().ascii_serialization(),"device_id":device_id,"capability":capability});
    if capability.is_some() {
        url.set_fragment(Some(&format!(
            "bootstrap={}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&entry)?)
        )));
    }
    Ok(
        json!({"url":url.to_string(),"device_id":device_id,"local":false,"capability":capability,"maintenance_socket":info["maintenance_socket"]}),
    )
}

async fn local_setup(data_dir: &Path) -> Result<Value> {
    let info = inspect(data_dir).await?;
    ensure!(
        info["web_enabled"] == true && !Config::load(data_dir)?.forwarding.any,
        "this Node has no local browser setup"
    );
    let launcher = Launcher::read(data_dir)?;
    ensure!(
        info["device_id"] == launcher.node_id,
        "local launcher does not match this node"
    );
    let result: Value = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(format!("{}/api/local/setup", launcher.origin))
        .header(reqwest::header::ORIGIN, &launcher.origin)
        .bearer_auth(launcher.capability)
        .json(&json!({}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let device = if result["kind"] == "uninitialized" {
        &result["invitation"]["device_id"]
    } else {
        &result["entry"]["device_id"]
    };
    ensure!(
        device == &info["device_id"],
        "local setup does not match this node"
    );
    Ok(result)
}

fn is_loopback(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "input file exceeds size limit");
    Ok(bytes)
}

async fn set_local_access(data_dir: &Path, enabled: bool) -> Result<bool> {
    validate_data_dir(data_dir)?;
    match maintenance::set_local_access(data_dir, enabled).await {
        Ok(()) => Ok(true),
        Err(error) if stopped(&error) => {
            let mut configuration = Config::load(data_dir)?;
            configuration.local_auto_access = enabled;
            configuration.save(data_dir)?;
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

async fn join(data_dir: &Path, link: &str, name: &str) -> Result<Value> {
    validate_data_dir(data_dir)?;
    if node_running(data_dir)? {
        let status = maintenance::join(data_dir, link, name).await?;
        return Ok(serde_json::to_value(status)?);
    }
    prepare_start(data_dir)?;
    crate::client::join_network(data_dir, link, name).await?;
    Ok(json!({"status":"joined"}))
}

fn node_running(dir: &Path) -> Result<bool> {
    if !dir.join("identity.json").exists() {
        return Ok(false);
    }
    match config::lock_data_dir(dir) {
        Ok(_lock) => Ok(false),
        Err(error)
            if matches!(
                error.downcast_ref::<fs::TryLockError>(),
                Some(fs::TryLockError::WouldBlock)
            ) =>
        {
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

/// Start owns the data directory and may initialize only an absent or empty one.
pub fn prepare_start(dir: &Path) -> Result<()> {
    validate_data_dir(dir)?;
    let empty = match dir.symlink_metadata() {
        Ok(metadata) => {
            ensure!(
                !metadata.file_type().is_symlink(),
                "data directory must not be a symbolic link"
            );
            fs::read_dir(dir)?.next().is_none()
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
    };
    if empty {
        let mut configuration = Config::local(DEFAULT_LISTEN, vec![], false);
        configuration.network.mesh = MeshConfig::public()?;
        initialize(dir, configuration)?;
    }
    Ok(())
}

fn initialize_from_options(options: InitOptions) -> Result<Value> {
    validate_data_dir(&options.data_dir)?;
    let roots: Vec<_> = options
        .roots
        .iter()
        .map(|root| root.canonicalize().context("resolve project root"))
        .collect::<Result<_>>()?;
    let mut configuration = Config::local(options.listen, roots, options.allow_insecure_lan);
    configuration.local_listen = options.local_listen;
    configuration.tls = options.tls;
    configuration.web_enabled = !options.no_web;
    configuration.local_auto_access = !options.no_local_auto_access;
    configuration.allowed_origins = options.allowed_origins;
    configuration.bins = options.bins.into_iter().collect();
    if let Some(socket) = options.codex_socket {
        configuration.providers.codex_app_server = AppServer {
            mode: AppServerMode::System,
            socket: Some(socket),
        };
    }
    if let Some(provider) = options.manager {
        let workspace = options
            .workspace
            .context("--manager requires --workspace")?;
        configuration.manager = Some(ManagerSettings {
            provider,
            workspace: workspace
                .canonicalize()
                .context("resolve manager workspace")?,
            model: provider.model(options.model.as_deref().unwrap_or_default()),
            reasoning_effort: None,
            backup: Default::default(),
        });
    }
    let mut relays: Vec<net::Relay> = options
        .relays
        .into_iter()
        .map(|url| net::Relay { url, token: None })
        .collect();
    for (url, file) in &options.relay_tokens {
        let named = net::relay_url(url)?;
        let relay = relays
            .iter_mut()
            .find(|relay| net::relay_url(&relay.url).is_ok_and(|url| url == named))
            .context("--relay-token names a relay missing from --relay")?;
        relay.token = Some(net::read_token_file(file)?);
    }
    configuration.network = NetworkConfig {
        relays,
        relay_listen: options.relay_listen,
        relay_url: options.relay_url,
        relay_operator_token: options
            .relay_operator_token
            .as_deref()
            .map(net::read_token_file)
            .transpose()?,
        relay_open: options.relay_open,
        relay_rate: options.relay_rate,
        relay_max_clients: options.relay_max_clients,
        relay_max_clients_per_ip: options.relay_max_clients_per_ip,
        mesh: init_mesh(options.no_mesh, options.mesh_network, options.bootstrap)?,
        ..NetworkConfig::default()
    };
    let forwarding = &mut configuration.forwarding;
    forwarding.any = options.forward_any;
    forwarding.sessions = options.forward_sessions.unwrap_or(forwarding.sessions);
    forwarding.queue = options.forward_queue.unwrap_or(forwarding.queue);
    forwarding.rate = options.forward_rate;
    forwarding.per_ip = options.forward_per_ip;
    forwarding.trusted_proxies = options.trusted_proxies;
    if let Some(url) = options.public_url {
        configuration.public_url = config::http_origin(&url)?
            .to_string()
            .trim_end_matches('/')
            .to_owned();
    } else {
        ensure!(
            configuration.listen.ip().is_loopback(),
            "set --public-url when listening beyond loopback"
        );
    }
    check_ports(
        configuration.listen,
        configuration.local_listen,
        configuration.web_enabled,
    )?;
    let mut provider_env = ProviderEnv::default();
    discovery::complete(
        &mut configuration,
        &mut provider_env,
        &discovery::Host::current(),
    )?;
    let web_enabled = configuration.web_enabled;
    let mesh = &configuration.network.mesh;
    let mesh = json!({"enabled":mesh.enabled,"network":mesh.network,"bootstrap":mesh.bootstrap});
    let open =
        if configuration.listen.ip().is_loopback() || configuration.listen.ip().is_unspecified() {
            format!("http://localhost:{}", configuration.listen.port())
        } else {
            configuration.public_url.clone()
        };
    configuration.provider_env = provider_env;
    initialize(&options.data_dir, configuration)?;
    let data_dir = fs::canonicalize(&options.data_dir)?;
    let device_id = config::identity(&data_dir)?.device_id()?;
    Ok(json!({
        "data_dir": data_dir,
        "web_enabled": web_enabled,
        "open": web_enabled.then_some(open),
        "mesh": mesh,
        "device_id": device_id,
    }))
}

fn initialize(dir: &Path, configuration: Config) -> Result<()> {
    ensure!(
        !dir.join("identity.json").exists() && !dir.join("config.json").exists(),
        "node already initialized; existing identity and configuration were preserved"
    );
    if dir.exists() {
        ensure!(
            fs::read_dir(dir)?.next().is_none(),
            "initialization requires an empty data directory; existing data was preserved"
        );
    }
    configuration.validate()?;
    configuration.network.validate()?;
    let identity = LocalIdentity::generate();
    config::private_dir(dir)?;
    let identity_bytes = zeroize::Zeroizing::new(serde_json::to_vec_pretty(&identity)?);
    create_private(&dir.join("identity.json"), &identity_bytes)?;
    create_private(
        &dir.join("config.json"),
        &serde_json::to_vec_pretty(&configuration)?,
    )?;
    let store = Store::open(dir.join("node.sqlite3"))?;
    create_private(
        &dir.join("bootstrap.json"),
        &serde_json::to_vec(&store.bootstrap_capability()?)?,
    )?;
    if configuration.tls {
        // A self-signed placeholder for first start and tests; a CA-issued pair the host installs
        // over these files is served from the next connection on.
        let url = reqwest::Url::parse(&configuration.public_url)?;
        let host = url.host_str().context("public_url has no host")?;
        let made = rcgen::generate_simple_self_signed(vec![host.trim_matches(['[', ']']).into()])?;
        create_private(&dir.join(config::TLS_CERT), made.cert.pem().as_bytes())?;
        create_private(
            &dir.join(config::TLS_KEY),
            made.signing_key.serialize_pem().as_bytes(),
        )?;
    }
    create_private(&dir.join("data-epoch.json"), &config::data_epoch_marker())?;
    Ok(())
}

fn check_ports(listen: SocketAddr, local_listen: SocketAddr, web_enabled: bool) -> Result<()> {
    for (address, flag) in [
        web_enabled.then_some((listen, "--listen")),
        Some((local_listen, "--local-listen")),
    ]
    .into_iter()
    .flatten()
    {
        std::net::TcpListener::bind(address)
            .with_context(|| format!("端口 {} 被占用，可用 {flag} 更换", address.port()))?;
    }
    Ok(())
}

fn create_private(path: &Path, bytes: &[u8]) -> Result<()> {
    privatefs::create_durable(path, bytes).context("create new private node file")
}

pub fn restart_fresh(dir: &Path) -> Result<()> {
    validate_data_dir(dir)?;
    match start_over(dir) {
        Ok(archive) => tracing::info!(
            event = "node.data_set_aside",
            archive = %archive.display(),
            "removed Node data set aside"
        ),
        Err(error) => tracing::error!(
            event = "node.data_set_aside_failed",
            %error,
            "removed Node data could not be set aside"
        ),
    }
    Err(program::become_program(
        &std::env::current_exe()?,
        std::env::args_os().skip(1),
    ))
    .context("restart the Node")
}

fn start_over(dir: &Path) -> Result<PathBuf> {
    let configuration = Config::load(dir)?;
    let name = dir
        .file_name()
        .context("data directory has no name")?
        .to_string_lossy();
    let stamp = clock::local_stamp();
    let archive = dir.with_file_name(format!("{name}-removed-{stamp}"));
    let fresh = dir.with_file_name(format!(".{name}-fresh-{stamp}"));
    ensure!(
        !archive.exists() && !fresh.exists(),
        "{} already exists",
        archive.display()
    );
    if let Err(error) = initialize(&fresh, configuration) {
        let _ = fs::remove_dir_all(&fresh);
        return Err(error);
    }
    fs::rename(dir, &archive)?;
    if let Err(error) = fs::rename(&fresh, dir) {
        fs::rename(&archive, dir)?;
        return Err(error.into());
    }
    Ok(archive)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("vibeface-{label}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn authoritative_core_version_prefers_running_health_over_inspector_binary() {
        use maintenance::Health;
        let running = "0.0.1-nightly.20261107.1";
        let health = Health {
            device_id: "device-fixture".into(),
            version: running.into(),
            web_enabled: true,
            http_enabled: true,
            write_state: None,
            peers: vec![],
            mesh: None,
        };
        assert_eq!(
            super::authoritative_core_version(Some(&health)),
            running,
            "a health-confirmed daemon must report its own version"
        );
        assert_eq!(
            super::authoritative_core_version(None),
            env!("CARGO_PKG_VERSION"),
            "offline inspection reports the inspecting Core binary"
        );
        assert_ne!(running, env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn inspect_reports_stopped_nodes_with_inspector_core_version() -> Result<()> {
        let dir = temp_dir("inspect-stopped-version");
        config::private_dir(&dir)?;
        let dir = dir.canonicalize()?;
        config::private_write(&dir.join("data-epoch.json"), &config::data_epoch_marker())?;
        let identity = LocalIdentity::generate();
        Config::local("127.0.0.1:4790".parse()?, vec![dir.clone()], false).save(&dir)?;
        config::private_write(&dir.join("identity.json"), &serde_json::to_vec(&identity)?)?;
        let inspected = inspect(&dir).await?;
        assert_eq!(inspected["status"], "stopped");
        assert!(inspected["version"].is_null());
        assert_eq!(inspected["core_version"], env!("CARGO_PKG_VERSION"));
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    fn free_addr() -> SocketAddr {
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
    }

    #[test]
    fn initialization_preserves_nonempty_data_directories() {
        let dir = temp_dir("existing");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("existing-data"), b"original").unwrap();
        let options: InitOptions = serde_json::from_value(json!({
            "data_dir": dir,
            "listen": "127.0.0.1:0",
            "local_listen": "127.0.0.1:0",
            "no_web": true,
            "no_mesh": true,
        }))
        .unwrap();
        assert!(initialize_from_options(options).is_err());
        assert_eq!(fs::read(dir.join("existing-data")).unwrap(), b"original");
        assert!(!dir.join("identity.json").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn control_rejects_relative_and_symlink_data_directories() {
        assert!(validate_data_dir(Path::new("relative-node")).is_err());
        let root = temp_dir("path-boundary");
        fs::create_dir(&root).unwrap();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        let link = root.join("link");
        vibeface_bridge::platform::privatefs::link_dir(&target, &link).unwrap();
        assert!(validate_data_dir(&link).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_nodes_join_the_public_mesh_unless_told_otherwise() {
        let public = MeshConfig::public().unwrap();
        assert!(public.enabled && public.network == mesh::DEFAULT_NETWORK);
        assert_eq!(public.bootstrap, [mesh::default_bootstrap().unwrap()]);
        assert_eq!(init_mesh(false, None, vec![]).unwrap(), public);
        assert!(!init_mesh(true, None, vec![]).unwrap().enabled);
        let own = "vibeface://node/82169062e48d543dc9af8e02340c5248d6fcbe43fceb7d593ab069e7e483c235?addr=127.0.0.1:4792";
        assert_eq!(
            init_mesh(false, None, vec![own.into()]).unwrap().bootstrap,
            [own]
        );
        let lab = init_mesh(false, Some("lab".into()), vec![]).unwrap();
        assert!(
            lab.bootstrap.is_empty(),
            "the default bootstrap node is on the default network only"
        );
        assert!(init_mesh(false, Some("Lab".into()), vec![]).is_err());
        assert!(init_mesh(false, None, vec!["https://not-a-node".into()]).is_err());
        let dir = temp_dir("mesh-default");
        let options: InitOptions = serde_json::from_value(json!({
            "data_dir": dir,
            "no_web": true,
            "listen": free_addr(),
            "local_listen": free_addr(),
        }))
        .unwrap();
        let result = initialize_from_options(options).unwrap();
        assert_eq!(result["mesh"]["enabled"], true);
        assert_eq!(Config::load(&dir).unwrap().network.mesh, public);
        fs::remove_dir_all(dir).unwrap();
    }

    /// `vibeface mesh` on a stopped Node: bootstrap edits land in config.json, checked as
    /// `init` checks them, and the empty table reads back.
    #[tokio::test]
    async fn mesh_bootstrap_edits_reach_the_config_of_a_stopped_node() -> Result<()> {
        let dir = temp_dir("mesh-control");
        let options: InitOptions = serde_json::from_value(json!({
            "data_dir": dir,
            "no_web": true,
            "listen": free_addr(),
            "local_listen": free_addr(),
        }))?;
        initialize_from_options(options)?;
        let device = "82169062e48d543dc9af8e02340c5248d6fcbe43fceb7d593ab069e7e483c235";
        let added = format!("vibeface://node/{device}?addr=127.0.0.1:4792");
        let edit = |add: Vec<&str>, remove: Vec<&str>| {
            mesh_control(&dir, json!({"op":"bootstrap","add":add,"remove":remove}))
        };
        let result = edit(vec![&added], vec![]).await?;
        assert_eq!(result["running"], false);
        let default = mesh::default_bootstrap()?;
        assert_eq!(result["bootstrap"], json!([default, added]));
        assert!(
            edit(vec!["vibeface://node/zz?addr=127.0.0.1:1"], vec![])
                .await
                .is_err()
        );
        assert!(
            edit(vec![], vec![&"ab".repeat(32)]).await.is_err(),
            "not listed"
        );
        let result = edit(vec![], vec![&mesh::default_bootstrap_id()?]).await?;
        assert_eq!(result["bootstrap"], json!([added]));
        assert_eq!(Config::load(&dir)?.network.mesh.bootstrap, [added]);
        let status = mesh_control(&dir, json!({"op":"status"})).await?;
        assert_eq!(
            (&status["records"], &status["neighbors"], &status["own"]),
            (&json!(0), &Value::Null, &Value::Null)
        );
        let records = mesh_control(&dir, json!({"op":"records","role":"gateway"})).await?;
        assert_eq!(records["records"], json!([]));
        assert!(mesh_control(&dir, json!({"op":"other"})).await.is_err());
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn a_public_node_runs_an_open_paced_relay_and_forwards_to_anyone() {
        let dir = temp_dir("public-node");
        let options: InitOptions = serde_json::from_value(json!({
            "data_dir": dir,
            "listen": free_addr(),
            "local_listen": free_addr(),
            "public_url": "https://app.example",
            "tls": true,
            "relay_listen": "127.0.0.1:0",
            "relay_open": true,
            "relay_rate": 65536,
            "relay_max_clients": 512,
            "relay_max_clients_per_ip": 16,
            "forward_any": true,
            "forward_sessions": 32,
            "forward_queue": 192,
            "forward_rate": 262144,
            "forward_per_ip": 8,
            "trusted_proxies": ["203.0.113.1"],
        }))
        .unwrap();
        initialize_from_options(options).unwrap();
        let saved = Config::load(&dir).unwrap();
        assert!(saved.network.mesh.enabled && saved.network.relay_open);
        assert_eq!(saved.network.relay_rate.map(NonZeroU32::get), Some(65536));
        assert_eq!(
            saved.network.relay_max_clients.map(NonZeroUsize::get),
            Some(512)
        );
        assert_eq!(
            saved
                .network
                .relay_max_clients_per_ip
                .map(NonZeroUsize::get),
            Some(16)
        );
        let forwarding = saved.forwarding;
        assert!(forwarding.any && forwarding.sessions == 32 && forwarding.queue == 192);
        assert_eq!(forwarding.rate.map(NonZeroU32::get), Some(262144));
        assert_eq!(forwarding.per_ip.map(NonZeroUsize::get), Some(8));
        assert_eq!(
            forwarding.trusted_proxies,
            ["203.0.113.1".parse::<IpAddr>().unwrap()]
        );
        assert!(saved.tls);
        let certificate = fs::read_to_string(dir.join(config::TLS_CERT)).unwrap();
        assert!(certificate.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(dir.join(config::TLS_KEY).is_file());
        fs::remove_dir_all(dir).unwrap();
        let open: InitOptions = serde_json::from_value(json!({
            "data_dir": temp_dir("open-relay"),
            "no_web": true,
            "listen": free_addr(),
            "local_listen": free_addr(),
            "relay_listen": "127.0.0.1:0",
            "relay_open": true,
        }))
        .unwrap();
        let error = initialize_from_options(open).unwrap_err().to_string();
        assert!(error.contains("open relay"), "{error}");
        let crowded: InitOptions = serde_json::from_value(json!({
            "data_dir": temp_dir("crowded"),
            "no_web": true,
            "listen": free_addr(),
            "local_listen": free_addr(),
            "forward_sessions": 32,
            "forward_queue": 256,
        }))
        .unwrap();
        let error = initialize_from_options(crowded).unwrap_err().to_string();
        assert!(error.contains("browser sockets"), "{error}");
    }

    #[test]
    fn relay_tokens_come_from_files_and_name_a_listed_relay() {
        let dir = temp_dir("relay-token");
        let token = dir.with_extension("token");
        fs::write(&token, "ab".repeat(32)).unwrap();
        let options = |relay_tokens: Value| -> InitOptions {
            serde_json::from_value(json!({
                "data_dir": dir,
                "no_web": true,
                "listen": free_addr(),
                "local_listen": free_addr(),
                "no_mesh": true,
                "relays": ["https://relay.example", "https://other.example"],
                "relay_tokens": relay_tokens,
            }))
            .unwrap()
        };
        let error = initialize_from_options(options(json!([["https://missing.example", token]])))
            .unwrap_err()
            .to_string();
        assert!(error.contains("--relay-token"), "{error}");
        assert!(!dir.exists());
        initialize_from_options(options(json!([["https://relay.example/", token]]))).unwrap();
        let relays = Config::load(&dir).unwrap().network.relays;
        assert_eq!(relays[0].token.as_deref(), Some("ab".repeat(32).as_str()));
        assert_eq!(relays[1].token, None);
        fs::remove_dir_all(dir).unwrap();
        fs::remove_file(token).unwrap();
    }

    #[test]
    fn initialization_selects_system_only_for_an_explicit_codex_socket() {
        for socket in [
            None,
            Some(std::env::temp_dir().join("vibeface-fixture-codex.sock")),
        ] {
            let dir = temp_dir("mode");
            let listen = free_addr();
            let local_listen = loop {
                let candidate = free_addr();
                if candidate != listen {
                    break candidate;
                }
            };
            let options: InitOptions = serde_json::from_value(json!({
                "data_dir": dir,
                "listen": listen,
                "local_listen": local_listen,
                "no_web": true,
                "codex_socket": socket,
                "no_mesh": true,
            }))
            .unwrap();
            initialize_from_options(options).unwrap();
            let saved = Config::load(&dir).unwrap().providers.codex_app_server;
            assert_eq!(
                saved.mode,
                if socket.is_some() {
                    AppServerMode::System
                } else {
                    AppServerMode::Spawn
                }
            );
            assert_eq!(saved.socket, socket);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn private_creation_never_overwrites_existing_file() {
        let dir = temp_dir("private");
        fs::create_dir(&dir).unwrap();
        let path = dir.join("identity.json");
        create_private(&path, b"original").unwrap();
        assert!(create_private(&path, b"replacement").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn relay_validation_happens_before_any_data_file_is_created() {
        let dir = temp_dir("bad-relay");
        let options: InitOptions = serde_json::from_value(json!({
            "data_dir": dir,
            "listen": "127.0.0.1:0",
            "local_listen": "127.0.0.1:0",
            "no_web": true,
            "relays": ["https://relay.test/?token=secret"],
            "no_mesh": true,
        }))
        .unwrap();
        let error = initialize_from_options(options).unwrap_err().to_string();
        assert!(!error.contains("secret"));
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn direct_start_initializes_empty_directories_and_preserves_existing_identity() {
        let root = temp_dir("direct");
        fs::create_dir(&root).unwrap();
        for name in ["missing", "empty"] {
            let dir = root.join(name);
            if name == "empty" {
                fs::create_dir(&dir).unwrap();
            }
            prepare_start(&dir).unwrap();
            let identity = fs::read(dir.join("identity.json")).unwrap();
            let config = fs::read(dir.join("config.json")).unwrap();
            prepare_start(&dir).unwrap();
            assert_eq!(fs::read(dir.join("identity.json")).unwrap(), identity);
            assert_eq!(fs::read(dir.join("config.json")).unwrap(), config);
            assert_eq!(
                Config::load(&dir).unwrap().network.mesh,
                MeshConfig::public().unwrap(),
                "a fresh direct start joins the public mesh"
            );
            let runtime = crate::runtime::Runtime::open(&dir).await.unwrap();
            drop(runtime);
        }
        let nonempty = root.join("nonempty");
        fs::create_dir(&nonempty).unwrap();
        fs::write(nonempty.join("existing-data"), b"preserve me").unwrap();
        prepare_start(&nonempty).unwrap();
        assert!(!nonempty.join("identity.json").exists());
        assert_eq!(
            fs::read(nonempty.join("existing-data")).unwrap(),
            b"preserve me"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn join_initializes_missing_and_empty_directories_only() {
        let root = temp_dir("join");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("empty")).unwrap();
        for name in ["missing", "empty"] {
            let dir = root.join(name);
            let error = join(&dir, "not-an-invite", "test device")
                .await
                .unwrap_err();
            assert!(
                error.downcast_ref::<std::io::Error>().is_none(),
                "{error:#}"
            );
            crate::runtime::Runtime::reopen(&dir).await.unwrap();
        }
        let nonempty = root.join("nonempty");
        fs::create_dir(&nonempty).unwrap();
        fs::write(nonempty.join("existing-data"), b"preserve me").unwrap();
        assert!(
            join(&nonempty, "not-an-invite", "test device")
                .await
                .is_err()
        );
        assert!(!nonempty.join("identity.json").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
