use crate::update;
use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use serde::Deserialize;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use update::release::{PROGRAMS, SERVICE_SCRIPT};
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
            "npm installations are updated with npm install -g @leaperone/vibeface@hourly"
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
        let binary = root.join("current").join(PROGRAMS[0]);
        ensure!(
            std::fs::canonicalize(&binary)? == std::fs::canonicalize(executable)?,
            "service management requires the current managed binary; run {}",
            binary.display()
        );
        root.join("current")
    };
    let binary = package.join(PROGRAMS[0]);
    let mut command = service_script(&package.join(SERVICE_SCRIPT))?;
    command
        .arg(action.argument())
        .arg("--data-dir")
        .arg(script_os_argument(data));
    if matches!(action, Action::Install | Action::InstallUpdater) {
        command.arg("--binary").arg(script_os_argument(&binary));
    }
    Ok(command)
}

/// Windows canonical paths can carry a verbatim `\\?\` prefix that PowerShell 5 `-File` and Task
/// Scheduler reject; spell them as ordinary DOS or UNC paths at the script boundary.
fn strip_verbatim_windows_path(path: &str) -> Cow<'_, str> {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        Cow::Owned(format!(r"\\{rest}"))
    } else if let Some(rest) = path.strip_prefix(r"\\?\").filter(|value| {
        let bytes = value.as_bytes();
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\'
    }) {
        Cow::Borrowed(rest)
    } else {
        Cow::Borrowed(path)
    }
}

fn script_os_argument(path: &Path) -> PathBuf {
    if cfg!(windows) {
        match path.to_str() {
            Some(path) => PathBuf::from(strip_verbatim_windows_path(path).as_ref()),
            None => path.to_path_buf(),
        }
    } else {
        path.to_path_buf()
    }
}

/// A command that runs the service script a package carries.
///
/// Unix runs the script directly. Windows has no script a process can start on its own, so the
/// shell that interprets it is named by its full path under the system directory, which keeps
/// anything earlier on the search path out of it.
pub(crate) fn service_script(script: &Path) -> Result<Command> {
    ensure!(
        script.is_file(),
        "this installation carries no service script: {}",
        script.display()
    );
    #[cfg(unix)]
    {
        Ok(Command::new(script))
    }
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot")
            .context("SystemRoot is required to run the service script")?;
        let shell = PathBuf::from(root).join("System32\\WindowsPowerShell\\v1.0\\powershell.exe");
        ensure!(
            shell.is_file(),
            "the service script needs Windows PowerShell: {}",
            shell.display()
        );
        let mut command = Command::new(shell);
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(script_os_argument(script));
        Ok(command)
    }
}

pub async fn run(options: Service) -> Result<()> {
    let data_dir = data_dir_or_home(options.data_dir)?;
    let data = std::fs::canonicalize(data_dir)
        .context("initialized data directory is required; run vibeface init first")?;
    inspect(&data).await?;
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
    use crate::tests::mark_runnable;
    #[cfg(windows)]
    use std::ffi::OsStr;
    use vibeface_bridge::platform::privatefs;

    #[test]
    fn strip_verbatim_windows_path_returns_dos_and_unc_spelling() {
        assert_eq!(
            strip_verbatim_windows_path(
                r"\\?\D:\Vibeface\current\scripts\install-node-service.ps1"
            ),
            r"D:\Vibeface\current\scripts\install-node-service.ps1"
        );
        assert_eq!(
            strip_verbatim_windows_path(r"\\?\UNC\server\share\data"),
            r"\\server\share\data"
        );
        assert_eq!(
            strip_verbatim_windows_path(r"D:\ordinary\path"),
            r"D:\ordinary\path"
        );
        assert_eq!(
            strip_verbatim_windows_path(r"\\?\Volume{fixture}\path"),
            r"\\?\Volume{fixture}\path"
        );
    }

    /// What `command` is expected to start to run the service script at `script`.
    fn expected_script_call(script: &Path) -> Vec<std::ffi::OsString> {
        #[cfg(unix)]
        {
            vec![script.as_os_str().to_owned()]
        }
        #[cfg(windows)]
        {
            let root = std::env::var_os("SystemRoot").unwrap();
            vec![
                PathBuf::from(root)
                    .join("System32\\WindowsPowerShell\\v1.0\\powershell.exe")
                    .into_os_string(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                script_os_argument(script).into_os_string(),
            ]
        }
    }

    #[test]
    #[cfg(windows)]
    fn command_spells_verbatim_windows_paths_for_script_arguments() {
        let root = std::env::temp_dir().join(format!("vibeface-verbatim-{}", uuid::Uuid::new_v4()));
        let package = root.join("releases/fixture");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(package.join("scripts")).unwrap();
        std::fs::write(package.join(PROGRAMS[0]), b"binary").unwrap();
        mark_runnable(&package.join(PROGRAMS[0]));
        std::fs::write(package.join(SERVICE_SCRIPT), b"script").unwrap();
        privatefs::link_dir(&package, &root.join("current")).unwrap();
        let data = root.join("data");
        std::fs::create_dir(&data).unwrap();
        let executable = package.join(PROGRAMS[0]);
        let install = command(
            &root.canonicalize().unwrap(),
            &data.canonicalize().unwrap(),
            &executable.canonicalize().unwrap(),
            Action::Install,
        )
        .unwrap();
        let args: Vec<_> = install
            .as_std()
            .get_args()
            .map(|value| value.to_owned())
            .collect();
        let verbatim = OsStr::new("--data-dir");
        let position = args.iter().position(|value| value == verbatim).unwrap();
        assert!(!args[position + 1].to_string_lossy().starts_with(r"\\?\"));
        let binary_flag = OsStr::new("--binary");
        let position = args.iter().position(|value| value == binary_flag).unwrap();
        assert!(!args[position + 1].to_string_lossy().starts_with(r"\\?\"));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Everything `command` built, the program first and then its arguments.
    fn whole_call(command: &Command) -> Vec<std::ffi::OsString> {
        let command = command.as_std();
        std::iter::once(command.get_program().to_owned())
            .chain(command.get_args().map(|argument| argument.to_owned()))
            .collect()
    }

    #[test]
    fn service_uses_current_and_preserves_argument_boundaries() {
        let root = std::env::temp_dir().join(format!("vibeface-service-{}", uuid::Uuid::new_v4()));
        let package = root.join("releases/fixture");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(package.join("scripts")).unwrap();
        std::fs::write(package.join(PROGRAMS[0]), b"binary").unwrap();
        mark_runnable(&package.join(PROGRAMS[0]));
        std::fs::write(package.join(SERVICE_SCRIPT), b"script").unwrap();
        privatefs::link_dir(&package, &root.join("current")).unwrap();
        let data = root.join("data with spaces");
        std::fs::create_dir(&data).unwrap();
        let executable = package.join(PROGRAMS[0]);
        let install = command(&root, &data, &executable, Action::Install).unwrap();
        let mut expected = expected_script_call(&root.join("current").join(SERVICE_SCRIPT));
        expected.extend([
            "install".into(),
            "--data-dir".into(),
            data.clone().into_os_string(),
            "--binary".into(),
            root.join("current").join(PROGRAMS[0]).into_os_string(),
        ]);
        assert_eq!(whole_call(&install), expected);
        let stop = command(&root, &data, &executable, Action::Stop).unwrap();
        assert_eq!(
            whole_call(&stop).len(),
            expected_script_call(Path::new("script")).len() + 3
        );
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
        let target = if cfg!(windows) {
            "x86_64-pc-windows-msvc"
        } else {
            "aarch64-apple-darwin"
        };
        let package = root.join("vendor").join(target);
        std::fs::create_dir_all(package.join("scripts")).unwrap();
        std::fs::create_dir(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/vibeface.mjs"), b"launcher").unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"@leaperone/vibeface","version":"1.2.3-hourly.2026101909"}"#,
        )
        .unwrap();
        std::fs::write(package.join(PROGRAMS[0]), b"binary").unwrap();
        std::fs::write(package.join(PROGRAMS[1]), b"core").unwrap();
        mark_runnable(&package.join(PROGRAMS[0]));
        mark_runnable(&package.join(PROGRAMS[1]));
        std::fs::create_dir(package.join("web")).unwrap();
        std::fs::write(package.join("web/index.html"), b"web").unwrap();
        std::fs::write(package.join(SERVICE_SCRIPT), b"script").unwrap();
        std::fs::write(
            package.join("BUILD-INFO.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": "1.2.3-hourly.2026101909", "target": target,
                "source_commit": "a".repeat(40), "core_version": "1.2.3-hourly.2026101909",
                "core_source_commit": "a".repeat(40), "app_version": "1.2.3-hourly.2026101909",
                "app_source_commit": "a".repeat(40),
                "update_protocol": vibeface_control::UPDATE_PROTOCOL,
                "data_epoch": vibeface_control::DATA_EPOCH,
            }))
            .unwrap(),
        )
        .unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let package = root.join("vendor").join(target);
        let binary = package.join(PROGRAMS[0]);
        let data = root.join("data with spaces");
        std::fs::create_dir(&data).unwrap();
        let running = update::lock_data_dir(&data).unwrap();
        assert!(command(&root, &data, &binary, Action::Install).is_err());
        assert!(command(&root, &data, &binary, Action::Stop).is_ok());
        drop(running);
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
        let installation = install().unwrap();
        let mut expected = expected_script_call(&package.join(SERVICE_SCRIPT));
        expected.extend([
            "install".into(),
            "--data-dir".into(),
            script_os_argument(&data).into_os_string(),
            "--binary".into(),
            script_os_argument(&binary).into_os_string(),
        ]);
        assert_eq!(whole_call(&installation), expected);
        assert!(command(&root, &data, &binary, Action::InstallUpdater).is_err());
        assert!(command(&root, &data, &binary, Action::UninstallUpdater).is_ok());
        assert!(command(&root.join("another-install"), &data, &binary, Action::Stop).is_err());
        let pending = data.join("update-pending.json");
        std::fs::write(&pending, b"{}").unwrap();
        let error = install().unwrap_err();
        assert!(error.to_string().contains("recover the pending update"));
        assert!(command(&root, &data, &binary, Action::Status).is_ok());
        assert_eq!(std::fs::read(&pending).unwrap(), b"{}");
        std::fs::remove_file(pending).unwrap();
        assert!(install().is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
}
