//! Local scheduling state is never reconstructed by importing shared conversation history.
use crate::{
    crypto::digest,
    model::{BlobRef, EventBody},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const SCHEDULING_VERSION: u32 = 1;
pub const MAX_PENDING_JOBS: usize = 256;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    Human,
    Autonomous,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagerAttempt {
    pub id: String,
    pub boot: String,
    pub read_only: bool,
    pub permission_version: u64,
    #[serde(default)]
    pub followup_version: Option<u64>,
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
    Unknown {
        attempt: ManagerAttempt,
        detail: String,
        fenced: bool,
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
            Self::Unknown { .. } => "unknown",
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
            | Self::Unknown { attempt, .. } => Some(attempt),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchedulerJob {
    pub version: u32,
    pub conversation_id: String,
    pub trigger: String,
    pub target_node: String,
    pub kind: TriggerKind,
    pub sequence: u64,
    pub basis_heads: Vec<String>,
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
            target_node: self.target_node.clone(),
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
    pub target_node: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxEntry {
    pub version: u32,
    pub network_id: String,
    pub conversation_id: String,
    pub trigger: String,
    pub target_node: String,
    pub delivered: bool,
}

impl OutboxEntry {
    pub fn receipt(&self) -> AdmissionReceipt {
        AdmissionReceipt {
            version: self.version,
            network_id: self.network_id.clone(),
            conversation_id: self.conversation_id.clone(),
            trigger: self.trigger.clone(),
            target_node: self.target_node.clone(),
            state: if self.delivered {
                "delivered"
            } else {
                "pending_delivery"
            }
            .into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClaimedJob {
    pub job: SchedulerJob,
    pub previous_native_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum JobOutcome {
    Completed,
    Failed(String),
    Cancelled(String),
    Unknown(String),
}

#[derive(Debug, Clone)]
pub struct SchedulerPublication {
    pub job: SchedulerJob,
    pub event_hashes: Vec<String>,
}

#[derive(Debug, Default)]
pub struct SchedulerPublicationBatch {
    pub published: Vec<SchedulerPublication>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SchedulerTerminalPublication {
    pub version: u32,
    pub job: SchedulerJob,
    pub outcome: JobOutcome,
    pub events: Vec<(EventBody, Vec<BlobRef>)>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SchedulerSession {
    pub version: u32,
    pub conversation_id: String,
    pub target_node: String,
    pub next_sequence: u64,
    pub pending: Vec<String>,
    pub active: Option<String>,
    pub unresolved: BTreeSet<String>,
    pub native_id: Option<String>,
    pub sdk_config: Option<String>,
    pub permission_version: u64,
}

impl SchedulerSession {
    pub fn new(conversation: &str, target: &str) -> Self {
        Self {
            version: SCHEDULING_VERSION,
            conversation_id: conversation.into(),
            target_node: target.into(),
            next_sequence: 1,
            pending: Vec::new(),
            active: None,
            unresolved: BTreeSet::new(),
            native_id: None,
            sdk_config: None,
            permission_version: 1,
        }
    }
}

pub(crate) fn scheduler_key(kind: &str, conversation: &str, target: &str, trigger: &str) -> String {
    format!(
        "scheduler:v1:{kind}:{}",
        digest(
            &serde_json::to_vec(&(conversation, target, trigger)).expect("string tuple serializes")
        )
    )
}
