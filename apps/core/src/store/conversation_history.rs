//! Keyset reads of the shared conversation projection. No reader updates projection state.
use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use vibeface_protocol::history::{
    HISTORY_MAX_LIMIT, HISTORY_PAGE_BYTES, HISTORY_RESPONSE_BYTES, HistoryPage, HistorySection,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HistoryBoundary {
    pub rank: String,
    pub id: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    network: String,
    conversation: String,
    section: HistorySection,
    authority: String,
    before: Option<HistoryBoundary>,
}

pub(super) fn history_page<T: Serialize>(
    network: &str,
    conversation: &str,
    section: HistorySection,
    authority: String,
    boundary: Option<HistoryBoundary>,
    items: Vec<T>,
    has_more: bool,
) -> Result<HistoryPage<T>> {
    let next_cursor = if has_more {
        Some(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Cursor {
            network: network.into(),
            conversation: conversation.into(),
            section,
            authority: authority.clone(),
            before: Some(boundary.context("history continuation has no boundary")?),
        })?))
    } else {
        None
    };
    let page = HistoryPage {
        conversation_id: conversation.into(),
        section,
        history_authority: authority,
        items,
        next_cursor,
        complete: !has_more,
    };
    ensure!(serde_json::to_vec(&page)?.len() < HISTORY_RESPONSE_BYTES,
        "history record exceeds the response byte limit");
    Ok(page)
}

/// Runs before applying an effective event, in that event's projection transaction.
/// This local cache revision also records late associations, so it need not converge
/// between replicas. The empty scope tracks changes across conversations.
pub(super) fn advance_revision(connection: &Connection, network: &str, event: &Event) -> Result<()> {
    use crate::workspace::WorkspaceCommand;
    let scope = event.scope_id.as_str();
    let affected = match &event.body {
        EventBody::BotDeleted { .. } | EventBody::SessionAttributed { .. }
        | EventBody::SessionMuted { .. } | EventBody::WorkDispatched { .. } => Some(""),
        EventBody::WorkspaceCommanded { command, .. } =>
            (!matches!(command, WorkspaceCommand::PostChannelMessage { .. })).then_some(""),
        EventBody::WorkProgress { work_id, thread_id: Some(thread), .. } => {
            let previous: Option<String> = connection.query_row(
                "SELECT thread_id FROM works WHERE network_id=? AND work_id=?",
                params![network, work_id], |row| row.get(0),
            ).optional()?.flatten();
            (previous.as_deref() != Some(thread.as_str())).then_some("")
        }
        EventBody::BotChanged { change: BotChange::Skill { .. }, .. } => Some(""),
        EventBody::ConversationOwner { .. } | EventBody::BotCreated { .. }
        | EventBody::MessageSteered { .. } | EventBody::QuestionClosed { .. }
        | EventBody::BotChanged { .. } | EventBody::BotAvatarSet { .. }
        | EventBody::ScheduleControlled { .. } | EventBody::MemoryControlled { .. } => Some(scope),
        EventBody::InputPosted { message_id, .. } | EventBody::MessagePosted { message_id, .. }
        | EventBody::BotMessaged { message_id, .. } => {
            let replacing: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE network_id=?1 AND conversation_id=?2 AND message_id=?3)
                 OR EXISTS(SELECT 1 FROM messages WHERE network_id=?1 AND conversation_id=?2 AND in_reply_to=?3)",
                params![network, scope, message_id], |row| row.get(0),
            )?;
            replacing.then_some(scope)
        }
        EventBody::QuestionAsked { question_id, .. } => {
            let replacing: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM questions WHERE network_id=?1 AND conversation_id=?2 AND question_id=?3)
                 OR EXISTS(SELECT 1 FROM messages WHERE network_id=?1 AND conversation_id=?2 AND answers=?3)",
                params![network, scope, question_id], |row| row.get(0),
            )?;
            replacing.then_some(scope)
        }
        _ => None,
    };
    if let Some(scope) = affected { bump_revision(connection, network, scope)?; }
    Ok(())
}

/// Cache metadata survives business projection rebuilds. Increment before clearing rows so
/// a replay can never reuse an old cursor's authority, even if the projected counts match.
pub(super) fn invalidate_all(connection: &Connection, network: &str) -> Result<()> {
    bump_revision(connection, network, "")
}

fn bump_revision(connection: &Connection, network: &str, scope: &str) -> Result<()> {
    connection.execute(
        "INSERT INTO conversation_history_revisions(network_id,conversation_id,revision) VALUES(?,?,1)
         ON CONFLICT(network_id,conversation_id) DO UPDATE SET revision=revision+1",
        params![network, scope],
    )?;
    Ok(())
}

impl Store {
    pub(super) fn history_cursor(
        &self,
        network: &str,
        conversation: &str,
        section: HistorySection,
        cursor: Option<&str>,
    ) -> Result<(String, Option<HistoryBoundary>)> {
        // Ordinary appends leave this unchanged. Governance changes and new fork evidence
        // invalidate all old windows, including rows whose keyset boundary still exists.
        let bundle: String = self.connection.query_row(
            "SELECT bundle FROM networks WHERE network_id=?", [network], |row| row.get(0),
        )?;
        let forks: (u64, Option<String>) = self.connection.query_row(
            "SELECT count(*),max(hash) FROM events WHERE network_id=? AND main=0",
            [network], |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let revisions: (u64, u64) = self.connection.query_row(
            "SELECT COALESCE((SELECT revision FROM conversation_history_revisions WHERE network_id=?1 AND conversation_id=''),0),
                    COALESCE((SELECT revision FROM conversation_history_revisions WHERE network_id=?1 AND conversation_id=?2),0)",
            params![network, conversation], |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let authority = digest(&serde_json::to_vec(&(bundle, forks, revisions))?);
        let before = cursor.map(|cursor| -> Result<Option<HistoryBoundary>> {
            ensure!(cursor.len() <= 8192, "history cursor is too large");
            let cursor: Cursor = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(cursor)
                .context("invalid history cursor")?).context("invalid history cursor")?;
            ensure!(cursor.network == network && cursor.conversation == conversation
                && cursor.section == section, "history cursor belongs to another scope");
            ensure!(cursor.authority == authority, "history changed; refresh the conversation");
            if let Some(before) = &cursor.before {
                ensure!(!before.rank.is_empty() && !before.id.is_empty(), "history cursor has an empty boundary");
            }
            Ok(cursor.before)
        }).transpose()?.flatten();
        Ok((authority, before))
    }

    pub(crate) fn conversation_history_authority(&self, network: &str, conversation: &str) -> Result<String> {
        Ok(self.history_cursor(network, conversation, HistorySection::Messages, None)?.0)
    }

    pub(crate) fn conversation_history_start_cursor(
        &self, network: &str, conversation: &str, section: HistorySection,
    ) -> Result<String> {
        let (authority, _) = self.history_cursor(network, conversation, section, None)?;
        Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Cursor {
            network: network.into(), conversation: conversation.into(), section, authority, before: None,
        })?))
    }

    pub fn conversation_messages_page(
        &self,
        network: &str,
        conversation: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage<Message>> {
        ensure!((1..=HISTORY_MAX_LIMIT).contains(&limit), "history limit must be 1 to 100");
        let (authority, before) = self.history_cursor(network, conversation, HistorySection::Messages, cursor)?;
        let seek = if before.is_some() { " AND (rank,message_id)<(?3,?4)" } else { "" };
        let mut statement = self.connection.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages
             WHERE network_id=?1 AND conversation_id=?2
             {seek}
             ORDER BY rank DESC,message_id DESC LIMIT ?5"
        ))?;
        let mut rows = statement.query(params![network, conversation,
            before.as_ref().map(|b| &b.rank), before.as_ref().map(|b| &b.id), limit + 1])?;
        let mut items = Vec::new();
        let mut boundary = before;
        let mut bytes = 0;
        let mut scanned = 0;
        let mut has_more = false;
        while let Some(row) = rows.next()? {
            if scanned == limit {
                has_more = true;
                break;
            }
            let message = message_row(row)??;
            // This is precisely target_from's first branch, using the target already read
            // with this row. Unknown legacy/reply targets retain the authoritative resolver.
            let main = match &message.target {
                Some(crate::workspace::InputTarget::BotMain { .. }) => true,
                Some(_) => false,
                None => scope_projection::message_is_main_from(
                    &self.connection, network, conversation, &message.message_id,
                )?,
            };
            if main {
                let size = serde_json::to_vec(&message)?.len() + 1;
                if !items.is_empty() && bytes + size > HISTORY_PAGE_BYTES {
                    has_more = true;
                    break;
                }
                bytes += size;
            }
            boundary = Some(HistoryBoundary { rank: message.rank.clone(), id: message.message_id.clone() });
            scanned += 1;
            if main {
                items.push(message);
            }
        }
        items.reverse();
        history_page(network, conversation, HistorySection::Messages, authority, boundary, items, has_more)
    }
}
