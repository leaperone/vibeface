use super::Store;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

const KEY_PREFIX: &str = "mailbox:";
const MAX_ACTIVE: usize = 200;
const RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const MAX_ATTEMPT_COUNT: u32 = 8;
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

fn key(network: &str, conversation: &str, trigger: &str) -> String {
    format!(
        "{KEY_PREFIX}{}",
        crate::crypto::digest(format!("{network}\0{conversation}\0{trigger}").as_bytes())
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
        params![
            key(&record.network_id, &record.conversation_id, &record.trigger),
            serde_json::to_string(record)?
        ],
    )?;
    Ok(())
}

fn records(connection: &Connection) -> Result<Vec<MailboxRecord>> {
    let mut statement = connection.prepare(
        "SELECT value FROM identity_meta WHERE key>='mailbox:' AND key<'mailbox;' ORDER BY key",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

fn delete(connection: &Connection, record: &MailboxRecord) -> Result<()> {
    connection.execute(
        "DELETE FROM identity_meta WHERE key=?",
        [key(
            &record.network_id,
            &record.conversation_id,
            &record.trigger,
        )],
    )?;
    Ok(())
}

fn reconcile(connection: &Connection, network: &str, now_ms: i64) -> Result<()> {
    let entries: Vec<_> = records(connection)?
        .into_iter()
        .filter(|record| record.network_id == network)
        .collect();
    for record in &entries {
        if !record.state.active() && now_ms.saturating_sub(record.queued_at_ms) >= RETENTION_MS {
            connection.execute(
                "DELETE FROM scheduler_outbox WHERE conversation_id=? AND trigger=?",
                params![record.conversation_id, record.trigger],
            )?;
            delete(connection, record)?;
        }
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
    if let Some(existing) = load_key(connection, &key(network, conversation, trigger))? {
        if existing.state.active() {
            connection.execute(
                "INSERT OR IGNORE INTO scheduler_outbox(conversation_id,trigger) VALUES(?,?)",
                params![conversation, trigger],
            )?;
        }
        return Ok(existing);
    }
    let active = records(connection)?
        .into_iter()
        .filter(|record| record.network_id == network && record.state.active())
        .count();
    anyhow::ensure!(
        active < MAX_ACTIVE,
        "mailbox capacity reached; existing deliveries remain queued"
    );
    connection.execute(
        "INSERT OR IGNORE INTO scheduler_outbox(conversation_id,trigger) VALUES(?,?)",
        params![conversation, trigger],
    )?;
    save(connection, &candidate)?;
    Ok(load_key(connection, &key(network, conversation, trigger))?.unwrap_or(candidate))
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
        let Some(mut record) = load_key(&self.connection, &key(network, conversation, trigger))?
        else {
            save(&self.connection, &candidate)?;
            return Ok(true);
        };
        if !record.state.active() {
            self.connection.execute(
                "DELETE FROM scheduler_outbox WHERE conversation_id=? AND trigger=?",
                params![conversation, trigger],
            )?;
            return Ok(false);
        }
        if record.next_attempt_ms > now_ms {
            return Ok(false);
        }
        record.attempts = record.attempts.saturating_add(1).min(MAX_ATTEMPT_COUNT);
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
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(mut record) = load_key(&tx, &key(network, conversation, trigger))? else {
            tx.commit()?;
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
        save(&tx, &record)?;
        Ok(tx.commit()?)
    }

    pub(crate) fn mailbox_delivered(
        &mut self,
        network: &str,
        conversation: &str,
        trigger: &str,
        now_ms: i64,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(mut record) = load_key(&tx, &key(network, conversation, trigger))? {
            record.state = MailboxState::Delivered;
            record.next_attempt_ms = now_ms;
            save(&tx, &record)?;
        }
        tx.execute(
            "DELETE FROM scheduler_outbox WHERE conversation_id=? AND trigger=?",
            params![conversation, trigger],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn mailbox_rejected(
        &mut self,
        network: &str,
        conversation: &str,
        trigger: &str,
        detail: &str,
        now_ms: i64,
    ) -> Result<()> {
        let candidate = MailboxRecord {
            network_id: network.into(),
            conversation_id: conversation.into(),
            trigger: trigger.into(),
            state: MailboxState::Dead,
            attempts: 0,
            queued_at_ms: now_ms,
            next_attempt_ms: now_ms,
            last_error: None,
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut record = load_key(&tx, &key(network, conversation, trigger))?.unwrap_or(candidate);
        record.state = MailboxState::Dead;
        record.next_attempt_ms = now_ms;
        record.last_error = Some(crate::logging::scrub(detail).chars().take(512).collect());
        save(&tx, &record)?;
        tx.execute(
            "DELETE FROM scheduler_outbox WHERE conversation_id=? AND trigger=?",
            params![conversation, trigger],
        )?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn mailbox_for_test(
        &self,
        network: &str,
        conversation: &str,
        trigger: &str,
    ) -> Result<Option<MailboxRecord>> {
        load_key(&self.connection, &key(network, conversation, trigger))
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
        let mut record = load_key(&connection, &key("n", "c", "message:m"))
            .unwrap()
            .unwrap();
        record.state = MailboxState::Unknown;
        record.attempts = 1;
        save(&connection, &record).unwrap();
        let reopened = load_key(&connection, &key("n", "c", "message:m"))
            .unwrap()
            .unwrap();
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
    fn mailbox_reconcile_keeps_active_rows() {
        let connection = connection();
        queue(&connection, "n", "c", "message:m", 1).unwrap();
        reconcile(&connection, "n", RETENTION_MS + 1).unwrap();
        let record = records(&connection).unwrap().pop().unwrap();
        assert_eq!(record.state, MailboxState::Queued);
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM scheduler_outbox", [], |row| row
                    .get::<_, i64>(0),)
                .unwrap(),
            1
        );
    }

    #[test]
    fn mailbox_rejects_new_entries_at_capacity_without_evicting_existing() {
        let connection = connection();
        for index in 0..MAX_ACTIVE {
            queue(
                &connection,
                "n",
                "c",
                &format!("message:{index}"),
                index as i64 + 1,
            )
            .unwrap();
        }
        assert!(queue(&connection, "n", "c", "message:overflow", 999).is_err());
        assert_eq!(records(&connection).unwrap().len(), MAX_ACTIVE);
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM scheduler_outbox", [], |row| row
                    .get::<_, i64>(0),)
                .unwrap(),
            MAX_ACTIVE as i64
        );
    }

    #[test]
    fn terminal_mailbox_states_remove_outbox_without_treating_rejection_as_delivered() {
        let mut store = Store::open(":memory:").unwrap();
        queue(&store.connection, "n", "c", "message:m", 1).unwrap();
        store.mailbox_delivered("n", "c", "message:m", 2).unwrap();
        assert_eq!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM scheduler_outbox", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .mailbox_for_test("n", "c", "message:m")
                .unwrap()
                .map(|record| record.state),
            Some(MailboxState::Delivered)
        );
        queue(&store.connection, "n", "c", "message:refused", 3).unwrap();
        store
            .mailbox_rejected("n", "c", "message:refused", "stale owner epoch", 4)
            .unwrap();
        let rejected = store
            .mailbox_for_test("n", "c", "message:refused")
            .unwrap()
            .unwrap();
        assert_eq!(rejected.state, MailboxState::Dead);
        assert_eq!(rejected.last_error.as_deref(), Some("stale owner epoch"));
        assert!(store.scheduler_outbox().unwrap().is_empty());
        assert!(
            !store
                .mailbox_before_attempt("n", "c", "message:refused", 5)
                .unwrap()
        );
    }

    #[test]
    fn delivered_rows_left_by_an_older_crash_are_reconciled_before_attempt() {
        let mut store = Store::open(":memory:").unwrap();
        queue(&store.connection, "n", "c", "message:m", 1).unwrap();
        let mut delivered = load_key(&store.connection, &key("n", "c", "message:m"))
            .unwrap()
            .unwrap();
        delivered.state = MailboxState::Delivered;
        save(&store.connection, &delivered).unwrap();
        store
            .connection
            .execute(
                "INSERT OR IGNORE INTO scheduler_outbox(conversation_id,trigger) VALUES(?,?)",
                params!["c", "message:m"],
            )
            .unwrap();
        assert!(
            !store
                .mailbox_before_attempt("n", "c", "message:m", 2)
                .unwrap()
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM scheduler_outbox", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
    }

    #[test]
    fn retry_limit_keeps_the_scheduler_outbox_for_reconciliation() {
        let mut store = Store::open(":memory:").unwrap();
        queue(&store.connection, "n", "c", "message:m", 1).unwrap();
        let mut record = load_key(&store.connection, &key("n", "c", "message:m"))
            .unwrap()
            .unwrap();
        record.state = MailboxState::Sending;
        record.attempts = MAX_ATTEMPT_COUNT;
        save(&store.connection, &record).unwrap();

        store
            .mailbox_failed("n", "c", "message:m", false, "retry limit", 2)
            .unwrap();

        assert_eq!(
            store
                .mailbox_for_test("n", "c", "message:m")
                .unwrap()
                .map(|record| record.state),
            Some(MailboxState::Queued)
        );
        assert_eq!(store.scheduler_outbox().unwrap().len(), 1);
    }
}
