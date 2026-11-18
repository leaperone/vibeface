//! Scope-bound, authenticated cursors. Reads never mutate projections or replay events.
use super::{
    query::{history_speaker, read_graph},
    validation::valid_id,
};
use crate::{
    model::EventBody,
    store::{Store, log},
    workspace::*,
};
use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore as _, rngs::OsRng};
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use vibeface_protocol::{
    history::{HISTORY_PAGE_BYTES, HISTORY_RESPONSE_BYTES},
    message_search::*,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    binding: String,
    before: String,
    mac: String,
}

// Shared by all read-pool connections. Restarting the Node expires cursors and the
// caller must refresh its first page; no cursor secret is written by a read path.
fn cursor_mac(binding: &str, before: &str) -> Result<String> {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    let key = KEY.get_or_init(|| {
        let mut key = [0; 32];
        OsRng.fill_bytes(&mut key);
        key
    });
    Ok(hex::encode(crate::crypto::hmac_sha256(
        key,
        &serde_json::to_vec(&("vibeface:message-history-cursor:1", binding, before))?,
    )))
}

fn read_cursor(cursor: Option<&str>, binding: &str) -> Result<Option<String>> {
    cursor
        .map(|cursor| {
            ensure!(cursor.len() <= 8192, "history cursor is too large");
            let cursor: Cursor = serde_json::from_slice(
                &URL_SAFE_NO_PAD
                    .decode(cursor)
                    .context("invalid history cursor")?,
            )
            .context("invalid history cursor")?;
            ensure!(
                cursor.binding == binding,
                "history scope changed; refresh history"
            );
            let expected = cursor_mac(&cursor.binding, &cursor.before)?;
            ensure!(
                expected.len() == cursor.mac.len()
                    && expected
                        .bytes()
                        .zip(cursor.mac.bytes())
                        .fold(0, |difference, (a, b)| difference | (a ^ b))
                        == 0,
                "invalid history cursor signature"
            );
            ensure!(
                !cursor.before.is_empty(),
                "history cursor has an empty boundary"
            );
            Ok(cursor.before)
        })
        .transpose()
}

// Candidates are in the same total event order as workspace_history. A byte bound
// may shorten a page; its continuation still names the last returned event.
fn page<T: Serialize>(
    candidates: Vec<(String, T)>,
    binding: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<(Vec<T>, Option<String>)> {
    let before = read_cursor(cursor, binding)?;
    if let Some(before) = &before {
        ensure!(
            candidates.iter().any(|(rank, _)| rank == before),
            "history changed; refresh history"
        );
    }
    let mut items = Vec::new();
    let mut bytes = 0;
    let mut boundary = None;
    let mut more = false;
    for (rank, item) in candidates.into_iter().rev() {
        if before.as_ref().is_some_and(|before| &rank >= before) {
            continue;
        }
        let size = serde_json::to_vec(&item)?.len() + 1;
        if items.len() == limit || (!items.is_empty() && bytes + size > HISTORY_PAGE_BYTES) {
            more = true;
            break;
        }
        ensure!(
            size < HISTORY_RESPONSE_BYTES - 16384,
            "history record exceeds response limit"
        );
        bytes += size;
        boundary = Some(rank);
        items.push(item);
    }
    let next_cursor = if more {
        let before = boundary.context("history continuation has no boundary")?;
        Some(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Cursor {
            binding: binding.into(),
            mac: cursor_mac(binding, &before)?,
            before,
        })?))
    } else {
        None
    };
    items.reverse();
    Ok((items, next_cursor))
}

impl Store {
    fn workspace_projection_authority(&self, network: &str) -> Result<String> {
        let authority = self.conversation_history_authority(network, "")?;
        // UI history spans messages in several Bot conversations. A competing
        // winner/replacement can change one without changing the workspace graph.
        // Ordinary new message IDs leave these projection revisions unchanged.
        let mut statement = self.connection.prepare("SELECT conversation_id,revision FROM conversation_history_revisions WHERE network_id=? ORDER BY conversation_id")?;
        let revisions = statement
            .query_map([network], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(crate::crypto::digest(&serde_json::to_vec(&(
            authority, revisions,
        ))?))
    }

    fn workspace_history_item_visible(
        &self,
        network: &str,
        event: &WorkspaceHistoryItem,
    ) -> Result<bool> {
        match &event.body {
            EventBody::InputPosted { message_id, .. }
            | EventBody::MessagePosted { message_id, .. } => Ok(self
                .message(network, &event.scope_id, message_id)?
                .is_some_and(|message| message.event_hash == event.event_hash)),
            _ => Ok(true),
        }
    }

    pub fn workspace_history(
        &self,
        network: &str,
        channel_id: Option<&str>,
        task_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<WorkspaceHistory> {
        ensure!((1..=512).contains(&limit), "workspace limit must be 1-512");
        let authority = self.workspace_projection_authority(network)?;
        let binding = crate::crypto::digest(&serde_json::to_vec(&(
            "workspace_history",
            network,
            channel_id,
            task_id,
            &authority,
        ))?);
        let mut visible = Vec::new();
        for (rank, event) in self.workspace_history_events(network, channel_id, task_id)? {
            if self.workspace_history_item_visible(network, &event)? {
                visible.push((rank, event));
            }
        }
        let (events, next_cursor) = page(visible, &binding, cursor, limit)?;
        let complete = next_cursor.is_none();
        Ok(WorkspaceHistory {
            history_authority: authority,
            events,
            next_cursor,
            complete,
            coverage: WorkspaceHistoryCoverage {
                limit,
                truncated: !complete,
                scope: if task_id.is_some() {
                    WorkspaceHistoryScope::Task
                } else if channel_id.is_some() {
                    WorkspaceHistoryScope::Channel
                } else {
                    WorkspaceHistoryScope::Workspace
                },
            },
        })
    }

    pub fn message_search(
        &self,
        network: &str,
        request: &MessageSearchRequest,
    ) -> Result<MessageSearchPage> {
        ensure!(
            (1..=MESSAGE_SEARCH_MAX_LIMIT).contains(&request.limit),
            "message search limit must be 1-100"
        );
        ensure!(
            !request.query.trim().is_empty() && request.query.len() <= 1024,
            "search query must contain 1-1024 bytes of text"
        );
        let graph = read_graph(&self.connection, network)?;
        let mut candidates = Vec::new();
        let conversation = match &request.scope {
            MessageSearchScope::BotMain { bot_id } => {
                valid_id(bot_id)?;
                let conversation: String = self
                    .connection
                    .query_row(
                        "SELECT conversation_id FROM bots WHERE network_id=? AND bot_id=?",
                        params![network, bot_id],
                        |row| row.get(0),
                    )
                    .optional()?
                    .context("Bot not found")?;
                for message in self.main_messages(network, &conversation)? {
                    let event = log::event_from(&self.connection, network, &message.event_hash)?
                        .context("message event not found")?;
                    candidates.push((
                        message.rank.clone(),
                        MessageSearchItem {
                            speaker: history_speaker(&self.connection, network, &graph, &event)?,
                            event_hash: message.event_hash,
                            message_id: message.message_id,
                            conversation_id: conversation.clone(),
                            rank: message.rank,
                            created_at_ms: message.created_at_ms,
                            text: message.text,
                        },
                    ));
                }
                conversation
            }
            scope => {
                let (channel, task) = match scope {
                    MessageSearchScope::Task { task_id } => {
                        valid_id(task_id)?;
                        (None, Some(task_id.as_str()))
                    }
                    MessageSearchScope::Channel { channel_id } => {
                        valid_id(channel_id)?;
                        (Some(channel_id.as_str()), None)
                    }
                    MessageSearchScope::BotMain { .. } => unreachable!(),
                };
                for (rank, event) in self.workspace_history_events(network, channel, task)? {
                    if !self.workspace_history_item_visible(network, &event)? {
                        continue;
                    }
                    let (message_id, text) = match event.body {
                        EventBody::InputPosted {
                            message_id, text, ..
                        }
                        | EventBody::MessagePosted {
                            message_id, text, ..
                        } => (message_id, text),
                        EventBody::WorkspaceCommanded {
                            command:
                                WorkspaceCommand::PostChannelMessage {
                                    message_id, text, ..
                                },
                            ..
                        } => (message_id, text),
                        _ => continue,
                    };
                    candidates.push((
                        rank.clone(),
                        MessageSearchItem {
                            event_hash: event.event_hash,
                            message_id,
                            conversation_id: event.scope_id,
                            rank,
                            created_at_ms: event.created_at_ms,
                            text,
                            speaker: event.speaker,
                        },
                    ));
                }
                String::new()
            }
        };
        // Literal Unicode text matching; JSON keys, IDs, attachment names, prompts,
        // metadata and control commands never participate in matching.
        let query = request.query.to_lowercase();
        candidates.retain(|(_, item)| item.text.to_lowercase().contains(&query));
        candidates.sort_by(|(a, _), (b, _)| a.cmp(b));
        let authority = if conversation.is_empty() {
            self.workspace_projection_authority(network)?
        } else {
            self.conversation_history_authority(network, &conversation)?
        };
        let binding = crate::crypto::digest(&serde_json::to_vec(&(
            "message_search",
            network,
            &request.scope,
            &request.query,
            authority,
        ))?);
        let (items, next_cursor) = page(
            candidates,
            &binding,
            request.cursor.as_deref(),
            request.limit,
        )?;
        Ok(MessageSearchPage {
            scope: request.scope.clone(),
            query: request.query.clone(),
            items,
            complete: next_cursor.is_none(),
            next_cursor,
            coverage: MessageSearchCoverage {
                source: MessageSearchSource::CommittedServerHistory,
                limit: request.limit,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workspace_history_cursor_authenticates_boundary_and_handles_equal_order_components()
    -> Result<()> {
        let candidates = || {
            (1..=9)
                .map(|id| {
                    let rank = crate::store::rank(7, 7, &format!("{id:064x}"));
                    (rank, id)
                })
                .collect::<Vec<_>>()
        };
        let (first, cursor) = page(candidates(), "scope", None, 3)?;
        assert_eq!(first, [7, 8, 9]);
        let cursor = cursor.context("continuation")?;
        let (second, next) = page(candidates(), "scope", Some(&cursor), 3)?;
        assert_eq!(second, [4, 5, 6]);
        let (last, end) = page(candidates(), "scope", next.as_deref(), 3)?;
        assert_eq!(last, [1, 2, 3]);
        assert!(end.is_none());
        assert!(page(candidates(), "different network or scope", Some(&cursor), 3).is_err());
        let mut forged: Cursor = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&cursor)?)?;
        forged.before = candidates()[0].0.clone();
        let forged = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged)?);
        assert!(page(candidates(), "scope", Some(&forged), 3).is_err());
        let (empty, next) = page::<u8>(vec![], "empty", None, 2)?;
        assert!(empty.is_empty() && next.is_none());
        Ok(())
    }
    #[test]
    fn workspace_history_byte_budget_keeps_continuation_and_new_appends_do_not_shift_pages()
    -> Result<()> {
        let candidates = || {
            (1..=4)
                .map(|id| (format!("{id:04}"), "中".repeat(100_000)))
                .collect::<Vec<_>>()
        };
        let (first, next) = page(candidates(), "bytes", None, 4)?;
        assert_eq!(first.len(), 1);
        let mut cursor = next;
        let mut count = first.len();
        while let Some(next) = cursor {
            let (items, next) = page(candidates(), "bytes", Some(&next), 4)?;
            count += items.len();
            cursor = next;
        }
        assert_eq!(count, 4);
        let candidates = || {
            (1..=4)
                .map(|id| (format!("{id:04}"), id))
                .collect::<Vec<_>>()
        };
        let (_, next) = page(candidates(), "append", None, 2)?;
        let mut appended = candidates();
        appended.push(("0005".into(), 5));
        let (older, next) = page(appended, "append", next.as_deref(), 2)?;
        assert_eq!(older, [1, 2]);
        assert!(next.is_none());
        Ok(())
    }
}
