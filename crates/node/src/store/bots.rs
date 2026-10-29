use super::{Store, authorize_node_from, create_conversation_from, log};
use crate::{crypto::LocalIdentity, model::*};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Component, Path};

const PREVIEW_CHARS: usize = 120;

/// The newest message of a bot's conversation, cut short for a conversation list.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LastMessage {
    pub text: String,
    pub author: String,
    pub role: String,
    pub created_at_ms: i64,
}

/// A message this bot sent another one; it is stored in the receiver's conversation.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SentBotMessage {
    pub message_id: String,
    pub to: String,
    pub text: String,
    pub created_at_ms: i64,
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.:".contains(character))
}

fn validate_definition(
    bot_id: &str,
    conversation_id: &str,
    name: &str,
    instructions: &str,
    project: Option<&BotProject>,
) -> Result<()> {
    ensure!(valid_id(bot_id), "invalid bot_id");
    ensure!(valid_id(conversation_id), "invalid conversation_id");
    validate_bot_name(name)?;
    validate_instructions(instructions)?;
    ensure!(
        serde_json::to_vec(&(name, instructions, project))?.len() <= 12000,
        "Bot definition is too large for the bot context"
    );
    if let Some(project) = project {
        ensure!(
            valid_id(&project.target_node),
            "invalid project target_node"
        );
        let cwd = Path::new(&project.cwd);
        ensure!(
            project.cwd.len() <= 4096
                && cwd.is_absolute()
                && !project.cwd.chars().any(char::is_control)
                && !cwd
                    .components()
                    .any(|part| matches!(part, Component::ParentDir | Component::CurDir)),
            "Bot project must use an absolute directory without parent components"
        );
    }
    Ok(())
}

type BotRow = (Bot, Option<String>, String, Option<String>, Option<String>);

fn bot_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<BotRow> {
    Ok((
        Bot {
            bot_id: r.get(0)?,
            conversation_id: r.get(1)?,
            name: r.get(2)?,
            instructions: r.get(3)?,
            project: None,
            models: BotModels::new(),
            skills: None,
            avatar: None,
            avatar_version: r
                .get::<_, Option<String>>(9)?
                .as_deref()
                .map(super::version),
            event_hash: r.get(5)?,
        },
        r.get(4)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
    ))
}

/// `BotCreated` gives the first name, instructions and project; a chat change, or the user's
/// `BotProjectSet`, with a later rank replaces each. The face is the latest by rank of a chat
/// change and the user's `BotAvatarSet`, `null` for the one the ID gives. A bot follows its device's model and uses every shared skill until the chat sets them;
/// an emptied model stays as a row so it outranks the one it replaced.
const MERGED_COLUMNS: &str = "b.bot_id,b.conversation_id,
    CASE WHEN n.rank>b.rank THEN n.value ELSE b.name END,
    CASE WHEN i.rank>b.rank THEN i.value ELSE b.instructions END,
    CASE WHEN p.rank>b.rank THEN p.value ELSE b.project END,b.event_hash,
    (SELECT json_group_object(m.provider,m.model) FROM bot_models m WHERE m.network_id=b.network_id AND m.conversation_id=b.conversation_id AND m.model<>''),
    s.value,a.value,a.rank";
const CREATED_COLUMNS: &str =
    "b.bot_id,b.conversation_id,b.name,b.instructions,b.project,b.event_hash,'{}',NULL,NULL,NULL";

/// Bots not deleted.
fn bots_where(
    connection: &Connection,
    network: &str,
    filter: &str,
    value: &str,
) -> Result<Vec<Bot>> {
    select_bots(
        connection,
        network,
        MERGED_COLUMNS,
        &format!(
            "{filter} AND NOT EXISTS (SELECT 1 FROM bot_deletions d WHERE d.network_id=b.network_id AND d.conversation_id=b.conversation_id)"
        ),
        value,
    )
}

pub(crate) fn bot_deleted_from(
    connection: &Connection,
    network: &str,
    scope: &str,
) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM bot_deletions WHERE network_id=? AND conversation_id=?",
            params![network, scope],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn select_bots(
    connection: &Connection,
    network: &str,
    columns: &str,
    filter: &str,
    value: &str,
) -> Result<Vec<Bot>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {columns} FROM bots b
         LEFT JOIN bot_profiles n ON n.network_id=b.network_id AND n.conversation_id=b.conversation_id AND n.field='name'
         LEFT JOIN bot_profiles i ON i.network_id=b.network_id AND i.conversation_id=b.conversation_id AND i.field='instructions'
         LEFT JOIN bot_profiles s ON s.network_id=b.network_id AND s.conversation_id=b.conversation_id AND s.field='skills'
         LEFT JOIN bot_profiles p ON p.network_id=b.network_id AND p.conversation_id=b.conversation_id AND p.field='project'
         LEFT JOIN bot_profiles a ON a.network_id=b.network_id AND a.conversation_id=b.conversation_id AND a.field='avatar'
         WHERE b.network_id=?1 {filter} ORDER BY b.bot_id"
    ))?;
    let library = connection
        .prepare("SELECT skill_id FROM skills WHERE network_id=? AND skill IS NOT NULL")?
        .query_map([network], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<std::collections::BTreeSet<_>>>()?;
    let rows = statement.query_map(params![network, value], bot_row)?;
    rows.map(|row| {
        let (mut bot, project, models, skills, avatar) = row?;
        bot.project = project
            .map(|p| serde_json::from_str::<Option<BotProject>>(&p))
            .transpose()?
            .flatten();
        bot.models = serde_json::from_str(&models)?;
        // A removed skill leaves every bot that listed it.
        bot.skills = skills
            .map(|s| serde_json::from_str::<SkillSet>(&s))
            .transpose()?
            .flatten()
            .map(|set| set.intersection(&library).cloned().collect());
        bot.avatar = super::optional_json::<Option<Avatar>>(avatar)?.flatten();
        Ok(bot)
    })
    .collect()
}

/// The bot ID and conversation one turn's `create_bot` call names, so a retry finds it again.
pub fn created_bot_id(scope: &str, trigger: &str, call_ref: &str) -> (String, String) {
    let digest = crate::crypto::digest(
        &serde_json::to_vec(&("create_bot", scope, trigger, call_ref)).expect("strings"),
    );
    (format!("bot-{digest}"), format!("c-{digest}"))
}

/// Bots with the name and instructions the chat last set.
pub(crate) fn bots_from(connection: &Connection, network: &str) -> Result<Vec<Bot>> {
    bots_where(connection, network, "AND ?2=''", "")
}

pub(crate) fn bot_for_conversation_from(
    connection: &Connection,
    network: &str,
    scope: &str,
) -> Result<Option<Bot>> {
    Ok(bots_where(connection, network, "AND b.conversation_id=?2", scope)?.pop())
}

impl Store {
    /// A Bot is a new conversation plus its definition; retries with the same definition return it.
    pub fn create_bot(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        spec: &BotCreateSpec,
        created_at_ms: i64,
    ) -> Result<Bot> {
        let conversation = spec.conversation_id.as_str();
        validate_definition(
            &spec.bot_id,
            conversation,
            &spec.name,
            &spec.instructions,
            spec.project.as_ref(),
        )?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize_node_from(&tx, network, &identity.device_id()?)?;
        // A retry compares the signed definition, not what the chat changed since.
        if let Some(existing) = select_bots(
            &tx,
            network,
            CREATED_COLUMNS,
            "AND b.bot_id=?2",
            &spec.bot_id,
        )?
        .pop()
        {
            ensure!(
                existing.conversation_id == conversation
                    && existing.name == spec.name
                    && existing.instructions == spec.instructions
                    && existing.project == spec.project,
                "Bot ID already exists with another definition"
            );
            ensure!(
                !bot_deleted_from(&tx, network, conversation)?,
                "this bot was deleted"
            );
            return Ok(existing);
        }
        ensure!(
            !log::scope_has_events(&tx, network, conversation)?,
            "a new Bot requires a new conversation scope"
        );
        create_conversation_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            conversation,
            &spec.name,
            spec.owner.clone(),
            created_at_ms,
        )?;
        log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            conversation,
            EventBody::BotCreated {
                bot_id: spec.bot_id.clone(),
                name: spec.name.clone(),
                instructions: spec.instructions.clone(),
                project: spec.project.clone(),
                attestation: Some(spec.attestation.clone()),
                cause: None,
            },
            vec![],
            created_at_ms,
        )?;
        let bot = bots_where(&tx, network, "AND b.conversation_id=?2", conversation)?
            .pop()
            .context("created Bot could not be projected")?;
        tx.commit()?;
        Ok(bot)
    }

    /// Bots with the name and instructions the chat last set.
    pub fn bots(&self, network: &str) -> Result<Vec<Bot>> {
        bots_from(&self.connection, network)
    }

    /// A bot another bot creates in a turn the user's own message started. Its conversation is
    /// scheduled by this Node, which schedules the creating one; the user's signed message
    /// stands behind every event. The same `call_ref` in the turn returns the same bot, and one
    /// message of the user creates at most `BOTS_PER_TURN`. Returns whether it is new.
    #[expect(
        clippy::too_many_arguments,
        reason = "the turn, its tool call and the definition"
    )]
    pub fn create_bot_in_turn(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        scope: &str,
        trigger: &str,
        call_ref: &str,
        definition: (&str, &str, Option<BotProject>),
        created_at_ms: i64,
    ) -> Result<(Bot, bool)> {
        let (name, instructions, project) = definition;
        vibeface_protocol::validate_id(call_ref).map_err(anyhow::Error::msg)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cause = super::bot_settings::user_cause(&tx, network, scope, trigger)?
            .context(super::bot_settings::NOT_FROM_USER)?;
        let (bot_id, conversation) = created_bot_id(scope, trigger, call_ref);
        validate_definition(&bot_id, &conversation, name, instructions, project.as_ref())?;
        if let Some(existing) =
            select_bots(&tx, network, CREATED_COLUMNS, "AND b.bot_id=?2", &bot_id)?.pop()
        {
            ensure!(
                existing.name == name
                    && existing.instructions == instructions
                    && existing.project == project,
                "call_ref {call_ref} already created another bot; use a new call_ref for a new bot"
            );
            ensure!(
                !bot_deleted_from(&tx, network, &conversation)?,
                "the bot this call_ref created was deleted"
            );
            return Ok((existing, false));
        }
        let created: usize = tx.query_row(
            "SELECT COUNT(*) FROM bots WHERE network_id=? AND cause=?",
            params![network, serde_json::to_string(&cause)?],
            |r| r.get(0),
        )?;
        ensure!(
            created < BOTS_PER_TURN,
            "one message creates at most {BOTS_PER_TURN} bots"
        );
        let local = identity.device_id()?;
        authorize_node_from(&tx, network, &local)?;
        for body in [
            EventBody::ConversationCreated { title: name.into() },
            EventBody::ConversationOwner {
                scheduler_node: local,
                epoch: 1,
                attestation: None,
                cause: Some(cause.clone()),
            },
            EventBody::BotCreated {
                bot_id: bot_id.clone(),
                name: name.into(),
                instructions: instructions.into(),
                project,
                attestation: None,
                cause: Some(cause.clone()),
            },
        ] {
            log::append_from(
                &tx,
                &self.blob_dir,
                identity,
                network,
                &conversation,
                body,
                vec![],
                created_at_ms,
            )?;
        }
        let bot = bot_for_conversation_from(&tx, network, &conversation)?
            .context("created bot could not be projected")?;
        tx.commit()?;
        Ok((bot, true))
    }

    /// Each bot conversation's newest message, by conversation; one indexed lookup per bot.
    pub fn last_messages(&self, network: &str) -> Result<BTreeMap<String, LastMessage>> {
        let mut statement = self.connection.prepare(
            "SELECT m.conversation_id,m.text,m.author,m.role,m.created_at_ms FROM bots b
             JOIN messages m ON m.network_id=b.network_id AND m.conversation_id=b.conversation_id
              AND m.rank=(SELECT max(x.rank) FROM messages x WHERE x.network_id=b.network_id AND x.conversation_id=b.conversation_id)
             WHERE b.network_id=?1",
        )?;
        let rows = statement.query_map(params![network], |r| {
            let text: String = r.get(1)?;
            let mut preview: String = text.chars().take(PREVIEW_CHARS).collect();
            if preview.len() < text.len() {
                preview.push('…');
            }
            Ok((
                r.get(0)?,
                LastMessage {
                    text: preview,
                    author: r.get(2)?,
                    role: r.get(3)?,
                    created_at_ms: r.get(4)?,
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// What the bot of `conversation` wrote to other bots, oldest first.
    pub fn sent_bot_messages(
        &self,
        network: &str,
        conversation: &str,
    ) -> Result<Vec<SentBotMessage>> {
        let mut statement = self.connection.prepare(
            "SELECT message_id,conversation_id,text,created_at_ms FROM messages
             WHERE network_id=?1 AND role='bot' AND json_extract(sender,'$.conversation_id')=?2
             ORDER BY created_at_ms,rank",
        )?;
        let rows = statement.query_map(params![network, conversation], |r| {
            Ok(SentBotMessage {
                message_id: r.get(0)?,
                to: r.get(1)?,
                text: r.get(2)?,
                created_at_ms: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn bot_for_conversation(&self, network: &str, scope: &str) -> Result<Option<Bot>> {
        bot_for_conversation_from(&self.connection, network, scope)
    }

    pub fn primary_bot(&self, network: &str) -> Result<Option<ScopeId>> {
        primary_bot_from(&self.connection, network)
    }

    pub fn bot_deleted(&self, network: &str, scope: &str) -> Result<bool> {
        bot_deleted_from(&self.connection, network, scope)
    }

    /// The user deletes the bot of `conversation` from the web. Returns the event written, or
    /// `None` when it is already deleted.
    pub fn delete_bot(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        conversation: &str,
        attestation: Attestation,
        at: i64,
    ) -> Result<Option<SignedEvent>> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if bot_deleted_from(&tx, network, conversation)? {
            return Ok(None);
        }
        ensure!(
            bot_for_conversation_from(&tx, network, conversation)?.is_some(),
            "no such bot"
        );
        let event = log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            conversation,
            EventBody::BotDeleted {
                attestation: Some(attestation),
                cause: None,
            },
            vec![],
            at,
        )?;
        tx.commit()?;
        Ok(Some(event))
    }

    /// A bot deletes another bot in a turn the user's own message started. A bot named again
    /// after it is gone, as a retry does, writes nothing. Returns the deleted bot's
    /// conversation and the event written, if any.
    pub fn delete_bot_in_turn(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        scope: &str,
        trigger: &str,
        wanted: &str,
        at: i64,
    ) -> Result<(ScopeId, Option<SignedEvent>)> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cause = super::bot_settings::user_cause(&tx, network, scope, trigger)?
            .context(super::bot_settings::NOT_FROM_USER)?;
        let target = match super::bot_settings::find_bot(&tx, network, wanted) {
            Ok(bot) => bot.conversation_id,
            Err(error) => {
                let name = wanted.trim().to_lowercase();
                let mut gone = select_bots(&tx, network, MERGED_COLUMNS, "AND ?2=''", "")?
                    .into_iter()
                    .filter(|bot| {
                        bot.conversation_id == wanted
                            || bot.bot_id == wanted
                            || bot.name.trim().to_lowercase() == name
                    });
                match (gone.next(), gone.next()) {
                    (Some(bot), None) if bot_deleted_from(&tx, network, &bot.conversation_id)? => {
                        return Ok((bot.conversation_id, None));
                    }
                    _ => return Err(error),
                }
            }
        };
        ensure!(
            target != scope,
            "a bot does not delete itself; the user deletes it from its panel"
        );
        let event = log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            &target,
            EventBody::BotDeleted {
                attestation: None,
                cause: Some(cause),
            },
            vec![],
            at,
        )?;
        tx.commit()?;
        Ok((target, Some(event)))
    }
}

/// The bot that judges what comes in without an owner: the user's latest choice while it
/// is a bot, otherwise the first bot created. Deleting the primary bot hands the role to the
/// earliest bot left.
pub(crate) fn primary_bot_from(connection: &Connection, network: &str) -> Result<Option<ScopeId>> {
    let chosen = connection
        .query_row(
            "SELECT envelope FROM events WHERE network_id=? AND scope_id=? AND effective=1
             ORDER BY lamport DESC,created_at_ms DESC,hash DESC LIMIT 1",
            params![network, BOTS_SCOPE],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|envelope| -> Result<Option<ScopeId>> {
            let envelope: SignedEvent = serde_json::from_str(&envelope)?;
            let event: Event = serde_json::from_str(&envelope.canonical_body)?;
            Ok(match event.body {
                EventBody::PrimaryBotSet {
                    conversation_id, ..
                } => Some(conversation_id),
                _ => None,
            })
        })
        .transpose()?
        .flatten();
    Ok(connection
        .query_row(
            "SELECT conversation_id FROM bots b WHERE network_id=?1
             AND NOT EXISTS (SELECT 1 FROM bot_deletions d WHERE d.network_id=b.network_id AND d.conversation_id=b.conversation_id)
             ORDER BY conversation_id IS ?2 DESC, rank LIMIT 1",
            params![network, chosen],
            |r| r.get(0),
        )
        .optional()?)
}
