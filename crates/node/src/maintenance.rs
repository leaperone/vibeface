use crate::runtime::Runtime;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::Notify,
};

const PROTOCOL: u32 = 1;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const SOCKET_NAME: &str = "maintenance.sock";

#[derive(Debug)]
pub struct Busy(pub String);
impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "node is busy: {}", self.0)
    }
}
impl std::error::Error for Busy {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Open,
    Maintenance,
    PendingActivation,
}
struct State {
    mode: Mode,
    active: usize,
}
pub struct Gate {
    state: Mutex<State>,
    changed: Notify,
}
#[must_use]
pub struct WorkGuard(Arc<Gate>);
struct MaintenanceGuard {
    gate: Arc<Gate>,
    pending: Option<PathBuf>,
}

impl Gate {
    pub fn new(pending_activation: bool) -> Self {
        Self {
            state: Mutex::new(State {
                mode: if pending_activation {
                    Mode::PendingActivation
                } else {
                    Mode::Open
                },
                active: 0,
            }),
            changed: Notify::new(),
        }
    }
    pub fn is_open(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).mode == Mode::Open
    }
    pub fn enter(self: &Arc<Self>) -> Result<WorkGuard> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.mode != Mode::Open {
            return Err(Busy("maintenance is in progress; retry after the update".into()).into());
        }
        state.active = state
            .active
            .checked_add(1)
            .context("too many active operations")?;
        Ok(WorkGuard(self.clone()))
    }
    pub async fn enter_when_open(self: &Arc<Self>) -> WorkGuard {
        loop {
            let changed = self.changed.notified();
            if let Ok(guard) = self.enter() {
                return guard;
            }
            changed.await;
        }
    }
    fn acquire(self: &Arc<Self>, pending: Option<PathBuf>) -> Result<MaintenanceGuard> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.mode != Mode::Open || state.active != 0 {
            return Err(Busy("active work or another maintenance request".into()).into());
        }
        state.mode = Mode::Maintenance;
        Ok(MaintenanceGuard {
            gate: self.clone(),
            pending,
        })
    }
    fn commit(&self, pending: &Path) -> Result<()> {
        self.commit_with_sync(pending, |parent| {
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        })
    }
    fn commit_with_sync(
        &self,
        pending: &Path,
        sync: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state.mode != Mode::Maintenance,
            "another maintenance lease is active"
        );
        let synced = match std::fs::symlink_metadata(pending) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file()
                        && !metadata.file_type().is_symlink()
                        && metadata.len() <= 65536,
                    "invalid update activation journal"
                );
                let journal: serde_json::Value = serde_json::from_slice(&std::fs::read(pending)?)?;
                ensure!(
                    journal["phase"] == "confirmed",
                    "update activation has not been confirmed"
                );
                std::fs::remove_file(pending)?;
                sync(pending.parent().context("update journal has no parent")?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => return Err(error.into()),
        };
        state.mode = Mode::Open;
        self.changed.notify_waiters();
        synced
    }
}
impl Drop for WorkGuard {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active -= 1;
    }
}
impl Drop for MaintenanceGuard {
    fn drop(&mut self) {
        let pending = self
            .pending
            .as_ref()
            .is_some_and(|path| path.try_exists().unwrap_or(true));
        self.gate
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mode = if pending {
            Mode::PendingActivation
        } else {
            Mode::Open
        };
        self.gate.changed.notify_waiters();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    pub device_id: String,
    pub version: String,
}
#[derive(Debug)]
#[must_use]
pub struct Lease {
    pub health: Health,
    _stream: UnixStream,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Command {
    Acquire,
    Probe,
    Commit,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    protocol: u32,
    command: Command,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Ready,
    Healthy,
    Committed,
    Busy,
    Error,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    protocol: u32,
    status: Status,
    health: Option<Health>,
    error: Option<String>,
}

async fn read_packet<T: for<'de> Deserialize<'de>>(stream: &mut UnixStream) -> Result<T> {
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        let mut packet = Vec::new();
        let mut chunk = [0u8; 256];
        loop {
            let count = stream.read(&mut chunk).await?;
            ensure!(count != 0, "maintenance socket closed before response");
            packet.extend_from_slice(&chunk[..count]);
            ensure!(packet.len() <= 4096, "maintenance packet is too large");
            if let Some(end) = packet.iter().position(|byte| *byte == b'\n') {
                ensure!(
                    end + 1 == packet.len(),
                    "unexpected maintenance packet data"
                );
                return serde_json::from_slice(&packet[..end])
                    .context("invalid maintenance packet");
            }
        }
    })
    .await
    .context("maintenance socket timed out")?
}
async fn write_packet<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    tokio::time::timeout(REQUEST_TIMEOUT, stream.write_all(&bytes))
        .await
        .context("maintenance socket timed out")??;
    Ok(())
}
pub fn socket_path(data_dir: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let device_id = crate::config::identity(data_dir)?.device_id()?;
    let socket_dir = PathBuf::from("/tmp").join(format!("vibeface-{}", &device_id[..32]));
    match std::fs::create_dir(&socket_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = std::fs::symlink_metadata(&socket_dir)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == std::fs::metadata(data_dir)?.uid(),
        "private maintenance socket directory has unexpected ownership"
    );
    std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(socket_dir.join(SOCKET_NAME))
}

async fn request(data_dir: &Path, command: Command) -> Result<(UnixStream, Response)> {
    let mut stream =
        tokio::time::timeout(REQUEST_TIMEOUT, UnixStream::connect(socket_path(data_dir)?))
            .await
            .context("maintenance connection timed out")??;
    write_packet(
        &mut stream,
        &Request {
            protocol: PROTOCOL,
            command,
        },
    )
    .await?;
    let response: Response = read_packet(&mut stream).await?;
    ensure!(
        response.protocol == PROTOCOL,
        "unsupported maintenance protocol"
    );
    match response.status {
        Status::Busy => {
            let error = response.error.unwrap_or_else(|| "active work".into());
            return Err(Busy(error.trim_start_matches("node is busy: ").to_owned()).into());
        }
        Status::Error => bail!(
            response
                .error
                .unwrap_or_else(|| "maintenance request failed".into())
        ),
        _ => {}
    }
    Ok((stream, response))
}
pub async fn acquire(data_dir: &Path) -> Result<Lease> {
    let (stream, response) = request(data_dir, Command::Acquire).await?;
    ensure!(
        response.status == Status::Ready,
        "maintenance lease was not granted"
    );
    Ok(Lease {
        health: response
            .health
            .context("maintenance response has no identity")?,
        _stream: stream,
    })
}
pub async fn probe(data_dir: &Path) -> Result<Health> {
    let (_, response) = request(data_dir, Command::Probe).await?;
    ensure!(
        response.status == Status::Healthy,
        "node did not report healthy"
    );
    response
        .health
        .context("maintenance response has no identity")
}
pub async fn commit(data_dir: &Path) -> Result<()> {
    let (_, response) = request(data_dir, Command::Commit).await?;
    ensure!(
        response.status == Status::Committed,
        "node activation was not committed"
    );
    Ok(())
}

pub async fn serve(runtime: Arc<Runtime>, data_dir: PathBuf) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let socket = socket_path(&data_dir)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&socket) {
        ensure!(
            metadata.file_type().is_socket(),
            "maintenance socket path is occupied by another file"
        );
        ensure!(
            UnixStream::connect(&socket).await.is_err(),
            "maintenance socket is already active"
        );
        std::fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    loop {
        let (stream, _) = listener.accept().await?;
        let runtime = runtime.clone();
        let pending = data_dir.join("update-pending.json");
        tokio::spawn(async move {
            if let Err(error) = connection(runtime, pending, stream).await {
                tracing::debug!(%error, "maintenance connection ended");
            }
        });
    }
}
async fn connection(runtime: Arc<Runtime>, pending: PathBuf, mut stream: UnixStream) -> Result<()> {
    let request: Request = read_packet(&mut stream).await?;
    ensure!(
        request.protocol == PROTOCOL,
        "unsupported maintenance protocol"
    );
    let health = Health {
        device_id: runtime.identity.device_id()?,
        version: env!("CARGO_PKG_VERSION").into(),
    };
    let mut response = Response {
        protocol: PROTOCOL,
        status: Status::Healthy,
        health: Some(health),
        error: None,
    };
    match request.command {
        Command::Probe => write_packet(&mut stream, &response).await?,
        Command::Commit => {
            let result = runtime.maintenance.commit(&pending);
            response.status = if result.is_ok() {
                Status::Committed
            } else {
                Status::Error
            };
            response.error = result.err().map(|error| error.to_string());
            write_packet(&mut stream, &response).await?;
        }
        Command::Acquire => {
            let lease = match runtime.maintenance.acquire(Some(pending)) {
                Ok(lease) => {
                    if let Err(error) = runtime.maintenance_idle().await {
                        drop(lease);
                        response.status = if error.downcast_ref::<Busy>().is_some() {
                            Status::Busy
                        } else {
                            Status::Error
                        };
                        response.error = Some(error.to_string());
                        write_packet(&mut stream, &response).await?;
                        return Ok(());
                    }
                    lease
                }
                Err(error) => {
                    response.status = Status::Busy;
                    response.error = Some(error.to_string());
                    write_packet(&mut stream, &response).await?;
                    return Ok(());
                }
            };
            response.status = Status::Ready;
            write_packet(&mut stream, &response).await?;
            let mut byte = [0];
            let _ = stream.read(&mut byte).await;
            drop(lease);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{self, Config},
        crypto::LocalIdentity,
    };

    #[test]
    fn device_id_health_uses_only_the_current_field() -> Result<()> {
        let wire = r#"{"device_id":"device","version":"version"}"#;
        let health: Health = serde_json::from_str(wire)?;
        assert_eq!(health.device_id, "device");
        assert_eq!(serde_json::to_string(&health)?, wire);
        assert!(serde_json::from_str::<Health>(&wire.replace("device_id", "node_id")).is_err());
        Ok(())
    }

    #[test]
    fn confirmed_unlink_sync_failure_does_not_leave_unrecoverable_maintenance() -> Result<()> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("commit-sync-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let pending = dir.join("update-pending.json");
        crate::config::private_write(&pending, br#"{"phase":"confirmed"}"#)?;
        let gate = Arc::new(Gate::new(true));
        assert!(gate.enter().is_err());
        let result = gate.commit_with_sync(&pending, |_| bail!("injected directory sync failure"));
        assert!(result.is_err());
        assert!(!pending.exists());
        assert!(gate.enter().is_ok());
        gate.commit(&pending)?;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn active_work_refuses_maintenance_and_nested_guards_are_safe() -> Result<()> {
        let gate = Arc::new(Gate::new(false));
        let first = gate.enter()?;
        let nested = gate.enter()?;
        assert!(gate.acquire(None).is_err());
        drop(first);
        assert!(gate.acquire(None).is_err());
        drop(nested);
        let lease = gate.acquire(None)?;
        assert!(gate.enter().is_err());
        drop(lease);
        assert!(gate.enter().is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn socket_lease_blocks_work_until_disconnect_and_pending_needs_commit() -> Result<()> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("maintenance-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        let dir = dir.canonicalize()?;
        config::private_write(&dir.join("data-epoch.json"), br#"{"data_epoch":7}"#)?;
        let mut config = Config::local("127.0.0.1:4790".parse()?, vec![dir.canonicalize()?], false);
        config.codex_socket = Some(dir.join("unavailable.sock"));
        config.save(&dir)?;
        config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&LocalIdentity::generate())?,
        )?;
        config::private_write(&dir.join("update-pending.json"), b"{}")?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let server = tokio::spawn(serve(node.clone(), dir.clone()));
        let result = async {
            for _ in 0..100 {
                if socket_path(&dir)?.exists() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(node.maintenance.enter().is_err());
            let health = probe(&dir).await?;
            assert_eq!(health.device_id, node.identity.device_id()?);
            assert_eq!(health.version, env!("CARGO_PKG_VERSION"));
            assert!(commit(&dir).await.is_err());
            config::private_write(
                &dir.join("update-pending.json"),
                br#"{"phase":"confirmed"}"#,
            )?;
            commit(&dir).await?;
            assert!(!dir.join("update-pending.json").exists());
            let work = node.maintenance.enter()?;
            assert!(
                acquire(&dir)
                    .await
                    .unwrap_err()
                    .downcast_ref::<Busy>()
                    .is_some()
            );
            drop(work);
            let lease = acquire(&dir).await?;
            assert_eq!(lease.health.device_id, health.device_id);
            assert!(node.maintenance.enter().is_err());
            assert!(
                crate::scheduler::status(&node, "test", "chat", "trigger")
                    .await
                    .is_err()
            );
            drop(lease);
            let resumed =
                tokio::time::timeout(Duration::from_secs(1), node.maintenance.enter_when_open())
                    .await?;
            drop(resumed);
            assert!(node.maintenance.enter().is_ok());
            let lease = acquire(&dir).await?;
            config::private_write(&dir.join("update-pending.json"), b"{}")?;
            drop(lease);
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if node.maintenance.state.lock().unwrap().mode == Mode::PendingActivation {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            assert!(node.maintenance.enter().is_err());
            assert_eq!(probe(&dir).await?.device_id, health.device_id);
            config::private_write(
                &dir.join("update-pending.json"),
                br#"{"phase":"confirmed"}"#,
            )?;
            commit(&dir).await?;
            assert!(!dir.join("update-pending.json").exists());
            assert!(node.maintenance.enter().is_ok());
            Ok::<_, anyhow::Error>(())
        }
        .await;
        server.abort();
        let _ = server.await;
        drop(node);
        let socket = socket_path(&dir)?;
        std::fs::remove_file(&socket)?;
        std::fs::remove_dir(socket.parent().unwrap())?;
        std::fs::remove_dir_all(&dir)?;
        result
    }
}
