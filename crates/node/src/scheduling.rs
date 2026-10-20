//! Local scheduling state is never reconstructed by importing shared conversation history.
//! A job is keyed by `(conversation, message_id | work_id)`, so retries and sync never add a second one.
use crate::{crypto::digest, model::VersionVector};
use serde::{Deserialize, Serialize};

pub const SCHEDULING_VERSION: u32 = 2;
pub const MAX_PENDING_JOBS: usize = 256;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    Human,
    WorkResult,
    /// An `executor=agent` work this Node accepted; its turn ends in `WorkFinished`, not a reply.
    AgentTask,
}

pub fn message_trigger(message_id: &str) -> String {
    format!("message:{message_id}")
}

pub fn work_trigger(work_id: &str) -> String {
    format!("work:{work_id}")
}

pub fn task_trigger(work_id: &str) -> String {
    format!("task:{work_id}")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagerAttempt {
    pub id: String,
    pub boot: String,
    pub read_only: bool,
    pub permission_version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Claimed {
        attempt: ManagerAttempt,
    },
    Running {
        attempt: ManagerAttempt,
    },
    Cancelling {
        attempt: ManagerAttempt,
    },
    Completed {
        attempt: ManagerAttempt,
    },
    Failed {
        attempt: ManagerAttempt,
        detail: String,
    },
    Cancelled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<ManagerAttempt>,
        evidence: String,
    },
    Interrupted {
        attempt: ManagerAttempt,
        detail: String,
    },
}

impl JobState {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Claimed { .. } => "claimed",
            Self::Running { .. } => "running",
            Self::Cancelling { .. } => "cancelling",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::Cancelled { .. } => "cancelled",
            Self::Interrupted { .. } => "interrupted",
        }
    }
    pub fn attempt(&self) -> Option<&ManagerAttempt> {
        match self {
            Self::Queued => None,
            Self::Cancelled { attempt, .. } => attempt.as_ref(),
            Self::Claimed { attempt }
            | Self::Running { attempt }
            | Self::Cancelling { attempt }
            | Self::Completed { attempt }
            | Self::Failed { attempt, .. }
            | Self::Interrupted { attempt, .. } => Some(attempt),
        }
    }
    pub fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. }
                | Self::Interrupted { .. }
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchedulerJob {
    pub version: u32,
    pub conversation_id: String,
    /// `message:<message_id>`, `work:<work_id>` or `task:<work_id>`.
    pub trigger: String,
    pub kind: TriggerKind,
    pub sequence: u64,
    /// The version vector the turn reads; later events are not part of its prompt.
    pub basis: VersionVector,
    /// The user message this turn ultimately answers; an agent task's own work ID.
    pub reply_to: String,
    /// The assistant reply's message ID, fixed at admission so retries reuse it.
    pub message_id: String,
    #[serde(flatten)]
    pub state: JobState,
}

impl SchedulerJob {
    pub fn receipt(&self, network: &str) -> AdmissionReceipt {
        AdmissionReceipt {
            version: self.version,
            network_id: network.into(),
            conversation_id: self.conversation_id.clone(),
            trigger: self.trigger.clone(),
            state: self.state.name().into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdmissionReceipt {
    pub version: u32,
    pub network_id: String,
    pub conversation_id: String,
    pub trigger: String,
    pub state: String,
}

/// A user message or work result for the conversation's owner, kept until the owner returns a
/// receipt for its trigger. The owner is read at delivery time, so a switch redirects it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxEntry {
    pub version: u32,
    pub network_id: String,
    pub conversation_id: String,
    pub trigger: String,
}

impl OutboxEntry {
    pub fn receipt(&self) -> AdmissionReceipt {
        AdmissionReceipt {
            version: self.version,
            network_id: self.network_id.clone(),
            conversation_id: self.conversation_id.clone(),
            trigger: self.trigger.clone(),
            state: "pending_delivery".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClaimedJob {
    pub job: SchedulerJob,
    pub previous_native_id: Option<String>,
    /// What the resumed native session has already seen; the prompt carries only the rest.
    pub covers: Option<VersionVector>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum JobOutcome {
    Completed,
    Failed(String),
    Cancelled(String),
    /// `process_unconfirmed` records a pending check for the SDK process in the same transaction.
    Interrupted {
        detail: String,
        process_unconfirmed: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SchedulerSession {
    pub version: u32,
    pub conversation_id: String,
    pub next_sequence: u64,
    pub pending: Vec<String>,
    pub active: Option<String>,
    /// The SDK's own session, cached for this conversation on this Node only. `covers` is the
    /// basis it has seen and `owner_epoch` the ownership it was built under.
    pub native_id: Option<String>,
    pub covers: VersionVector,
    pub owner_epoch: u64,
    pub sdk_config: Option<String>,
    pub permission_version: u64,
}

impl SchedulerSession {
    pub fn new(conversation: &str) -> Self {
        Self {
            version: SCHEDULING_VERSION,
            conversation_id: conversation.into(),
            next_sequence: 1,
            pending: Vec::new(),
            active: None,
            native_id: None,
            covers: VersionVector::new(),
            owner_epoch: 0,
            sdk_config: None,
            permission_version: 1,
        }
    }
}

/// A coordination process whose exit this Node cannot prove. Only the user resolves it, with evidence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingCheck {
    pub version: u32,
    pub check_id: String,
    pub conversation_id: String,
    pub trigger: String,
    pub attempt_id: String,
    pub detail: String,
    pub process_groups: Vec<u32>,
    pub created_at_ms: i64,
}

impl PendingCheck {
    pub fn id(attempt_id: &str) -> String {
        format!("manager:{attempt_id}")
    }
}

/// `scheduler:v2:<kind>:<conversation digest>:<trigger digest>`, so one conversation's
/// entries form an index range.
pub(crate) fn scheduler_key(kind: &str, conversation: &str, trigger: &str) -> String {
    format!(
        "{}{}",
        scheduler_prefix(kind, conversation),
        digest(trigger.as_bytes())
    )
}

pub(crate) fn scheduler_prefix(kind: &str, conversation: &str) -> String {
    format!("scheduler:v2:{kind}:{}:", digest(conversation.as_bytes()))
}
