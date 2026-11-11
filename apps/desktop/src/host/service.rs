use super::model::{NodeIdentity, NodeStatus, device_id, safe_path};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateAvailability {
    NotInstalled,
    Current,
    NewerAvailable,
    ImmutableConflict,
    DowngradeRefused,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeUpdate {
    pub bundled_version: String,
    pub installed_version: Option<String>,
    pub running_version: Option<String>,
    pub availability: UpdateAvailability,
    pub recovery_required: bool,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    NotInstalled,
    Installed,
    InstalledStopped,
    Partial,
    Conflict,
    Unavailable,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedNodeStatus {
    pub ok: bool,
    pub state: ServiceState,
    pub device_id: Option<String>,
    pub data_dir: Option<std::path::PathBuf>,
    pub service_id: Option<String>,
    pub managed_root: Option<std::path::PathBuf>,
    pub service_running: Option<bool>,
    pub core_status: Option<NodeStatus>,
    pub runtime_update: RuntimeUpdate,
}
impl ManagedNodeStatus {
    pub fn identity(&self) -> Option<NodeIdentity> {
        Some(NodeIdentity {
            data_dir: self.data_dir.clone()?,
            device_id: self.device_id.clone()?,
        })
    }
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(self.ok, "后台节点操作失败。");
        let update = &self.runtime_update;
        for version in std::iter::once(Some(&update.bundled_version)).chain([
            update.installed_version.as_ref(),
            update.running_version.as_ref(),
        ]) {
            if let Some(version) = version {
                ensure!(
                    !version.is_empty()
                        && version.len() <= 160
                        && !version.chars().any(char::is_control),
                    "后台节点版本无效。"
                );
            }
        }
        if self.state == ServiceState::NotInstalled {
            ensure!(
                self.device_id.is_none()
                    && self.data_dir.is_none()
                    && self.service_id.is_none()
                    && self.managed_root.is_none()
                    && self.service_running.is_none()
                    && self.core_status.is_none()
                    && update.availability == UpdateAvailability::NotInstalled
                    && !update.recovery_required,
                "未安装状态包含无效数据。"
            );
        } else {
            self.identity().context("后台节点身份缺失。")?.validate()?;
            safe_path(
                self.managed_root
                    .as_deref()
                    .context("后台节点运行目录缺失。")?,
            )?;
            let id = self
                .service_id
                .as_deref()
                .context("后台节点服务标识缺失。")?;
            ensure!(
                !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control),
                "后台节点服务标识无效。"
            );
            if matches!(
                self.state,
                ServiceState::Installed | ServiceState::InstalledStopped | ServiceState::Partial
            ) {
                ensure!(self.service_running.is_some(), "后台节点服务状态缺失。");
            }
        }
        Ok(())
    }
    pub(crate) fn manageable(&self) -> bool {
        matches!(
            self.state,
            ServiceState::Installed | ServiceState::InstalledStopped | ServiceState::Partial
        )
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundAction {
    Status,
    Install,
    Update,
    Start,
    Stop,
    Uninstall,
}
impl BackgroundAction {
    fn argument(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Install => "install",
            Self::Update => "update",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Uninstall => "uninstall",
        }
    }
}
async fn read_bounded(mut stream: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(output);
        }
        ensure!(
            output.len() + read <= 64 * 1024,
            "后台节点命令输出超出范围。"
        );
        output.extend_from_slice(&buffer[..read]);
    }
}
pub(crate) async fn invoke(
    cli: &Path,
    profile: &Path,
    action: BackgroundAction,
    expected: Option<&str>,
) -> Result<ManagedNodeStatus> {
    let mut command = Command::new(cli);
    command
        .arg("desktop-node")
        .arg("--profile-dir")
        .arg(profile);
    if action != BackgroundAction::Status {
        let id = expected
            .context("更改后台节点需要已验证身份。")
            .and_then(|id| {
                device_id(id)?;
                Ok(id)
            })?;
        command.arg("--expected-device-id").arg(id);
    }
    command
        .arg(action.argument())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command.spawn().context("无法运行随包 CLI。")?;
    let stdout = child.stdout.take().context("CLI 输出管道缺失。")?;
    let stderr = child.stderr.take().context("CLI 错误管道缺失。")?;
    let timeout = Duration::from_secs(if action == BackgroundAction::Update {
        120
    } else {
        30
    });
    let result = tokio::time::timeout(timeout, async {
        let (output, _, status) =
            tokio::try_join!(read_bounded(stdout), read_bounded(stderr), async {
                Ok::<_, anyhow::Error>(child.wait().await?)
            })?;
        ensure!(status.success(), "后台节点操作未成功，请刷新状态核对。");
        let status: ManagedNodeStatus =
            serde_json::from_slice(&output).context("后台节点返回无效结果。")?;
        status.validate()?;
        Ok::<_, anyhow::Error>(status)
    })
    .await;
    match result {
        Ok(Ok(status)) => Ok(status),
        failure => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            match failure {
                Ok(Err(error)) => Err(error),
                _ => bail!("后台节点操作超时，结果未知。请刷新状态核对，勿重复操作。"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibeface_bridge::platform::privatefs;

    #[tokio::test]
    async fn cli_spawn_failure_retains_the_underlying_os_error_without_command_arguments()
    -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("vf-cli-spawn-error-{}", uuid::Uuid::new_v4()));
        privatefs::create_dir_new(&root)?;
        let binary = root.join(if cfg!(windows) {
            "missing-cli.exe"
        } else {
            "missing-cli"
        });
        let error = invoke(&binary, &root, BackgroundAction::Status, None)
            .await
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::NotFound)
        );
        assert!(error.to_string().contains("无法运行随包 CLI"));
        assert!(!format!("{error:#}").contains("--profile-dir"));
        assert!(!format!("{error:#}").contains(root.to_string_lossy().as_ref()));
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
