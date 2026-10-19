use super::Runtime;
use crate::config::{Config, ManagerSettings};
use crate::model::NodeRole;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSettings {
    pub roots: Vec<PathBuf>,
    pub manager: Option<ManagerSettings>,
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let available = |path: &Path| {
        path.metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    };
    if path.components().count() != 1 || path.is_absolute() {
        return available(path);
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| available(&directory.join(path)))
    })
}

impl Runtime {
    pub(crate) fn settings_snapshot(&self) -> NodeSettings {
        self.settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) async fn node_settings(&self) -> Result<Value> {
        let settings = self.settings_snapshot();
        let view = self.membership_state().await?.project()?;
        let peer = self.identity.peer_id()?;
        let scheduler_available = self.config.roles.contains(&NodeRole::Scheduler)
            && view
                .authorize_node(&peer, Some(NodeRole::Scheduler))
                .is_ok();
        let defaults = json!({
            "workspace":self.data_dir.canonicalize()?.join("manager-workspace"),
            "cursor_sdk_bin":"cursor-sdk-bridge",
            "node_bin":"node",
            "codex_sdk_script":std::env::current_exe()?.parent().context("Node executable has no directory")?.join("sidecars/codex-manager.mjs"),
        });
        let mut missing = Vec::new();
        if let Some(manager) = &settings.manager {
            if manager.provider == "cursor_sdk" {
                if !executable(&manager.cursor_sdk_bin) {
                    missing.push("Cursor SDK Bridge");
                }
            } else {
                if !executable(&manager.node_bin) {
                    missing.push("Node.js");
                }
                if !executable(&self.config.codex_bin) {
                    missing.push("Codex CLI");
                }
                if !manager.codex_sdk_script.is_file() {
                    missing.push("Codex SDK sidecar");
                }
                if !manager.codex_sdk_script.parent().is_some_and(|directory| {
                    directory.join("node_modules/@openai/codex-sdk").is_dir()
                }) {
                    missing.push("Codex SDK dependencies");
                }
                if scheduler_available && self.native_socket().is_none() {
                    missing.push("Codex App Server");
                }
            }
        }
        let readiness = if settings.manager.is_none() {
            "disabled"
        } else if !scheduler_available {
            "role_unavailable"
        } else if missing.is_empty() {
            "authentication_unverified"
        } else {
            "missing_prerequisites"
        };
        Ok(
            json!({"settings":settings,"defaults":defaults,"readiness":{"manager":readiness,"missing":missing,"scheduler_available":scheduler_available}}),
        )
    }

    pub(crate) async fn configure_node(
        &self,
        mut settings: NodeSettings,
        subject: &str,
        local_cookie: Option<&str>,
    ) -> Result<Value> {
        let _save = self.config_save.lock().await;
        let _work = self.maintenance.enter()?;
        let view = self.membership_state().await?.project()?;
        if let Some(cookie) = local_cookie {
            let session =
                self.local_access
                    .session(cookie, &self.identity.peer_id()?, &view.network_id)?;
            ensure!(
                session.subject_id == subject,
                "local session identity changed"
            );
        } else {
            ensure!(
                view.principal(subject)?.can_manage,
                "management permission required"
            );
        }
        let directory = &self.data_dir;
        let default_workspace = directory.canonicalize()?.join("manager-workspace");
        ensure!(
            settings.roots.len() <= 128,
            "at most 128 project roots are allowed"
        );
        for root in &mut settings.roots {
            ensure!(
                root.is_absolute(),
                "project root must be an existing absolute directory"
            );
            *root = root.canonicalize().context("project root does not exist")?;
            ensure!(root.is_dir(), "project root must be a directory");
        }
        settings.roots.sort();
        settings.roots.dedup();
        if let Some(manager) = &mut settings.manager {
            ensure!(
                manager.workspace.is_absolute(),
                "manager workspace must be an existing absolute directory"
            );
            ensure!(
                matches!(manager.provider.as_str(), "cursor_sdk" | "codex_sdk"),
                "unknown manager provider"
            );
            ensure!(
                manager.model.len() <= 256 && !manager.model.chars().any(char::is_control),
                "invalid manager model"
            );
            ensure!(
                manager.provider != "cursor_sdk" || !manager.model.trim().is_empty(),
                "Cursor SDK requires a model"
            );
            for path in [
                &manager.cursor_sdk_bin,
                &manager.node_bin,
                &manager.codex_sdk_script,
            ] {
                ensure!(
                    !path.as_os_str().is_empty(),
                    "manager program paths must not be empty"
                );
            }
            if manager.workspace == default_workspace {
                match std::fs::symlink_metadata(&default_workspace) {
                    Ok(metadata) => ensure!(
                        metadata.is_dir() && !metadata.file_type().is_symlink(),
                        "default manager workspace must be a real directory"
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        crate::config::private_dir(&default_workspace)?
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            manager.workspace = manager
                .workspace
                .canonicalize()
                .context("manager workspace does not exist")?;
        }
        let mut configuration = Config::load(directory)?;
        configuration.manager = settings.manager.clone();
        configuration.roots = settings.roots.clone();
        configuration.save(directory)?;
        *self
            .settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = settings;
        if self.settings_snapshot().manager.is_some_and(|manager| {
            manager.provider == "codex_sdk" || self.config.codex_socket.is_some()
        }) && let Err(error) = self.ensure_native_client().await
        {
            tracing::warn!(%error, "native Codex is unavailable after settings change");
        }
        self.schedule_wakeup.notify_one();
        self.node_settings().await
    }
}
