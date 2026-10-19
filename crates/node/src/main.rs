use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
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
    client::Client,
    config::{self, Config, ManagerSettings},
    crypto::{LocalIdentity, digest},
    identity::{
        ClusterState, IdentityKind, Member,
        local::{BootstrapCapability, InviteEnvelope, PairRequest, PairReview},
    },
    local_access::Launcher,
    maintenance,
    network::NetworkConfig,
    runtime::Runtime,
    store::Store,
    update::{self, Update},
};

mod service;

const DEFAULT_LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4790);

#[derive(Parser)]
#[command(
    version,
    about = "Personal device cluster for local coding agents. Run without a command to start this node.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Data directory for direct startup. Subcommands accept their own --data-dir.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Discovery or relay endpoints for direct startup. May be repeated.
    #[arg(long = "bootstrap-url", value_name = "URL")]
    bootstrap_urls: Vec<String>,
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
    /// Display this node's trusted bootstrap or local browser entry.
    Access(Access),
    /// Select a release channel, check, or apply a managed update.
    Update(Update),
    /// Join a cluster using its invite and pinned Peer ID.
    Pair(Pair),
    /// Send a native Codex App Server method without retrying writes.
    Codex {
        method: String,
        #[arg(long, default_value = "{}")]
        json: String,
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Keep reading notifications and accept manual JSON responses on stdin.
        #[arg(long)]
        watch: bool,
    },
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
    /// Override discovery or relay endpoints for this run. May be repeated.
    #[arg(long = "bootstrap-url", value_name = "URL")]
    bootstrap_urls: Vec<String>,
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
    /// Existing project directories available to this device's Agent. May be repeated.
    #[arg(long = "root")]
    roots: Vec<PathBuf>,
    /// Serve Web and public discovery/relay without joining a cluster or starting an Agent.
    #[arg(long, conflicts_with_all = ["manager", "roots", "codex_socket"])]
    relay_only: bool,
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
    /// Discovery or relay endpoints to save in the configuration. May be repeated.
    #[arg(long = "bootstrap-url", value_name = "URL")]
    bootstrap_urls: Vec<String>,
    /// Reachable WebSocket addresses published for this Node. May be repeated.
    #[arg(long = "advertise-url")]
    advertise_urls: Vec<String>,
    #[arg(long)]
    mdns: bool,
    /// Offer bounded encrypted forwarding to other personal clusters.
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

#[derive(Args)]
#[command(group(ArgGroup::new("invitation").required(true).args(["invite_file", "invite"])))]
struct Pair {
    #[command(flatten)]
    data: DataDir,
    #[arg(long)]
    invite_file: Option<PathBuf>,
    /// Base64url encoded invitation copied from a trusted management client.
    #[arg(long)]
    invite: Option<String>,
    #[arg(long, default_value = "Node")]
    name: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Invitation {
    url: String,
    peer_id: String,
    relay: bool,
    invite: InviteSecret,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InviteSecret {
    invite: InviteEnvelope,
    capability: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PendingPair {
    url: String,
    peer_id: String,
    relay: bool,
    invite_id: String,
    request_id: String,
    name: String,
    #[serde(default)]
    request_hash: Option<String>,
}

impl Cli {
    fn normalize_bootstrap_urls(&mut self) -> Result<()> {
        let urls = match &mut self.command {
            None => &mut self.bootstrap_urls,
            Some(Action::Serve(options)) => &mut options.bootstrap_urls,
            Some(Action::Init(options)) => &mut options.bootstrap_urls,
            _ => return Ok(()),
        };
        for url in urls {
            *url = network_url(url)?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let mut cli = Cli::parse();
    cli.normalize_bootstrap_urls()?;
    let command = match cli.command {
        Some(command) => command,
        None => {
            let data_dir = cli.data_dir.unwrap_or_else(config::default_dir);
            prepare_start(&data_dir, &cli.bootstrap_urls)?;
            Action::Serve(Serve {
                data: DataDir { data_dir },
                bootstrap_urls: cli.bootstrap_urls,
            })
        }
    };
    match command {
        Action::Init(options) => init(*options),
        Action::Serve(options) => {
            let runtime = Arc::new(
                Runtime::open_with_bootstrap_urls(&options.data.data_dir, &options.bootstrap_urls)
                    .await?,
            );
            runtime.start().await?;
            eprintln!(
                "Web: {}\nRun vibeface access with the same --data-dir for the trusted browser entry.",
                runtime.config.public_url
            );
            tokio::select! {
                result = api::serve(runtime.clone()) => result,
                result = maintenance::serve(runtime, options.data.data_dir) => result,
            }
        }
        Action::Access(options) => access(options).await,
        Action::Status(options) => status(options).await,
        Action::Service(options) => service::run(options).await,
        Action::Update(options) => update::run(options).await,
        Action::Pair(options) => pair(options).await,
        Action::Codex {
            method,
            json,
            socket,
            watch,
        } => {
            let socket = socket
                .or_else(|| std::env::var_os("VIBEFACE_CODEX_SOCKET").map(PathBuf::from))
                .context("--socket or VIBEFACE_CODEX_SOCKET is required")?;
            let params: Value = serde_json::from_str(&json).context("--json must be valid JSON")?;
            ensure!(
                !method.is_empty() && method.len() <= 256 && params.is_object(),
                "Codex method must be nonempty and --json must be an object"
            );
            vibeface_bridge::app_server::call(&socket, &method, params, watch).await
        }
        Action::Tool {
            name,
            json,
            call_ref,
        } => call_tool(name, json, call_ref).await,
    }
}

fn prepare_start(dir: &Path, bootstrap_urls: &[String]) -> Result<()> {
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
        configuration.network.bootstrap_urls = bootstrap_urls.to_vec();
        initialize(dir, configuration)?;
    }
    Ok(())
}

async fn status(options: Access) -> Result<()> {
    let data = fs::canonicalize(&options.data.data_dir)
        .context("initialized data directory is required; run vibeface init first")?;
    let configuration = Config::load(&data)?;
    let peer_id = config::identity(&data)?.peer_id()?;
    let health = maintenance::probe(&data).await.and_then(|health| {
        ensure!(
            health.peer_id == peer_id,
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
                "status":state, "peer_id":peer_id, "data_dir":data,
                "listen":configuration.listen, "roles":configuration.roles,
                "version":health.as_ref().ok().map(|health| &health.version),
                "cli_version":env!("CARGO_PKG_VERSION"), "error":error,
            })
        );
    } else {
        println!(
            "Peer ID: {peer_id}\nStatus: {state}\nListen: {}\nData: {}",
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
    configuration.local_auto_access = !options.no_local_auto_access && !options.relay_only;
    configuration.codex_socket = options.codex_socket;
    configuration.codex_bin = options.codex_bin;
    if options.relay_only {
        configuration.roles = vec![vibeface::model::NodeRole::Relay];
    }
    if let Some(url) = options.public_url {
        configuration.public_url = origin(&url)?.to_string().trim_end_matches('/').to_owned();
    }
    configuration.allowed_origins = options.allowed_origins;
    configuration.network = NetworkConfig {
        bootstrap_urls: options.bootstrap_urls,
        advertise_urls: options
            .advertise_urls
            .iter()
            .map(|value| network_url(value))
            .collect::<Result<_>>()?,
        mdns: options.mdns,
        relay_enabled: options.public_relay || options.relay_only,
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
    initialize(dir, configuration)
}

fn initialize(dir: &Path, configuration: Config) -> Result<()> {
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
    let capability = store.bootstrap_capability()?;
    create_private(
        &dir.join("bootstrap.json"),
        &serde_json::to_vec(&capability)?,
    )?;
    create_private(&dir.join("data-epoch.json"), br#"{"data_epoch":6}"#)?;
    println!("Initialized device with Peer ID {}", identity.peer_id()?);
    println!("Configuration: {}", dir.join("config.json").display());
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

async fn access(options: Access) -> Result<()> {
    let dir = &options.data.data_dir;
    let epoch: Value = serde_json::from_slice(&read_bounded(&dir.join("data-epoch.json"), 4096)?)?;
    ensure!(
        epoch == json!({"data_epoch":6}),
        "data directory uses another format; existing data was preserved"
    );
    let configuration = Config::load(dir)?;
    let identity = config::identity(dir)?;
    let store = Store::open_readonly(dir.join("node.sqlite3"))?;
    let initialized = store.has_cluster()?;
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
                && launcher.peer_id == identity.peer_id()?
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
            entry["peer_id"] == launcher.peer_id && entry["url"] == launcher.origin,
            "local browser entry does not match this node"
        );
        url.set_fragment(Some(&format!("local={token}&node={}", launcher.peer_id)));
        if options.json {
            println!(
                "{}",
                json!({"url":url.to_string(),"peer_id":launcher.peer_id,"local":true})
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
    let entry = json!({"url":url.origin().ascii_serialization(),"peer_id":identity.peer_id()?,"capability":capability});
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

fn invitation(options: &Pair) -> Result<Invitation> {
    let bytes = if let Some(path) = &options.invite_file {
        read_bounded(path, 128 * 1024)?
    } else {
        let encoded = options.invite.as_deref().context("invitation required")?;
        ensure!(encoded.len() <= 192 * 1024, "invitation is too large");
        URL_SAFE_NO_PAD
            .decode(encoded)
            .context("invalid base64url invitation")?
    };
    let invitation: Invitation =
        serde_json::from_slice(&bytes).context("invalid invitation file")?;
    ensure!(
        invitation.peer_id.len() == 64 && hex::decode(&invitation.peer_id).is_ok(),
        "invitation requires a pinned Node identity"
    );
    ensure!(
        invitation.invite.invite.payload.new_kind == IdentityKind::Node,
        "invitation must authorize a Node"
    );
    ensure!(
        digest(invitation.invite.capability.as_bytes())
            == invitation.invite.invite.payload.capability_hash,
        "invitation capability mismatch"
    );
    network_url(&invitation.url)?;
    Ok(invitation)
}

fn verify_review(
    review: &PairReview,
    invite: &InviteEnvelope,
    member: &Member,
    request_hash: &str,
) -> Result<()> {
    let payload = &invite.payload;
    ensure!(
        review.version == payload.version
            && review.invite_id == payload.invite_id
            && review.cluster_id == payload.cluster_id
            && review.inviter_subject_id == payload.inviter_subject_id
            && review.epoch == payload.epoch
            && review.expires_at_ms == payload.expires_at_ms
            && review.proposal_hash == request_hash
            && review.new_member == *member,
        "pairing review does not match the invitation and local device"
    );
    Ok(())
}

async fn pair(options: Pair) -> Result<()> {
    let invitation = invitation(&options)?;
    let url = network_url(&invitation.url)?;
    let runtime = Runtime::open(&options.data.data_dir).await?;
    let mut configuration = Config::load(&options.data.data_dir)?;
    let identity = &runtime.identity;
    let payload = &invitation.invite.invite.payload;
    let pending_path = options.data.data_dir.join("pairing.pending.json");
    if runtime.has_membership().await? {
        ensure!(
            pending_path.exists()
                && runtime.membership_state().await?.genesis.cluster_id == payload.cluster_id,
            "this Node already belongs to a cluster; existing membership was preserved"
        );
    }
    ensure!(
        !payload.manage
            && payload
                .node_roles
                .iter()
                .all(|role| configuration.roles.contains(role)),
        "invitation exceeds this Node's configured capabilities"
    );
    let member = Member::node(identity, &options.name, payload.node_roles.clone())?;
    let mut pending = if pending_path.exists() {
        let pending: PendingPair = serde_json::from_slice(&read_bounded(&pending_path, 8192)?)?;
        ensure!(
            pending.url == url
                && pending.peer_id == invitation.peer_id
                && pending.relay == invitation.relay
                && pending.invite_id == payload.invite_id
                && pending.name == options.name,
            "finish the pending pairing with its original invitation and device name"
        );
        pending
    } else {
        let pending = PendingPair {
            url: url.clone(),
            peer_id: invitation.peer_id.clone(),
            relay: invitation.relay,
            invite_id: payload.invite_id.clone(),
            request_id: uuid::Uuid::new_v4().to_string(),
            name: options.name.clone(),
            request_hash: None,
        };
        create_private(&pending_path, &serde_json::to_vec(&pending)?)?;
        pending
    };
    let mut client = Client::connect(&url, identity, &invitation.peer_id, invitation.relay).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(660);
    let mut accepted = false;
    loop {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "pairing is still pending; run the same command again to resume"
        );
        let status = client
            .call("pair_status", json!({"request_id":pending.request_id}))
            .await?;
        match status
            .get("status")
            .and_then(Value::as_str)
            .context("invalid pairing status")?
        {
            "granted" => {
                let state: ClusterState = serde_json::from_value(
                    status
                        .get("state")
                        .context("paired cluster state is missing")?
                        .clone(),
                )?;
                let view = state.project()?;
                ensure!(
                    view.cluster_id == payload.cluster_id,
                    "paired cluster does not match the invitation"
                );
                let authorized = view.authorize_node(&identity.peer_id()?, None)?;
                ensure!(
                    authorized == member
                        && authorized
                            .node_roles
                            .iter()
                            .all(|role| configuration.roles.contains(role)),
                    "paired cluster grants unexpected local permissions"
                );
                let installed = runtime.install_membership(state).await?;
                let current = installed.authorize_node(&identity.peer_id()?, None)?;
                ensure!(
                    current == member,
                    "installed cluster no longer grants the requested local membership"
                );
                configuration.roles = current.node_roles;
                if !configuration.network.bootstrap_urls.contains(&url) {
                    configuration.network.bootstrap_urls.push(url.clone());
                }
                configuration.save(&options.data.data_dir)?;
                fs::remove_file(pending_path)?;
                println!(
                    "Paired node {} with cluster {}",
                    identity.peer_id()?,
                    payload.cluster_id
                );
                println!("Start the Node to connect to its cluster.");
                return Ok(());
            }
            "not_found" => {
                ensure!(
                    !accepted,
                    "pairing request disappeared; pending receipt was preserved"
                );
                let prepared = client.call("prepare_pair_request", json!({"invite":invitation.invite.invite,"capability":invitation.invite.capability,"name":options.name,"request_id":pending.request_id})).await?;
                let mut request: PairRequest = serde_json::from_value(
                    prepared
                        .get("request")
                        .context("pairing request template missing")?
                        .clone(),
                )?;
                ensure!(
                    request.request_id == pending.request_id
                        && request.invite_id == payload.invite_id
                        && request.capability == invitation.invite.capability
                        && request.member == member,
                    "pairing request does not match this device and invitation"
                );
                let signing_bytes = request.signing_bytes()?;
                let request_hash = digest(&signing_bytes);
                ensure!(
                    pending
                        .request_hash
                        .as_ref()
                        .is_none_or(|previous| previous == &request_hash),
                    "pairing request template changed; original pending receipt was preserved"
                );
                pending.request_hash = Some(request_hash);
                config::private_write(&pending_path, &serde_json::to_vec(&pending)?)?;
                request.proof = identity.sign_payload("identity-pair", &signing_bytes)?;
                let result = client
                    .call("pair_request", json!({"request":request}))
                    .await?;
                let review: PairReview = serde_json::from_value(
                    result
                        .get("review")
                        .context("pairing review missing")?
                        .clone(),
                )?;
                verify_review(
                    &review,
                    &invitation.invite.invite,
                    &member,
                    pending
                        .request_hash
                        .as_deref()
                        .context("pending request hash is missing")?,
                )?;
                let signature =
                    identity.sign_payload("identity-pair-review", &review.signing_bytes()?)?;
                client
                    .call(
                        "pair_accept",
                        json!({"request_id":pending.request_id,"signature":signature}),
                    )
                    .await?;
                accepted = true;
                println!(
                    "Pairing request submitted. Confirm this device in the management client."
                );
            }
            "pending" => {
                if !accepted {
                    let review: PairReview = serde_json::from_value(
                        status
                            .get("review")
                            .context("pending review missing")?
                            .clone(),
                    )?;
                    verify_review(
                        &review,
                        &invitation.invite.invite,
                        &member,
                        pending
                            .request_hash
                            .as_deref()
                            .context("pending request hash is missing")?,
                    )?;
                    let signature =
                        identity.sign_payload("identity-pair-review", &review.signing_bytes()?)?;
                    client
                        .call(
                            "pair_accept",
                            json!({"request_id":pending.request_id,"signature":signature}),
                        )
                        .await?;
                    accepted = true;
                    println!(
                        "Pairing request resumed. Confirm this device in the management client."
                    );
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            _ => bail!("unsupported pairing status; pending receipt was preserved"),
        }
    }
}

async fn call_tool(name: String, arguments: String, call_ref: Option<String>) -> Result<()> {
    ensure!(!name.is_empty() && name.len() <= 128, "invalid tool name");
    let arguments: Value =
        serde_json::from_str(&arguments).context("invalid tool arguments JSON")?;
    ensure!(
        arguments.is_object(),
        "tool arguments must be a JSON object"
    );
    let write = !matches!(
        name.as_str(),
        "nodes" | "context" | "task_status" | "call_lookup"
    );
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
        "write tools require --call-ref; keep it and query call_lookup after any uncertain result"
    );
    let call_id = call_ref.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    ensure!(
        !call_id.is_empty() && call_id.len() <= 256 && !call_id.chars().any(char::is_control),
        "invalid call reference"
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
        .json(&json!({"name": name, "arguments": arguments, "call_id": call_id}))
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
    fn peer_id_pending_pair_uses_only_the_current_field() {
        let wire = json!({"url":"http://localhost:4790", "peer_id":"device", "relay":false,
            "invite_id":"invite", "request_id":"request", "name":"device", "request_hash":null});
        let pending: PendingPair = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(pending.peer_id, "device");
        assert_eq!(serde_json::to_value(pending).unwrap(), wire);
        let mut old = wire;
        let fields = old.as_object_mut().unwrap();
        fields.remove("peer_id");
        fields.insert("node_id".into(), json!("device"));
        assert!(serde_json::from_value::<PendingPair>(old).is_err());
    }

    #[test]
    fn cli_direct_start_keeps_data_directory_arguments_unambiguous() {
        let default = Cli::try_parse_from(["vibeface"]).unwrap();
        assert!(default.command.is_none() && default.data_dir.is_none());
        assert!(default.bootstrap_urls.is_empty());
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
        assert!(options.bootstrap_urls.is_empty());
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
    fn cli_bootstrap_urls_normalize_for_direct_start_serve_and_init() {
        for command in [None, Some("serve"), Some("init")] {
            let mut arguments = vec!["vibeface"];
            arguments.extend(command);
            arguments.extend([
                "--bootstrap-url",
                "https://node.test",
                "--bootstrap-url",
                "ws://127.0.0.1:4790/api/network/ws",
            ]);
            let mut cli = Cli::try_parse_from(arguments).unwrap();
            cli.normalize_bootstrap_urls().unwrap();
            let urls = match cli.command {
                None => cli.bootstrap_urls,
                Some(Action::Serve(options)) => options.bootstrap_urls,
                Some(Action::Init(options)) => options.bootstrap_urls,
                _ => panic!("expected startup or initialization"),
            };
            assert_eq!(
                urls,
                [
                    "wss://node.test/api/network/ws",
                    "ws://127.0.0.1:4790/api/network/ws"
                ]
            );
        }
    }

    #[test]
    fn invalid_bootstrap_urls_do_not_initialize_data_directories() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "vibeface-invalid-bootstrap-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&root)?;
        for command in [None, Some("serve"), Some("init")] {
            let dir = root.join(command.unwrap_or("direct"));
            for url in [
                "file:///tmp/node",
                "https://fixture-user:fixture-password@node.test",
                "https://node.test/?token=fixture-query",
                "https://node.test/other",
                "https://node.test/#fragment",
            ] {
                let mut arguments = vec!["vibeface"];
                arguments.extend(command);
                arguments.extend(["--data-dir", dir.to_str().unwrap(), "--bootstrap-url", url]);
                let mut cli = Cli::try_parse_from(arguments)?;
                let error = cli.normalize_bootstrap_urls().unwrap_err().to_string();
                for secret in ["fixture-user", "fixture-password", "fixture-query"] {
                    assert!(!error.contains(secret));
                }
                assert!(!dir.exists());
            }
        }
        let urls = vec![String::from("wss://node.test/api/network/ws"); 9];
        assert!(prepare_start(&root.join("direct"), &urls).is_err());
        let dir = root.join("init");
        let mut arguments = vec!["vibeface", "init", "--data-dir", dir.to_str().unwrap()];
        for _ in 0..9 {
            arguments.extend(["--bootstrap-url", "https://node.test"]);
        }
        let mut cli = Cli::try_parse_from(arguments)?;
        cli.normalize_bootstrap_urls()?;
        let Some(Action::Init(options)) = cli.command else {
            panic!("expected init")
        };
        assert!(init(*options).is_err());
        assert!(fs::read_dir(&root)?.next().is_none());
        fs::remove_dir(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn startup_bootstrap_urls_persist_and_overrides_preserve_saved_config() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("vibeface-bootstrap-{}", uuid::Uuid::new_v4()));
        for command in [None, Some("init")] {
            let dir = root.join(command.unwrap_or("direct"));
            let mut arguments = vec!["vibeface"];
            arguments.extend(command);
            arguments.extend([
                "--data-dir",
                dir.to_str().unwrap(),
                "--bootstrap-url",
                "https://first.test",
                "--bootstrap-url",
                "http://second.test",
            ]);
            let mut cli = Cli::try_parse_from(arguments)?;
            cli.normalize_bootstrap_urls()?;
            match cli.command {
                None => prepare_start(&dir, &cli.bootstrap_urls)?,
                Some(Action::Init(options)) => init(*options)?,
                _ => panic!("expected direct startup or init"),
            }
            let mut configuration = Config::load(&dir)?;
            assert_eq!(
                configuration.network.bootstrap_urls,
                [
                    "wss://first.test/api/network/ws",
                    "ws://second.test/api/network/ws"
                ]
            );
            configuration.roles = vec![vibeface::model::NodeRole::Replica];
            configuration.codex_socket = Some(dir.join("codex.sock"));
            configuration.save(&dir)?;
            let saved = fs::read(dir.join("config.json"))?;
            let identity = fs::read(dir.join("identity.json"))?;
            let mut cli = Cli::try_parse_from([
                "vibeface",
                "serve",
                "--data-dir",
                dir.to_str().unwrap(),
                "--bootstrap-url",
                "https://override.test",
            ])?;
            cli.normalize_bootstrap_urls()?;
            let Some(Action::Serve(options)) = cli.command else {
                panic!("expected serve")
            };
            prepare_start(&dir, &options.bootstrap_urls)?;
            let runtime =
                Runtime::open_with_bootstrap_urls(&options.data.data_dir, &options.bootstrap_urls)
                    .await?;
            assert_eq!(
                runtime.config.network.bootstrap_urls,
                ["wss://override.test/api/network/ws"]
            );
            assert!(Runtime::open(&dir).await.is_err());
            assert_eq!(fs::read(dir.join("config.json"))?, saved);
            assert_eq!(fs::read(dir.join("identity.json"))?, identity);
            drop(runtime);
            let runtime = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match Runtime::open(&dir).await {
                        Ok(runtime) => break Ok(runtime),
                        Err(error)
                            if matches!(
                                error.downcast_ref::<std::fs::TryLockError>(),
                                Some(std::fs::TryLockError::WouldBlock)
                            ) =>
                        {
                            tokio::task::yield_now().await;
                        }
                        Err(error) => break Err(error),
                    }
                }
            })
            .await
            .context("data directory lock was not released after dropping Runtime")??;
            assert_eq!(
                runtime.config.network.bootstrap_urls,
                [
                    "wss://first.test/api/network/ws",
                    "ws://second.test/api/network/ws"
                ]
            );
            assert_eq!(fs::read(dir.join("config.json"))?, saved);
            assert_eq!(fs::read(dir.join("identity.json"))?, identity);
            drop(runtime);
        }
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
            prepare_start(&dir, &[]).unwrap();
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
            let peer_id = config::identity(&dir).unwrap().peer_id().unwrap();
            prepare_start(&dir, &[]).unwrap();
            assert_eq!(fs::read(dir.join("identity.json")).unwrap(), identity);
            assert_eq!(fs::read(dir.join("config.json")).unwrap(), config);
            let runtime = Runtime::open(&dir).await.unwrap();
            assert_eq!(runtime.identity.peer_id().unwrap(), peer_id);
            assert_eq!(runtime.config.timeout_secs, 1234);
            drop(runtime);
            fs::write(dir.join("config.json"), b"invalid configuration").unwrap();
            prepare_start(&dir, &[]).unwrap();
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
        prepare_start(&uninitialized, &[]).unwrap();
        assert!(Runtime::open(&uninitialized).await.is_err());
        assert_eq!(
            fs::read(uninitialized.join("existing-data")).unwrap(),
            b"preserve me"
        );
        assert!(!uninitialized.join("identity.json").exists());
        let file = root.join("file");
        fs::write(&file, b"preserve file").unwrap();
        assert!(prepare_start(&file, &[]).is_err());
        #[cfg(unix)]
        for target in [root.join("empty"), root.join("absent")] {
            let link = root.join("link");
            std::os::unix::fs::symlink(target, &link).unwrap();
            assert!(prepare_start(&link, &[]).is_err());
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
        assert!(Cli::try_parse_from(["vibeface", "tool", "send_task", "--json", "{}"]).is_ok());
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
            .is_ok()
        );
    }

    #[test]
    fn relay_only_initialization_does_not_join_a_cluster() -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("vibeface-relay-only-{}", uuid::Uuid::new_v4()));
        let cli = Cli::try_parse_from([
            "vibeface",
            "init",
            "--relay-only",
            "--data-dir",
            dir.to_str().context("temporary path is invalid")?,
        ])?;
        let Some(Action::Init(options)) = cli.command else {
            bail!("expected init command")
        };
        init(*options)?;
        let configuration = Config::load(&dir)?;
        assert_eq!(configuration.roles, vec![vibeface::model::NodeRole::Relay]);
        assert!(configuration.network.relay_enabled);
        assert!(configuration.manager.is_none() && configuration.roots.is_empty());
        assert!(!configuration.local_auto_access);
        let store = Store::open_readonly(dir.join("node.sqlite3"))?;
        assert!(!store.has_cluster()?);
        drop(store);
        assert!(
            Cli::try_parse_from([
                "vibeface",
                "init",
                "--relay-only",
                "--manager",
                "codex_sdk",
                "--workspace",
                "."
            ])
            .is_err()
        );
        fs::remove_dir_all(dir)?;
        Ok(())
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
    fn pairing_review_binds_the_exact_local_request() {
        use vibeface::{identity::local::InvitePayload, model::NodeRole};
        let device = LocalIdentity::generate();
        let member = Member::node(&device, "Build Node", vec![NodeRole::Replica]).unwrap();
        let invite = InviteEnvelope {
            payload: InvitePayload {
                version: vibeface::identity::IDENTITY_VERSION,
                invite_id: "invite".into(),
                request_id: "invite-request".into(),
                cluster_id: "ab".repeat(32),
                inviter_subject_id: "cd".repeat(32),
                capability_hash: digest(b"fixture capability"),
                new_kind: IdentityKind::Node,
                node_roles: vec![NodeRole::Replica],
                manage: false,
                epoch: 1,
                expires_at_ms: 42,
                nonce: "invite nonce".into(),
            },
            inviter_signature: String::new(),
        };
        let request = PairRequest {
            request_id: "request".into(),
            invite_id: "invite".into(),
            capability: "fixture capability".into(),
            member: member.clone(),
            nonce: "request nonce".into(),
            proof: String::new(),
        };
        for relay in [false, true] {
            let mut envelope = json!({
                "url":"ws://127.0.0.1:4790/api/network/ws",
                "peer_id":device.peer_id().unwrap(),
                "relay":relay,
                "invite":{"invite":&invite,"capability":"fixture capability"}
            });
            let mut options = Pair {
                data: DataDir {
                    data_dir: PathBuf::new(),
                },
                invite_file: None,
                invite: Some(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&envelope).unwrap())),
                name: "Build Node".into(),
            };
            assert_eq!(super::invitation(&options).unwrap().relay, relay);
            let parsed: Invitation = serde_json::from_value(envelope.clone()).unwrap();
            assert_eq!(parsed.peer_id, device.peer_id().unwrap());
            let mut old = envelope.clone();
            let fields = old.as_object_mut().unwrap();
            let peer_id = fields.remove("peer_id").unwrap();
            fields.insert("node_id".into(), peer_id);
            assert!(serde_json::from_value::<Invitation>(old).is_err());
            envelope.as_object_mut().unwrap().remove("relay");
            options.invite = Some(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&envelope).unwrap()));
            assert!(super::invitation(&options).is_err());
        }
        let request_hash = digest(&request.signing_bytes().unwrap());
        let mut review = PairReview {
            version: vibeface::identity::IDENTITY_VERSION,
            invite_id: "invite".into(),
            cluster_id: "ab".repeat(32),
            inviter_subject_id: "cd".repeat(32),
            new_member: member.clone(),
            proposal_hash: request_hash.clone(),
            parents: vec![],
            epoch: 1,
            expires_at_ms: 42,
        };
        verify_review(&review, &invite, &member, &request_hash).unwrap();
        review.proposal_hash = invite.payload.hash().unwrap();
        assert!(verify_review(&review, &invite, &member, &request_hash).is_err());
        review.proposal_hash = request_hash.clone();
        review.new_member.name = "Another Node".into();
        assert!(verify_review(&review, &invite, &member, &request_hash).is_err());
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
