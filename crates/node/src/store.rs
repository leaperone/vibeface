//! One SQLite store per Node. Business events form one signed log per device;
//! projections are updated incrementally by the reducer as each event is stored.
use crate::{
    crypto::{LocalIdentity, digest},
    identity::{AuthorizationView, Member, NetworkState},
    model::*,
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) mod bots;
mod log;
mod reduce;
pub(crate) mod scheduler;
mod sync;
pub use sync::{MAX_PULL_BYTES, MAX_PULL_EVENTS};
#[cfg(test)]
mod tests;
pub(crate) mod work;

const INITIAL_SCHEMA: &str = include_str!("../migrations/202610180001_initial.sql");
pub const SCHEMA_VERSION: u32 = 7;

/// Whether this Node may append to its own log (design 5.7 and 5.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteState {
    /// Started with other member Nodes; waits for one to confirm it holds nothing newer of ours.
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

pub struct Store {
    pub(crate) connection: Connection,
    blob_dir: PathBuf,
    process_lock: Option<Arc<std::fs::File>>,
}

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
    let merged = if let Some(previous) = &previous {
        serde_json::from_str::<NetworkState>(previous)?.merge(state)?
    } else {
        state.clone()
    };
    let encoded = serde_json::to_string(&merged)?;
    if previous.as_ref() == Some(&encoded) {
        return Ok(false);
    }
    connection.execute("INSERT INTO networks(network_id,revision,bundle) VALUES(?,?,?) ON CONFLICT(network_id) DO UPDATE SET revision=excluded.revision,bundle=excluded.bundle",params![network_id,merged.epoch(),encoded])?;
    Ok(true)
}

/// Total order shared by prompts and UI: a Kahn sort of the DAG with ties broken by
/// `(created_at_ms, hash)`. Lamport clocks make it a plain string comparison.
pub(crate) fn rank(lamport: u64, created_at_ms: i64, hash: &str) -> String {
    format!("{lamport:020}:{created_at_ms:020}:{hash}")
}

fn private_file_connection(path: &Path) -> Result<Connection> {
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
                connection.execute_batch(&format!("BEGIN IMMEDIATE;{INITIAL_SCHEMA}COMMIT;"))?;
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
        for network in store.networks()? {
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
    /// The member record of a current, unrevoked Node.
    pub fn authorize_node(&self, network_id: &str, device_id: &str) -> Result<Member> {
        authorize_node_from(&self.connection, network_id, device_id)
    }
    /// A write signed by a device this network has admitted and not revoked.
    pub fn authorize_attestation(&self, network_id: &str, body: &EventBody) -> Result<()> {
        if let Some(attestation) = body.attestation() {
            membership_from(&self.connection, network_id)?.client(&attestation.device_id)?;
        }
        Ok(())
    }
    /// Stores each event or reports why not; one bad event never aborts the batch.
    /// Signatures are verified here, once; stored events are trusted afterwards.
    pub fn ingest_events(
        &mut self,
        network_id: &str,
        envelopes: &[SignedEvent],
    ) -> Result<Vec<IngestStatus>> {
        ensure!(envelopes.len() <= 1024, "event batch is too large");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = crate::runtime::now_ms();
        let view = membership_from(&tx, network_id)?;
        let statuses = envelopes
            .iter()
            .map(|envelope| log::ingest_from(&tx, &view, envelope, now))
            .collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(statuses)
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

    /// The conversation's events covered by `basis` (default: everything stored), in shared order.
    pub fn context(
        &self,
        network_id: &str,
        scope_id: &str,
        basis: Option<&VersionVector>,
    ) -> Result<Context> {
        context_from(&self.connection, network_id, scope_id, basis)
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
    fn referenced_size(&self, network: &str, scope: &str, hash: &str) -> Result<u64> {
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
    /// Attachments an event carries, in its signed order.
    pub fn event_blobs(&self, network: &str, event_hash: &str) -> Result<Vec<BlobRef>> {
        let mut statement = self.connection.prepare(
            "SELECT blob,size FROM event_blobs WHERE network_id=? AND hash=? ORDER BY blob",
        )?;
        statement
            .query_map(params![network, event_hash], |r| {
                Ok(BlobRef {
                    hash: r.get(0)?,
                    size: r.get(1)?,
                })
            })?
            .map(|row| Ok(row?))
            .collect()
    }
    pub fn message(&self, network: &str, scope: &str, message_id: &str) -> Result<Option<Message>> {
        message_from(&self.connection, network, scope, message_id)
    }
    pub fn works(&self, network: &str, scope: &str) -> Result<Vec<WorkProjection>> {
        reduce::works_from(&self.connection, network, scope)
    }
    pub fn autonomy(&self, network: &str, scope: &str) -> Result<Autonomy> {
        autonomy_from(&self.connection, network, scope)
    }
    /// Optimistic: a stale `expected_version` is refused, so two clients never silently override.
    #[expect(clippy::too_many_arguments, reason = "the decision and its signature")]
    pub fn set_autonomy(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        scope: &str,
        mode: AutonomyMode,
        expected_version: u64,
        attestation: Attestation,
        at: i64,
    ) -> Result<SignedEvent> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            conversation_from(&tx, network, scope)?.is_some(),
            "conversation not found"
        );
        ensure!(
            autonomy_from(&tx, network, scope)?.version == expected_version,
            "autonomy changed; refresh before changing it"
        );
        let envelope = log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            scope,
            EventBody::AutonomySet {
                mode,
                version: expected_version + 1,
                attestation: Some(attestation),
            },
            vec![],
            at,
        )?;
        tx.commit()?;
        Ok(envelope)
    }
    /// Every local append checks this gate; it lives only as long as this connection.
    pub fn set_write_state(&mut self, state: WriteState) -> Result<()> {
        self.connection
            .execute("UPDATE temp.write_gate SET state=?", [state.as_str()])?;
        Ok(())
    }
    /// Triage cards: open ones first, newest first, at most `limit`.
    pub fn alerts(&self, network_id: &str, limit: usize) -> Result<Vec<Value>> {
        let mut statement = self.connection.prepare(
            "SELECT a.raise,a.node,a.created_at_ms,r.resolution FROM alerts a LEFT JOIN alert_resolutions r ON r.network_id=a.network_id AND r.alert_id=a.alert_id WHERE a.network_id=? ORDER BY r.resolution IS NOT NULL, a.rank DESC LIMIT ?",
        )?;
        let rows = statement.query_map(params![network_id, limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (raise, node, created_at_ms, resolution) = row?;
            let mut alert: Value = serde_json::from_str(&raise)?;
            alert["node"] = node.into();
            alert["created_at_ms"] = created_at_ms.into();
            alert["resolution"] = resolution.map_or(Value::Null, Value::String);
            if let Some(object) = alert.as_object_mut() {
                object.remove("type");
            }
            Ok(alert)
        })
        .collect()
    }
    pub fn local_value(&self, network_id: &str, key: &str) -> Result<Option<Value>> {
        local_load(&self.connection, network_id, key)
    }
    pub fn local_set(&mut self, network_id: &str, key: &str, value: &Value) -> Result<()> {
        local_save(&self.connection, network_id, key, value)
    }
    pub fn local_delete(&mut self, network_id: &str, key: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM local_values WHERE network_id=? AND key=?",
            params![network_id, key],
        )?;
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
    pub fn budget_usage(&self, network_id: &str, day: &str, limit: u64) -> Result<BudgetUsage> {
        validate_day(day)?;
        let used = self
            .connection
            .query_row(
                "SELECT used FROM budget_days WHERE network_id=? AND day=?",
                params![network_id, day],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        Ok(BudgetUsage {
            day: day.into(),
            used,
            limit,
        })
    }
    pub fn reserve_budget(
        &mut self,
        network_id: &str,
        day: &str,
        units: u64,
        limit: u64,
    ) -> Result<BudgetUsage> {
        validate_day(day)?;
        ensure!(
            units > 0 && limit <= i64::MAX as u64,
            "invalid budget units/limit"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let used: u64 = tx
            .query_row(
                "SELECT used FROM budget_days WHERE network_id=? AND day=?",
                params![network_id, day],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let next = used.checked_add(units).context("budget overflow")?;
        ensure!(next <= limit, "daily model budget exhausted");
        tx.execute("INSERT INTO budget_days(network_id,day,used) VALUES(?,?,?) ON CONFLICT(network_id,day) DO UPDATE SET used=excluded.used",params![network_id,day,next])?;
        tx.commit()?;
        Ok(BudgetUsage {
            day: day.into(),
            used: next,
            limit,
        })
    }
    pub fn networks(&self) -> Result<Vec<NetworkId>> {
        let mut statement = self
            .connection
            .prepare("SELECT network_id FROM networks ORDER BY network_id")?;
        Ok(statement
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
    pub fn scopes(&self, network_id: &str) -> Result<Vec<ScopeId>> {
        let mut statement = self.connection.prepare(
            "SELECT conversation_id FROM conversations WHERE network_id=? ORDER BY conversation_id",
        )?;
        Ok(statement
            .query_map([network_id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
    /// A host-wide counter, so request IDs of a restarted executor never repeat.
    pub fn next_executor_epoch(&mut self) -> Result<u64> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: u64 = tx
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM identity_meta WHERE key='executor_epoch'",
                [],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let next = current + 1;
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES('executor_epoch',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
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

const MESSAGE_COLUMNS: &str =
    "message_id,role,text,event_hash,author,created_at_ms,in_reply_to,status,report_results,blobs";

fn message_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Message>> {
    let role: String = r.get(1)?;
    let status: Option<String> = r.get(7)?;
    let blobs: String = r.get(9)?;
    let message_id: String = r.get(0)?;
    let text: String = r.get(2)?;
    let event_hash: String = r.get(3)?;
    let author: String = r.get(4)?;
    let created_at_ms: i64 = r.get(5)?;
    let in_reply_to: Option<String> = r.get(6)?;
    let report_results: Option<bool> = r.get(8)?;
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
            report_results,
            blobs: serde_json::from_str(&blobs)?,
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

pub(crate) fn autonomy_from(
    connection: &Connection,
    network: &str,
    scope: &str,
) -> Result<Autonomy> {
    let row: Option<(String, u64)> = connection
        .query_row(
            "SELECT mode,version FROM autonomy WHERE network_id=? AND conversation_id=?",
            params![network, scope],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match row {
        Some((mode, version)) => Autonomy {
            mode: serde_json::from_value(Value::String(mode))?,
            version,
        },
        None => Autonomy {
            mode: AutonomyMode::Report,
            version: 0,
        },
    })
}

pub(crate) fn local_load<T: DeserializeOwned>(
    connection: &Connection,
    network: &str,
    key: &str,
) -> Result<Option<T>> {
    optional_json(
        connection
            .query_row(
                "SELECT value FROM local_values WHERE network_id=? AND key=?",
                params![network, key],
                |r| r.get(0),
            )
            .optional()?,
    )
}

pub(crate) fn local_save(
    connection: &Connection,
    network: &str,
    key: &str,
    value: &impl serde::Serialize,
) -> Result<()> {
    connection.execute("INSERT INTO local_values(network_id,key,value) VALUES(?,?,?) ON CONFLICT(network_id,key) DO UPDATE SET value=excluded.value", params![network,key,serde_json::to_string(value)?])?;
    Ok(())
}

/// Values whose key starts with `prefix`, through an index range rather than a table scan.
pub(crate) fn local_prefix<T: DeserializeOwned>(
    connection: &Connection,
    network: &str,
    prefix: &str,
) -> Result<Vec<T>> {
    let mut end = prefix.to_owned();
    end.push(char::MAX);
    let mut statement = connection.prepare(
        "SELECT value FROM local_values WHERE network_id=? AND key>=? AND key<? ORDER BY key",
    )?;
    let rows = statement.query_map(params![network, prefix, end], |r| r.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
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

fn optional_json<T: DeserializeOwned>(value: Option<String>) -> Result<Option<T>> {
    value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
}

fn validate_day(day: &str) -> Result<()> {
    ensure!(
        day.len() == 10
            && day.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }),
        "day must be YYYY-MM-DD"
    );
    Ok(())
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

/// The newest checkpoint and the events it does not cover, in shared order. The next checkpoint
/// needs only these, so a long conversation does not reload its whole history every turn.
pub(crate) fn since_checkpoint_from(
    connection: &Connection,
    network_id: &str,
    scope_id: &str,
) -> Result<Context> {
    let mut newest = connection.prepare(
        "SELECT envelope FROM events WHERE network_id=? AND scope_id=? ORDER BY lamport DESC,created_at_ms DESC,hash DESC",
    )?;
    let mut rows = newest.query(params![network_id, scope_id])?;
    let mut covers = VersionVector::new();
    while let Some(row) = rows.next()? {
        let envelope: SignedEvent = serde_json::from_str(&row.get::<_, String>(0)?)?;
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        if let EventBody::ContextCheckpoint { covers: found, .. } = event.body {
            covers = found;
            break;
        }
    }
    context_after(connection, network_id, scope_id, None, &covers)
}

fn context_after(
    connection: &Connection,
    network_id: &str,
    scope_id: &str,
    basis: Option<&VersionVector>,
    covers: &VersionVector,
) -> Result<Context> {
    let basis = match basis {
        Some(basis) => basis.clone(),
        None => log::version_vector_from(connection, network_id)?,
    };
    let mut statement = connection.prepare(
        "SELECT author,seq,hash,envelope FROM events WHERE network_id=? AND scope_id=?
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
        if seq > basis.get(&author).copied().unwrap_or(0) {
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
