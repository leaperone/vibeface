use crate::crypto::LocalIdentity;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

/// Bumped whenever stored data stops being readable by the previous build.
pub const DATA_EPOCH: u32 = 4;
/// SHA-256 over the migration files (`migrations/*.sql`, by name) that `DATA_EPOCH` stores.
/// Any schema change fails a test until both constants change together.
pub const DATA_EPOCH_SCHEMA_SHA256: &str =
    "05e9d5a8379821893bb8b5d0b7ca472b6abbf931c9579b1eaeb62433891a08f7";
/// Bumped whenever a release package stops being installable by the previous updater.
pub const UPDATE_PROTOCOL: u32 = 1;
pub fn data_epoch_marker() -> Vec<u8> {
    format!(r#"{{"data_epoch":{DATA_EPOCH}}}"#).into_bytes()
}

pub const DEFAULT_LOCAL_LISTEN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 4791);

pub const UNNAMED_NODE: &str = "Node";

pub fn absolute_path(value: &str) -> std::result::Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("specify an absolute path".into())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    /// Loopback-only local session listener; fixed so its origin and browser storage persist.
    pub local_listen: SocketAddr,
    pub public_url: String,
    pub web_enabled: bool,
    pub allowed_origins: Vec<String>,
    pub allow_insecure_lan: bool,
    pub local_auto_access: bool,
    /// Serves only the web app, the relay and ciphertext forwarding; never joins a network.
    pub public_entry: bool,
    pub roots: Vec<PathBuf>,
    pub network: crate::net::NetworkConfig,
    pub manager: Option<ManagerSettings>,
    pub codex_bin: PathBuf,
    pub timeout_secs: u64,
    pub daily_decision_limit: u64,
    /// Where triaged local sessions go; the webhook secret is a separate private file.
    #[serde(default)]
    pub triage: crate::triage::TriageConfig,
    /// Which local providers are on and the roles each serves; their keys are a separate file.
    #[serde(default)]
    pub providers: crate::providers::Providers,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerSettings {
    pub provider: crate::manager::ManagerKind,
    pub workspace: PathBuf,
    pub model: String,
    pub cursor_sdk_bin: PathBuf,
}
impl Config {
    pub fn local(listen: SocketAddr, roots: Vec<PathBuf>, allow_insecure_lan: bool) -> Self {
        Self {
            listen,
            local_listen: DEFAULT_LOCAL_LISTEN,
            public_url: format!("http://{listen}"),
            web_enabled: true,
            allowed_origins: vec![],
            allow_insecure_lan,
            local_auto_access: true,
            public_entry: false,
            roots,
            network: crate::net::NetworkConfig::default(),
            manager: None,
            codex_bin: "codex".into(),
            timeout_secs: 600,
            daily_decision_limit: 20,
            triage: Default::default(),
            providers: Default::default(),
        }
    }
    pub fn load(dir: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(&std::fs::read(dir.join("config.json"))?)?;
        config.register_private_paths(dir);
        config.validate()?;
        Ok(config)
    }
    fn register_private_paths(&self, dir: &Path) {
        for path in std::iter::once(dir)
            .chain(self.roots.iter().map(PathBuf::as_path))
            .chain(std::iter::once(self.codex_bin.as_path()))
            .chain(
                self.providers
                    .codex_app_server
                    .socket
                    .iter()
                    .map(PathBuf::as_path),
            )
            .chain(self.triage.codex_sessions.iter().map(PathBuf::as_path))
        {
            crate::logging::add_private_path(path);
        }
        if let Some(manager) = &self.manager {
            crate::logging::add_private_path(&manager.workspace);
            crate::logging::add_private_path(&manager.cursor_sdk_bin);
        }
        for project in &self.triage.projects {
            crate::logging::add_private_path(Path::new(&project.prefix));
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.network.validate()?;
        let url = http_origin(&self.public_url)?;
        let local_origin = url.host_str().is_some_and(|h| {
            h == "localhost"
                || h.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        ensure!(
            url.scheme() == "https"
                || (url.scheme() == "http"
                    && ((self.listen.ip().is_loopback() && local_origin)
                        || self.allow_insecure_lan)),
            "HTTP on a LAN requires --allow-insecure-lan; public endpoints require HTTPS"
        );
        self.browser_origins()?;
        ensure!(
            self.local_listen.ip().is_loopback() && self.local_listen != self.listen,
            "local_listen must be a loopback address different from listen"
        );
        ensure!(
            self.timeout_secs > 0 && self.timeout_secs <= 86400,
            "invalid SDK timeout"
        );
        for root in &self.roots {
            ensure!(
                root.is_absolute() && root.is_dir(),
                "project root must be an existing absolute directory"
            );
        }
        ensure!(
            self.network.relay_mode != crate::net::RelayServiceMode::DiscoveryOnly
                || self.public_entry,
            "discovery-only runs as a public entry outside personal networks"
        );
        if self.public_entry {
            ensure!(
                self.network.relay_listen.is_some()
                    && self.network.entry.is_none()
                    && self.manager.is_none()
                    && self.roots.is_empty(),
                "a public entry runs a relay and nothing else"
            );
        }
        self.triage.validate()?;
        self.providers.validate(
            self.manager
                .as_ref()
                .is_some_and(|manager| manager.provider == crate::manager::ManagerKind::CodexSdk),
        )?;
        ensure!(
            !self.codex_bin.as_os_str().is_empty(),
            "the Codex program path must not be empty"
        );
        if let Some(m) = &self.manager {
            ensure!(m.workspace.is_dir(), "manager workspace does not exist");
        }
        Ok(())
    }

    pub fn browser_origins(&self) -> Result<Vec<String>> {
        let mut origins = Vec::new();
        let mut authorities = std::collections::HashMap::new();
        for value in std::iter::once(&self.public_url).chain(&self.allowed_origins) {
            let url = http_origin(value)?;
            let local = url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
            });
            ensure!(
                url.scheme() == "https" || local || self.allow_insecure_lan,
                "non-loopback HTTP origins require allow_insecure_lan"
            );
            let origin = url.origin().ascii_serialization();
            let authority = origin.split_once("://").unwrap().1.to_owned();
            if let Some(previous) = authorities.insert(authority, origin.clone()) {
                ensure!(
                    previous == origin,
                    "an authority cannot use both HTTP and HTTPS"
                );
            }
            if !origins.contains(&origin) {
                origins.push(origin);
            }
        }
        let mut aliases = vec!["127.0.0.1", "localhost", "0.0.0.0"];
        if self.listen.is_ipv6() {
            aliases.extend(["[::1]", "[::]"]);
        }
        for host in aliases {
            let origin = reqwest::Url::parse(&format!("http://{host}:{}", self.listen.port()))?
                .origin()
                .ascii_serialization();
            let authority = origin.split_once("://").unwrap().1;
            if !authorities.contains_key(authority) {
                origins.push(origin);
            }
        }
        Ok(origins)
    }
    pub fn save(&self, dir: &Path) -> Result<()> {
        self.register_private_paths(dir);
        self.validate()?;
        private_write(&dir.join("config.json"), &serde_json::to_vec_pretty(self)?)
    }
}

pub fn http_origin(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "URL must be an HTTP(S) origin without credentials, a path, query, or fragment"
    );
    Ok(url)
}
pub fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(temp, path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}
pub fn identity(dir: &Path) -> Result<LocalIdentity> {
    serde_json::from_slice(
        &std::fs::read(dir.join("identity.json")).context("run vibeface init first")?,
    )
    .context("invalid local identity")
}
pub fn lock_data_dir(dir: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(dir.join("node.lock"))?;
    lock.try_lock().context(
        "stop the Node using this data directory before changing its local pairing state",
    )?;
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_changes_bump_the_data_epoch() {
        use sha2::{Digest, Sha256};
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
            .collect();
        files.sort();
        assert!(!files.is_empty(), "no migrations in {}", dir.display());
        let mut hasher = Sha256::new();
        for path in files {
            let name = path.file_name().unwrap().to_str().unwrap();
            let body = std::fs::read(&path).unwrap();
            hasher.update(format!("{name}\0{}\0", body.len()));
            hasher.update(body);
        }
        let actual = hex::encode(hasher.finalize());
        assert_eq!(
            actual, DATA_EPOCH_SCHEMA_SHA256,
            "schema changed: bump DATA_EPOCH and update the recorded fingerprint DATA_EPOCH_SCHEMA_SHA256 in crates/node/src/config.rs"
        );
    }

    #[test]
    fn browser_aliases_do_not_change_network_exposure() {
        let mut config = Config::local("127.0.0.1:4790".parse().unwrap(), vec![], false);
        assert_eq!(
            config.browser_origins().unwrap(),
            vec![
                "http://127.0.0.1:4790",
                "http://localhost:4790",
                "http://0.0.0.0:4790"
            ]
        );
        assert_eq!(
            config.listen,
            "127.0.0.1:4790".parse::<SocketAddr>().unwrap()
        );
        assert!(!config.allow_insecure_lan);
        config.listen = "0.0.0.0:4790".parse().unwrap();
        assert!(config.validate().is_err());
        config.allow_insecure_lan = true;
        config.public_url = "http://192.168.0.99:4790".into();
        assert!(config.validate().is_ok());
        assert_eq!(config.browser_origins().unwrap().len(), 4);
    }

    #[test]
    fn extra_browser_origins_are_explicit_and_unambiguous() {
        let mut config = Config::local("127.0.0.1:4790".parse().unwrap(), vec![], false);
        config.public_url = "https://node.example".into();
        config.allowed_origins = vec![
            "https://other.example/".into(),
            "https://other.example".into(),
        ];
        assert!(config.validate().is_ok());
        assert_eq!(config.browser_origins().unwrap().len(), 5);
        for invalid in [
            "https://user:secret@example.com",
            "https://other.example/path",
            "https://other.example?query",
            "https://other.example#fragment",
            "file:///tmp/node",
            "http://192.168.0.99:4790",
        ] {
            config.allowed_origins = vec![invalid.into()];
            assert!(config.validate().is_err(), "{invalid}");
        }
        config.allowed_origins = vec!["http://node.example".into()];
        config.allow_insecure_lan = true;
        assert!(config.validate().is_err());
        config.allowed_origins = vec!["http://192.168.0.99:4790".into()];
        assert!(config.validate().is_ok());
        config.public_url = "https://localhost:4790".into();
        config.allowed_origins.clear();
        assert!(
            !config
                .browser_origins()
                .unwrap()
                .contains(&"http://localhost:4790".into())
        );
    }
}
