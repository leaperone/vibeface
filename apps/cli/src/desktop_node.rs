//! App-private durable Node provisioning. This command is intentionally hidden from the public
//! CLI surface; the Electron main process is its only caller.
use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
    time::Duration,
};
use vibeface_bridge::platform::privatefs;
use vibeface_control::{ControlRequest, DATA_EPOCH, UPDATE_PROTOCOL};

const RECEIPT: &str = "desktop-node.json";
const MAX_RECEIPT: u64 = 16 * 1024;

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
    Start,
    Stop,
    Uninstall,
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
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: u32,
    device_id: String,
    data_dir: PathBuf,
    managed_root: PathBuf,
    service_id: String,
    version: String,
    source_commit: String,
    update_protocol: u32,
    data_epoch: u32,
}

#[derive(Deserialize)]
struct InspectResult {
    data_dir: PathBuf,
    device_id: String,
    status: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeService {
    installed: bool,
    running: bool,
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
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => other,
    };
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

/// Acquire the existing data lock without creating it. Success proves no Core currently holds it.
fn require_data_unlocked(data: &Path) -> Result<()> {
    let path = data.join("node.lock");
    let metadata = fs::symlink_metadata(&path)
        .context("cannot verify that the App-owned Node data lock exists")?;
    ensure!(
        metadata.file_type().is_file() && privatefs::is_private(&path)?,
        "Node data lock is not a private regular file"
    );
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .context("cannot verify that the App-owned Node data lock is free")?;
    file.try_lock()
        .context("Node data is in use; stop the App-owned child and retry")?;
    file.unlock()?;
    Ok(())
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

fn service_id(profile: &Path) -> String {
    let hash = Sha256::digest(profile.as_os_str().to_string_lossy().as_bytes());
    format!("desktop-{}", hex::encode(&hash[..10]))
}

fn receipt_path(root: &Path) -> PathBuf {
    root.join(RECEIPT)
}

fn read_receipt(root: &Path) -> Result<Option<Receipt>> {
    let path = receipt_path(root);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_dir()
                    && !metadata.file_type().is_symlink()
                    && privatefs::is_private(root)?,
                "managed runtime root is unsafe"
            );
            metadata
        }
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

fn package_version(package: &Path) -> Result<(String, String)> {
    let build = crate::update::release::verify_sealed_package(
        package,
        env!("CARGO_PKG_VERSION"),
        &target()?,
    )?;
    Ok((build.version, build.source_commit))
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

fn ensure_runtime(root: &Path, source: &Path) -> Result<PathBuf> {
    let root = fs::canonicalize(root)?;
    let (version, _) = package_version(source)?;
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
            package_version(&staging)?;
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
        package_version(&destination)?;
        crate::update::validate_desktop_package(&destination)?;
        ensure!(
            crate::update::release::same_package_bytes(source, &destination)?,
            "existing runtime version has different contents"
        );
    }
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
    privatefs::point_dir_link(&destination, &current)?;
    Ok(current)
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

fn service_command(
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
    let mut command = service_command(package, data, id, "verify")?;
    let output = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .context("inspect current-user Node service registration")?;
    ensure!(
        output.status.success() && output.stdout.len() <= 4096,
        "service registration does not match the Desktop receipt"
    );
    let result: NativeService =
        serde_json::from_slice(&output.stdout).context("invalid service status")?;
    Ok(result)
}

async fn run_service(package: &Path, data: &Path, id: &str, action: &str) -> Result<()> {
    let status = service_command(package, data, id, action)?
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .context("run current-user Node service manager")?;
    ensure!(
        status.success(),
        "current-user Node service operation failed"
    );
    Ok(())
}

async fn wait_health(data: &Path, expected: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(result) = inspect(data).await
            && result.device_id == expected
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("background Node did not become healthy with its existing identity");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn output(
    state: &'static str,
    receipt: Option<&Receipt>,
    service_running: Option<bool>,
    core_status: Option<String>,
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
        })?
    );
    Ok(())
}

pub async fn run(options: DesktopNode) -> Result<()> {
    match run_inner(options).await {
        Ok(()) => Ok(()),
        Err(_) => anyhow::bail!("desktop_node_operation_failed"),
    }
}

async fn run_inner(options: DesktopNode) -> Result<()> {
    if matches!(options.action, Action::Status)
        && matches!(
            fs::symlink_metadata(&options.profile_dir),
            Err(ref error) if error.kind() == io::ErrorKind::NotFound
        )
    {
        return output("not_installed", None, None, None);
    }
    let profile = canonical_profile(&options.profile_dir)?;
    let root = profile.join("node-runtime");
    let existing = read_receipt(&root)?;
    match options.action {
        Action::Status => {
            let Some(receipt) = existing.as_ref() else {
                return output("not_installed", None, None, None);
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
                return output("conflict", Some(receipt), None, None);
            }
            if !matches!(
                crate::update::desktop_policy_matches(&data, &receipt.managed_root),
                Ok(true)
            ) {
                return output("conflict", Some(receipt), None, None);
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
                    output(
                        state,
                        Some(receipt),
                        Some(native.running),
                        Some(core.status),
                    )
                }
                (Ok(native), Ok(core)) => output(
                    "conflict",
                    Some(receipt),
                    Some(native.running),
                    Some(core.status),
                ),
                (Err(_), _) => output("conflict", Some(receipt), None, None),
                (_, Err(_)) => output("unavailable", Some(receipt), None, None),
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
                            output(
                                "installed",
                                Some(receipt),
                                Some(true),
                                Some("running".into()),
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
                    let (version, source_commit) = package_version(&package)?;
                    let receipt = Receipt {
                        schema: 1,
                        device_id: expected.into(),
                        data_dir: data.clone(),
                        managed_root: root.clone(),
                        service_id: service_id(&profile),
                        version,
                        source_commit,
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
                    output(
                        "installed",
                        Some(&receipt),
                        Some(true),
                        Some("running".into()),
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
                    output(
                        "installed",
                        Some(receipt),
                        Some(true),
                        Some("running".into()),
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
                    require_data_unlocked(&data)?;
                    let native = native_service(&package, &data, &receipt.service_id).await?;
                    ensure!(
                        native.installed && !native.running,
                        "background service manager did not confirm the stopped Node"
                    );
                    output(
                        "installed_stopped",
                        Some(receipt),
                        Some(false),
                        Some("stopped".into()),
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
                    require_data_unlocked(&data)?;
                    let native = native_service(&package, &data, &receipt.service_id).await?;
                    ensure!(
                        !native.installed,
                        "background service registration remains after removal"
                    );
                    fs::remove_file(receipt_path(&receipt.managed_root))?;
                    output("not_installed", None, Some(false), Some("stopped".into()))
                }
                Action::Status => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "vibeface-desktop-node-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&path).unwrap();
        privatefs::restrict_dir(&path).unwrap();
        path
    }

    fn sealed_fixture(root: &Path) -> PathBuf {
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
            "core_version": env!("CARGO_PKG_VERSION"), "core_source_commit": source_commit,
            "app_version": env!("CARGO_PKG_VERSION"), "app_source_commit": source_commit,
            "version": env!("CARGO_PKG_VERSION"), "target": target().unwrap(),
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
        let profile = root.join("profile");
        fs::create_dir(&profile).unwrap();
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
}
