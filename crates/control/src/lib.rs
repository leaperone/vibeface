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
pub const DATA_EPOCH: u32 = 1;
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
    DiscoverLocalNodes {
        app_profile_dir: PathBuf,
        #[serde(default)]
        remembered_data_dirs: Vec<PathBuf>,
    },
    Access {
        data_dir: PathBuf,
        json: bool,
    },
    LocalSetup {
        data_dir: PathBuf,
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

/// Read by owner processes only. A loopback connection does not prove an OS account.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalHttpCredential {
    pub origin: String,
    pub node_id: String,
    pub capability: String,
}

impl LocalHttpCredential {
    pub fn read(data_dir: &Path) -> Result<Self> {
        use vibeface_bridge::platform::privatefs;
        privatefs::ensure_private(data_dir, "the local Node directory")?;
        let credential: Self = serde_json::from_slice(&privatefs::read_private_bounded(
            &data_dir.join("local-access.json"),
            4096,
            "the local HTTP credential",
        )?)?;
        let token = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        };
        let url = reqwest::Url::parse(&credential.origin)?;
        ensure!(
            url.scheme() == "http"
                && url.host_str() == Some("127.0.0.1")
                && url.port().is_some()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none()
                && url.username().is_empty()
                && url.password().is_none()
                && token(&credential.node_id)
                && token(&credential.capability),
            "invalid local HTTP credential"
        );
        Ok(credential)
    }

    pub fn require_origin(&self, url: &reqwest::Url) -> Result<()> {
        ensure!(
            url.origin().ascii_serialization() == self.origin
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none()
                && url.username().is_empty()
                && url.password().is_none(),
            "local HTTP credential belongs to another Node origin"
        );
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_local_nodes_control_request_has_a_strict_wire_shape() -> Result<()> {
        let request = ControlRequest::DiscoverLocalNodes {
            app_profile_dir: PathBuf::from("/controlled/profile"),
            remembered_data_dirs: vec![PathBuf::from("/controlled/node")],
        };
        assert_eq!(
            serde_json::to_value(request)?,
            serde_json::json!({
                "command":"discover_local_nodes",
                "app_profile_dir":"/controlled/profile",
                "remembered_data_dirs":["/controlled/node"]
            })
        );
        Ok(())
    }

    #[test]
    fn local_http_credentials_require_owner_protection_and_bind_the_origin() -> Result<()> {
        use vibeface_bridge::platform::privatefs;
        let dir =
            std::env::temp_dir().join(format!("vibeface-local-http-{}", uuid::Uuid::new_v4()));
        privatefs::create_dir(&dir)?;
        let path = dir.join("local-access.json");
        let credential = LocalHttpCredential {
            origin: "http://127.0.0.1:4791".into(),
            node_id: "a".repeat(64),
            capability: "b".repeat(64),
        };
        privatefs::write_atomic(&path, &serde_json::to_vec(&credential)?)?;
        let read = LocalHttpCredential::read(&dir)?;
        read.require_origin(&reqwest::Url::parse(&credential.origin)?)?;
        for origin in [
            "http://127.0.0.1:4792",
            "https://remote.example",
            "http://secret@127.0.0.1:4791",
            "http://127.0.0.1:4791/?redirect=remote",
        ] {
            assert!(read.require_origin(&reqwest::Url::parse(origin)?).is_err());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
            assert!(
                LocalHttpCredential::read(&dir).is_err(),
                "credentials readable by another account are rejected"
            );
            std::fs::remove_file(&path)?;
            let target = dir.join("credential");
            privatefs::write_atomic(&target, &serde_json::to_vec(&credential)?)?;
            std::os::unix::fs::symlink(&target, &path)?;
            assert!(
                LocalHttpCredential::read(&dir).is_err(),
                "a credential symlink is rejected"
            );
        }
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// A one-megabyte-and-one-byte file the fixtures below pour into their standard output.
    fn oversized_file() -> Result<PathBuf> {
        let path =
            std::env::temp_dir().join(format!("vibeface-control-bulk-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, vec![b'.'; MAX_CONTROL_PACKET + 1])?;
        Ok(path)
    }

    /// Writes an executable fixture that behaves the same on either family, and returns its path.
    ///
    /// A shell script is the shortest way to ask for exactly the output size and exit status a
    /// test needs, and the two families spell that differently.
    fn fixture(name: &str, unix_body: &str, windows_body: &str) -> Result<PathBuf> {
        let _ = windows_body;
        let _ = unix_body;
        #[cfg(unix)]
        let (suffix, body) = ("", format!("#!/bin/sh\n{unix_body}"));
        #[cfg(windows)]
        let (suffix, body) = (".cmd", format!("@echo off\r\n{windows_body}"));
        let path = std::env::temp_dir().join(format!(
            "vibeface-control-{name}-{}{suffix}",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(path)
    }

    /// A child that stays alive after closing its streams, used to reach the wait deadline.
    fn lingering_child() -> Result<tokio::process::Child> {
        #[cfg(unix)]
        let mut command = Command::new("sh");
        #[cfg(unix)]
        command.args(["-c", "exec sleep 1"]);
        #[cfg(windows)]
        let mut command = Command::new("cmd");
        // `waitfor` returns once its deadline passes, with no signal ever sent.
        #[cfg(windows)]
        command.args(["/c", "waitfor /t 1 vibeface-control-test"]);
        Ok(command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?)
    }

    #[tokio::test]
    async fn control_client_bounds_child_output_before_waiting_for_exit() -> Result<()> {
        let bulk = oversized_file()?;
        let quoted = bulk.display();
        let path = fixture(
            "output",
            &format!("cat '{quoted}'\n"),
            &format!("type \"{quoted}\"\r\n"),
        )?;
        let error = request(
            &path,
            &ControlRequest::Inspect {
                data_dir: std::env::temp_dir().join("vibeface-control-test"),
            },
        )
        .await
        .expect_err("oversized child output must be rejected");
        assert!(error.to_string().contains("stdout exceeds"), "{error:#}");
        std::fs::remove_file(path)?;
        std::fs::remove_file(bulk)?;
        Ok(())
    }

    #[tokio::test]
    async fn control_client_rejects_ok_response_from_nonzero_child() -> Result<()> {
        let path = fixture(
            "status",
            "cat >/dev/null\nprintf '%s\\n' '{\"ok\":true,\"result\":{}}'\nexit 7\n",
            "more > nul\r\necho {\"ok\":true,\"result\":{}}\r\nexit /b 7\r\n",
        )?;
        let error = request(
            &path,
            &ControlRequest::Inspect {
                data_dir: std::env::temp_dir().join("vibeface-control-test"),
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
        let mut child = lingering_child()?;
        let error = wait_for_exit(&mut child, Some(Duration::from_millis(20)))
            .await
            .expect_err("a hanging child must hit the wait deadline");
        let _ = child.kill().await;
        let _ = child.wait().await;
        assert!(error.to_string().contains("deadline"), "{error:#}");
        Ok(())
    }
}
