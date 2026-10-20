//! What the chat changes about assistants: a profile's name and instructions, the skill library
//! every assistant shares and each assistant's routines. Every change is one `AssistantChanged`
//! the owner writes in a turn the user's own signed message started.
use super::{Store, bots::bot_for_conversation_from, log, message_from};
use crate::{crypto::digest, model::*};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};

pub const NOT_FROM_USER: &str =
    "assistant settings change only when the user's own message asks for it";

fn change_digest(conversation: &str, trigger: &str, call_ref: &str) -> String {
    digest(
        &serde_json::to_vec(&("assistant_change", conversation, trigger, call_ref))
            .expect("strings"),
    )
}

/// What a settings tool asked for; the writer adds the values it replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeRequest {
    Profile {
        name: Option<String>,
        instructions: Option<String>,
    },
    SaveSkill {
        skill_id: Option<String>,
        skill: Skill,
    },
    RemoveSkill {
        skill_id: String,
    },
    SaveRoutine {
        routine_id: Option<String>,
        name: String,
        prompt: String,
        first_run_at_ms: i64,
        every_minutes: u32,
        enabled: Option<bool>,
    },
    RemoveRoutine {
        routine_id: String,
    },
}

impl ChangeRequest {
    /// Whether `change` already sets what this request asks for. A retry compares this, because
    /// the first write changed the values a new `before` would read.
    fn matches(&self, change: &AssistantChange) -> bool {
        let after = |edit: &Option<Edit>| edit.as_ref().map(|edit| edit.after.clone());
        match (self, change) {
            (
                Self::Profile { name, instructions },
                AssistantChange::Profile {
                    name: changed_name,
                    instructions: changed_instructions,
                },
            ) => *name == after(changed_name) && *instructions == after(changed_instructions),
            (
                Self::SaveSkill { skill_id, skill },
                AssistantChange::Skill {
                    skill_id: id,
                    after: Some(saved),
                    ..
                },
            ) => skill_id.as_ref().is_none_or(|wanted| wanted == id) && skill == saved,
            (
                Self::RemoveSkill { skill_id },
                AssistantChange::Skill {
                    skill_id: id,
                    after: None,
                    ..
                },
            ) => skill_id == id,
            (
                Self::SaveRoutine {
                    routine_id,
                    name,
                    prompt,
                    first_run_at_ms,
                    every_minutes,
                    enabled,
                },
                AssistantChange::Routine {
                    routine_id: id,
                    after: Some(saved),
                    ..
                },
            ) => {
                routine_id.as_ref().is_none_or(|wanted| wanted == id)
                    && *name == saved.name
                    && *prompt == saved.prompt
                    && *first_run_at_ms == saved.first_run_at_ms
                    && *every_minutes == saved.every_minutes
                    && enabled.is_none_or(|enabled| enabled == saved.enabled)
            }
            (
                Self::RemoveRoutine { routine_id },
                AssistantChange::Routine {
                    routine_id: id,
                    after: None,
                    ..
                },
            ) => routine_id == id,
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

/// Routines not removed, of one conversation or of all, by name.
pub(crate) fn routines_from(
    connection: &Connection,
    network: &str,
    conversation: Option<&str>,
) -> Result<Vec<RoutineEntry>> {
    let mut statement = connection.prepare(
        "SELECT routine_id,conversation_id,routine,changed_at_ms FROM routines
         WHERE network_id=?1 AND routine IS NOT NULL AND (?2 IS NULL OR conversation_id=?2)",
    )?;
    let rows = statement.query_map(params![network, conversation], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    let mut routines = rows
        .map(|row| {
            let (routine_id, conversation_id, routine, changed_at_ms) = row?;
            Ok(RoutineEntry {
                routine_id,
                conversation_id,
                routine: serde_json::from_str(&routine)?,
                changed_at_ms,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    routines
        .sort_by_cached_key(|entry| (entry.routine.name.to_lowercase(), entry.routine_id.clone()));
    Ok(routines)
}

pub(crate) fn routine_from(
    connection: &Connection,
    network: &str,
    id: &str,
) -> Result<Option<RoutineEntry>> {
    let row: Option<(String, Option<String>, i64)> = connection
        .query_row(
            "SELECT conversation_id,routine,changed_at_ms FROM routines WHERE network_id=? AND routine_id=?",
            params![network, id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((conversation_id, Some(routine), changed_at_ms)) = row else {
        return Ok(None);
    };
    Ok(Some(RoutineEntry {
        routine_id: id.into(),
        conversation_id,
        routine: serde_json::from_str(&routine)?,
        changed_at_ms,
    }))
}

const CHANGE_COLUMNS: &str = "change_id,cause,event_hash,created_at_ms,change";

fn change_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(ChangeRecord, String)> {
    Ok((
        ChangeRecord {
            change_id: r.get(0)?,
            cause: r.get(1)?,
            event_hash: r.get(2)?,
            created_at_ms: r.get(3)?,
            change: AssistantChange::Profile {
                name: None,
                instructions: None,
            },
        },
        r.get(4)?,
    ))
}

fn with_change((mut record, change): (ChangeRecord, String)) -> Result<ChangeRecord> {
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
            &format!(
                "SELECT {CHANGE_COLUMNS} FROM assistant_changes WHERE network_id=? AND change_id=?"
            ),
            params![network, change_id],
            change_row,
        )
        .optional()?
        .map(with_change)
        .transpose()
}

/// Turns the request into a change against the current projection. A change must set something
/// new, and an update or removal must name something that exists here.
fn resolve(
    connection: &Connection,
    network: &str,
    scope: &str,
    digest: &str,
    request: ChangeRequest,
) -> Result<AssistantChange> {
    let change = match request {
        ChangeRequest::Profile { name, instructions } => {
            ensure!(
                name.is_some() || instructions.is_some(),
                "name or instructions is required"
            );
            let bot = bot_for_conversation_from(connection, network, scope)?
                .context("this conversation has no assistant profile to change")?;
            if let Some(name) = &name {
                validate_bot_name(name)?;
                ensure!(
                    *name != bot.name,
                    "the assistant is already named {name}; nothing to change"
                );
            }
            if let Some(instructions) = &instructions {
                validate_instructions(instructions)?;
                ensure!(
                    *instructions != bot.instructions,
                    "the instructions already read like this; nothing to change"
                );
            }
            AssistantChange::Profile {
                name: name.map(|after| Edit {
                    before: bot.name,
                    after,
                }),
                instructions: instructions.map(|after| Edit {
                    before: bot.instructions,
                    after,
                }),
            }
        }
        ChangeRequest::SaveSkill { skill_id, skill } => {
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
            AssistantChange::Skill {
                skill_id,
                before,
                after: Some(skill),
            }
        }
        ChangeRequest::RemoveSkill { skill_id } => {
            let before = skill_from(connection, network, &skill_id)?
                .with_context(|| format!("skill {skill_id} not found"))?;
            AssistantChange::Skill {
                skill_id,
                before: Some(before),
                after: None,
            }
        }
        ChangeRequest::SaveRoutine {
            routine_id,
            name,
            prompt,
            first_run_at_ms,
            every_minutes,
            enabled,
        } => {
            let (routine_id, before) = match routine_id {
                Some(id) => {
                    let before = own_routine(connection, network, scope, &id)?;
                    (id, Some(before))
                }
                None => (format!("routine-{digest}"), None),
            };
            let after = Routine {
                name,
                prompt,
                first_run_at_ms,
                every_minutes,
                enabled: enabled.unwrap_or(before.as_ref().is_none_or(|before| before.enabled)),
            };
            after.validate()?;
            ensure!(
                before.as_ref() != Some(&after),
                "the routine already has these settings; nothing to change"
            );
            AssistantChange::Routine {
                routine_id,
                before,
                after: Some(after),
            }
        }
        ChangeRequest::RemoveRoutine { routine_id } => {
            let before = own_routine(connection, network, scope, &routine_id)?;
            AssistantChange::Routine {
                routine_id,
                before: Some(before),
                after: None,
            }
        }
    };
    change.validate()?;
    Ok(change)
}

/// A routine belongs to the assistant that created it; others cannot change it.
fn own_routine(connection: &Connection, network: &str, scope: &str, id: &str) -> Result<Routine> {
    Ok(routine_from(connection, network, id)?
        .filter(|entry| entry.conversation_id == scope)
        .with_context(|| format!("routine {id} not found for this assistant"))?
        .routine)
}

impl Store {
    /// Writes the change a turn's tool call asked for. The same `call_ref` in the same turn
    /// returns the stored change and writes nothing; a different request under it is refused.
    /// Returns whether an event was written.
    #[expect(
        clippy::too_many_arguments,
        reason = "the turn, its tool call and the change"
    )]
    pub fn change_assistant(
        &mut self,
        identity: &crate::crypto::LocalIdentity,
        network: &str,
        scope: &str,
        trigger: &str,
        call_ref: &str,
        request: ChangeRequest,
        at: i64,
    ) -> Result<(ChangeRecord, bool)> {
        vibeface_protocol::validate_id(call_ref).map_err(anyhow::Error::msg)?;
        let cause = trigger.strip_prefix("message:").context(NOT_FROM_USER)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let message = message_from(&tx, network, scope, cause)?
            .context("the user's message is not stored on this device")?;
        ensure!(message.role == Role::User, NOT_FROM_USER);
        let digest = change_digest(scope, trigger, call_ref);
        let change_id = format!("change-{digest}");
        if let Some(existing) = change_from(&tx, network, &change_id)? {
            ensure!(
                request.matches(&existing.change),
                "call_ref {call_ref} already made a different change; use a new call_ref for a new change"
            );
            return Ok((existing, false));
        }
        let change = resolve(&tx, network, scope, &digest, request)?;
        log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            scope,
            EventBody::AssistantChanged {
                change_id: change_id.clone(),
                cause: cause.into(),
                change,
            },
            vec![],
            at,
        )?;
        let record = change_from(&tx, network, &change_id)?.context("change was not projected")?;
        tx.commit()?;
        Ok((record, true))
    }

    pub fn assistant_change(&self, network: &str, change_id: &str) -> Result<Option<ChangeRecord>> {
        change_from(&self.connection, network, change_id)
    }

    /// One conversation's changes in shared order, each at its earliest copy.
    pub fn assistant_changes(&self, network: &str, scope: &str) -> Result<Vec<ChangeRecord>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {CHANGE_COLUMNS} FROM assistant_changes WHERE network_id=? AND conversation_id=? ORDER BY rank"
        ))?;
        let rows = statement.query_map(params![network, scope], change_row)?;
        rows.map(|row| with_change(row?)).collect()
    }

    pub fn skills(&self, network: &str) -> Result<Vec<SkillEntry>> {
        skills_from(&self.connection, network)
    }

    pub fn skill(&self, network: &str, skill_id: &str) -> Result<Option<Skill>> {
        skill_from(&self.connection, network, skill_id)
    }

    pub fn routines(&self, network: &str, scope: &str) -> Result<Vec<RoutineEntry>> {
        routines_from(&self.connection, network, Some(scope))
    }

    pub fn routine(&self, network: &str, routine_id: &str) -> Result<Option<RoutineEntry>> {
        routine_from(&self.connection, network, routine_id)
    }
}
