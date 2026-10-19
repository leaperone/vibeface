use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use vibeface::{config, update};

#[derive(Args)]
pub struct Service {
    /// Data directory bound to this user's single installed node service.
    #[arg(long, global = true, default_value_os_t = config::default_dir())]
    data_dir: PathBuf,
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

fn command(root: &Path, data: &Path, executable: &Path, action: Action) -> Result<Command> {
    let package = if let Some(npm_root) = update::npm_install_root(executable)? {
        ensure!(
            npm_root == root,
            "npm installation root does not match the program"
        );
        ensure!(
            !matches!(action, Action::InstallUpdater),
            "npm installations are updated with npm install -g vibeface@nightly"
        );
        if matches!(action, Action::Install) {
            drop(
                config::lock_data_dir(data)
                    .context("stop the node before installing its npm service")?,
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
    if matches!(action, Action::Install | Action::Start | Action::Restart) {
        config::Config::load(data)?;
    }
    let mut command = Command::new(package.join("scripts/install-node-service.sh"));
    command.arg(action.argument()).arg("--data-dir").arg(data);
    if matches!(action, Action::Install | Action::InstallUpdater) {
        command.arg("--binary").arg(binary);
    }
    Ok(command)
}

pub async fn run(options: Service) -> Result<()> {
    ensure!(
        options.data_dir.is_absolute(),
        "--data-dir must be absolute"
    );
    let data = std::fs::canonicalize(options.data_dir)
        .context("initialized data directory is required; run vibeface init first")?;
    config::identity(&data)?;
    let executable = std::env::current_exe()?;
    let npm_root = update::npm_install_root(&executable)?;
    let root = match &npm_root {
        Some(root) => root.clone(),
        None => update::managed_install_root(&data)?,
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
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        config::Config::local(
            "127.0.0.1:4790".parse().unwrap(),
            vec![project.clone()],
            false,
        )
        .save(&data)
        .unwrap();
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
        std::fs::remove_dir(project).unwrap();
        assert!(config::Config::load(&data).is_err());
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
            assert!(command(&root, &data, &executable, action).is_err());
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
            r#"{"name":"vibeface","version":"1.2.3-nightly.20261019.1"}"#,
        )
        .unwrap();
        std::fs::write(package.join("vibeface"), b"binary").unwrap();
        std::fs::write(package.join("scripts/install-node-service.sh"), b"script").unwrap();
        std::fs::write(
            package.join("BUILD-INFO.json"),
            r#"{"version":"1.2.3-nightly.20261019.1","update_protocol":2,"data_epoch":4}"#,
        )
        .unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let package = root.join("vendor/aarch64-apple-darwin");
        let binary = package.join("vibeface");
        let data = root.join("data with spaces");
        std::fs::create_dir(&data).unwrap();
        config::Config::local("127.0.0.1:4790".parse().unwrap(), vec![root.clone()], false)
            .save(&data)
            .unwrap();
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
        let running = config::lock_data_dir(&data).unwrap();
        assert!(command(&root, &data, &binary, Action::Install).is_err());
        assert!(command(&root, &data, &binary, Action::Stop).is_ok());
        drop(running);
        std::fs::remove_dir_all(root).unwrap();
    }
}
