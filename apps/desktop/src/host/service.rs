use super::model::device_id;
use anyhow::{Context, Result, bail, ensure};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

pub use vibeface_control::managed_node::{ManagedNodeStatus, ServiceState, UpdateAvailability};
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

#[cfg(all(test, unix))]
mod contract_tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    const NOT_INSTALLED: &str = r#"{"state":"not_installed","device_id":null,"data_dir":null,"service_id":null,"managed_root":null,"service_running":null,"core_status":null,"runtime_update":{"bundled_version":"2.0.0","installed_version":null,"running_version":null,"availability":"not_installed","recovery_required":false}}"#;
    const INSTALLED: &str = r#"{"state":"installed","device_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","data_dir":DATA,"service_id":"fixture.service","managed_root":ROOT,"service_running":true,"core_status":"running","runtime_update":{"bundled_version":"2.0.0","installed_version":"2.0.0","running_version":"2.0.0","availability":"current","recovery_required":false}}"#;

    struct Fixture {
        root: std::path::PathBuf,
        cli: std::path::PathBuf,
    }
    impl Fixture {
        fn new() -> Result<Self> {
            let root =
                std::env::temp_dir().join(format!("vf-managed-contract-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&root)?;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
            let cli = root.join("cli");
            fs::write(
                &cli,
                r#"#!/bin/sh
set -eu
fixture_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
[ "$1" = desktop-node ] && [ "$2" = --profile-dir ] && [ "$3" = "$fixture_dir" ] || exit 91
printf '%s\n' "$@" >> "$fixture_dir/calls"
cat "$fixture_dir/reply"
exit "$(cat "$fixture_dir/exit-code")"
"#,
            )?;
            fs::set_permissions(&cli, fs::Permissions::from_mode(0o700))?;
            Ok(Self { root, cli })
        }
        fn literal(&self, text: &str) -> Result<String> {
            Ok(text
                .replace("DATA", &serde_json::to_string(&self.root.join("node"))?)
                .replace("ROOT", &serde_json::to_string(&self.root.join("managed"))?))
        }
        async fn run(&self, text: &str, code: u8) -> Result<ManagedNodeStatus> {
            fs::write(self.root.join("reply"), self.literal(text)?)?;
            fs::write(self.root.join("exit-code"), code.to_string())?;
            let result = invoke(&self.cli, &self.root, BackgroundAction::Status, None).await;
            assert_eq!(
                fs::read_to_string(self.root.join("calls"))?,
                format!(
                    "desktop-node\n--profile-dir\n{}\nstatus\n",
                    self.root.display()
                )
            );
            result
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn managed_child_success_requires_complete_consistent_literal_contract() -> Result<()> {
        let fixture = Fixture::new()?;
        assert_eq!(
            fixture.run(NOT_INSTALLED, 0).await?.state,
            ServiceState::NotInstalled
        );
        let fixture = Fixture::new()?;
        let installed = fixture.run(INSTALLED, 0).await?;
        assert!(installed.manageable());
        assert_eq!(installed.identity().unwrap().device_id, "a".repeat(64));
        // Core health and service registration are independent observations.
        for wire in [
            INSTALLED
                .replace(
                    "\"core_status\":\"running\"",
                    "\"core_status\":\"unavailable\"",
                )
                .replace("\"running_version\":\"2.0.0\"", "\"running_version\":null"),
            INSTALLED
                .replace("\"state\":\"installed\"", "\"state\":\"installed_stopped\"")
                .replace("\"service_running\":true", "\"service_running\":false")
                .replace("\"running_version\":\"2.0.0\"", "\"running_version\":null"),
            INSTALLED
                .replace("\"state\":\"installed\"", "\"state\":\"conflict\"")
                .replace("\"service_running\":true", "\"service_running\":null")
                .replace("\"core_status\":\"running\"", "\"core_status\":null")
                .replace("\"running_version\":\"2.0.0\"", "\"running_version\":null"),
            INSTALLED
                .replace("\"state\":\"installed\"", "\"state\":\"unavailable\"")
                .replace("\"service_running\":true", "\"service_running\":null")
                .replace("\"core_status\":\"running\"", "\"core_status\":null")
                .replace("\"running_version\":\"2.0.0\"", "\"running_version\":null"),
        ] {
            Fixture::new()?.run(&wire, 0).await?;
        }
        for (wire, from, to) in [
            (NOT_INSTALLED, "\"device_id\":null,", ""),
            (NOT_INSTALLED, "\"installed_version\":null,", ""),
            (
                NOT_INSTALLED,
                "\"device_id\":null",
                "\"device_id\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
            ),
            (
                NOT_INSTALLED,
                "\"installed_version\":null",
                "\"installed_version\":\"2.0.0\"",
            ),
            (
                NOT_INSTALLED,
                "\"running_version\":null",
                "\"running_version\":\"2.0.0\"",
            ),
            (
                NOT_INSTALLED,
                "\"recovery_required\":false",
                "\"recovery_required\":true",
            ),
            (NOT_INSTALLED, "\"data_dir\":null,", ""),
            (NOT_INSTALLED, "\"service_id\":null,", ""),
            (NOT_INSTALLED, "\"managed_root\":null,", ""),
            (NOT_INSTALLED, "\"service_running\":null,", ""),
            (NOT_INSTALLED, "\"core_status\":null,", ""),
            (NOT_INSTALLED, "\"running_version\":null,", ""),
            (NOT_INSTALLED, "\"state\":", "\"ok\":true,\"state\":"),
            (
                INSTALLED,
                "\"state\":\"installed\"",
                "\"state\":\"unknown\"",
            ),
            (
                INSTALLED,
                "\"availability\":\"current\"",
                "\"availability\":\"unknown\"",
            ),
            (
                INSTALLED,
                "\"service_running\":true",
                "\"service_running\":null",
            ),
            (
                INSTALLED,
                "\"core_status\":\"running\"",
                "\"core_status\":null",
            ),
            (
                INSTALLED,
                "\"service_id\":\"fixture.service\"",
                "\"service_id\":null",
            ),
            (
                INSTALLED,
                "\"installed_version\":\"2.0.0\"",
                "\"installed_version\":null",
            ),
            (
                INSTALLED,
                "\"running_version\":\"2.0.0\"",
                "\"running_version\":\"1.0.0\"",
            ),
        ] {
            assert!(
                Fixture::new()?
                    .run(&wire.replace(from, to), 0)
                    .await
                    .is_err(),
                "{from} -> {to}"
            );
        }
        assert!(Fixture::new()?.run(INSTALLED, 7).await.is_err());
        assert!(Fixture::new()?.run("{broken", 0).await.is_err());
        Ok(())
    }
}
