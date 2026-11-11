//! One SQLite store per Node. Business events form one signed log per device; projections are
//! updated incrementally as each effective event is stored, and rebuilt from the effective
//! events when governance or fork evidence changes which events count.
use crate::{
    crypto::{LocalIdentity, digest},
    identity::{AuthorizationView, Member, NetworkState},
    model::*,
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use vibeface_bridge::platform::privatefs;

pub(crate) mod admission;
pub(crate) mod bot_settings;
pub(crate) mod bots;
mod log;
pub(crate) mod mailbox;
pub(crate) mod questions;
mod reduce;
pub(crate) mod scheduler;
pub(crate) mod scope_projection;
mod sync;
pub(crate) mod triage;
pub use sync::{MAX_PULL_BYTES, MAX_PULL_EVENTS};
mod schema_guard;
#[cfg(test)]
mod tests;
pub(crate) mod work;
pub(crate) mod workspace;

const INITIAL_SCHEMA: &str = include_str!("../migrations/1_initial.sql");
pub const SCHEMA_VERSION: u32 = crate::config::DATA_EPOCH;

/// Whether this Node may append to its own log (design 5.7 and 5.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteState {
    /// Started from a database file it never wrote from, with other member Nodes; waits for
    /// them to confirm it holds nothing newer of ours.
    Aligning,
    /// No other member Node answered; writable once the user confirms no backup was restored.
    ReadOnly,
    Writable,
    /// Another copy of this device's log wrote different events at the same sequence.
    Forked,
}

impl WriteState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aligning => "aligning",
            Self::ReadOnly => "read_only",
            Self::Writable => "writable",
            Self::Forked => "forked",
        }
    }
}

/// One ingested batch: each envelope's status, the events it stored on their authors' main
/// chains (an event that waited for an ancestor is stored with that ancestor), and whether it
/// changed which events count and so rebuilt the projections.
pub struct Ingested {
    pub statuses: Vec<IngestStatus>,
    pub stored: Vec<HashedEvent>,
    pub rebuilt: bool,
}

pub struct Store {
    pub(crate) connection: Connection,
    blob_dir: PathBuf,
    process_lock: Option<Arc<std::fs::File>>,
}

pub(crate) struct HistoryFilter<'a> {
    pub kinds: &'a [String],
    pub author: Option<&'a str>,
    pub keyword: Option<&'a str>,
}

pub(crate) fn network_from(connection: &Connection) -> Result<Option<NetworkId>> {
    let mut statement = connection.prepare("SELECT network_id FROM networks LIMIT 2")?;
    let mut rows = statement.query([])?;
    let network = rows.next()?.map(|row| row.get(0)).transpose()?;
    ensure!(
        rows.next()?.is_none(),
        "Node has more than one personal network"
    );
    Ok(network)
}

/// Merges `state` into the stored history. Returns whether the projections were rebuilt.
///
/// A recovery takes effect with time alone, and a late veto can put the earlier directory back, so
/// this also rebuilds whenever the same history now has another recovery key in effect; that voids
/// every invite and join request. Nothing here is refused for undoing a recovery: the directory in
/// effect is a function of the signed history and the wall clock, and the rebuild follows it.
pub(crate) fn install_trust_bundle_in(
    connection: &Connection,
    network_id: &str,
    state: &NetworkState,
) -> Result<bool> {
    ensure!(
        state.genesis.network_id == network_id,
        "membership belongs to another personal network"
    );
    ensure!(
        network_from(connection)?.is_none_or(|installed| installed == network_id),
        "Node already belongs to another personal network"
    );
    state.project()?;
    let previous: Option<String> = connection
        .query_row(
            "SELECT bundle FROM networks WHERE network_id=?",
            [network_id],
            |r| r.get(0),
        )
        .optional()?;
    let merged = match &previous {
        Some(previous) => serde_json::from_str::<NetworkState>(previous)?.merge(state)?,
        None => state.clone(),
    };
    let encoded = serde_json::to_string(&merged)?;
    let view = merged.project()?;
    let changed = previous.as_ref() != Some(&encoded);
    let flipped = recovery_since_ms(connection, &view.recovery_key)?.is_none();
    if !changed && !flipped {
        return Ok(false);
    }
    if changed {
        connection.execute("INSERT INTO networks(network_id,bundle) VALUES(?,?) ON CONFLICT(network_id) DO UPDATE SET bundle=excluded.bundle",params![network_id,encoded])?;
    }
    if flipped {
        connection.execute(
            "INSERT INTO identity_meta(key,value) VALUES('recovery_since',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [format!("{}:{}", view.recovery_key, crate::runtime::now_ms())],
        )?;
        connection.execute("DELETE FROM invites", [])?;
    }
    log::recompute(connection, &view)
}

/// The recovery key this Node last saw in effect, and since when. Shown only, never a reason to
/// refuse a history.
fn recovery_since(connection: &Connection) -> Result<Option<(String, i64)>> {
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM identity_meta WHERE key='recovery_since'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(value
        .as_deref()
        .and_then(|value| value.split_once(':'))
        .and_then(|(key, ms)| Some((key.to_owned(), ms.parse().ok()?))))
}

/// When this Node first saw `recovery_key`; signed records carry no time of their own.
pub(crate) fn recovery_since_ms(
    connection: &Connection,
    recovery_key: &str,
) -> Result<Option<i64>> {
    Ok(recovery_since(connection)?
        .filter(|(key, _)| key == recovery_key)
        .map(|(_, ms)| ms))
}

/// Total order shared by prompts and UI: a Kahn sort of the DAG with ties broken by
/// `(created_at_ms, hash)`. Lamport clocks make it a plain string comparison.
pub(crate) fn rank(lamport: u64, created_at_ms: i64, hash: &str) -> String {
    format!("{lamport:020}:{created_at_ms:020}:{hash}")
}

/// What a client names to change a projected row: the start of the hash of the event that last
/// set it, which ends the row's rank (or is the event's hash). Any change gives a new version.
pub(crate) fn version(rank: &str) -> String {
    rank.rsplit(':')
        .next()
        .unwrap_or(rank)
        .chars()
        .take(16)
        .collect()
}

pub(crate) fn private_file_connection(path: &Path) -> Result<Connection> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("database requires a private directory")?;
    if !parent.exists() {
        crate::config::private_dir(parent)?;
    }
    let metadata = std::fs::symlink_metadata(parent)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "database directory must not be a link"
    );
    privatefs::ensure_private(parent, "the database directory")?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            privatefs::is_plain_file(&metadata),
            "database must be a regular file"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            privatefs::create_new(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    privatefs::restrict_file(path)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let name = PathBuf::from(name);
        match std::fs::symlink_metadata(&name) {
            Ok(metadata) => {
                ensure!(
                    privatefs::is_plain_file(&metadata),
                    "database sidecar must be a regular file"
                );
                privatefs::restrict_file(&name)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(Connection::open_with_flags(
        path.canonicalize()?,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?)
}

fn blob_dir_for(path: &Path) -> PathBuf {
    if path == Path::new(":memory:") {
        std::env::temp_dir().join(format!("vibeface-blobs-{}", uuid::Uuid::new_v4()))
    } else {
        path.parent().unwrap_or(Path::new(".")).join("blobs")
    }
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let connection = if path == Path::new(":memory:") {
            Connection::open_in_memory()?
        } else {
            private_file_connection(path)?
        };
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: u32 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        match version {
            0 => {
                let objects: i64 = connection.query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                    [],
                    |r| r.get(0),
                )?;
                ensure!(objects == 0, "unversioned database is not supported");
                connection.execute_batch(&format!(
                    "BEGIN IMMEDIATE;{INITIAL_SCHEMA}PRAGMA user_version = {SCHEMA_VERSION};COMMIT;"
                ))?;
            }
            SCHEMA_VERSION => {
                schema_guard::validate_application_schema(&connection)?;
            }
            _ => bail!(
                "unsupported database schema version {version}; initialize a fresh Node data directory"
            ),
        }
        let store = Self {
            connection,
            blob_dir: blob_dir_for(path),
            process_lock: None,
        };
        if let Some(network) = store.network()? {
            store
                .trust_bundle(&network)?
                .context("membership is missing")?
                .project()?;
        }
        store.connection.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;
            CREATE TEMP TABLE write_gate(state TEXT NOT NULL); INSERT INTO write_gate VALUES('writable');",
        )?;
        Ok(store)
    }
    #[cfg(test)]
    pub fn install_trust_bundle(&mut self, network_id: &str, state: &NetworkState) -> Result<bool> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = install_trust_bundle_in(&tx, network_id, state)?;
        tx.commit()?;
        Ok(changed)
    }
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(
            privatefs::is_plain_file(&metadata),
            "database must be a regular file"
        );
        let connection = Connection::open_with_flags(
            path.canonicalize()?,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(
            version == SCHEMA_VERSION,
            "unsupported database schema version {version}"
        );
        schema_guard::validate_application_schema(&connection)?;
        network_from(&connection)?;
        Ok(Self {
            connection,
            blob_dir: blob_dir_for(path),
            process_lock: None,
        })
    }
    pub fn trust_bundle(&self, network_id: &str) -> Result<Option<NetworkState>> {
        optional_json(
            self.connection
                .query_row(
                    "SELECT bundle FROM networks WHERE network_id=?",
                    [network_id],
                    |r| r.get(0),
                )
                .optional()?,
        )
    }
    /// Calls `written` whenever any log head is written, whichever path appended or ingested it.
    pub(crate) fn on_log_head_written(
        &mut self,
        written: impl Fn() + Send + 'static,
    ) -> Result<()> {
        self.connection
            .update_hook(Some(move |_, _: &str, table: &str, _| {
                if table == "log_heads" {
                    written();
                }
            }))?;
        Ok(())
    }
    /// The member record of a current, unrevoked Node.
    pub fn authorize_node(&self, network_id: &str, device_id: &str) -> Result<Member> {
        authorize_node_from(&self.connection, network_id, device_id)
    }
    /// Stores each event or rejects invalid input; database and internal errors roll back the batch.
    /// Signatures are verified here, once; stored events are trusted afterwards.
    pub fn ingest_events(
        &mut self,
        network_id: &str,
        envelopes: &[SignedEvent],
    ) -> Result<Ingested> {
        self.ingest_events_with_seen_at(network_id, envelopes, crate::runtime::now_ms())
    }

    #[cfg(test)]
    pub(crate) fn ingest_events_at(
        &mut self,
        network_id: &str,
        envelopes: &[SignedEvent],
        seen_at_ms: i64,
    ) -> Result<Ingested> {
        self.ingest_events_with_seen_at(network_id, envelopes, seen_at_ms)
    }

    fn ingest_events_with_seen_at(
        &mut self,
        network_id: &str,
        envelopes: &[SignedEvent],
        seen_at_ms: i64,
    ) -> Result<Ingested> {
        ensure!(envelopes.len() <= 1024, "event batch is too large");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let view = membership_from(&tx, network_id)?;
        let mut placed = log::Placed::default();
        let statuses = envelopes
            .iter()
            .map(|envelope| log::ingest_from(&tx, &view, envelope, seen_at_ms, &mut placed))
            .collect::<Result<Vec<_>>>()?;
        let rebuilt = placed.stale && log::recompute(&tx, &view)?;
        tx.commit()?;
        Ok(Ingested {
            statuses,
            stored: placed.stored,
            rebuilt,
        })
    }

    pub fn append_event(
        &mut self,
        identity: &LocalIdentity,
        network_id: &str,
        scope_id: &str,
        body: EventBody,
        blobs: Vec<BlobRef>,
        created_at_ms: i64,
    ) -> Result<SignedEvent> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let envelope = log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network_id,
            scope_id,
            body,
            blobs,
            created_at_ms,
        )?;
        tx.commit()?;
        Ok(envelope)
    }

    /// Creates a conversation scheduled by the Node the user signed for.
    pub fn create_conversation(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        scope: &str,
        title: &str,
        owner: FirstOwner,
        created_at_ms: i64,
    ) -> Result<Conversation> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = conversation_from(&tx, network, scope)? {
            ensure!(existing.title == title, "conversation ID already exists");
            return Ok(existing);
        }
        ensure!(
            !log::scope_has_events(&tx, network, scope)?,
            "conversation ID already exists"
        );
        create_conversation_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            scope,
            title,
            owner,
            created_at_ms,
        )?;
        let conversation =
            conversation_from(&tx, network, scope)?.context("conversation missing")?;
        tx.commit()?;
        Ok(conversation)
    }

    pub fn event(&self, network_id: &str, hash: &str) -> Result<Option<SignedEvent>> {
        optional_json(
            self.connection
                .query_row(
                    "SELECT envelope FROM events WHERE network_id=? AND hash=?",
                    params![network_id, hash],
                    |r| r.get(0),
                )
                .optional()?,
        )
    }

    /// `{author: max_seq}` of stored events: every stored event's dependencies are stored too.
    pub fn version_vector(&self, network_id: &str) -> Result<VersionVector> {
        log::version_vector_from(&self.connection, network_id)
    }

    /// What a turn's prompt reads at the job's basis: the events after the newest checkpoint, or
    /// after what the resumed session `resumed` has seen, plus the trigger and the question it
    /// answers wherever they sit. Covered history stays on disk.
    pub(crate) fn turn_history(
        &self,
        network_id: &str,
        scope_id: &str,
        job: &crate::scheduling::SchedulerJob,
        resumed: Option<(&VersionVector, &str)>,
    ) -> Result<crate::scheduler::TurnHistory> {
        let basis = Some(&job.basis);
        let connection = &self.connection;
        let (checkpoint, since) = match resumed {
            Some((covers, _)) => (None, covers.clone()),
            None if job.scope == crate::scheduling::SessionScope::Main => {
                match latest_checkpoint_from(connection, network_id, scope_id, basis)? {
                    Some((covers, body)) => (Some(body), covers),
                    None => (None, VersionVector::new()),
                }
            }
            None => (None, VersionVector::new()),
        };
        let mut recent = context_after(connection, network_id, scope_id, basis, &since)?;
        recent.events = recent
            .events
            .into_iter()
            .filter_map(|event| {
                match scheduler::event_in_session_scope_from(
                    connection,
                    network_id,
                    scope_id,
                    &job.scope,
                    &event.event,
                ) {
                    Ok(true) => Some(Ok(event)),
                    Ok(false) => None,
                    Err(error) => Some(Err(error)),
                }
            })
            .collect::<Result<_>>()?;
        let trigger = if let Some(assignment_id) = job.trigger.strip_prefix("assignment:") {
            self.workspace_assignment(network_id, assignment_id)?
                .map(|assignment| {
                    Ok::<HashedEvent, anyhow::Error>(HashedEvent {
                        hash: assignment.event_hash.clone(),
                        event: log::event_from(connection, network_id, &assignment.event_hash)?
                            .context("assignment event missing")?,
                    })
                })
                .transpose()?
        } else if let Some(message_id) = job.trigger.strip_prefix("message:") {
            message_from(connection, network_id, scope_id, message_id)?
                .map(|message| {
                    Ok::<HashedEvent, anyhow::Error>(HashedEvent {
                        hash: message.event_hash.clone(),
                        event: log::event_from(connection, network_id, &message.event_hash)?
                            .context("input event missing")?,
                    })
                })
                .transpose()?
        } else {
            match crate::scheduler::trigger_event_key(&job.trigger) {
                Some((kind, subject)) => event_about_from(
                    connection, network_id, scope_id, kind, subject,
                )?
                .filter(|event| {
                    !matches!(
                        event.event.body,
                        EventBody::MessagePosted {
                            role: Role::Assistant,
                            ..
                        }
                    )
                }),
                None => None,
            }
        };
        let question = match trigger.as_ref().map(|event| &event.event.body) {
            Some(EventBody::MessagePosted {
                answers: Some(question),
                ..
            }) => event_about_from(connection, network_id, scope_id, "question_asked", question)?
                .map(|event| event.event.body),
            _ => None,
        };
        let task_context =
            if let crate::scheduling::SessionScope::WorkspaceTask { task_id } = &job.scope {
                let task = self
                    .workspace_task(network_id, task_id)?
                    .context("task not found")?;
                let assignment = if let Some(id) = job.trigger.strip_prefix("assignment:") {
                    self.workspace_assignment(network_id, id)?
                } else {
                    task.current_assignment
                        .as_ref()
                        .map(|id| self.workspace_assignment(network_id, id))
                        .transpose()?
                        .flatten()
                };
                Some(serde_json::json!({"task":task,"assignment":assignment}))
            } else {
                None
            };
        Ok(crate::scheduler::TurnHistory {
            task_context,
            effective_input: self.queued_workspace_input(network_id, &job.reply_to)?,
            recent,
            since,
            checkpoint,
            trigger,
            question,
            resumed_by: resumed.map(|(_, node)| node.to_owned()),
            pending_blobs: Default::default(),
        })
    }

    /// The effective event at `author`/`seq` in this conversation.
    pub(crate) fn scope_event(
        &self,
        network_id: &str,
        scope_id: &str,
        author: &str,
        seq: u64,
    ) -> Result<Option<HashedEvent>> {
        self.connection
            .query_row(
                "SELECT hash,envelope FROM events WHERE network_id=? AND scope_id=? AND author=? AND seq=? AND effective=1 LIMIT 1",
                params![network_id, scope_id, author, seq],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(hash, envelope)| {
                let envelope: SignedEvent = serde_json::from_str(&envelope)?;
                Ok(HashedEvent {
                    hash,
                    event: serde_json::from_str(&envelope.canonical_body)?,
                })
            })
            .transpose()
    }

    /// Up to `limit` effective events of the conversation within `basis`, of `kinds` when any are
    /// given, next to the event `at` names by author and seq: before it newest first, or after it
    /// oldest first. Without `at`, the newest. The flag says more follow in that direction.
    #[cfg(test)]
    pub(crate) fn history_page(
        &self,
        network_id: &str,
        scope_id: &str,
        basis: &VersionVector,
        at: Option<(&str, u64, bool)>,
        kinds: &[String],
        limit: usize,
    ) -> Result<(Vec<HashedEvent>, bool)> {
        self.search_history_page(
            network_id,
            scope_id,
            basis,
            at,
            HistoryFilter {
                kinds,
                author: None,
                keyword: None,
            },
            limit,
        )
    }

    pub(crate) fn search_history_page(
        &self,
        network_id: &str,
        scope_id: &str,
        basis: &VersionVector,
        at: Option<(&str, u64, bool)>,
        filter: HistoryFilter<'_>,
        limit: usize,
    ) -> Result<(Vec<HashedEvent>, bool)> {
        use rusqlite::types::Value as Sql;
        ensure!((1..=100).contains(&limit), "history limit must be 1-100");
        ensure!(
            filter
                .keyword
                .is_none_or(|keyword| !keyword.is_empty() && keyword.len() <= 256),
            "keyword must contain 1-256 bytes"
        );
        let mut sql = String::from(
            "SELECT author,seq,hash,envelope FROM events WHERE network_id=? AND scope_id=? AND effective=1
             AND seq<=COALESCE((SELECT value FROM json_each(?) WHERE key=events.author),0)",
        );
        let mut values = vec![
            Sql::from(network_id.to_owned()),
            Sql::from(scope_id.to_owned()),
            Sql::from(serde_json::to_string(basis)?),
        ];
        let after = at.is_some_and(|(_, _, after)| after);
        if let Some((author, seq, _)) = at {
            let (lamport, created, hash): (i64, i64, String) = self
                .connection
                .query_row(
                    "SELECT lamport,created_at_ms,hash FROM events WHERE network_id=? AND scope_id=? AND author=? AND seq=? AND effective=1 LIMIT 1",
                    params![network_id, scope_id, author, seq],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?
                .context("no event with this author and seq in this conversation")?;
            sql.push_str(if after {
                " AND (lamport,created_at_ms,hash)>(?,?,?)"
            } else {
                " AND (lamport,created_at_ms,hash)<(?,?,?)"
            });
            values.extend([Sql::from(lamport), Sql::from(created), Sql::from(hash)]);
        }
        if !filter.kinds.is_empty() {
            sql.push_str(&format!(
                " AND kind IN ({})",
                vec!["?"; filter.kinds.len()].join(",")
            ));
            values.extend(filter.kinds.iter().cloned().map(Sql::from));
        }
        if let Some(author) = filter.author {
            sql.push_str(" AND author=?");
            values.push(Sql::from(author.to_owned()));
        }
        if let Some(keyword) = filter.keyword {
            let body = "json_extract(json_extract(events.envelope,'$.canonical_body'),'$.body')";
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM json_tree(CASE events.kind
                    WHEN 'bot_created' THEN json_object('type','bot_configuration',
                        'name',json_extract({body},'$.name'),
                        'instructions',json_extract({body},'$.instructions'),
                        'project',json_extract({body},'$.project'))
                    ELSE json_remove({body},'$.attestation') END) AS visible
                    WHERE visible.type='text' AND instr(lower(visible.atom),lower(?))>0)"
            ));
            values.push(Sql::from(keyword.to_owned()));
        }
        sql.push_str(if after {
            " ORDER BY lamport,created_at_ms,hash"
        } else {
            " ORDER BY lamport DESC,created_at_ms DESC,hash DESC"
        });
        sql.push_str(" LIMIT ?");
        values.push(Sql::from((limit + 1) as i64));
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(values))?;
        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            if events.len() == limit {
                return Ok((events, true));
            }
            let envelope: SignedEvent = serde_json::from_str(&row.get::<_, String>(3)?)?;
            events.push(HashedEvent {
                hash: row.get(2)?,
                event: serde_json::from_str(&envelope.canonical_body)?,
            });
        }
        Ok((events, false))
    }

    fn blob_path(&self, hash: &str) -> Result<PathBuf> {
        ensure!(
            hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid blob hash"
        );
        Ok(self.blob_dir.join("sha256").join(&hash[..2]).join(hash))
    }

    pub fn has_blob(&self, blob: &BlobRef) -> Result<bool> {
        Ok(log::blob_file_matches(&self.blob_path(&blob.hash)?, blob))
    }

    pub(crate) fn verified_blob_path(
        &self,
        network: &str,
        scope: &str,
        attachment: &Attachment,
    ) -> Result<(PathBuf, Vec<u8>)> {
        let (blob, bytes) = self.read_blob(network, scope, &attachment.hash)?;
        ensure!(blob == attachment.blob(), "attachment metadata mismatch");
        Ok((self.blob_path(&attachment.hash)?.canonicalize()?, bytes))
    }

    pub(crate) fn verified_input_attachment(
        &self,
        network: &str,
        conversation: &str,
        message_id: &str,
        attachment: &Attachment,
    ) -> Result<(PathBuf, Vec<u8>)> {
        if let Some(input) = self.queued_workspace_input(network, message_id)? {
            ensure!(
                input.conversation_id == conversation
                    && input.input.attachments.contains(attachment),
                "attachment is outside the signed input"
            );
            if self
                .referenced_size(network, conversation, &attachment.hash)
                .is_err()
            {
                return self.verified_blob_path(
                    network,
                    crate::workspace::WORKSPACE_SCOPE,
                    attachment,
                );
            }
        }
        self.verified_blob_path(network, conversation, attachment)
    }

    /// Test fixture that stores a whole attachment at once; production writes go through
    /// `partial_blob_file` and `blob_file`.
    #[cfg(test)]
    pub fn put_blob(&mut self, expected: &BlobRef, content: &[u8]) -> Result<()> {
        ensure!(
            content.len() <= MAX_BLOB_BYTES && content.len() as u64 == expected.size,
            "blob size mismatch or limit exceeded"
        );
        ensure!(digest(content) == expected.hash, "blob hash mismatch");
        let path = self.blob_path(&expected.hash)?;
        if log::blob_file_matches(&path, expected) {
            return Ok(());
        }
        let parent = path.parent().context("blob path")?;
        std::fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".{}.{}", expected.hash, uuid::Uuid::new_v4()));
        privatefs::create_durable(&temporary, content)?;
        privatefs::durable_rename(&temporary, &path)?;
        Ok(())
    }

    /// Reads an attachment referenced by a stored event of this conversation.
    pub(crate) fn referenced_size(&self, network: &str, scope: &str, hash: &str) -> Result<u64> {
        self.connection
            .query_row(
                "SELECT b.size FROM event_blobs b JOIN events e ON e.network_id=b.network_id AND e.hash=b.hash WHERE b.network_id=? AND b.blob=? AND e.scope_id=? LIMIT 1",
                params![network, hash, scope],
                |r| r.get(0),
            )
            .optional()?
            .context("attachment is not referenced by this conversation")
    }

    /// Up to `len` bytes of an attachment from `offset`, and its size. The file was verified
    /// when stored; the reader checks the assembled download against the hash.
    pub fn read_blob_range(
        &self,
        network: &str,
        scope: &str,
        hash: &str,
        offset: u64,
        len: usize,
    ) -> Result<(u64, Vec<u8>)> {
        use std::io::{Read, Seek};
        let size = self.referenced_size(network, scope, hash)?;
        let mut file = std::fs::File::open(self.blob_path(hash)?)
            .context("attachment content is not on this device")?;
        ensure!(
            file.metadata()?.len() == size,
            "stored attachment failed content verification"
        );
        let start = offset.min(size);
        let mut bytes = vec![0; len.min((size - start) as usize)];
        file.seek(std::io::SeekFrom::Start(start))?;
        file.read_exact(&mut bytes)?;
        Ok((size, bytes))
    }

    pub fn read_blob(&self, network: &str, scope: &str, hash: &str) -> Result<(BlobRef, Vec<u8>)> {
        let size = self.referenced_size(network, scope, hash)?;
        let bytes = std::fs::read(self.blob_path(hash)?)
            .context("attachment content is not on this device")?;
        ensure!(
            bytes.len() as u64 == size && digest(&bytes) == hash,
            "stored attachment failed content verification"
        );
        Ok((
            BlobRef {
                hash: hash.into(),
                size,
            },
            bytes,
        ))
    }

    pub(crate) fn attach_process_lock(&mut self, lock: Arc<std::fs::File>) {
        self.process_lock = Some(lock);
    }

    pub fn conversations(&self, network_id: &str) -> Result<Vec<Conversation>> {
        conversations_from(&self.connection, network_id)
    }
    pub fn conversation(&self, network_id: &str, scope_id: &str) -> Result<Option<Conversation>> {
        conversation_from(&self.connection, network_id, scope_id)
    }
    pub(crate) fn has_conversation(&self, network_id: &str, scope_id: &str) -> Result<bool> {
        Ok(self.conversation(network_id, scope_id)?.is_some())
    }
    pub fn messages(&self, network_id: &str, scope_id: &str) -> Result<Vec<Message>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE network_id=? AND conversation_id=? ORDER BY rank"
        ))?;
        statement
            .query_map(params![network_id, scope_id], message_row)?
            .map(|row| row?)
            .collect()
    }
    pub fn main_messages(&self, network_id: &str, scope_id: &str) -> Result<Vec<Message>> {
        let mut messages = Vec::new();
        for message in self.messages(network_id, scope_id)? {
            if scope_projection::message_is_main_from(
                &self.connection,
                network_id,
                scope_id,
                &message.message_id,
            )? {
                messages.push(message);
            }
        }
        Ok(messages)
    }
    pub fn message(&self, network: &str, scope: &str, message_id: &str) -> Result<Option<Message>> {
        message_from(&self.connection, network, scope, message_id)
    }
    pub fn works(&self, network: &str, scope: &str) -> Result<Vec<WorkProjection>> {
        reduce::works_from(&self.connection, network, scope)
    }
    pub fn work(&self, network: &str, work_id: &str) -> Result<Option<WorkProjection>> {
        reduce::work_from(&self.connection, network, work_id)
    }
    /// How many works in `scope` ended with a result nobody has to settle.
    pub fn finished_works(&self, network: &str, scope: &str) -> Result<usize> {
        Ok(self.connection.query_row(
            "SELECT count(*) FROM works WHERE network_id=? AND conversation_id=? AND dispatch IS NOT NULL
             AND finish IS NOT NULL AND json_extract(finish,'$.outcome')<>'unverified'",
            params![network, scope],
            |r| r.get(0),
        )?)
    }
    /// Works in `scope`, or every conversation, that want the user: see
    /// [`reduce::open_works_from`].
    pub fn open_works(&self, network: &str, scope: Option<&str>) -> Result<Vec<WorkProjection>> {
        reduce::open_works_from(&self.connection, network, scope)
    }
    /// This database file as the filesystem knows it: inode and creation time. Copying the data
    /// directory or restoring it from a backup writes a new file, so the values change.
    fn file_identity(&self) -> Option<String> {
        let path = self.connection.path().filter(|path| !path.is_empty())?;
        let born = std::fs::metadata(path)
            .ok()?
            .created()
            .ok()
            .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |at| at.as_nanos());
        Some(format!(
            "{}:{born}",
            privatefs::file_identity(Path::new(path)).ok()?
        ))
    }
    /// Whether this Node last wrote from this very file, so it holds everything it wrote.
    pub fn wrote_from_this_file(&self) -> Result<bool> {
        let Some(identity) = self.file_identity() else {
            return Ok(false);
        };
        let recorded: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM identity_meta WHERE key='write-file'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(recorded == Some(identity))
    }
    /// Notes that this Node writes from this file; a later start from the same file needs no
    /// alignment.
    pub fn record_write_file(&self) -> Result<()> {
        if let Some(identity) = self.file_identity() {
            self.connection.execute(
                "INSERT INTO identity_meta(key,value) VALUES('write-file',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [identity],
            )?;
        }
        Ok(())
    }
    /// Every local append checks this gate; it lives only as long as this connection.
    pub fn set_write_state(&mut self, state: WriteState) -> Result<()> {
        self.connection
            .execute("UPDATE temp.write_gate SET state=?", [state.as_str()])?;
        Ok(())
    }
    pub fn pending_notifications(
        &self,
        network_id: &str,
        client_id: &str,
    ) -> Result<Vec<Notification>> {
        let mut statement = self.connection.prepare("SELECT n.notification_id,n.conversation_id,n.kind,n.text FROM notifications n WHERE n.network_id=? AND NOT EXISTS(SELECT 1 FROM notification_acks a WHERE a.network_id=n.network_id AND a.notification_id=n.notification_id AND a.client_id=?) ORDER BY n.rank")?;
        let rows = statement
            .query_map(params![network_id, client_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(notification_id, conversation_id, kind, text)| {
                Ok(Notification {
                    target: scope_projection::notification_target_from(
                        &self.connection,
                        network_id,
                        &notification_id,
                    )?,
                    notification_id,
                    conversation_id,
                    kind,
                    text,
                })
            })
            .collect()
    }
    /// A Node belongs to at most one network.
    pub fn network(&self) -> Result<Option<NetworkId>> {
        network_from(&self.connection)
    }
    /// A member Node's last `capabilities` answer, kept only here to show while it is offline.
    pub fn peer_capabilities(&self, peer: &str) -> Result<Option<Value>> {
        let stored: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM identity_meta WHERE key=?",
                [format!("capabilities:{peer}")],
                |r| r.get(0),
            )
            .optional()?;
        Ok(stored
            .map(|value| serde_json::from_str(&value))
            .transpose()?)
    }
    pub fn keep_peer_capabilities(&self, peer: &str, capabilities: &Value) -> Result<()> {
        self.connection.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [format!("capabilities:{peer}"), capabilities.to_string()],
        )?;
        Ok(())
    }
    /// A host-wide counter, so request IDs of a restarted App Server never repeat.
    pub fn next_codex_epoch(&mut self) -> Result<u64> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: u64 = tx
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM identity_meta WHERE key='codex_epoch'",
                [],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let next = current + 1;
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES('codex_epoch',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [next.to_string()],
        )?;
        tx.commit()?;
        Ok(next)
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the conversation and its signed first owner"
)]
pub(crate) fn create_conversation_from(
    connection: &Connection,
    blob_dir: &Path,
    identity: &LocalIdentity,
    network: &str,
    scope: &str,
    title: &str,
    owner: FirstOwner,
    created_at_ms: i64,
) -> Result<()> {
    authorize_node_from(connection, network, &owner.scheduler_node)?;
    log::append_from(
        connection,
        blob_dir,
        identity,
        network,
        scope,
        EventBody::ConversationCreated {
            title: title.into(),
        },
        vec![],
        created_at_ms,
    )?;
    log::append_from(
        connection,
        blob_dir,
        identity,
        network,
        scope,
        EventBody::ConversationOwner {
            scheduler_node: owner.scheduler_node,
            epoch: 1,
            attestation: Some(owner.attestation),
            cause: None,
        },
        vec![],
        created_at_ms,
    )?;
    Ok(())
}

fn conversation_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Conversation> {
    Ok(Conversation {
        conversation_id: r.get(0)?,
        title: r.get(1)?,
        owner: r.get(2)?,
        owner_epoch: r.get(3)?,
    })
}

pub(crate) fn conversations_from(
    connection: &Connection,
    network_id: &str,
) -> Result<Vec<Conversation>> {
    let mut statement = connection.prepare("SELECT conversation_id,title,owner,owner_epoch FROM conversations WHERE network_id=? AND title IS NOT NULL ORDER BY conversation_id")?;
    statement
        .query_map([network_id], conversation_row)?
        .map(|row| Ok(row?))
        .collect()
}

pub(crate) fn conversation_from(
    connection: &Connection,
    network: &str,
    scope: &str,
) -> Result<Option<Conversation>> {
    Ok(connection
        .query_row(
            "SELECT conversation_id,title,owner,owner_epoch FROM conversations WHERE network_id=? AND conversation_id=? AND title IS NOT NULL",
            params![network, scope],
            conversation_row,
        )
        .optional()?)
}

const MESSAGE_COLUMNS: &str = "message_id,role,text,event_hash,author,created_at_ms,in_reply_to,status,report_results,attachments,answers,rank,sender,failure,target";

fn message_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Message>> {
    let role: String = r.get(1)?;
    let status: Option<String> = r.get(7)?;
    let attachments: String = r.get(9)?;
    let message_id: String = r.get(0)?;
    let text: String = r.get(2)?;
    let event_hash: String = r.get(3)?;
    let author: String = r.get(4)?;
    let created_at_ms: i64 = r.get(5)?;
    let in_reply_to: Option<String> = r.get(6)?;
    let report_results: Option<bool> = r.get(8)?;
    let answers: Option<String> = r.get(10)?;
    let rank: String = r.get(11)?;
    let sender: Option<String> = r.get(12)?;
    let failure: Option<String> = r.get(13)?;
    let target: Option<String> = r.get(14)?;
    Ok((|| {
        Ok(Message {
            message_id,
            target: optional_json(target)?,
            role: serde_json::from_value(Value::String(role))?,
            text,
            event_hash,
            author,
            created_at_ms,
            in_reply_to,
            status: status
                .map(|status| serde_json::from_value(Value::String(status)))
                .transpose()?,
            failure: failure
                .map(|failure| serde_json::from_value(Value::String(failure)))
                .transpose()?,
            report_results,
            answers,
            sender: optional_json(sender)?,
            attachments: serde_json::from_str(&attachments)?,
            rank,
        })
    })())
}

pub(crate) fn message_from(
    connection: &Connection,
    network: &str,
    scope: &str,
    message_id: &str,
) -> Result<Option<Message>> {
    connection
        .query_row(
            &format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE network_id=? AND conversation_id=? AND message_id=?"),
            params![network, scope, message_id],
            message_row,
        )
        .optional()?
        .transpose()
}

pub(crate) fn membership_from(
    connection: &Connection,
    network_id: &str,
) -> Result<AuthorizationView> {
    let encoded: String = connection
        .query_row(
            "SELECT bundle FROM networks WHERE network_id=?",
            [network_id],
            |r| r.get(0),
        )
        .optional()?
        .context("unknown network")?;
    let view = serde_json::from_str::<NetworkState>(&encoded)?.project()?;
    ensure!(
        view.network_id == network_id,
        "stored membership network mismatch"
    );
    Ok(view)
}

pub(crate) fn authorize_node_from(
    connection: &Connection,
    network_id: &str,
    device_id: &str,
) -> Result<Member> {
    membership_from(connection, network_id)?.node(device_id)
}

/// A unit enum's serde name, for a text column.
pub(crate) fn enum_text<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(text)) => text,
        _ => unreachable!("a unit enum serializes to a string"),
    }
}

pub(crate) fn enum_from<T: DeserializeOwned>(text: &str) -> Result<T> {
    Ok(serde_json::from_value(Value::String(text.into()))?)
}

fn optional_json<T: DeserializeOwned>(value: Option<String>) -> Result<Option<T>> {
    value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
}

/// The newest effective checkpoint of the conversation within `basis` (default: all), and the
/// vector it covers. Read through the kind index, so its cost does not grow with the history.
pub(crate) fn latest_checkpoint_from(
    connection: &Connection,
    network_id: &str,
    scope_id: &str,
    basis: Option<&VersionVector>,
) -> Result<Option<(VersionVector, EventBody)>> {
    let mut statement = connection.prepare(
        "SELECT author,seq,envelope FROM events WHERE network_id=? AND scope_id=? AND kind='context_checkpoint' AND effective=1 ORDER BY lamport DESC,created_at_ms DESC,hash DESC",
    )?;
    let mut rows = statement.query(params![network_id, scope_id])?;
    while let Some(row) = rows.next()? {
        let (author, seq): (String, u64) = (row.get(0)?, row.get(1)?);
        if basis.is_some_and(|basis| seq > basis.get(&author).copied().unwrap_or(0)) {
            continue;
        }
        let envelope: SignedEvent = serde_json::from_str(&row.get::<_, String>(2)?)?;
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        if let EventBody::ContextCheckpoint { covers, .. } = &event.body {
            return Ok(Some((covers.clone(), event.body)));
        }
    }
    Ok(None)
}

/// The newest checkpoint and the events it does not cover, in shared order. The next checkpoint
/// needs only these, so a long conversation does not reload its whole history every turn.
pub(crate) fn since_checkpoint_from(
    connection: &Connection,
    network_id: &str,
    scope_id: &str,
) -> Result<Context> {
    let covers = latest_checkpoint_from(connection, network_id, scope_id, None)?
        .map(|(covers, _)| covers)
        .unwrap_or_default();
    context_after(connection, network_id, scope_id, None, &covers)
}

/// The first effective event in shared order of `kind` about `subject`, wherever it sits in the
/// conversation's history.
pub(crate) fn event_about_from(
    connection: &Connection,
    network_id: &str,
    scope_id: &str,
    kind: &str,
    subject: &str,
) -> Result<Option<HashedEvent>> {
    let found: Option<(String, String)> = connection
        .query_row(
            "SELECT hash,envelope FROM events WHERE network_id=? AND scope_id=? AND kind=? AND subject=? AND effective=1 ORDER BY lamport,created_at_ms,hash LIMIT 1",
            params![network_id, scope_id, kind, subject],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    found
        .map(|(hash, envelope)| {
            let envelope: SignedEvent = serde_json::from_str(&envelope)?;
            Ok(HashedEvent {
                hash,
                event: serde_json::from_str(&envelope.canonical_body)?,
            })
        })
        .transpose()
}

pub(crate) fn context_after(
    connection: &Connection,
    network_id: &str,
    scope_id: &str,
    basis: Option<&VersionVector>,
    covers: &VersionVector,
) -> Result<Context> {
    // A former Node's counted branch can run past the main chain, so only an explicit basis cuts.
    let limit = basis;
    let basis = match basis {
        Some(basis) => basis.clone(),
        None => log::version_vector_from(connection, network_id)?,
    };
    let mut statement = connection.prepare(
        "SELECT author,seq,hash,envelope FROM events WHERE network_id=? AND scope_id=? AND effective=1
         AND seq>COALESCE((SELECT value FROM json_each(?) WHERE key=events.author),0)
         ORDER BY lamport,created_at_ms,hash",
    )?;
    let rows = statement.query_map(
        params![network_id, scope_id, serde_json::to_string(covers)?],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        },
    )?;
    let mut events = Vec::new();
    for row in rows {
        let (author, seq, hash, envelope) = row?;
        if let Some(limit) = &limit
            && seq > limit.get(&author).copied().unwrap_or(0)
        {
            continue;
        }
        let envelope: SignedEvent = serde_json::from_str(&envelope)?;
        events.push(HashedEvent {
            hash,
            event: serde_json::from_str(&envelope.canonical_body)?,
        });
    }
    Ok(Context {
        network_id: network_id.into(),
        scope_id: scope_id.into(),
        basis,
        events,
    })
}
