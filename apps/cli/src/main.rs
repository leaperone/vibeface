use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, Read, Seek, SeekFrom, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use vibeface_control::{
    ControlRequest,
    maintenance::{self, MaintenanceCommand, SecretString, WalletCommand},
};

mod service;
mod triage;
mod update;

const DEFAULT_LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4790);
const DEFAULT_LOCAL_LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4791);
const NO_HOME: &str =
    "no data directory: HOME is unset or not absolute; pass --data-dir or set VIBEFACE_DATA_DIR";
const LOG_FILE: &str = "node.log";
const MAX_LOG_RECORD: usize = 8 * 1024;

#[derive(Parser)]
#[command(
    name = "vibeface",
    version,
    about = "Personal device network for local coding agents.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = absolute_path)]
    data_dir: Option<PathBuf>,
    #[arg(long, env = "VIBEFACE_APP_DIR", value_parser = absolute_path)]
    app_dir: Option<PathBuf>,
    #[arg(long)]
    no_web: bool,
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    Init(Box<Init>),
    Serve(Serve),
    Status(Access),
    Access(LocalEntry),
    Join(Join),
    Wallet(WalletOptions),
    Service(crate::service::Service),
    Update(crate::update::Update),
    Tool {
        name: String,
        #[arg(long)]
        json: String,
        #[arg(long)]
        call_ref: Option<String>,
    },
    Codex {
        #[arg(long, env = "VIBEFACE_CODEX_SOCKET")]
        socket: Option<PathBuf>,
        #[command(subcommand)]
        command: CodexCommand,
    },
    Hook(Hook),
    Hooks(Hooks),
    Triage(Triage),
    Requests(LocalNode),
    Respond {
        request_id: String,
        result: String,
        #[arg(long)]
        node: Option<String>,
        #[command(flatten)]
        local: LocalNode,
    },
    Log(LogRecord),
    Logs(Logs),
}

#[derive(Args, Serialize)]
struct Init {
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = data_dir, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    #[arg(long, default_value_t = DEFAULT_LISTEN)]
    listen: SocketAddr,
    #[arg(long, default_value_t = local_listen())]
    local_listen: SocketAddr,
    #[arg(long)]
    public_url: Option<String>,
    #[arg(long)]
    no_web: bool,
    #[arg(long = "allowed-origin")]
    allowed_origins: Vec<String>,
    #[arg(long = "root")]
    roots: Vec<PathBuf>,
    #[arg(long)]
    no_local_auto_access: bool,
    #[arg(long)]
    allow_insecure_lan: bool,
    #[arg(long, value_enum, requires = "workspace")]
    manager: Option<triage::Provider>,
    #[arg(long, requires = "manager")]
    workspace: Option<PathBuf>,
    #[arg(long, requires = "manager")]
    model: Option<String>,
    #[arg(long = "bin", value_name = "PROVIDER=PATH", value_parser = provider_bin)]
    bins: Vec<(triage::Provider, PathBuf)>,
    #[arg(long)]
    codex_socket: Option<PathBuf>,
    #[arg(long = "relay", value_name = "URL")]
    relays: Vec<String>,
    #[arg(long = "discovery", value_name = "URL")]
    discovery: Vec<String>,
    #[arg(long, value_enum, default_value = "forward")]
    relay_mode: RelayServiceMode,
    #[arg(long, value_name = "ADDR")]
    relay_listen: Option<SocketAddr>,
    #[arg(long, value_name = "URL", requires = "relay_listen")]
    relay_url: Option<String>,
    #[arg(long, requires = "relay_listen", conflicts_with_all = ["manager", "roots", "entry"])]
    public_entry: bool,
    #[arg(long, value_name = "URL", requires = "relay_token_file")]
    entry: Option<String>,
    #[arg(long, value_name = "URL", requires = "entry")]
    entry_relay: Option<String>,
    #[arg(long, value_name = "PATH", requires = "entry")]
    relay_token_file: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum RelayServiceMode {
    Forward,
    DiscoveryOnly,
}

impl Default for RelayServiceMode {
    fn default() -> Self {
        Self::Forward
    }
}

#[derive(Args)]
struct Serve {
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = data_dir, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    #[arg(long)]
    no_web: bool,
    #[arg(long, env = "VIBEFACE_APP_DIR", value_parser = absolute_path)]
    app_dir: Option<PathBuf>,
}

#[derive(Args)]
struct Access {
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = data_dir, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct LocalEntry {
    #[command(flatten)]
    access: Access,
    #[arg(long, conflicts_with = "disable")]
    enable: bool,
    #[arg(long)]
    disable: bool,
}

#[derive(Args)]
struct Join {
    link: String,
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = data_dir, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    #[arg(long)]
    name: String,
}

#[derive(Args)]
struct WalletOptions {
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = data_dir, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: WalletAction,
}

#[derive(Subcommand)]
enum WalletAction {
    Status,
    Create {
        #[arg(long)]
        passphrase_fd: Option<i32>,
    },
    Unlock {
        #[arg(long)]
        passphrase_fd: Option<i32>,
    },
    Lock,
    Configure {
        file: PathBuf,
    },
    RegisterKey {
        #[arg(long)]
        intent_id: String,
        #[arg(long)]
        gas_cap: String,
    },
    RotateKey {
        #[arg(long)]
        intent_id: String,
        #[arg(long)]
        gas_cap: String,
    },
    RevokeKey {
        #[arg(long)]
        intent_id: String,
        #[arg(long)]
        gas_cap: String,
    },
    Backup {
        output: PathBuf,
    },
}

#[derive(Subcommand)]
enum CodexCommand {
    Call {
        method: String,
        params: Option<String>,
    },
    Send {
        thread: String,
        text: String,
        #[arg(long)]
        id: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        image: Vec<String>,
    },
    Watch {
        thread: String,
    },
}

#[derive(Args)]
struct LocalNode {
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = data_dir, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    #[arg(long, env = "VIBEFACE_TRIAGE_URL")]
    node_url: Option<String>,
}

#[derive(Args)]
struct Hook {
    #[arg(value_enum)]
    agent: triage::Provider,
    #[command(flatten)]
    local: LocalNode,
    #[arg(long)]
    test: bool,
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
    #[arg(long, value_parser = absolute_path)]
    home: Option<PathBuf>,
    #[arg(long)]
    dry_run: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum AttentionKind {
    Question,
    Approval,
    #[value(name = "browser-takeover")]
    BrowserTakeover,
}

#[derive(Args)]
struct Triage {
    #[command(subcommand)]
    action: TriageAction,
    #[command(flatten)]
    local: LocalNode,
}

#[derive(Subcommand, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum TriageAction {
    Events {
        #[arg(long)]
        since: Option<i64>,
    },
    Raise {
        #[arg(long)]
        event_id: String,
        #[arg(long, value_parser = ["info", "attention", "urgent"])]
        severity: String,
        #[arg(long)]
        question: String,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long, value_enum)]
        attention_kind: Option<AttentionKind>,
        #[arg(long = "choice")]
        choices: Vec<String>,
    },
    Dismiss {
        #[arg(long)]
        event_id: String,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ValueEnum, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl std::str::FromStr for LogLevel {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        serde_json::from_value(Value::String(value.to_ascii_lowercase()))
            .context("level is one of trace, debug, info, warn, error")
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum LogSource {
    Node,
    Web,
    Agent,
    Cli,
    Codex,
    Cursor,
}

impl std::str::FromStr for LogSource {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        serde_json::from_value(Value::String(value.to_ascii_lowercase()))
            .context("source is one of node, web, agent, cli, codex, cursor")
    }
}

#[derive(Args)]
struct LogRecord {
    level: LogLevel,
    message: String,
    #[arg(long, env = "VIBEFACE_NODE_CONVERSATION")]
    conversation: Option<String>,
    #[arg(long)]
    work: Option<String>,
    #[arg(long)]
    thread: Option<String>,
    #[arg(long)]
    event: Option<String>,
    #[arg(long)]
    source: Option<LogSource>,
    #[command(flatten)]
    local: LocalNode,
}

#[derive(Args)]
struct Logs {
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = data_dir, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    level: Option<LogLevel>,
    #[arg(long)]
    source: Vec<LogSource>,
    #[arg(long)]
    conversation: Option<String>,
    #[arg(long)]
    work: Option<String>,
    #[arg(long)]
    grep: Option<String>,
    #[arg(long, default_value_t = 200)]
    limit: usize,
    #[arg(short, long)]
    follow: bool,
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => {
            let data_dir = data_dir_or_home(
                cli.data_dir
                    .or_else(|| std::env::var_os("VIBEFACE_DATA_DIR").map(PathBuf::from)),
            )?;
            let mut args = vec![
                "--data-dir".to_owned(),
                data_dir.to_string_lossy().into_owned(),
            ];
            if cli.no_web {
                args.push("--no-web".into());
            }
            if let Some(app_dir) = cli.app_dir {
                args.extend(["--app-dir".into(), app_dir.to_string_lossy().into_owned()]);
            }
            exec_core(&args.iter().map(String::as_str).collect::<Vec<_>>())
        }
        Some(Action::Serve(options)) => {
            let mut args = vec![
                "--data-dir".to_owned(),
                options.data_dir.to_string_lossy().into_owned(),
            ];
            if options.no_web {
                args.push("--no-web".into());
            }
            if let Some(app_dir) = options.app_dir {
                args.extend(["--app-dir".into(), app_dir.to_string_lossy().into_owned()]);
            }
            exec_core(&args.iter().map(String::as_str).collect::<Vec<_>>())
        }
        Some(Action::Init(options)) => {
            let result = control(
                &options.data_dir,
                ControlRequest::Init {
                    options: serde_json::to_value(&*options)?,
                },
            )
            .await?;
            println!("Vibeface 已就绪");
            if let Some(open) = result["open"].as_str() {
                println!("打开 {open}");
            }
            if result["discovery_only"] == true {
                if let Some(device_id) = result["device_id"].as_str() {
                    println!("Discovery device ID: {device_id}");
                }
            } else if result["public_entry"] == true {
                if let Some(path) = result["relay_token_file"].as_str() {
                    println!(
                        "Relay token: {path} (copy it to your Node and pass --relay-token-file)"
                    );
                }
            }
            Ok(())
        }
        Some(Action::Status(options)) => status(options).await,
        Some(Action::Access(options)) => access(options).await,
        Some(Action::Join(options)) => join(options).await,
        Some(Action::Wallet(options)) => wallet(options).await,
        Some(Action::Tool {
            name,
            json,
            call_ref,
        }) => call_tool(name, json, call_ref).await,
        Some(Action::Codex { socket, command }) => codex(socket, command).await,
        Some(Action::Hook(options)) => {
            if let Err(error) = hook(options).await {
                eprintln!("vibeface hook: {error:#}");
            }
            Ok(())
        }
        Some(Action::Hooks(options)) => hooks(options).await,
        Some(Action::Triage(options)) => {
            post_triage(&options.local, serde_json::to_value(options.action)?).await
        }
        Some(Action::Requests(local)) => post_triage(&local, json!({"action":"requests"})).await,
        Some(Action::Respond {
            request_id,
            result,
            node,
            local,
        }) => {
            let result: Value = serde_json::from_str(&result).context("result must be JSON")?;
            post_triage(
                &local,
                json!({"action":"respond","request_id":request_id,"result":result,"node":node}),
            )
            .await
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

fn core_binary() -> Result<PathBuf> {
    vibeface_control::sibling_core(&std::env::current_exe()?)
}

async fn control(_data_dir: &Path, request: ControlRequest) -> Result<Value> {
    vibeface_control::request(&core_binary()?, &request).await
}

async fn inspect_socket(data_dir: &Path) -> Result<PathBuf> {
    let info = control(
        data_dir,
        ControlRequest::Inspect {
            data_dir: data_dir.to_owned(),
        },
    )
    .await?;
    info["maintenance_socket"]
        .as_str()
        .map(PathBuf::from)
        .context("core did not return a maintenance socket")
}

async fn status(options: Access) -> Result<()> {
    let mut result = control(
        &options.data_dir,
        ControlRequest::Inspect {
            data_dir: options.data_dir.clone(),
        },
    )
    .await?;
    result["cli_version"] = json!(env!("CARGO_PKG_VERSION"));
    if options.json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        println!(
            "Peer ID: {}\nStatus: {}\nListen: {}\nData: {}",
            result["device_id"].as_str().unwrap_or_default(),
            result["status"].as_str().unwrap_or_default(),
            result["listen"].as_str().unwrap_or_default(),
            result["data_dir"].as_str().unwrap_or_default()
        );
        if let Some(version) = result["version"]
            .as_str()
            .or_else(|| result["core_version"].as_str())
        {
            println!("Version: {version}");
        }
        if let Some(web) = result["web_enabled"].as_bool() {
            println!("Web: {}", if web { "enabled" } else { "disabled" });
        }
        if let Some(write_state) = result["write_state"].as_str() {
            println!("Writes: {write_state}");
        }
        if let Some(peers) = result["peers"].as_array() {
            let now = now_ms();
            for peer in peers {
                let link = match peer["reachable"].as_bool() {
                    Some(true) => "reachable",
                    Some(false) => "unreachable",
                    None => "not tried yet",
                };
                let synced = peer["synced_ms"]
                    .as_i64()
                    .map_or("never synced".into(), |at| {
                        format!("synced {}s ago", now.saturating_sub(at) / 1000)
                    });
                println!(
                    "Peer: {} ({}) {link}, {synced}",
                    peer["name"].as_str().unwrap_or_default(),
                    peer["device_id"].as_str().unwrap_or_default()
                );
            }
        }
        if let Some(error) = result["error"].as_str() {
            println!("Detail: {error}");
        }
    }
    ensure!(
        result["status"] == "running",
        "node is not reachable through its private maintenance socket"
    );
    Ok(())
}

async fn access(options: LocalEntry) -> Result<()> {
    if options.enable || options.disable {
        let result = control(
            &options.access.data_dir,
            ControlRequest::SetLocalAccess {
                data_dir: options.access.data_dir.clone(),
                enabled: options.enable,
            },
        )
        .await?;
        let running = result["running"].as_bool().unwrap_or(true);
        if options.disable || !running {
            let state = if options.enable { "on" } else { "off" };
            let note = if running {
                ""
            } else {
                "; the node applies it when it starts"
            };
            println!("local browser access without confirmation is {state}{note}");
            return Ok(());
        }
    }
    let result = control(
        &options.access.data_dir,
        ControlRequest::Access {
            data_dir: options.access.data_dir.clone(),
            json: options.access.json,
        },
    )
    .await?;
    if options.access.json {
        let mut output = json!({
            "url": result["url"],
            "device_id": result["device_id"],
        });
        if result["public_entry"] == true {
            output["public_entry"] = Value::Bool(true);
        } else if result["local"] == true {
            output["local"] = Value::Bool(true);
        } else {
            if let Some(capability) = result["capability"].as_str() {
                output["capability"] = json!(capability);
            }
            if let Some(url) = output["url"].as_str() {
                let mut url = Url::parse(url).context("core returned an invalid access URL")?;
                url.set_fragment(None);
                output["url"] = json!(url.origin().ascii_serialization());
            }
        }
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{}", result["url"].as_str().unwrap_or_default());
    }
    Ok(())
}

async fn join(options: Join) -> Result<()> {
    let data_dir = options.data_dir.clone();
    let running = control(
        &data_dir,
        ControlRequest::Inspect {
            data_dir: data_dir.clone(),
        },
    )
    .await
    .ok()
    .and_then(|value| value["status"].as_str().map(|status| status == "running"))
    .unwrap_or(false);
    if !running {
        let result = control(
            &data_dir,
            ControlRequest::Join {
                data_dir: data_dir.clone(),
                link: options.link.clone(),
                name: options.name.clone(),
            },
        )
        .await?;
        match result["status"].as_str() {
            Some("joined") => {
                println!("已加入。运行 vibeface 后就能连接。");
                Ok(())
            }
            Some("waiting" | "confirming") => {
                join_through_running_node(&data_dir, &options.link, &options.name).await
            }
            Some("failed") => bail!(join_failure_text(
                result["reason"].as_str().unwrap_or_default()
            )),
            Some("cancelled") => bail!("这次加入已在这台电脑的网页上取消。"),
            _ => {
                println!("{}", serde_json::to_string(&result)?);
                Ok(())
            }
        }
    } else {
        join_through_running_node(&data_dir, &options.link, &options.name).await
    }
}

async fn join_through_running_node(data_dir: &Path, link: &str, name: &str) -> Result<()> {
    let socket = inspect_socket(data_dir).await?;
    let mut shown = None;
    let mut unreachable = 0;
    loop {
        let response = match maintenance::request(
            &socket,
            MaintenanceCommand::Join {
                link: link.to_owned(),
                name: name.to_owned(),
            },
        )
        .await
        {
            Ok(response) => response,
            Err(error) if error.downcast_ref::<maintenance::Busy>().is_some() => {
                bail!("Vibeface 正在更新。等它重新启动后，再运行同一条命令。");
            }
            Err(error) if error.downcast_ref::<maintenance::Refused>().is_some() => {
                bail!(join_failure_text(&error.to_string()));
            }
            Err(error) => {
                unreachable += 1;
                if unreachable < 5 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                bail!(
                    "连不上正在运行的 Vibeface（{error}）。\n加入可能还在进行：确认它在运行后，再运行同一条命令，会接着上次的请求，不会重复提交。"
                );
            }
        };
        unreachable = 0;
        let status = response.join.unwrap_or(Value::Null);
        let state = status["status"].as_str().unwrap_or_default();
        if shown.as_ref() != Some(&status) {
            match state {
                "waiting" => println!("已提交加入请求，等待管理设备批准。"),
                "confirming" => println!(
                    "安全码：{}。在「{}」上比对一致后允许。",
                    status["code"].as_str().unwrap_or_default(),
                    status["admin"].as_str().unwrap_or_default()
                ),
                "joined" => {
                    println!("已加入。这台电脑的 Vibeface 已开始同步，不用重启。");
                    return Ok(());
                }
                "failed" => bail!(join_failure_text(
                    status["reason"].as_str().unwrap_or_default()
                )),
                "cancelled" => bail!("这次加入已在这台电脑的网页上取消。"),
                _ => println!("{}", serde_json::to_string(&status)?),
            }
            shown = Some(status.clone());
        }
        if state != "waiting" && state != "confirming" {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

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
            "这不是有效的加入链接。到管理设备的“添加设备”重新复制。",
        ),
        (
            "Vibeface ticket",
            "这不是有效的加入链接。到管理设备的“添加设备”重新复制。",
        ),
    ];
    TABLE.iter().find(|(pattern, _)| reason.contains(pattern)).map_or_else(|| format!("没能加入：{reason}\n检查网络后再运行同一条命令，会接着上次的请求，不会重复提交。"), |(_, text)| (*text).to_owned())
}

async fn wallet(options: WalletOptions) -> Result<()> {
    let dir = options.data_dir.clone();
    match options.command {
        WalletAction::Status => wallet_request(&dir, WalletCommand::Status).await,
        WalletAction::Create { passphrase_fd } => {
            wallet_request(
                &dir,
                WalletCommand::Create {
                    passphrase: read_wallet_passphrase(passphrase_fd)?,
                },
            )
            .await
        }
        WalletAction::Unlock { passphrase_fd } => {
            wallet_request(
                &dir,
                WalletCommand::Unlock {
                    passphrase: read_wallet_passphrase(passphrase_fd)?,
                },
            )
            .await
        }
        WalletAction::Lock => wallet_request(&dir, WalletCommand::Lock).await,
        WalletAction::Configure { file } => {
            let bytes = read_bounded_local(&file, 16 * 1024)?;
            let config: Value =
                serde_json::from_slice(&bytes).context("invalid delivery configuration")?;
            let socket = inspect_socket(&dir).await?;
            let result =
                maintenance::request(&socket, MaintenanceCommand::DeliveryConfig(config)).await?;
            if let Some(delivery) = result.delivery {
                println!("{}", serde_json::to_string(&delivery)?);
            } else {
                println!("Delivery settings applied.");
            }
            Ok(())
        }
        WalletAction::RegisterKey { intent_id, gas_cap } => {
            key_operation(&dir, "register", intent_id, gas_cap).await
        }
        WalletAction::RotateKey { intent_id, gas_cap } => {
            key_operation(&dir, "rotate", intent_id, gas_cap).await
        }
        WalletAction::RevokeKey { intent_id, gas_cap } => {
            key_operation(&dir, "revoke", intent_id, gas_cap).await
        }
        WalletAction::Backup { output } => {
            let result = control(
                &dir,
                ControlRequest::Wallet {
                    data_dir: dir.clone(),
                    operation: json!({"op":"backup","output":output}),
                },
            )
            .await?;
            if result.is_null() {
                println!("Encrypted delivery wallet backup saved.");
            } else {
                println!("{}", serde_json::to_string(&result)?);
            }
            Ok(())
        }
    }
}

async fn wallet_request(dir: &Path, command: WalletCommand) -> Result<()> {
    let socket = inspect_socket(dir).await?;
    let response =
        maintenance::request(&socket, MaintenanceCommand::DeliveryWallet(command)).await?;
    println!(
        "{}",
        serde_json::to_string(&response.wallet.unwrap_or(Value::Null))?
    );
    Ok(())
}

async fn key_operation(dir: &Path, action: &str, intent_id: String, gas_cap: String) -> Result<()> {
    let socket = inspect_socket(dir).await?;
    let response = maintenance::request(
        &socket,
        MaintenanceCommand::DeliveryKeyOperation {
            action: json!(action),
            intent_id,
            gas_cap,
        },
    )
    .await?;
    println!(
        "{}",
        serde_json::to_string(&response.delivery.unwrap_or(Value::Null))?
    );
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

fn read_text_bounded(path: &Path, limit: u64) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "local input is not a regular file"
            );
            let bytes = read_bounded_local(path, limit)?;
            Ok(Some(
                String::from_utf8(bytes).context("local input must be UTF-8")?,
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_wallet_passphrase(fd: Option<i32>) -> Result<SecretString> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{FileTypeExt, MetadataExt},
    };
    use zeroize::Zeroizing;
    let mut bytes = Zeroizing::new(Vec::new());
    if let Some(fd) = fd {
        ensure!(fd >= 0, "wallet passphrase descriptor is invalid");
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
    Ok(SecretString::new(
        String::from_utf8(bytes.to_vec()).context("wallet passphrase must be UTF-8")?,
    ))
}

async fn codex(socket: Option<PathBuf>, command: CodexCommand) -> Result<()> {
    use vibeface_control::agent::{Command as AgentCommand, run};
    let required_socket = || {
        socket
            .clone()
            .context("--socket or VIBEFACE_CODEX_SOCKET is required")
    };
    match command {
        CodexCommand::Call { method, params } => {
            run(&required_socket()?, AgentCommand::Call { method, params }).await
        }
        CodexCommand::Watch { thread } => {
            run(
                &required_socket()?,
                AgentCommand::Watch { thread_id: thread },
            )
            .await
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
            let args = json!({"thread":thread,"prompt":text,"call_ref":id,"agent":"codex","model":model,"effort":effort,"images":image});
            let result = dispatch_work(&args, &id, socket.as_deref()).await?;
            println!("{}", serde_json::to_string(&result)?);
            Ok(())
        }
    }
}

async fn call_tool(name: String, arguments: String, call_ref: Option<String>) -> Result<()> {
    ensure!(!name.is_empty() && name.len() <= 128, "invalid tool name");
    let mut arguments: Value =
        serde_json::from_str(&arguments).context("invalid tool arguments JSON")?;
    ensure!(
        arguments.is_object(),
        "tool arguments must be a JSON object"
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
    if let Some(call_ref) = &call_ref {
        arguments["call_ref"] = json!(call_ref);
    }
    if name != "dispatch_work" {
        let result = post_tool(
            &name,
            arguments.clone(),
            call_ref.as_deref().unwrap_or_default(),
        )
        .await
        .map_err(|error| {
            let response = tool_failure(&name, &arguments, "execution", call_ref.is_some(), &error);
            eprintln!(
                "{}",
                serde_json::to_string(&response)
                    .unwrap_or_else(|_| "{\"error\":\"tool call failed\"}".into())
            );
            anyhow::anyhow!(
                response["error"]
                    .as_str()
                    .unwrap_or("tool call failed")
                    .to_owned()
            )
        })?;
        // Core performs schema validation and annotation at the authenticated HTTP boundary.
        println!("{}", serde_json::to_string(&result)?);
        return Ok(());
    }
    let call_id = call_ref
        .as_deref()
        .context("dispatch_work requires --call-ref or arguments.call_ref")?;
    vibeface_protocol::validate_id(call_id)
        .map_err(|error| anyhow::anyhow!("arguments.call_ref is invalid: {error}"))?;
    ensure!(
        arguments["prompt"].is_string(),
        "dispatch_work requires a prompt"
    );
    ensure!(
        arguments["thread"].is_string()
            || (arguments["title"].is_string() && arguments["cwd"].is_string()),
        "dispatch_work requires thread or title and cwd"
    );
    let result = {
        let socket = std::env::var_os("VIBEFACE_CODEX_SOCKET").map(PathBuf::from);
        dispatch_work(&arguments, call_id, socket.as_deref()).await
    };
    let result = result.map_err(|error| {
        let response = tool_failure(&name, &arguments, "execution", true, &error);
        eprintln!(
            "{}",
            serde_json::to_string(&response)
                .unwrap_or_else(|_| "{\"error\":\"tool call failed\"}".into())
        );
        anyhow::anyhow!(
            response["error"]
                .as_str()
                .unwrap_or("tool call failed")
                .to_owned()
        )
    })?;
    println!(
        "{}",
        serde_json::to_string(&annotate_tool(&name, &arguments, result))?
    );
    Ok(())
}

async fn dispatch_work(arguments: &Value, call_id: &str, socket: Option<&Path>) -> Result<Value> {
    vibeface_control::agent::dispatch(
        arguments,
        |ref_id| post_tool("call_lookup", json!({"call_ref":ref_id}), call_id),
        |args| post_tool("track_work", args, call_id),
        |work_id, report| post_tool("bind_work", json!({"work_id":work_id,"report":report}), call_id),
        |args| async move {
            let asked = json!({"agent":args["agent"],"thread":args["thread"],"continues":args["continues"]});
            let ready = post_tool("wait_work", asked, call_id).await?;
            Ok(ready["agent"].as_str().map(str::to_owned))
        },
        socket,
    ).await
}

fn origin(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("invalid node URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "node URL must use http or https"
    );
    ensure!(
        url.host_str().is_some() && url.username().is_empty() && url.password().is_none(),
        "node URL must not contain credentials"
    );
    ensure!(
        url.path() == "/" || url.path().is_empty(),
        "node URL must not contain a path"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "node URL must not contain query or fragment"
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

async fn post_tool(name: &str, arguments: Value, call_id: &str) -> Result<Value> {
    let url =
        origin(&std::env::var("VIBEFACE_NODE_URL").context("VIBEFACE_NODE_URL is required")?)?;
    ensure!(
        is_loopback(&url),
        "SDK tools must connect to a loopback node URL"
    );
    let token = std::env::var("VIBEFACE_TOOL_TOKEN").context("VIBEFACE_TOOL_TOKEN is required")?;
    ensure!(!token.is_empty(), "VIBEFACE_TOOL_TOKEN is empty");
    let response = reqwest::Client::builder().timeout(Duration::from_secs(30)).no_proxy().redirect(reqwest::redirect::Policy::none()).build()?.post(url.join("sdk.v1.SdkCustomToolCallbackService/CallCustomTool")?).bearer_auth(token).json(&json!({"toolName":name,"args":arguments,"agentId":"cli"})).send().await.map_err(|error| anyhow::anyhow!("reach local node tool API; preserve call_ref {call_id} and query call_lookup before retrying: {error}"))?;
    let status = response.status();
    let body = bounded_body(response).await?;
    if !status.is_success() {
        if let Ok(error) = serde_json::from_slice::<Value>(&body) {
            if error.get("call_record").is_some() {
                eprintln!("{}", serde_json::to_string(&error)?);
            }
        }
        let reason = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|error| error["error"].as_str().map(str::to_owned))
            .unwrap_or_default();
        bail!("node tool rejected with HTTP {status}: {reason}; preserve call_ref {call_id}");
    }
    let reply: Value = serde_json::from_slice(&body).context("invalid node tool response")?;
    Ok(reply["result"].clone())
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len().saturating_add(chunk.len()) <= 1024 * 1024,
            "node response exceeds 1 MiB"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn tool_failure(
    name: &str,
    args: &Value,
    stage: &str,
    unknown: bool,
    error: &anyhow::Error,
) -> Value {
    json!({"error":format!("{error:#}"),"code":if stage == "validation" { "invalid_tool_arguments" } else { "tool_execution_failed" },"call_record":{"tool":name,"call_ref":args.get("call_ref"),"stage":stage,"execution":if unknown { "unknown" } else { "not_started" },"confirmation":if unknown { "unknown" } else { "confirmed" },"approval":"unknown","retry":if unknown { "lookup_same_ref" } else { "none" }}})
}

fn annotate_tool(name: &str, args: &Value, mut value: Value) -> Value {
    if !value.is_object() {
        value = json!({"value": value});
    }
    let state = value["state"]
        .as_str()
        .or_else(|| value["state"]["state"].as_str());
    let outcome = value["outcome"]
        .as_str()
        .or_else(|| value["state"]["outcome"].as_str());
    let unverified = value["found"] == false
        || state.is_some_and(|state| state.contains("unverified"))
        || outcome == Some("unverified");
    let read = matches!(
        name,
        "search"
            | "tool_search"
            | "tool_describe"
            | "history_search"
            | "history_read"
            | "nodes"
            | "context"
            | "work_status"
            | "call_lookup"
            | "read_skill"
            | "recall"
            | "fetch_url"
            | "list_bots"
            | "read_bot"
            | "list_sessions"
            | "wait_work"
    );
    let (execution, confirmation) = if unverified {
        ("unknown", "unknown")
    } else if value.get("work_id").is_some() {
        match (state, outcome) {
            (Some("dispatch_rejected"), _) | (_, Some("rejected")) => ("not_started", "confirmed"),
            (_, Some("completed" | "failed" | "interrupted")) => ("completed", "confirmed"),
            (Some("running" | "interrupting"), _) => ("started", "confirmed"),
            _ => ("unknown", "pending"),
        }
    } else if read && name != "call_lookup" {
        ("read", "confirmed")
    } else if value.get("event_hash").is_some()
        || value.get("bot_id").is_some()
        || value.get("message").is_some()
        || value["deleted"] == true
        || value["status"] == "applied"
    {
        ("applied", "confirmed")
    } else if matches!(
        value["status"].as_str(),
        Some("asked" | "declined" | "unchanged")
    ) {
        ("not_started", "confirmed")
    } else {
        ("unknown", "unknown")
    };
    let approval = match value["status"].as_str() {
        Some("asked") => "pending",
        Some("applied") => "allowed",
        Some("declined") => "denied",
        _ if confirmation == "unknown" || value.get("work_id").is_some() => "unknown",
        _ => "not_required",
    };
    value["call_record"] = json!({"tool":name,"call_ref":args.get("call_ref").or_else(|| value.get("call_ref")),"stage":if name == "call_lookup" { "lookup" } else { "result" },"execution":execution,"confirmation":confirmation,"approval":approval,"retry":if matches!(confirmation, "unknown" | "pending") { "lookup_same_ref" } else { "none"}});
    value
}

fn hook_event(agent: triage::Provider, input: &Value) -> Option<triage::AgentEvent> {
    match agent {
        triage::Provider::Codex => {
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
                .then(|| triage::from_hook(agent, input, 0))
                .flatten()
        }
        triage::Provider::Claude => {
            triage::interactive_claude(std::env::var("CLAUDE_CODE_ENTRYPOINT").ok().as_deref())
                .then(|| {
                    let bytes = input["transcript_path"]
                        .as_str()
                        .and_then(|path| fs::metadata(path).ok())
                        .map_or(0, |metadata| metadata.len());
                    triage::from_hook(agent, input, bytes)
                })
                .flatten()
        }
        triage::Provider::Pi | triage::Provider::Opencode => triage::from_hook(agent, input, 0),
        triage::Provider::Cursor => None,
    }
}

async fn hook(options: Hook) -> Result<()> {
    let (mut event, input) = if options.test {
        (
            triage::AgentEvent {
                source: triage::Source::Test,
                agent: options.agent,
                session_id: "test".into(),
                turn_id: uuid::Uuid::new_v4().to_string(),
                cwd: std::env::current_dir()?.to_string_lossy().into(),
                test: true,
                ..Default::default()
            },
            Value::Null,
        )
    } else {
        let mut bytes = Vec::new();
        std::io::stdin().take(1 << 20).read_to_end(&mut bytes)?;
        let input: Value = serde_json::from_slice(&bytes).context("hook input is not JSON")?;
        let Some(event) = hook_event(options.agent, &input) else {
            return Ok(());
        };
        (event, input)
    };
    if !event.cwd.is_empty() {
        event.repo = git(&event.cwd, &["rev-parse", "--show-toplevel"]).and_then(|top| {
            Path::new(&top)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
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
        let url = local_node_url(&options.local).await?;
        post_local(&url, "/api/hooks", &body, Duration::from_secs(5)).await
    }
    .await;
    match sent {
        Ok(result) if options.test => println!("{result}"),
        Ok(result) => {
            if let Some(request_id) = result["request_id"].as_str() {
                let answer = post_local(
                    &local_node_url(&options.local).await?,
                    "/api/hooks/wait",
                    &json!({"request_id":request_id}),
                    Duration::from_secs(triage::HOOK_ANSWER_WITHIN_SECS + 10),
                )
                .await?;
                let output = match options.agent {
                    triage::Provider::Claude => triage::hook_output(&input, &answer),
                    _ => (answer != json!({"decision":null})).then_some(answer),
                };
                if let Some(output) = output {
                    println!("{}", serde_json::to_string(&output)?);
                }
            }
        }
        Err(error) => {
            let mut spool_body = body;
            spool_body["hold"] = Value::Null;
            let spool = options.local.data_dir.join(triage::SPOOL_DIR);
            private_dir(&spool)?;
            private_write(
                &spool.join(format!("{}.json", uuid::Uuid::new_v4())),
                &serde_json::to_vec(&spool_body)?,
            )?;
            return Err(error.context("spooled for the Node's next scan"));
        }
    }
    Ok(())
}

async fn hooks(options: Hooks) -> Result<()> {
    if matches!(options.action, HooksAction::Test) {
        let result = post_local(&local_node_url(&options.local).await?, "/api/hooks", &json!({"source":"test","session_id":"test","turn_id":uuid::Uuid::new_v4().to_string(),"test":true}), Duration::from_secs(5)).await?;
        ensure!(
            result["status"] == "accepted",
            "unexpected answer: {result}"
        );
        println!(
            "Node received the test event {}",
            result["event_id"].as_str().unwrap_or_default()
        );
        return Ok(());
    }
    let program = std::env::current_exe()?;
    let data_dir = options
        .local
        .data_dir
        .canonicalize()
        .unwrap_or_else(|_| options.local.data_dir.clone());
    let home = options
        .home
        .clone()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .filter(|path| path.is_absolute())
        .context("HOME is not an absolute path; pass --home")?;
    let claude_home = options
        .home
        .as_deref()
        .map(|path| path.join(".claude"))
        .unwrap_or_else(|| data_dir.join("providers/claude"));
    let codex_home = options
        .home
        .as_deref()
        .map(|path| path.join(".codex"))
        .unwrap_or_else(|| data_dir.join("providers/codex"));
    let stamp = now_ms();
    let mut planned = Vec::new();
    for file in triage::hook_files_with_claude_root(&home, &claude_home, &codex_home) {
        let mut settings: Value = match fs::read(&file.path) {
            Ok(bytes) if bytes.len() > 1 << 20 => bail!("{} exceeds 1 MiB", file.path.display()),
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
    let extensions = extension_steps(&options, &home, &program, &data_dir)?;
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
            private_dir(parent)?;
        }
        private_write(&path, &serde_json::to_vec_pretty(&settings)?)?;
        println!("{}: changed {}", path.display(), changed.join(", "));
    }
    for (path, step) in extensions {
        match step {
            ExtensionStep::Unchanged => println!("{}: unchanged", path.display()),
            ExtensionStep::Write(_) if options.dry_run => {
                println!("{}: would write", path.display())
            }
            ExtensionStep::Write(code) => {
                if let Some(parent) = path.parent() {
                    private_dir(parent)?;
                }
                private_write(&path, code.as_bytes())?;
                println!(
                    "{}: written; sessions already open load it after a restart",
                    path.display()
                );
            }
            ExtensionStep::Remove if options.dry_run => {
                println!("{}: would remove", path.display())
            }
            ExtensionStep::Remove => {
                fs::remove_file(&path)?;
                println!("{}: removed", path.display());
            }
        }
    }
    if matches!(options.action, HooksAction::Install) {
        let config = codex_home.join("config.toml");
        let enabled = read_text_bounded(&config, 1 << 20)
            .ok()
            .flatten()
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

enum ExtensionStep {
    Unchanged,
    Write(String),
    Remove,
}

fn extension_steps(
    options: &Hooks,
    home: &Path,
    program: &Path,
    data_dir: &Path,
) -> Result<Vec<(PathBuf, ExtensionStep)>> {
    let mut steps = Vec::new();
    for agent in triage::Provider::ALL {
        let Some(triage::Watch::Extension { file, source }) = agent.watch() else {
            continue;
        };
        let path = home.join(file);
        let current = read_text_bounded(&path, 1 << 20)?;
        let ours = current
            .as_deref()
            .is_some_and(|text| text.lines().next() == source.lines().next());
        let code = triage::extension_code(source, program, agent, data_dir);
        let step = match options.action {
            HooksAction::Status | HooksAction::Test => {
                println!(
                    "{}: {}",
                    path.display(),
                    if ours { "installed" } else { "not installed" }
                );
                continue;
            }
            HooksAction::Install if current.as_deref() == Some(code.as_str()) => {
                ExtensionStep::Unchanged
            }
            HooksAction::Install if current.is_some() && !ours => bail!(
                "{} was not written by vibeface; nothing was changed",
                path.display()
            ),
            HooksAction::Install => ExtensionStep::Write(code),
            HooksAction::Uninstall if !ours => ExtensionStep::Unchanged,
            HooksAction::Uninstall => ExtensionStep::Remove,
        };
        steps.push((path, step));
    }
    Ok(steps)
}

async fn log_record(options: LogRecord) -> Result<()> {
    let source = options
        .source
        .unwrap_or(if std::env::var_os("VIBEFACE_TRIAGE_URL").is_some() {
            LogSource::Agent
        } else {
            LogSource::Cli
        });
    ensure!(
        matches!(source, LogSource::Agent | LogSource::Cli),
        "--source is agent or cli"
    );
    let body = json!({"source":source,"records":[{"level":options.level,"message":options.message,"event":options.event,"conversation_id":options.conversation,"work_id":options.work,"thread_id":options.thread}]});
    post_local(
        &local_node_url(&options.local).await?,
        "/api/log",
        &body,
        Duration::from_secs(10),
    )
    .await?;
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LogValue {
    ts_ms: i64,
    level: LogLevel,
    source: LogSource,
    event: String,
    message: String,
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    work_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    fields: serde_json::Map<String, Value>,
}

fn log_matches(record: &LogValue, options: &Logs, since: Option<i64>) -> bool {
    let same = |want: &Option<String>, have: &Option<String>| {
        want.as_ref().is_none_or(|want| have.as_ref() == Some(want))
    };
    since.is_none_or(|since| record.ts_ms >= since)
        && options.level.is_none_or(|level| record.level >= level)
        && (options.source.is_empty() || options.source.contains(&record.source))
        && same(&options.conversation, &record.conversation_id)
        && same(&options.work, &record.work_id)
        && options.grep.as_deref().is_none_or(|needle| {
            let needle = needle.to_lowercase();
            [
                Some(&record.message),
                Some(&record.event),
                record.conversation_id.as_ref(),
                record.work_id.as_ref(),
                record.thread_id.as_ref(),
            ]
            .into_iter()
            .flatten()
            .any(|part| part.to_lowercase().contains(&needle))
                || Value::Object(record.fields.clone())
                    .to_string()
                    .to_lowercase()
                    .contains(&needle)
        })
}

async fn logs(options: Logs) -> Result<()> {
    let current = options.data_dir.join(LOG_FILE);
    ensure!(
        current.exists(),
        "no Node log in {}; start the Node with this --data-dir first",
        options.data_dir.display()
    );
    let since = options.since.as_deref().map(since_ms).transpose()?;
    let print = |record: &LogValue| -> Result<()> {
        let line = if options.json {
            serde_json::to_string(record)?
        } else {
            log_line(record)
        };
        let mut out = std::io::stdout().lock();
        if writeln!(out, "{line}").is_err() {
            std::process::exit(0);
        }
        Ok(())
    };
    let mut records = Vec::new();
    for path in log_files(&options.data_dir) {
        let file = fs::File::open(path)?;
        for line in std::io::BufReader::new(file).split(b'\n') {
            let line = line?;
            if line.len() > MAX_LOG_RECORD {
                continue;
            }
            if let Ok(record) = serde_json::from_slice::<LogValue>(&line) {
                if log_matches(&record, &options, since) {
                    records.push(record);
                    if records.len() > options.limit.max(1) {
                        records.remove(0);
                    }
                }
            }
        }
    }
    for record in &records {
        print(record)?;
    }
    if !options.follow {
        return Ok(());
    }
    let mut offset = fs::metadata(&current).map(|m| m.len()).unwrap_or(0);
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let Some(len) = fs::metadata(&current).ok().map(|m| m.len()) else {
            continue;
        };
        if len < offset {
            offset = 0;
        }
        if len == offset {
            continue;
        }
        let mut file = fs::File::open(&current)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1);
        offset += complete as u64;
        for line in bytes[..complete].split(|byte| *byte == b'\n') {
            if line.len() <= MAX_LOG_RECORD {
                if let Ok(record) = serde_json::from_slice::<LogValue>(line) {
                    if log_matches(&record, &options, since) {
                        print(&record)?;
                    }
                }
            }
        }
    }
}

fn log_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for index in (1..=4).rev() {
        let path = dir.join(format!("{LOG_FILE}.{index}"));
        if path.exists() {
            files.push(path);
        }
    }
    files.push(dir.join(LOG_FILE));
    files
}

fn log_line(record: &LogValue) -> String {
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

fn utc_time(ms: i64) -> String {
    let (days, rest) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
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

fn since_ms(text: &str) -> Result<i64> {
    let trimmed = text.trim();
    let number_end = trimmed.trim_end_matches(char::is_alphabetic).len();
    let (number, unit) = trimmed.split_at(number_end);
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
    Ok(now_ms() - number * unit_ms)
}

async fn triage_post(local: &LocalNode, mut body: Value) -> Result<()> {
    if let Ok(attempt) = std::env::var("VIBEFACE_TRIAGE_ATTEMPT") {
        body["_attempt"] = json!(attempt);
    }
    let result = post_local(
        &local_node_url(local).await?,
        "/api/triage",
        &body,
        Duration::from_secs(30),
    )
    .await?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

async fn post_triage(local: &LocalNode, body: Value) -> Result<()> {
    triage_post(local, body).await
}

async fn local_node_url(local: &LocalNode) -> Result<Url> {
    let value = match local
        .node_url
        .clone()
        .or_else(|| std::env::var("VIBEFACE_TRIAGE_URL").ok())
    {
        Some(value) => value,
        None => {
            let info = control(
                &local.data_dir,
                ControlRequest::Inspect {
                    data_dir: local.data_dir.clone(),
                },
            )
            .await?;
            format!(
                "http://{}",
                info["local_listen"]
                    .as_str()
                    .context("core did not return a local listener")?
            )
        }
    };
    let url = origin(&value)?;
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
        .post(url.join(path.trim_start_matches('/'))?)
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
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && !text.is_empty()).then_some(text)
}

fn private_dir(path: &Path) -> Result<()> {
    let created = if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "private path is not a private directory"
        );
        false
    } else {
        fs::create_dir_all(path)?;
        true
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if created {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "private file path is not a regular file"
        );
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn provider_bin(value: &str) -> Result<(triage::Provider, PathBuf), String> {
    let (provider, path) = value.split_once('=').ok_or("use PROVIDER=PATH")?;
    Ok((
        triage::Provider::from_str(provider, false).map_err(|_| "unknown provider")?,
        path.into(),
    ))
}

fn absolute_path(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("specify an absolute path".into())
    }
}

fn data_dir(value: &str) -> Result<PathBuf, String> {
    if value.is_empty() {
        Err(NO_HOME.into())
    } else {
        absolute_path(value)
    }
}

fn default_data_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join(".vibeface"))
        .unwrap_or_default()
}

fn data_dir_or_home(dir: Option<PathBuf>) -> Result<PathBuf> {
    let dir = dir.unwrap_or_else(default_data_dir);
    ensure!(!dir.as_os_str().is_empty(), NO_HOME);
    ensure!(dir.is_absolute(), "specify an absolute data directory");
    Ok(dir)
}

fn local_listen() -> SocketAddr {
    DEFAULT_LOCAL_LISTEN
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triage_attention_kind_parses_and_serializes_for_the_existing_request_boundary() {
        let base = [
            "vibeface",
            "triage",
            "raise",
            "--event-id",
            "event",
            "--severity",
            "attention",
            "--question",
            "Continue in this browser",
            "--request-id",
            "request",
            "--choice",
            "Continue",
        ];
        for (flag, expected) in [
            (None, Value::Null),
            (Some("question"), json!("question")),
            (Some("approval"), json!("approval")),
            (Some("browser-takeover"), json!("browser_takeover")),
        ] {
            let mut args = base.to_vec();
            if let Some(flag) = flag {
                args.extend(["--attention-kind", flag]);
            }
            let Some(Action::Triage(options)) = Cli::try_parse_from(args).unwrap().command else {
                panic!("expected triage")
            };
            let body = serde_json::to_value(options.action).unwrap();
            assert_eq!(
                body,
                json!({"action":"raise","event_id":"event","severity":"attention","question":"Continue in this browser","request_id":"request","attention_kind":expected,"choices":["Continue"]})
            );
        }
    }

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
        let dir =
            std::env::temp_dir().join(format!("vibeface-wallet-cli-{}", uuid::Uuid::new_v4()));
        private_dir(&dir)?;
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
        use vibeface_control::agent::{Command as AgentCommand, run};
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
                AgentCommand::Call {
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
    }

    #[test]
    fn data_directory_defaults_to_home_unless_named() {
        assert_eq!(
            data_dir_or_home(Some(PathBuf::from("/arg"))).unwrap(),
            Path::new("/arg")
        );
        assert!(data_dir_or_home(Some(PathBuf::from("relative"))).is_err());
        assert!(data_dir_or_home(Some(PathBuf::new())).is_err());
    }

    #[test]
    fn cli_direct_start_keeps_data_directory_arguments_unambiguous() {
        let default = Cli::try_parse_from(["vibeface"]).unwrap();
        assert!(default.command.is_none() && default.data_dir.is_none());
        let direct = Cli::try_parse_from(["vibeface", "--data-dir", "/private/node"]).unwrap();
        assert!(direct.command.is_none());
        assert_eq!(direct.data_dir.unwrap(), Path::new("/private/node"));
        let explicit =
            Cli::try_parse_from(["vibeface", "serve", "--data-dir", "/private/node"]).unwrap();
        let Some(Action::Serve(options)) = explicit.command else {
            panic!("expected serve")
        };
        assert_eq!(options.data_dir, Path::new("/private/node"));
        assert!(
            Cli::try_parse_from([
                "vibeface",
                "--data-dir",
                "/one",
                "serve",
                "--data-dir",
                "/two"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["vibeface", "--data-dir", "relative"]).is_err());
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

    #[tokio::test]
    async fn hooks_install_writes_pi_extension_once_and_removes_only_its_own() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("vibeface-hooks-{}", uuid::Uuid::new_v4()));
        let home = dir.join("home");
        let data = dir.join("data");
        let run = |action| {
            hooks(Hooks {
                action,
                local: LocalNode {
                    data_dir: data.clone(),
                    node_url: None,
                },
                home: Some(home.clone()),
                dry_run: false,
            })
        };
        run(HooksAction::Install).await?;
        let path = home.join(".pi/agent/extensions/vibeface.ts");
        let code = fs::read_to_string(&path)?;
        assert!(code.starts_with("// vibeface:") && code.contains("const HOOK ="));
        run(HooksAction::Install).await?;
        assert_eq!(fs::read_to_string(&path)?, code);
        run(HooksAction::Uninstall).await?;
        assert!(!path.exists());
        fs::write(&path, "// the user's own\n")?;
        assert!(run(HooksAction::Install).await.is_err());
        run(HooksAction::Uninstall).await?;
        assert_eq!(fs::read_to_string(&path)?, "// the user's own\n");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn hooks_install_writes_the_opencode_plugin_once() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("vibeface-hooks-{}", uuid::Uuid::new_v4()));
        let home = dir.join("home");
        let data = dir.join("data");
        let run = |action| {
            hooks(Hooks {
                action,
                local: LocalNode {
                    data_dir: data.clone(),
                    node_url: None,
                },
                home: Some(home.clone()),
                dry_run: false,
            })
        };
        run(HooksAction::Install).await?;
        let path = home.join(".config/opencode/plugins/vibeface.ts");
        let code = fs::read_to_string(&path)?;
        assert!(code.contains("const HOOK ="));
        run(HooksAction::Install).await?;
        assert_eq!(fs::read_to_string(&path)?, code);
        run(HooksAction::Uninstall).await?;
        assert!(!path.exists());
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn hooks_install_uses_node_owned_provider_roots_by_default() -> Result<()> {
        use std::sync::{Mutex, OnceLock};
        static HOME_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = HOME_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = std::env::temp_dir().join(format!("vibeface-hooks-{}", uuid::Uuid::new_v4()));
        let home = dir.join("home");
        let data = dir.join("data");
        let previous = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }
        let result = hooks(Hooks {
            action: HooksAction::Install,
            local: LocalNode {
                data_dir: data.clone(),
                node_url: None,
            },
            home: None,
            dry_run: false,
        })
        .await;
        match previous {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        result?;
        assert!(data.join("providers/codex/hooks.json").is_file());
        assert!(data.join("providers/claude/settings.json").is_file());
        assert!(!home.join(".codex").exists() && !home.join(".claude").exists());
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
