use crate::crypto::LocalIdentity;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

/// Bumped whenever stored data stops being readable by the previous build.
pub const DATA_EPOCH: u32 = 9;

pub fn data_epoch_marker() -> Vec<u8> {
    format!(r#"{{"data_epoch":{DATA_EPOCH}}}"#).into_bytes()
}

pub const DEFAULT_LOCAL_LISTEN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 4791);

/// This machine's name, the default device name shown when creating or joining a network.
pub fn hostname() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is writable for its full length and gethostname NUL-terminates within it.
    let ok = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0;
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..end]);
    let name = name.trim().trim_end_matches(".local");
    if ok && !name.is_empty() {
        name.chars().take(128).collect()
    } else {
        "Node".into()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    /// Loopback-only local session listener; fixed so its origin and browser storage persist.
    pub local_listen: SocketAddr,
    pub public_url: String,
    pub allowed_origins: Vec<String>,
    pub allow_insecure_lan: bool,
    pub local_auto_access: bool,
    /// Serves only the web app, the relay and ciphertext forwarding; never joins a network.
    pub public_entry: bool,
    pub roots: Vec<PathBuf>,
    pub network: crate::net::NetworkConfig,
    pub manager: Option<ManagerSettings>,
    pub codex_socket: Option<PathBuf>,
    pub codex_bin: PathBuf,
    pub timeout_secs: u64,
    pub daily_decision_limit: u64,
    /// Where triaged local sessions go; the webhook secret is a separate private file.
    #[serde(default)]
    pub triage: crate::triage::TriageConfig,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerSettings {
    pub provider: crate::manager::ManagerKind,
    pub workspace: PathBuf,
    pub model: String,
    pub cursor_sdk_bin: PathBuf,
    pub node_bin: PathBuf,
    pub codex_sdk_script: PathBuf,
}
impl Config {
    pub fn local(listen: SocketAddr, roots: Vec<PathBuf>, allow_insecure_lan: bool) -> Self {
        Self {
            listen,
            local_listen: DEFAULT_LOCAL_LISTEN,
            public_url: format!("http://{listen}"),
            allowed_origins: vec![],
            allow_insecure_lan,
            local_auto_access: true,
            public_entry: false,
            roots,
            network: crate::net::NetworkConfig::default(),
            manager: None,
            codex_socket: None,
            codex_bin: "codex".into(),
            timeout_secs: 600,
            daily_decision_limit: 20,
            triage: Default::default(),
        }
    }
    pub fn load(dir: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(&std::fs::read(dir.join("config.json"))?)?;
        config.validate()?;
        Ok(config)
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
pub fn default_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".vibeface")
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
