//! Account-owned Node settings. Personal-network business and governance never use this route.
use super::{DesktopHost, NodeIdentity, NodeStatus};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{fmt, num::{NonZeroU32, NonZeroUsize}, path::PathBuf};
use vibeface_bridge::platform::ipc::Endpoint;
use vibeface_client::{Action, LogFilter, NodeSettings, Provider, ProviderSettings, SecretChange, TriageSettings};
use vibeface_control::maintenance;

pub enum NodeOperatorRead {
    NodeSettings,
    TriageSettings,
    RelaySettings,
    ProviderModels { provider: Provider },
    ProviderProbe { provider: Provider },
    Logs { filter: LogFilter },
    Diagnostics { since_ms: Option<i64>, until_ms: Option<i64> },
}

/// This enum has no Debug/Serialize implementation: provider and webhook changes may contain
/// secrets. The only wire serialization happens immediately before the private IPC call.
pub enum NodeOperatorChange {
    NodeSettings(NodeSettings),
    ProviderSettings(ProviderSettings),
    Triage(TriageSettings),
    Relay {
        volunteer: bool,
        listen: Option<String>,
        url: Option<String>,
        rate: Option<NonZeroU32>,
        max_clients: Option<NonZeroUsize>,
        max_clients_per_ip: Option<NonZeroUsize>,
    },
    TestWebhook,
    CreateDecisionToken { label: String },
    RevokeDecisionToken { token_id: String },
}

#[derive(Debug)]
pub struct OperatorOutcomeUnknown;
impl fmt::Display for OperatorOutcomeUnknown {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("本机设置操作结果未知。请先刷新并核对状态，不要直接重复操作。")
    }
}
impl std::error::Error for OperatorOutcomeUnknown {}

#[derive(Debug)]
pub struct OperatorRefused;
impl fmt::Display for OperatorRefused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Node 已拒绝这次本机设置操作，请核对当前权限和设置。")
    }
}
impl std::error::Error for OperatorRefused {}

#[derive(Debug)]
struct OperatorUnavailable;
impl fmt::Display for OperatorUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("所选本机节点未通过身份、私有目录或运行状态检查，请重新扫描并选择。")
    }
}
impl std::error::Error for OperatorUnavailable {}

#[derive(Debug)]
struct OperatorReadFailed;
impl fmt::Display for OperatorReadFailed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("无法读取本机 Node 设置，请重新核对所选节点。")
    }
}
impl std::error::Error for OperatorReadFailed {}

const READ_METHODS: [&str; 7] = [
    "node_settings", "triage_settings", "relay_settings", "provider_models", "provider_probe",
    "logs", "diagnostics",
];
const CHANGE_METHODS: [&str; 7] = [
    "configure_node", "configure_provider", "configure_triage", "configure_relay", "test_webhook",
    "create_decision_token", "revoke_decision_token",
];

impl NodeOperatorRead {
    fn wire(self) -> Result<(&'static str, Value)> {
        Ok(match self {
            Self::NodeSettings => ("node_settings", json!({})),
            Self::TriageSettings => ("triage_settings", json!({})),
            Self::RelaySettings => ("relay_settings", json!({})),
            Self::ProviderModels { provider } => ("provider_models", json!({ "provider": provider })),
            Self::ProviderProbe { provider } => ("provider_probe", json!({ "provider": provider })),
            Self::Logs { filter } => ("logs", serde_json::to_value(filter)?),
            Self::Diagnostics { since_ms, until_ms } => ("diagnostics", json!({ "since_ms": since_ms, "until_ms": until_ms })),
        })
    }
}

impl NodeOperatorChange {
    fn wire(self) -> Result<(&'static str, Value)> {
        Ok(match self {
            Self::NodeSettings(settings) => ("configure_node", serde_json::to_value(settings)?),
            Self::ProviderSettings(settings) => ("configure_provider", serde_json::to_value(settings)?),
            Self::Triage(settings) => {
                let mut params = json!({
                    "default_outlet": settings.default_outlet,
                    "projects": settings.projects,
                    "webhook_url": settings.webhook_url,
                    "scan_codex_sessions": settings.scan_codex_sessions,
                    "codex_sessions": settings.codex_sessions
                });
                match settings.webhook_secret {
                    SecretChange::Keep => {},
                    SecretChange::Clear => params["webhook_secret"] = Value::Null,
                    SecretChange::Set(secret) => params["webhook_secret"] = Value::String(secret),
                }
                ("configure_triage", params)
            }
            Self::Relay { volunteer, listen, url, rate, max_clients, max_clients_per_ip } => ("configure_relay", json!({
                "volunteer": volunteer, "listen": listen, "url": url, "rate": rate,
                "max_clients": max_clients, "max_clients_per_ip": max_clients_per_ip
            })),
            Self::TestWebhook => ("test_webhook", json!({})),
            Self::CreateDecisionToken { label } => ("create_decision_token", json!({ "label": label })),
            Self::RevokeDecisionToken { token_id } => ("revoke_decision_token", json!({ "token_id": token_id })),
        })
    }
}

impl TryFrom<Action> for NodeOperatorChange {
    type Error = anyhow::Error;

    fn try_from(action: Action) -> Result<Self> {
        Ok(match action {
            Action::NodeSettings(settings) => Self::NodeSettings(settings),
            Action::ProviderSettings(settings) => Self::ProviderSettings(settings),
            Action::Triage(settings) => Self::Triage(settings),
            Action::Relay { volunteer, listen, url, rate, max_clients, max_clients_per_ip } => Self::Relay {
                volunteer, listen, url, rate, max_clients, max_clients_per_ip,
            },
            Action::TestWebhook => Self::TestWebhook,
            Action::CreateDecisionToken { label } => Self::CreateDecisionToken { label },
            Action::RevokeDecisionToken { token_id } => Self::RevokeDecisionToken { token_id },
            _ => anyhow::bail!("该操作需要个人网络客户端权限，不能使用本机 Node 所有者通道。"),
        })
    }
}

fn sanitize_call_error(error: anyhow::Error, change: bool) -> anyhow::Error {
    // A settings handler can fail after saving part of a change. Even an explicit server
    // error therefore does not prove a write had no effect.
    if change {
        return OperatorOutcomeUnknown.into();
    }
    if error.is::<maintenance::Refused>() || error.is::<maintenance::Busy>() {
        OperatorRefused.into()
    } else {
        OperatorReadFailed.into()
    }
}

impl DesktopHost {
    async fn operator_socket(&self, expected: &NodeIdentity) -> Result<PathBuf> {
        ensure!(
            self.selected.as_ref() == Some(expected),
            "请先选择并核对要配置的本机节点。"
        );
        let inspected = self.verify(expected, Some(NodeStatus::Running)).await?;
        let socket = inspected.maintenance_socket.context("所选节点没有报告本机维护入口。")?;
        // The descriptor comes only from the fixed bundled Core inspect, after its own
        // identity/health check. Peer credentials and private endpoint checks remain mandatory.
        let endpoint = Endpoint::from_path(&socket)?;
        endpoint.ensure_private("本机 Node 设置入口")?;
        Ok(socket)
    }

    async fn operator_call(&self, expected: &NodeIdentity, method: &'static str, params: Value, change: bool) -> Result<Value> {
        ensure!(
            if change { CHANGE_METHODS.contains(&method) } else { READ_METHODS.contains(&method) },
            "该方法不属于本机 Node 设置接口。"
        );
        let socket = self.operator_socket(expected).await
            .map_err(|_| anyhow::Error::from(OperatorUnavailable))?;
        // maintenance::call bounds packets, verifies the OS account, and times out. A write
        // may have reached Node when transport fails; it is never automatically retried.
        let reply = maintenance::call(&socket, method, params).await
            .map_err(|error| sanitize_call_error(error, change))?;
        let current = self.operator_socket(expected).await.map_err(|_| {
            if change { anyhow::Error::from(OperatorOutcomeUnknown) } else { anyhow::Error::from(OperatorReadFailed) }
        })?;
        if current != socket {
            return Err(if change { OperatorOutcomeUnknown.into() } else { OperatorReadFailed.into() });
        }
        Ok(reply)
    }

    pub async fn operator_read(&self, expected: &NodeIdentity, query: NodeOperatorRead) -> Result<Value> {
        let (method, params) = query.wire()?;
        self.operator_call(expected, method, params, false).await
    }

    /// The result stays in the caller's ephemeral operation flow. In particular decision-token
    /// creation may return a secret: never put its Value in HostSnapshot or diagnostics.
    pub async fn operator_change(&self, expected: &NodeIdentity, change: NodeOperatorChange) -> Result<Value> {
        let (method, params) = change.wire()?;
        self.operator_call(expected, method, params, true).await
    }
}

#[cfg(test)]
#[path = "operator_tests.rs"]
mod tests;
