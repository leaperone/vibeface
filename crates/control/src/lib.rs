//! Protocols shared by the human CLI and the Vibeface core process.
//!
//! This crate deliberately contains no data store, network daemon, scheduler, or provider
//! executor.  It only knows bounded wire values and the direct App Server client used by both
//! processes.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::Write as _,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
};

pub const UPDATE_PROTOCOL: u32 = 1;
pub const DATA_EPOCH: u32 = 12;
pub const MAX_CONTROL_PACKET: usize = 1024 * 1024;
const CONTROL_IO_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// A bounded request handled by a standalone core process.  The core owns all data access and
/// turns these DTOs into its internal Config, identity and Store values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlRequest {
    Init {
        options: Value,
    },
    Inspect {
        data_dir: PathBuf,
    },
    Access {
        data_dir: PathBuf,
        json: bool,
    },
    SetLocalAccess {
        data_dir: PathBuf,
        enabled: bool,
    },
    Join {
        data_dir: PathBuf,
        link: String,
        name: String,
    },
    Wallet {
        data_dir: PathBuf,
        operation: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ControlResponse {
    pub fn success(result: impl Into<Value>) -> Self {
        Self {
            ok: true,
            result: Some(result.into()),
            error: None,
        }
    }

    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            result: None,
            error: Some(error.into()),
        }
    }

    pub fn into_result(self) -> Result<Value> {
        if self.ok {
            return Ok(self.result.unwrap_or(Value::Null));
        }
        bail!(
            self.error
                .unwrap_or_else(|| "core control request failed".into())
        )
    }
}

async fn read_bounded<R>(mut stream: R, limit: usize, label: &str, forward: bool) -> Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            return Ok(bytes);
        }
        ensure!(
            bytes.len().saturating_add(count) <= limit,
            "core control {label} exceeds {limit} bytes"
        );
        if forward {
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(&chunk[..count]);
            let _ = stderr.flush();
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
}

async fn wait_for_exit(child: &mut Child, timeout: Option<Duration>) -> Result<ExitStatus> {
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, child.wait())
            .await
            .context("core control process did not exit before the deadline")?
            .map_err(Into::into),
        None => Ok(child.wait().await?),
    }
}

/// Execute one bounded request in the sibling core binary and decode its single JSON response.
pub async fn request(core: &Path, request: &ControlRequest) -> Result<Value> {
    ensure!(core.is_absolute(), "core executable path must be absolute");
    let mut child = Command::new(core)
        .arg("--control")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("start core control process: {}", core.display()))?;
    let mut stdin = child
        .stdin
        .take()
        .context("core control stdin unavailable")?;
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() <= MAX_CONTROL_PACKET,
        "core control request is too large"
    );
    match tokio::time::timeout(CONTROL_WRITE_TIMEOUT, stdin.write_all(&bytes)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(error.into());
        }
        Err(error) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(error).context("core control stdin timed out");
        }
    }
    drop(stdin);
    let stdout = child
        .stdout
        .take()
        .context("core control stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("core control stderr unavailable")?;
    let forward_stderr = matches!(request, ControlRequest::Join { .. });
    let streams = async {
        tokio::try_join!(
            read_bounded(stdout, MAX_CONTROL_PACKET, "stdout", false),
            read_bounded(stderr, MAX_CONTROL_PACKET, "stderr", forward_stderr),
        )
    };
    let streams = if forward_stderr {
        streams.await
    } else {
        match tokio::time::timeout(CONTROL_IO_TIMEOUT, streams).await {
            Ok(streams) => streams,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                bail!("core control request timed out; outcome may be unknown")
            }
        }
    };
    let (output, error_output) = match streams {
        Ok(streams) => streams,
        Err(error) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(error);
        }
    };
    let status = match wait_for_exit(&mut child, (!forward_stderr).then_some(CONTROL_IO_TIMEOUT))
        .await
    {
        Ok(status) => status,
        Err(error) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(error).context("core control request timed out; outcome may be unknown");
        }
    };
    if !status.success() {
        bail!(
            "core control process failed: {}",
            String::from_utf8_lossy(&error_output)
        );
    }
    let response: ControlResponse =
        serde_json::from_slice(&output).context("invalid core control response")?;
    response.into_result()
}

/// Resolve the core executable installed next to the CLI.  Development builds use Cargo's
/// target directory, while packaged installs put both binaries in the same release directory.
pub fn sibling_core(cli: &Path) -> Result<PathBuf> {
    let parent = cli.parent().context("CLI executable has no parent")?;
    let name = if cfg!(windows) {
        "vibeface-core.exe"
    } else {
        "vibeface-core"
    };
    Ok(parent.join(name))
}

pub mod agent;
pub mod maintenance;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn control_client_bounds_child_output_before_waiting_for_exit() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("vibeface-control-output-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"#!/bin/sh\nhead -c 1048577 /dev/zero\n")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        let error = request(
            &path,
            &ControlRequest::Inspect {
                data_dir: "/tmp/vibeface-control-test".into(),
            },
        )
        .await
        .expect_err("oversized child output must be rejected");
        assert!(error.to_string().contains("stdout exceeds"), "{error:#}");
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[tokio::test]
    async fn control_client_rejects_ok_response_from_nonzero_child() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("vibeface-control-status-{}", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            b"#!/bin/sh\nprintf '%s\\n' '{\"ok\":true,\"result\":{}}'\nexit 7\n",
        )?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        let error = request(
            &path,
            &ControlRequest::Inspect {
                data_dir: "/tmp/vibeface-control-test".into(),
            },
        )
        .await
        .expect_err("nonzero child must not prove an accepted result");
        assert!(
            error.to_string().contains("core control process failed"),
            "{error:#}"
        );
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[tokio::test]
    async fn control_client_bounds_a_child_that_closes_streams_but_keeps_running() -> Result<()> {
        let mut child = Command::new("sh")
            .args(["-c", "exec sleep 1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let error = wait_for_exit(&mut child, Some(Duration::from_millis(20)))
            .await
            .expect_err("a hanging child must hit the wait deadline");
        let _ = child.kill().await;
        let _ = child.wait().await;
        assert!(error.to_string().contains("deadline"), "{error:#}");
        Ok(())
    }
}
