use super::Runtime;
use super::codex_server::Link;
use crate::config::{Config, ManagerSettings};
use crate::discovery::{self, Paths, Target};
use crate::manager::{ManagerConfig, Sdk};
use crate::providers::{self, AppServer, AppServerMode, Provider, ProviderRoots, Role};
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

fn provider_setup(
    provider: Provider,
    local: &providers::Local,
    roots: &ProviderRoots,
) -> Option<Value> {
    let directory = match provider {
        Provider::Codex if local.launches(provider) => &roots.codex_home,
        Provider::Claude => &roots.claude_config,
        Provider::Opencode => &roots.opencode,
        Provider::Codex | Provider::Cursor | Provider::Pi => return None,
    };
    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
    let mut argv = vec!["env".to_owned()];
    if provider == Provider::Opencode {
        for name in [
            "OPENCODE_CONFIG",
            "OPENCODE_CONFIG_CONTENT",
            "OPENCODE_CLI_CONFIG_CONTENT",
            "OPENCODE_TEST_HOME",
        ] {
            argv.extend(["-u".into(), name.into()]);
        }
    }
    argv.extend(
        roots
            .env(provider)
            .into_iter()
            .map(|(name, value)| quote(&format!("{name}={value}"))),
    );
    argv.push(quote(&local.bin(provider).to_string_lossy()));
    argv.extend(match provider {
        Provider::Codex => vec!["login".into()],
        Provider::Claude => vec!["auth".into(), "login".into()],
        Provider::Opencode => vec!["--standalone".into(), "auth".into(), "login".into()],
        Provider::Cursor | Provider::Pi => unreachable!(),
    });
    Some(json!({"directory":directory,"command":argv.join(" ")}))
}

/// One change to a provider; absent fields keep their value. A variable set to a string is set
/// or replaced, set to `null` is removed. Values are never read back.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderChange {
    provider: Provider,
    /// `true` moves the bot to this provider; `false` stops it if it runs here.
    bot: Option<bool>,
    /// Whether this provider executes dispatched work.
    execution: Option<bool>,
    /// The device default model for dispatched work. Empty clears the override.
    execution_model: Option<String>,
    /// The provider's program; empty restores its default.
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
        let provider = self.provider;
        match self.bot {
            Some(true) => {
                ensure!(
                    provider.supports().contains(&Role::Agent),
                    "{} does not run bots here yet",
                    provider.label()
                );
                let mut chosen = target
                    .manager
                    .take()
                    .or(default)
                    .context("no bot to start")?;
                if chosen.provider != provider {
                    chosen.reasoning_effort = None;
                }
                chosen.provider = provider;
                chosen.model = provider.model(&chosen.model);
                *target.manager = Some(chosen);
            }
            Some(false)
                if target
                    .manager
                    .as_ref()
                    .is_some_and(|m| m.provider == provider) =>
            {
                *target.manager = None
            }
            _ => {}
        }
        match self.execution {
            Some(true) => {
                ensure!(
                    provider.supports().contains(&Role::Execution),
                    "{} does not execute dispatched work here yet",
                    provider.label()
                );
                target.local.switches.execution.insert(provider);
            }
            Some(false) => {
                target.local.switches.execution.remove(&provider);
            }
            None => {}
        }
        if let Some(model) = self.execution_model {
            ensure!(
                provider.supports().contains(&Role::Execution),
                "{} does not execute dispatched work here yet",
                provider.label()
            );
            let model = model.trim();
            if model.is_empty() {
                target.local.switches.execution_models.remove(&provider);
            } else {
                let model = provider.model(model);
                crate::model::validate_model(&model)?;
                target
                    .local
                    .switches
                    .execution_models
                    .insert(provider, model);
            }
        }
        if let Some(bin) = self.bin {
            let trimmed = PathBuf::from(bin.to_string_lossy().trim());
            if trimmed.as_os_str().is_empty() {
                target.local.bins.remove(&provider);
            } else {
                providers::validate_bin(provider, &trimmed)?;
                target.local.bins.insert(provider, trimmed);
            }
        }
        if let Some(app_server) = self.app_server {
            ensure!(provider.on_app_server(), "only Codex has an App Server");
            target.local.switches.codex_app_server = app_server;
        }
        target.local.env.apply(provider, self.env)
    }
}

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
    fn default_manager(&self, provider: Provider) -> Result<ManagerSettings> {
        Ok(ManagerSettings {
            provider,
            workspace: self.data_dir.canonicalize()?.join("manager-workspace"),
            model: provider.model(""),
            reasoning_effort: None,
            backup: Default::default(),
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
    pub(crate) fn manager_config(
        &self,
        settings: ManagerSettings,
        local: &providers::Local,
    ) -> Result<ManagerConfig> {
        let sdk = match settings.provider {
            Provider::Cursor => Sdk::Cursor {
                bridge: self.cursor.get().cloned(),
                sdk_bin: local.bin(Provider::Cursor),
                owner: None,
                local_store_root: None,
            },
            Provider::Codex => Sdk::Codex {
                socket: self.codex.socket(),
            },
            Provider::Claude => Sdk::Claude {
                bin: local.bin(Provider::Claude),
                processes: self.claude.processes(),
            },
            Provider::Pi => Sdk::Pi {
                bin: local.bin(Provider::Pi),
                pi: self.pi.clone(),
            },
            Provider::Opencode => Sdk::Opencode {
                server: self.opencode.clone(),
            },
        };
        Ok(ManagerConfig {
            sdk,
            workspace: settings.workspace,
            model: settings.model,
            reasoning_effort: settings.reasoning_effort,
            timeout_secs: self.config.timeout_secs,
            custom_tools: BTreeMap::new(),
            tool_cli: None,
            instructions: String::new(),
            tool_env: self.agent_env(),
            tool_token: None,
            provider_env: local.env.get(settings.provider).clone(),
            process_started: None,
            codex_turn: None,
        })
    }

    /// The variables this Node sets for `provider`, and only that provider.
    pub(crate) fn provider_env(&self, provider: Provider) -> BTreeMap<String, String> {
        self.providers().env.get(provider).clone()
    }

    pub(crate) async fn provider_status(
        &self,
        provider: Provider,
        local: &providers::Local,
        settings: &NodeSettings,
    ) -> Value {
        let configured = local.env.get(provider);
        let roots = ProviderRoots::new(&self.data_dir);
        let runtime_env = roots.env(provider);
        let app_server = &local.switches.codex_app_server;
        let bot = settings
            .manager
            .as_ref()
            .is_some_and(|m| m.provider == provider);
        let execution = local.switches.execution.contains(&provider);
        let roles: Vec<Role> = [(bot, Role::Agent), (execution, Role::Execution)]
            .into_iter()
            .filter_map(|(serves, role)| serves.then_some(role))
            .collect();
        let launches = local.launches(provider);
        let mut issues = Vec::new();
        let bin = local.bin(provider);
        let required = provider.key_variables(configured).unwrap_or_else(|_| {
            issues.push(json!({"code":"config_invalid"}));
            vec![]
        });
        let inherited = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        let names: BTreeSet<&String> = configured.keys().chain(&required).collect();
        let env: Vec<Value> = names
            .into_iter()
            .filter(|name| !runtime_env.contains_key(*name))
            .map(|name| {
                json!({"name":name,"set":configured.contains_key(name),
                    "inherited":inherited(name),"required":required.contains(name)})
            })
            .collect();
        let link = if provider.on_app_server() {
            Some(self.codex.link().await.sanitized())
        } else {
            None
        };
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
                _ => {}
            }
        }
        let probe = self.provider_probe_snapshot(provider).await;
        let model = settings
            .manager
            .as_ref()
            .filter(|manager| manager.provider == provider)
            .map_or("", |manager| manager.model.as_str());
        let readiness = probe.readiness(model);
        let execution_model = local
            .switches
            .execution_models
            .get(&provider)
            .cloned()
            .unwrap_or_default();
        let execution_readiness = probe.readiness(&execution_model);
        let state = if roles.is_empty() {
            "off"
        } else {
            readiness.state()
        };
        let codex = provider.on_app_server();
        let setup = provider_setup(provider, local, &roots);
        json!({
            "provider":provider, "roles":roles, "supports":provider.supports(),
            "installed":discovery::Host::current().installed(&bin), "bin":bin,
            "app_server":codex.then(|| json!({"mode":app_server.mode,
                "socket":app_server.socket,"link":link})),
            "home":codex.then(|| configured.get(discovery::CODEX_HOME)),
            "sessions":codex.then(|| self.triage.config().codex_sessions),
            "env":env, "state":state, "issues":issues, "probe":probe, "readiness":readiness,
            "execution_model":execution_model, "execution_readiness":execution_readiness,
            "setup":setup,
        })
    }

    pub(crate) async fn refresh_provider_status(&self, provider: Provider) -> Value {
        self.probe_provider(provider).await;
        self.provider_status(provider, &self.providers(), &self.settings_snapshot())
            .await
    }

    pub(crate) fn settings_snapshot(&self) -> NodeSettings {
        self.settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) async fn manager_state(&self) -> Option<Value> {
        self.manager_readiness()
            .await
            .map(|readiness| json!(readiness.state()))
    }

    pub(crate) async fn node_settings(&self) -> Result<Value> {
        let settings = self.settings_snapshot();
        let default = self.default_manager(Provider::Codex)?;
        let defaults = json!({"workspace":default.workspace});
        let local = self.providers();
        let mut providers = Vec::new();
        for provider in Provider::ALL {
            providers.push(self.provider_status(provider, &local, &settings).await);
        }
        let readiness = settings.manager.as_ref().and_then(|manager| {
            providers
                .iter()
                .find(|status| status["provider"] == manager.provider.name())
                .map(|status| status["readiness"].clone())
        });
        let suggested: Vec<PathBuf> = self
            .host
            .project_folders()
            .into_iter()
            .filter(|folder| !settings.roots.contains(folder))
            .collect();
        let mut response = json!({"settings":settings,"defaults":defaults,
            "readiness":{"manager":readiness},"providers":providers,
            "suggested_roots":suggested});
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

    /// Chooses or stops the bot on a provider, turns its execution on or off, sets the
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
        let mut change = change;
        // Tasks reach a device only through its bot (see `authorize_dispatch`), so a provider set
        // to execute them on a device with no bot also runs the bot there.
        if change.execution == Some(true) && change.bot.is_none() && self.manager().is_none() {
            change.bot = Some(true);
        }
        let default = match change.bot {
            Some(true) if self.manager().is_none() => Some(self.default_manager(change.provider)?),
            _ => None,
        };
        let change = change.expand(&self.host);
        let provider = change.provider;
        self.write_providers(|target| change.apply(target, default))
            .await?;
        drop(_work);
        drop(_save);
        self.apply_provider_launches().await?;
        self.probe_provider(provider).await;
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
        self.enforce_provider_roots(&mut next)?;
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
            .is_some_and(|manager| manager.provider.on_app_server());
        next.validate(codex_agent)?;
        let manager_changed = manager != previous_manager;
        if manager_changed
            || next.switches != previous.switches
            || next.bins != previous.bins
            || triage != previous_triage
        {
            let mut configuration = Config::load(&self.data_dir)?;
            configuration.manager = manager.clone();
            configuration.providers = next.switches.clone();
            configuration.bins = next.bins.clone();
            configuration.triage = triage.clone();
            configuration.save(&self.data_dir)?;
        }
        if next.env != previous.env {
            next.env.save(&self.data_dir)?;
        }
        if !filled.is_empty() {
            tracing::info!(event = "providers.discovered", filled = ?filled.names(), "filled missing provider paths from this computer");
        }
        let _active = self.manager_controls.lock().await;
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
        if manager_changed
            || next.switches != previous.switches
            || next.bins != previous.bins
            || next.env != previous.env
        {
            self.pending_provider_launches
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        drop(_active);
        self.schedule_wakeup.notify_one();
        Ok(filled)
    }

    pub(crate) async fn apply_provider_launches(&self) -> Result<bool> {
        use std::sync::atomic::Ordering::SeqCst;
        let _save = self.config_save.lock().await;
        if !self.pending_provider_launches.load(SeqCst) {
            return Ok(true);
        }
        let active = self.manager_controls.lock().await;
        if !active.is_empty() || self.triage.turn.try_lock().is_err() {
            self.pending_provider_launches.store(true, SeqCst);
            return Ok(false);
        }
        let store = self.store.lock().await;
        let busy_work = match store.work_records() {
            Ok(works) => works.iter().any(|work| {
                matches!(
                    work.kind,
                    crate::model::WorkKind::Agent(Provider::Codex | Provider::Opencode)
                ) && !work.state.finished()
            }),
            Err(error) => {
                self.pending_provider_launches.store(true, SeqCst);
                return Err(error);
            }
        };
        let busy_job = match store.scheduler_jobs_all() {
            Ok(jobs) => jobs.iter().any(|job| {
                matches!(
                    job.state.name(),
                    "claimed" | "running" | "cancelling" | "reconciling" | "steering"
                )
            }),
            Err(error) => {
                self.pending_provider_launches.store(true, SeqCst);
                return Err(error);
            }
        };
        if busy_work || busy_job {
            self.pending_provider_launches.store(true, SeqCst);
            return Ok(false);
        }
        let local = self.providers();
        let codex_agent = self
            .manager()
            .is_some_and(|manager| manager.provider.on_app_server());
        self.codex.retarget(local.codex_target(codex_agent));
        self.opencode.set_launch(
            local.bin(Provider::Opencode),
            local.env.get(Provider::Opencode).clone(),
        );
        self.refresh_cursor();
        self.pending_provider_launches.store(false, SeqCst);
        Ok(true)
    }

    /// Adds folders work may run in; the caller holds `config_save`.
    fn add_roots(&self, roots: &[PathBuf]) -> Result<()> {
        let mut next = self.settings_snapshot().roots;
        next.extend(roots.iter().cloned());
        next.sort();
        next.dedup();
        ensure!(next.len() <= 128, "at most 128 project roots are allowed");
        let mut configuration = Config::load(&self.data_dir)?;
        configuration.roots = next.clone();
        configuration.save(&self.data_dir)?;
        self.settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .roots = next;
        self.schedule_wakeup.notify_one();
        Ok(())
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
            manager
                .provider
                .validate_reasoning_effort(manager.reasoning_effort.as_deref())?;
            manager.backup.validate()?;
            manager.workspace = self.host.expand(&manager.workspace);
            ensure!(
                manager.workspace.is_absolute(),
                "manager workspace must be an existing absolute directory"
            );
            ensure!(
                manager.model.len() <= 256 && !manager.model.chars().any(char::is_control),
                "invalid manager model"
            );
            ensure!(
                !manager.provider.needs_model() || !manager.model.trim().is_empty(),
                "{} requires a model",
                manager.provider.label()
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
        let _active = self.manager_controls.lock().await;
        *self
            .settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = settings;
        if manager_changed {
            self.pending_provider_launches
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        drop(_active);
        drop(_work);
        drop(_save);
        if manager_changed {
            self.apply_provider_launches().await?;
            if let Some(manager) = self.manager() {
                self.probe_provider(manager.provider).await;
            }
        }
        self.schedule_wakeup.notify_one();
        self.node_settings().await
    }
}

/// The answer that lets a proposal be written; any other answer writes nothing.
pub(crate) const ALLOW: &str = "允许";
const DENY: &str = "不允许";
const MAX_PROPOSALS: usize = 64;

/// Provider paths a bot asked the user to allow on this device, and what came of it.
pub(crate) struct Proposal {
    conversation: String,
    paths: Paths,
    /// Folders to add to the ones work may run in.
    roots: Vec<PathBuf>,
    outcome: Option<Value>,
}

/// What a bot proposes: paths it found itself, each optional.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalInput {
    #[serde(default)]
    execution: BTreeSet<Provider>,
    codex_home: Option<PathBuf>,
    app_server_socket: Option<PathBuf>,
    codex_sessions: Option<PathBuf>,
    #[serde(default)]
    bins: BTreeMap<Provider, PathBuf>,
    #[serde(default)]
    roots: Vec<PathBuf>,
}

impl ProposalInput {
    /// Existing folders, as `configure_node` stores them.
    fn roots(&mut self) -> Result<Vec<PathBuf>> {
        ensure!(self.roots.len() <= 16, "propose at most 16 roots at once");
        let mut roots = std::mem::take(&mut self.roots)
            .into_iter()
            .map(|root| {
                ensure!(
                    root.is_absolute() && root.as_os_str().len() <= 1024 && root.is_dir(),
                    "roots must be absolute folders that exist on this device"
                );
                Ok(root.canonicalize()?)
            })
            .collect::<Result<Vec<_>>>()?;
        roots.sort();
        roots.dedup();
        Ok(roots)
    }

    fn paths(self) -> Result<Paths> {
        ensure!(
            self.codex_home.is_none(),
            "the Codex runtime directory is owned by this Node"
        );
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
        for provider in &self.execution {
            ensure!(
                provider.supports().contains(&Role::Execution),
                "{} does not execute dispatched work here yet",
                provider.label()
            );
        }
        Ok(Paths {
            execution: self.execution,
            app_server: existing(self.app_server_socket, "app_server_socket", |path| {
                path.symlink_metadata().is_ok()
            })?
            .map(|socket| AppServer {
                mode: AppServerMode::System,
                socket: Some(socket),
            }),
            codex_home: existing(self.codex_home, "codex_home", Path::is_dir)?,
            bins: self
                .bins
                .into_iter()
                .map(|(provider, bin)| {
                    ensure!(
                        bin.is_absolute()
                            && bin.as_os_str().len() <= 1024
                            && discovery::runnable(&bin, None),
                        "bins.{} must be an absolute path that exists on this device",
                        provider.name()
                    );
                    Ok((provider, bin))
                })
                .collect::<Result<_>>()?,
            codex_sessions: existing(self.codex_sessions, "codex_sessions", Path::is_dir)?,
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
        let mut input =
            serde_json::from_value::<ProposalInput>(input).context("invalid provider proposal")?;
        let current = self.settings_snapshot().roots;
        let asked_roots = !input.roots.is_empty();
        let roots: Vec<PathBuf> = input
            .roots()?
            .into_iter()
            .filter(|root| !current.contains(root))
            .collect();
        let given = input.paths()?;
        let codex = (given.is_empty() && !asked_roots)
            || given
                .execution
                .iter()
                .any(|provider| provider.on_app_server())
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
        if paths.is_empty() && roots.is_empty() {
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
                    &serde_json::to_string(&(&paths, &roots))?
                ]
                .join("\0")
                .as_bytes()
            )[..32]
        );
        let body = crate::model::EventBody::QuestionAsked {
            question_id: question_id.clone(),
            attention: crate::model::AttentionRequest {
                kind: crate::model::AttentionKind::Approval,
                device_id: Some(device.clone()),
            },
            severity: crate::model::Severity::Attention,
            text: self.proposal_text(&paths, &roots),
            choices: [ALLOW, DENY]
                .map(|label| crate::model::Choice {
                    label: label.into(),
                })
                .into(),
            about: crate::model::Subject {
                node: device,
                agent: None,
                session_id: format!("provider-setup-{}", &question_id[2..18]),
                cwd: String::new(),
                summary: "Bot 与任务".into(),
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
            tracing::info!(event = "question.asked", conversation_id = scope, question_id, filled = ?paths.names(), roots = roots.len(), "provider paths proposed");
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
                roots: roots.clone(),
                outcome: None,
            });
        let shown: Vec<String> = roots.iter().map(|root| self.host.display(root)).collect();
        Ok(json!({"status":"asked","question_id":question_id,"proposed":paths,"roots":shown}))
    }

    /// One line per setting the card asks about. Paths are shortened so the full ones stay on
    /// this computer; the card is a shared record.
    fn proposal_text(&self, paths: &Paths, roots: &[PathBuf]) -> String {
        let shown = |path: &PathBuf| self.host.display(path);
        let only_folder = paths.is_empty() && roots.len() == 1;
        let mut lines = vec![if !paths.execution.is_empty() {
            let names: Vec<&str> = paths
                .execution
                .iter()
                .map(|provider| provider.label())
                .collect();
            format!("要让这台电脑用 {} 执行任务吗？", names.join("、"))
        } else if only_folder {
            format!("要让 Bot 在 {} 里工作吗？", shown(&roots[0]))
        } else if paths.is_empty() {
            "要让 Bot 在这些文件夹里工作吗？".to_owned()
        } else {
            "要改这台电脑的这些 AI 设置吗？".to_owned()
        }];
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
            ("会话目录", &paths.codex_sessions),
        ];
        for (label, path) in named {
            if let Some(path) = path {
                lines.push(format!("{label}：{}", shown(path)));
            }
        }
        for (provider, bin) in &paths.bins {
            lines.push(format!("{} 程序：{}", provider.label(), shown(bin)));
        }
        for root in roots.iter().filter(|_| !only_folder) {
            lines.push(format!("可用文件夹：{}", shown(root)));
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
            let filled = if paths.is_empty() {
                Paths::default()
            } else {
                self.write_providers(|target| paths.apply(target)).await?
            };
            if !proposal.roots.is_empty() {
                self.add_roots(&proposal.roots)?;
            }
            tracing::info!(event = "providers.applied", question_id, applied = ?paths.names(), roots = proposal.roots.len(), "provider paths the user allowed were written");
            json!({"status":"applied","question_id":question_id,"applied":paths.names(),"roots_added":proposal.roots.len(),"also_filled":filled.names()})
        } else {
            json!({"status":"declined","question_id":question_id,"answer":answer})
        };
        proposal.outcome = Some(outcome.clone());
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn private_setup_commands_run_with_the_same_roots_and_preserve_auth_environment() -> Result<()>
    {
        let dir = std::env::temp_dir().join(format!("vibeface-setup-'{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let bin = dir.join("fake provider");
        std::fs::write(
            &bin,
            r#"#!/usr/bin/env python3
import json, os, pathlib, sys
names = ['CODEX_HOME', 'CLAUDE_CONFIG_DIR', 'XDG_DATA_HOME', 'XDG_CONFIG_HOME', 'XDG_STATE_HOME', 'XDG_CACHE_HOME', 'OPENCODE_DB', 'OPENCODE_CONFIG_DIR', 'OPENCODE_CONFIG', 'OPENCODE_CONFIG_CONTENT', 'OPENCODE_CLI_CONFIG_CONTENT', 'OPENCODE_TEST_HOME']
pathlib.Path(__file__).with_name('observed.json').write_text(json.dumps({'args': sys.argv[1:], 'env': {name: os.environ.get(name) for name in names}, 'auth': os.environ.get('FAKE_LOGIN_AUTH')}))
"#,
        )?;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700))?;
        let local = providers::Local {
            switches: providers::Providers::default(),
            bins: Provider::ALL
                .into_iter()
                .map(|provider| (provider, bin.clone()))
                .collect(),
            env: providers::ProviderEnv::default(),
        };
        let roots = ProviderRoots::new(&dir);
        for (provider, args) in [
            (Provider::Codex, vec!["login"]),
            (Provider::Claude, vec!["auth", "login"]),
            (Provider::Opencode, vec!["--standalone", "auth", "login"]),
        ] {
            let setup = provider_setup(provider, &local, &roots).context("private setup")?;
            let mut command = std::process::Command::new("/bin/sh");
            command.args(["-c", setup["command"].as_str().context("command")?]);
            command.env("FAKE_LOGIN_AUTH", "fixture-auth");
            for name in [
                "CODEX_HOME",
                "CLAUDE_CONFIG_DIR",
                "OPENCODE_DB",
                "OPENCODE_CONFIG",
                "OPENCODE_CONFIG_CONTENT",
                "OPENCODE_CLI_CONFIG_CONTENT",
                "OPENCODE_CONFIG_DIR",
                "OPENCODE_TEST_HOME",
                "XDG_DATA_HOME",
                "XDG_CONFIG_HOME",
                "XDG_STATE_HOME",
                "XDG_CACHE_HOME",
            ] {
                command.env(name, "/external/user");
            }
            assert!(command.status()?.success());
            let observed: Value =
                serde_json::from_slice(&std::fs::read(dir.join("observed.json"))?)?;
            assert_eq!(observed["args"], json!(args));
            assert_eq!(observed["auth"], "fixture-auth");
            for (name, value) in roots.env(provider) {
                if observed["env"].get(&name).is_some() {
                    assert_eq!(observed["env"][name], value);
                }
            }
            if provider == Provider::Opencode {
                for name in [
                    "OPENCODE_CONFIG",
                    "OPENCODE_CONFIG_CONTENT",
                    "OPENCODE_CLI_CONFIG_CONTENT",
                    "OPENCODE_TEST_HOME",
                ] {
                    assert_eq!(observed["env"][name], Value::Null);
                }
            }
        }
        let mut system = local;
        system.switches.codex_app_server.mode = AppServerMode::System;
        assert!(provider_setup(Provider::Codex, &system, &roots).is_none());
        assert!(provider_setup(Provider::Pi, &system, &roots).is_none());
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn clearing_a_cursor_execution_model_removes_the_override() {
        let mut local = providers::Local {
            switches: Default::default(),
            bins: Default::default(),
            env: Default::default(),
        };
        let mut manager = None;
        let mut triage = Default::default();
        for input in ["custom", "  ", "auto", ""] {
            let change: ProviderChange = serde_json::from_value(json!({
                "provider":"cursor", "execution_model":input,
            }))
            .unwrap();
            change
                .apply(
                    Target {
                        local: &mut local,
                        manager: &mut manager,
                        triage: &mut triage,
                    },
                    None,
                )
                .unwrap();
            assert_eq!(
                local
                    .switches
                    .execution_models
                    .get(&Provider::Cursor)
                    .map(String::as_str),
                (!input.trim().is_empty()).then_some(input)
            );
        }
        assert_eq!(Provider::Cursor.model(""), "auto");
    }
}
