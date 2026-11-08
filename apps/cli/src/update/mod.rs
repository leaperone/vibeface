pub mod release;

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Subcommand};
use release::{Channel, PROGRAMS, SERVICE_SCRIPT};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    future::Future,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;
use vibeface_bridge::platform::privatefs;
use vibeface_control::{self, ControlRequest, maintenance};

#[derive(Args)]
pub struct Update {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Select a release channel and install or remove the automatic update timer.
    Configure {
        /// Release channel to follow.
        #[arg(long, value_enum)]
        channel: Channel,
        /// Enable or disable automatic updates.
        #[arg(long, action = clap::ArgAction::Set)]
        automatic: bool,
        /// Root of the managed Vibeface installation.
        #[arg(long)]
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
    /// Node data directory.
    #[arg(long, env = "VIBEFACE_DATA_DIR", value_parser = absolute_path, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    channel: Channel,
    automatic: bool,
    install_root: PathBuf,
}

#[derive(Debug)]
struct Manifest {
    core_version: semver::Version,
    version: semver::Version,
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
    device_id: String,
    backup: PathBuf,
    /// Whether reconciliation may start the service. A stopped update never does.
    resume_service: bool,
}

#[derive(Debug, Deserialize)]
struct InspectResult {
    status: String,
    device_id: String,
    data_dir: PathBuf,
    maintenance_socket: PathBuf,
    listen: SocketAddr,
    version: Option<String>,
    core_version: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MaintenanceHealth {
    device_id: String,
    version: String,
}

fn absolute_path(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("path must be absolute".into())
    }
}

fn default_data_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from(".vibeface"),
        |home| PathBuf::from(home).join(".vibeface"),
    )
}

fn core_binary() -> Result<PathBuf> {
    vibeface_control::sibling_core(&std::env::current_exe()?)
}

async fn inspect(data: &Path, core: &Path) -> Result<InspectResult> {
    let value = vibeface_control::request(
        core,
        &ControlRequest::Inspect {
            data_dir: data.to_owned(),
        },
    )
    .await?;
    let result: InspectResult =
        serde_json::from_value(value).context("invalid core inspect response")?;
    ensure!(
        result.data_dir == data,
        "core inspected a different data directory"
    );
    ensure!(
        !result.device_id.is_empty(),
        "core inspect returned no device identity"
    );
    Ok(result)
}

fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    privatefs::restrict_dir(path)
}

fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    private_dir(path.parent().context("private file has no parent")?)?;
    privatefs::write_atomic(path, bytes)
}

pub(crate) fn lock_data_dir(data: &Path) -> Result<File> {
    let file = privatefs::open_or_create(&data.join("node.lock"))?;
    file.try_lock()
        .context("stop the Node using this data directory before updating")?;
    Ok(file)
}

fn policy(data: &Path) -> Result<Policy> {
    let path = data.join("update.json");
    Ok(serde_json::from_slice(&fs::read(path).context(
        "configure updates with an explicit --install-root before using the updater",
    )?)?)
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

pub fn npm_install_root(executable: &Path) -> Result<Option<PathBuf>> {
    let executable = fs::canonicalize(executable)?;
    let Some(package) = executable.parent() else {
        return Ok(None);
    };
    let Some(vendor) = package
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "vendor"))
    else {
        return Ok(None);
    };
    let root = vendor.parent().context("npm package root is missing")?;
    #[derive(Deserialize)]
    struct NpmPackage {
        name: String,
        version: semver::Version,
    }
    let npm: NpmPackage = serde_json::from_slice(&fs::read(root.join("package.json"))?)
        .context("invalid npm package metadata")?;
    ensure!(
        npm.name == "@leaperone/vibeface"
            && npm.version == manifest(package)?.version
            && root.join("bin/vibeface.mjs").is_file()
            && executable
                .file_name()
                .is_some_and(|name| name == PROGRAMS[0]),
        "npm package does not match the native Vibeface program"
    );
    Ok(Some(root.to_path_buf()))
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
    let _lock = update_lock(&data)?;
    if let Some(root) = npm_install_root(&std::env::current_exe()?)? {
        match options.action {
            Action::Status(_) => {
                println!(
                    "{}",
                    serde_json::json!({"distribution":"npm", "install_root":root,
                    "current_version":env!("CARGO_PKG_VERSION"), "automatic":false,
                    "recovery_required":data.join("update-pending.json").exists()})
                );
                return Ok(());
            }
            Action::Apply {
                automatic: true, ..
            } => return Ok(()),
            _ => bail!(
                "npm owns this installation; wait for idle, stop any node service, then run npm install -g @leaperone/vibeface@nightly and restart the service"
            ),
        }
    }
    match options.action {
        Action::Status(_) => {
            let policy = policy(&data)?;
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
            persist(&data.join("update.json"), &new)?;
            println!("{}", serde_json::to_string(&new)?);
            Ok(())
        }
        Action::Check(_) => {
            let policy = policy(&data)?;
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
            let policy = policy(&data)?;
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
            let inspected = inspect(&data, &core_binary()?).await?;
            prepare_firewall(&inspected, &policy.install_root, &candidate).await?;
            let service = Service::load(&data).await?;
            let lease = match maintenance::acquire(&inspected.maintenance_socket).await {
                Ok(lease) => lease,
                Err(error) if error.downcast_ref::<maintenance::Busy>().is_some() => {
                    println!("Update deferred; the node could not become idle: {error:#}");
                    return Ok(());
                }
                Err(error) => return Err(error.context("request node maintenance")),
            };
            let health: MaintenanceHealth = serde_json::from_value(lease.health.clone())
                .context("maintenance response has invalid health")?;
            ensure!(
                health.version == manifest(&previous)?.core_version.to_string(),
                "running node does not match current package"
            );
            ensure!(
                health.device_id == inspected.device_id,
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
            private_dir(&backups)?;
            let mut journal = Journal {
                phase: Phase::Prepared,
                previous,
                candidate,
                device_id: health.device_id.clone(),
                backup: backups.join(uuid::Uuid::new_v4().to_string()),
                resume_service: true,
            };
            persist(&data.join("update-pending.json"), &journal)?;
            // The open lease closes admission until the service has actually stopped.
            let applied = activate(&policy, &data, &service, &mut journal).await;
            drop(lease);
            if let Err(error) = applied {
                match recover(&policy, &data).await {
                    Ok(()) if installed(&policy.install_root)? == journal.candidate => {
                        maintenance_commit(&data)
                            .await
                            .context("confirmed update must reopen admission")?;
                        healthy(&data, &journal.candidate, &journal.device_id).await?;
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

async fn maintenance_commit(data: &Path) -> Result<()> {
    let inspected = inspect(data, &core_binary()?).await?;
    let response = maintenance::request(
        &inspected.maintenance_socket,
        vibeface_control::maintenance::MaintenanceCommand::Commit,
    )
    .await?;
    ensure!(
        matches!(
            response.status,
            vibeface_control::maintenance::MaintenanceStatus::Committed
        ),
        "node activation was not committed"
    );
    Ok(())
}

pub fn update_lock(data: &Path) -> Result<File> {
    let file = privatefs::open_or_create(&data.join("update.lock"))?;
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
    let build: release::BuildInfo =
        serde_json::from_slice(&fs::read(package.join("BUILD-INFO.json"))?)
            .context("invalid package manifest")?;
    build.validate()?;
    for file in [PROGRAMS[0], PROGRAMS[1], SERVICE_SCRIPT] {
        ensure!(
            fs::symlink_metadata(package.join(file))?
                .file_type()
                .is_file(),
            "package is missing required file {file}"
        );
    }
    for binary in PROGRAMS {
        let path = package.join(binary);
        ensure!(
            privatefs::is_executable(&path),
            "package binary is not a program this account can run: {binary}"
        );
    }
    let web = fs::symlink_metadata(package.join("web"))?;
    ensure!(
        web.is_dir()
            && !web.file_type().is_symlink()
            && fs::read_dir(package.join("web"))?.next().is_some(),
        "package web directory is missing or empty"
    );
    Ok(Manifest {
        core_version: build.core_version.parse()?,
        version: build.version.parse()?,
    })
}

fn installed(root: &Path) -> Result<PathBuf> {
    ensure!(root.is_absolute(), "install-root must be absolute");
    let root = fs::canonicalize(root)?;
    let current = root.join("current");
    ensure!(
        privatefs::is_dir_link(&current)?,
        "managed installation requires a current link"
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
    let desktop = crate::desktop_node::managed_service_for_data(data)?;
    let service_id = desktop.as_ref().map(|service| service.service_id.as_str());
    let mut command = timer_command(policy, data, service_id)?;
    checked(&mut command, "configure automatic update service").await
}

fn timer_command(policy: &Policy, data: &Path, service_id: Option<&str>) -> Result<Command> {
    let current = policy.install_root.join("current");
    let mut command = crate::service::service_script(&current.join(SERVICE_SCRIPT))?;
    if policy.automatic {
        command
            .arg("install-updater")
            .arg("--binary")
            .arg(current.join(PROGRAMS[0]));
    } else {
        command.arg("uninstall-updater");
    }
    command.arg("--data-dir").arg(data);
    if let Some(service_id) = service_id {
        command.arg("--service-id").arg(service_id);
    }
    Ok(command)
}

fn persist(path: &Path, value: &impl Serialize) -> Result<()> {
    private_write(path, &serde_json::to_vec_pretty(value)?)?;
    privatefs::sync_dir(path.parent().context("missing parent directory")?)
}

/// Bind an App-managed Node to the existing updater policy without installing an updater task.
/// A policy already bound to another root is never rewritten or taken over.
pub(crate) fn bind_desktop_install_root(data: &Path, root: &Path) -> Result<()> {
    let root = fs::canonicalize(root)?;
    installed(&root)?;
    let policy_path = data.join("update.json");
    match desktop_saved_policy(data)? {
        Some(existing) => {
            ensure!(
                existing.install_root == root,
                "node updater is already bound to a different installation"
            );
        }
        None => {
            persist(
                &policy_path,
                &Policy {
                    channel: Channel::Nightly,
                    automatic: false,
                    install_root: root,
                },
            )?;
        }
    }
    Ok(())
}

pub(crate) fn desktop_policy_matches(data: &Path, root: &Path) -> Result<bool> {
    let root = fs::canonicalize(root)?;
    let Some(saved) = desktop_saved_policy(data)? else {
        return Ok(false);
    };
    if saved.install_root != root {
        return Ok(false);
    }
    installed(&root)?;
    Ok(true)
}

pub(crate) fn desktop_policy_compatible(data: &Path, root: &Path) -> Result<bool> {
    Ok(desktop_saved_policy(data)?.is_none_or(|saved| saved.install_root == root))
}

pub(crate) fn desktop_policy_uses_root(data: &Path, root: &Path) -> Result<bool> {
    Ok(desktop_saved_policy(data)?.is_some_and(|saved| saved.install_root == root))
}

pub(crate) fn desktop_policy_root(data: &Path) -> Result<Option<PathBuf>> {
    Ok(desktop_saved_policy(data)?.map(|saved| saved.install_root))
}

fn desktop_saved_policy(data: &Path) -> Result<Option<Policy>> {
    let path = data.join("update.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.file_type().is_file()
            && metadata.len() <= 64 * 1024
            && privatefs::is_private(&path)?,
        "unsafe Node updater policy"
    );
    let saved: Policy =
        serde_json::from_slice(&fs::read(path)?).context("invalid update policy")?;
    Ok(Some(saved))
}

pub(crate) fn validate_desktop_package(package: &Path) -> Result<()> {
    manifest(package).map(|_| ())
}

pub(crate) fn package_release_version(package: &Path) -> Result<String> {
    Ok(manifest(package)?.version.to_string())
}

pub(crate) fn switch_managed_current(managed_root: &Path, release: &Path) -> Result<()> {
    switch(managed_root, release)
}

fn switch(root: &Path, package: &Path) -> Result<()> {
    let root = fs::canonicalize(root)?;
    let package = fs::canonicalize(package)?;
    ensure!(
        package.starts_with(&root) && package != root,
        "package is outside installation"
    );
    manifest(&package)?;
    privatefs::point_dir_link(&package, &root.join("current"))
}

/// Stop, switch, start, and prove health. The journal stays prepared until `acknowledge`.
async fn handoff(
    policy: &Policy,
    data: &Path,
    service: &impl ServiceControl,
    journal: &Journal,
) -> Result<()> {
    service.stop().await?;
    let lock = wait_for_node_lock(data).await?;
    backup(data, &journal.backup)?;
    switch(&policy.install_root, &journal.candidate)?;
    drop(lock);
    service.start().await?;
    service
        .healthy(data, &journal.candidate, &journal.device_id)
        .await?;
    Ok(())
}

/// Confirm and open admission only after the caller has proved the final state.
async fn acknowledge(
    data: &Path,
    service: &impl ServiceControl,
    journal: &mut Journal,
) -> Result<()> {
    journal.phase = Phase::Confirmed;
    persist(&data.join("update-pending.json"), journal)?;
    service.commit(data).await?;
    Ok(())
}

async fn activate(
    policy: &Policy,
    data: &Path,
    service: &impl ServiceControl,
    journal: &mut Journal,
) -> Result<()> {
    handoff(policy, data, service, journal).await?;
    acknowledge(data, service, journal).await?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum RecoveryMode {
    /// Ordinary CLI recovery follows `resume_service` in the journal.
    ResumeJournal,
    /// Desktop explicit recovery never starts or stops the service.
    KeepStopped,
}

async fn recover(policy: &Policy, data: &Path) -> Result<()> {
    if !data.join("update-pending.json").exists() {
        return Ok(());
    }
    let service = Service::load(data).await?;
    recover_with_service(policy, data, &service, RecoveryMode::ResumeJournal).await
}

async fn recover_with_service(
    policy: &Policy,
    data: &Path,
    service: &impl ServiceControl,
    mode: RecoveryMode,
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
        journal.device_id == service.device_id(data).await?,
        "update journal belongs to another node"
    );
    let confirmed = journal.phase == Phase::Confirmed;
    let wanted = if confirmed {
        journal.candidate.clone()
    } else {
        journal.previous.clone()
    };
    let keep_stopped = matches!(mode, RecoveryMode::KeepStopped) || !journal.resume_service;
    if keep_stopped {
        let _node_lock =
            lock_data_dir(data).context("stop the Node before recovering a stopped update")?;
        if installed(&root)? != wanted {
            switch(&root, &wanted)?;
        }
        ensure!(
            installed(&root)? == wanted,
            "restored program does not match the journal"
        );
        fs::remove_file(&pending)?;
        privatefs::sync_dir(data)?;
        eprintln!(
            "Recovered {} program; business data was kept.",
            if confirmed { "confirmed" } else { "previous" }
        );
        return Ok(());
    }
    if confirmed
        && installed(&root)? == wanted
        && service
            .healthy(data, &wanted, &journal.device_id)
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
    service.healthy(data, &wanted, &journal.device_id).await?;
    journal.candidate = wanted;
    journal.phase = Phase::Confirmed;
    persist(&pending, &journal)?;
    service.commit(data).await?;
    eprintln!(
        "Recovered {} program; business data was kept.",
        if confirmed { "confirmed" } else { "previous" }
    );
    Ok(())
}

fn desktop_managed_policy(managed_root: &Path) -> Result<Policy> {
    Ok(Policy {
        channel: Channel::Nightly,
        automatic: false,
        install_root: fs::canonicalize(managed_root)?,
    })
}

/// Reconcile an interrupted App-managed release. Status must not call this.
/// Desktop recovery runs only while Core reports a stopped Node.
pub(crate) async fn recover_desktop_managed(data: &Path, managed_root: &Path) -> Result<()> {
    let pending = data.join("update-pending.json");
    if !pending.exists() {
        return Ok(());
    }
    let policy = desktop_managed_policy(managed_root)?;
    let metadata = fs::symlink_metadata(&pending)?;
    ensure!(
        metadata.is_file() && metadata.len() <= 64 * 1024,
        "invalid pending update journal"
    );
    let journal: Journal = serde_json::from_slice(&fs::read(&pending)?)?;
    let current = installed(&policy.install_root)?;
    let inspected = inspect(data, &current.join(PROGRAMS[1])).await?;
    ensure!(
        inspected.status != "running",
        "desktop_node_recovery_required"
    );
    ensure!(
        inspected.device_id == journal.device_id,
        "update journal belongs to another node"
    );
    let service = Service::load(data).await?;
    recover_with_service(&policy, data, &service, RecoveryMode::KeepStopped).await
}

pub(crate) struct DesktopManagedRelease<'a> {
    pub managed_root: &'a Path,
    pub previous: PathBuf,
    pub candidate: PathBuf,
    pub device_id: String,
    pub resume_service: bool,
    /// The installed service program is already the managed `current` link.
    pub tracks_current: bool,
}

const DESKTOP_ACTIVATION_BUDGET: Duration = Duration::from_secs(60);

enum Admission {
    Idle,
    Busy,
}

/// The only writer of a prepared desktop journal. Busy admission leaves the data directory unchanged.
fn persist_after_admission(data: &Path, journal: &Journal, admission: Admission) -> Result<()> {
    match admission {
        Admission::Busy => bail!("update deferred; the node could not become idle"),
        Admission::Idle => persist(&data.join("update-pending.json"), journal)?,
    }
    Ok(())
}

fn discard_unswitched_prepare(data: &Path, root: &Path, previous: &Path) -> Result<()> {
    if installed(root)? == previous {
        let pending = data.join("update-pending.json");
        if pending.exists() {
            fs::remove_file(&pending)?;
            privatefs::sync_dir(data)?;
        }
    }
    Ok(())
}

fn prepare_backup_dir(backups: &Path) -> Result<()> {
    if !backups.exists() {
        fs::create_dir(backups)?;
    }
    ensure!(
        fs::symlink_metadata(backups)?.is_dir(),
        "backup directory must not be a symlink"
    );
    private_dir(backups)?;
    Ok(())
}

async fn switch_stopped(policy: &Policy, data: &Path, journal: &Journal) -> Result<File> {
    let lock = lock_data_dir(data).context("stop the Node before updating")?;
    backup(data, &journal.backup)?;
    switch(&policy.install_root, &journal.candidate)?;
    Ok(lock)
}

fn restore_stopped(policy: &Policy, journal: &Journal) -> Result<()> {
    if installed(&policy.install_root)? != journal.previous {
        switch(&policy.install_root, &journal.previous)?;
    }
    ensure!(
        installed(&policy.install_root)? == journal.previous,
        "restored program does not match the journal"
    );
    Ok(())
}

async fn reconcile_running(
    policy: &Policy,
    data: &Path,
    service: &impl ServiceControl,
    error: anyhow::Error,
) -> Result<()> {
    match recover_with_service(policy, data, service, RecoveryMode::ResumeJournal).await {
        Ok(()) => {
            bail!("update failed; previous program restored, data was kept: {error:#}")
        }
        Err(rollback) => bail!(
            "update failed: {error:#}; recovery also failed: {rollback:#}; keep update-pending.json and run update again"
        ),
    }
}

/// Drop a confirmed App-managed journal after the service registration reflects the new package.
pub(crate) fn complete_desktop_managed_release(data: &Path) -> Result<()> {
    let pending = data.join("update-pending.json");
    if !pending.exists() {
        return Ok(());
    }
    let journal: Journal = serde_json::from_slice(&fs::read(&pending)?)?;
    ensure!(
        journal.phase == Phase::Confirmed,
        "managed release was not confirmed"
    );
    fs::remove_file(pending)?;
    privatefs::sync_dir(data)?;
    Ok(())
}

/// Switch an App-managed runtime. `prove` runs before the journal is cleared.
/// A stopped release is refused when the service is not already registered to `current`.
pub(crate) async fn apply_desktop_managed_release<F, Fut>(
    data: &Path,
    release: DesktopManagedRelease<'_>,
    prove: F,
) -> Result<()>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    ensure!(
        release.tracks_current,
        "stopped node service is not registered to the current program"
    );
    let policy = desktop_managed_policy(release.managed_root)?;
    if data.join("update-pending.json").exists() && release.resume_service {
        let inspected = inspect(data, &release.previous.join(PROGRAMS[1])).await?;
        ensure!(
            inspected.status != "running",
            "desktop_node_recovery_required"
        );
    }
    recover_desktop_managed(data, release.managed_root).await?;
    let root = fs::canonicalize(release.managed_root)?;
    ensure!(
        !root.starts_with(data),
        "managed runtime must not live inside node data"
    );
    manifest(&release.previous)?;
    manifest(&release.candidate)?;
    let backups = root.join("backups");
    let mut journal = Journal {
        phase: Phase::Prepared,
        previous: release.previous.clone(),
        candidate: release.candidate.clone(),
        device_id: release.device_id.clone(),
        backup: backups.join(uuid::Uuid::new_v4().to_string()),
        resume_service: release.resume_service,
    };
    if release.resume_service {
        let inspected = inspect(data, &release.previous.join(PROGRAMS[1])).await?;
        ensure!(
            inspected.device_id == release.device_id,
            "running node identity changed"
        );
        ensure!(
            inspected.status == "running",
            "registered service and Core health disagree"
        );
        let lease = match maintenance::acquire(&inspected.maintenance_socket).await {
            Ok(lease) => lease,
            Err(error) if error.downcast_ref::<maintenance::Busy>().is_some() => {
                return persist_after_admission(data, &journal, Admission::Busy);
            }
            Err(error) => return Err(error.context("request node maintenance")),
        };
        let health: MaintenanceHealth = serde_json::from_value(lease.health.clone())
            .context("maintenance response has invalid health")?;
        ensure!(
            health.version == manifest(&release.previous)?.core_version.to_string(),
            "running node does not match current package"
        );
        ensure!(
            health.device_id == release.device_id,
            "running node identity changed"
        );
        prepare_backup_dir(&backups)?;
        if let Err(error) = persist_after_admission(data, &journal, Admission::Idle) {
            drop(lease);
            return Err(error);
        }
        let service = match Service::load(data).await {
            Ok(service) => service,
            Err(error) => {
                drop(lease);
                discard_unswitched_prepare(data, &root, &journal.previous)?;
                return Err(error);
            }
        };
        let handed = match tokio::time::timeout(
            DESKTOP_ACTIVATION_BUDGET,
            handoff(&policy, data, &service, &journal),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "desktop node update deadline expired before activation finished"
            )),
        };
        if let Err(error) = handed {
            if error.to_string().contains("deadline expired") {
                return Err(error);
            }
            return reconcile_running(&policy, data, &service, error).await;
        }
        if let Err(error) = prove(journal.candidate.clone()).await {
            return reconcile_running(&policy, data, &service, error).await;
        }
        acknowledge(data, &service, &mut journal).await?;
        drop(lease);
        return Ok(());
    }
    prepare_backup_dir(&backups)?;
    persist_after_admission(data, &journal, Admission::Idle)?;
    let _node_lock = match switch_stopped(&policy, data, &journal).await {
        Ok(lock) => lock,
        Err(error) => {
            discard_unswitched_prepare(data, &root, &journal.previous)?;
            return Err(error);
        }
    };
    if let Err(error) = prove(journal.candidate.clone()).await {
        if let Err(rollback) = restore_stopped(&policy, &journal) {
            return Err(error.context(rollback));
        }
        let pending = data.join("update-pending.json");
        if pending.exists() {
            fs::remove_file(pending)?;
            privatefs::sync_dir(data)?;
        }
        return Err(error);
    }
    journal.phase = Phase::Confirmed;
    persist(&data.join("update-pending.json"), &journal)?;
    complete_desktop_managed_release(data)?;
    Ok(())
}

async fn wait_for_node_lock(data: &Path) -> Result<File> {
    for _ in 0..150 {
        if let Ok(lock) = lock_data_dir(data) {
            return Ok(lock);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("node did not release its data lock after stopping")
}

async fn healthy(data: &Path, package: &Path, device_id: &str) -> Result<()> {
    let version = manifest(package)?.core_version.to_string();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(1))
        .build()?;
    let mut last = String::new();
    for _ in 0..150 {
        match inspect(data, &package.join(PROGRAMS[1])).await {
            Ok(inspected) => {
                let running_version = inspected
                    .core_version
                    .as_deref()
                    .or(inspected.version.as_deref());
                if inspected.status != "running" {
                    last = inspected
                        .error
                        .unwrap_or_else(|| "node is not running".into());
                } else if inspected.device_id != device_id
                    || running_version != Some(version.as_str())
                {
                    bail!("started node has an unexpected version or identity");
                } else {
                    let health = maintenance::request(
                        &inspected.maintenance_socket,
                        vibeface_control::maintenance::MaintenanceCommand::Probe,
                    )
                    .await
                    .and_then(|response| {
                        ensure!(
                            matches!(
                                response.status,
                                vibeface_control::maintenance::MaintenanceStatus::Healthy
                            ),
                            "node did not report healthy"
                        );
                        let health: MaintenanceHealth = serde_json::from_value(
                            response
                                .health
                                .context("maintenance response has no health")?,
                        )
                        .context("maintenance response has invalid health")?;
                        Ok(health)
                    });
                    match health {
                        Ok(health)
                            if health.device_id == device_id && health.version == version =>
                        {
                            let mut listen = inspected.listen;
                            if listen.ip().is_unspecified() {
                                listen.set_ip(if listen.is_ipv6() {
                                    Ipv6Addr::LOCALHOST.into()
                                } else {
                                    Ipv4Addr::LOCALHOST.into()
                                });
                            }
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
                            match client
                                .get(&url)
                                .header(reqwest::header::HOST, &host)
                                .send()
                                .await
                            {
                                Ok(response) if response.status().is_success() => {
                                    if let Ok(info) = response.json::<serde_json::Value>().await {
                                        ensure!(
                                            info.get("device_id")
                                                .and_then(serde_json::Value::as_str)
                                                == Some(device_id)
                                                && info["version"] == version,
                                            "HTTP listener belongs to a different node or version"
                                        );
                                        return Ok(());
                                    }
                                    last = "invalid local HTTP health response".into();
                                }
                                Ok(response) => {
                                    last = format!(
                                        "local HTTP listener returned {}",
                                        response.status()
                                    )
                                }
                                Err(error) => last = error.to_string(),
                            }
                        }
                        Ok(_) => bail!("started node has an unexpected version or identity"),
                        Err(error) => last = error.to_string(),
                    }
                }
            }
            Err(error) => last = error.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    bail!("node failed its private health check: {last}")
}

fn backup(source: &Path, destination: &Path) -> Result<()> {
    privatefs::create_dir_new(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let target = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&path)?;
        // A local endpoint of a running Node holds no data to copy. Only Unix gives one a
        // directory entry; a Windows pipe has no name in the file system.
        #[cfg(unix)]
        if std::os::unix::fs::FileTypeExt::is_socket(&metadata.file_type()) {
            continue;
        }
        if metadata.is_symlink() {
            privatefs::copy_link(&path, &target)?;
        } else if metadata.is_dir() {
            backup(&path, &target)?;
        } else {
            ensure!(
                privatefs::is_plain_file(&metadata),
                "unsupported data file: {}",
                path.display()
            );
            fs::copy(&path, &target)?;
            privatefs::restrict_file(&target)?;
            fs::OpenOptions::new()
                .write(true)
                .open(target)?
                .sync_all()?;
        }
    }
    privatefs::sync_dir(destination)
}

async fn prepare_firewall(inspected: &InspectResult, root: &Path, candidate: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if inspected.listen.ip().is_loopback() {
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
        for binary in [
            candidate.join("vibeface"),
            candidate.join("vibeface-core"),
            root.join("current/vibeface"),
            root.join("current/vibeface-core"),
        ] {
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
    let _ = (inspected, root, candidate);
    Ok(())
}

struct Service {
    desktop: Option<crate::desktop_node::ManagedService>,
    #[cfg(target_os = "macos")]
    domain: String,
    #[cfg(target_os = "macos")]
    plist: PathBuf,
}

/// The scheduled task this account runs the Node under on Windows.
#[cfg(windows)]
pub(crate) const TASK: &str = "\\Vibeface\\Node";

/// The task scheduler of this installation of Windows.
#[cfg(windows)]
pub(crate) fn schtasks() -> Result<PathBuf> {
    let root = std::env::var_os("SystemRoot")
        .context("SystemRoot is required to reach the task scheduler")?;
    let path = PathBuf::from(root).join("System32\\schtasks.exe");
    ensure!(
        path.is_file(),
        "the task scheduler is required to manage this account's Node: {}",
        path.display()
    );
    Ok(path)
}

trait ServiceControl {
    async fn stop(&self) -> Result<()>;
    async fn start(&self) -> Result<()>;
    async fn device_id(&self, data: &Path) -> Result<String> {
        Ok(inspect(data, &core_binary()?).await?.device_id)
    }
    async fn healthy(&self, data: &Path, package: &Path, device_id: &str) -> Result<()> {
        healthy(data, package, device_id).await
    }
    async fn commit(&self, data: &Path) -> Result<()> {
        maintenance_commit(data).await
    }
}

impl Service {
    async fn load(data: &Path) -> Result<Self> {
        if let Some(desktop) = crate::desktop_node::managed_service_for_data(data)? {
            let binary = core_binary()?;
            ensure!(
                inspect(data, &binary).await?.device_id == desktop.device_id,
                "Desktop Node identity changed since service installation"
            );
            ensure!(
                crate::desktop_node::managed_service_registered(&desktop).await?,
                "Desktop Node service registration is missing"
            );
            #[cfg(target_os = "macos")]
            return Ok(Self {
                desktop: Some(desktop),
                domain: String::new(),
                plist: PathBuf::new(),
            });
            #[cfg(not(target_os = "macos"))]
            return Ok(Self {
                desktop: Some(desktop),
            });
        }
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
                desktop: None,
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
            Ok(Self { desktop: None })
        }
        #[cfg(windows)]
        {
            let output = Command::new(schtasks()?)
                .args(["/Query", "/TN", TASK, "/XML", "ONE"])
                .output()
                .await?;
            ensure!(
                output.status.success(),
                "installed node task of this account is required"
            );
            ensure!(
                task_data_matches(&task_command_line(&output.stdout)?, data),
                "installed node service uses a different data directory"
            );
            Ok(Self { desktop: None })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        bail!(
            "automatic updates require macOS launchd, Linux systemd user services, or the Windows task scheduler"
        )
    }
}

/// The whole command line a task definition runs, read out of the definition the task
/// scheduler printed.
///
/// The scheduler prints a definition as XML, which it writes the same way whatever language
/// Windows is displaying, unlike its readable listing.
#[cfg(any(windows, test))]
fn task_command_line(definition: &[u8]) -> Result<String> {
    let text = if definition.len() >= 2 && definition[..2] == [0xff, 0xfe] {
        let units: Vec<u16> = definition[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16(&units).context("the task scheduler printed an unreadable definition")?
    } else {
        String::from_utf8(definition.to_vec())
            .context("the task scheduler printed an unreadable definition")?
    };
    let mut line = String::new();
    for tag in ["Command", "Arguments"] {
        if let Some(value) = between(&text, &format!("<{tag}>"), &format!("</{tag}>")) {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&unescape(value));
        }
    }
    ensure!(!line.is_empty(), "the installed node task runs no program");
    Ok(line)
}

/// The text between the first `open` and the `close` that follows it.
#[cfg(any(windows, test))]
fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let rest = &text[text.find(open)? + open.len()..];
    Some(&rest[..rest.find(close)?])
}

/// XML text as the characters it stands for.
#[cfg(any(windows, test))]
fn unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Whether `line` starts the Node on `data` and on nothing else.
#[cfg(any(windows, test))]
fn task_data_matches(line: &str, data: &Path) -> bool {
    let quoted = format!("--data-dir \"{}\"", data.display());
    line.matches("--data-dir").count() == 1
        && line.ends_with(&quoted)
        && line[..line.len() - quoted.len()]
            .split_whitespace()
            .any(|word| word == "serve")
}

impl ServiceControl for Service {
    async fn stop(&self) -> Result<()> {
        if let Some(desktop) = &self.desktop {
            let current = crate::desktop_node::managed_service_for_data(&desktop.data_dir)?
                .context("Desktop Node service registration is missing")?;
            return crate::desktop_node::run_managed_service(&current, "stop").await;
        }
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
        #[cfg(windows)]
        {
            // Ending a task that is not running is reported as a failure, so the state is read
            // first and a Node that already stopped is left alone.
            if task_is_running().await? {
                checked(
                    Command::new(schtasks()?).args(["/End", "/TN", TASK]),
                    "stop node service",
                )
                .await?;
            }
            Ok(())
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        {
            bail!("unsupported service platform")
        }
    }

    async fn start(&self) -> Result<()> {
        if let Some(desktop) = &self.desktop {
            let current = crate::desktop_node::managed_service_for_data(&desktop.data_dir)?
                .context("Desktop Node service registration is missing")?;
            return crate::desktop_node::run_managed_service(&current, "start").await;
        }
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
        #[cfg(windows)]
        {
            if task_is_running().await? {
                return Ok(());
            }
            checked(
                Command::new(schtasks()?).args(["/Run", "/TN", TASK]),
                "start node service",
            )
            .await
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        {
            bail!("unsupported service platform")
        }
    }
}

/// Whether this account's Node task has a run in progress.
#[cfg(windows)]
async fn task_is_running() -> Result<bool> {
    let root =
        std::env::var_os("SystemRoot").context("SystemRoot is required to query the node task")?;
    let shell = PathBuf::from(root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let output = Command::new(shell)
        .args(["-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference='Stop'; $scheduler=New-Object -ComObject 'Schedule.Service'; $scheduler.Connect(); [int]$scheduler.GetFolder('\\').GetTask($env:VIBEFACE_NODE_TASK).State"])
        .env("VIBEFACE_NODE_TASK", TASK)
        .output().await?;
    ensure!(
        output.status.success(),
        "could not query the installed node task of this account"
    );
    let state = String::from_utf8(output.stdout)?
        .trim()
        .parse::<u32>()
        .context("invalid scheduled task state")?;
    ensure!(state <= 4, "unknown scheduled task state");
    Ok(state == 4)
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

    /// Marks `path` as a program this account can run, where that is something a file carries.
    fn mark_runnable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Windows carries the right to run in the name, which the fixtures already use.
        #[cfg(windows)]
        let _ = path;
        assert!(privatefs::is_executable(path));
    }

    /// Writes a fixture service script that records the arguments it was handed.
    fn recording_service_script(package: &Path) -> PathBuf {
        let script = package.join(SERVICE_SCRIPT);
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        #[cfg(unix)]
        {
            fs::write(&script, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\n").unwrap();
            mark_runnable(&script);
        }
        #[cfg(windows)]
        fs::write(
            &script,
            "$args | Set-Content -LiteralPath \"$PSCommandPath.args\"\r\n",
        )
        .unwrap();
        script
    }

    /// What the script `recording_service_script` wrote was handed.
    fn recorded_arguments(script: &Path) -> String {
        let mut name = script.file_name().unwrap().to_owned();
        name.push(".args");
        fs::read_to_string(script.with_file_name(name))
            .unwrap()
            .replace("\r\n", "\n")
    }

    #[test]
    fn updater_timer_command_uses_desktop_service_identity_when_present() {
        let root = sandbox();
        let data = root.join("data");
        private_dir(&data).unwrap();
        let package = package(&root, "1.0.0");
        privatefs::link_dir(&package, &root.join("current")).unwrap();
        let policy = Policy {
            channel: Channel::Nightly,
            automatic: true,
            install_root: root.clone(),
        };
        let command = timer_command(&policy, &data, Some("desktop-a1b2")).unwrap();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--service-id", "desktop-a1b2"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--data-dir", data.to_string_lossy().as_ref()])
        );
        let legacy = timer_command(&policy, &data, None).unwrap();
        assert!(!legacy.as_std().get_args().any(|arg| arg == "--service-id"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn device_id_journal_uses_only_the_current_field() -> Result<()> {
        let wire = r#"{"phase":"prepared","previous":"old","candidate":"new","device_id":"device","backup":"backup","resume_service":true}"#;
        let journal: Journal = serde_json::from_str(wire)?;
        assert_eq!(journal.device_id, "device");
        assert_eq!(serde_json::to_string(&journal)?, wire);
        assert!(serde_json::from_str::<Journal>(&wire.replace("device_id", "node_id")).is_err());
        Ok(())
    }

    fn sandbox() -> PathBuf {
        // Unix keeps the parent short because a local endpoint a test binds inside it has a
        // length limit of its own.
        #[cfg(unix)]
        let parent = PathBuf::from("/tmp");
        #[cfg(windows)]
        let parent = std::env::temp_dir();
        let directory = parent.join(format!("vibeface-updater-{}", uuid::Uuid::new_v4()));
        private_dir(&directory).unwrap();
        fs::canonicalize(directory).unwrap()
    }

    // A fixture program here is a shell script, which only Unix can run. Windows runs the
    // real programs through `scripts/verify-windows-node.py` instead.
    #[cfg(unix)]
    #[tokio::test]
    async fn update_health_inspects_the_candidate_core() -> Result<()> {
        let root = sandbox();
        let candidate = package(&root, "9.0.0");
        let data = root.join("data");
        let marker = root.join("candidate-inspected");
        let response = serde_json::json!({"ok":true,"result":{
            "status":"running","device_id":"unexpected","data_dir":data,
            "maintenance_socket":root.join("socket"),"listen":"127.0.0.1:4790",
            "version":"9.0.0","core_version":"9.0.0"
        }});
        let core = candidate.join(PROGRAMS[1]);
        fs::write(
            &core,
            format!(
                "#!/bin/sh\ncat >/dev/null\ntouch '{}'\nprintf '%s\\n' '{}'\n",
                marker.display(),
                response
            ),
        )?;
        mark_runnable(&core);
        let error = healthy(&data, &candidate, "expected").await.unwrap_err();
        assert!(marker.exists(), "health must execute the candidate Core");
        assert!(error.to_string().contains("unexpected version or identity"));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    fn package(root: &Path, version: &str) -> PathBuf {
        let directory = root.join("releases").join(version);
        fs::create_dir_all(directory.join("scripts")).unwrap();
        fs::write(directory.join(PROGRAMS[0]), b"binary").unwrap();
        fs::write(directory.join(PROGRAMS[1]), b"core").unwrap();
        fs::create_dir(directory.join("web")).unwrap();
        fs::write(directory.join("web/index.html"), b"web").unwrap();
        fs::write(directory.join(SERVICE_SCRIPT), b"script").unwrap();
        mark_runnable(&directory.join(PROGRAMS[0]));
        mark_runnable(&directory.join(PROGRAMS[1]));
        fs::write(
            directory.join("BUILD-INFO.json"),
            serde_json::to_vec(&build_info(version)).unwrap(),
        )
        .unwrap();
        directory
    }

    fn build_info(version: &str) -> serde_json::Value {
        serde_json::json!({
            "version": version, "target": if cfg!(windows) { "x86_64-pc-windows-msvc" } else { "aarch64-apple-darwin" }, "source_commit": "a".repeat(40),
            "core_version": version, "core_source_commit": "a".repeat(40),
            "app_version": version, "app_source_commit": "a".repeat(40),
            "update_protocol": vibeface_control::UPDATE_PROTOCOL,
            "data_epoch": vibeface_control::DATA_EPOCH,
        })
    }

    #[tokio::test]
    async fn first_update_configuration_creates_an_explicit_policy() -> Result<()> {
        let root = sandbox();
        let data = root.join("data");
        private_dir(&data)?;
        fs::write(data.join("identity.json"), b"opaque identity fixture")?;
        let package = package(&root, "1.0.0");
        privatefs::link_dir(&package, &root.join("current"))?;
        let installer = recording_service_script(&package);

        assert!(
            run(Update {
                action: Action::Status(Data {
                    data_dir: data.clone()
                }),
            })
            .await
            .is_err()
        );
        run(Update {
            action: Action::Configure {
                channel: Channel::Nightly,
                automatic: false,
                install_root: root.clone(),
                data: Data {
                    data_dir: data.clone(),
                },
            },
        })
        .await?;
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(data.join("update.json"))?)?;
        assert_eq!(
            saved,
            serde_json::json!({"channel":"nightly","automatic":false,"install_root":root})
        );
        assert_eq!(
            recorded_arguments(&installer),
            format!("uninstall-updater\n--data-dir\n{}\n", data.display())
        );
        run(Update {
            action: Action::Configure {
                channel: Channel::Nightly,
                automatic: true,
                install_root: root.clone(),
                data: Data {
                    data_dir: data.clone(),
                },
            },
        })
        .await?;
        assert_eq!(
            recorded_arguments(&installer),
            format!(
                "install-updater\n--binary\n{}\n--data-dir\n{}\n",
                root.join("current").join(PROGRAMS[0]).display(),
                data.display()
            )
        );
        run(Update {
            action: Action::Status(Data { data_dir: data }),
        })
        .await?;
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn npm_installation_requires_matching_package_metadata() {
        let root = sandbox();
        let original = package(&root, "1.2.3-nightly.20261019.1");
        assert_eq!(npm_install_root(&original.join(PROGRAMS[0])).unwrap(), None);
        fs::create_dir_all(root.join("vendor")).unwrap();
        let target = if cfg!(windows) {
            "x86_64-pc-windows-msvc"
        } else {
            "aarch64-apple-darwin"
        };
        let native = root.join("vendor").join(target);
        fs::rename(original, &native).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::write(root.join("bin/vibeface.mjs"), b"launcher").unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"name":"@leaperone/vibeface","version":"1.2.3-nightly.20261019.1"}"#,
        )
        .unwrap();
        assert_eq!(
            npm_install_root(&native.join(PROGRAMS[0])).unwrap(),
            Some(root.clone())
        );
        for metadata in [
            r#"{"name":"another-package","version":"1.2.3-nightly.20261019.1"}"#,
            r#"{"name":"vibeface","version":"1.2.3-nightly.20261019.1"}"#,
            r#"{"name":"@leaperone/vibeface","version":"2.0.0"}"#,
            r#"{"name":"@leaperone/vibeface"}"#,
        ] {
            fs::write(root.join("package.json"), metadata).unwrap();
            assert!(npm_install_root(&native.join(PROGRAMS[0])).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_switch_is_confined_and_preserves_previous_package() {
        let root = sandbox();
        let old = package(&root, "1.0.0");
        let new = package(&root, "1.1.0");
        privatefs::link_dir(&old, &root.join("current")).unwrap();
        assert_eq!(installed(&root).unwrap(), old);
        let data = root.join("data");
        fs::create_dir(&data).unwrap();
        persist(
            &data.join("update.json"),
            &Policy {
                install_root: root.clone(),
                channel: Channel::Stable,
                automatic: false,
            },
        )
        .unwrap();
        assert_eq!(managed_install_root(&data).unwrap(), root);
        switch(&root, &new).unwrap();
        assert_eq!(installed(&root).unwrap(), new);
        assert!(old.join(PROGRAMS[0]).is_file());
        let outside = sandbox();
        let escaped = package(&outside, "2.0.0");
        assert!(switch(&root, &escaped).is_err());
        assert_eq!(installed(&root).unwrap(), new);
        for field in ["data_epoch", "update_protocol"] {
            for value in [0, 2, 12] {
                let mut build = build_info("1.0.0");
                build[field] = serde_json::json!(value);
                fs::write(
                    old.join("BUILD-INFO.json"),
                    serde_json::to_vec(&build).unwrap(),
                )
                .unwrap();
                assert!(
                    manifest(&old)
                        .unwrap_err()
                        .to_string()
                        .contains("current update protocol and data epoch")
                );
            }
        }
        let mut build = build_info("1.0.0");
        build["legacy"] = serde_json::json!(true);
        fs::write(
            old.join("BUILD-INFO.json"),
            serde_json::to_vec(&build).unwrap(),
        )
        .unwrap();
        assert!(manifest(&old).is_err());
        fs::write(old.join("BUILD-INFO.json"), r#"{"version":"1.0.0"}"#).unwrap();
        assert!(manifest(&old).is_err());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_backup_keeps_wal_and_links_without_following_them() {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        let root = sandbox();
        let data = root.join("data");
        private_dir(&data).unwrap();
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

    // Windows has no socket file to skip and no link a copy may follow except a directory
    // link, so the same guarantees are checked against what it does have.
    #[cfg(windows)]
    #[test]
    fn private_backup_keeps_wal_and_links_without_following_them() {
        let root = sandbox();
        let data = root.join("data");
        private_dir(&data).unwrap();
        fs::write(data.join("node.sqlite3"), b"db").unwrap();
        fs::write(data.join("node.sqlite3-wal"), b"wal").unwrap();
        let elsewhere = root.join("elsewhere");
        private_dir(&elsewhere).unwrap();
        fs::write(elsewhere.join("not-business-data"), b"outside").unwrap();
        privatefs::link_dir(&elsewhere, &data.join("link")).unwrap();
        let destination = root.join("backup");
        backup(&data, &destination).unwrap();
        assert_eq!(
            fs::read(destination.join("node.sqlite3-wal")).unwrap(),
            b"wal"
        );
        assert_eq!(
            privatefs::read_dir_link(&destination.join("link")).unwrap(),
            elsewhere
        );
        assert!(
            !destination.join("link/not-business-data").is_file()
                || privatefs::is_dir_link(&destination.join("link")).unwrap(),
            "a copy must carry the link across instead of reading through it"
        );
        privatefs::ensure_private(&destination.join("node.sqlite3"), "the copied database")
            .unwrap();
        assert_eq!(fs::read(data.join("node.sqlite3")).unwrap(), b"db");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn task_definitions_match_only_the_managed_data_dir() {
        let definition = |command: &str, arguments: &str| {
            format!(
                "<Task><Actions><Exec><Command>{command}</Command>\
                 <Arguments>{arguments}</Arguments></Exec></Actions></Task>"
            )
        };
        let data = Path::new("C:\\Users\\someone\\My Node");
        let line = task_command_line(
            definition(
                "C:\\Program Files\\Vibeface\\vibeface.exe",
                "serve --data-dir &quot;C:\\Users\\someone\\My Node&quot;",
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(task_data_matches(&line, data));
        assert!(!task_data_matches(&line, Path::new("C:\\Users\\someone")));
        let twice = task_command_line(
            definition(
                "C:\\Program Files\\Vibeface\\vibeface.exe",
                "serve --data-dir &quot;C:\\elsewhere&quot; --data-dir &quot;C:\\Users\\someone\\My Node&quot;",
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(!task_data_matches(&twice, data));
        let other_verb = task_command_line(
            definition(
                "C:\\Program Files\\Vibeface\\vibeface.exe",
                "update apply --data-dir &quot;C:\\Users\\someone\\My Node&quot;",
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(!task_data_matches(&other_verb, data));
        let mut utf16: Vec<u8> = vec![0xff, 0xfe];
        for unit in definition(
            "vibeface.exe",
            "serve --data-dir &quot;C:\\Users\\someone\\My Node&quot;",
        )
        .encode_utf16()
        {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        assert!(task_data_matches(&task_command_line(&utf16).unwrap(), data));
        assert!(task_command_line(b"<Task></Task>").is_err());
    }

    #[test]
    fn service_paths_match_only_the_managed_data_dir() {
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
            ensure!(self.failure != "stop", "simulated uncertain service stop");
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
        async fn device_id(&self, _: &Path) -> Result<String> {
            Ok("device".into())
        }
        async fn commit(&self, data: &Path) -> Result<()> {
            let pending = data.join("update-pending.json");
            let journal: Journal = serde_json::from_slice(&fs::read(&pending)?)?;
            ensure!(
                journal.phase == Phase::Confirmed,
                "only a confirmed activation can open admission"
            );
            fs::remove_file(pending)?;
            privatefs::sync_dir(data)?;
            Ok(())
        }
    }

    fn activation_fixture() -> (Policy, PathBuf, Journal) {
        let root = sandbox();
        let data = root.join("data");
        private_dir(&data).unwrap();
        private_dir(&root.join("backups")).unwrap();
        fs::write(data.join("identity.json"), b"opaque identity fixture").unwrap();
        fs::write(data.join("config.json"), b"opaque config fixture").unwrap();
        fs::write(data.join("node.sqlite3"), b"tasks-and-deduplication").unwrap();
        fs::write(data.join("node.sqlite3-wal"), b"uncheckpointed-events").unwrap();
        let previous = package(&root, "1.0.0");
        let candidate = package(&root, "1.1.0");
        privatefs::link_dir(&previous, &root.join("current")).unwrap();
        let journal = Journal {
            phase: Phase::Prepared,
            previous,
            candidate,
            device_id: "device".into(),
            backup: root.join("backups/fixture"),
            resume_service: true,
        };
        persist(&data.join("update-pending.json"), &journal).unwrap();
        (
            Policy {
                install_root: root,
                channel: Channel::Stable,
                automatic: false,
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
            recover_with_service(&policy, &data, &service, RecoveryMode::ResumeJournal)
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
        recover_with_service(&policy, &data, &service, RecoveryMode::ResumeJournal)
            .await
            .unwrap();
        assert_eq!(installed(&policy.install_root).unwrap(), journal.previous);
        journal.phase = Phase::Confirmed;
        persist(&data.join("update-pending.json"), &journal).unwrap();
        recover_with_service(&policy, &data, &service, RecoveryMode::ResumeJournal)
            .await
            .unwrap();
        assert_eq!(installed(&policy.install_root).unwrap(), journal.candidate);
        assert!(!data.join("update-pending.json").exists());
        assert_eq!(
            fs::read(data.join("node.sqlite3")).unwrap(),
            b"tasks-and-deduplication"
        );
        drop(lock);
        assert!(update_lock(&data).is_ok());
        fs::remove_dir_all(policy.install_root).unwrap();
    }

    #[tokio::test]
    async fn desktop_stopped_managed_release_switches_without_service_restart() {
        let (policy, data, journal) = activation_fixture();
        fs::remove_file(data.join("update-pending.json")).unwrap();
        let pending = data.join("update-pending.json");
        let preserved = [
            data.join("identity.json"),
            data.join("config.json"),
            data.join("node.sqlite3"),
            data.join("node.sqlite3-wal"),
        ];
        let before: Vec<_> = preserved
            .iter()
            .map(|path| fs::read(path).unwrap())
            .collect();
        let policy_bytes = br#"{"channel":"nightly","automatic":false}"#.to_vec();
        fs::write(data.join("update.json"), &policy_bytes).unwrap();
        fs::write(data.join("providers.json"), b"provider-material").unwrap();
        let release = super::DesktopManagedRelease {
            managed_root: &policy.install_root,
            previous: journal.previous.clone(),
            candidate: journal.candidate.clone(),
            device_id: journal.device_id.clone(),
            resume_service: false,
            tracks_current: true,
        };
        let during_proof = pending.clone();
        let locked_data = data.clone();
        super::apply_desktop_managed_release(&data, release, move |_| {
            let during_proof = during_proof.clone();
            async move {
                assert!(
                    lock_data_dir(&locked_data).is_err(),
                    "a concurrent Core cannot start during final proof"
                );
                assert!(during_proof.exists());
                let stored: Journal =
                    serde_json::from_slice(&fs::read(&during_proof).unwrap()).unwrap();
                assert_eq!(stored.phase, Phase::Prepared);
                assert!(!stored.resume_service);
                Ok(())
            }
        })
        .await
        .unwrap();
        assert_eq!(installed(&policy.install_root).unwrap(), journal.candidate);
        assert!(!pending.exists());
        for (path, bytes) in preserved.iter().zip(before) {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        assert_eq!(fs::read(data.join("update.json")).unwrap(), policy_bytes);
        assert_eq!(
            fs::read(data.join("providers.json")).unwrap(),
            b"provider-material"
        );
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
        recover_with_service(&policy, &data, &service, RecoveryMode::ResumeJournal)
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

    #[test]
    fn busy_admission_does_not_write_prepared_journal() {
        let (policy, data, journal) = activation_fixture();
        let pending = data.join("update-pending.json");
        fs::remove_file(&pending).unwrap();
        let error = persist_after_admission(&data, &journal, Admission::Busy).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("update deferred; the node could not become idle")
        );
        assert!(!pending.exists());
        assert_eq!(installed(&policy.install_root).unwrap(), journal.previous);
        persist_after_admission(&data, &journal, Admission::Idle).unwrap();
        assert!(pending.exists());
        let stored: Journal = serde_json::from_slice(&fs::read(&pending).unwrap()).unwrap();
        assert_eq!(stored.phase, Phase::Prepared);
        fs::remove_dir_all(policy.install_root).unwrap();
    }

    #[tokio::test]
    async fn running_handoff_starts_once_and_keeps_the_journal_until_acknowledgement() {
        let (policy, data, mut journal) = activation_fixture();
        fs::write(data.join("providers.json"), b"provider-material").unwrap();
        let service = FakeService {
            candidate: journal.candidate.clone(),
            failure: "none",
            starts: std::sync::atomic::AtomicUsize::new(0),
            stops: std::sync::atomic::AtomicUsize::new(0),
        };
        handoff(&policy, &data, &service, &journal).await.unwrap();
        assert!(data.join("update-pending.json").exists());
        let stored: Journal =
            serde_json::from_slice(&fs::read(data.join("update-pending.json")).unwrap()).unwrap();
        assert_eq!(stored.phase, Phase::Prepared);
        assert_eq!(service.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(service.stops.load(std::sync::atomic::Ordering::SeqCst), 1);
        acknowledge(&data, &service, &mut journal).await.unwrap();
        assert!(!data.join("update-pending.json").exists());
        assert_eq!(service.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(service.stops.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(installed(&policy.install_root).unwrap(), journal.candidate);
        assert_eq!(
            fs::read(data.join("providers.json")).unwrap(),
            b"provider-material"
        );
        fs::remove_dir_all(policy.install_root).unwrap();
    }

    #[tokio::test]
    async fn uncertain_stop_switch_start_and_health_keep_the_journal() {
        for failure in ["stop", "switch", "start", "health"] {
            let (policy, data, journal) = activation_fixture();
            fs::write(data.join("providers.json"), b"provider-material").unwrap();
            let update_policy = br#"{"channel":"stable","automatic":false}"#;
            fs::write(data.join("update.json"), update_policy).unwrap();
            if failure == "switch" {
                fs::remove_file(journal.candidate.join("BUILD-INFO.json")).unwrap();
            }
            let service = FakeService {
                candidate: journal.candidate.clone(),
                failure,
                starts: std::sync::atomic::AtomicUsize::new(0),
                stops: std::sync::atomic::AtomicUsize::new(0),
            };
            assert!(
                handoff(&policy, &data, &service, &journal).await.is_err(),
                "{failure}"
            );
            assert!(data.join("update-pending.json").exists(), "{failure}");
            let stored: Journal =
                serde_json::from_slice(&fs::read(data.join("update-pending.json")).unwrap())
                    .unwrap();
            assert_eq!(stored.phase, Phase::Prepared, "{failure}");
            assert_eq!(
                fs::read(data.join("providers.json")).unwrap(),
                b"provider-material",
                "{failure}"
            );
            assert_eq!(
                fs::read(data.join("update.json")).unwrap(),
                update_policy,
                "{failure}"
            );
            if failure == "stop" || failure == "switch" {
                assert_eq!(
                    installed(&policy.install_root).unwrap(),
                    journal.previous,
                    "{failure}"
                );
                assert_eq!(
                    service.starts.load(std::sync::atomic::Ordering::SeqCst),
                    0,
                    "{failure}"
                );
            }
            if failure == "stop" {
                let recovered =
                    recover_with_service(&policy, &data, &service, RecoveryMode::ResumeJournal)
                        .await;
                assert!(
                    recovered.is_err(),
                    "an uncertain stop is not a successful rollback"
                );
                assert!(data.join("update-pending.json").exists());
            }
            fs::remove_dir_all(policy.install_root).unwrap();
        }
    }

    #[tokio::test]
    async fn stopped_journal_recovery_does_not_start_the_service() {
        let (policy, data, mut journal) = activation_fixture();
        journal.resume_service = false;
        switch(&policy.install_root, &journal.candidate).unwrap();
        persist(&data.join("update-pending.json"), &journal).unwrap();
        let service = FakeService {
            candidate: journal.candidate.clone(),
            failure: "none",
            starts: std::sync::atomic::AtomicUsize::new(0),
            stops: std::sync::atomic::AtomicUsize::new(0),
        };
        let held = lock_data_dir(&data).unwrap();
        let pending_before = fs::read(data.join("update-pending.json")).unwrap();
        assert!(
            recover_with_service(&policy, &data, &service, RecoveryMode::ResumeJournal)
                .await
                .is_err()
        );
        assert_eq!(installed(&policy.install_root).unwrap(), journal.candidate);
        assert_eq!(
            fs::read(data.join("update-pending.json")).unwrap(),
            pending_before
        );
        held.unlock().unwrap();
        drop(held);
        recover_with_service(&policy, &data, &service, RecoveryMode::ResumeJournal)
            .await
            .unwrap();
        assert_eq!(service.starts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(service.stops.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(installed(&policy.install_root).unwrap(), journal.previous);
        assert!(!data.join("update-pending.json").exists());
        assert_eq!(
            fs::read(data.join("identity.json")).unwrap(),
            b"opaque identity fixture"
        );
        fs::remove_dir_all(policy.install_root).unwrap();
    }

    #[tokio::test]
    async fn stopped_release_refuses_before_effects_when_service_does_not_track_current() {
        let (policy, data, journal) = activation_fixture();
        fs::remove_file(data.join("update-pending.json")).unwrap();
        let release = super::DesktopManagedRelease {
            managed_root: &policy.install_root,
            previous: journal.previous.clone(),
            candidate: journal.candidate.clone(),
            device_id: journal.device_id.clone(),
            resume_service: false,
            tracks_current: false,
        };
        let error = super::apply_desktop_managed_release(&data, release, |_| async {
            panic!("a refused stopped update must not switch or start");
        })
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("stopped node service is not registered to the current program")
        );
        assert!(!data.join("update-pending.json").exists());
        assert_eq!(installed(&policy.install_root).unwrap(), journal.previous);
        assert_eq!(
            fs::read(data.join("node.sqlite3")).unwrap(),
            b"tasks-and-deduplication"
        );
        fs::remove_dir_all(policy.install_root).unwrap();
    }

    #[tokio::test]
    async fn desktop_keep_stopped_recovery_ignores_resume_service() {
        let (policy, data, mut journal) = activation_fixture();
        journal.resume_service = true;
        switch(&policy.install_root, &journal.candidate).unwrap();
        persist(&data.join("update-pending.json"), &journal).unwrap();
        fs::write(data.join("providers.json"), b"provider-material").unwrap();
        fs::write(
            data.join("update.json"),
            br#"{"channel":"stable","automatic":false}"#,
        )
        .unwrap();
        let preserved = [
            data.join("identity.json"),
            data.join("config.json"),
            data.join("node.sqlite3"),
            data.join("node.sqlite3-wal"),
            data.join("providers.json"),
            data.join("update.json"),
        ];
        let before: Vec<_> = preserved
            .iter()
            .map(|path| fs::read(path).unwrap())
            .collect();
        let service = FakeService {
            candidate: journal.candidate.clone(),
            failure: "none",
            starts: std::sync::atomic::AtomicUsize::new(0),
            stops: std::sync::atomic::AtomicUsize::new(0),
        };
        recover_with_service(&policy, &data, &service, RecoveryMode::KeepStopped)
            .await
            .unwrap();
        assert_eq!(service.starts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(service.stops.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(installed(&policy.install_root).unwrap(), journal.previous);
        assert!(!data.join("update-pending.json").exists());
        for (path, bytes) in preserved.iter().zip(before) {
            if path.exists() {
                assert_eq!(fs::read(path).unwrap(), bytes);
            }
        }
        fs::remove_dir_all(policy.install_root).unwrap();
    }

    #[tokio::test]
    async fn desktop_keep_stopped_recovery_leaves_journal_on_bad_device() {
        let (policy, data, mut journal) = activation_fixture();
        journal.device_id = "other-device".into();
        persist(&data.join("update-pending.json"), &journal).unwrap();
        let service = FakeService {
            candidate: journal.candidate.clone(),
            failure: "none",
            starts: std::sync::atomic::AtomicUsize::new(0),
            stops: std::sync::atomic::AtomicUsize::new(0),
        };
        assert!(
            recover_with_service(&policy, &data, &service, RecoveryMode::KeepStopped)
                .await
                .is_err()
        );
        assert!(data.join("update-pending.json").exists());
        fs::remove_dir_all(policy.install_root).unwrap();
    }

    #[tokio::test]
    async fn failed_stopped_proof_restores_previous_without_starting() {
        let (policy, data, journal) = activation_fixture();
        fs::remove_file(data.join("update-pending.json")).unwrap();
        let release = super::DesktopManagedRelease {
            managed_root: &policy.install_root,
            previous: journal.previous.clone(),
            candidate: journal.candidate.clone(),
            device_id: journal.device_id.clone(),
            resume_service: false,
            tracks_current: true,
        };
        let error = super::apply_desktop_managed_release(&data, release, |_| async {
            Err(anyhow::anyhow!("candidate failed its stopped-state proof"))
        })
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("candidate failed its stopped-state proof")
        );
        assert!(!data.join("update-pending.json").exists());
        assert_eq!(installed(&policy.install_root).unwrap(), journal.previous);
        fs::remove_dir_all(policy.install_root).unwrap();
    }
}
