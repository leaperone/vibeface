//! Core-owned one-shot operations used by the sibling CLI.

use crate::{config::{self, Config, ManagerSettings}, crypto::LocalIdentity, maintenance, net::{self, NetworkConfig}, providers::{AppServer, AppServerMode, Provider}, store::Store};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fs::{self, OpenOptions}, io::Write, net::{IpAddr, Ipv4Addr, SocketAddr}, path::{Path, PathBuf}, time::Duration};

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
    relay_mode: Option<crate::net::RelayServiceMode>,
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
}

fn default_listen() -> SocketAddr { DEFAULT_LISTEN }
fn default_local_listen() -> SocketAddr { config::DEFAULT_LOCAL_LISTEN }

pub async fn handle(request: vibeface_control::ControlRequest) -> Result<Value> {
    match request {
        vibeface_control::ControlRequest::Init { options } => {
            let options: InitOptions = serde_json::from_value(options).context("invalid init options")?;
            initialize_from_options(options)
        }
        vibeface_control::ControlRequest::Inspect { data_dir } => inspect(&data_dir).await,
        vibeface_control::ControlRequest::Access { data_dir, json: _ } => access(&data_dir).await,
        vibeface_control::ControlRequest::SetLocalAccess { data_dir, enabled } => {
            set_local_access(&data_dir, enabled).await.map(|running| json!({"running":running}))
        }
        vibeface_control::ControlRequest::Join { data_dir, link, name } => join(&data_dir, &link, &name).await,
        vibeface_control::ControlRequest::Wallet { data_dir: _, operation: _ } => {
            bail!("wallet control is handled by the authenticated maintenance socket")
        }
    }
}

pub async fn inspect(data_dir: &Path) -> Result<Value> {
    let data_dir = fs::canonicalize(data_dir)
        .context("initialized data directory is required; run vibeface init first")?;
    let configuration = Config::load(&data_dir)?;
    let identity = config::identity(&data_dir)?;
    let device_id = identity.device_id()?;
    let socket = maintenance::socket_path(&data_dir)?;
    let health = maintenance::probe(&data_dir).await.and_then(|health| {
        ensure!(health.device_id == device_id, "running node identity does not match this data directory");
        Ok(health)
    });
    let status = match &health {
        Ok(_) => "running",
        Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|error| matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)) => "stopped",
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
        "version": health.as_ref().ok().map(|health| &health.version),
        "write_state": health.as_ref().ok().and_then(|health| health.write_state),
        "peers": health.as_ref().ok().map(|health| &health.peers),
        "core_version": env!("CARGO_PKG_VERSION"),
        "error": error,
    }))
}

async fn access(data_dir: &Path) -> Result<Value> {
    let info = inspect(data_dir).await?;
    ensure!(info["web_enabled"] == true, "browser access is disabled; restart without --no-web or enable web_enabled in config.json");
    let config = Config::load(data_dir)?;
    let device_id = config::identity(data_dir)?.device_id()?;
    if config.public_entry {
        return Ok(json!({"url":config.public_url,"device_id":device_id,"public_entry":true}));
    }
    let mut url = config.public_url.clone();
    if config.listen.ip().is_loopback() || config.listen.ip().is_unspecified() {
        url = format!("http://localhost:{}", config.listen.port());
    }
    Ok(json!({"url":url,"device_id":device_id,"local":false,"maintenance_socket":info["maintenance_socket"]}))
}

async fn set_local_access(data_dir: &Path, enabled: bool) -> Result<bool> {
    match maintenance::set_local_access(data_dir, enabled).await {
        Ok(()) => Ok(true),
        Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|error| matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)) => {
            let mut configuration = Config::load(data_dir)?;
            configuration.local_auto_access = enabled;
            configuration.save(data_dir)?;
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

async fn join(data_dir: &Path, link: &str, name: &str) -> Result<Value> {
    if node_running(data_dir)? {
        let status = maintenance::join(data_dir, link, name).await?;
        return Ok(serde_json::to_value(status)?);
    }
    prepare_start(data_dir)?;
    crate::client::join_network(data_dir, link, name).await?;
    Ok(json!({"status":"joined"}))
}

fn node_running(dir: &Path) -> Result<bool> {
    if !dir.join("identity.json").exists() { return Ok(false); }
    match config::lock_data_dir(dir) {
        Ok(_lock) => Ok(false),
        Err(error) if matches!(error.downcast_ref::<fs::TryLockError>(), Some(fs::TryLockError::WouldBlock)) => Ok(true),
        Err(error) => Err(error),
    }
}

/// Start owns the data directory and may initialize only an absent or empty one.
pub fn prepare_start(dir: &Path) -> Result<()> {
    let empty = match dir.symlink_metadata() {
        Ok(metadata) => {
            ensure!(!metadata.file_type().is_symlink(), "data directory must not be a symbolic link");
            fs::read_dir(dir)?.next().is_none()
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
    };
    if empty {
        let configuration = Config::local(DEFAULT_LISTEN, vec![], false);
        initialize(dir, configuration)?;
    }
    Ok(())
}

fn initialize_from_options(options: InitOptions) -> Result<Value> {
    let roots: Vec<_> = options.roots.iter().map(|root| root.canonicalize().context("resolve project root")).collect::<Result<_>>()?;
    let mut configuration = Config::local(options.listen, roots, options.allow_insecure_lan);
    configuration.local_listen = options.local_listen;
    configuration.web_enabled = !options.no_web;
    configuration.local_auto_access = !options.no_local_auto_access;
    configuration.allowed_origins = options.allowed_origins;
    configuration.bins = options.bins.into_iter().collect();
    if let Some(socket) = options.codex_socket {
        configuration.providers.codex_app_server = AppServer { mode: AppServerMode::System, socket: Some(socket) };
    }
    if let Some(provider) = options.manager {
        let workspace = options.workspace.context("--manager requires --workspace")?;
        configuration.manager = Some(ManagerSettings {
            provider,
            workspace: workspace.canonicalize().context("resolve manager workspace")?,
            model: options.model.unwrap_or_default(),
            reasoning_effort: None,
            backup: Default::default(),
        });
    }
    configuration.network = NetworkConfig {
        relays: options.relays,
        discovery: options.discovery,
        relay_mode: options.relay_mode.unwrap_or_default(),
        relay_listen: options.relay_listen,
        relay_url: options.relay_url,
        ..NetworkConfig::default()
    };
    configuration.public_entry = options.public_entry;
    if let Some(url) = options.public_url { configuration.public_url = config::http_origin(&url)?.to_string().trim_end_matches('/').to_owned(); }
    else { ensure!(configuration.listen.ip().is_loopback(), "set --public-url when listening beyond loopback"); }
    if let Some(entry) = options.entry {
        let url = config::http_origin(&entry)?.to_string().trim_end_matches('/').to_owned();
        configuration.network.entry = Some(crate::net::PublicEntry { relay: options.entry_relay.unwrap_or_else(|| url.clone()), url });
    }
    initialize(&options.data_dir, configuration)?;
    Ok(json!({"data_dir":fs::canonicalize(options.data_dir)?}))
}

fn initialize(dir: &Path, configuration: Config) -> Result<()> {
    ensure!(!dir.join("identity.json").exists() && !dir.join("config.json").exists(), "node already initialized; existing identity and configuration were preserved");
    if dir.exists() { ensure!(fs::read_dir(dir)?.next().is_none(), "initialization requires an empty data directory; existing data was preserved"); }
    configuration.validate()?;
    configuration.network.validate()?;
    let identity = LocalIdentity::generate();
    config::private_dir(dir)?;
    create_private(&dir.join("identity.json"), &serde_json::to_vec_pretty(&identity)?)?;
    create_private(&dir.join("config.json"), &serde_json::to_vec_pretty(&configuration)?)?;
    let store = Store::open(dir.join("node.sqlite3"))?;
    if !configuration.public_entry {
        create_private(&dir.join("bootstrap.json"), &serde_json::to_vec(&store.bootstrap_capability()?)?)?;
    }
    create_private(&dir.join("data-epoch.json"), &config::data_epoch_marker())?;
    Ok(())
}

fn create_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    let mut file = options.open(path).context("create new private node file")?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if let Some(parent) = path.parent() { fs::File::open(parent)?.sync_all()?; }
    Ok(())
}

pub fn restart_fresh(dir: &Path) -> Result<()> {
    let configuration = Config::load(dir)?;
    let name = dir.file_name().context("data directory has no name")?.to_string_lossy();
    let stamp = crate::runtime::now_ms();
    let archive = dir.with_file_name(format!("{name}-removed-{stamp}"));
    let fresh = dir.with_file_name(format!(".{name}-fresh-{stamp}"));
    ensure!(!archive.exists() && !fresh.exists(), "{} already exists", archive.display());
    initialize(&fresh, configuration)?;
    fs::rename(dir, &archive)?;
    if let Err(error) = fs::rename(&fresh, dir) { fs::rename(&archive, dir)?; return Err(error.into()); }
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(crate::sibling_cli()).args(std::env::args_os().skip(1)).exec();
    Err(error).context("restart the Node")
}
