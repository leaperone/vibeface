use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type ClusterId = String;
pub type PeerId = String;
pub type EventHash = String;
pub type ScopeId = String;
pub const WIRE_VERSION: u32 = 2;
pub const MAX_EVENT_BYTES: usize = 1_048_576;
pub const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct BlobRef {
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageKind {
    Task,
    Progress,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentMessage {
    pub message_id: String,
    pub target_node: PeerId,
    pub origin_trigger: EventHash,
    pub in_reply_to: Option<EventHash>,
    pub kind: AgentMessageKind,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventBody {
    AgentMessage {
        message: AgentMessage,
    },
    ConversationCreated {
        title: String,
    },
    BotCreated {
        bot_id: String,
        name: String,
        instructions: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<BotProject>,
        membership_role_proof: Box<crate::identity::ClusterState>,
    },
    MessagePosted {
        message_id: String,
        role: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scheduler_node: Option<PeerId>,
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
    pub cluster_id: ClusterId,
    pub scope_id: ScopeId,
    pub author: PeerId,
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
    pub cluster_id: ClusterId,
    pub scope_id: ScopeId,
    pub heads: Vec<EventHash>,
    pub events: Vec<SignedEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    pub cluster_id: ClusterId,
    pub scope_id: ScopeId,
    pub heads: Vec<EventHash>,
    pub events: Vec<Event>,
    pub missing: MissingDependencies,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BotProject {
    pub target_node: PeerId,
    pub cwd: String,
}

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
    pub scheduler_node: PeerId,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    Complete,
    Partial,
    Unreachable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosureSync {
    pub scope_id: ScopeId,
    pub target: PeerId,
    pub state: SyncState,
    pub missing: MissingDependencies,
    pub processed: usize,
    pub changed: bool,
    pub errors: Vec<String>,
}
impl ClosureSync {
    pub fn require_complete(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.state == SyncState::Complete,
            "history is {:?}: {} events, {} blobs, {} conflicts",
            self.state,
            self.missing.events.len(),
            self.missing.blobs.len(),
            self.missing.conflicts.len()
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncReport {
    pub state: SyncState,
    pub scopes: Vec<ClosureSync>,
    pub unreachable_sources: Vec<PeerId>,
}
