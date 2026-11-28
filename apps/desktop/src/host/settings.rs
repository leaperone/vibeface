use super::model::{NodeIdentity, safe_path};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{io::ErrorKind, path::Path};
use vibeface_bridge::platform::privatefs;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeMode {
    #[default]
    Undecided,
    Controller,
    Local,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    pub open_business_on_connect: bool,
    pub notifications: bool,
    pub theme: Theme,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            open_business_on_connect: true,
            notifications: true,
            theme: Theme::System,
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub node_mode: NodeMode,
    pub selected_node: Option<NodeIdentity>,
    pub remembered_data_dirs: Vec<std::path::PathBuf>,
    pub preferences: Preferences,
}
impl Settings {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.remembered_data_dirs.len() <= 16,
            "记住的节点目录过多。"
        );
        for path in &self.remembered_data_dirs {
            safe_path(path)?;
        }
        if let Some(node) = &self.selected_node {
            node.validate()?;
        }
        Ok(())
    }
    pub(crate) fn load(profile: &Path) -> Result<Self> {
        let path = profile.join("native-desktop-settings.json");
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let settings: Self = serde_json::from_slice(&privatefs::read_private_bounded(
            &path,
            64 * 1024,
            "桌面设置",
        )?)
        .context("桌面设置已损坏。")?;
        settings.validate()?;
        Ok(settings)
    }
    pub(crate) fn save(&self, profile: &Path) -> Result<()> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        ensure!(bytes.len() <= 64 * 1024, "桌面设置超出范围。");
        privatefs::write_atomic(&profile.join("native-desktop-settings.json"), &bytes)
    }
}
