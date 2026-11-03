use super::Store;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;

const KEY_PREFIX: &str = "mailbox:";
const MAX_ACTIVE: usize = 200;
const TTL_MS: i64 = 24 * 60 * 60 * 1000;
const RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const MAX_ATTEMPTS: u32 = 8;
const BACKOFF_MS: i64 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MailboxState {
    Queued,
    Sending,
    Unknown,
    Dead,
    Delivered,
}

impl MailboxState {
    fn active(self) -> bool {
        matches!(self, Self::Queued | Self::Sending | Self::Unknown)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MailboxRecord {
    pub network_id: String,
    pub conversation_id: String,
    pub trigger: String,
    pub state: MailboxState,
    pub attempts: u32,
    pub queued_at_ms: i64,
    pub next_attempt_ms: i64,
    pub last_error: Option<String>,
}

fn key(record: &MailboxRecord) -> String {
    format!(
        "{KEY_PREFIX}{}",
        crate::crypto::digest(
            format!(
                "{}\0{}\0{}",
                record.network_id, record.conversation_id, record.trigger
            )
            .as_bytes()
        )
    )
}

fn load_key(connection: &Connection, key: &str) -> Result<Option<MailboxRecord>> {
    let value = connection
        .query_row(
            "SELECT value FROM identity_meta WHERE key=?",
            [key],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    match value {
        Some(value) => Ok(Some(serde_json::from_str(&value)?)),
        None => Ok(None),
    }
}

fn save(connection: &Connection, record: &MailboxRecord) -> Result<()> {
    connection.execute(
        "INSERT INTO identity_meta(key,value) VALUES(?,?)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key(record), serde_json::to_string(record)?],
    )?;
    Ok(())
}

fn records(connection: &Connection) -> Result<Vec<MailboxRecord>> {
    let mut statement = connection
        .prepare("SELECT value FROM identity_meta WHERE key LIKE 'mailbox:%' ORDER BY key")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

fn delete(connection: &Connection, record: &MailboxRecord) -> Result<()> {
    connection.execute("DELETE FROM identity_meta WHERE key=?", [key(record)])?;
    Ok(())
}

fn reconcile(connection: &Connection, network: &str, now_ms: i64) -> Result<()> {
    let mut entries: Vec<_> = records(connection)?
        .into_iter()
        .filter(|record| record.network_id == network)
        .collect();
    for record in &mut entries {
        if record.state.active() && now_ms.saturating_sub(record.queued_at_ms) >= TTL_MS {
            record.state = MailboxState::Dead;
            record.last_error = Some("mailbox entry expired".into());
            connection.execute(
                "DELETE FROM scheduler_outbox WHERE conversation_id=? AND trigger=?",
                params![record.conversation_id, record.trigger],
            )?;
            save(connection, record)?;
        } else if !record.state.active()
            && now_ms.saturating_sub(record.queued_at_ms) >= RETENTION_MS
        {
            delete(connection, record)?;
        }
    }
    let mut active: Vec<_> = entries
        .into_iter()
        .filter(|record| record.state.active())
        .collect();
    active.sort_by_key(|record| Reverse(record.queued_at_ms));
    for record in active.into_iter().skip(MAX_ACTIVE) {
        let mut dead = record;
        dead.state = MailboxState::Dead;
        dead.last_error = Some("mailbox capacity exceeded".into());
        connection.execute(
            "DELETE FROM scheduler_outbox WHERE conversation_id=? AND trigger=?",
            params![dead.conversation_id, dead.trigger],
        )?;
        save(connection, &dead)?;
    }
    Ok(())
}

pub(crate) fn queue(
    connection: &Connection,
    network: &str,
    conversation: &str,
    trigger: &str,
    now_ms: i64,
) -> Result<MailboxRecord> {
    reconcile(connection, network, now_ms)?;
    let candidate = MailboxRecord {
        network_id: network.into(),
        conversation_id: conversation.into(),
        trigger: trigger.into(),
        state: MailboxState::Queued,
        attempts: 0,
        queued_at_ms: now_ms,
        next_attempt_ms: now_ms,
        last_error: None,
    };
    if let Some(existing) = load_key(connection, &key(&candidate))? {
        if existing.state.active() || existing.state == MailboxState::Delivered {
            return Ok(existing);
        }
    }
    connection.execute(
        "INSERT OR IGNORE INTO scheduler_outbox(conversation_id,trigger) VALUES(?,?)",
        params![conversation, trigger],
    )?;
    save(connection, &candidate)?;
    Ok(candidate)
}

impl Store {
    pub(crate) fn mailbox_before_attempt(
        &mut self,
        network: &str,
        conversation: &str,
        trigger: &str,
        now_ms: i64,
    ) -> Result<bool> {
        reconcile(&self.connection, network, now_ms)?;
        let candidate = MailboxRecord {
            network_id: network.into(),
            conversation_id: conversation.into(),
            trigger: trigger.into(),
            state: MailboxState::Queued,
            attempts: 0,
            queued_at_ms: now_ms,
            next_attempt_ms: now_ms,
            last_error: None,
        };
        let Some(mut record) = load_key(&self.connection, &key(&candidate))? else {
            save(&self.connection, &candidate)?;
            return Ok(true);
        };
        if !record.state.active() || record.next_attempt_ms > now_ms {
            return Ok(false);
        }
        record.attempts = record.attempts.saturating_add(1);
        if record.attempts > MAX_ATTEMPTS {
            record.state = MailboxState::Dead;
            record.last_error = Some("mailbox retry limit exceeded".into());
            self.connection.execute(
                "DELETE FROM scheduler_outbox WHERE conversation_id=? AND trigger=?",
                params![conversation, trigger],
            )?;
            save(&self.connection, &record)?;
            return Ok(false);
        }
        record.state = MailboxState::Sending;
        record.next_attempt_ms = now_ms;
        save(&self.connection, &record)?;
        Ok(true)
    }

    pub(crate) fn mailbox_failed(
        &mut self,
        network: &str,
        conversation: &str,
        trigger: &str,
        unknown: bool,
        detail: &str,
        now_ms: i64,
    ) -> Result<()> {
        let candidate = MailboxRecord {
            network_id: network.into(),
            conversation_id: conversation.into(),
            trigger: trigger.into(),
            state: MailboxState::Queued,
            attempts: 0,
            queued_at_ms: now_ms,
            next_attempt_ms: now_ms,
            last_error: None,
        };
        let Some(mut record) = load_key(&self.connection, &key(&candidate))? else {
            return Ok(());
        };
        record.state = if unknown {
            MailboxState::Unknown
        } else {
            MailboxState::Queued
        };
        record.next_attempt_ms =
            now_ms + BACKOFF_MS * (1_i64 << record.attempts.saturating_sub(1).min(5));
        record.last_error = Some(crate::logging::scrub(detail).chars().take(512).collect());
        if record.attempts >= MAX_ATTEMPTS {
            record.state = MailboxState::Dead;
        }
        save(&self.connection, &record)
    }

    pub(crate) fn mailbox_delivered(
        &mut self,
        network: &str,
        conversation: &str,
        trigger: &str,
        now_ms: i64,
    ) -> Result<()> {
        let candidate = MailboxRecord {
            network_id: network.into(),
            conversation_id: conversation.into(),
            trigger: trigger.into(),
            state: MailboxState::Delivered,
            attempts: 0,
            queued_at_ms: now_ms,
            next_attempt_ms: now_ms,
            last_error: None,
        };
        if let Some(mut record) = load_key(&self.connection, &key(&candidate))? {
            record.state = MailboxState::Delivered;
            record.next_attempt_ms = now_ms;
            save(&self.connection, &record)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn mailbox_for_test(
        &self,
        network: &str,
        conversation: &str,
        trigger: &str,
    ) -> Result<Option<MailboxRecord>> {
        let candidate = MailboxRecord {
            network_id: network.into(),
            conversation_id: conversation.into(),
            trigger: trigger.into(),
            state: MailboxState::Queued,
            attempts: 0,
            queued_at_ms: 0,
            next_attempt_ms: 0,
            last_error: None,
        };
        load_key(&self.connection, &key(&candidate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE identity_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
                 CREATE TABLE scheduler_outbox(conversation_id TEXT NOT NULL,trigger TEXT NOT NULL,
                 PRIMARY KEY(conversation_id,trigger));",
            )
            .unwrap();
        connection
    }

    #[test]
    fn mailbox_state_survives_reload_and_marks_unknown_without_replaying_the_trigger() {
        let connection = connection();
        let queued = queue(&connection, "n", "c", "message:m", 1).unwrap();
        assert_eq!(queued.state, MailboxState::Queued);
        let mut record = load_key(&connection, &key(&queued)).unwrap().unwrap();
        record.state = MailboxState::Unknown;
        record.attempts = 1;
        save(&connection, &record).unwrap();
        let reopened = load_key(&connection, &key(&queued)).unwrap().unwrap();
        assert_eq!(reopened.state, MailboxState::Unknown);
        assert_eq!(reopened.attempts, 1);
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM scheduler_outbox WHERE conversation_id='c' AND trigger='message:m'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn mailbox_expires_active_rows() {
        let connection = connection();
        queue(&connection, "n", "c", "message:m", 1).unwrap();
        reconcile(&connection, "n", TTL_MS + 1).unwrap();
        let record = records(&connection).unwrap().pop().unwrap();
        assert_eq!(record.state, MailboxState::Dead);
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM scheduler_outbox", [], |row| row
                    .get::<_, i64>(0),)
                .unwrap(),
            0
        );
    }
}
