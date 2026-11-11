use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::VecDeque, fs::File, path::Path, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
    task::JoinHandle,
};
use vibeface_bridge::platform::{
    ipc::{Endpoint, Listener, Stream},
    privatefs,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Activation {
    Focus,
    Join { ticket: String },
}
impl Activation {
    pub fn parse(raw: &str) -> Option<Self> {
        let input = raw.trim();
        if input.len() > 4096 {
            return None;
        }
        let ticket = input.strip_prefix("vibeface://join/").unwrap_or(input);
        let suffix = ticket.strip_prefix("vibeface1")?;
        if suffix.is_empty()
            || !suffix
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return None;
        }
        Some(Self::Join {
            ticket: ticket.into(),
        })
    }
    fn validate(&self) -> Result<()> {
        match self {
            Self::Focus => Ok(()),
            Self::Join { ticket } => {
                ensure!(
                    Self::parse(ticket).as_ref() == Some(self),
                    "无效的加入激活。"
                );
                Ok(())
            }
        }
    }
}
#[derive(Default)]
pub struct ActivationQueue {
    pending: VecDeque<Activation>,
}
impl ActivationQueue {
    pub fn enqueue(&mut self, activation: Activation) -> bool {
        if activation.validate().is_err() {
            return false;
        }
        if self.pending.contains(&activation) {
            return true;
        }
        if self.pending.len() >= 32 {
            return false;
        }
        self.pending.push_back(activation);
        true
    }
    pub fn pop(&mut self) -> Option<Activation> {
        self.pending.pop_front()
    }
    pub fn defer(&mut self, activation: Activation) -> bool {
        if activation.validate().is_err()
            || (self.pending.len() >= 32 && !self.pending.contains(&activation))
        {
            return false;
        }
        self.pending.retain(|queued| queued != &activation);
        self.pending.push_front(activation);
        true
    }
    pub fn len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(unix)]
fn private_runtime_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("无法创建桌面私有激活目录。"),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "桌面激活目录必须是当前账号的普通私有目录。"
    );
    // Never repair an existing directory owned by somebody else or reachable by other users.
    privatefs::ensure_private(path, "桌面激活目录")
}

fn activation_endpoint(profile: &Path) -> Result<Endpoint> {
    let key = format!("{:x}", Sha256::digest(profile.to_string_lossy().as_bytes()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // prepare_profile has already verified that this directory belongs to this account.
        let account = std::fs::metadata(profile)?.uid();
        let directory = std::path::PathBuf::from(format!("/tmp/vibeface-desktop-{account}"));
        private_runtime_directory(&directory)?;
        let endpoint = Endpoint::in_private_dir(&directory, &format!("{}.sock", &key[..32]))?;
        // A 128-bit profile hash leaves room even for Darwin's /private/tmp spelling.
        use std::os::unix::ffi::OsStrExt;
        ensure!(
            endpoint.path().as_os_str().as_bytes().len() < 104,
            "桌面激活端点超过系统路径限制。"
        );
        Ok(endpoint)
    }
    #[cfg(windows)]
    {
        Endpoint::in_private_dir(profile, &format!("desktop-{}.sock", &key[..16]))
    }
}

pub enum InstanceClaim {
    Primary(SingleInstance),
    Forwarded,
}
pub struct SingleInstance {
    receiver: mpsc::Receiver<Activation>,
    pending: ActivationQueue,
    task: JoinHandle<()>,
    _lock: File,
}
impl SingleInstance {
    pub async fn claim(profile: &Path, argv: &[String]) -> Result<InstanceClaim> {
        super::prepare_profile(profile)?;
        let profile = std::fs::canonicalize(profile)?;
        let endpoint = activation_endpoint(&profile)?;
        endpoint.ensure_private("桌面激活端点")?;
        let lock_path = profile.join("desktop-instance.lock");
        let lock = privatefs::open_or_create(&lock_path)?;
        privatefs::ensure_private(&lock_path, "桌面实例锁")?;
        let activation = argv
            .iter()
            .find_map(|raw| Activation::parse(raw))
            .unwrap_or(Activation::Focus);
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                // A primary may hold the lock while its listener is starting. Only connect retries.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match Stream::connect(&endpoint).await {
                        Ok(stream) => break stream,
                        Err(error) if tokio::time::Instant::now() >= deadline => {
                            return Err(error).context("已有桌面实例无法接收激活。");
                        }
                        Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
                    }
                };
                stream.require_current_account("桌面激活")?;
                let bytes = serde_json::to_vec(&activation)?;
                tokio::time::timeout(Duration::from_secs(2), async {
                    stream.write_u32(bytes.len() as u32).await?;
                    stream.write_all(&bytes).await?;
                    ensure!(stream.read_u8().await? == 1, "桌面激活队列已满。");
                    Ok::<_, anyhow::Error>(())
                })
                .await
                .context("桌面激活超时，尚未确认接收。")??;
                return Ok(InstanceClaim::Forwarded);
            }
            Err(error) => return Err(error.into()),
        }
        // The held OS lock proves no cooperating primary can race stale socket cleanup.
        endpoint.ensure_private("桌面激活端点")?;
        let mut listener = Listener::claim(&endpoint).await?;
        let (sender, receiver) = mpsc::channel(32);
        sender.try_send(activation)?;
        let task = tokio::spawn(async move {
            loop {
                let Ok(mut stream) = listener.accept().await else {
                    return;
                };
                if stream.require_current_account("桌面激活").is_err() {
                    continue;
                }
                let _ = tokio::time::timeout(Duration::from_secs(2), async {
                    let size = stream.read_u32().await? as usize;
                    ensure!(size <= 8192, "桌面激活过大。");
                    let mut bytes = vec![0; size];
                    stream.read_exact(&mut bytes).await?;
                    let activation: Activation = serde_json::from_slice(&bytes)?;
                    activation.validate()?;
                    let accepted = sender.try_send(activation).is_ok();
                    stream.write_u8(u8::from(accepted)).await?;
                    Ok::<_, anyhow::Error>(())
                })
                .await;
            }
        });
        Ok(InstanceClaim::Primary(Self {
            receiver,
            pending: ActivationQueue::default(),
            task,
            _lock: lock,
        }))
    }
    fn receive_pending(&mut self) {
        while self.pending.len() < 32 {
            match self.receiver.try_recv() {
                Ok(activation) => {
                    self.pending.enqueue(activation);
                }
                Err(_) => break,
            }
        }
    }
    /// Drain older IPC arrivals before admitting platform URL events into the same queue.
    pub fn pending_capacity(&mut self) -> usize {
        self.receive_pending();
        32 - self.pending.len()
    }
    pub fn enqueue(&mut self, activation: Activation) -> bool {
        self.pending.enqueue(activation)
    }
    pub fn try_recv(&mut self) -> Option<Activation> {
        self.receive_pending();
        self.pending.pop()
    }
    pub fn defer(&mut self, activation: Activation) -> bool {
        self.pending.defer(activation)
    }
}
impl Drop for SingleInstance {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn activation_runtime_directory_refuses_links_and_permissive_permissions() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("vf-activation-root-{}", uuid::Uuid::new_v4()));
        privatefs::create_dir_new(&root)?;
        let directory = root.join("private");
        private_runtime_directory(&directory)?;
        privatefs::ensure_private(&directory, "synthetic activation directory")?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755))?;
        assert!(private_runtime_directory(&directory).is_err());
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        let alias = root.join("alias");
        symlink(&directory, &alias)?;
        assert!(private_runtime_directory(&alias).is_err());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
