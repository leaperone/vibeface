use super::{Store, bots::bot_for_conversation_from, log, message_from};
use crate::{
    crypto::digest,
    model::*,
    providers::Provider,
    scheduling::{
        AdmissionReceipt, MemoryConflict, StagedMemory, TriggerKind, ahead, bot_message_trigger,
    },
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use std::path::Path;

pub const NOT_FROM_USER: &str = "bot settings change only when the user's own message asks for it";

/// A review receives data only. Its JSON proposes mutations without executing tools.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MemoryReviewPolicy {
    #[default]
    Disabled,
    ReadOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum MemoryReviewOperation {
    Add {
        text: String,
    },
    Update {
        memory_id: String,
        epoch: u32,
        expected_revision: String,
        text: String,
    },
    Forget {
        memory_id: String,
        epoch: u32,
        expected_revision: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "MemoryReviewInput")]
pub struct MemoryReview {
    pub operations: Vec<MemoryReviewOperation>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryReviewInput {
    operations: Vec<MemoryReviewOperation>,
}

impl TryFrom<MemoryReviewInput> for MemoryReview {
    type Error = anyhow::Error;

    fn try_from(input: MemoryReviewInput) -> Result<Self> {
        let review = Self {
            operations: input.operations,
        };
        review.validate()?;
        Ok(review)
    }
}

impl MemoryReview {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.operations.len() <= 20,
            "a memory review proposes at most 20 operations"
        );
        for operation in &self.operations {
            match operation {
                MemoryReviewOperation::Add { text }
                | MemoryReviewOperation::Update { text, .. } => validate_memory(text)?,
                MemoryReviewOperation::Forget { .. } => {}
            }
            if let MemoryReviewOperation::Update {
                memory_id,
                expected_revision,
                ..
            }
            | MemoryReviewOperation::Forget {
                memory_id,
                expected_revision,
                ..
            } = operation
            {
                vibeface_protocol::validate_id(memory_id).map_err(anyhow::Error::msg)?;
                ensure!(
                    !expected_revision.is_empty()
                        && expected_revision.len() <= 256
                        && !expected_revision.chars().any(char::is_control),
                    "memory review requires a bounded expected revision"
                );
                ensure!(
                    !looks_secret(memory_id) && !looks_secret(expected_revision),
                    "memory review must not contain secrets"
                );
            }
        }
        Ok(())
    }
}

pub(crate) fn change_digest(conversation: &str, trigger: &str, call_ref: &str) -> String {
    digest(&serde_json::to_vec(&("bot_change", conversation, trigger, call_ref)).expect("strings"))
}

/// One turn's tool call names one change, so a retried call finds it again.
pub fn change_id(conversation: &str, trigger: &str, call_ref: &str) -> String {
    format!("change-{}", change_digest(conversation, trigger, call_ref))
}

/// What a settings or reminder tool asked for, tagged by the tool's name; the writer adds the
/// values it replaces.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChangeRequest {
    /// `default_avatar` goes back to the face the bot's ID gives, `avatar` picks one.
    UpdateProfile {
        name: Option<String>,
        instructions: Option<String>,
        avatar: Option<AvatarParts>,
        default_avatar: Option<bool>,
    },
    /// A bot's profile, this one's or another's; `bot` is its conversation ID, bot ID or name.
    /// `model` is the one for the writing device's provider, empty for its default;
    /// `all_skills` uses every shared skill again, `skills` only those.
    ConfigureBot {
        bot: String,
        name: Option<String>,
        instructions: Option<String>,
        model: Option<String>,
        skills: Option<std::collections::BTreeSet<String>>,
        all_skills: Option<bool>,
        avatar: Option<AvatarParts>,
        default_avatar: Option<bool>,
    },
    SaveSkill {
        skill_id: Option<String>,
        name: String,
        description: String,
        body: String,
    },
    RemoveSkill {
        skill_id: String,
    },
    SaveSchedule {
        schedule_id: Option<String>,
        name: String,
        prompt: String,
        first_run_at_ms: i64,
        every_minutes: Option<u32>,
        enabled: Option<bool>,
    },
    RemoveSchedule {
        schedule_id: String,
    },
    Remember {
        memory_id: Option<String>,
        text: String,
    },
    Forget {
        memory_id: String,
    },
    /// A one-shot schedule the bot sets for its own follow-up, in any turn it may wake in.
    Remind {
        note: String,
        in_minutes: Option<u32>,
        at_ms: Option<i64>,
    },
}

/// The parts of a face the chat names; the others stay as the bot shows them now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AvatarParts {
    pub outline: Option<Outline>,
    pub eyes: Option<Eyes>,
    pub hue: Option<Hue>,
}

impl AvatarParts {
    fn over(self, shown: Avatar) -> Avatar {
        Avatar {
            outline: self.outline.unwrap_or(shown.outline),
            eyes: self.eyes.unwrap_or(shown.eyes),
            hue: self.hue.unwrap_or(shown.hue),
        }
    }

    fn fits(self, avatar: Avatar) -> bool {
        self.outline.is_none_or(|outline| outline == avatar.outline)
            && self.eyes.is_none_or(|eyes| eyes == avatar.eyes)
            && self.hue.is_none_or(|hue| hue == avatar.hue)
    }
}

/// The profile fields one request sets; `skills` is `Some(None)` for every shared skill and
/// `avatar` `Some(None)` for the face the bot's ID gives.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfileRequest {
    name: Option<String>,
    instructions: Option<String>,
    model: Option<(Provider, String)>,
    skills: Option<SkillSet>,
    avatar: Option<Option<AvatarParts>>,
}

fn avatar_request(
    avatar: Option<AvatarParts>,
    default: Option<bool>,
) -> Result<Option<Option<AvatarParts>>> {
    Ok(match (avatar, default) {
        (Some(_), Some(true)) => bail!("give avatar or default_avatar, not both"),
        (_, Some(true)) => Some(None),
        (Some(parts), _) => {
            ensure!(
                parts != AvatarParts::default(),
                "avatar names an outline, eyes or hue"
            );
            Some(Some(parts))
        }
        (None, _) => None,
    })
}

impl ChangeRequest {
    pub(super) fn matches_memory(&self, id: &str, after: Option<&str>) -> bool {
        match self {
            Self::Remember { memory_id, text } => {
                memory_id.as_deref().is_none_or(|wanted| wanted == id)
                    && after == Some(text.as_str())
            }
            Self::Forget { memory_id } => memory_id == id && after.is_none(),
            _ => false,
        }
    }

    /// Every change but a reminder needs the user's own message to stand on.
    fn needs_user(&self) -> bool {
        !matches!(self, Self::Remind { .. })
    }

    /// A model applies to `provider`, the one the writing device runs bots on.
    fn profile(&self, provider: Provider) -> Result<Option<ProfileRequest>> {
        Ok(Some(match self {
            Self::UpdateProfile {
                name,
                instructions,
                avatar,
                default_avatar,
            } => ProfileRequest {
                name: name.clone(),
                instructions: instructions.clone(),
                model: None,
                skills: None,
                avatar: avatar_request(*avatar, *default_avatar)?,
            },
            Self::ConfigureBot {
                name,
                instructions,
                model,
                skills,
                all_skills,
                avatar,
                default_avatar,
                ..
            } => ProfileRequest {
                name: name.clone(),
                instructions: instructions.clone(),
                model: model
                    .as_ref()
                    .map(|model| (provider, model.trim().to_owned())),
                skills: match (skills, all_skills) {
                    (Some(_), Some(true)) => bail!("give skills or all_skills, not both"),
                    (_, Some(true)) => Some(None),
                    (Some(skills), _) => Some(Some(skills.clone())),
                    (None, _) => None,
                },
                avatar: avatar_request(*avatar, *default_avatar)?,
            },
            _ => return Ok(None),
        }))
    }

    /// Whether `change` already sets what this request asks for. A retry compares this, because
    /// the first write changed the values a new `before` would read.
    fn matches(&self, change: &BotChange, provider: Provider) -> bool {
        let after = |edit: &Option<Edit>| edit.as_ref().map(|edit| edit.after.clone());
        if let Ok(Some(wanted)) = self.profile(provider) {
            let BotChange::Profile {
                name,
                instructions,
                model,
                skills,
                avatar,
            } = change
            else {
                return false;
            };
            let avatar = match (wanted.avatar, avatar) {
                (None, None) => true,
                (Some(parts), Some(edit)) => match (parts, edit.after) {
                    (None, None) => true,
                    (Some(parts), Some(after)) => parts.fits(after),
                    _ => false,
                },
                _ => false,
            };
            return avatar
                && wanted.name == after(name)
                && wanted.instructions == after(instructions)
                && wanted.model
                    == model
                        .as_ref()
                        .map(|edit| (edit.provider, edit.after.clone()))
                && wanted.skills == skills.as_ref().map(|edit| edit.after.clone());
        }
        match (self, change) {
            (
                Self::SaveSkill {
                    skill_id,
                    name,
                    description,
                    body,
                },
                BotChange::Skill {
                    skill_id: id,
                    after: Some(saved),
                    ..
                },
            ) => {
                skill_id.as_ref().is_none_or(|wanted| wanted == id)
                    && *name == saved.name
                    && *description == saved.description
                    && *body == saved.body
            }
            (
                Self::RemoveSkill { skill_id },
                BotChange::Skill {
                    skill_id: id,
                    after: None,
                    ..
                },
            ) => skill_id == id,
            (
                Self::SaveSchedule {
                    schedule_id,
                    name,
                    prompt,
                    first_run_at_ms,
                    every_minutes,
                    enabled,
                },
                BotChange::Schedule {
                    schedule_id: id,
                    after: Some(saved),
                    ..
                },
            ) => {
                schedule_id.as_ref().is_none_or(|wanted| wanted == id)
                    && *name == saved.name
                    && *prompt == saved.prompt
                    && *first_run_at_ms == saved.first_run_at_ms
                    && *every_minutes == saved.every_minutes
                    && enabled.is_none_or(|enabled| enabled == saved.enabled)
            }
            (
                Self::RemoveSchedule { schedule_id },
                BotChange::Schedule {
                    schedule_id: id,
                    after: None,
                    ..
                },
            ) => schedule_id == id,
            // A retry computes a new time from `in_minutes`; the note names the reminder.
            (
                Self::Remind { note, .. },
                BotChange::Schedule {
                    before: None,
                    after: Some(saved),
                    ..
                },
            ) => *note == saved.prompt && saved.every_minutes.is_none(),
            (
                Self::Remember { .. } | Self::Forget { .. },
                BotChange::Memory {
                    memory_id, after, ..
                },
            ) => self.matches_memory(memory_id, after.as_deref()),
            _ => false,
        }
    }
}

pub(crate) fn skill_from(
    connection: &Connection,
    network: &str,
    id: &str,
) -> Result<Option<Skill>> {
    let skill: Option<Option<String>> = connection
        .query_row(
            "SELECT skill FROM skills WHERE network_id=? AND skill_id=?",
            params![network, id],
            |r| r.get(0),
        )
        .optional()?;
    super::optional_json(skill.flatten())
}

/// Skills not removed, by name.
pub(crate) fn skills_from(connection: &Connection, network: &str) -> Result<Vec<SkillEntry>> {
    let mut statement = connection
        .prepare("SELECT skill_id,skill FROM skills WHERE network_id=? AND skill IS NOT NULL")?;
    let rows = statement.query_map([network], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut skills = rows
        .map(|row| {
            let (skill_id, skill) = row?;
            Ok(SkillEntry {
                skill_id,
                skill: serde_json::from_str(&skill)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    skills.sort_by_cached_key(|entry| (entry.skill.name.to_lowercase(), entry.skill_id.clone()));
    Ok(skills)
}

const SCHEDULE_COLUMNS: &str =
    "schedule_id,conversation_id,schedule,changed_at_ms,enabled,enabled_at_ms";

/// The definition with the pause state that won on its own, and the later local first-seen time.
fn schedule_row(
    r: &rusqlite::Row<'_>,
) -> rusqlite::Result<(String, String, String, i64, bool, i64)> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
    ))
}

fn schedule_entry(
    (schedule_id, conversation_id, schedule, changed_at_ms, enabled, enabled_at_ms): (
        String,
        String,
        String,
        i64,
        bool,
        i64,
    ),
) -> Result<ScheduleEntry> {
    let mut schedule: Schedule = serde_json::from_str(&schedule)?;
    schedule.enabled = enabled;
    Ok(ScheduleEntry {
        schedule_id,
        conversation_id,
        schedule,
        changed_at_ms: changed_at_ms.max(enabled_at_ms),
    })
}

/// A deleted bot's schedules stop with it.
const LIVE_SCHEDULE: &str = "removed=0 AND NOT EXISTS (SELECT 1 FROM bot_deletions d
    WHERE d.network_id=schedules.network_id AND d.conversation_id=schedules.conversation_id)";

/// Schedules not removed, of one conversation or of all, by name.
pub(crate) fn schedules_from(
    connection: &Connection,
    network: &str,
    conversation: Option<&str>,
) -> Result<Vec<ScheduleEntry>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {SCHEDULE_COLUMNS} FROM schedules
         WHERE network_id=?1 AND {LIVE_SCHEDULE} AND (?2 IS NULL OR conversation_id=?2)"
    ))?;
    let rows = statement.query_map(params![network, conversation], schedule_row)?;
    let mut schedules = rows
        .map(|row| schedule_entry(row?))
        .collect::<Result<Vec<_>>>()?;
    schedules.sort_by_cached_key(|entry| {
        (
            entry.schedule.name.to_lowercase(),
            entry.schedule_id.clone(),
        )
    });
    Ok(schedules)
}

pub(crate) fn schedule_from(
    connection: &Connection,
    network: &str,
    id: &str,
) -> Result<Option<ScheduleEntry>> {
    connection
        .query_row(
            &format!(
                "SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE network_id=? AND schedule_id=? AND {LIVE_SCHEDULE}"
            ),
            params![network, id],
            schedule_row,
        )
        .optional()?
        .map(schedule_entry)
        .transpose()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryRecord {
    pub entry: MemoryEntry,
    pub epoch: u32,
    pub revision: String,
    pub control_revision: u32,
    pub origin: MemoryOrigin,
    pub user_controlled: bool,
    pub essential: bool,
    pub source_event_hash: String,
}

impl MemoryRecord {
    pub fn metadata(&self) -> MemoryMetadata {
        MemoryMetadata {
            epoch: self.epoch,
            revision: self.revision.clone(),
            control_revision: self.control_revision,
            origin: self.origin,
            user_controlled: self.user_controlled,
            essential: self.essential,
            source_event_hash: self.source_event_hash.clone(),
        }
    }
}

fn memory_state_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryState> {
    let origin: String = row.get(4)?;
    Ok(MemoryState {
        memory_id: row.get(0)?,
        text: row.get(1)?,
        metadata: MemoryMetadata {
            epoch: row.get(2)?,
            revision: row.get(3)?,
            origin: serde_json::from_str(&origin).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
            user_controlled: row.get(5)?,
            essential: row.get(6)?,
            source_event_hash: row.get(7)?,
            control_revision: row.get(8)?,
        },
    })
}

const MEMORY_COLUMNS: &str = "memory_id,text,epoch,revision,origin,user_controlled,essential,source_event_hash,control_revision";

pub(crate) fn memory_state_from(
    connection: &Connection,
    network: &str,
    scope: &str,
    memory_id: &str,
) -> Result<Option<MemoryState>> {
    Ok(connection.query_row(
        &format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE network_id=? AND conversation_id=? AND memory_id=?"),
        params![network, scope, memory_id], memory_state_row,
    ).optional()?)
}

pub(crate) fn memory_records_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
) -> Result<Vec<MemoryRecord>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {MEMORY_COLUMNS} FROM memories WHERE network_id=? AND conversation_id=? AND text IS NOT NULL ORDER BY changed_at_ms,memory_id"
    ))?;
    let rows = statement.query_map(params![network, conversation], memory_state_row)?;
    rows.map(|row| {
        let state = row?;
        let metadata = state.metadata;
        Ok(MemoryRecord {
            entry: MemoryEntry {
                memory_id: state.memory_id,
                text: state.text.context("live memory has no text")?,
            },
            epoch: metadata.epoch,
            revision: metadata.revision,
            control_revision: metadata.control_revision,
            origin: metadata.origin,
            user_controlled: metadata.user_controlled,
            essential: metadata.essential,
            source_event_hash: metadata.source_event_hash,
        })
    })
    .collect()
}

pub(crate) struct MemoryOperation {
    pub memory_id: String,
    pub base: MemoryBase,
    pub version: MemoryVersion,
    pub after: Option<String>,
    pub origin: MemoryOrigin,
    pub user_controlled: bool,
    pub essential: bool,
}

pub(crate) fn memory_operation(
    network: &str,
    hash: &str,
    event: &Event,
) -> Result<Option<MemoryOperation>> {
    let (memory_id, base, after, origin, user_controlled, essential) = match &event.body {
        EventBody::MemoryControlled {
            memory_id,
            base,
            after,
            ..
        } => (memory_id, base, after, MemoryOrigin::User, true, true),
        EventBody::BotChanged {
            change:
                BotChange::Memory {
                    memory_id,
                    base,
                    after,
                    origin,
                    essential,
                    ..
                },
            ..
        } => (memory_id, base, after, (*origin).into(), false, *essential),
        _ => return Ok(None),
    };
    Ok(Some(MemoryOperation {
        memory_id: memory_id.clone(),
        base: base.clone(),
        after: after.clone(),
        origin,
        user_controlled,
        essential,
        version: MemoryVersion {
            revision: if user_controlled {
                crate::crypto::request_key(network, &event.scope_id, &event.body, &event.author)?
            } else {
                hash.into()
            },
            control_revision: if user_controlled {
                base.next_control_revision()?
            } else {
                0
            },
            source_event_hash: hash.into(),
        },
    }))
}

#[derive(Debug, Clone)]
pub struct MemoryControlCommand {
    pub memory_id: String,
    pub base: MemoryBase,
    pub after: Option<String>,
    pub attestation: Attestation,
    pub operation_id: String,
}

fn memory_receipt_from(
    connection: &Connection,
    network: &str,
    scope: &str,
    memory_id: &str,
    operation_id: &str,
    current: Option<&MemoryState>,
    body: Option<&EventBody>,
) -> Result<MemoryReceipt> {
    let mut statement = connection.prepare("SELECT hash,envelope FROM events WHERE network_id=? AND scope_id=? AND subject=? AND kind='memory_controlled' AND effective=1")?;
    let rows = statement.query_map(params![network, scope, memory_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut found = false;
    for row in rows {
        let (hash, envelope) = row?;
        let envelope: SignedEvent = serde_json::from_str(&envelope)?;
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        let operation = memory_operation(network, &hash, &event)?.context("memory operation")?;
        if operation.version.revision != operation_id {
            continue;
        }
        if let Some(body) = body {
            ensure!(
                crate::crypto::attestation_payload(network, scope, &event.body)?
                    == crate::crypto::attestation_payload(network, scope, body)?
                    && event.body.attestation().map(|value| &value.device_id)
                        == body.attestation().map(|value| &value.device_id),
                "memory operation identity collision"
            );
        }
        found = true;
    }
    Ok(if !found {
        MemoryReceipt::NotSeenHere
    } else if current.is_some_and(|state| {
        state.metadata.user_controlled && state.metadata.revision == operation_id
    }) {
        MemoryReceipt::Recorded
    } else {
        MemoryReceipt::Superseded
    })
}

fn memory_sync_conflicts_from(
    connection: &Connection,
    network: &str,
    scope: &str,
) -> Result<MemorySyncConflictPage> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut statement = connection.prepare("SELECT hash,envelope FROM events WHERE network_id=? AND scope_id=? AND effective=1 AND kind IN ('memory_controlled','bot_changed') ORDER BY hash")?;
    let rows = statement.query_map(params![network, scope], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut operations = BTreeMap::<String, BTreeMap<String, MemoryOperation>>::new();
    for row in rows {
        let (hash, envelope) = row?;
        let envelope: SignedEvent = serde_json::from_str(&envelope)?;
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        if let Some(operation) = memory_operation(network, &hash, &event)? {
            operations
                .entry(operation.memory_id.clone())
                .or_default()
                .entry(operation.version.revision.clone())
                .or_insert(operation);
        }
    }
    let mut items = Vec::new();
    for (memory_id, operations) in operations {
        let Some(winner) = memory_state_from(connection, network, scope, &memory_id)? else {
            continue;
        };
        let mut chain = BTreeSet::new();
        let mut cursor = winner.metadata.base().version;
        while let Some(version) = cursor {
            if !chain.insert(version.revision.clone()) {
                break;
            }
            cursor = if let Some(operation) = operations.get(&version.revision) {
                operation.base.version.clone()
            } else {
                let source = log::event_from(connection, network, &version.source_event_hash)?
                    .context("memory base is not stored here")?;
                memory_operation(network, &version.source_event_hash, &source)?
                    .context("memory base is not a memory operation")?
                    .base
                    .version
            };
        }
        for (revision, losing) in &operations {
            if chain.contains(revision) {
                continue;
            }
            items.push(MemorySyncConflict {
                memory_id: memory_id.clone(),
                losing_revision: revision.clone(),
                winning_revision: winner.metadata.revision.clone(),
                losing_event_hash: losing.version.source_event_hash.clone(),
                winning_event_hash: winner.metadata.source_event_hash.clone(),
                reason: if winner.metadata.user_controlled && !losing.user_controlled {
                    MemorySyncConflictReason::UserProtected
                } else if losing.user_controlled {
                    MemorySyncConflictReason::ConcurrentUser
                } else {
                    MemorySyncConflictReason::ConcurrentAutomatic
                },
            });
        }
    }
    let has_more = items.len() > 50;
    items.truncate(50);
    Ok(MemorySyncConflictPage { items, has_more })
}

/// One bot's notes, oldest first.
pub(crate) fn memories_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
) -> Result<Vec<MemoryEntry>> {
    Ok(memory_records_from(connection, network, conversation)?
        .into_iter()
        .map(|record| record.entry)
        .collect())
}

const CHANGE_COLUMNS: &str = "change_id,cause,event_hash,created_at_ms,change,conversation_id";

type ChangeRow = (ChangeRecord, Option<String>, String);

fn change_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChangeRow> {
    Ok((
        ChangeRecord {
            change_id: r.get(0)?,
            conversation_id: r.get(5)?,
            cause: None,
            event_hash: r.get(2)?,
            created_at_ms: r.get(3)?,
            change: BotChange::Profile {
                name: None,
                instructions: None,
                model: None,
                skills: None,
                avatar: None,
            },
        },
        r.get(1)?,
        r.get(4)?,
    ))
}

fn with_change((mut record, cause, change): ChangeRow) -> Result<ChangeRecord> {
    record.cause = super::optional_json(cause)?;
    record.change = serde_json::from_str(&change)?;
    Ok(record)
}

pub(crate) fn change_from(
    connection: &Connection,
    network: &str,
    change_id: &str,
) -> Result<Option<ChangeRecord>> {
    connection
        .query_row(
            &format!("SELECT {CHANGE_COLUMNS} FROM bot_changes WHERE network_id=? AND change_id=?"),
            params![network, change_id],
            change_row,
        )
        .optional()?
        .map(with_change)
        .transpose()
}

/// One turn's `message_bot` call names one message, so a retried call finds it again.
pub fn bot_message_id(scope: &str, trigger: &str, call_ref: &str) -> String {
    format!(
        "bot-message-{}",
        digest(&serde_json::to_vec(&("bot_message", scope, trigger, call_ref)).expect("strings"))
    )
}

/// The user's own message that started the turn of `trigger` in `scope`, if it did.
pub(crate) fn user_cause(
    connection: &Connection,
    network: &str,
    scope: &str,
    trigger: &str,
) -> Result<Option<Cause>> {
    let Some(message) = trigger.strip_prefix("message:") else {
        return Ok(None);
    };
    let message = message_from(connection, network, scope, message)?
        .context("the user's message is not stored on this device")?;
    Ok(
        (message.role == Role::User && message.answers.is_none()).then(|| Cause {
            conversation_id: scope.into(),
            message_id: message.message_id,
        }),
    )
}

/// A bot by its conversation ID, bot ID or name; a name must name one bot.
pub(crate) fn find_bot(connection: &Connection, network: &str, wanted: &str) -> Result<Bot> {
    let bots = super::bots::bots_from(connection, network)?;
    if let Some(bot) = bots
        .iter()
        .find(|bot| bot.conversation_id == wanted || bot.bot_id == wanted)
    {
        return Ok(bot.clone());
    }
    let wanted = wanted.trim().to_lowercase();
    let mut named = bots
        .into_iter()
        .filter(|bot| bot.name.trim().to_lowercase() == wanted);
    match (named.next(), named.next()) {
        (Some(bot), None) => Ok(bot),
        (Some(_), Some(_)) => bail!("several bots are named {wanted}; use its conversation_id"),
        (None, _) => bail!("no bot named {wanted}; list_bots shows them"),
    }
}

/// A schedule belongs to the bot that created it; others cannot change it.
fn own_schedule(connection: &Connection, network: &str, scope: &str, id: &str) -> Result<Schedule> {
    Ok(schedule_from(connection, network, id)?
        .filter(|entry| entry.conversation_id == scope)
        .with_context(|| format!("schedule {id} not found for this bot"))?
        .schedule)
}

/// A new time ahead must fit under the bot's limit; replacing one keeps its place.
fn room_for_schedule(
    connection: &Connection,
    network: &str,
    scope: &str,
    replacing: Option<&str>,
    now: i64,
) -> Result<()> {
    let active = schedules_from(connection, network, Some(scope))?
        .into_iter()
        .filter(|entry| Some(entry.schedule_id.as_str()) != replacing)
        .filter(|entry| ahead(&entry.schedule, now))
        .count();
    ensure!(
        active < MAX_ACTIVE_SCHEDULES,
        "this bot already has {MAX_ACTIVE_SCHEDULES} schedules; remove one first"
    );
    Ok(())
}

fn once_window(at: i64, now: i64, max_lead_ms: i64) -> Result<()> {
    ensure!(
        at >= now + ONCE_MIN_LEAD_MS && at <= now + max_lead_ms,
        "a one-time schedule runs between 1 minute and {} days from now",
        max_lead_ms / 86_400_000
    );
    Ok(())
}

/// What a turn of `depth` may still start; see `scheduler::chain_depth`.
fn next_depth(depth: u8) -> Result<u8> {
    let next = depth.saturating_add(1);
    ensure!(
        next <= MAX_CHAIN_DEPTH,
        "{MAX_CHAIN_DEPTH} automatic wakes already followed the user's last message; wait for the user"
    );
    Ok(next)
}

/// Turns the request into a change against the current projection. A change must set something
/// new, and an update or removal must name something that exists here.
#[expect(
    clippy::too_many_arguments,
    reason = "the turn, its request and the writer's provider"
)]
fn resolve(
    connection: &Connection,
    network: &str,
    scope: &str,
    trigger: &str,
    digest: &str,
    request: ChangeRequest,
    provider: Provider,
    now: i64,
) -> Result<BotChange> {
    if let Some(ProfileRequest {
        name,
        instructions,
        model,
        skills,
        avatar,
    }) = request.profile(provider)?
    {
        ensure!(
            name.is_some()
                || instructions.is_some()
                || model.is_some()
                || skills.is_some()
                || avatar.is_some(),
            "name, instructions, model, skills, all_skills, avatar or default_avatar is required"
        );
        let bot = bot_for_conversation_from(connection, network, scope)?
            .context("this conversation has no bot profile to change")?;
        if let Some(name) = &name {
            validate_bot_name(name)?;
            ensure!(
                *name != bot.name,
                "the bot is already named {name}; nothing to change"
            );
        }
        if let Some(instructions) = &instructions {
            validate_instructions(instructions)?;
            ensure!(
                *instructions != bot.instructions,
                "the instructions already read like this; nothing to change"
            );
        }
        if let Some((provider, model)) = &model {
            validate_model(model)?;
            ensure!(
                model != bot.model(*provider).unwrap_or_default(),
                "the bot already uses this model; nothing to change"
            );
        }
        if let Some(Some(wanted)) = &skills {
            let library = skills_from(connection, network)?;
            if let Some(missing) = wanted
                .iter()
                .find(|id| !library.iter().any(|entry| &entry.skill_id == *id))
            {
                bail!("skill {missing} not found; bot.skills and list_bots show skill IDs");
            }
        }
        if let Some(skills) = &skills {
            validate_skill_set(skills)?;
            ensure!(
                *skills != bot.skills,
                "the bot already uses these skills; nothing to change"
            );
        }
        let shown = bot.shown_avatar().avatar;
        let avatar = avatar.map(|parts| parts.map(|parts| parts.over(shown)));
        if let Some(after) = avatar {
            ensure!(
                after.map_or(bot.avatar.is_some(), |after| after != shown),
                "the bot already has this face; nothing to change"
            );
        }
        let model = model.map(|(provider, after)| ModelEdit {
            provider,
            before: bot.model(provider).unwrap_or_default().to_owned(),
            after,
        });
        let change = BotChange::Profile {
            name: name.map(|after| Edit {
                before: bot.name,
                after,
            }),
            instructions: instructions.map(|after| Edit {
                before: bot.instructions,
                after,
            }),
            model,
            skills: skills.map(|after| SkillsEdit {
                before: bot.skills,
                after,
            }),
            avatar: avatar.map(|after| AvatarEdit {
                before: bot.avatar,
                after,
            }),
        };
        change.validate()?;
        return Ok(change);
    }
    let change = match request {
        ChangeRequest::UpdateProfile { .. } | ChangeRequest::ConfigureBot { .. } => {
            bail!("a profile change is resolved above")
        }
        ChangeRequest::SaveSkill {
            skill_id,
            name,
            description,
            body,
        } => {
            let skill = Skill {
                name,
                description,
                body,
            };
            skill.validate()?;
            let (skill_id, before) = match skill_id {
                Some(id) => {
                    let before = skill_from(connection, network, &id)?
                        .with_context(|| format!("skill {id} not found"))?;
                    (id, Some(before))
                }
                None => {
                    let wanted = skill.name.trim().to_lowercase();
                    if let Some(existing) = skills_from(connection, network)?
                        .into_iter()
                        .find(|entry| entry.skill.name.trim().to_lowercase() == wanted)
                    {
                        bail!(
                            "a skill named {} exists as {}; pass its skill_id to update it",
                            existing.skill.name,
                            existing.skill_id
                        );
                    }
                    (format!("skill-{digest}"), None)
                }
            };
            ensure!(
                before.as_ref() != Some(&skill),
                "the skill already reads like this; nothing to change"
            );
            BotChange::Skill {
                skill_id,
                before,
                after: Some(skill),
            }
        }
        ChangeRequest::RemoveSkill { skill_id } => {
            let before = skill_from(connection, network, &skill_id)?
                .with_context(|| format!("skill {skill_id} not found"))?;
            BotChange::Skill {
                skill_id,
                before: Some(before),
                after: None,
            }
        }
        ChangeRequest::SaveSchedule {
            schedule_id,
            name,
            prompt,
            first_run_at_ms,
            every_minutes,
            enabled,
        } => {
            let (schedule_id, before) = match schedule_id {
                Some(id) => {
                    let before = own_schedule(connection, network, scope, &id)?;
                    (id, Some(before))
                }
                None => (format!("schedule-{digest}"), None),
            };
            let after = Schedule {
                name,
                prompt,
                first_run_at_ms,
                every_minutes,
                enabled: enabled.unwrap_or(before.as_ref().is_none_or(|before| before.enabled)),
                depth: 0,
            };
            after.validate()?;
            if every_minutes.is_none()
                && before
                    .as_ref()
                    .is_none_or(|before| before.first_run_at_ms != first_run_at_ms)
            {
                once_window(first_run_at_ms, now, ONCE_MAX_LEAD_MS)?;
            }
            if before.as_ref().is_none_or(|before| !ahead(before, now)) && ahead(&after, now) {
                room_for_schedule(connection, network, scope, Some(&schedule_id), now)?;
            }
            ensure!(
                before.as_ref() != Some(&after),
                "the schedule already has these settings; nothing to change"
            );
            BotChange::Schedule {
                schedule_id,
                before,
                after: Some(after),
            }
        }
        ChangeRequest::RemoveSchedule { schedule_id } => {
            let before = own_schedule(connection, network, scope, &schedule_id)?;
            BotChange::Schedule {
                schedule_id,
                before: Some(before),
                after: None,
            }
        }
        ChangeRequest::Remind {
            note,
            in_minutes,
            at_ms,
        } => {
            let at = match (in_minutes, at_ms) {
                (Some(minutes), None) => now + i64::from(minutes) * 60_000,
                (None, Some(at)) => at,
                _ => bail!("give either in_minutes or at_ms"),
            };
            let longest = i64::from(*SCHEDULE_MINUTES.end()) * 60_000;
            once_window(at, now, longest)?;
            let name: String = note
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or_default()
                .chars()
                .take(MAX_SCHEDULE_NAME_CHARS)
                .collect();
            let after = Schedule {
                name,
                prompt: note,
                first_run_at_ms: at,
                every_minutes: None,
                enabled: true,
                depth: next_depth(super::scheduler::chain_depth(
                    connection, network, scope, trigger,
                )?)?,
            };
            after.validate()?;
            room_for_schedule(connection, network, scope, None, now)?;
            BotChange::Schedule {
                schedule_id: format!("schedule-{digest}"),
                before: None,
                after: Some(after),
            }
        }
        ChangeRequest::Remember { memory_id, text } => {
            validate_memory(&text)?;
            let notes = memories_from(connection, network, scope)?;
            if let Some(same) = notes.iter().find(|note| note.text == text) {
                bail!("this is already kept as {}", same.memory_id);
            }
            let memory_id = memory_id.unwrap_or_else(|| format!("memory-{digest}"));
            let state = memory_state_from(connection, network, scope, &memory_id)?;
            ensure!(
                !state
                    .as_ref()
                    .is_some_and(|state| state.metadata.user_controlled),
                "this note is user-controlled; change it directly"
            );
            if state.as_ref().is_none_or(|state| state.text.is_none()) {
                ensure!(
                    notes.len() < MAX_MEMORIES,
                    "this bot already keeps {MAX_MEMORIES} notes; forget one first"
                );
            }
            BotChange::Memory {
                memory_id,
                before: state.as_ref().and_then(|state| state.text.clone()),
                after: Some(text),
                base: state
                    .as_ref()
                    .map_or_else(MemoryBase::absent, |state| state.metadata.base()),
                origin: AutomaticMemoryOrigin::Agent,
                essential: state.as_ref().is_none_or(|state| state.metadata.essential),
            }
        }
        ChangeRequest::Forget { memory_id } => {
            let state = memory_state_from(connection, network, scope, &memory_id)?
                .context("note not found for this bot")?;
            ensure!(
                !state.metadata.user_controlled,
                "this note is user-controlled; change it directly"
            );
            ensure!(state.text.is_some(), "note not found for this bot");
            BotChange::Memory {
                memory_id,
                base: state.metadata.base(),
                before: state.text,
                after: None,
                origin: AutomaticMemoryOrigin::Agent,
                essential: state.metadata.essential,
            }
        }
    };
    change.validate()?;
    Ok(change)
}

impl Store {
    /// Commit staged memory changes in the same transaction that closes the turn. A changed
    /// committed revision wins over a stale staged value; the turn remains successful and the
    /// conflicts are returned for the local scheduler projection.
    #[expect(
        clippy::too_many_arguments,
        reason = "the completion transaction supplies the same turn identity and store handles"
    )]
    pub(crate) fn commit_staged_memory_from(
        connection: &Connection,
        blob_dir: &Path,
        identity: &crate::crypto::LocalIdentity,
        network: &str,
        scope: &str,
        trigger: &str,
        staged: impl IntoIterator<Item = StagedMemory>,
        at: i64,
    ) -> Result<Vec<MemoryConflict>> {
        let cause = user_cause(connection, network, scope, trigger)?;
        let mut conflicts = Vec::new();
        for staged in staged {
            let current = memory_state_from(connection, network, scope, &staged.memory_id)?;
            let current_base = current
                .as_ref()
                .map_or_else(MemoryBase::absent, |state| state.metadata.base());
            let current_text = current.as_ref().and_then(|state| state.text.clone());
            let current_revision = current_base
                .version
                .as_ref()
                .map(|version| version.revision.clone())
                .unwrap_or_default();
            let current_epoch = current_base.epoch;
            let reason = if current
                .as_ref()
                .is_some_and(|state| state.metadata.user_controlled)
            {
                Some("user_controlled")
            } else if current_epoch != staged.base.epoch {
                Some("epoch_changed")
            } else if !current_base.same_version(&staged.base) {
                Some("revision_changed")
            } else if current_text != staged.before {
                Some("value_changed")
            } else if current_text.is_none()
                && staged.after.is_some()
                && memory_records_from(connection, network, scope)?.len() >= MAX_MEMORIES
            {
                Some("capacity_exceeded")
            } else {
                None
            };
            if let Some(reason) = reason {
                tracing::warn!(
                    event = "agent.memory_conflict",
                    conversation_id = scope,
                    memory_id = staged.memory_id,
                    reason,
                    "staged memory was not committed"
                );
                conflicts.push(MemoryConflict {
                    call_ref: staged.call_ref,
                    memory_id: staged.memory_id,
                    expected_epoch: staged.base.epoch,
                    actual_epoch: current_epoch,
                    expected_revision: staged
                        .base
                        .version
                        .as_ref()
                        .map(|version| version.revision.clone())
                        .unwrap_or_default(),
                    actual_revision: current_revision,
                    reason: reason.into(),
                });
                continue;
            }
            let change = BotChange::Memory {
                memory_id: staged.memory_id.clone(),
                before: staged.before,
                after: staged.after,
                base: staged.base,
                origin: staged.origin,
                essential: staged.essential,
            };
            change.validate()?;
            log::append_from(
                connection,
                blob_dir,
                identity,
                network,
                scope,
                EventBody::BotChanged {
                    change_id: change_id(scope, trigger, &staged.call_ref),
                    cause: cause.clone(),
                    change,
                },
                vec![],
                at,
            )?;
        }
        Ok(conflicts)
    }

    /// Writes the change a turn's tool call asked for. The same `call_ref` in the same turn
    /// returns the stored change and writes nothing; a different request under it is refused.
    /// Returns whether an event was written.
    #[expect(
        clippy::too_many_arguments,
        reason = "the turn, its tool call and the change"
    )]
    pub fn change_bot(
        &mut self,
        identity: &crate::crypto::LocalIdentity,
        network: &str,
        scope: &str,
        trigger: &str,
        call_ref: &str,
        request: ChangeRequest,
        provider: Provider,
        at: i64,
    ) -> Result<(ChangeRecord, bool)> {
        vibeface_protocol::validate_id(call_ref).map_err(anyhow::Error::msg)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cause = user_cause(&tx, network, scope, trigger)?;
        ensure!(cause.is_some() || !request.needs_user(), NOT_FROM_USER);
        let target = match &request {
            ChangeRequest::ConfigureBot { bot, .. } => find_bot(&tx, network, bot)?.conversation_id,
            _ => scope.to_owned(),
        };
        let digest = change_digest(scope, trigger, call_ref);
        let change_id = format!("change-{digest}");
        if let Some(existing) = change_from(&tx, network, &change_id)? {
            ensure!(
                request.matches(&existing.change, provider),
                "call_ref {call_ref} already made a different change; use a new call_ref for a new change"
            );
            return Ok((existing, false));
        }
        let change = resolve(
            &tx, network, &target, trigger, &digest, request, provider, at,
        )?;
        log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            &target,
            EventBody::BotChanged {
                change_id: change_id.clone(),
                cause,
                change,
            },
            vec![],
            at,
        )?;
        let record = change_from(&tx, network, &change_id)?.context("change was not projected")?;
        tx.commit()?;
        Ok((record, true))
    }

    pub fn bot_change(&self, network: &str, change_id: &str) -> Result<Option<ChangeRecord>> {
        change_from(&self.connection, network, change_id)
    }

    /// One conversation's changes in shared order, each at its earliest copy.
    pub fn bot_changes(&self, network: &str, scope: &str) -> Result<Vec<ChangeRecord>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {CHANGE_COLUMNS} FROM bot_changes WHERE network_id=? AND conversation_id=? ORDER BY rank"
        ))?;
        let rows = statement.query_map(params![network, scope], change_row)?;
        rows.map(|row| with_change(row?)).collect()
    }

    pub fn overwritten(&self, network: &str, scope: &str) -> Result<Vec<Overwritten>> {
        overwritten_from(&self.connection, network, scope)
    }

    pub fn skills(&self, network: &str) -> Result<Vec<SkillEntry>> {
        skills_from(&self.connection, network)
    }

    pub fn skill(&self, network: &str, skill_id: &str) -> Result<Option<Skill>> {
        skill_from(&self.connection, network, skill_id)
    }

    pub fn schedules(&self, network: &str, scope: &str) -> Result<Vec<ScheduleEntry>> {
        schedules_from(&self.connection, network, Some(scope))
    }

    pub fn schedule(&self, network: &str, schedule_id: &str) -> Result<Option<ScheduleEntry>> {
        schedule_from(&self.connection, network, schedule_id)
    }

    pub fn memories(&self, network: &str, scope: &str) -> Result<Vec<MemoryEntry>> {
        memories_from(&self.connection, network, scope)
    }

    pub(crate) fn memory_records(&self, network: &str, scope: &str) -> Result<Vec<MemoryRecord>> {
        memory_records_from(&self.connection, network, scope)
    }

    pub fn memory_state(
        &self,
        network: &str,
        scope: &str,
        memory_id: &str,
        operation_id: Option<&str>,
    ) -> Result<MemoryStateResult> {
        vibeface_protocol::validate_id(memory_id).map_err(anyhow::Error::msg)?;
        let current = memory_state_from(&self.connection, network, scope, memory_id)?;
        let receipt = operation_id
            .map(|operation_id| {
                ensure!(
                    hex::decode(operation_id).is_ok_and(|value| value.len() == 32),
                    "invalid memory operation ID"
                );
                memory_receipt_from(
                    &self.connection,
                    network,
                    scope,
                    memory_id,
                    operation_id,
                    current.as_ref(),
                    None,
                )
            })
            .transpose()?;
        Ok(MemoryStateResult { current, receipt })
    }

    pub fn memory_sync_conflicts(
        &self,
        network: &str,
        scope: &str,
    ) -> Result<MemorySyncConflictPage> {
        memory_sync_conflicts_from(&self.connection, network, scope)
    }

    pub fn control_memory(
        &mut self,
        identity: &crate::crypto::LocalIdentity,
        network: &str,
        scope: &str,
        command: MemoryControlCommand,
        at: i64,
    ) -> Result<MemoryControlResult> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let body = EventBody::MemoryControlled {
            memory_id: command.memory_id.clone(),
            base: command.base.clone(),
            after: command.after,
            attestation: Some(command.attestation),
        };
        let author = identity.device_id()?;
        let event = Event {
            version: WIRE_VERSION,
            network_id: network.into(),
            author: author.clone(),
            seq: 1,
            prev: None,
            deps: vec![],
            scope_id: scope.into(),
            created_at_ms: at,
            body,
            blobs: vec![],
        };
        crate::crypto::validate_event_shape(&event)?;
        let view = super::membership_from(&tx, network)?;
        view.node(&author)?;
        view.client(
            &event
                .body
                .attestation()
                .context("missing device signature")?
                .device_id,
        )?;
        bot_for_conversation_from(&tx, network, scope)?.context("no such bot")?;
        let operation_id = crate::crypto::request_key(network, scope, &event.body, &author)?;
        ensure!(
            operation_id == command.operation_id,
            "memory operation ID does not match this write"
        );
        let current = memory_state_from(&tx, network, scope, &command.memory_id)?;
        let receipt = memory_receipt_from(
            &tx,
            network,
            scope,
            &command.memory_id,
            &operation_id,
            current.as_ref(),
            Some(&event.body),
        )?;
        let recorded = match receipt {
            MemoryReceipt::Recorded => Some(MemoryControlStatus::AlreadyRecorded),
            MemoryReceipt::Superseded => Some(MemoryControlStatus::Superseded),
            MemoryReceipt::NotSeenHere => None,
        };
        if let Some(state) = recorded {
            return Ok(MemoryControlResult {
                state,
                operation_id,
                current,
            });
        }
        log::validate_memory_base(&tx, network, scope, &event.body)?;
        let current_base = current
            .as_ref()
            .map_or_else(MemoryBase::absent, |state| state.metadata.base());
        if !current_base.same_version(&command.base) {
            return Ok(MemoryControlResult {
                state: MemoryControlStatus::Conflict,
                operation_id,
                current,
            });
        }
        if current.as_ref().is_none_or(|state| state.text.is_none())
            && matches!(
                &event.body,
                EventBody::MemoryControlled { after: Some(_), .. }
            )
        {
            ensure!(
                memory_records_from(&tx, network, scope)?.len() < MAX_MEMORIES,
                "this bot already keeps {MAX_MEMORIES} notes; forget one first"
            );
        }
        log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            scope,
            event.body,
            vec![],
            at,
        )?;
        let current = memory_state_from(&tx, network, scope, &command.memory_id)?;
        tx.commit()?;
        Ok(MemoryControlResult {
            state: MemoryControlStatus::Applied,
            operation_id,
            current,
        })
    }

    pub fn find_bot(&self, network: &str, wanted: &str) -> Result<Bot> {
        find_bot(&self.connection, network, wanted)
    }

    /// A message one bot sent another, wherever it went.
    pub fn bot_message(&self, network: &str, message_id: &str) -> Result<Option<Message>> {
        let scope: Option<String> = self
            .connection
            .query_row(
                "SELECT conversation_id FROM messages WHERE network_id=? AND message_id=? AND role='bot'",
                params![network, message_id],
                |r| r.get(0),
            )
            .optional()?;
        scope
            .map(|scope| message_from(&self.connection, network, &scope, message_id))
            .transpose()
            .map(Option::flatten)
    }

    /// Posts `text` from the bot of `scope` into another bot's conversation and queues the
    /// turn that wakes it, once. The same `call_ref` in the same turn returns the stored message
    /// and writes nothing; other text under it is refused. Returns the message, its receipt and
    /// whether it was written.
    #[expect(
        clippy::too_many_arguments,
        reason = "the turn, its tool call and the message"
    )]
    pub fn message_bot(
        &mut self,
        identity: &crate::crypto::LocalIdentity,
        network: &str,
        scope: &str,
        trigger: &str,
        call_ref: &str,
        to: &str,
        text: &str,
        at: i64,
    ) -> Result<(Message, AdmissionReceipt, bool)> {
        vibeface_protocol::validate_id(call_ref).map_err(anyhow::Error::msg)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sender = bot_for_conversation_from(&tx, network, scope)?
            .context("this conversation has no bot to send from")?;
        let target = find_bot(&tx, network, to)?.conversation_id;
        ensure!(target != scope, "a bot does not message itself");
        let message_id = bot_message_id(scope, trigger, call_ref);
        let bot_trigger = bot_message_trigger(&message_id);
        if let Some(existing) = message_from(&tx, network, &target, &message_id)? {
            ensure!(
                existing.text == text,
                "call_ref {call_ref} already sent another message; use a new call_ref for a new message"
            );
            let receipt = AdmissionReceipt {
                network_id: network.into(),
                conversation_id: target,
                trigger: bot_trigger,
                state: "sent".into(),
            };
            return Ok((existing, receipt, false));
        }
        ensure!(
            !text.trim().is_empty() && text.len() <= MAX_BOT_MESSAGE_BYTES,
            "a message to a bot is 1-{MAX_BOT_MESSAGE_BYTES} bytes"
        );
        let sent: usize = tx.query_row(
            "SELECT COUNT(*) FROM messages WHERE network_id=? AND role='bot' AND json_extract(sender,'$.conversation_id')=? AND created_at_ms>?",
            params![network, scope, at - 3_600_000],
            |r| r.get(0),
        )?;
        ensure!(
            sent < BOT_MESSAGES_PER_HOUR,
            "this bot already sent {BOT_MESSAGES_PER_HOUR} messages to other bots in the last hour"
        );
        let hops = next_depth(super::scheduler::chain_depth(&tx, network, scope, trigger)?)?;
        log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            &target,
            EventBody::BotMessaged {
                message_id: message_id.clone(),
                from: Sender {
                    conversation_id: scope.into(),
                    name: sender.name,
                },
                text: text.into(),
                hops,
            },
            vec![],
            at,
        )?;
        let owner = super::conversation_from(&tx, network, &target)?.and_then(|c| c.owner);
        let receipt = if owner == Some(identity.device_id()?) {
            super::scheduler::admit_from(
                &tx,
                network,
                &target,
                &bot_trigger,
                TriggerKind::BotMessage,
                &message_id,
            )?
        } else {
            super::scheduler::queue_delivery_from(&tx, network, &target, &bot_trigger)?
        };
        let message =
            message_from(&tx, network, &target, &message_id)?.context("message missing")?;
        tx.commit()?;
        Ok((message, receipt, true))
    }

    /// The user's pause, resume or deletion of a schedule from the web. Nothing is written when
    /// the schedule is already that way. Returns the event written, if any.
    #[expect(
        clippy::too_many_arguments,
        reason = "the schedule, the user's signed action and its time"
    )]
    pub fn control_schedule(
        &mut self,
        identity: &crate::crypto::LocalIdentity,
        network: &str,
        scope: &str,
        schedule_id: &str,
        action: ScheduleAction,
        attestation: Attestation,
        at: i64,
    ) -> Result<Option<String>> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(entry) = schedule_from(&tx, network, schedule_id)? else {
            ensure!(
                action == ScheduleAction::Delete,
                "schedule {schedule_id} not found"
            );
            return Ok(None);
        };
        ensure!(
            entry.conversation_id == scope,
            "schedule {schedule_id} belongs to another conversation"
        );
        let already = match action {
            ScheduleAction::Pause => !entry.schedule.enabled,
            ScheduleAction::Resume => entry.schedule.enabled,
            ScheduleAction::Delete => false,
        };
        if already {
            return Ok(None);
        }
        let event = log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            scope,
            EventBody::ScheduleControlled {
                schedule_id: schedule_id.into(),
                action,
                attestation: Some(attestation),
            },
            vec![],
            at,
        )?;
        tx.commit()?;
        Ok(Some(event.hash))
    }
}

/// A profile setting a chat change or the user's face pick writes; a model is per provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(tag = "field", rename_all = "snake_case")]
pub enum Setting {
    Name,
    Instructions,
    Skills,
    Avatar,
    Model { provider: Provider },
}

/// What a write built on: the value its writer saw, or, for the user's face pick, the version.
#[derive(Debug, PartialEq)]
enum Saw {
    Value(serde_json::Value),
    Version(Option<String>),
}

/// One write to a setting.
#[derive(Debug)]
struct SettingWrite {
    change_id: Option<String>,
    hash: String,
    by: String,
    saw: Saw,
    after: serde_json::Value,
}

/// A chat change another device replaced without seeing it; `value` is the setting now, set by `by`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Overwritten {
    pub change_id: String,
    #[serde(flatten)]
    pub setting: Setting,
    pub by: String,
    pub value: serde_json::Value,
}

/// `writes` are one setting's, in rank order; the last stands, as the projection has it. Walking
/// back from it, each write the next did not build on was replaced unseen, until one that was.
/// A write by the device whose write stands, or already at the value that stands, lost nothing.
fn overwritten_in(setting: Setting, writes: &[SettingWrite]) -> Vec<Overwritten> {
    let Some(stands) = writes.last() else {
        return vec![];
    };
    let mut lost = Vec::new();
    for pair in writes.windows(2).rev() {
        let (earlier, later) = (&pair[0], &pair[1]);
        let built_on = match &later.saw {
            Saw::Value(value) => *value == earlier.after,
            Saw::Version(version) => {
                version.as_deref() == Some(super::version(&earlier.hash).as_str())
            }
        };
        if built_on {
            break;
        }
        if let Some(change_id) = &earlier.change_id
            && earlier.by != stands.by
            && earlier.after != stands.after
        {
            lost.push(Overwritten {
                change_id: change_id.clone(),
                setting,
                by: stands.by.clone(),
                value: stands.after.clone(),
            });
        }
    }
    lost.reverse();
    lost
}

/// The chat changes of `scope`'s bot that another device replaced unseen. Read from the events
/// that count, so every replica holding them agrees and a replay or rebuild adds nothing.
pub(crate) fn overwritten_from(
    connection: &Connection,
    network: &str,
    scope: &str,
) -> Result<Vec<Overwritten>> {
    let mut statement = connection.prepare(
        "SELECT hash,envelope FROM events WHERE network_id=? AND scope_id=? AND effective=1
         AND kind IN ('bot_changed','bot_avatar_set') ORDER BY lamport,created_at_ms,hash",
    )?;
    let rows = statement.query_map(params![network, scope], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    fn json(value: impl serde::Serialize) -> serde_json::Result<serde_json::Value> {
        serde_json::to_value(value)
    }
    let mut writes = std::collections::BTreeMap::<Setting, Vec<SettingWrite>>::new();
    for row in rows {
        let (hash, envelope) = row?;
        let envelope: SignedEvent = serde_json::from_str(&envelope)?;
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        let edits = match &event.body {
            EventBody::BotChanged {
                change_id,
                change:
                    BotChange::Profile {
                        name,
                        instructions,
                        model,
                        skills,
                        avatar,
                    },
                ..
            } => [
                name.as_ref()
                    .map(|edit| (Setting::Name, json(&edit.before), json(&edit.after))),
                instructions
                    .as_ref()
                    .map(|edit| (Setting::Instructions, json(&edit.before), json(&edit.after))),
                model.as_ref().map(|edit| {
                    let setting = Setting::Model {
                        provider: edit.provider,
                    };
                    (setting, json(&edit.before), json(&edit.after))
                }),
                skills
                    .as_ref()
                    .map(|edit| (Setting::Skills, json(&edit.before), json(&edit.after))),
                avatar
                    .as_ref()
                    .map(|edit| (Setting::Avatar, json(edit.before), json(edit.after))),
            ]
            .into_iter()
            .flatten()
            .map(|(setting, before, after)| {
                Ok((
                    setting,
                    Some(change_id.clone()),
                    Saw::Value(before?),
                    after?,
                ))
            })
            .collect::<Result<Vec<_>>>()?,
            EventBody::BotAvatarSet {
                version, avatar, ..
            } => vec![(
                Setting::Avatar,
                None,
                Saw::Version(version.clone()),
                json(avatar)?,
            )],
            _ => continue,
        };
        let by = event
            .body
            .attestation()
            .map_or(&event.author, |attestation| &attestation.device_id);
        for (setting, change_id, saw, after) in edits {
            writes.entry(setting).or_default().push(SettingWrite {
                change_id,
                hash: hash.clone(),
                by: by.clone(),
                saw,
                after,
            });
        }
    }
    Ok(writes
        .into_iter()
        .flat_map(|(setting, writes)| overwritten_in(setting, &writes))
        .collect())
}
