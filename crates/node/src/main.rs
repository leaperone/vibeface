use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Args, Parser, Subcommand, ValueEnum};
use reqwest::Url;
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
    crypto::LocalIdentity,
    identity::local::{BootstrapCapability, random_hex},
    local_access::Launcher,
    maintenance,
    model::NodeRole,
    net::{self, NetworkConfig, PublicEntry},
    runtime::Runtime,
    store::Store,
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
    Serve(Serve),
    /// Check this local node through its private maintenance socket.
    Status(Access),
    /// Manage this user's installed node service and update timer.
    Service(service::Service),
    /// Display this node's local browser entry.
    Access(Access),
    /// Select a release channel, check, or apply a managed update.
    Update(Update),
    /// Join a personal network with an invite link from a management device.
    Join(Join),
    /// Call a node tool from a locally managed SDK process.
    Tool {
        name: String,
        #[arg(long)]
        json: String,
        /// Stable reference for a write intent; reuse it after a timeout or partial result.
        #[arg(long)]
        call_ref: Option<String>,
    },
}

#[derive(Args)]
struct DataDir {
    #[arg(long, default_value_os_t = config::default_dir())]
    data_dir: PathBuf,
}

#[derive(Args)]
struct Serve {
    #[command(flatten)]
    data: DataDir,
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
    /// Loopback address of this device's own browser session.
    #[arg(long, default_value_t = config::DEFAULT_LOCAL_LISTEN)]
    local_listen: SocketAddr,
    #[arg(long)]
    public_url: Option<String>,
    /// Additional browser origins allowed to reach this node. May be repeated.
    #[arg(long = "allowed-origin")]
    allowed_origins: Vec<String>,
    /// Existing project directories available to this device's Agent. May be repeated.
    #[arg(long = "root")]
    roots: Vec<PathBuf>,
    /// Require ordinary device pairing for local browser access.
    #[arg(long)]
    no_local_auto_access: bool,
    #[arg(long)]
    allow_insecure_lan: bool,
    #[arg(long, value_enum, requires = "workspace")]
    manager: Option<Manager>,
    #[arg(long, requires = "manager")]
    workspace: Option<PathBuf>,
    #[arg(long, requires = "manager")]
    model: Option<String>,
    #[arg(long, default_value = "cursor-sdk-bridge")]
    cursor_sdk_bin: PathBuf,
    #[arg(long, default_value = "node")]
    node_bin: PathBuf,
    #[arg(long)]
    codex_sdk_script: Option<PathBuf>,
    #[arg(long)]
    codex_socket: Option<PathBuf>,
    #[arg(long, default_value = "codex")]
    codex_bin: PathBuf,
    /// iroh relay of another Node in the personal network, e.g. `https://relay.example`. May be repeated.
    #[arg(long = "relay", value_name = "URL")]
    relays: Vec<String>,
    /// Run an iroh relay for the personal network on this address.
    #[arg(long, value_name = "ADDR")]
    relay_listen: Option<SocketAddr>,
    /// The relay's URL as other Nodes reach it; defaults to `http://<relay-listen>`.
    #[arg(long, value_name = "URL", requires = "relay_listen")]
    relay_url: Option<String>,
    /// Reach other Nodes only through relays, as behind a strict NAT.
    #[arg(long)]
    relay_paths_only: bool,
    /// Serve only the web app, a relay and ciphertext forwarding for other people's Nodes;
    /// this Node never joins a personal network.
    #[arg(long, requires = "relay_listen", conflicts_with_all = ["manager", "roots", "entry"])]
    public_entry: bool,
    /// Public entry that browsers use to reach this Node, e.g. `https://vb.example`.
    #[arg(long, value_name = "URL", requires = "relay_token_file")]
    entry: Option<String>,
    /// The public entry's relay when it is not on the entry's own origin.
    #[arg(long, value_name = "URL", requires = "entry")]
    entry_relay: Option<String>,
    /// File holding the public entry's operator token (`entry-token` in its data directory).
    #[arg(long, value_name = "PATH", requires = "entry")]
    relay_token_file: Option<PathBuf>,
}

#[derive(Args)]
struct Access {
    #[command(flatten)]
    data: DataDir,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct Join {
    /// Invite link, `https://<gateway>/join#vibeface1…`.
    link: String,
    #[command(flatten)]
    data: DataDir,
    /// Device name shown to the network; defaults to this machine's host name.
    #[arg(long)]
    name: Option<String>,
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
            if !data_dir.join("config.json").exists() {
                check_ports(DEFAULT_LISTEN, config::DEFAULT_LOCAL_LISTEN)?;
            }
            prepare_start(&data_dir)?;
            Action::Serve(Serve {
                data: DataDir { data_dir },
            })
        }
    };
    match command {
        Action::Init(options) => {
            check_ports(options.listen, options.local_listen)?;
            init(*options)
        }
        Action::Serve(options) => {
            let runtime = Arc::new(Runtime::open(&options.data.data_dir).await?);
            runtime.start().await?;
            eprintln!("Web: {}", runtime.config.public_url);
            if !runtime.config.public_entry
                && (runtime.config.listen.ip().is_loopback()
                    || runtime.config.listen.ip().is_unspecified())
            {
                eprintln!(
                    "Local setup: http://localhost:{}",
                    runtime.config.listen.port()
                );
            }
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            let result = tokio::select! {
                result = api::serve(runtime.clone()) => result,
                result = maintenance::serve(runtime.clone(), options.data.data_dir) => result,
                _ = terminate.recv() => Ok(()),
                _ = tokio::signal::ctrl_c() => Ok(()),
            };
            // The App Server group must not outlive the Node that manages it.
            runtime.shutdown().await;
            result
        }
        Action::Access(options) => access(options).await,
        Action::Status(options) => status(options).await,
        Action::Service(options) => service::run(options).await,
        Action::Update(options) => update::run(options).await,
        Action::Join(options) => join(options).await,
        Action::Tool {
            name,
            json,
            call_ref,
        } => call_tool(name, json, call_ref).await,
    }
}

/// Fails before any file is written, so a busy port never leaves a half-made data directory.
fn check_ports(listen: SocketAddr, local_listen: SocketAddr) -> Result<()> {
    for (address, flag) in [(listen, "--listen"), (local_listen, "--local-listen")] {
        std::net::TcpListener::bind(address)
            .with_context(|| format!("端口 {} 被占用，可用 {flag} 更换", address.port()))?;
    }
    Ok(())
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
        initialize(dir, Config::local(DEFAULT_LISTEN, vec![], false), None)?;
    }
    Ok(())
}

async fn status(options: Access) -> Result<()> {
    let data = fs::canonicalize(&options.data.data_dir)
        .context("initialized data directory is required; run vibeface init first")?;
    let configuration = Config::load(&data)?;
    let device_id = config::identity(&data)?.device_id()?;
    let health = maintenance::probe(&data).await.and_then(|health| {
        ensure!(
            health.device_id == device_id,
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
                "status":state, "device_id":device_id, "data_dir":data,
                "listen":configuration.listen, "roles":configuration.roles,
                "version":health.as_ref().ok().map(|health| &health.version),
                "cli_version":env!("CARGO_PKG_VERSION"), "error":error,
            })
        );
    } else {
        println!(
            "Peer ID: {device_id}\nStatus: {state}\nListen: {}\nData: {}",
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
        .map(|root| root.canonicalize().context("resolve project root"))
        .collect::<Result<_>>()?;
    let mut configuration = Config::local(options.listen, roots, options.allow_insecure_lan);
    configuration.local_listen = options.local_listen;
    configuration.local_auto_access = !options.no_local_auto_access;
    configuration.codex_socket = options.codex_socket;
    configuration.codex_bin = options.codex_bin;
    if let Some(url) = options.public_url {
        configuration.public_url = origin(&url)?.to_string().trim_end_matches('/').to_owned();
    }
    configuration.allowed_origins = options.allowed_origins;
    let entry = match options.entry {
        Some(url) => {
            let url = origin(&url)?.to_string().trim_end_matches('/').to_owned();
            Some(PublicEntry {
                relay: options.entry_relay.unwrap_or_else(|| url.clone()),
                url,
            })
        }
        None => None,
    };
    configuration.network = NetworkConfig {
        relays: options.relays,
        relay_listen: options.relay_listen,
        relay_url: options.relay_url,
        relay_paths_only: options.relay_paths_only,
        entry,
        ..NetworkConfig::default()
    };
    configuration.public_entry = options.public_entry;
    if options.public_entry {
        configuration.roles = vec![NodeRole::Relay];
    }
    let entry_token = match &options.relay_token_file {
        Some(path) => Some(net::read_token_file(path)?),
        None => options.public_entry.then(|| random_hex(32)),
    };
    if let Some(manager) = options.manager {
        let workspace = options
            .workspace
            .context("--manager requires --workspace")?;
        configuration.manager = Some(ManagerSettings {
            provider: manager.provider().to_owned(),
            workspace: workspace
                .canonicalize()
                .context("resolve manager workspace")?,
            model: options.model.unwrap_or_else(|| {
                if matches!(manager, Manager::CursorSdk) {
                    "auto".into()
                } else {
                    String::new()
                }
            }),
            cursor_sdk_bin: options.cursor_sdk_bin,
            node_bin: options.node_bin,
            codex_sdk_script: match options.codex_sdk_script {
                Some(path) => path,
                None => std::env::current_exe()?
                    .parent()
                    .context("node executable has no parent directory")?
                    .join("sidecars/codex-manager.mjs"),
            },
        });
    }
    initialize(dir, configuration, entry_token)
}

fn initialize(dir: &Path, configuration: Config, entry_token: Option<String>) -> Result<()> {
    ensure!(
        !dir.join("identity.json").exists() && !dir.join("config.json").exists(),
        "node already initialized; existing identity and configuration were preserved"
    );
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
    configuration.network.validate()?;
    let identity = LocalIdentity::generate();
    let encoded_identity = serde_json::to_vec_pretty(&identity)?;
    let encoded_configuration = serde_json::to_vec_pretty(&configuration)?;
    config::private_dir(dir)?;
    create_private(&dir.join("identity.json"), &encoded_identity)?;
    create_private(&dir.join("config.json"), &encoded_configuration)?;
    let store = Store::open(dir.join("node.sqlite3"))?;
    if !configuration.public_entry {
        let capability = store.bootstrap_capability()?;
        create_private(
            &dir.join("bootstrap.json"),
            &serde_json::to_vec(&capability)?,
        )?;
    }
    if let Some(token) = &entry_token {
        create_private(&dir.join(net::ENTRY_TOKEN_FILE), token.as_bytes())?;
    }
    create_private(&dir.join("data-epoch.json"), br#"{"data_epoch":7}"#)?;
    println!("Initialized device with Peer ID {}", identity.device_id()?);
    println!("Configuration: {}", dir.join("config.json").display());
    if configuration.public_entry {
        println!(
            "Relay token: {} (copy it to your Node and pass --relay-token-file)",
            dir.join(net::ENTRY_TOKEN_FILE).display()
        );
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

async fn access(options: Access) -> Result<()> {
    let dir = &options.data.data_dir;
    let epoch: Value = serde_json::from_slice(&read_bounded(&dir.join("data-epoch.json"), 4096)?)?;
    ensure!(
        epoch == json!({"data_epoch":7}),
        "data directory uses another format; existing data was preserved"
    );
    let configuration = Config::load(dir)?;
    let identity = config::identity(dir)?;
    if configuration.public_entry {
        if options.json {
            println!(
                "{}",
                json!({"url":configuration.public_url,"device_id":identity.device_id()?,"public_entry":true})
            );
        } else {
            println!("{}", configuration.public_url);
        }
        return Ok(());
    }
    let store = Store::open_readonly(dir.join("node.sqlite3"))?;
    let initialized = store.has_network()?;
    let capability = if initialized {
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
    if initialized && configuration.local_auto_access {
        let path = dir.join("local-access.json");
        let metadata = fs::symlink_metadata(&path)
            .context("start the node before opening local browser access")?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "local launcher credential must be a private regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                metadata.permissions().mode() & 0o077 == 0,
                "local launcher credential must be private"
            );
        }
        let launcher: Launcher = serde_json::from_slice(&read_bounded(&path, 4096)?)?;
        let mut url = origin(&launcher.origin)?;
        ensure!(
            url.scheme() == "http"
                && url.host_str() == Some("127.0.0.1")
                && launcher.node_id == identity.device_id()?
                && launcher.capability.len() == 64
                && hex::decode(&launcher.capability).is_ok(),
            "local launcher does not match this node"
        );
        let entry: Value = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
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
        if options.json {
            println!(
                "{}",
                json!({"url":url.to_string(),"device_id":launcher.node_id,"local":true})
            );
        } else {
            println!("{url}");
        }
        return Ok(());
    }
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
    let entry = json!({"url":url.origin().ascii_serialization(),"device_id":identity.device_id()?,"capability":capability});
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

async fn join(options: Join) -> Result<()> {
    let name = options.name.unwrap_or_else(config::hostname);
    let state =
        vibeface::client::join_network(&options.data.data_dir, &options.link, &name).await?;
    println!(
        "已加入个人网络 {}。启动 Node 后即可连接。",
        state.genesis.network_id
    );
    Ok(())
}

async fn call_tool(name: String, arguments: String, call_ref: Option<String>) -> Result<()> {
    ensure!(!name.is_empty() && name.len() <= 128, "invalid tool name");
    let mut arguments: Value =
        serde_json::from_str(&arguments).context("invalid tool arguments JSON")?;
    ensure!(
        arguments.is_object(),
        "tool arguments must be a JSON object"
    );
    let write = name == "dispatch_work";
    let argument_ref = arguments["call_ref"].as_str();
    ensure!(
        call_ref
            .as_deref()
            .zip(argument_ref)
            .is_none_or(|(flag, argument)| flag == argument),
        "--call-ref must match arguments.call_ref"
    );
    let call_ref = call_ref.or_else(|| argument_ref.map(String::from));
    ensure!(
        !write || call_ref.is_some(),
        "dispatch_work requires --call-ref; keep it and query call_lookup after any uncertain result"
    );
    if let Some(call_ref) = &call_ref {
        arguments["call_ref"] = json!(call_ref);
    }
    let call_id = call_ref.unwrap_or_default();
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
        .json(&json!({"name": name, "arguments": arguments}))
        .send()
        .await
        .with_context(|| format!("reach local node tool API; preserve call_ref {call_id} and query call_lookup before retrying"))?;
    ensure!(
        response.status().is_success(),
        "node tool rejected with HTTP {}; preserve call_ref {}",
        response.status(),
        call_id
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
        assert_eq!(options.data.data_dir, Path::new("/private/node data"));
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
            vec!["vibeface", "--bootstrap-url", "https://node.test", "serve"],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
    }

    #[test]
    fn relay_flags_are_saved_and_invalid_relays_leave_no_directory() -> Result<()> {
        let root = std::env::temp_dir().join(format!("vibeface-relays-{}", uuid::Uuid::new_v4()));
        let dir = root.join("node");
        let cli = Cli::try_parse_from([
            "vibeface",
            "init",
            "--data-dir",
            dir.to_str().context("temporary path is invalid")?,
            "--relay",
            "https://relay.test",
            "--relay-listen",
            "127.0.0.1:0",
            "--relay-paths-only",
        ])?;
        let Some(Action::Init(options)) = cli.command else {
            bail!("expected init command")
        };
        init(*options)?;
        let network = Config::load(&dir)?.network;
        assert_eq!(network.relays, ["https://relay.test"]);
        assert_eq!(network.relay_listen, Some("127.0.0.1:0".parse()?));
        assert!(network.relay_paths_only);
        for url in [
            "file:///tmp/node",
            "https://fixture-user:fixture-password@relay.test",
            "https://relay.test/?token=fixture-query",
        ] {
            let bad = root.join("bad");
            let cli = Cli::try_parse_from([
                "vibeface",
                "init",
                "--data-dir",
                bad.to_str().context("temporary path is invalid")?,
                "--relay",
                url,
            ])?;
            let Some(Action::Init(options)) = cli.command else {
                bail!("expected init command")
            };
            let error = format!("{:#}", init(*options).unwrap_err());
            for secret in ["fixture-user", "fixture-password", "fixture-query"] {
                assert!(!error.contains(secret));
            }
            assert!(!bad.exists());
        }
        assert!(
            Cli::try_parse_from(["vibeface", "init", "--relay-url", "https://relay.test"]).is_err()
        );
        fs::remove_dir_all(root)?;
        Ok(())
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
            assert!(!configuration.allow_insecure_lan);
            assert!(configuration.network.relays.is_empty());
            assert!(
                configuration.network.relay_listen.is_none() && !configuration.network.n0_relays
            );
            configuration.timeout_secs = 1234;
            configuration.save(&dir).unwrap();
            let identity = fs::read(dir.join("identity.json")).unwrap();
            let config = fs::read(dir.join("config.json")).unwrap();
            let device_id = config::identity(&dir).unwrap().device_id().unwrap();
            prepare_start(&dir).unwrap();
            assert_eq!(fs::read(dir.join("identity.json")).unwrap(), identity);
            assert_eq!(fs::read(dir.join("config.json")).unwrap(), config);
            let runtime = Runtime::open(&dir).await.unwrap();
            assert_eq!(runtime.identity.device_id().unwrap(), device_id);
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
    fn cli_requires_one_invite_link_and_supports_accountless_flags() {
        assert!(Cli::try_parse_from(["vibeface", "join"]).is_err());
        assert!(Cli::try_parse_from(["vibeface", "pair", "--invite", "abc"]).is_err());
        assert!(
            Cli::try_parse_from([
                "vibeface",
                "join",
                "https://node.example/join#vibeface1abc",
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
                "--relay",
                "https://relay.test",
                "--relay-listen",
                "0.0.0.0:3340",
                "--relay-url",
                "https://relay.test"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "vibeface",
                "init",
                "--manager",
                "cursor_sdk",
                "--workspace",
                "."
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["vibeface", "tool", "dispatch_work", "--json", "{}"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "vibeface",
                "codex",
                "thread/read",
                "--json",
                "{}",
                "--socket",
                "/tmp/codex.sock",
                "--watch"
            ])
            .is_err(),
            "no CLI holds the native Socket or answers native requests for an Agent"
        );
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
