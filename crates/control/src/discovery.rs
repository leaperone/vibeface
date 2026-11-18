//! Shared Core/Desktop discovery wire contract.
//!
//! Discovery is deliberately a small, strict protocol.  The status-specific fields are
//! represented as optional values in the public projection so existing desktop views can render
//! them, while the serde boundary requires exactly the fields allowed by each status.

use anyhow::{Result, ensure};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{MapAccess, Visitor},
};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{net::SocketAddr, path::PathBuf};

use crate::inspection::{NodeIdentity, NodeStatus, validate_path};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeSource {
    Remembered,
    AppProfile,
    DefaultHome,
    UserService,
    RunningProcess,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    IdentityMismatch,
    MaintenanceUnavailable,
    InspectionUnavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeSummary {
    pub data_dir: PathBuf,
    pub device_id: String,
    pub sources: Vec<NodeSource>,
    pub status: NodeStatus,
    pub unavailable_reason: Option<UnavailableReason>,
    pub local_listen: Option<SocketAddr>,
    pub listen: Option<SocketAddr>,
    pub web_enabled: Option<bool>,
    pub core_version: Option<String>,
    pub user_service_cli: Option<PathBuf>,
}

impl NodeSummary {
    pub fn identity(&self) -> NodeIdentity {
        NodeIdentity {
            data_dir: self.data_dir.clone(),
            device_id: self.device_id.clone(),
        }
    }

    pub fn id(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(self.data_dir.to_string_lossy().as_bytes());
        hash.update([0]);
        hash.update(&self.device_id);
        format!("{:x}", hash.finalize())
    }

    pub fn validate(&self) -> Result<()> {
        self.identity().validate()?;
        ensure!(
            !self.sources.is_empty() && self.sources.len() <= 5,
            "节点发现来源无效。"
        );
        for (index, source) in self.sources.iter().enumerate() {
            ensure!(
                !self.sources[..index].contains(source),
                "节点发现来源重复。"
            );
        }
        if let Some(cli) = &self.user_service_cli {
            validate_path(cli)?;
            ensure!(
                self.sources.contains(&NodeSource::UserService),
                "用户服务缺少注册来源。"
            );
        }
        match self.status {
            NodeStatus::Running | NodeStatus::Stopped => {
                ensure!(
                    self.unavailable_reason.is_none(),
                    "可用节点不应携带不可用原因。"
                );
                for value in [&self.local_listen, &self.listen] {
                    ensure!(value.is_some(), "节点监听地址缺失。");
                }
                ensure!(self.web_enabled.is_some(), "节点接入状态缺失。");
                let version = self
                    .core_version
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("节点版本缺失。"))?;
                ensure!(
                    !version.is_empty()
                        && version.len() <= 160
                        && !version.chars().any(char::is_control),
                    "节点版本无效。"
                );
            }
            NodeStatus::Unavailable => {
                ensure!(self.unavailable_reason.is_some(), "节点不可用原因缺失。");
                ensure!(
                    self.local_listen.is_none()
                        && self.listen.is_none()
                        && self.web_enabled.is_none()
                        && self.core_version.is_none(),
                    "不可用节点携带了配置摘要。"
                );
            }
        }
        Ok(())
    }
}

impl Serialize for NodeSummary {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = Map::new();
        map.insert(
            "status".into(),
            serde_json::to_value(self.status).map_err(serde::ser::Error::custom)?,
        );
        map.insert("device_id".into(), Value::String(self.device_id.clone()));
        map.insert(
            "data_dir".into(),
            serde_json::to_value(&self.data_dir).map_err(serde::ser::Error::custom)?,
        );
        map.insert(
            "sources".into(),
            serde_json::to_value(&self.sources).map_err(serde::ser::Error::custom)?,
        );
        match self.status {
            NodeStatus::Running | NodeStatus::Stopped => {
                for (key, value) in [
                    (
                        "local_listen",
                        serde_json::to_value(self.local_listen)
                            .map_err(serde::ser::Error::custom)?,
                    ),
                    (
                        "listen",
                        serde_json::to_value(self.listen).map_err(serde::ser::Error::custom)?,
                    ),
                    (
                        "web_enabled",
                        serde_json::to_value(self.web_enabled)
                            .map_err(serde::ser::Error::custom)?,
                    ),
                    (
                        "core_version",
                        serde_json::to_value(self.core_version.as_deref())
                            .map_err(serde::ser::Error::custom)?,
                    ),
                ] {
                    map.insert(key.into(), value);
                }
            }
            NodeStatus::Unavailable => {
                map.insert(
                    "unavailable_reason".into(),
                    serde_json::to_value(self.unavailable_reason)
                        .map_err(serde::ser::Error::custom)?,
                );
            }
        }
        if let Some(cli) = &self.user_service_cli {
            map.insert(
                "user_service_cli".into(),
                serde_json::to_value(cli).map_err(serde::ser::Error::custom)?,
            );
        }
        Value::Object(map).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for NodeSummary {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let mut map = deserializer.deserialize_map(DuplicateMapVisitor)?;
        const KNOWN: &[&str] = &[
            "status",
            "device_id",
            "data_dir",
            "sources",
            "unavailable_reason",
            "local_listen",
            "listen",
            "web_enabled",
            "core_version",
            "user_service_cli",
        ];
        if let Some(key) = map.keys().find(|key| !KNOWN.contains(&key.as_str())) {
            return Err(serde::de::Error::custom(format!(
                "unknown discovery field: {key}"
            )));
        }
        let take = |map: &mut Map<String, Value>, key: &str| {
            map.remove(key)
                .ok_or_else(|| serde::de::Error::custom(format!("missing discovery field: {key}")))
        };
        let status: NodeStatus =
            serde_json::from_value(take(&mut map, "status")?).map_err(serde::de::Error::custom)?;
        let device_id: String = serde_json::from_value(take(&mut map, "device_id")?)
            .map_err(serde::de::Error::custom)?;
        let data_dir: PathBuf = serde_json::from_value(take(&mut map, "data_dir")?)
            .map_err(serde::de::Error::custom)?;
        let sources: Vec<NodeSource> =
            serde_json::from_value(take(&mut map, "sources")?).map_err(serde::de::Error::custom)?;
        let take_optional =
            |map: &mut Map<String, Value>,
             key: &str|
             -> std::result::Result<Option<Value>, D::Error> { Ok(map.remove(key)) };
        let unavailable_raw = take_optional(&mut map, "unavailable_reason")?;
        let local_raw = take_optional(&mut map, "local_listen")?;
        let listen_raw = take_optional(&mut map, "listen")?;
        let web_raw = take_optional(&mut map, "web_enabled")?;
        let version_raw = take_optional(&mut map, "core_version")?;
        let cli_raw = take_optional(&mut map, "user_service_cli")?;
        let unavailable_reason: Option<UnavailableReason> = unavailable_raw
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(serde::de::Error::custom)?;
        let local_listen: Option<SocketAddr> = local_raw
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(serde::de::Error::custom)?;
        let listen: Option<SocketAddr> = listen_raw
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(serde::de::Error::custom)?;
        let web_enabled: Option<bool> = web_raw
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(serde::de::Error::custom)?;
        let core_version: Option<String> = version_raw
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(serde::de::Error::custom)?;
        let user_service_cli: Option<PathBuf> = cli_raw
            .map(serde_json::from_value)
            .transpose()
            .map_err(serde::de::Error::custom)?;
        let unavailable_present = unavailable_raw.is_some();
        let config_present = local_raw.is_some()
            || listen_raw.is_some()
            || web_raw.is_some()
            || version_raw.is_some();
        let node = Self {
            status,
            device_id,
            data_dir,
            sources,
            unavailable_reason,
            local_listen,
            listen,
            web_enabled,
            core_version,
            user_service_cli,
        };
        // Presence and null are both rejected for forbidden fields by checking the raw options.
        if matches!(node.status, NodeStatus::Unavailable) && config_present {
            return Err(serde::de::Error::custom(
                "unavailable node must not carry configuration fields",
            ));
        }
        if matches!(node.status, NodeStatus::Running | NodeStatus::Stopped) && unavailable_present {
            return Err(serde::de::Error::custom(
                "running or stopped node must not carry unavailable_reason",
            ));
        }
        node.validate().map_err(serde::de::Error::custom)?;
        Ok(node)
    }
}

struct DuplicateMapVisitor;
impl<'de> Visitor<'de> for DuplicateMapVisitor {
    type Value = Map<String, Value>;
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a discovery node object")
    }
    fn visit_map<A: MapAccess<'de>>(
        self,
        mut access: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut map = Map::new();
        while let Some((key, value)) = access.next_entry::<String, Value>()? {
            if map.insert(key.clone(), value).is_some() {
                return Err(serde::de::Error::custom(format!(
                    "duplicate discovery field: {key}"
                )));
            }
        }
        Ok(map)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    pub nodes: Vec<NodeSummary>,
    pub warnings: Vec<String>,
    pub user_service_registered: bool,
}

impl Discovery {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.nodes.len() <= 64 && self.warnings.len() <= 128,
            "节点发现结果超出范围。"
        );
        for (index, node) in self.nodes.iter().enumerate() {
            node.validate()?;
            ensure!(
                !self.nodes[..index]
                    .iter()
                    .any(|other| other.data_dir == node.data_dir),
                "节点目录重复。"
            );
        }
        ensure!(
            self.warnings.iter().all(|value| !value.is_empty()
                && value.len() <= 80
                && value.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')),
            "节点发现提示无效。"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn base(status: &str) -> Value {
        let mut value = serde_json::json!({"status":status,"device_id":"a".repeat(64),"data_dir":"/tmp/node","sources":["remembered"]});
        if status == "unavailable" {
            value["unavailable_reason"] = serde_json::json!("inspection_unavailable");
        } else {
            value["local_listen"] = serde_json::json!("127.0.0.1:1");
            value["listen"] = serde_json::json!("127.0.0.1:2");
            value["web_enabled"] = serde_json::json!(true);
            value["core_version"] = serde_json::json!("0.0.1");
        }
        value
    }
    #[test]
    fn status_specific_fields_are_strict() {
        for status in ["running", "stopped"] {
            assert!(serde_json::from_value::<NodeSummary>(base(status)).is_ok());
        }
        assert!(serde_json::from_value::<NodeSummary>(base("unavailable")).is_ok());
        let mut invalid = base("unavailable");
        invalid["core_version"] = Value::Null;
        assert!(serde_json::from_value::<NodeSummary>(invalid).is_err());
        let mut invalid = base("running");
        invalid["unavailable_reason"] = Value::Null;
        assert!(serde_json::from_value::<NodeSummary>(invalid).is_err());
        let duplicate = r#"{"status":"stopped","status":"stopped","device_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","data_dir":"/tmp/node","sources":["remembered"],"local_listen":"127.0.0.1:1","listen":"127.0.0.1:2","web_enabled":true,"core_version":"0.0.1"}"#;
        assert!(serde_json::from_str::<NodeSummary>(duplicate).is_err());
    }
}
