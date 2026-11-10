//! Settings pages use stable IDs. Clients sort loaded skills and schedules by their names.
use super::{
    conversation_history::{HistoryBoundary, history_page},
    *,
};
use crate::scheduling::MemoryConflict;
use vibeface_protocol::history::{
    HISTORY_MAX_LIMIT, HISTORY_PAGE_BYTES, HistoryPage, HistorySection,
};

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationMemoryConflict {
    pub history_id: String,
    #[serde(flatten)]
    pub conflict: MemoryConflict,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct ConflictBoundary {
    offset: i64,
    digest: String,
}

impl Store {
    pub fn conversation_skills_page(
        &self,
        network: &str,
        conversation: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage<SkillEntry>> {
        let bot = self.bot_for_conversation(network, conversation)?;
        self.settings_page(
            network,
            conversation,
            HistorySection::Skills,
            cursor,
            limit,
            |id| {
                if bot
                    .as_ref()
                    .and_then(|bot| bot.skills.as_ref())
                    .is_some_and(|skills| !skills.contains(id))
                {
                    return Ok(None);
                }
                Ok(self.skill(network, id)?.map(|skill| SkillEntry {
                    skill_id: id.into(),
                    skill,
                }))
            },
        )
    }

    pub fn conversation_schedules_page(
        &self,
        network: &str,
        conversation: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage<ScheduleEntry>> {
        self.settings_page(
            network,
            conversation,
            HistorySection::Schedules,
            cursor,
            limit,
            |id| self.schedule(network, id),
        )
    }

    pub fn conversation_memory_page(
        &self,
        network: &str,
        conversation: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage<MemoryState>> {
        self.settings_page(
            network,
            conversation,
            HistorySection::Memory,
            cursor,
            limit,
            |id| bot_settings::memory_state_from(&self.connection, network, conversation, id),
        )
    }

    pub fn conversation_memory_sync_conflicts_page(
        &self,
        network: &str,
        conversation: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage<MemorySyncConflict>> {
        self.settings_page(network, conversation, HistorySection::MemorySyncConflicts, cursor, limit, |id| {
            let (memory, revision) = id.split_once('\0').context("invalid memory conflict boundary")?;
            let (losing_hash, user_controlled): (String, bool) = self.connection.query_row(
                "SELECT source_event_hash,user_controlled FROM memory_history_operations WHERE network_id=? AND conversation_id=? AND memory_id=? AND revision=?",
                params![network, conversation, memory, revision], |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let Some(winner) = bot_settings::memory_state_from(&self.connection, network, conversation, memory)? else { return Ok(None); };
            Ok(Some(MemorySyncConflict {
                memory_id: memory.into(), losing_revision: revision.into(), winning_revision: winner.metadata.revision,
                losing_event_hash: losing_hash, winning_event_hash: winner.metadata.source_event_hash,
                reason: if winner.metadata.user_controlled && !user_controlled { MemorySyncConflictReason::UserProtected }
                    else if user_controlled { MemorySyncConflictReason::ConcurrentUser }
                    else { MemorySyncConflictReason::ConcurrentAutomatic },
            }))
        })
    }

    fn settings_page<T: Serialize>(
        &self,
        network: &str,
        conversation: &str,
        section: HistorySection,
        cursor: Option<&str>,
        limit: usize,
        mut project: impl FnMut(&str) -> Result<Option<T>>,
    ) -> Result<HistoryPage<T>> {
        ensure!(
            (1..=HISTORY_MAX_LIMIT).contains(&limit),
            "history limit must be 1 to 100"
        );
        let (authority, before) = self.history_cursor(network, conversation, section, cursor)?;
        let (selection, key, order) = match section {
            HistorySection::Skills => (
                "SELECT skill_id,skill_id FROM skills WHERE network_id=?1 AND skill IS NOT NULL",
                "(skill_id,skill_id)", "skill_id DESC",
            ),
            HistorySection::Schedules => (
                "SELECT schedule_id,schedule_id FROM schedules WHERE network_id=?1 AND conversation_id=?2 AND removed=0 AND schedule IS NOT NULL
                 AND NOT EXISTS(SELECT 1 FROM bot_deletions d WHERE d.network_id=schedules.network_id AND d.conversation_id=schedules.conversation_id)",
                "(schedule_id,schedule_id)", "schedule_id DESC",
            ),
            HistorySection::Memory => (
                "SELECT CAST(changed_at_ms AS TEXT),memory_id FROM memories WHERE network_id=?1 AND conversation_id=?2 AND text IS NOT NULL",
                "(changed_at_ms,memory_id)", "changed_at_ms DESC,memory_id DESC",
            ),
            HistorySection::MemorySyncConflicts => (
                "SELECT memory_id,revision FROM memory_history_operations WHERE network_id=?1 AND conversation_id=?2 AND winning=0",
                "(memory_id,revision)", "memory_id DESC,revision DESC",
            ),
            _ => bail!("history section uses its own typed reader"),
        };
        let seek = if before.is_some() {
            if section == HistorySection::Memory {
                format!(" AND {key}<(CAST(?3 AS INTEGER),?4)")
            } else {
                format!(" AND {key}<(?3,?4)")
            }
        } else {
            String::new()
        };
        let mut statement = self
            .connection
            .prepare(&format!("{selection}{seek} ORDER BY {order} LIMIT ?5"))?;
        let mut rows = statement.query(params![
            network,
            conversation,
            before.as_ref().map(|b| b.rank.as_str()),
            before.as_ref().map(|b| b.id.as_str()),
            limit + 1
        ])?;
        let mut boundary = before.clone();
        let mut items = Vec::new();
        let mut bytes = 0;
        let mut scanned = 0;
        let mut has_more = false;
        while let Some(row) = rows.next()? {
            if scanned == limit {
                has_more = true;
                break;
            }
            let rank: String = row.get(0)?;
            let id: String = row.get(1)?;
            let lookup = if section == HistorySection::MemorySyncConflicts {
                format!("{rank}\0{id}")
            } else {
                id.clone()
            };
            let item = project(&lookup)?;
            if let Some(item) = &item {
                let size = serde_json::to_vec(item)?.len() + 1;
                if !items.is_empty() && bytes + size > HISTORY_PAGE_BYTES {
                    has_more = true;
                    break;
                }
                bytes += size;
            }
            boundary = Some(HistoryBoundary { rank, id });
            scanned += 1;
            if let Some(item) = item {
                items.push(item);
            }
        }
        items.reverse();
        history_page(
            network,
            conversation,
            section,
            authority,
            boundary,
            items,
            has_more,
        )
    }

    /// SQL counts only the active sessions' staged entries, without deserializing SDK state.
    pub fn conversation_memory_staged_count(&self, conversation: &str) -> Result<usize> {
        Ok(self.connection.query_row(
            "SELECT count(*) FROM scheduler_sessions s,json_each(s.shown,'$.staged_memories')
             WHERE s.conversation_id=? AND s.active IS NOT NULL",
            [conversation],
            |row| row.get(0),
        )?)
    }

    pub fn conversation_memory_conflicts_page(
        &self,
        network: &str,
        conversation: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage<ConversationMemoryConflict>> {
        ensure!(
            (1..=HISTORY_MAX_LIMIT).contains(&limit),
            "history limit must be 1 to 100"
        );
        let (authority, before) = self.history_cursor(
            network,
            conversation,
            HistorySection::MemoryConflicts,
            cursor,
        )?;
        let before_conflict = before.as_ref().map(|boundary| -> Result<ConflictBoundary> {
            let position: ConflictBoundary = serde_json::from_str(&boundary.id).context("invalid memory conflict boundary")?;
            ensure!(position.offset >= -1, "invalid memory conflict offset");
            let current: Option<Option<String>> = self.connection.query_row(
                "SELECT json_extract(shown,'$.memory_conflicts') FROM scheduler_sessions WHERE conversation_id=? AND scope=?",
                params![conversation, boundary.rank], |row| row.get(0),
            ).optional()?;
            let current = current.context("local memory conflicts changed; refresh the conversation")?.unwrap_or_else(|| "[]".into());
            ensure!(digest(current.as_bytes()) == position.digest, "local memory conflicts changed; refresh the conversation");
            Ok(position)
        }).transpose()?;
        let seek = match before_conflict.as_ref() {
            Some(boundary) if boundary.offset == -1 => " AND scope<?2",
            Some(_) => " AND scope<=?2",
            None => "",
        };
        let mut statement = self.connection.prepare(&format!(
            "SELECT scope FROM scheduler_sessions WHERE conversation_id=?1{seek} ORDER BY scope DESC LIMIT ?3"
        ))?;
        let mut sessions = statement.query(params![
            conversation,
            before.as_ref().map(|b| b.rank.as_str()),
            limit + 1
        ])?;
        let mut boundary = before.clone();
        let mut items = Vec::new();
        let mut bytes = 0;
        let mut scanned_sessions = 0;
        let mut has_more = false;
        'sessions: while let Some(session) = sessions.next()? {
            if scanned_sessions == limit {
                has_more = true;
                break;
            }
            let scope: String = session.get(0)?;
            let index = match before.as_ref().filter(|b| b.rank == scope) {
                Some(_) => {
                    before_conflict
                        .as_ref()
                        .context("missing conflict boundary")?
                        .offset
                }
                None => i64::MAX,
            };
            // Only the selected session's conflict JSON is hashed. Historical SDK state and
            // unrelated sessions never enter a Rust allocation.
            let current: Option<String> = self.connection.query_row(
                "SELECT json_extract(shown,'$.memory_conflicts') FROM scheduler_sessions WHERE conversation_id=? AND scope=?",
                params![conversation, scope], |row| row.get(0),
            )?;
            let conflict_digest = digest(current.unwrap_or_else(|| "[]".into()).as_bytes());
            let mut statement = self.connection.prepare(
                "WITH conflicts AS (
                    SELECT CAST(j.key AS INTEGER) AS position,j.value,
                        row_number() OVER(PARTITION BY j.value ORDER BY CAST(j.key AS INTEGER)) AS occurrence
                    FROM scheduler_sessions s,json_each(s.shown,'$.memory_conflicts') j
                    WHERE s.conversation_id=?1 AND s.scope=?2)
                 SELECT position,value,occurrence FROM conflicts WHERE position<?3
                 ORDER BY position DESC LIMIT ?4",
            )?;
            let mut conflicts =
                statement.query(params![conversation, scope, index, limit - items.len() + 1])?;
            while let Some(row) = conflicts.next()? {
                if items.len() == limit {
                    has_more = true;
                    break 'sessions;
                }
                let index: i64 = row.get(0)?;
                let body: String = row.get(1)?;
                let occurrence: i64 = row.get(2)?;
                let conflict = ConversationMemoryConflict {
                    history_id: digest(&serde_json::to_vec(&(&scope, &body, occurrence))?),
                    conflict: serde_json::from_str(&body)?,
                };
                let size = serde_json::to_vec(&conflict)?.len() + 1;
                if !items.is_empty() && bytes + size > HISTORY_PAGE_BYTES {
                    has_more = true;
                    break 'sessions;
                }
                bytes += size;
                boundary = Some(HistoryBoundary {
                    rank: scope.clone(),
                    id: serde_json::to_string(&ConflictBoundary {
                        offset: index,
                        digest: conflict_digest.clone(),
                    })?,
                });
                items.push(conflict);
            }
            boundary = Some(HistoryBoundary {
                rank: scope,
                id: serde_json::to_string(&ConflictBoundary {
                    offset: -1,
                    digest: conflict_digest,
                })?,
            });
            scanned_sessions += 1;
        }
        items.reverse();
        history_page(
            network,
            conversation,
            HistorySection::MemoryConflicts,
            authority,
            boundary,
            items,
            has_more,
        )
    }

    pub fn conversation_overwritten_page(
        &self,
        network: &str,
        conversation: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage<bot_settings::Overwritten>> {
        ensure!(
            (1..=HISTORY_MAX_LIMIT).contains(&limit),
            "history limit must be 1 to 100"
        );
        let (authority, before) =
            self.history_cursor(network, conversation, HistorySection::Overwritten, cursor)?;
        let start = match &before {
            Some(boundary) => boundary
                .rank
                .split_once(':')
                .context("invalid overwritten boundary")?
                .0
                .parse::<i64>()?,
            None => 13,
        };
        ensure!((0..=13).contains(&start), "invalid overwritten setting");
        let mut boundary = before.clone();
        let mut items = Vec::new();
        let mut bytes = 0;
        let mut scanned = 0;
        let mut has_more = false;
        'settings: for number in (0..=start).rev() {
            let head: Option<(String, String, String)> = self.connection.query_row(
                "SELECT rank,by,after_value FROM conversation_setting_writes WHERE network_id=? AND conversation_id=? AND setting=? ORDER BY rank DESC LIMIT 1",
                params![network, conversation, number], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional()?;
            let Some((head_rank, by, after)) = head else {
                continue;
            };
            let cutoff: Option<String> = self.connection.query_row(
                "SELECT rank FROM conversation_setting_writes WHERE network_id=? AND conversation_id=? AND setting=? AND built_on_previous=1 ORDER BY rank DESC LIMIT 1",
                params![network, conversation, number], |row| row.get(0),
            ).optional()?;
            let until = if number == start {
                before
                    .as_ref()
                    .map(|b| {
                        b.rank
                            .split_once(':')
                            .map(|(_, rank)| rank.to_owned())
                            .context("invalid overwritten boundary")
                    })
                    .transpose()?
                    .unwrap_or(head_rank)
            } else {
                head_rank
            };
            let mut statement = self.connection.prepare(
                "SELECT rank,event_hash,change_id,by,after_value FROM conversation_setting_writes
                 WHERE network_id=?1 AND conversation_id=?2 AND setting=?3 AND rank>=?4 AND rank<?5
                 ORDER BY rank DESC LIMIT ?6",
            )?;
            let mut rows = statement.query(params![
                network,
                conversation,
                number,
                cutoff.unwrap_or_default(),
                until,
                limit - scanned + 1
            ])?;
            while let Some(row) = rows.next()? {
                if scanned == limit {
                    has_more = true;
                    break 'settings;
                }
                let rank: String = row.get(0)?;
                let hash: String = row.get(1)?;
                let change: Option<String> = row.get(2)?;
                let writer: String = row.get(3)?;
                let earlier_after: String = row.get(4)?;
                let change = match change {
                    Some(id)
                        if writer != by
                            && serde_json::from_str::<Value>(&earlier_after)?
                                != serde_json::from_str::<Value>(&after)? =>
                    {
                        self.bot_change(network, &id)?
                    }
                    _ => None,
                };
                let main = match change.as_ref().and_then(|change| change.cause.as_ref()) {
                    Some(cause) => scope_projection::message_is_main_from(
                        &self.connection,
                        network,
                        &cause.conversation_id,
                        &cause.message_id,
                    )?,
                    None => false,
                };
                let item = if main {
                    change
                        .map(|change| -> Result<_> {
                            Ok(bot_settings::Overwritten {
                                change_id: change.change_id,
                                setting: super::conversation_settings_projection::setting(number)?,
                                by: by.clone(),
                                value: serde_json::from_str(&after)?,
                            })
                        })
                        .transpose()?
                } else {
                    None
                };
                if let Some(item) = &item {
                    let size = serde_json::to_vec(item)?.len() + 1;
                    if !items.is_empty() && bytes + size > HISTORY_PAGE_BYTES {
                        has_more = true;
                        break 'settings;
                    }
                    bytes += size;
                }
                boundary = Some(HistoryBoundary {
                    rank: format!("{number}:{rank}"),
                    id: hash,
                });
                scanned += 1;
                if let Some(item) = item {
                    items.push(item);
                }
            }
        }
        items.reverse();
        history_page(
            network,
            conversation,
            HistorySection::Overwritten,
            authority,
            boundary,
            items,
            has_more,
        )
    }
}
