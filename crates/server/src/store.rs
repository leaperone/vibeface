use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use vibeface_protocol::{BridgeEvent, CommandRecord, CommandRequest, CommandStatus, now_ms};

pub struct Store(Connection);

impl Store {
    pub fn open(path: &Path, device_id: &str) -> Result<Self> {
        #[cfg(unix)]
        if path != Path::new(":memory:") {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .mode(0o600)
                .open(path)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        let store = Self(Connection::open(path)?);
        store.0.pragma_update(None, "journal_mode", "WAL")?;
        store.0.execute_batch(
            "CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS commands (id TEXT PRIMARY KEY, data TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS commands_status ON commands(json_extract(data,'$.status'));
             CREATE TABLE IF NOT EXISTS events (seq INTEGER PRIMARY KEY, data TEXT NOT NULL);",
        )?;
        match store.metadata("device_id")? {
            Some(existing) if existing != device_id => bail!("database belongs to another device"),
            None => store.set_metadata("device_id", device_id)?,
            _ => {}
        }
        store.mark_unknown()?;
        Ok(store)
    }

    fn metadata(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    fn set_metadata(&self, key: &str, value: &str) -> Result<()> {
        self.0.execute("INSERT INTO metadata VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])?;
        Ok(())
    }

    pub fn seen(&self) -> Result<()> {
        self.set_metadata("last_seen_ms", &now_ms().to_string())
    }

    pub fn last_seen(&self) -> Result<Option<u64>> {
        self.metadata("last_seen_ms")?
            .map(|s| s.parse().context("invalid last_seen_ms"))
            .transpose()
    }

    pub fn command(&self, id: &str) -> Result<Option<CommandRecord>> {
        let data: Option<String> = self
            .0
            .query_row("SELECT data FROM commands WHERE id=?1", [id], |r| r.get(0))
            .optional()?;
        data.map(|s| serde_json::from_str(&s).context("invalid command record"))
            .transpose()
    }

    pub fn insert(&self, request: CommandRequest) -> Result<CommandRecord> {
        let record = CommandRecord::new(request, CommandStatus::Queued);
        self.0.execute(
            "INSERT INTO commands VALUES (?1,?2)",
            params![record.request.id, serde_json::to_string(&record)?],
        )?;
        Ok(record)
    }

    pub fn update(&self, record: &CommandRecord) -> Result<()> {
        if let Some(existing) = self.command(&record.request.id)? {
            if existing.request != record.request {
                bail!("command content does not match dispatch");
            }
            if !existing.status.is_pending()
                || existing.status == CommandStatus::Running
                    && record.status == CommandStatus::Queued
                || existing.status == record.status && existing.updated_at_ms > record.updated_at_ms
            {
                return Ok(());
            }
        }
        self.0.execute(
            "INSERT INTO commands VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
            params![record.request.id, serde_json::to_string(record)?],
        )?;
        Ok(())
    }

    pub fn command_unknown(&self, id: &str, error: String) -> Result<()> {
        let mut record = self.command(id)?.context("unknown command id")?;
        if record.status.is_pending() {
            record.status = CommandStatus::Unknown;
            record.error = Some(error);
            record.updated_at_ms = now_ms();
            self.0.execute(
                "UPDATE commands SET data=?2 WHERE id=?1",
                params![id, serde_json::to_string(&record)?],
            )?;
        }
        Ok(())
    }

    pub fn pending_ids(&self) -> Result<Vec<String>> {
        let mut query = self.0.prepare("SELECT id FROM commands WHERE json_extract(data,'$.status') IN ('queued','running','unknown') ORDER BY rowid")?;
        let ids = query.query_map([], |r| r.get::<_, String>(0))?;
        Ok(ids.collect::<std::result::Result<_, _>>()?)
    }

    pub fn mark_unknown(&self) -> Result<()> {
        self.0.execute("UPDATE commands SET data=json_set(data,'$.status','unknown','$.updated_at_ms',?1) WHERE json_extract(data,'$.status') IN ('queued','running')", [now_ms() as i64])?;
        Ok(())
    }

    pub fn last_seq(&self) -> Result<u64> {
        Ok(self
            .0
            .query_row("SELECT COALESCE(MAX(seq),0) FROM events", [], |r| r.get(0))?)
    }

    pub fn event(&self, event: &BridgeEvent) -> Result<bool> {
        if event.seq == 0 || event.seq > i64::MAX as u64 {
            bail!("invalid event sequence");
        }
        let data: Option<String> = self
            .0
            .query_row(
                "SELECT data FROM events WHERE seq=?1",
                [event.seq as i64],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(data) = data {
            if serde_json::from_str::<BridgeEvent>(&data)? != *event {
                bail!("event sequence already exists with different content");
            }
            return Ok(false);
        }
        if event.seq != self.last_seq()? + 1 {
            bail!("event sequence must follow the persisted cursor");
        }
        self.0.execute(
            "INSERT INTO events VALUES (?1,?2)",
            params![event.seq as i64, serde_json::to_string(event)?],
        )?;
        Ok(true)
    }

    pub fn events(&self, after: u64, limit: usize) -> Result<Vec<BridgeEvent>> {
        if after > i64::MAX as u64 {
            bail!("event cursor is too large");
        }
        let mut query = self
            .0
            .prepare("SELECT data FROM events WHERE seq>?1 ORDER BY seq LIMIT ?2")?;
        let rows = query.query_map(params![after as i64, limit as i64], |r| {
            r.get::<_, String>(0)
        })?;
        let mut events = Vec::new();
        let mut bytes = 0;
        for row in rows {
            let data = row?;
            if !events.is_empty() && bytes + data.len() > 1_048_576 {
                break;
            }
            bytes += data.len();
            events.push(serde_json::from_str(&data)?);
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use vibeface_protocol::Command;

    #[test]
    fn local_updates_and_terminal_replays_are_idempotent() {
        let store = Store::open(Path::new(":memory:"), "test").unwrap();
        let request = CommandRequest {
            id: "local-1".into(),
            command: Command::Snapshot,
        };
        let running = CommandRecord::new(request.clone(), CommandStatus::Running);
        store.update(&running).unwrap();
        assert_eq!(store.command("local-1").unwrap(), Some(running.clone()));
        let done = running.clone().finish(Ok(json!({"done":true})));
        store.update(&done).unwrap();
        store.update(&running).unwrap();
        store
            .update(&CommandRecord::new(request.clone(), CommandStatus::Queued))
            .unwrap();
        store
            .command_unknown("local-1", "missing ledger".into())
            .unwrap();
        assert_eq!(store.command("local-1").unwrap(), Some(done));
        assert!(
            store
                .command_unknown("nonexistent", "missing ledger".into())
                .is_err()
        );
        store
            .insert(CommandRequest {
                id: "pending-1".into(),
                ..request
            })
            .unwrap();
        store
            .command_unknown("pending-1", "missing ledger".into())
            .unwrap();
        let unknown = store.command("pending-1").unwrap().unwrap();
        assert_eq!(unknown.status, CommandStatus::Unknown);
        assert_eq!(unknown.error.as_deref(), Some("missing ledger"));
        assert_eq!(unknown.request.command, Command::Snapshot);
    }

    #[test]
    fn command_retry_conflict_and_disconnect_are_safe() {
        let store = Store::open(Path::new(":memory:"), "test").unwrap();
        let request = CommandRequest {
            id: "cmd-1".into(),
            command: Command::Snapshot,
        };
        let record = store.insert(request.clone()).unwrap();
        assert_eq!(store.command(&request.id).unwrap(), Some(record));
        let changed = CommandRequest {
            command: Command::ListThreads { cursor: None },
            ..request.clone()
        };
        assert!(
            store
                .update(&CommandRecord::new(changed, CommandStatus::Running))
                .is_err()
        );
        store.mark_unknown().unwrap();
        assert_eq!(
            store.command("cmd-1").unwrap().unwrap().status,
            CommandStatus::Unknown
        );
        assert_eq!(store.pending_ids().unwrap(), vec!["cmd-1"]);
        let done =
            CommandRecord::new(request, CommandStatus::Running).finish(Ok(json!({"done":true})));
        store.update(&done).unwrap();
        store.mark_unknown().unwrap();
        assert_eq!(
            store.command("cmd-1").unwrap().unwrap().status,
            CommandStatus::Succeeded
        );
    }

    #[test]
    fn event_retry_conflict_and_cursor_preserve_order() {
        let store = Store::open(Path::new(":memory:"), "test").unwrap();
        let first = BridgeEvent {
            seq: 1,
            at_ms: 1,
            kind: "turn/completed".into(),
            thread_id: None,
            turn_id: None,
            payload: json!({"done":true}),
        };
        assert!(store.event(&first).unwrap());
        assert!(!store.event(&first).unwrap());
        assert!(
            store
                .event(&BridgeEvent {
                    payload: json!({"done":false}),
                    ..first.clone()
                })
                .is_err()
        );
        assert!(
            store
                .event(&BridgeEvent {
                    seq: 3,
                    ..first.clone()
                })
                .is_err()
        );
        let second = BridgeEvent {
            seq: 2,
            ..first.clone()
        };
        store.event(&second).unwrap();
        assert_eq!(store.events(0, 1).unwrap(), vec![first]);
        assert_eq!(store.events(1, 100).unwrap(), vec![second]);
        assert_eq!(store.last_seq().unwrap(), 2);
    }
}
