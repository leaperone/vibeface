use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

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
    ReadThread {
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
}

impl Command {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(thread_id) = self.thread_id() {
            validate_id(thread_id)?;
        }
        match self {
            Self::SteerTurn { text, turn_id, .. } => {
                validate_id(turn_id)?;
                bounded_text(text)
            }
            Self::StartTurn { text, .. } => bounded_text(text),
            Self::InterruptTurn { turn_id, .. } => validate_id(turn_id),
            Self::StartThread { cwd } => bounded_text(cwd),
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
            | Self::ListTurns { thread_id, .. }
            | Self::StartTurn { thread_id, .. }
            | Self::SteerTurn { thread_id, .. }
            | Self::InterruptTurn { thread_id, .. } => Some(thread_id),
            _ => None,
        }
    }
}

fn bounded_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
        Err("text must be nonempty and at most 65536 bytes".into())
    } else {
        Ok(())
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
        let input = json!({"kind":"start_turn","params":{"thread_id":"t1","text":"hello"}});
        let command: Command = serde_json::from_value(input.clone()).unwrap();
        command.validate().unwrap();
        assert_eq!(serde_json::to_value(command).unwrap(), input);
        assert!(validate_id("../bad").is_err());
        assert!(validate_id(&"a".repeat(129)).is_err());
        assert!(
            Command::StartTurn {
                thread_id: "t1".into(),
                text: " ".into()
            }
            .validate()
            .is_err()
        );
        assert!(
            Command::StartTurn {
                thread_id: "t1".into(),
                text: "a".repeat(MAX_TEXT_BYTES + 1)
            }
            .validate()
            .is_err()
        );
        assert!(
            Command::SteerTurn {
                thread_id: "t1".into(),
                turn_id: "../bad".into(),
                text: "hello".into()
            }
            .validate()
            .is_err()
        );
        assert!(
            Command::RespondRequest {
                request_id: RpcId::Number(7),
                response: json!("a".repeat(MAX_TEXT_BYTES))
            }
            .validate()
            .is_err()
        );
        let numeric: RpcId = serde_json::from_value(json!(7)).unwrap();
        let text: RpcId = serde_json::from_value(json!("7")).unwrap();
        assert_ne!(numeric, text);
        assert_eq!(serde_json::to_value(numeric).unwrap(), json!(7));
        assert_eq!(serde_json::to_value(text).unwrap(), json!("7"));
    }
}
