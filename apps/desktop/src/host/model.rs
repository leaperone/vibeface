use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub(crate) fn safe_path(path: &Path) -> Result<()> {
    let text = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("路径不是有效的 Unicode。"))?;
    ensure!(
        path.is_absolute() && text.len() <= 4096 && !text.chars().any(char::is_control),
        "请选择有效的绝对路径。"
    );
    ensure!(
        !path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir)),
        "路径不能包含上级目录。"
    );
    Ok(())
}
pub(crate) fn device_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 64
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "节点身份无效。"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodeIdentity {
    pub data_dir: PathBuf,
    pub device_id: String,
}
impl NodeIdentity {
    pub(crate) fn validate(&self) -> Result<()> {
        safe_path(&self.data_dir)?;
        device_id(&self.device_id)
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Running,
    Stopped,
    Unavailable,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Inspect {
    pub data_dir: PathBuf,
    pub device_id: String,
    pub status: NodeStatus,
    pub local_listen: String,
    pub listen: String,
    pub web_enabled: bool,
    pub core_version: String,
    #[serde(default)]
    pub error: Option<String>,
}
impl Inspect {
    pub fn identity(&self) -> NodeIdentity {
        NodeIdentity {
            data_dir: self.data_dir.clone(),
            device_id: self.device_id.clone(),
        }
    }
    pub(crate) fn validate(&self) -> Result<()> {
        self.identity().validate()?;
        ensure!(
            !self.core_version.is_empty() && self.core_version.len() <= 160,
            "节点版本无效。"
        );
        for value in [&self.local_listen, &self.listen] {
            ensure!(
                !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
                "节点监听地址无效。"
            );
        }
        Ok(())
    }
}
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalNode {
    pub data_dir: PathBuf,
    pub device_id: String,
    pub sources: Vec<NodeSource>,
    pub status: NodeStatus,
    #[serde(default)]
    pub unavailable_reason: Option<UnavailableReason>,
    #[serde(default)]
    pub local_listen: Option<String>,
    #[serde(default)]
    pub listen: Option<String>,
    #[serde(default)]
    pub web_enabled: Option<bool>,
    #[serde(default)]
    pub core_version: Option<String>,
}
impl LocalNode {
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
    fn validate(&self) -> Result<()> {
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
        if self.status == NodeStatus::Unavailable {
            ensure!(self.unavailable_reason.is_some(), "节点不可用原因缺失。");
        } else {
            let inspected = Inspect {
                data_dir: self.data_dir.clone(),
                device_id: self.device_id.clone(),
                status: self.status,
                local_listen: self.local_listen.clone().unwrap_or_default(),
                listen: self.listen.clone().unwrap_or_default(),
                web_enabled: self
                    .web_enabled
                    .ok_or_else(|| anyhow::anyhow!("节点接入状态缺失。"))?,
                core_version: self.core_version.clone().unwrap_or_default(),
                error: None,
            };
            inspected.validate()?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Discovery {
    pub nodes: Vec<LocalNode>,
    pub warnings: Vec<String>,
}
impl Discovery {
    pub(crate) fn validate(&self) -> Result<()> {
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
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeState {
    Ready,
    Missing {
        core_missing: bool,
        cli_missing: bool,
    },
}
#[derive(Clone, Debug, Serialize)]
pub struct OwnedNode {
    pub identity: NodeIdentity,
    pub process_id: Option<u32>,
}
