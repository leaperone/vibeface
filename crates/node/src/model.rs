use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type NetworkId = String;
pub type DeviceId = String;
pub type EventHash = String;
pub type ScopeId = String;
/// `{author: max_seq}`: the exact set of events a store holds, because every stored event's
/// `prev` and `deps` are stored too.
pub type VersionVector = BTreeMap<DeviceId, u64>;
pub const WIRE_VERSION: u32 = 1;
pub const MAX_EVENT_BYTES: usize = 1_048_576;
pub const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct BlobRef {
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// How a coordination turn ended. Partial text of stopped or interrupted turns is kept.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReplyStatus {
    Completed,
    Stopped,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyMode {
    Continue,
    Report,
    Off,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// `codex`: a Codex turn this device's coordination Agent sent and the Node watches.
/// `task`: work handed to a device's coordination Agent as a task turn.
pub enum WorkKind {
    Codex,
    Task,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkPhase {
    Started,
    Running,
    Interrupting,
    Finishing,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkOutcome {
    Completed,
    Failed,
    Interrupted,
    /// Never started; retrying with the same `work_id` is safe.
    Rejected,
    /// The real result cannot be proven; only the user or a later probe settles it.
    Unverified,
    /// Its target was removed before reporting a result; written by the conversation's owner.
    Abandoned,
}

/// How soon the user should look at a triaged session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Attention,
    Urgent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AlertResolution {
    /// A decision went back to the session.
    Decided,
    Dismissed,
}

/// Scope of the triage cards every device shows.
pub const ATTENTION_SCOPE: &str = "attention";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct OpenWork {
    pub work_id: String,
    pub summary: String,
}

/// The user's device signature over an event body the connected Node writes on its behalf.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Attestation {
    pub device_id: DeviceId,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventBody {
    ConversationCreated {
        title: String,
    },
    /// The user's choice of scheduling Node; the highest epoch wins, ties pick the smaller hash.
    ConversationOwner {
        scheduler_node: DeviceId,
        epoch: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    BotCreated {
        bot_id: String,
        name: String,
        instructions: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<BotProject>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    /// `report_results` and `attachments` belong to user messages; `in_reply_to` and `status` to
    /// assistant replies. `attachments` repeats the event's blobs so the user's signature covers them.
    MessagePosted {
        message_id: String,
        role: Role,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<BlobRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report_results: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        in_reply_to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<ReplyStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    /// Written by the target Node when it accepts the work, so no progress precedes it.
    WorkDispatched {
        work_id: String,
        trigger: String,
        dispatcher: DeviceId,
        call_ref: String,
        kind: WorkKind,
        cwd: String,
        /// A short line for the user; `prompt` is what the work gets.
        title: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        continues: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    WorkProgress {
        work_id: String,
        phase: WorkPhase,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    /// Written by the target. The user settles an `unverified` result through any entry Node,
    /// signed; the owner records `abandoned` after the target is removed.
    WorkFinished {
        work_id: String,
        outcome: WorkOutcome,
        result_text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    AutonomySet {
        mode: AutonomyMode,
        version: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    NotificationAcked {
        notification_id: String,
        client_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    /// A triaged session that needs the user, in scope `attention`; a later raise of the same
    /// `matter` (an escalation) replaces the card.
    AttentionRaised {
        alert_id: String,
        matter: String,
        agent: String,
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        cwd: String,
        severity: Severity,
        summary: String,
        count: u64,
    },
    AttentionResolved {
        alert_id: String,
        resolution: AlertResolution,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    /// Written only by the owner in a read-only closing turn; covered events stay stored.
    ContextCheckpoint {
        covers: VersionVector,
        summary: String,
        standing_instructions: Vec<String>,
        open_work: Vec<OpenWork>,
        decisions: Vec<String>,
    },
}

impl EventBody {
    /// Present on writes that need the user's own device signature; see `crypto::validate_event_shape`.
    pub fn attestation(&self) -> Option<&Attestation> {
        match self {
            Self::ConversationOwner { attestation, .. }
            | Self::BotCreated { attestation, .. }
            | Self::MessagePosted { attestation, .. }
            | Self::WorkFinished { attestation, .. }
            | Self::AutonomySet { attestation, .. }
            | Self::NotificationAcked { attestation, .. }
            | Self::AttentionResolved { attestation, .. } => attestation.as_ref(),
            _ => None,
        }
    }
    pub fn work_id(&self) -> Option<&str> {
        match self {
            Self::WorkDispatched { work_id, .. }
            | Self::WorkProgress { work_id, .. }
            | Self::WorkFinished { work_id, .. } => Some(work_id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub version: u32,
    pub network_id: NetworkId,
    pub author: DeviceId,
    pub seq: u64,
    pub prev: Option<EventHash>,
    pub deps: Vec<EventHash>,
    pub scope_id: ScopeId,
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

/// A capability this Node declares in its own `config.json`; membership never carries roles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    Gateway,
    Relay,
    Replica,
    Scheduler,
}

pub fn validate_role_dependencies(roles: &[NodeRole]) -> anyhow::Result<()> {
    anyhow::ensure!(
        roles.contains(&NodeRole::Replica)
            || !roles
                .iter()
                .any(|role| matches!(role, NodeRole::Gateway | NodeRole::Scheduler)),
        "gateway and scheduler roles require replica"
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HashedEvent {
    pub hash: EventHash,
    #[serde(flatten)]
    pub event: Event,
}

/// A conversation's events up to `basis`, in the deterministic linear order shared by prompts and UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    pub network_id: NetworkId,
    pub scope_id: ScopeId,
    pub basis: VersionVector,
    pub events: Vec<HashedEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BotProject {
    pub target_node: DeviceId,
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
    pub attestation: Attestation,
    pub owner: FirstOwner,
}

/// The user's signed choice of a new conversation's first scheduling Node. Any entry can write
/// it, so a retry through another entry keeps the same owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FirstOwner {
    pub scheduler_node: DeviceId,
    pub attestation: Attestation,
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
    pub owner: Option<DeviceId>,
    pub owner_epoch: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub message_id: String,
    pub role: Role,
    pub text: String,
    pub event_hash: EventHash,
    pub author: DeviceId,
    pub created_at_ms: i64,
    pub in_reply_to: Option<String>,
    pub status: Option<ReplyStatus>,
    pub report_results: Option<bool>,
    pub blobs: Vec<BlobRef>,
}
/// Derived from the event that raised it; its ID is that event's hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Notification {
    pub notification_id: String,
    pub conversation_id: ScopeId,
    pub kind: String,
    pub text: String,
}
/// DAG projection of one `work_id`; the target Node's local state machine is the authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkProjection {
    pub work_id: String,
    pub conversation_id: ScopeId,
    pub target_node: DeviceId,
    pub trigger: String,
    pub dispatcher: DeviceId,
    pub kind: WorkKind,
    pub cwd: String,
    pub title: String,
    pub prompt: String,
    pub phase: Option<WorkPhase>,
    pub summary: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub outcome: Option<WorkOutcome>,
    pub result_text: String,
    pub failure_reason: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Autonomy {
    pub mode: AutonomyMode,
    pub version: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BudgetUsage {
    pub day: String,
    pub used: u64,
    pub limit: u64,
}

/// Per-event result of ingest; a batch never aborts on one bad event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum IngestStatus {
    Accepted,
    Duplicate,
    Pending,
    Conflict,
    Rejected(String),
}
