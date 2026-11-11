use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum RpcId {
    Number(i64),
    String(String),
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
