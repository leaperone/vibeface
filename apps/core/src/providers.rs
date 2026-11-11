//! The AI providers this Node runs locally, what a bot on each can do, and the variables each
//! provider's processes get. Provider configuration and model keys live in one private `config.json`
//! snapshot in the data directory and
//! never reach events, logs or the web, which only learns which names are set.
use crate::scheduling::TriggerKind;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use vibeface_bridge::agent::{CURSOR_AUTH_OVERRIDES, CustomToolConfig, is_cursor_sdk_bridge};

mod login;
pub use login::AuthenticationSource;
pub use login::ExternalCodexLogin;

const MAX_VARIABLES: usize = 64;

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
pub enum ProviderProbeReadiness {
    Starting,
    Ready,
    Unverified { issue: ProbeIssue },
    NeedsSetup { issue: ProbeIssue },
}

impl ProviderProbeReadiness {
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
    pub fn readiness(&self, model: &str) -> ProviderProbeReadiness {
        use ProviderProbeReadiness::{NeedsSetup, Unverified};
        match &self.launch {
            LaunchCheck::Failed { issue } => {
                return NeedsSetup {
                    issue: issue.clone(),
                };
            }
            LaunchCheck::Starting => return ProviderProbeReadiness::Starting,
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
        if !self.model_known(model.as_str()) {
            return Unverified {
                issue: ProbeIssue::new(ProbeStage::ModelList, ProbeCode::ModelUnverified),
            };
        }
        ProviderProbeReadiness::Ready
    }

    fn model_known(&self, model: &str) -> bool {
        let model = self.provider.model(model);
        match &self.catalog {
            ModelCatalog::Available { models, .. } => models.iter().any(|choice| {
                if model.is_empty() {
                    choice.default
                } else {
                    choice.id == model
                }
            }),
            ModelCatalog::Unavailable { .. } => false,
        }
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptMode {
    /// The thread keeps its instructions, the tool catalog and what it was shown, so a turn
    /// carries only what is new and a thread lost on the App Server is replaced by a full one.
    Thread,
    /// Every turn carries everything.
    Turn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillInjection {
    Folder,
    Plugin,
    PromptIndex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMode {
    StaticCatalog,
    DynamicCatalog,
    NativeCallback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    ReadOnly,
    Workspace,
    FullAccess,
    ProviderControlled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyMode {
    Serial,
    Parallel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMode {
    Native,
    Request,
    Handoff,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySupport {
    Supported,
    Unsupported,
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPersistence {
    AppServer,
    Process,
    ProviderStore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StablePromptPlacement {
    ThreadInstructions,
    SessionInstructions,
    TurnInstructions,
    UserTurnPrefix,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DynamicContextPlacement {
    UserTurn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextStreamMode {
    Snapshot,
    DeltaAndSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelEvidence {
    AppServerTurnEnd,
    ProviderTerminal,
    ProviderTerminalOrManagedProcessExit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationStatus {
    Confirmed,
    Rejected,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessEvidence {
    BinaryDiscovery,
    LaunchPrerequisites,
    AppServerHandshake,
    AppServerStarting,
    AppServerUnavailable,
    ManagedLaunchFailed,
    ConfigurationInvalid,
    AuthenticationNotObserved,
    ModelCatalogNotObserved,
    ReadinessNotObserved,
    PrerequisitesRejected,
    ProviderProbe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderObservation {
    pub status: ObservationStatus,
    pub evidence: ReadinessEvidence,
}

impl ProviderObservation {
    pub fn new(status: ObservationStatus, evidence: ReadinessEvidence) -> Self {
        Self { status, evidence }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderReadiness {
    pub installed: ProviderObservation,
    pub launchable: ProviderObservation,
    pub authenticated: ProviderObservation,
    pub model_known: ProviderObservation,
    pub ready: ProviderObservation,
}

impl ProviderReadiness {
    pub fn from_prerequisites(installed: bool, launchable: ProviderObservation) -> Self {
        Self {
            installed: ProviderObservation::new(
                if installed {
                    ObservationStatus::Confirmed
                } else {
                    ObservationStatus::Rejected
                },
                ReadinessEvidence::BinaryDiscovery,
            ),
            launchable,
            authenticated: ProviderObservation::new(
                ObservationStatus::Unknown,
                ReadinessEvidence::AuthenticationNotObserved,
            ),
            model_known: ProviderObservation::new(
                ObservationStatus::Unknown,
                ReadinessEvidence::ModelCatalogNotObserved,
            ),
            ready: ProviderObservation::new(
                if launchable.status == ObservationStatus::Rejected {
                    ObservationStatus::Rejected
                } else {
                    ObservationStatus::Unknown
                },
                if launchable.status == ObservationStatus::Rejected {
                    ReadinessEvidence::PrerequisitesRejected
                } else {
                    ReadinessEvidence::ReadinessNotObserved
                },
            ),
        }
    }

    pub fn with_probe(mut self, probe: &ProviderProbe, model: &str) -> Self {
        let observed = |status| ProviderObservation::new(status, ReadinessEvidence::ProviderProbe);
        match probe.launch {
            LaunchCheck::Launchable => self.launchable = observed(ObservationStatus::Confirmed),
            LaunchCheck::Failed { .. } => self.launchable = observed(ObservationStatus::Rejected),
            LaunchCheck::Starting => self.launchable = observed(ObservationStatus::Unknown),
            LaunchCheck::Unchecked => {}
        }
        self.authenticated = observed(match probe.authentication {
            Authentication::Authenticated | Authentication::NotRequired => {
                ObservationStatus::Confirmed
            }
            Authentication::SignInRequired => ObservationStatus::Rejected,
            Authentication::Unverified => ObservationStatus::Unknown,
        });
        let readiness = probe.readiness(model);
        self.model_known = observed(if probe.model_known(model) {
            ObservationStatus::Confirmed
        } else if matches!(&readiness, ProviderProbeReadiness::NeedsSetup { issue } if issue.code == ProbeCode::ModelUnavailable)
        {
            ObservationStatus::Rejected
        } else {
            ObservationStatus::Unknown
        });
        self.ready = observed(match readiness {
            ProviderProbeReadiness::Ready => ObservationStatus::Confirmed,
            ProviderProbeReadiness::NeedsSetup { .. } => ObservationStatus::Rejected,
            ProviderProbeReadiness::Starting | ProviderProbeReadiness::Unverified { .. } => {
                ObservationStatus::Unknown
            }
        });
        self
    }
}

/// The provider facts used by the prompt compiler and scheduler. This is a description of a
/// provider adapter, not a permission grant. Tool effects and workspace checks remain in the Node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDescriptor {
    pub provider: Provider,
    pub sessions: SessionPersistence,
    pub prompt_mode: PromptMode,
    pub stable_prompt: StablePromptPlacement,
    pub dynamic_context: DynamicContextPlacement,
    pub tool_mode: ToolMode,
    pub skill_injection: SkillInjection,
    pub workspace_required: bool,
    pub sandbox: SandboxMode,
    pub steer: bool,
    pub concurrency: ConcurrencyMode,
    pub compaction: CompactionMode,
    pub usage: CapabilitySupport,
    pub approvals: CapabilitySupport,
    pub model_catalog: CapabilitySupport,
    pub streaming: bool,
    pub cancel: bool,
    pub resume_local_session: bool,
    pub text_stream_mode: TextStreamMode,
    pub cancel_confirmation_evidence: CancelEvidence,
}

impl ProviderDescriptor {
    pub fn for_provider(provider: Provider) -> Self {
        Self {
            provider,
            sessions: match provider.sessions() {
                Sessions::AppServer => SessionPersistence::AppServer,
                Sessions::Store(_) => SessionPersistence::ProviderStore,
                Sessions::Process => SessionPersistence::Process,
            },
            prompt_mode: if provider == Provider::Cursor {
                PromptMode::Turn
            } else {
                PromptMode::Thread
            },
            stable_prompt: match provider {
                Provider::Codex => StablePromptPlacement::ThreadInstructions,
                Provider::Cursor => StablePromptPlacement::TurnInstructions,
                Provider::Claude | Provider::Pi => StablePromptPlacement::SessionInstructions,
                Provider::Opencode => StablePromptPlacement::UserTurnPrefix,
            },
            dynamic_context: DynamicContextPlacement::UserTurn,
            tool_mode: ToolMode::DynamicCatalog,
            skill_injection: match provider {
                Provider::Codex | Provider::Cursor | Provider::Pi => SkillInjection::Folder,
                Provider::Claude => SkillInjection::Plugin,
                Provider::Opencode => SkillInjection::PromptIndex,
            },
            workspace_required: true,
            sandbox: if provider == Provider::Codex {
                SandboxMode::FullAccess
            } else {
                SandboxMode::ProviderControlled
            },
            steer: provider != Provider::Cursor,
            concurrency: if provider == Provider::Codex {
                ConcurrencyMode::Parallel
            } else {
                ConcurrencyMode::Serial
            },
            compaction: CompactionMode::Handoff,
            usage: if provider == Provider::Cursor {
                CapabilitySupport::Supported
            } else {
                CapabilitySupport::Unsupported
            },
            approvals: CapabilitySupport::Unsupported,
            model_catalog: if matches!(provider, Provider::Codex | Provider::Cursor) {
                CapabilitySupport::Supported
            } else {
                CapabilitySupport::Unsupported
            },
            streaming: true,
            cancel: true,
            resume_local_session: true,
            text_stream_mode: if matches!(provider, Provider::Codex | Provider::Claude) {
                TextStreamMode::Snapshot
            } else {
                TextStreamMode::DeltaAndSnapshot
            },
            cancel_confirmation_evidence: match provider {
                Provider::Codex => CancelEvidence::AppServerTurnEnd,
                Provider::Cursor => CancelEvidence::ProviderTerminal,
                Provider::Claude | Provider::Pi | Provider::Opencode => {
                    CancelEvidence::ProviderTerminalOrManagedProcessExit
                }
            },
        }
    }

    pub fn check_steer(&self) -> Result<()> {
        ensure!(
            self.steer,
            "{} cannot steer a running turn",
            self.provider.label()
        );
        Ok(())
    }

    pub fn compact_or_handoff(&self) -> Result<CompactionMode> {
        ensure!(
            self.compaction == CompactionMode::Handoff,
            "{} has no Node compaction adapter",
            self.provider.label()
        );
        Ok(CompactionMode::Handoff)
    }
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
            Provider::Cursor => "agent",
            Provider::Codex | Provider::Claude | Provider::Pi | Provider::Opencode => self.name(),
        }
    }

    /// The variables holding this provider's model keys: it needs them to start, and logs
    /// never show their values. `env` is what this Node sets for the provider; Codex reads
    /// the names from the configuration in its `CODEX_HOME`.
    pub fn key_variables(self, env: &BTreeMap<String, String>) -> Result<Vec<String>> {
        match self {
            Provider::Codex => crate::manager::provider_environment_keys(env),
            Provider::Cursor | Provider::Claude | Provider::Pi | Provider::Opencode => Ok(vec![]),
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
        WorkCapabilities {
            images: matches!(self, Provider::Codex | Provider::Pi | Provider::Opencode),
            effort: matches!(self, Provider::Codex | Provider::Claude | Provider::Pi),
            any_thread: self == Provider::Codex,
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
        self.descriptor().steer
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
        self.descriptor().prompt_mode
    }

    /// Name prefixes of the variables the Node sets itself for this provider's processes.
    fn reserved_env(self) -> &'static [&'static str] {
        match self {
            Provider::Cursor => &["CURSOR_CONFIG_DIR", "CURSOR_DATA_DIR"],
            Provider::Opencode => &["OPENCODE_SERVER_", "OPENCODE_CONFIG_CONTENT"],
            Provider::Codex | Provider::Claude | Provider::Pi => &[],
        }
    }

    /// The discovery entry points and essential reads. The router checks each discovered
    /// target against the current turn, including when a thread keeps this initial catalog.
    pub fn tools_for(self, kind: TriggerKind) -> BTreeMap<String, CustomToolConfig> {
        crate::scheduler::tools(match self.prompt_mode() {
            PromptMode::Thread => TriggerKind::Human,
            PromptMode::Turn => kind,
        })
        .into_iter()
        .filter(|(name, _)| {
            crate::tools::Tool::parse(name).is_some_and(crate::tools::Tool::initially_exposed)
        })
        .collect()
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

    pub fn descriptor(self) -> ProviderDescriptor {
        ProviderDescriptor::for_provider(self)
    }

    pub fn capabilities(self) -> Value {
        json!(self.descriptor())
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
    pub(crate) fn remove_cursor_auth_overrides(&mut self) {
        if let Some(cursor) = self.0.get_mut(&Provider::Cursor) {
            cursor.retain(|name, _| !CURSOR_AUTH_OVERRIDES.contains(&name.as_str()));
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        for (provider, variables) in &self.0 {
            for (name, value) in variables {
                validate(*provider, name, value)?;
                crate::logging::add_private_path(std::path::Path::new(value));
            }
        }
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

pub(crate) fn validate_env_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 256
            && name.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
            }),
        "variable names use letters, digits and underscores"
    );
    Ok(())
}

fn validate(provider: Provider, name: &str, value: &str) -> Result<()> {
    validate_env_name(name)?;
    ensure!(
        provider != Provider::Cursor || !CURSOR_AUTH_OVERRIDES.contains(&name),
        "Cursor uses the official CLI subscription login; authentication overrides are unsupported"
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

/// What the Node's App Server is started with.
#[derive(Clone, PartialEq)]
pub struct CodexLaunch {
    pub bin: PathBuf,
    pub env: BTreeMap<String, String>,
    pub external_auth: Option<ExternalCodexLogin>,
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
    pub pi_sessions: PathBuf,
    pub opencode: PathBuf,
}

impl ProviderRoots {
    pub fn new(data_dir: &Path) -> Self {
        let codex_home = data_dir.join("providers/codex");
        let claude_config = data_dir.join("providers/claude");
        Self {
            codex_sessions: codex_home.join("sessions"),
            claude_projects: claude_config.join("projects"),
            pi_sessions: data_dir.join("providers/pi/sessions"),
            opencode: data_dir.join("providers/opencode"),
            codex_home,
            claude_config,
        }
    }

    pub fn env(&self, provider: Provider) -> BTreeMap<String, String> {
        let paths = match provider {
            Provider::Codex => vec![("CODEX_HOME", self.codex_home.clone())],
            Provider::Claude => vec![("CLAUDE_CONFIG_DIR", self.claude_config.clone())],
            Provider::Pi => vec![("PI_CODING_AGENT_SESSION_DIR", self.pi_sessions.clone())],
            Provider::Opencode => vec![
                ("XDG_DATA_HOME", self.opencode.join("data")),
                ("XDG_CONFIG_HOME", self.opencode.join("config")),
                ("XDG_STATE_HOME", self.opencode.join("state")),
                ("XDG_CACHE_HOME", self.opencode.join("cache")),
                (
                    "OPENCODE_DB",
                    self.opencode.join("data/opencode/opencode.db"),
                ),
                ("OPENCODE_CONFIG_DIR", self.opencode.join("config/opencode")),
            ],
            Provider::Cursor => vec![],
        };
        let mut env: BTreeMap<_, _> = paths
            .into_iter()
            .map(|(name, path)| (name.into(), path.to_string_lossy().into_owned()))
            .collect();
        if provider == Provider::Opencode {
            for name in [
                "OPENCODE_DISABLE_PROJECT_CONFIG",
                "OPENCODE_CONFIG_PROJECT_DISABLE",
                "OPENCODE_DISABLE_CLAUDE_CODE",
            ] {
                env.insert(name.into(), "true".into());
            }
        }
        env
    }

    /// Applies private roots without reading personal provider configuration.
    pub fn apply(&self, local: &mut Local) -> Result<()> {
        for provider in Provider::ALL {
            local.env.apply(
                provider,
                self.env(provider)
                    .into_iter()
                    .map(|(name, value)| (name, Some(value)))
                    .collect(),
            )?;
        }
        Ok(())
    }

    /// A new private home inherits only safe model/provider settings, never the user's auth
    /// or session store. An existing private configuration always wins.
    pub fn prepare_codex_config(&self, host: &crate::discovery::Host) -> Result<Option<String>> {
        let target = self.codex_home.join("config.toml");
        match std::fs::symlink_metadata(&target) {
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => anyhow::bail!("could not inspect private Codex configuration"),
        }
        let Some(home) = host
            .codex_home
            .clone()
            .or_else(|| host.home.as_ref().map(|home| home.join(".codex")))
        else {
            return Ok(None);
        };
        let Some(source) = crate::manager::read_codex_config(&home)
            .map_err(|_| crate::discovery::LocalSourceError)?
        else {
            return Ok(None);
        };
        let projected = crate::manager::codex_config_projection(&source)
            .map_err(|_| crate::discovery::LocalSourceError)?;
        if projected.is_empty() {
            return Ok(None);
        }
        toml::to_string(&projected)
            .map(Some)
            .map_err(|_| anyhow::anyhow!("could not prepare private Codex configuration"))
    }

    pub fn publish_codex_config(&self, source: &str) -> Result<bool> {
        let target = self.codex_home.join("config.toml");
        crate::config::private_dir(&self.codex_home)?;
        let temp = self
            .codex_home
            .join(format!(".config-{}.toml", uuid::Uuid::new_v4()));
        crate::config::private_write(&temp, source.as_bytes())
            .map_err(|_| anyhow::anyhow!("could not prepare private Codex configuration"))?;
        // A hard link publishes complete contents without replacing a concurrent creator.
        let created = match std::fs::hard_link(&temp, &target) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(_) => Err(anyhow::anyhow!(
                "could not create private Codex configuration"
            )),
        };
        std::fs::remove_file(temp).map_err(|_| {
            anyhow::anyhow!("could not remove private Codex configuration staging file")
        })?;
        let created = created?;
        if created {
            std::fs::File::open(&self.codex_home)?.sync_all()?;
        }
        Ok(created)
    }
}

impl Local {
    pub(crate) fn codex_external_auth(&self) -> Option<ExternalCodexLogin> {
        login::external_codex_login(self)
    }
    pub fn bin(&self, provider: Provider) -> PathBuf {
        self.bins
            .get(&provider)
            .filter(|bin| provider != Provider::Cursor || !is_cursor_sdk_bridge(bin))
            .cloned()
            .unwrap_or_else(|| provider.default_bin().into())
    }

    /// npm wrappers use `env node`; keep the selected installation's Node ahead of
    /// any inherited version without changing the saved environment.
    pub fn launch_env(&self, provider: Provider) -> BTreeMap<String, String> {
        self.launch_env_on(provider, &crate::discovery::Host::current())
    }

    pub(crate) fn launch_env_on(
        &self,
        provider: Provider,
        host: &crate::discovery::Host,
    ) -> BTreeMap<String, String> {
        let mut env = self.env.get(provider).clone();
        if provider == Provider::Cursor {
            env.retain(|name, _| !CURSOR_AUTH_OVERRIDES.contains(&name.as_str()));
        }
        let path = env
            .get("PATH")
            .map(std::ffi::OsString::from)
            .or_else(|| host.path.clone());
        let mut program = self.bin(provider);
        if program.components().count() == 1
            && !program.is_absolute()
            && let Some(found) = path
                .iter()
                .flat_map(std::env::split_paths)
                .map(|dir| dir.join(&program))
                .find(|path| crate::discovery::executable_file(path))
        {
            program = found;
        }
        let Some(bin) = program.parent().filter(|bin| bin.is_absolute()) else {
            return env;
        };
        let sibling = vibeface_bridge::platform::program::candidates(&bin.join("node"))
            .iter()
            .any(|node| crate::discovery::executable_file(node));
        let bin = if sibling {
            bin.to_path_buf()
        } else {
            if !crate::discovery::uses_path_node(&program)
                || crate::discovery::runnable(Path::new("node"), path.as_ref())
            {
                return env;
            }
            let Some(node) = (crate::discovery::Host {
                path: path.clone(),
                ..host.clone()
            })
            .program("node") else {
                return env;
            };
            let Some(bin) = node.parent() else {
                return env;
            };
            bin.to_path_buf()
        };
        let paths = std::iter::once(bin.clone()).chain(
            path.iter()
                .flat_map(std::env::split_paths)
                .filter(|dir| dir != &bin),
        );
        if let Some(path) = std::env::join_paths(paths)
            .ok()
            .and_then(|path| path.into_string().ok())
        {
            env.insert("PATH".into(), path);
        }
        env
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
                env: self.launch_env(Provider::Codex),
                external_auth: self.codex_external_auth(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn local_cli_logins_update_only_owned_access_snapshots_and_do_not_import_sessions() -> Result<()>
    {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("local-login-{}", uuid::Uuid::new_v4()));
        let host = crate::discovery::Host {
            home: Some(dir.join("person")),
            ..Default::default()
        };
        let roots = ProviderRoots::new(&dir.join("node"));
        let mut local = Local {
            switches: Providers::default(),
            bins: BTreeMap::new(),
            env: ProviderEnv::default(),
        };
        roots.apply(&mut local)?;
        for (provider, source, target, credential) in [
            (
                Provider::Codex,
                dir.join("person/.codex/auth.json"),
                roots.codex_home.join("auth.json"),
                json!({"auth_mode":"chatgpt", "tokens":{"access_token":"synthetic-access", "refresh_token":"synthetic-refresh", "account_id":"fixture-account"}, "unrelated":"not-imported"}),
            ),
            (
                Provider::Claude,
                dir.join("person/.claude/.credentials.json"),
                roots.claude_config.join("local-login-access.json"),
                json!({"claudeAiOauth":{"accessToken":"synthetic-access", "refreshToken":"synthetic-refresh", "expiresAt":0}, "unrelated":"not-imported"}),
            ),
        ] {
            let executable = dir.join(if cfg!(windows) {
                format!("{}.cmd", provider.default_bin())
            } else {
                provider.default_bin().into()
            });
            crate::config::private_dir(&dir)?;
            crate::config::private_write(&executable, b"#!/bin/sh\nexit 0\n")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
            }
            local.bins.insert(provider, executable);
            assert!(!roots.prepare_login(provider, &local, &host)?);
            crate::config::private_dir(source.parent().unwrap())?;
            crate::config::private_write(&source, &serde_json::to_vec(&credential)?)?;
            std::fs::write(source.with_file_name("sessions"), "personal-session")?;
            assert!(roots.prepare_login(provider, &local, &host)?);
            assert!(vibeface_bridge::platform::privatefs::is_private(&target)?);
            let imported: Value = serde_json::from_slice(&std::fs::read(&target)?)?;
            assert!(imported.get("unrelated").is_none());
            if provider == Provider::Codex {
                assert_eq!(imported["auth_mode"], "chatgptAuthTokens");
                assert_eq!(imported["tokens"]["refresh_token"], "");
            } else {
                assert!(imported["claudeAiOauth"].get("refreshToken").is_none());
                assert!(!roots.claude_config.join(".credentials.json").exists());
                assert!(roots.imported_login_expired(Provider::Claude, &local));
            }
            assert!(!target.with_file_name("sessions").exists());
            assert_eq!(
                roots.authentication_source(provider, &local),
                AuthenticationSource::ImportedCliLogin
            );
            let original_target = std::fs::read(&target)?;
            let mut refreshed = credential.clone();
            if provider == Provider::Codex {
                refreshed["tokens"]["access_token"] = json!("synthetic-new-access");
            } else {
                refreshed["claudeAiOauth"]["accessToken"] = json!("synthetic-new-access");
                refreshed["claudeAiOauth"]["expiresAt"] =
                    json!(vibeface_protocol::now_ms() + 3_600_000);
            }
            let original = serde_json::to_vec(&refreshed)?;
            crate::config::private_write(&source, &original)?;
            assert!(roots.prepare_login(provider, &local, &host)?);
            assert!(!roots.prepare_login(provider, &local, &host)?);
            assert_eq!(
                std::fs::read(&source)?,
                original,
                "the source alone retains refresh ownership"
            );
            if provider == Provider::Codex {
                assert_eq!(
                    std::fs::read(&target)?,
                    original_target,
                    "source updates never overwrite the CLI credential file"
                );
            } else {
                assert!(!roots.claude_config.join(".credentials.json").exists());
            }
            let updated: Value = serde_json::from_slice(&std::fs::read(
                target.parent().unwrap().join("local-login-access.json"),
            )?)?;
            if provider == Provider::Claude {
                assert_eq!(
                    roots.borrowed_claude_environment(&local)?["CLAUDE_CODE_OAUTH_TOKEN"],
                    "synthetic-new-access"
                );
            } else {
                assert_eq!(
                    local
                        .codex_external_auth()
                        .context("external snapshot")?
                        .access_token,
                    "synthetic-new-access"
                );
            }
            assert!(updated.to_string().contains("synthetic-new-access"));
            assert!(!updated.to_string().contains("synthetic-refresh"));
            assert!(!roots.imported_login_expired(Provider::Claude, &local));
            let private_target = if provider == Provider::Claude {
                roots.claude_config.join(".credentials.json")
            } else {
                target.clone()
            };
            crate::config::private_write(&private_target, b"private-refreshed-login")?;
            assert!(!roots.prepare_login(provider, &local, &host)?);
            assert_eq!(std::fs::read(&private_target)?, b"private-refreshed-login");
            assert_eq!(
                roots.authentication_source(provider, &local),
                AuthenticationSource::PrivateLogin
            );
            std::fs::remove_file(&private_target)?;
            let name = if provider == Provider::Codex {
                "CODEX_API_KEY"
            } else {
                "CLAUDE_CODE_OAUTH_TOKEN"
            };
            local.env.apply(
                provider,
                BTreeMap::from([(name.into(), Some("explicit-private".into()))]),
            )?;
            assert!(!roots.prepare_login(provider, &local, &host)?);
            assert_eq!(
                roots.authentication_source(provider, &local),
                AuthenticationSource::Environment
            );
            local
                .env
                .apply(provider, BTreeMap::from([(name.into(), None)]))?;
            crate::config::private_write(&source, b"synthetic-secret-invalid-json")?;
            let error = roots.prepare_login(provider, &local, &host).unwrap_err();
            assert!(error.is::<crate::discovery::LocalSourceError>());
            assert!(!format!("{error:#}").contains("synthetic-secret"));
            assert!(!private_target.exists());
        }
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

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
            ProviderProbeReadiness::NeedsSetup {
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
        assert_eq!(probe.readiness("model"), ProviderProbeReadiness::Ready);
        assert_eq!(probe.readiness(""), ProviderProbeReadiness::Ready);
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
            ProviderProbeReadiness::NeedsSetup {
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
                external_auth: None,
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
        let mut env = ProviderEnv::default();
        env.apply(
            Provider::Codex,
            BTreeMap::from([("CPA_API_KEY".into(), Some("fixture-codex-value".into()))]),
        )?;
        env.apply(
            Provider::Cursor,
            BTreeMap::from([(
                "FIXTURE_CURSOR_KEY".into(),
                Some("fixture-cursor-value".into()),
            )]),
        )?;
        for (provider, name) in [
            (Provider::Codex, "VIBEFACE_DATA_DIR"),
            (Provider::Cursor, "CURSOR_CONFIG_DIR"),
            (Provider::Cursor, "CURSOR_DATA_DIR"),
            (Provider::Cursor, "CURSOR_API_KEY"),
            (Provider::Cursor, "CURSOR_AUTH_TOKEN"),
            (Provider::Cursor, "AGENT_CLI_CREDENTIAL_STORE"),
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
        let mut configuration = crate::config::Config::local("127.0.0.1:0".parse()?, vec![], false);
        configuration.provider_env = env;
        configuration.save(&dir)?;
        vibeface_bridge::platform::privatefs::ensure_private(
            &dir.join("config.json"),
            "the saved private configuration",
        )?;
        let loaded = crate::config::Config::load(&dir)?.provider_env;
        assert!(!loaded.get(Provider::Cursor).contains_key("CPA_API_KEY"));
        assert!(
            !loaded
                .get(Provider::Codex)
                .contains_key("FIXTURE_CURSOR_KEY")
        );
        assert_eq!(
            loaded.get(Provider::Cursor)["FIXTURE_CURSOR_KEY"],
            "fixture-cursor-value"
        );
        let mut removed = loaded.clone();
        removed.apply(
            Provider::Codex,
            BTreeMap::from([("CPA_API_KEY".into(), None)]),
        )?;
        assert!(removed.get(Provider::Codex).is_empty());
        let mut historical = serde_json::to_value(&configuration)?;
        for name in CURSOR_AUTH_OVERRIDES {
            historical["provider_env"]["cursor"][name] = json!("synthetic-unused-auth");
        }
        crate::config::private_write(&dir.join("config.json"), &serde_json::to_vec(&historical)?)?;
        let loaded = crate::config::Config::load(&dir)?.provider_env;
        let local = Local {
            switches: Providers::default(),
            bins: BTreeMap::new(),
            env: loaded,
        };
        assert_eq!(
            ProviderRoots::new(&dir).authentication_source(Provider::Cursor, &local),
            AuthenticationSource::LocalCliLogin
        );
        for name in CURSOR_AUTH_OVERRIDES {
            assert!(!local.env.get(Provider::Cursor).contains_key(name));
            assert!(!local.launch_env(Provider::Cursor).contains_key(name));
        }
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

    #[cfg(unix)]
    #[test]
    fn a_global_npm_wrapper_launches_with_node_from_another_directory() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("separate-node-{}", uuid::Uuid::new_v4()));
        let wrapper = dir.join(".npm-global/bin/claude");
        let node = dir.join(".local/bin/node");
        for (path, source) in [
            (&wrapper, "#!/usr/bin/env node\n"),
            (&node, "#!/bin/sh\nprintf 'external-node:%s\\n' \"$1\"\n"),
        ] {
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(path, source)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        let host = crate::discovery::Host {
            home: Some(dir.clone()),
            path: Some("/usr/bin:/bin".into()),
            ..Default::default()
        };
        let mut local = Local {
            switches: Providers::default(),
            bins: BTreeMap::from([(Provider::Claude, wrapper.clone())]),
            env: ProviderEnv::default(),
        };
        let saved = dir.join("empty").display().to_string();
        local.env.apply(
            Provider::Claude,
            BTreeMap::from([("PATH".into(), Some(saved.clone()))]),
        )?;
        let launch = local.launch_env_on(Provider::Claude, &host);
        let output = std::process::Command::new(&wrapper)
            .env_clear()
            .envs(&launch)
            .output()?;
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout)?,
            format!("external-node:{}\n", wrapper.display())
        );
        assert_eq!(
            local.env.get(Provider::Claude)["PATH"],
            saved,
            "saved PATH is preserved"
        );
        assert_eq!(
            std::env::split_paths(&launch["PATH"]).next(),
            node.parent().map(Path::to_path_buf)
        );
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn selected_npm_wrappers_launch_with_their_sibling_node() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("provider-node-{}", uuid::Uuid::new_v4()));
        let bin = dir.join("versions/node/v20.10.0/bin");
        let module = bin.join("../lib/node_modules/@openai/codex/bin/codex.js");
        std::fs::create_dir_all(module.parent().unwrap())?;
        std::fs::create_dir_all(&bin)?;
        std::fs::write(&module, "#!/usr/bin/env node\n")?;
        std::fs::set_permissions(&module, std::fs::Permissions::from_mode(0o700))?;
        std::os::unix::fs::symlink(&module, bin.join("codex"))?;
        std::fs::write(
            bin.join("node"),
            "#!/bin/sh\nprintf 'fixture-node:%s\\n' \"$1\"\n",
        )?;
        std::fs::set_permissions(bin.join("node"), std::fs::Permissions::from_mode(0o700))?;
        let mut local = Local {
            switches: Providers::default(),
            bins: BTreeMap::from([(Provider::Codex, bin.join("codex"))]),
            env: ProviderEnv::default(),
        };
        local.switches.execution.insert(Provider::Codex);
        local.env.apply(
            Provider::Codex,
            BTreeMap::from([
                (
                    "PATH".into(),
                    Some(dir.join("empty").to_string_lossy().into_owned()),
                ),
                ("FIXTURE_API_KEY".into(), Some("fixture-auth".into())),
            ]),
        )?;
        let CodexTarget::Spawn(launch) = local.codex_target(false) else {
            panic!("the selected Codex executes work");
        };
        let output = std::process::Command::new(&launch.bin)
            .arg("--version")
            .env_clear()
            .envs(&launch.env)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout)?,
            format!("fixture-node:{}\n", bin.join("codex").display())
        );
        assert_eq!(launch.env["FIXTURE_API_KEY"], "fixture-auth");
        assert_eq!(
            local.env.get(Provider::Codex)["PATH"],
            dir.join("empty").to_string_lossy()
        );
        assert_eq!(local.launch_env(Provider::Codex), launch.env);
        assert!(local.launch_env(Provider::Cursor).is_empty());
        for provider in Provider::ALL.into_iter().filter(|p| *p != Provider::Codex) {
            let program = bin.join(provider.default_bin());
            std::os::unix::fs::symlink(&module, &program)?;
            local.bins.insert(provider, program.clone());
            let output = std::process::Command::new(&program)
                .env_clear()
                .envs(local.launch_env(provider))
                .output()?;
            assert!(output.status.success());
            assert_eq!(
                String::from_utf8(output.stdout)?,
                format!("fixture-node:{}\n", program.display())
            );
            assert!(!local.env.get(provider).contains_key("PATH"));
        }
        local.bins.remove(&Provider::Codex);
        local.env.apply(
            Provider::Codex,
            BTreeMap::from([(
                "PATH".into(),
                Some(format!(
                    "{}:{}:{}",
                    dir.join("empty").display(),
                    bin.display(),
                    bin.display()
                )),
            )]),
        )?;
        let from_path = local.launch_env(Provider::Codex);
        assert_eq!(
            std::env::split_paths(&from_path["PATH"]).collect::<Vec<_>>(),
            vec![bin.clone(), dir.join("empty")]
        );
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn private_codex_config_projects_selected_metadata_and_never_overwrites() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("vibeface-codex-config-{}", uuid::Uuid::new_v4()));
        let source = dir.join("global");
        let fallback = dir.join("home/.codex");
        crate::config::private_dir(&source)?;
        crate::config::private_dir(&fallback)?;
        std::fs::write(fallback.join("config.toml"), "model='wrong-source'")?;
        std::fs::write(
            source.join("config.toml"),
            r#"
model = "unused-model"
model_provider = "unused"
model_reasoning_effort = "high"
profile = "selected"
experimental_bearer_token = "SYNTHETIC_INLINE_SECRET"
[profiles.selected]
model = "selected-model"
model_provider = "active"
sandbox_mode = "danger-full-access"
[model_providers.active]
name = "Fixture provider"
base_url = "https://api.example.test/v1"
wire_api = "responses"
env_key = "FIXTURE_API_KEY"
env_http_headers = { "X-Fixture" = "FIXTURE_HEADER_TOKEN" }
requires_openai_auth = false
http_headers = { "Authorization" = "SYNTHETIC_INLINE_SECRET" }
experimental_bearer_token = "SYNTHETIC_INLINE_SECRET"
[model_providers.unused]
env_key = "not-an-environment-name"
base_url = "https://user:SYNTHETIC_INLINE_SECRET@example.test/?token=secret"
[mcp_servers.fixture]
command = "do-not-copy"
[hooks]
command = "do-not-copy"
[projects."/private/project"]
trust_level = "trusted"
"#,
        )?;
        std::fs::write(source.join("auth.json"), "SYNTHETIC_AUTH_FILE")?;
        std::fs::create_dir_all(source.join("sessions"))?;
        let host = crate::discovery::Host {
            codex_home: Some(source.clone()),
            home: Some(dir.join("home")),
            ..Default::default()
        };
        let roots = ProviderRoots::new(&dir.join("node"));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let created = std::thread::scope(|scope| {
            (0..8)
                .map(|_| {
                    let barrier = barrier.clone();
                    let roots = &roots;
                    let host = &host;
                    scope.spawn(move || {
                        barrier.wait();
                        match roots.prepare_codex_config(host)? {
                            Some(projected) => roots.publish_codex_config(&projected),
                            None => Ok(false),
                        }
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|task| task.join().unwrap())
                .collect::<Result<Vec<_>>>()
        })?;
        assert_eq!(created.into_iter().filter(|created| *created).count(), 1);
        let target = roots.codex_home.join("config.toml");
        let bytes = std::fs::read(&target)?;
        let projected: toml::Value = toml::from_str(std::str::from_utf8(&bytes)?)?;
        let expected: toml::Value = toml::from_str(
            r#"
model = "selected-model"
model_provider = "active"
model_reasoning_effort = "high"
[model_providers.active]
name = "Fixture provider"
base_url = "https://api.example.test/v1"
wire_api = "responses"
env_key = "FIXTURE_API_KEY"
env_http_headers = { "X-Fixture" = "FIXTURE_HEADER_TOKEN" }
requires_openai_auth = false
"#,
        )?;
        assert_eq!(projected, expected);
        assert_eq!(
            std::fs::metadata(&target)?.permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&roots.codex_home)?.permissions().mode() & 0o777,
            0o700
        );
        assert!(!roots.codex_home.join("auth.json").exists());
        assert!(!roots.codex_sessions.exists());
        let bin = dir.join("fake-codex");
        std::fs::write(&bin, "#!/bin/sh\n/bin/cat \"$CODEX_HOME/config.toml\"\n")?;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700))?;
        let mut local = Local {
            switches: Providers::default(),
            bins: [(Provider::Codex, bin)].into_iter().collect(),
            env: ProviderEnv::default(),
        };
        local.switches.execution.insert(Provider::Codex);
        roots.apply(&mut local)?;
        let CodexTarget::Spawn(launch) = local.codex_target(false) else {
            unreachable!()
        };
        assert_eq!(
            Provider::Codex.key_variables(&launch.env)?,
            vec!["FIXTURE_API_KEY", "FIXTURE_HEADER_TOKEN"]
        );
        let output = std::process::Command::new(launch.bin)
            .env_clear()
            .envs(launch.env)
            .output()?;
        assert!(output.status.success());
        assert_eq!(output.stdout, bytes);
        std::fs::write(
            source.join("config.toml"),
            "invalid = [ SYNTHETIC_INLINE_SECRET",
        )?;
        assert!(roots.prepare_codex_config(&host)?.is_none());
        assert_eq!(std::fs::read(&target)?, bytes);
        let fallback_roots = ProviderRoots::new(&dir.join("fallback-node"));
        let projected = fallback_roots
            .prepare_codex_config(&crate::discovery::Host {
                home: host.home.clone(),
                ..Default::default()
            })?
            .context("safe fallback configuration")?;
        assert!(!fallback_roots.codex_home.join("config.toml").exists());
        assert!(fallback_roots.publish_codex_config(&projected)?);
        assert!(!fallback_roots.publish_codex_config(&projected)?);
        assert!(
            std::fs::read_to_string(fallback_roots.codex_home.join("config.toml"))?
                .contains("wrong-source")
        );
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn invalid_global_codex_config_is_read_only_by_explicit_preparation() -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("vibeface-codex-enable-{}", uuid::Uuid::new_v4()));
        let source = dir.join("global");
        crate::config::private_dir(&source)?;
        let host = crate::discovery::Host {
            codex_home: Some(source.clone()),
            ..Default::default()
        };
        for (index, config) in ["model='SYNTHETIC_INLINE_SECRET",
            "model_provider='active'\n[model_providers.active]\nbase_url='https://api.example.test/?token=SYNTHETIC_INLINE_SECRET'"].into_iter().enumerate() {
            std::fs::write(source.join("config.toml"), config)?;
            let roots = ProviderRoots::new(&dir.join(index.to_string()));
            let mut local = Local { switches: Providers::default(), bins: BTreeMap::new(), env: ProviderEnv::default() };
            local.switches.execution.insert(Provider::Codex);
            roots.apply(&mut local)?;
            assert!(!roots.codex_home.join("config.toml").exists());
            let error = roots.prepare_codex_config(&host).unwrap_err();
            assert!(!format!("{error:#}").contains("SYNTHETIC_INLINE_SECRET"));
            assert!(!roots.codex_home.join("config.toml").exists());
        }
        std::fs::remove_dir_all(dir)?;
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
    fn runtime_root_environment_can_be_saved_and_loaded_without_losing_auth() -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("vibeface-provider-roots-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let roots = ProviderRoots::new(&dir);
        let mut local = Local {
            switches: Providers::default(),
            bins: BTreeMap::new(),
            env: ProviderEnv::default(),
        };
        local.env.apply(
            Provider::Opencode,
            BTreeMap::from([("ANTHROPIC_API_KEY".into(), Some("fixture-auth".into()))]),
        )?;
        roots.apply(&mut local)?;
        let mut configuration = crate::config::Config::local("127.0.0.1:0".parse()?, vec![], false);
        configuration.provider_env = local.env;
        configuration.save(&dir)?;
        let loaded = crate::config::Config::load(&dir)?.provider_env;
        for provider in Provider::ALL {
            for (name, value) in roots.env(provider) {
                assert_eq!(loaded.get(provider).get(&name), Some(&value));
            }
        }
        assert_eq!(
            loaded
                .get(Provider::Opencode)
                .get("ANTHROPIC_API_KEY")
                .map(String::as_str),
            Some("fixture-auth")
        );
        assert!(!loaded.get(Provider::Pi).contains_key("ANTHROPIC_API_KEY"));
        std::fs::remove_dir_all(dir)?;
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
    fn provider_descriptors_cover_every_provider() {
        for provider in Provider::ALL {
            let descriptor = ProviderDescriptor::for_provider(provider);
            assert_eq!(descriptor.provider, provider);
            if provider == Provider::Cursor {
                assert_eq!(descriptor.model_catalog, CapabilitySupport::Supported);
            }
            assert_eq!(descriptor.prompt_mode, provider.prompt_mode());
            assert_eq!(descriptor.steer, provider.supports_steer());
            assert!(descriptor.workspace_required);
            assert_eq!(
                descriptor.stable_prompt,
                match provider {
                    Provider::Codex => StablePromptPlacement::ThreadInstructions,
                    Provider::Cursor => StablePromptPlacement::TurnInstructions,
                    Provider::Claude | Provider::Pi => StablePromptPlacement::SessionInstructions,
                    Provider::Opencode => StablePromptPlacement::UserTurnPrefix,
                }
            );
            assert_eq!(
                descriptor.dynamic_context,
                DynamicContextPlacement::UserTurn
            );
            assert_eq!(
                descriptor.compact_or_handoff().unwrap(),
                CompactionMode::Handoff
            );
            let summary = provider.capabilities();
            assert_eq!(summary, serde_json::to_value(&descriptor).unwrap());
            if provider == Provider::Cursor {
                assert_eq!(summary["cancel_confirmation_evidence"], "provider_terminal");
            }
            assert!(summary.get("observed").is_none());
            assert!(summary.get("env").is_none());
        }
    }

    #[test]
    fn readiness_requires_observed_authentication_and_model_evidence() {
        let handshake = ProviderObservation::new(
            ObservationStatus::Confirmed,
            ReadinessEvidence::AppServerHandshake,
        );
        let observed = ProviderReadiness::from_prerequisites(false, handshake);
        assert_eq!(observed.installed.status, ObservationStatus::Rejected);
        assert_eq!(observed.launchable.status, ObservationStatus::Confirmed);
        assert_eq!(observed.authenticated.status, ObservationStatus::Unknown);
        assert_eq!(observed.model_known.status, ObservationStatus::Unknown);
        assert_eq!(observed.ready.status, ObservationStatus::Unknown);
        let prerequisites = ProviderObservation::new(
            ObservationStatus::Unknown,
            ReadinessEvidence::LaunchPrerequisites,
        );
        let present = ProviderReadiness::from_prerequisites(true, prerequisites);
        assert_eq!(present.installed.status, ObservationStatus::Confirmed);
        assert_eq!(present.launchable.status, ObservationStatus::Unknown);
        assert_eq!(present.ready.status, ObservationStatus::Unknown);
        let missing = ProviderReadiness::from_prerequisites(
            false,
            ProviderObservation::new(
                ObservationStatus::Rejected,
                ReadinessEvidence::PrerequisitesRejected,
            ),
        );
        assert_eq!(missing.ready.status, ObservationStatus::Rejected);
        assert_eq!(missing.authenticated.status, ObservationStatus::Unknown);

        let mut probe = ProviderProbe {
            provider: Provider::Cursor,
            config_id: "fixture".into(),
            checked_at_ms: Some(1),
            launch: LaunchCheck::Launchable,
            authentication: Authentication::Authenticated,
            catalog: ModelCatalog::Available {
                models: vec![ModelChoice {
                    id: "fixture-model".into(),
                    name: "Fixture".into(),
                    default: false,
                    effort: EffortOptions::Unknown,
                }],
                coverage: CatalogCoverage::Partial,
            },
            version: None,
        };
        let observed = present.clone().with_probe(&probe, "fixture-model");
        assert_eq!(observed.launchable.status, ObservationStatus::Confirmed);
        assert_eq!(observed.authenticated.status, ObservationStatus::Confirmed);
        assert_eq!(observed.model_known.status, ObservationStatus::Confirmed);
        assert_eq!(observed.ready.status, ObservationStatus::Confirmed);
        assert_eq!(observed.ready.evidence, ReadinessEvidence::ProviderProbe);
        let unlisted = present.clone().with_probe(&probe, "auto");
        assert_eq!(unlisted.model_known.status, ObservationStatus::Unknown);
        assert_eq!(unlisted.ready.status, ObservationStatus::Unknown);
        probe.catalog = ModelCatalog::default();
        let without_catalog = present.clone().with_probe(&probe, "fixture-model");
        assert_eq!(
            without_catalog.authenticated.status,
            ObservationStatus::Confirmed
        );
        assert_eq!(
            without_catalog.model_known.status,
            ObservationStatus::Unknown
        );
        assert_eq!(without_catalog.ready.status, ObservationStatus::Unknown);
        probe.authentication = Authentication::SignInRequired;
        let signed_out = present.with_probe(&probe, "fixture-model");
        assert_eq!(signed_out.authenticated.status, ObservationStatus::Rejected);
        assert_eq!(signed_out.ready.status, ObservationStatus::Rejected);
    }

    #[test]
    fn undeclared_steer_and_native_compaction_are_rejected() {
        for provider in Provider::ALL {
            assert_eq!(
                provider.descriptor().check_steer().is_ok(),
                provider.supports_steer()
            );
            let mut descriptor = provider.descriptor();
            for compaction in [
                CompactionMode::Native,
                CompactionMode::Request,
                CompactionMode::Unsupported,
            ] {
                descriptor.compaction = compaction;
                assert!(descriptor.compact_or_handoff().is_err());
            }
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
        assert_eq!(local.bin(Provider::Cursor), PathBuf::from("agent"));
        assert!(local.launches(Provider::Claude) && local.launches(Provider::Codex));
        Ok(())
    }

    #[test]
    fn every_provider_starts_with_dynamic_discovery_and_essential_reads() {
        let expected = BTreeSet::from([
            "tool_search",
            "tool_describe",
            "tool_call",
            "context",
            "read_skill",
            "call_lookup",
        ]);
        for provider in Provider::ALL {
            assert_eq!(
                ProviderDescriptor::for_provider(provider).tool_mode,
                ToolMode::DynamicCatalog
            );
            for kind in [
                TriggerKind::Human,
                TriggerKind::AgentTask,
                TriggerKind::Schedule,
            ] {
                let tools = provider.tools_for(kind);
                assert_eq!(
                    tools.keys().map(String::as_str).collect::<BTreeSet<_>>(),
                    expected
                );
            }
        }
    }

    #[test]
    fn discovered_tools_keep_the_turn_and_token_effect_boundary() -> Result<()> {
        use crate::tools::{Effect, Tool};
        for kind in [
            None,
            Some(TriggerKind::Human),
            Some(TriggerKind::AgentTask),
            Some(TriggerKind::Schedule),
        ] {
            let mut offset = 0;
            let mut names = BTreeSet::new();
            loop {
                let found = Tool::search(json!({"limit":30,"offset":offset}), kind)?;
                for row in found["tools"].as_array().unwrap() {
                    let name = row["name"].as_str().unwrap();
                    assert!(
                        names.insert(name.to_owned()),
                        "catalog pagination repeats {name}"
                    );
                    let tool = Tool::parse(name).unwrap();
                    assert!(!matches!(tool, Tool::Search | Tool::Describe | Tool::Call));
                    assert!(kind.map_or(tool.effect() == Effect::Read, |kind| {
                        tool.effect().offered(kind)
                    }));
                }
                if found["more"] == false {
                    assert!(found["next"].is_null());
                    break;
                }
                let next = found["next"].as_u64().context("next catalog page")?;
                assert!(next > offset);
                offset = next;
            }
            assert!(names.contains("workspace_state"));
            assert_eq!(names.contains("workspace_command"), kind.is_some());
            assert_eq!(
                Tool::describe(json!({"tool":"update_profile"}), kind).is_ok(),
                kind == Some(TriggerKind::Human)
            );
        }
        let dispatch = Tool::search(json!({"query":"DISPATCH_WORK"}), Some(TriggerKind::Human))?;
        assert_eq!(dispatch["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(dispatch["tools"][0]["name"], "dispatch_work");
        let described = Tool::describe(json!({"tool":"dispatch_work"}), Some(TriggerKind::Human))?;
        assert_eq!(described["effect"], "work");
        assert_eq!(described["requires_call_ref"], true);
        assert_eq!(
            described["input_schema"],
            crate::scheduler::tools(TriggerKind::Human)["dispatch_work"].input_schema
        );
        assert!(Tool::search(json!({"limit":0}), Some(TriggerKind::Human)).is_err());
        assert!(Tool::describe(json!({"tool":"nodes","unknown":true}), None).is_err());
        Ok(())
    }

    #[test]
    fn tool_calls_resolve_the_actual_effect_and_cannot_wrap_internal_or_discovery_tools()
    -> Result<()> {
        use crate::tools::{Effect, Tool};
        let (target, args) = Tool::resolve_call(
            "tool_call",
            json!({
                "tool":"update_profile","arguments":{"name":"new name"},"call_ref":"rename-once",
            }),
        )?;
        assert_eq!(target, Tool::UpdateProfile);
        assert_eq!(target.effect(), Effect::Settings);
        assert!(!target.effect().offered(TriggerKind::Schedule));
        assert_eq!(args, json!({"name":"new name","call_ref":"rename-once"}));
        let (_, retry) = Tool::resolve_call(
            "tool_call",
            json!({
                "tool":"update_profile","arguments":args,"call_ref":"rename-once",
            }),
        )?;
        assert_eq!(retry["call_ref"], "rename-once");
        assert!(
            Tool::resolve_call(
                "tool_call",
                json!({
                    "tool":"update_profile","arguments":retry,"call_ref":"another-intent",
                })
            )
            .is_err()
        );
        assert!(
            Tool::resolve_call("tool_call", json!({"tool":"update_profile","arguments":{}}))
                .is_err()
        );
        for tool in [
            "track_work",
            "bind_work",
            "wait_work",
            "tool_search",
            "tool_describe",
            "tool_call",
        ] {
            assert!(Tool::resolve_call("tool_call", json!({"tool":tool,"arguments":{}})).is_err());
            assert!(Tool::describe(json!({"tool":tool}), Some(TriggerKind::Human)).is_err());
        }
        assert_eq!(
            Tool::resolve_call("track_work", json!({}))?.0,
            Tool::TrackWork
        );
        assert!(Tool::resolve_call("tool_call", json!({"tool":"nodes","arguments":[]})).is_err());
        Ok(())
    }
}
