//! Owner-only maintenance wire values and client.
//!
//! The server remains in `apps/core`; this module intentionally owns no Core, Store, or wallet
//! runtime types. It keeps the admitted request stream open for an acquired lease.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vibeface_bridge::platform::ipc::{Endpoint, Stream};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub const PROTOCOL: u32 = 1;
/// Names the maintenance endpoint in the message a person reads.
const LABEL: &str = "the maintenance socket";
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// A `Client` answer carries a whole RPC result, such as diagnostics or a bot list.
pub const MAX_PACKET: usize = 1024 * 1024;
/// Provider probes and webhook tests wait on the network.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(120);
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
    DeliveryBudget(Value),
    DeliveryKeyOperation {
        action: Value,
        intent_id: String,
        gas_cap: String,
    },
    /// Edit the mesh bootstrap nodes in config.json and the running mesh.
    MeshBootstrap {
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// Set one config.json value; the running Node applies it when it restarts.
    ConfigSet {
        key: String,
        value: Value,
    },
    /// Call a client method as the owner at this computer, with what a local page may do.
    Client {
        method: String,
        params: Value,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceRequest {
    pub protocol: u32,
    pub command: MaintenanceCommand,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum MaintenanceStatus {
    Ready,
    Healthy,
    Committed,
    Busy,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Health {
    pub device_id: String,
    pub version: String,
    pub web_enabled: bool,
    pub http_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_state: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peers: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WalletStatus {
    Absent,
    Unavailable { reason: String },
    Locked { address: String },
    Unlocked { address: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeJoin {
    Waiting,
    Confirming { code: String, admin: String },
    Joined,
    Failed { reason: String },
    Cancelled,
}

#[derive(Debug, Clone)]
pub enum MaintenanceResponse {
    LeaseGranted(Health),
    Probed(Health),
    Committed(Health),
    Applied(Health),
    Wallet {
        health: Health,
        wallet: WalletStatus,
    },
    Delivery {
        health: Health,
        delivery: Value,
    },
    Join {
        health: Health,
        join: NodeJoin,
    },
    ClientResult {
        health: Health,
        result: Value,
    },
    Busy {
        health: Health,
        error: String,
    },
    Refused {
        health: Health,
        error: String,
    },
}

#[derive(Debug)]
struct RawResponse {
    protocol: u32,
    status: MaintenanceStatus,
    health: Option<Value>,
    error: Option<String>,
    wallet: Option<Value>,
    delivery: Option<Value>,
    join: Option<Value>,
    result: Option<Value>,
}

impl<'de> Deserialize<'de> for RawResponse {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use serde::de::{Error, MapAccess, Visitor};
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawResponse;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a maintenance response object")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut m: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut p = None;
                let mut st = None;
                let mut h = None;
                let mut e = None;
                let mut w = None;
                let mut d = None;
                let mut j = None;
                let mut r = None;
                let mut seen = [false; 8];
                while let Some(k) = m.next_key::<String>()? {
                    let (idx, name) = match k.as_str() {
                        "protocol" => (0, "protocol"),
                        "status" => (1, "status"),
                        "health" => (2, "health"),
                        "error" => (3, "error"),
                        "wallet" => (4, "wallet"),
                        "delivery" => (5, "delivery"),
                        "join" => (6, "join"),
                        "result" => (7, "result"),
                        _ => {
                            return Err(A::Error::unknown_field(
                                &k,
                                &[
                                    "protocol", "status", "health", "error", "wallet", "delivery",
                                    "join", "result",
                                ],
                            ));
                        }
                    };
                    if seen[idx] {
                        return Err(A::Error::custom(format!(
                            "duplicate maintenance response field: {name}"
                        )));
                    }
                    seen[idx] = true;
                    match idx {
                        0 => p = Some(m.next_value()?),
                        1 => st = Some(m.next_value()?),
                        2 => h = Some(m.next_value()?),
                        3 => e = m.next_value()?,
                        4 => w = Some(m.next_value()?),
                        5 => d = Some(m.next_value()?),
                        6 => j = Some(m.next_value()?),
                        7 => r = Some(m.next_value()?),
                        _ => unreachable!(),
                    }
                }
                Ok(RawResponse {
                    protocol: p.ok_or_else(|| A::Error::missing_field("protocol"))?,
                    status: st.ok_or_else(|| A::Error::missing_field("status"))?,
                    health: h,
                    error: e,
                    wallet: w,
                    delivery: d,
                    join: j,
                    result: r,
                })
            }
        }
        deserializer.deserialize_map(V)
    }
}

#[derive(Serialize)]
struct WireResponse<'a> {
    protocol: u32,
    status: MaintenanceStatus,
    health: &'a Health,
    error: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wallet: Option<WalletStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delivery: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    join: Option<&'a NodeJoin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a Value>,
}

impl Serialize for MaintenanceResponse {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let (status, health, wallet, delivery, join, result) = match self {
            Self::LeaseGranted(health) => {
                (MaintenanceStatus::Ready, health, None, None, None, None)
            }
            Self::Probed(health) => (MaintenanceStatus::Healthy, health, None, None, None, None),
            Self::Committed(health) => {
                (MaintenanceStatus::Committed, health, None, None, None, None)
            }
            Self::Applied(health) => (MaintenanceStatus::Healthy, health, None, None, None, None),
            Self::Wallet { health, wallet } => (
                MaintenanceStatus::Healthy,
                health,
                Some(wallet.clone()),
                None,
                None,
                None,
            ),
            Self::Delivery { health, delivery } => (
                MaintenanceStatus::Healthy,
                health,
                None,
                Some(delivery),
                None,
                None,
            ),
            Self::Join { health, join } => (
                MaintenanceStatus::Healthy,
                health,
                None,
                None,
                Some(join),
                None,
            ),
            Self::ClientResult { health, result } => (
                MaintenanceStatus::Healthy,
                health,
                None,
                None,
                None,
                Some(result),
            ),
            Self::Busy { health, .. } => (MaintenanceStatus::Busy, health, None, None, None, None),
            Self::Refused { health, .. } => {
                (MaintenanceStatus::Error, health, None, None, None, None)
            }
        };
        let error = match self {
            Self::Busy { error, .. } | Self::Refused { error, .. } => Some(error.as_str()),
            _ => None,
        };
        WireResponse {
            protocol: PROTOCOL,
            status,
            health,
            error,
            wallet,
            delivery,
            join,
            result,
        }
        .serialize(serializer)
    }
}

impl MaintenanceResponse {
    fn validate_health(health: Value) -> Result<Health> {
        serde_json::from_value(health).context("maintenance response has malformed health")
    }

    fn parse(command: &MaintenanceCommand, raw: RawResponse) -> Result<Self> {
        ensure!(raw.protocol == PROTOCOL, "unsupported maintenance protocol");
        let health =
            Self::validate_health(raw.health.context("maintenance response has no health")?)?;
        let payload_count = [
            raw.wallet.is_some(),
            raw.delivery.is_some(),
            raw.join.is_some(),
            raw.result.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if matches!(
            raw.status,
            MaintenanceStatus::Busy | MaintenanceStatus::Error
        ) {
            ensure!(raw.error.is_some(), "maintenance failure has no error");
            ensure!(
                payload_count == 0,
                "maintenance failure contains an operation payload"
            );
            return match raw.status {
                MaintenanceStatus::Busy => Err(Busy(
                    raw.error
                        .unwrap()
                        .trim_start_matches("node is busy: ")
                        .to_owned(),
                )
                .into()),
                MaintenanceStatus::Error => Err(Refused(raw.error.unwrap()).into()),
                _ => unreachable!(),
            };
        }
        ensure!(
            raw.error.is_none(),
            "successful maintenance response has a non-null error"
        );
        match command {
            MaintenanceCommand::Acquire => {
                ensure!(
                    raw.status == MaintenanceStatus::Ready && payload_count == 0,
                    "invalid maintenance Acquire response"
                );
                Ok(Self::LeaseGranted(health))
            }
            MaintenanceCommand::Probe => {
                ensure!(
                    raw.status == MaintenanceStatus::Healthy && payload_count == 0,
                    "invalid maintenance Probe response"
                );
                Ok(Self::Probed(health))
            }
            MaintenanceCommand::Commit => {
                ensure!(
                    raw.status == MaintenanceStatus::Committed && payload_count == 0,
                    "invalid maintenance Commit response"
                );
                Ok(Self::Committed(health))
            }
            MaintenanceCommand::DeliveryWallet(_) => {
                ensure!(
                    raw.status == MaintenanceStatus::Healthy
                        && raw.wallet.as_ref().is_some_and(|value| !value.is_null())
                        && payload_count == 1,
                    "invalid maintenance wallet response"
                );
                Ok(Self::Wallet {
                    health,
                    wallet: serde_json::from_value(raw.wallet.unwrap())
                        .context("maintenance response has malformed wallet")?,
                })
            }
            MaintenanceCommand::DeliveryBudget(_)
            | MaintenanceCommand::DeliveryKeyOperation { .. } => {
                ensure!(
                    raw.status == MaintenanceStatus::Healthy
                        && raw.delivery.as_ref().is_some_and(|value| value.is_object())
                        && payload_count == 1,
                    "invalid maintenance delivery response"
                );
                Ok(Self::Delivery {
                    health,
                    delivery: raw.delivery.unwrap(),
                })
            }
            MaintenanceCommand::Join { .. } => {
                ensure!(
                    raw.status == MaintenanceStatus::Healthy
                        && raw.join.as_ref().is_some_and(|value| !value.is_null())
                        && payload_count == 1,
                    "invalid maintenance Join response"
                );
                Ok(Self::Join {
                    health,
                    join: serde_json::from_value(raw.join.unwrap())
                        .context("maintenance response has malformed join")?,
                })
            }
            MaintenanceCommand::Client { .. } => {
                ensure!(
                    raw.status == MaintenanceStatus::Healthy
                        && raw.result.is_some()
                        && payload_count == 1,
                    "invalid maintenance Client response"
                );
                Ok(Self::ClientResult {
                    health,
                    result: raw.result.unwrap(),
                })
            }
            MaintenanceCommand::LocalAccess(_)
            | MaintenanceCommand::DeliveryConfig(_)
            | MaintenanceCommand::MeshBootstrap { .. }
            | MaintenanceCommand::ConfigSet { .. } => {
                ensure!(
                    raw.status == MaintenanceStatus::Healthy && payload_count == 0,
                    "invalid applied maintenance response"
                );
                Ok(Self::Applied(health))
            }
        }
    }

    pub fn health(&self) -> &Health {
        match self {
            Self::LeaseGranted(h) | Self::Probed(h) | Self::Committed(h) | Self::Applied(h) => h,
            Self::Wallet { health, .. }
            | Self::Delivery { health, .. }
            | Self::Join { health, .. }
            | Self::ClientResult { health, .. }
            | Self::Busy { health, .. }
            | Self::Refused { health, .. } => health,
        }
    }

    pub fn client_result(self) -> Result<Value> {
        match self {
            Self::ClientResult { result, .. } => Ok(result),
            _ => Err(anyhow::anyhow!(
                "maintenance response is not a Client result"
            )),
        }
    }
}

/// A successful Acquire response plus the still-open owner-only stream. Dropping it releases
/// the server-side maintenance gate.
#[must_use]
pub struct Lease {
    pub health: Health,
    _stream: Stream,
}

async fn read_response(stream: &mut Stream, timeout: Duration) -> Result<RawResponse> {
    let mut packet = Zeroizing::new(Vec::new());
    tokio::time::timeout(timeout, async {
        let mut chunk = Zeroizing::new([0_u8; 8192]);
        loop {
            let count = stream.read(&mut chunk[..]).await?;
            ensure!(count != 0, "maintenance socket closed before response");
            let start = packet.len();
            packet.extend_from_slice(&chunk[..count]);
            ensure!(
                packet.len() <= MAX_PACKET,
                "maintenance packet is too large"
            );
            if let Some(end) = chunk[..count]
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|at| start + at)
            {
                ensure!(
                    end + 1 == packet.len(),
                    "unexpected maintenance packet data"
                );
                let response: RawResponse = serde_json::from_slice(&packet[..end])
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

async fn write_request(stream: &mut Stream, command: &MaintenanceCommand) -> Result<()> {
    #[derive(Serialize)]
    struct Request<'a> {
        protocol: u32,
        command: &'a MaintenanceCommand,
    }
    let mut packet = Zeroizing::new(serde_json::to_vec(&Request {
        protocol: PROTOCOL,
        command,
    })?);
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
) -> Result<(Stream, MaintenanceResponse)> {
    let endpoint = Endpoint::from_path(socket)?;
    endpoint.ensure_private(LABEL)?;
    let timeout = match &command {
        MaintenanceCommand::DeliveryKeyOperation { .. } => DELIVERY_KEY_OPERATION_TIMEOUT,
        MaintenanceCommand::Client { .. } => CLIENT_TIMEOUT,
        _ => REQUEST_TIMEOUT,
    };
    let mut stream = tokio::time::timeout(REQUEST_TIMEOUT, Stream::connect(&endpoint))
        .await
        .context("maintenance connection timed out")??;
    stream.require_current_account(LABEL)?;
    write_request(&mut stream, &command).await?;
    let response = read_response(&mut stream, timeout).await?;
    let response = MaintenanceResponse::parse(&command, response)?;
    Ok((stream, response))
}

/// Request a non-lease command. Acquire is rejected here so a successful lease cannot be
/// accidentally dropped by a convenience function which returns only its response.
pub async fn request(socket: &Path, command: MaintenanceCommand) -> Result<MaintenanceResponse> {
    ensure!(
        !matches!(&command, MaintenanceCommand::Acquire),
        "maintenance Acquire requires maintenance::acquire so its lease stream is retained"
    );
    let (_, response) = exchange(socket, command).await?;
    Ok(response)
}

/// Call a client method on the running Node as its local operator and return its result.
pub async fn call(socket: &Path, method: &str, params: Value) -> Result<Value> {
    let response = request(
        socket,
        MaintenanceCommand::Client {
            method: method.to_owned(),
            params,
        },
    )
    .await?;
    response.client_result()
}

pub async fn acquire(socket: &Path) -> Result<Lease> {
    let (stream, response) = exchange(socket, MaintenanceCommand::Acquire).await?;
    let MaintenanceResponse::LeaseGranted(health) = response else {
        unreachable!("Acquire parser guarantees LeaseGranted")
    };
    Ok(Lease {
        health,
        _stream: stream,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        sync::oneshot,
    };
    use vibeface_bridge::platform::{ipc, ipc::Listener, privatefs};

    #[test]
    fn client_response_distinguishes_null_from_missing_or_unrelated_payloads() -> Result<()> {
        let decode = |wire| {
            let raw = serde_json::from_str::<RawResponse>(wire)?;
            MaintenanceResponse::parse(
                &MaintenanceCommand::Client {
                    method: "test".into(),
                    params: Value::Null,
                },
                raw,
            )?
            .client_result()
        };
        assert_eq!(
            decode(
                r#"{"protocol":1,"status":"healthy","health":{"device_id":"device","version":"version","web_enabled":false,"http_enabled":false},"result":null}"#
            )?,
            Value::Null
        );
        assert_eq!(
            decode(
                r#"{"protocol":1,"status":"healthy","health":{"device_id":"device","version":"version","web_enabled":false,"http_enabled":false},"error":null,"result":{"saved":true}}"#
            )?,
            serde_json::json!({"saved":true})
        );
        for wire in [
            r#"{"protocol":1,"status":"healthy","health":{"device_id":"device","version":"version","web_enabled":false,"http_enabled":false}}"#,
            r#"{"protocol":1,"status":"ready","health":{"device_id":"device","version":"version","web_enabled":false,"http_enabled":false},"result":{}}"#,
            r#"{"protocol":1,"status":"healthy","health":{"device_id":"device","version":"version","web_enabled":false,"http_enabled":false},"wallet":{},"result":{}}"#,
            r#"{"protocol":1,"status":"healthy","health":{"device_id":"device","version":"version","web_enabled":false,"http_enabled":false},"error":"refused","result":{}}"#,
        ] {
            assert!(decode(wire).is_err(), "{wire}");
        }
        assert!(
            serde_json::from_str::<RawResponse>(
                r#"{"protocol":1,"protocol":1,"status":"healthy","health":{"device_id":"device","version":"version","web_enabled":false,"http_enabled":false},"result":null}"#
            )
            .is_err()
        );
        assert!(
            MaintenanceResponse::parse(
                &MaintenanceCommand::Client {
                    method: "test".into(),
                    params: Value::Null
                },
                serde_json::from_str(r#"{"protocol":1,"status":"error","error":"refused"}"#)?
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn operation_null_payloads_are_not_treated_as_missing() -> Result<()> {
        let health = r#"{"device_id":"device","version":"version","web_enabled":false,"http_enabled":false}"#;
        let cases = [
            (
                MaintenanceCommand::DeliveryWallet(WalletCommand::Status),
                "wallet",
            ),
            (MaintenanceCommand::DeliveryBudget(Value::Null), "delivery"),
            (
                MaintenanceCommand::DeliveryKeyOperation {
                    action: Value::Null,
                    intent_id: "intent".into(),
                    gas_cap: "1".into(),
                },
                "delivery",
            ),
            (
                MaintenanceCommand::Join {
                    link: "link".into(),
                    name: "name".into(),
                },
                "join",
            ),
        ];
        for (command, field) in cases {
            let wire =
                format!(r#"{{"protocol":1,"status":"healthy","health":{health},"{field}":null}}"#);
            let raw = serde_json::from_str::<RawResponse>(&wire)?;
            assert!(
                MaintenanceResponse::parse(&command, raw).is_err(),
                "explicit null {field} must be rejected"
            );
        }
        for field in ["wallet", "delivery", "join", "result"] {
            let wire = format!(
                r#"{{"protocol":1,"status":"error","health":{health},"error":"refused","{field}":null}}"#
            );
            let raw = serde_json::from_str::<RawResponse>(&wire)?;
            let command = MaintenanceCommand::Probe;
            assert!(
                MaintenanceResponse::parse(&command, raw).is_err(),
                "failure with explicit null {field} must be rejected"
            );
        }
        for command in [
            MaintenanceCommand::DeliveryBudget(Value::Null),
            MaintenanceCommand::DeliveryKeyOperation {
                action: Value::Null,
                intent_id: "intent".into(),
                gas_cap: "1".into(),
            },
        ] {
            let raw = serde_json::from_str::<RawResponse>(
                r#"{"protocol":1,"status":"healthy","health":{"device_id":"device","version":"version","web_enabled":false,"http_enabled":false},"delivery":[]}"#,
            )?;
            assert!(MaintenanceResponse::parse(&command, raw).is_err());
        }
        Ok(())
    }

    #[tokio::test]
    async fn request_refuses_acquire_without_dropping_a_lease() {
        let socket = std::env::temp_dir().join(format!(
            "vibeface-maintenance-refusal-{}.sock",
            std::process::id()
        ));
        let error = request(&socket, MaintenanceCommand::Acquire)
            .await
            .expect_err("Acquire must use the lease-returning API");
        assert!(error.to_string().contains("lease stream is retained"));
    }

    #[tokio::test]
    async fn acquire_keeps_the_server_stream_until_drop() -> Result<()> {
        let directory = ipc::runtime_dir(&uuid::Uuid::new_v4().simple().to_string())?;
        privatefs::create_dir(&directory)?;
        let endpoint = Endpoint::in_private_dir(&directory, "maintenance.sock")?;
        let mut listener = Listener::bind(&endpoint).await?;
        let (ready_tx, ready_rx) = oneshot::channel();
        let (closed_tx, mut closed_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await?;
            let mut request = Vec::new();
            loop {
                let mut byte = [0_u8; 1];
                let count = stream.read(&mut byte).await?;
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&byte);
                if byte[0] == b'\n' {
                    break;
                }
            }
            let response = serde_json::json!({
                "protocol": PROTOCOL,
                "status": "ready",
                "health":{"device_id":"device","version":"version","web_enabled":false,"http_enabled":false}
            });
            stream.write_all(&serde_json::to_vec(&response)?).await?;
            stream.write_all(b"\n").await?;
            let _ = ready_tx.send(());
            let mut tail = Vec::new();
            stream.read_to_end(&mut tail).await?;
            let _ = closed_tx.send(());
            Ok::<_, anyhow::Error>(request)
        });
        let lease = acquire(endpoint.path()).await?;
        assert_eq!(lease.health.device_id, "device");
        ready_rx.await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut closed_rx)
                .await
                .is_err(),
            "the server must keep waiting while the lease is held"
        );
        drop(lease);
        tokio::time::timeout(Duration::from_secs(1), closed_rx).await??;
        let request = server.await??;
        assert!(
            serde_json::from_slice::<serde_json::Value>(&request)?
                .get("command")
                .is_some()
        );
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn wallet_passphrase_is_sent_as_plain_text_in_the_command() {
        let command = WalletCommand::Create {
            passphrase: SecretString::new("a secure passphrase".into()),
        };
        assert_eq!(
            serde_json::to_value(&command).unwrap(),
            serde_json::json!({"op": "create", "passphrase": "a secure passphrase"})
        );
    }
}
