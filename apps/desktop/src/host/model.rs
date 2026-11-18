use anyhow::{Result, ensure};
use serde::Serialize;
use std::path::Path;

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
pub use vibeface_control::discovery::{Discovery, NodeSource, NodeSummary as LocalNode};
pub(crate) use vibeface_control::inspection::validate_device_id as device_id;
pub use vibeface_control::inspection::{Inspection as Inspect, NodeIdentity, NodeStatus};
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
