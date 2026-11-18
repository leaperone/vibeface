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
pub(crate) use vibeface_control::inspection::validate_device_id as device_id;
pub use vibeface_control::inspection::{Inspection as Inspect, NodeIdentity, NodeStatus};
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
    #[serde(default)]
    pub user_service_cli: Option<PathBuf>,
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
        if let Some(cli) = &self.user_service_cli {
            safe_path(cli)?;
            ensure!(
                self.sources.contains(&NodeSource::UserService),
                "用户服务缺少注册来源。"
            );
        }
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
            for listen in [&self.local_listen, &self.listen] {
                let value = listen
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("节点监听地址缺失。"))?;
                value.parse::<std::net::SocketAddr>()?;
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
        Ok(())
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Discovery {
    pub nodes: Vec<LocalNode>,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub user_service_registered: bool,
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
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryState {
    #[default]
    Scanning,
    Ready,
    Unavailable,
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
