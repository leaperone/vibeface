use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type NetworkId = String;
pub type NodeId = String;
pub type EventHash = String;
pub type ScopeId = String;

#[derive(Clone, Debug)]
pub struct AgentSession {
    pub space_id: vibeface_secure::authorization::SpaceId,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct BlobRef {
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventBody {
    ConversationCreated {
        title: String,
    },
    MessagePosted {
        message_id: String,
        role: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scheduler_node: Option<NodeId>,
    },
    TaskCreated {
        task_id: String,
        title: String,
        goal: String,
        acceptance: String,
    },
    TaskChanged {
        task_id: String,
        status: String,
        detail: String,
    },
    DispatchRecorded {
        operation_id: String,
        task_id: Option<String>,
        target_node: NodeId,
        session_id: String,
        action: Value,
    },
    ExecutionObserved {
        operation_id: String,
        task_id: Option<String>,
        target_node: NodeId,
        session_id: String,
        status: String,
        detail: Value,
    },
    DecisionRecorded {
        decision_id: String,
        text: String,
        effective_constraints: Vec<String>,
        tool_configuration: Value,
    },
    NotificationRaised {
        notification_id: String,
        kind: String,
        text: String,
    },
    NotificationAcknowledged {
        notification_id: String,
        client_id: String,
    },
    ContextCheckpoint {
        summary: String,
        covered_heads: Vec<EventHash>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AdmissionProof {
    pub invocation:
        vibeface_secure::authorization::Signed<vibeface_secure::authorization::Invocation>,
    pub source_body: Value,
    pub challenge: vibeface_secure::authorization::FreshnessChallenge,
    pub receipts: Vec<
        vibeface_secure::authorization::Signed<vibeface_secure::authorization::FreshnessReceipt>,
    >,
    pub admitted_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventCause {
    Invocation {
        admission: AdmissionProof,
    },
    Derived {
        admission: AdmissionProof,
        source: EventHash,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub version: u32,
    #[serde(rename = "space_id")]
    pub network_id: NetworkId,
    pub resource_id: vibeface_secure::authorization::ResourceId,
    pub cause: EventCause,
    pub scope_id: ScopeId,
    pub author: NodeId,
    pub sequence: u64,
    pub previous: Option<EventHash>,
    pub parents: Vec<EventHash>,
    pub created_at_ms: i64,
    pub body: EventBody,
    pub blobs: Vec<BlobRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SignedEvent {
    pub hash: EventHash,
    pub canonical_body: String,
    pub signature: String,
}

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
pub struct OperationRequest {
    pub operation_id: String,
    #[serde(rename = "space_id")]
    pub network_id: NetworkId,
    pub conversation_id: ScopeId,
    pub task_id: Option<String>,
    pub target_node: NodeId,
    pub session_id: String,
    pub expected_revision: u64,
    pub basis_heads: Vec<EventHash>,
    pub action: Value,
    pub resources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<crate::binding::SignedBinding>,
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
        Some("conversations" | "conversation" | "context") => Action::ConversationRead,
        Some("nodes" | "session" | "operation") => Action::OperationStatusRead,
        Some("create_conversation") => Action::ConversationCreate,
        Some("message") => Action::MessageAppend,
        Some("create_task") => Action::TaskCreate,
        Some("update_task") => match body["status"].as_str() {
            Some("accepted") => Action::TaskAccept,
            Some("cancelled") => Action::TaskCancel,
            Some("open" | "working" | "blocked" | "needs_review") => Action::TaskProgress,
            _ => anyhow::bail!("invalid task status"),
        },
        Some("rebind_execution") => Action::ExecutionRebind,
        Some("notifications") => Action::NotificationRead,
        Some("ack") => Action::NotificationAck,
        Some("export") => Action::Export,
        Some("reconcile") => Action::OperationReconcile,
        Some("resolve_unknown") => Action::OperationResolve,
        Some("operate") => match body["request"]["action"]["type"].as_str() {
            Some("open") => Action::ExecutionOpen,
            Some("read") => Action::OperationStatusRead,
            Some("send") => Action::ExecutionSend,
            Some("steer") => Action::ExecutionSteer,
            Some("interrupt") => Action::ExecutionInterrupt,
            Some("respond_request") => Action::ExecutionRespondApproval,
            _ => anyhow::bail!("unknown operation action"),
        },
        _ => anyhow::bail!("unknown method"),
    })
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
        assert!(authorization_action(&Value::Null).is_err());
        Ok(())
    }

    #[test]
    fn authorization_actions_match_nested_execution_requests() -> anyhow::Result<()> {
        for (kind, expected) in [
            ("open", Action::ExecutionOpen),
            ("read", Action::OperationStatusRead),
            ("send", Action::ExecutionSend),
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
