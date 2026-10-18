use crate::{
    crypto::{LocalIdentity, digest},
    model::*,
};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

const INITIAL_SCHEMA: &str = include_str!("../migrations/202610180001_initial.sql");

pub struct Store {
    pub(crate) connection: Connection,
    pub(crate) policy: crate::identity::journal::PolicyState,
}
fn verify_schema(connection: &Connection) -> Result<()> {
    let metadata: (String, u32, u32, String, String) = connection.query_row(
        "SELECT format_id,data_epoch,layout_version,schema_fingerprint,initialization_state FROM schema_metadata WHERE singleton=1",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    ).context("database uses another format; existing data was preserved")?;
    ensure!(
        metadata
            == (
                crate::config::FORMAT_ID.into(),
                crate::config::DATA_EPOCH,
                crate::config::LAYOUT_VERSION,
                digest(INITIAL_SCHEMA.as_bytes()),
                "ready".into()
            ),
        "database uses another format; existing data was preserved"
    );
    let version: u32 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(
        version == 0,
        "unexpected SQLite schema version; existing data was preserved"
    );
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(INITIAL_SCHEMA)?;
    ensure!(
        schema_objects(connection)? == schema_objects(&expected)?,
        "database schema does not match this format; existing data was preserved"
    );
    Ok(())
}

fn schema_objects(connection: &Connection) -> Result<Vec<(String, String, String, String)>> {
    let mut statement = connection.prepare(
        "SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name",
    )?;
    Ok(statement
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?)
}

fn validate_persisted_state(connection: &Connection) -> Result<()> {
    let policy = crate::identity::journal::PolicyState::load(connection)?;
    let mut statement = connection.prepare("SELECT space_id,revision,bundle FROM spaces")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u64>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut seen = BTreeSet::new();
    for row in rows {
        let (space, revision, encoded) = row?;
        let id = vibeface_secure::authorization::SpaceId::parse(&space)?;
        let current = policy
            .verified()
            .space_history(&id)
            .and_then(|history| history.last())
            .context("stored space has no pinned policy")?;
        ensure!(
            serde_json::to_string(current)? == encoded && current.payload.sequence == revision,
            "stored space projection differs from verified policy"
        );
        seen.insert(id);
        load_events_from(connection, &space)?;
    }
    let expected: BTreeSet<_> = policy
        .pins()
        .iter()
        .filter_map(|pin| match &pin.domain {
            vibeface_secure::authorization::PolicyDomain::Space(space) => Some(space.clone()),
            _ => None,
        })
        .collect();
    ensure!(seen == expected, "stored space projection is incomplete");
    crate::identity::policy::validate_resource_projections(connection)?;
    crate::identity::admission::validate_persisted_authorizations(connection, &policy)?;
    crate::identity::enrollment_store::validate_enrollment_storage(connection, &policy)?;
    crate::identity::execution_handoff::validate_execution_handoff_storage(connection, &policy)?;
    Ok(())
}

fn private_sidecars(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let sidecar = PathBuf::from(name);
        if let Ok(meta) = fs::symlink_metadata(&sidecar) {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "database sidecar must be a regular file"
            );
            fs::set_permissions(sidecar, fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenPurpose {
    Ready,
    Initialize,
}

struct SnapshotDirectory(PathBuf);

impl Drop for SnapshotDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct DatabaseReadView {
    connection: Connection,
    _snapshot: Option<SnapshotDirectory>,
}

fn database_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn database_read_view(path: &Path) -> Result<DatabaseReadView> {
    let files: Vec<_> = std::iter::once(path.to_path_buf())
        .chain(["-wal", "-journal"].map(|suffix| database_sidecar(path, suffix)))
        .map(|file| match fs::symlink_metadata(&file) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "database snapshot source must be a regular file"
                );
                Ok((file, Some(metadata)))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((file, None)),
            Err(error) => Err(error.into()),
        })
        .collect::<Result<_>>()?;
    let has_recovery = files
        .iter()
        .skip(1)
        .any(|(_, metadata)| metadata.as_ref().is_some_and(|metadata| metadata.len() > 0));
    if !has_recovery {
        let mut uri = reqwest::Url::from_file_path(path.canonicalize()?)
            .map_err(|_| anyhow::anyhow!("invalid database path"))?;
        uri.query_pairs_mut().append_pair("immutable", "1");
        return Ok(DatabaseReadView {
            connection: Connection::open_with_flags(
                uri.as_str(),
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?,
            _snapshot: None,
        });
    }
    let snapshot_path =
        std::env::temp_dir().join(format!("vibeface-db-check-{}", uuid::Uuid::new_v4()));
    fs::DirBuilder::new().mode(0o700).create(&snapshot_path)?;
    let snapshot = SnapshotDirectory(snapshot_path);
    let destination = snapshot.0.join("node.sqlite3");
    for (index, (source, metadata)) in files.iter().enumerate() {
        if metadata.is_some() {
            let target = match index {
                0 => destination.clone(),
                1 => database_sidecar(&destination, "-wal"),
                _ => database_sidecar(&destination, "-journal"),
            };
            fs::copy(source, &target).context("copy database for read-only format verification")?;
            fs::set_permissions(target, fs::Permissions::from_mode(0o600))?;
        }
    }
    for (source, previous) in files {
        let current = match fs::symlink_metadata(source) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let unchanged = match (previous, current) {
            (None, None) => true,
            (Some(before), Some(after)) => {
                !after.file_type().is_symlink()
                    && (
                        before.dev(),
                        before.ino(),
                        before.len(),
                        before.mtime(),
                        before.mtime_nsec(),
                    ) == (
                        after.dev(),
                        after.ino(),
                        after.len(),
                        after.mtime(),
                        after.mtime_nsec(),
                    )
            }
            _ => false,
        };
        ensure!(
            unchanged,
            "database changed during read-only format verification"
        );
    }
    Ok(DatabaseReadView {
        connection: Connection::open_with_flags(
            destination.canonicalize()?,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?,
        _snapshot: Some(snapshot),
    })
}

fn prepare_database(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("database requires a private directory")?;
    if !parent.exists() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    crate::config::verify_private_dir(parent)?;
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "database must be a regular file"
            );
            for suffix in ["-wal", "-shm", "-journal"] {
                let mut name = path.as_os_str().to_owned();
                name.push(suffix);
                if let Ok(meta) = fs::symlink_metadata(PathBuf::from(name)) {
                    ensure!(
                        meta.is_file() && !meta.file_type().is_symlink(),
                        "database sidecar must be a regular file"
                    );
                }
            }
            if meta.len() > 0 {
                let view = database_read_view(path)?;
                let connection = &view.connection;
                let tables: i64 = connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |r| r.get(0))?;
                if tables > 0 {
                    verify_schema(connection)?;
                } else {
                    let version: u32 =
                        connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
                    ensure!(
                        version == 0,
                        "database uses another format; existing data was preserved"
                    );
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct StoredEvent {
    pub(crate) envelope: SignedEvent,
    pub(crate) event: Event,
    pub(crate) status: String,
}

impl Store {
    pub fn verify_existing_database(path: &Path) -> Result<()> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!("data format database is missing; existing data was preserved")
            }
            Err(error) => return Err(error.into()),
        };
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink(),
            "database must be a regular file"
        );
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            if let Ok(meta) = fs::symlink_metadata(PathBuf::from(name)) {
                ensure!(
                    meta.is_file() && !meta.file_type().is_symlink(),
                    "database sidecar must be a regular file"
                );
            }
        }
        ensure!(
            meta.len() > 0,
            "data format database is empty; existing data was preserved"
        );
        let view = database_read_view(path)?;
        verify_schema(&view.connection)
    }
    pub fn open_read_only(path: &Path) -> Result<Self> {
        Self::verify_existing_database(path)?;
        let connection = Connection::open_with_flags(
            path.canonicalize()?,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        verify_schema(&connection)?;
        validate_persisted_state(&connection)?;
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            if let Ok(metadata) = fs::symlink_metadata(PathBuf::from(name)) {
                ensure!(
                    metadata.is_file()
                        && !metadata.file_type().is_symlink()
                        && metadata.permissions().mode() & 0o077 == 0,
                    "database sidecar must remain private"
                );
            }
        }
        let policy = crate::identity::journal::PolicyState::load(&connection)?;
        Ok(Self { connection, policy })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_for(path.as_ref(), OpenPurpose::Ready)
    }

    pub fn initialize(path: &Path) -> Result<Self> {
        Self::open_for(path, OpenPurpose::Initialize)
    }

    fn open_for(path: &Path, purpose: OpenPurpose) -> Result<Self> {
        let memory = path == Path::new(":memory:");
        if !memory {
            prepare_database(path)?;
        }
        let canonical = if memory {
            None
        } else {
            Some(path.canonicalize()?)
        };
        let path = canonical.as_deref().unwrap_or(path);
        let connection = if memory {
            Connection::open_in_memory()?
        } else {
            Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?
        };
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let tables: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        if tables == 0 {
            let tx =
                rusqlite::Transaction::new_unchecked(&connection, TransactionBehavior::Immediate)?;
            tx.execute_batch(INITIAL_SCHEMA)?;
            tx.execute(
                "INSERT INTO schema_metadata(singleton,format_id,data_epoch,layout_version,schema_fingerprint,initialization_state) VALUES(1,?,?,?,?,'ready')",
                params![crate::config::FORMAT_ID, crate::config::DATA_EPOCH, crate::config::LAYOUT_VERSION, digest(INITIAL_SCHEMA.as_bytes())],
            )?;
            tx.commit()?;
        }
        verify_schema(&connection)?;
        validate_persisted_state(&connection)?;
        if !memory {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            private_sidecars(path)?;
        }
        let policy = crate::identity::journal::PolicyState::load(&connection)?;
        let mut store = Self { connection, policy };
        store
            .connection
            .pragma_update(None, "journal_mode", "WAL")?;
        if !memory {
            private_sidecars(path)?;
        }
        store.recover_running()?;
        for network in store.networks()? {
            store.rebuild_projections(&network)?;
        }
        Ok(store)
    }
    pub fn authorize_node(
        &self,
        network_id: &str,
        node_id: &str,
        role: Option<NodeRole>,
    ) -> Result<NodeCertificate> {
        authorize_node_from(&self.connection, network_id, node_id, role)
    }
    pub fn save_local_device(&mut self, network_id: &str, identity: &LocalIdentity) -> Result<()> {
        let node_id = identity.node_id()?;
        let certificate = self.authorize_node(network_id, &node_id, None)?;
        self.connection.execute(
            "INSERT INTO local_devices(space_id,device_id,public_key) VALUES(?,?,?) ON CONFLICT(space_id,device_id) DO UPDATE SET public_key=excluded.public_key",
            params![network_id, node_id, certificate.public_key],
        )?;
        Ok(())
    }
    pub fn is_local_device(&self, network_id: &str, node_id: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM local_devices WHERE space_id=? AND device_id=?",
            params![network_id, node_id],
            |r| r.get::<_, i64>(0),
        )? == 1)
    }
    fn public_key(&self, network_id: &str, author: &str) -> Result<String> {
        self.connection
            .query_row(
                "SELECT public_key FROM node_certificates WHERE space_id=? AND node_id=?",
                params![network_id, author],
                |r| r.get(0),
            )
            .optional()?
            .context("unknown historical event author")
    }
    pub fn verify_event(&self, network_id: &str, envelope: &SignedEvent) -> Result<Event> {
        let event = crate::identity::event_authorization::verify_event_envelope(
            &self.policy,
            network_id,
            envelope,
        )?;
        ensure!(
            event.network_id == network_id,
            "event belongs to another space"
        );
        Ok(event)
    }
    pub fn ingest_event(
        &mut self,
        network_id: &str,
        envelope: &SignedEvent,
    ) -> Result<IngestOutcome> {
        let event = self.verify_event(network_id, envelope)?;
        let encoded = serde_json::to_string(envelope)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT envelope FROM events WHERE space_id=? AND hash=?",
                params![network_id, envelope.hash],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            ensure!(old == encoded, "conflicting envelope for event hash");
        } else {
            tx.execute("INSERT INTO events(space_id,resource_id,hash,scope_id,author,sequence,envelope,status) VALUES(?,?,?,?,?,?,?,'pending')",params![network_id,event.resource_id.as_str(),envelope.hash,event.scope_id,event.author,event.sequence,encoded])?;
        }
        rebuild_projections_from(&tx, network_id)?;
        tx.commit()?;
        let status: String = self.connection.query_row(
            "SELECT status FROM events WHERE space_id=? AND hash=?",
            params![network_id, envelope.hash],
            |r| r.get(0),
        )?;
        Ok(IngestOutcome {
            hash: envelope.hash.clone(),
            status,
            missing: self.missing_dependencies(network_id, std::slice::from_ref(&envelope.hash))?,
        })
    }
    fn load_events(&self, network_id: &str) -> Result<BTreeMap<String, StoredEvent>> {
        load_events_from(&self.connection, network_id)
    }
    pub fn event(&self, network_id: &str, hash: &str) -> Result<Option<SignedEvent>> {
        optional_json(
            self.connection
                .query_row(
                    "SELECT envelope FROM events WHERE space_id=? AND hash=?",
                    params![network_id, hash],
                    |r| r.get(0),
                )
                .optional()?,
        )
    }
    pub fn heads(&self, network_id: &str, scope_id: &str) -> Result<Vec<EventHash>> {
        Ok(heads_from_events(&self.load_events(network_id)?, scope_id))
    }
    pub fn missing_dependencies(
        &self,
        network_id: &str,
        heads: &[EventHash],
    ) -> Result<MissingDependencies> {
        let events = self.load_events(network_id)?;
        let mut pending = Vec::new();
        let mut visited = BTreeSet::new();
        let mut stack = heads.to_vec();
        let mut missing = MissingDependencies::default();
        while let Some(hash) = stack.pop() {
            if !visited.insert(hash.clone()) {
                continue;
            }
            let Some(entry) = events.get(&hash) else {
                missing.events.push(hash);
                continue;
            };
            if matches!(entry.status.as_str(), "conflict" | "invalid") {
                missing.conflicts.push(hash.clone());
            }
            if entry.status == "pending" {
                pending.push(hash.clone());
            }
            stack.extend(dependencies(&entry.event).cloned());
            for blob in &entry.event.blobs {
                if !self.has_blob(network_id, blob)? {
                    missing.blobs.push(blob.clone());
                }
            }
        }
        if missing.events.is_empty() && missing.blobs.is_empty() && missing.conflicts.is_empty() {
            missing.conflicts.extend(pending);
        }
        missing.events.sort();
        missing.events.dedup();
        missing.blobs.sort();
        missing.blobs.dedup();
        missing.conflicts.sort();
        missing.conflicts.dedup();
        Ok(missing)
    }
    pub fn context(
        &self,
        network_id: &str,
        scope_id: &str,
        basis_heads: &[EventHash],
    ) -> Result<Context> {
        let heads = if basis_heads.is_empty() {
            self.heads(network_id, scope_id)?
        } else {
            basis_heads.to_vec()
        };
        let missing = self.missing_dependencies(network_id, &heads)?;
        let events = self.load_events(network_id)?;
        let closure = closure_hashes(&events, &heads);
        for hash in &heads {
            if let Some(event) = events.get(hash) {
                ensure!(
                    event.event.scope_id == scope_id,
                    "basis head belongs to another scope"
                );
            }
        }
        let ordered = topological(&events, &closure);
        Ok(Context {
            network_id: network_id.into(),
            scope_id: scope_id.into(),
            heads,
            events: ordered
                .into_iter()
                .filter_map(|h| events.get(&h))
                .filter(|e| e.status == "ready")
                .map(|e| e.event.clone())
                .collect(),
            missing,
        })
    }
    pub fn export_snapshot(&self, network_id: &str, scope_id: &str) -> Result<Snapshot> {
        let events = self.load_events(network_id)?;
        let heads = self.heads(network_id, scope_id)?;
        let closure = closure_hashes(&events, &heads);
        Ok(Snapshot {
            version: WIRE_VERSION,
            network_id: network_id.into(),
            scope_id: scope_id.into(),
            heads,
            events: closure
                .iter()
                .filter_map(|h| events.get(h))
                .map(|e| e.envelope.clone())
                .collect(),
        })
    }
    /// Import a trusted local archive; network callers authenticate the supplying Node.
    pub fn import_snapshot(
        &mut self,
        network_id: &str,
        snapshot: &Snapshot,
    ) -> Result<MissingDependencies> {
        ensure!(
            snapshot.version == WIRE_VERSION && snapshot.network_id == network_id,
            "invalid snapshot network/version"
        );
        for event in &snapshot.events {
            self.verify_event(network_id, event)?;
        }
        for event in &snapshot.events {
            self.ingest_event(network_id, event)?;
        }
        for hash in &snapshot.heads {
            if let Some(event) = self.event(network_id, hash)? {
                let body: Event = serde_json::from_str(&event.canonical_body)?;
                ensure!(
                    body.scope_id == snapshot.scope_id,
                    "snapshot head scope mismatch"
                );
            }
        }
        self.missing_dependencies(network_id, &snapshot.heads)
    }
    pub fn put_blob(&mut self, network_id: &str, expected: &BlobRef, content: &[u8]) -> Result<()> {
        ensure!(
            content.len() <= MAX_BLOB_BYTES && content.len() as u64 == expected.size,
            "blob size mismatch or limit exceeded"
        );
        ensure!(digest(content) == expected.hash, "blob hash mismatch");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO blobs(space_id,hash,size,content) VALUES(?,?,?,?) ON CONFLICT(space_id,hash) DO UPDATE SET size=excluded.size,content=excluded.content",
            params![network_id, expected.hash, expected.size, content],
        )?;
        rebuild_projections_from(&tx, network_id)?;
        tx.commit()?;
        Ok(())
    }
    pub fn get_blob(&self, network_id: &str, hash: &str) -> Result<Option<Vec<u8>>> {
        let content: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT content FROM blobs WHERE space_id=? AND hash=?",
                params![network_id, hash],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(content) = &content {
            ensure!(
                content.len() <= MAX_BLOB_BYTES && digest(content) == hash,
                "stored blob failed content verification"
            );
        }
        Ok(content)
    }
    pub fn has_blob(&self, network_id: &str, blob: &BlobRef) -> Result<bool> {
        has_blob_from(&self.connection, network_id, blob)
    }
    pub fn rebuild_projections(&mut self, network_id: &str) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        rebuild_projections_from(&tx, network_id)?;
        tx.commit()?;
        Ok(())
    }
    pub fn conversations(&self, network_id: &str) -> Result<Vec<Conversation>> {
        let mut statement=self.connection.prepare("SELECT conversation_id,title FROM conversations WHERE space_id=? ORDER BY conversation_id")?;
        let rows = statement.query_map([network_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (conversation_id, title) = row?;
            let heads = self.heads(network_id, &conversation_id)?;
            result.push(Conversation {
                conversation_id,
                title,
                heads,
            });
        }
        Ok(result)
    }
    pub fn messages(&self, network_id: &str, scope_id: &str) -> Result<Vec<Message>> {
        let mut statement=self.connection.prepare("SELECT message_id,role,text,event_hash,created_at_ms FROM messages WHERE space_id=? AND conversation_id=? ORDER BY ordinal")?;
        Ok(statement
            .query_map(params![network_id, scope_id], |r| {
                Ok(Message {
                    message_id: r.get(0)?,
                    role: r.get(1)?,
                    text: r.get(2)?,
                    event_hash: r.get(3)?,
                    created_at_ms: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
    pub fn tasks(&self, network_id: &str, scope_id: &str) -> Result<Vec<Task>> {
        let mut statement=self.connection.prepare("SELECT task_id,conversation_id,title,goal,acceptance,status,detail,event_hash FROM tasks WHERE space_id=? AND conversation_id=? ORDER BY task_id")?;
        Ok(statement
            .query_map(params![network_id, scope_id], |r| {
                Ok(Task {
                    task_id: r.get(0)?,
                    conversation_id: r.get(1)?,
                    title: r.get(2)?,
                    goal: r.get(3)?,
                    acceptance: r.get(4)?,
                    status: r.get(5)?,
                    detail: r.get(6)?,
                    event_hash: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
    pub fn local_value(&self, network_id: &str, key: &str) -> Result<Option<Value>> {
        optional_json(
            self.connection
                .query_row(
                    "SELECT value FROM local_values WHERE space_id=? AND key=?",
                    params![network_id, key],
                    |r| r.get(0),
                )
                .optional()?,
        )
    }
    pub fn local_set(&mut self, network_id: &str, key: &str, value: &Value) -> Result<()> {
        self.connection.execute("INSERT INTO local_values(space_id,key,value) VALUES(?,?,?) ON CONFLICT(space_id,key) DO UPDATE SET value=excluded.value",params![network_id,key,serde_json::to_string(value)?])?;
        Ok(())
    }
    pub fn local_delete(&mut self, network_id: &str, key: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM local_values WHERE space_id=? AND key=?",
            params![network_id, key],
        )?;
        Ok(())
    }
    pub fn session(
        &self,
        network_id: &str,
        target_node: &str,
        session_id: &str,
    ) -> Result<Option<ExecutionSession>> {
        optional_json(self.connection.query_row("SELECT record FROM execution_sessions WHERE space_id=? AND target_node=? AND session_id=?",params![network_id,target_node,session_id],|r|r.get(0)).optional()?)
    }
    pub fn sessions(&self, network_id: &str) -> Result<Vec<ExecutionSession>> {
        let mut statement=self.connection.prepare("SELECT record FROM execution_sessions WHERE space_id=? ORDER BY target_node,session_id")?;
        let rows = statement.query_map([network_id], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub fn upsert_session(&mut self, network_id: &str, session: &ExecutionSession) -> Result<()> {
        ensure!(session.network_id == network_id, "session network mismatch");
        ensure!(
            session.revision <= i64::MAX as u64,
            "session revision overflow"
        );
        if let Some(old) = self.session(network_id, &session.target_node, &session.session_id)? {
            ensure!(
                session.revision >= old.revision,
                "session revision rollback"
            );
        }
        let changed=self.connection.execute("INSERT INTO execution_sessions(space_id,target_node,session_id,revision,record) VALUES(?,?,?,?,?) ON CONFLICT(space_id,target_node,session_id) DO UPDATE SET revision=excluded.revision,record=excluded.record WHERE excluded.revision>=execution_sessions.revision",params![network_id,session.target_node,session.session_id,session.revision,serde_json::to_string(session)?])?;
        ensure!(changed == 1, "session revision rollback");
        Ok(())
    }
    pub fn operation(
        &self,
        network_id: &str,
        operation_id: &str,
    ) -> Result<Option<OperationRecord>> {
        let row: Option<(String, String, String)> = self
            .connection
            .query_row(
                "SELECT request,state,result FROM operations WHERE space_id=? AND operation_id=?",
                params![network_id, operation_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(request, state, result)| {
            Ok(OperationRecord {
                request: serde_json::from_str(&request)?,
                state: serde_json::from_str(&state)?,
                result: serde_json::from_str(&result)?,
            })
        })
        .transpose()
    }
    pub fn operations(&self, network_id: &str) -> Result<Vec<OperationRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT operation_id FROM operations WHERE space_id=? ORDER BY operation_id",
        )?;
        let ids = statement
            .query_map([network_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| {
                self.operation(network_id, &id)?
                    .context("operation disappeared")
            })
            .collect()
    }
    pub fn task_creation(&self, network: &str, scope: &str, task_id: &str) -> Result<SignedEvent> {
        let context = self.context(network, scope, &[])?;
        ensure!(
            context.missing.is_complete(),
            "task history is incomplete or conflicting"
        );
        let matches = context.events.iter().filter(|event| {
            event.scope_id == scope && matches!(&event.body, EventBody::TaskCreated { task_id: id, .. } if id == task_id)
        }).collect::<Vec<_>>();
        ensure!(matches.len() == 1, "task creation is missing or ambiguous");
        let hash = digest(&serde_json::to_vec(matches[0])?);
        self.event(network, &hash)?
            .context("task creation envelope is missing")
    }

    pub(crate) fn verify_binding_creation(
        &self,
        proof: &crate::binding::SignedBinding,
    ) -> Result<()> {
        let binding = &proof.binding;
        let key = self.public_key(&binding.network_id, &binding.authority_node)?;
        crate::binding::verify_binding(proof, &key)?;
        self.verify_binding_task(binding, true)
    }

    pub(crate) fn verify_historical_binding(
        &self,
        proof: &crate::binding::SignedBinding,
    ) -> Result<()> {
        let binding = &proof.binding;
        let key = self.public_key(&binding.network_id, &binding.authority_node)?;
        crate::binding::verify_binding(proof, &key)?;
        self.verify_binding_task(binding, false)
    }

    pub(crate) fn verify_binding_task(
        &self,
        binding: &crate::binding::TaskBinding,
        require_current_executor: bool,
    ) -> Result<()> {
        crate::binding::validate_binding(binding)?;
        let envelope = self
            .event(&binding.network_id, &binding.task_ref)?
            .context("task creation is missing")?;
        let creation = self.verify_event(&binding.network_id, &envelope)?;
        ensure!(
            creation.author == binding.authority_node
                && creation.scope_id == binding.conversation_id,
            "binding authority does not match signed task creation"
        );
        ensure!(
            matches!(&creation.body, EventBody::TaskCreated { task_id, .. } if task_id == &binding.task_id),
            "binding references a different task"
        );
        ensure!(
            self.missing_dependencies(
                &binding.network_id,
                std::slice::from_ref(&binding.task_ref)
            )?
            .is_complete(),
            "binding task history is incomplete or conflicting"
        );
        if require_current_executor {
            self.authorize_node(
                &binding.network_id,
                &binding.target_node,
                Some(NodeRole::Executor),
            )?;
        }
        if let Some(seal) = &binding.previous_seal {
            let previous = binding
                .previous_binding_hash
                .as_ref()
                .context("missing previous binding")?;
            let value = self
                .local_value(
                    &binding.network_id,
                    &format!("task-binding-record:{previous}"),
                )?
                .context("previous binding is missing")?;
            let prior: crate::binding::TaskBinding =
                serde_json::from_value(value["binding"].clone())?;
            ensure!(
                crate::binding::binding_hash(&prior)? == *previous
                    && prior.task_ref == binding.task_ref
                    && prior.generation.checked_add(1) == Some(binding.generation),
                "binding history does not extend previous generation"
            );
            let key = self.public_key(&binding.network_id, &prior.target_node)?;
            crate::binding::verify_seal(seal, &key)?;
            ensure!(
                seal.seal.network_id == binding.network_id
                    && seal.seal.binding_hash == *previous
                    && seal.seal.target_node == prior.target_node,
                "seal does not close previous binding"
            );
        }
        Ok(())
    }

    pub fn task_binding(
        &self,
        network: &str,
        task_ref: &str,
    ) -> Result<Option<crate::binding::SignedBinding>> {
        let Some(value) = self.local_value(network, &format!("task-binding-current:{task_ref}"))?
        else {
            return Ok(None);
        };
        let hash = value.as_str().context("invalid binding pointer")?;
        let record = self
            .local_value(network, &format!("task-binding-record:{hash}"))?
            .context("missing binding record")?;
        if record["signature"].is_null() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(record)?))
    }

    pub fn bind_task_authorized(
        &mut self,
        identity: &LocalIdentity,
        auth: &vibeface_secure::authorization::VerifiedAuthorization,
        proof: &crate::identity::policy::FreshnessProof,
        transport: &vibeface_secure::authorization::TransportEvidence,
        source_body: &Value,
        now_ms: u64,
    ) -> Result<crate::binding::SignedBinding> {
        use vibeface_secure::authorization::Action;
        ensure!(
            auth.action() == Action::ExecutionOpen && source_body["method"] == "bind_execution",
            "initial binding requires an execution-open invocation"
        );
        let required = |field: &str| {
            source_body[field]
                .as_str()
                .with_context(|| format!("{field} is required"))
        };
        let space = auth.resource().space.as_str();
        let scope = required("conversation_id")?;
        let task_id = required("task_id")?;
        let creation = self.task_creation(space, scope, task_id)?;
        ensure!(
            creation.hash == required("task_ref")?,
            "binding refers to another task creation"
        );
        let task = self.verify_event(space, &creation)?;
        ensure!(
            task.resource_id == auth.resource().resource && task.author == identity.node_id()?,
            "task authority or resource mismatch"
        );
        let binding = crate::binding::TaskBinding {
            network_id: space.to_owned(),
            conversation_id: scope.to_owned(),
            task_ref: creation.hash,
            task_id: task_id.to_owned(),
            authority_node: task.author,
            generation: 1,
            target_node: required("target_node")?.to_owned(),
            session_id: required("session_id")?.to_owned(),
            open_spec: serde_json::json!({"type":"open","executor":required("executor")?,"cwd":required("cwd")?}),
            previous_binding_hash: None,
            previous_seal: None,
        };
        self.verify_binding_task(&binding, true)?;
        let facts = self.request_facts(source_body, auth.audience())?;
        let checked = self.policy.verified().verify_invocation(
            auth.signed_invocation(),
            &facts,
            transport,
            now_ms,
        )?;
        ensure!(
            checked.invocation_hash() == auth.invocation_hash(),
            "binding invocation changed"
        );
        self.policy
            .verified()
            .verify_freshness(auth, &proof.challenge, &proof.receipts, now_ms)?;
        let signed = crate::binding::sign_binding(identity, &binding)?;
        let hash = crate::binding::binding_hash(&binding)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_key = format!("task-binding-current:{}", binding.task_ref);
        let current: Option<String> = tx
            .query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![space, current_key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(current) = current {
            let current: String = serde_json::from_str(&current)?;
            ensure!(
                current == hash,
                "task is already bound; changing it requires execution-rebind authorization"
            );
            return Ok(signed);
        }
        let ready: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE space_id=? AND hash=? AND resource_id=? AND status='ready')",
            params![space,binding.task_ref,auth.resource().resource.as_str()], |row| row.get(0))?;
        ensure!(ready, "task creation changed before binding");
        crate::identity::admission::consume_authorization_in(
            &tx,
            &self.policy,
            auth,
            proof,
            transport,
            source_body,
            None,
            now_ms,
        )?;
        tx.execute(
            "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
            params![
                space,
                format!("task-binding-record:{hash}"),
                serde_json::to_string(&signed)?
            ],
        )?;
        tx.execute(
            "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
            params![space, current_key, serde_json::to_string(&hash)?],
        )?;
        tx.commit()?;
        Ok(signed)
    }

    fn install_verified_binding(&mut self, proof: &crate::binding::SignedBinding) -> Result<()> {
        let binding = &proof.binding;
        let hash = crate::binding::binding_hash(binding)?;
        let current_key = format!("task-binding-current:{}", binding.task_ref);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![binding.network_id, current_key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(current) = current {
            let current: String = serde_json::from_str(&current)?;
            if current != hash {
                let record: String = tx.query_row(
                    "SELECT value FROM local_values WHERE space_id=? AND key=?",
                    params![binding.network_id, format!("task-binding-record:{current}")],
                    |r| r.get(0),
                )?;
                let previous: crate::binding::TaskBinding = serde_json::from_value(
                    serde_json::from_str::<Value>(&record)?["binding"].clone(),
                )?;
                ensure!(
                    binding.generation != previous.generation,
                    "conflicting signed binding generation"
                );
                if binding.generation < previous.generation {
                    // Historical proof may be retained, but must never move the current fence backwards.
                    tx.execute(
                        "INSERT OR IGNORE INTO local_values(space_id,key,value) VALUES(?,?,?)",
                        params![
                            binding.network_id,
                            format!("task-binding-record:{hash}"),
                            serde_json::to_string(proof)?
                        ],
                    )?;
                    tx.commit()?;
                    return Ok(());
                }
                ensure!(
                    binding.previous_binding_hash.as_ref() == Some(&current),
                    "binding generation has missing predecessors"
                );
            }
        }
        tx.execute("INSERT INTO local_values(space_id,key,value) VALUES(?,?,?) ON CONFLICT(space_id,key) DO UPDATE SET value=excluded.value",params![binding.network_id,format!("task-binding-record:{hash}"),serde_json::to_string(proof)?])?;
        tx.execute("INSERT INTO local_values(space_id,key,value) VALUES(?,?,?) ON CONFLICT(space_id,key) DO UPDATE SET value=excluded.value",params![binding.network_id,current_key,serde_json::to_string(&hash)?])?;
        if let Some(seal) = &binding.previous_seal {
            tx.execute(
                "INSERT OR IGNORE INTO local_values(space_id,key,value) VALUES(?,?,?)",
                params![
                    binding.network_id,
                    format!("task-binding-seal:{}", seal.seal.binding_hash),
                    serde_json::to_string(seal)?
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn install_historical_binding(
        &mut self,
        proof: &crate::binding::SignedBinding,
    ) -> Result<()> {
        self.verify_historical_binding(proof)?;
        self.install_verified_binding(proof)
    }

    pub fn validate_operation_binding(&self, request: &OperationRequest) -> Result<()> {
        let proof = request
            .binding
            .as_ref()
            .context("execution requires a confirmed task binding")?;
        self.verify_binding_creation(proof)?;
        validate_operation_binding_from(&self.connection, request)
    }

    pub fn finish_operation(
        &mut self,
        network_id: &str,
        operation_id: &str,
        state: OperationState,
        result: Value,
    ) -> Result<OperationRecord> {
        ensure!(
            state.terminal() || state == OperationState::Unknown,
            "invalid operation completion state"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String, String)> = tx
            .query_row(
                "SELECT request,state,result FROM operations WHERE space_id=? AND operation_id=?",
                params![network_id, operation_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (request, old_state, old_result) = row.context("unknown operation")?;
        let request: OperationRequest = serde_json::from_str(&request)?;
        let old_state: OperationState = serde_json::from_str(&old_state)?;
        if old_state.terminal() {
            ensure!(
                old_state == state && serde_json::from_str::<Value>(&old_result)? == result,
                "terminal operation cannot be changed"
            );
            return Ok(OperationRecord {
                request,
                state,
                result,
            });
        }
        ensure!(
            matches!(old_state, OperationState::Running | OperationState::Unknown)
                || (old_state == OperationState::Accepted && state == OperationState::Rejected),
            "operation has not started"
        );
        tx.execute(
            "UPDATE operations SET state=?,result=? WHERE space_id=? AND operation_id=?",
            params![
                serde_json::to_string(&state)?,
                serde_json::to_string(&result)?,
                network_id,
                operation_id
            ],
        )?;
        if state.terminal() {
            tx.execute(
                "DELETE FROM resource_reservations WHERE space_id=? AND operation_id=?",
                params![network_id, operation_id],
            )?;
            let session:Option<String>=tx.query_row("SELECT record FROM execution_sessions WHERE space_id=? AND target_node=? AND session_id=?",params![network_id,request.target_node,request.session_id],|r|r.get(0)).optional()?;
            if let Some(session) = session {
                let mut session: ExecutionSession = serde_json::from_str(&session)?;
                session.revision = session.revision.max(request.expected_revision + 1);
                tx.execute("UPDATE execution_sessions SET revision=?,record=? WHERE space_id=? AND target_node=? AND session_id=?",params![session.revision,serde_json::to_string(&session)?,network_id,request.target_node,request.session_id])?;
            }
        }
        tx.commit()?;
        Ok(OperationRecord {
            request,
            state,
            result,
        })
    }
    pub fn recover_running(&mut self) -> Result<usize> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut count = tx.execute(
            "UPDATE operations SET state='\"unknown\"' WHERE state='\"running\"'",
            [],
        )?;
        count += crate::identity::execution_handoff::recover_execution_handoffs_in(&tx)?;
        tx.execute("UPDATE continuation_claims SET record=json_set(record,'$.state','unknown') WHERE json_extract(record,'$.state')='running'",[])?;
        tx.execute(
            "UPDATE local_values SET value=json_set(value,'$.state','unknown') WHERE key GLOB 'scheduler:invocation:*' AND json_extract(value,'$.state')='running'",
            [],
        )?;
        tx.execute(
            "UPDATE local_values SET value=json_set(value,'$.active.state','unknown','$.native_id',NULL) WHERE key GLOB 'scheduler:session:*' AND json_extract(value,'$.active.state') IN ('running','unknown')",
            [],
        )?;
        tx.commit()?;
        Ok(count)
    }
    pub fn claim_continuation(
        &mut self,
        network_id: &str,
        result_event_hash: &str,
        policy_version: &str,
        scheduler_node: &str,
    ) -> Result<ContinuationClaim> {
        ensure!(!policy_version.is_empty(), "policy version is required");
        self.authorize_node(network_id, scheduler_node, Some(NodeRole::Scheduler))?;
        let event = self
            .event(network_id, result_event_hash)?
            .context("result event is missing")?;
        let event = self.verify_event(network_id, &event)?;
        ensure!(
            matches!(event.body, EventBody::ExecutionObserved { .. }),
            "continuation requires an execution result"
        );
        ensure!(
            self.is_local_device(network_id, &event.author)?,
            "continuation must be claimed at its event-producing Node"
        );
        ensure!(
            self.missing_dependencies(network_id, &[result_event_hash.into()])?
                .is_complete(),
            "result history incomplete"
        );
        let claim = ContinuationClaim {
            result_event_hash: result_event_hash.into(),
            policy_version: policy_version.into(),
            scheduler_node: scheduler_node.into(),
            state: "running".into(),
            result: Value::Null,
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize_node_from(&tx, network_id, scheduler_node, Some(NodeRole::Scheduler))?;
        authorize_node_from(&tx, network_id, &event.author, None)?;
        let changed=tx.execute("INSERT OR IGNORE INTO continuation_claims(space_id,result_event_hash,policy_version,record) VALUES(?,?,?,?)",params![network_id,result_event_hash,policy_version,serde_json::to_string(&claim)?])?;
        ensure!(
            changed == 1,
            "continuation has already been claimed; reconcile its original attempt"
        );
        tx.commit()?;
        Ok(claim)
    }
    pub fn continuation(
        &self,
        network_id: &str,
        result_event_hash: &str,
        policy_version: &str,
    ) -> Result<Option<ContinuationClaim>> {
        optional_json(self.connection.query_row("SELECT record FROM continuation_claims WHERE space_id=? AND result_event_hash=? AND policy_version=?",params![network_id,result_event_hash,policy_version],|r|r.get(0)).optional()?)
    }
    pub fn finish_continuation(
        &mut self,
        network_id: &str,
        result_event_hash: &str,
        policy_version: &str,
        scheduler_node: &str,
        state: &str,
        result: Value,
    ) -> Result<ContinuationClaim> {
        ensure!(
            matches!(state, "completed" | "failed" | "unknown"),
            "invalid continuation completion"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record:String=tx.query_row("SELECT record FROM continuation_claims WHERE space_id=? AND result_event_hash=? AND policy_version=?",params![network_id,result_event_hash,policy_version],|r|r.get(0))?;
        let mut claim: ContinuationClaim = serde_json::from_str(&record)?;
        ensure!(
            claim.scheduler_node == scheduler_node,
            "continuation belongs to a different processing attempt"
        );
        if matches!(claim.state.as_str(), "completed" | "failed") {
            ensure!(
                claim.state == state && claim.result == result,
                "completed continuation cannot change"
            );
            return Ok(claim);
        }
        claim.state = state.into();
        claim.result = result;
        tx.execute("UPDATE continuation_claims SET record=? WHERE space_id=? AND result_event_hash=? AND policy_version=?",params![serde_json::to_string(&claim)?,network_id,result_event_hash,policy_version])?;
        tx.commit()?;
        Ok(claim)
    }
    pub fn queue_notification(
        &mut self,
        network_id: &str,
        notification: &Notification,
    ) -> Result<()> {
        ensure!(
            !notification.notification_id.is_empty(),
            "notification ID required"
        );
        let encoded = serde_json::to_string(notification)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT record FROM notification_outbox WHERE space_id=? AND notification_id=?",
                params![network_id, notification.notification_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            ensure!(old == encoded, "notification ID reused with different body");
        } else {
            tx.execute(
                "INSERT INTO notification_outbox(space_id,notification_id,record) VALUES(?,?,?)",
                params![network_id, notification.notification_id, encoded],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn pending_notifications(
        &self,
        network_id: &str,
        client_id: &str,
    ) -> Result<Vec<Notification>> {
        let mut statement=self.connection.prepare("SELECT n.record FROM notification_outbox n WHERE n.space_id=? AND (n.derived=0 OR EXISTS(SELECT 1 FROM events e WHERE e.space_id=n.space_id AND e.hash=json_extract(n.record,'$.event_hash') AND e.status='ready')) AND NOT EXISTS(SELECT 1 FROM notification_acks a WHERE a.space_id=n.space_id AND a.notification_id=n.notification_id AND a.client_id=?) ORDER BY n.notification_id")?;
        let rows =
            statement.query_map(params![network_id, client_id], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub fn ack_notification(
        &mut self,
        network_id: &str,
        notification_id: &str,
        client_id: &str,
    ) -> Result<()> {
        ensure!(!client_id.is_empty(), "client ID is required");
        let exists:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM notification_outbox WHERE space_id=? AND notification_id=?)",params![network_id,notification_id],|r|r.get(0))?;
        ensure!(exists, "unknown notification");
        self.connection.execute("INSERT INTO notification_acks(space_id,notification_id,client_id,event_hash) VALUES(?,?,?,'') ON CONFLICT(space_id,notification_id,client_id) DO UPDATE SET event_hash=''",params![network_id,notification_id,client_id])?;
        Ok(())
    }
    pub fn budget_usage(&self, network_id: &str, day: &str, limit: u64) -> Result<BudgetUsage> {
        validate_day(day)?;
        let used = self
            .connection
            .query_row(
                "SELECT used FROM budget_days WHERE space_id=? AND day=?",
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
                "SELECT used FROM budget_days WHERE space_id=? AND day=?",
                params![network_id, day],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let next = used.checked_add(units).context("budget overflow")?;
        ensure!(next <= limit, "daily model budget exhausted");
        tx.execute("INSERT INTO budget_days(space_id,day,used) VALUES(?,?,?) ON CONFLICT(space_id,day) DO UPDATE SET used=excluded.used",params![network_id,day,next])?;
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
            .prepare("SELECT space_id FROM spaces ORDER BY space_id")?;
        Ok(statement
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
    pub fn scopes(&self, network_id: &str) -> Result<Vec<ScopeId>> {
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT scope_id FROM events WHERE space_id=? ORDER BY scope_id")?;
        Ok(statement
            .query_map([network_id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
}
pub(crate) fn rebuild_projections_from(connection: &Connection, network_id: &str) -> Result<()> {
    let policy = crate::identity::journal::PolicyState::load(connection)?;
    let mut events = load_events_from(connection, network_id)?;
    let mut author_sequences: BTreeMap<(String, String, u64), Vec<String>> = BTreeMap::new();
    for (hash, e) in &events {
        author_sequences
            .entry((
                e.event.scope_id.clone(),
                e.event.author.clone(),
                e.event.sequence,
            ))
            .or_default()
            .push(hash.clone());
    }
    for e in events.values_mut() {
        e.status = "pending".into();
    }
    for hashes in author_sequences.values().filter(|v| v.len() > 1) {
        for hash in hashes {
            events.get_mut(hash).unwrap().status = "conflict".into();
        }
    }
    for hash in events.keys().cloned().collect::<Vec<_>>() {
        let event = &events[&hash].event;
        if let Some(previous) = &event.previous
            && let Some(prior) = events.get(previous)
            && (prior.event.scope_id != event.scope_id
                || prior.event.author != event.author
                || prior.event.sequence + 1 != event.sequence)
        {
            events.get_mut(&hash).unwrap().status = "invalid".into();
        }
    }
    let all: BTreeSet<String> = events.keys().cloned().collect();
    let ordered = topological(&events, &all);
    for hash in &ordered {
        let e = &events[hash];
        if e.status != "pending" {
            continue;
        }
        let deps: Vec<String> = dependencies(&e.event).cloned().collect();
        if deps.iter().any(|h| {
            events
                .get(h)
                .is_some_and(|p| matches!(p.status.as_str(), "invalid" | "conflict"))
        }) {
            events.get_mut(hash).unwrap().status = "invalid".into();
            continue;
        }
        if !deps
            .iter()
            .all(|h| events.get(h).is_some_and(|p| p.status == "ready"))
        {
            continue;
        }
        let mut blobs_complete = true;
        for blob in &e.event.blobs {
            if !has_blob_from(connection, network_id, blob)? {
                blobs_complete = false;
                break;
            }
        }
        if blobs_complete
            && matches!(
                crate::identity::event_authorization::validate_event_evidence(
                    &policy, connection, &e.event, &events,
                )?,
                crate::identity::event_authorization::EventEvidenceStatus::Valid
            )
        {
            events.get_mut(hash).unwrap().status = "ready".into();
        }
    }
    for (hash, e) in &events {
        connection.execute(
            "UPDATE events SET status=? WHERE space_id=? AND hash=?",
            params![e.status, network_id, hash],
        )?;
    }
    project_events_from(connection, network_id, &events, &ordered)
}

pub(crate) fn rebuild_admitted_projections_from(
    connection: &Connection,
    network_id: &str,
) -> Result<()> {
    ensure!(
        !connection.is_autocommit(),
        "admitted projections require their admission transaction"
    );
    let events = load_retained_event_rows_from(connection, network_id)?;
    let all = events.keys().cloned().collect();
    let ordered = topological(&events, &all);
    project_events_from(connection, network_id, &events, &ordered)
}

fn project_events_from(
    connection: &Connection,
    network_id: &str,
    events: &BTreeMap<String, StoredEvent>,
    ordered: &[String],
) -> Result<()> {
    connection.execute("DELETE FROM messages WHERE space_id=?", [network_id])?;
    connection.execute("DELETE FROM tasks WHERE space_id=?", [network_id])?;
    connection.execute("DELETE FROM conversations WHERE space_id=?", [network_id])?;
    let mut projected_notifications = BTreeSet::new();
    for (ordinal, hash) in ordered.iter().enumerate() {
        let stored = &events[hash];
        if stored.status != "ready" {
            continue;
        }
        let event = &stored.event;
        match &event.body {
            EventBody::ConversationCreated { title } => {
                connection.execute("INSERT OR IGNORE INTO conversations(space_id,resource_id,conversation_id,title) VALUES(?,?,?,?)",params![network_id,event.resource_id.as_str(),event.scope_id,title])?;
            }
            EventBody::MessagePosted {
                message_id,
                role,
                text,
                ..
            } => {
                connection.execute("INSERT INTO messages(space_id,conversation_id,event_hash,message_id,role,text,created_at_ms,ordinal) VALUES(?,?,?,?,?,?,?,?)",params![network_id,event.scope_id,hash,message_id,role,text,event.created_at_ms,ordinal])?;
            }
            EventBody::TaskCreated {
                task_id,
                title,
                goal,
                acceptance,
            } => {
                connection.execute("INSERT OR IGNORE INTO tasks(space_id,conversation_id,task_id,title,goal,acceptance,status,detail,event_hash) VALUES(?,?,?,?,?,?,'open','',?)",params![network_id,event.scope_id,task_id,title,goal,acceptance,hash])?;
            }
            EventBody::TaskChanged {
                task_id,
                status,
                detail,
            } => {
                connection.execute("UPDATE tasks SET status=?,detail=?,event_hash=? WHERE space_id=? AND conversation_id=? AND task_id=?",params![status,detail,hash,network_id,event.scope_id,task_id])?;
            }
            EventBody::NotificationRaised {
                notification_id,
                kind,
                text,
            } => {
                let notification = Notification {
                    notification_id: notification_id.clone(),
                    conversation_id: event.scope_id.clone(),
                    kind: kind.clone(),
                    text: text.clone(),
                    event_hash: hash.clone(),
                };
                if projected_notifications.insert(notification_id.clone()) {
                    connection.execute("INSERT INTO notification_outbox(space_id,notification_id,record,derived) VALUES(?,?,?,1) ON CONFLICT(space_id,notification_id) DO UPDATE SET record=excluded.record WHERE notification_outbox.derived=1",params![network_id,notification_id,serde_json::to_string(&notification)?])?;
                }
            }
            EventBody::NotificationAcknowledged {
                notification_id,
                client_id,
            } => {
                connection.execute("INSERT OR IGNORE INTO notification_acks(space_id,notification_id,client_id,event_hash) VALUES(?,?,?,?)",params![network_id,notification_id,client_id,hash])?;
            }
            _ => {}
        }
    }
    Ok(())
}
fn authorize_node_from(
    connection: &Connection,
    network_id: &str,
    node_id: &str,
    _role: Option<NodeRole>,
) -> Result<NodeCertificate> {
    use vibeface_secure::authorization::{DeviceId, PublicKey, SpaceId};
    let key: String = connection.query_row(
        "SELECT public_key FROM node_certificates WHERE space_id=? AND node_id=?",
        params![network_id, node_id],
        |row| row.get(0),
    )?;
    ensure!(
        crate::crypto::fingerprint(&key)? == node_id,
        "Node certificate projection mismatch"
    );
    let policy = crate::identity::journal::PolicyState::load(connection)?;
    let public_key = PublicKey::parse(&key)?;
    let certificate = policy.verified().verify_service_device(
        &SpaceId::parse(network_id)?,
        &DeviceId::from_key(&public_key),
        crate::runtime::now_ms() as u64,
    )?;
    ensure!(
        certificate.payload.public_key == public_key,
        "Node certificate key mismatch"
    );
    Ok(NodeCertificate {
        node_id: node_id.to_owned(),
        public_key: key,
        roles: vec![],
        revoked: false,
    })
}

pub(crate) fn validate_released_operation_reservations_from(
    connection: &Connection,
    space: &str,
    operation: &str,
) -> Result<()> {
    let remaining: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM resource_reservations WHERE space_id=? AND operation_id=?)",
        params![space, operation],
        |row| row.get(0),
    )?;
    ensure!(
        !remaining,
        "completed operation still has resource reservations"
    );
    Ok(())
}

pub(crate) fn validate_operation_binding_from(
    connection: &Connection,
    request: &OperationRequest,
) -> Result<()> {
    let proof = request
        .binding
        .as_ref()
        .context("execution requires a confirmed task binding")?;
    let binding = &proof.binding;
    ensure!(
        binding.network_id == request.network_id
            && binding.conversation_id == request.conversation_id
            && Some(binding.task_id.as_str()) == request.task_id.as_deref(),
        "operation does not match its bound task"
    );
    ensure!(
        binding.target_node == request.target_node && binding.session_id == request.session_id,
        "operation target or session differs from confirmed task binding"
    );
    if request.action["type"] == "open" {
        ensure!(
            request.action == binding.open_spec,
            "open execution differs from confirmed binding"
        );
    }
    let ready: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE space_id=? AND hash=? AND status='ready')",
        params![binding.network_id, binding.task_ref],
        |r| r.get(0),
    )?;
    ensure!(ready, "task creation is incomplete or conflicting");
    let hash = crate::binding::binding_hash(binding)?;
    let current: Option<String> = connection
        .query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![
                request.network_id,
                format!("task-binding-current:{}", binding.task_ref)
            ],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        current
            .as_deref()
            .map(serde_json::from_str::<String>)
            .transpose()?
            .as_deref()
            == Some(hash.as_str()),
        "task binding is missing or superseded"
    );
    let sealed: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_values WHERE space_id=? AND key=?)",
        params![request.network_id, format!("task-binding-seal:{hash}")],
        |r| r.get(0),
    )?;
    ensure!(
        !sealed,
        "execution binding is sealed; old commands cannot resume it"
    );
    Ok(())
}

pub(crate) fn heads_from_events(
    events: &BTreeMap<String, StoredEvent>,
    scope_id: &str,
) -> Vec<String> {
    let mut heads: BTreeSet<String> = events
        .iter()
        .filter(|(_, e)| e.event.scope_id == scope_id)
        .map(|(h, _)| h.clone())
        .collect();
    for entry in events.values().filter(|e| e.event.scope_id == scope_id) {
        for parent in dependencies(&entry.event) {
            heads.remove(parent);
        }
    }
    heads.into_iter().collect()
}
pub(crate) fn load_events_from(
    connection: &Connection,
    network_id: &str,
) -> Result<BTreeMap<String, StoredEvent>> {
    let result = load_retained_event_rows_from(connection, network_id)?;
    let policy = crate::identity::journal::PolicyState::load(connection)?;
    for stored in result.values() {
        ensure!(
            crate::identity::event_authorization::verify_event_envelope(
                &policy,
                network_id,
                &stored.envelope,
            )? == stored.event,
            "stored event differs from signed envelope"
        );
    }
    for stored in result.values() {
        let evidence = crate::identity::event_authorization::validate_event_evidence(
            &policy,
            connection,
            &stored.event,
            &result,
        )?;
        ensure!(
            stored.status != "ready"
                || matches!(
                    evidence,
                    crate::identity::event_authorization::EventEvidenceStatus::Valid
                ),
            "ready event has incomplete admission or causal evidence"
        );
    }
    Ok(result)
}

fn load_retained_event_rows_from(
    connection: &Connection,
    network_id: &str,
) -> Result<BTreeMap<String, StoredEvent>> {
    let mut query = connection
        .prepare("SELECT hash,envelope,status,scope_id,author,sequence,resource_id FROM events WHERE space_id=? ORDER BY hash")?;
    let rows = query.query_map([network_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, u64>(5)?,
            r.get::<_, String>(6)?,
        ))
    })?;
    let mut result = BTreeMap::new();
    for row in rows {
        let (hash, envelope, status, scope, author, sequence, resource) = row?;
        let envelope: SignedEvent = serde_json::from_str(&envelope)?;
        ensure!(envelope.hash == hash, "stored event hash mismatch");
        ensure!(
            envelope.canonical_body.len() <= MAX_EVENT_BYTES
                && digest(envelope.canonical_body.as_bytes()) == hash,
            "stored event body hash mismatch"
        );
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        ensure!(
            event.network_id == network_id
                && serde_json::to_string(&event)? == envelope.canonical_body,
            "stored event body is not canonical for its space"
        );
        crate::crypto::validate_event_shape(&event)?;
        ensure!(
            event.scope_id == scope
                && event.author == author
                && event.sequence == sequence
                && event.resource_id.as_str() == resource,
            "stored event metadata does not match signed body"
        );
        result.insert(
            hash,
            StoredEvent {
                envelope,
                event,
                status,
            },
        );
    }
    Ok(result)
}

pub(crate) fn has_blob_from(
    connection: &Connection,
    network_id: &str,
    blob: &BlobRef,
) -> Result<bool> {
    let content: Option<Vec<u8>> = connection
        .query_row(
            "SELECT content FROM blobs WHERE space_id=? AND hash=? AND size=?",
            params![network_id, blob.hash, blob.size],
            |r| r.get(0),
        )
        .optional()?;
    Ok(content.is_some_and(|c| {
        c.len() as u64 == blob.size && c.len() <= MAX_BLOB_BYTES && digest(&c) == blob.hash
    }))
}
fn optional_json<T: DeserializeOwned>(value: Option<String>) -> Result<Option<T>> {
    value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
}
fn dependencies(event: &Event) -> impl Iterator<Item = &String> {
    event.parents.iter().chain(event.previous.iter())
}
pub(crate) fn closure_hashes(
    events: &BTreeMap<String, StoredEvent>,
    heads: &[String],
) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    let mut stack = heads.to_vec();
    while let Some(hash) = stack.pop() {
        if result.insert(hash.clone())
            && let Some(event) = events.get(&hash)
        {
            stack.extend(dependencies(&event.event).cloned());
        }
    }
    result
}
fn topological(events: &BTreeMap<String, StoredEvent>, closure: &BTreeSet<String>) -> Vec<String> {
    let mut incoming: BTreeMap<String, usize> = BTreeMap::new();
    let mut children: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for hash in closure {
        if let Some(event) = events.get(hash) {
            let deps: BTreeSet<_> = dependencies(&event.event)
                .filter(|h| closure.contains(*h) && events.contains_key(*h))
                .cloned()
                .collect();
            incoming.insert(hash.clone(), deps.len());
            for parent in deps {
                children.entry(parent).or_default().insert(hash.clone());
            }
        }
    }
    let mut ready: BTreeSet<String> = incoming
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(h, _)| h.clone())
        .collect();
    let mut ordered = Vec::new();
    while let Some(hash) = ready.pop_first() {
        ordered.push(hash.clone());
        if let Some(next) = children.get(&hash) {
            for child in next {
                let count = incoming.get_mut(child).unwrap();
                *count -= 1;
                if *count == 0 {
                    ready.insert(child.clone());
                }
            }
        }
    }
    ordered
}
pub(crate) fn canonical_resource(resource: &str) -> Result<String> {
    ensure!(resource.len() <= 4096, "resource path too long");
    let path = Path::new(resource);
    ensure!(path.is_absolute(), "resource paths must be absolute");
    // Reject ambiguous non-existent paths rather than guessing how a future symlink resolves.
    let canonical = if path.exists() {
        std::fs::canonicalize(path)?
    } else {
        ensure!(
            !path.components().any(|c| matches!(c, Component::ParentDir)),
            "nonexistent resource path cannot contain parent traversal"
        );
        let mut ancestor = path.to_path_buf();
        let mut tail = Vec::new();
        while !ancestor.exists() {
            tail.push(
                ancestor
                    .file_name()
                    .context("invalid resource path")?
                    .to_os_string(),
            );
            ensure!(ancestor.pop(), "resource has no existing ancestor");
        }
        let mut canonical = std::fs::canonicalize(&ancestor)?;
        for component in tail.iter().rev() {
            canonical.push(component);
        }
        canonical
    };
    canonical
        .to_str()
        .map(str::to_string)
        .context("resource path must be UTF-8")
}
pub(crate) fn paths_overlap(left: &str, right: &str) -> bool {
    let left = Path::new(left);
    let right = Path::new(right);
    left.starts_with(right) || right.starts_with(left)
}
pub(crate) fn check_execution_resources_in(
    connection: &Connection,
    target: &str,
    resources: &[String],
) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT resource FROM resource_reservations WHERE target_node=?1 UNION ALL SELECT resource FROM execution_handoff_reservations WHERE target_node=?1",
    )?;
    let existing = statement.query_map([target], |row| row.get::<_, String>(0))?;
    for resource in existing {
        let resource = resource?;
        ensure!(
            !resources
                .iter()
                .any(|requested| paths_overlap(requested, &resource)),
            "resource overlaps an unresolved operation on this device"
        );
    }
    Ok(())
}

pub(crate) fn check_execution_session_in(
    connection: &Connection,
    original_space: &str,
    target: &str,
    session: &str,
    control: bool,
) -> Result<()> {
    let busy: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations WHERE space_id=?1 AND target_node=?2 AND session_id=?3 AND state IN ('\"accepted\"','\"running\"','\"unknown\"') AND (?4=0 OR control=1) UNION ALL SELECT 1 FROM execution_handoffs WHERE original_space_id=?1 AND target_node=?2 AND session_id=?3 AND state IN ('accepted','running','unknown') AND (?4=0 OR control=1))",
        params![original_space,target,session,control], |row| row.get(0),
    )?;
    ensure!(!busy, "execution session has an unresolved operation");
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::test_support::{TestNetwork, now};
    use anyhow::Result;
    use serde_json::json;
    use vibeface_secure::{
        Identity,
        authorization::{
            bootstrap::{self, recovery},
            *,
        },
    };

    fn setup() -> Result<(TestNetwork, Store)> {
        let network = TestNetwork::new()?;
        let store = network.memory()?;
        Ok((network, store))
    }

    fn second_space(network: &TestNetwork, store: &Store) -> Result<TestNetwork> {
        let root = Identity::generate().map_err(anyhow::Error::msg)?;
        let public_key = PublicKey::of(&root);
        let space = SpaceId::team(&public_key);
        let mut document = store
            .authorization_state()
            .space(&network.space)
            .unwrap()
            .clone();
        document.space = space.clone();
        document.root = SpaceRoot::Team {
            genesis_root: public_key.clone(),
        };
        document.current_root = public_key.clone();
        document.previous = None;
        document.sequence = 0;
        document.devices.clear();
        document.revoked_devices.clear();
        document.memberships.clear();
        document.grants.clear();
        document.resources.clear();
        document.governance_grant = None;
        let signed_genesis = Signed::sign(document.clone(), &root)?;
        let mut state = store.authorization_state().clone();
        let genesis = state.trust_space_genesis(&public_key, signed_genesis.clone())?;
        let resource = Signed::sign(
            ResourceRecord {
                version: AUTH_VERSION,
                space: space.clone(),
                resource: network.resource.clone(),
                kind: ResourceKind::Project,
                host: ServiceId::from_key(&PublicKey::parse(&network.node.public_key_hex()?)?),
                issued_under: genesis.clone(),
                nonce: Nonce::random()?,
            },
            &root,
        )?;
        let mut grant = state
            .grant(&network.bundle.grant_leaf)
            .unwrap()
            .payload
            .clone();
        grant.owner_space = space.clone();
        grant.scope = ResourceScope::Exact(ResourceRef {
            space: space.clone(),
            resource: network.resource.clone(),
        });
        grant.issued_under = genesis.clone();
        grant.nonce = Nonce::random()?;
        let grant = Signed::sign(grant, &root)?;
        let service = ServiceId::from_key(&PublicKey::parse(&network.node.public_key_hex()?)?);
        let members = [
            Actor::Account(network.account.clone()),
            Actor::Service(service.clone()),
        ]
        .into_iter()
        .map(|subject| {
            Signed::sign(
                Membership {
                    version: AUTH_VERSION,
                    space: space.clone(),
                    subject,
                    roles: vec![SpaceRole::Operator],
                    issued_under: genesis.clone(),
                    not_before_ms: now(),
                    expires_at_ms: now() + 1_000_000,
                    nonce: Nonce::random()?,
                },
                &root,
            )
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
        let node_certificate = network
            .bundle
            .journal
            .iter()
            .find_map(|entry| match entry {
                JournalEntry::Device(certificate)
                    if certificate.payload.subject == Actor::Service(service.clone()) =>
                {
                    Some(certificate.hash())
                }
                _ => None,
            })
            .context("account service certificate missing")??;
        document.previous = Some(genesis);
        document.sequence = 1;
        document.devices = vec![node_certificate];
        document.resources = vec![resource.hash()?];
        document.grants = vec![grant.hash()?];
        document.memberships = members
            .iter()
            .map(Signed::hash)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        document.memberships.sort();
        let mut journal = network.bundle.journal.clone();
        journal.push(JournalEntry::SpaceGenesis(signed_genesis));
        journal.push(JournalEntry::Resource(resource));
        journal.extend(members.into_iter().map(JournalEntry::Membership));
        journal.push(JournalEntry::Grant(grant.clone()));
        journal.push(JournalEntry::Space(Signed::sign(document, &root)?));
        let mut pins = network.pins.clone();
        pins.push(PolicyPin {
            domain: PolicyDomain::Space(space.clone()),
            genesis_root: public_key,
            minimum_head: None,
        });
        let directory = network
            .directory
            .parent()
            .unwrap()
            .join(uuid::Uuid::new_v4().to_string());
        crate::config::private_dir(&directory)?;
        Ok(TestNetwork {
            created: network.created.clone(),
            root: Identity::from_secret_hex(&network.root.secret_hex())
                .map_err(anyhow::Error::msg)?,
            recovery: Identity::from_secret_hex(&network.recovery.secret_hex())
                .map_err(anyhow::Error::msg)?,
            client: Identity::from_secret_hex(&network.client.secret_hex())
                .map_err(anyhow::Error::msg)?,
            node: network.node.clone(),
            account: network.account.clone(),
            space,
            resource: network.resource.clone(),
            bundle: ClientAuthorizationBundle {
                journal,
                grant_leaf: grant.hash()?,
            },
            pins,
            directory,
            project: network.project.clone(),
        })
    }

    fn blob_result(
        network: &TestNetwork,
        store: &mut Store,
        id: &str,
    ) -> Result<(SignedEvent, BlobRef, Vec<u8>)> {
        let request = network.request(store, id, id)?;
        network.admit(store, &request)?;
        network.claim(store, id)?;
        let result = json!({"evidence":"necessary content"});
        store.finish_operation(
            network.space.as_str(),
            id,
            OperationState::Succeeded,
            result.clone(),
        )?;
        let content = serde_json::to_vec(&result)?;
        let blob = BlobRef {
            hash: digest(&content),
            size: content.len() as u64,
        };
        store.put_blob(network.space.as_str(), &blob, &content)?;
        let source = store
            .operation_dispatch(network.space.as_str(), id)?
            .unwrap();
        let event = store.append_derived_event(
            &network.node,
            network.space.as_str(),
            &source.hash,
            EventBody::ExecutionObserved {
                operation_id: id.into(),
                task_id: request.task_id,
                target_node: request.target_node,
                session_id: request.session_id,
                status: "succeeded".into(),
                detail: json!({"result_blob":blob}),
            },
            vec![blob.clone()],
            (now() + 2) as i64,
        )?;
        Ok((event, blob, content))
    }

    fn result(
        network: &TestNetwork,
        store: &mut Store,
        id: &str,
    ) -> Result<(OperationRequest, SignedEvent)> {
        let request = network.request(store, id, id)?;
        network.admit(store, &request)?;
        network.claim(store, id)?;
        let event = network.complete(
            store,
            &request,
            OperationState::Succeeded,
            json!({"result":"done"}),
        )?;
        Ok((request, event))
    }

    fn notify(
        network: &TestNetwork,
        store: &mut Store,
        request: &OperationRequest,
        result: &SignedEvent,
    ) -> Result<SignedEvent> {
        store.append_derived_event(
            &network.node,
            network.space.as_str(),
            &result.hash,
            EventBody::NotificationRaised {
                notification_id: format!("result:{}:succeeded", request.operation_id),
                kind: "succeeded".into(),
                text: format!(
                    "执行会话 {} 的本轮执行状态为 succeeded。工单验收仍需本人确认。",
                    request.session_id
                ),
            },
            vec![],
            (now() + 3) as i64,
        )
    }

    fn ack(network: &TestNetwork, store: &mut Store, notification_id: &str) -> Result<SignedEvent> {
        let mut body = network.body("ack", "conversation");
        body["notification_id"] = json!(notification_id);
        body["client_id"] = json!("phone");
        network.append(
            store,
            &body,
            EventBody::NotificationAcknowledged {
                notification_id: notification_id.into(),
                client_id: "phone".into(),
            },
            now(),
        )
    }

    fn seal(
        network: &TestNetwork,
        store: &mut Store,
        binding: &crate::binding::SignedBinding,
    ) -> Result<crate::binding::SignedSeal> {
        let mut body = network.body("seal_execution", &binding.binding.conversation_id);
        body["task_id"] = json!(binding.binding.task_id);
        body["task_ref"] = json!(binding.binding.task_ref);
        body["binding"] = json!(binding);
        let (auth, transport) = network.authorization(store, &body, now())?;
        let proof = network.freshness(store, &auth, CheckStage::Admit, now())?;
        store.seal_task_authorized(&network.node, &auth, &proof, &transport, &body, now())
    }

    fn rebind(
        network: &TestNetwork,
        store: &mut Store,
        binding: &crate::binding::SignedBinding,
        previous_seal: &crate::binding::SignedSeal,
        session: &str,
    ) -> Result<crate::binding::SignedBinding> {
        let mut body = network.body("rebind_execution", &binding.binding.conversation_id);
        body["task_id"] = json!(binding.binding.task_id);
        body["task_ref"] = json!(binding.binding.task_ref);
        body["binding"] = json!(binding);
        body["previous_seal"] = json!(previous_seal);
        body["target_node"] = json!(network.node.node_id()?);
        body["session_id"] = json!(session);
        body["executor"] = binding.binding.open_spec["executor"].clone();
        body["cwd"] = binding.binding.open_spec["cwd"].clone();
        let (auth, transport) = network.authorization(store, &body, now())?;
        let proof = network.freshness(store, &auth, CheckStage::Admit, now())?;
        store.rebind_task_authorized(&network.node, &auth, &proof, &transport, &body, now())
    }

    #[test]
    fn admitted_projection_fold_matches_full_rebuild_without_replaying_ledger() -> Result<()> {
        fn snapshot(store: &Store, space: &str) -> Result<Vec<Vec<String>>> {
            [
                "SELECT json_array(resource_id,conversation_id,title) FROM conversations WHERE space_id=? ORDER BY conversation_id",
                "SELECT json_array(conversation_id,event_hash,message_id,role,text,created_at_ms,ordinal) FROM messages WHERE space_id=? ORDER BY event_hash",
                "SELECT json_array(conversation_id,task_id,title,goal,acceptance,status,detail,event_hash) FROM tasks WHERE space_id=? ORDER BY conversation_id,task_id",
                "SELECT json_array(notification_id,record,derived) FROM notification_outbox WHERE space_id=? ORDER BY notification_id",
                "SELECT json_array(notification_id,client_id,event_hash) FROM notification_acks WHERE space_id=? ORDER BY notification_id,client_id",
                "SELECT json_array(operation_id,target_node,session_id,request,state,control,result) FROM operations WHERE space_id=? ORDER BY operation_id",
                "SELECT json_array(device_id,nonce,authorization_id,body_hash) FROM authorization_nonces WHERE space_id=? ORDER BY device_id,nonce",
                "SELECT json_array(nonce,invocation_hash,stage,challenge,expires_at_ms,consumed) FROM authorization_challenges WHERE space_id=? ORDER BY nonce",
                "SELECT json_array(hash,status) FROM events WHERE space_id=? ORDER BY hash",
            ].into_iter().map(|sql| {
                Ok(store.connection.prepare(sql)?.query_map([space], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?)
            }).collect()
        }
        let (network, mut store) = setup()?;
        let space = network.space.as_str();
        network.conversation(&mut store, "conversation")?;
        network.conversation(&mut store, "parallel")?;
        network.message(&mut store, "conversation", "first")?;
        network.message(&mut store, "parallel", "other conversation")?;
        network.task(&mut store, "parallel", "parallel-task")?;
        let (request, result) = result(&network, &mut store, "projection-fold")?;
        notify(&network, &mut store, &request, &result)?;
        ack(&network, &mut store, "result:projection-fold:succeeded")?;
        network.message(&mut store, "conversation", "last")?;
        let expected = snapshot(&store, space)?;
        assert_eq!(
            store.messages(space, "parallel")?[0].text,
            "other conversation"
        );
        assert!(!store.operations(space)?.is_empty());
        assert!(!expected[3].is_empty() && !expected[4].is_empty() && !expected[6].is_empty());
        assert!(rebuild_admitted_projections_from(&store.connection, space).is_err());
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM messages WHERE space_id=?", [space])?;
        tx.execute("DELETE FROM tasks WHERE space_id=?", [space])?;
        tx.execute(
            "UPDATE conversations SET title='changed projection' WHERE space_id=?",
            [space],
        )?;
        tx.execute(
            "UPDATE notification_outbox SET record='{}' WHERE space_id=? AND derived=1",
            [space],
        )?;
        let verification = vibeface_secure::signature_verification::SignatureVerification::new();
        verification.run(|| rebuild_admitted_projections_from(&tx, space))?;
        assert_eq!(verification.counts().actual_verifications, 0);
        tx.commit()?;
        assert_eq!(snapshot(&store, space)?, expected);
        store.rebuild_projections(space)?;
        assert_eq!(snapshot(&store, space)?, expected);
        Ok(())
    }

    #[test]
    fn causal_import_waits_for_every_dependency_and_blob_then_converges() -> Result<()> {
        let (network, mut source) = setup()?;
        let space = network.space.as_str();
        network.conversation(&mut source, "conversation")?;
        network.message(&mut source, "conversation", "request")?;
        let (event, blob, content) = blob_result(&network, &mut source, "result")?;
        let mut replica = network.memory()?;
        assert_eq!(replica.ingest_event(space, &event)?.status, "pending");
        assert!(
            !replica
                .missing_dependencies(space, std::slice::from_ref(&event.hash))?
                .events
                .is_empty()
        );
        let snapshot = source.export_snapshot(space, "conversation")?;
        for envelope in &snapshot.events {
            if envelope.hash != event.hash {
                replica.ingest_event(space, envelope)?;
            }
        }
        assert_eq!(
            replica.context(space, "conversation", &[])?.missing.blobs,
            vec![blob.clone()]
        );
        assert!(replica.put_blob(space, &blob, b"wrong").is_err());
        replica.put_blob(space, &blob, &content)?;
        replica.ingest_event(space, &event)?;
        assert!(
            replica
                .context(space, "conversation", &[])?
                .missing
                .is_complete()
        );
        assert_eq!(
            replica.messages(space, "conversation")?,
            source.messages(space, "conversation")?
        );
        assert_eq!(
            replica.heads(space, "conversation")?,
            source.heads(space, "conversation")?
        );
        assert!(replica.operations(space)?.is_empty());
        assert_eq!(replica.context(space, "conversation", &[])?.events.len(), 5);
        Ok(())
    }

    #[test]
    fn corrupted_stored_blob_blocks_context_until_verified_content_is_restored() -> Result<()> {
        let (network, mut store) = setup()?;
        let space = network.space.as_str();
        network.conversation(&mut store, "conversation")?;
        let (_, blob, content) = blob_result(&network, &mut store, "blob")?;
        store.connection.execute(
            "UPDATE blobs SET content=? WHERE space_id=? AND hash=?",
            params![b"tampered".as_slice(), space, blob.hash],
        )?;
        assert!(store.get_blob(space, &blob.hash).is_err());
        let error = store.context(space, "conversation", &[]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("ready event has incomplete admission or causal evidence")
        );
        assert!(store.rebuild_projections(space).is_err());
        store.put_blob(space, &blob, &content)?;
        assert!(
            store
                .context(space, "conversation", &[])?
                .missing
                .is_complete()
        );
        Ok(())
    }

    #[test]
    fn author_equivocation_removes_both_projections_and_blocks_descendants() -> Result<()> {
        let (network, mut store) = setup()?;
        let space = network.space.as_str();
        let root = network.conversation(&mut store, "conversation")?;
        let left = network.signed_message(
            &mut store,
            "one",
            2,
            Some(root.hash.clone()),
            vec![root.hash.clone()],
        )?;
        let right = network.signed_message(
            &mut store,
            "two",
            2,
            Some(root.hash.clone()),
            vec![root.hash],
        )?;
        store.ingest_event(space, &left)?;
        assert_eq!(store.messages(space, "conversation")?.len(), 1);
        assert_eq!(store.ingest_event(space, &right)?.status, "conflict");
        assert!(store.messages(space, "conversation")?.is_empty());
        let child = network.signed_message(
            &mut store,
            "child",
            3,
            Some(left.hash.clone()),
            vec![left.hash],
        )?;
        assert_eq!(store.ingest_event(space, &child)?.status, "invalid");
        assert!(
            !store
                .context(space, "conversation", &[])?
                .missing
                .is_complete()
        );
        assert!(
            network
                .message(&mut store, "conversation", "blocked")
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn revoked_author_history_verifies_but_new_commands_are_denied() -> Result<()> {
        let (network, mut store) = setup()?;
        let space = network.space.as_str();
        let root = network.conversation(&mut store, "conversation")?;
        let old = network.signed_message(
            &mut store,
            "historical",
            2,
            Some(root.hash.clone()),
            vec![root.hash],
        )?;
        network.revoke_node(&mut store)?;
        assert!(
            store
                .authorize_node(space, &network.node.node_id()?, None)
                .is_err()
        );
        assert_eq!(store.ingest_event(space, &old)?.status, "ready");
        assert!(
            network
                .message(&mut store, "conversation", "denied")
                .is_err()
        );
        assert!(store.verify_event(space, &old).is_ok());
        Ok(())
    }

    #[test]
    fn queued_operation_cannot_start_after_known_executor_revocation() -> Result<()> {
        let (network, mut store) = setup()?;
        network.conversation(&mut store, "conversation")?;
        let request = network.request(&mut store, "queued", "session")?;
        network.admit(&mut store, &request)?;
        network.revoke_node(&mut store)?;
        assert!(network.claim(&mut store, "queued").is_err());
        assert_eq!(
            store
                .operation(network.space.as_str(), "queued")?
                .unwrap()
                .state,
            OperationState::Accepted
        );
        assert_eq!(
            store.connection.query_row(
                "SELECT count(*) FROM resource_reservations",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            1
        );
        Ok(())
    }

    #[test]
    fn concurrent_local_writers_serialize_author_sequences_and_projection_updates() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut initial = network.memory()?;
        network.conversation(&mut initial, "conversation")?;
        let store = std::sync::Mutex::new(initial);
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| -> Result<()> {
            let threads = (0..2)
                .map(|writer| {
                    let network = &network;
                    let store = &store;
                    let barrier = &barrier;
                    scope.spawn(move || -> Result<()> {
                        barrier.wait();
                        for index in 0..6 {
                            network.message(
                                &mut store.lock().unwrap(),
                                "conversation",
                                &format!("{writer}-{index}"),
                            )?;
                        }
                        Ok(())
                    })
                })
                .collect::<Vec<_>>();
            for thread in threads {
                thread.join().unwrap()?;
            }
            Ok(())
        })?;
        let store = store.into_inner().unwrap();
        assert_eq!(
            store
                .messages(network.space.as_str(), "conversation")?
                .len(),
            12
        );
        let context = store.context(network.space.as_str(), "conversation", &[])?;
        assert!(context.missing.is_complete());
        assert_eq!(context.events.len(), 13);
        assert_eq!(context.heads.len(), 1);
        Ok(())
    }

    #[test]
    fn corrupted_stored_event_never_becomes_a_complete_context() -> Result<()> {
        let (network, mut store) = setup()?;
        let mut event = network.conversation(&mut store, "conversation")?;
        event.canonical_body = event.canonical_body.replace("work", "tampered");
        store.connection.execute(
            "UPDATE events SET envelope=? WHERE space_id=? AND hash=?",
            params![
                serde_json::to_string(&event)?,
                network.space.as_str(),
                event.hash
            ],
        )?;
        assert!(
            store
                .context(network.space.as_str(), "conversation", &[])
                .is_err()
        );
        assert!(store.rebuild_projections(network.space.as_str()).is_err());
        Ok(())
    }

    #[test]
    fn task_binding_cas_rejects_competing_targets_and_rolls_back_failed_commits() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut initial = network.memory()?;
        network.conversation(&mut initial, "conversation")?;
        let creation = network.task(&mut initial, "conversation", "task")?;
        let accepted: u64 = initial.connection.query_row(
            "SELECT count(*) FROM accepted_authorizations",
            [],
            |row| row.get(0),
        )?;
        let consumed_before: (u64,u64) = initial.connection.query_row(
            "SELECT (SELECT count(*) FROM authorization_nonces),(SELECT count(*) FROM authorization_challenges WHERE consumed=1)",
            [],|row| Ok((row.get(0)?,row.get(1)?)))?;
        initial.connection.execute_batch("CREATE TRIGGER reject_binding_pointer BEFORE INSERT ON local_values WHEN NEW.key LIKE 'task-binding-current:%' BEGIN SELECT RAISE(ABORT,'binding commit failure'); END;")?;
        assert!(
            network
                .bind(&mut initial, &creation, "task", "failed", &network.project)
                .is_err()
        );
        assert_eq!(
            initial.connection.query_row(
                "SELECT count(*) FROM accepted_authorizations",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            accepted
        );
        assert_eq!(initial.connection.query_row(
            "SELECT (SELECT count(*) FROM authorization_nonces),(SELECT count(*) FROM authorization_challenges WHERE consumed=1)",
            [],|row| Ok((row.get::<_,u64>(0)?,row.get::<_,u64>(1)?)))?,consumed_before);
        assert!(
            initial
                .task_binding(network.space.as_str(), &creation.hash)?
                .is_none()
        );
        assert_eq!(
            initial.connection.query_row(
                "SELECT count(*) FROM local_values WHERE key LIKE 'task-binding-record:%'",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            0
        );
        initial
            .connection
            .execute_batch("DROP TRIGGER reject_binding_pointer;")?;
        let store = std::sync::Mutex::new(initial);
        let barrier = std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            ["first", "second"]
                .into_iter()
                .map(|session| {
                    let store = &store;
                    let network = &network;
                    let creation = &creation;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        network.bind(
                            &mut store.lock().unwrap(),
                            creation,
                            "task",
                            session,
                            &network.project,
                        )
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let binding = results.into_iter().find_map(Result::ok).unwrap();
        let store = store.into_inner().unwrap();
        assert_eq!(
            store
                .task_binding(network.space.as_str(), &creation.hash)?
                .unwrap(),
            binding
        );
        Ok(())
    }

    #[test]
    fn unknown_binding_cannot_move_and_seal_fences_delayed_commands() -> Result<()> {
        let (network, mut store) = setup()?;
        let space = network.space.as_str();
        network.conversation(&mut store, "conversation")?;
        let request = network.request(&mut store, "work", "session")?;
        let binding = request.binding.as_ref().unwrap();
        let mut unbound = request.clone();
        unbound.binding = None;
        assert!(network.admit(&mut store, &unbound).is_err());
        let mut wrong = request.clone();
        wrong.target_node = "0".repeat(64);
        assert!(network.admit(&mut store, &wrong).is_err());
        network.admit(&mut store, &request)?;
        network.claim(&mut store, "work")?;
        store.finish_operation(
            space,
            "work",
            OperationState::Unknown,
            json!({"reason":"lost connection"}),
        )?;
        assert!(seal(&network, &mut store, binding).is_err());
        let creation = store.event(space, &binding.binding.task_ref)?.unwrap();
        assert!(
            network
                .bind(
                    &mut store,
                    &creation,
                    &binding.binding.task_id,
                    "replacement",
                    &network.project
                )
                .is_err()
        );
        store.finish_operation(
            space,
            "work",
            OperationState::Failed,
            json!({"verified":"stopped"}),
        )?;
        store.local_set(
            space,
            "completion:work",
            &json!({"state":"failed","result":{"verified":"stopped"}}),
        )?;
        let mut session = store
            .session(space, &network.node.node_id()?, "session")?
            .unwrap();
        session.state = "idle".into();
        store.upsert_session(space, &session)?;
        let error = seal(&network, &mut store, binding).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("execution completion is not durable")
        );
        network.complete(
            &mut store,
            &request,
            OperationState::Failed,
            json!({"verified":"stopped"}),
        )?;
        let mut session = store
            .session(space, &network.node.node_id()?, "session")?
            .unwrap();
        session.state = "idle".into();
        store.upsert_session(space, &session)?;
        let sealed = seal(&network, &mut store, binding)?;
        let mut delayed = request.clone();
        delayed.operation_id = "delayed".into();
        assert!(network.admit(&mut store, &delayed).is_err());
        let replacement = rebind(&network, &mut store, binding, &sealed, "replacement")?;
        store.install_historical_binding(binding)?;
        assert_eq!(
            store
                .task_binding(space, &binding.binding.task_ref)?
                .unwrap(),
            replacement
        );
        assert!(network.admit(&mut store, &delayed).is_err());
        Ok(())
    }

    #[test]
    fn two_scheduler_requests_at_one_revision_admit_only_one_side_effect() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut initial = network.memory()?;
        network.conversation(&mut initial, "conversation")?;
        let request = network.request(&mut initial, "first", "session")?;
        let store = std::sync::Mutex::new(initial);
        let barrier = std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            ["scheduler-a", "scheduler-b"]
                .into_iter()
                .map(|id| {
                    let mut request = request.clone();
                    request.operation_id = id.into();
                    let network = &network;
                    let barrier = &barrier;
                    let store = &store;
                    scope.spawn(move || {
                        barrier.wait();
                        network.admit(&mut store.lock().unwrap(), &request)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let store = store.into_inner().unwrap();
        assert_eq!(store.operations(network.space.as_str())?.len(), 1);
        assert_eq!(
            store.connection.query_row(
                "SELECT count(*) FROM accepted_authorizations WHERE operation_id IS NOT NULL",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            1
        );
        Ok(())
    }

    #[test]
    fn operation_cas_reserves_resources_and_reconciles_unknown_without_replay() -> Result<()> {
        let (network, mut store) = setup()?;
        let space = network.space.as_str();
        network.conversation(&mut store, "conversation")?;
        for path in ["repo/child", "repo-two"] {
            fs::create_dir_all(network.project.join(path))?;
        }
        let first = network.request_at(
            &mut store,
            "operation",
            "first",
            &network.project.join("repo"),
        )?;
        assert_eq!(
            network.admit(&mut store, &first)?.state,
            OperationState::Accepted
        );
        assert_eq!(
            network.admit(&mut store, &first)?.state,
            OperationState::Accepted
        );
        let mut changed = first.clone();
        changed.action = json!({"type":"send","text":"changed"});
        assert!(network.admit(&mut store, &changed).is_err());
        let overlap = network.request_at(
            &mut store,
            "overlap",
            "second",
            &network.project.join("repo/child"),
        )?;
        assert!(network.admit(&mut store, &overlap).is_err());
        let sibling = network.request_at(
            &mut store,
            "sibling",
            "third",
            &network.project.join("repo-two"),
        )?;
        network.admit(&mut store, &sibling)?;
        network.claim(&mut store, "operation")?;
        assert!(network.claim(&mut store, "operation").is_err());
        assert_eq!(store.recover_running()?, 1);
        assert_eq!(
            store.operation(space, "operation")?.unwrap().state,
            OperationState::Unknown
        );
        assert!(network.claim(&mut store, "operation").is_err());
        assert!(network.admit(&mut store, &overlap).is_err());
        network.complete(
            &mut store,
            &first,
            OperationState::Succeeded,
            json!({"actual":"reconciled"}),
        )?;
        assert_eq!(
            store
                .session(space, &network.node.node_id()?, "first")?
                .unwrap()
                .revision,
            1
        );
        let mut stale = first.clone();
        stale.operation_id = "next".into();
        stale.action = json!({"type":"send","text":"new work"});
        assert!(network.admit(&mut store, &stale).is_err());
        stale.expected_revision = 1;
        network.admit(&mut store, &stale)?;
        assert!(
            store
                .finish_operation(space, "operation", OperationState::Failed, Value::Null)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn running_send_allows_deduped_controls_but_blocks_other_sends_and_controls() -> Result<()> {
        let (network, mut store) = setup()?;
        let space = network.space.as_str();
        network.conversation(&mut store, "conversation")?;
        let open = network.request(&mut store, "open", "session")?;
        network.admit(&mut store, &open)?;
        network.claim(&mut store, "open")?;
        network.complete(&mut store, &open, OperationState::Succeeded, Value::Null)?;
        let mut send = open.clone();
        send.operation_id = "send".into();
        send.action = json!({"type":"send","text":"work"});
        send.expected_revision = 1;
        network.admit(&mut store, &send)?;
        network.claim(&mut store, "send")?;
        let mut another = send.clone();
        another.operation_id = "another".into();
        assert!(network.admit(&mut store, &another).is_err());
        let mut control = send.clone();
        control.operation_id = "interrupt".into();
        control.action = json!({"type":"interrupt","turn_id":"turn"});
        control.resources.clear();
        network.admit(&mut store, &control)?;
        network.claim(&mut store, "interrupt")?;
        assert_eq!(
            network.admit(&mut store, &control)?.state,
            OperationState::Running
        );
        let mut second = control.clone();
        second.operation_id = "steer".into();
        second.action = json!({"type":"steer","turn_id":"turn","text":"more"});
        assert!(network.admit(&mut store, &second).is_err());
        network.complete(&mut store, &control, OperationState::Succeeded, Value::Null)?;
        assert!(network.admit(&mut store, &another).is_err());
        second.expected_revision = 2;
        network.admit(&mut store, &second)?;
        assert_eq!(
            store.operation(space, "send")?.unwrap().state,
            OperationState::Running
        );
        let mut session = store
            .session(space, &network.node.node_id()?, "session")?
            .unwrap();
        session.revision = 3;
        store.upsert_session(space, &session)?;
        assert!(network.claim(&mut store, "steer").is_err());
        Ok(())
    }

    #[test]
    fn root_recovery_preserves_revoked_authors_without_restoring_authority() -> Result<()> {
        let (network, mut store) = setup()?;
        let space = network.space.as_str();
        let root = network.conversation(&mut store, "conversation")?;
        let old = network.signed_message(
            &mut store,
            "retired author",
            2,
            Some(root.hash.clone()),
            vec![root.hash.clone()],
        )?;
        let material = recovery::build_account_recovery_material(
            &network.created,
            &network.root,
            &network.recovery,
            "http://localhost:12345",
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let next_root = Identity::generate().map_err(anyhow::Error::msg)?;
        let next_client = Identity::generate().map_err(anyhow::Error::msg)?;
        let node = Identity::generate().map_err(anyhow::Error::msg)?;
        let nonce = Nonce::random()?;
        let recovered_at = (now() + 10) as f64;
        let claim = bootstrap::create_node_claim(
            &node,
            nonce.as_str(),
            recovered_at,
            recovered_at + 100_000.0,
        )
        .map_err(anyhow::Error::msg)?;
        let claim = bootstrap::verify_node_claim(
            &claim,
            &node.public_key_hex(),
            nonce.as_str(),
            recovered_at,
        )
        .map_err(anyhow::Error::msg)?;
        let bundle = serde_json::to_string(&network.bundle)?;
        let pins = serde_json::to_string(&network.pins)?;
        let prepared = recovery::prepare_account_recovery(
            &material,
            &bundle,
            &next_root,
            &next_client,
            &claim,
            recovered_at + 10_000_000.0,
            recovered_at,
        )
        .map_err(anyhow::Error::msg)?;
        let possession = recovery::sign_recovery_node_possession(
            &prepared.node_request_json().map_err(anyhow::Error::msg)?,
            &node,
            &pins,
            &bundle,
            nonce.as_str(),
            recovered_at,
        )
        .map_err(anyhow::Error::msg)?;
        let recovered =
            recovery::finish_account_recovery(&prepared, &next_root, &possession, recovered_at)
                .map_err(anyhow::Error::msg)?;
        let recovered: ClientAuthorizationBundle =
            serde_json::from_str(&recovered.bundle_json().map_err(anyhow::Error::msg)?)?;
        store.install_policy_journal(&recovered.journal, recovered_at as u64)?;
        assert!(
            store
                .authorize_node(space, &network.node.node_id()?, None)
                .is_err()
        );
        assert_eq!(store.ingest_event(space, &old)?.status, "ready");
        assert!(
            network
                .message(&mut store, "conversation", "denied")
                .is_err()
        );
        assert!(store.verify_event(space, &old).is_ok());
        let mut fresh = Store::initialize(Path::new(":memory:"))?;
        let update = fresh.policy.prepare_bootstrap(
            &network.pins,
            &recovered.journal,
            recovered_at as u64,
        )?;
        let tx = fresh
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let policy = crate::identity::journal::commit_in(&tx, &update)?;
        crate::identity::policy::project_policy_in(&tx, &policy)?;
        tx.commit()?;
        fresh.policy = policy;
        assert_eq!(fresh.ingest_event(space, &root)?.status, "ready");
        assert_eq!(fresh.ingest_event(space, &old)?.status, "ready");
        assert!(
            fresh
                .authorize_node(space, &network.node.node_id()?, None)
                .is_err()
        );
        assert_eq!(fresh.messages(space, "conversation")?.len(), 1);
        Ok(())
    }

    #[test]
    fn management_clients_cannot_author_business_dag_events() -> Result<()> {
        let (network, mut store) = setup()?;
        let root = network.conversation(&mut store, "conversation")?;
        let mut event: Event = serde_json::from_str(&root.canonical_body)?;
        let client = LocalIdentity {
            secret_key_hex: network.client.secret_hex(),
        };
        event.author = client.node_id()?;
        let forged = client.sign_event(event)?;
        assert!(store.ingest_event(network.space.as_str(), &forged).is_err());
        assert!(
            store
                .authorize_node(network.space.as_str(), &client.node_id()?, None)
                .is_err()
        );
        assert!(
            store
                .authorize_node(
                    network.space.as_str(),
                    &network.node.node_id()?,
                    Some(NodeRole::Replica)
                )
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn all_replicated_and_local_ids_are_network_scoped() -> Result<()> {
        let (left, mut store) = setup()?;
        let right = TestNetwork::new()?;
        right.install(&mut store)?;
        let a = left.conversation(&mut store, "conversation")?;
        let b = right.conversation(&mut store, "conversation")?;
        assert!(store.ingest_event(right.space.as_str(), &a).is_err());
        assert!(store.event(left.space.as_str(), &b.hash)?.is_none());
        assert_eq!(store.conversations(left.space.as_str())?.len(), 1);
        assert_eq!(store.conversations(right.space.as_str())?.len(), 1);
        let ra = left.request(&mut store, "same-id", "session")?;
        let rb = right.request(&mut store, "same-id", "session")?;
        left.admit(&mut store, &ra)?;
        right.admit(&mut store, &rb)?;
        assert_eq!(
            store
                .operation(left.space.as_str(), "same-id")?
                .unwrap()
                .request
                .target_node,
            left.node.node_id()?
        );
        assert_eq!(
            store
                .operation(right.space.as_str(), "same-id")?
                .unwrap()
                .request
                .target_node,
            right.node.node_id()?
        );
        store.local_set(left.space.as_str(), "auth", &json!("a"))?;
        store.local_set(right.space.as_str(), "auth", &json!("b"))?;
        assert_eq!(
            store.local_value(left.space.as_str(), "auth")?,
            Some(json!("a"))
        );
        assert_eq!(
            store.local_value(right.space.as_str(), "auth")?,
            Some(json!("b"))
        );
        let blob = BlobRef {
            hash: digest(b"private"),
            size: 7,
        };
        store.put_blob(left.space.as_str(), &blob, b"private")?;
        assert!(store.get_blob(right.space.as_str(), &blob.hash)?.is_none());
        Ok(())
    }

    #[test]
    fn same_device_resources_are_reserved_across_networks_without_sharing_records() -> Result<()> {
        let (left, mut store) = setup()?;
        let right = second_space(&left, &store)?;
        right.install(&mut store)?;
        store.local_set(
            right.space.as_str(),
            &format!("resource-root:{}", right.resource),
            &json!(left.project),
        )?;
        for path in ["repo", "repo-two"] {
            fs::create_dir_all(left.project.join(path))?;
        }
        left.conversation(&mut store, "conversation")?;
        right.conversation(&mut store, "conversation")?;
        let a = left.request_at(&mut store, "same-op", "left", &left.project.join("repo"))?;
        let b = right.request_at(&mut store, "same-op", "right", &left.project.join("repo"))?;
        left.admit(&mut store, &a)?;
        assert!(right.admit(&mut store, &b).is_err());
        assert!(store.operation(right.space.as_str(), "same-op")?.is_none());
        let c = right.request_at(
            &mut store,
            "separate",
            "separate",
            &left.project.join("repo-two"),
        )?;
        right.admit(&mut store, &c)?;
        assert_eq!(store.operations(left.space.as_str())?.len(), 1);
        assert_eq!(store.operations(right.space.as_str())?.len(), 1);
        Ok(())
    }

    #[test]
    fn notifications_budget_and_continuation_claims_are_durable_deduped_state() -> Result<()> {
        let (network, mut store) = setup()?;
        let space = network.space.as_str();
        network.conversation(&mut store, "conversation")?;
        let (_, event) = result(&network, &mut store, "operation")?;
        let notification = Notification {
            notification_id: "notice".into(),
            conversation_id: "conversation".into(),
            kind: "turn_completed".into(),
            text: "awaiting acceptance".into(),
            event_hash: event.hash.clone(),
        };
        store.queue_notification(space, &notification)?;
        store.queue_notification(space, &notification)?;
        assert_eq!(store.pending_notifications(space, "phone")?.len(), 1);
        store.ack_notification(space, "notice", "phone")?;
        assert!(store.pending_notifications(space, "phone")?.is_empty());
        assert_eq!(store.pending_notifications(space, "laptop")?.len(), 1);
        assert_eq!(store.reserve_budget(space, "2026-10-16", 2, 3)?.used, 2);
        assert!(store.reserve_budget(space, "2026-10-16", 2, 3).is_err());
        assert_eq!(store.budget_usage(space, "2026-10-16", 3)?.used, 2);
        let node = network.node.node_id()?;
        store.claim_continuation(space, &event.hash, "v1", &node)?;
        assert!(
            store
                .claim_continuation(space, &event.hash, "v1", &node)
                .is_err()
        );
        store.recover_running()?;
        assert_eq!(
            store.continuation(space, &event.hash, "v1")?.unwrap().state,
            "unknown"
        );
        assert!(
            store
                .claim_continuation(space, &event.hash, "v1", &node)
                .is_err()
        );
        store.finish_continuation(
            space,
            &event.hash,
            "v1",
            &node,
            "completed",
            json!({"next":"ask_user"}),
        )?;
        store.rebuild_projections(space)?;
        assert_eq!(store.pending_notifications(space, "laptop")?.len(), 1);
        Ok(())
    }

    #[test]
    fn replicated_notification_and_ack_rebuild_without_model_or_operation() -> Result<()> {
        let (network, mut source) = setup()?;
        let space = network.space.as_str();
        let mut target = network.memory()?;
        network.conversation(&mut source, "conversation")?;
        let (request, event) = result(&network, &mut source, "operation")?;
        notify(&network, &mut source, &request, &event)?;
        ack(&network, &mut source, "result:operation:succeeded")?;
        target.import_snapshot(space, &source.export_snapshot(space, "conversation")?)?;
        assert!(target.pending_notifications(space, "phone")?.is_empty());
        assert_eq!(target.pending_notifications(space, "laptop")?.len(), 1);
        target.rebuild_projections(space)?;
        assert!(target.pending_notifications(space, "phone")?.is_empty());
        assert!(target.operations(space)?.is_empty());
        Ok(())
    }

    #[test]
    fn replicated_notification_conflicts_converge_and_keep_acknowledgments() -> Result<()> {
        let (network, mut left) = setup()?;
        let space = network.space.as_str();
        let mut right = network.memory()?;
        network.conversation(&mut left, "conversation")?;
        let (request, event) = result(&network, &mut left, "operation")?;
        let notice = notify(&network, &mut left, &request, &event)?;
        right.import_snapshot(space, &left.export_snapshot(space, "conversation")?)?;
        ack(&network, &mut left, "result:operation:succeeded")?;
        right.import_snapshot(space, &left.export_snapshot(space, "conversation")?)?;
        let notice: Event = serde_json::from_str(&notice.canonical_body)?;
        let fork = network.signed_message(
            &mut left,
            "equivocated notification slot",
            notice.sequence,
            notice.previous,
            notice.parents,
        )?;
        left.ingest_event(space, &fork)?;
        right.ingest_event(space, &fork)?;
        assert_eq!(
            left.pending_notifications(space, "laptop")?,
            right.pending_notifications(space, "laptop")?
        );
        assert!(left.pending_notifications(space, "laptop")?.is_empty());
        assert!(left.pending_notifications(space, "phone")?.is_empty());
        assert_eq!(
            left.connection.query_row(
                "SELECT count(*) FROM notification_acks WHERE client_id='phone'",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            1
        );
        Ok(())
    }

    fn running_file() -> Result<(TestNetwork, PathBuf)> {
        let network = TestNetwork::new()?;
        let db = network.directory.join("node.sqlite3");
        let mut store = network.at(&db)?;
        network.conversation(&mut store, "conversation")?;
        let request = network.request(&mut store, "running", "session")?;
        network.admit(&mut store, &request)?;
        network.claim(&mut store, "running")?;
        drop(store);
        Ok((network, db))
    }

    #[test]
    fn incompatible_membership_is_rejected_before_recovery_mutates_operations() -> Result<()> {
        let (_network, db) = running_file()?;
        let connection = Connection::open(&db)?;
        connection.execute("UPDATE spaces SET bundle='{}'", [])?;
        drop(connection);
        assert!(Store::open(&db).is_err());
        let connection = Connection::open(&db)?;
        assert_eq!(
            connection.query_row(
                "SELECT state FROM operations WHERE operation_id='running'",
                [],
                |row| row.get::<_, String>(0)
            )?,
            "\"running\""
        );
        assert_eq!(
            connection.query_row("SELECT count(*) FROM conversations", [], |row| row
                .get::<_, u64>(0))?,
            1
        );
        Ok(())
    }

    #[test]
    fn reopening_keeps_unknown_effects_reserved_and_rejects_future_schema() -> Result<()> {
        let (network, db) = running_file()?;
        let store = Store::open(&db)?;
        assert_eq!(
            store
                .operation(network.space.as_str(), "running")?
                .unwrap()
                .state,
            OperationState::Unknown
        );
        assert_eq!(
            store.connection.query_row(
                "SELECT count(*) FROM resource_reservations",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            1
        );
        assert_eq!(store.conversations(network.space.as_str())?.len(), 1);
        drop(store);
        let connection = Connection::open(&db)?;
        connection.execute_batch("PRAGMA user_version=3")?;
        drop(connection);
        assert!(Store::open(&db).is_err());
        Ok(())
    }

    #[test]
    fn read_only_access_keeps_live_operations_and_projections_unchanged() -> Result<()> {
        let network = TestNetwork::new()?;
        let db = network.directory.join("node.sqlite3");
        let mut live = network.at(&db)?;
        network.conversation(&mut live, "conversation")?;
        let request = network.request(&mut live, "running", "session")?;
        network.admit(&mut live, &request)?;
        network.claim(&mut live, "running")?;
        let access = Store::open_read_only(&db)?;
        assert!(!access.trusted_policy_pins().is_empty());
        assert_eq!(
            access
                .operation(network.space.as_str(), "running")?
                .unwrap()
                .state,
            OperationState::Running
        );
        assert_eq!(access.conversations(network.space.as_str())?.len(), 1);
        assert!(
            access
                .connection
                .execute("DELETE FROM operations", [])
                .is_err()
        );
        drop(access);
        for suffix in ["-wal", "-shm"] {
            assert_eq!(
                fs::metadata(network.directory.join(format!("node.sqlite3{suffix}")))?
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_eq!(
            live.operation(network.space.as_str(), "running")?
                .unwrap()
                .state,
            OperationState::Running
        );
        assert_eq!(
            live.connection
                .query_row("SELECT count(*) FROM resource_reservations", [], |row| row
                    .get::<_, u64>(
                    0
                ))?,
            request.resources.len() as u64
        );
        Ok(())
    }

    #[test]
    fn schema_and_signed_state_are_validated_before_recovery() -> Result<()> {
        for (mutation, structural) in [
            ("DROP TABLE tasks", true),
            ("DROP INDEX events_author_sequence", true),
            ("CREATE TABLE identity_network (state TEXT)", true),
            ("UPDATE events SET envelope='{}'", false),
            ("UPDATE spaces SET bundle='{}'", false),
            ("UPDATE policy_journal SET entry='{}'", false),
            (
                "UPDATE accepted_authorizations SET actor_id='changed'",
                false,
            ),
            ("DELETE FROM authorization_nonces", false),
            (
                "UPDATE local_values SET value=json_set(value,'$.deadline',0) WHERE key LIKE 'lease:%'",
                false,
            ),
        ] {
            let (network, db) = running_file()?;
            let connection = Connection::open(&db)?;
            connection.execute_batch(mutation)?;
            drop(connection);
            let snapshot = || -> Result<BTreeMap<String, (Vec<u8>, u32)>> {
                let mut files = BTreeMap::new();
                for entry in fs::read_dir(&network.directory)? {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        files.insert(
                            entry.file_name().to_string_lossy().into_owned(),
                            (
                                fs::read(entry.path())?,
                                entry.metadata()?.permissions().mode(),
                            ),
                        );
                    }
                }
                Ok(files)
            };
            let before = snapshot()?;
            assert_eq!(Store::verify_existing_database(&db).is_err(), structural);
            assert!(Store::open(&db).is_err());
            if structural {
                assert_eq!(snapshot()?, before);
            }
            let connection = Connection::open(&db)?;
            assert_eq!(
                connection.query_row(
                    "SELECT state FROM operations WHERE operation_id='running'",
                    [],
                    |row| row.get::<_, String>(0)
                )?,
                "\"running\""
            );
        }
        Ok(())
    }

    #[test]
    fn format_verification_uses_committed_wal_without_mutating_the_source() -> Result<()> {
        let network = TestNetwork::new()?;
        let db = network.directory.join("wal-format.sqlite3");
        drop(Store::initialize(&db)?);
        let writer = Connection::open(&db)?;
        writer.pragma_update(None, "wal_autocheckpoint", 0)?;
        writer.execute("UPDATE schema_metadata SET data_epoch=data_epoch-1", [])?;
        let wal = database_sidecar(&db, "-wal");
        assert!(fs::metadata(&wal)?.len() > 32);
        let mut uri = reqwest::Url::from_file_path(&db).unwrap();
        uri.query_pairs_mut().append_pair("immutable", "1");
        let main_only = Connection::open_with_flags(
            uri.as_str(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
        assert_eq!(
            main_only.query_row("SELECT data_epoch FROM schema_metadata", [], |row| row
                .get::<_, u32>(0))?,
            crate::config::DATA_EPOCH
        );
        drop(main_only);
        let snapshot = || -> Result<Vec<(PathBuf, Vec<u8>, u32)>> {
            std::iter::once(db.clone())
                .chain(["-wal", "-shm", "-journal"].map(|suffix| database_sidecar(&db, suffix)))
                .filter(|file| file.exists())
                .map(|file| {
                    Ok((
                        file.clone(),
                        fs::read(&file)?,
                        fs::metadata(file)?.permissions().mode(),
                    ))
                })
                .collect()
        };
        let before = snapshot()?;
        assert!(Store::verify_existing_database(&db).is_err());
        assert_eq!(snapshot()?, before);
        assert!(Store::open(&db).is_err());
        assert_eq!(snapshot()?, before);
        assert!(Store::initialize(&db).is_err());
        assert_eq!(snapshot()?, before);
        writer.execute(
            "UPDATE schema_metadata SET data_epoch=?",
            [crate::config::DATA_EPOCH],
        )?;
        writer.execute(
            "INSERT INTO identity_meta(key,value) VALUES('wal-fixture','committed')",
            [],
        )?;
        let before = snapshot()?;
        Store::verify_existing_database(&db)?;
        assert_eq!(snapshot()?, before);
        let view = database_read_view(&db)?;
        assert_eq!(
            view.connection.query_row(
                "SELECT value FROM identity_meta WHERE key='wal-fixture'",
                [],
                |row| row.get::<_, String>(0)
            )?,
            "committed"
        );
        drop(view);
        assert_eq!(snapshot()?, before);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn resource_normalization_follows_symlinks_before_parent_components() -> Result<()> {
        let network = TestNetwork::new()?;
        let root = &network.project;
        fs::create_dir_all(root.join("real/child"))?;
        std::os::unix::fs::symlink(root.join("real/child"), root.join("link"))?;
        assert_eq!(
            canonical_resource(root.join("link/..").to_str().unwrap())?,
            root.join("real").canonicalize()?.to_str().unwrap()
        );
        assert!(canonical_resource(root.join("absent/../other").to_str().unwrap()).is_err());
        Ok(())
    }
}
