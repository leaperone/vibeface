//! Personal workspace commands and projections share the signed event log.
use crate::{
    model::{DeviceId, EventHash, ScopeId},
    scheduling::SessionScope,
};
use serde::{Deserialize, Serialize};
pub use vibeface_protocol::workspace::{
    Assignment, AssignmentState, AssignmentSubmission, Channel, InputControl, InputIntent,
    InputRequest, InputRoute, InputTarget, QueuedInput, TaskAction, TaskState, WorkspaceCommand,
    WorkspaceCommandReceipt, WorkspaceCoverage, WorkspaceInput, WorkspaceSpeaker, WorkspaceState,
    WorkspaceTask,
};

pub const WORKSPACE_SCOPE: &str = "workspace";
pub const MAX_AUTOMATIC_ASSIGNMENTS: usize = 8;
pub const MAX_TASK_DEPTH: usize = 4;
pub const MAX_MEMBER_REPLIES: usize = 2;

/// A ControlInput qualification or CAS check rejected the command before any append.
#[derive(Debug)]
pub(crate) struct WorkspaceCommandNotSaved {
    pub command_id: String,
    pub reason: String,
}

impl WorkspaceCommandNotSaved {
    pub(crate) fn new(command_id: &str, reason: &'static str) -> Self {
        Self {
            command_id: command_id.to_owned(),
            reason: reason.to_owned(),
        }
    }
}

impl std::fmt::Display for WorkspaceCommandNotSaved {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for WorkspaceCommandNotSaved {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedInputTarget {
    pub target: InputTarget,
    pub conversation_id: ScopeId,
    pub bot_id: String,
    pub owner_node: DeviceId,
    pub owner_epoch: u64,
    pub scope: SessionScope,
    pub task_id: Option<String>,
    pub task_revision: Option<u64>,
    pub assignment_id: Option<String>,
    pub channel_id: Option<String>,
    pub channel_owner_epoch: Option<u64>,
    pub reply_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceHistoryItem {
    pub speaker: WorkspaceSpeaker,
    pub event_hash: EventHash,
    pub created_at_ms: i64,
    pub scope_id: String,
    pub kind: String,
    pub body: crate::model::EventBody,
}
