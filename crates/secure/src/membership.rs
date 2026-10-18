//! Local Node capabilities and canonical signed JSON encoding.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    Gateway,
    Relay,
    Replica,
    Scheduler,
    Executor,
}

pub fn validate_role_dependencies(roles: &[NodeRole]) -> anyhow::Result<()> {
    anyhow::ensure!(
        roles.contains(&NodeRole::Replica)
            || !roles
                .iter()
                .any(|role| { matches!(role, NodeRole::Gateway | NodeRole::Scheduler) }),
        "gateway and scheduler roles require replica"
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodeCertificate {
    pub node_id: String,
    pub public_key: String,
    pub roles: Vec<NodeRole>,
    pub revoked: bool,
}

/// This restricted JSON encoding is shared with the browser signing adapter.
pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    fn normalize(value: &mut Value) -> Result<()> {
        match value {
            Value::Object(object) => {
                object.sort_keys();
                for value in object.values_mut() {
                    normalize(value)?;
                }
            }
            Value::Array(array) => {
                for value in array {
                    normalize(value)?;
                }
            }
            Value::Number(number) => {
                let valid = number.as_u64().is_some_and(|n| n <= MAX_SAFE_INTEGER)
                    || number
                        .as_i64()
                        .is_some_and(|n| n.unsigned_abs() <= MAX_SAFE_INTEGER);
                ensure!(valid, "signed JSON requires safe integers");
            }
            _ => {}
        }
        Ok(())
    }
    let mut value = serde_json::to_value(value)?;
    normalize(&mut value)?;
    let bytes = serde_json::to_vec(&value)?;
    ensure!(bytes.len() <= 2 * 1024 * 1024, "identity payload too large");
    Ok(bytes)
}
