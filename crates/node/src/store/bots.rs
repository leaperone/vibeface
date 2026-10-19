use super::{Store, authorize_node_from, create_conversation_from, log};
use crate::{crypto::LocalIdentity, model::*};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, TransactionBehavior, params};
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
    ensure!(
        !name.trim().is_empty() && name.len() <= 512 && !name.chars().any(char::is_control),
        "invalid Bot name"
    );
    ensure!(
        instructions.len() <= 8192,
        "Bot instructions must be at most 8192 bytes"
    );
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

const BOT_COLUMNS: &str = "bot_id,conversation_id,name,instructions,project,event_hash";

fn bots_where(
    connection: &Connection,
    network: &str,
    filter: &str,
    value: &str,
) -> Result<Vec<Bot>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {BOT_COLUMNS} FROM bots WHERE network_id=?1 {filter} ORDER BY bot_id"
    ))?;
    let rows = statement.query_map(params![network, value], bot_row)?;
    rows.map(|row| {
        let (mut bot, project) = row?;
        bot.project = project.map(|p| serde_json::from_str(&p)).transpose()?;
        Ok(bot)
    })
    .collect()
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
        authorize_node_from(&tx, network, &identity.peer_id()?, Some(NodeRole::Gateway))?;
        if let Some(existing) = bots_where(&tx, network, "AND bot_id=?2", &spec.bot_id)?.pop() {
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
            },
            vec![],
            created_at_ms,
        )?;
        let bot = bots_where(&tx, network, "AND conversation_id=?2", conversation)?
            .pop()
            .context("created Bot could not be projected")?;
        tx.commit()?;
        Ok(bot)
    }

    pub fn bots(&self, network: &str) -> Result<Vec<Bot>> {
        bots_where(&self.connection, network, "AND ?2=''", "")
    }

    pub fn bot_for_conversation(&self, network: &str, scope: &str) -> Result<Option<Bot>> {
        Ok(bots_where(&self.connection, network, "AND conversation_id=?2", scope)?.pop())
    }
}
