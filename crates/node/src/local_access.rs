use crate::{config, crypto};
use anyhow::{Context, Result, ensure};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio::sync::watch;

const TOKEN_LIFETIME: Duration = Duration::from_secs(60);
pub const SESSION_SECONDS: u64 = 86400;

pub fn cookie_name(node_id: &str) -> String {
    format!("vibeface_local_{node_id}")
}

pub struct LocalAccess {
    dir: PathBuf,
    state: Mutex<State>,
    changes: watch::Sender<u64>,
}

struct State {
    enabled: bool,
    generation: u64,
    launcher_hash: String,
    origin: Option<String>,
    tokens: HashMap<String, Pending>,
    sessions: HashMap<String, LocalSession>,
}

struct Pending {
    node_id: String,
    network_id: String,
    expires: Instant,
}

#[derive(Clone)]
pub struct LocalSession {
    pub node_id: String,
    pub network_id: String,
    pub device_id: String,
    generation: u64,
    expires: Instant,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launcher {
    pub origin: String,
    pub node_id: String,
    pub capability: String,
}

impl LocalAccess {
    pub fn open(dir: &Path, enabled: bool) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(dir)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "local access requires a private data directory"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                metadata.permissions().mode() & 0o077 == 0,
                "local access directory must be private"
            );
        }
        let (changes, _) = watch::channel(0);
        Ok(Self {
            dir: dir.to_owned(),
            state: Mutex::new(State {
                enabled,
                generation: 0,
                launcher_hash: String::new(),
                origin: None,
                tokens: HashMap::new(),
                sessions: HashMap::new(),
            }),
            changes,
        })
    }

    pub fn publish_listener(&self, origin: String, node_id: String) -> Result<()> {
        let url = reqwest::Url::parse(&origin)?;
        ensure!(
            url.scheme() == "http"
                && url.host_str() == Some("127.0.0.1")
                && url.port().is_some()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none()
                && url.username().is_empty()
                && url.password().is_none(),
            "local access listener must use a fixed loopback origin"
        );
        let launcher = Launcher {
            origin: origin.clone(),
            node_id,
            capability: random_secret(),
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("local access state unavailable"))?;
        config::private_write(
            &self.dir.join("local-access.json"),
            &serde_json::to_vec(&launcher)?,
        )?;
        state.origin = Some(origin);
        state.launcher_hash = crypto::digest(launcher.capability.as_bytes());
        Ok(())
    }

    pub fn origin(&self) -> Result<String> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("local access state unavailable"))?
            .origin
            .clone()
            .context("local access listener is not running")
    }

    pub fn enabled(&self) -> bool {
        self.state.lock().is_ok_and(|state| state.enabled)
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub fn issue(&self, capability: &str, node_id: &str, network_id: &str) -> Result<String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("local access state unavailable"))?;
        ensure!(
            state.enabled
                && capability.len() == 64
                && crypto::digest(capability.as_bytes()) == state.launcher_hash,
            "local access launcher credential is invalid or disabled"
        );
        Self::issue_pending(&mut state, node_id, network_id)
    }

    pub(crate) fn issue_from_local_setup(&self, node_id: &str, network_id: &str) -> Result<String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("local access state unavailable"))?;
        ensure!(
            state.enabled && state.origin.is_some(),
            "local access is unavailable"
        );
        Self::issue_pending(&mut state, node_id, network_id)
    }

    fn issue_pending(state: &mut State, node_id: &str, network_id: &str) -> Result<String> {
        state
            .tokens
            .retain(|_, pending| pending.expires > Instant::now());
        ensure!(
            state.tokens.len() < 64,
            "too many pending local access entries"
        );
        let token = random_secret();
        state.tokens.insert(
            crypto::digest(token.as_bytes()),
            Pending {
                node_id: node_id.to_owned(),
                network_id: network_id.to_owned(),
                expires: Instant::now() + TOKEN_LIFETIME,
            },
        );
        Ok(token)
    }

    pub fn exchange(
        &self,
        token: &str,
        node_id: &str,
        network_id: &str,
        device_id: &str,
    ) -> Result<String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("local access state unavailable"))?;
        ensure!(
            state.enabled && token.len() == 64,
            "local access is disabled or the entry is invalid"
        );
        let pending = state
            .tokens
            .remove(&crypto::digest(token.as_bytes()))
            .context("local entry was already used or expired")?;
        ensure!(
            pending.expires > Instant::now()
                && pending.node_id == node_id
                && pending.network_id == network_id,
            "local entry belongs to another node or personal network"
        );
        state
            .sessions
            .retain(|_, session| session.expires > Instant::now());
        ensure!(
            state.sessions.len() < 256,
            "too many local browser sessions"
        );
        let cookie = random_secret();
        let generation = state.generation;
        state.sessions.insert(
            crypto::digest(cookie.as_bytes()),
            LocalSession {
                node_id: node_id.to_owned(),
                network_id: network_id.to_owned(),
                device_id: device_id.to_owned(),
                generation,
                expires: Instant::now() + Duration::from_secs(SESSION_SECONDS),
            },
        );
        Ok(cookie)
    }

    pub fn session(&self, cookie: &str, node_id: &str, network_id: &str) -> Result<LocalSession> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("local access state unavailable"))?;
        ensure!(
            state.enabled && cookie.len() == 64,
            "local browser session is disabled or missing"
        );
        let session = state
            .sessions
            .get(&crypto::digest(cookie.as_bytes()))
            .context("local browser session is no longer valid")?;
        ensure!(
            session.expires > Instant::now()
                && session.generation == state.generation
                && session.node_id == node_id
                && session.network_id == network_id,
            "local browser session no longer matches this node and personal network"
        );
        Ok(session.clone())
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("local access state unavailable"))?;
        let mut configuration = config::Config::load(&self.dir)?;
        configuration.local_auto_access = enabled;
        configuration.save(&self.dir)?;
        if state.enabled == enabled {
            return Ok(());
        }
        state.enabled = enabled;
        state.generation = state.generation.wrapping_add(1);
        state.tokens.clear();
        state.sessions.clear();
        self.changes.send_replace(state.generation);
        Ok(())
    }
}

fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_entries_are_single_use_and_disabling_revokes_bound_sessions() -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("vibeface-local-access-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        config::Config::local("127.0.0.1:4790".parse()?, vec![], false).save(&dir)?;
        let local = LocalAccess::open(&dir, true)?;
        local.publish_listener("http://127.0.0.1:4791".into(), "node".into())?;
        let launcher: Launcher =
            serde_json::from_slice(&std::fs::read(dir.join("local-access.json"))?)?;
        let browser = crypto::LocalIdentity::generate();
        assert!(
            local
                .issue("forwarded-from-loopback", "node", "network")
                .is_err()
        );
        let token = local.issue(&launcher.capability, "node", "network")?;
        let cookie = local.exchange(&token, "node", "network", &browser.device_id()?)?;
        assert!(
            local
                .exchange(&token, "node", "network", &browser.device_id()?)
                .is_err()
        );
        assert_eq!(
            local.session(&cookie, "node", "network")?.device_id,
            browser.device_id()?
        );
        assert!(local.session(&cookie, "other-node", "network").is_err());
        assert!(local.session(&cookie, "node", "other-network").is_err());
        let pending = local.issue(&launcher.capability, "node", "network")?;
        let mut changes = local.subscribe();
        local.set_enabled(false)?;
        assert!(changes.has_changed()?);
        assert!(!config::Config::load(&dir)?.local_auto_access);
        assert!(local.session(&cookie, "node", "network").is_err());
        assert!(
            local
                .issue(&launcher.capability, "node", "network")
                .is_err()
        );
        local.set_enabled(true)?;
        assert!(
            local
                .exchange(&pending, "node", "network", &browser.device_id()?)
                .is_err()
        );
        assert!(local.session(&cookie, "node", "network").is_err());
        changes.borrow_and_update();
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
