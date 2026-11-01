use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use reqwest::Url;
use serde::Serialize;
use serde_json::{Value, json};
use std::{io::Read, net::{IpAddr, Ipv4Addr, SocketAddr}, path::{Path, PathBuf}, process::Command, time::Duration};
use vibeface_control::ControlRequest;

const DEFAULT_LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4790);

#[derive(Parser)]
#[command(name = "vibeface", version, about = "Personal device network for local coding agents.", args_conflicts_with_subcommands = true)]
struct Cli {
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = absolute_path)]
    data_dir: Option<PathBuf>,
    #[arg(long)]
    no_web: bool,
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    Init(Init),
    Serve(Serve),
    Status(Access),
    Access(LocalEntry),
    Join(Join),
    Wallet(WalletOptions),
    Service(crate::service::Service),
    Update(crate::update::Update),
    Tool { name: String, #[arg(long)] json: String, #[arg(long)] call_ref: Option<String> },
    Codex { #[arg(long, env = "VIBEFACE_CODEX_SOCKET")] socket: Option<PathBuf>, #[command(subcommand)] command: CodexCommand },
    Hook(Hook),
    Hooks(Hooks),
    Triage(Triage),
    Requests(LocalNode),
    Respond { request_id: String, result: String, #[arg(long)] node: Option<String>, #[command(flatten)] local: LocalNode },
    Log(LogRecord),
    Logs(Logs),
}

#[derive(Args, Serialize)]
struct Init {
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = absolute_path, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    #[arg(long, default_value_t = DEFAULT_LISTEN)]
    listen: SocketAddr,
    #[arg(long, default_value_t = local_listen())]
    local_listen: SocketAddr,
    #[arg(long)] public_url: Option<String>,
    #[arg(long)] no_web: bool,
    #[arg(long = "allowed-origin")] allowed_origins: Vec<String>,
    #[arg(long = "root")] roots: Vec<PathBuf>,
    #[arg(long)] no_local_auto_access: bool,
    #[arg(long)] allow_insecure_lan: bool,
    #[arg(long = "relay")] relays: Vec<String>,
    #[arg(long = "discovery")] discovery: Vec<String>,
    #[arg(long)] relay_mode: Option<String>,
    #[arg(long)] relay_listen: Option<SocketAddr>,
    #[arg(long)] relay_url: Option<String>,
    #[arg(long)] public_entry: bool,
    #[arg(long)] entry: Option<String>,
    #[arg(long)] entry_relay: Option<String>,
    #[arg(long)] relay_token_file: Option<PathBuf>,
}

#[derive(Args)]
struct Serve {
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = absolute_path, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    #[arg(long)] no_web: bool,
    #[arg(long, env = "VIBEFACE_APP_DIR", value_parser = absolute_path)]
    app_dir: Option<PathBuf>,
}

#[derive(Args)]
struct Access {
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = absolute_path, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    #[arg(long)] json: bool,
}

#[derive(Args)]
struct LocalEntry {
    #[command(flatten)] access: Access,
    #[arg(long, conflicts_with = "disable")] enable: bool,
    #[arg(long)] disable: bool,
}

#[derive(Args)]
struct Join {
    link: String,
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = absolute_path, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    #[arg(long)] name: String,
}

#[derive(Args)]
struct WalletOptions {
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = absolute_path, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    #[command(subcommand)] command: WalletAction,
}
#[derive(Subcommand)]
enum WalletAction {
    Status,
    Create { #[arg(long)] passphrase_fd: Option<i32> },
    Unlock { #[arg(long)] passphrase_fd: Option<i32> },
    Lock,
    Configure { file: PathBuf },
    RegisterKey { #[arg(long)] intent_id: String, #[arg(long)] gas_cap: String },
    RotateKey { #[arg(long)] intent_id: String, #[arg(long)] gas_cap: String },
    RevokeKey { #[arg(long)] intent_id: String, #[arg(long)] gas_cap: String },
    Backup { output: PathBuf },
}

#[derive(Subcommand)]
enum CodexCommand { Call { method: String, params: Option<String> }, Watch { thread: String }, Send { thread: String, text: String, #[arg(long)] id: String, #[arg(long)] model: Option<String>, #[arg(long)] effort: Option<String>, #[arg(long)] image: Vec<String> } }

#[derive(Args)]
struct LocalNode {
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = absolute_path, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    #[arg(long, env = "VIBEFACE_TRIAGE_URL")] node_url: Option<String>,
}
#[derive(Clone, Copy, ValueEnum)] enum HookAgent { Codex, Claude, Pi, Opencode, Cursor }
#[derive(Args)] struct Hook { agent: HookAgent, #[command(flatten)] local: LocalNode, #[arg(long)] test: bool, #[arg(long)] dry_run: bool }
#[derive(Clone, Copy, ValueEnum)] enum HooksAction { Install, Uninstall, Status, Test }
#[derive(Args)] struct Hooks { action: HooksAction, #[command(flatten)] local: LocalNode, #[arg(long)] home: Option<PathBuf>, #[arg(long)] dry_run: bool }
#[derive(Args)] struct Triage { #[command(subcommand)] action: TriageAction, #[command(flatten)] local: LocalNode }
#[derive(Subcommand, Serialize)] #[serde(tag = "action", rename_all = "snake_case")] enum TriageAction { Events { #[arg(long)] since: Option<i64> }, Raise { #[arg(long)] event_id: String, #[arg(long)] severity: String, #[arg(long)] question: String, #[arg(long)] request_id: Option<String>, #[arg(long = "choice")] choices: Vec<String> }, Dismiss { #[arg(long)] event_id: String } }
#[derive(Args)] struct LogRecord { level: String, message: String, #[arg(long)] conversation: Option<String>, #[arg(long)] work: Option<String>, #[arg(long)] thread: Option<String>, #[arg(long)] event: Option<String>, #[arg(long)] source: Option<String>, #[command(flatten)] local: LocalNode }
#[derive(Args)] struct Logs { #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = absolute_path, default_value_os_t = default_data_dir())] data_dir: PathBuf, #[arg(long)] json: bool, #[arg(long)] since: Option<String>, #[arg(long)] grep: Option<String>, #[arg(long, default_value_t = 200)] limit: usize, #[arg(short, long)] follow: bool }

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => {
            let data_dir = cli.data_dir.or_else(|| std::env::var_os("VIBEFACE_DATA_DIR").map(PathBuf::from)).unwrap_or_else(default_data_dir);
            let mut args = vec!["--data-dir".to_owned(), data_dir.to_string_lossy().into_owned()];
            if cli.no_web { args.push("--no-web".into()); }
            exec_core(&args.iter().map(String::as_str).collect::<Vec<_>>())
        }
        Some(Action::Serve(options)) => {
            let mut args = vec!["--data-dir".to_owned(), options.data_dir.to_string_lossy().into_owned()];
            if options.no_web { args.push("--no-web".into()); }
            if let Some(app_dir) = options.app_dir { args.extend(["--app-dir".into(), app_dir.to_string_lossy().into_owned()]); }
            exec_core(&args.iter().map(String::as_str).collect::<Vec<_>>())
        }
        Some(Action::Init(options)) => {
            let value = serde_json::to_value(&options)?;
            let result = vibeface_control::request(&core_binary()?, &ControlRequest::Init { options: value }).await?;
            println!("{}", serde_json::to_string(&result)?);
            Ok(())
        }
        Some(Action::Status(options)) => status(options).await,
        Some(Action::Access(options)) => access(options).await,
        Some(Action::Join(options)) => join(options).await,
        Some(Action::Wallet(options)) => wallet(options).await,
        Some(Action::Tool { name, json, call_ref }) => tool(name, json, call_ref).await,
        Some(Action::Codex { socket, command }) => codex(socket, command).await,
        Some(Action::Hook(options)) => hook(options).await,
        Some(Action::Hooks(options)) => hooks(options).await,
        Some(Action::Triage(options)) => post_triage(&options.local, serde_json::to_value(options.action)?).await,
        Some(Action::Requests(local)) => post_triage(&local, json!({"action":"requests"})).await,
        Some(Action::Respond { request_id, result, node, local }) => {
            let result: Value = serde_json::from_str(&result).context("result must be JSON")?;
            post_triage(&local, json!({"action":"respond","request_id":request_id,"result":result,"node":node})).await
        }
        Some(Action::Log(options)) => log_record(options).await,
        Some(Action::Logs(options)) => logs(options).await,
        Some(Action::Service(options)) => crate::service::run(options).await,
        Some(Action::Update(options)) => crate::update::run(options).await,
    }
}

fn exec_core(args: &[&str]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let error = Command::new(core_binary()?).args(args).exec();
    Err(error).context("exec vibeface-core")
}

fn core_binary() -> Result<PathBuf> { vibeface_control::sibling_core(&std::env::current_exe()?) }

async fn status(options: Access) -> Result<()> {
    let mut result = control(&options.data_dir, ControlRequest::Inspect { data_dir: options.data_dir.clone() }).await?;
    result["cli_version"] = json!(env!("CARGO_PKG_VERSION"));
    if options.json { println!("{}", serde_json::to_string(&result)?); } else {
        println!("Peer ID: {}\nStatus: {}\nListen: {}\nData: {}", result["device_id"].as_str().unwrap_or_default(), result["status"].as_str().unwrap_or_default(), result["listen"].as_str().unwrap_or_default(), result["data_dir"].as_str().unwrap_or_default());
        if let Some(version) = result["version"].as_str() { println!("Version: {version}"); }
    }
    ensure!(result["status"] == "running", "node is not reachable through its private maintenance socket");
    Ok(())
}

async fn access(options: LocalEntry) -> Result<()> {
    if options.enable || options.disable {
        control(&options.access.data_dir, ControlRequest::SetLocalAccess { data_dir: options.access.data_dir.clone(), enabled: options.enable }).await?;
    }
    let result = control(&options.access.data_dir, ControlRequest::Access { data_dir: options.access.data_dir.clone(), json: options.access.json }).await?;
    if options.access.json { println!("{}", serde_json::to_string(&result)?); } else { println!("{}", result["url"].as_str().unwrap_or_default()); }
    Ok(())
}

async fn join(options: Join) -> Result<()> {
    let data_dir = options.data_dir.clone();
    let result = control(&data_dir, ControlRequest::Join { data_dir, link: options.link, name: options.name }).await?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

async fn control(_data_dir: &Path, request: ControlRequest) -> Result<Value> { vibeface_control::request(&core_binary()?, &request).await }

async fn wallet(options: WalletOptions) -> Result<()> {
    let (command, passphrase_fd) = match options.command {
        WalletAction::Status => ("status", None), WalletAction::Create { passphrase_fd } => ("create", passphrase_fd), WalletAction::Unlock { passphrase_fd } => ("unlock", passphrase_fd), WalletAction::Lock => ("lock", None), WalletAction::Configure { .. } => ("configure", None), WalletAction::RegisterKey { .. } => ("register-key", None), WalletAction::RotateKey { .. } => ("rotate-key", None), WalletAction::RevokeKey { .. } => ("revoke-key", None), WalletAction::Backup { .. } => ("backup", None),
    };
    let socket = inspect_socket(&options.data_dir).await?;
    let operation = json!({"op":command,"passphrase_fd":passphrase_fd});
    let result = vibeface_control::maintenance::request(&socket, vibeface_control::maintenance::Command::DeliveryWallet(operation)).await?;
    println!("{}", serde_json::to_string(&result.wallet.unwrap_or(Value::Null))?);
    Ok(())
}

async fn inspect_socket(data_dir: &Path) -> Result<PathBuf> {
    let result = control(data_dir, ControlRequest::Inspect { data_dir: data_dir.to_owned() }).await?;
    result["maintenance_socket"].as_str().map(PathBuf::from).context("core did not return a maintenance socket")
}

async fn tool(name: String, arguments: String, call_ref: Option<String>) -> Result<()> {
    ensure!(!name.is_empty() && name.len() <= 128, "invalid tool name");
    let mut args: Value = serde_json::from_str(&arguments).context("invalid tool arguments JSON")?;
    ensure!(args.is_object(), "tool arguments must be a JSON object");
    if let Some(call_ref) = call_ref { args["call_ref"] = json!(call_ref); }
    let result = post_tool(&name, args).await?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

async fn post_tool(name: &str, args: Value) -> Result<Value> {
    let url = Url::parse(&std::env::var("VIBEFACE_NODE_URL").context("VIBEFACE_NODE_URL is required")?)?;
    ensure!(url.host_str().is_some_and(|host| host == "localhost" || host == "127.0.0.1" || host == "::1"), "SDK tools must connect to a loopback node URL");
    let token = std::env::var("VIBEFACE_TOOL_TOKEN").context("VIBEFACE_TOOL_TOKEN is required")?;
    let response = reqwest::Client::builder().timeout(Duration::from_secs(30)).no_proxy().build()?.post(url.join("api/tool")?).bearer_auth(token).json(&json!({"toolName":name,"args":args,"agentId":"cli"})).send().await?;
    let status = response.status();
    let body = response.bytes().await?;
    ensure!(body.len() <= 1024 * 1024, "node response exceeds 1 MiB");
    ensure!(status.is_success(), "node tool rejected with HTTP {status}");
    Ok(serde_json::from_slice::<Value>(&body)?["result"].clone())
}

async fn codex(socket: Option<PathBuf>, command: CodexCommand) -> Result<()> {
    let socket = socket.context("--socket or VIBEFACE_CODEX_SOCKET is required")?;
    use vibeface_control::agent::{run, Command};
    match command {
        CodexCommand::Call { method, params } => run(&socket, Command::Call { method, params }).await,
        CodexCommand::Watch { thread } => run(&socket, Command::Watch { thread_id: thread }).await,
        CodexCommand::Send { .. } => bail!("codex send must run inside a managed agent turn"),
    }
}

async fn post_triage(local: &LocalNode, body: Value) -> Result<()> {
    let url = local_url(local).await?;
    let token = std::env::var("VIBEFACE_TOOL_TOKEN").ok();
    let mut request = reqwest::Client::new().post(url.join("api/triage")?).json(&body);
    if let Some(token) = token { request = request.bearer_auth(token); }
    let response = request.send().await?;
    let status = response.status();
    let body = response.bytes().await?;
    ensure!(status.is_success(), "the Node refused it (HTTP {status})");
    println!("{}", String::from_utf8_lossy(&body));
    Ok(())
}

async fn hook(options: Hook) -> Result<()> {
    let event = if options.test { json!({"source":"test","session_id":"test","turn_id":uuid::Uuid::new_v4().to_string(),"test":true}) } else { let mut input = Vec::new(); std::io::stdin().take(1 << 20).read_to_end(&mut input)?; serde_json::from_slice(&input).context("hook input is not JSON")? };
    if options.dry_run { println!("{}", serde_json::to_string_pretty(&event)?); return Ok(()); }
    let response = reqwest::Client::new().post(local_url(&options.local).await?.join("api/hooks")?).json(&event).send().await?;
    println!("{}", response.text().await?);
    Ok(())
}
async fn hooks(options: Hooks) -> Result<()> { if matches!(options.action, HooksAction::Test) { return hook(Hook { agent: HookAgent::Codex, local: options.local, test: true, dry_run: options.dry_run }).await; } bail!("hook installation is provided by the packaged CLI service manager") }
async fn log_record(options: LogRecord) -> Result<()> { post_triage(&options.local, json!({"source":options.source.unwrap_or_else(|| "cli".into()),"records":[{"level":options.level,"message":options.message,"event":options.event,"conversation_id":options.conversation,"work_id":options.work,"thread_id":options.thread}]})).await }
async fn logs(_options: Logs) -> Result<()> { bail!("log rendering is provided by the core HTTP API") }
async fn local_url(local: &LocalNode) -> Result<Url> {
    let value = match local.node_url.clone().or_else(|| std::env::var("VIBEFACE_TRIAGE_URL").ok()) {
        Some(value) => value,
        None => {
            let info = control(&local.data_dir, ControlRequest::Inspect { data_dir: local.data_dir.clone() }).await?;
            format!("http://{}", info["local_listen"].as_str().context("core did not return a local listener")?)
        }
    };
    let url = Url::parse(&value)?;
    ensure!(url.host_str().is_some_and(|host| host == "localhost" || host == "127.0.0.1" || host == "::1"), "the local Node URL must be a loopback address");
    Ok(url)
}
fn absolute_path(value: &str) -> Result<PathBuf, String> { let path = PathBuf::from(value); if !path.is_absolute() { return Err("path must be absolute".into()); } Ok(path) }
fn default_data_dir() -> PathBuf { std::env::var_os("HOME").map_or_else(|| PathBuf::from(".vibeface"), |home| PathBuf::from(home).join(".vibeface")) }
fn local_listen() -> SocketAddr { "127.0.0.1:4791".parse().expect("valid local listener") }

mod service;
mod update;
