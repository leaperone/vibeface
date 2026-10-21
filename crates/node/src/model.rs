use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type NetworkId = String;
pub type DeviceId = String;
pub type EventHash = String;
pub type ScopeId = String;
/// `{author: max_seq}`: the exact set of events a store holds, because every stored event's
/// `prev` and `deps` are stored too.
pub type VersionVector = BTreeMap<DeviceId, u64>;
pub const WIRE_VERSION: u32 = 1;
pub const MAX_EVENT_BYTES: usize = 1_048_576;
pub const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct BlobRef {
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// How a coordination turn ended. Partial text of stopped or interrupted turns is kept.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReplyStatus {
    Completed,
    Stopped,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// `codex`: a Codex turn this device's coordination Agent sent and the Node watches.
/// `task`: work handed to a device's coordination Agent as a task turn.
pub enum WorkKind {
    Codex,
    Task,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkPhase {
    Started,
    Running,
    Interrupting,
    Finishing,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkOutcome {
    Completed,
    Failed,
    Interrupted,
    /// Never started; retrying with the same `work_id` is safe.
    Rejected,
    /// The real result cannot be proven; only the user or a later probe settles it.
    Unverified,
    /// Its target was removed before reporting a result; written by the conversation's owner.
    Abandoned,
}

/// How soon the user should look at a triaged session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Attention,
    Urgent,
}

/// Scope of the questions about sessions no assistant owns; they show only on the pending page.
pub const SESSIONS_SCOPE: &str = "sessions";

/// Scope of the user's choice of primary assistant, the one that judges what comes in without an
/// owner. Its latest event by rank stands; nothing projects it.
pub const ASSISTANTS_SCOPE: &str = "assistants";

/// The device, project and session a question is about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    pub node: DeviceId,
    /// `codex` or `claude`.
    pub agent: String,
    /// A Codex thread ID or a Claude Code session ID.
    pub session_id: String,
    pub cwd: String,
    /// One line: what the session last said.
    pub summary: String,
    /// The App Server request `node` holds until the question is answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub label: String,
    /// Session questions only: choosing it attributes the session to this assistant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<ScopeId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct OpenWork {
    pub work_id: String,
    pub summary: String,
}

/// The user's device signature over an event body the connected Node writes on its behalf.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Attestation {
    pub device_id: DeviceId,
    pub signature: String,
}

pub const MAX_INSTRUCTIONS_BYTES: usize = 8192;
pub const MAX_SKILL_NAME_CHARS: usize = 64;
pub const MAX_SKILL_DESCRIPTION_BYTES: usize = 512;
pub const MAX_SKILL_BODY_BYTES: usize = 16 * 1024;
pub const MAX_ROUTINE_NAME_CHARS: usize = 120;
pub const MAX_ROUTINE_PROMPT_BYTES: usize = 8 * 1024;
/// Every 5 minutes up to every 4 weeks; a day is 1440 and a week 10080.
pub const ROUTINE_MINUTES: std::ops::RangeInclusive<u32> = 5..=10080 * 4;

/// One setting's value before and after a change; `before` is what the writer saw.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub before: String,
    pub after: String,
}

/// A saved workflow in the library every assistant of the network shares.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// A turn its assistant runs at `first_run_at_ms` and then every `every_minutes`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Routine {
    pub name: String,
    pub prompt: String,
    pub first_run_at_ms: i64,
    pub every_minutes: u32,
    pub enabled: bool,
}

/// What a chat turn changed about its assistant. A `None` after removes the skill or routine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AssistantChange {
    Profile {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<Edit>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Edit>,
    },
    Skill {
        skill_id: String,
        before: Option<Skill>,
        after: Option<Skill>,
    },
    Routine {
        routine_id: String,
        before: Option<Routine>,
        after: Option<Routine>,
    },
}

fn validate_line(value: &str, what: &str, max_chars: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.trim().is_empty()
            && value.chars().count() <= max_chars
            && !value.chars().any(char::is_control),
        "{what} must be one line of 1-{max_chars} characters"
    );
    Ok(())
}

pub fn validate_bot_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.trim().is_empty() && name.len() <= 512 && !name.chars().any(char::is_control),
        "invalid Bot name"
    );
    Ok(())
}

pub fn validate_instructions(instructions: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        instructions.len() <= MAX_INSTRUCTIONS_BYTES,
        "Bot instructions must be at most {MAX_INSTRUCTIONS_BYTES} bytes"
    );
    Ok(())
}

impl Skill {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_line(&self.name, "a skill name", MAX_SKILL_NAME_CHARS)?;
        anyhow::ensure!(
            self.description.len() <= MAX_SKILL_DESCRIPTION_BYTES,
            "a skill description must be at most {MAX_SKILL_DESCRIPTION_BYTES} bytes"
        );
        anyhow::ensure!(
            !self.body.trim().is_empty() && self.body.len() <= MAX_SKILL_BODY_BYTES,
            "a skill body must be 1-{MAX_SKILL_BODY_BYTES} bytes"
        );
        Ok(())
    }
}

impl Routine {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_line(&self.name, "a routine name", MAX_ROUTINE_NAME_CHARS)?;
        anyhow::ensure!(
            !self.prompt.trim().is_empty() && self.prompt.len() <= MAX_ROUTINE_PROMPT_BYTES,
            "a routine prompt must be 1-{MAX_ROUTINE_PROMPT_BYTES} bytes"
        );
        anyhow::ensure!(
            self.first_run_at_ms >= 0,
            "first_run_at_ms must be a Unix time in milliseconds"
        );
        anyhow::ensure!(
            ROUTINE_MINUTES.contains(&self.every_minutes),
            "every_minutes must be between {} and {}",
            ROUTINE_MINUTES.start(),
            ROUTINE_MINUTES.end()
        );
        Ok(())
    }
}

impl AssistantChange {
    /// Every change sets something new; a removal needs something to remove.
    pub fn validate(&self) -> anyhow::Result<()> {
        let id = |value: &str| vibeface_protocol::validate_id(value).map_err(anyhow::Error::msg);
        match self {
            Self::Profile { name, instructions } => {
                anyhow::ensure!(
                    name.is_some() || instructions.is_some(),
                    "a profile change names a field"
                );
                if let Some(name) = name {
                    validate_bot_name(&name.after)?;
                    anyhow::ensure!(name.before != name.after, "the name is unchanged");
                }
                if let Some(instructions) = instructions {
                    validate_instructions(&instructions.after)?;
                    anyhow::ensure!(
                        instructions.before != instructions.after,
                        "the instructions are unchanged"
                    );
                }
            }
            Self::Skill {
                skill_id,
                before,
                after,
            } => {
                id(skill_id)?;
                anyhow::ensure!(
                    (before.is_some() || after.is_some()) && before != after,
                    "the skill is unchanged"
                );
                after.as_ref().map(Skill::validate).transpose()?;
            }
            Self::Routine {
                routine_id,
                before,
                after,
            } => {
                id(routine_id)?;
                anyhow::ensure!(
                    (before.is_some() || after.is_some()) && before != after,
                    "the routine is unchanged"
                );
                after.as_ref().map(Routine::validate).transpose()?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventBody {
    ConversationCreated {
        title: String,
    },
    /// The user's choice of scheduling Node; the highest epoch wins, ties pick the smaller hash.
    ConversationOwner {
        scheduler_node: DeviceId,
        epoch: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    BotCreated {
        bot_id: String,
        name: String,
        instructions: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<BotProject>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    /// `report_results` and `attachments` belong to user messages; `in_reply_to` and `status` to
    /// assistant replies. `attachments` repeats the event's blobs so the user's signature covers them.
    MessagePosted {
        message_id: String,
        role: Role,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<BlobRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report_results: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        in_reply_to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<ReplyStatus>,
        /// A user message answering this question; its `message_id` is `a-<question_id>`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answers: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    /// Written by the target Node when it accepts the work, so no progress precedes it.
    WorkDispatched {
        work_id: String,
        trigger: String,
        dispatcher: DeviceId,
        call_ref: String,
        kind: WorkKind,
        cwd: String,
        /// A short line for the user; `prompt` is what the work gets.
        title: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        continues: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
        /// An existing App Server thread the prompt went into.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<String>,
    },
    WorkProgress {
        work_id: String,
        phase: WorkPhase,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    /// Written by the target. The user settles an `unverified` result through any entry Node,
    /// signed; the owner records `abandoned` after the target is removed.
    WorkFinished {
        work_id: String,
        outcome: WorkOutcome,
        result_text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    NotificationAcked {
        notification_id: String,
        client_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    /// In the conversation of the assistant that owns the session, or in `SESSIONS_SCOPE` for a
    /// session no assistant owns. Its one answer is the user message `a-<question_id>`.
    QuestionAsked {
        question_id: String,
        severity: Severity,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        choices: Vec<Choice>,
        about: Subject,
    },
    /// In the chosen assistant's conversation: that assistant owns the session from now on.
    SessionAttributed {
        node: DeviceId,
        agent: String,
        session_id: String,
        cwd: String,
        summary: String,
        /// The session question that led to it.
        question_id: String,
    },
    /// In `ASSISTANTS_SCOPE`: the user makes this conversation's assistant the primary one.
    PrimaryAssistantSet {
        conversation_id: ScopeId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    /// Written by the owner in a turn the user's message `cause` started; that message is one
    /// of its parents. `change_id` derives from the turn and the tool's `call_ref`.
    AssistantChanged {
        change_id: String,
        cause: String,
        change: AssistantChange,
    },
    /// Written only by the owner in a read-only closing turn; covered events stay stored.
    ContextCheckpoint {
        covers: VersionVector,
        summary: String,
        standing_instructions: Vec<String>,
        open_work: Vec<OpenWork>,
        decisions: Vec<String>,
    },
}

impl EventBody {
    /// Present on writes that need the user's own device signature; see `crypto::validate_event_shape`.
    pub fn attestation(&self) -> Option<&Attestation> {
        match self {
            Self::ConversationOwner { attestation, .. }
            | Self::BotCreated { attestation, .. }
            | Self::MessagePosted { attestation, .. }
            | Self::WorkFinished { attestation, .. }
            | Self::NotificationAcked { attestation, .. }
            | Self::PrimaryAssistantSet { attestation, .. } => attestation.as_ref(),
            _ => None,
        }
    }
    pub fn work_id(&self) -> Option<&str> {
        match self {
            Self::WorkDispatched { work_id, .. }
            | Self::WorkProgress { work_id, .. }
            | Self::WorkFinished { work_id, .. } => Some(work_id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub version: u32,
    pub network_id: NetworkId,
    pub author: DeviceId,
    pub seq: u64,
    pub prev: Option<EventHash>,
    pub deps: Vec<EventHash>,
    pub scope_id: ScopeId,
    pub created_at_ms: i64,
    pub body: EventBody,
    pub blobs: Vec<BlobRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SignedEvent {
    pub hash: EventHash,
    pub canonical_body: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HashedEvent {
    pub hash: EventHash,
    #[serde(flatten)]
    pub event: Event,
}

/// A conversation's events up to `basis`, in the deterministic linear order shared by prompts and UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    pub network_id: NetworkId,
    pub scope_id: ScopeId,
    pub basis: VersionVector,
    pub events: Vec<HashedEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BotProject {
    pub target_node: DeviceId,
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BotCreateSpec {
    pub bot_id: String,
    pub conversation_id: ScopeId,
    pub name: String,
    pub instructions: String,
    pub project: Option<BotProject>,
    pub attestation: Attestation,
    pub owner: FirstOwner,
}

/// The user's signed choice of a new conversation's first scheduling Node. Any entry can write
/// it, so a retry through another entry keeps the same owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FirstOwner {
    pub scheduler_node: DeviceId,
    pub attestation: Attestation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Bot {
    pub bot_id: String,
    pub conversation_id: ScopeId,
    pub name: String,
    pub instructions: String,
    pub project: Option<BotProject>,
    pub event_hash: EventHash,
}

/// The earliest `AssistantChanged` of one `change_id`; the chat shows it as a card.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangeRecord {
    pub change_id: String,
    pub cause: String,
    pub event_hash: EventHash,
    pub created_at_ms: i64,
    pub change: AssistantChange,
}

/// Output only: `flatten` cannot read back a `deny_unknown_fields` struct.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SkillEntry {
    pub skill_id: String,
    #[serde(flatten)]
    pub skill: Skill,
}

/// A routine that is not removed; `changed_at_ms` is when its current definition was written.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RoutineEntry {
    pub routine_id: String,
    pub conversation_id: ScopeId,
    #[serde(flatten)]
    pub routine: Routine,
    pub changed_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Conversation {
    pub conversation_id: ScopeId,
    pub title: String,
    pub owner: Option<DeviceId>,
    pub owner_epoch: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub message_id: String,
    pub role: Role,
    pub text: String,
    pub event_hash: EventHash,
    pub author: DeviceId,
    pub created_at_ms: i64,
    pub in_reply_to: Option<String>,
    pub status: Option<ReplyStatus>,
    pub report_results: Option<bool>,
    /// The question this user message answers.
    pub answers: Option<String>,
    pub blobs: Vec<BlobRef>,
}
/// Derived from the event that raised it; its ID is that event's hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Notification {
    pub notification_id: String,
    pub conversation_id: ScopeId,
    pub kind: String,
    pub text: String,
}
/// DAG projection of one `work_id`; the target Node's local state machine is the authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkProjection {
    pub work_id: String,
    pub conversation_id: ScopeId,
    pub target_node: DeviceId,
    pub trigger: String,
    pub dispatcher: DeviceId,
    pub kind: WorkKind,
    pub cwd: String,
    pub title: String,
    pub prompt: String,
    pub call_ref: String,
    pub continues: Option<String>,
    pub parent: Option<String>,
    /// The existing thread a send went into; `thread_id` is the thread the work runs in.
    pub thread: Option<String>,
    pub phase: Option<WorkPhase>,
    pub summary: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub outcome: Option<WorkOutcome>,
    pub result_text: String,
    pub failure_reason: Option<String>,
    pub state: WorkStatus,
}

/// Where a work stands, as every device reads it from the DAG.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkStatus {
    Accepted,
    Running,
    Interrupting,
    Finished,
    Unverified,
}

impl WorkStatus {
    pub fn of(kind: WorkKind, phase: Option<WorkPhase>, outcome: Option<WorkOutcome>) -> Self {
        match (outcome, phase) {
            (Some(WorkOutcome::Unverified), _) => Self::Unverified,
            (Some(_), _) => Self::Finished,
            // An agent task runs as its device's coordination turn, without Codex phases.
            (None, _) if kind == WorkKind::Task => Self::Running,
            (None, Some(WorkPhase::Interrupting)) => Self::Interrupting,
            (None, Some(_)) => Self::Running,
            (None, None) => Self::Accepted,
        }
    }
}

/// Per-event result of ingest; a batch never aborts on one bad event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum IngestStatus {
    Accepted,
    Duplicate,
    Pending,
    Conflict,
    Rejected(String),
}
