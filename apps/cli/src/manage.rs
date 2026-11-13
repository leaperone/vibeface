//! Everything the web settings and management pages do, from a terminal.
//!
//! Node settings, providers, reads and relay settings go over the owner-only maintenance socket
//! as this Node's local operator. Governance and the user's own signed writes go through
//! `vibeface client`, a management client with its own key, approved like a browser.

use crate::{data_dir, default_data_dir, read_passphrase};
use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    io::{IsTerminal, Read},
    path::{Path, PathBuf},
    time::Duration,
};
use vibeface_client::{self as client, ClientKey, Enrollment, Entry, Progress};
use vibeface_control::{ControlRequest, maintenance};

#[derive(Args)]
pub struct NodeDir {
    /// Node data directory.
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = data_dir, default_value_os_t = default_data_dir())]
    pub data_dir: PathBuf,
}

/// The JSON a sugar command prints.
fn print(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn json_arg(text: Option<&str>) -> Result<Value> {
    let value: Value =
        serde_json::from_str(text.unwrap_or("{}")).context("params must be a JSON object")?;
    ensure!(value.is_object(), "params must be a JSON object");
    Ok(value)
}

/// `text`, or standard input when it is `-`, which keeps a secret off the command line.
fn arg_or_stdin(text: String) -> Result<String> {
    if text != "-" {
        return Ok(text);
    }
    let mut input = String::new();
    std::io::stdin()
        .take((1 << 20) + 1)
        .read_to_string(&mut input)
        .context("read standard input")?;
    ensure!(
        input.len() <= 1 << 20,
        "standard input holds more than 1 MiB"
    );
    Ok(input.trim_end_matches(['\r', '\n']).to_owned())
}

/// Calls `method` on the running Node as its local operator.
pub async fn operator(data_dir: &Path, method: &str, params: Value) -> Result<Value> {
    let socket = crate::inspect_socket(data_dir).await?;
    maintenance::call(&socket, method, params).await
}

#[derive(Args)]
pub struct Call {
    /// Client method, such as node_settings or rpc.
    pub method: String,
    /// Parameters as a JSON object.
    pub params: Option<String>,
    #[command(flatten)]
    pub node: NodeDir,
}

pub async fn call(options: Call) -> Result<()> {
    let params = json_arg(options.params.as_deref())?;
    print(&operator(&options.node.data_dir, &options.method, params).await?)
}

#[derive(Args)]
pub struct NodeOptions {
    #[command(flatten)]
    pub node: NodeDir,
    #[command(subcommand)]
    pub command: NodeAction,
}

#[derive(Subcommand)]
pub enum NodeAction {
    /// Show project roots, the bot and provider settings.
    Settings,
    /// Change project roots or the bot; what is not given stays.
    Configure {
        /// Project root; repeat for several. Replaces the current list.
        #[arg(long = "root", value_parser = crate::absolute_path)]
        roots: Vec<PathBuf>,
        /// Remove every project root.
        #[arg(long, conflicts_with = "roots")]
        no_roots: bool,
        /// The bot as JSON: {"provider","workspace","model","reasoning_effort"}.
        #[arg(long)]
        manager: Option<String>,
        /// Stop running a bot on this Node.
        #[arg(long, conflicts_with = "manager")]
        no_manager: bool,
    },
    /// Show whether this Node may write.
    WriteState,
    /// Confirm this Node may write again after it was restored.
    ConfirmWritable,
    /// Collect diagnostics.
    Diagnostics {
        #[arg(long)]
        since_ms: Option<i64>,
        #[arg(long)]
        until_ms: Option<i64>,
    },
    /// Start over after this Node was removed from the personal network.
    Rejoin,
}

pub async fn node(options: NodeOptions) -> Result<()> {
    let dir = &options.node.data_dir;
    let result = match options.command {
        NodeAction::Settings => operator(dir, "node_settings", json!({})).await?,
        NodeAction::Configure {
            roots,
            no_roots,
            manager,
            no_manager,
        } => {
            let current = operator(dir, "node_settings", json!({})).await?;
            let roots = if no_roots {
                json!([])
            } else if roots.is_empty() {
                current["settings"]["roots"].clone()
            } else {
                json!(roots)
            };
            let manager = match (manager, no_manager) {
                (Some(manager), _) => {
                    serde_json::from_str(&manager).context("--manager must be JSON")?
                }
                (None, true) => Value::Null,
                (None, false) => current["settings"]["manager"].clone(),
            };
            operator(
                dir,
                "configure_node",
                json!({"roots":roots,"manager":manager}),
            )
            .await?
        }
        NodeAction::WriteState => operator(dir, "write_state", json!({})).await?,
        NodeAction::ConfirmWritable => operator(dir, "confirm_writable", json!({})).await?,
        NodeAction::Diagnostics { since_ms, until_ms } => {
            operator(
                dir,
                "diagnostics",
                json!({"since_ms":since_ms,"until_ms":until_ms}),
            )
            .await?
        }
        NodeAction::Rejoin => operator(dir, "rejoin", json!({})).await?,
    };
    print(&result)
}

#[derive(Args)]
pub struct ProviderOptions {
    #[command(flatten)]
    pub node: NodeDir,
    #[command(subcommand)]
    pub command: ProviderAction,
}

#[derive(Subcommand)]
pub enum ProviderAction {
    /// Change a provider: {"bot","execution","execution_model","bin","app_server","connect_local","env","opencode_endpoint"}.
    Configure {
        #[arg(value_enum)]
        provider: crate::triage::Provider,
        /// The change as a JSON object; absent fields keep their value. `-` reads it from
        /// standard input, for one holding API keys.
        change: String,
    },
    /// Check whether a provider is ready.
    Probe {
        #[arg(value_enum)]
        provider: crate::triage::Provider,
    },
    /// List a provider's models.
    Models {
        #[arg(value_enum)]
        provider: crate::triage::Provider,
    },
    /// Test the Codex App Server connection.
    TestCodex,
}

pub async fn provider(options: ProviderOptions) -> Result<()> {
    let dir = &options.node.data_dir;
    let result = match options.command {
        ProviderAction::Configure { provider, change } => {
            let mut change = json_arg(Some(&arg_or_stdin(change)?))?;
            change["provider"] = json!(provider.name());
            operator(dir, "configure_provider", change).await?
        }
        ProviderAction::Probe { provider } => {
            operator(dir, "provider_probe", json!({"provider":provider.name()})).await?
        }
        ProviderAction::Models { provider } => {
            operator(dir, "provider_models", json!({"provider":provider.name()})).await?
        }
        ProviderAction::TestCodex => operator(dir, "test_codex_app_server", json!({})).await?,
    };
    print(&result)
}

#[derive(Args)]
pub struct DeliveryOptions {
    #[command(flatten)]
    pub node: NodeDir,
    #[command(subcommand)]
    pub command: DeliveryAction,
}

#[derive(Subcommand)]
pub enum DeliveryAction {
    /// Show whether delivery is on and its wallet.
    Status,
    /// List delivery orders, newest first.
    List,
    /// Show one order.
    Get { order_id: String },
    /// Nodes in the public mesh that offer delivery.
    Providers,
}

pub async fn delivery(options: DeliveryOptions) -> Result<()> {
    let params = match options.command {
        DeliveryAction::Status => json!({"op":"status"}),
        DeliveryAction::List => json!({"op":"list"}),
        DeliveryAction::Get { order_id } => json!({"op":"get","order_id":order_id}),
        DeliveryAction::Providers => json!({"op":"providers"}),
    };
    print(&operator(&options.node.data_dir, "delivery", params).await?)
}

#[derive(Args)]
pub struct RelayOptions {
    #[command(flatten)]
    pub node: NodeDir,
    #[command(subcommand)]
    pub command: RelayAction,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Switch {
    On,
    Off,
}

#[derive(Subcommand)]
pub enum RelayAction {
    /// Show the public relay this Node volunteers and how others see it.
    Status,
    /// Offer this Node's relay to the public mesh, or stop.
    Volunteer {
        #[arg(value_enum)]
        state: Switch,
        /// The relay's URL as others reach it.
        #[arg(long)]
        url: Option<String>,
        /// Address the relay listens on.
        #[arg(long)]
        listen: Option<String>,
        /// Bytes a second read from each connection.
        #[arg(long)]
        rate: Option<u64>,
        /// Connections from outside the personal network held at once.
        #[arg(long)]
        max_clients: Option<u64>,
        /// Connections taken from one address.
        #[arg(long)]
        max_clients_per_ip: Option<u64>,
    },
}

fn relay_change(command: RelayAction) -> Option<Value> {
    let RelayAction::Volunteer {
        state,
        url,
        listen,
        rate,
        max_clients,
        max_clients_per_ip,
    } = command
    else {
        return None;
    };
    let mut change = json!({"volunteer":matches!(state, Switch::On)});
    for (key, value) in [
        ("url", url.map(Value::from)),
        ("listen", listen.map(Value::from)),
        ("rate", rate.map(Value::from)),
        ("max_clients", max_clients.map(Value::from)),
        ("max_clients_per_ip", max_clients_per_ip.map(Value::from)),
    ] {
        if let Some(value) = value {
            change[key] = value;
        }
    }
    Some(change)
}

pub async fn relay(options: RelayOptions) -> Result<()> {
    let dir = &options.node.data_dir;
    let result = match relay_change(options.command) {
        Some(change) => operator(dir, "configure_relay", change).await?,
        None => operator(dir, "relay_settings", json!({})).await?,
    };
    print(&result)
}

#[derive(Args)]
pub struct ConfigOptions {
    #[command(flatten)]
    pub node: NodeDir,
    #[command(subcommand)]
    pub command: ConfigAction,
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Set one config.json value, such as mesh.allow_private true or mesh.bootstrap '[...]'.
    /// The value is JSON, or text when it is not; null restores the default; `-` reads it from
    /// standard input.
    Set { key: String, value: String },
}

fn config_value(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

pub async fn config(options: ConfigOptions) -> Result<()> {
    let ConfigAction::Set { key, value } = options.command;
    let dir = options.node.data_dir;
    let result = crate::control(ControlRequest::ConfigSet {
        data_dir: dir.clone(),
        key: key.clone(),
        value: config_value(&arg_or_stdin(value)?),
    })
    .await?;
    if result["restart_required"] == true {
        println!("Saved {key}; restart the Node to apply it.");
    } else {
        println!("Saved {key}.");
    }
    Ok(())
}

/// Where the management client keeps its key and how it unlocks it.
#[derive(Args)]
pub struct ClientAccess {
    /// Directory holding this client's sealed key.
    #[arg(long, global = true, env = "VIBEFACE_CLIENT_DIR", value_parser = data_dir, default_value_os_t = default_client_dir())]
    pub client_dir: PathBuf,
    /// Protected channel holding the client key passphrase: a file descriptor on Unix, a
    /// handle value on Windows.
    #[arg(long, global = true)]
    pub passphrase_fd: Option<i64>,
    /// Gateway the client connects through; the local Node's by default.
    #[arg(long, global = true)]
    pub url: Option<String>,
    /// Target Node for an existing client or recovery. Defaults to the remembered/local Node.
    #[arg(long, global = true)]
    pub node: Option<String>,
    /// Node data directory, for the local Gateway and an empty Node's capability.
    #[arg(long, global = true, env = "VIBEFACE_DATA_DIR", value_parser = data_dir, default_value_os_t = default_data_dir())]
    pub data_dir: PathBuf,
    /// Named identity profile beneath the client directory.
    #[arg(long, global = true)]
    pub profile: Option<String>,
    /// Print compact machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,
    /// Private output file for an explicitly requested one-time decision token.
    #[arg(long, global = true)]
    pub secret_output: Option<PathBuf>,
}

fn default_client_dir() -> PathBuf {
    let home = default_data_dir();
    if home.as_os_str().is_empty() {
        return home;
    }
    home.with_file_name(".vibeface-client")
}

impl ClientAccess {
    pub(crate) fn resolved_client_dir(&self) -> Result<PathBuf> {
        match self.profile.as_deref() {
            None => Ok(self.client_dir.clone()),
            Some(name) => {
                ensure!(
                    !name.is_empty()
                        && name.len() <= 64
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
                    "profile must contain 1 to 64 letters, numbers, dashes or underscores"
                );
                Ok(self.client_dir.join("profiles").join(name))
            }
        }
    }

    pub(crate) fn unlock(&self) -> Result<ClientKey> {
        let dir = self.resolved_client_dir()?;
        ensure!(
            ClientKey::exists(&dir),
            "no client key yet; run vibeface client enroll or vibeface client create-network"
        );
        let passphrase = read_passphrase(self.passphrase_fd, "client key passphrase", false)?;
        ClientKey::open(&dir, passphrase.as_str())
    }

    /// The existing key, or a new one sealed with a passphrase entered now.
    fn unlock_or_create(&self) -> Result<ClientKey> {
        let dir = self.resolved_client_dir()?;
        if ClientKey::exists(&dir) {
            return self.unlock();
        }
        let passphrase = read_passphrase(self.passphrase_fd, "new client key passphrase", true)?;
        ClientKey::create(&dir, passphrase.as_str())
    }

    /// Explicit routing overrides retain the enrolled identity and network.
    pub(crate) fn entry(&self, key: &ClientKey) -> Result<Option<Entry>> {
        if self.url.is_none() && self.node.is_none() {
            return Ok(None);
        }
        let remembered = key.entry();
        Ok(Some(Entry {
            origin: self
                .url
                .clone()
                .or_else(|| remembered.map(|entry| entry.origin.clone()))
                .context("this target needs --url or a remembered Gateway")?,
            node_id: self
                .node
                .clone()
                .or_else(|| remembered.map(|entry| entry.node_id.clone()))
                .context("this target needs --node or an enrolled entry")?,
        }))
    }

    /// The local Node's Gateway and device ID, or `--url` with `node`.
    async fn local_entry(&self, node: Option<String>) -> Result<Entry> {
        let node = node.or_else(|| self.node.clone());
        if let (Some(origin), Some(node_id)) = (&self.url, &node) {
            let entry = Entry {
                origin: origin.clone(),
                node_id: node_id.clone(),
            };
            client::Endpoint::Gateway(entry.clone()).validate()?;
            return Ok(entry);
        }
        let info = crate::control(ControlRequest::Inspect {
            data_dir: self.data_dir.clone(),
        })
        .await;
        let origin = match (&self.url, &info) {
            (Some(url), _) => url.clone(),
            (None, Ok(info)) => info["public_url"]
                .as_str()
                .context("the local Node has no Gateway URL")?
                .to_owned(),
            (None, Err(error)) => bail!("no local Node ({error:#}); pass --url"),
        };
        let node_id = match (node, &info) {
            (Some(node), _) => node,
            (None, Ok(info)) => info["device_id"]
                .as_str()
                .context("the local Node has no device ID")?
                .to_owned(),
            (None, Err(error)) => bail!("no local Node ({error:#})"),
        };
        Ok(Entry { origin, node_id })
    }

    /// The one-time capability `init` left for whoever creates the network on this Node.
    fn capability(&self) -> Result<String> {
        let bytes = std::fs::read(self.data_dir.join("bootstrap.json"))
            .context("this Node has no unused capability; run this on the Node's computer")?;
        let value: Value = serde_json::from_slice(&bytes)?;
        Ok(value["capability"]
            .as_str()
            .context("bootstrap.json is damaged")?
            .to_owned())
    }

    async fn capability_for(&self, entry: &Entry) -> Result<String> {
        let info = crate::control(ControlRequest::Inspect {
            data_dir: self.data_dir.clone(),
        })
        .await?;
        ensure!(
            info["device_id"].as_str() == Some(entry.node_id.as_str()),
            "setup capability belongs to the local Node; select it or recover an initialized remote Node"
        );
        self.capability()
    }
}

#[derive(Args)]
pub struct ClientOptions {
    #[command(flatten)]
    pub access: ClientAccess,
    #[command(subcommand)]
    pub command: ClientAction,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Settle {
    Approve,
    Veto,
}

#[derive(Subcommand)]
pub enum ClientAction {
    #[command(flatten)]
    Lifecycle(crate::client_lifecycle::LifecycleCommand),
    #[command(flatten)]
    Business(crate::client_commands::BusinessCommand),
    #[command(flatten)]
    Access(crate::cli_client::ClientCommand),
    /// Create a personal network on this empty Node, with this client as its first manager.
    CreateNetwork {
        /// Where to save the new recovery file; it must not exist yet.
        #[arg(long, value_parser = crate::absolute_path)]
        recovery_file: PathBuf,
        /// This client's name in the network.
        #[arg(long, default_value = "CLI")]
        name: String,
        /// The Node's name in the network.
        #[arg(long)]
        node_name: Option<String>,
        /// Protected channel holding the recovery password.
        #[arg(long)]
        recovery_password_fd: Option<i64>,
    },
    /// Join a personal network with an invite link; a manager approves it.
    Enroll {
        /// The invite link; `-` reads it from standard input.
        link: String,
        #[arg(long, default_value = "CLI")]
        name: String,
    },
    /// Show this client's membership and the directory.
    Status,
    /// Create an invite link for a new device.
    Invite,
    /// List the devices waiting to join.
    Requests,
    /// Approve or reject a device waiting to join.
    Approve {
        join_id: String,
        /// Let the device manage the network.
        #[arg(long)]
        manage: bool,
        /// The short code the joining device shows; asked for when absent.
        #[arg(long)]
        code: Option<String>,
        #[arg(long, conflicts_with_all = ["manage", "code"])]
        reject: bool,
    },
    /// Rename a device or change whether it manages the network.
    Grant {
        device_id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, conflicts_with = "no_manage")]
        manage: bool,
        #[arg(long)]
        no_manage: bool,
    },
    /// Remove a device from the network; your own ID leaves it.
    Revoke { device_id: String },
    /// Approve the waiting recovery now, or stop it.
    Recovery {
        #[arg(value_enum)]
        decision: Settle,
    },
    /// Recover the network with a recovery file; it takes effect after the recovery wait.
    Recover {
        #[arg(long, value_parser = crate::absolute_path)]
        recovery_file: PathBuf,
        /// Where to save the recovery file that replaces it; it must not exist yet.
        #[arg(long, value_parser = crate::absolute_path, required_unless_present = "review")]
        new_recovery_file: Option<PathBuf>,
        #[arg(long, default_value = "CLI")]
        name: String,
        /// Retained Member[] as JSON, @FILE, or - for stdin. Keep the connected Node;
        /// omit this new client, which joins as a manager. Omit to retain all current members.
        #[arg(long, value_name = "JSON|@FILE|-")]
        members: Option<String>,
        /// Print verified candidate members without creating a recovery file or submitting recovery.
        #[arg(long, conflicts_with = "members")]
        review: bool,
        /// Protected channel holding the recovery password.
        #[arg(long)]
        recovery_password_fd: Option<i64>,
    },
    /// Call a client method as this management client.
    Call {
        method: String,
        params: Option<String>,
    },
}

pub async fn client(options: ClientOptions) -> Result<()> {
    let access = options.access;
    match options.command {
        ClientAction::Lifecycle(command) => crate::client_lifecycle::run(&access, command).await,
        ClientAction::Business(command) => crate::cli_client::business(&access, command).await,
        ClientAction::Access(command) => crate::cli_client::run(&access, command).await,
        ClientAction::CreateNetwork {
            recovery_file,
            name,
            node_name,
            recovery_password_fd,
        } => {
            let entry = access.local_entry(None).await?;
            let capability = zeroize::Zeroizing::new(access.capability_for(&entry).await?);
            let mut key = access.unlock_or_create()?;
            let password = read_passphrase(recovery_password_fd, "recovery password", true)?;
            client::IdentityPlan::discard_applied(&key, password.as_str())?;
            let mut pending = if client::IdentityPlan::exists(&key) {
                let info = client::IdentityPlan::inspect(&key, password.as_str())?
                    .context("pending identity operation missing")?;
                ensure!(
                    info.kind == client::IdentityPlanKind::Creation
                        && info.target == entry
                        && info.backup_path == recovery_file,
                    "another identity operation is pending; inspect it with client identity-plan status"
                );
                client::IdentityPlan::load(&key, password.as_str())?
            } else {
                client::prepare_create(
                    &key,
                    entry,
                    &capability,
                    &name,
                    node_name.as_deref(),
                    &recovery_file,
                    password.as_str(),
                )
                .await?
            };
            eprintln!(
                "Recovery backup saved and verified: {} (SHA-256 {})",
                pending.backup_path().display(),
                pending.backup_sha256()
            );
            let created = pending.submit(&mut key).await?;
            if pending.state() == client::IdentityOperationState::Applied {
                pending.discard()?;
            }
            if access.json {
                return crate::cli_client::session::print_value(
                    &json!({"network_id":created["network_id"],"principal":created["principal"],"recovery_file":recovery_file,"state":"applied"}),
                    true,
                );
            }
            println!(
                "Personal network {} created; this client manages it.",
                created["network_id"].as_str().unwrap_or_default()
            );
            println!(
                "Keep {} and its password safe: they recover the network.",
                recovery_file.display()
            );
            Ok(())
        }
        ClientAction::Enroll { link, name } => {
            ensure!(
                access.node.is_none(),
                "an invitation already selects its Node; omit --node when enrolling"
            );
            ensure!(
                link != "-" || access.passphrase_fd != Some(0),
                "the invite link and the passphrase cannot both come from standard input"
            );
            let link = arg_or_stdin(link)?;
            let origin = match &access.url {
                Some(url) => url.clone(),
                None => access.local_entry(None).await?.origin,
            };
            let mut key = access.unlock_or_create()?;
            let (network, entry) = {
                let mut enrollment = Enrollment::start(&key, &origin, &link, &name).await?;
                let mut shown = None;
                loop {
                    match enrollment.poll().await? {
                        Progress::Waiting => {}
                        Progress::Code(code) => {
                            if shown.as_ref() != Some(&code) {
                                eprintln!(
                                    "Short code {code}: check that the approving device shows the same."
                                );
                                shown = Some(code);
                            }
                        }
                        Progress::Joined => break,
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                enrollment.finish()?
            };
            key.remember(&network, entry)?;
            if access.json {
                return crate::cli_client::session::print_value(
                    &json!({"network_id":network,"client_id":key.device_id(),"joined":true}),
                    true,
                );
            }
            println!("Joined personal network {network}.");
            Ok(())
        }
        ClientAction::Recover {
            recovery_file,
            new_recovery_file,
            name,
            members,
            review,
            recovery_password_fd,
        } => {
            ensure!(
                members.as_deref() != Some("-")
                    || (access.passphrase_fd != Some(0) && recovery_password_fd != Some(0)),
                "--members and passwords cannot both come from standard input"
            );
            let members: Option<Vec<_>> = members
                .as_deref()
                .map(|source| {
                    serde_json::from_value(crate::client_commands::json_document(source)?)
                        .context("--members must be a JSON array of Member objects")
                })
                .transpose()?;
            let entry = access.local_entry(None).await?;
            let mut key = access.unlock_or_create()?;
            let password = read_passphrase(recovery_password_fd, "recovery password", false)?;
            if review {
                let saved = client::IdentityPlan::inspect(&key, password.as_str())?;
                let (source, review) = if saved
                    .is_some_and(|info| info.state != client::IdentityOperationState::Applied)
                {
                    (
                        "saved_plan",
                        client::IdentityPlan::recovery_review(&key, password.as_str(), &entry)?,
                    )
                } else {
                    (
                        "current_state",
                        client::review_recovery(
                            &key,
                            entry,
                            &recovery_file,
                            password.as_str(),
                            client::RecoveryOptions {
                                name: &name,
                                capability: None,
                                members: None,
                            },
                        )
                        .await?,
                    )
                };
                let mut result = serde_json::to_value(review)?;
                result["source"] = json!(source);
                return crate::cli_client::session::print_value(&result, access.json);
            }
            let new_recovery_file =
                new_recovery_file.context("--new-recovery-file is required to submit recovery")?;
            client::IdentityPlan::discard_applied(&key, password.as_str())?;
            let capability = access
                .capability_for(&entry)
                .await
                .ok()
                .map(zeroize::Zeroizing::new);
            let mut pending = if client::IdentityPlan::exists(&key) {
                let pending = client::IdentityPlan::load(&key, password.as_str())?;
                pending.validate_recovery_intent(
                    &entry,
                    &new_recovery_file,
                    &name,
                    members.as_deref(),
                )?;
                pending
            } else {
                client::prepare_recovery(
                    &key,
                    entry,
                    &recovery_file,
                    password.as_str(),
                    &new_recovery_file,
                    client::RecoveryOptions {
                        name: &name,
                        capability: capability.as_deref().map(String::as_str),
                        members: members.as_deref(),
                    },
                )
                .await?
            };
            eprintln!(
                "Recovery members: {}",
                serde_json::to_string(
                    pending
                        .recovery_members()
                        .context("recovery members missing")?
                )?
            );
            eprintln!(
                "Recovery backup saved and verified: {} (SHA-256 {})",
                pending.backup_path().display(),
                pending.backup_sha256()
            );
            let result = pending.submit(&mut key).await?;
            let state = pending.state();
            if state == client::IdentityOperationState::Applied {
                pending.discard()?;
            }
            if !access.json {
                eprintln!(
                    "Recovery state: {state:?}; recovery file: {}",
                    new_recovery_file.display()
                );
            }
            crate::cli_client::session::print_value(
                &json!({"principal":result["principal"],"recovery":result["recovery"],"state":state,"recovery_file":new_recovery_file}),
                access.json,
            )
        }
        command => {
            let key = access.unlock()?;
            let entry = access.entry(&key)?;
            let mut session = key.connect(entry.as_ref()).await?;
            signed(&mut session, command, access.json).await
        }
    }
}

async fn signed(
    session: &mut client::Session<'_>,
    command: ClientAction,
    compact: bool,
) -> Result<()> {
    let result = match command {
        ClientAction::Status => {
            let mut authenticated = session.call("authenticate", json!({})).await?;
            json!({"principal":authenticated["principal"].take(),"view":authenticated["view"].take()})
        }
        ClientAction::Invite => {
            let link = session.invite().await?;
            if compact {
                return crate::cli_client::session::print_value(
                    &json!({"link":link,"single_use":true,"expires_in_seconds":600}),
                    true,
                );
            }
            println!("{link}");
            println!("The link works once, for ten minutes.");
            return Ok(());
        }
        ClientAction::Requests => json!(session.requests().await?),
        ClientAction::Approve {
            join_id,
            manage,
            code,
            reject,
        } => {
            if reject {
                session.reject_join(&join_id).await?
            } else {
                approve(session, &join_id, manage, code).await?
            }
        }
        ClientAction::Grant {
            device_id,
            name,
            manage,
            no_manage,
        } => {
            let manage = (manage || no_manage).then_some(manage);
            session.grant(&device_id, name.as_deref(), manage).await?["principal"].take()
        }
        ClientAction::Revoke { device_id } => session.revoke(&device_id).await?["principal"].take(),
        ClientAction::Recovery { decision } => {
            let mut settled = session
                .settle_recovery(matches!(decision, Settle::Approve))
                .await?;
            json!({"view":settled["view"].take()})
        }
        ClientAction::Call { method, params } => {
            session.call(&method, json_arg(params.as_deref())?).await?
        }
        ClientAction::CreateNetwork { .. }
        | ClientAction::Enroll { .. }
        | ClientAction::Recover { .. }
        | ClientAction::Business(_)
        | ClientAction::Lifecycle(_)
        | ClientAction::Access(_) => unreachable!("handled before connecting"),
    };
    crate::cli_client::session::print_value(&result, compact)
}

/// Both approval steps: confirm, compare the short codes, then sign the grant.
async fn approve(
    session: &mut client::Session<'_>,
    join_id: &str,
    manage: bool,
    expected: Option<String>,
) -> Result<Value> {
    session.confirm_join(join_id).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10 * 60);
    let code = loop {
        if let Some(code) = session.join_code(join_id).await? {
            break code;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "the joining device did not answer; run this again to keep waiting"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    match expected {
        Some(expected) => ensure!(
            expected.trim() == code,
            "the codes differ: someone may sit between the devices; reject this join"
        ),
        None => {
            ensure!(
                std::io::stdin().is_terminal(),
                "short code {code}: pass --code with the code the joining device shows"
            );
            eprintln!("Short code {code}. Does the joining device show the same? [y/N]");
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            ensure!(
                answer.trim().eq_ignore_ascii_case("y"),
                "not approved; reject the join if the codes differ"
            );
        }
    }
    session.grant_join(join_id, manage).await
}

#[derive(Args)]
pub struct BotsOptions {
    #[command(flatten)]
    pub access: ClientAccess,
    #[command(subcommand)]
    pub command: BotsAction,
}

#[derive(Subcommand)]
pub enum BotsAction {
    /// List bots and which one is primary.
    List,
    /// Create a bot.
    Create {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "")]
        instructions: String,
        /// Node whose folder the bot works in; needs --cwd.
        #[arg(long, requires = "cwd")]
        project_node: Option<String>,
        #[arg(long, requires = "project_node")]
        cwd: Option<String>,
    },
    /// Delete a bot by its conversation ID.
    Delete { conversation_id: String },
    /// Make a bot the primary one.
    SetPrimary { conversation_id: String },
    /// Move a bot's project folder, or clear it.
    SetProject {
        conversation_id: String,
        #[arg(long, requires = "cwd", conflicts_with = "clear")]
        project_node: Option<String>,
        #[arg(long, requires = "project_node")]
        cwd: Option<String>,
        #[arg(long, required_unless_present = "project_node")]
        clear: bool,
    },
    /// Pause, resume or delete one of a bot's schedules.
    Schedule {
        conversation_id: String,
        schedule_id: String,
        #[arg(value_enum)]
        action: ScheduleAction,
    },
    /// Show one of a bot's memories.
    Memory {
        conversation_id: String,
        memory_id: String,
    },
    /// Write or replace one memory.
    Remember {
        conversation_id: String,
        memory_id: String,
        text: String,
    },
    /// Delete one memory.
    Forget {
        conversation_id: String,
        memory_id: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ScheduleAction {
    Pause,
    Resume,
    Delete,
}

fn project(node: Option<String>, cwd: Option<String>) -> Value {
    match (node, cwd) {
        (Some(node), Some(cwd)) => json!({"target_node":node,"cwd":cwd}),
        _ => Value::Null,
    }
}

pub async fn bots(options: BotsOptions) -> Result<()> {
    let access = options.access;
    let dir = &access.data_dir;
    match &options.command {
        BotsAction::List => return print(&operator(dir, "rpc", json!({"method":"bots"})).await?),
        BotsAction::Memory {
            conversation_id,
            memory_id,
        } => {
            return print(&operator(dir, "rpc", json!({"method":"memory_state","conversation_id":conversation_id,"memory_id":memory_id})).await?);
        }
        _ => {}
    }
    let profile = crate::cli_client::session::Profile::open(&access)?;
    let mut identifiers = json!({});
    let action = match options.command {
        BotsAction::Create {
            name,
            instructions,
            project_node,
            cwd,
        } => {
            let bot_id = uuid::Uuid::new_v4().simple().to_string();
            let conversation_id = uuid::Uuid::new_v4().simple().to_string();
            identifiers = json!({"bot_id":bot_id,"conversation_id":conversation_id});
            client::Action::CreateBot {
                bot_id,
                conversation_id,
                name,
                instructions,
                scheduler_node: profile.endpoint.node_id().to_owned(),
                project: serde_json::from_value(project(project_node, cwd))?,
            }
        }
        BotsAction::Delete { conversation_id } => client::Action::DeleteBot { conversation_id },
        BotsAction::SetPrimary { conversation_id } => {
            client::Action::SetPrimaryBot { conversation_id }
        }
        BotsAction::SetProject {
            conversation_id,
            project_node,
            cwd,
            clear: _,
        } => client::Action::SetBotProject {
            conversation_id,
            project: serde_json::from_value(project(project_node, cwd))?,
        },
        BotsAction::Schedule {
            conversation_id,
            schedule_id,
            action,
        } => client::Action::Schedule {
            conversation_id,
            schedule_id,
            action: match action {
                ScheduleAction::Pause => client::wire::ScheduleAction::Pause,
                ScheduleAction::Resume => client::wire::ScheduleAction::Resume,
                ScheduleAction::Delete => client::wire::ScheduleAction::Delete,
            },
        },
        BotsAction::Remember {
            conversation_id,
            memory_id,
            text,
        } => memory_action(&profile, conversation_id, memory_id, Some(text)).await?,
        BotsAction::Forget {
            conversation_id,
            memory_id,
        } => memory_action(&profile, conversation_id, memory_id, None).await?,
        BotsAction::List | BotsAction::Memory { .. } => unreachable!("read as the local operator"),
    };
    let operation = profile.submit(action).await?;
    identifiers["operation"] = serde_json::to_value(&operation)?;
    if let Some(result) = &operation.result {
        identifiers["result"] = result.clone();
    }
    profile.print(&identifiers)?;
    crate::cli_client::session::require_accepted(&operation)
}

async fn memory_action(
    profile: &crate::cli_client::session::Profile,
    conversation_id: String,
    memory_id: String,
    after: Option<String>,
) -> Result<client::Action> {
    let state = profile
        .read(client::ReadRequest::Memory {
            conversation_id: conversation_id.clone(),
            memory_id: memory_id.clone(),
        })
        .await?;
    let base: client::MemoryBase = serde_json::from_value(memory_base(&state["current"])?)?;
    ensure!(
        after.is_some() || base.version.is_some(),
        "there is no such memory to delete"
    );
    Ok(client::Action::Memory {
        conversation_id,
        memory_id,
        base,
        after,
    })
}

/// The version a change stands on: none for a new memory.
fn memory_base(current: &Value) -> Result<Value> {
    if current.is_null() {
        return Ok(json!({"epoch":vibeface_control::DATA_EPOCH,"version":null}));
    }
    let metadata = &current["metadata"];
    Ok(json!({"epoch":metadata["epoch"],"version":{
        "revision":metadata["revision"].as_str().context("memory revision is missing")?,
        "control_revision":metadata["control_revision"].as_u64().context("memory control revision is missing")?,
        "source_event_hash":metadata["source_event_hash"].as_str().context("memory source is missing")?,
    }}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    #[derive(Subcommand)]
    enum Command {
        Relay(RelayOptions),
        Config(ConfigOptions),
        Client(ClientOptions),
        Bots(BotsOptions),
        Call(Call),
        Node(NodeOptions),
    }

    fn parse(args: &[&str]) -> Result<Command> {
        Ok(Cli::try_parse_from(std::iter::once("vibeface").chain(args.iter().copied()))?.command)
    }

    #[test]
    fn recovery_review_needs_no_replacement_file_and_keeps_selection_explicit() -> Result<()> {
        let Command::Client(options) = parse(&[
            "client",
            "recover",
            "--review",
            "--recovery-file",
            "/tmp/original.json",
        ])?
        else {
            anyhow::bail!("expected client recovery")
        };
        let ClientAction::Recover {
            review,
            new_recovery_file,
            members,
            ..
        } = options.command
        else {
            anyhow::bail!("expected recovery options")
        };
        assert!(review);
        assert_eq!(new_recovery_file, None);
        assert_eq!(members, None);
        assert!(parse(&["client", "recover", "--recovery-file", "/tmp/original.json"]).is_err());
        assert!(
            parse(&[
                "client",
                "recover",
                "--review",
                "--recovery-file",
                "/tmp/original.json",
                "--members",
                "[]"
            ])
            .is_err()
        );
        let Command::Client(options) = parse(&[
            "client",
            "recover",
            "--recovery-file",
            "/tmp/original.json",
            "--new-recovery-file",
            "/tmp/replacement.json",
            "--members",
            "[]",
        ])?
        else {
            anyhow::bail!("expected client recovery")
        };
        let ClientAction::Recover {
            review, members, ..
        } = options.command
        else {
            anyhow::bail!("expected recovery options")
        };
        assert!(!review);
        assert_eq!(members.as_deref(), Some("[]"));
        Ok(())
    }

    #[test]
    fn client_routing_overrides_pin_the_requested_node_and_keep_the_identity() -> Result<()> {
        let root = std::env::temp_dir().join(format!("cli-routing-{}", uuid::Uuid::new_v4()));
        let mut key = ClientKey::create(&root, "synthetic routing password")?;
        let network = "a".repeat(64);
        key.remember(
            &network,
            Entry {
                origin: "http://127.0.0.1:4790".into(),
                node_id: "b".repeat(64),
            },
        )?;
        let target = "c".repeat(64);
        let Command::Client(options) = parse(&[
            "client",
            "read",
            "nodes",
            "--url",
            "http://127.0.0.1:4795",
            "--node",
            &target,
        ])?
        else {
            anyhow::bail!("expected client")
        };
        assert_eq!(
            options.access.entry(&key)?,
            Some(Entry {
                origin: "http://127.0.0.1:4795".into(),
                node_id: target
            })
        );
        assert_eq!(key.network_id(), Some(network.as_str()));
        assert_eq!(key.entry().unwrap().node_id, "b".repeat(64));
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn relay_volunteering_sends_only_the_given_limits() -> Result<()> {
        let Command::Relay(options) = parse(&[
            "relay",
            "volunteer",
            "on",
            "--rate",
            "16384",
            "--url",
            "http://relay.example:3340",
        ])?
        else {
            bail!("relay")
        };
        assert_eq!(
            relay_change(options.command),
            Some(json!({"volunteer":true,"rate":16384,"url":"http://relay.example:3340"}))
        );
        let Command::Relay(options) = parse(&["relay", "volunteer", "off"])? else {
            bail!("relay")
        };
        assert_eq!(
            relay_change(options.command),
            Some(json!({"volunteer":false}))
        );
        let Command::Relay(options) = parse(&["relay", "status"])? else {
            bail!("relay")
        };
        assert_eq!(relay_change(options.command), None);
        assert!(parse(&["relay", "volunteer", "maybe"]).is_err());
        Ok(())
    }

    #[test]
    fn config_values_are_json_or_else_text() -> Result<()> {
        assert_eq!(config_value("true"), json!(true));
        assert_eq!(
            config_value("[\"vibeface://a@b:1\"]"),
            json!(["vibeface://a@b:1"])
        );
        assert_eq!(config_value("lab-1"), json!("lab-1"));
        assert_eq!(config_value("null"), Value::Null);
        let Command::Config(options) = parse(&["config", "set", "mesh.allow_private", "true"])?
        else {
            bail!("config")
        };
        let ConfigAction::Set { key, value } = options.command;
        assert_eq!(
            (key.as_str(), value.as_str()),
            ("mesh.allow_private", "true")
        );
        Ok(())
    }

    #[test]
    fn calls_take_a_json_object_and_client_commands_parse() -> Result<()> {
        assert_eq!(json_arg(None)?, json!({}));
        assert_eq!(json_arg(Some(r#"{"op":"list"}"#))?, json!({"op":"list"}));
        assert!(json_arg(Some("[1]")).is_err());
        assert!(json_arg(Some("not json")).is_err());
        let node = std::env::temp_dir().join("node");
        let node = node.to_str().context("a UTF-8 temporary directory")?;
        assert!(matches!(
            parse(&["call", "node_settings", "--data-dir", node])?,
            Command::Call(Call { method, .. }) if method == "node_settings"
        ));
        assert!(matches!(
            parse(&["client", "approve", "abc", "--code", "123456", "--manage"])?,
            Command::Client(ClientOptions {
                command: ClientAction::Approve {
                    manage: true,
                    code: Some(_),
                    reject: false,
                    ..
                },
                ..
            })
        ));
        assert!(parse(&["client", "approve", "abc", "--reject", "--manage"]).is_err());
        assert!(
            parse(&[
                "client",
                "create-network",
                "--recovery-file",
                "relative.json"
            ])
            .is_err()
        );
        assert!(
            parse(&["bots", "set-project", "c1"]).is_err(),
            "clear or a folder"
        );
        assert!(parse(&["bots", "set-project", "c1", "--clear"]).is_ok());
        assert!(parse(&["bots", "create", "--name", "x", "--cwd", "/w"]).is_err());
        assert!(matches!(
            parse(&["node", "configure", "--no-manager"])?,
            Command::Node(NodeOptions {
                command: NodeAction::Configure {
                    no_manager: true,
                    ..
                },
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn a_memory_change_stands_on_the_current_version() -> Result<()> {
        assert_eq!(
            memory_base(&Value::Null)?,
            json!({"epoch":1,"version":null})
        );
        let current = json!({"memory_id":"m","text":"t","metadata":{"epoch":1,"revision":"aa",
            "control_revision":3,"source_event_hash":"bb","origin":"user"}});
        assert_eq!(
            memory_base(&current)?,
            json!({"epoch":1,"version":{"revision":"aa","control_revision":3,"source_event_hash":"bb"}})
        );
        Ok(())
    }

    #[test]
    fn the_client_key_lives_beside_the_node_data_by_default() {
        let dir = default_client_dir();
        if !dir.as_os_str().is_empty() {
            assert_eq!(dir.file_name().unwrap(), ".vibeface-client");
            assert_eq!(dir.parent(), default_data_dir().parent());
        }
    }
}
