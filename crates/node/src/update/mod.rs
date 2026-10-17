pub mod release;

use crate::{config, maintenance};
use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Subcommand};
use release::Channel;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;

#[derive(Args)]
pub struct Update {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Select a release channel and install or remove the automatic update timer.
    Configure {
        #[arg(long, value_enum)]
        channel: Channel,
        #[arg(long, action = clap::ArgAction::Set)]
        automatic: bool,
        #[arg(long, default_value_os_t = default_install_root())]
        install_root: PathBuf,
        #[command(flatten)]
        data: Data,
    },
    /// Show the local policy and any interrupted activation.
    Status(Data),
    /// Check the selected channel without stopping the node.
    Check(Data),
    /// Install a newer compatible release when this node is idle.
    Apply {
        /// Used by the user-service timer; disabled policies do nothing.
        #[arg(long)]
        automatic: bool,
        #[command(flatten)]
        data: Data,
    },
}

#[derive(Args)]
struct Data {
    #[arg(long, default_value_os_t = config::default_dir())]
    data_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    channel: Channel,
    automatic: bool,
    install_root: PathBuf,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            channel: Channel::Stable,
            automatic: false,
            install_root: default_install_root(),
        }
    }
}

#[derive(Deserialize)]
struct Manifest {
    version: semver::Version,
    update_protocol: u32,
    data_epoch: u32,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    Confirmed,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    phase: Phase,
    previous: PathBuf,
    candidate: PathBuf,
    node_id: String,
    backup: PathBuf,
}

fn default_install_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/share/vibeface")
}

fn policy(data: &Path) -> Result<Policy> {
    let path = data.join("update.json");
    if path.exists() {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    } else {
        Ok(Policy::default())
    }
}

/// Resolve the same managed installation used by this node's update policy.
pub fn managed_install_root(data: &Path) -> Result<PathBuf> {
    let root = policy(data)?.install_root;
    ensure!(root.is_absolute(), "install-root must be absolute");
    let root = fs::canonicalize(root)
        .context("managed installation is required; install a published Vibeface package first")?;
    installed(&root)?;
    Ok(root)
}

pub async fn run(options: Update) -> Result<()> {
    let data = match &options.action {
        Action::Configure { data, .. }
        | Action::Status(data)
        | Action::Check(data)
        | Action::Apply { data, .. } => &data.data_dir,
    };
    ensure!(data.is_absolute(), "--data-dir must be absolute");
    let data = fs::canonicalize(data).context("initialized data directory is required")?;
    config::verify_data_format(&data)?;
    config::identity(&data)?;
    let _lock = update_lock(&data)?;
    let policy_path = data.join("update.json");
    let policy = policy(&data)?;
    match options.action {
        Action::Status(_) => {
            let current = installed(&policy.install_root)
                .and_then(|path| manifest(&path).map(|m| m.version))
                .ok();
            println!(
                "{}",
                serde_json::json!({"policy":policy,"current_version":current,
                    "recovery_required":data.join("update-pending.json").exists()})
            );
            Ok(())
        }
        Action::Configure {
            channel,
            automatic,
            install_root,
            ..
        } => {
            ensure!(
                !data.join("update-pending.json").exists(),
                "interrupted update: run update apply with the existing policy first"
            );
            ensure!(
                install_root.is_absolute(),
                "--install-root must be absolute"
            );
            let install_root = fs::canonicalize(install_root)?;
            let package = installed(&install_root)?;
            manifest(&package)?;
            let new = Policy {
                channel,
                automatic,
                install_root,
            };
            // Keep the old policy if the platform cannot install the requested timer.
            configure_timer(&new, &data).await?;
            persist(&policy_path, &new)?;
            println!("{}", serde_json::to_string(&new)?);
            Ok(())
        }
        Action::Check(_) => {
            let current = manifest(&installed(&policy.install_root)?)?.version;
            let latest = release::check(policy.channel).await?;
            let newer = latest.as_ref().filter(|r| r.version > current);
            println!(
                "{}",
                serde_json::json!({"channel":policy.channel,"current_version":current,
                    "available_version":newer.map(|r|&r.version),
                    "tag":newer.map(|r|&r.tag)})
            );
            Ok(())
        }
        Action::Apply { automatic, .. } => {
            recover(&policy, &data).await?;
            if automatic && !policy.automatic {
                println!("Automatic updates are disabled.");
                return Ok(());
            }
            let previous = installed(&policy.install_root)?;
            let current = manifest(&previous)?.version;
            let Some(latest) = release::check(policy.channel)
                .await?
                .filter(|r| r.version > current)
            else {
                println!("No newer release is available in the selected channel.");
                return Ok(());
            };
            let candidate = fs::canonicalize(release::stage(&latest, &policy.install_root).await?)?;
            ensure!(
                manifest(&candidate)?.version == latest.version,
                "staged version changed"
            );
            prepare_firewall(&data, &policy.install_root, &candidate).await?;
            let service = Service::load(&data).await?;
            let lease = match maintenance::acquire(&data).await {
                Ok(lease) => lease,
                Err(error) if error.downcast_ref::<maintenance::Busy>().is_some() => {
                    println!("Update deferred; the node could not become idle: {error:#}");
                    return Ok(());
                }
                Err(error) => return Err(error.context("request node maintenance")),
            };
            ensure!(
                lease.health.version == current.to_string(),
                "running node does not match current package"
            );
            ensure!(
                lease.health.node_id == config::identity(&data)?.node_id()?,
                "running node identity changed"
            );
            let root = fs::canonicalize(&policy.install_root)?;
            ensure!(
                !root.starts_with(&data),
                "install-root must not be inside the node data directory"
            );
            let backups = root.join("backups");
            if !backups.exists() {
                fs::create_dir(&backups)?;
            }
            ensure!(
                fs::symlink_metadata(&backups)?.is_dir(),
                "backup directory must not be a symlink"
            );
            config::private_dir(&backups)?;
            let mut journal = Journal {
                phase: Phase::Prepared,
                previous,
                candidate,
                node_id: lease.health.node_id.clone(),
                backup: backups.join(uuid::Uuid::new_v4().to_string()),
            };
            persist(&data.join("update-pending.json"), &journal)?;
            // The open lease closes admission until the service has actually stopped.
            let applied = activate(&policy, &data, &service, &mut journal).await;
            drop(lease);
            if let Err(error) = applied {
                match recover(&policy, &data).await {
                    Ok(()) if installed(&policy.install_root)? == journal.candidate => {
                        maintenance::commit(&data)
                            .await
                            .context("confirmed update must reopen admission")?;
                        healthy(&data, &journal.candidate, &journal.node_id).await?;
                    }
                    Ok(()) => {
                        bail!("update failed; previous program restored, data was kept: {error:#}")
                    }
                    Err(rollback) => bail!(
                        "update failed: {error:#}; recovery also failed: {rollback:#}; keep update-pending.json and run update apply again"
                    ),
                }
            }
            println!(
                "Updated to {}. Backup: {}",
                latest.version,
                journal.backup.display()
            );
            Ok(())
        }
    }
}

fn update_lock(data: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(data.join("update.lock"))?;
    file.try_lock()
        .context("another updater is already running")?;
    Ok(file)
}

fn manifest(package: &Path) -> Result<Manifest> {
    let metadata = fs::symlink_metadata(package.join("BUILD-INFO.json"))?;
    ensure!(
        metadata.is_file() && metadata.len() <= 64 * 1024,
        "invalid package manifest"
    );
    let manifest: Manifest = serde_json::from_slice(&fs::read(package.join("BUILD-INFO.json"))?)
        .context("this package needs a manual bootstrap to the updater-enabled release")?;
    ensure!(
        manifest.update_protocol == 2 && manifest.data_epoch == crate::config::DATA_EPOCH,
        "package does not support the current updater and data compatibility epoch; manual upgrade required"
    );
    ensure!(
        fs::symlink_metadata(package.join("vibeface"))?.is_file(),
        "package binary is missing"
    );
    ensure!(
        fs::symlink_metadata(package.join("scripts/install-node-service.sh"))?.is_file(),
        "package service installer is missing"
    );
    Ok(manifest)
}

fn installed(root: &Path) -> Result<PathBuf> {
    ensure!(root.is_absolute(), "install-root must be absolute");
    let root = fs::canonicalize(root)?;
    let current = root.join("current");
    ensure!(
        fs::symlink_metadata(&current)?.is_symlink(),
        "managed installation requires a current symlink"
    );
    let package = fs::canonicalize(current)?;
    ensure!(
        package.starts_with(&root) && package != root,
        "current points outside the installation"
    );
    manifest(&package)?;
    Ok(package)
}

async fn configure_timer(policy: &Policy, data: &Path) -> Result<()> {
    let mut command = Command::new(
        policy
            .install_root
            .join("current/scripts/install-node-service.sh"),
    );
    if policy.automatic {
        command
            .arg("install-updater")
            .arg("--binary")
            .arg(policy.install_root.join("current/vibeface"));
    } else {
        command.arg("uninstall-updater");
    }
    command.arg("--data-dir").arg(data);
    checked(&mut command, "configure automatic update service").await
}

fn persist(path: &Path, value: &impl Serialize) -> Result<()> {
    config::private_write(path, &serde_json::to_vec_pretty(value)?)?;
    File::open(path.parent().context("missing parent directory")?)?.sync_all()?;
    Ok(())
}

fn switch(root: &Path, package: &Path) -> Result<()> {
    let root = fs::canonicalize(root)?;
    let package = fs::canonicalize(package)?;
    ensure!(
        package.starts_with(&root) && package != root,
        "package is outside installation"
    );
    manifest(&package)?;
    let temporary = root.join(format!(".current-{}", uuid::Uuid::new_v4()));
    std::os::unix::fs::symlink(&package, &temporary)?;
    if let Err(error) = fs::rename(&temporary, root.join("current")) {
        let _ = fs::remove_file(temporary);
        return Err(error.into());
    }
    File::open(root)?.sync_all()?;
    Ok(())
}

async fn activate(
    policy: &Policy,
    data: &Path,
    service: &impl ServiceControl,
    journal: &mut Journal,
) -> Result<()> {
    service.stop().await?;
    let lock = wait_for_node_lock(data).await?;
    backup(data, &journal.backup)?;
    switch(&policy.install_root, &journal.candidate)?;
    drop(lock);
    service.start().await?;
    service
        .healthy(data, &journal.candidate, &journal.node_id)
        .await?;
    // Confirm durably before opening admission; recovery must never undo a committed update.
    journal.phase = Phase::Confirmed;
    persist(&data.join("update-pending.json"), journal)?;
    service.commit(data).await?;
    Ok(())
}

async fn recover(policy: &Policy, data: &Path) -> Result<()> {
    if !data.join("update-pending.json").exists() {
        return Ok(());
    }
    let service = Service::load(data).await?;
    recover_with_service(policy, data, &service).await
}

async fn recover_with_service(
    policy: &Policy,
    data: &Path,
    service: &impl ServiceControl,
) -> Result<()> {
    let pending = data.join("update-pending.json");
    if !pending.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(&pending)?;
    ensure!(
        metadata.is_file() && metadata.len() <= 64 * 1024,
        "invalid pending update journal"
    );
    let mut journal: Journal = serde_json::from_slice(&fs::read(&pending)?)?;
    let root = fs::canonicalize(&policy.install_root)?;
    ensure!(
        journal.backup.parent() == Some(root.join("backups").as_path())
            && journal
                .backup
                .file_name()
                .is_some_and(|name| !name.is_empty()),
        "journal backup path is outside installation"
    );
    for package in [&journal.previous, &journal.candidate] {
        ensure!(
            package.is_absolute()
                && fs::canonicalize(package)? == *package
                && package.starts_with(&root)
                && package != &root,
            "journal package path is outside installation"
        );
        manifest(package)?;
    }
    ensure!(
        journal.node_id == config::identity(data)?.node_id()?,
        "update journal belongs to another node"
    );
    let confirmed = journal.phase == Phase::Confirmed;
    let wanted = if confirmed {
        journal.candidate.clone()
    } else {
        journal.previous.clone()
    };
    if confirmed
        && installed(&root)? == wanted
        && service
            .healthy(data, &wanted, &journal.node_id)
            .await
            .is_ok()
    {
        service.commit(data).await?;
        return Ok(());
    }
    service.stop().await?;
    let lock = wait_for_node_lock(data).await?;
    switch(&root, &wanted)?;
    drop(lock);
    service.start().await?;
    service.healthy(data, &wanted, &journal.node_id).await?;
    journal.candidate = wanted;
    journal.phase = Phase::Confirmed;
    persist(&pending, &journal)?;
    service.commit(data).await?;
    println!(
        "Recovered {} program; business data was kept.",
        if confirmed { "confirmed" } else { "previous" }
    );
    Ok(())
}

async fn wait_for_node_lock(data: &Path) -> Result<File> {
    for _ in 0..150 {
        if let Ok(lock) = config::lock_data_dir(data) {
            return Ok(lock);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("node did not release its data lock after stopping")
}

async fn healthy(data: &Path, package: &Path, node_id: &str) -> Result<()> {
    let version = manifest(package)?.version.to_string();
    let mut listen = config::Config::load(data)?.listen;
    if listen.ip().is_unspecified() {
        listen.set_ip(if listen.is_ipv6() {
            std::net::Ipv6Addr::LOCALHOST.into()
        } else {
            std::net::Ipv4Addr::LOCALHOST.into()
        });
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(1))
        .build()?;
    let url = format!("http://{listen}/api/info");
    let host = format!(
        "{}:{}",
        if listen.is_ipv6() {
            "[::1]"
        } else {
            "127.0.0.1"
        },
        listen.port()
    );
    let mut last = String::new();
    for _ in 0..150 {
        match maintenance::probe(data).await {
            Ok(health) if health.node_id == node_id && health.version == version => {
                match client
                    .get(&url)
                    .header(reqwest::header::HOST, &host)
                    .send()
                    .await
                {
                    Ok(response) if response.status().is_success() => {
                        if let Ok(info) = response.json::<serde_json::Value>().await {
                            ensure!(
                                info["node_id"] == node_id && info["version"] == version,
                                "HTTP listener belongs to a different node or version"
                            );
                            return Ok(());
                        }
                        last = "invalid local HTTP health response".into();
                    }
                    Ok(response) => {
                        last = format!("local HTTP listener returned {}", response.status())
                    }
                    Err(error) => last = error.to_string(),
                }
            }
            Ok(_) => bail!("started node has an unexpected version or identity"),
            Err(error) => last = error.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    bail!("node failed its private health check: {last}")
}

fn backup(source: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    fs::create_dir(destination)?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let target = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_socket() {
            continue;
        }
        if metadata.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(path)?, target)?;
        } else if metadata.is_dir() {
            backup(&path, &target)?;
        } else {
            ensure!(
                metadata.is_file(),
                "unsupported data file: {}",
                path.display()
            );
            fs::copy(&path, &target)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
            File::open(target)?.sync_all()?;
        }
    }
    File::open(destination)?.sync_all()?;
    Ok(())
}

async fn prepare_firewall(data: &Path, root: &Path, candidate: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if config::Config::load(data)?.listen.ip().is_loopback() {
            return Ok(());
        }
        let tool = "/usr/libexec/ApplicationFirewall/socketfilterfw";
        let state = Command::new(tool).arg("--getglobalstate").output().await?;
        ensure!(
            state.status.success(),
            "could not inspect application firewall before LAN update"
        );
        let state = String::from_utf8(state.stdout)?;
        if state.contains("State = 0") {
            return Ok(());
        }
        ensure!(
            state.contains("State = 1"),
            "unknown application firewall state; update deferred"
        );
        for binary in [candidate.join("vibeface"), root.join("current/vibeface")] {
            checked(
                Command::new(tool).arg("--add").arg(&binary),
                "retain this node's application firewall permission",
            )
            .await?;
            let permission = Command::new(tool)
                .arg("--getappblocked")
                .arg(&binary)
                .output()
                .await?;
            ensure!(
                permission.status.success()
                    && String::from_utf8(permission.stdout)?.contains("is permitted."),
                "application firewall did not permit the new node binary; old service was kept"
            );
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (data, root, candidate);
    Ok(())
}

struct Service {
    #[cfg(target_os = "macos")]
    domain: String,
    #[cfg(target_os = "macos")]
    plist: PathBuf,
}

trait ServiceControl {
    async fn stop(&self) -> Result<()>;
    async fn start(&self) -> Result<()>;
    async fn healthy(&self, data: &Path, package: &Path, node_id: &str) -> Result<()> {
        healthy(data, package, node_id).await
    }
    async fn commit(&self, data: &Path) -> Result<()> {
        maintenance::commit(data).await
    }
}

impl Service {
    async fn load(data: &Path) -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            let home = std::env::var_os("HOME").context("HOME is required for the user service")?;
            let plist =
                PathBuf::from(home).join("Library/LaunchAgents/com.leaperone.vibeface.node.plist");
            let output = Command::new("/usr/bin/plutil")
                .args(["-extract", "ProgramArguments", "json", "-o", "-"])
                .arg(&plist)
                .output()
                .await?;
            ensure!(
                output.status.success(),
                "installed node LaunchAgent is required"
            );
            let args: Vec<String> = serde_json::from_slice(&output.stdout)?;
            ensure!(
                service_data_matches(&args, data),
                "installed node service uses a different data directory"
            );
            let uid = Command::new("/usr/bin/id").arg("-u").output().await?;
            ensure!(
                uid.status.success(),
                "could not identify user service domain"
            );
            let uid = String::from_utf8(uid.stdout)?.trim().to_owned();
            ensure!(
                !uid.is_empty() && uid.bytes().all(|c| c.is_ascii_digit()),
                "invalid user id"
            );
            Ok(Self {
                domain: format!("gui/{uid}"),
                plist,
            })
        }
        #[cfg(target_os = "linux")]
        {
            let output = Command::new("systemctl")
                .args(["--user", "cat", "vibeface.service"])
                .output()
                .await?;
            ensure!(
                output.status.success(),
                "installed systemd user node service is required"
            );
            let unit = String::from_utf8(output.stdout)?;
            let escaped = data
                .to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('%', "%%")
                .replace('$', "$$");
            let mut starts = Vec::new();
            for line in unit
                .lines()
                .filter_map(|line| line.strip_prefix("ExecStart="))
            {
                if line.is_empty() {
                    starts.clear();
                } else {
                    starts.push(line);
                }
            }
            ensure!(
                starts.len() == 1 && starts[0].ends_with(&format!("--data-dir \"{escaped}\"")),
                "installed node service uses a different data directory or an unsupported ExecStart"
            );
            Ok(Self {})
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        bail!("automatic updates require macOS launchd or Linux systemd user services")
    }
}

impl ServiceControl for Service {
    async fn stop(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let label = format!("{}/com.leaperone.vibeface.node", self.domain);
            let loaded = Command::new("/bin/launchctl")
                .args(["print", &label])
                .output()
                .await?;
            if loaded.status.success() {
                checked(
                    Command::new("/bin/launchctl").args(["bootout", &label]),
                    "stop node service",
                )
                .await?;
            }
            Ok(())
        }
        #[cfg(target_os = "linux")]
        {
            checked(
                Command::new("systemctl").args(["--user", "stop", "vibeface.service"]),
                "stop node service",
            )
            .await
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            bail!("unsupported service platform")
        }
    }

    async fn start(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let label = format!("{}/com.leaperone.vibeface.node", self.domain);
            let loaded = Command::new("/bin/launchctl")
                .args(["print", &label])
                .output()
                .await?;
            if loaded.status.success() {
                checked(
                    Command::new("/bin/launchctl").args(["kickstart", &label]),
                    "start node service",
                )
                .await
            } else {
                checked(
                    Command::new("/bin/launchctl")
                        .args(["bootstrap", &self.domain])
                        .arg(&self.plist),
                    "start node service",
                )
                .await
            }
        }
        #[cfg(target_os = "linux")]
        {
            checked(
                Command::new("systemctl").args(["--user", "start", "vibeface.service"]),
                "start node service",
            )
            .await
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            bail!("unsupported service platform")
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn service_data_matches(args: &[String], data: &Path) -> bool {
    args.get(1).is_some_and(|s| s == "serve")
        && args.iter().filter(|s| *s == "--data-dir").count() == 1
        && args.windows(2).any(|pair| {
            pair[0] == "--data-dir" && fs::canonicalize(&pair[1]).is_ok_and(|actual| actual == data)
        })
}

async fn checked(command: &mut Command, action: &str) -> Result<()> {
    let mut child = command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| action.to_owned())?;
    let status = tokio::time::timeout(Duration::from_secs(120), child.wait())
        .await
        .with_context(|| format!("timed out attempting to {action}"))??;
    ensure!(status.success(), "could not {action}: {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn sandbox() -> PathBuf {
        let directory =
            PathBuf::from("/tmp").join(format!("vibeface-updater-{}", uuid::Uuid::new_v4()));
        config::private_dir(&directory).unwrap();
        fs::canonicalize(directory).unwrap()
    }

    fn package(root: &Path, version: &str) -> PathBuf {
        let directory = root.join("releases").join(version);
        fs::create_dir_all(directory.join("scripts")).unwrap();
        fs::write(directory.join("vibeface"), b"binary").unwrap();
        fs::write(directory.join("scripts/install-node-service.sh"), b"script").unwrap();
        fs::write(
            directory.join("BUILD-INFO.json"),
            format!(r#"{{"version":"{version}","update_protocol":2,"data_epoch":3}}"#),
        )
        .unwrap();
        directory
    }

    #[test]
    fn managed_switch_is_confined_and_preserves_previous_package() {
        let root = sandbox();
        let old = package(&root, "1.0.0");
        let new = package(&root, "1.1.0");
        std::os::unix::fs::symlink(&old, root.join("current")).unwrap();
        assert_eq!(installed(&root).unwrap(), old);
        let data = root.join("data");
        fs::create_dir(&data).unwrap();
        persist(
            &data.join("update.json"),
            &Policy {
                install_root: root.clone(),
                ..Policy::default()
            },
        )
        .unwrap();
        assert_eq!(managed_install_root(&data).unwrap(), root);
        switch(&root, &new).unwrap();
        assert_eq!(installed(&root).unwrap(), new);
        assert!(old.join("vibeface").is_file());
        let outside = sandbox();
        let escaped = package(&outside, "2.0.0");
        assert!(switch(&root, &escaped).is_err());
        assert_eq!(installed(&root).unwrap(), new);
        fs::write(
            old.join("BUILD-INFO.json"),
            r#"{"version":"1.0.0","update_protocol":1,"data_epoch":3}"#,
        )
        .unwrap();
        assert!(manifest(&old).is_err());
        fs::write(old.join("BUILD-INFO.json"), r#"{"version":"1.0.0"}"#).unwrap();
        assert!(manifest(&old).is_err());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn private_backup_keeps_wal_and_links_without_following_them() {
        use std::os::unix::net::UnixListener;
        let root = sandbox();
        let data = root.join("data");
        config::private_dir(&data).unwrap();
        fs::write(data.join("node.sqlite3"), b"db").unwrap();
        fs::write(data.join("node.sqlite3-wal"), b"wal").unwrap();
        std::os::unix::fs::symlink("missing-private-target", data.join("link")).unwrap();
        let _socket = UnixListener::bind(data.join("maintenance.sock")).unwrap();
        let destination = root.join("backup");
        backup(&data, &destination).unwrap();
        assert_eq!(
            fs::read(destination.join("node.sqlite3-wal")).unwrap(),
            b"wal"
        );
        assert_eq!(
            fs::read_link(destination.join("link")).unwrap(),
            Path::new("missing-private-target")
        );
        assert!(!destination.join("maintenance.sock").exists());
        assert_eq!(
            fs::metadata(destination.join("node.sqlite3"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(fs::read(data.join("node.sqlite3")).unwrap(), b"db");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn service_paths_match_only_the_managed_defaults() {
        let root = sandbox();
        let data = root.join("data");
        fs::create_dir(&data).unwrap();
        let args = vec![
            "/private/wrapper".into(),
            "serve".into(),
            "--data-dir".into(),
            data.to_string_lossy().into(),
        ];
        assert!(service_data_matches(&args, &data));
        assert!(!service_data_matches(&args, &root));
        fs::remove_dir_all(root).unwrap();
    }

    struct FakeService {
        candidate: PathBuf,
        failure: &'static str,
        starts: std::sync::atomic::AtomicUsize,
        stops: std::sync::atomic::AtomicUsize,
    }

    impl ServiceControl for FakeService {
        async fn stop(&self) -> Result<()> {
            self.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn start(&self) -> Result<()> {
            let attempt = self
                .starts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ensure!(
                self.failure != "start" || attempt != 0,
                "simulated service start failure"
            );
            Ok(())
        }
        async fn healthy(&self, data: &Path, package: &Path, _: &str) -> Result<()> {
            ensure!(
                data.join("update-pending.json").exists(),
                "admission must remain closed until the program is healthy"
            );
            ensure!(
                self.failure != "health" || package != self.candidate,
                "simulated unhealthy candidate"
            );
            Ok(())
        }
        async fn commit(&self, data: &Path) -> Result<()> {
            let pending = data.join("update-pending.json");
            let journal: Journal = serde_json::from_slice(&fs::read(&pending)?)?;
            ensure!(
                journal.phase == Phase::Confirmed,
                "only a confirmed activation can open admission"
            );
            fs::remove_file(pending)?;
            File::open(data)?.sync_all()?;
            Ok(())
        }
    }

    fn activation_fixture() -> (Policy, PathBuf, Journal) {
        let root = sandbox();
        let data = root.join("data");
        config::private_dir(&data).unwrap();
        config::private_dir(&root.join("backups")).unwrap();
        config::Config::local("127.0.0.1:4790".parse().unwrap(), vec![], false)
            .save(&data)
            .unwrap();
        let identity = crate::crypto::LocalIdentity::generate();
        persist(&data.join("identity.json"), &identity).unwrap();
        fs::write(data.join("auth.sqlite3"), b"account-session-history").unwrap();
        fs::write(data.join("node.sqlite3"), b"operations-and-deduplication").unwrap();
        fs::write(data.join("node.sqlite3-wal"), b"uncheckpointed-events").unwrap();
        let previous = package(&root, "1.0.0");
        let candidate = package(&root, "1.1.0");
        std::os::unix::fs::symlink(&previous, root.join("current")).unwrap();
        let journal = Journal {
            phase: Phase::Prepared,
            previous,
            candidate,
            node_id: identity.node_id().unwrap(),
            backup: root.join("backups/fixture"),
        };
        persist(&data.join("update-pending.json"), &journal).unwrap();
        (
            Policy {
                install_root: root,
                ..Policy::default()
            },
            data,
            journal,
        )
    }

    #[tokio::test]
    async fn activation_failures_restore_program_without_restoring_business_data() {
        for failure in ["backup", "start", "health"] {
            let (policy, data, mut journal) = activation_fixture();
            let filenames = [
                "identity.json",
                "config.json",
                "node.sqlite3",
                "auth.sqlite3",
                "node.sqlite3-wal",
            ];
            let before: Vec<_> = filenames
                .iter()
                .map(|name| fs::read(data.join(name)).unwrap())
                .collect();
            if failure == "backup" {
                fs::create_dir(&journal.backup).unwrap();
            }
            let service = FakeService {
                candidate: journal.candidate.clone(),
                failure,
                starts: std::sync::atomic::AtomicUsize::new(0),
                stops: std::sync::atomic::AtomicUsize::new(0),
            };
            assert!(
                activate(&policy, &data, &service, &mut journal)
                    .await
                    .is_err()
            );
            if failure == "backup" {
                assert_eq!(installed(&policy.install_root).unwrap(), journal.previous);
            }
            recover_with_service(&policy, &data, &service)
                .await
                .unwrap();
            assert_eq!(installed(&policy.install_root).unwrap(), journal.previous);
            assert!(!data.join("update-pending.json").exists());
            for (name, bytes) in filenames.iter().zip(before) {
                assert_eq!(fs::read(data.join(name)).unwrap(), bytes);
            }
            fs::remove_dir_all(policy.install_root).unwrap();
        }
    }

    #[tokio::test]
    async fn interrupted_prepared_activation_rolls_back_but_confirmed_never_does() {
        let (policy, data, mut journal) = activation_fixture();
        let service = FakeService {
            candidate: journal.candidate.clone(),
            failure: "none",
            starts: std::sync::atomic::AtomicUsize::new(0),
            stops: std::sync::atomic::AtomicUsize::new(0),
        };
        let lock = update_lock(&data).unwrap();
        assert!(update_lock(&data).is_err());
        switch(&policy.install_root, &journal.candidate).unwrap();
        recover_with_service(&policy, &data, &service)
            .await
            .unwrap();
        assert_eq!(installed(&policy.install_root).unwrap(), journal.previous);
        journal.phase = Phase::Confirmed;
        persist(&data.join("update-pending.json"), &journal).unwrap();
        recover_with_service(&policy, &data, &service)
            .await
            .unwrap();
        assert_eq!(installed(&policy.install_root).unwrap(), journal.candidate);
        assert!(!data.join("update-pending.json").exists());
        assert_eq!(
            fs::read(data.join("node.sqlite3")).unwrap(),
            b"operations-and-deduplication"
        );
        drop(lock);
        assert!(update_lock(&data).is_ok());
        fs::remove_dir_all(policy.install_root).unwrap();
    }

    #[tokio::test]
    async fn successful_update_backs_up_data_and_confirmed_recovery_does_not_restart() {
        let (policy, data, mut journal) = activation_fixture();
        let service = FakeService {
            candidate: journal.candidate.clone(),
            failure: "none",
            starts: std::sync::atomic::AtomicUsize::new(0),
            stops: std::sync::atomic::AtomicUsize::new(0),
        };
        activate(&policy, &data, &service, &mut journal)
            .await
            .unwrap();
        assert_eq!(installed(&policy.install_root).unwrap(), journal.candidate);
        assert!(!data.join("update-pending.json").exists());
        assert_eq!(
            fs::read(journal.backup.join("node.sqlite3-wal")).unwrap(),
            b"uncheckpointed-events"
        );
        assert_eq!(
            fs::read(journal.backup.join("identity.json")).unwrap(),
            fs::read(data.join("identity.json")).unwrap()
        );
        fs::write(data.join("node.sqlite3"), b"new-user-writes-must-survive").unwrap();
        persist(&data.join("update-pending.json"), &journal).unwrap();
        recover_with_service(&policy, &data, &service)
            .await
            .unwrap();
        assert_eq!(service.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(service.stops.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            fs::read(data.join("node.sqlite3")).unwrap(),
            b"new-user-writes-must-survive"
        );
        fs::remove_dir_all(policy.install_root).unwrap();
    }
}
