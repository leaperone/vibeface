use crate::crypto::LocalIdentity;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, SocketAddr},
    num::{NonZeroU32, NonZeroUsize},
    path::{Path, PathBuf},
};
use vibeface_bridge::platform::privatefs;
use zeroize::Zeroizing;

/// Fixed at 1 before the first stable release; schema changes update the fingerprint below.
pub use vibeface_control::DATA_EPOCH;
/// SHA-256 of the Core and delivery initial SQL schemas.
pub const DATA_EPOCH_SCHEMA_SHA256: &str =
    "498584b54cd7ae95f073f4d0278edfe173db5bfa5a8adfd24dbe55a7273dcbc8";
/// Bumped whenever a release package stops being installable by the previous updater.
pub use vibeface_control::UPDATE_PROTOCOL;
pub fn data_epoch_marker() -> Vec<u8> {
    format!(r#"{{"data_epoch":{DATA_EPOCH}}}"#).into_bytes()
}

pub const DEFAULT_LOCAL_LISTEN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 4791);

pub const UNNAMED_NODE: &str = "Node";

/// The certificate chain and key `listen` serves under `tls`: at first the self-signed pair
/// `init --tls` makes for `public_url`'s host, then whatever the host installs over them.
pub const TLS_CERT: &str = "tls-cert.pem";
pub const TLS_KEY: &str = "tls-key.pem";

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulingLimits {
    pub machine_slots: usize,
    pub task_limit: usize,
}

impl Default for SchedulingLimits {
    fn default() -> Self {
        Self {
            machine_slots: 4,
            task_limit: 2,
        }
    }
}

impl SchedulingLimits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=32).contains(&self.machine_slots),
            "machine_slots must be between 1 and 32"
        );
        ensure!(
            (1..=self.machine_slots).contains(&self.task_limit),
            "task_limit must be between 1 and machine_slots"
        );
        Ok(())
    }
}

/// How this Node forwards browser sessions to other devices.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Forwarding {
    /// Forward to any device, not only this network's Nodes, as a public node does.
    pub any: bool,
    /// Sessions forwarded at once.
    pub sessions: usize,
    /// Sessions waiting for a slot; one more is refused as busy.
    pub queue: usize,
    /// Bytes a second each session carries, both ways together; unlimited when unset.
    pub rate: Option<NonZeroU32>,
    /// `/api/client` connections one client address holds on `listen`, whatever their target;
    /// unlimited when unset.
    pub per_ip: Option<NonZeroUsize>,
    /// Reverse proxies whose single `X-Real-IP` names the client; any other peer is the client.
    pub trusted_proxies: Vec<IpAddr>,
}

impl Default for Forwarding {
    fn default() -> Self {
        Self {
            any: false,
            sessions: 256,
            queue: 0,
            rate: None,
            per_ip: None,
            trusted_proxies: vec![],
        }
    }
}

impl Forwarding {
    pub fn validate(&self) -> Result<()> {
        // Waiting sessions hold their browser sockets too.
        ensure!(
            self.sessions >= 1
                && self.sessions.saturating_add(self.queue) <= crate::api::MAX_CLIENT_SOCKETS,
            "forwarded and waiting sessions must fit the {} browser sockets",
            crate::api::MAX_CLIENT_SOCKETS
        );
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    /// Loopback-only local session listener; fixed so its origin and browser storage persist.
    pub local_listen: SocketAddr,
    pub public_url: String,
    /// `listen` speaks only TLS, with `TLS_CERT` and `TLS_KEY`, read again once they change.
    #[serde(default)]
    pub tls: bool,
    pub web_enabled: bool,
    pub allowed_origins: Vec<String>,
    pub allow_insecure_lan: bool,
    pub local_auto_access: bool,
    #[serde(default)]
    pub forwarding: Forwarding,
    pub roots: Vec<PathBuf>,
    pub network: crate::net::NetworkConfig,
    pub manager: Option<ManagerSettings>,
    /// The programs chosen for providers; the others run their `default_bin`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub bins: std::collections::BTreeMap<crate::providers::Provider, PathBuf>,
    pub timeout_secs: u64,
    pub daily_decision_limit: u64,
    #[serde(default)]
    pub scheduling: SchedulingLimits,
    /// Where triaged local sessions go; the webhook secret is a separate private file.
    #[serde(default)]
    pub triage: crate::triage::TriageConfig,
    /// Which local providers are on and the roles each serves.
    #[serde(default)]
    pub providers: crate::providers::Providers,
    /// Provider variables belong to this private snapshot and never to shared records.
    pub(crate) provider_env: crate::providers::ProviderEnv,
    /// An initial private Codex configuration, published from this snapshot when idle.
    pub(crate) codex_config: Option<String>,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerSettings {
    pub provider: crate::providers::Provider,
    pub workspace: PathBuf,
    pub model: String,
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub backup: crate::scheduler::BackupPolicy,
}
impl Config {
    pub fn local(listen: SocketAddr, roots: Vec<PathBuf>, allow_insecure_lan: bool) -> Self {
        Self {
            listen,
            local_listen: DEFAULT_LOCAL_LISTEN,
            public_url: format!("http://{listen}"),
            tls: false,
            web_enabled: true,
            allowed_origins: vec![],
            allow_insecure_lan,
            local_auto_access: true,
            forwarding: Default::default(),
            roots,
            network: crate::net::NetworkConfig::default(),
            manager: None,
            bins: Default::default(),
            timeout_secs: 600,
            daily_decision_limit: 20,
            scheduling: Default::default(),
            triage: Default::default(),
            providers: Default::default(),
            provider_env: Default::default(),
            codex_config: None,
        }
    }
    pub fn load(dir: &Path) -> Result<Self> {
        let bytes = Zeroizing::new(std::fs::read(dir.join("config.json"))?);
        let mut config: Self = serde_json::from_slice(&bytes)?;
        config.provider_env.remove_cursor_auth_overrides();
        config.register_private_paths(dir);
        config.validate()?;
        Ok(config)
    }
    fn register_private_paths(&self, dir: &Path) {
        for path in std::iter::once(dir)
            .chain(self.roots.iter().map(PathBuf::as_path))
            .chain(self.bins.values().map(PathBuf::as_path))
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
        }
        for project in &self.triage.projects {
            crate::logging::add_private_path(Path::new(&project.prefix));
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.scheduling.validate()?;
        self.network.validate()?;
        self.forwarding.validate()?;
        let url = http_origin(&self.public_url)?;
        ensure!(
            !self.tls || url.scheme() == "https",
            "tls needs an HTTPS public_url"
        );
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
            ensure!(root.is_absolute(), "project root must be an absolute path");
        }
        self.triage.validate()?;
        self.providers.validate(
            self.manager
                .as_ref()
                .is_some_and(|manager| manager.provider.on_app_server()),
        )?;
        for (provider, bin) in &self.bins {
            crate::providers::validate_bin(*provider, bin)?;
        }
        if let Some(m) = &self.manager {
            ensure!(
                m.provider
                    .supports()
                    .contains(&crate::providers::Role::Agent),
                "{} does not run bots",
                m.provider.name()
            );
            ensure!(
                m.workspace.is_absolute(),
                "manager workspace must be absolute"
            );
            m.provider
                .validate_reasoning_effort(m.reasoning_effort.as_deref())?;
            m.backup.validate()?;
        }
        self.provider_env.validate()?;
        if let Some(source) = &self.codex_config {
            toml::from_str::<toml::Value>(source)
                .map_err(|_| anyhow::anyhow!("invalid private Codex configuration"))?;
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
        self.save_and_publish(dir, || {})
    }

    pub(crate) fn save_and_publish(&self, dir: &Path, publish: impl FnOnce()) -> Result<()> {
        self.register_private_paths(dir);
        self.validate()?;
        let bytes = Zeroizing::new(serde_json::to_vec_pretty(self)?);
        let path = dir.join("config.json");
        let saved = private_write(&path, &bytes);
        // A rename may succeed before its directory flush fails; memory must follow the file.
        if saved.is_ok()
            || std::fs::read(&path)
                .map(Zeroizing::new)
                .is_ok_and(|stored| stored.as_slice() == bytes.as_slice())
        {
            publish();
        }
        saved
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
    privatefs::restrict_dir(path)
}
pub fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    privatefs::write_atomic(path, bytes)
}
pub fn identity(dir: &Path) -> Result<LocalIdentity> {
    let bytes = Zeroizing::new(
        std::fs::read(dir.join("identity.json")).context("run vibeface init first")?,
    );
    serde_json::from_slice(&bytes).context("invalid local identity")
}
pub fn lock_data_dir(dir: &Path) -> Result<std::fs::File> {
    let lock = privatefs::open_or_create(&dir.join("node.lock"))?;
    lock.try_lock().context(
        "stop the Node using this data directory before changing its local pairing state",
    )?;
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduling_limits_bound_all_coordinator_reservations() {
        assert!(SchedulingLimits::default().validate().is_ok());
        for limits in [
            SchedulingLimits {
                machine_slots: 0,
                task_limit: 1,
            },
            SchedulingLimits {
                machine_slots: 33,
                task_limit: 1,
            },
            SchedulingLimits {
                machine_slots: 2,
                task_limit: 0,
            },
            SchedulingLimits {
                machine_slots: 2,
                task_limit: 3,
            },
        ] {
            assert!(limits.validate().is_err());
        }
    }

    #[test]
    fn private_configuration_keeps_unavailable_directories_and_one_provider_snapshot() -> Result<()>
    {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("private-configuration-{}", uuid::Uuid::new_v4()));
        private_dir(&dir)?;
        let root = dir.join("unmounted-root");
        let workspace = dir.join("unmounted-workspace");
        let mut configuration = Config::local("127.0.0.1:0".parse()?, vec![root.clone()], false);
        configuration.providers.execution.clear();
        configuration.manager = Some(ManagerSettings {
            provider: crate::providers::Provider::Pi,
            workspace: workspace.clone(),
            model: "fixture-model".into(),
            reasoning_effort: None,
            backup: Default::default(),
        });
        configuration.provider_env.apply(
            crate::providers::Provider::Pi,
            [(
                "FIXTURE_API_KEY".into(),
                Some("synthetic-private-key".into()),
            )]
            .into(),
        )?;
        let published = std::cell::Cell::new(false);
        std::fs::create_dir(dir.join("config.json"))?;
        std::fs::write(dir.join("config.json/sentinel"), "preserved")?;
        assert!(
            configuration
                .save_and_publish(&dir, || published.set(true))
                .is_err()
        );
        assert!(!published.get());
        assert_eq!(
            std::fs::read_to_string(dir.join("config.json/sentinel"))?,
            "preserved"
        );
        std::fs::remove_dir_all(dir.join("config.json"))?;
        configuration.save_and_publish(&dir, || published.set(true))?;
        assert!(published.get());
        let saved = Config::load(&dir)?;
        assert_eq!(saved.roots, std::slice::from_ref(&root));
        assert_eq!(
            saved.manager.as_ref().context("manager")?.workspace,
            workspace
        );
        assert!(saved.provider_env == configuration.provider_env);
        assert!(!root.exists() && !workspace.exists());
        assert!(!dir.join("provider-env.json").exists());
        privatefs::ensure_private(&dir.join("config.json"), "private configuration")?;
        let mut malformed = serde_json::to_value(&saved)?;
        malformed.as_object_mut().unwrap().remove("provider_env");
        assert!(serde_json::from_value::<Config>(malformed).is_err());
        configuration.roots = vec!["relative-root".into()];
        published.set(false);
        assert!(
            configuration
                .save_and_publish(&dir, || published.set(true))
                .is_err()
        );
        assert!(!published.get());
        assert!(Config::load(&dir)?.provider_env == saved.provider_env);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn schema_fingerprint_matches_the_final_data_format() {
        use sha2::{Digest, Sha256};
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = root.join("migrations");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
            .collect();
        assert!(!files.is_empty(), "no migrations in {}", dir.display());
        files.push(root.join("src/delivery/schema.sql"));
        files.sort();
        let mut hasher = Sha256::new();
        for path in files {
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let body = std::fs::read(&path).unwrap();
            hasher.update(format!("{name}\0{}\0", body.len()));
            hasher.update(body);
        }
        let actual = hex::encode(hasher.finalize());
        assert_eq!(
            actual, DATA_EPOCH_SCHEMA_SHA256,
            "final schema changed: keep DATA_EPOCH at 1 and update DATA_EPOCH_SCHEMA_SHA256 in apps/core/src/config.rs"
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
