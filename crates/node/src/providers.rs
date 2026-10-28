//! The AI providers this Node runs locally, what a bot on each can do, and the variables each
//! provider's processes get. The bot's provider and which providers execute work live in `config.json`;
//! the variables are model keys, so they stay in one private file of the data directory and
//! never reach events, logs or the web, which only learns which names are set.
use crate::scheduling::TriggerKind;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use vibeface_bridge::agent::CustomToolConfig;

pub const ENV_FILE: &str = "provider-env.json";
const MAX_VARIABLES: usize = 64;
const CURSOR_KEY: &str = "CURSOR_API_KEY";

/// A coding agent product: what a bot runs on, what dispatched work runs on, or whose session
/// the Node watches. Its methods are everything the runtime needs to know about the product;
/// nothing else matches on the provider.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Deserialize,
    Serialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    Codex,
    Cursor,
    Claude,
    Pi,
    Opencode,
}

/// The coordination Agent (the bot) or the execution layer that runs dispatched work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Agent,
    Execution,
}

/// How the Node hears from the provider's sessions it did not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watch {
    /// `vibeface hook <provider>` on these events of a JSON hook file, relative to the home
    /// directory.
    Hooks {
        file: &'static str,
        events: &'static [&'static str],
    },
    /// An extension the provider loads from `file`, relative to the home directory: `source`
    /// with `__VIBEFACE_HOOK__` replaced by the JSON argv of `vibeface hook <provider>`. Its
    /// first line marks the file as the Node's.
    Extension {
        file: &'static str,
        source: &'static str,
    },
}

/// What a bot turn's prompt carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptMode {
    /// The thread keeps its instructions, the tool catalog and what it was shown, so a turn
    /// carries only what is new and a thread lost on the App Server is replaced by a full one.
    Thread,
    /// Every turn carries everything.
    Turn,
}

impl Provider {
    pub const ALL: [Provider; 5] = [
        Provider::Codex,
        Provider::Cursor,
        Provider::Claude,
        Provider::Pi,
        Provider::Opencode,
    ];

    /// How config, events, the CLI and peers name the provider; serde and clap agree.
    pub fn name(self) -> &'static str {
        match self {
            Provider::Codex => "codex",
            Provider::Cursor => "cursor",
            Provider::Claude => "claude",
            Provider::Pi => "pi",
            Provider::Opencode => "opencode",
        }
    }

    /// How the owner reads the provider's name.
    pub fn label(self) -> &'static str {
        match self {
            Provider::Codex => "Codex",
            Provider::Cursor => "Cursor",
            Provider::Claude => "Claude Code",
            Provider::Pi => "pi",
            Provider::Opencode => "opencode",
        }
    }

    /// The program the Node runs for this provider unless the owner or discovery chose one.
    pub fn default_bin(self) -> &'static str {
        match self {
            Provider::Codex => "codex",
            Provider::Cursor => "cursor-sdk-bridge",
            Provider::Claude => "claude",
            Provider::Pi => "pi",
            Provider::Opencode => "opencode",
        }
    }

    /// The variables holding this provider's model keys: it needs them to start, and logs
    /// never show their values. `env` is what this Node sets for the provider; Codex reads
    /// the names from the configuration in its `CODEX_HOME`.
    pub fn key_variables(self, env: &BTreeMap<String, String>) -> Result<Vec<String>> {
        match self {
            Provider::Codex => crate::manager::provider_environment_keys(env),
            Provider::Cursor => Ok(vec![CURSOR_KEY.into()]),
            Provider::Claude | Provider::Pi | Provider::Opencode => Ok(vec![]),
        }
    }

    /// What the runtime can run on each provider: the Cursor bridge runs only bots.
    pub fn supports(self) -> &'static [Role] {
        match self {
            Provider::Codex | Provider::Claude | Provider::Pi | Provider::Opencode => {
                &[Role::Agent, Role::Execution]
            }
            Provider::Cursor => &[Role::Agent],
        }
    }

    /// A Codex bot runs its turns on this Node's App Server, which must be up first.
    pub fn on_app_server(self) -> bool {
        self == Provider::Codex
    }

    /// Whether a queued user message can join the bot's running reply.
    pub fn supports_steer(self) -> bool {
        match self {
            Provider::Codex | Provider::Pi | Provider::Opencode => true,
            Provider::Cursor | Provider::Claude => false,
        }
    }

    /// `vibeface hooks install` sets this up; `None` when the Node does not watch the
    /// provider's own sessions.
    pub fn watch(self) -> Option<Watch> {
        match self {
            Provider::Codex => Some(Watch::Hooks {
                file: ".codex/hooks.json",
                events: &["Stop", "PermissionRequest"],
            }),
            Provider::Claude => Some(Watch::Hooks {
                file: ".claude/settings.json",
                events: &["Stop", "StopFailure", "Notification", "PermissionRequest"],
            }),
            Provider::Pi => Some(Watch::Extension {
                file: ".pi/agent/extensions/vibeface.ts",
                source: include_str!("../extensions/pi.ts"),
            }),
            Provider::Cursor | Provider::Opencode => None,
        }
    }

    pub fn prompt_mode(self) -> PromptMode {
        match self {
            Provider::Codex | Provider::Claude | Provider::Pi | Provider::Opencode => {
                PromptMode::Thread
            }
            Provider::Cursor => PromptMode::Turn,
        }
    }

    /// Name prefixes of the variables the Node sets itself for this provider's processes.
    fn reserved_env(self) -> &'static [&'static str] {
        match self {
            Provider::Cursor => &["CURSOR_SDK_"],
            Provider::Opencode => &["OPENCODE_SERVER_", "OPENCODE_CONFIG_CONTENT"],
            Provider::Codex | Provider::Claude | Provider::Pi => &[],
        }
    }

    /// The tools a turn of `kind` is offered. A thread lists them once for all its turns, so it
    /// gets every tool a user's own turn has.
    pub fn tools_for(self, kind: TriggerKind) -> BTreeMap<String, CustomToolConfig> {
        crate::scheduler::tools(match self.prompt_mode() {
            PromptMode::Thread => TriggerKind::Human,
            PromptMode::Turn => kind,
        })
    }

    /// The model the bot runs with: Cursor needs one, and Codex, Claude Code and pi read "auto" as a
    /// model name, so there an empty one means their default.
    pub fn model(self, model: &str) -> String {
        match self {
            Provider::Cursor if model.trim().is_empty() => "auto".into(),
            Provider::Codex | Provider::Claude | Provider::Pi if model == "auto" => String::new(),
            _ => model.into(),
        }
    }

    pub fn capabilities(self) -> Value {
        let codex = self == Provider::Codex;
        json!({
            "verified": false,
            "streaming": true,
            "cancel": true,
            "steer": self.supports_steer(),
            "resume_local_session": true,
            "text_stream_mode": if codex { "snapshot" } else { "delta_and_snapshot" },
            "cancel_request": true,
            "cancel_confirmation": true,
            "cancel_confirmation_evidence": if codex { "app_server_turn_end" } else { "provider_terminal_or_managed_process_exit" },
            "native_steer": self.supports_steer(),
            "run_reconcile": false,
        })
    }
}

/// Which Codex App Server this Node uses: the one already running on this computer, so its
/// sessions are shared, or one the Node starts and stops itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppServerMode {
    #[default]
    System,
    Spawn,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppServer {
    pub mode: AppServerMode,
    /// The explicitly selected system App Server socket. Kept while the
    /// Node starts its own, so switching back restores it.
    pub socket: Option<PathBuf>,
}

/// What the providers do besides the bot, which `Config.manager` chooses. A provider is in
/// use while the bot runs on it or while it executes dispatched work.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Providers {
    /// The providers dispatched work runs on.
    pub execution: BTreeSet<Provider>,
    #[serde(default)]
    pub codex_app_server: AppServer,
}

impl Providers {
    /// Whether the App Server is in use: for the bot (`codex_agent`) or to execute work.
    pub fn app_server_used(&self, codex_agent: bool) -> bool {
        codex_agent
            || self
                .execution
                .iter()
                .any(|provider| provider.on_app_server())
    }

    pub fn validate(&self, codex_agent: bool) -> Result<()> {
        for provider in &self.execution {
            ensure!(
                provider.supports().contains(&Role::Execution),
                "{} does not execute dispatched work here yet",
                provider.label()
            );
        }
        ensure!(
            !self.app_server_used(codex_agent)
                || self.codex_app_server.mode != AppServerMode::System
                || self.codex_app_server.socket.is_some(),
            "set an explicit App Server socket before enabling system execution"
        );
        if let Some(socket) = &self.codex_app_server.socket {
            let text = socket.to_string_lossy();
            ensure!(
                socket.is_absolute() && text.len() <= 1024 && !text.contains('\0'),
                "the App Server socket must be an absolute path"
            );
        }
        Ok(())
    }
}

/// The variables each provider's processes get on top of the Node's own environment. A
/// provider's variables never reach another provider's processes. No `Debug`: it holds keys.
#[derive(Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct ProviderEnv(BTreeMap<Provider, BTreeMap<String, String>>);

static NO_VARIABLES: BTreeMap<String, String> = BTreeMap::new();

impl ProviderEnv {
    pub fn load(dir: &Path) -> Result<Self> {
        let env: Self = match std::fs::read(dir.join(ENV_FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("invalid provider environment")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(error.into()),
        };
        for (provider, variables) in &env.0 {
            for (name, value) in variables {
                validate(*provider, name, value)?;
                crate::logging::add_private_path(std::path::Path::new(value));
            }
        }
        env.redact_in_logs();
        Ok(env)
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        crate::config::private_write(&dir.join(ENV_FILE), &serde_json::to_vec(self)?)?;
        self.redact_in_logs();
        Ok(())
    }

    pub fn get(&self, provider: Provider) -> &BTreeMap<String, String> {
        self.0.get(&provider).unwrap_or(&NO_VARIABLES)
    }

    /// `Some` sets or replaces a variable, `None` removes it.
    pub fn apply(
        &mut self,
        provider: Provider,
        changes: BTreeMap<String, Option<String>>,
    ) -> Result<()> {
        let env = self.0.entry(provider).or_default();
        for (name, value) in changes {
            match value {
                Some(value) => {
                    validate(provider, &name, &value)?;
                    env.insert(name, value);
                }
                None => {
                    env.remove(&name);
                }
            }
        }
        ensure!(
            env.len() <= MAX_VARIABLES,
            "at most {MAX_VARIABLES} variables per provider"
        );
        if env.is_empty() {
            self.0.remove(&provider);
        }
        Ok(())
    }

    fn redact_in_logs(&self) {
        for value in self.0.values().flat_map(BTreeMap::values) {
            crate::logging::add_secret(value);
        }
    }
}

fn validate(provider: Provider, name: &str, value: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 256
            && name.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
            }),
        "variable names use letters, digits and underscores"
    );
    ensure!(
        !name.starts_with("VIBEFACE_")
            && !provider
                .reserved_env()
                .iter()
                .any(|prefix| name.starts_with(prefix)),
        "{name} is set by Vibeface"
    );
    ensure!(
        !value.is_empty() && value.len() <= 4096 && !value.contains('\0'),
        "a variable value must be 1 to 4096 characters"
    );
    Ok(())
}

/// A program path the owner or discovery chose for `provider`.
pub fn validate_bin(provider: Provider, bin: &Path) -> Result<()> {
    let text = bin.to_string_lossy();
    ensure!(
        !text.trim().is_empty() && text.len() <= 4096 && !text.contains('\0'),
        "invalid {} program path",
        provider.label()
    );
    Ok(())
}

/// The Cursor key the bridge is started with and the rest of the Cursor variables. A key set
/// here wins over the one the Node process inherited.
pub fn cursor_launch(env: &BTreeMap<String, String>) -> (Option<String>, BTreeMap<String, String>) {
    let mut env = env.clone();
    let key = env
        .remove(CURSOR_KEY)
        .or_else(|| std::env::var(CURSOR_KEY).ok());
    (key, env)
}

/// What the Node's App Server is started with.
#[derive(Clone, PartialEq)]
pub struct CodexLaunch {
    pub bin: PathBuf,
    pub env: BTreeMap<String, String>,
}

/// The App Server the Node talks to right now.
#[derive(Clone, PartialEq)]
pub enum CodexTarget {
    /// Codex does not execute work.
    Off,
    /// The App Server at the explicitly configured socket.
    System(PathBuf),
    Spawn(CodexLaunch),
}

impl CodexTarget {
    /// Which App Server keeps the threads: the explicitly selected system server, or the
    /// Node's own by the Codex home it stores them in. A thread or turn belongs to one of them.
    pub fn server(&self) -> String {
        match self {
            CodexTarget::Off => "off".into(),
            CodexTarget::System(_) => "system".into(),
            CodexTarget::Spawn(launch) => format!(
                "own:{}",
                launch.env.get("CODEX_HOME").map_or("", String::as_str)
            ),
        }
    }
}

/// Everything about the providers that only this machine knows.
#[derive(Clone)]
pub struct Local {
    pub switches: Providers,
    /// The programs the owner or discovery chose; every other provider runs its `default_bin`.
    pub bins: BTreeMap<Provider, PathBuf>,
    pub env: ProviderEnv,
}

impl Local {
    pub fn bin(&self, provider: Provider) -> PathBuf {
        self.bins
            .get(&provider)
            .cloned()
            .unwrap_or_else(|| provider.default_bin().into())
    }

    /// Whether the Node starts the provider's program itself: Codex on the system App Server
    /// runs the program and variables of that server.
    pub fn launches(&self, provider: Provider) -> bool {
        !provider.on_app_server() || self.switches.codex_app_server.mode == AppServerMode::Spawn
    }

    pub fn validate(&self, codex_agent: bool) -> Result<()> {
        self.switches.validate(codex_agent)?;
        if self.switches.app_server_used(codex_agent)
            && self.switches.codex_app_server.mode == AppServerMode::Spawn
        {
            ensure!(
                self.env
                    .get(Provider::Codex)
                    .get("CODEX_HOME")
                    .is_some_and(|home| std::path::Path::new(home).is_absolute()),
                "set an explicit absolute CODEX_HOME before starting a managed App Server"
            );
        }
        Ok(())
    }
    /// The variables reach only an App Server the Node starts; the system one keeps its own.
    pub fn codex_target(&self, codex_agent: bool) -> CodexTarget {
        let app_server = &self.switches.codex_app_server;
        match (self.switches.app_server_used(codex_agent), app_server.mode) {
            (false, _) => CodexTarget::Off,
            (true, AppServerMode::System) => match &app_server.socket {
                Some(socket) => CodexTarget::System(socket.clone()),
                None => CodexTarget::Off,
            },
            (true, AppServerMode::Spawn) => CodexTarget::Spawn(CodexLaunch {
                bin: self.bin(Provider::Codex),
                env: self.env.get(Provider::Codex).clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threads_belong_to_the_app_server_that_keeps_them() {
        let own = |home: Option<&str>| {
            CodexTarget::Spawn(CodexLaunch {
                bin: "codex".into(),
                env: home
                    .map(|home| ("CODEX_HOME".to_owned(), home.to_owned()))
                    .into_iter()
                    .collect(),
            })
            .server()
        };
        assert_eq!(
            CodexTarget::System("/tmp/first.sock".into()).server(),
            CodexTarget::System("/tmp/a.sock".into()).server()
        );
        assert_ne!(
            CodexTarget::System("/tmp/first.sock".into()).server(),
            own(None)
        );
        assert_ne!(own(None), own(Some("/tmp/home")));
    }

    #[test]
    fn variables_are_validated_and_stay_with_their_provider() -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("vibeface-provider-env-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let mut env = ProviderEnv::load(&dir)?;
        env.apply(
            Provider::Codex,
            BTreeMap::from([("CPA_API_KEY".into(), Some("fixture-codex-value".into()))]),
        )?;
        env.apply(
            Provider::Cursor,
            BTreeMap::from([("CURSOR_API_KEY".into(), Some("fixture-cursor-value".into()))]),
        )?;
        for (provider, name) in [
            (Provider::Codex, "VIBEFACE_DATA_DIR"),
            (Provider::Cursor, "CURSOR_SDK_PORT"),
            (Provider::Codex, "1KEY"),
            (Provider::Codex, "A-B"),
        ] {
            let mut copy = env.clone();
            assert!(
                copy.apply(
                    provider,
                    BTreeMap::from([(name.into(), Some("value".into()))])
                )
                .is_err(),
                "{name}"
            );
        }
        env.save(&dir)?;
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.join(ENV_FILE))?.permissions().mode() & 0o777,
            0o600
        );
        let loaded = ProviderEnv::load(&dir)?;
        assert!(!loaded.get(Provider::Cursor).contains_key("CPA_API_KEY"));
        assert!(!loaded.get(Provider::Codex).contains_key("CURSOR_API_KEY"));
        let (key, rest) = cursor_launch(loaded.get(Provider::Cursor));
        assert_eq!(key.as_deref(), Some("fixture-cursor-value"));
        assert!(rest.is_empty());
        let mut removed = loaded.clone();
        removed.apply(
            Provider::Codex,
            BTreeMap::from([("CPA_API_KEY".into(), None)]),
        )?;
        assert!(removed.get(Provider::Codex).is_empty());
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn the_execution_set_decides_whether_codex_runs_work() {
        let mut local = Local {
            switches: Providers::default(),
            bins: BTreeMap::new(),
            env: ProviderEnv::default(),
        };
        assert!(local.switches.execution.is_empty() && local.switches.validate(false).is_ok());
        assert!(local.codex_target(false) == CodexTarget::Off);
        assert!(local.validate(true).is_err());
        local.switches.execution.insert(Provider::Codex);
        assert!(local.validate(false).is_err());
        local.switches.codex_app_server.socket = Some("/configured/app-server.sock".into());
        assert!(local.validate(false).is_ok());
        assert!(matches!(local.codex_target(false), CodexTarget::System(_)));
        local.switches.execution.clear();
        assert!(local.validate(true).is_ok());
        assert!(matches!(local.codex_target(true), CodexTarget::System(_)));
        assert!(local.codex_target(false) == CodexTarget::Off);
        local.switches.execution.insert(Provider::Cursor);
        assert!(
            local.validate(true).is_err(),
            "only a provider that executes work joins the set"
        );
        assert!(
            serde_json::from_value::<Providers>(serde_json::json!({
                "execution":["codex"],"codex":{"enabled":true,"roles":["agent"]},
            }))
            .is_err(),
            "per-provider switches are gone"
        );
        assert!(
            serde_json::from_value::<Providers>(serde_json::json!({"execution":true})).is_err()
        );
    }

    #[test]
    fn codex_execution_requires_an_explicit_socket_or_spawn_mode() -> Result<()> {
        let saved: Providers = serde_json::from_value(serde_json::json!({"execution":["codex"]}))?;
        let mut env = ProviderEnv::default();
        env.apply(
            Provider::Codex,
            BTreeMap::from([("CPA_API_KEY".into(), Some("fixture-value".into()))]),
        )?;
        let mut local = Local {
            switches: saved,
            bins: BTreeMap::new(),
            env,
        };
        assert!(local.switches.validate(false).is_err());
        assert!(local.codex_target(false) == CodexTarget::Off);
        local.switches.codex_app_server = AppServer {
            mode: AppServerMode::Spawn,
            socket: Some("/tmp/kept.sock".into()),
        };
        let CodexTarget::Spawn(launch) = local.codex_target(false) else {
            panic!("spawn mode starts the Node's own App Server");
        };
        assert_eq!(launch.env["CPA_API_KEY"], "fixture-value");
        local.switches.codex_app_server.mode = AppServerMode::System;
        assert!(local.codex_target(false) == CodexTarget::System("/tmp/kept.sock".into()));
        local.switches.codex_app_server.socket = Some("relative.sock".into());
        assert!(local.switches.validate(false).is_err());
        Ok(())
    }

    #[test]
    fn a_provider_has_one_name_everywhere() {
        use clap::ValueEnum as _;
        assert_eq!(
            Provider::ALL.map(Provider::name),
            ["codex", "cursor", "claude", "pi", "opencode"]
        );
        for provider in Provider::ALL {
            assert_eq!(serde_json::to_value(provider).unwrap(), provider.name());
            assert_eq!(
                serde_json::from_value::<Provider>(json!(provider.name())).unwrap(),
                provider
            );
            assert_eq!(
                provider.to_possible_value().unwrap().get_name(),
                provider.name()
            );
        }
        for retired in ["codex_sdk", "cursor_sdk"] {
            assert!(serde_json::from_value::<Provider>(json!(retired)).is_err());
            assert!(<Provider as clap::ValueEnum>::from_str(retired, false).is_err());
        }
    }

    #[test]
    fn variables_and_programs_are_kept_per_provider() -> Result<()> {
        let env: ProviderEnv = serde_json::from_value(json!({
            "claude":{"ANTHROPIC_API_KEY":"fixture-claude-value"},"opencode":{"OPENCODE_MODEL":"m"},
        }))?;
        assert_eq!(
            env.get(Provider::Claude)["ANTHROPIC_API_KEY"],
            "fixture-claude-value"
        );
        assert!(env.get(Provider::Pi).is_empty() && env.get(Provider::Codex).is_empty());
        assert!(serde_json::from_value::<ProviderEnv>(json!({"cursor_sdk":{}})).is_err());
        let local = Local {
            switches: Providers::default(),
            bins: serde_json::from_value(json!({"pi":"/opt/pi/bin/pi"}))?,
            env,
        };
        assert_eq!(local.bin(Provider::Pi), PathBuf::from("/opt/pi/bin/pi"));
        assert_eq!(local.bin(Provider::Opencode), PathBuf::from("opencode"));
        assert_eq!(
            local.bin(Provider::Cursor),
            PathBuf::from("cursor-sdk-bridge")
        );
        assert!(local.launches(Provider::Claude) && !local.launches(Provider::Codex));
        Ok(())
    }
}
