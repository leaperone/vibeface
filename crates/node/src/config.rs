use crate::{
    crypto::LocalIdentity,
    model::{NodeRole, validate_role_dependencies},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub public_url: String,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    pub allow_insecure_lan: bool,
    pub roles: Vec<NodeRole>,
    pub roots: Vec<PathBuf>,
    #[serde(default)]
    pub network: crate::network::NetworkConfig,
    pub manager: Option<ManagerSettings>,
    pub codex_socket: Option<PathBuf>,
    pub codex_bin: PathBuf,
    pub claude_bin: PathBuf,
    pub cursor_bin: PathBuf,
    pub timeout_secs: u64,
    pub daily_decision_limit: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerSettings {
    pub provider: String,
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
            public_url: format!("http://{listen}"),
            allowed_origins: vec![],
            allow_insecure_lan,
            roles: vec![
                NodeRole::Gateway,
                NodeRole::Relay,
                NodeRole::Replica,
                NodeRole::Scheduler,
                NodeRole::Executor,
            ],
            roots,
            network: crate::network::NetworkConfig::default(),
            manager: None,
            codex_socket: None,
            codex_bin: "codex".into(),
            claude_bin: "claude".into(),
            cursor_bin: "cursor-agent".into(),
            timeout_secs: 600,
            daily_decision_limit: 20,
        }
    }
    pub fn load(dir: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(&std::fs::read(dir.join("config.json"))?)?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        validate_role_dependencies(&self.roles)?;
        let url = browser_origin(&self.public_url)?;
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
            self.timeout_secs > 0 && self.timeout_secs <= 86400,
            "invalid executor timeout"
        );
        for root in &self.roots {
            ensure!(
                root.is_absolute() && root.is_dir(),
                "execution root must be an existing absolute directory"
            );
        }
        if let Some(m) = &self.manager {
            ensure!(
                matches!(m.provider.as_str(), "cursor_sdk" | "codex_sdk"),
                "unknown manager provider"
            );
            ensure!(m.workspace.is_dir(), "manager workspace does not exist");
        }
        Ok(())
    }

    pub fn browser_origins(&self) -> Result<Vec<String>> {
        let mut origins = Vec::new();
        let mut authorities = std::collections::HashMap::new();
        for value in std::iter::once(&self.public_url).chain(&self.allowed_origins) {
            let url = browser_origin(value)?;
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

fn browser_origin(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "browser origin must use HTTP or HTTPS without credentials, a path, query or fragment"
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
        &std::fs::read(dir.join("identity.json")).context("run vibeface-node init first")?,
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

    #[test]
    fn business_roles_require_replica() {
        let mut config = Config::local("127.0.0.1:0".parse().unwrap(), vec![], false);
        assert!(config.validate().is_ok());
        for roles in [
            vec![NodeRole::Relay],
            vec![NodeRole::Replica],
            vec![NodeRole::Relay, NodeRole::Replica],
            vec![NodeRole::Gateway, NodeRole::Relay, NodeRole::Replica],
            vec![NodeRole::Scheduler, NodeRole::Replica],
            vec![NodeRole::Executor, NodeRole::Replica],
        ] {
            config.roles = roles;
            assert!(config.validate().is_ok());
        }
        for role in [NodeRole::Gateway, NodeRole::Scheduler, NodeRole::Executor] {
            config.roles = vec![role, NodeRole::Relay];
            assert_eq!(
                config.validate().unwrap_err().to_string(),
                "gateway, scheduler and executor roles require replica"
            );
        }
    }
}
