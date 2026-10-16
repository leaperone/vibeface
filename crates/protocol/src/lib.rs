use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

pub const VERSION: u32 = 1;
pub const MAX_TEXT_BYTES: usize = 65_536;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum RpcId {
    Number(i64),
    String(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum Command {
    Snapshot,
    ListTasks,
    ResolveCommand {
        command_id: String,
        evidence: String,
    },
    ReadTask {
        task_id: String,
    },
    CreateTask {
        task: TaskInput,
    },
    UpdateTask {
        task_id: String,
        update: TaskUpdate,
    },
    ControlTask {
        task_id: String,
        action: TaskControl,
    },
    TaskCommand {
        task_id: String,
        command: Box<Command>,
    },
    ChatHistory {
        #[serde(default)]
        after: u64,
        #[serde(default = "history_limit")]
        limit: usize,
    },
    /// Internal ledger command; client submissions are rejected.
    RunTask {
        task_id: String,
        captured_event_seq: u64,
    },
    ListThreads {
        #[serde(default)]
        cursor: Option<String>,
    },
    ReadThread {
        thread_id: String,
    },
    ResumeThread {
        thread_id: String,
    },
    ListTurns {
        thread_id: String,
        #[serde(default)]
        cursor: Option<String>,
    },
    StartThread {
        cwd: String,
    },
    StartTurn {
        thread_id: String,
        text: String,
    },
    SteerTurn {
        thread_id: String,
        turn_id: String,
        text: String,
    },
    InterruptTurn {
        thread_id: String,
        turn_id: String,
    },
    RespondRequest {
        request_id: RpcId,
        response: Value,
    },
    Manage {
        prompt: String,
    },
}

impl Command {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(thread_id) = self.thread_id() {
            validate_id(thread_id)?;
        }
        let bounded = |s: &str| {
            if s.trim().is_empty() || s.len() > MAX_TEXT_BYTES {
                Err("text must be nonempty and at most 65536 bytes".into())
            } else {
                Ok(())
            }
        };
        match self {
            Self::ResolveCommand {
                command_id,
                evidence,
            } => {
                validate_id(command_id)?;
                bounded_text(evidence)
            }
            Self::ReadTask { task_id } | Self::RunTask { task_id, .. } => validate_id(task_id),
            Self::CreateTask { task } => task.validate(),
            Self::UpdateTask { task_id, update } => {
                validate_id(task_id)?;
                update.validate()
            }
            Self::ControlTask { task_id, action } => {
                validate_id(task_id)?;
                action.validate()
            }
            Self::TaskCommand { task_id, command } => {
                validate_id(task_id)?;
                if !matches!(
                    command.as_ref(),
                    Self::ReadThread { .. }
                        | Self::ResumeThread { .. }
                        | Self::ListTurns { .. }
                        | Self::StartThread { .. }
                        | Self::StartTurn { .. }
                        | Self::SteerTurn { .. }
                        | Self::InterruptTurn { .. }
                ) {
                    return Err("unsupported task-scoped command".into());
                }
                command.validate()
            }
            Self::ChatHistory { limit, .. } if *limit == 0 || *limit > 1000 => {
                Err("history limit must be 1-1000".into())
            }
            Self::SteerTurn { text, turn_id, .. } => {
                validate_id(turn_id)?;
                bounded(text)
            }
            Self::StartTurn { text, .. } => bounded(text),
            Self::InterruptTurn { turn_id, .. } => validate_id(turn_id),
            Self::Manage { prompt } => bounded(prompt),
            Self::StartThread { cwd } => bounded(cwd),
            Self::RespondRequest { response, .. }
                if serde_json::to_vec(response)
                    .map_err(|e| e.to_string())?
                    .len()
                    > MAX_TEXT_BYTES =>
            {
                Err("response is too large".into())
            }
            _ => Ok(()),
        }
    }

    pub fn thread_id(&self) -> Option<&str> {
        match self {
            Self::ReadThread { thread_id }
            | Self::ResumeThread { thread_id }
            | Self::ListTurns { thread_id, .. }
            | Self::StartTurn { thread_id, .. }
            | Self::SteerTurn { thread_id, .. }
            | Self::InterruptTurn { thread_id, .. } => Some(thread_id),
            Self::TaskCommand { command, .. } => command.thread_id(),
            _ => None,
        }
    }

    pub fn task_id(&self) -> Option<&str> {
        match self {
            Self::ReadTask { task_id }
            | Self::UpdateTask { task_id, .. }
            | Self::ControlTask { task_id, .. }
            | Self::TaskCommand { task_id, .. }
            | Self::RunTask { task_id, .. } => Some(task_id),
            Self::CreateTask { task } => Some(&task.id),
            _ => None,
        }
    }
}

fn history_limit() -> usize {
    100
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    NeedsContinue,
    WaitingUser,
    Blocked,
    Paused,
    AwaitingReview,
    Completed,
    Unknown,
}

impl TaskStatus {
    pub fn allows_auto(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::NeedsContinue)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TaskInput {
    pub id: String,
    pub goal: String,
    pub cwd: String,
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub auto_continue: bool,
    #[serde(default)]
    pub decision_limit: u32,
}
impl TaskInput {
    pub fn validate(&self) -> Result<(), String> {
        validate_id(&self.id)?;
        bounded_text(&self.goal)?;
        bounded_text(&self.cwd)?;
        if self.acceptance_criteria.is_empty() || self.acceptance_criteria.len() > 32 {
            return Err("task requires 1-32 acceptance criteria".into());
        }
        for criterion in &self.acceptance_criteria {
            bounded_text(criterion)?;
        }
        if self.decision_limit > 1000 || (self.auto_continue && self.decision_limit == 0) {
            return Err("automatic tasks require 1-1000 decisions".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TaskUpdate {
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub evidence: Option<String>,
    #[serde(default)]
    pub thread_ids: Option<Vec<String>>,
}
impl TaskUpdate {
    pub fn validate(&self) -> Result<(), String> {
        if self.status == Some(TaskStatus::Completed) {
            return Err("completion requires explicit user acceptance".into());
        }
        for text in [&self.summary, &self.evidence].into_iter().flatten() {
            bounded_text(text)?;
        }
        if let Some(threads) = &self.thread_ids {
            if threads.len() > 64 {
                return Err("at most 64 threads per task".into());
            }
            for thread in threads {
                validate_id(thread)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskControl {
    Pause,
    Resume {
        auto_continue: bool,
        decision_limit: u32,
    },
    Accept {
        evidence: String,
    },
}
impl TaskControl {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Resume {
                auto_continue,
                decision_limit,
            } if *decision_limit > 1000 || (*auto_continue && *decision_limit == 0) => {
                Err("automatic tasks require 1-1000 decisions".into())
            }
            Self::Accept { evidence } => bounded_text(evidence),
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TaskRecord {
    pub id: String,
    pub goal: String,
    pub cwd: String,
    pub acceptance_criteria: Vec<String>,
    pub thread_ids: Vec<String>,
    pub status: TaskStatus,
    pub auto_continue: bool,
    pub remaining_decisions: u32,
    pub summary: Option<String>,
    pub evidence: Option<String>,
    pub dirty_seq: u64,
    pub handled_seq: u64,
    pub manager_session: Option<String>,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ChatMessage {
    pub seq: u64,
    pub command_id: String,
    pub role: String,
    pub content: Value,
    pub at_ms: u64,
}

fn bounded_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
        Err("text must be nonempty and at most 65536 bytes".into())
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CommandRequest {
    pub id: String,
    pub command: Command,
}

impl CommandRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_id(&self.id)?;
        self.command.validate()
    }
}

pub fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
    {
        return Err(
            "id must contain 1-128 ASCII letters, digits, hyphens, dots or underscores".into(),
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Unknown,
}

impl CommandStatus {
    pub fn is_pending(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Unknown)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CommandRecord {
    pub request: CommandRequest,
    pub status: CommandStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub updated_at_ms: u64,
}

impl CommandRecord {
    pub fn new(request: CommandRequest, status: CommandStatus) -> Self {
        Self {
            request,
            status,
            output: None,
            error: None,
            updated_at_ms: now_ms(),
        }
    }

    pub fn finish(mut self, result: Result<Value, String>) -> Self {
        self.updated_at_ms = now_ms();
        match result {
            Ok(value) => {
                self.status = CommandStatus::Succeeded;
                self.output = Some(value);
                self.error = None;
            }
            Err(error) => {
                self.status = CommandStatus::Failed;
                self.error = Some(error);
            }
        }
        self
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct BridgeEvent {
    pub seq: u64,
    pub at_ms: u64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub payload: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeviceStatus {
    pub device_id: String,
    pub online: bool,
    pub last_seen_ms: Option<u64>,
    pub last_event_seq: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToServer {
    Hello { version: u32, device_id: String },
    Heartbeat,
    Event { event: BridgeEvent },
    CommandUpdate { record: CommandRecord },
    CommandUnknown { id: String, error: String },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToBridge {
    Welcome {
        version: u32,
        last_event_seq: u64,
        pending_command_ids: Vec<String>,
    },
    Ack {
        seq: u64,
    },
    Command {
        request: CommandRequest,
    },
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn command_contract_round_trip_and_validation() {
        let input = json!({"id":"job-1","command":{"kind":"start_turn","params":{"thread_id":"t1","text":"hello"}}});
        let command: CommandRequest = serde_json::from_value(input.clone()).unwrap();
        command.validate().unwrap();
        assert_eq!(serde_json::to_value(command).unwrap(), input);
        assert!(validate_id("../bad").is_err());
        assert!(Command::Manage { prompt: " ".into() }.validate().is_err());
    }

    #[test]
    fn result_preserves_failure_and_unknown_is_not_success() {
        let request = CommandRequest {
            id: "a".into(),
            command: Command::Snapshot,
        };
        let record =
            CommandRecord::new(request, CommandStatus::Running).finish(Err("offline".into()));
        assert_eq!(record.status, CommandStatus::Failed);
        assert_eq!(record.error.as_deref(), Some("offline"));
        assert!(CommandStatus::Unknown.is_pending());
        assert!(!CommandStatus::Succeeded.is_pending());
    }
}
