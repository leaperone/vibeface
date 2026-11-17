//! Successful managed-node child output. Failures are carried by process failure, never this DTO.
use crate::inspection::{
    NodeIdentity, NodeStatus, required_nullable, validate_path, validate_version,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateAvailability {
    NotInstalled,
    Current,
    NewerAvailable,
    ImmutableConflict,
    DowngradeRefused,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    NotInstalled,
    Installed,
    InstalledStopped,
    Partial,
    Conflict,
    Unavailable,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeUpdate {
    pub bundled_version: String,
    #[serde(deserialize_with = "required_nullable")]
    pub installed_version: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub running_version: Option<String>,
    pub availability: UpdateAvailability,
    pub recovery_required: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedNodeStatus {
    pub state: ServiceState,
    #[serde(deserialize_with = "required_nullable")]
    pub device_id: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub data_dir: Option<PathBuf>,
    #[serde(deserialize_with = "required_nullable")]
    pub service_id: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub managed_root: Option<PathBuf>,
    #[serde(deserialize_with = "required_nullable")]
    pub service_running: Option<bool>,
    #[serde(deserialize_with = "required_nullable")]
    pub core_status: Option<NodeStatus>,
    pub runtime_update: RuntimeUpdate,
}
impl ManagedNodeStatus {
    pub fn identity(&self) -> Option<NodeIdentity> {
        Some(NodeIdentity {
            data_dir: self.data_dir.clone()?,
            device_id: self.device_id.clone()?,
        })
    }
    pub fn manageable(&self) -> bool {
        matches!(
            self.state,
            ServiceState::Installed | ServiceState::InstalledStopped | ServiceState::Partial
        )
    }
    pub fn validate(&self) -> Result<()> {
        validate(self)
    }
}
pub fn validate(value: &ManagedNodeStatus) -> Result<()> {
    let update = &value.runtime_update;
    for version in std::iter::once(Some(&update.bundled_version))
        .chain([
            update.installed_version.as_ref(),
            update.running_version.as_ref(),
        ])
        .flatten()
    {
        validate_version(version)?;
    }
    if value.state == ServiceState::NotInstalled {
        ensure!(
            value.device_id.is_none()
                && value.data_dir.is_none()
                && value.service_id.is_none()
                && value.managed_root.is_none()
                && value.service_running.is_none()
                && value.core_status.is_none()
                && update.installed_version.is_none()
                && update.running_version.is_none()
                && update.availability == UpdateAvailability::NotInstalled
                && !update.recovery_required,
            "inconsistent not-installed result"
        );
        return Ok(());
    }
    value
        .identity()
        .context("managed Node identity missing")?
        .validate()?;
    validate_path(
        value
            .managed_root
            .as_deref()
            .context("managed root missing")?,
    )?;
    let id = value.service_id.as_deref().context("service ID missing")?;
    ensure!(
        !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control),
        "invalid service ID"
    );
    ensure!(
        update.installed_version.is_some()
            && update.availability != UpdateAvailability::NotInstalled,
        "installed runtime version missing"
    );
    match value.state {
        ServiceState::Installed | ServiceState::InstalledStopped | ServiceState::Partial => {
            ensure!(
                value.service_running.is_some() && value.core_status.is_some(),
                "service or Core observation missing"
            );
            if value.state == ServiceState::Installed {
                ensure!(
                    value.service_running == Some(true),
                    "installed service is not running"
                );
            }
            if value.state == ServiceState::InstalledStopped {
                ensure!(
                    value.service_running == Some(false),
                    "stopped service is running"
                );
            }
        }
        ServiceState::Conflict => ensure!(
            value.service_running.is_some() == value.core_status.is_some(),
            "incomplete conflicting observations"
        ),
        ServiceState::Unavailable => ensure!(
            value.service_running.is_none() && value.core_status.is_none(),
            "unavailable result must preserve unknown observations"
        ),
        ServiceState::NotInstalled => unreachable!(),
    }
    if update.availability == UpdateAvailability::Current {
        ensure!(
            update.installed_version.as_deref() == Some(update.bundled_version.as_str())
                && update
                    .running_version
                    .as_ref()
                    .is_none_or(|running| Some(running) == update.installed_version.as_ref()),
            "current runtime does not match the bundled/installed version"
        );
    }
    let running =
        value.service_running == Some(true) && value.core_status == Some(NodeStatus::Running);
    ensure!(
        update.running_version.is_some() == running,
        "inconsistent running runtime version"
    );
    Ok(())
}
