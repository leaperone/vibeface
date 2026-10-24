use super::Runtime;
use super::codex_server::Link;
use crate::config::{Config, ManagerSettings};
use crate::manager::{ManagerConfig, ManagerKind, Sdk};
use crate::providers::{self, AppServer, AppServerMode, Provider, Role};
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
    /// `true` moves the bot to this provider; `false` stops it if it runs here.
    bot: Option<bool>,
    /// Whether Codex executes dispatched work.
    execution: Option<bool>,
    /// The Codex program; empty restores the default.
    bin: Option<PathBuf>,
    /// Which Codex App Server to use.
    app_server: Option<AppServer>,
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

    /// The Manager that runs bots on this Node and its provider, if it is on.
    pub(crate) fn manager(&self) -> Option<ManagerSettings> {
        self.settings_snapshot().manager
    }

    /// What a newly chosen bot starts with; the workspace is created when it is chosen.
    fn default_manager(&self, provider: ManagerKind) -> Result<ManagerSettings> {
        Ok(ManagerSettings {
            provider,
            workspace: self.data_dir.canonicalize()?.join("manager-workspace"),
            model: match provider {
                ManagerKind::CursorSdk => "auto".into(),
                ManagerKind::CodexSdk => String::new(),
            },
            cursor_sdk_bin: DEFAULT_CURSOR_BIN.into(),
        })
    }

    /// Creates the default bot workspace when it is missing; it must be a real directory.
    fn ensure_default_workspace(&self, workspace: &Path) -> Result<()> {
        if workspace != self.data_dir.canonicalize()?.join("manager-workspace") {
            return Ok(());
        }
        match std::fs::symlink_metadata(workspace) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "default manager workspace must be a real directory"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                crate::config::private_dir(workspace)?
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    /// A turn of the bot `settings` describe, in its provider's SDK on this Node. Its tools see
    /// the Node-wide shell environment; the caller adds what belongs to the turn.
    pub(crate) fn manager_config(&self, settings: ManagerSettings) -> ManagerConfig {
        let sdk = match settings.provider {
            ManagerKind::CursorSdk => Sdk::Cursor {
                bridge: self.cursor.get().cloned(),
                sdk_bin: settings.cursor_sdk_bin,
                agents: self.cursor_agents.clone(),
            },
            ManagerKind::CodexSdk => Sdk::Codex {
                socket: self.codex.socket(),
            },
        };
        ManagerConfig {
            sdk,
            workspace: settings.workspace,
            model: settings.model,
            timeout_secs: self.config.timeout_secs,
            custom_tools: BTreeMap::new(),
            tool_cli: None,
            instructions: String::new(),
            tool_env: self.agent_env(),
            tool_token: None,
            provider_env: self.provider_env(settings.provider),
            process_started: None,
            codex_turn: None,
        }
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
        let configured = local.env.get(provider);
        let app_server = &local.switches.codex_app_server;
        let bot = settings
            .manager
            .as_ref()
            .is_some_and(|m| Provider::of(m.provider) == provider);
        let execution = provider == Provider::Codex && local.switches.execution;
        let roles: Vec<Role> = [(bot, Role::Agent), (execution, Role::Execution)]
            .into_iter()
            .filter_map(|(serves, role)| serves.then_some(role))
            .collect();
        // Codex bots run on the App Server; the system one runs with its own program and
        // variables.
        let launches = provider == Provider::Cursor || app_server.mode == AppServerMode::Spawn;
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
        let link = (provider == Provider::Codex).then_some(self.codex.link().await);
        // A provider nothing runs on has nothing to set up; its checks start once it is used.
        if !roles.is_empty() {
            if launches && !executable(&bin) {
                issues.push(json!({"code":"bin_missing","path":bin}));
            }
            for name in required.iter().filter(|_| launches) {
                if !configured.contains_key(name) && !inherited(name) {
                    issues.push(json!({"code":"env_missing","name":name}));
                }
            }
            match &link {
                Some(Link::LaunchFailed { reason }) if issues.is_empty() => {
                    issues.push(json!({"code":"launch_failed","detail":reason}))
                }
                Some(Link::NotFound { .. }) => issues.push(json!({"code":"app_server_not_found"})),
                Some(Link::HandshakeFailed { reason, .. }) => {
                    issues.push(json!({"code":"app_server_unreachable","detail":reason}))
                }
                Some(Link::Starting) => starting = true,
                _ => {}
            }
        }
        let state = if roles.is_empty() {
            "off"
        } else if !issues.is_empty() {
            "needs_setup"
        } else if starting {
            "starting"
        } else {
            "ready"
        };
        json!({
            "provider":provider, "roles":roles, "supports":provider.supports(),
            "bin":(provider == Provider::Codex).then_some(&local.codex_bin),
            "app_server":(provider == Provider::Codex).then(|| json!({"mode":app_server.mode,
                "socket":app_server.socket,"link":link})),
            "env":env, "state":state, "issues":issues,
        })
    }

    pub(crate) fn settings_snapshot(&self) -> NodeSettings {
        self.settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether the bot's provider works now; the bot switch alone does not make it run.
    pub(crate) async fn manager_ready(&self) -> bool {
        let settings = self.settings_snapshot();
        let Some(manager) = &settings.manager else {
            return false;
        };
        let status = self
            .provider_status(Provider::of(manager.provider), &self.providers(), &settings)
            .await;
        status["state"] == "ready"
    }

    pub(crate) async fn node_settings(&self) -> Result<Value> {
        let settings = self.settings_snapshot();
        let default = self.default_manager(ManagerKind::CodexSdk)?;
        let defaults = json!({
            "workspace":default.workspace, "cursor_sdk_bin":default.cursor_sdk_bin,
        });
        let local = self.providers();
        let mut missing = Vec::new();
        if let Some(manager) = &settings.manager {
            if manager.provider == ManagerKind::CursorSdk {
                if !executable(&manager.cursor_sdk_bin) {
                    missing.push("Cursor SDK Bridge");
                }
            } else if local.switches.codex_app_server.mode == AppServerMode::Spawn
                && !executable(&local.codex_bin)
            {
                missing.push("Codex CLI");
            }
        }
        let readiness = if settings.manager.is_none() {
            "disabled"
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

    /// Chooses or stops the bot on a provider, turns Codex execution on or off, sets the
    /// provider's program and variables, then restarts what runs on it. This is the only place
    /// that decides what each provider is used for. The variables are written only to this
    /// Node's private file.
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
        let previous_manager = self.settings_snapshot().manager;
        let mut manager = previous_manager.clone();
        let kind = change.provider.manager_kind();
        match change.bot {
            Some(true) => {
                let mut chosen = match manager {
                    Some(current) => current,
                    None => self.default_manager(kind)?,
                };
                chosen.provider = kind;
                // Cursor needs a model; Codex reads "auto" as a model name, so it gets its default.
                match kind {
                    ManagerKind::CursorSdk if chosen.model.trim().is_empty() => {
                        chosen.model = "auto".into()
                    }
                    ManagerKind::CodexSdk if chosen.model == "auto" => chosen.model.clear(),
                    _ => {}
                }
                self.ensure_default_workspace(&chosen.workspace)?;
                manager = Some(chosen);
            }
            Some(false) if manager.as_ref().is_some_and(|m| m.provider == kind) => manager = None,
            _ => {}
        }
        if let Some(execution) = change.execution {
            ensure!(
                change.provider == Provider::Codex,
                "only Codex executes dispatched work"
            );
            next.switches.execution = execution;
        }
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
        if let Some(app_server) = change.app_server {
            ensure!(
                change.provider == Provider::Codex,
                "only Codex has an App Server"
            );
            next.switches.codex_app_server = app_server;
            next.switches.validate()?;
        }
        next.env.apply(change.provider, change.env)?;
        let manager_changed = manager != previous_manager;
        if manager_changed
            || next.switches != previous.switches
            || next.codex_bin != previous.codex_bin
        {
            let mut configuration = Config::load(&self.data_dir)?;
            configuration.manager = manager.clone();
            configuration.providers = next.switches.clone();
            configuration.codex_bin = next.codex_bin.clone();
            configuration.save(&self.data_dir)?;
        }
        if next.env != previous.env {
            next.env.save(&self.data_dir)?;
        }
        self.settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .manager = manager;
        *self
            .providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next.clone();
        self.codex.retarget(next.codex_target());
        if manager_changed || next.env.get(Provider::Cursor) != previous.env.get(Provider::Cursor) {
            self.refresh_cursor();
        }
        self.schedule_wakeup.notify_one();
        self.node_settings().await
    }

    /// Saves the folders and the current bot's details. Whether the bot runs, and on
    /// which provider, is chosen only through `configure_provider`.
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
        let current = self.settings_snapshot().manager.map(|m| m.provider);
        ensure!(
            settings.manager.as_ref().map(|m| m.provider) == current,
            "choose the bot with configure_provider"
        );
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
            ensure!(
                !manager.cursor_sdk_bin.as_os_str().is_empty(),
                "manager program paths must not be empty"
            );
            self.ensure_default_workspace(&manager.workspace)?;
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
