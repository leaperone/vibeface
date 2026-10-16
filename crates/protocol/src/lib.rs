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
        let bounded = |s: &str| {
            if s.trim().is_empty() || s.len() > MAX_TEXT_BYTES {
                Err("text must be nonempty and at most 65536 bytes".into())
            } else {
                Ok(())
            }
        };
        match self {
            Self::StartTurn { text, .. } | Self::SteerTurn { text, .. } => bounded(text),
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
            _ => None,
        }
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
