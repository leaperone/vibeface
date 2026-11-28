pub mod amount;
pub mod client_frame;
pub mod delivery;

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Whether this Node may append to its own log (design 5.7 and 5.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteState {
    /// Started from a database file it never wrote from, with other member Nodes; waits for
    /// them to confirm it holds nothing newer of ours.
    Aligning,
    /// No other member Node answered; writable once the user confirms no backup was restored.
    ReadOnly,
    Writable,
    /// Another copy of this device's log wrote different events at the same sequence.
    Forked,
}

impl WriteState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aligning => "aligning",
            Self::ReadOnly => "read_only",
            Self::Writable => "writable",
            Self::Forked => "forked",
        }
    }
}

impl std::str::FromStr for WriteState {
    type Err = serde::de::value::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::deserialize(serde::de::value::StrDeserializer::new(value))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum RpcId {
    Number(i64),
    String(String),
}

/// Serde's internally tagged unit decoder ignores map entries even with deny_unknown_fields.
pub fn empty_operation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<(), D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Empty {}
    Empty::deserialize(deserializer).map(|_| ())
}

pub fn valid_business_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
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
    fn node_write_states_keep_their_wire_and_storage_names() {
        for (state, name) in [
            (WriteState::Aligning, "aligning"),
            (WriteState::ReadOnly, "read_only"),
            (WriteState::Writable, "writable"),
            (WriteState::Forked, "forked"),
        ] {
            assert_eq!(state.as_str(), name);
            assert_eq!(serde_json::to_value(state).unwrap(), json!(name));
            assert_eq!(name.parse(), Ok(state));
        }
        assert!("unknown".parse::<WriteState>().is_err());
    }

    #[test]
    fn business_ids_allow_colons_without_widening_protocol_ids() {
        for id in ["Az09-_.:", ".", "..", ":", &"a".repeat(128)] {
            assert!(valid_business_id(id), "{id:?}");
        }
        for id in ["", "bad/id", "bad id", "bad\nid", "é", &"a".repeat(129)] {
            assert!(!valid_business_id(id), "{id:?}");
        }
        assert!(validate_id("scope:item").is_err());
    }

    #[test]
    fn ids_reject_path_components_and_rpc_ids_preserve_their_wire_type() {
        assert!(validate_id("thread-1.turn_2").is_ok());
        assert!(validate_id("").is_err());
        assert!(validate_id("../bad").is_err());
        assert!(validate_id(&"a".repeat(129)).is_err());
        let numeric: RpcId = serde_json::from_value(json!(7)).unwrap();
        let text: RpcId = serde_json::from_value(json!("7")).unwrap();
        assert_ne!(numeric, text);
        assert_eq!(serde_json::to_value(numeric).unwrap(), json!(7));
        assert_eq!(serde_json::to_value(text).unwrap(), json!("7"));
    }
}

pub mod workspace;

pub mod history;

pub mod message_search;
