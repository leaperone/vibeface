use crate::{
    config::{self, Config},
    crypto::digest,
};
use anyhow::{Context, Result, ensure};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, watch};

const SESSION_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
const LAUNCH_LIFETIME: Duration = Duration::from_secs(60);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Persistent {
    generation: u64,
    unlock_key: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchDescriptor {
    node_id: String,
    address: String,
    credential: String,
}

struct State {
    persistent: Persistent,
    enabled: bool,
    launch_tokens: HashMap<String, Instant>,
    sessions: HashMap<String, Instant>,
}

pub struct LocalAccess {
    directory: PathBuf,
    credential: String,
    cookie_name: String,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
    pub(crate) sockets: Arc<Semaphore>,
}

pub struct Session {
    pub generation: u64,
    expires: Instant,
    changed: watch::Receiver<u64>,
}

impl Session {
    pub async fn revoked(mut self) {
        loop {
            if *self.changed.borrow_and_update() != self.generation {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep_until(self.expires.into()) => return,
                result = self.changed.changed() => if result.is_err() { return; },
            }
        }
    }
}

impl LocalAccess {
    pub fn load(directory: &Path, configuration: &Config, node_id: &str) -> Result<Self> {
        let path = directory.join("local-access.json");
        let persistent = if path.try_exists()? {
            let state: Persistent = serde_json::from_slice(&read_private(&path)?)?;
            ensure!(
                state.unlock_key.len() == 64 && hex::decode(&state.unlock_key).is_ok(),
                "invalid local vault key"
            );
            state
        } else {
            let state = Persistent {
                generation: 1,
                unlock_key: random_token(),
            };
            config::private_write(&path, &serde_json::to_vec(&state)?)?;
            state
        };
        let (changed, _) = watch::channel(persistent.generation);
        Ok(Self {
            directory: directory.to_owned(),
            credential: random_token(),
            cookie_name: format!("vf_local_{}", &node_id[..16]),
            state: Mutex::new(State {
                persistent,
                enabled: configuration.local_access.enabled,
                launch_tokens: HashMap::new(),
                sessions: HashMap::new(),
            }),
            changed,
            sockets: Arc::new(Semaphore::new(32)),
        })
    }

    pub fn publish(&self, address: SocketAddr, node_id: String) -> Result<()> {
        ensure!(
            address.ip().is_loopback(),
            "local listener must be loopback"
        );
        let mut configuration = Config::load(&self.directory)?;
        if configuration.local_access.listen.port() == 0 {
            configuration.local_access.listen = address;
            configuration.save(&self.directory)?;
        }
        let descriptor = LaunchDescriptor {
            node_id,
            address: format!("http://{address}"),
            credential: self.credential.clone(),
        };
        config::private_write(
            &self.directory.join("local-launch.json"),
            &serde_json::to_vec(&descriptor)?,
        )
    }

    pub fn enabled(&self) -> bool {
        self.state.lock().unwrap().enabled
    }

    pub fn bootstrap_capability(&self) -> Result<String> {
        let capability: crate::identity::local::BootstrapCapability =
            serde_json::from_slice(&read_private(&self.directory.join("bootstrap.json"))?)?;
        ensure!(
            capability.capability.len() == 64 && hex::decode(&capability.capability).is_ok(),
            "invalid bootstrap capability"
        );
        Ok(capability.capability)
    }

    pub fn launch(&self, credential: &str) -> Result<String> {
        ensure!(
            digest(credential.as_bytes()) == digest(self.credential.as_bytes()),
            "invalid local launch credential"
        );
        let mut state = self.state.lock().unwrap();
        state
            .launch_tokens
            .retain(|_, expires| *expires > Instant::now());
        ensure!(
            state.launch_tokens.len() < 16,
            "too many pending local launches"
        );
        let token = random_token();
        state
            .launch_tokens
            .insert(digest(token.as_bytes()), Instant::now() + LAUNCH_LIFETIME);
        Ok(token)
    }

    pub fn exchange(&self, token: &str) -> Result<String> {
        let mut state = self.state.lock().unwrap();
        let expires = state
            .launch_tokens
            .remove(&digest(token.as_bytes()))
            .context("local launch expired or already used")?;
        ensure!(expires > Instant::now(), "local launch expired");
        state
            .sessions
            .retain(|_, expires| *expires > Instant::now());
        ensure!(state.sessions.len() < 32, "too many local sessions");
        let cookie = random_token();
        state
            .sessions
            .insert(digest(cookie.as_bytes()), Instant::now() + SESSION_LIFETIME);
        Ok(format!(
            "{}={cookie}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
            self.cookie_name,
            SESSION_LIFETIME.as_secs()
        ))
    }

    pub fn session(&self, cookie: &str) -> Result<Session> {
        let mut matching = cookie.split(';').filter_map(|part| {
            part.trim()
                .split_once('=')
                .filter(|(name, _)| *name == self.cookie_name)
                .map(|(_, value)| value)
        });
        let token = matching.next().context("local session required")?;
        ensure!(matching.next().is_none(), "ambiguous local session");
        let state = self.state.lock().unwrap();
        let expires = *state
            .sessions
            .get(&digest(token.as_bytes()))
            .context("local session expired")?;
        ensure!(expires > Instant::now(), "local session expired");
        Ok(Session {
            generation: state.persistent.generation,
            expires,
            changed: self.changed.subscribe(),
        })
    }

    pub fn unlock_key(&self, session: &Session) -> Result<Option<String>> {
        let state = self.state.lock().unwrap();
        ensure!(
            session.generation == state.persistent.generation && session.expires > Instant::now(),
            "local session expired"
        );
        Ok(state.enabled.then(|| state.persistent.unlock_key.clone()))
    }

    pub fn set_enabled(&self, session: &Session, enabled: bool) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        ensure!(
            session.generation == state.persistent.generation && session.expires > Instant::now(),
            "local session expired"
        );
        if state.enabled == enabled {
            return Ok(());
        }
        let mut configuration = Config::load(&self.directory)?;
        configuration.local_access.enabled = enabled;
        let next_generation = if enabled {
            state.persistent.generation
        } else {
            state
                .persistent
                .generation
                .checked_add(1)
                .context("local generation exhausted")?
        };
        if !enabled {
            config::private_write(
                &self.directory.join("local-access.json"),
                &serde_json::to_vec(&Persistent {
                    generation: next_generation,
                    unlock_key: state.persistent.unlock_key.clone(),
                })?,
            )?;
        }
        configuration.save(&self.directory)?;
        state.persistent.generation = next_generation;
        state.enabled = enabled;
        if !enabled {
            state.sessions.clear();
            state.launch_tokens.clear();
            self.changed.send_replace(state.persistent.generation);
        }
        Ok(())
    }
}

fn random_token() -> String {
    let mut bytes = [0; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn read_private(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == config::effective_uid()
            && metadata.permissions().mode() & 0o077 == 0
            && metadata.len() <= 4096,
        "local access file must be private and owned by the current user"
    );
    Ok(std::fs::read(path)?)
}

pub async fn browser_url(directory: &Path) -> Result<String> {
    config::verify_private_dir(directory)?;
    let descriptor: LaunchDescriptor =
        serde_json::from_slice(&read_private(&directory.join("local-launch.json"))?)?;
    ensure!(
        descriptor.node_id == config::identity(directory)?.node_id()?,
        "local launch belongs to another node"
    );
    let mut url = reqwest::Url::parse(&descriptor.address)?;
    ensure!(
        url.scheme() == "http"
            && url.host_str().is_some_and(|host| host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback()))
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "invalid local launch address"
    );
    let response: serde_json::Value = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?
        .post(url.join("api/local/launch")?)
        .bearer_auth(&descriptor.credential)
        .json(&serde_json::json!({}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let token = response["token"]
        .as_str()
        .context("local launch token missing")?;
    ensure!(
        token.len() == 64 && hex::decode(token).is_ok(),
        "invalid local launch token"
    );
    url.set_fragment(Some(&format!("local_access={token}")));
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_launch_is_one_use_and_disabled_sessions_cannot_return() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("vibeface-local-access-{}", uuid::Uuid::new_v4()));
        config::private_dir(&directory)?;
        let configuration = Config::local("127.0.0.1:4790".parse()?, vec![], false);
        configuration.save(&directory)?;
        let local = LocalAccess::load(&directory, &configuration, &"a".repeat(64))?;
        assert!(local.enabled());
        assert!(local.launch("not-the-launch-credential").is_err());
        let token = local.launch(&local.credential)?;
        let cookie = local.exchange(&token)?;
        assert!(local.exchange(&token).is_err());
        let old = local.session(&cookie)?;
        let closed = local.session(&cookie)?;
        assert!(local.unlock_key(&old)?.is_some());
        local.set_enabled(&old, false)?;
        tokio::time::timeout(Duration::from_millis(100), closed.revoked()).await?;
        assert!(local.session(&cookie).is_err());
        assert!(local.unlock_key(&old).is_err());
        assert!(local.set_enabled(&old, true).is_err());
        assert!(!Config::load(&directory)?.local_access.enabled);
        let token = local.launch(&local.credential)?;
        let cookie2 = local.exchange(&token)?;
        let fresh = local.session(&cookie2)?;
        assert!(local.unlock_key(&fresh)?.is_none());
        local.set_enabled(&fresh, true)?;
        assert!(local.unlock_key(&fresh)?.is_some());
        assert!(local.session(&cookie).is_err());
        local.publish("127.0.0.1:49876".parse()?, "a".repeat(64))?;
        assert_eq!(Config::load(&directory)?.local_access.listen.port(), 49876);
        let reopened = LocalAccess::load(&directory, &Config::load(&directory)?, &"a".repeat(64))?;
        assert!(reopened.session(&cookie2).is_err());
        assert_eq!(
            reopened.state.lock().unwrap().persistent.generation,
            fresh.generation
        );
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}
