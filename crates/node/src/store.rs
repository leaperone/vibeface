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

pub(crate) mod bot_settings;
pub(crate) mod bots;
mod log;
pub(crate) mod questions;
mod reduce;
pub(crate) mod scheduler;
mod sync;
pub(crate) mod triage;
pub use sync::{MAX_PULL_BYTES, MAX_PULL_EVENTS};
#[cfg(test)]
mod tests;
pub(crate) mod work;

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
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("database requires a private directory")?;
    if !parent.exists() {
        crate::config::private_dir(parent)?;
    }
    let metadata = std::fs::symlink_metadata(parent)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.permissions().mode() & 0o077 == 0,
        "database directory must be private and not a symlink"
    );
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "database must be a regular file"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        match std::fs::symlink_metadata(&name) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "database sidecar must be a regular file"
                );
                std::fs::set_permissions(&name, std::fs::Permissions::from_mode(0o600))?;
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
                let tables:i64 = connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",[],|r|r.get(0))?;
                ensure!(tables == 0, "unversioned database is not supported");
                connection.execute_batch(&format!(
                    "BEGIN IMMEDIATE;{INITIAL_SCHEMA}PRAGMA user_version = {SCHEMA_VERSION};COMMIT;"
                ))?;
            }
            SCHEMA_VERSION => {}
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
            metadata.is_file() && !metadata.file_type().is_symlink(),
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
    pub(crate) fn on_log_head_written(&self, written: impl Fn() + Send + 'static) {
        self.connection
            .update_hook(Some(move |_, _: &str, table: &str, _| {
                if table == "log_heads" {
                    written();
                }
            }));
    }
    /// The member record of a current, unrevoked Node.
    pub fn authorize_node(&self, network_id: &str, device_id: &str) -> Result<Member> {
        authorize_node_from(&self.connection, network_id, device_id)
    }
    /// Stores each event or reports why not; one bad event never aborts the batch.
    /// Signatures are verified here, once; stored events are trusted afterwards.
    pub fn ingest_events(
        &mut self,
        network_id: &str,
        envelopes: &[SignedEvent],
    ) -> Result<Ingested> {
        ensure!(envelopes.len() <= 1024, "event batch is too large");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let view = membership_from(&tx, network_id)?;
        let mut placed = log::Placed::default();
        let statuses = envelopes
            .iter()
            .map(|envelope| log::ingest_from(&tx, &view, envelope, &mut placed))
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

    /// The conversation's effective events covered by `basis` (default: all of them), in shared
    /// order.
    pub fn context(
        &self,
        network_id: &str,
        scope_id: &str,
        basis: Option<&VersionVector>,
    ) -> Result<Context> {
        context_from(&self.connection, network_id, scope_id, basis)
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
            None => match latest_checkpoint_from(connection, network_id, scope_id, basis)? {
                Some((covers, body)) => (Some(body), covers),
                None => (None, VersionVector::new()),
            },
        };
        let recent = context_after(connection, network_id, scope_id, basis, &since)?;
        let trigger = match crate::scheduler::trigger_event_key(&job.trigger) {
            Some((kind, subject)) => {
                event_about_from(connection, network_id, scope_id, kind, subject)?.filter(|event| {
                    !matches!(
                        event.event.body,
                        EventBody::MessagePosted {
                            role: Role::Assistant,
                            ..
                        }
                    )
                })
            }
            None => None,
        };
        let question = match trigger.as_ref().map(|event| &event.event.body) {
            Some(EventBody::MessagePosted {
                answers: Some(question),
                ..
            }) => event_about_from(connection, network_id, scope_id, "question_asked", question)?
                .map(|event| event.event.body),
            _ => None,
        };
        Ok(crate::scheduler::TurnHistory {
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
    pub(crate) fn history_page(
        &self,
        network_id: &str,
        scope_id: &str,
        basis: &VersionVector,
        at: Option<(&str, u64, bool)>,
        kinds: &[String],
        limit: usize,
    ) -> Result<(Vec<HashedEvent>, bool)> {
        use rusqlite::types::Value as Sql;
        let mut sql = String::from(
            "SELECT author,seq,hash,envelope FROM events WHERE network_id=? AND scope_id=? AND effective=1",
        );
        let mut values = vec![
            Sql::from(network_id.to_owned()),
            Sql::from(scope_id.to_owned()),
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
        if !kinds.is_empty() {
            sql.push_str(&format!(
                " AND kind IN ({})",
                vec!["?"; kinds.len()].join(",")
            ));
            values.extend(kinds.iter().cloned().map(Sql::from));
        }
        sql.push_str(if after {
            " ORDER BY lamport,created_at_ms,hash"
        } else {
            " ORDER BY lamport DESC,created_at_ms DESC,hash DESC"
        });
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(values))?;
        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            let (author, seq): (String, u64) = (row.get(0)?, row.get(1)?);
            if seq > basis.get(&author).copied().unwrap_or(0) {
                continue;
            }
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

    /// Attachments are files named by their SHA-256; writes are atomic and reads re-verify.
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
        {
            use std::{io::Write, os::unix::fs::OpenOptionsExt};
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(content)?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, &path)?;
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
    pub fn message(&self, network: &str, scope: &str, message_id: &str) -> Result<Option<Message>> {
        message_from(&self.connection, network, scope, message_id)
    }
    pub fn works(&self, network: &str, scope: &str) -> Result<Vec<WorkProjection>> {
        reduce::works_from(&self.connection, network, scope)
    }
    /// Works in every conversation that have no result yet or whose result waits for the user.
    pub fn open_works(&self, network: &str) -> Result<Vec<WorkProjection>> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT conversation_id FROM works WHERE network_id=? AND dispatch IS NOT NULL
             AND (finish IS NULL OR json_extract(finish,'$.outcome')='unverified')",
        )?;
        let scopes = statement
            .query_map([network], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut open = Vec::new();
        for scope in scopes {
            open.extend(
                self.works(network, &scope)?
                    .into_iter()
                    .filter(|work| work.outcome.is_none_or(|o| o == WorkOutcome::Unverified)),
            );
        }
        Ok(open)
    }
    /// This database file as the filesystem knows it: inode and creation time. Copying the data
    /// directory or restoring it from a backup writes a new file, so the values change.
    fn file_identity(&self) -> Option<String> {
        use std::os::unix::fs::MetadataExt as _;
        let metadata =
            std::fs::metadata(self.connection.path().filter(|path| !path.is_empty())?).ok()?;
        let born = metadata
            .created()
            .ok()
            .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |at| at.as_nanos());
        Some(format!("{}:{born}", metadata.ino()))
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
        statement
            .query_map(params![network_id, client_id], |r| {
                Ok(Notification {
                    notification_id: r.get(0)?,
                    conversation_id: r.get(1)?,
                    kind: r.get(2)?,
                    text: r.get(3)?,
                })
            })?
            .map(|row| Ok(row?))
            .collect()
    }
    /// Counts one more model run against today's UTC limit, or refuses it.
    pub fn reserve_budget(&mut self, network_id: &str, units: u64, limit: u64) -> Result<()> {
        ensure!(
            units > 0 && limit <= i64::MAX as u64,
            "invalid budget units/limit"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (day, used): (String, u64) = tx.query_row(
            "SELECT date('now'), COALESCE((SELECT used FROM budget_days WHERE network_id=?1 AND day=date('now')), 0)",
            [network_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let next = used.checked_add(units).context("budget overflow")?;
        ensure!(next <= limit, "daily model budget exhausted");
        tx.execute("INSERT INTO budget_days(network_id,day,used) VALUES(?,?,?) ON CONFLICT(network_id,day) DO UPDATE SET used=excluded.used",params![network_id,day,next])?;
        tx.commit()?;
        Ok(())
    }
    /// A Node belongs to at most one network.
    pub fn network(&self) -> Result<Option<NetworkId>> {
        Ok(self
            .connection
            .query_row("SELECT network_id FROM networks", [], |r| r.get(0))
            .optional()?)
    }
    pub fn scopes(&self, network_id: &str) -> Result<Vec<ScopeId>> {
        let mut statement = self.connection.prepare(
            "SELECT conversation_id FROM conversations WHERE network_id=? ORDER BY conversation_id",
        )?;
        Ok(statement
            .query_map([network_id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
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

const MESSAGE_COLUMNS: &str = "message_id,role,text,event_hash,author,created_at_ms,in_reply_to,status,report_results,attachments,answers,rank,sender,failure";

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
    Ok((|| {
        Ok(Message {
            message_id,
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

pub(crate) fn context_from(
    connection: &Connection,
    network_id: &str,
    scope_id: &str,
    basis: Option<&VersionVector>,
) -> Result<Context> {
    context_after(
        connection,
        network_id,
        scope_id,
        basis,
        &VersionVector::new(),
    )
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

fn context_after(
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
