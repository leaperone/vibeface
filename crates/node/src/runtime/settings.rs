use super::Runtime;
use super::codex_server::Link;
use crate::config::{Config, ManagerSettings};
use crate::discovery::{self, Paths, Target};
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

impl ProviderChange {
    /// Reads a leading `~` in the paths the owner typed as this computer's home folder.
    fn expand(mut self, host: &discovery::Host) -> Self {
        self.bin = self.bin.map(|bin| host.expand(&bin));
        if let Some(socket) = self
            .app_server
            .as_mut()
            .and_then(|server| server.socket.as_mut())
        {
            *socket = host.expand(socket);
        }
        if let Some(Some(home)) = self.env.get_mut(discovery::CODEX_HOME) {
            *home = host.expand(Path::new(home)).to_string_lossy().into_owned();
        }
        self
    }

    /// `default` is the bot a provider starts when none runs yet.
    fn apply(self, target: Target, default: Option<ManagerSettings>) -> Result<()> {
        let kind = self.provider.manager_kind();
        match self.bot {
            Some(true) => {
                let mut chosen = target
                    .manager
                    .take()
                    .or(default)
                    .context("no bot to start")?;
                chosen.provider = kind;
                // Cursor needs a model; Codex reads "auto" as a model name, so it gets its default.
                match kind {
                    ManagerKind::CursorSdk if chosen.model.trim().is_empty() => {
                        chosen.model = "auto".into()
                    }
                    ManagerKind::CodexSdk if chosen.model == "auto" => chosen.model.clear(),
                    _ => {}
                }
                *target.manager = Some(chosen);
            }
            Some(false) if target.manager.as_ref().is_some_and(|m| m.provider == kind) => {
                *target.manager = None
            }
            _ => {}
        }
        if let Some(execution) = self.execution {
            ensure!(
                self.provider == Provider::Codex,
                "only Codex executes dispatched work"
            );
            target.local.switches.execution = execution;
        }
        if let Some(bin) = self.bin {
            ensure!(
                self.provider == Provider::Codex,
                "only Codex has a program path here"
            );
            let text = bin.to_string_lossy();
            ensure!(
                text.len() <= 4096 && !text.contains('\0'),
                "invalid Codex program path"
            );
            target.local.codex_bin = match text.trim() {
                "" => DEFAULT_CODEX_BIN.into(),
                trimmed => trimmed.into(),
            };
        }
        if let Some(app_server) = self.app_server {
            ensure!(
                self.provider == Provider::Codex,
                "only Codex has an App Server"
            );
            target.local.switches.codex_app_server = app_server;
        }
        target.local.env.apply(self.provider, self.env)
    }
}

const DEFAULT_CODEX_BIN: &str = "codex";
const DEFAULT_CURSOR_BIN: &str = "cursor-sdk-bridge";

fn executable(path: &Path) -> bool {
    discovery::runnable(path, std::env::var_os("PATH").as_ref())
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
            "home":(provider == Provider::Codex).then(|| configured.get(discovery::CODEX_HOME)),
            "sessions":(provider == Provider::Codex).then(|| self.triage.config().codex_sessions),
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
        let mut response = json!({"settings":settings,"defaults":defaults,
            "readiness":{"manager":readiness,"missing":missing},"providers":providers});
        response["display_paths"] = json!(self.host.display_paths(&response));
        Ok(response)
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
        let kind = change.provider.manager_kind();
        let default = match change.bot {
            Some(true) if self.manager().is_none() => Some(self.default_manager(kind)?),
            _ => None,
        };
        let change = change.expand(&self.host);
        self.write_providers(|target| change.apply(target, default))
            .await?;
        self.node_settings().await
    }

    /// The one write of this device's provider settings. `edit` changes them; then what a use
    /// that is on still misses is found on this computer and written with them, the
    /// configuration and variables are saved, and what runs on the providers follows. The
    /// caller holds `config_save`. Returns what discovery filled in.
    pub(crate) async fn write_providers(
        &self,
        edit: impl FnOnce(Target) -> Result<()>,
    ) -> Result<Paths> {
        let previous = self.providers();
        let mut next = previous.clone();
        let previous_manager = self.settings_snapshot().manager;
        let mut manager = previous_manager.clone();
        let previous_triage = self.triage.config();
        let mut triage = previous_triage.clone();
        edit(Target {
            local: &mut next,
            manager: &mut manager,
            triage: &mut triage,
        })?;
        let filled = discovery::missing(
            &Target {
                local: &mut next,
                manager: &mut manager,
                triage: &mut triage,
            },
            false,
            &self.host,
        )?;
        filled.apply(Target {
            local: &mut next,
            manager: &mut manager,
            triage: &mut triage,
        })?;
        if let Some(chosen) = &manager {
            self.ensure_default_workspace(&chosen.workspace)?;
        }
        triage.validate()?;
        let codex_agent = manager
            .as_ref()
            .is_some_and(|manager| manager.provider == ManagerKind::CodexSdk);
        next.validate(codex_agent)?;
        let manager_changed = manager != previous_manager;
        if manager_changed
            || next.switches != previous.switches
            || next.codex_bin != previous.codex_bin
            || triage != previous_triage
        {
            let mut configuration = Config::load(&self.data_dir)?;
            configuration.manager = manager.clone();
            configuration.providers = next.switches.clone();
            configuration.codex_bin = next.codex_bin.clone();
            configuration.triage = triage.clone();
            configuration.save(&self.data_dir)?;
        }
        if next.env != previous.env {
            next.env.save(&self.data_dir)?;
        }
        if !filled.is_empty() {
            tracing::info!(event = "providers.discovered", filled = ?filled.names(), "filled missing provider paths from this computer");
        }
        self.settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .manager = manager;
        *self
            .providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next.clone();
        if triage != previous_triage {
            self.triage.set_config(triage);
        }
        self.codex.retarget(next.codex_target(codex_agent));
        if manager_changed || next.env.get(Provider::Cursor) != previous.env.get(Provider::Cursor) {
            self.refresh_cursor();
        }
        self.schedule_wakeup.notify_one();
        Ok(filled)
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
            *root = self.host.expand(root);
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
            manager.workspace = self.host.expand(&manager.workspace);
            manager.cursor_sdk_bin = self.host.expand(&manager.cursor_sdk_bin);
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

/// The agent a settings card is about: the user's answer goes to `apply_provider_config`, not
/// to a session.
pub(crate) const SETTINGS_AGENT: &str = "vibeface";
/// The answer that lets a proposal be written; any other answer writes nothing.
pub(crate) const ALLOW: &str = "允许";
const DENY: &str = "不允许";
const MAX_PROPOSALS: usize = 64;

/// Provider paths a bot asked the user to allow on this device, and what came of it.
pub(crate) struct Proposal {
    conversation: String,
    paths: Paths,
    outcome: Option<Value>,
}

/// What a bot proposes: paths it found itself, each optional.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalInput {
    #[serde(default)]
    execution: bool,
    codex_home: Option<PathBuf>,
    app_server_socket: Option<PathBuf>,
    codex_sessions: Option<PathBuf>,
    cursor_sdk_bin: Option<PathBuf>,
}

impl ProposalInput {
    fn paths(self) -> Result<Paths> {
        let existing = |path: Option<PathBuf>, what: &str, kind: fn(&Path) -> bool| {
            path.map(|path| {
                ensure!(
                    path.is_absolute() && path.as_os_str().len() <= 1024 && kind(&path),
                    "{what} must be an absolute path that exists on this device"
                );
                Ok(path)
            })
            .transpose()
        };
        Ok(Paths {
            execution: self.execution.then_some(true),
            app_server: existing(self.app_server_socket, "app_server_socket", |path| {
                path.symlink_metadata().is_ok()
            })?
            .map(|socket| AppServer {
                mode: AppServerMode::System,
                socket: Some(socket),
            }),
            codex_home: existing(self.codex_home, "codex_home", Path::is_dir)?,
            codex_bin: None,
            codex_sessions: existing(self.codex_sessions, "codex_sessions", Path::is_dir)?,
            cursor_sdk_bin: existing(self.cursor_sdk_bin, "cursor_sdk_bin", |path| {
                discovery::runnable(path, None)
            })?,
        })
    }
}

impl Runtime {
    /// Resolves what a bot proposes against this device's settings, then asks the user in the
    /// bot's chat. Nothing is written until `apply_provider_config` finds the user allowed it.
    pub(crate) async fn propose_provider_config(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
        input: Value,
    ) -> Result<Value> {
        let given = serde_json::from_value::<ProposalInput>(input)
            .context("invalid provider proposal")?
            .paths()?;
        let codex = given.is_empty()
            || given.execution.is_some()
            || given.app_server.is_some()
            || given.codex_home.is_some()
            || given.codex_sessions.is_some();
        let host = discovery::Host {
            codex_home: given.codex_home.clone().or(self.host.codex_home.clone()),
            ..self.host.clone()
        };
        let (mut local, mut manager, mut triage) = (
            self.providers(),
            self.settings_snapshot().manager,
            self.triage.config(),
        );
        given.apply(Target {
            local: &mut local,
            manager: &mut manager,
            triage: &mut triage,
        })?;
        let found = discovery::missing(
            &Target {
                local: &mut local,
                manager: &mut manager,
                triage: &mut triage,
            },
            codex,
            &host,
        )?;
        let (mut local, mut manager, mut triage) = (
            self.providers(),
            self.settings_snapshot().manager,
            self.triage.config(),
        );
        let paths = given.or(found).changes(&Target {
            local: &mut local,
            manager: &mut manager,
            triage: &mut triage,
        });
        if paths.is_empty() {
            return Ok(json!({"status":"unchanged"}));
        }
        let device = self.identity.device_id()?;
        let question_id = format!(
            "q-{}",
            &crate::crypto::digest(
                [
                    device.as_str(),
                    scope,
                    trigger,
                    &serde_json::to_string(&paths)?
                ]
                .join("\0")
                .as_bytes()
            )[..32]
        );
        let body = crate::model::EventBody::QuestionAsked {
            question_id: question_id.clone(),
            severity: crate::model::Severity::Attention,
            text: self.proposal_text(&paths),
            choices: [ALLOW, DENY]
                .map(|label| crate::model::Choice {
                    label: label.into(),
                })
                .into(),
            about: crate::model::Subject {
                node: device,
                agent: SETTINGS_AGENT.into(),
                session_id: format!("provider-setup-{}", &question_id[2..18]),
                cwd: String::new(),
                summary: "AI 设置".into(),
                request_id: None,
            },
        };
        let mut proposals = self.provider_proposals.lock().await;
        let (_, written) =
            self.store
                .lock()
                .await
                .ask(&self.identity, network, scope, body, super::now_ms())?;
        if let Some(event) = &written {
            tracing::info!(event = "question.asked", conversation_id = scope, question_id, filled = ?paths.names(), "provider paths proposed");
            self.publish_changed(network, scope, super::Changed::Event(&event.hash));
        }
        if proposals.len() >= MAX_PROPOSALS && !proposals.contains_key(&question_id) {
            proposals.clear();
        }
        proposals
            .entry(question_id.clone())
            .or_insert_with(|| Proposal {
                conversation: scope.into(),
                paths: paths.clone(),
                outcome: None,
            });
        Ok(json!({"status":"asked","question_id":question_id,"proposed":paths}))
    }

    /// One line per setting the card asks about. Paths are shortened so the full ones stay on
    /// this computer; the card is a shared record.
    fn proposal_text(&self, paths: &Paths) -> String {
        let shown = |path: &PathBuf| self.host.display(path);
        let mut lines = vec!["要把这台设备的 AI 设置改成这样吗？".to_owned()];
        if let Some(on) = paths.execution {
            lines.push(
                if on {
                    "用 Codex 执行任务"
                } else {
                    "不再用 Codex 执行任务"
                }
                .into(),
            );
        }
        if let Some(server) = &paths.app_server {
            lines.push(match (&server.mode, &server.socket) {
                (AppServerMode::System, Some(socket)) => {
                    format!("连接已在运行的 Codex：{}", shown(socket))
                }
                _ => "由 Vibeface 启动 Codex".into(),
            });
        }
        let named = [
            ("Codex 目录", &paths.codex_home),
            ("Codex 程序", &paths.codex_bin),
            ("会话目录", &paths.codex_sessions),
            ("Cursor 组件", &paths.cursor_sdk_bin),
        ];
        for (label, path) in named {
            if let Some(path) = path {
                lines.push(format!("{label}：{}", shown(path)));
            }
        }
        let mut text = crate::logging::scrub(&lines.join("\n"));
        while text.len() > 1024 {
            text.pop();
        }
        text
    }

    /// Writes a proposal once the user's answer to its card allows it, through the same path
    /// as Settings. A second call returns the first outcome.
    pub(crate) async fn apply_provider_config(
        &self,
        network: &str,
        scope: &str,
        question_id: &str,
    ) -> Result<Value> {
        let mut proposals = self.provider_proposals.lock().await;
        let proposal = proposals.get_mut(question_id).context(
            "this device holds no proposal with that question_id; after a restart, propose it again",
        )?;
        ensure!(
            proposal.conversation == scope,
            "the proposal was made in another conversation"
        );
        if let Some(outcome) = &proposal.outcome {
            return Ok(outcome.clone());
        }
        let question = self
            .store
            .lock()
            .await
            .question(network, question_id)?
            .context("the proposal's question is missing")?;
        let answer = question["answer"]["text"]
            .as_str()
            .context("the user has not answered yet; apply it in the turn their answer starts")?
            .trim()
            .to_owned();
        let outcome = if answer == ALLOW {
            let _save = self.config_save.lock().await;
            let _work = self.maintenance.enter()?;
            let paths = proposal.paths.clone();
            let filled = self.write_providers(|target| paths.apply(target)).await?;
            tracing::info!(event = "providers.applied", question_id, applied = ?paths.names(), "provider paths the user allowed were written");
            json!({"status":"applied","question_id":question_id,"applied":paths.names(),"also_filled":filled.names()})
        } else {
            json!({"status":"declined","question_id":question_id,"answer":answer})
        };
        proposal.outcome = Some(outcome.clone());
        Ok(outcome)
    }
}
