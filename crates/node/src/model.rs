use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type NetworkId = String;
pub type NodeId = String;
pub type EventHash = String;
pub type ScopeId = String;
pub const WIRE_VERSION: u32 = 1;
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
pub struct Event {
    pub version: u32,
    pub network_id: NetworkId,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    Gateway,
    Relay,
    Replica,
    Scheduler,
    Executor,
}

pub fn validate_role_dependencies(roles: &[NodeRole]) -> anyhow::Result<()> {
    anyhow::ensure!(
        roles.contains(&NodeRole::Replica)
            || !roles.iter().any(|role| {
                matches!(
                    role,
                    NodeRole::Gateway | NodeRole::Scheduler | NodeRole::Executor
                )
            }),
        "gateway, scheduler and executor roles require replica"
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodeCertificate {
    pub node_id: NodeId,
    pub public_key: String,
    pub roles: Vec<NodeRole>,
    pub revoked: bool,
}

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
    pub network_id: NetworkId,
    pub scope_id: ScopeId,
    pub heads: Vec<EventHash>,
    pub events: Vec<SignedEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
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
