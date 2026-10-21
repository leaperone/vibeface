use super::{Store, authorize_node_from, create_conversation_from, log};
use crate::{crypto::LocalIdentity, model::*};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::path::{Component, Path};

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
        "Bot definition is too large for the assistant context"
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

fn bot_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(Bot, Option<String>)> {
    Ok((
        Bot {
            bot_id: r.get(0)?,
            conversation_id: r.get(1)?,
            name: r.get(2)?,
            instructions: r.get(3)?,
            project: None,
            event_hash: r.get(5)?,
        },
        r.get(4)?,
    ))
}

/// `BotCreated` gives the first name and instructions; a chat change with a later rank replaces each.
const MERGED_COLUMNS: &str = "b.bot_id,b.conversation_id,
    CASE WHEN n.rank>b.rank THEN n.value ELSE b.name END,
    CASE WHEN i.rank>b.rank THEN i.value ELSE b.instructions END,
    b.project,b.event_hash";
const CREATED_COLUMNS: &str =
    "b.bot_id,b.conversation_id,b.name,b.instructions,b.project,b.event_hash";

fn bots_where(
    connection: &Connection,
    network: &str,
    filter: &str,
    value: &str,
) -> Result<Vec<Bot>> {
    select_bots(connection, network, MERGED_COLUMNS, filter, value)
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
         LEFT JOIN assistant_profiles n ON n.network_id=b.network_id AND n.conversation_id=b.conversation_id AND n.field='name'
         LEFT JOIN assistant_profiles i ON i.network_id=b.network_id AND i.conversation_id=b.conversation_id AND i.field='instructions'
         WHERE b.network_id=?1 {filter} ORDER BY b.bot_id"
    ))?;
    let rows = statement.query_map(params![network, value], bot_row)?;
    rows.map(|row| {
        let (mut bot, project) = row?;
        bot.project = project.map(|p| serde_json::from_str(&p)).transpose()?;
        Ok(bot)
    })
    .collect()
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
        bots_where(&self.connection, network, "AND ?2=''", "")
    }

    pub fn bot_for_conversation(&self, network: &str, scope: &str) -> Result<Option<Bot>> {
        bot_for_conversation_from(&self.connection, network, scope)
    }

    pub fn primary_assistant(&self, network: &str) -> Result<Option<ScopeId>> {
        primary_assistant_from(&self.connection, network)
    }
}

/// The assistant that judges what comes in without an owner: the user's latest choice while it
/// is an assistant, otherwise the first assistant created.
pub(crate) fn primary_assistant_from(
    connection: &Connection,
    network: &str,
) -> Result<Option<ScopeId>> {
    let chosen = connection
        .query_row(
            "SELECT envelope FROM events WHERE network_id=? AND scope_id=? AND effective=1
             ORDER BY lamport DESC,created_at_ms DESC,hash DESC LIMIT 1",
            params![network, ASSISTANTS_SCOPE],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|envelope| -> Result<Option<ScopeId>> {
            let envelope: SignedEvent = serde_json::from_str(&envelope)?;
            let event: Event = serde_json::from_str(&envelope.canonical_body)?;
            Ok(match event.body {
                EventBody::PrimaryAssistantSet {
                    conversation_id, ..
                } => Some(conversation_id),
                _ => None,
            })
        })
        .transpose()?
        .flatten();
    Ok(connection
        .query_row(
            "SELECT conversation_id FROM bots WHERE network_id=?1 ORDER BY conversation_id IS ?2 DESC, rank LIMIT 1",
            params![network, chosen],
            |r| r.get(0),
        )
        .optional()?)
}
