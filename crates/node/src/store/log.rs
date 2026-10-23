//! One signed append-only log per device. An event is stored once its predecessor and
//! dependencies are. The first chain stored per device is its main chain; an event that forks
//! it is stored beside it as evidence. Which stored events count (`effective`) follows from the
//! governance view and the fork evidence alone, so every replica holding the same evidence
//! agrees.
use super::{rank, reduce};
use crate::{
    crypto::{LocalIdentity, check_event, verify_event_signature},
    identity::{AuthorizationView, WriteBound},
    model::*,
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

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

/// A stored event, by hash.
pub(crate) fn event_from(
    connection: &Connection,
    network: &str,
    hash: &str,
) -> Result<Option<Event>> {
    let envelope: Option<String> = connection
        .query_row(
            "SELECT envelope FROM events WHERE network_id=? AND hash=?",
            params![network, hash],
            |r| r.get(0),
        )
        .optional()?;
    envelope
        .map(|envelope| {
            let envelope: SignedEvent = serde_json::from_str(&envelope)?;
            Ok(serde_json::from_str(&envelope.canonical_body)?)
        })
        .transpose()
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
/// Sets `stale` when the effective set must be recomputed before the batch commits.
pub(crate) fn ingest_from(
    connection: &Connection,
    view: &AuthorizationView,
    envelope: &SignedEvent,
    stale: &mut bool,
) -> Result<IngestStatus> {
    let network = view.network_id.as_str();
    let event = match check_event(network, envelope) {
        Ok(event) => event,
        Err(error) => return Ok(IngestStatus::Rejected(error.to_string())),
    };
    for table in ["events", "pending_events"] {
        if exists(connection, table, network, &envelope.hash)? {
            return Ok(IngestStatus::Duplicate);
        }
    }
    // Writes past a revocation bound are stored but never count; nothing else is admitted.
    if view.write_bound(&event.author).is_none() {
        return Ok(IngestStatus::Rejected("unknown author".into()));
    }
    if let Some(attestation) = event.body.attestation()
        && !view.attester(&attestation.device_id)
    {
        return Ok(IngestStatus::Rejected(
            "attestation is not signed by a client of this network".into(),
        ));
    }
    if let Err(error) = verify_event_signature(envelope, &event.author) {
        return Ok(IngestStatus::Rejected(error.to_string()));
    }
    let status = place(connection, view, envelope, &event, stale)?;
    if matches!(status, IngestStatus::Accepted | IngestStatus::Conflict) {
        cascade(connection, view, &envelope.hash, stale)?;
    }
    Ok(status)
}

fn cascade(
    connection: &Connection,
    view: &AuthorizationView,
    hash: &str,
    stale: &mut bool,
) -> Result<()> {
    let mut ready = vec![hash.to_owned()];
    while let Some(hash) = ready.pop() {
        let waiting: Vec<String> = connection
            .prepare(
                "DELETE FROM pending_events WHERE network_id=? AND missing=? RETURNING envelope",
            )?
            .query_map(params![view.network_id, hash], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for envelope in waiting {
            let envelope: SignedEvent = serde_json::from_str(&envelope)?;
            let event: Event = serde_json::from_str(&envelope.canonical_body)?;
            if matches!(
                place(connection, view, &envelope, &event, stale)?,
                IngestStatus::Accepted | IngestStatus::Conflict
            ) {
                ready.push(envelope.hash);
            }
        }
    }
    Ok(())
}

fn pending(
    connection: &Connection,
    network: &str,
    envelope: &SignedEvent,
    event: &Event,
    missing: &str,
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
        "INSERT INTO pending_events(network_id,hash,author,seq,envelope,missing) VALUES(?,?,?,?,?,?)",
        params![network, envelope.hash, event.author, event.seq, serde_json::to_string(envelope)?, missing],
    )?;
    Ok(IngestStatus::Pending)
}

fn place(
    connection: &Connection,
    view: &AuthorizationView,
    envelope: &SignedEvent,
    event: &Event,
    stale: &mut bool,
) -> Result<IngestStatus> {
    let network = view.network_id.as_str();
    let (max, head_hash) = head(connection, network, &event.author)?.unwrap_or((0, String::new()));
    if let Some(prev) = &event.prev {
        let stored: Option<(String, u64)> = connection
            .query_row(
                "SELECT author,seq FROM events WHERE network_id=? AND hash=?",
                params![network, prev],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match stored {
            None if event.seq <= max + 1
                || exists(connection, "pending_events", network, prev)? =>
            {
                return pending(connection, network, envelope, event, prev);
            }
            None => {
                return Ok(IngestStatus::Rejected(format!(
                    "sequence gap: expected {} got {}",
                    max + 1,
                    event.seq
                )));
            }
            Some((author, seq)) if author != event.author || seq + 1 != event.seq => {
                return Ok(IngestStatus::Rejected(
                    "prev is not this log's previous event".into(),
                ));
            }
            Some(_) => {}
        }
    }
    for dep in &event.deps {
        if !exists(connection, "events", network, dep)? {
            return pending(connection, network, envelope, event, dep);
        }
    }
    if let Err(error) = authorize_cause(connection, network, event) {
        return Ok(IngestStatus::Rejected(error.to_string()));
    }
    if let Err(error) = authorize_owner(connection, network, event) {
        tracing::warn!(event = "sync.event_rejected", author = event.author, seq = event.seq, scope = event.scope_id, %error, "rejected a write only the conversation's scheduling Node makes");
        return Ok(IngestStatus::Rejected(error.to_string()));
    }
    let main = match &event.prev {
        Some(prev) => *prev == head_hash,
        None => max == 0,
    };
    *stale |= !insert_event(connection, view, envelope, event, main)?;
    Ok(if main {
        IngestStatus::Accepted
    } else {
        IngestStatus::Conflict
    })
}

/// The user's device signature on `cause` is what authorizes a write a bot makes for the user:
/// a bot change, or a bot it creates or deletes. One of the event's stored parents must be that
/// signed user message. The one change without a cause is a new one-shot schedule: a reminder
/// the bot sets itself.
fn authorize_cause(connection: &Connection, network: &str, event: &Event) -> Result<()> {
    let cause = match &event.body {
        EventBody::BotDeleted {
            cause: Some(cause), ..
        } => cause,
        EventBody::BotChanged {
            cause: None,
            change,
            ..
        } => {
            ensure!(
                matches!(
                    change,
                    BotChange::Schedule {
                        before: None,
                        after: Some(Schedule {
                            every_minutes: None,
                            depth: 1..,
                            ..
                        }),
                        ..
                    }
                ),
                "only a new one-time reminder may change a bot without the user's message"
            );
            return Ok(());
        }
        EventBody::BotChanged {
            cause: Some(cause), ..
        }
        | EventBody::BotCreated {
            cause: Some(cause), ..
        }
        | EventBody::ConversationOwner {
            cause: Some(cause), ..
        } => cause,
        _ => return Ok(()),
    };
    // A cause makes only a conversation its author just created, never an existing one.
    if let EventBody::ConversationOwner { .. } = &event.body {
        let created = match &event.prev {
            Some(prev) => event_from(connection, network, prev)?,
            None => None,
        };
        ensure!(
            created.is_some_and(|created| created.scope_id == event.scope_id
                && matches!(created.body, EventBody::ConversationCreated { .. })),
            "a created bot's first owner follows its creation"
        );
    }
    for parent in event.prev.iter().chain(&event.deps) {
        let Some(parent) = event_from(connection, network, parent)? else {
            continue;
        };
        if parent.scope_id == cause.conversation_id
            && matches!(&parent.body, EventBody::MessagePosted {
                message_id, role: Role::User, answers: None, attestation: Some(_), ..
            } if *message_id == cause.message_id)
        {
            return Ok(());
        }
    }
    bail!("a bot's write for the user must follow the user's own signed message")
}

/// The conversation whose scheduling Node alone makes this write: a checkpoint, a reminder its
/// bot set itself, or its bot's message to another bot.
fn owner_scope(event: &Event) -> Option<&str> {
    match &event.body {
        EventBody::ContextCheckpoint { .. } | EventBody::BotChanged { cause: None, .. } => {
            Some(&event.scope_id)
        }
        EventBody::BotMessaged { from, .. } => Some(&from.conversation_id),
        _ => None,
    }
}

/// An owner-only write must come from the owner its author knew when writing: of the owner
/// records the event's parents hold (its author's earlier events, and each dependency's log up
/// to that dependency), those of the highest epoch must name the author. An owner change the
/// author had not seen yet does not count against it. The parents are stored before an event
/// is placed, so every replica reads the same records.
fn authorize_owner(connection: &Connection, network: &str, event: &Event) -> Result<()> {
    let Some(scope) = owner_scope(event) else {
        return Ok(());
    };
    let mut seen = BTreeMap::from([(event.author.clone(), event.seq - 1)]);
    for dep in &event.deps {
        let (author, seq): (String, u64) = connection.query_row(
            "SELECT author,seq FROM events WHERE network_id=? AND hash=?",
            params![network, dep],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let known = seen.entry(author).or_default();
        *known = (*known).max(seq);
    }
    let mut statement = connection.prepare(
        "SELECT author,seq,envelope FROM events WHERE network_id=? AND scope_id=? AND instr(envelope,'conversation_owner')>0",
    )?;
    let mut rows = statement.query(params![network, scope])?;
    let mut newest: Option<(u64, bool)> = None;
    while let Some(row) = rows.next()? {
        let (author, seq): (String, u64) = (row.get(0)?, row.get(1)?);
        if seen.get(&author).is_none_or(|known| seq > *known) {
            continue;
        }
        let envelope: SignedEvent = serde_json::from_str(&row.get::<_, String>(2)?)?;
        let EventBody::ConversationOwner {
            scheduler_node,
            epoch,
            ..
        } = serde_json::from_str::<Event>(&envelope.canonical_body)?.body
        else {
            continue;
        };
        let named = scheduler_node == event.author;
        newest = match newest {
            Some((best, any)) if best == epoch => Some((best, any || named)),
            Some((best, _)) if best > epoch => newest,
            _ => Some((epoch, named)),
        };
    }
    ensure!(
        newest.is_some_and(|(_, named)| named),
        "only the conversation's scheduling Node makes this write"
    );
    Ok(())
}

fn has_forks(connection: &Connection, network: &str, author: &str) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM events WHERE network_id=? AND author=? AND main=0 LIMIT 1",
            params![network, author],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Stores the event and applies it at once when it plainly counts. Returns false when only a
/// recompute can tell: it forks its log, or its log has forked.
fn insert_event(
    connection: &Connection,
    view: &AuthorizationView,
    envelope: &SignedEvent,
    event: &Event,
    main: bool,
) -> Result<bool> {
    let network = view.network_id.as_str();
    let settled = main && !has_forks(connection, network, &event.author)?;
    let effective = settled
        && match view.write_bound(&event.author) {
            Some(WriteBound::Unlimited) => true,
            Some(WriteBound::Cutoff(revocation)) => event.seq <= revocation.cutoff_seq,
            None => false,
        };
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
        "INSERT INTO events(network_id,author,seq,hash,scope_id,lamport,created_at_ms,envelope,main,effective) VALUES(?,?,?,?,?,?,?,?,?,?)",
        params![network, event.author, event.seq, envelope.hash, event.scope_id, lamport, event.created_at_ms, serde_json::to_string(envelope)?, main, effective],
    )?;
    for blob in &event.blobs {
        connection.execute(
            "INSERT INTO event_blobs(network_id,hash,blob,size) VALUES(?,?,?,?)",
            params![network, envelope.hash, blob.hash, blob.size],
        )?;
    }
    if main {
        connection.execute(
            "INSERT INTO log_heads(network_id,author,max_seq,head_hash) VALUES(?,?,?,?) ON CONFLICT(network_id,author) DO UPDATE SET max_seq=excluded.max_seq,head_hash=excluded.head_hash",
            params![network, event.author, event.seq, envelope.hash],
        )?;
    }
    if effective {
        reduce::apply(
            connection,
            network,
            &envelope.hash,
            event,
            &rank(lamport, event.created_at_ms, &envelope.hash),
        )?;
    }
    Ok(settled)
}

/// The events that count for one author: a current member's log up to its first fork; a former
/// Node's up to its bound, along the branch ending at the bound's hash when that is stored.
/// Depending on an event never makes another one count.
fn effective_set(
    connection: &Connection,
    view: &AuthorizationView,
    author: &str,
    rows: &[(u64, String, bool, bool)],
) -> Result<BTreeSet<String>> {
    let fork = rows
        .iter()
        .filter(|(_, _, main, _)| !main)
        .map(|(seq, ..)| *seq)
        .min()
        .unwrap_or(u64::MAX);
    let (limit, mut tip) = match view.write_bound(author) {
        None => return Ok(BTreeSet::new()),
        Some(WriteBound::Unlimited) => (fork - 1, None),
        Some(WriteBound::Cutoff(revocation)) => (
            revocation.cutoff_seq.min(fork - 1),
            revocation.cutoff_hash.clone().filter(|hash| {
                revocation.cutoff_seq >= fork
                    && rows
                        .iter()
                        .any(|(seq, row, ..)| row == hash && *seq == revocation.cutoff_seq)
            }),
        ),
    };
    let mut wanted: BTreeSet<String> = rows
        .iter()
        .filter(|(seq, _, main, _)| *main && *seq <= limit)
        .map(|(_, hash, ..)| hash.clone())
        .collect();
    while let Some(hash) = tip {
        let (seq, envelope): (u64, String) = connection.query_row(
            "SELECT seq,envelope FROM events WHERE network_id=? AND hash=?",
            params![view.network_id, hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if seq < fork {
            break;
        }
        let envelope: SignedEvent = serde_json::from_str(&envelope)?;
        tip = serde_json::from_str::<Event>(&envelope.canonical_body)?.prev;
        wanted.insert(hash);
    }
    Ok(wanted)
}

/// Brings every `effective` flag in line with `view` and the fork evidence. When one moves, the
/// projections are rebuilt from the effective events in shared order; local ledgers are left
/// alone. Returns whether the projections were rebuilt.
pub(crate) fn recompute(connection: &Connection, view: &AuthorizationView) -> Result<bool> {
    let network = view.network_id.as_str();
    let mut logs: BTreeMap<String, Vec<(u64, String, bool, bool)>> = BTreeMap::new();
    {
        let mut statement = connection
            .prepare("SELECT author,seq,hash,main,effective FROM events WHERE network_id=?")?;
        let rows = statement.query_map([network], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?),
            ))
        })?;
        for row in rows {
            let (author, row) = row?;
            logs.entry(author).or_default().push(row);
        }
    }
    let mut flips = Vec::new();
    for (author, rows) in &logs {
        let wanted = effective_set(connection, view, author, rows)?;
        for (_, hash, _, effective) in rows {
            if *effective != wanted.contains(hash) {
                flips.push((hash, !effective));
            }
        }
    }
    if flips.is_empty() {
        return Ok(false);
    }
    for (hash, effective) in flips {
        connection.execute(
            "UPDATE events SET effective=? WHERE network_id=? AND hash=?",
            params![effective, network, hash],
        )?;
    }
    reduce::clear(connection, network)?;
    let mut statement = connection.prepare(
        "SELECT hash,lamport,created_at_ms,envelope FROM events WHERE network_id=? AND effective=1
         ORDER BY lamport,created_at_ms,hash",
    )?;
    let mut rows = statement.query([network])?;
    while let Some(row) = rows.next()? {
        let hash: String = row.get(0)?;
        let envelope: SignedEvent = serde_json::from_str(&row.get::<_, String>(3)?)?;
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        reduce::apply(
            connection,
            network,
            &hash,
            &event,
            &rank(row.get(1)?, row.get(2)?, &hash),
        )?;
    }
    Ok(true)
}

/// Appends to this device's log. `deps` name every other author's latest event in the
/// conversation, so the new event follows everything this Node has seen there, and an
/// bot change also names the user message that caused it.
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
    let gate: String =
        connection.query_row("SELECT state FROM temp.write_gate", [], |r| r.get(0))?;
    match gate.as_str() {
        "writable" => {}
        "forked" => bail!("this Node's log has forked and it no longer writes"),
        "read_only" => {
            bail!("this Node is read-only until you confirm it was not restored from a backup")
        }
        _ => bail!("this Node is aligning with other Nodes; try again shortly"),
    }
    let author = identity.device_id()?;
    let view = super::membership_from(connection, network)?;
    view.node(&author)?;
    if let Some(attestation) = body.attestation() {
        view.client(&attestation.device_id)?;
    }
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
    // A bot's message also follows its own conversation, where its sender's ownership is.
    let sender = match &body {
        EventBody::BotMessaged { from, .. } => from.conversation_id.as_str(),
        _ => scope,
    };
    let mut deps = Vec::new();
    for other in authors {
        if let Some(hash) = connection
            .query_row(
                "SELECT hash FROM events WHERE network_id=? AND scope_id IN (?,?) AND author=? AND main=1 ORDER BY seq DESC LIMIT 1",
                params![network, scope, sender, other],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            deps.push(hash);
        }
    }
    let prev = (max > 0).then_some(head_hash);
    if let EventBody::BotChanged {
        cause: Some(cause), ..
    }
    | EventBody::BotCreated {
        cause: Some(cause), ..
    }
    | EventBody::BotDeleted {
        cause: Some(cause), ..
    }
    | EventBody::ConversationOwner {
        cause: Some(cause), ..
    } = &body
    {
        let message = super::message_from(
            connection,
            network,
            &cause.conversation_id,
            &cause.message_id,
        )?
        .context("the user's message is not stored on this device")?;
        if prev.as_ref() != Some(&message.event_hash) {
            deps.push(message.event_hash);
        }
    }
    let envelope = identity.sign_event(Event {
        version: WIRE_VERSION,
        network_id: network.into(),
        author,
        seq: max + 1,
        prev,
        deps,
        scope_id: scope.into(),
        created_at_ms,
        body,
        blobs,
    })?;
    let event = check_event(network, &envelope).context("appended event is invalid")?;
    authorize_cause(connection, network, &event)?;
    authorize_owner(connection, network, &event)?;
    ensure!(
        insert_event(connection, &view, &envelope, &event, true)?,
        "this Node's log has forked and it no longer writes"
    );
    Ok(envelope)
}
