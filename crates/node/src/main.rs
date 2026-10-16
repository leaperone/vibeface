use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use vibeface_node::{
    api,
    auth::AuthStore,
    config::{self, Config, ManagerSettings, PeerConfig},
    crypto::{LocalIdentity, verify_trust_bundle},
    model::TrustBundle,
    runtime::Runtime,
    store::Store,
};

#[derive(Parser)]
#[command(version, about = "Personal device network for local coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Create a private local identity and configuration.
    Init(Box<Init>),
    /// Start this node with its configured roles.
    Serve(DataDir),
    /// Join a personal network using its invite and pinned fingerprint.
    Pair(Pair),
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
    #[arg(long, default_value = "127.0.0.1:4790")]
    listen: SocketAddr,
    #[arg(long)]
    public_url: Option<String>,
    /// Existing directories that executors may access. May be repeated.
    #[arg(long = "root")]
    roots: Vec<PathBuf>,
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
}

#[derive(Args)]
struct Pair {
    #[command(flatten)]
    data: DataDir,
    #[arg(long)]
    url: String,
    #[arg(long)]
    code: String,
    /// Network fingerprint displayed by the node that issued the invite.
    #[arg(long)]
    network_id: String,
    #[arg(long)]
    username: String,
    #[arg(long, required = true)]
    password_stdin: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PairResponse {
    network_id: String,
    trust_bundle: TrustBundle,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PendingPair {
    origin: String,
    username: String,
    response: PairResponse,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().command {
        Action::Init(options) => init(*options),
        Action::Serve(options) => {
            let runtime = Arc::new(Runtime::open(&options.data_dir).await?);
            runtime.start().await?;
            api::serve(runtime).await
        }
        Action::Pair(options) => pair(options).await,
        Action::Tool { name, json } => call_tool(name, json).await,
    }
}

fn init(options: Init) -> Result<()> {
    let dir = &options.data.data_dir;
    ensure!(
        !dir.join("identity.json").exists() && !dir.join("config.json").exists(),
        "node already initialized; existing identity and configuration were preserved"
    );
    ensure!(
        !dir.symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink()),
        "data directory must not be a symbolic link"
    );
    let roots: Vec<_> = options
        .roots
        .iter()
        .map(|root| root.canonicalize().context("resolve execution root"))
        .collect::<Result<_>>()?;
    let mut configuration = Config::local(options.listen, roots, options.allow_insecure_lan);
    if let Some(url) = options.public_url {
        configuration.public_url = origin(&url)?.to_string().trim_end_matches('/').to_owned();
    }
    if let Some(manager) = options.manager {
        let workspace = options
            .workspace
            .context("--manager requires --workspace")?;
        configuration.manager = Some(ManagerSettings {
            provider: manager.provider().to_owned(),
            workspace: workspace
                .canonicalize()
                .context("resolve manager workspace")?,
            model: options.model.unwrap_or_default(),
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
    configuration.validate()?;
    let identity = LocalIdentity::generate();
    let encoded_identity = serde_json::to_vec_pretty(&identity)?;
    let encoded_configuration = serde_json::to_vec_pretty(&configuration)?;
    config::private_dir(dir)?;
    create_private(&dir.join("identity.json"), &encoded_identity)?;
    create_private(&dir.join("config.json"), &encoded_configuration)?;
    println!("Initialized node {}", identity.node_id()?);
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

fn read_password() -> Result<String> {
    let mut bytes = Vec::new();
    std::io::stdin().take(1027).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1026, "password input is too long");
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    let password = String::from_utf8(bytes).context("password must be UTF-8")?;
    ensure!(
        (12..=1024).contains(&password.len()) && !password.contains(['\n', '\r', '\0']),
        "password must contain 12 to 1024 bytes without line breaks or NUL"
    );
    Ok(password)
}

async fn pair(options: Pair) -> Result<()> {
    let dir = options.data.data_dir;
    let mut configuration = Config::load(&dir)?;
    let identity = config::identity(&dir)?;
    let mut url = origin(&options.url)?;
    ensure!(
        url.scheme() == "https" || is_loopback(&url) || configuration.allow_insecure_lan,
        "pairing over plaintext LAN requires allow_insecure_lan in config.json"
    );
    ensure!(
        options.network_id.len() == 64 && hex::decode(&options.network_id).is_ok(),
        "network fingerprint must contain 64 hexadecimal characters"
    );
    ensure!(
        (3..=64).contains(&options.username.len())
            && options
                .username
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-')),
        "username must contain 3 to 64 ASCII letters, digits, dots, dashes or underscores"
    );
    let username = options.username.to_ascii_lowercase();
    let password = read_password()?;
    let node_id = identity.node_id()?;
    let public_key = identity.public_key_hex()?;
    let proof = identity.sign_payload(
        "pair",
        &serde_json::to_vec(&[&options.code, &node_id, &public_key])?,
    )?;
    let mut auth = AuthStore::open(&dir.join("auth.sqlite3"))?;
    let pending_path = dir.join("pairing.pending.json");
    let pending: PendingPair = if pending_path.exists() {
        let pending: PendingPair = serde_json::from_slice(&fs::read(&pending_path)?)
            .context("invalid pending pairing receipt")?;
        ensure!(
            pending.origin == url.origin().ascii_serialization() && pending.username == username,
            "finish the pending pairing with its original URL and username first"
        );
        verify_pair(
            &pending.response,
            &options.network_id,
            &identity,
            &mut configuration,
        )?;
        pending
    } else {
        ensure!(
            !auth
                .accounts()?
                .iter()
                .any(|account| account.username == username
                    || account.network_id == options.network_id),
            "username or network is already installed on this node"
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let response = client
            .post(url.join("api/pair")?)
            .header("Origin", url.origin().ascii_serialization())
            .json(&json!({
                "code": options.code,
                "node_id": node_id,
                "public_key": public_key,
                "proof": proof,
            }))
            .send()
            .await
            .context("reach pairing node")?;
        ensure!(
            response.status().is_success(),
            "pairing rejected with HTTP {}",
            response.status()
        );
        let paired: PairResponse = serde_json::from_slice(&bounded_body(response).await?)
            .context("invalid pairing response")?;
        verify_pair(&paired, &options.network_id, &identity, &mut configuration)?;
        let pending = PendingPair {
            origin: url.origin().ascii_serialization(),
            username: username.clone(),
            response: paired,
        };
        config::private_write(&pending_path, &serde_json::to_vec(&pending)?)?;
        pending
    };
    let paired = pending.response;
    let mut store = Store::open(dir.join("node.sqlite3"))?;
    store.install_trust_bundle(&paired.network_id, &paired.trust_bundle)?;
    store.save_local_identity(&paired.network_id, &identity)?;
    let existing = auth
        .accounts()?
        .into_iter()
        .find(|account| account.username == username || account.network_id == paired.network_id);
    if let Some(existing) = existing {
        ensure!(
            existing.username == username && existing.network_id == paired.network_id,
            "local account conflicts with pending pairing"
        );
        let login = auth.login(&username, &password)?;
        auth.logout(&login.token)?;
    } else {
        auth.install_account(&username, &password, &paired.network_id)?;
    }
    let peer_scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(peer_scheme)
        .map_err(|_| anyhow::anyhow!("invalid peer URL scheme"))?;
    url.set_path("/peer");
    if !configuration
        .peers
        .iter()
        .any(|peer| peer.network_id == paired.network_id && peer.url == url.as_str())
    {
        configuration.peers.push(PeerConfig {
            network_id: paired.network_id.clone(),
            url: url.to_string(),
        });
    }
    configuration.save(&dir)?;
    fs::remove_file(pending_path)?;
    println!("Paired node {node_id} with network {}", paired.network_id);
    println!("Restart the node to connect to its new peer.");
    Ok(())
}

fn verify_pair(
    paired: &PairResponse,
    network_id: &str,
    identity: &LocalIdentity,
    configuration: &mut Config,
) -> Result<()> {
    ensure!(
        paired.network_id == network_id && paired.trust_bundle.network_id == network_id,
        "pairing response does not match the pinned network fingerprint"
    );
    verify_trust_bundle(&paired.trust_bundle)?;
    let certificate = paired
        .trust_bundle
        .nodes
        .iter()
        .find(|node| identity.node_id().is_ok_and(|id| node.node_id == id))
        .context("pairing response omits this device")?;
    ensure!(
        certificate.public_key == identity.public_key_hex()?
            && !certificate.revoked
            && !certificate.roles.is_empty()
            && certificate
                .roles
                .iter()
                .all(|role| configuration.roles.contains(role)),
        "pairing response does not grant this device's configured roles"
    );
    configuration.roles = certificate.roles.clone();
    Ok(())
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
    fn cli_requires_pairing_pin_and_stdin_password() {
        assert!(
            Cli::try_parse_from([
                "vibeface-node",
                "pair",
                "--url",
                "http://localhost:4790",
                "--code",
                "test",
                "--username",
                "harry"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "vibeface-node",
                "init",
                "--manager",
                "cursor_sdk",
                "--workspace",
                "."
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["vibeface-node", "tool", "dispatch", "--json", "{}"]).is_ok());
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
    fn pairing_checks_network_pin_and_narrows_invited_roles() {
        use vibeface_node::model::NodeRole;
        let owner = LocalIdentity::generate();
        let device = LocalIdentity::generate();
        let network_id = owner.network_id().unwrap();
        let response = PairResponse {
            network_id: network_id.clone(),
            trust_bundle: owner
                .sign_trust_bundle(
                    1,
                    vec![device.certificate(vec![NodeRole::Replica]).unwrap()],
                )
                .unwrap(),
        };
        let mut configuration = Config::local("127.0.0.1:4790".parse().unwrap(), vec![], false);
        assert!(
            verify_pair(
                &response,
                &device.network_id().unwrap(),
                &device,
                &mut configuration
            )
            .is_err()
        );
        verify_pair(&response, &network_id, &device, &mut configuration).unwrap();
        assert_eq!(configuration.roles, vec![NodeRole::Replica]);
        let other = LocalIdentity::generate();
        assert!(verify_pair(&response, &network_id, &other, &mut configuration).is_err());
        configuration.roles = vec![NodeRole::Executor];
        assert!(verify_pair(&response, &network_id, &device, &mut configuration).is_err());
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
