use super::Runtime;
use super::codex_server::Launch;
use crate::config::{Config, ManagerSettings};
use crate::manager::ManagerKind;
use crate::providers::{self, Provider, Role};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSettings {
    pub roots: Vec<PathBuf>,
    pub manager: Option<ManagerSettings>,
}

/// One change to a provider; absent fields keep their value. A variable set to a string is set
/// or replaced, set to `null` is removed. Values are never read back.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderChange {
    provider: Provider,
    enabled: Option<bool>,
    roles: Option<BTreeSet<Role>>,
    /// The Codex program; empty restores the default.
    bin: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, Option<String>>,
}

const DEFAULT_CODEX_BIN: &str = "codex";
const DEFAULT_CURSOR_BIN: &str = "cursor-sdk-bridge";

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
    pub(crate) fn providers(&self) -> providers::Local {
        self.providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The assistant, while its provider is on and serves as the Agent.
    pub(crate) fn assistant(&self) -> Option<ManagerSettings> {
        let providers = self.providers();
        self.settings_snapshot()
            .manager
            .filter(|manager| providers.serves(Provider::of(manager.provider), Role::Agent))
    }

    /// The variables this Node sets for the provider behind `kind`, and only that provider.
    pub(crate) fn provider_env(&self, kind: ManagerKind) -> BTreeMap<String, String> {
        self.providers().env.get(Provider::of(kind)).clone()
    }

    async fn provider_status(
        &self,
        provider: Provider,
        local: &providers::Local,
        settings: &NodeSettings,
    ) -> Value {
        let switch = local.switches.get(provider);
        let configured = local.env.get(provider);
        let mut issues = Vec::new();
        let (bin, required) = match provider {
            Provider::Codex => {
                let required = crate::manager::provider_environment_keys(configured)
                    .unwrap_or_else(|_| {
                        issues.push(json!({"code":"config_invalid"}));
                        vec![]
                    });
                (local.codex_bin.clone(), required)
            }
            Provider::Cursor => (
                settings
                    .manager
                    .as_ref()
                    .map_or(DEFAULT_CURSOR_BIN.into(), |m| m.cursor_sdk_bin.clone()),
                vec!["CURSOR_API_KEY".to_owned()],
            ),
        };
        let inherited = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        let names: BTreeSet<&String> = configured.keys().chain(&required).collect();
        let env: Vec<Value> = names
            .into_iter()
            .map(|name| {
                json!({"name":name,"set":configured.contains_key(name),
                    "inherited":inherited(name),"required":required.contains(name)})
            })
            .collect();
        let mut starting = false;
        if switch.enabled {
            if !executable(&bin) {
                issues.push(json!({"code":"bin_missing","path":bin}));
            }
            for name in &required {
                if !configured.contains_key(name) && !inherited(name) {
                    issues.push(json!({"code":"env_missing","name":name}));
                }
            }
            if provider == Provider::Codex && switch.roles.contains(&Role::Execution) {
                match self.codex.launch_state().await {
                    Some(Launch::Failed(detail)) if issues.is_empty() => {
                        issues.push(json!({"code":"launch_failed","detail":detail}))
                    }
                    Some(Launch::Starting) => starting = true,
                    _ => {}
                }
            }
        }
        let state = if !switch.enabled {
            "off"
        } else if !issues.is_empty() {
            "needs_setup"
        } else if starting {
            "starting"
        } else {
            "ready"
        };
        json!({
            "provider":provider, "enabled":switch.enabled, "roles":switch.roles,
            "supports":provider.supports(),
            "bin":(provider == Provider::Codex).then_some(&local.codex_bin),
            "assistant":settings.manager.as_ref().is_some_and(|m| Provider::of(m.provider) == provider),
            "env":env, "state":state, "issues":issues,
        })
    }

    pub(crate) fn settings_snapshot(&self) -> NodeSettings {
        self.settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) async fn node_settings(&self) -> Result<Value> {
        let settings = self.settings_snapshot();
        let defaults = json!({
            "workspace":self.data_dir.canonicalize()?.join("manager-workspace"),
            "cursor_sdk_bin":"cursor-sdk-bridge",
            "node_bin":"node",
            "codex_sdk_script":std::env::current_exe()?.parent().context("Node executable has no directory")?.join("sidecars/codex-manager.mjs"),
        });
        let local = self.providers();
        let mut missing = Vec::new();
        if let Some(manager) = &settings.manager {
            if manager.provider == ManagerKind::CursorSdk {
                if !executable(&manager.cursor_sdk_bin) {
                    missing.push("Cursor SDK Bridge");
                }
            } else {
                if !executable(&manager.node_bin) {
                    missing.push("Node.js");
                }
                if !executable(&local.codex_bin) {
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
            }
        }
        let readiness = if settings.manager.is_none() {
            "disabled"
        } else if self.assistant().is_none() {
            "provider_off"
        } else if missing.is_empty() {
            "authentication_unverified"
        } else {
            "missing_prerequisites"
        };
        let mut providers = Vec::new();
        for provider in Provider::ALL {
            providers.push(self.provider_status(provider, &local, &settings).await);
        }
        Ok(
            json!({"settings":settings,"defaults":defaults,"readiness":{"manager":readiness,"missing":missing},
                "providers":providers}),
        )
    }

    /// Only the owner changes this Node's settings: a management device, or a browser on this
    /// machine through its local session.
    async fn authorize_settings_write(
        &self,
        subject: &str,
        local_cookie: Option<&str>,
    ) -> Result<()> {
        let view = self.membership_state().await?.project()?;
        if let Some(cookie) = local_cookie {
            let device_id = self.identity.device_id()?;
            view.node(&device_id)?;
            let session = self
                .local_access
                .session(cookie, &device_id, &view.network_id)?;
            ensure!(
                session.device_id == subject,
                "local session identity changed"
            );
        } else {
            view.manager(subject)?;
        }
        Ok(())
    }

    /// Turns a provider on or off, sets its roles, program and variables, then restarts what
    /// runs on it. The variables are written only to this Node's private file.
    pub(crate) async fn configure_provider(
        &self,
        change: ProviderChange,
        subject: &str,
        local_cookie: Option<&str>,
    ) -> Result<Value> {
        let _save = self.config_save.lock().await;
        let _work = self.maintenance.enter()?;
        self.authorize_settings_write(subject, local_cookie).await?;
        let previous = self.providers();
        let mut next = previous.clone();
        let switch = next.switches.get_mut(change.provider);
        if let Some(enabled) = change.enabled {
            switch.enabled = enabled;
        }
        if let Some(roles) = change.roles {
            switch.roles = roles;
        }
        next.switches.validate()?;
        if let Some(bin) = change.bin {
            ensure!(
                change.provider == Provider::Codex,
                "only Codex has a program path here"
            );
            let text = bin.to_string_lossy();
            ensure!(
                text.len() <= 4096 && !text.contains('\0'),
                "invalid Codex program path"
            );
            next.codex_bin = match text.trim() {
                "" => DEFAULT_CODEX_BIN.into(),
                trimmed => trimmed.into(),
            };
        }
        next.env.apply(change.provider, change.env)?;
        if next.switches != previous.switches || next.codex_bin != previous.codex_bin {
            let mut configuration = Config::load(&self.data_dir)?;
            configuration.providers = next.switches.clone();
            configuration.codex_bin = next.codex_bin.clone();
            configuration.save(&self.data_dir)?;
        }
        if next.env != previous.env {
            next.env.save(&self.data_dir)?;
        }
        *self
            .providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next.clone();
        self.codex.relaunch(next.codex_launch());
        if next.switches.cursor != previous.switches.cursor
            || next.env.get(Provider::Cursor) != previous.env.get(Provider::Cursor)
        {
            self.refresh_cursor();
        }
        self.schedule_wakeup.notify_one();
        self.node_settings().await
    }

    pub(crate) async fn configure_node(
        &self,
        mut settings: NodeSettings,
        subject: &str,
        local_cookie: Option<&str>,
    ) -> Result<Value> {
        let _save = self.config_save.lock().await;
        let _work = self.maintenance.enter()?;
        self.authorize_settings_write(subject, local_cookie).await?;
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
                manager.model.len() <= 256 && !manager.model.chars().any(char::is_control),
                "invalid manager model"
            );
            ensure!(
                manager.provider != ManagerKind::CursorSdk || !manager.model.trim().is_empty(),
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
        let manager_changed = self.settings_snapshot().manager != settings.manager;
        *self
            .settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = settings;
        if manager_changed {
            self.refresh_cursor();
        }
        self.schedule_wakeup.notify_one();
        self.node_settings().await
    }
}
