use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type NetworkId = String;
pub type NodeId = String;
pub type EventHash = String;
pub type ScopeId = String;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSession {
    pub space_id: vibeface_secure::authorization::SpaceId,
    pub resource_id: vibeface_secure::authorization::ResourceId,
    pub service: vibeface_secure::authorization::ServiceId,
    pub device: vibeface_secure::authorization::DeviceId,
    pub conversation_id: String,
    pub sdk_session: vibeface_secure::authorization::SessionRef,
    pub grant_leaf: vibeface_secure::authorization::ObjectHash,
    pub initiator: vibeface_secure::authorization::Actor,
    pub initiating_grant: Option<vibeface_secure::authorization::ObjectHash>,
    pub parent_invocation: vibeface_secure::authorization::ObjectHash,
    pub expected_policies: Vec<vibeface_secure::authorization::PolicyStamp>,
    pub expires_at_ms: u64,
}
pub const WIRE_VERSION: u32 = 2;
pub const MAX_EVENT_BYTES: usize = 1_048_576;
pub const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;

pub use vibeface_secure::authorization::events::{
    AdmissionProof, BlobRef, Event, EventBody, EventCause, SignedEvent, StoredTransport,
};

pub use vibeface_secure::membership::{NodeCertificate, NodeRole, validate_role_dependencies};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct MissingDependencies {
    pub events: Vec<EventHash>,
    pub blobs: Vec<BlobRef>,
    pub conflicts: Vec<EventHash>,
}
impl MissingDependencies {
    pub fn is_complete(&self) -> bool {
        self.events.is_empty() && self.blobs.is_empty() && self.conflicts.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    #[serde(rename = "space_id")]
    pub network_id: NetworkId,
    pub scope_id: ScopeId,
    pub heads: Vec<EventHash>,
    pub events: Vec<SignedEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    #[serde(rename = "space_id")]
    pub network_id: NetworkId,
    pub scope_id: ScopeId,
    pub heads: Vec<EventHash>,
    pub events: Vec<Event>,
    pub missing: MissingDependencies,
}

pub use vibeface_secure::authorization::events::BotProject;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BotCreateSpec {
    pub bot_id: String,
    pub conversation_id: ScopeId,
    pub name: String,
    pub instructions: String,
    pub project: Option<BotProject>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Bot {
    pub bot_id: String,
    pub conversation_id: ScopeId,
    pub name: String,
    pub instructions: String,
    pub project: Option<BotProject>,
    pub event_hash: EventHash,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Conversation {
    pub conversation_id: ScopeId,
    pub title: String,
    pub heads: Vec<EventHash>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub message_id: String,
    pub role: String,
    pub text: String,
    pub event_hash: EventHash,
    pub created_at_ms: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub task_id: String,
    pub conversation_id: ScopeId,
    pub title: String,
    pub goal: String,
    pub acceptance: String,
    pub status: String,
    pub detail: String,
    pub event_hash: EventHash,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ManagerDispatchAuthority {
    pub scheduler_node: NodeId,
    pub trigger: EventHash,
    pub followup_version: u64,
    pub attempt_id: String,
    pub permission_version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OperationRequest {
    pub operation_id: String,
    #[serde(rename = "space_id")]
    pub network_id: NetworkId,
    pub conversation_id: ScopeId,
    pub task_id: Option<String>,
    pub target_node: NodeId,
    pub session_id: Option<String>,
    pub expected_revision: u64,
    pub basis_heads: Vec<EventHash>,
    pub action: Value,
    pub resources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<crate::binding::SignedBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_authority: Option<ManagerDispatchAuthority>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Accepted,
    Running,
    Unknown,
    Succeeded,
    Failed,
    Rejected,
}
impl OperationState {
    pub fn terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Rejected)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OperationRecord {
    pub request: OperationRequest,
    pub state: OperationState,
    pub result: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSession {
    #[serde(rename = "space_id")]
    pub network_id: NetworkId,
    pub target_node: NodeId,
    pub session_id: String,
    pub revision: u64,
    pub state: String,
    pub executor: String,
    pub cwd: String,
    pub native_session_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Notification {
    pub notification_id: String,
    pub conversation_id: ScopeId,
    pub kind: String,
    pub text: String,
    pub event_hash: EventHash,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContinuationClaim {
    pub result_event_hash: EventHash,
    pub policy_version: String,
    pub scheduler_node: NodeId,
    pub state: String,
    pub result: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BudgetUsage {
    pub day: String,
    pub used: u64,
    pub limit: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestOutcome {
    pub hash: EventHash,
    pub status: String,
    pub missing: MissingDependencies,
}

pub fn authorization_action(
    body: &Value,
) -> anyhow::Result<vibeface_secure::authorization::Action> {
    use vibeface_secure::authorization::Action;

    Ok(match body["method"].as_str() {
        Some("context") if body["blob_hash"].is_string() => Action::AttachmentRead,
        Some(
            "bots"
            | "conversations"
            | "conversation"
            | "progress_read"
            | "context"
            | "execution_share_request"
            | "schedule"
            | "manager_status",
        ) => Action::ConversationRead,
        Some("nodes" | "session" | "operation" | "execution_status") => Action::OperationStatusRead,
        Some("create_conversation" | "create_bot") => Action::ConversationCreate,
        Some("cancel_manager") => Action::AgentInterrupt,
        Some("set_followup") => Action::TaskFollowup,
        Some("resolve_manager_unknown") => Action::OperationResolve,
        Some("read_artifact") => Action::AttachmentRead,
        Some("execution_control") => execution_control_action(&body["action"]["type"])?,
        Some("task_control") => match body["action"].as_str() {
            Some("status") => Action::OperationStatusRead,
            _ => execution_control_action(&body["action"])?,
        },
        Some("bind_execution") => Action::ExecutionOpen,
        Some("replica_inventory" | "replica_event" | "replica_blob") => Action::ReplicaRead,
        Some("message") => Action::MessageAppend,
        Some("create_task") => Action::TaskCreate,
        Some("agent_start" | "agent_result" | "agent_notification") => Action::AgentStart,
        Some("update_task") => match body["status"].as_str() {
            Some("accepted") => Action::TaskAccept,
            Some("cancelled") => Action::TaskCancel,
            Some("open" | "working" | "blocked" | "needs_review") => Action::TaskProgress,
            _ => anyhow::bail!("invalid task status"),
        },
        Some("seal_execution" | "rebind_execution" | "seal_remote_execution") => {
            Action::ExecutionRebind
        }
        Some("remote_execution_seal_status") => Action::OperationStatusRead,
        Some("notifications") => Action::NotificationRead,
        Some("ack") => Action::NotificationAck,
        Some("export") => Action::Export,
        Some("reconcile") => Action::OperationReconcile,
        Some("resolve_unknown") => Action::OperationResolve,
        Some("operate") => match body["request"]["action"]["type"].as_str() {
            Some("open") => Action::ExecutionOpen,
            Some("read") => Action::OperationStatusRead,
            Some("send") => Action::ExecutionSend,
            Some("command") => Action::ExecutionCommand,
            Some("steer") => Action::ExecutionSteer,
            Some("interrupt") => Action::ExecutionInterrupt,
            Some("respond_request") => Action::ExecutionRespondApproval,
            _ => anyhow::bail!("unknown operation action"),
        },
        _ => anyhow::bail!("unknown method"),
    })
}

fn execution_control_action(
    kind: &Value,
) -> anyhow::Result<vibeface_secure::authorization::Action> {
    use vibeface_secure::authorization::Action;
    match kind.as_str() {
        Some("steer") => Ok(Action::ExecutionSteer),
        Some("interrupt") => Ok(Action::ExecutionInterrupt),
        Some("respond_request") => Ok(Action::ExecutionRespondApproval),
        _ => anyhow::bail!("unknown execution control"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use vibeface_secure::authorization::Action;

    #[test]
    fn authorization_actions_distinguish_privileged_task_outcomes() -> anyhow::Result<()> {
        for (status, expected) in [
            ("accepted", Action::TaskAccept),
            ("cancelled", Action::TaskCancel),
            ("open", Action::TaskProgress),
            ("working", Action::TaskProgress),
            ("blocked", Action::TaskProgress),
            ("needs_review", Action::TaskProgress),
        ] {
            assert_eq!(
                authorization_action(&json!({"method":"update_task", "status":status}))?,
                expected
            );
        }
        assert!(authorization_action(&json!({"method":"update_task", "status":"done"})).is_err());
        assert!(authorization_action(&json!({"method":"update_task"})).is_err());
        assert!(authorization_action(&json!({"method":"unknown"})).is_err());
        assert_eq!(
            authorization_action(&json!({"method":"bind_execution"}))?,
            Action::ExecutionOpen
        );
        assert_eq!(
            authorization_action(&json!({"method":"context","blob_hash":"a"}))?,
            Action::AttachmentRead
        );
        assert_eq!(
            authorization_action(&json!({"method":"replica_event"}))?,
            Action::ReplicaRead
        );
        assert!(authorization_action(&json!({"method":"replica_push"})).is_err());
        assert!(authorization_action(&Value::Null).is_err());
        Ok(())
    }

    #[test]
    fn authorization_actions_match_nested_execution_requests() -> anyhow::Result<()> {
        for (kind, expected) in [
            ("open", Action::ExecutionOpen),
            ("read", Action::OperationStatusRead),
            ("send", Action::ExecutionSend),
            ("command", Action::ExecutionCommand),
            ("steer", Action::ExecutionSteer),
            ("interrupt", Action::ExecutionInterrupt),
            ("respond_request", Action::ExecutionRespondApproval),
        ] {
            assert_eq!(
                authorization_action(
                    &json!({"method":"operate", "request":{"action":{"type":kind}}})
                )?,
                expected
            );
        }
        assert!(
            authorization_action(
                &json!({"method":"operate", "request":{"action":{"type":"unknown"}}})
            )
            .is_err()
        );
        assert!(authorization_action(&json!({"method":"operate"})).is_err());
        Ok(())
    }
}
