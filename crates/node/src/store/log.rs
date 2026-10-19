//! One signed append-only log per device. An event is stored once its predecessor and
//! dependencies are; a second event at the same `(author, seq)` is kept aside as a conflict.
use super::{rank, reduce};
use crate::{
    crypto::{LocalIdentity, check_event, verify_event_signature},
    model::*,
};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

pub const MAX_PENDING_EVENTS: i64 = 10_000;

pub(crate) fn blob_path(dir: &Path, hash: &str) -> PathBuf {
    dir.join("sha256")
        .join(&hash[..2.min(hash.len())])
        .join(hash)
}

pub(crate) fn blob_file_matches(path: &Path, blob: &BlobRef) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() == blob.size)
}

fn exists(connection: &Connection, table: &str, network: &str, hash: &str) -> Result<bool> {
    Ok(connection
        .query_row(
            &format!("SELECT 1 FROM {table} WHERE network_id=? AND hash=?"),
            params![network, hash],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(crate) fn scope_has_events(
    connection: &Connection,
    network: &str,
    scope: &str,
) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM events WHERE network_id=? AND scope_id=? LIMIT 1",
            params![network, scope],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(crate) fn version_vector_from(connection: &Connection, network: &str) -> Result<VersionVector> {
    let mut statement =
        connection.prepare("SELECT author,max_seq FROM log_heads WHERE network_id=?")?;
    let rows = statement.query_map([network], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn head(connection: &Connection, network: &str, author: &str) -> Result<Option<(u64, String)>> {
    Ok(connection
        .query_row(
            "SELECT max_seq,head_hash FROM log_heads WHERE network_id=? AND author=?",
            params![network, author],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// Verifies once, then places; events waiting on this one are placed without re-verifying.
pub(crate) fn ingest_from(
    connection: &Connection,
    network: &str,
    envelope: &SignedEvent,
    now: i64,
) -> Result<IngestStatus> {
    let event = match check_event(network, envelope) {
        Ok(event) => event,
        Err(error) => return Ok(IngestStatus::Rejected(error.to_string())),
    };
    for table in ["events", "pending_events", "log_conflicts"] {
        if exists(connection, table, network, &envelope.hash)? {
            return Ok(IngestStatus::Duplicate);
        }
    }
    let key: Option<String> = connection
        .query_row(
            "SELECT public_key FROM node_certificates WHERE network_id=? AND peer_id=?",
            params![network, event.author],
            |r| r.get(0),
        )
        .optional()?;
    let Some(key) = key else {
        return Ok(IngestStatus::Rejected("unknown author".into()));
    };
    if let Err(error) = verify_event_signature(envelope, &key) {
        return Ok(IngestStatus::Rejected(error.to_string()));
    }
    let status = place(connection, network, envelope, &event, now)?;
    if matches!(status, IngestStatus::Accepted | IngestStatus::Conflict) {
        cascade(connection, network, &envelope.hash, now)?;
    }
    Ok(status)
}

fn cascade(connection: &Connection, network: &str, hash: &str, now: i64) -> Result<()> {
    let mut ready = vec![hash.to_owned()];
    while let Some(hash) = ready.pop() {
        let waiting: Vec<String> = connection
            .prepare(
                "DELETE FROM pending_events WHERE network_id=? AND missing=? RETURNING envelope",
            )?
            .query_map(params![network, hash], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for envelope in waiting {
            let envelope: SignedEvent = serde_json::from_str(&envelope)?;
            let event: Event = serde_json::from_str(&envelope.canonical_body)?;
            if matches!(
                place(connection, network, &envelope, &event, now)?,
                IngestStatus::Accepted | IngestStatus::Conflict
            ) {
                ready.push(envelope.hash);
            }
        }
    }
    Ok(())
}

fn conflict(
    connection: &Connection,
    network: &str,
    envelope: &SignedEvent,
    event: &Event,
    kept: &str,
    now: i64,
) -> Result<IngestStatus> {
    connection.execute(
        "INSERT INTO log_conflicts(network_id,hash,author,seq,envelope,kept_hash,detected_at_ms) VALUES(?,?,?,?,?,?,?)",
        params![network, envelope.hash, event.author, event.seq, serde_json::to_string(envelope)?, kept, now],
    )?;
    Ok(IngestStatus::Conflict)
}

fn pending(
    connection: &Connection,
    network: &str,
    envelope: &SignedEvent,
    event: &Event,
    missing: &str,
    now: i64,
) -> Result<IngestStatus> {
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM pending_events WHERE network_id=?",
        [network],
        |r| r.get(0),
    )?;
    if count >= MAX_PENDING_EVENTS {
        return Ok(IngestStatus::Rejected("too many pending events".into()));
    }
    connection.execute(
        "INSERT INTO pending_events(network_id,hash,author,seq,envelope,missing,received_at_ms) VALUES(?,?,?,?,?,?,?)",
        params![network, envelope.hash, event.author, event.seq, serde_json::to_string(envelope)?, missing, now],
    )?;
    Ok(IngestStatus::Pending)
}

fn place(
    connection: &Connection,
    network: &str,
    envelope: &SignedEvent,
    event: &Event,
    now: i64,
) -> Result<IngestStatus> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT hash FROM events WHERE network_id=? AND author=? AND seq=?",
            params![network, event.author, event.seq],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(kept) = stored {
        return conflict(connection, network, envelope, event, &kept, now);
    }
    let (max, head_hash) = head(connection, network, &event.author)?.unwrap_or((0, String::new()));
    if let Some(prev) = &event.prev
        && exists(connection, "log_conflicts", network, prev)?
    {
        return conflict(connection, network, envelope, event, &head_hash, now);
    }
    if event.seq > max + 1 {
        return match &event.prev {
            Some(prev) if exists(connection, "pending_events", network, prev)? => {
                pending(connection, network, envelope, event, prev, now)
            }
            _ => Ok(IngestStatus::Rejected(format!(
                "sequence gap: expected {} got {}",
                max + 1,
                event.seq
            ))),
        };
    }
    if event.prev.as_deref().unwrap_or("") != head_hash {
        return conflict(connection, network, envelope, event, &head_hash, now);
    }
    for dep in &event.deps {
        if !exists(connection, "events", network, dep)?
            && !exists(connection, "log_conflicts", network, dep)?
        {
            return pending(connection, network, envelope, event, dep, now);
        }
    }
    insert_event(connection, network, envelope, event)?;
    Ok(IngestStatus::Accepted)
}

fn insert_event(
    connection: &Connection,
    network: &str,
    envelope: &SignedEvent,
    event: &Event,
) -> Result<()> {
    let mut lamport = 0u64;
    for parent in event.prev.iter().chain(&event.deps) {
        let parent: Option<u64> = connection
            .query_row(
                "SELECT lamport FROM events WHERE network_id=? AND hash=?",
                params![network, parent],
                |r| r.get(0),
            )
            .optional()?;
        lamport = lamport.max(parent.unwrap_or(0));
    }
    lamport += 1;
    connection.execute(
        "INSERT INTO events(network_id,author,seq,hash,scope_id,lamport,created_at_ms,envelope) VALUES(?,?,?,?,?,?,?,?)",
        params![network, event.author, event.seq, envelope.hash, event.scope_id, lamport, event.created_at_ms, serde_json::to_string(envelope)?],
    )?;
    for blob in &event.blobs {
        connection.execute(
            "INSERT INTO event_blobs(network_id,hash,blob,size) VALUES(?,?,?,?)",
            params![network, envelope.hash, blob.hash, blob.size],
        )?;
    }
    connection.execute(
        "INSERT INTO log_heads(network_id,author,max_seq,head_hash) VALUES(?,?,?,?) ON CONFLICT(network_id,author) DO UPDATE SET max_seq=excluded.max_seq,head_hash=excluded.head_hash",
        params![network, event.author, event.seq, envelope.hash],
    )?;
    reduce::apply(
        connection,
        network,
        &envelope.hash,
        event,
        &rank(lamport, event.created_at_ms, &envelope.hash),
    )
}

/// Appends to this device's log. `deps` name every other author's latest event in the
/// conversation, so the new event follows everything this Node has seen there.
#[expect(clippy::too_many_arguments, reason = "an event's fields")]
pub(crate) fn append_from(
    connection: &Connection,
    blob_dir: &Path,
    identity: &LocalIdentity,
    network: &str,
    scope: &str,
    body: EventBody,
    blobs: Vec<BlobRef>,
    created_at_ms: i64,
) -> Result<SignedEvent> {
    let author = identity.peer_id()?;
    super::authorize_node_from(connection, network, &author, None)?;
    for blob in &blobs {
        ensure!(
            blob_file_matches(&blob_path(blob_dir, &blob.hash), blob),
            "attachment content is missing"
        );
    }
    let (max, head_hash) = head(connection, network, &author)?.unwrap_or((0, String::new()));
    let authors: Vec<String> = connection
        .prepare("SELECT author FROM log_heads WHERE network_id=? AND author<>?")?
        .query_map(params![network, author], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut deps = Vec::new();
    for other in authors {
        if let Some(hash) = connection
            .query_row(
                "SELECT hash FROM events WHERE network_id=? AND scope_id=? AND author=? ORDER BY seq DESC LIMIT 1",
                params![network, scope, other],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            deps.push(hash);
        }
    }
    let envelope = identity.sign_event(Event {
        version: WIRE_VERSION,
        network_id: network.into(),
        author,
        seq: max + 1,
        prev: (max > 0).then_some(head_hash),
        deps,
        scope_id: scope.into(),
        created_at_ms,
        body,
        blobs,
    })?;
    let event = check_event(network, &envelope).context("appended event is invalid")?;
    insert_event(connection, network, &envelope, &event)?;
    Ok(envelope)
}
