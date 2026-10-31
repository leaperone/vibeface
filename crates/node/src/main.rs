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
    crypto::{LocalIdentity, random_hex},
    identity::local::BootstrapCapability,
    local_access::Launcher,
    maintenance,
    net::{self, NetworkConfig, PublicEntry, RelayServiceMode},
    providers::Provider,
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
    /// Data directory for direct startup; defaults to VIBEFACE_DATA_DIR, then ~/.vibeface.
    /// Subcommands accept their own --data-dir.
    #[arg(long, value_parser = config::absolute_path)]
    data_dir: Option<PathBuf>,
    /// Disable browser access for this run; local tools and hooks remain available.
    #[arg(long)]
    no_web: bool,
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
    /// Manage the experimental delivery wallet through its owner-only local socket.
    Wallet(WalletOptions),
    /// Manage this user's installed node service and update timer.
    Service(service::Service),
    /// Display this node's local browser entry, or turn opening it without confirmation on or off.
    Access(LocalEntry),
    /// Select a release channel, check, or apply a managed update.
    Update(Update),
    /// Join a personal network with an invite link from a management device.
    Join(Join),
    /// Forward a coding agent's hook stdin to this device's Node, printing the answer when its
    /// session waits on one; always exits 0.
    Hook(Hook),
    /// Install, remove, check or test this Node's hooks and extensions in coding agents.
    Hooks(Hooks),
    /// The coordination Agent's triage answers: list pending events, ask about or dismiss one.
    Triage(Triage),
    /// List the requests sessions hold for the coordination Agent: App Server requests and
    /// hooks waiting for an answer.
    Requests(LocalNode),
    /// Answer a held request once with its result JSON; answering again prints the first answer.
    Respond {
        request_id: String,
        result: String,
        /// The device holding the request, when it is not this one.
        #[arg(long, value_name = "DEVICE")]
        node: Option<String>,
        #[command(flatten)]
        local: LocalNode,
    },
    /// Add a record to this device's Node log, e.g. from the coordination Agent or a script.
    Log(LogRecord),
    /// Read this device's Node log, newest last.
    Logs(Logs),
    /// Call a node tool from a locally managed SDK process.
    Tool {
        name: String,
        #[arg(long)]
        json: String,
        /// Stable reference for a write intent; reuse it after a timeout or partial result.
        #[arg(long)]
        call_ref: Option<String>,
    },
    /// Talk to this device's Codex App Server directly.
    Codex {
        /// App Server socket; the coordination Agent gets it in VIBEFACE_CODEX_SOCKET.
        #[arg(long, env = "VIBEFACE_CODEX_SOCKET")]
        socket: Option<PathBuf>,
        #[command(subcommand)]
        command: CodexCommand,
    },
}

#[derive(Subcommand)]
enum CodexCommand {
    /// Call any App Server method and print its result.
    Call {
        method: String,
        params: Option<String>,
    },
    /// Add a message to a thread as a work of this bot turn: it steers the running turn or
    /// starts one, and its result comes back like dispatch_work's. Reusing --id only reads back.
    Send {
        thread: String,
        text: String,
        /// The work's call_ref, unique within this turn; keep it to retry an unknown result.
        #[arg(long)]
        id: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        /// Local image path or URL; repeat for more.
        #[arg(long)]
        image: Vec<String>,
    },
    /// Print the thread's notifications as JSON lines.
    Watch { thread: String },
}

#[derive(Args)]
struct DataDir {
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = config::data_dir, default_value_os_t = config::default_data_dir())]
    data_dir: PathBuf,
}

#[derive(Args)]
struct WalletOptions {
    #[command(flatten)]
    data: DataDir,
    #[command(subcommand)]
    command: WalletAction,
}

#[derive(Subcommand)]
enum WalletAction {
    Status,
    /// Create an independent encrypted wallet; it starts locked.
    Create {
        /// Read the passphrase from an owner-only file or pipe descriptor instead of the terminal.
        #[arg(long)]
        passphrase_fd: Option<i32>,
    },
    /// Unlock this process's wallet without sending its passphrase to a network endpoint.
    Unlock {
        #[arg(long)]
        passphrase_fd: Option<i32>,
    },
    Lock,
    /// Apply local experimental delivery settings from a JSON file.
    Configure {
        file: PathBuf,
    },
    /// Register this Node's receiving key on the configured local test chain.
    RegisterKey {
        #[arg(long)]
        intent_id: String,
        #[arg(long)]
        gas_cap: String,
    },
    /// Rotate the receiving key while retaining material for already-held orders.
    RotateKey {
        #[arg(long)]
        intent_id: String,
        #[arg(long)]
        gas_cap: String,
    },
    /// Revoke new receiving rights without deleting old held-order keys.
    RevokeKey {
        #[arg(long)]
        intent_id: String,
        #[arg(long)]
        gas_cap: String,
    },
    /// Save the separate encrypted wallet backup without overwriting an existing file.
    Backup {
        output: PathBuf,
    },
}

#[derive(Args)]
struct Serve {
    #[command(flatten)]
    data: DataDir,
    /// Disable browser access for this run; local tools and hooks remain available.
    #[arg(long)]
    no_web: bool,
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
    /// Keep browser access off when this node starts.
    #[arg(long)]
    no_web: bool,
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
    manager: Option<Provider>,
    #[arg(long, requires = "manager")]
    workspace: Option<PathBuf>,
    #[arg(long, requires = "manager")]
    model: Option<String>,
    /// A provider's program, e.g. `cursor=/opt/bin/cursor-sdk-bridge`; the others run their
    /// default name. May be repeated.
    #[arg(long = "bin", value_name = "PROVIDER=PATH", value_parser = provider_bin)]
    bins: Vec<(Provider, PathBuf)>,
    /// Socket of the running Codex App Server; found under the Codex folder when omitted.
    #[arg(long)]
    codex_socket: Option<PathBuf>,
    /// iroh relay of another Node in the personal network, e.g. `https://relay.example`. May be repeated.
    #[arg(long = "relay", value_name = "URL")]
    relays: Vec<String>,
    /// Pin a discovery server, e.g. vibeface://<device-id>@discovery.example:4790. May be repeated.
    #[arg(long = "discovery", value_name = "URL")]
    discovery: Vec<String>,
    /// Forward business ciphertext, or only exchange observed addresses for direct connections.
    #[arg(long, value_enum, default_value = "forward")]
    relay_mode: RelayServiceMode,
    /// Run a TCP data relay or a UDP discovery service on this address.
    #[arg(long, value_name = "ADDR")]
    relay_listen: Option<SocketAddr>,
    /// The relay's explicitly configured URL as other Nodes reach it.
    #[arg(long, value_name = "URL", requires = "relay_listen")]
    relay_url: Option<String>,
    /// Run public web and the selected relay service outside personal networks.
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
struct LocalNode {
    #[command(flatten)]
    data: DataDir,
    /// This device's local Node; defaults to `$VIBEFACE_TRIAGE_URL`, then the configured local listener.
    #[arg(long, value_name = "URL")]
    node_url: Option<String>,
}

#[derive(Args)]
struct Hook {
    #[arg(value_enum)]
    agent: Provider,
    #[command(flatten)]
    local: LocalNode,
    /// Send a test event instead of reading stdin.
    #[arg(long)]
    test: bool,
    /// Print the normalized event without sending it.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum HooksAction {
    Install,
    Uninstall,
    Status,
    Test,
}

#[derive(Args)]
struct Hooks {
    #[arg(value_enum)]
    action: HooksAction,
    #[command(flatten)]
    local: LocalNode,
    /// Home directory holding the coding agents' hook and extension files, Codex's included;
    /// defaults to `$HOME`, with Codex's in `$CODEX_HOME` when set.
    #[arg(long, value_name = "DIR", value_parser = config::absolute_path)]
    home: Option<PathBuf>,
    /// Show what would change without writing.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct Triage {
    #[command(subcommand)]
    action: TriageAction,
    #[command(flatten)]
    local: LocalNode,
}

#[derive(Subcommand)]
enum TriageAction {
    /// Pending events as JSON.
    Events {
        #[arg(long, value_name = "MS")]
        since: Option<i64>,
    },
    /// The user should decide: ask in the chat of the bot that owns the event's session.
    Raise {
        #[arg(long)]
        event_id: String,
        #[arg(long, value_parser = ["info", "attention", "urgent"])]
        severity: String,
        #[arg(long)]
        question: String,
        /// Hold this request until the question is answered.
        #[arg(long)]
        request_id: Option<String>,
        /// An answer the user can pick; repeat for more.
        #[arg(long = "choice", value_name = "LABEL")]
        choices: Vec<String>,
    },
    /// Handled silently.
    Dismiss {
        #[arg(long)]
        event_id: String,
    },
}

#[derive(Args)]
struct LogRecord {
    /// trace, debug, info, warn or error.
    level: vibeface::logging::Level,
    message: String,
    #[arg(long, env = "VIBEFACE_NODE_CONVERSATION")]
    conversation: Option<String>,
    #[arg(long)]
    work: Option<String>,
    /// Codex thread.
    #[arg(long)]
    thread: Option<String>,
    /// Stable dotted name, e.g. `agent.retry`; defaults to `<source>.log`.
    #[arg(long)]
    event: Option<String>,
    /// `agent` or `cli`; defaults to `agent` inside a coordination Agent's shell.
    #[arg(long)]
    source: Option<vibeface::logging::Source>,
    #[command(flatten)]
    local: LocalNode,
}

#[derive(Args)]
struct Logs {
    #[command(flatten)]
    data: DataDir,
    /// Age such as `30m`, `2h`, `1d`, or a Unix time in milliseconds.
    #[arg(long)]
    since: Option<String>,
    /// The least severe level shown.
    #[arg(long)]
    level: Option<vibeface::logging::Level>,
    /// node, web, agent, cli, codex or cursor; repeat for more.
    #[arg(long)]
    source: Vec<vibeface::logging::Source>,
    #[arg(long)]
    conversation: Option<String>,
    #[arg(long)]
    work: Option<String>,
    /// Case-insensitive text in the message, event, ids or fields.
    #[arg(long)]
    grep: Option<String>,
    /// Show at most this many of the newest records.
    #[arg(long, default_value_t = 200)]
    limit: usize,
    /// Keep printing new records as they are written.
    #[arg(short, long)]
    follow: bool,
    /// Print the records as JSON lines.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct Access {
    #[command(flatten)]
    data: DataDir,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct LocalEntry {
    #[command(flatten)]
    access: Access,
    /// Let browsers on this computer open the Node without confirming, then print the entry.
    #[arg(long, conflicts_with = "disable")]
    enable: bool,
    /// Require confirmation for browsers on this computer; signs out their local sessions.
    #[arg(long)]
    disable: bool,
}

#[derive(Args)]
struct Join {
    /// Invite link: `https://<gateway>/join#vibeface1…`, the app's `vibeface://join?link=…`, or
    /// the ticket alone.
    link: String,
    #[command(flatten)]
    data: DataDir,
    /// Device name chosen by its owner and shown to the network.
    #[arg(long)]
    name: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let command = match cli.command {
        Some(command) => command,
        None => {
            let data_dir = config::data_dir_or_home(
                cli.data_dir
                    .or_else(|| std::env::var_os("VIBEFACE_DATA_DIR").map(PathBuf::from)),
                std::env::var_os("HOME"),
            )?;
            if !data_dir.join("config.json").exists() {
                check_ports(
                    DEFAULT_LISTEN,
                    config::DEFAULT_LOCAL_LISTEN,
                    !cli.no_web,
                    false,
                )?;
            }
            prepare_start(&data_dir)?;
            Action::Serve(Serve {
                data: DataDir { data_dir },
                no_web: cli.no_web,
            })
        }
    };
    let (data_dir, quiet_libraries) = match &command {
        Action::Serve(options) => (Some(options.data.data_dir.as_path()), false),
        Action::Update(_) => (None, false),
        _ => (None, true),
    };
    vibeface::logging::init(data_dir, quiet_libraries);
    match command {
        Action::Init(options) => {
            check_ports(
                options.listen,
                options.local_listen,
                !options.no_web,
                options.public_entry,
            )?;
            init(*options)
        }
        Action::Serve(options) => {
            let mut runtime = Runtime::open(&options.data.data_dir).await?;
            runtime.config.web_enabled &= !options.no_web;
            let runtime = Arc::new(runtime);
            runtime.start().await?;
            let config = &runtime.config;
            if !config.web_enabled {
                eprintln!("Web: disabled");
            } else if !config.public_entry
                && (config.listen.ip().is_loopback() || config.listen.ip().is_unspecified())
            {
                // One local address: a browser joined on another origin keeps a separate identity.
                eprintln!("Web: http://localhost:{}", config.listen.port());
            } else {
                eprintln!("Web: {}", config.public_url);
            }
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            let mut rejoin = false;
            let result = tokio::select! {
                result = api::serve(runtime.clone()) => result,
                result = maintenance::serve(runtime.clone(), options.data.data_dir.clone()) => result,
                _ = runtime.rejoin_requested() => { rejoin = true; Ok(()) }
                _ = terminate.recv() => Ok(()),
                _ = tokio::signal::ctrl_c() => Ok(()),
            };
            runtime.shutdown().await;
            if rejoin {
                return restart_fresh(&options.data.data_dir);
            }
            result
        }
        Action::Access(options) => access(options).await,
        Action::Status(options) => status(options).await,
        Action::Wallet(options) => wallet(options).await,
        Action::Service(options) => service::run(options).await,
        Action::Update(options) => update::run(options).await,
        Action::Join(options) => join(options).await,
        Action::Tool {
            name,
            json,
            call_ref,
        } => call_tool(name, json, call_ref).await,
        Action::Codex { socket, command } => {
            use vibeface::agent_cli::{Command, run};
            let socket = || {
                socket
                    .clone()
                    .context("--socket or VIBEFACE_CODEX_SOCKET is required")
            };
            match command {
                CodexCommand::Call { method, params } => {
                    run(&socket()?, Command::Call { method, params }).await
                }
                CodexCommand::Send {
                    thread,
                    text,
                    id,
                    model,
                    effort,
                    image,
                } => {
                    ensure!(
                        std::env::var_os("VIBEFACE_TOOL_TOKEN").is_some(),
                        "codex send runs inside a bot turn (VIBEFACE_NODE_URL and VIBEFACE_TOOL_TOKEN); outside one, ask the bot in its chat"
                    );
                    let args = json!({"thread":thread,"prompt":text,"call_ref":id,"agent":"codex",
                        "model":model,"effort":effort,"images":image});
                    let result = dispatch_work(&args, &id, &socket()?).await?;
                    println!("{}", serde_json::to_string(&result)?);
                    Ok(())
                }
                CodexCommand::Watch { thread } => {
                    run(&socket()?, Command::Watch { thread_id: thread }).await
                }
            }
        }
        Action::Requests(local) => post_triage(&local, json!({"action":"requests"})).await,
        Action::Respond {
            request_id,
            result,
            node,
            local,
        } => {
            let result: serde_json::Value =
                serde_json::from_str(&result).context("result must be JSON")?;
            let body =
                json!({"action":"respond","request_id":request_id,"result":result,"node":node});
            post_triage(&local, body).await
        }
        Action::Hook(options) => {
            if let Err(error) = hook(options).await {
                eprintln!("vibeface hook: {error:#}");
            }
            Ok(())
        }
        Action::Hooks(options) => hooks(options, vibeface::discovery::Host::current()).await,
        Action::Triage(options) => triage(options).await,
        Action::Log(options) => log_record(options).await,
        Action::Logs(options) => logs(options).await,
    }
}

async fn wallet(options: WalletOptions) -> Result<()> {
    use vibeface::delivery::wallet::{WalletCommand, backup};
    let dir = options.data.data_dir;
    let command = match options.command {
        WalletAction::Status => WalletCommand::Status,
        WalletAction::Create { passphrase_fd } => WalletCommand::Create {
            passphrase: read_wallet_passphrase(passphrase_fd)?,
        },
        WalletAction::Unlock { passphrase_fd } => WalletCommand::Unlock {
            passphrase: read_wallet_passphrase(passphrase_fd)?,
        },
        WalletAction::Lock => WalletCommand::Lock,
        WalletAction::Configure { file } => {
            let bytes = read_bounded_local(&file, 16 * 1024)?;
            let config =
                serde_json::from_slice(&bytes).context("invalid delivery configuration")?;
            maintenance::configure_delivery(&dir, config).await?;
            println!("Delivery settings applied.");
            return Ok(());
        }
        WalletAction::RegisterKey { intent_id, gas_cap } => {
            let result = maintenance::delivery_key_operation(
                &dir,
                vibeface::delivery::KeyAction::Register,
                intent_id,
                gas_cap,
            )
            .await?;
            println!("{}", serde_json::to_string(&result)?);
            return Ok(());
        }
        WalletAction::RotateKey { intent_id, gas_cap } => {
            let result = maintenance::delivery_key_operation(
                &dir,
                vibeface::delivery::KeyAction::Rotate,
                intent_id,
                gas_cap,
            )
            .await?;
            println!("{}", serde_json::to_string(&result)?);
            return Ok(());
        }
        WalletAction::RevokeKey { intent_id, gas_cap } => {
            let result = maintenance::delivery_key_operation(
                &dir,
                vibeface::delivery::KeyAction::Revoke,
                intent_id,
                gas_cap,
            )
            .await?;
            println!("{}", serde_json::to_string(&result)?);
            return Ok(());
        }
        WalletAction::Backup { output } => {
            backup(&dir, &output)?;
            println!("Encrypted delivery wallet backup saved.");
            return Ok(());
        }
    };
    let result = maintenance::delivery_wallet(&dir, command).await?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

fn read_bounded_local(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= limit,
        "local delivery input is not a bounded regular file"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "local delivery input exceeds its limit"
    );
    Ok(bytes)
}

fn read_wallet_passphrase(fd: Option<i32>) -> Result<vibeface::delivery::wallet::SecretString> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{FileTypeExt, MetadataExt},
    };
    use zeroize::Zeroizing;
    let mut bytes = Zeroizing::new(Vec::new());
    if let Some(fd) = fd {
        ensure!(fd >= 0, "wallet passphrase descriptor is invalid");
        // Duplicate the borrowed descriptor so the caller retains ownership of its original.
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        ensure!(
            duplicate >= 0,
            "wallet passphrase descriptor cannot be read"
        );
        let file = unsafe { fs::File::from_raw_fd(duplicate) };
        let metadata = file.metadata()?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0
                && (metadata.is_file() || metadata.file_type().is_fifo()),
            "wallet passphrase descriptor must be an owner-only regular file or pipe"
        );
        file.take(1026).read_to_end(&mut bytes)?;
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    } else {
        let mut terminal = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .context("wallet passphrase requires a terminal or --passphrase-fd")?;
        let mut previous = std::mem::MaybeUninit::<libc::termios>::uninit();
        ensure!(
            unsafe { libc::tcgetattr(terminal.as_raw_fd(), previous.as_mut_ptr()) } == 0,
            "wallet terminal settings are unavailable"
        );
        let previous = unsafe { previous.assume_init() };
        let mut hidden = previous;
        hidden.c_lflag &= !(libc::ECHO | libc::ECHONL);
        ensure!(
            unsafe { libc::tcsetattr(terminal.as_raw_fd(), libc::TCSAFLUSH, &hidden) } == 0,
            "wallet terminal cannot disable echo"
        );
        struct RestoreEcho {
            fd: i32,
            previous: libc::termios,
        }
        impl Drop for RestoreEcho {
            fn drop(&mut self) {
                unsafe {
                    libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.previous);
                }
            }
        }
        let restore = RestoreEcho {
            fd: terminal.as_raw_fd(),
            previous,
        };
        terminal.write_all(b"Delivery wallet passphrase: ")?;
        terminal.flush()?;
        loop {
            let mut byte = [0];
            ensure!(
                terminal.read(&mut byte)? == 1,
                "wallet terminal closed before a passphrase was entered"
            );
            if matches!(byte[0], b'\n' | b'\r') {
                break;
            }
            ensure!(bytes.len() < 1024, "wallet passphrase exceeds its limit");
            bytes.push(byte[0]);
        }
        drop(restore);
        terminal.write_all(b"\n")?;
    }
    ensure!(
        (12..=1024).contains(&bytes.len())
            && !bytes.iter().any(|byte| matches!(byte, b'\n' | b'\r' | 0)),
        "wallet passphrase must contain 12 to 1024 bytes on one line"
    );
    let text = String::from_utf8(bytes.to_vec()).context("wallet passphrase must be UTF-8")?;
    Ok(vibeface::delivery::wallet::SecretString::new(text))
}

async fn log_record(options: LogRecord) -> Result<()> {
    use vibeface::logging::Source;
    let source =
        options
            .source
            .unwrap_or(match std::env::var_os(vibeface::runtime::TRIAGE_URL_ENV) {
                Some(_) => Source::Agent,
                None => Source::Cli,
            });
    ensure!(
        matches!(source, Source::Agent | Source::Cli),
        "--source is agent or cli"
    );
    let body = json!({"source":source,"records":[{"level":options.level,"message":options.message,
        "event":options.event,"conversation_id":options.conversation,"work_id":options.work,
        "thread_id":options.thread}]});
    post_local(
        &local_node_url(&options.local)?,
        "api/log",
        &body,
        Duration::from_secs(10),
    )
    .await?;
    Ok(())
}

async fn logs(options: Logs) -> Result<()> {
    use vibeface::logging::{self, Filter};
    let dir = &options.data.data_dir;
    ensure!(
        dir.join(logging::FILE).exists(),
        "no Node log in {}; start the Node with this --data-dir first",
        dir.display()
    );
    let filter = Filter {
        since_ms: options.since.as_deref().map(since_ms).transpose()?,
        level: options.level,
        sources: options.source,
        conversation_id: options.conversation,
        work_id: options.work,
        text: options.grep,
        limit: Some(options.limit),
        ..Default::default()
    };
    let print = |record: &logging::Record| -> Result<()> {
        let line = match options.json {
            true => serde_json::to_string(record)?,
            false => log_line(record),
        };
        let mut out = std::io::stdout().lock();
        // A closed pipe, as with `| head`, ends the output quietly.
        if writeln!(out, "{line}").is_err() {
            std::process::exit(0);
        }
        Ok(())
    };
    for record in logging::read(dir, &filter)? {
        print(&record)?;
    }
    if !options.follow {
        return Ok(());
    }
    use std::os::unix::fs::MetadataExt as _;
    let current = dir.join(logging::FILE);
    let rotated = dir.join(format!("{}.1", logging::FILE));
    let identity = |path: &Path| fs::metadata(path).ok().map(|m| (m.ino(), m.len()));
    let (mut inode, mut offset) = identity(&current).unwrap_or_default();
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let Some((now_inode, len)) = identity(&current) else {
            continue;
        };
        if now_inode != inode {
            if identity(&rotated).is_some_and(|(old, _)| old == inode) {
                for record in tail_records(&rotated, offset)?.0 {
                    filter
                        .matches(&record)
                        .then(|| print(&record))
                        .transpose()?;
                }
            }
            (inode, offset) = (now_inode, 0);
        }
        if len > offset {
            let (records, read) = tail_records(&current, offset)?;
            offset += read;
            for record in records {
                filter
                    .matches(&record)
                    .then(|| print(&record))
                    .transpose()?;
            }
        }
    }
}

/// Complete records after `offset`, and how many bytes they took; a half-written line waits.
fn tail_records(path: &Path, offset: u64) -> Result<(Vec<vibeface::logging::Record>, u64)> {
    use std::io::Seek as _;
    let mut file = fs::File::open(path)?;
    file.seek(std::io::SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    let records = bytes[..complete]
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect();
    Ok((records, complete as u64))
}

fn since_ms(text: &str) -> Result<i64> {
    let (number, unit) = text.split_at(text.trim_end_matches(char::is_alphabetic).len());
    let number: i64 = number
        .parse()
        .context("--since is like 30m, 2h, 1d or a time in ms")?;
    let unit_ms = match unit {
        "" => return Ok(number),
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => bail!("--since unit is s, m, h or d"),
    };
    Ok(vibeface::runtime::now_ms() - number * unit_ms)
}

fn log_line(record: &vibeface::logging::Record) -> String {
    let mut line = format!(
        "{} {:<5} {:<6} {} {}",
        utc_time(record.ts_ms),
        format!("{:?}", record.level).to_uppercase(),
        format!("{:?}", record.source).to_lowercase(),
        record.event,
        record.message
    );
    for (name, id) in [
        ("conversation", &record.conversation_id),
        ("work", &record.work_id),
        ("thread", &record.thread_id),
    ] {
        if let Some(id) = id {
            line.push_str(&format!(" {name}={id}"));
        }
    }
    if !record.fields.is_empty() {
        line.push(' ');
        line.push_str(&Value::Object(record.fields.clone()).to_string());
    }
    line
}

/// `2026-10-21T03:20:06.123Z`.
fn utc_time(ms: i64) -> String {
    let (days, rest) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // Howard Hinnant's days-to-civil conversion.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

async fn post_triage(local: &LocalNode, mut body: serde_json::Value) -> Result<()> {
    if let Ok(attempt) = std::env::var("VIBEFACE_TRIAGE_ATTEMPT") {
        body["_attempt"] = json!(attempt);
    }
    let result = post_local(
        &local_node_url(local)?,
        "api/triage",
        &body,
        Duration::from_secs(30),
    )
    .await?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

/// Fails before any file is written, so a busy port never leaves a half-made data directory.
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
                "listen":configuration.listen, "public_entry":configuration.public_entry,
                "web_enabled":health.as_ref().map_or(configuration.web_enabled, |health| health.web_enabled),
                "version":health.as_ref().ok().map(|health| &health.version),
                "write_state":health.as_ref().ok().and_then(|health| health.write_state),
                "peers":health.as_ref().ok().map(|health| &health.peers),
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
            println!(
                "Web: {}",
                if health.web_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            if let Some(state) = health.write_state {
                println!("Writes: {}", state.as_str());
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |now| now.as_millis() as i64);
            for peer in &health.peers {
                let link = match peer.reachable {
                    Some(true) => "reachable",
                    Some(false) => "unreachable",
                    None => "not tried yet",
                };
                let synced = peer.synced_ms.map_or("never synced".into(), |at| {
                    format!("synced {}s ago", now.saturating_sub(at) / 1000)
                });
                println!("Peer: {} ({}) {link}, {synced}", peer.name, peer.device_id);
            }
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
    configuration.web_enabled = !options.no_web;
    configuration.local_auto_access = !options.no_local_auto_access;
    configuration.providers.codex_app_server.socket = options.codex_socket;
    configuration.bins = options.bins.into_iter().collect();
    if let Some(url) = options.public_url {
        configuration.public_url = origin(&url)?.to_string().trim_end_matches('/').to_owned();
    } else {
        ensure!(
            configuration.listen.ip().is_loopback(),
            "set --public-url when listening beyond loopback"
        );
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
    if options.relay_mode == RelayServiceMode::Forward && options.relay_listen.is_some() {
        ensure!(
            options.relay_url.is_some(),
            "--relay-listen requires --relay-url for a forwarding relay"
        );
    }
    configuration.network = NetworkConfig {
        relays: options.relays,
        discovery: options.discovery,
        relay_mode: options.relay_mode,
        relay_listen: options.relay_listen,
        relay_url: options.relay_url,
        entry,
        ..NetworkConfig::default()
    };
    configuration.public_entry = options.public_entry;
    let entry_token = match &options.relay_token_file {
        Some(path) => Some(net::read_token_file(path)?),
        None => (options.public_entry && options.relay_mode == RelayServiceMode::Forward)
            .then(|| random_hex(32)),
    };
    if let Some(manager) = options.manager {
        let workspace = options
            .workspace
            .context("--manager requires --workspace")?;
        configuration.manager = Some(ManagerSettings {
            provider: manager,
            workspace: workspace
                .canonicalize()
                .context("resolve manager workspace")?,
            model: manager.model(options.model.as_deref().unwrap_or_default()),
        });
    }
    let mut env = vibeface::providers::ProviderEnv::default();
    vibeface::discovery::complete(
        &mut configuration,
        &mut env,
        &vibeface::discovery::Host::current(),
    )?;
    initialize_announced(dir, configuration, entry_token)?;
    if env != vibeface::providers::ProviderEnv::default() {
        env.save(dir)?;
    }
    Ok(())
}

fn initialize_announced(
    dir: &Path,
    configuration: Config,
    entry_token: Option<String>,
) -> Result<()> {
    let public_entry = configuration.public_entry;
    let discovery_only = configuration.network.relay_mode == RelayServiceMode::DiscoveryOnly;
    let web_enabled = configuration.web_enabled;
    let local = !public_entry
        && (configuration.listen.ip().is_loopback() || configuration.listen.ip().is_unspecified());
    let open = if local {
        format!("http://localhost:{}", configuration.listen.port())
    } else {
        configuration.public_url.clone()
    };
    initialize(dir, configuration, entry_token)?;
    println!("Vibeface 已就绪");
    if web_enabled {
        println!("打开 {open}");
    }
    if discovery_only {
        println!(
            "Discovery device ID: {}",
            config::identity(dir)?.device_id()?
        );
    } else if public_entry {
        println!(
            "Relay token: {} (copy it to your Node and pass --relay-token-file)",
            dir.join(net::ENTRY_TOKEN_FILE).display()
        );
    }
    Ok(())
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
    create_private(
        &dir.join("data-epoch.json"),
        &crate::config::data_epoch_marker(),
    )
}

/// Restarts this removed Node fresh. Replacing the process image keeps its PID, so a service
/// manager sees no exit and a Node started by hand keeps running; nothing of the old runtime
/// survives to write into the data set aside.
fn restart_fresh(dir: &Path) -> Result<()> {
    match start_over(dir) {
        Ok(archive) => {
            tracing::info!(event = "node.data_set_aside", archive = %archive.display(), "removed Node data set aside")
        }
        Err(error) => {
            tracing::error!(event = "node.data_set_aside_failed", %error, "removed Node data could not be set aside")
        }
    }
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(std::env::current_exe()?)
        .args(std::env::args_os().skip(1))
        .exec();
    Err(error).context("restart the Node")
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
    let stamp = local_time_stamp();
    let archive = dir.with_file_name(format!("{name}-removed-{stamp}"));
    let fresh = dir.with_file_name(format!(".{}-fresh-{stamp}", name.trim_start_matches('.')));
    ensure!(
        !archive.exists() && !fresh.exists(),
        "{} already exists",
        archive.display()
    );
    let made = initialize(&fresh, configuration, entry_token);
    if let Err(error) = made {
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

fn local_time_stamp() -> String {
    let mut buffer = [0u8; 32];
    // SAFETY: `localtime_r` fills `tm` from `now`; `strftime` writes at most `buffer.len()` bytes.
    let written = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        libc::strftime(
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            c"%Y%m%d-%H%M%S".as_ptr(),
            &tm,
        )
    };
    String::from_utf8_lossy(&buffer[..written]).into_owned()
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
    config::http_origin(value).context("invalid node URL")
}

fn provider_bin(value: &str) -> Result<(Provider, PathBuf), String> {
    let (provider, path) = value.split_once('=').ok_or("use PROVIDER=PATH")?;
    Ok((ValueEnum::from_str(provider, false)?, path.into()))
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

async fn access(entry: LocalEntry) -> Result<()> {
    let options = entry.access;
    let dir = &options.data.data_dir;
    let epoch: Value = serde_json::from_slice(&read_bounded(&dir.join("data-epoch.json"), 4096)?)?;
    ensure!(
        epoch == json!({"data_epoch":crate::config::DATA_EPOCH}),
        "data directory uses another format; existing data was preserved"
    );
    let mut configuration = Config::load(dir)?;
    if entry.enable || entry.disable {
        ensure!(
            !configuration.public_entry,
            "a public entry has no local browser access"
        );
        let running = set_local_access(dir, entry.enable).await?;
        if entry.disable || !running {
            let state = if entry.enable { "on" } else { "off" };
            let note = if running {
                ""
            } else {
                "; the node applies it when it starts"
            };
            println!("local browser access without confirmation is {state}{note}");
            return Ok(());
        }
        configuration = Config::load(dir)?;
    }
    ensure!(
        configuration.web_enabled
            && maintenance::probe(dir)
                .await
                .map_or(true, |health| health.web_enabled),
        "browser access is disabled; restart without --no-web or enable web_enabled in config.json"
    );
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
        // The link opens the localhost page, which keeps a browser already joined there and
        // sends only an unjoined one on to the launcher origin's temporary session.
        let listen = configuration.listen.ip();
        if listen.is_loopback() || listen.is_unspecified() {
            url = origin(&format!("http://localhost:{}", configuration.listen.port()))?;
        }
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

/// Tells the running Node, which saves and applies it at once; a stopped Node only has its
/// configuration changed. Returns whether a Node was running.
async fn set_local_access(dir: &Path, enabled: bool) -> Result<bool> {
    match maintenance::set_local_access(dir, enabled).await {
        Ok(()) => Ok(true),
        Err(error)
            if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                )
            }) =>
        {
            let mut configuration = Config::load(dir)?;
            configuration.local_auto_access = enabled;
            configuration.save(dir)?;
            Ok(false)
        }
        Err(error) => Err(error),
    }
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
    prepare_start(&options.data.data_dir)?;
    if node_running(&options.data.data_dir)? {
        return join_through_running_node(&options).await;
    }
    vibeface::client::join_network(&options.data.data_dir, &options.link, &options.name).await?;
    println!("已加入。运行 vibeface 后就能连接。");
    Ok(())
}

fn node_running(dir: &Path) -> Result<bool> {
    // Without an identity no Node runs here, and opening the data directory says so itself.
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

/// The running Node owns this computer's pairing state, so it runs the join; this command asks
/// it to start and then follows along. Every ask carries the same link, which the Node reads as
/// "how far is it", never as a second request.
async fn join_through_running_node(options: &Join) -> Result<()> {
    use vibeface::client::NodeJoin;
    const UNREACHABLE_TRIES: u32 = 5;
    let dir = &options.data.data_dir;
    let mut shown = None;
    let mut unreachable = 0;
    println!("这台电脑上的 Vibeface 正在运行，交给它来加入。");
    loop {
        let status = match maintenance::join(dir, &options.link, &options.name).await {
            Ok(status) => status,
            Err(error) if error.downcast_ref::<maintenance::Busy>().is_some() => {
                bail!("Vibeface 正在更新。等它重新启动后，再运行同一条命令。")
            }
            Err(error) if error.downcast_ref::<maintenance::Refused>().is_some() => {
                bail!(join_failure_text(&error.to_string()))
            }
            Err(error) => {
                unreachable += 1;
                if unreachable < UNREACHABLE_TRIES {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                bail!(
                    "连不上正在运行的 Vibeface（{error}）。\n\
                     加入可能还在进行：确认它在运行后，再运行同一条命令，会接着上次的请求，不会重复提交。"
                )
            }
        };
        unreachable = 0;
        if shown.as_ref() != Some(&status) {
            match &status {
                NodeJoin::Waiting => println!("已提交加入请求，等待管理设备批准。"),
                NodeJoin::Confirming { code, admin } => {
                    println!("安全码：{code}。在「{admin}」上比对一致后允许。")
                }
                NodeJoin::Joined => {
                    println!("已加入。这台电脑的 Vibeface 已开始同步，不用重启。");
                    return Ok(());
                }
                NodeJoin::Failed { reason } => bail!(join_failure_text(reason)),
                NodeJoin::Cancelled => bail!("这次加入已在这台电脑的网页上取消。"),
            }
            shown = Some(status);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Says why a join ended and what to do next, in the words the rest of `join` uses.
fn join_failure_text(reason: &str) -> String {
    const TABLE: [(&str, &str); 10] = [
        (
            "already been used",
            "这条加入链接已经用过了。在管理设备上点“添加设备”重新生成，再运行新命令。",
        ),
        (
            "expired",
            "这条加入链接已过期。在管理设备上点“添加设备”重新生成，再运行新命令。",
        ),
        (
            "Rejected",
            "管理设备拒绝了这次加入。确认是自己的电脑后，在管理设备上重新生成链接再试。",
        ),
        (
            "Withdrawn",
            "这次加入已撤回。在管理设备上重新生成链接再试。",
        ),
        (
            "approval timed out",
            "等了很久也没有批准。在管理设备上重新生成链接再试。",
        ),
        (
            "already belongs to a personal network",
            "这台电脑已经在一个网络里了，不用再加入。",
        ),
        (
            "another join is already running",
            "这台电脑正在它的网页上加入。在网页上完成，或先在网页上取消。",
        ),
        (
            "another join is in progress",
            "这条链接上次用的是另一个名字。用上次的 --name 重新运行，会接着等批准。",
        ),
        (
            "invite link",
            "这不是有效的加入链接。在管理设备上点“添加设备”重新复制。",
        ),
        (
            "Vibeface ticket",
            "这不是有效的加入链接。在管理设备上点“添加设备”重新复制。",
        ),
    ];
    TABLE
        .iter()
        .find(|(pattern, _)| reason.contains(pattern))
        .map_or_else(
            || {
                format!(
                    "没能加入：{reason}\n检查网络后再运行同一条命令，会接着上次的请求，不会重复提交。"
                )
            },
            |(_, text)| (*text).to_owned(),
        )
}

async fn call_tool(name: String, arguments: String, call_ref: Option<String>) -> Result<()> {
    ensure!(!name.is_empty() && name.len() <= 128, "invalid tool name");
    let mut arguments: Value =
        serde_json::from_str(&arguments).context("invalid tool arguments JSON")?;
    ensure!(
        arguments.is_object(),
        "tool arguments must be a JSON object"
    );
    let tool = vibeface::tools::Tool::parse(&name).context("unknown coordination tool")?;
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
        !tool.needs_call_ref() || call_ref.is_some(),
        "{name} requires --call-ref; keep it and query call_lookup after any uncertain result"
    );
    if let Some(call_ref) = &call_ref {
        arguments["call_ref"] = json!(call_ref);
    }
    let call_id = call_ref.unwrap_or_default();
    let result = if tool == vibeface::tools::Tool::DispatchWork {
        let socket = std::env::var_os("VIBEFACE_CODEX_SOCKET")
            .context("VIBEFACE_CODEX_SOCKET is required")?;
        dispatch_work(&arguments, &call_id, Path::new(&socket)).await?
    } else {
        post_tool(&name, arguments, &call_id).await?
    };
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

/// `dispatch_work` and `codex send`: the Node tracks and watches the work; the CLI sends it.
async fn dispatch_work(arguments: &Value, call_id: &str, socket: &Path) -> Result<Value> {
    vibeface::agent_cli::dispatch(
        arguments,
        |args| post_tool("track_work", args, call_id),
        |work_id, report| {
            post_tool(
                "bind_work",
                json!({"work_id":work_id,"report":report}),
                call_id,
            )
        },
        |args| async move {
            let asked = json!({"agent":args["agent"],"thread":args["thread"],"continues":args["continues"]});
            let ready = post_tool("wait_work", asked, call_id).await?;
            Ok(ready["agent"].as_str().map(str::to_owned))
        },
        socket,
    )
    .await
}

async fn post_tool(name: &str, arguments: Value, call_id: &str) -> Result<Value> {
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
        .post(url.join(api::TOOL_PATH.trim_start_matches('/'))?)
        .bearer_auth(token)
        .json(&json!({"toolName": name, "args": arguments, "agentId": "cli"}))
        .send()
        .await
        .with_context(|| format!("reach local node tool API; preserve call_ref {call_id} and query call_lookup before retrying"))?;
    let status = response.status();
    let body = bounded_body(response).await?;
    if !status.is_success() {
        let reason = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|error| error["error"].as_str().map(str::to_owned))
            .unwrap_or_default();
        bail!("node tool rejected with HTTP {status}: {reason}; preserve call_ref {call_id}");
    }
    let mut reply: Value = serde_json::from_slice(&body).context("invalid node tool response")?;
    Ok(reply["result"].take())
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

fn local_node_url(local: &LocalNode) -> Result<Url> {
    let url = match &local.node_url {
        Some(url) => url.clone(),
        None => match std::env::var(vibeface::runtime::TRIAGE_URL_ENV) {
            Ok(url) => url,
            Err(_) => format!(
                "http://{}",
                Config::load(&local.data.data_dir)
                    .map_or(config::DEFAULT_LOCAL_LISTEN, |c| c.local_listen)
            ),
        },
    };
    let url = origin(&url)?;
    ensure!(
        is_loopback(&url),
        "the local Node URL must be a loopback address"
    );
    Ok(url)
}

async fn post_local(url: &Url, path: &str, body: &Value, total: Duration) -> Result<Value> {
    let response = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(total)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(url.join(path)?)
        .json(body)
        .send()
        .await
        .context("the local Node is unreachable")?;
    let status = response.status();
    let value: Value =
        serde_json::from_slice(&bounded_body(response).await?).unwrap_or(Value::Null);
    ensure!(
        status.is_success(),
        "the Node refused it (HTTP {status}): {}",
        value["error"].as_str().unwrap_or_default()
    );
    Ok(value)
}

fn git(cwd: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && !text.is_empty()).then_some(text)
}

/// The event a provider's hook input describes. `None` for a hook the Node does not read and
/// for a session nobody drives: Codex tells by its rollout's first line, Claude Code by the
/// entry point it hands its hooks.
fn hook_event(agent: Provider, input: &Value) -> Result<Option<vibeface::triage::AgentEvent>> {
    use vibeface::triage;
    Ok(match agent {
        Provider::Codex => {
            let first = input["transcript_path"]
                .as_str()
                .and_then(|path| fs::File::open(path).ok())
                .and_then(|file| {
                    let mut line = String::new();
                    std::io::BufRead::read_line(
                        &mut std::io::BufReader::new(file.take(1 << 20)),
                        &mut line,
                    )
                    .ok()
                    .map(|_| line)
                })
                .unwrap_or_default();
            let meta = triage::rollout_meta(&first);
            triage::interactive_codex(&meta["source"], meta["originator"].as_str())
                .then(|| triage::from_codex_hook(input))
                .flatten()
        }
        Provider::Claude => {
            triage::interactive_claude(std::env::var("CLAUDE_CODE_ENTRYPOINT").ok().as_deref())
                .then(|| {
                    triage::from_claude_hook(
                        input,
                        input["transcript_path"]
                            .as_str()
                            .and_then(|path| fs::metadata(path).ok())
                            .map_or(0, |metadata| metadata.len()),
                    )
                })
                .flatten()
        }
        // Its extension runs only in sessions the user drives.
        Provider::Pi => triage::from_pi_extension(input),
        // Its plugin stays out of the Node's own serve.
        Provider::Opencode => triage::from_opencode_plugin(input),
        Provider::Cursor => bail!("{} has no hooks", agent.name()),
    })
}

async fn hook(options: Hook) -> Result<()> {
    use vibeface::triage::{self, AgentEvent};
    let (mut event, input) = if options.test {
        let event = AgentEvent {
            source: triage::Source::Test,
            agent: options.agent,
            session_id: "test".into(),
            turn_id: uuid::Uuid::new_v4().to_string(),
            cwd: std::env::current_dir()?.to_string_lossy().into(),
            test: true,
            ..Default::default()
        };
        (event, Value::Null)
    } else {
        let mut input = Vec::new();
        std::io::stdin().take(1 << 20).read_to_end(&mut input)?;
        let input: Value = serde_json::from_slice(&input).context("hook input is not JSON")?;
        let Some(event) = hook_event(options.agent, &input)? else {
            return Ok(());
        };
        (event, input)
    };
    if !event.cwd.is_empty() {
        let top = git(&event.cwd, &["rev-parse", "--show-toplevel"]);
        event.repo = top.and_then(|top| {
            Path::new(&top)
                .file_name()
                .map(|n| n.to_string_lossy().into())
        });
        event.git_branch = git(&event.cwd, &["rev-parse", "--abbrev-ref", "HEAD"]);
        event.git_head = git(&event.cwd, &["rev-parse", "HEAD"]);
    }
    let body = serde_json::to_value(&event)?;
    if options.dry_run {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    let sent = async {
        post_local(
            &local_node_url(&options.local)?,
            "api/hooks",
            &body,
            Duration::from_secs(5),
        )
        .await
    }
    .await;
    match sent {
        Ok(result) if options.test => println!("{result}"),
        // The session waits on the request this event holds: print its answer for the session,
        // or nothing so that the session decides.
        Ok(result) => {
            if let Some(request_id) = result["request_id"].as_str() {
                let answer = post_local(
                    &local_node_url(&options.local)?,
                    "api/hooks/wait",
                    &json!({"request_id":request_id}),
                    vibeface::runtime::HOOK_ANSWER_WITHIN + Duration::from_secs(10),
                )
                .await?;
                let output = match options.agent {
                    Provider::Claude => vibeface::runtime::claude::hook_output(&input, &answer),
                    _ => (answer != json!({"decision":null})).then_some(answer),
                };
                if let Some(output) = output {
                    println!("{}", serde_json::to_string(&output)?);
                }
            }
        }
        Err(error) => {
            // Nobody waits for the answer to a spooled event.
            let mut body = body;
            body["hold"] = serde_json::Value::Null;
            let spool = options
                .local
                .data
                .data_dir
                .join(vibeface::runtime::HOOK_SPOOL);
            config::private_dir(&spool)?;
            config::private_write(
                &spool.join(format!("{}.json", uuid::Uuid::new_v4())),
                &serde_json::to_vec(&body)?,
            )?;
            return Err(error.context("spooled for the Node's next scan"));
        }
    }
    Ok(())
}

/// The home folder `vibeface hooks` edits and Codex's folder in it. `--home` holds every file,
/// Codex's too; without it, `$HOME`, and Codex's folder is found as Codex finds it: `CODEX_HOME`,
/// which must be an existing folder, else `$HOME/.codex`.
fn hook_homes(home: Option<&Path>, host: &vibeface::discovery::Host) -> Result<(PathBuf, PathBuf)> {
    if let Some(home) = home {
        return Ok((home.to_owned(), home.join(".codex")));
    }
    let home = host
        .home
        .clone()
        .filter(|home| home.is_absolute())
        .context("HOME is not an absolute path; pass --home")?;
    let codex_home = match host.codex_home.clone() {
        Some(codex_home) => {
            ensure!(
                codex_home.is_absolute() && codex_home.is_dir(),
                "CODEX_HOME {} is not an existing absolute folder",
                codex_home.display()
            );
            codex_home
        }
        None => home.join(".codex"),
    };
    Ok((home, codex_home))
}

async fn hooks(options: Hooks, host: vibeface::discovery::Host) -> Result<()> {
    use vibeface::triage;
    if let HooksAction::Test = options.action {
        let result = post_local(
            &local_node_url(&options.local)?,
            "api/hooks",
            &json!({"source":"test","session_id":"test","turn_id":uuid::Uuid::new_v4().to_string(),"test":true}),
            Duration::from_secs(5),
        )
        .await?;
        ensure!(
            result["status"] == "accepted",
            "unexpected answer: {result}"
        );
        println!("Node received the test event {}", result["event_id"]);
        return Ok(());
    }
    let (home, codex_home) = hook_homes(options.home.as_deref(), &host)?;
    let program = std::env::current_exe()?;
    let data_dir = match options.local.data.data_dir.canonicalize() {
        Ok(dir) => dir,
        Err(_) => options.local.data.data_dir.clone(),
    };
    let stamp = vibeface::runtime::now_ms();
    // Every file is read and checked before any is written: one that cannot change leaves all.
    let mut planned = Vec::new();
    for file in triage::hook_files(&home, &codex_home) {
        let mut settings: Value = match fs::read(&file.path) {
            Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => Value::Null,
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
                format!("{} is not valid JSON; left unchanged", file.path.display())
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
            Err(error) => return Err(error.into()),
        };
        let command = triage::hook_command(&program, file.agent, &data_dir);
        let changed = match options.action {
            HooksAction::Install => triage::add_hooks(&mut settings, &file, &command)?,
            HooksAction::Uninstall => triage::remove_hooks(&mut settings, &file),
            HooksAction::Status | HooksAction::Test => {
                let events = triage::installed(&settings, &file);
                println!(
                    "{}: {}",
                    file.path.display(),
                    if events.is_empty() {
                        "not installed".into()
                    } else {
                        events.join(", ")
                    }
                );
                continue;
            }
        };
        planned.push((file.path, settings, changed, command));
    }
    let extensions = extensions(&options, &home, &program, &data_dir)?;
    for (path, settings, changed, command) in planned {
        if changed.is_empty() {
            println!("{}: unchanged", path.display());
            continue;
        }
        if options.dry_run {
            println!(
                "{}: would change {} with `{command}`",
                path.display(),
                changed.join(", ")
            );
            continue;
        }
        if path.exists() {
            let backup = path.with_file_name(format!(
                "{}.bak-vibeface-{stamp}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            fs::copy(&path, &backup)?;
            println!("{}: backed up to {}", path.display(), backup.display());
        } else if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, serde_json::to_vec_pretty(&settings)?)?;
        println!("{}: changed {}", path.display(), changed.join(", "));
    }
    for (path, step) in extensions {
        let shown = path.display();
        match step {
            Step::Unchanged => println!("{shown}: unchanged"),
            Step::Write(_) if options.dry_run => println!("{shown}: would write"),
            Step::Write(code) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&path, code)?;
                println!("{shown}: written; sessions already open load it after a restart");
            }
            Step::Remove if options.dry_run => println!("{shown}: would remove"),
            Step::Remove => {
                fs::remove_file(&path)?;
                println!("{shown}: removed");
            }
        }
    }
    if let HooksAction::Install = options.action {
        let config = codex_home.join("config.toml");
        let enabled = fs::read_to_string(&config)
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
            .is_some_and(|table| {
                table
                    .get("features")
                    .and_then(|f| f.get("hooks"))
                    .and_then(|h| h.as_bool())
                    == Some(true)
            });
        if !enabled {
            println!(
                "Codex runs hooks only with `[features] hooks = true` in {}; this command does not edit it.",
                config.display()
            );
        }
        println!(
            "Codex asks to trust a new hook before its first run; sessions already open pick hooks up after a restart or resume."
        );
    }
    Ok(())
}

/// What `vibeface hooks` does to one extension file.
enum Step {
    Unchanged,
    Write(String),
    Remove,
}

/// The extension files of `vibeface hooks` and what to do to each: written whole, and removed
/// only when the Node wrote them. Status is printed here; nothing is written.
fn extensions(
    options: &Hooks,
    home: &Path,
    program: &Path,
    data_dir: &Path,
) -> Result<Vec<(PathBuf, Step)>> {
    use vibeface::providers::Watch;
    let mut steps = Vec::new();
    for agent in Provider::ALL {
        let Some(Watch::Extension { file, source }) = agent.watch() else {
            continue;
        };
        let path = home.join(file);
        let current = match fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let ours = current
            .as_deref()
            .is_some_and(|text| text.lines().next() == source.lines().next());
        let code = vibeface::triage::extension_code(source, program, agent, data_dir);
        let step = match options.action {
            HooksAction::Status | HooksAction::Test => {
                println!(
                    "{}: {}",
                    path.display(),
                    if ours { "installed" } else { "not installed" }
                );
                continue;
            }
            HooksAction::Install if current.as_deref() == Some(code.as_str()) => Step::Unchanged,
            HooksAction::Install if current.is_some() && !ours => {
                bail!(
                    "{} was not written by vibeface; nothing was changed",
                    path.display()
                )
            }
            HooksAction::Install => Step::Write(code),
            HooksAction::Uninstall if !ours => Step::Unchanged,
            HooksAction::Uninstall => Step::Remove,
        };
        steps.push((path, step));
    }
    Ok(steps)
}

async fn triage(options: Triage) -> Result<()> {
    let body = match options.action {
        TriageAction::Events { since } => json!({"action":"events","since":since}),
        TriageAction::Raise {
            event_id,
            severity,
            question,
            request_id,
            choices,
        } => {
            json!({"action":"raise","event_id":event_id,"severity":severity,"question":question,
                "request_id":request_id,"choices":choices})
        }
        TriageAction::Dismiss { event_id } => json!({"action":"dismiss","event_id":event_id}),
    };
    post_triage(&options.local, body).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches};

    #[test]
    fn receiving_key_commands_require_an_explicit_stable_intent() {
        let intent = uuid::Uuid::new_v4().to_string();
        let data_dir = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for action in ["register-key", "rotate-key", "revoke-key"] {
            assert!(
                Cli::try_parse_from([
                    "vibeface",
                    "wallet",
                    "--data-dir",
                    &data_dir,
                    action,
                    "--gas-cap",
                    "1000000"
                ])
                .is_err()
            );
            assert!(
                Cli::try_parse_from([
                    "vibeface",
                    "wallet",
                    "--data-dir",
                    &data_dir,
                    action,
                    "--intent-id",
                    &intent,
                    "--gas-cap",
                    "1000000"
                ])
                .is_ok()
            );
        }
    }

    #[test]
    fn wallet_passphrase_requires_a_protected_descriptor_and_cli_never_accepts_it_as_text()
    -> Result<()> {
        use std::os::{
            fd::AsRawFd,
            unix::fs::{OpenOptionsExt, PermissionsExt},
        };
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/wallet-cli-tests")
            .join(uuid::Uuid::new_v4().to_string());
        config::private_dir(&dir)?;
        let path = dir.join("synthetic-passphrase");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(b"synthetic test passphrase\n")?;
        drop(file);
        let file = fs::File::open(&path)?;
        read_wallet_passphrase(Some(file.as_raw_fd()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        let file = fs::File::open(&path)?;
        assert!(read_wallet_passphrase(Some(file.as_raw_fd())).is_err());
        let data_dir = dir.to_string_lossy();
        assert!(
            Cli::try_parse_from([
                "vibeface",
                "wallet",
                "--data-dir",
                &data_dir,
                "unlock",
                "--passphrase",
                "text"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "vibeface",
                "wallet",
                "--data-dir",
                &data_dir,
                "unlock",
                "--passphrase-fd",
                "3"
            ])
            .is_ok()
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn codex_call_refuses_turn_writes_before_reaching_the_app_server() {
        use vibeface::agent_cli::{Command, run};
        let socket = Path::new("/nonexistent/vibeface-codex.sock");
        for method in [
            "turn/start",
            "turn/steer",
            "turn/interrupt",
            "thread/start",
            "review/start",
        ] {
            let refused = run(
                socket,
                Command::Call {
                    method: method.into(),
                    params: None,
                },
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(
                refused.contains(method) && refused.contains("codex send"),
                "{refused}"
            );
        }
        let read = run(
            socket,
            Command::Call {
                method: "thread/list".into(),
                params: None,
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(!read.contains("codex send"), "{read}");
    }

    #[test]
    fn data_directory_defaults_to_home_unless_named() {
        let home = || Some("/home/me".into());
        let direct = |argument: Option<&str>, env: Option<&str>, home| {
            config::data_dir_or_home(argument.or(env).map(PathBuf::from), home)
        };
        assert_eq!(
            direct(Some("/arg"), Some("/env"), home()).unwrap(),
            Path::new("/arg")
        );
        assert_eq!(
            direct(None, Some("/env"), home()).unwrap(),
            Path::new("/env")
        );
        assert_eq!(
            direct(None, None, home()).unwrap(),
            Path::new("/home/me/.vibeface")
        );
        for home in [None, Some("relative".into())] {
            assert_eq!(
                direct(None, None, home).unwrap_err().to_string(),
                config::NO_HOME
            );
        }
        assert!(direct(None, Some("relative"), home()).is_err());

        fn isolated(
            command: clap::Command,
            path: &str,
            home: Option<&'static str>,
        ) -> clap::Command {
            let names: Vec<String> = command
                .get_subcommands()
                .map(|sub| sub.get_name().to_owned())
                .collect();
            let command = command.mut_args(|arg| {
                if arg.get_id() != "data_dir" || path == "vibeface" {
                    return arg;
                }
                assert_eq!(arg.get_env().unwrap(), "VIBEFACE_DATA_DIR", "{path}");
                let arg = arg.env(None::<&'static str>);
                match home {
                    Some(value) if !arg.get_default_values().is_empty() => arg.default_value(value),
                    _ => arg,
                }
            });
            names.iter().fold(command, |command, name| {
                command.mut_subcommand(name, |sub| isolated(sub, &format!("{path} {name}"), home))
            })
        }
        let parse = |home, arguments: &[&str]| {
            isolated(Cli::command(), "vibeface", home)
                .try_get_matches_from(arguments)
                .and_then(|matches| Cli::from_arg_matches(&matches))
        };
        let Some(Action::Serve(serve)) = parse(None, &["vibeface", "serve"]).unwrap().command
        else {
            panic!("serve");
        };
        assert_eq!(serve.data.data_dir, config::default_data_dir());

        let no_home = Some("");
        for arguments in [
            &["vibeface", "status", "--data-dir", "/d"][..],
            &["vibeface", "update", "status", "--data-dir", "/d"],
            &[
                "vibeface",
                "join",
                "vibeface1x",
                "--name",
                "n",
                "--data-dir",
                "/d",
            ],
            &["vibeface", "service", "status", "--data-dir", "/d"],
            &["vibeface", "service", "--data-dir", "/d", "status"],
        ] {
            assert!(parse(no_home, arguments).is_ok(), "{arguments:?}");
        }
        for arguments in [
            &["vibeface", "status"][..],
            &["vibeface", "update", "status"],
        ] {
            let error = parse(no_home, arguments).err().unwrap().to_string();
            assert!(error.contains(config::NO_HOME), "{arguments:?}: {error}");
        }
    }

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
        assert!(!default.no_web && !options.no_web);
        assert!(
            Cli::try_parse_from(["vibeface", "--data-dir", "/data/node", "--no-web"])
                .unwrap()
                .no_web
        );
        let Some(Action::Serve(headless)) =
            Cli::try_parse_from(["vibeface", "serve", "--data-dir", "/data/node", "--no-web"])
                .unwrap()
                .command
        else {
            panic!("expected serve")
        };
        assert!(headless.no_web);
        for value in ["", "relative-node"] {
            for command in [
                vec![],
                vec!["serve"],
                vec!["service", "status"],
                vec!["update", "status"],
            ] {
                let mut arguments = vec!["vibeface"];
                arguments.extend(command);
                arguments.extend(["--data-dir", value]);
                assert!(Cli::try_parse_from(arguments).is_err());
            }
        }
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
            "--relay-url",
            "https://relay.test",
            "--no-web",
        ])?;
        let Some(Action::Init(options)) = cli.command else {
            bail!("expected init command")
        };
        init(*options)?;
        let configuration = Config::load(&dir)?;
        assert!(
            !configuration.web_enabled,
            "init --no-web persists the headless service setting"
        );
        let network = configuration.network;
        assert_eq!(network.relays, ["https://relay.test"]);
        assert_eq!(network.relay_listen, Some("127.0.0.1:0".parse()?));
        assert!(
            Cli::try_parse_from([
                "vibeface",
                "init",
                "--data-dir",
                "/data/node",
                "--relay-paths-only"
            ])
            .is_err()
        );
        assert!(serde_json::from_value::<NetworkConfig>(json!({"relay_paths_only":true})).is_err());
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

    #[tokio::test]
    async fn join_initializes_missing_and_empty_directories_only() {
        let root = std::env::temp_dir().join(format!("vibeface-join-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let attempt = |data_dir: PathBuf| {
            join(Join {
                link: "not-an-invite".into(),
                data: DataDir { data_dir },
                name: "我的电脑".into(),
            })
        };
        fs::create_dir(root.join("empty")).unwrap();
        for name in ["missing", "empty"] {
            let dir = root.join(name);
            let error = attempt(dir.clone()).await.unwrap_err();
            assert!(
                error.downcast_ref::<std::io::Error>().is_none(),
                "{error:#}"
            );
            Runtime::open(&dir).await.unwrap();
        }
        let uninitialized = root.join("uninitialized");
        fs::create_dir(&uninitialized).unwrap();
        fs::write(uninitialized.join("existing-data"), b"preserve me").unwrap();
        assert!(attempt(uninitialized.clone()).await.is_err());
        assert!(!uninitialized.join("identity.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn join_while_the_node_runs_hands_the_link_to_it_and_explains_its_answer() {
        let dir = std::env::temp_dir().join(format!("vibeface-join-busy-{}", uuid::Uuid::new_v4()));
        prepare_start(&dir).unwrap();
        let node = Arc::new(Runtime::open(&dir).await.unwrap());
        let server = tokio::spawn(maintenance::serve(node.clone(), dir.clone()));
        let socket = maintenance::socket_path(&dir).unwrap();
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let error = join(Join {
            link: "https://gateway.example/join#not-a-ticket".into(),
            data: DataDir {
                data_dir: dir.clone(),
            },
            name: "我的电脑".into(),
        })
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("不是有效的加入链接"),
            "the running Node read the link and its refusal reads as what to do: {error:#}"
        );
        server.abort();
        let _ = server.await;
        drop(node);
        let _ = fs::remove_file(&socket);
        let _ = fs::remove_dir_all(socket.parent().unwrap());
        fs::remove_dir_all(dir).unwrap();
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
                "Build Node",
                "--data-dir",
                "/data/node"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "vibeface",
                "join",
                "https://node.example/join#vibeface1abc",
                "--data-dir",
                "/data/node"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from(["vibeface", "access", "--json", "--data-dir", "/data/node"])
                .is_ok()
        );
        assert!(
            Cli::try_parse_from(["vibeface", "access", "--enable", "--data-dir", "/data/node"])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["vibeface", "access", "--enable", "--disable"]).is_err());
        assert!(Cli::try_parse_from(["vibeface", "status", "--enable"]).is_err());
        assert!(
            Cli::try_parse_from(["vibeface", "status", "--json", "--data-dir", "/data/node"])
                .is_ok()
        );
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
                "--data-dir",
                "/data/node",
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
                "--data-dir",
                "/data/node",
                "--manager",
                "cursor",
                "--workspace",
                ".",
                "--bin",
                "cursor=/opt/bin/cursor-sdk-bridge"
            ])
            .is_ok()
        );
        for refused in [
            &["--manager", "cursor_sdk", "--workspace", "."][..],
            &["--bin", "/opt/bin/claude"],
        ] {
            assert!(
                Cli::try_parse_from(
                    ["vibeface", "init", "--data-dir", "/data/node"]
                        .into_iter()
                        .chain(refused.iter().copied())
                )
                .is_err()
            );
        }
        assert!(Cli::try_parse_from(["vibeface", "tool", "dispatch_work", "--json", "{}"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "vibeface",
                "codex",
                "--socket",
                "/tmp/codex.sock",
                "send",
                "t",
                "hi",
                "--id",
                "m",
                "--image",
                "a.png",
                "--image",
                "b.png"
            ])
            .is_ok()
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

    #[tokio::test]
    async fn hooks_install_writes_the_pi_extension_once_and_removes_only_its_own() {
        let dir = std::env::temp_dir().join(format!("vibeface-hooks-{}", uuid::Uuid::new_v4()));
        let (home, data) = (dir.join("home"), dir.join("data"));
        let run = async |action: &str| {
            let parsed = Cli::try_parse_from([
                "vibeface",
                "hooks",
                action,
                "--home",
                home.to_str().unwrap(),
                "--data-dir",
                data.to_str().unwrap(),
            ])
            .unwrap();
            let Some(Action::Hooks(options)) = parsed.command else {
                panic!("expected hooks")
            };
            hooks(options, Default::default()).await
        };
        let extensions = home.join(".pi/agent/extensions");
        let path = extensions.join("vibeface.ts");
        run("install").await.unwrap();
        let code = fs::read_to_string(&path).unwrap();
        let program = std::env::current_exe().unwrap();
        let hook = json!([program, "hook", "pi", "--data-dir", data]).to_string();
        assert!(code.starts_with("// vibeface:") && !code.contains("__VIBEFACE_HOOK__"));
        assert!(code.contains(&format!("const HOOK = {hook};")), "{code}");
        run("install").await.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), code);
        assert_eq!(fs::read_dir(&extensions).unwrap().count(), 1);
        run("uninstall").await.unwrap();
        assert!(!path.exists());
        fs::write(&path, "// the user's own\n").unwrap();
        let claude = home.join(".claude/settings.json");
        let before = fs::read(&claude).ok();
        assert!(run("install").await.is_err());
        assert_eq!(
            fs::read(&claude).ok(),
            before,
            "a refused install writes nothing"
        );
        assert!(!home.join(".config/opencode/plugins/vibeface.ts").exists());
        run("uninstall").await.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "// the user's own\n");
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn hooks_install_finds_codex_as_codex_does_and_home_holds_every_file() {
        use vibeface::discovery::Host;
        let dir = std::env::temp_dir().join(format!("vibeface-hooks-{}", uuid::Uuid::new_v4()));
        let (home, codex, data) = (dir.join("home"), dir.join("codex"), dir.join("data"));
        let run = async |host: Host, home: Option<&Path>| {
            let mut args = vec![
                "vibeface",
                "hooks",
                "install",
                "--data-dir",
                data.to_str().unwrap(),
            ];
            if let Some(home) = home {
                args.extend(["--home", home.to_str().unwrap()]);
            }
            let Some(Action::Hooks(options)) = Cli::try_parse_from(args).unwrap().command else {
                panic!("expected hooks")
            };
            hooks(options, host).await
        };
        let env = |home: PathBuf, codex_home: Option<PathBuf>| Host {
            home: Some(home),
            codex_home,
            path: None,
        };
        let given = dir.join("given");
        run(env(home.clone(), Some(codex.clone())), Some(&given))
            .await
            .unwrap();
        assert!(given.join(".codex/hooks.json").is_file());
        assert!(given.join(".claude/settings.json").is_file());
        assert!(
            !codex.exists() && !home.exists(),
            "--home keeps CODEX_HOME and $HOME untouched"
        );
        assert!(
            run(env(home.clone(), Some(codex.clone())), None)
                .await
                .is_err()
                && !home.exists(),
            "a CODEX_HOME that does not exist is refused before anything is written"
        );
        fs::create_dir_all(&codex).unwrap();
        run(env(home.clone(), Some(codex.clone())), None)
            .await
            .unwrap();
        assert!(codex.join("hooks.json").is_file());
        assert!(home.join(".claude/settings.json").is_file());
        assert!(
            !home.join(".codex").exists(),
            "CODEX_HOME moves Codex's files"
        );
        let plain = dir.join("plain");
        run(env(plain.clone(), None), None).await.unwrap();
        assert!(plain.join(".codex/hooks.json").is_file());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn hooks_install_writes_the_opencode_plugin_once() {
        let dir = std::env::temp_dir().join(format!("vibeface-hooks-{}", uuid::Uuid::new_v4()));
        let (home, data) = (dir.join("home"), dir.join("data"));
        let run = async |action: &str| {
            let parsed = Cli::try_parse_from([
                "vibeface",
                "hooks",
                action,
                "--home",
                home.to_str().unwrap(),
                "--data-dir",
                data.to_str().unwrap(),
            ])
            .unwrap();
            let Some(Action::Hooks(options)) = parsed.command else {
                panic!("expected hooks")
            };
            hooks(options, Default::default()).await
        };
        let path = home.join(".config/opencode/plugins/vibeface.ts");
        run("install").await.unwrap();
        let code = fs::read_to_string(&path).unwrap();
        let program = std::env::current_exe().unwrap();
        let hook = json!([program, "hook", "opencode", "--data-dir", data]).to_string();
        assert!(code.contains(&format!("const HOOK = {hook};")), "{code}");
        run("install").await.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), code);
        run("uninstall").await.unwrap();
        assert!(!path.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
