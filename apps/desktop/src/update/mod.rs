mod install;
mod release;

use anyhow::{Context, Result, ensure};
use install::Installation;
use release::{Candidate, Record};
use semver::Version;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use vibeface_bridge::platform::privatefs;

const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
const INITIAL_DELAY: Duration = Duration::from_secs(3);

#[derive(Clone)]
enum Status {
    Checking,
    Current,
    WaitingForExit { version: String, retry: bool },
    Retrying,
    Unavailable,
}

#[derive(Clone)]
pub(crate) struct UpdateStatus(Arc<Mutex<Status>>);

impl UpdateStatus {
    fn set(&self, state: Status) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = state;
    }

    pub(crate) fn label(&self) -> String {
        match &*self.0.lock().unwrap_or_else(|error| error.into_inner()) {
            Status::Checking => "正在检查桌面更新".into(),
            Status::Current => "已是当前渠道的最新版本".into(),
            Status::WaitingForExit {
                version,
                retry: false,
            } => format!("已下载 {version}，退出 Vibeface 后自动安装"),
            Status::WaitingForExit {
                version,
                retry: true,
            } => format!("{version} 上次安装未完成，退出 Vibeface 后重试"),
            Status::Retrying => "暂时无法完成桌面更新，稍后自动重试".into(),
            Status::Unavailable => "当前运行方式需要通过原安装渠道升级".into(),
        }
    }
}

struct RunningUpdate {
    task: tokio::task::JoinHandle<()>,
    install_lock: File,
    pending: Arc<Mutex<Option<Candidate>>>,
    target: Target,
}

pub(crate) struct Updater {
    running: Option<RunningUpdate>,
    status: UpdateStatus,
}

#[derive(Clone)]
struct Target {
    installation: Installation,
    cache: PathBuf,
    current: Version,
}

impl Target {
    fn open(base: &Path) -> Result<Option<Self>> {
        let executable = std::env::current_exe()?;
        let Some(installation) = Installation::detect(&executable)? else {
            return Ok(None);
        };
        let cache = cache_directory(base, &installation)?;
        let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
        Ok(Some(Self {
            installation,
            cache,
            current,
        }))
    }

    fn archive(&self) -> PathBuf {
        self.cache
            .join(format!("package{}", self.installation.asset_suffix()))
    }

    fn apply(&self, candidate: &Candidate) -> Result<()> {
        let installation_lock = private_lock(&self.installation.lock_path()?)?;
        installation_lock
            .try_lock()
            .context("another Desktop instance is using this installation")?;
        let download_lock = private_lock(&self.cache.join("download.lock"))?;
        download_lock.try_lock()?;
        candidate.validate(&self.current, self.installation.asset_suffix())?;
        let archive = self.archive();
        ensure!(
            verify_file(&archive, &candidate.asset)?,
            "the prepared Desktop update no longer matches the release"
        );
        let applied = self.installation.activate(
            &archive,
            &self.current.to_string(),
            &candidate.version.to_string(),
            &candidate.source_commit,
            candidate.signed,
            candidate.current_asset_sha256.as_deref(),
        );
        let marker = self.cache.join("install-error");
        if let Err(error) = applied {
            let _ = privatefs::write_atomic(&marker, b"automatic desktop update failed\n");
            return Err(error);
        }
        let _ = fs::remove_file(marker);
        let _ = fs::remove_file(&archive);
        Ok(())
    }
}

fn private_lock(path: &Path) -> Result<File> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            privatefs::is_plain_file(&metadata),
            "Desktop update lock is not a regular file"
        );
        privatefs::ensure_private(path, "Desktop update lock")?;
    }
    let file = privatefs::open_or_create(path)?;
    ensure!(
        privatefs::is_plain_file(&file.metadata()?),
        "Desktop update lock is not a regular file"
    );
    privatefs::ensure_private(path, "Desktop update lock")?;
    Ok(file)
}

fn cache_directory(base: &Path, installation: &Installation) -> Result<PathBuf> {
    let key = hex::encode(Sha256::digest(
        installation.key().as_os_str().as_encoded_bytes(),
    ));
    let directory = base.join(key);
    for path in [base, directory.as_path()] {
        if !path.try_exists()? {
            privatefs::create_dir(path)?;
        }
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Desktop update directory is not a regular directory"
        );
        privatefs::ensure_private(path, "Desktop update directory")?;
    }
    Ok(directory)
}

impl Updater {
    pub(crate) fn start(
        base: Option<&Path>,
        fixture: bool,
        runtime: &tokio::runtime::Handle,
    ) -> Result<Self> {
        let status = UpdateStatus(Arc::new(Mutex::new(Status::Unavailable)));
        let target = base.filter(|_| !fixture).and_then(|base| {
            Target::open(base).unwrap_or_else(|error| {
                eprintln!("Vibeface Desktop update setup: {error:#}");
                None
            })
        });
        let Some(target) = target else {
            return Ok(Self {
                status,
                running: None,
            });
        };
        let install_lock = match private_lock(&target.installation.lock_path()?) {
            Ok(lock) => lock,
            Err(error) => {
                eprintln!("Vibeface Desktop update lock: {error:#}");
                return Ok(Self {
                    status,
                    running: None,
                });
            }
        };
        install_lock
            .try_lock_shared()
            .context("正在安装桌面更新，请稍后打开 Vibeface")?;
        let client = release::client()?;
        let pending = Arc::new(Mutex::new(None));
        status.set(Status::Checking);
        let task = runtime.spawn(check_periodically(
            target.clone(),
            client,
            status.clone(),
            pending.clone(),
        ));
        Ok(Self {
            status,
            running: Some(RunningUpdate {
                task,
                install_lock,
                pending,
                target,
            }),
        })
    }

    pub(crate) fn status(&self) -> UpdateStatus {
        self.status.clone()
    }

    pub(crate) async fn finish(self) -> Result<()> {
        let Some(RunningUpdate {
            task,
            install_lock,
            pending,
            target,
        }) = self.running
        else {
            return Ok(());
        };
        task.abort();
        let _ = task.await;
        let candidate = pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        drop(install_lock);
        if let Some(candidate) = candidate {
            #[cfg(not(windows))]
            tokio::task::spawn_blocking(move || target.apply(&candidate)).await??;
            #[cfg(windows)]
            windows::launch(target, candidate)?;
        }
        Ok(())
    }
}

async fn check_periodically(
    target: Target,
    client: reqwest::Client,
    status: UpdateStatus,
    pending: Arc<Mutex<Option<Candidate>>>,
) {
    tokio::time::sleep(INITIAL_DELAY).await;
    loop {
        let next = check_once(&target, &client).await;
        let delay = {
            let mut prepared = pending.lock().unwrap_or_else(|error| error.into_inner());
            match next {
                Ok(Some(candidate)) => {
                    status.set(Status::WaitingForExit {
                        version: candidate.version.to_string(),
                        retry: target.cache.join("install-error").is_file(),
                    });
                    *prepared = Some(candidate);
                    CHECK_INTERVAL
                }
                Ok(None) => {
                    status.set(Status::Current);
                    *prepared = None;
                    CHECK_INTERVAL
                }
                Err(error) => {
                    eprintln!("Vibeface Desktop update check: {error:#}");
                    if prepared.is_none() {
                        status.set(Status::Retrying);
                    }
                    Duration::from_secs(10 * 60)
                }
            }
        };
        tokio::time::sleep(delay).await;
    }
}

async fn check_once(target: &Target, client: &reqwest::Client) -> Result<Option<Candidate>> {
    let suffix = target.installation.asset_suffix();
    let Some(candidate) = release::check(client, &target.current, suffix).await? else {
        return Ok(None);
    };
    let lock = private_lock(&target.cache.join("download.lock"))?;
    lock.try_lock()
        .context("another Desktop instance is preparing the update")?;
    stage(target, candidate.clone(), client).await?;
    Ok(Some(candidate))
}

async fn stage(target: &Target, candidate: Candidate, client: &reqwest::Client) -> Result<()> {
    let archive = target.archive();
    let existing = archive.clone();
    let record = candidate.asset.clone();
    if tokio::task::spawn_blocking(move || verify_file(&existing, &record)).await?? {
        return Ok(());
    }
    let partial = target.cache.join("download.partial");
    match fs::remove_file(&partial) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let result = async {
        let mut output = tokio::fs::File::from_std(privatefs::create_new(&partial)?);
        let mut response = candidate.download(client).await?;
        let mut received = 0_u64;
        let mut digest = Sha256::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)?
        {
            received += chunk.len() as u64;
            ensure!(
                received <= candidate.asset.size,
                "Desktop update exceeds its published size"
            );
            digest.update(&chunk);
            output.write_all(&chunk).await?;
        }
        ensure!(
            received == candidate.asset.size
                && hex::encode(digest.finalize()) == candidate.asset.sha256,
            "downloaded Desktop update differs from the released asset"
        );
        output.sync_all().await?;
        drop(output);
        privatefs::durable_rename(&partial, &archive)?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

fn verify_file(path: &Path, record: &Record) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !privatefs::is_plain_file(&metadata)
        || metadata.len() != record.size
        || !privatefs::is_private(path)?
    {
        return Ok(false);
    }
    let mut sha256 = Sha256::new();
    let size = io::copy(&mut File::open(path)?.take(record.size + 1), &mut sha256)?;
    Ok(size == record.size && hex::encode(sha256.finalize()) == record.sha256)
}

#[cfg(windows)]
pub(crate) fn apply_requested(base: Option<&Path>) -> Result<bool> {
    windows::apply_requested(base)
}

#[cfg(windows)]
mod windows {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::{
        io::{BufRead, Write},
        os::windows::process::CommandExt,
        process::{ChildStdin, Command, Stdio},
        sync::OnceLock,
    };

    const UPDATE_ARGUMENT: &str = "--apply-prepared-desktop-update";
    const HELPER: &str = "install-helper.exe";
    static PARENT_PIPE: OnceLock<ChildStdin> = OnceLock::new();

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Plan {
        installation: Installation,
        candidate: Candidate,
    }

    pub(super) fn launch(target: Target, candidate: Candidate) -> Result<()> {
        let helper = target.cache.join(HELPER);
        match fs::remove_file(&helper) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut copy = privatefs::create_new(&helper)?;
        io::copy(&mut File::open(std::env::current_exe()?)?, &mut copy)?;
        copy.sync_all()?;
        drop(copy);
        let mut child = Command::new(&helper)
            .arg(UPDATE_ARGUMENT)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
            .context("launch Desktop update helper")?;
        let mut pipe = child
            .stdin
            .take()
            .context("Desktop update helper has no control channel")?;
        let plan = Plan {
            installation: target.installation,
            candidate,
        };
        serde_json::to_writer(&mut pipe, &plan)?;
        pipe.write_all(b"\n")?;
        // Windows holds the running executable open. The OS closes this handle at process exit.
        PARENT_PIPE
            .set(pipe)
            .map_err(|_| anyhow::anyhow!("Desktop update helper already started"))?;
        Ok(())
    }

    pub(super) fn apply_requested(base: Option<&Path>) -> Result<bool> {
        let mut arguments = std::env::args_os().skip(1);
        if !arguments
            .next()
            .is_some_and(|argument| argument == UPDATE_ARGUMENT)
        {
            return Ok(false);
        }
        ensure!(
            arguments.next().is_none(),
            "unexpected Desktop update arguments"
        );
        let mut input = Vec::new();
        let mut pipe = io::BufReader::new(io::stdin().lock().take(16 * 1024));
        pipe.read_until(b'\n', &mut input)?;
        ensure!(
            input.ends_with(b"\n"),
            "missing complete Desktop update plan"
        );
        let Plan {
            installation,
            candidate,
        } = serde_json::from_slice(&input)?;
        ensure!(
            pipe.read(&mut [0])? == 0 && pipe.get_ref().limit() != 0,
            "unexpected data in Desktop update plan"
        );
        let cache = cache_directory(
            base.context("Desktop update cache directory is unavailable")?,
            &installation,
        )?;
        ensure!(
            fs::canonicalize(cache.join(HELPER))? == fs::canonicalize(std::env::current_exe()?)?,
            "Desktop installer helper is outside its private update directory"
        );
        let target = Target {
            installation,
            cache,
            current: Version::parse(env!("CARGO_PKG_VERSION"))?,
        };
        target.apply(&candidate)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_update_bytes_must_match_size_and_digest() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("vibeface-update-{}", uuid::Uuid::new_v4()));
        privatefs::create_dir_new(&dir)?;
        let path = dir.join("download");
        privatefs::write_atomic(&path, b"released")?;
        let record = Record {
            name: "update".into(),
            size: 8,
            sha256: hex::encode(Sha256::digest(b"released")),
        };
        assert!(verify_file(&path, &record)?);
        privatefs::write_atomic(&path, b"modified")?;
        assert!(!verify_file(&path, &record)?);
        privatefs::write_atomic(&path, b"released longer")?;
        assert!(!verify_file(&path, &record)?);
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
