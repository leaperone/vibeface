use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Args, Parser, Subcommand, ValueEnum};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use vibeface::{
    api,
    config::{self, Config, ManagerSettings},
    crypto::{LocalIdentity, digest},
    identity::local::BootstrapCapability,
    maintenance,
    network::NetworkConfig,
    runtime::Runtime,
    update::{self, Update},
};

mod service;

const DEFAULT_LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4790);

#[derive(Parser)]
#[command(
    version,
    about = "Personal device network for local coding agents. Run without a command to start this node.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Data directory for direct startup. Subcommands accept their own --data-dir.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    /// Create a private local identity and configuration.
    Init(Box<Init>),
    /// Start this node with its configured roles.
    Serve(DataDir),
    /// Check this local node through its private maintenance socket.
    Status(Access),
    /// Manage this user's installed node service and update timer.
    Service(service::Service),
    /// Display this node's trusted entry for account setup or enrollment.
    Access(Access),
    /// Select a release channel, check, or apply a managed update.
    Update(Update),
    /// Call a node tool from a locally managed SDK process.
    Tool {
        name: String,
        #[arg(long)]
        json: String,
    },
}

#[derive(Args)]
struct DataDir {
    #[arg(long, default_value_os_t = config::default_dir())]
    data_dir: PathBuf,
}

#[derive(Clone, Copy, ValueEnum)]
enum Manager {
    #[value(name = "cursor_sdk")]
    CursorSdk,
    #[value(name = "codex_sdk")]
    CodexSdk,
}

impl Manager {
    fn provider(self) -> &'static str {
        match self {
            Self::CursorSdk => "cursor_sdk",
            Self::CodexSdk => "codex_sdk",
        }
    }
}

#[derive(Args)]
struct Init {
    #[command(flatten)]
    data: DataDir,
    #[arg(long, default_value_t = DEFAULT_LISTEN)]
    listen: SocketAddr,
    #[arg(long)]
    public_url: Option<String>,
    /// Additional browser origins allowed to reach this node. May be repeated.
    #[arg(long = "allowed-origin")]
    allowed_origins: Vec<String>,
    /// Existing directories that executors may access. May be repeated.
    #[arg(long = "root")]
    roots: Vec<PathBuf>,
    #[arg(long)]
    allow_insecure_lan: bool,
    #[arg(long, value_enum)]
    manager: Option<Manager>,
    #[arg(long, requires = "manager")]
    model: Option<String>,
    /// Known discovery or relay WebSocket endpoints. May be repeated.
    #[arg(long = "bootstrap-url")]
    bootstrap_urls: Vec<String>,
    /// Reachable WebSocket addresses published for this Node. May be repeated.
    #[arg(long = "advertise-url")]
    advertise_urls: Vec<String>,
    #[arg(long)]
    mdns: bool,
    /// Offer bounded encrypted forwarding to other personal networks.
    #[arg(long)]
    public_relay: bool,
}

#[derive(Args)]
struct Access {
    #[command(flatten)]
    data: DataDir,
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let command = match cli.command {
        Some(command) => command,
        None => {
            let data_dir = cli.data_dir.unwrap_or_else(config::default_dir);
            prepare_start(&data_dir)?;
            Action::Serve(DataDir { data_dir })
        }
    };
    match command {
        Action::Init(options) => init(*options),
        Action::Serve(options) => {
            let runtime = Arc::new(Runtime::open(&options.data_dir).await?);
            runtime.start().await?;
            eprintln!(
                "Web: {}\nRun vibeface access with the same --data-dir for the trusted browser entry.",
                runtime.config.public_url
            );
            tokio::select! {
                result = api::serve(runtime.clone()) => result,
                result = maintenance::serve(runtime, options.data_dir) => result,
            }
        }
        Action::Access(options) => access(options),
        Action::Status(options) => status(options).await,
        Action::Service(options) => service::run(options).await,
        Action::Update(options) => update::run(options).await,
        Action::Tool { name, json } => call_tool(name, json).await,
    }
}

fn prepare_start(dir: &Path) -> Result<()> {
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
        initialize(dir, Config::local(DEFAULT_LISTEN, vec![], false))?;
    }
    Ok(())
}

async fn status(options: Access) -> Result<()> {
    let data = fs::canonicalize(&options.data.data_dir)
        .context("initialized data directory is required; run vibeface init first")?;
    let configuration = Config::load(&data)?;
    let node_id = config::identity(&data)?.node_id()?;
    let health = maintenance::probe(&data).await.and_then(|health| {
        ensure!(
            health.node_id == node_id,
            "running node identity does not match this data directory"
        );
        Ok(health)
    });
    let state = match &health {
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
    if options.json {
        println!(
            "{}",
            json!({
                "status":state, "node_id":node_id, "data_dir":data,
                "listen":configuration.listen, "roles":configuration.roles,
                "version":health.as_ref().ok().map(|health| &health.version),
                "cli_version":env!("CARGO_PKG_VERSION"), "error":error,
            })
        );
    } else {
        println!(
            "Node: {node_id}\nStatus: {state}\nListen: {}\nData: {}",
            configuration.listen,
            data.display()
        );
        if let Ok(health) = &health {
            println!("Version: {}", health.version);
        }
        if let Some(error) = error {
            println!("Detail: {error}");
        }
    }
    health
        .map(|_| ())
        .context("node is not reachable through its private maintenance socket")
}

fn init(options: Init) -> Result<()> {
    let dir = &options.data.data_dir;
    let roots: Vec<_> = options
        .roots
        .iter()
        .map(|root| root.canonicalize().context("resolve execution root"))
        .collect::<Result<_>>()?;
    let mut configuration = Config::local(options.listen, roots, options.allow_insecure_lan);
    if let Some(url) = options.public_url {
        configuration.public_url = origin(&url)?.to_string().trim_end_matches('/').to_owned();
    }
    configuration.allowed_origins = options.allowed_origins;
    configuration.network = NetworkConfig {
        bootstrap_urls: options
            .bootstrap_urls
            .iter()
            .map(|value| network_url(value))
            .collect::<Result<_>>()?,
        advertise_urls: options
            .advertise_urls
            .iter()
            .map(|value| network_url(value))
            .collect::<Result<_>>()?,
        mdns: options.mdns,
        relay_enabled: options.public_relay,
    };
    if let Some(manager) = options.manager {
        configuration.manager = Some(ManagerSettings {
            provider: manager.provider().to_owned(),
            model: options.model.unwrap_or_else(|| {
                if matches!(manager, Manager::CursorSdk) {
                    "auto".into()
                } else {
                    String::new()
                }
            }),
        });
    }
    initialize(dir, configuration)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Initialization {
    target: PathBuf,
    format: Value,
    configuration: Config,
}

fn initialize(dir: &Path, configuration: Config) -> Result<()> {
    ensure!(
        !dir.symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink()),
        "data directory must not be a symbolic link"
    );
    if dir.exists() {
        ensure!(
            fs::read_dir(dir)?.next().is_none(),
            "initialization requires an empty data directory; existing data was preserved"
        );
    }
    configuration.validate()?;
    let absolute = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()?.join(dir)
    };
    let parent = absolute
        .parent()
        .context("data directory requires a parent")?;
    fs::create_dir_all(parent)?;
    let target = parent.canonicalize()?.join(
        absolute
            .file_name()
            .context("data directory requires a name")?,
    );
    let staging = target.with_file_name(format!(
        ".vibeface-initialize-{}",
        &digest(target.as_os_str().as_encoded_bytes())[..24]
    ));
    if !staging.exists() {
        config::private_dir(&staging)?;
    }
    config::verify_private_dir(&staging)?;
    for entry in fs::read_dir(&staging)? {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().context("unrecognized initialization file")?;
        let published = matches!(
            name,
            "initializing.json"
                | "data-epoch.json"
                | "identity.json"
                | "config.json"
                | "bootstrap.json"
                | "node.sqlite3"
                | "node.sqlite3-wal"
                | "node.sqlite3-shm"
                | "node.sqlite3-journal"
                | "node.lock"
        );
        let pending = [
            "initializing.json",
            "data-epoch.json",
            "identity.json",
            "config.json",
            "bootstrap.json",
        ]
        .iter()
        .any(|file| name == format!(".{file}.initializing.tmp"));
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            (published || pending)
                && metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == config::effective_uid()
                && metadata.permissions().mode() & 0o077 == 0,
            "unrecognized initialization staging data was preserved"
        );
    }
    let manifest = staging.join("initializing.json");
    if manifest.exists() {
        let initialization: Initialization =
            serde_json::from_slice(&read_bounded(&manifest, 65_536)?)?;
        ensure!(
            initialization.target == target && initialization.format == config::data_format(),
            "initialization format or target mismatch"
        );
    } else {
        ensure!(
            fs::read_dir(&staging)?.all(|entry| entry.is_ok_and(|entry| matches!(
                entry.file_name().to_str(),
                Some("node.lock" | ".initializing.json.initializing.tmp")
            ))),
            "unrecognized initialization staging data was preserved"
        );
    }
    let _lock = config::lock_data_dir(&staging)?;
    for name in [
        "initializing.json",
        "data-epoch.json",
        "identity.json",
        "config.json",
        "bootstrap.json",
    ] {
        let pending = staging.join(format!(".{name}.initializing.tmp"));
        match fs::symlink_metadata(&pending) {
            Ok(metadata) => {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                ensure!(
                    metadata.is_file()
                        && !metadata.file_type().is_symlink()
                        && metadata.uid() == config::effective_uid()
                        && metadata.permissions().mode() & 0o077 == 0,
                    "invalid unfinished initialization file was preserved"
                );
                fs::remove_file(pending)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let manifest = staging.join("initializing.json");
    if !manifest.exists() {
        ensure!(
            fs::read_dir(&staging)?
                .all(|entry| entry.is_ok_and(|entry| entry.file_name() == "node.lock")),
            "unrecognized initialization staging data was preserved"
        );
        create_private(
            &manifest,
            &serde_json::to_vec(&Initialization {
                target: target.clone(),
                format: config::data_format(),
                configuration,
            })?,
        )?;
    }
    for entry in fs::read_dir(&staging)? {
        let entry = entry?;
        let name = entry.file_name();
        ensure!(
            matches!(
                name.to_str(),
                Some(
                    "initializing.json"
                        | "data-epoch.json"
                        | "identity.json"
                        | "config.json"
                        | "bootstrap.json"
                        | "node.sqlite3"
                        | "node.sqlite3-wal"
                        | "node.sqlite3-shm"
                        | "node.sqlite3-journal"
                        | "node.lock"
                )
            ) && entry.file_type()?.is_file()
                && !entry.file_type()?.is_symlink(),
            "unrecognized initialization staging data was preserved"
        );
    }
    let initialization: Initialization = serde_json::from_slice(&read_bounded(&manifest, 65_536)?)?;
    ensure!(
        initialization.target == target && initialization.format == config::data_format(),
        "initialization format or target mismatch"
    );
    let configuration = initialization.configuration;
    configuration.validate()?;
    let marker = staging.join("data-epoch.json");
    if !marker.exists() {
        create_private(&marker, &serde_json::to_vec(&config::data_format())?)?;
    }
    config::verify_format_marker(&staging)?;
    let identity_path = staging.join("identity.json");
    if !identity_path.exists() {
        create_private(
            &identity_path,
            &serde_json::to_vec_pretty(&LocalIdentity::generate())?,
        )?;
    }
    let identity = config::identity(&staging)?;
    let configuration_path = staging.join("config.json");
    if !configuration_path.exists() {
        create_private(
            &configuration_path,
            &serde_json::to_vec_pretty(&configuration)?,
        )?;
    }
    let actual = Config::load(&staging)?;
    ensure!(
        serde_json::to_value(&actual)? == serde_json::to_value(&configuration)?,
        "initialization configuration changed"
    );
    {
        let store = vibeface::store::Store::initialize(&staging.join("node.sqlite3"))?;
        let bootstrap_path = staging.join("bootstrap.json");
        if !bootstrap_path.exists() {
            create_private(
                &bootstrap_path,
                &serde_json::to_vec(&BootstrapCapability::generate())?,
            )?;
        }
        let capability: BootstrapCapability =
            serde_json::from_slice(&read_bounded(&bootstrap_path, 4096)?)?;
        store.install_bootstrap_capability(&capability)?;
    }
    fs::File::open(&staging)?.sync_all()?;
    if target.exists() {
        ensure!(
            fs::read_dir(&target)?.next().is_none(),
            "data directory changed during initialization; existing data was preserved"
        );
    }
    fs::rename(&staging, &target)?;
    fs::File::open(parent)?.sync_all()?;
    fs::remove_file(target.join("initializing.json"))?;
    fs::File::open(&target)?.sync_all()?;
    println!("Initialized node {}", identity.node_id()?);
    println!("Configuration: {}", dir.join("config.json").display());
    Ok(())
}

fn create_private(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(
        matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
        "private node file already exists"
    );
    let parent = path
        .parent()
        .context("private node file requires a directory")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid private node filename")?;
    let pending = parent.join(format!(".{name}.initializing.tmp"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&pending)
        .context("create new private node file")?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::hard_link(&pending, path).context("publish new private node file without replacement")?;
    fs::remove_file(pending)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn origin(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("invalid node URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "node URL must be an HTTP(S) origin without credentials, a path, query, or fragment"
    );
    Ok(url)
}

fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

fn network_url(value: &str) -> Result<String> {
    ensure!(value.len() <= 2048, "network URL is too long");
    let mut url = Url::parse(value).context("invalid network URL")?;
    ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && matches!(url.path(), "/" | "/api/network/ws"),
        "network URL must identify the Node WebSocket endpoint without credentials"
    );
    let scheme = match url.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        _ => bail!("unsupported network URL scheme"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid network URL scheme"))?;
    url.set_path("/api/network/ws");
    Ok(url.to_string())
}

fn access(options: Access) -> Result<()> {
    let dir = &options.data.data_dir;
    config::verify_data_format(dir)?;
    let configuration = Config::load(dir)?;
    let identity = config::identity(dir)?;
    let store = vibeface::store::Store::open_read_only(&dir.join("node.sqlite3"))?;
    let capability = if !store.trusted_policy_pins().is_empty() {
        None
    } else {
        let capability: BootstrapCapability =
            serde_json::from_slice(&read_bounded(&dir.join("bootstrap.json"), 4096)?)
                .context("initial bootstrap capability is unavailable")?;
        ensure!(
            capability.capability.len() == 64 && hex::decode(&capability.capability).is_ok(),
            "invalid local bootstrap capability"
        );
        Some(capability.capability)
    };
    let mut url = origin(&configuration.public_url)?;
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
        url = origin(&format!("http://{host}:{}", configuration.listen.port()))?;
    }
    let entry = json!({"url":url.origin().ascii_serialization(),"node_id":identity.node_id()?,"node_public_key":identity.public_key_hex()?,"relay":false,"claim_nonce":capability,"capability":capability});
    if options.json {
        println!("{}", serde_json::to_string(&entry)?);
    } else {
        if capability.is_some() {
            url.set_fragment(Some(&format!(
                "bootstrap={}",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&entry)?)
            )));
        }
        println!("{url}");
    }
    Ok(())
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "input file exceeds size limit");
    Ok(bytes)
}

async fn call_tool(name: String, arguments: String) -> Result<()> {
    ensure!(!name.is_empty() && name.len() <= 128, "invalid tool name");
    let arguments: Value =
        serde_json::from_str(&arguments).context("invalid tool arguments JSON")?;
    ensure!(
        arguments.is_object(),
        "tool arguments must be a JSON object"
    );
    let url =
        origin(&std::env::var("VIBEFACE_NODE_URL").context("VIBEFACE_NODE_URL is required")?)?;
    ensure!(
        is_loopback(&url),
        "SDK tools must connect to a loopback node URL"
    );
    let token = std::env::var("VIBEFACE_TOOL_TOKEN").context("VIBEFACE_TOOL_TOKEN is required")?;
    ensure!(!token.is_empty(), "VIBEFACE_TOOL_TOKEN is empty");
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(url.join("api/tools")?)
        .bearer_auth(token)
        .json(&json!({"name": name, "arguments": arguments, "call_id": uuid::Uuid::new_v4().to_string()}))
        .send()
        .await
        .context("reach local node tool API")?;
    ensure!(
        response.status().is_success(),
        "node tool rejected with HTTP {}",
        response.status()
    );
    let result: Value = serde_json::from_slice(&bounded_body(response).await?)
        .context("invalid node tool response")?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > 1024 * 1024 {
            bail!("node response exceeds 1 MiB");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_direct_start_keeps_data_directory_arguments_unambiguous() {
        let default = Cli::try_parse_from(["vibeface"]).unwrap();
        assert!(default.command.is_none() && default.data_dir.is_none());
        let direct = Cli::try_parse_from(["vibeface", "--data-dir", "/private/node data"]).unwrap();
        assert!(direct.command.is_none());
        assert_eq!(direct.data_dir.unwrap(), Path::new("/private/node data"));
        let explicit =
            Cli::try_parse_from(["vibeface", "serve", "--data-dir", "/private/node data"]).unwrap();
        let Some(Action::Serve(options)) = explicit.command else {
            panic!("expected explicit serve")
        };
        assert!(explicit.data_dir.is_none());
        assert_eq!(options.data_dir, Path::new("/private/node data"));
        for arguments in [
            vec!["vibeface", "--data-dir", "/one", "serve"],
            vec![
                "vibeface",
                "--data-dir",
                "/one",
                "serve",
                "--data-dir",
                "/two",
            ],
            vec!["vibeface", "--unknown"],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
    }

    #[tokio::test]
    async fn direct_start_initializes_empty_directories_and_preserves_existing_identity() {
        let root = std::env::temp_dir().join(format!("vibeface-direct-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        for name in ["missing", "empty"] {
            let dir = root.join(name);
            if name == "empty" {
                fs::create_dir(&dir).unwrap();
            }
            prepare_start(&dir).unwrap();
            let mut configuration = Config::load(&dir).unwrap();
            assert_eq!(configuration.listen, "127.0.0.1:4790".parse().unwrap());
            assert_eq!(configuration.public_url, "http://127.0.0.1:4790");
            assert!(configuration.roots.is_empty() && configuration.manager.is_none());
            assert!(!configuration.allow_insecure_lan && !configuration.network.mdns);
            assert!(configuration.network.bootstrap_urls.is_empty());
            assert!(configuration.network.advertise_urls.is_empty());
            assert!(!configuration.network.relay_enabled);
            configuration.timeout_secs = 1234;
            configuration.save(&dir).unwrap();
            let identity = fs::read(dir.join("identity.json")).unwrap();
            let config = fs::read(dir.join("config.json")).unwrap();
            let node_id = config::identity(&dir).unwrap().node_id().unwrap();
            prepare_start(&dir).unwrap();
            assert_eq!(fs::read(dir.join("identity.json")).unwrap(), identity);
            assert_eq!(fs::read(dir.join("config.json")).unwrap(), config);
            let runtime = Runtime::open(&dir).await.unwrap();
            assert_eq!(runtime.identity.node_id().unwrap(), node_id);
            assert_eq!(runtime.config.timeout_secs, 1234);
            drop(runtime);
            fs::write(dir.join("config.json"), b"invalid configuration").unwrap();
            prepare_start(&dir).unwrap();
            assert!(Runtime::open(&dir).await.is_err());
            assert_eq!(fs::read(dir.join("identity.json")).unwrap(), identity);
            assert_eq!(
                fs::read(dir.join("config.json")).unwrap(),
                b"invalid configuration"
            );
        }
        let uninitialized = root.join("uninitialized");
        fs::create_dir(&uninitialized).unwrap();
        fs::write(uninitialized.join("existing-data"), b"preserve me").unwrap();
        prepare_start(&uninitialized).unwrap();
        assert!(Runtime::open(&uninitialized).await.is_err());
        assert_eq!(
            fs::read(uninitialized.join("existing-data")).unwrap(),
            b"preserve me"
        );
        assert!(!uninitialized.join("identity.json").exists());
        let file = root.join("file");
        fs::write(&file, b"preserve file").unwrap();
        assert!(prepare_start(&file).is_err());
        #[cfg(unix)]
        for target in [root.join("empty"), root.join("absent")] {
            let link = root.join("link");
            std::os::unix::fs::symlink(target, &link).unwrap();
            assert!(prepare_start(&link).is_err());
            fs::remove_file(link).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cli_requires_one_invitation_and_supports_accountless_flags() {
        assert!(Cli::try_parse_from(["vibeface", "pair"]).is_err());
        assert!(
            Cli::try_parse_from([
                "vibeface",
                "pair",
                "--invite",
                "abc",
                "--invite-file",
                "invite.json"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "vibeface",
                "pair",
                "--invite",
                "abc",
                "--name",
                "Build Node"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["vibeface", "access", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["vibeface", "status", "--json"]).is_ok());
        for action in [
            "install",
            "start",
            "stop",
            "restart",
            "status",
            "uninstall",
            "install-updater",
            "uninstall-updater",
        ] {
            assert!(
                Cli::try_parse_from([
                    "vibeface",
                    "service",
                    action,
                    "--data-dir",
                    "/private/node data"
                ])
                .is_ok()
            );
        }
        assert!(Cli::try_parse_from(["vibeface", "service", "unknown"]).is_err());
        assert!(
            Cli::try_parse_from([
                "vibeface",
                "init",
                "--bootstrap-url",
                "wss://relay.test/api/network/ws",
                "--advertise-url",
                "wss://node.test/api/network/ws",
                "--mdns",
                "--public-relay"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["vibeface", "init", "--manager", "cursor_sdk"]).is_ok());
        assert!(Cli::try_parse_from(["vibeface", "tool", "dispatch", "--json", "{}"]).is_ok());
    }

    #[test]
    fn node_url_rejects_paths_and_embedded_credentials() {
        for value in [
            "http://user:password@localhost:4790",
            "http://localhost/api",
            "http://localhost/?token=secret",
            "file:///tmp/socket",
        ] {
            assert!(origin(value).is_err());
        }
        assert!(is_loopback(&origin("http://127.0.0.1:4790").unwrap()));
        assert!(is_loopback(&origin("http://[::1]:4790").unwrap()));
        assert!(!is_loopback(&origin("http://192.168.1.5:4790").unwrap()));
    }

    #[test]
    fn network_urls_normalize_without_accepting_credentials() {
        assert_eq!(
            network_url("https://node.test").unwrap(),
            "wss://node.test/api/network/ws"
        );
        assert_eq!(
            network_url("ws://127.0.0.1:4790/api/network/ws").unwrap(),
            "ws://127.0.0.1:4790/api/network/ws"
        );
        for value in [
            "file:///tmp/node",
            "https://user:secret@node.test",
            "https://node.test/?token=secret",
            "https://node.test/other",
        ] {
            assert!(network_url(value).is_err());
        }
    }

    #[test]
    fn initialization_preserves_nonempty_data_directories() {
        let dir =
            std::env::temp_dir().join(format!("vibeface-cli-existing-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("existing-data"), b"original").unwrap();
        let parsed =
            Cli::try_parse_from(["vibeface", "init", "--data-dir", dir.to_str().unwrap()]).unwrap();
        let Some(Action::Init(options)) = parsed.command else {
            panic!("expected init")
        };
        assert!(init(*options).is_err());
        assert_eq!(fs::read(dir.join("existing-data")).unwrap(), b"original");
        assert!(!dir.join("identity.json").exists());
        assert!(!dir.join("config.json").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn interrupted_initialization_resumes_each_durable_stage_without_replacing_keys() -> Result<()>
    {
        let root =
            std::env::temp_dir().join(format!("vibeface-init-resume-{}", uuid::Uuid::new_v4()));
        config::private_dir(&root)?;
        for stage in 0..8 {
            let target = root.canonicalize()?.join(format!("node-{stage}"));
            let staging = target.with_file_name(format!(
                ".vibeface-initialize-{}",
                &digest(target.as_os_str().as_encoded_bytes())[..24]
            ));
            config::private_dir(&staging)?;
            let configuration = Config::local(DEFAULT_LISTEN, vec![], false);
            let identity = LocalIdentity::generate();
            let capability = BootstrapCapability::generate();
            let identity_bytes = serde_json::to_vec_pretty(&identity)?;
            let capability_bytes = serde_json::to_vec(&capability)?;
            if stage >= 1 {
                create_private(
                    &staging.join("initializing.json"),
                    &serde_json::to_vec(&Initialization {
                        target: target.clone(),
                        format: config::data_format(),
                        configuration: configuration.clone(),
                    })?,
                )?;
            }
            if stage >= 2 {
                create_private(
                    &staging.join("data-epoch.json"),
                    &serde_json::to_vec(&config::data_format())?,
                )?;
            }
            if stage >= 3 {
                create_private(&staging.join("identity.json"), &identity_bytes)?;
            }
            if stage >= 4 {
                create_private(
                    &staging.join("config.json"),
                    &serde_json::to_vec_pretty(&configuration)?,
                )?;
            }
            if stage >= 5 {
                drop(vibeface::store::Store::open(staging.join("node.sqlite3"))?);
            }
            if stage >= 6 {
                create_private(&staging.join("bootstrap.json"), &capability_bytes)?;
            }
            if stage >= 7 {
                let store = vibeface::store::Store::open(staging.join("node.sqlite3"))?;
                store.install_bootstrap_capability(&capability)?;
            }
            let interrupted_name = match stage {
                0 => "initializing.json",
                1 => "data-epoch.json",
                2 => "identity.json",
                3 => "config.json",
                _ => "bootstrap.json",
            };
            let unfinished = staging.join(format!(".{interrupted_name}.initializing.tmp"));
            if staging.join(interrupted_name).exists() {
                fs::hard_link(staging.join(interrupted_name), &unfinished)?;
            } else {
                use std::os::unix::fs::OpenOptionsExt;
                let mut file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&unfinished)?;
                file.write_all(b"{incomplete")?;
                file.sync_all()?;
            }
            initialize(&target, configuration)?;
            config::verify_data_format(&target)?;
            assert!(!staging.exists());
            assert!(!target.join("identity.sqlite3").exists());
            if stage >= 3 {
                assert_eq!(fs::read(target.join("identity.json"))?, identity_bytes);
            }
            if stage >= 6 {
                assert_eq!(fs::read(target.join("bootstrap.json"))?, capability_bytes);
            }
            let store = vibeface::store::Store::open(target.join("node.sqlite3"))?;
            let actual: BootstrapCapability =
                serde_json::from_slice(&fs::read(target.join("bootstrap.json"))?)?;
            store.verify_bootstrap_capability(&actual.capability)?;
        }
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn initialization_refuses_public_staging_before_persisting_secrets() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("vibeface-init-public-{}", uuid::Uuid::new_v4()));
        config::private_dir(&root)?;
        let target = root.canonicalize()?.join("node");
        let staging = target.with_file_name(format!(
            ".vibeface-initialize-{}",
            &digest(target.as_os_str().as_encoded_bytes())[..24]
        ));
        fs::create_dir(&staging)?;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))?;
        assert!(initialize(&target, Config::local(DEFAULT_LISTEN, vec![], false)).is_err());
        assert!(!target.exists());
        assert!(fs::read_dir(&staging)?.next().is_none());
        assert_eq!(fs::metadata(&staging)?.permissions().mode() & 0o777, 0o755);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn private_creation_never_overwrites_existing_file() {
        let dir = std::env::temp_dir().join(format!("vibeface-cli-{}", uuid::Uuid::new_v4()));
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
}
