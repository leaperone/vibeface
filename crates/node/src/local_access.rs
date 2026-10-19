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

pub fn cookie_name(peer_id: &str) -> String {
    format!("vibeface_local_{peer_id}")
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
    peer_id: String,
    cluster_id: String,
    expires: Instant,
}

#[derive(Clone)]
pub struct LocalSession {
    pub peer_id: String,
    pub cluster_id: String,
    pub subject_id: String,
    pub public_key: String,
    generation: u64,
    expires: Instant,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launcher {
    pub origin: String,
    pub peer_id: String,
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

    pub fn publish_listener(&self, origin: String, peer_id: String) -> Result<()> {
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
            peer_id,
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

    pub fn issue(&self, capability: &str, peer_id: &str, cluster_id: &str) -> Result<String> {
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
        Self::issue_pending(&mut state, peer_id, cluster_id)
    }

    pub(crate) fn issue_from_local_setup(&self, peer_id: &str, cluster_id: &str) -> Result<String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("local access state unavailable"))?;
        ensure!(
            state.enabled && state.origin.is_some(),
            "local access is unavailable"
        );
        Self::issue_pending(&mut state, peer_id, cluster_id)
    }

    fn issue_pending(state: &mut State, peer_id: &str, cluster_id: &str) -> Result<String> {
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
                peer_id: peer_id.to_owned(),
                cluster_id: cluster_id.to_owned(),
                expires: Instant::now() + TOKEN_LIFETIME,
            },
        );
        Ok(token)
    }

    pub fn exchange(
        &self,
        token: &str,
        peer_id: &str,
        cluster_id: &str,
        subject_id: &str,
        public_key: &str,
    ) -> Result<String> {
        ensure!(
            crypto::fingerprint(public_key)? == subject_id,
            "browser identity does not match its public key"
        );
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
                && pending.peer_id == peer_id
                && pending.cluster_id == cluster_id,
            "local entry belongs to another node or cluster"
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
                peer_id: peer_id.to_owned(),
                cluster_id: cluster_id.to_owned(),
                subject_id: subject_id.to_owned(),
                public_key: public_key.to_owned(),
                generation,
                expires: Instant::now() + Duration::from_secs(SESSION_SECONDS),
            },
        );
        Ok(cookie)
    }

    pub fn session(&self, cookie: &str, peer_id: &str, cluster_id: &str) -> Result<LocalSession> {
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
                && session.peer_id == peer_id
                && session.cluster_id == cluster_id,
            "local browser session no longer matches this node and cluster"
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
                .issue("forwarded-from-loopback", "node", "cluster")
                .is_err()
        );
        let token = local.issue(&launcher.capability, "node", "cluster")?;
        let cookie = local.exchange(
            &token,
            "node",
            "cluster",
            &browser.peer_id()?,
            &browser.public_key_hex()?,
        )?;
        assert!(
            local
                .exchange(
                    &token,
                    "node",
                    "cluster",
                    &browser.peer_id()?,
                    &browser.public_key_hex()?
                )
                .is_err()
        );
        assert_eq!(
            local.session(&cookie, "node", "cluster")?.subject_id,
            browser.peer_id()?
        );
        assert!(local.session(&cookie, "other-node", "cluster").is_err());
        assert!(local.session(&cookie, "node", "other-cluster").is_err());
        let pending = local.issue(&launcher.capability, "node", "cluster")?;
        let mut changes = local.subscribe();
        local.set_enabled(false)?;
        assert!(changes.has_changed()?);
        assert!(!config::Config::load(&dir)?.local_auto_access);
        assert!(local.session(&cookie, "node", "cluster").is_err());
        assert!(
            local
                .issue(&launcher.capability, "node", "cluster")
                .is_err()
        );
        local.set_enabled(true)?;
        assert!(
            local
                .exchange(
                    &pending,
                    "node",
                    "cluster",
                    &browser.peer_id()?,
                    &browser.public_key_hex()?
                )
                .is_err()
        );
        assert!(local.session(&cookie, "node", "cluster").is_err());
        changes.borrow_and_update();
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
