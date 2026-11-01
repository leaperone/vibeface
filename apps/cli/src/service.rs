use crate::update;
use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use vibeface_control::{self, ControlRequest};

#[derive(Args)]
pub struct Service {
    /// Data directory bound to this user's single installed node service.
    /// Defaults to ~/.vibeface.
    #[arg(long, global = true, env = "VIBEFACE_DATA_DIR", value_parser = absolute_path)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand, Clone, Copy)]
enum Action {
    /// Install and start a user service using this installation's native binary.
    Install,
    /// Start the installed node service.
    Start,
    /// Stop the node service without deleting its configuration or data.
    Stop,
    /// Restart the installed node service.
    Restart,
    /// Show the native service manager's node and updater status.
    Status,
    /// Remove the node service and updater, keeping all node data.
    Uninstall,
    /// Install the update timer; update configure selects its policy.
    InstallUpdater,
    /// Remove the update timer without changing its saved policy.
    UninstallUpdater,
}

impl Action {
    fn argument(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Status => "status",
            Self::Uninstall => "uninstall",
            Self::InstallUpdater => "install-updater",
            Self::UninstallUpdater => "uninstall-updater",
        }
    }
}

#[derive(Debug, Deserialize)]
struct InspectResult {
    data_dir: PathBuf,
    device_id: String,
    maintenance_socket: PathBuf,
}

fn absolute_path(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("path must be absolute".into())
    }
}

fn data_dir_or_home(dir: Option<PathBuf>) -> Result<PathBuf> {
    let dir = dir
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".vibeface")))
        .unwrap_or_else(|| PathBuf::from(".vibeface"));
    ensure!(
        !dir.as_os_str().is_empty() && dir.is_absolute(),
        "specify an absolute data directory"
    );
    Ok(dir)
}

fn core_binary() -> Result<PathBuf> {
    vibeface_control::sibling_core(&std::env::current_exe()?)
}

async fn inspect(data: &Path) -> Result<InspectResult> {
    let value = vibeface_control::request(
        &core_binary()?,
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
    ensure!(
        !result.maintenance_socket.as_os_str().is_empty(),
        "core inspect returned no maintenance socket"
    );
    Ok(result)
}

fn command(root: &Path, data: &Path, executable: &Path, action: Action) -> Result<Command> {
    let package = if let Some(npm_root) = update::npm_install_root(executable)? {
        ensure!(
            npm_root == root,
            "npm installation root does not match the program"
        );
        ensure!(
            !matches!(action, Action::InstallUpdater),
            "npm installations are updated with npm install -g @leaperone/vibeface@nightly"
        );
        if matches!(action, Action::Install) {
            let _idle = update::lock_data_dir(data)
                .context("stop the node before installing its npm service")?;
            ensure!(
                !data.join("update-pending.json").exists(),
                "recover the pending update with the previous managed installation before installing its npm service"
            );
        }
        executable
            .parent()
            .context("npm native package is missing")?
            .to_path_buf()
    } else {
        let binary = root.join("current/vibeface");
        ensure!(
            std::fs::canonicalize(&binary)? == std::fs::canonicalize(executable)?,
            "service management requires the current managed binary; run {}",
            binary.display()
        );
        root.join("current")
    };
    let binary = package.join("vibeface");
    let mut command = Command::new(package.join("scripts/install-node-service.sh"));
    command.arg(action.argument()).arg("--data-dir").arg(data);
    if matches!(action, Action::Install | Action::InstallUpdater) {
        command.arg("--binary").arg(binary);
    }
    Ok(command)
}

pub async fn run(options: Service) -> Result<()> {
    let data_dir = data_dir_or_home(options.data_dir)?;
    let data = std::fs::canonicalize(data_dir)
        .context("initialized data directory is required; run vibeface init first")?;
    let inspected = inspect(&data).await?;
    ensure!(
        inspected.data_dir == data,
        "core inspected a different data directory"
    );
    let executable = std::env::current_exe()?;
    let npm_root = update::npm_install_root(&executable)?;
    let root = match &npm_root {
        Some(root) => root.clone(),
        None => update::managed_install_root(&data)?,
    };
    let _update_lock = if npm_root.is_some() && matches!(options.action, Action::Install) {
        Some(
            update::update_lock(&data)
                .context("wait for the updater before installing the npm service")?,
        )
    } else {
        None
    };
    let mut operation = command(&root, &data, &executable, options.action)?;
    if npm_root.is_some() && matches!(options.action, Action::Install) {
        let status = command(&root, &data, &executable, Action::UninstallUpdater)?
            .status()
            .await
            .context("disable the managed updater for the npm installation")?;
        ensure!(
            status.success(),
            "could not disable the previous updater: {status}"
        );
    }
    let status = operation
        .status()
        .await
        .context("run user service manager")?;
    ensure!(status.success(), "user service command failed: {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_uses_current_and_preserves_argument_boundaries() {
        let root = std::env::temp_dir().join(format!("vibeface-service-{}", uuid::Uuid::new_v4()));
        let package = root.join("releases/fixture");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("vibeface"), b"binary").unwrap();
        std::os::unix::fs::symlink(&package, root.join("current")).unwrap();
        let data = root.join("data with spaces");
        std::fs::create_dir(&data).unwrap();
        let executable = package.join("vibeface");
        let install = command(&root, &data, &executable, Action::Install).unwrap();
        let install = install.as_std();
        assert_eq!(
            install.get_program(),
            root.join("current/scripts/install-node-service.sh")
        );
        assert_eq!(
            install.get_args().collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new("install"),
                std::ffi::OsStr::new("--data-dir"),
                data.as_os_str(),
                std::ffi::OsStr::new("--binary"),
                root.join("current/vibeface").as_os_str(),
            ]
        );
        let stop = command(&root, &data, &executable, Action::Stop).unwrap();
        assert_eq!(stop.as_std().get_args().count(), 3);
        let unrelated = root.join("local-build");
        std::fs::write(&unrelated, b"another binary").unwrap();
        assert!(command(&root, &data, &unrelated, Action::Install).is_err());
        for action in [
            Action::Stop,
            Action::Status,
            Action::Uninstall,
            Action::InstallUpdater,
            Action::UninstallUpdater,
        ] {
            assert!(command(&root, &data, &executable, action).is_ok());
        }
        for action in [Action::Install, Action::Start, Action::Restart] {
            assert!(command(&root, &data, &executable, action).is_ok());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn npm_service_uses_the_native_package_and_rejects_a_second_updater() {
        let root =
            std::env::temp_dir().join(format!("vibeface-npm-service-{}", uuid::Uuid::new_v4()));
        let package = root.join("vendor/aarch64-apple-darwin");
        std::fs::create_dir_all(package.join("scripts")).unwrap();
        std::fs::create_dir(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/vibeface.mjs"), b"launcher").unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"@leaperone/vibeface","version":"1.2.3-nightly.20261019.1"}"#,
        )
        .unwrap();
        std::fs::write(package.join("vibeface"), b"binary").unwrap();
        std::fs::write(package.join("vibeface-core"), b"core").unwrap();
        std::fs::set_permissions(
            package.join("vibeface"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::fs::set_permissions(
            package.join("vibeface-core"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::fs::create_dir(package.join("web")).unwrap();
        std::fs::write(package.join("web/index.html"), b"web").unwrap();
        std::fs::write(package.join("scripts/install-node-service.sh"), b"script").unwrap();
        std::fs::write(
            package.join("BUILD-INFO.json"),
            format!(
                r#"{{"version":"1.2.3-nightly.20261019.1","core_version":"2.0.0","update_protocol":{},"data_epoch":{}}}"#,
                vibeface_control::UPDATE_PROTOCOL,
                vibeface_control::DATA_EPOCH
            ),
        )
        .unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let package = root.join("vendor/aarch64-apple-darwin");
        let binary = package.join("vibeface");
        let data = root.join("data with spaces");
        std::fs::create_dir(&data).unwrap();
        let install = command(&root, &data, &binary, Action::Install).unwrap();
        assert_eq!(
            install.as_std().get_program(),
            package.join("scripts/install-node-service.sh")
        );
        assert_eq!(
            install.as_std().get_args().collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new("install"),
                std::ffi::OsStr::new("--data-dir"),
                data.as_os_str(),
                std::ffi::OsStr::new("--binary"),
                binary.as_os_str(),
            ]
        );
        assert!(command(&root, &data, &binary, Action::InstallUpdater).is_err());
        assert!(command(&root, &data, &binary, Action::UninstallUpdater).is_ok());
        assert!(command(&root.join("another-install"), &data, &binary, Action::Stop).is_err());
        let running = update::lock_data_dir(&data).unwrap();
        assert!(command(&root, &data, &binary, Action::Install).is_err());
        assert!(command(&root, &data, &binary, Action::Stop).is_ok());
        drop(running);
        let pending = data.join("update-pending.json");
        std::fs::write(&pending, b"{}").unwrap();
        // A child another test thread is spawning may still hold the released lock, as in
        // `Runtime::reopen`.
        let install = || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match command(&root, &data, &binary, Action::Install) {
                    Err(error)
                        if matches!(
                            error.downcast_ref(),
                            Some(std::fs::TryLockError::WouldBlock)
                        ) && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    result => return result,
                }
            }
        };
        let error = install().unwrap_err();
        assert!(error.to_string().contains("recover the pending update"));
        assert!(command(&root, &data, &binary, Action::Status).is_ok());
        assert_eq!(std::fs::read(&pending).unwrap(), b"{}");
        std::fs::remove_file(pending).unwrap();
        assert!(install().is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
}
