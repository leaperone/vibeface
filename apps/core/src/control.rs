//! Core-owned one-shot operations used by the sibling CLI.

use crate::{
    config::{self, Config, ManagerSettings},
    crypto::{self, LocalIdentity},
    discovery,
    local_access::Launcher,
    maintenance,
    net::{self, NetworkConfig, RelayServiceMode},
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
    #[serde(default)]
    discovery: Vec<String>,
    #[serde(default)]
    relay_mode: Option<RelayServiceMode>,
    relay_listen: Option<SocketAddr>,
    relay_url: Option<String>,
    #[serde(default)]
    public_entry: bool,
    entry: Option<String>,
    entry_relay: Option<String>,
    relay_token_file: Option<PathBuf>,
    manager: Option<Provider>,
    workspace: Option<PathBuf>,
    model: Option<String>,
    #[serde(default)]
    bins: Vec<(Provider, PathBuf)>,
    codex_socket: Option<PathBuf>,
    /// `off`, `public`, or a `vibeface://` discovery URL of a private bootstrap source. A fresh
    /// Node without a private entry defaults to `public`.
    bootstrap: Option<String>,
    /// HTTPS base of a private bootstrap source, for Nodes whose UDP is blocked.
    bootstrap_https: Option<String>,
    /// Paired with `catalog_relays`: the catalogue a discovery-only entry publishes.
    #[serde(default)]
    catalog_gateways: Vec<String>,
    #[serde(default)]
    catalog_relays: Vec<String>,
    /// Grant lifetime the catalogue's issuer hands out, in minutes.
    grant_ttl_minutes: Option<i64>,
    /// How a public entry's relay admits Nodes.
    relay_admission: Option<RelayAdmissionMode>,
    /// The discovery entry whose grants a `grants` relay admits.
    grant_issuer: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum RelayAdmissionMode {
    OperatorToken,
    Grants,
}

fn default_listen() -> SocketAddr {
    DEFAULT_LISTEN
}

/// The bootstrap a fresh Node gets: an explicit choice, else public unless this Node is
/// infrastructure itself or already has a private entry.
fn resolve_bootstrap(
    choice: Option<&str>,
    https: Option<&str>,
    infrastructure: bool,
    private_entry: bool,
) -> Result<net::bootstrap::Bootstrap> {
    use net::bootstrap::Bootstrap;
    let bootstrap = match choice.map(str::trim) {
        None if infrastructure || private_entry => Bootstrap::Off,
        None | Some("public") => Bootstrap::Public,
        Some("off") => Bootstrap::Off,
        Some(discovery) => Bootstrap::Custom {
            discovery: discovery.to_owned(),
            https: https.map(str::to_owned),
        },
    };
    ensure!(
        https.is_none() || matches!(bootstrap, Bootstrap::Custom { .. }),
        "--bootstrap-https belongs to a custom --bootstrap discovery URL"
    );
    bootstrap.validate()?;
    Ok(bootstrap)
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
        Err(error)
            if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                )
            }) =>
        {
            "stopped"
        }
        Err(_) => "unavailable",
    };
    let error = health.as_ref().err().map(|error| format!("{error:#}"));
    Ok(json!({
        "status": status,
        "device_id": device_id,
        "data_dir": data_dir,
        "maintenance_socket": socket,
        "listen": configuration.listen,
        "local_listen": configuration.local_listen,
        "public_url": configuration.public_url,
        "public_entry": configuration.public_entry,
        "web_enabled": health.as_ref().map_or(configuration.web_enabled, |health| health.web_enabled),
        "http_enabled": health.as_ref().is_ok_and(|health| health.http_enabled),
        "version": health.as_ref().ok().map(|health| &health.version),
        "write_state": health.as_ref().ok().and_then(|health| health.write_state),
        "bootstrap": {
            "mode": configuration.network.bootstrap.label(),
            "status": health.as_ref().ok().and_then(|health| health.bootstrap.as_ref()),
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
    if configuration.public_entry {
        return Ok(
            json!({"url":configuration.public_url,"device_id":device_id,"public_entry":true}),
        );
    }
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
        info["web_enabled"] == true && info["public_entry"] == false,
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
        Err(error)
            if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                )
            }) =>
        {
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
        configuration.network.bootstrap = net::bootstrap::Bootstrap::Public;
        initialize(dir, configuration, None)?;
    }
    Ok(())
}

fn initialize_from_options(options: InitOptions) -> Result<Value> {
    validate_data_dir(&options.data_dir)?;
    ensure!(
        options.entry.is_none() || options.relay_token_file.is_some(),
        "--entry requires --relay-token-file"
    );
    ensure!(
        options.entry_relay.is_none() || options.entry.is_some(),
        "--entry-relay requires --entry"
    );
    let roots: Vec<_> = options
        .roots
        .iter()
        .map(|root| root.canonicalize().context("resolve project root"))
        .collect::<Result<_>>()?;
    let mut configuration = Config::local(options.listen, roots, options.allow_insecure_lan);
    configuration.local_listen = options.local_listen;
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
    let relay_mode = options.relay_mode.unwrap_or_default();
    ensure!(
        options.catalog_gateways.len() == options.catalog_relays.len(),
        "--catalog-gateway and --catalog-relay are given in pairs"
    );
    ensure!(
        options.catalog_gateways.is_empty()
            || (options.public_entry && relay_mode == RelayServiceMode::DiscoveryOnly),
        "a catalogue is published by a discovery-only public entry"
    );
    let catalog = (!options.catalog_gateways.is_empty()).then(|| net::bootstrap::CatalogConfig {
        entries: options
            .catalog_gateways
            .iter()
            .zip(&options.catalog_relays)
            .map(|(gateway_url, relay_url)| net::bootstrap::CatalogEntry {
                gateway_url: gateway_url.clone(),
                relay_url: relay_url.clone(),
            })
            .collect(),
        grant_ttl_ms: options
            .grant_ttl_minutes
            .map_or(net::bootstrap::DEFAULT_GRANT_TTL_MS, |minutes| {
                minutes.saturating_mul(60_000)
            }),
    });
    ensure!(
        options.grant_ttl_minutes.is_none() || catalog.is_some(),
        "--grant-ttl-minutes belongs to a published catalogue"
    );
    let relay_admission = match options
        .relay_admission
        .unwrap_or(RelayAdmissionMode::OperatorToken)
    {
        RelayAdmissionMode::OperatorToken => {
            ensure!(
                options.grant_issuer.is_none(),
                "--grant-issuer belongs to --relay-admission grants"
            );
            net::RelayAdmission::OperatorToken
        }
        RelayAdmissionMode::Grants => net::RelayAdmission::Grants {
            issuer: options
                .grant_issuer
                .clone()
                .context("--relay-admission grants requires --grant-issuer")?,
        },
    };
    let bootstrap = resolve_bootstrap(
        options.bootstrap.as_deref(),
        options.bootstrap_https.as_deref(),
        options.public_entry,
        options.entry.is_some(),
    )?;
    let mut discovery = options.discovery;
    // A private entry's Node still registers with the official Discovery, so others find it by
    // device ID. Bootstrap stays off: an official relay grant could become its home relay in
    // place of the entry's.
    if options.entry.is_some()
        && !options.public_entry
        && options.bootstrap.is_none()
        && discovery.is_empty()
    {
        discovery.push(net::bootstrap::PublicDefaults::load()?.discovery_url());
    }
    configuration.network = NetworkConfig {
        relays: options.relays,
        discovery,
        relay_mode,
        relay_listen: options.relay_listen,
        relay_url: options.relay_url,
        bootstrap,
        relay_admission,
        catalog,
        ..NetworkConfig::default()
    };
    configuration.public_entry = options.public_entry;
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
    if let Some(entry) = options.entry {
        let url = config::http_origin(&entry)?
            .to_string()
            .trim_end_matches('/')
            .to_owned();
        configuration.network.entry = Some(crate::net::PublicEntry {
            relay: options.entry_relay.unwrap_or_else(|| url.clone()),
            url,
        });
    }
    let bootstrap_http = configuration.public_entry
        && match relay_mode {
            RelayServiceMode::DiscoveryOnly => configuration.network.catalog.is_some(),
            RelayServiceMode::Forward => configuration.network.relay_admission.is_grants(),
        };
    check_ports(
        configuration.listen,
        configuration.local_listen,
        configuration.web_enabled || bootstrap_http,
        configuration.public_entry,
    )?;
    // A relay admitting by grants holds no operator token for anyone to copy.
    let operator_relay = configuration.public_entry
        && relay_mode == RelayServiceMode::Forward
        && !configuration.network.relay_admission.is_grants();
    let entry_token = match options.relay_token_file {
        Some(path) => Some(net::read_token_file(&path)?),
        None => operator_relay.then(|| crypto::random_hex(32)),
    };
    let mut provider_env = ProviderEnv::default();
    discovery::complete(
        &mut configuration,
        &mut provider_env,
        &discovery::Host::current(),
    )?;
    let public_entry = configuration.public_entry;
    let discovery_only = configuration.network.relay_mode == RelayServiceMode::DiscoveryOnly;
    let web_enabled = configuration.web_enabled;
    let bootstrap = configuration.network.bootstrap.label();
    let open = if !public_entry
        && (configuration.listen.ip().is_loopback() || configuration.listen.ip().is_unspecified())
    {
        format!("http://localhost:{}", configuration.listen.port())
    } else {
        configuration.public_url.clone()
    };
    configuration.provider_env = provider_env;
    initialize(&options.data_dir, configuration, entry_token)?;
    let data_dir = fs::canonicalize(&options.data_dir)?;
    let device_id = config::identity(&data_dir)?.device_id()?;
    Ok(json!({
        "data_dir": data_dir,
        "web_enabled": web_enabled,
        "open": web_enabled.then_some(open),
        "discovery_only": discovery_only,
        "public_entry": public_entry,
        "bootstrap": bootstrap,
        "device_id": device_id,
        "relay_token_file": operator_relay.then(|| options.data_dir.join(net::ENTRY_TOKEN_FILE)),
    }))
}

fn initialize(dir: &Path, configuration: Config, entry_token: Option<String>) -> Result<()> {
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
    if !configuration.public_entry {
        create_private(
            &dir.join("bootstrap.json"),
            &serde_json::to_vec(&store.bootstrap_capability()?)?,
        )?;
    }
    if let Some(token) = entry_token {
        create_private(&dir.join(net::ENTRY_TOKEN_FILE), token.as_bytes())?;
    }
    create_private(&dir.join("data-epoch.json"), &config::data_epoch_marker())?;
    Ok(())
}

fn check_ports(
    listen: SocketAddr,
    local_listen: SocketAddr,
    web_enabled: bool,
    public_entry: bool,
) -> Result<()> {
    for (address, flag) in [
        web_enabled.then_some((listen, "--listen")),
        (!public_entry).then_some((local_listen, "--local-listen")),
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
    let entry_token = configuration
        .network
        .entry
        .is_some()
        .then(|| net::read_token_file(&dir.join(net::ENTRY_TOKEN_FILE)))
        .transpose()?;
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
    if let Err(error) = initialize(&fresh, configuration, entry_token) {
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
            bootstrap: None,
            peers: vec![],
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
        let identity = crypto::LocalIdentity::generate();
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
        let options = InitOptions {
            data_dir: dir.clone(),
            listen: "127.0.0.1:0".parse().unwrap(),
            local_listen: "127.0.0.1:0".parse().unwrap(),
            public_url: None,
            no_web: true,
            allowed_origins: vec![],
            roots: vec![],
            no_local_auto_access: false,
            allow_insecure_lan: false,
            relays: vec![],
            discovery: vec![],
            relay_mode: None,
            relay_listen: None,
            relay_url: None,
            public_entry: false,
            entry: None,
            entry_relay: None,
            relay_token_file: None,
            manager: None,
            workspace: None,
            model: None,
            bins: vec![],
            codex_socket: None,
            bootstrap: Some("off".into()),
            bootstrap_https: None,
            catalog_gateways: vec![],
            catalog_relays: vec![],
            grant_ttl_minutes: None,
            relay_admission: None,
            grant_issuer: None,
        };
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
    fn fresh_nodes_bootstrap_publicly_unless_told_otherwise() {
        use net::bootstrap::Bootstrap;
        let custom = "vibeface://82169062e48d543dc9af8e02340c5248d6fcbe43fceb7d593ab069e7e483c235@127.0.0.1:4792";
        assert_eq!(
            resolve_bootstrap(None, None, false, false).unwrap(),
            Bootstrap::Public
        );
        assert_eq!(
            resolve_bootstrap(None, None, true, false).unwrap(),
            Bootstrap::Off,
            "public entries are the infrastructure"
        );
        assert_eq!(
            resolve_bootstrap(None, None, false, true).unwrap(),
            Bootstrap::Off,
            "a private entry is not silently paired with the public one"
        );
        assert_eq!(
            resolve_bootstrap(Some("off"), None, false, false).unwrap(),
            Bootstrap::Off
        );
        assert_eq!(
            resolve_bootstrap(Some("public"), None, false, true).unwrap(),
            Bootstrap::Public
        );
        assert_eq!(
            resolve_bootstrap(
                Some(custom),
                Some("http://127.0.0.1:4800/bootstrap"),
                false,
                false
            )
            .unwrap(),
            Bootstrap::Custom {
                discovery: custom.into(),
                https: Some("http://127.0.0.1:4800/bootstrap".into())
            }
        );
        assert!(
            resolve_bootstrap(Some("public"), Some("https://x.example"), false, false).is_err()
        );
        assert!(resolve_bootstrap(Some("https://not-a-discovery"), None, false, false).is_err());
        let dir = temp_dir("bootstrap-default");
        let options: InitOptions = serde_json::from_value(json!({
            "data_dir": dir,
            "no_web": true,
            "listen": free_addr(),
            "local_listen": free_addr(),
        }))
        .unwrap();
        let result = initialize_from_options(options).unwrap();
        assert_eq!(result["bootstrap"], "public");
        assert_eq!(
            Config::load(&dir).unwrap().network.bootstrap,
            Bootstrap::Public
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn grant_admitting_entries_hold_no_operator_token() {
        let dir = temp_dir("grants-entry");
        let listen = free_addr();
        let relay_listen = loop {
            let candidate = free_addr();
            if candidate != listen {
                break candidate;
            }
        };
        let options: InitOptions = serde_json::from_value(json!({
            "data_dir": dir,
            "no_web": true,
            "listen": listen,
            "public_url": "https://entry.example",
            "public_entry": true,
            "relay_listen": relay_listen,
            "relay_url": "https://entry.example",
            "relay_admission": "grants",
            "grant_issuer": "82169062e48d543dc9af8e02340c5248d6fcbe43fceb7d593ab069e7e483c235",
        }))
        .unwrap();
        let result = initialize_from_options(options).unwrap();
        assert_eq!(result["bootstrap"], "off");
        assert!(result["relay_token_file"].is_null());
        assert!(!dir.join(net::ENTRY_TOKEN_FILE).exists());
        let saved = Config::load(&dir).unwrap();
        assert!(saved.network.relay_admission.is_grants());
        assert!(saved.network.bootstrap.is_off());
        fs::remove_dir_all(dir).unwrap();
        let missing: InitOptions = serde_json::from_value(json!({
            "data_dir": temp_dir("grants-missing"),
            "public_url": "https://entry.example",
            "public_entry": true,
            "relay_listen": "127.0.0.1:0",
            "relay_admission": "grants",
        }))
        .unwrap();
        assert!(
            initialize_from_options(missing)
                .unwrap_err()
                .to_string()
                .contains("grant-issuer")
        );
    }

    #[test]
    fn initialization_rejects_an_entry_without_an_explicit_relay_token_file() {
        let dir = temp_dir("entry-token");
        let options: InitOptions = serde_json::from_value(json!({
            "data_dir": dir,
            "entry": "https://entry.example"
        }))
        .unwrap();
        let error = initialize_from_options(options).unwrap_err().to_string();
        assert!(error.contains("relay-token-file"), "{error}");
    }

    #[test]
    fn a_private_entry_node_registers_with_the_official_discovery_without_its_bootstrap() {
        let official = net::bootstrap::PublicDefaults::load()
            .unwrap()
            .discovery_url();
        let own = "vibeface://82169062e48d543dc9af8e02340c5248d6fcbe43fceb7d593ab069e7e483c235@127.0.0.1:4792";
        for (extra, discovery, bootstrap) in [
            (json!({}), vec![official.clone()], "off"),
            (json!({"discovery": [own]}), vec![own.to_owned()], "off"),
            (json!({"bootstrap": "off"}), vec![], "off"),
            (json!({"bootstrap": "public"}), vec![], "public"),
        ] {
            let dir = temp_dir("private-entry");
            let token = dir.with_extension("token");
            fs::write(&token, "ab".repeat(32)).unwrap();
            let mut options = json!({
                "data_dir": dir,
                "no_web": true,
                "listen": free_addr(),
                "local_listen": free_addr(),
                "entry": "https://entry.example",
                "relay_token_file": token,
            });
            options
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let result = initialize_from_options(serde_json::from_value(options).unwrap()).unwrap();
            assert_eq!(result["bootstrap"], bootstrap, "{extra}");
            let saved = Config::load(&dir).unwrap();
            assert_eq!(saved.network.discovery, discovery, "{extra}");
            assert_eq!(saved.network.bootstrap.label(), bootstrap, "{extra}");
            fs::remove_dir_all(dir).unwrap();
            fs::remove_file(token).unwrap();
        }
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
            let options = InitOptions {
                data_dir: dir.clone(),
                listen,
                local_listen,
                public_url: None,
                no_web: true,
                allowed_origins: vec![],
                roots: vec![],
                no_local_auto_access: false,
                allow_insecure_lan: false,
                relays: vec![],
                discovery: vec![],
                relay_mode: None,
                relay_listen: None,
                relay_url: None,
                public_entry: false,
                entry: None,
                entry_relay: None,
                relay_token_file: None,
                manager: None,
                workspace: None,
                model: None,
                bins: vec![],
                codex_socket: socket.clone(),
                bootstrap: Some("off".into()),
                bootstrap_https: None,
                catalog_gateways: vec![],
                catalog_relays: vec![],
                grant_ttl_minutes: None,
                relay_admission: None,
                grant_issuer: None,
            };
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
        let options = InitOptions {
            data_dir: dir.clone(),
            listen: "127.0.0.1:0".parse().unwrap(),
            local_listen: "127.0.0.1:0".parse().unwrap(),
            public_url: None,
            no_web: true,
            allowed_origins: vec![],
            roots: vec![],
            no_local_auto_access: false,
            allow_insecure_lan: false,
            relays: vec!["https://relay.test/?token=secret".into()],
            discovery: vec![],
            relay_mode: None,
            relay_listen: None,
            relay_url: None,
            public_entry: false,
            entry: None,
            entry_relay: None,
            relay_token_file: None,
            manager: None,
            workspace: None,
            model: None,
            bins: vec![],
            codex_socket: None,
            bootstrap: Some("off".into()),
            bootstrap_https: None,
            catalog_gateways: vec![],
            catalog_relays: vec![],
            grant_ttl_minutes: None,
            relay_admission: None,
            grant_issuer: None,
        };
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
                Config::load(&dir).unwrap().network.bootstrap,
                net::bootstrap::Bootstrap::Public,
                "a fresh direct start bootstraps from the public infrastructure"
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
