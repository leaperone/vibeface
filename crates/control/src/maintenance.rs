//! Owner-only Unix maintenance wire values and client.
//!
//! The server remains in `apps/core`; this module intentionally owns no Core, Store, or wallet
//! runtime types. It keeps the admitted request stream open for an acquired lease.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub const PROTOCOL: u32 = 1;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_PACKET: usize = 4 * 1024;
const DELIVERY_INFO_TIMEOUT_SECS: u64 = 30;
const DELIVERY_PROVE_TIMEOUT_SECS: u64 = 7_200;
const DELIVERY_VERIFY_TIMEOUT_SECS: u64 = 120;
const DELIVERY_GRACE_TIMEOUT_SECS: u64 = 60;
const DELIVERY_KEY_OPERATION_TIMEOUT: Duration = Duration::from_secs(
    DELIVERY_INFO_TIMEOUT_SECS
        + DELIVERY_PROVE_TIMEOUT_SECS
        + DELIVERY_VERIFY_TIMEOUT_SECS
        + DELIVERY_GRACE_TIMEOUT_SECS,
);

/// The Node answered that work is still active. Sending the request again is safe but will be
/// refused until the active work finishes.
#[derive(Debug)]
pub struct Busy(pub String);
impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "node is busy: {}", self.0)
    }
}
impl std::error::Error for Busy {}

/// The Node answered and explicitly rejected the command. This is distinct from a transport
/// error, whose outcome may be unknown for a command with side effects.
#[derive(Debug)]
pub struct Refused(pub String);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refused {}

/// A wallet passphrase which is serialized only for the owner-only local socket and wiped on drop.
#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Pure wire command for the existing local delivery wallet actions.
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum WalletCommand {
    Status,
    Create { passphrase: SecretString },
    Unlock { passphrase: SecretString },
    Lock,
}

/// Existing maintenance commands. The value fields are decoded by Core at the trust boundary.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum MaintenanceCommand {
    Acquire,
    Probe,
    Commit,
    /// Turn local browser access without confirmation on or off.
    LocalAccess(bool),
    /// Start this Node's join with an invite link, or read how far the same join got.
    Join {
        link: String,
        name: String,
    },
    DeliveryWallet(WalletCommand),
    DeliveryConfig(Value),
    DeliveryKeyOperation {
        action: Value,
        intent_id: String,
        gas_cap: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceRequest {
    pub protocol: u32,
    pub command: MaintenanceCommand,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MaintenanceStatus {
    Ready,
    Healthy,
    Committed,
    Busy,
    Error,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceResponse {
    pub protocol: u32,
    pub status: MaintenanceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallet: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join: Option<Value>,
}

/// A successful Acquire response plus the still-open Unix stream. Dropping it releases the
/// server-side maintenance gate.
#[must_use]
pub struct Lease {
    pub health: Value,
    stream: UnixStream,
}

impl Lease {
    /// Access the held stream when a caller needs to keep it alive explicitly.
    pub fn stream(&mut self) -> &mut UnixStream {
        &mut self.stream
    }
}

fn validate_socket(socket: &Path) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    ensure!(
        socket.is_absolute(),
        "maintenance socket path must be absolute"
    );
    let parent = socket
        .parent()
        .context("maintenance socket has no parent")?;
    let parent_metadata = match std::fs::symlink_metadata(parent) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        parent_metadata.is_dir() && !parent_metadata.file_type().is_symlink(),
        "maintenance socket parent is not a private directory"
    );
    let socket_metadata = match std::fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        !socket_metadata.file_type().is_symlink(),
        "maintenance socket must not be a symlink"
    );
    ensure!(
        socket_metadata.file_type().is_socket(),
        "maintenance socket path is occupied by another file"
    );
    ensure!(
        socket_metadata.uid() == parent_metadata.uid(),
        "maintenance socket has unexpected ownership"
    );
    Ok(())
}

async fn read_response(stream: &mut UnixStream, timeout: Duration) -> Result<MaintenanceResponse> {
    let mut packet = Zeroizing::new(Vec::new());
    tokio::time::timeout(timeout, async {
        let mut chunk = Zeroizing::new([0_u8; 256]);
        loop {
            let count = stream.read(&mut chunk[..]).await?;
            ensure!(count != 0, "maintenance socket closed before response");
            packet.extend_from_slice(&chunk[..count]);
            ensure!(
                packet.len() <= MAX_PACKET,
                "maintenance packet is too large"
            );
            if let Some(end) = packet.iter().position(|byte| *byte == b'\n') {
                ensure!(
                    end + 1 == packet.len(),
                    "unexpected maintenance packet data"
                );
                let response: MaintenanceResponse = serde_json::from_slice(&packet[..end])
                    .context("invalid maintenance response")?;
                ensure!(
                    response.protocol == PROTOCOL,
                    "unsupported maintenance protocol"
                );
                return Ok::<_, anyhow::Error>(response);
            }
        }
    })
    .await
    .context("maintenance socket timed out")?
}

async fn write_request(stream: &mut UnixStream, command: MaintenanceCommand) -> Result<()> {
    let request = MaintenanceRequest {
        protocol: PROTOCOL,
        command,
    };
    let mut packet = Zeroizing::new(serde_json::to_vec(&request)?);
    packet.push(b'\n');
    ensure!(
        packet.len() <= MAX_PACKET,
        "maintenance packet is too large"
    );
    tokio::time::timeout(REQUEST_TIMEOUT, stream.write_all(&packet))
        .await
        .context("maintenance socket timed out")??;
    Ok(())
}

async fn exchange(
    socket: &Path,
    command: MaintenanceCommand,
) -> Result<(UnixStream, MaintenanceResponse)> {
    validate_socket(socket)?;
    let timeout = if matches!(&command, MaintenanceCommand::DeliveryKeyOperation { .. }) {
        DELIVERY_KEY_OPERATION_TIMEOUT
    } else {
        REQUEST_TIMEOUT
    };
    let mut stream = tokio::time::timeout(REQUEST_TIMEOUT, UnixStream::connect(socket))
        .await
        .context("maintenance connection timed out")??;
    use std::os::unix::fs::MetadataExt;
    let expected_uid = std::fs::symlink_metadata(
        socket
            .parent()
            .context("maintenance socket has no parent")?,
    )?
    .uid();
    ensure!(
        stream.peer_cred()?.uid() == expected_uid,
        "maintenance socket requires its local owner"
    );
    write_request(&mut stream, command).await?;
    let response = read_response(&mut stream, timeout).await?;
    match response.status {
        MaintenanceStatus::Busy => {
            let message = response.error.unwrap_or_else(|| "active work".into());
            Err(Busy(message.trim_start_matches("node is busy: ").to_owned()).into())
        }
        MaintenanceStatus::Error => Err(Refused(
            response
                .error
                .unwrap_or_else(|| "maintenance request failed".into()),
        )
        .into()),
        _ => Ok((stream, response)),
    }
}

/// Request a non-lease command. Acquire is rejected here so a successful lease cannot be
/// accidentally dropped by a convenience function which returns only its response.
pub async fn request(socket: &Path, command: MaintenanceCommand) -> Result<MaintenanceResponse> {
    ensure!(
        !matches!(command, MaintenanceCommand::Acquire),
        "maintenance Acquire requires maintenance::acquire so its lease stream is retained"
    );
    let (_, response) = exchange(socket, command).await?;
    Ok(response)
}

/// Compatibility name used by callers that spell out the transport operation.
pub async fn maintenance_request(
    socket: &Path,
    command: MaintenanceCommand,
) -> Result<MaintenanceResponse> {
    request(socket, command).await
}

pub async fn acquire(socket: &Path) -> Result<Lease> {
    let (stream, response) = exchange(socket, MaintenanceCommand::Acquire).await?;
    ensure!(
        matches!(response.status, MaintenanceStatus::Ready),
        "maintenance lease was not granted"
    );
    Ok(Lease {
        health: response
            .health
            .context("maintenance response has no health")?,
        stream,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_refuses_acquire_without_dropping_a_lease() {
        let socket =
            std::env::temp_dir().join(format!("vibeface-maintenance-{}.sock", std::process::id()));
        let error = request(&socket, MaintenanceCommand::Acquire)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("lease stream is retained"));
    }

    #[test]
    fn wallet_passphrase_is_not_debug_or_cloneable() {
        let command = WalletCommand::Create {
            passphrase: SecretString::new("a secure passphrase".into()),
        };
        let bytes = serde_json::to_vec(&command).unwrap();
        assert!(std::str::from_utf8(&bytes).unwrap().contains("passphrase"));
    }
}
