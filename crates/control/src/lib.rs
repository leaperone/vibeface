//! Protocols shared by the human CLI and the Vibeface core process.
//!
//! This crate deliberately contains no data store, network daemon, scheduler, or provider
//! executor.  It only knows bounded wire values and the direct App Server client used by both
//! processes.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::{Path, PathBuf}, process::Stdio, time::Duration};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, process::Command};

pub const UPDATE_PROTOCOL: u32 = 1;
pub const DATA_EPOCH: u32 = 12;
pub const MAINTENANCE_PROTOCOL: u32 = 1;
pub const MAINTENANCE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_CONTROL_PACKET: usize = 1024 * 1024;

/// A bounded request handled by a standalone core process.  The core owns all data access and
/// turns these DTOs into its internal Config, identity and Store values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlRequest {
    Init { options: Value },
    Inspect { data_dir: PathBuf },
    Access { data_dir: PathBuf, json: bool },
    SetLocalAccess { data_dir: PathBuf, enabled: bool },
    Join { data_dir: PathBuf, link: String, name: String },
    Wallet { data_dir: PathBuf, operation: Value },
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
        Self { ok: true, result: Some(result.into()), error: None }
    }

    pub fn failure(error: impl Into<String>) -> Self {
        Self { ok: false, result: None, error: Some(error.into()) }
    }

    pub fn into_result(self) -> Result<Value> {
        if self.ok {
            return Ok(self.result.unwrap_or(Value::Null));
        }
        bail!(self.error.unwrap_or_else(|| "core control request failed".into()))
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
    let mut stdin = child.stdin.take().context("core control stdin unavailable")?;
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    ensure!(bytes.len() <= MAX_CONTROL_PACKET, "core control request is too large");
    stdin.write_all(&bytes).await?;
    drop(stdin);
    let mut stdout = child.stdout.take().context("core control stdout unavailable")?;
    let mut stderr = child.stderr.take().context("core control stderr unavailable")?;
    let mut output = Vec::new();
    let mut error_output = Vec::new();
    let (stdout_result, stderr_result) = tokio::join!(
        stdout.read_to_end(&mut output),
        stderr.read_to_end(&mut error_output),
    );
    stdout_result?;
    stderr_result?;
    let status = child.wait().await?;
    ensure!(output.len() <= MAX_CONTROL_PACKET, "core control response is too large");
    if !status.success() && output.is_empty() {
        bail!("core control process failed: {}", String::from_utf8_lossy(&error_output));
    }
    let response: ControlResponse = serde_json::from_slice(&output)
        .context("invalid core control response")?;
    response.into_result()
}

/// Resolve the core executable installed next to the CLI.  Development builds use Cargo's
/// target directory, while packaged installs put both binaries in the same release directory.
pub fn sibling_core(cli: &Path) -> Result<PathBuf> {
    let parent = cli.parent().context("CLI executable has no parent")?;
    let name = if cfg!(windows) { "vibeface-core.exe" } else { "vibeface-core" };
    Ok(parent.join(name))
}

pub mod maintenance;
pub mod agent;
