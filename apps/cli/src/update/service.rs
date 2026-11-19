#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
use anyhow::bail;
use anyhow::{Context, Result, ensure};
#[cfg(any(target_os = "macos", test))]
use std::fs;
use std::path::Path;
#[cfg(any(target_os = "macos", windows))]
use std::path::PathBuf;
use tokio::process::Command;
use vibeface_control::inspection::NodeIdentity;

pub(super) struct Service {
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

pub(super) trait ServiceControl {
    async fn stop(&self) -> Result<()>;
    async fn start(&self) -> Result<()>;
    async fn device_id(&self, data: &Path) -> Result<String> {
        Ok(super::inspect(data, &super::core_binary()?)
            .await?
            .device_id)
    }
    async fn healthy(&self, data: &Path, package: &Path, device_id: &str) -> Result<()> {
        super::healthy(data, package, device_id).await
    }
    async fn commit(&self, data: &Path) -> Result<()> {
        super::maintenance_commit(data).await
    }
}

impl Service {
    pub(super) fn desktop(desktop: crate::desktop_node::ManagedService) -> Self {
        Self {
            desktop: Some(desktop),
            #[cfg(target_os = "macos")]
            domain: String::new(),
            #[cfg(target_os = "macos")]
            plist: PathBuf::new(),
        }
    }

    pub(super) async fn load(data: &Path) -> Result<Self> {
        if let Some(desktop) = crate::desktop_node::managed_service_for_data(data)? {
            let binary = super::core_binary()?;
            super::inspect(data, &binary)
                .await?
                .require_identity(&NodeIdentity {
                    data_dir: data.to_owned(),
                    device_id: desktop.device_id.clone(),
                })
                .context("Desktop Node identity changed since service installation")?;
            ensure!(
                crate::desktop_node::managed_service_registered(&desktop).await?,
                "Desktop Node service registration is missing"
            );
            return Ok(Self::desktop(desktop));
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
pub(super) fn task_command_line(definition: &[u8]) -> Result<String> {
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
pub(super) fn task_data_matches(line: &str, data: &Path) -> bool {
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
                super::checked(
                    Command::new("/bin/launchctl").args(["bootout", &label]),
                    "stop node service",
                )
                .await?;
            }
            Ok(())
        }
        #[cfg(target_os = "linux")]
        {
            super::checked(
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
                super::checked(
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
                super::checked(
                    Command::new("/bin/launchctl").args(["kickstart", &label]),
                    "start node service",
                )
                .await
            } else {
                super::checked(
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
            super::checked(
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
            super::checked(
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
pub(super) fn service_data_matches(args: &[String], data: &Path) -> bool {
    args.get(1).is_some_and(|s| s == "serve")
        && args.iter().filter(|s| *s == "--data-dir").count() == 1
        && args.windows(2).any(|pair| {
            pair[0] == "--data-dir" && fs::canonicalize(&pair[1]).is_ok_and(|actual| actual == data)
        })
}
