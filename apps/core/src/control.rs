//! Core-owned one-shot operations used by the sibling CLI.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crate::{
    config::{self, Config, ManagerSettings},
    crypto::{self, LocalIdentity},
    discovery, maintenance,
    local_access::Launcher,
    net::{self, NetworkConfig, PublicEntry, RelayServiceMode},
    providers::{AppServer, AppServerMode, Provider, ProviderEnv},
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

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
}

fn default_listen() -> SocketAddr {
    DEFAULT_LISTEN
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
        vibeface_control::ControlRequest::Access { data_dir, json: _ } => access(&data_dir).await,
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

fn wallet(data_dir: &Path, operation: Value) -> Result<Value> {
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

pub async fn inspect(data_dir: &Path) -> Result<Value> {
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
        "version": health.as_ref().ok().map(|health| &health.version),
        "write_state": health.as_ref().ok().and_then(|health| health.write_state),
        "peers": health.as_ref().ok().map(|health| &health.peers),
        "core_version": env!("CARGO_PKG_VERSION"),
        "error": error,
    }))
}

async fn access(data_dir: &Path) -> Result<Value> {
    let info = inspect(data_dir).await?;
    ensure!(
        info["web_enabled"] == true,
        "browser access is disabled; restart without --no-web or enable web_enabled in config.json"
    );
    let config = Config::load(data_dir)?;
    let device_id = config::identity(data_dir)?.device_id()?;
    if config.public_entry {
        return Ok(json!({"url":config.public_url,"device_id":device_id,"public_entry":true}));
    }
    let mut url = config.public_url.clone();
    if config.listen.ip().is_loopback() || config.listen.ip().is_unspecified() {
        url = format!("http://localhost:{}", config.listen.port());
    }
    Ok(
        json!({"url":url,"device_id":device_id,"local":false,"maintenance_socket":info["maintenance_socket"]}),
    )
}

async fn set_local_access(data_dir: &Path, enabled: bool) -> Result<bool> {
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
        let configuration = Config::local(DEFAULT_LISTEN, vec![], false);
        initialize(dir, configuration, None)?;
    }
    Ok(())
}

fn initialize_from_options(options: InitOptions) -> Result<Value> {
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
    configuration.network = NetworkConfig {
        relays: options.relays,
        discovery: options.discovery,
        relay_mode,
        relay_listen: options.relay_listen,
        relay_url: options.relay_url,
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
    check_ports(
        configuration.listen,
        configuration.local_listen,
        configuration.web_enabled,
        configuration.public_entry,
    )?;
    let entry_token = match options.relay_token_file {
        Some(path) => Some(net::read_token_file(&path)?),
        None => (configuration.public_entry && relay_mode == RelayServiceMode::Forward)
            .then(|| crypto::random_hex(32)),
    };
    let mut provider_env = ProviderEnv::default();
    discovery::complete(
        &mut configuration,
        &mut provider_env,
        &discovery::Host::current(),
    )?;
    initialize(&options.data_dir, configuration, entry_token)?;
    if provider_env != ProviderEnv::default() {
        provider_env.save(&options.data_dir)?;
    }
    Ok(json!({"data_dir":fs::canonicalize(options.data_dir)?}))
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
    create_private(
        &dir.join("identity.json"),
        &serde_json::to_vec_pretty(&identity)?,
    )?;
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
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).context("create new private node file")?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

pub fn restart_fresh(dir: &Path) -> Result<()> {
    let configuration = Config::load(dir)?;
    let name = dir
        .file_name()
        .context("data directory has no name")?
        .to_string_lossy();
    let stamp = crate::runtime::now_ms();
    let archive = dir.with_file_name(format!("{name}-removed-{stamp}"));
    let fresh = dir.with_file_name(format!(".{name}-fresh-{stamp}"));
    ensure!(
        !archive.exists() && !fresh.exists(),
        "{} already exists",
        archive.display()
    );
    initialize(&fresh, configuration, None)?;
    fs::rename(dir, &archive)?;
    if let Err(error) = fs::rename(&fresh, dir) {
        fs::rename(&archive, dir)?;
        return Err(error.into());
    }
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(crate::sibling_cli())
        .args(std::env::args_os().skip(1))
        .exec();
    Err(error).context("restart the Node")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("vibeface-{label}-{}", uuid::Uuid::new_v4()))
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
        };
        assert!(initialize_from_options(options).is_err());
        assert_eq!(fs::read(dir.join("existing-data")).unwrap(), b"original");
        assert!(!dir.join("identity.json").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn initialization_selects_system_only_for_an_explicit_codex_socket() {
        for socket in [
            None,
            Some(PathBuf::from("/tmp/vibeface-fixture-codex.sock")),
        ] {
            let dir = temp_dir("mode");
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
                codex_socket: socket.clone(),
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
            crate::runtime::Runtime::open(&dir).await.unwrap();
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
