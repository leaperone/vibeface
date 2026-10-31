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
/// the Node watches. Its methods say most of what the runtime needs to know about the product;
/// the code that speaks a provider's protocol also matches on it: its `Executor` and the resident
/// process the runtime keeps for it, its bot turn's `Sdk` and its hook parser. What only Codex has
/// also names it: its folder (discovery, its hook file), its App Server (whose connection, and
/// whether Codex executes work, the capabilities a Node offers report), and its being the default
/// provider of a new bot.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStage {
    Install,
    Launch,
    Authentication,
    ModelList,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeCode {
    NotInstalled,
    ConfigInvalid,
    MissingCredential,
    LaunchUnverified,
    LaunchFailed,
    SignInRequired,
    AuthenticationUnverified,
    CatalogUnavailable,
    ModelUnverified,
    ModelUnavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProbeIssue {
    pub stage: ProbeStage,
    pub code: ProbeCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<crate::model::FailureKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_error: Option<String>,
}

impl ProbeIssue {
    pub fn new(stage: ProbeStage, code: ProbeCode) -> Self {
        use crate::model::FailureKind;
        Self {
            stage,
            code,
            kind: match code {
                ProbeCode::NotInstalled => Some(FailureKind::MissingBinary),
                ProbeCode::MissingCredential => Some(FailureKind::MissingKey),
                ProbeCode::SignInRequired => Some(FailureKind::AuthFailed),
                ProbeCode::ModelUnavailable => Some(FailureKind::NoModel),
                _ => None,
            },
            safe_error: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LaunchCheck {
    Unchecked,
    Starting,
    Launchable,
    Failed { issue: ProbeIssue },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Authentication {
    Unverified,
    Authenticated,
    NotRequired,
    SignInRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogCoverage {
    Partial,
    Complete,
    AccountAvailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EffortOptions {
    ProviderControlled,
    Unknown,
    Selectable {
        options: Vec<String>,
        default: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ModelChoice {
    pub id: String,
    pub name: String,
    pub default: bool,
    pub effort: EffortOptions,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ModelCatalog {
    Unavailable {
        reason: ProbeCode,
    },
    Available {
        models: Vec<ModelChoice>,
        coverage: CatalogCoverage,
    },
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::Unavailable {
            reason: ProbeCode::CatalogUnavailable,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProviderProbe {
    pub provider: Provider,
    pub config_id: String,
    pub checked_at_ms: Option<i64>,
    pub launch: LaunchCheck,
    pub authentication: Authentication,
    pub catalog: ModelCatalog,
    pub version: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProviderReadiness {
    Starting,
    Ready,
    Unverified { issue: ProbeIssue },
    NeedsSetup { issue: ProbeIssue },
}

impl ProviderReadiness {
    pub fn state(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Unverified { .. } => "unverified",
            Self::NeedsSetup { .. } => "needs_setup",
        }
    }

    pub fn can_attempt(&self) -> bool {
        matches!(self, Self::Ready | Self::Unverified { .. })
    }
}

impl ProviderProbe {
    pub fn readiness(&self, model: &str) -> ProviderReadiness {
        use ProviderReadiness::{NeedsSetup, Unverified};
        match &self.launch {
            LaunchCheck::Failed { issue } => {
                return NeedsSetup {
                    issue: issue.clone(),
                };
            }
            LaunchCheck::Starting => return ProviderReadiness::Starting,
            _ => {}
        }
        if self.authentication == Authentication::SignInRequired {
            return NeedsSetup {
                issue: ProbeIssue::new(ProbeStage::Authentication, ProbeCode::SignInRequired),
            };
        }
        let model = self.provider.model(model);
        if let ModelCatalog::Available {
            models,
            coverage: CatalogCoverage::AccountAvailable,
        } = &self.catalog
            && !model.is_empty()
            && !models.iter().any(|choice| choice.id == model)
        {
            return NeedsSetup {
                issue: ProbeIssue::new(ProbeStage::ModelList, ProbeCode::ModelUnavailable),
            };
        }
        if self.launch == LaunchCheck::Unchecked {
            return Unverified {
                issue: ProbeIssue::new(ProbeStage::Launch, ProbeCode::LaunchUnverified),
            };
        }
        if self.authentication == Authentication::Unverified {
            return Unverified {
                issue: ProbeIssue::new(
                    ProbeStage::Authentication,
                    ProbeCode::AuthenticationUnverified,
                ),
            };
        }
        let known_model = match &self.catalog {
            ModelCatalog::Available { models, .. } => models.iter().any(|choice| {
                if model.is_empty() {
                    choice.default
                } else {
                    choice.id == model
                }
            }),
            ModelCatalog::Unavailable { .. } => false,
        };
        if !known_model {
            return Unverified {
                issue: ProbeIssue::new(ProbeStage::ModelList, ProbeCode::ModelUnverified),
            };
        }
        ProviderReadiness::Ready
    }
}

/// How the Node hears from the provider's sessions it did not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watch {
    /// `vibeface hook <provider>` on these events of a JSON hook file, relative to the home
    /// directory; Codex's is relative to its own folder.
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

/// What a dispatched work on the provider can ask for. `track_work` refuses anything else
/// before it registers the work, and `dispatch_work` describes it from here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkCapabilities {
    pub images: bool,
    pub effort: bool,
    /// It takes work in any of its sessions; otherwise only in one this device's works ran in,
    /// since another may be open in a program the Node does not see.
    pub any_thread: bool,
}

/// Where the provider keeps a bot's threads, which decides whether a thread outlives the
/// process that ran it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sessions {
    /// On the App Server the thread was created on.
    AppServer,
    /// In the provider's own store, named so, whichever process serves them.
    Store(&'static str),
    /// Only in the process that ran the turn.
    Process,
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

    /// What the runtime can run on each provider.
    pub fn supports(self) -> &'static [Role] {
        match self {
            Provider::Codex
            | Provider::Cursor
            | Provider::Claude
            | Provider::Pi
            | Provider::Opencode => &[Role::Agent, Role::Execution],
        }
    }

    /// A bot on the provider runs only on a model it is given; the others have a default.
    pub fn needs_model(self) -> bool {
        self == Provider::Cursor
    }

    pub fn validate_reasoning_effort(self, effort: Option<&str>) -> Result<()> {
        let Some(effort) = effort else { return Ok(()) };
        let options: &[&str] = match self {
            Self::Codex => &["none", "minimal", "low", "medium", "high", "xhigh"],
            Self::Claude => &["low", "medium", "high", "max"],
            Self::Pi => &["off", "minimal", "low", "medium", "high", "xhigh"],
            Self::Cursor | Self::Opencode => &[],
        };
        ensure!(
            options.contains(&effort),
            "{} does not support this reasoning effort",
            self.label()
        );
        Ok(())
    }

    /// Each bot on the provider runs in a resident instance of its own, with a private store under
    /// the data directory (see `CursorInstance`), and each of its turns takes a new tool token
    /// rather than the one its previous turn had.
    pub fn instance_per_bot(self) -> bool {
        self == Provider::Cursor
    }

    /// A Codex bot runs its turns on this Node's App Server, which must be up first.
    pub fn on_app_server(self) -> bool {
        self.sessions() == Sessions::AppServer
    }

    pub fn sessions(self) -> Sessions {
        match self {
            Provider::Codex => Sessions::AppServer,
            Provider::Opencode => Sessions::Store(crate::runtime::opencode::SERVER),
            Provider::Cursor | Provider::Claude | Provider::Pi => Sessions::Process,
        }
    }

    /// What a work on the provider can ask for.
    pub fn work(self) -> WorkCapabilities {
        match self {
            Provider::Codex => WorkCapabilities {
                images: true,
                effort: true,
                any_thread: true,
            },
            Provider::Claude => WorkCapabilities {
                images: false,
                effort: true,
                any_thread: false,
            },
            Provider::Pi => WorkCapabilities {
                images: true,
                effort: true,
                any_thread: false,
            },
            Provider::Opencode => WorkCapabilities {
                images: true,
                effort: false,
                any_thread: false,
            },
            Provider::Cursor => WorkCapabilities {
                images: false,
                effort: false,
                any_thread: false,
            },
        }
    }

    /// The providers whose work can ask for what `can` reads.
    pub fn executing(can: fn(WorkCapabilities) -> bool) -> Vec<&'static str> {
        Provider::ALL
            .into_iter()
            .filter(|provider| provider.supports().contains(&Role::Execution))
            .filter(|provider| can(provider.work()))
            .map(Provider::name)
            .collect()
    }

    /// Refuses what a dispatch's `args` ask of a work on this provider that it cannot do.
    pub fn check_work(self, args: &Value) -> Result<()> {
        let can = self.work();
        for (field, taken, takers) in [
            ("images", can.images, Provider::executing(|can| can.images)),
            ("effort", can.effort, Provider::executing(|can| can.effort)),
        ] {
            let asked = match &args[field] {
                Value::Null => false,
                Value::Array(items) => !items.is_empty(),
                _ => true,
            };
            ensure!(
                !asked || taken,
                "{} work takes no {field}; leave {field} out, or set agent to one of {takers:?}",
                self.label()
            );
        }
        Ok(())
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
                file: "hooks.json",
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
            Provider::Opencode => Some(Watch::Extension {
                file: ".config/opencode/plugins/vibeface.ts",
                source: crate::runtime::opencode::PLUGIN,
            }),
            Provider::Cursor => None,
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

    pub fn native_compaction(self) -> bool {
        matches!(self, Provider::Codex | Provider::Claude | Provider::Pi)
    }

    /// Name prefixes of the variables the Node sets itself for this provider's processes.
    fn reserved_env(self) -> &'static [&'static str] {
        match self {
            Provider::Cursor => &["CURSOR_SDK_", "CURSOR_DATA_DIR"],
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
            "context_compaction": if self.native_compaction() { "native" } else { "handoff" },
            // Codex's and Claude Code's runners send whole text; the others stream it as well.
            "text_stream_mode": match self {
                Provider::Codex | Provider::Claude => "snapshot",
                _ => "delta_and_snapshot",
            },
            "cancel_request": true,
            "cancel_confirmation": true,
            "cancel_confirmation_evidence": if codex {
                "app_server_turn_end"
            } else if self == Provider::Cursor {
                "provider_terminal"
            } else {
                "provider_terminal_or_managed_process_exit"
            },
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
    Spawn,
    System,
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
    /// A device-local default model for dispatched work on each provider. Empty values follow
    /// the provider's own default; the settings entry point removes empty overrides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub execution_models: BTreeMap<Provider, String>,
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
        for (provider, model) in &self.execution_models {
            ensure!(
                provider.supports().contains(&Role::Execution),
                "{} does not execute dispatched work here yet",
                provider.label()
            );
            crate::model::validate_model(model)?;
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

    /// The configured execution override, or the provider's own default when it is empty.
    pub fn execution_model(&self, provider: Provider) -> String {
        self.execution_models
            .get(&provider)
            .map_or_else(|| provider.model(""), |model| provider.model(model))
    }

    /// Resolves one execution request without consulting mutable settings again after it is
    /// persisted into a work record.
    pub fn resolve_execution_model(&self, provider: Provider, requested: Option<&str>) -> String {
        requested
            .filter(|model| !model.trim().is_empty())
            .map(|model| provider.model(model.trim()))
            .unwrap_or_else(|| self.execution_model(provider))
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
        env.hide_values();
        Ok(env)
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        crate::config::private_write(&dir.join(ENV_FILE), &serde_json::to_vec(self)?)?;
        self.hide_values();
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

    fn hide_values(&self) {
        for (name, value) in self.0.values().flat_map(BTreeMap::iter) {
            if crate::logging::credential_name(name) || crate::model::looks_secret(value) {
                crate::logging::add_credential(value);
            } else {
                crate::logging::add_secret(value);
            }
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

/// Private provider storage owned by one Node. It is derived from the Node data directory and is
/// never part of shared settings or work records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRoots {
    pub codex_home: PathBuf,
    pub codex_sessions: PathBuf,
    pub claude_config: PathBuf,
    pub claude_projects: PathBuf,
}

impl ProviderRoots {
    pub fn new(data_dir: &Path) -> Self {
        let codex_home = data_dir.join("providers/codex");
        let claude_config = data_dir.join("providers/claude");
        Self {
            codex_sessions: codex_home.join("sessions"),
            claude_projects: claude_config.join("projects"),
            codex_home,
            claude_config,
        }
    }
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
    fn readiness_keeps_unknowns_attemptable_and_blocks_only_proven_failures() {
        let mut probe = ProviderProbe {
            provider: Provider::Codex,
            config_id: "fixture-config".into(),
            checked_at_ms: None,
            launch: LaunchCheck::Unchecked,
            authentication: Authentication::Unverified,
            catalog: ModelCatalog::default(),
            version: None,
        };
        assert_eq!(probe.readiness("manual-model").state(), "unverified");
        assert!(probe.readiness("manual-model").can_attempt());
        for code in [
            ProbeCode::NotInstalled,
            ProbeCode::MissingCredential,
            ProbeCode::LaunchFailed,
        ] {
            probe.launch = LaunchCheck::Failed {
                issue: ProbeIssue::new(ProbeStage::Launch, code),
            };
            assert_eq!(probe.readiness("manual-model").state(), "needs_setup");
            assert!(!probe.readiness("manual-model").can_attempt());
        }
        probe.launch = LaunchCheck::Launchable;
        probe.authentication = Authentication::SignInRequired;
        assert!(matches!(
            probe.readiness("model"),
            ProviderReadiness::NeedsSetup {
                issue: ProbeIssue {
                    code: ProbeCode::SignInRequired,
                    ..
                }
            }
        ));
        probe.authentication = Authentication::Authenticated;
        probe.catalog = ModelCatalog::Available {
            models: vec![ModelChoice {
                id: "model".into(),
                name: "Model".into(),
                default: true,
                effort: EffortOptions::Unknown,
            }],
            coverage: CatalogCoverage::Partial,
        };
        assert_eq!(probe.readiness("model"), ProviderReadiness::Ready);
        assert_eq!(probe.readiness(""), ProviderReadiness::Ready);
        assert_eq!(probe.readiness("manual-model").state(), "unverified");
        if let ModelCatalog::Available { coverage, .. } = &mut probe.catalog {
            *coverage = CatalogCoverage::Complete;
        }
        assert!(probe.readiness("manual-model").can_attempt());
        if let ModelCatalog::Available { coverage, .. } = &mut probe.catalog {
            *coverage = CatalogCoverage::AccountAvailable;
        }
        assert!(matches!(
            probe.readiness("manual-model"),
            ProviderReadiness::NeedsSetup {
                issue: ProbeIssue {
                    code: ProbeCode::ModelUnavailable,
                    ..
                }
            }
        ));
    }

    #[test]
    fn effort_validation_uses_native_values_without_guessing_model_support() {
        for (provider, accepted, rejected) in [
            (Provider::Codex, Some("xhigh"), "max"),
            (Provider::Claude, Some("max"), "xhigh"),
            (Provider::Pi, Some("off"), "none"),
            (Provider::Cursor, None, "medium"),
            (Provider::Opencode, None, "medium"),
        ] {
            assert!(provider.validate_reasoning_effort(accepted).is_ok());
            assert!(provider.validate_reasoning_effort(Some(rejected)).is_err());
        }
    }

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
            (Provider::Cursor, "CURSOR_DATA_DIR"),
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
    fn cursor_cancel_confirmation_requires_the_provider_terminal() {
        assert_eq!(
            Provider::Cursor.capabilities()["cancel_confirmation_evidence"],
            "provider_terminal"
        );
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
        assert!(local.validate(true).is_ok());
        local.switches.execution.insert(Provider::Codex);
        assert!(local.validate(false).is_ok());
        assert!(matches!(local.codex_target(false), CodexTarget::Spawn(_)));
        local.switches.execution.clear();
        assert!(local.validate(true).is_ok());
        assert!(matches!(local.codex_target(true), CodexTarget::Spawn(_)));
        assert!(local.codex_target(false) == CodexTarget::Off);
        local.switches.execution.insert(Provider::Cursor);
        assert!(
            local.validate(true).is_ok(),
            "Cursor can execute dispatched work"
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
        assert!(local.switches.validate(false).is_ok());
        assert!(matches!(local.codex_target(false), CodexTarget::Spawn(_)));
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
    fn execution_model_overrides_are_separate_from_the_bot_and_clear_to_provider_default() {
        let mut providers = Providers::default();
        providers
            .execution_models
            .insert(Provider::Codex, "fixture-execution".into());
        assert_eq!(
            providers.execution_model(Provider::Codex),
            "fixture-execution"
        );
        assert_eq!(
            providers.resolve_execution_model(Provider::Codex, None),
            "fixture-execution"
        );
        assert_eq!(
            providers.resolve_execution_model(Provider::Codex, Some("fixture-request")),
            "fixture-request"
        );
        providers.execution_models.remove(&Provider::Codex);
        assert_eq!(providers.execution_model(Provider::Codex), "");
    }

    #[test]
    fn provider_roots_are_derived_inside_the_node_data_directory() {
        let roots = ProviderRoots::new(Path::new("/private/node"));
        assert_eq!(
            roots.codex_home,
            PathBuf::from("/private/node/providers/codex")
        );
        assert_eq!(roots.codex_sessions, roots.codex_home.join("sessions"));
        assert_eq!(
            roots.claude_config,
            PathBuf::from("/private/node/providers/claude")
        );
        assert_eq!(roots.claude_projects, roots.claude_config.join("projects"));
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
        assert!(local.launches(Provider::Claude) && local.launches(Provider::Codex));
        Ok(())
    }
}
