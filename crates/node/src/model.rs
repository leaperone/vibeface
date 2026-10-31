use crate::providers::Provider;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
pub use vibeface_bridge::failure::FailureKind;

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

pub const MAX_ATTACHMENT_NAME_BYTES: usize = 255;

/// A file the user attached to a message. The name belongs to the message, not the content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub hash: String,
    pub size: u64,
    pub name: String,
}

impl Attachment {
    pub fn blob(&self) -> BlobRef {
        BlobRef {
            hash: self.hash.clone(),
            size: self.size,
        }
    }
}

/// A file name as the web signs it: no path separators or control characters, at most
/// `MAX_ATTACHMENT_NAME_BYTES`, never empty.
pub fn valid_attachment_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_ATTACHMENT_NAME_BYTES
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    /// Another bot of this network wrote to this one; only `BotMessaged` makes it.
    Bot,
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

/// `Agent`: a turn on one of this device's providers that the Node watches, named by the
/// provider. `task`: work handed to a device's coordination Agent as a task turn.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(into = "String", try_from = "String")]
pub enum WorkKind {
    Agent(Provider),
    Task,
}

impl From<WorkKind> for String {
    fn from(kind: WorkKind) -> String {
        match kind {
            WorkKind::Agent(provider) => provider.name().into(),
            WorkKind::Task => "task".into(),
        }
    }
}

impl TryFrom<String> for WorkKind {
    type Error = serde_json::Error;
    fn try_from(name: String) -> Result<Self, Self::Error> {
        if name == "task" {
            return Ok(WorkKind::Task);
        }
        serde_json::from_value(serde_json::Value::String(name)).map(WorkKind::Agent)
    }
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
}

/// How soon the user should look at a triaged session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Attention,
    Urgent,
}

/// Why the device that asked a question closed it unanswered.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// The request it held is gone, so no answer can reach it.
    RequestLost,
}

/// Scope of the user's choice of primary bot, the one that judges what comes in without an
/// owner. Its latest event by rank stands; nothing projects it.
pub const BOTS_SCOPE: &str = "bots";

/// The device, project and session a question is about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    pub node: DeviceId,
    /// The provider running the session; `None` when the question is about `node`'s own
    /// settings, whose answer goes to `apply_provider_config`.
    pub agent: Option<crate::providers::Provider>,
    /// The provider's thread or session ID.
    pub session_id: String,
    pub cwd: String,
    /// One line: what the session said in the event this subject comes from. A session's
    /// attribution keeps the line from when it was claimed; later turns do not update it.
    pub summary: String,
    /// The request `node` holds until the question is answered: an App Server's, or one a
    /// session's hook, plugin or extension waits on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    Question,
    Approval,
    BrowserTakeover,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttentionRequest {
    pub kind: AttentionKind,
    pub device_id: Option<DeviceId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub label: String,
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
pub const MAX_SCHEDULE_NAME_CHARS: usize = 120;
pub const MAX_SCHEDULE_PROMPT_BYTES: usize = 8 * 1024;
/// Every 5 minutes up to every 4 weeks; a day is 1440 and a week 10080.
pub const SCHEDULE_MINUTES: std::ops::RangeInclusive<u32> = 5..=10080 * 4;
/// Repeating schedules and one-shot ones still ahead, per bot.
pub const MAX_ACTIVE_SCHEDULES: usize = 20;
/// A one-shot time is set at least this far ahead, and at most `ONCE_MAX_LEAD_MS`.
pub const ONCE_MIN_LEAD_MS: i64 = 60_000;
pub const ONCE_MAX_LEAD_MS: i64 = 366 * 24 * 60 * 60_000;
/// How many automatic wakes (reminders a bot sets itself, messages between bots)
/// may follow one message of the user.
pub const MAX_CHAIN_DEPTH: u8 = 4;
pub const MAX_MEMORY_CHARS: usize = 500;
pub const MAX_MEMORIES: usize = 50;
pub const MAX_BOT_MESSAGE_BYTES: usize = 4000;
pub const BOT_MESSAGES_PER_HOUR: usize = 10;
/// Bots one message of the user may have a bot create.
pub const BOTS_PER_TURN: usize = 3;
pub const MAX_MODEL_CHARS: usize = 256;
pub const MAX_BOT_SKILLS: usize = 200;

/// The shared skills a bot uses: `None` is every skill in the library, later ones included.
pub type SkillSet = Option<BTreeSet<String>>;

/// A bot's skill set before and after a change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillsEdit {
    pub before: SkillSet,
    pub after: SkillSet,
}

/// A face's outline; `packages/client/src/avatar/face.ts` draws it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outline {
    Round,
    Squircle,
    Drop,
    Bean,
    Cloud,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Eyes {
    Dot,
    Pill,
    Oval,
    Wide,
}

/// The OKLCH hues a face may take. 60 to 90 stays the waiting halo's amber.
pub const HUES: [u16; 12] = [20, 45, 100, 130, 155, 180, 205, 230, 255, 285, 315, 345];

/// One of `HUES`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "u16", into = "u16")]
pub struct Hue(u16);

impl TryFrom<u16> for Hue {
    type Error = String;
    fn try_from(value: u16) -> Result<Self, String> {
        if HUES.contains(&value) {
            Ok(Self(value))
        } else {
            Err(format!("hue is one of {HUES:?}"))
        }
    }
}

impl From<Hue> for u16 {
    fn from(hue: Hue) -> u16 {
        hue.0
    }
}

/// The face picked for a bot. Without one the face comes from the bot's ID, as do its eye
/// spacing and tilt either way.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Avatar {
    pub outline: Outline,
    pub eyes: Eyes,
    pub hue: Hue,
}

/// In `face.ts` order: the ID derivation indexes these.
const OUTLINES: [Outline; 5] = [
    Outline::Round,
    Outline::Squircle,
    Outline::Drop,
    Outline::Bean,
    Outline::Cloud,
];
const EYES: [Eyes; 4] = [Eyes::Dot, Eyes::Pill, Eyes::Oval, Eyes::Wide];

impl Avatar {
    /// The face `faceOf` in `face.ts` gives a bot that has none picked; the two must agree, as
    /// `packages/client/src/avatar/derived-faces.json` checks.
    pub fn derived(bot_id: &str) -> Self {
        let mut hash: u32 = 2_166_136_261;
        for c in bot_id.chars() {
            // `charCodeAt(0)` of each code point: the first UTF-16 unit.
            let unit = c.encode_utf16(&mut [0; 2])[0];
            hash = (hash ^ u32::from(unit)).wrapping_mul(16_777_619);
        }
        hash ^= hash >> 16;
        let more = scramble(hash, 7);
        Self {
            outline: OUTLINES[(hash >> 3) as usize % OUTLINES.len()],
            eyes: EYES[(more >> 4) as usize % EYES.len()],
            hue: Hue(HUES[hash as usize % HUES.len()]),
        }
    }
}

/// `unit` in `face.ts` before it divides by 2^32.
fn scramble(seed: u32, n: u32) -> u32 {
    let mut h = (seed ^ n.wrapping_mul(0x9e37_79b1)).wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^ (h >> 16)
}

/// The face a bot shows, and whether one was picked rather than given by its ID.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ShownAvatar {
    #[serde(flatten)]
    pub avatar: Avatar,
    pub picked: bool,
}

/// `None` is the face the bot's ID gives.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AvatarEdit {
    pub before: Option<Avatar>,
    pub after: Option<Avatar>,
}

/// One setting's value before and after a change; `before` is what the writer saw.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub before: String,
    pub after: String,
}

/// A bot's model on one provider, set from a device running that provider; an empty `after`
/// follows the device's default.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelEdit {
    pub provider: Provider,
    pub before: String,
    pub after: String,
}

/// A bot's effort override on one provider; `None` follows the device's setting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReasoningEffortEdit {
    pub provider: Provider,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// A bot's model per provider. The provider is device-local, so a device runs the entry of its
/// own provider or, without one, its default model.
pub type BotModels = BTreeMap<Provider, String>;

/// A saved workflow in the library every bot of the network shares.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// A turn its bot runs at `first_run_at_ms`, then every `every_minutes` when set: a routine
/// repeats, a reminder runs once.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub name: String,
    pub prompt: String,
    pub first_run_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_minutes: Option<u32>,
    pub enabled: bool,
    /// Automatic wakes since the user's last message: 0 for what the user asked for, one more
    /// than the setting turn for a reminder the bot set itself.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub depth: u8,
}

fn is_zero(value: &u8) -> bool {
    *value == 0
}

/// What the user's web client does to a schedule; it never creates or edits one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleAction {
    Pause,
    Resume,
    Delete,
}

/// The user's own signed message that started the turn a write stands on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Cause {
    pub conversation_id: ScopeId,
    pub message_id: String,
}

/// The bot that wrote to another one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Sender {
    pub conversation_id: ScopeId,
    pub name: String,
}

/// What a chat turn changed about its bot. A `None` after removes the skill, schedule or
/// note.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BotChange {
    Profile {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<Edit>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Edit>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelEdit>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<ReasoningEffortEdit>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skills: Option<SkillsEdit>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        avatar: Option<AvatarEdit>,
    },
    Skill {
        skill_id: String,
        before: Option<Skill>,
        after: Option<Skill>,
    },
    Schedule {
        schedule_id: String,
        before: Option<Schedule>,
        after: Option<Schedule>,
    },
    Memory {
        memory_id: String,
        before: Option<String>,
        after: Option<String>,
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

/// Empty means the device's default model.
pub fn validate_model(model: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        model.trim() == model
            && model.chars().count() <= MAX_MODEL_CHARS
            && !model.chars().any(char::is_control),
        "a model is one line of at most {MAX_MODEL_CHARS} characters, or empty for the device's default"
    );
    Ok(())
}

pub fn validate_skill_set(skills: &SkillSet) -> anyhow::Result<()> {
    if let Some(skills) = skills {
        anyhow::ensure!(
            skills.len() <= MAX_BOT_SKILLS,
            "a bot uses at most {MAX_BOT_SKILLS} skills"
        );
        for skill in skills {
            vibeface_protocol::validate_id(skill).map_err(anyhow::Error::msg)?;
        }
    }
    Ok(())
}

pub fn validate_instructions(instructions: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        instructions.len() <= MAX_INSTRUCTIONS_BYTES,
        "Bot instructions must be at most {MAX_INSTRUCTIONS_BYTES} bytes"
    );
    refuse_secret(instructions)
}

/// Bot settings sync to every device, so they never hold a credential.
fn refuse_secret(text: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !looks_secret(text),
        "this looks like a credential; keys, tokens and passwords are never kept in bot settings"
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
        refuse_secret(&self.description)?;
        refuse_secret(&self.body)
    }
}

impl Schedule {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_line(&self.name, "a schedule name", MAX_SCHEDULE_NAME_CHARS)?;
        anyhow::ensure!(
            !self.prompt.trim().is_empty() && self.prompt.len() <= MAX_SCHEDULE_PROMPT_BYTES,
            "a schedule prompt must be 1-{MAX_SCHEDULE_PROMPT_BYTES} bytes"
        );
        anyhow::ensure!(
            self.first_run_at_ms >= 0,
            "first_run_at_ms must be a Unix time in milliseconds"
        );
        if let Some(every) = self.every_minutes {
            anyhow::ensure!(
                SCHEDULE_MINUTES.contains(&every),
                "every_minutes must be between {} and {}",
                SCHEDULE_MINUTES.start(),
                SCHEDULE_MINUTES.end()
            );
        }
        anyhow::ensure!(
            self.depth <= MAX_CHAIN_DEPTH,
            "at most {MAX_CHAIN_DEPTH} automatic wakes may follow the user's message"
        );
        refuse_secret(&self.prompt)
    }
}

/// A note a bot keeps across turns: short, and never a credential.
pub fn validate_memory(text: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !text.trim().is_empty() && text.chars().count() <= MAX_MEMORY_CHARS,
        "a note must be 1-{MAX_MEMORY_CHARS} characters"
    );
    anyhow::ensure!(
        !looks_secret(text),
        "this looks like a credential; keys, tokens and passwords are never remembered"
    );
    Ok(())
}

/// Obvious credentials: private key blocks, bearer tokens, secret assignments, well-known key
/// prefixes and long unbroken runs that mix cases and digits as keys do.
pub fn looks_secret(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "-----begin",
        "bearer ",
        "password=",
        "password:",
        "passwd=",
        "secret=",
        "api_key=",
        "apikey=",
        "token=",
    ];
    const PREFIXES: &[&str] = &[
        "sk-",
        "sk_",
        "ghp_",
        "gho_",
        "ghs_",
        "ghu_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "xapp-",
        "akia",
        "aiza",
        "glpat-",
        "hf_",
        "npm_",
    ];
    if MARKERS
        .iter()
        .any(|marker| text.to_lowercase().contains(marker))
    {
        return true;
    }
    text.split(|c: char| c.is_whitespace() || "\"'`,;()[]{}<>".contains(c))
        .any(|word| {
            let lower = word.to_ascii_lowercase();
            (word.len() >= 20 && PREFIXES.iter().any(|prefix| lower.starts_with(prefix)))
                || (word.len() >= 32
                    && word
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_+/=".contains(c))
                    && word.chars().any(|c| c.is_ascii_digit())
                    && word.chars().any(|c| c.is_ascii_uppercase())
                    && word.chars().any(|c| c.is_ascii_lowercase()))
        })
}

impl BotChange {
    /// Every change sets something new; a removal needs something to remove.
    pub fn validate(&self) -> anyhow::Result<()> {
        let id = |value: &str| vibeface_protocol::validate_id(value).map_err(anyhow::Error::msg);
        match self {
            Self::Profile {
                name,
                instructions,
                model,
                reasoning_effort,
                skills,
                avatar,
            } => {
                anyhow::ensure!(
                    name.is_some()
                        || instructions.is_some()
                        || model.is_some()
                        || reasoning_effort.is_some()
                        || skills.is_some()
                        || avatar.is_some(),
                    "a profile change names a field"
                );
                if let Some(avatar) = avatar {
                    anyhow::ensure!(avatar.before != avatar.after, "the face is unchanged");
                }
                if let Some(model) = model {
                    validate_model(&model.after)?;
                    anyhow::ensure!(model.before != model.after, "the model is unchanged");
                }
                if let Some(edit) = reasoning_effort {
                    edit.provider
                        .validate_reasoning_effort(edit.before.as_deref())?;
                    edit.provider
                        .validate_reasoning_effort(edit.after.as_deref())?;
                    anyhow::ensure!(
                        edit.before != edit.after,
                        "the reasoning effort is unchanged"
                    );
                }
                if let Some(skills) = skills {
                    validate_skill_set(&skills.after)?;
                    anyhow::ensure!(skills.before != skills.after, "the skills are unchanged");
                }
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
            Self::Schedule {
                schedule_id,
                before,
                after,
            } => {
                id(schedule_id)?;
                anyhow::ensure!(
                    (before.is_some() || after.is_some()) && before != after,
                    "the schedule is unchanged"
                );
                after.as_ref().map(Schedule::validate).transpose()?;
            }
            Self::Memory {
                memory_id,
                before,
                after,
            } => {
                id(memory_id)?;
                anyhow::ensure!(
                    (before.is_some() || after.is_some()) && before != after,
                    "the note is unchanged"
                );
                after.as_deref().map(validate_memory).transpose()?;
            }
        }
        Ok(())
    }
}

/// The `client_id` of an ack that closes a failed work in 待处理; see `NotificationAcked`.
pub const INBOX_CLIENT: &str = "inbox";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventBody {
    ConversationCreated {
        title: String,
    },
    /// The user's choice of scheduling Node; the highest epoch wins, ties pick the smaller hash.
    /// A bot another bot creates gets its first owner through the user's message `cause`.
    ConversationOwner {
        scheduler_node: DeviceId,
        epoch: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<Cause>,
    },
    /// Signed by the user on the web, or written in a bot's turn the user's message `cause`
    /// started, with that message among its parents.
    BotCreated {
        bot_id: String,
        name: String,
        instructions: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<BotProject>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<Cause>,
    },
    /// `report_results` and `attachments` belong to user messages; `in_reply_to` and `status` to
    /// bot replies. `attachments` names the event's blobs so the user's signature covers them.
    MessagePosted {
        message_id: String,
        role: Role,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report_results: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        in_reply_to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<ReplyStatus>,
        /// Why a failed or interrupted reply ended; clients word it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<FailureKind>,
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
    /// signed; the owner records `unverified` after the target is removed.
    WorkFinished {
        work_id: String,
        outcome: WorkOutcome,
        result_text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure_reason: Option<String>,
        /// The kind of execution failure that ended it, when one did; clients word it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<FailureKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    /// A client read the notification. With `client_id` [`INBOX_CLIENT`], `notification_id`
    /// names a failed work or refused dispatch the user closed in 待处理 for every device.
    NotificationAcked {
        notification_id: String,
        client_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    /// In the conversation of the bot that owns the session. Its one answer is the user
    /// message `a-<question_id>`.
    QuestionAsked {
        question_id: String,
        attention: AttentionRequest,
        severity: Severity,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        choices: Vec<Choice>,
        about: Subject,
    },
    /// Written by the device that asked, in the question's conversation, once nothing can act
    /// on an answer. An answer that stands keeps the question answered, whichever came first.
    QuestionClosed {
        question_id: String,
        reason: CloseReason,
    },
    /// Written by the device running the session, in the conversation of the bot that owns
    /// it from now on: the primary bot when the session shows up without an owner, its bot
    /// deleted included, or the bot its owner hands it to. The latest by rank stands.
    SessionAttributed {
        node: DeviceId,
        agent: crate::providers::Provider,
        session_id: String,
        cwd: String,
        /// What the session last said when it first came to a bot; a hand-over keeps it.
        summary: String,
    },
    /// Written by the device running the session, in its owner's conversation: that bot stopped
    /// following it, and its events are dropped until it is handed to a bot again or that bot
    /// is deleted.
    SessionMuted {
        node: DeviceId,
        session_id: String,
    },
    /// In `BOTS_SCOPE`: the user makes this conversation's bot the primary one.
    PrimaryBotSet {
        conversation_id: ScopeId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    /// Written by the owner in a turn the user's message `cause` started; that message is one
    /// of its parents. A bot may change another bot, so the cause may sit in another
    /// conversation. Only a new one-shot schedule, a reminder the bot sets itself in
    /// another turn, has no `cause`. `change_id` derives from the turn and the tool's `call_ref`.
    BotChanged {
        change_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<Cause>,
        change: BotChange,
    },
    /// In the deleted bot's conversation: signed by the user on the web, or written in another
    /// bot's turn the user's message `cause` started, with that message among its parents.
    /// Nothing brings the bot back.
    BotDeleted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<Cause>,
    },
    /// In the bot's conversation: the user picks its face from the web, or `None` for the one
    /// its ID gives. It and a chat change of the face win by rank alike. `version` is the face's
    /// version the user saw, `None` before any change; the signature covers it, so a request
    /// replayed after the face moved on cannot be pointed at the new version.
    BotAvatarSet {
        version: Option<String>,
        avatar: Option<Avatar>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    /// In the bot's conversation: the user moves its project folder, or clears it, from the web.
    BotProjectSet {
        project: Option<BotProject>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    /// The user pauses, resumes or deletes a schedule of this conversation from the web.
    ScheduleControlled {
        schedule_id: String,
        action: ScheduleAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    /// Written only by the owner when it takes a schedule's due time, whether it runs or skips
    /// it as too late; no Node takes a time someone already claimed. Of concurrent claims the
    /// highest `owner_epoch` wins, ties pick the smaller author.
    ScheduleRunClaimed {
        schedule_id: String,
        slot_ms: i64,
        owner_epoch: u64,
    },
    /// Written by the Node that steered user message `message_id` into its bot's running reply
    /// `reply`, once that reply completed with it: the message needs no reply of its own.
    MessageSteered {
        message_id: String,
        reply: String,
    },
    /// In the receiving bot's conversation, written by the sender's scheduling Node.
    /// `message_id` derives from the sending turn and the tool's `call_ref`.
    BotMessaged {
        message_id: String,
        from: Sender,
        text: String,
        hops: u8,
    },
    /// Written only by the owner in a read-only closing turn; covered events stay stored.
    ContextCheckpoint {
        covers: VersionVector,
        summary: String,
        /// Excerpts of the newest user messages it covers; history, not instructions to keep.
        recent_user_requests: Vec<String>,
        open_work: Vec<OpenWork>,
        decisions: Vec<String>,
    },
}

impl EventBody {
    /// This Node's write with every credential it knows blanked, so no key reaches the shared
    /// record. A write the user signed is theirs and stays as signed.
    pub fn without_credentials(self) -> anyhow::Result<Self> {
        if self.attestation().is_some() {
            return Ok(self);
        }
        let mut value = serde_json::to_value(&self)?;
        if !crate::logging::blank_credentials_in(&mut value) {
            return Ok(self);
        }
        Ok(serde_json::from_value(value)?)
    }
    /// Present on writes that need the user's own device signature; see `crypto::validate_event_shape`.
    pub fn attestation(&self) -> Option<&Attestation> {
        match self {
            Self::ConversationOwner { attestation, .. }
            | Self::BotCreated { attestation, .. }
            | Self::BotDeleted { attestation, .. }
            | Self::MessagePosted { attestation, .. }
            | Self::WorkFinished { attestation, .. }
            | Self::NotificationAcked { attestation, .. }
            | Self::PrimaryBotSet { attestation, .. }
            | Self::BotProjectSet { attestation, .. }
            | Self::BotAvatarSet { attestation, .. }
            | Self::ScheduleControlled { attestation, .. } => attestation.as_ref(),
            _ => None,
        }
    }
    /// The event's type tag, stored beside it so reads by type use an index.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ConversationCreated { .. } => "conversation_created",
            Self::ConversationOwner { .. } => "conversation_owner",
            Self::BotCreated { .. } => "bot_created",
            Self::MessagePosted { .. } => "message_posted",
            Self::WorkDispatched { .. } => "work_dispatched",
            Self::WorkProgress { .. } => "work_progress",
            Self::WorkFinished { .. } => "work_finished",
            Self::NotificationAcked { .. } => "notification_acked",
            Self::QuestionAsked { .. } => "question_asked",
            Self::QuestionClosed { .. } => "question_closed",
            Self::SessionAttributed { .. } => "session_attributed",
            Self::SessionMuted { .. } => "session_muted",
            Self::PrimaryBotSet { .. } => "primary_bot_set",
            Self::BotProjectSet { .. } => "bot_project_set",
            Self::BotAvatarSet { .. } => "bot_avatar_set",
            Self::BotChanged { .. } => "bot_changed",
            Self::BotDeleted { .. } => "bot_deleted",
            Self::ScheduleControlled { .. } => "schedule_controlled",
            Self::ScheduleRunClaimed { .. } => "schedule_run_claimed",
            Self::MessageSteered { .. } => "message_steered",
            Self::BotMessaged { .. } => "bot_messaged",
            Self::ContextCheckpoint { .. } => "context_checkpoint",
        }
    }
    /// The message, work or question the event is about; a turn finds its trigger by it.
    pub fn subject(&self) -> Option<&str> {
        match self {
            Self::MessagePosted { message_id, .. } | Self::BotMessaged { message_id, .. } => {
                Some(message_id)
            }
            Self::QuestionAsked { question_id, .. } => Some(question_id),
            body => body.work_id(),
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
    pub models: BotModels,
    pub reasoning_efforts: BTreeMap<Provider, String>,
    pub skills: SkillSet,
    pub avatar: Option<Avatar>,
    /// The start of the hash of the event that last set the face, `None` before any change.
    pub avatar_version: Option<String>,
    pub event_hash: EventHash,
}

impl Bot {
    /// The model this bot runs on `provider`; `None` follows the device's default.
    pub fn model(&self, provider: Provider) -> Option<&str> {
        self.models.get(&provider).map(String::as_str)
    }

    pub fn reasoning_effort(&self, provider: Provider) -> Option<&str> {
        self.reasoning_efforts.get(&provider).map(String::as_str)
    }

    pub fn shown_avatar(&self) -> ShownAvatar {
        ShownAvatar {
            avatar: self.avatar.unwrap_or_else(|| Avatar::derived(&self.bot_id)),
            picked: self.avatar.is_some(),
        }
    }
}

/// The earliest `BotChanged` of one `change_id`; the chat shows it as a card.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangeRecord {
    pub change_id: String,
    /// The bot it changed; another bot's turn may have made it.
    pub conversation_id: ScopeId,
    pub cause: Option<Cause>,
    pub event_hash: EventHash,
    pub created_at_ms: i64,
    pub change: BotChange,
}

/// Output only: `flatten` cannot read back a `deny_unknown_fields` struct.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SkillEntry {
    pub skill_id: String,
    #[serde(flatten)]
    pub skill: Skill,
}

/// A schedule that is not removed, with its current pause state. `changed_at_ms` is when its
/// definition or its pause state was first stored on this Node, whichever is later.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ScheduleEntry {
    pub schedule_id: String,
    pub conversation_id: ScopeId,
    #[serde(flatten)]
    pub schedule: Schedule,
    pub changed_at_ms: i64,
}

/// A note one bot keeps.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MemoryEntry {
    pub memory_id: String,
    pub text: String,
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
    pub failure: Option<FailureKind>,
    pub report_results: Option<bool>,
    /// The question this user message answers.
    pub answers: Option<String>,
    /// The bot a bot message came from.
    pub sender: Option<Sender>,
    pub attachments: Vec<Attachment>,
    /// Its place in the conversation's shared order; clients merge pushed messages by it.
    pub rank: String,
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
    pub failure: Option<FailureKind>,
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
