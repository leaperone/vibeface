//! App-private durable Node provisioning. This command is intentionally hidden from the public
//! CLI surface; the Electron main process is its only caller.
use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};
use vibeface_bridge::platform::privatefs;
use vibeface_control::{ControlRequest, DATA_EPOCH, UPDATE_PROTOCOL};

const RECEIPT: &str = "desktop-node.json";
const MAX_RECEIPT: u64 = 16 * 1024;

#[derive(Debug, Clone, Copy)]
enum OperationFailure {
    Package,
    Service,
    Health,
    Lock,
    Registration,
    UpdateRefused,
}

impl std::fmt::Display for OperationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Package => "desktop_node_package_invalid",
            Self::Service => "desktop_node_service_failed",
            Self::Health => "desktop_node_health_unconfirmed",
            Self::Lock => "desktop_node_lock_unconfirmed",
            Self::Registration => "desktop_node_registration_unconfirmed",
            Self::UpdateRefused => "desktop_node_update_refused",
        })
    }
}

impl std::error::Error for OperationFailure {}

fn public_failure(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<OperationFailure>() {
        Some(failure) => anyhow::anyhow!(*failure),
        None => anyhow::anyhow!("desktop_node_operation_failed"),
    }
}

#[derive(Args)]
pub struct DesktopNode {
    /// Absolute Electron profile directory trusted by the App.
    #[arg(long, value_parser = absolute_path)]
    profile_dir: PathBuf,
    /// Existing Node identity to preserve. Required for every mutation.
    #[arg(long, value_parser = nonempty)]
    expected_device_id: Option<String>,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand, Clone, Copy)]
enum Action {
    Status,
    Install,
    Update,
    Start,
    Stop,
    Uninstall,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct RuntimeUpdateJson {
    bundled_version: String,
    installed_version: Option<String>,
    running_version: Option<String>,
    availability: &'static str,
}

#[derive(Serialize)]
struct ResultJson {
    ok: bool,
    state: &'static str,
    device_id: Option<String>,
    data_dir: Option<PathBuf>,
    service_id: Option<String>,
    managed_root: Option<PathBuf>,
    service_running: Option<bool>,
    core_status: Option<String>,
    runtime_update: RuntimeUpdateJson,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    schema: u32,
    pub(crate) device_id: String,
    pub(crate) data_dir: PathBuf,
    pub(crate) managed_root: PathBuf,
    pub(crate) service_id: String,
    update_protocol: u32,
    data_epoch: u32,
}

#[derive(Deserialize)]
struct InspectResult {
    data_dir: PathBuf,
    device_id: String,
    status: String,
    #[serde(default)]
    core_version: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeService {
    installed: bool,
    running: bool,
}

pub(crate) struct ManagedService {
    pub(crate) package: PathBuf,
    pub(crate) data_dir: PathBuf,
    pub(crate) service_id: String,
    pub(crate) device_id: String,
}

fn absolute_path(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("path must be absolute".into())
    }
}

fn nonempty(value: &str) -> Result<String, String> {
    if !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control) {
        Ok(value.to_owned())
    } else {
        Err("invalid device identity".into())
    }
}

fn target() -> Result<String> {
    let arch = std::env::consts::ARCH;
    let suffix = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        "windows" => "pc-windows-msvc",
        other => anyhow::bail!("unsupported desktop platform: {other}"),
    };
    Ok(format!("{arch}-{suffix}"))
}

fn canonical_profile(path: &Path) -> Result<PathBuf> {
    let profile = fs::canonicalize(path).context("Desktop profile directory is missing")?;
    ensure!(
        profile.is_dir() && privatefs::is_private(&profile)?,
        "Desktop profile directory must be a private directory owned by this account"
    );
    Ok(profile)
}

/// Runs before Chromium and preferences use the App-owned directory, without creating a Node.
pub(crate) fn prepare_profile(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "Desktop profile path must be absolute");
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Desktop profile must be a real directory"
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)?,
        Err(error) => return Err(error.into()),
    }
    ensure!(
        privatefs::owner_of(path)? == privatefs::current_account()?,
        "Desktop profile belongs to another account"
    );
    privatefs::restrict_dir(path)?;
    canonical_profile(path)?;
    Ok(())
}

fn data_dir(profile: &Path) -> Result<PathBuf> {
    let data = profile.join("node");
    let canonical =
        fs::canonicalize(&data).context("App-owned Node data directory is not initialized")?;
    ensure!(
        canonical == data && canonical.is_dir() && privatefs::is_private(&canonical)?,
        "Node data directory is not a private App-owned directory"
    );
    for name in ["config.json", "identity.json"] {
        let path = canonical.join(name);
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.file_type().is_file(),
            "initialized Node data is invalid"
        );
    }
    Ok(canonical)
}

/// Mutating service operations may create the lock for a freshly initialized, unstarted Node.
fn data_unlocked(data: &Path) -> Result<bool> {
    let path = data.join("node.lock");
    let file =
        privatefs::open_or_create(&path).context("cannot open the App-owned Node data lock")?;
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.file_type().is_file() && privatefs::is_private(&path)?,
        "Node data lock is not a private regular file"
    );
    match file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    file.unlock()?;
    Ok(true)
}

fn require_data_unlocked(data: &Path) -> Result<()> {
    ensure!(
        data_unlocked(data)?,
        "Node data is in use; stop the App-owned child and retry"
    );
    Ok(())
}

async fn wait_data_unlocked(data: &Path) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if data_unlocked(data)? {
            return Ok(());
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "Node did not release its data lock after stopping"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn inspect(data: &Path) -> Result<InspectResult> {
    let binary = source_package()?.join(crate::update::release::PROGRAMS[1]);
    let value = vibeface_control::request(
        &binary,
        &ControlRequest::Inspect {
            data_dir: data.to_owned(),
        },
    )
    .await?;
    let result: InspectResult =
        serde_json::from_value(value).context("Core returned invalid Node identity")?;
    ensure!(
        result.data_dir == data
            && !result.device_id.is_empty()
            && ["running", "stopped", "unavailable"].contains(&result.status.as_str()),
        "Core inspected a different or invalid Node"
    );
    Ok(result)
}

fn source_package() -> Result<PathBuf> {
    let executable = fs::canonicalize(std::env::current_exe()?)?;
    let package = executable
        .parent()
        .context("bundled CLI has no runtime parent")?;
    let build = crate::update::release::verify_sealed_package(
        package,
        env!("CARGO_PKG_VERSION"),
        &target()?,
    )?;
    crate::update::validate_desktop_package(package)?;
    ensure!(
        build.version == env!("CARGO_PKG_VERSION"),
        "bundled runtime version mismatch"
    );
    Ok(package.to_path_buf())
}

pub(crate) fn service_id(profile: &Path) -> String {
    let hash = Sha256::digest(profile.as_os_str().to_string_lossy().as_bytes());
    format!("desktop-{}", hex::encode(&hash[..10]))
}

fn receipt_path(root: &Path) -> PathBuf {
    root.join(RECEIPT)
}

fn read_receipt(root: &Path) -> Result<Option<Receipt>> {
    let root_metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        root_metadata.file_type().is_dir()
            && !root_metadata.file_type().is_symlink()
            && privatefs::is_private(root)?,
        "managed runtime root is unsafe"
    );
    let path = receipt_path(root);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.file_type().is_file()
            && metadata.len() <= MAX_RECEIPT
            && privatefs::is_private(&path)?,
        "invalid Desktop Node receipt"
    );
    let receipt: Receipt =
        serde_json::from_slice(&fs::read(path)?).context("invalid Desktop Node receipt")?;
    ensure!(
        receipt.schema == 1
            && receipt.update_protocol == UPDATE_PROTOCOL
            && receipt.data_epoch == DATA_EPOCH,
        "unsupported Desktop Node receipt"
    );
    ensure!(
        receipt.managed_root == *root && receipt.service_id.starts_with("desktop-"),
        "Desktop Node receipt does not own this runtime"
    );
    Ok(Some(receipt))
}

fn bundled_package_version(package: &Path) -> Result<String> {
    let build = crate::update::release::verify_sealed_package(
        package,
        env!("CARGO_PKG_VERSION"),
        &target()?,
    )?;
    Ok(build.version)
}

fn managed_package_version(package: &Path) -> Result<String> {
    crate::update::package_release_version(package)
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    ensure!(
        metadata.file_type().is_dir(),
        "runtime contains a non-directory root"
    );
    fs::create_dir(destination)?;
    privatefs::restrict_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let src = entry.path();
        let dst = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&src)?;
        if metadata.file_type().is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            ensure!(
                metadata.file_type().is_file(),
                "runtime contains a symlink or special file"
            );
            fs::copy(&src, &dst)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let executable = metadata.permissions().mode() & 0o111 != 0;
                fs::set_permissions(
                    &dst,
                    fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }),
                )?;
            }
        }
    }
    Ok(())
}

fn ensure_root(profile: &Path) -> Result<PathBuf> {
    let root = profile.join("node-runtime");
    match fs::symlink_metadata(&root) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_dir()
                && !metadata.file_type().is_symlink()
                && privatefs::is_private(&root)?,
            "managed runtime root is not private"
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(&root)?;
            privatefs::restrict_dir(&root)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(root)
}

fn stage_runtime_release(root: &Path, source: &Path) -> Result<PathBuf> {
    let root = fs::canonicalize(root)?;
    let version = managed_package_version(source)?;
    let releases = root.join("releases");
    if !releases.exists() {
        fs::create_dir(&releases)?;
        privatefs::restrict_dir(&releases)?;
    }
    ensure!(
        fs::symlink_metadata(&releases)?.file_type().is_dir() && privatefs::is_private(&releases)?,
        "managed releases directory is unsafe"
    );
    let destination = releases.join(&version);
    if !destination.exists() {
        let staging = releases.join(format!(".stage-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            copy_tree(source, &staging)?;
            managed_package_version(&staging)?;
            crate::update::validate_desktop_package(&staging)?;
            fs::rename(&staging, &destination)?;
            Ok::<_, anyhow::Error>(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        result?;
    } else {
        ensure!(
            fs::symlink_metadata(&destination)?.file_type().is_dir()
                && privatefs::is_private(&destination)?,
            "existing runtime release is unsafe"
        );
        managed_package_version(&destination)?;
        crate::update::validate_desktop_package(&destination)?;
        ensure!(
            crate::update::release::same_package_bytes(source, &destination)?,
            "existing runtime version has different contents"
        );
    }
    Ok(destination)
}

fn ensure_runtime(root: &Path, source: &Path) -> Result<PathBuf> {
    let root = fs::canonicalize(root)?;
    let destination = stage_runtime_release(&root, source)?;
    let current = root.join("current");
    match fs::symlink_metadata(&current) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_symlink() && privatefs::is_dir_link(&current)?,
            "managed runtime current entry is not a link"
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if current.exists() {
        let old = fs::canonicalize(&current)?;
        ensure!(
            old.starts_with(&root) && old != root,
            "managed runtime current link escapes its root"
        );
    }
    crate::update::switch_managed_current(&root, &destination)?;
    Ok(current)
}

fn parse_release_version(version: &str) -> Result<Version> {
    Version::parse(version).context("managed runtime version is invalid")
}

fn runtime_update_for_packages(
    bundled: &Path,
    installed: Option<&Path>,
    running_version: Option<&str>,
) -> Result<RuntimeUpdateJson> {
    let bundled_version = bundled_package_version(bundled)
        .or_else(|_| managed_package_version(bundled))?;
    let installed_version = installed.map(managed_package_version).transpose()?;
    let availability = match installed {
        None => "not_installed",
        Some(current) => {
            let bundled_parsed = parse_release_version(&bundled_version)?;
            let current_parsed = parse_release_version(installed_version.as_deref().unwrap())?;
            if bundled_parsed < current_parsed {
                "downgrade_refused"
            } else if bundled_parsed == current_parsed {
                if crate::update::release::same_package_bytes(bundled, current)? {
                    "current"
                } else {
                    "immutable_conflict"
                }
            } else {
                "newer_available"
            }
        }
    };
    Ok(RuntimeUpdateJson {
        bundled_version,
        installed_version,
        running_version: running_version.map(str::to_owned),
        availability,
    })
}

fn managed_current(root: &Path) -> Result<PathBuf> {
    let root = fs::canonicalize(root)?;
    ensure!(
        root.is_dir() && privatefs::is_private(&root)?,
        "managed runtime root is unsafe"
    );
    let releases = fs::canonicalize(root.join("releases"))?;
    ensure!(
        releases.parent() == Some(root.as_path()) && privatefs::is_private(&releases)?,
        "managed releases directory escapes its root"
    );
    let current = root.join("current");
    ensure!(
        privatefs::is_dir_link(&current)?,
        "managed current entry is not a directory link"
    );
    let package = fs::canonicalize(&current)?;
    ensure!(
        package.parent() == Some(releases.as_path()) && privatefs::is_private(&package)?,
        "managed current link escapes its release directory"
    );
    crate::update::release::verify_sealed_contents(&package, &target()?)?;
    crate::update::validate_desktop_package(&package)?;
    Ok(current)
}

pub(crate) fn managed_service_for_data(data: &Path) -> Result<Option<ManagedService>> {
    let data = fs::canonicalize(data).context("initialized data directory is required")?;
    if !data.file_name().is_some_and(|name| name == "node") {
        return Ok(None);
    }
    let profile = fs::canonicalize(
        data.parent()
            .context("Node data directory has no profile")?,
    )?;
    ensure!(
        data == profile.join("node") && privatefs::is_private(&profile)?,
        "Desktop Node profile is not private"
    );
    let root = profile.join("node-runtime");
    if let Some(saved_root) = crate::update::desktop_policy_root(&data)?
        && saved_root != root
        && (read_receipt(&saved_root)?.is_some()
            || saved_root
                .file_name()
                .is_some_and(|name| name == "node-runtime"))
    {
        anyhow::bail!("Desktop updater policy points to another profile-owned runtime");
    }
    let Some(receipt) = read_receipt(&root)? else {
        ensure!(
            !crate::update::desktop_policy_uses_root(&data, &root)?,
            "Desktop Node receipt is missing for its updater policy"
        );
        return Ok(None);
    };
    ensure!(
        receipt.data_dir == data
            && receipt.managed_root == root
            && receipt.service_id == service_id(&profile),
        "Desktop Node receipt does not own this data directory"
    );
    ensure!(
        crate::update::desktop_policy_matches(&data, &root)?,
        "Desktop Node updater policy changed"
    );
    let package = managed_current(&root)?;
    Ok(Some(ManagedService {
        package,
        data_dir: data,
        service_id: receipt.service_id,
        device_id: receipt.device_id,
    }))
}

pub(crate) fn service_command(
    package: &Path,
    data: &Path,
    id: &str,
    action: &str,
) -> Result<tokio::process::Command> {
    crate::update::release::verify_sealed_contents(package, &target()?)?;
    crate::update::validate_desktop_package(package)?;
    let mut command =
        crate::service::service_script(&package.join(crate::update::release::SERVICE_SCRIPT))?;
    command
        .arg(action)
        .arg("--data-dir")
        .arg(data)
        .arg("--service-id")
        .arg(id)
        .arg("--binary")
        .arg(package.join(crate::update::release::PROGRAMS[0]));
    Ok(command)
}

async fn native_service(package: &Path, data: &Path, id: &str) -> Result<NativeService> {
    let mut command =
        service_command(package, data, id, "verify").context(OperationFailure::Package)?;
    let output = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .context(OperationFailure::Registration)?;
    ensure!(
        output.status.success() && output.stdout.len() <= 4096,
        OperationFailure::Registration
    );
    let result: NativeService =
        serde_json::from_slice(&output.stdout).context(OperationFailure::Registration)?;
    Ok(result)
}

pub(crate) async fn run_managed_service(service: &ManagedService, action: &str) -> Result<()> {
    run_service(
        &service.package,
        &service.data_dir,
        &service.service_id,
        action,
    )
    .await
}

pub(crate) async fn managed_service_registered(service: &ManagedService) -> Result<bool> {
    Ok(
        native_service(&service.package, &service.data_dir, &service.service_id)
            .await?
            .installed,
    )
}

async fn run_service(package: &Path, data: &Path, id: &str, action: &str) -> Result<()> {
    let status = service_command(package, data, id, action)
        .context(OperationFailure::Package)?
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .context(OperationFailure::Service)?;
    ensure!(status.success(), OperationFailure::Service);
    Ok(())
}

async fn wait_health(data: &Path, expected: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(result) = inspect(data).await
            && result.device_id == expected
            && result.status == "running"
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(OperationFailure::Health);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn runtime_update_for_receipt(
    receipt: Option<&Receipt>,
    running_version: Option<&str>,
) -> Result<RuntimeUpdateJson> {
    let bundled = match source_package() {
        Ok(value) => value,
        Err(error) if cfg!(test) => {
            return Ok(RuntimeUpdateJson {
                bundled_version: env!("CARGO_PKG_VERSION").into(),
                installed_version: None,
                running_version: running_version.map(str::to_owned),
                availability: if receipt.is_some() {
                    "current"
                } else {
                    "not_installed"
                },
            });
        }
        Err(error) => return Err(error),
    };
    let installed = receipt
        .map(|value| managed_current(&value.managed_root))
        .transpose()?;
    runtime_update_for_packages(&bundled, installed.as_deref(), running_version)
}

fn emit(
    state: &'static str,
    receipt: Option<&Receipt>,
    service_running: Option<bool>,
    core_status: Option<String>,
    running_version: Option<&str>,
) -> Result<()> {
    output(
        state,
        receipt,
        service_running,
        core_status,
        runtime_update_for_receipt(receipt, running_version)?,
    )
}

fn output(
    state: &'static str,
    receipt: Option<&Receipt>,
    service_running: Option<bool>,
    core_status: Option<String>,
    runtime_update: RuntimeUpdateJson,
) -> Result<()> {
    let receipt = receipt.filter(|_| state != "not_installed");
    let service_running = (state != "not_installed")
        .then_some(service_running)
        .flatten();
    let core_status = (state != "not_installed").then_some(core_status).flatten();
    println!(
        "{}",
        serde_json::to_string(&ResultJson {
            ok: true,
            state,
            device_id: receipt.map(|value| value.device_id.clone()),
            data_dir: receipt.map(|value| value.data_dir.clone()),
            service_id: receipt.map(|value| value.service_id.clone()),
            managed_root: receipt.map(|value| value.managed_root.clone()),
            service_running,
            core_status,
            runtime_update,
        })?
    );
    Ok(())
}

pub async fn run(options: DesktopNode) -> Result<()> {
    match run_inner(options).await {
        Ok(()) => Ok(()),
        Err(error) => Err(public_failure(error)),
    }
}

async fn run_inner(options: DesktopNode) -> Result<()> {
    if matches!(options.action, Action::Status)
        && matches!(
            fs::symlink_metadata(&options.profile_dir),
            Err(ref error) if error.kind() == io::ErrorKind::NotFound
        )
    {
        return emit("not_installed", None, None, None, None);
    }
    let profile = canonical_profile(&options.profile_dir)?;
    let root = profile.join("node-runtime");
    let existing = read_receipt(&root)?;
    match options.action {
        Action::Status => {
            let Some(receipt) = existing.as_ref() else {
                return emit("not_installed", None, None, None, None);
            };
            let data = data_dir(&profile)?;
            ensure!(
                receipt.data_dir == data,
                "managed Node data directory changed"
            );
            ensure!(
                service_id(&profile) == receipt.service_id,
                "managed Node service identity changed"
            );
            if options
                .expected_device_id
                .as_deref()
                .is_some_and(|id| id != receipt.device_id)
            {
                return emit("conflict", Some(receipt), None, None, None);
            }
            if !matches!(
                crate::update::desktop_policy_matches(&data, &receipt.managed_root),
                Ok(true)
            ) {
                return emit("conflict", Some(receipt), None, None, None);
            }
            let package = managed_current(&receipt.managed_root)?;
            let native = native_service(&package, &data, &receipt.service_id).await;
            let core = inspect(&data).await;
            match (native, core) {
                (Ok(native), Ok(core)) if core.device_id == receipt.device_id => {
                    let state = if native.installed {
                        if native.running {
                            "installed"
                        } else {
                            "installed_stopped"
                        }
                    } else {
                        "partial"
                    };
                    let running_version = if native.running {
                        core.core_version.as_deref()
                    } else {
                        None
                    };
                    emit(
                        state,
                        Some(receipt),
                        Some(native.running),
                        Some(core.status),
                        running_version,
                    )
                }
                (Ok(native), Ok(core)) => emit(
                    "conflict",
                    Some(receipt),
                    Some(native.running),
                    Some(core.status),
                    core.core_version.as_deref(),
                ),
                (Err(_), _) => emit("conflict", Some(receipt), None, None, None),
                (_, Err(_)) => emit("unavailable", Some(receipt), None, None, None),
            }
        }
        action => {
            let expected = options
                .expected_device_id
                .as_deref()
                .context("--expected-device-id is required for mutations")?;
            let data = data_dir(&profile)?;
            let _update_lock = crate::update::update_lock(&data)
                .context("another Node updater is active; retry the Desktop operation")?;
            let inspected = inspect(&data).await?;
            ensure!(
                inspected.device_id == expected,
                "Node identity changed; refusing service operation"
            );
            match action {
                Action::Install => {
                    if let Some(receipt) = existing.as_ref() {
                        ensure!(
                            receipt.device_id == expected
                                && receipt.data_dir == data
                                && receipt.service_id == service_id(&profile),
                            "existing Desktop Node receipt belongs to another identity"
                        );
                        ensure!(
                            crate::update::desktop_policy_matches(&data, &receipt.managed_root)?,
                            "Desktop Node updater policy changed"
                        );
                        let package = managed_current(&receipt.managed_root)?;
                        let native = native_service(&package, &data, &receipt.service_id).await?;
                        if native.installed && native.running {
                            ensure!(
                                inspected.status == "running",
                                "registered service and Core health disagree"
                            );
                            emit(
                                "installed",
                                Some(receipt),
                                Some(true),
                                Some("running".into()),
                                inspected.core_version.as_deref(),
                            )?;
                            return Ok(());
                        }
                        require_data_unlocked(&data)?;
                    } else {
                        require_data_unlocked(&data)?;
                    }
                    let source = source_package()?;
                    let requested_root = profile.join("node-runtime");
                    ensure!(
                        crate::update::desktop_policy_compatible(&data, &requested_root)?,
                        "Node updater is already bound to another installation"
                    );
                    let root = ensure_root(&profile)?;
                    let package = ensure_runtime(&root, &source)?;
                    if existing.is_none() {
                        crate::update::bind_desktop_install_root(&data, &root)?;
                    }
                    let receipt = Receipt {
                        schema: 1,
                        device_id: expected.into(),
                        data_dir: data.clone(),
                        managed_root: root.clone(),
                        service_id: service_id(&profile),
                        update_protocol: UPDATE_PROTOCOL,
                        data_epoch: DATA_EPOCH,
                    };
                    privatefs::write_atomic(&receipt_path(&root), &serde_json::to_vec(&receipt)?)?;
                    run_service(&package, &data, &receipt.service_id, "install").await?;
                    wait_health(&data, expected).await?;
                    let native = native_service(&package, &data, &receipt.service_id).await?;
                    ensure!(
                        native.installed && native.running,
                        "background service manager did not confirm the installed Node"
                    );
                    let final_inspect = inspect(&data).await?;
                    emit(
                        "installed",
                        Some(&receipt),
                        Some(true),
                        Some("running".into()),
                        final_inspect.core_version.as_deref(),
                    )
                }
                Action::Update => {
                    let receipt = existing.as_ref().context("Desktop Node is not installed")?;
                    ensure!(
                        receipt.device_id == expected
                            && receipt.data_dir == data
                            && receipt.service_id == service_id(&profile),
                        "managed Node identity or data directory changed"
                    );
                    ensure!(
                        crate::update::desktop_policy_matches(&data, &receipt.managed_root)?,
                        "Desktop Node updater policy changed"
                    );
                    let source = source_package()?;
                    let installed = managed_current(&receipt.managed_root)?;
                    let update_meta = runtime_update_for_packages(
                        &source,
                        Some(&installed),
                        inspected.core_version.as_deref(),
                    )?;
                    match update_meta.availability {
                        "current" => {
                            let native =
                                native_service(&installed, &data, &receipt.service_id).await?;
                            if native.installed && native.running {
                                ensure!(
                                    inspected.status == "running",
                                    "registered service and Core health disagree"
                                );
                                let bundled_version = bundled_package_version(&source)?;
                                if let Some(running) = inspected.core_version.as_deref() {
                                    ensure!(
                                        running == bundled_version,
                                        "running Core still reports an older program version"
                                    );
                                }
                            }
                            let core_status = if native.running && inspected.status == "running" {
                                "running".to_string()
                            } else if inspected.status == "stopped" {
                                "stopped".to_string()
                            } else {
                                inspected.status.clone()
                            };
                            return emit(
                                if native.running { "installed" } else { "installed_stopped" },
                                Some(receipt),
                                Some(native.running),
                                Some(core_status),
                                inspected.core_version.as_deref(),
                            );
                        }
                        "newer_available" => {}
                        "downgrade_refused" | "immutable_conflict" => {
                            anyhow::bail!(OperationFailure::UpdateRefused)
                        }
                        _ => anyhow::bail!(OperationFailure::UpdateRefused),
                    }
                    let native = native_service(&installed, &data, &receipt.service_id).await?;
                    if inspected.status == "running" && !(native.installed && native.running) {
                        anyhow::bail!(OperationFailure::UpdateRefused);
                    }
                    let staged = stage_runtime_release(&receipt.managed_root, &source)
                        .context(OperationFailure::Package)?;
                    let was_running = native.installed && native.running;
                    if was_running {
                        if inspected.status != "running" {
                            anyhow::bail!(OperationFailure::Health);
                        }
                        run_service(&installed, &data, &receipt.service_id, "stop")
                            .await
                            .context(OperationFailure::Service)?;
                        wait_data_unlocked(&data)
                            .await
                            .context(OperationFailure::Lock)?;
                    } else {
                        require_data_unlocked(&data).context(OperationFailure::Lock)?;
                    }
                    crate::update::switch_managed_current(&receipt.managed_root, &staged)
                        .context(OperationFailure::Package)?;
                    let package = managed_current(&receipt.managed_root)?;
                    if native.installed {
                        run_service(&package, &data, &receipt.service_id, "install").await?;
                    }
                    if was_running {
                        run_service(&package, &data, &receipt.service_id, "start").await?;
                        wait_health(&data, expected).await?;
                    }
                    let native = native_service(&package, &data, &receipt.service_id).await?;
                    let final_inspect = inspect(&data).await?;
                    let bundled_version = bundled_package_version(&source)?;
                    ensure!(
                        managed_package_version(&package)? == bundled_version,
                        "managed runtime did not switch to the bundled version"
                    );
                    if was_running {
                        ensure!(
                            native.installed && native.running,
                            "background service manager did not confirm the updated Node"
                        );
                        ensure!(
                            final_inspect.status == "running",
                            "updated Core did not return to a running state"
                        );
                        if let Some(running) = final_inspect.core_version.as_deref() {
                            ensure!(
                                running == bundled_version,
                                "running Core still reports an older program version"
                            );
                        }
                    }
                    let core_status = if final_inspect.status == "running" {
                        "running".to_string()
                    } else if final_inspect.status == "stopped" {
                        "stopped".to_string()
                    } else {
                        final_inspect.status.clone()
                    };
                    emit(
                        if native.running { "installed" } else { "installed_stopped" },
                        Some(receipt),
                        Some(native.running),
                        Some(core_status),
                        final_inspect.core_version.as_deref(),
                    )
                }
                Action::Start => {
                    let receipt = existing.as_ref().context("Desktop Node is not installed")?;
                    ensure!(
                        receipt.device_id == expected && receipt.data_dir == data,
                        "managed Node identity or data directory changed"
                    );
                    require_data_unlocked(&data)?;
                    ensure!(
                        crate::update::desktop_policy_matches(&data, &receipt.managed_root)?,
                        "updater policy points outside Desktop managed runtime"
                    );
                    let package = managed_current(&receipt.managed_root)?;
                    run_service(&package, &data, &receipt.service_id, "start").await?;
                    wait_health(&data, expected).await?;
                    let native = native_service(&package, &data, &receipt.service_id).await?;
                    ensure!(
                        native.installed && native.running,
                        "background service manager did not confirm the started Node"
                    );
                    let final_inspect = inspect(&data).await?;
                    emit(
                        "installed",
                        Some(receipt),
                        Some(true),
                        Some("running".into()),
                        final_inspect.core_version.as_deref(),
                    )
                }
                Action::Stop => {
                    let receipt = existing.as_ref().context("Desktop Node is not installed")?;
                    ensure!(
                        receipt.device_id == expected && receipt.data_dir == data,
                        "managed Node identity or data directory changed"
                    );
                    ensure!(
                        crate::update::desktop_policy_matches(&data, &receipt.managed_root)?,
                        "Desktop Node updater policy changed"
                    );
                    let package = managed_current(&receipt.managed_root)?;
                    run_service(&package, &data, &receipt.service_id, "stop").await?;
                    wait_data_unlocked(&data)
                        .await
                        .context(OperationFailure::Lock)?;
                    let native = native_service(&package, &data, &receipt.service_id).await?;
                    ensure!(
                        native.installed && !native.running,
                        "background service manager did not confirm the stopped Node"
                    );
                    emit(
                        "installed_stopped",
                        Some(receipt),
                        Some(false),
                        Some("stopped".into()),
                        None,
                    )
                }
                Action::Uninstall => {
                    let receipt = existing.as_ref().context("Desktop Node is not installed")?;
                    ensure!(
                        receipt.device_id == expected && receipt.data_dir == data,
                        "managed Node identity or data directory changed"
                    );
                    ensure!(
                        crate::update::desktop_policy_matches(&data, &receipt.managed_root)?,
                        "Desktop Node updater policy changed"
                    );
                    let package = managed_current(&receipt.managed_root)?;
                    run_service(&package, &data, &receipt.service_id, "uninstall-node").await?;
                    wait_data_unlocked(&data)
                        .await
                        .context(OperationFailure::Lock)?;
                    let native = native_service(&package, &data, &receipt.service_id).await?;
                    ensure!(
                        !native.installed,
                        "background service registration remains after removal"
                    );
                    fs::remove_file(receipt_path(&receipt.managed_root))?;
                    emit("not_installed", None, Some(false), Some("stopped".into()), None)
                }
                Action::Status => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_errors_expose_only_fixed_codes_without_private_causes() {
        let classified =
            anyhow::anyhow!("private fixture contents").context(OperationFailure::Service);
        assert_eq!(
            format!("{:?}", public_failure(classified)),
            "desktop_node_service_failed"
        );
        assert_eq!(
            format!(
                "{:?}",
                public_failure(anyhow::anyhow!("private fixture contents"))
            ),
            "desktop_node_operation_failed"
        );
    }

    fn temp(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "vibeface-desktop-node-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&path).unwrap();
        privatefs::restrict_dir(&path).unwrap();
        path
    }

    #[test]
    fn profile_preparation_is_private_idempotent_and_creates_no_node() {
        let root = temp("profile-permissions");
        let profile = root.join("Desktop 数据");
        fs::create_dir(&profile).unwrap();
        fs::write(
            profile.join("preferences.json"),
            b"keep existing preferences",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&profile, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(canonical_profile(&profile).is_err());
        }
        for _ in 0..2 {
            prepare_profile(&profile).unwrap();
            assert!(privatefs::is_private(&profile).unwrap());
            assert_eq!(
                fs::read(profile.join("preferences.json")).unwrap(),
                b"keep existing preferences"
            );
            assert!(!profile.join("node").exists());
        }
        let fresh = root.join("fresh");
        prepare_profile(&fresh).unwrap();
        assert!(privatefs::is_private(&fresh).unwrap());
        assert!(fs::read_dir(&fresh).unwrap().next().is_none());
        let file = root.join("not-a-profile");
        fs::write(&file, b"preserve").unwrap();
        assert!(prepare_profile(&file).is_err());
        assert_eq!(fs::read(file).unwrap(), b"preserve");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn service_stop_waits_for_the_real_lock_and_a_fresh_install_can_create_it() {
        let data = temp("service-data-lock");
        assert!(!data.join("node.lock").exists());
        require_data_unlocked(&data).unwrap();
        assert!(privatefs::is_private(&data.join("node.lock")).unwrap());
        let lock = privatefs::open_or_create(&data.join("node.lock")).unwrap();
        lock.try_lock().unwrap();
        assert!(!data_unlocked(&data).unwrap());
        assert!(require_data_unlocked(&data).is_err());
        let directory = data.clone();
        let waiting = tokio::spawn(async move { wait_data_unlocked(&directory).await });
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "a loaded service is not stopped until its Core releases the lock"
        );
        drop(lock);
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(data_unlocked(&data).unwrap());
        fs::remove_dir_all(data).unwrap();
    }

    fn sealed_fixture_version(root: &Path, version: &str) -> PathBuf {
        fs::create_dir_all(root).unwrap();
        let package = root.join("runtime");
        fs::create_dir(&package).unwrap();
        let names = [
            crate::update::release::PROGRAMS[0],
            crate::update::release::PROGRAMS[1],
            crate::update::release::SERVICE_SCRIPT,
            "web/index.html",
        ];
        for (index, name) in names.iter().enumerate() {
            let path = package.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, format!("fixture-{index}")).unwrap();
            if index < 2 {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                }
            }
        }
        let source_commit = "a".repeat(40);
        let build = serde_json::json!({
            "core_version": version, "core_source_commit": source_commit,
            "app_version": version, "app_source_commit": source_commit,
            "version": version, "target": target().unwrap(),
            "source_commit": source_commit, "update_protocol": UPDATE_PROTOCOL, "data_epoch": DATA_EPOCH
        });
        fs::write(
            package.join("BUILD-INFO.json"),
            serde_json::to_vec(&build).unwrap(),
        )
        .unwrap();
        let mut files = Vec::new();
        let mut names = names.to_vec();
        names.push("BUILD-INFO.json");
        names.sort();
        for name in names {
            let bytes = fs::read(package.join(name)).unwrap();
            files.push(format!("{}  {name}", hex::encode(Sha256::digest(bytes))));
        }
        fs::write(package.join("FILES.sha256"), files.join("\n") + "\n").unwrap();
        package
    }

    fn sealed_fixture(root: &Path) -> PathBuf {
        sealed_fixture_version(root, env!("CARGO_PKG_VERSION"))
    }

    fn install_test_receipt(profile: &Path, data: &Path, root: &Path) -> Receipt {
        Receipt {
            schema: 1,
            device_id: "device-fixture".into(),
            data_dir: data.to_path_buf(),
            managed_root: root.to_path_buf(),
            service_id: service_id(profile),
            update_protocol: UPDATE_PROTOCOL,
            data_epoch: DATA_EPOCH,
        }
    }

    #[test]
    fn service_identity_is_stable_and_profile_scoped() {
        let first = Path::new("/tmp/profile one");
        assert_eq!(service_id(first), service_id(first));
        assert_ne!(service_id(first), service_id(Path::new("/tmp/profile two")));
        assert!(service_id(first).starts_with("desktop-"));
    }

    #[test]
    fn expected_device_id_rejects_empty_or_control_text() {
        assert!(nonempty("").is_err());
        assert!(nonempty("node\nother").is_err());
        assert_eq!(nonempty("device-01").unwrap(), "device-01");
    }

    #[tokio::test]
    async fn readonly_status_does_not_create_node_or_runtime_directories() {
        let root = temp("status");
        let profile_path = root.join("profile");
        fs::create_dir(&profile_path).unwrap();
        let profile = fs::canonicalize(&profile_path).unwrap();
        privatefs::restrict_dir(&profile).unwrap();
        run_inner(DesktopNode {
            profile_dir: profile.clone(),
            expected_device_id: None,
            action: Action::Status,
        })
        .await
        .unwrap();
        assert!(!profile.join("node").exists());
        assert!(!profile.join("node-runtime").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn readonly_status_on_missing_profile_does_not_create_it() {
        let root = temp("missing-profile");
        let profile = root.join("not-created");
        run_inner(DesktopNode {
            profile_dir: profile.clone(),
            expected_device_id: None,
            action: Action::Status,
        })
        .await
        .unwrap();
        assert!(!profile.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sealed_runtime_rejects_tampering_and_wrong_target() {
        let root = temp("seal");
        let package = sealed_fixture(&root);
        let version = env!("CARGO_PKG_VERSION");
        let target = target().unwrap();
        assert!(crate::update::release::verify_sealed_package(&package, version, &target).is_ok());
        assert!(
            crate::update::release::verify_sealed_package(&package, version, "wrong-target")
                .is_err()
        );
        fs::write(package.join("web/index.html"), b"modified").unwrap();
        assert!(crate::update::release::verify_sealed_package(&package, version, &target).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runtime_update_availability_covers_current_newer_conflict_and_downgrade() {
        let root = temp("runtime-compare");
        let bundled = sealed_fixture_version(&root.join("bundled"), env!("CARGO_PKG_VERSION"));
        let installed = sealed_fixture_version(&root.join("installed"), "0.0.1-nightly.20261107.1");
        let newer =
            runtime_update_for_packages(&bundled, Some(&installed), None).unwrap();
        assert_eq!(newer.availability, "newer_available");
        let matching = sealed_fixture_version(&root.join("matching"), env!("CARGO_PKG_VERSION"));
        let current = runtime_update_for_packages(&matching, Some(&matching), None).unwrap();
        assert_eq!(current.availability, "current");
        let downgraded = sealed_fixture_version(&root.join("downgraded"), "0.0.1-nightly.20261106.1");
        let downgrade =
            runtime_update_for_packages(&downgraded, Some(&installed), None).unwrap();
        assert_eq!(downgrade.availability, "downgrade_refused");
        let same_version = sealed_fixture_version(&root.join("same"), "0.0.1-nightly.20261107.1");
        fs::write(same_version.join("web/index.html"), b"changed-bytes").unwrap();
        let conflict =
            runtime_update_for_packages(&same_version, Some(&installed), None).unwrap();
        assert_eq!(conflict.availability, "immutable_conflict");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runtime_staging_does_not_switch_current_until_requested() {
        let root = temp("stage-only");
        let managed = root.join("managed");
        fs::create_dir(&managed).unwrap();
        privatefs::restrict_dir(&managed).unwrap();
        let first = sealed_fixture_version(&root.join("first"), "0.0.1-nightly.20261107.1");
        let first_release = stage_runtime_release(&managed, &first).unwrap();
        crate::update::switch_managed_current(&managed, &first_release).unwrap();
        let current_before = managed_package_version(&managed_current(&managed).unwrap()).unwrap();
        let second = sealed_fixture_version(&root.join("second"), env!("CARGO_PKG_VERSION"));
        stage_runtime_release(&managed, &second).unwrap();
        let current_after = managed_package_version(&managed_current(&managed).unwrap()).unwrap();
        assert_eq!(current_before, current_after);
        assert_eq!(current_before, "0.0.1-nightly.20261107.1");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_runtime_copy_is_sealed_and_reconciles_repeat_install() {
        let root = temp("copy");
        let source = sealed_fixture(&root);
        let managed = root.join("managed-runtime");
        fs::create_dir(&managed).unwrap();
        privatefs::restrict_dir(&managed).unwrap();
        let first = ensure_runtime(&managed, &source).unwrap();
        let second = ensure_runtime(&managed, &source).unwrap();
        assert_eq!(first, second);
        let copied = fs::canonicalize(managed.join("current")).unwrap();
        assert!(crate::update::release::same_package_bytes(&source, &copied).unwrap());
        assert!(
            crate::update::release::verify_sealed_contents(&copied, &target().unwrap()).is_ok()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn valid_private_receipt_resolves_profile_service_across_runtime_switch() {
        let root = temp("receipt-lifecycle");
        let profile_path = root.join("profile");
        fs::create_dir(&profile_path).unwrap();
        privatefs::restrict_dir(&profile_path).unwrap();
        let profile = fs::canonicalize(profile_path).unwrap();
        let data = profile.join("node");
        fs::create_dir(&data).unwrap();
        privatefs::restrict_dir(&data).unwrap();
        let managed_root = profile.join("node-runtime");
        fs::create_dir(&managed_root).unwrap();
        privatefs::restrict_dir(&managed_root).unwrap();
        let source = sealed_fixture(&root);
        let current = ensure_runtime(&managed_root, &source).unwrap();
        crate::update::bind_desktop_install_root(&data, &managed_root).unwrap();

        let receipt = install_test_receipt(&profile, &data, &managed_root);
        privatefs::write_atomic(
            &receipt_path(&managed_root),
            &serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        let loaded = read_receipt(&managed_root).unwrap().unwrap();
        assert_eq!(loaded.service_id, service_id(&profile));

        let next_source = sealed_fixture_version(&root.join("next"), "0.0.2");
        let next_release = managed_root.join("releases/0.0.2");
        copy_tree(&next_source, &next_release).unwrap();
        crate::update::release::verify_sealed_contents(&next_release, &target().unwrap()).unwrap();
        crate::update::validate_desktop_package(&next_release).unwrap();
        privatefs::point_dir_link(&next_release, &managed_root.join("current")).unwrap();
        let managed = managed_service_for_data(&data).unwrap().unwrap();
        assert_eq!(managed.service_id, service_id(&profile));
        assert_eq!(managed.package, current);
        let command =
            service_command(&managed.package, &data, &managed.service_id, "stop").unwrap();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--service-id", managed.service_id.as_str()])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--data-dir", data.to_string_lossy().as_ref()])
        );
        assert!(args.windows(2).any(|pair| {
            pair == [
                "--binary",
                managed
                    .package
                    .join(crate::update::release::PROGRAMS[0])
                    .to_string_lossy()
                    .as_ref(),
            ]
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn desktop_policy_without_receipt_refuses_legacy_service_fallback() {
        let root = temp("missing-receipt-policy");
        let profile_path = root.join("profile");
        fs::create_dir(&profile_path).unwrap();
        privatefs::restrict_dir(&profile_path).unwrap();
        let profile = fs::canonicalize(profile_path).unwrap();
        let data = profile.join("node");
        fs::create_dir(&data).unwrap();
        privatefs::restrict_dir(&data).unwrap();
        let managed_root = profile.join("node-runtime");
        fs::create_dir(&managed_root).unwrap();
        privatefs::restrict_dir(&managed_root).unwrap();
        let policy = serde_json::json!({
            "channel": "nightly", "automatic": false, "install_root": managed_root
        });
        privatefs::write_atomic(
            &data.join("update.json"),
            &serde_json::to_vec(&policy).unwrap(),
        )
        .unwrap();

        assert!(managed_service_for_data(&data).is_err());
        privatefs::write_atomic(&receipt_path(&managed_root), b"{}").unwrap();
        assert!(managed_service_for_data(&data).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
