//! Node-local executor gateway work journal and immutable signed webhook outbox.
use crate::{
    clock::now_ms,
    crypto::{digest, random_hex},
    model::EventBody,
    webhook::{Delivery, OutboxEntry, post_webhook},
    work::{Signal, WorkRecord, WorkSpec, WorkState, accept, transition},
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
};

const DELIVER_BATCH: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Caller {
    pub id: String,
    pub can_write: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    pub spec: WorkSpec,
    pub record: WorkRecord,
    pub thread: Option<String>,
    pub write: bool,
    pub effort: Option<String>,
    pub failure_reason: Option<String>,
    details: super::TurnDetails,
}

impl Entry {
    pub(crate) fn compact(&self) -> Value {
        let outcome = match &self.record.state {
            WorkState::Finished { outcome } => Some(outcome),
            _ => None,
        };
        let state = if self.record.progress == "needs_input"
            && !self.record.state.finished()
            && !matches!(self.record.state, WorkState::Unverified { .. })
        {
            "needs_input"
        } else {
            self.record.state.name()
        };
        json!({
            "work_id": self.record.work_id,
            "state": state,
            "outcome": outcome,
            "final_reply": self.record.text,
            "changed_files": self.details.changed_files,
            "failed_commands": self.details.failed_commands,
            "failure_reason": self.failure_reason,
            "sandbox": if self.write { "workspace-write" } else { "read-only" },
            "write": self.write,
            "effort": self.effort,
            "thread": self.thread,
        })
    }
}

pub(crate) struct GatewayStore {
    connection: Mutex<Connection>,
    changed: tokio::sync::watch::Sender<()>,
}

impl GatewayStore {
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        let dir = dir.join("gateway");
        crate::config::private_dir(&dir)?;
        let path = dir.join("gateway.sqlite3");
        let connection = Connection::open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        connection.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS gateway_callers(
               caller_id TEXT PRIMARY KEY,
               key_hash TEXT NOT NULL UNIQUE,
               can_write INTEGER NOT NULL,
               webhook_url TEXT,
               webhook_secret BLOB,
               created_ms INTEGER NOT NULL,
               CHECK((webhook_url IS NULL) = (webhook_secret IS NULL))
             );
             CREATE TABLE IF NOT EXISTS gateway_entries(
               work_id TEXT PRIMARY KEY,
               caller_id TEXT NOT NULL REFERENCES gateway_callers(caller_id),
               call_ref TEXT NOT NULL,
               spec_json TEXT NOT NULL,
               record_json TEXT NOT NULL,
               thread TEXT,
               write_flag INTEGER NOT NULL,
               effort TEXT,
               failure_reason TEXT,
               details TEXT,
               state TEXT NOT NULL,
               admitted_ms INTEGER NOT NULL,
               UNIQUE(caller_id, call_ref)
             );
             CREATE INDEX IF NOT EXISTS gateway_entries_thread
               ON gateway_entries(thread, admitted_ms);
             CREATE INDEX IF NOT EXISTS gateway_entries_pending
               ON gateway_entries(admitted_ms) WHERE state!='finished';
             CREATE TABLE IF NOT EXISTS gateway_outbox(
               event_id TEXT PRIMARY KEY,
               url TEXT NOT NULL,
               secret BLOB NOT NULL,
               body TEXT NOT NULL,
               state TEXT NOT NULL,
               attempts INTEGER NOT NULL,
               next_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS gateway_outbox_due
               ON gateway_outbox(next_ms,event_id) WHERE state='pending';
             CREATE TABLE IF NOT EXISTS gateway_controls(
               work_id TEXT NOT NULL REFERENCES gateway_entries(work_id),
               control_key TEXT NOT NULL,
               verb TEXT NOT NULL,
               prompt TEXT NOT NULL,
               PRIMARY KEY(work_id, control_key)
             );",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
            changed: tokio::sync::watch::channel(()).0,
        })
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("gateway journal lock poisoned"))
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<()> {
        self.changed.subscribe()
    }

    pub(crate) fn mint(
        &self,
        can_write: bool,
        url: Option<&str>,
        secret: Option<&str>,
    ) -> Result<Value> {
        match (url, secret) {
            (None, None) => {}
            (Some(url), Some(secret)) => {
                let parsed = reqwest::Url::parse(url)
                    .map_err(|_| super::err("invalid_request", "invalid webhook URL"))?;
                if !matches!(parsed.scheme(), "http" | "https")
                    || parsed.host_str().is_none()
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                    || parsed.fragment().is_some()
                    || url.len() > 4096
                    || secret.is_empty()
                    || secret.len() > 4096
                {
                    bail!(super::err(
                        "invalid_request",
                        "invalid webhook destination or secret"
                    ));
                }
            }
            _ => bail!(super::err(
                "invalid_request",
                "webhook_url and webhook_secret must be supplied together"
            )),
        }
        let caller_id = format!("gw_{}", random_hex(8));
        let key = format!("vgw_{}", random_hex(32));
        self.connection()?.execute(
            "INSERT INTO gateway_callers(caller_id,key_hash,can_write,webhook_url,webhook_secret,created_ms)
             VALUES(?,?,?,?,?,?)",
            params![caller_id, digest(key.as_bytes()), can_write as i64, url,
                secret.map(str::as_bytes), now_ms()],
        )?;
        Ok(json!({"caller_id": caller_id, "key": key}))
    }

    pub(crate) fn authenticate(&self, key: &str) -> Result<Caller> {
        self.connection()?
            .query_row(
                "SELECT caller_id,can_write FROM gateway_callers WHERE key_hash=?",
                [digest(key.as_bytes())],
                |row| {
                    Ok(Caller {
                        id: row.get(0)?,
                        can_write: row.get::<_, i64>(1)? != 0,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| super::err("unauthorized", "invalid gateway caller key"))
    }

    pub(crate) fn accept(
        &self,
        caller: &Caller,
        spec: &WorkSpec,
        write: bool,
        effort: Option<&str>,
    ) -> Result<(Entry, bool)> {
        if write && !caller.can_write {
            bail!(super::err(
                "unauthorized",
                "write not allowed for this caller"
            ));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let conversation = format!("gateway:{}", caller.id);
        if spec.conversation != conversation
            || spec.trigger != "gateway"
            || spec.work_id != WorkSpec::id(&conversation, "gateway", &spec.call_ref)
        {
            bail!(super::err(
                "invalid_request",
                "invalid gateway work identity"
            ));
        }
        if let Some(existing) = load_entry(&tx, &spec.work_id)? {
            if !admission_same(&existing, spec, write, effort) {
                bail!(super::err(
                    "idempotency_conflict",
                    "call_ref already used with different work parameters"
                ));
            }
            return Ok((existing, false));
        }
        if let Some(thread) = spec.thread.as_deref() {
            let prior = thread_entry(&tx, thread)?.ok_or_else(|| {
                super::err(
                    "work_not_found",
                    "thread is not owned by this gateway caller",
                )
            })?;
            if prior.spec.conversation != conversation {
                bail!(super::err(
                    "work_not_found",
                    "thread is not owned by this gateway caller"
                ));
            }
            if !prior.record.state.finished() {
                bail!(super::err(
                    "thread_busy",
                    "thread has unfinished gateway work"
                ));
            }
            if prior.spec.kind != spec.kind || prior.spec.cwd != spec.cwd || prior.write != write {
                bail!(super::err(
                    "invalid_request",
                    "continuation must keep provider, cwd and sandbox"
                ));
            }
        }
        let at = now_ms();
        let (record, _) = accept(spec.clone(), at);
        let entry = Entry {
            spec: spec.clone(),
            record,
            thread: spec.thread.clone(),
            write,
            effort: effort.map(str::to_owned),
            failure_reason: None,
            details: Default::default(),
        };
        tx.execute(
            "INSERT INTO gateway_entries(work_id,caller_id,call_ref,spec_json,record_json,thread,write_flag,effort,failure_reason,state,admitted_ms)
             VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            params![entry.record.work_id, caller.id, spec.call_ref, serde_json::to_string(spec)?,
                serde_json::to_string(&entry.record)?, entry.thread, write as i64, effort,
                entry.failure_reason, entry.record.state.name(), at],
        )?;
        tx.commit()?;
        self.changed.send_replace(());
        Ok((entry, true))
    }

    pub(crate) fn find(&self, caller: &Caller, id: &str) -> Result<Option<Entry>> {
        let connection = self.connection()?;
        Ok(load_entry(&connection, id)?
            .filter(|entry| entry.spec.conversation == format!("gateway:{}", caller.id)))
    }

    pub(crate) fn get(&self, caller: &Caller, id: &str) -> Result<Entry> {
        self.find(caller, id)?
            .ok_or_else(|| super::err("work_not_found", "gateway work not found"))
    }

    pub(crate) fn native_thread(&self, thread: &str) -> Result<Option<Entry>> {
        let connection = self.connection()?;
        thread_entry(&connection, thread)
    }

    pub(crate) fn by_thread(&self, caller: &Caller, thread: &str) -> Result<Entry> {
        self.native_thread(thread)?
            .filter(|entry| entry.spec.conversation == format!("gateway:{}", caller.id))
            .ok_or_else(|| super::err("work_not_found", "no gateway work on thread"))
    }

    pub(crate) fn pending(&self) -> Result<Vec<WorkRecord>> {
        let connection = self.connection()?;
        let mut stmt = connection.prepare(
            "SELECT record_json FROM gateway_entries WHERE state!='finished' ORDER BY admitted_ms",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub(crate) fn signal(&self, id: &str, signal: Signal) -> Result<WorkRecord> {
        self.signal_with_details(id, signal, None)
    }

    pub(super) fn signal_with_details(
        &self,
        id: &str,
        signal: Signal,
        details: Option<super::TurnDetails>,
    ) -> Result<WorkRecord> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut entry = load_entry(&tx, id)?
            .ok_or_else(|| super::err("work_not_found", "gateway work not found"))?;
        let before = entry.clone();
        if let Signal::ThreadCreated(thread) = &signal {
            if entry.thread.as_deref().is_some_and(|bound| bound != thread) {
                bail!(super::err(
                    "thread_busy",
                    "work is already bound to a different native thread"
                ));
            }
            if entry.thread.is_none() && thread_entry(&tx, thread)?.is_some() {
                bail!(super::err(
                    "thread_busy",
                    "native thread is already bound to gateway work"
                ));
            }
        }
        // Input requests can arrive between thread/start and turn/start. Keep this marker until
        // a terminal transition; ordinary commentary cannot hide an outstanding input request.
        let event = match &signal {
            Signal::Progress { summary, at_ms }
                if summary == "needs_input"
                    && entry.thread.is_some()
                    && !entry.record.state.finished()
                    && !matches!(entry.record.state, WorkState::Unverified { .. }) =>
            {
                if entry.record.progress != "needs_input" {
                    entry.record.progress = summary.clone();
                    entry.record.progress_at_ms = *at_ms;
                }
                None
            }
            Signal::Progress { .. } if entry.record.progress == "needs_input" => None,
            _ => transition(&mut entry.record, signal),
        };
        if let Some(thread) = entry.record.state.thread() {
            entry.thread = Some(thread.to_owned());
        }
        if let Some(EventBody::WorkFinished { failure_reason, .. }) = event {
            entry.failure_reason = failure_reason;
            if let Some(details) = details {
                entry.details = details;
            }
        }
        let changed = entry != before;
        if changed {
            tx.execute(
                "UPDATE gateway_entries SET record_json=?,thread=?,failure_reason=?,details=?,state=? WHERE work_id=?",
                params![serde_json::to_string(&entry.record)?, entry.thread, entry.failure_reason,
                    serde_json::to_string(&entry.details)?,entry.record.state.name(), id],
            )?;
            if let Some((event_id, kind)) = webhook_kind(&entry) {
                queue_webhook(&tx, &entry, &event_id, kind)?;
            }
        }
        tx.commit()?;
        if changed {
            self.changed.send_replace(());
        }
        Ok(entry.record)
    }

    pub(crate) fn claim_control(
        &self,
        caller: &Caller,
        id: &str,
        key: &str,
        verb: &str,
        prompt: &str,
    ) -> Result<bool> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let entry = load_entry(&tx, id)?
            .filter(|entry| entry.spec.conversation == format!("gateway:{}", caller.id))
            .ok_or_else(|| super::err("work_not_found", "gateway work not found"))?;
        let changed = tx.execute(
            "INSERT INTO gateway_controls(work_id,control_key,verb,prompt) VALUES(?,?,?,?)
             ON CONFLICT(work_id,control_key) DO NOTHING",
            params![entry.record.work_id, key, verb, prompt],
        )?;
        if changed == 0 {
            let stored: (String, String) = tx.query_row(
                "SELECT verb,prompt FROM gateway_controls WHERE work_id=? AND control_key=?",
                params![id, key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if stored != (verb.to_owned(), prompt.to_owned()) {
                bail!(super::err(
                    "idempotency_conflict",
                    "control call_ref already used with different parameters"
                ));
            }
        }
        tx.commit()?;
        Ok(changed != 0)
    }

    pub(crate) async fn deliver(&self) -> Result<()> {
        let rows = {
            let connection = self.connection()?;
            let mut stmt = connection.prepare(
                "SELECT event_id,url,secret,body,attempts,next_ms FROM gateway_outbox
                 WHERE state='pending' AND next_ms<=? ORDER BY next_ms,event_id LIMIT ?",
            )?;
            stmt.query_map(params![now_ms(), DELIVER_BATCH as i64], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    OutboxEntry {
                        event_id: row.get(0)?,
                        body: row.get(3)?,
                        state: Delivery::Pending,
                        attempts: row.get(4)?,
                        next_ms: row.get(5)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (url, secret, mut entry) in rows {
            let (status, _) = post_webhook(&url, &secret, &entry.event_id, &entry.body).await;
            let attempts_before = entry.attempts;
            entry.record(status, now_ms());
            self.connection()?.execute(
                "UPDATE gateway_outbox SET state=?,attempts=?,next_ms=? WHERE event_id=? AND state='pending' AND attempts=?",
                params![delivery_name(entry.state), entry.attempts, entry.next_ms, entry.event_id, attempts_before],
            )?;
        }
        Ok(())
    }
}

fn admission_same(entry: &Entry, spec: &WorkSpec, write: bool, effort: Option<&str>) -> bool {
    entry.spec.prompt == spec.prompt
        && entry.spec.cwd == spec.cwd
        && entry.spec.kind == spec.kind
        && entry.spec.model == spec.model
        && entry.write == write
        && entry.effort.as_deref() == effort
        && entry.spec.thread == spec.thread
        && entry.spec.continues == spec.continues
}

fn thread_entry(connection: &Connection, thread: &str) -> Result<Option<Entry>> {
    let id: Option<String> = connection.query_row(
        "SELECT work_id FROM gateway_entries WHERE thread=? ORDER BY admitted_ms DESC,rowid DESC LIMIT 1",
        [thread], |row| row.get(0),
    ).optional()?;
    id.map(|id| load_entry(connection, &id))
        .transpose()
        .map(Option::flatten)
}

fn load_entry(connection: &Connection, id: &str) -> Result<Option<Entry>> {
    let row = connection.query_row(
        "SELECT spec_json,record_json,thread,write_flag,effort,failure_reason,details FROM gateway_entries WHERE work_id=?",
        [id], |row| Ok((row.get::<_,String>(0)?, row.get::<_,String>(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?,row.get::<_,Option<String>>(6)?)),
    ).optional()?;
    row.map(
        |(spec, record, thread, write, effort, failure_reason, details)| {
            Ok(Entry {
                spec: serde_json::from_str(&spec)?,
                record: serde_json::from_str(&record)?,
                thread,
                write,
                effort,
                failure_reason,
                details: details
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()?
                    .unwrap_or_default(),
            })
        },
    )
    .transpose()
}

fn webhook_kind(entry: &Entry) -> Option<(String, &'static str)> {
    if let WorkState::Finished { outcome } = &entry.record.state {
        return Some((
            format!(
                "terminal-{}-{}",
                entry.record.work_id,
                serde_json::to_value(outcome).ok()?.as_str()?
            ),
            "terminal",
        ));
    }
    if entry.record.progress == "needs_input"
        && !matches!(entry.record.state, WorkState::Unverified { .. })
    {
        return Some((
            format!("needs_input-{}", entry.record.work_id),
            "needs_input",
        ));
    }
    None
}

fn queue_webhook(tx: &Connection, entry: &Entry, event_id: &str, kind: &str) -> Result<()> {
    let caller = entry
        .spec
        .conversation
        .strip_prefix("gateway:")
        .context("gateway caller")?;
    let destination: (Option<String>, Option<Vec<u8>>) = tx.query_row(
        "SELECT webhook_url,webhook_secret FROM gateway_callers WHERE caller_id=?",
        [caller],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (Some(url), Some(secret)) = destination else {
        return Ok(());
    };
    let body = json!({"type": kind, "event_id": event_id, "work": entry.compact()}).to_string();
    let outbox = OutboxEntry::new(event_id, body, now_ms());
    tx.execute(
        "INSERT INTO gateway_outbox(event_id,url,secret,body,state,attempts,next_ms)
         VALUES(?,?,?,?,?,?,?) ON CONFLICT(event_id) DO NOTHING",
        params![
            event_id,
            url,
            secret,
            outbox.body,
            delivery_name(outbox.state),
            outbox.attempts,
            outbox.next_ms
        ],
    )?;
    Ok(())
}

fn delivery_name(state: Delivery) -> &'static str {
    match state {
        Delivery::Pending => "pending",
        Delivery::Delivered => "delivered",
        Delivery::Dead => "dead",
    }
}

#[cfg(test)]
mod tests;
