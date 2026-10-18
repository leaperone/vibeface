use crate::{
    crypto::{LocalIdentity, digest},
    model::*,
    scheduling::*,
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

pub(crate) mod bots;
pub(crate) mod followup;

const INITIAL_SCHEMA: &str = include_str!("../migrations/202610180001_initial.sql");

pub struct Store {
    pub(crate) connection: Connection,
    pub(crate) policy: crate::identity::journal::PolicyState,
    pub(crate) target_recovery_import:
        Option<crate::identity::cross_node_recovery::TargetRecoveryImport>,
    pub(crate) account_node_enrollment_import:
        Option<vibeface_secure::authorization::bootstrap::recovery::node_enrollment::AccountNodeEnrollmentImport>,
    pub(crate) foreign_execution_policy:
        crate::identity::foreign_execution_policy::ForeignExecutionPolicyCache,
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

fn validate_persisted_state(
    connection: &Connection,
) -> Result<crate::identity::journal::PolicyState> {
    let snapshot = if connection.is_autocommit() {
        Some(connection.unchecked_transaction()?)
    } else {
        None
    };
    let mut policy = crate::identity::journal::PolicyState::load(connection)?;
    let local_keys = connection.prepare(
        "SELECT DISTINCT d.device_id,d.public_key,n.public_key FROM local_devices d JOIN node_certificates n ON n.space_id=d.space_id AND n.node_id=d.device_id ORDER BY d.public_key,d.device_id",
    )?.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (device, public_key, certificate_key) in local_keys {
        ensure!(
            public_key == certificate_key && crate::crypto::fingerprint(&public_key)? == device,
            "local device key differs from its stored certificate"
        );
        let source = vibeface_secure::authorization::PublicKey::parse(&public_key)?;
        crate::identity::foreign_execution_policy::rehydrate_task_execution_policy_in(
            connection,
            &mut policy,
            &source,
        )?;
        crate::identity::gateway_forward::rehydrate_gateway_sources_in(
            connection,
            &mut policy,
            &source,
            crate::runtime::now_ms() as u64,
        )?;
    }
    crate::identity::foreign_execution_policy::validate_task_execution_policy_links_in(
        connection, &policy,
    )?;
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
        load_events_from(connection, &policy, &space)?;
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
    let checkpoints =
        crate::identity::admission::verified_admission_checkpoints_from(connection, &policy)?;
    crate::identity::enrollment_store::validate_enrollment_storage(connection, &policy)?;
    crate::identity::execution_handoff::validate_execution_handoff_storage(connection, &policy)?;
    policy.local_admissions = checkpoints;
    if let Some(snapshot) = snapshot {
        snapshot.commit()?;
    }
    Ok(policy)
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
        let policy = validate_persisted_state(&connection)?;
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
        Ok(Self {
            connection,
            policy,
            target_recovery_import: None,
            account_node_enrollment_import: None,
            foreign_execution_policy: Default::default(),
        })
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
        let policy = validate_persisted_state(&connection)?;
        if !memory {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            private_sidecars(path)?;
        }
        let mut store = Self {
            connection,
            policy,
            target_recovery_import: None,
            account_node_enrollment_import: None,
            foreign_execution_policy: Default::default(),
        };
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
        _role: Option<NodeRole>,
    ) -> Result<NodeCertificate> {
        let snapshot = if self.connection.is_autocommit() {
            Some(self.connection.unchecked_transaction()?)
        } else {
            None
        };
        let policy =
            crate::identity::journal::PolicyState::reload_verified(&self.connection, &self.policy)?;
        let certificate =
            authorize_verified_node_from(&self.connection, policy.verified(), network_id, node_id)?;
        if let Some(snapshot) = snapshot {
            snapshot.commit()?;
        }
        Ok(certificate)
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
            &self.connection,
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
        rebuild_projections_from(&tx, &self.policy, network_id)?;
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
    pub fn enqueue_trigger(
        &mut self,
        network: &str,
        conversation: &str,
        trigger: &str,
        target: &str,
    ) -> Result<AdmissionReceipt> {
        self.admit_trigger(network, conversation, trigger, target, target)
    }

    pub fn admit_trigger(
        &mut self,
        network: &str,
        conversation: &str,
        trigger: &str,
        local_node: &str,
        target: &str,
    ) -> Result<AdmissionReceipt> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let receipt = admit_scheduler_from(
            &tx,
            &self.policy,
            network,
            conversation,
            trigger,
            local_node,
            target,
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    pub(crate) fn next_scheduler_candidate(
        &mut self,
        space: &str,
        conversation: &str,
        target: &str,
        boot: &str,
        skip: &BTreeSet<String>,
    ) -> Result<SchedulerSelection> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = crate::identity::journal::PolicyState::reload_verified(&tx, &self.policy)?;
        let mut session = load_scheduler_session(&tx, space, conversation, target)?;
        recover_scheduler_session(&tx, space, &mut session, boot)?;
        let (selection, _) = select_scheduler_candidate_in(
            &tx,
            &current,
            space,
            &mut session,
            skip,
            None,
            crate::runtime::now_ms() as u64,
        )?;
        save_scheduler_session(&tx, space, &session)?;
        tx.commit()?;
        Ok(selection)
    }

    pub fn scheduler_job(
        &self,
        network: &str,
        conversation: &str,
        trigger: &str,
        target: &str,
    ) -> Result<Option<SchedulerJob>> {
        scheduler_load(
            &self.connection,
            network,
            &scheduler_key("job", conversation, target, trigger),
        )
    }

    pub fn scheduler_status(
        &self,
        network: &str,
        conversation: &str,
        trigger: &str,
        target: &str,
    ) -> Result<Option<AdmissionReceipt>> {
        if let Some(job) = self.scheduler_job(network, conversation, trigger, target)? {
            return Ok(Some(job.receipt(network)));
        }
        Ok(scheduler_load::<OutboxEntry>(
            &self.connection,
            network,
            &scheduler_key("outbox", conversation, target, trigger),
        )?
        .map(|entry| entry.receipt()))
    }

    pub fn pending_scheduler_conversations(
        &self,
        network: &str,
        target: &str,
    ) -> Result<Vec<String>> {
        Ok(scheduler_sessions(&self.connection, network)?
            .into_iter()
            .filter(|s| s.target_node == target && !s.pending.is_empty())
            .map(|s| s.conversation_id)
            .collect())
    }

    pub fn scheduler_jobs(
        &self,
        network: &str,
        conversation: &str,
        target: &str,
    ) -> Result<Vec<SchedulerJob>> {
        let mut statement = self.connection.prepare(
            "SELECT value FROM local_values WHERE space_id=? AND key LIKE 'scheduler:v1:job:%'",
        )?;
        let rows = statement.query_map([network], |r| r.get::<_, String>(0))?;
        let mut jobs = Vec::new();
        for row in rows {
            let job: SchedulerJob = serde_json::from_str(&row?)?;
            ensure!(
                job.version == SCHEDULING_VERSION,
                "unsupported scheduler job version"
            );
            if job.conversation_id == conversation && job.target_node == target {
                ensure!(
                    job.resource_id
                        == conversation_resource_in(&self.connection, network, conversation)?,
                    "scheduler job belongs to another resource"
                );
                jobs.push(job);
            }
        }
        jobs.sort_by_key(|job| job.sequence);
        Ok(jobs)
    }

    pub fn scheduler_outbox(&self, network: &str) -> Result<Vec<OutboxEntry>> {
        Ok(self
            .scheduler_outbox_all(network)?
            .into_iter()
            .filter(|entry| !entry.delivered)
            .collect())
    }

    pub fn scheduler_outbox_all(&self, network: &str) -> Result<Vec<OutboxEntry>> {
        let mut statement = self.connection.prepare(
            "SELECT value FROM local_values WHERE space_id=? AND key LIKE 'scheduler:v1:outbox:%'",
        )?;
        let rows = statement.query_map([network], |r| r.get::<_, String>(0))?;
        let mut entries = Vec::new();
        for row in rows {
            let entry: OutboxEntry = serde_json::from_str(&row?)?;
            ensure!(
                entry.version == SCHEDULING_VERSION,
                "unsupported scheduler outbox version"
            );
            entries.push(entry);
        }
        Ok(entries)
    }

    pub fn acknowledge_outbox(
        &mut self,
        network: &str,
        receipt: &AdmissionReceipt,
    ) -> Result<AdmissionReceipt> {
        ensure!(
            receipt.version == SCHEDULING_VERSION && receipt.space_id == network,
            "incompatible scheduling acknowledgement"
        );
        ensure!(
            matches!(
                receipt.state.as_str(),
                "queued"
                    | "claimed"
                    | "running"
                    | "cancelling"
                    | "completed"
                    | "failed"
                    | "cancelled"
                    | "unknown"
            ),
            "acknowledgement does not prove durable admission"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = scheduler_key(
            "outbox",
            &receipt.conversation_id,
            &receipt.target_node,
            &receipt.trigger,
        );
        let mut entry: OutboxEntry =
            scheduler_load(&tx, network, &key)?.context("scheduling outbox identity not found")?;
        ensure!(
            entry.space_id == receipt.space_id
                && entry.resource_id == receipt.resource_id
                && entry.conversation_id == receipt.conversation_id
                && entry.target_node == receipt.target_node
                && entry.trigger == receipt.trigger,
            "scheduling acknowledgement identity mismatch"
        );
        entry.delivered = true;
        scheduler_save(&tx, network, &key, &entry)?;
        tx.commit()?;
        Ok(entry.receipt())
    }

    pub fn recover_scheduler_jobs(
        &mut self,
        network: &str,
        target: &str,
        boot: &str,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for mut session in scheduler_sessions(&tx, network)?
            .into_iter()
            .filter(|s| s.target_node == target)
        {
            recover_scheduler_session(&tx, network, &mut session, boot)?;
            save_scheduler_session(&tx, network, &session)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mark_started(
        &mut self,
        space: &str,
        conversation: &str,
        target: &str,
        trigger: &str,
        attempt_id: &str,
    ) -> Result<SchedulerJob> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        crate::identity::admission::validate_manager_start_in(
            &tx,
            &self.policy,
            space,
            trigger,
            attempt_id,
            crate::runtime::now_ms() as u64,
        )?;
        let key = scheduler_key("job", conversation, target, trigger);
        let mut job: SchedulerJob =
            scheduler_load(&tx, space, &key)?.context("manager job missing")?;
        let JobState::Claimed { attempt } = &job.state else {
            bail!("manager job is not claimed")
        };
        ensure!(attempt.id == attempt_id, "manager attempt changed");
        job.state = JobState::Running {
            attempt: attempt.clone(),
        };
        scheduler_save(&tx, space, &key, &job)?;
        tx.commit()?;
        Ok(job)
    }

    pub fn save_scheduler_native_id(
        &mut self,
        network: &str,
        conversation: &str,
        target: &str,
        trigger: &str,
        attempt_id: &str,
        native_id: &str,
    ) -> Result<()> {
        self.update_scheduler_attempt(
            network,
            conversation,
            target,
            trigger,
            attempt_id,
            |job, session| {
                ensure!(
                    matches!(
                        job.state,
                        JobState::Running { .. } | JobState::Claimed { .. }
                    ),
                    "scheduler attempt is no longer active"
                );
                if !job
                    .state
                    .attempt()
                    .context("scheduler attempt missing")?
                    .read_only
                {
                    session.native_id = Some(native_id.into());
                }
                Ok(())
            },
        )?;
        Ok(())
    }

    pub fn request_manager_cancel(
        &mut self,
        network: &str,
        conversation: &str,
        target: &str,
        trigger: &str,
    ) -> Result<Option<SchedulerJob>> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = load_scheduler_session(&tx, network, conversation, target)?;
        let key = scheduler_key("job", conversation, target, trigger);
        let Some(mut job): Option<SchedulerJob> = scheduler_load(&tx, network, &key)? else {
            return Ok(None);
        };
        if scheduler_publication_from(&tx, network, &job)?.is_some() {
            return Ok(Some(job));
        }
        if matches!(job.state, JobState::Queued) {
            job.state = JobState::Cancelled {
                attempt: None,
                evidence: "user cancelled before the scheduler claimed this trigger".into(),
            };
            session.pending.retain(|pending| pending != trigger);
            scheduler_save(&tx, network, &key, &job)?;
            save_scheduler_session(&tx, network, &session)?;
        } else if let JobState::Claimed { attempt } | JobState::Running { attempt } = &job.state {
            ensure!(
                session.active.as_deref() == Some(trigger),
                "management reply changed; refresh before stopping"
            );
            job.state = JobState::Cancelling {
                attempt: attempt.clone(),
            };
            session.permission_version = session
                .permission_version
                .checked_add(1)
                .context("scheduler permission version overflow")?;
            session.native_id = None;
            scheduler_save(&tx, network, &key, &job)?;
            save_scheduler_session(&tx, network, &session)?;
        }
        tx.commit()?;
        Ok(Some(job))
    }

    pub(crate) fn fence_manager_permission(
        &mut self,
        network: &str,
        conversation: &str,
        target: &str,
        trigger: &str,
        attempt_id: &str,
    ) -> Result<()> {
        self.update_scheduler_attempt(
            network,
            conversation,
            target,
            trigger,
            attempt_id,
            |job, session| {
                let attempt = job.state.attempt().context("manager attempt missing")?;
                if attempt.permission_version == session.permission_version {
                    session.permission_version = session
                        .permission_version
                        .checked_add(1)
                        .context("scheduler permission version overflow")?;
                }
                Ok(())
            },
        )?;
        Ok(())
    }

    pub fn manager_attempt_allowed(
        &self,
        network: &str,
        conversation: &str,
        target: &str,
        trigger: &str,
        attempt_id: &str,
        write: bool,
    ) -> Result<bool> {
        let session = load_scheduler_session(&self.connection, network, conversation, target)?;
        let Some(job) = self.scheduler_job(network, conversation, trigger, target)? else {
            return Ok(false);
        };
        let Some(attempt) = job.state.attempt() else {
            return Ok(false);
        };
        if scheduler_publication_from(&self.connection, network, &job)?.is_some() {
            return Ok(false);
        }

        Ok(session.active.as_deref() == Some(trigger)
            && attempt.id == attempt_id
            && attempt.permission_version == session.permission_version
            && matches!(
                job.state,
                JobState::Claimed { .. } | JobState::Running { .. }
            )
            && (!write || (!attempt.read_only && session.unresolved.is_empty())))
    }

    pub fn scheduler_publication_pending(
        &self,
        network: &str,
        conversation: &str,
        target: &str,
        trigger: &str,
    ) -> Result<bool> {
        let Some(job) = self.scheduler_job(network, conversation, trigger, target)? else {
            return Ok(false);
        };
        Ok(scheduler_publication_from(&self.connection, network, &job)?.is_some())
    }

    pub fn fence_manager_unknown(
        &mut self,
        network: &str,
        conversation: &str,
        target: &str,
        trigger: &str,
        attempt_id: &str,
    ) -> Result<SchedulerJob> {
        self.update_scheduler_attempt(
            network,
            conversation,
            target,
            trigger,
            attempt_id,
            |job, session| {
                let JobState::Unknown { fenced, .. } = &mut job.state else {
                    bail!("scheduler job is not unknown");
                };
                *fenced = true;
                if session.active.as_deref() == Some(&job.trigger) {
                    session.active = None;
                }
                session.native_id = None;
                Ok(())
            },
        )
    }

    pub fn resolve_manager_unknown(
        &mut self,
        network: &str,
        conversation: &str,
        target: &str,
        trigger: &str,
        attempt_id: &str,
        evidence: &str,
    ) -> Result<()> {
        ensure!(
            !evidence.is_empty(),
            "manager reconciliation evidence is required"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = load_scheduler_session(&tx, network, conversation, target)?;
        let job: SchedulerJob = scheduler_load(
            &tx,
            network,
            &scheduler_key("job", conversation, target, trigger),
        )?
        .context("scheduler job missing")?;
        ensure!(
            matches!(job.state, JobState::Unknown { fenced: true, .. })
                && job
                    .state
                    .attempt()
                    .is_some_and(|attempt| attempt.id == attempt_id),
            "unknown scheduler tools have not been fenced or attempt does not match"
        );
        scheduler_save(
            &tx,
            network,
            &scheduler_key("resolution", conversation, target, trigger),
            &serde_json::json!({"attempt_id":attempt_id,"evidence":evidence}),
        )?;
        session.unresolved.remove(trigger);
        session.native_id = None;
        save_scheduler_session(&tx, network, &session)?;
        tx.commit()?;
        Ok(())
    }

    pub fn scheduler_maintenance_idle(&self, network: &str) -> Result<()> {
        for session in scheduler_sessions(&self.connection, network)? {
            ensure!(
                session.pending.is_empty()
                    && session.active.is_none()
                    && session.unresolved.is_empty(),
                crate::maintenance::Busy(
                    "scheduler invocation requires completion or reconciliation".into()
                )
            );
        }
        ensure!(
            self.scheduler_outbox(network)?.is_empty(),
            crate::maintenance::Busy("scheduler delivery is pending".into())
        );
        Ok(())
    }

    fn update_scheduler_attempt(
        &mut self,
        network: &str,
        conversation: &str,
        target: &str,
        trigger: &str,
        attempt_id: &str,
        update: impl FnOnce(&mut SchedulerJob, &mut SchedulerSession) -> Result<()>,
    ) -> Result<SchedulerJob> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = load_scheduler_session(&tx, network, conversation, target)?;
        let key = scheduler_key("job", conversation, target, trigger);
        let mut job: SchedulerJob =
            scheduler_load(&tx, network, &key)?.context("scheduler job missing")?;
        ensure!(
            job.state
                .attempt()
                .is_some_and(|attempt| attempt.id == attempt_id),
            "scheduler attempt does not match"
        );
        update(&mut job, &mut session)?;
        scheduler_save(&tx, network, &key, &job)?;
        save_scheduler_session(&tx, network, &session)?;
        tx.commit()?;
        Ok(job)
    }

    pub(crate) fn admit_manager_control(
        &mut self,
        auth: &vibeface_secure::authorization::VerifiedAuthorization,
        freshness: &crate::identity::policy::FreshnessProof,
        transport: &crate::identity::policy::RequestTransport,
        body: &Value,
        now: u64,
    ) -> Result<SchedulerJob> {
        use vibeface_secure::authorization::{Action, Actor};
        ensure!(
            matches!(auth.actor(), Actor::Account(_)),
            "manager control requires an Account"
        );
        let method = body["method"]
            .as_str()
            .context("manager control method is required")?;
        ensure!(
            matches!(
                (method, auth.action()),
                ("cancel_manager", Action::AgentInterrupt)
                    | ("resolve_manager_unknown", Action::OperationResolve)
            ),
            "manager control action mismatch"
        );
        let space = auth.resource().space.as_str();
        let scope = body["conversation_id"]
            .as_str()
            .context("conversation_id is required")?;
        let target = body["target_node"]
            .as_str()
            .context("target_node is required")?;
        let trigger = body["trigger"].as_str().context("trigger is required")?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let receipt_key = format!("manager-control:{}", auth.operation_id());
        let admission = crate::identity::admission::consume_request_authorization_in(
            &tx,
            &self.policy,
            auth,
            freshness,
            transport,
            body,
            None,
            now,
        )?;
        if let Some(receipt) = scheduler_load::<ManagerControlReceipt>(&tx, space, &receipt_key)? {
            ensure!(
                receipt.admission == admission,
                "manager control retry changed its original authorization"
            );
            tx.commit()?;
            return Ok(receipt.job);
        }
        let key = scheduler_key("job", scope, target, trigger);
        let mut job: SchedulerJob =
            scheduler_load(&tx, space, &key)?.context("scheduler job missing")?;
        ensure!(
            job.resource_id == auth.resource().resource,
            "manager control belongs to another Project"
        );
        let actual = job.state.attempt().map(|attempt| attempt.id.clone());
        ensure!(
            body["attempt_id"].as_str() == actual.as_deref(),
            "manager control attempt changed"
        );
        let mut session = load_scheduler_session(&tx, space, scope, target)?;
        if method == "cancel_manager" {
            if scheduler_publication_from(&tx, space, &job)?.is_none() {
                match &job.state {
                    JobState::Queued => {
                        job.state = JobState::Cancelled {
                            attempt: None,
                            evidence: "Account cancelled before SDK start".into(),
                        };
                        session.pending.retain(|value| value != trigger);
                    }
                    JobState::Claimed { attempt } | JobState::Running { attempt } => {
                        ensure!(
                            session.active.as_deref() == Some(trigger),
                            "manager active attempt changed"
                        );
                        job.state = JobState::Cancelling {
                            attempt: attempt.clone(),
                        };
                        session.permission_version = session
                            .permission_version
                            .checked_add(1)
                            .context("permission version exhausted")?;
                        session.native_id = None;
                    }
                    _ => {}
                }
            }
        } else {
            ensure!(
                matches!(job.state, JobState::Unknown { fenced: true, .. }),
                "unknown manager must be fenced before resolution"
            );
            let evidence = body["evidence"]
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .context("reconciliation evidence is required")?;
            scheduler_save(
                &tx,
                space,
                &scheduler_key("resolution", scope, target, trigger),
                &serde_json::json!({"attempt_id":actual,"evidence":evidence,"admission":admission}),
            )?;
            session.unresolved.remove(trigger);
            session.native_id = None;
        }
        scheduler_save(&tx, space, &key, &job)?;
        save_scheduler_session(&tx, space, &session)?;
        scheduler_save(
            &tx,
            space,
            &receipt_key,
            &ManagerControlReceipt {
                admission,
                job: job.clone(),
            },
        )?;
        tx.commit()?;
        Ok(job)
    }

    pub(crate) fn accept_message_authorized(
        &mut self,
        identity: &LocalIdentity,
        auth: &vibeface_secure::authorization::VerifiedAuthorization,
        freshness: &crate::identity::policy::FreshnessProof,
        transport: &crate::identity::policy::RequestTransport,
        body: &Value,
        now: u64,
    ) -> Result<(SignedEvent, AdmissionReceipt)> {
        use vibeface_secure::authorization::Actor;
        ensure!(
            matches!(auth.actor(), Actor::Account(_)) && body["method"] == "message",
            "message requires an Account admission"
        );
        let space = auth.resource().space.as_str();
        let scope = body["conversation_id"]
            .as_str()
            .context("conversation_id is required")?;
        let target = body["scheduler_node"]
            .as_str()
            .context("signed scheduler_node is required")?;
        ensure!(
            conversation_resource_in(&self.connection, space, scope)? == auth.resource().resource,
            "message resource mismatch"
        );
        let event = EventBody::MessagePosted {
            message_id: body["message_id"]
                .as_str()
                .context("message_id is required")?
                .into(),
            role: "user".into(),
            text: body["text"].as_str().context("text is required")?.into(),
            scheduler_node: Some(target.into()),
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior = load_events_from(&tx, &self.policy, space)?.into_values().find(|prior| prior.event.scope_id == scope
            && matches!(&prior.event.body, EventBody::MessagePosted { message_id, .. } if body["message_id"].as_str() == Some(message_id)));
        if let Some(prior) = prior {
            ensure!(
                prior.status == "ready"
                    && prior.event.resource_id == auth.resource().resource
                    && prior.event.body == event
                    && prior.event.blobs.is_empty(),
                "message ID reused with different content or target"
            );
            let EventCause::Invocation { admission } = &prior.event.cause else {
                bail!("message has no Account admission")
            };
            ensure!(
                admission.invocation.payload.actor == *auth.actor()
                    && admission.source_body["report_results"] == body["report_results"],
                "message retry changed its Account or result reporting"
            );
            let receipt = admit_scheduler_from(
                &tx,
                &self.policy,
                space,
                scope,
                &prior.envelope.hash,
                &identity.node_id()?,
                target,
            )?;
            tx.commit()?;
            return Ok((prior.envelope, receipt));
        }
        let admitted = crate::identity::admission::consume_request_authorization_in(
            &tx,
            &self.policy,
            auth,
            freshness,
            transport,
            body,
            None,
            now,
        )?;
        let mut events = crate::identity::event_authorization::append_admitted_events_in(
            &tx,
            &self.policy,
            identity,
            &admitted,
            vec![(scope.into(), event, vec![])],
            now,
        )?;
        let envelope = events.remove(0);
        let receipt = admit_scheduler_from(
            &tx,
            &self.policy,
            space,
            scope,
            &envelope.hash,
            &identity.node_id()?,
            target,
        )?;
        let checkpoint = crate::identity::admission::prepare_local_admission_checkpoint_in(
            &tx,
            &self.policy,
            &admitted,
        )?;
        tx.commit()?;
        self.policy.local_admissions.insert(checkpoint);
        Ok((envelope, receipt))
    }

    pub(crate) fn retain_scheduler_result(
        &mut self,
        space: &str,
        job: &SchedulerJob,
        output: &str,
        blobs: &[BlobRef],
    ) -> Result<()> {
        let key = scheduler_key(
            "known-result",
            &job.conversation_id,
            &job.target_node,
            &job.trigger,
        );
        let value = SchedulerKnownResult {
            job: job.clone(),
            output: output.into(),
            blobs: blobs.into(),
        };
        if let Some(old) = scheduler_load::<SchedulerKnownResult>(&self.connection, space, &key)? {
            ensure!(
                old.job.state.attempt().map(|a| &a.id) == job.state.attempt().map(|a| &a.id)
                    && old.output == output
                    && old.blobs == blobs,
                "known manager output changed"
            );
        } else {
            scheduler_save(&self.connection, space, &key, &value)?;
        }
        Ok(())
    }
    pub(crate) fn pending_scheduler_results(
        &self,
        space: &str,
    ) -> Result<Vec<SchedulerKnownResult>> {
        let mut statement=self.connection.prepare("SELECT value FROM local_values WHERE space_id=? AND key LIKE 'scheduler:v1:known-result:%'")?;
        statement
            .query_map([space], |row| row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect()
    }

    pub(crate) fn stage_scheduler_terminal(
        &mut self,
        identity: &LocalIdentity,
        auth: &vibeface_secure::authorization::VerifiedAuthorization,
        freshness: &crate::identity::policy::FreshnessProof,
        transport: &crate::identity::policy::RequestTransport,
        body: &Value,
        job: &SchedulerJob,
        outcome: JobOutcome,
        events: Vec<(EventBody, Vec<BlobRef>)>,
        now: u64,
    ) -> Result<SchedulerJob> {
        let space = auth.resource().space.as_str();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: SchedulerJob = scheduler_load(
            &tx,
            space,
            &scheduler_key("job", &job.conversation_id, &job.target_node, &job.trigger),
        )?
        .context("scheduler job missing")?;
        ensure!(
            &current == job && auth.resource().resource == job.resource_id,
            "scheduler terminal changed its job or resource"
        );
        validate_scheduler_terminal(job, &outcome, &events)?;
        if scheduler_publication_from(&tx, space, job)?.is_none() {
            let admission = crate::identity::admission::consume_request_authorization_in(
                &tx,
                &self.policy,
                auth,
                freshness,
                transport,
                body,
                None,
                now,
            )?;
            let marker = SchedulerTerminalPublication {
                version: SCHEDULING_VERSION,
                job: job.clone(),
                outcome,
                events,
                admission,
                created_at_ms: i64::try_from(now)?,
            };
            scheduler_save(
                &tx,
                space,
                &scheduler_key(
                    "publication",
                    &job.conversation_id,
                    &job.target_node,
                    &job.trigger,
                ),
                &marker,
            )?;
        }
        tx.commit()?;
        self.flush_scheduler_publications(identity, space, &job.target_node)?;
        self.scheduler_job(space, &job.conversation_id, &job.trigger, &job.target_node)?
            .context("scheduler job disappeared")
    }

    pub(crate) fn flush_scheduler_publications(
        &mut self,
        identity: &LocalIdentity,
        space: &str,
        target: &str,
    ) -> Result<SchedulerPublicationBatch> {
        ensure!(
            identity.node_id()? == target,
            "scheduler publication belongs to another Node"
        );
        let values = self.connection.prepare("SELECT value FROM local_values WHERE space_id=? AND key LIKE 'scheduler:v1:publication:%'")?
            .query_map([space], |row| row.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
        let mut batch = SchedulerPublicationBatch::default();
        for value in values {
            let marker: SchedulerTerminalPublication = serde_json::from_str(&value)?;
            if marker.job.target_node != target {
                continue;
            }
            let result = (|| -> Result<Option<SchedulerPublication>> {
                let tx = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                if !scheduler_scope_publishable(
                    &tx,
                    &self.policy,
                    space,
                    &marker.job.conversation_id,
                )? {
                    return Ok(None);
                }
                let current: SchedulerJob = scheduler_load(
                    &tx,
                    space,
                    &scheduler_key(
                        "job",
                        &marker.job.conversation_id,
                        target,
                        &marker.job.trigger,
                    ),
                )?
                .context("scheduler job missing")?;
                ensure!(
                    current == marker.job,
                    "scheduler publication refers to another attempt"
                );
                let signed = crate::identity::event_authorization::append_admitted_events_in(
                    &tx,
                    &self.policy,
                    identity,
                    &marker.admission,
                    marker
                        .events
                        .iter()
                        .cloned()
                        .map(|(body, blobs)| (marker.job.conversation_id.clone(), body, blobs))
                        .collect(),
                    marker.admission.admitted_at_ms,
                )?;
                let job = finish_scheduler_in(&tx, space, &current, &marker.outcome)?;
                tx.execute(
                    "DELETE FROM local_values WHERE space_id=? AND key=?",
                    params![
                        space,
                        scheduler_key(
                            "known-result",
                            &job.conversation_id,
                            &job.target_node,
                            &job.trigger
                        )
                    ],
                )?;
                tx.execute(
                    "DELETE FROM local_values WHERE space_id=? AND key=?",
                    params![
                        space,
                        scheduler_key("publication", &job.conversation_id, target, &job.trigger)
                    ],
                )?;
                let checkpoint = crate::identity::admission::prepare_local_admission_checkpoint_in(
                    &tx,
                    &self.policy,
                    &marker.admission,
                )?;
                tx.commit()?;
                self.policy.local_admissions.insert(checkpoint);
                Ok(Some(SchedulerPublication {
                    job,
                    event_hashes: signed.into_iter().map(|event| event.hash).collect(),
                }))
            })();
            match result {
                Ok(Some(value)) => batch.published.push(value),
                Ok(None) => {}
                Err(error) => batch.errors.push(format!(
                    "{}:{}: {error:#}",
                    marker.job.conversation_id, marker.job.trigger
                )),
            }
        }
        Ok(batch)
    }

    pub(crate) fn finish_scheduler_locally(
        &mut self,
        space: &str,
        job: &SchedulerJob,
        outcome: JobOutcome,
    ) -> Result<SchedulerJob> {
        ensure!(
            !matches!(outcome, JobOutcome::Completed),
            "completed scheduler output requires an exact admitted publication"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: SchedulerJob = scheduler_load(
            &tx,
            space,
            &scheduler_key("job", &job.conversation_id, &job.target_node, &job.trigger),
        )?
        .context("scheduler job missing")?;
        ensure!(
            current.state.attempt().map(|a| &a.id) == job.state.attempt().map(|a| &a.id),
            "scheduler attempt changed"
        );
        let job = finish_scheduler_in(&tx, space, &current, &outcome)?;
        tx.commit()?;
        Ok(job)
    }
    fn load_events(&self, network_id: &str) -> Result<BTreeMap<String, StoredEvent>> {
        load_events_from(&self.connection, &self.policy, network_id)
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
        rebuild_projections_from(&tx, &self.policy, network_id)?;
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
        rebuild_projections_from(&tx, &self.policy, network_id)?;
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
        let snapshot = if self.connection.is_autocommit() {
            Some(self.connection.unchecked_transaction()?)
        } else {
            None
        };
        let rows = self
            .connection
            .prepare("SELECT envelope,resource_id FROM events WHERE space_id=? AND scope_id=? ORDER BY hash")?
            .query_map(params![network, scope], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut creation = None;
        let mut resource = None;
        for (encoded, indexed_resource) in rows {
            let envelope: SignedEvent = serde_json::from_str(&encoded)?;
            let event: Event = serde_json::from_str(&envelope.canonical_body)?;
            ensure!(
                event.network_id == network
                    && event.scope_id == scope
                    && event.resource_id.as_str() == indexed_resource
                    && resource.as_ref().is_none_or(|id| id == &event.resource_id),
                "task history belongs to another space, scope or resource"
            );
            resource = Some(event.resource_id);
            if matches!(&event.body, EventBody::TaskCreated { task_id: id, .. } if id == task_id) {
                ensure!(creation.is_none(), "task creation is ambiguous");
                creation = Some(envelope);
            }
        }
        let creation = creation.context("task creation is missing")?;
        crate::identity::event_authorization::validate_retained_event_scope_in(
            &self.connection,
            &creation,
        )?;
        if let Some(snapshot) = snapshot {
            snapshot.commit()?;
        }
        Ok(creation)
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
        let snapshot = if self.connection.is_autocommit() {
            Some(self.connection.unchecked_transaction()?)
        } else {
            None
        };
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
        crate::identity::event_authorization::validate_retained_event_dependencies_in(
            &self.connection,
            &envelope,
        )?;
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
        if let Some(snapshot) = snapshot {
            snapshot.commit()?;
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
        self.bind_task_request_authorized(
            identity,
            auth,
            proof,
            &crate::identity::policy::RequestTransport::Existing(transport.clone()),
            source_body,
            now_ms,
        )
    }

    pub(crate) fn bind_task_request_authorized(
        &mut self,
        identity: &LocalIdentity,
        auth: &vibeface_secure::authorization::VerifiedAuthorization,
        proof: &crate::identity::policy::FreshnessProof,
        transport: &crate::identity::policy::RequestTransport,
        source_body: &Value,
        now_ms: u64,
    ) -> Result<crate::binding::SignedBinding> {
        transport.check_current(auth, now_ms)?;
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
        let checked = crate::identity::policy::authorize_request_invocation_for_body(
            self.policy.verified(),
            auth.signed_invocation(),
            &facts,
            transport,
            source_body,
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
        crate::identity::admission::check_policy_heads_in(
            &tx,
            self.policy.verified(),
            &checked,
            now_ms,
        )?;
        crate::identity::admission::check_request_manager_tool_in(
            &tx,
            &self.policy,
            &checked,
            transport,
            now_ms,
        )?;
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
        crate::identity::admission::consume_request_authorization_in(
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
        if let Some(proof) = &request.binding {
            self.verify_binding_creation(proof)?;
        }
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
pub(crate) fn scheduler_publication_from(
    connection: &Connection,
    network: &str,
    job: &SchedulerJob,
) -> Result<Option<SchedulerTerminalPublication>> {
    let marker: Option<SchedulerTerminalPublication> = scheduler_load(
        connection,
        network,
        &scheduler_key(
            "publication",
            &job.conversation_id,
            &job.target_node,
            &job.trigger,
        ),
    )?;
    if let Some(marker) = &marker {
        ensure!(
            marker.version == SCHEDULING_VERSION && marker.job == *job,
            "scheduler publication does not match its exact invocation"
        );
    }
    Ok(marker)
}

fn validate_scheduler_terminal(
    job: &SchedulerJob,
    outcome: &JobOutcome,
    events: &[(EventBody, Vec<BlobRef>)],
) -> Result<()> {
    match outcome {
        JobOutcome::Completed => {
            ensure!(
                !matches!(job.state, JobState::Cancelling { .. }),
                "cancelled manager credentials cannot complete a reply"
            );
            ensure!(events.iter().filter(|(body, _)| matches!(body, EventBody::MessagePosted { message_id, role, .. } if message_id == &job.message_id && role == "assistant")).count() == 1, "completed scheduler job requires one final reply");
            ensure!(
                events
                    .iter()
                    .filter(|(body, _)| matches!(body, EventBody::MessagePosted { .. }))
                    .count()
                    == 1,
                "completed scheduler job cannot publish additional replies"
            );
        }
        JobOutcome::Cancelled(evidence) => {
            ensure!(!evidence.is_empty(), "cancellation evidence is required")
        }
        _ => {}
    }
    Ok(())
}

fn scheduler_scope_publishable(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    network: &str,
    conversation: &str,
) -> Result<bool> {
    let events = load_events_from(connection, policy, network)?;
    for hash in closure_hashes(&events, &heads_from_events(&events, conversation)) {
        let Some(stored) = events.get(&hash) else {
            return Ok(false);
        };
        if stored.status != "ready" {
            return Ok(false);
        }
        for blob in &stored.event.blobs {
            if !has_blob_from(connection, network, blob)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

pub(crate) fn scheduler_load<T: DeserializeOwned>(
    connection: &Connection,
    network: &str,
    key: &str,
) -> Result<Option<T>> {
    optional_json(
        connection
            .query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![network, key],
                |r| r.get(0),
            )
            .optional()?,
    )
}

pub(crate) fn scheduler_save(
    connection: &Connection,
    network: &str,
    key: &str,
    value: &impl serde::Serialize,
) -> Result<()> {
    connection.execute("INSERT INTO local_values(space_id,key,value) VALUES(?,?,?) ON CONFLICT(space_id,key) DO UPDATE SET value=excluded.value", params![network,key,serde_json::to_string(value)?])?;
    Ok(())
}

pub(crate) fn load_scheduler_session(
    connection: &Connection,
    network: &str,
    conversation: &str,
    target: &str,
) -> Result<SchedulerSession> {
    let session: SchedulerSession = scheduler_load(
        connection,
        network,
        &scheduler_key("session", conversation, target, ""),
    )?
    .unwrap_or_else(|| SchedulerSession::new(conversation, target));
    ensure!(
        session.version == SCHEDULING_VERSION,
        "unsupported scheduler session version"
    );
    Ok(session)
}

pub(crate) fn save_scheduler_session(
    connection: &Connection,
    network: &str,
    session: &SchedulerSession,
) -> Result<()> {
    scheduler_save(
        connection,
        network,
        &scheduler_key(
            "session",
            &session.conversation_id,
            &session.target_node,
            "",
        ),
        session,
    )
}

fn scheduler_sessions(connection: &Connection, network: &str) -> Result<Vec<SchedulerSession>> {
    let mut statement = connection.prepare(
        "SELECT value FROM local_values WHERE space_id=? AND key LIKE 'scheduler:v1:session:%'",
    )?;
    let rows = statement.query_map([network], |r| r.get::<_, String>(0))?;
    let mut sessions = Vec::new();
    for row in rows {
        let session: SchedulerSession = serde_json::from_str(&row?)?;
        ensure!(
            session.version == SCHEDULING_VERSION,
            "unsupported scheduler session version"
        );
        sessions.push(session);
    }
    Ok(sessions)
}

fn validated_scheduler_trigger(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    network: &str,
    conversation: &str,
    trigger: &str,
    target: &str,
) -> Result<TriggerKind> {
    authorize_node_from(connection, network, target, Some(NodeRole::Scheduler))?;
    let resource = conversation_resource_in(connection, network, conversation)?;
    let events = crate::identity::event_authorization::retained_scope_events_in(
        connection,
        network,
        &resource,
        conversation,
    )?;
    let stored = events
        .get(trigger)
        .context("scheduler trigger is missing")?;
    ensure!(
        stored.status == "ready"
            && stored.event.scope_id == conversation
            && stored.event.resource_id == resource,
        "scheduler trigger belongs to another resource or is not ready"
    );
    let origin = crate::identity::services::resolve_current_agent_origin(
        policy,
        connection,
        &events,
        trigger,
        crate::runtime::now_ms() as u64,
    )?;
    match &stored.event.body {
        EventBody::MessagePosted {
            role,
            scheduler_node,
            ..
        } if role == "user" => {
            ensure!(
                scheduler_node.as_deref() == Some(target),
                "message is addressed to another scheduler"
            );
            ensure!(
                matches!(
                    origin.parent.actor(),
                    vibeface_secure::authorization::Actor::Account(_)
                ),
                "scheduler message has no account authority"
            );
            Ok(TriggerKind::Human)
        }
        EventBody::ExecutionObserved { .. } => {
            followup::validate_followup_result_from(
                connection,
                policy,
                network,
                conversation,
                trigger,
                target,
            )?;
            Ok(TriggerKind::Autonomous)
        }
        _ => bail!("invalid scheduler trigger"),
    }
}

pub(crate) fn conversation_resource_in(
    connection: &Connection,
    space: &str,
    conversation: &str,
) -> Result<vibeface_secure::authorization::ResourceId> {
    let id: String = connection
        .query_row(
            "SELECT resource_id FROM conversations WHERE space_id=? AND conversation_id=?",
            params![space, conversation],
            |row| row.get(0),
        )
        .context("conversation resource is unavailable")?;
    vibeface_secure::authorization::ResourceId::parse(&id).map_err(Into::into)
}

pub(crate) fn admit_scheduler_from(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    network: &str,
    conversation: &str,
    trigger: &str,
    local_node: &str,
    target: &str,
) -> Result<AdmissionReceipt> {
    let kind =
        validated_scheduler_trigger(connection, policy, network, conversation, trigger, target)?;
    if local_node != target {
        let key = scheduler_key("outbox", conversation, target, trigger);
        if let Some(entry) = scheduler_load::<OutboxEntry>(connection, network, &key)? {
            return Ok(entry.receipt());
        }
        let count: usize = connection.query_row("SELECT COUNT(*) FROM local_values WHERE space_id=? AND key LIKE 'scheduler:v1:outbox:%' AND json_extract(value,'$.delivered')=0", [network], |r| r.get(0))?;
        ensure!(
            count < MAX_PENDING_JOBS,
            "scheduler delivery queue is full; message was not admitted"
        );
        let entry = OutboxEntry {
            resource_id: conversation_resource_in(connection, network, conversation)?,
            version: SCHEDULING_VERSION,
            space_id: network.into(),
            conversation_id: conversation.into(),
            trigger: trigger.into(),
            target_node: target.into(),
            delivered: false,
        };
        scheduler_save(connection, network, &key, &entry)?;
        return Ok(entry.receipt());
    }
    let key = scheduler_key("job", conversation, target, trigger);
    if let Some(job) = scheduler_load::<SchedulerJob>(connection, network, &key)? {
        return Ok(job.receipt(network));
    }
    let mut session = load_scheduler_session(connection, network, conversation, target)?;
    ensure!(
        session.pending.len() < MAX_PENDING_JOBS,
        "scheduler inbox is full; trigger was not admitted"
    );
    let job = SchedulerJob {
        resource_id: conversation_resource_in(connection, network, conversation)?,
        version: SCHEDULING_VERSION,
        conversation_id: conversation.into(),
        trigger: trigger.into(),
        target_node: target.into(),
        kind,
        sequence: session.next_sequence,
        basis_heads: {
            let mut heads = vec![trigger.into()];
            if let Some(head) = followup::report_intent_head_from(
                connection,
                policy,
                network,
                conversation,
                trigger,
            )? {
                heads.push(head);
            }
            heads
        },
        message_id: format!(
            "scheduler-message-{}",
            digest(&serde_json::to_vec(&(
                network,
                conversation,
                trigger,
                target
            ))?)
        ),
        state: JobState::Queued,
    };
    session.next_sequence = session
        .next_sequence
        .checked_add(1)
        .context("scheduler queue sequence overflow")?;
    session.pending.push(trigger.into());
    scheduler_save(connection, network, &key, &job)?;
    save_scheduler_session(connection, network, &session)?;
    Ok(job.receipt(network))
}

pub(crate) fn recover_scheduler_session(
    connection: &Connection,
    network: &str,
    session: &mut SchedulerSession,
    boot: &str,
) -> Result<()> {
    let Some(trigger) = session.active.clone() else {
        return Ok(());
    };
    let key = scheduler_key(
        "job",
        &session.conversation_id,
        &session.target_node,
        &trigger,
    );
    let mut job: SchedulerJob =
        scheduler_load(connection, network, &key)?.context("missing active scheduler job")?;
    let Some(attempt) = job.state.attempt().cloned() else {
        bail!("active scheduler job has no attempt");
    };
    if scheduler_publication_from(connection, network, &job)?.is_some() {
        return Ok(());
    }
    if attempt.boot == boot {
        return Ok(());
    }
    match job.state {
        JobState::Claimed { .. } => {
            job.state = JobState::Queued;
            if !session.pending.contains(&trigger) {
                session.pending.push(trigger.clone());
            }
            session.active = None;
        }
        JobState::Running { .. } | JobState::Cancelling { .. } => {
            job.state = JobState::Unknown {
                attempt,
                detail: "Node restarted after the SDK start boundary; this call was not replayed"
                    .into(),
                fenced: false,
            };
            session.unresolved.insert(trigger);
            session.native_id = None;
            session.permission_version = session
                .permission_version
                .checked_add(1)
                .context("scheduler permission version overflow")?;
        }
        JobState::Unknown { .. } => {}
        _ => session.active = None,
    }
    scheduler_save(connection, network, &key, &job)?;
    Ok(())
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SchedulerWithdrawal {
    FollowupOff {
        event: String,
    },
    TaskFinalized {
        event: String,
        status: String,
    },
    DeviceRevoked {
        account: String,
        device: String,
        policy: String,
    },
    DeviceExpired {
        device: String,
        expires_at_ms: u64,
    },
    GrantRemoved {
        grant: String,
        policy: String,
    },
    GrantExpired {
        grant: String,
        expires_at_ms: u64,
    },
}

#[derive(Debug)]
pub(crate) struct SchedulerDeferred {
    pub trigger: String,
    pub error: anyhow::Error,
}

pub(crate) struct SchedulerSelection {
    pub job: Option<SchedulerJob>,
    pub deferred: Vec<SchedulerDeferred>,
}

enum SchedulerEligibility {
    Ready(Option<followup::ResultFollowupPolicy>),
    Withdrawn(SchedulerWithdrawal),
    Deferred(anyhow::Error),
}

fn scheduler_credential_withdrawal(
    state: &vibeface_secure::authorization::VerifiedPolicySet,
    proof: &AdmissionProof,
    now: u64,
) -> Result<Option<SchedulerWithdrawal>> {
    use vibeface_secure::authorization::{Actor, PolicyDomain};
    let invocation = &proof.invocation.payload;
    let Actor::Account(account) = &invocation.actor else {
        return Ok(None);
    };
    let document = state
        .account(account)
        .context("Account policy is unavailable")?;
    if document.revoked_devices.contains(&invocation.device) {
        return Ok(Some(SchedulerWithdrawal::DeviceRevoked {
            account: account.to_string(),
            device: invocation.device.to_string(),
            policy: state
                .head(&PolicyDomain::Account(account.clone()))
                .context("Account policy head is unavailable")?
                .head
                .to_string(),
        }));
    }
    let mut registered_device = false;
    for hash in &document.devices {
        let certificate = state
            .device(hash)
            .context("Account device proof is unavailable")?;
        if certificate.payload.device == invocation.device {
            registered_device = true;
            if certificate.payload.expires_at_ms <= now {
                return Ok(Some(SchedulerWithdrawal::DeviceExpired {
                    device: invocation.device.to_string(),
                    expires_at_ms: certificate.payload.expires_at_ms,
                }));
            }
        }
    }
    ensure!(
        registered_device,
        "original Account device is absent without an explicit revocation"
    );
    let mut cursor = Some(invocation.grant_leaf.clone());
    let mut visited = BTreeSet::new();
    while let Some(hash) = cursor {
        ensure!(
            visited.insert(hash.clone())
                && visited.len() <= vibeface_secure::authorization::MAX_DELEGATION_DEPTH,
            "grant ancestry is incomplete or cyclic"
        );
        let grant = state
            .grant(&hash)
            .context("original grant proof is unavailable")?;
        let owner = state
            .space(&grant.payload.owner_space)
            .context("grant owner policy is unavailable")?;
        let head = state
            .head(&PolicyDomain::Space(grant.payload.owner_space.clone()))
            .context("grant owner policy head is unavailable")?;
        if !owner.grants.contains(&hash) {
            return Ok(Some(SchedulerWithdrawal::GrantRemoved {
                grant: hash.to_string(),
                policy: head.head.to_string(),
            }));
        }
        if grant.payload.expires_at_ms <= now {
            return Ok(Some(SchedulerWithdrawal::GrantExpired {
                grant: hash.to_string(),
                expires_at_ms: grant.payload.expires_at_ms,
            }));
        }
        cursor = grant.payload.parent.clone();
    }
    Ok(None)
}

fn scheduler_eligibility_in(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    space: &str,
    job: &SchedulerJob,
    now: u64,
) -> SchedulerEligibility {
    let checked = (|| -> Result<SchedulerEligibility> {
        let mut events = load_events_from(connection, policy, space)?;
        events.retain(|_, stored| stored.event.resource_id == job.resource_id);
        let heads = heads_from_events(&events, &job.conversation_id);
        ensure!(
            closure_hashes(&events, &heads).iter().all(|hash| events
                .get(hash)
                .is_some_and(|event| event.status == "ready")),
            "scheduler history is incomplete or conflicting"
        );
        ensure!(
            closure_hashes(&events, &job.basis_heads)
                .iter()
                .all(|hash| events
                    .get(hash)
                    .is_some_and(|event| event.status == "ready")),
            "queued scheduler proof is incomplete or conflicting"
        );
        let source = events
            .get(&job.trigger)
            .context("scheduler trigger is unavailable")?;
        ensure!(source.status == "ready", "scheduler trigger is not ready");
        ensure!(
            match (&job.kind, &source.event.body) {
                (
                    TriggerKind::Human,
                    EventBody::MessagePosted {
                        role,
                        scheduler_node: Some(target),
                        ..
                    },
                ) => role == "user" && target == &job.target_node,
                (TriggerKind::Autonomous, EventBody::ExecutionObserved { .. }) => true,
                _ => false,
            },
            "scheduler job kind or target differs from its original trigger"
        );
        let mut authorities = Vec::new();
        if job.kind == TriggerKind::Autonomous {
            let (withdrawal, proofs) = followup::scheduler_followup_evidence_in(
                connection,
                policy,
                space,
                &job.conversation_id,
                &events,
                &job.trigger,
                now,
            )?;
            if let Some(reason) = withdrawal {
                return Ok(SchedulerEligibility::Withdrawn(reason));
            }
            authorities.extend(proofs);
        }
        let readiness = match job.kind {
            TriggerKind::Human => crate::identity::services::resolve_current_agent_origin(
                policy,
                connection,
                &events,
                &job.trigger,
                now,
            )
            .map(|_| None),
            TriggerKind::Autonomous => followup::validate_followup_result_in_view(
                connection,
                policy,
                policy.verified(),
                &events,
                space,
                &job.conversation_id,
                &job.trigger,
                &job.target_node,
                &heads,
                now,
            )
            .map(Some),
        };
        match readiness {
            Ok(followup) => Ok(SchedulerEligibility::Ready(followup)),
            Err(error) => {
                let parent =
                    crate::identity::services::agent_account_message(&events, &job.trigger)?;
                let EventCause::Invocation { admission } = &parent.event.cause else {
                    bail!("scheduler Account source has no admission")
                };
                authorities.push(admission.clone());
                for authority in &authorities {
                    if let Some(reason) =
                        scheduler_credential_withdrawal(policy.verified(), authority, now)?
                    {
                        return Ok(SchedulerEligibility::Withdrawn(reason));
                    }
                }
                Ok(SchedulerEligibility::Deferred(error))
            }
        }
    })();
    checked.unwrap_or_else(SchedulerEligibility::Deferred)
}

fn select_scheduler_candidate_in(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    space: &str,
    session: &mut SchedulerSession,
    skip: &BTreeSet<String>,
    expected: Option<&str>,
    now: u64,
) -> Result<(SchedulerSelection, Option<followup::ResultFollowupPolicy>)> {
    if session.active.is_some() {
        return Ok((
            SchedulerSelection {
                job: None,
                deferred: Vec::new(),
            },
            None,
        ));
    }
    let mut queued = Vec::new();
    for trigger in &session.pending {
        if skip.contains(trigger) || expected.is_some_and(|requested| requested != trigger) {
            continue;
        }
        let job: SchedulerJob = scheduler_load(
            connection,
            space,
            &scheduler_key(
                "job",
                &session.conversation_id,
                &session.target_node,
                trigger,
            ),
        )?
        .context("queued scheduler job is missing")?;
        ensure!(
            matches!(job.state, JobState::Queued),
            "pending scheduler job is not queued"
        );
        ensure!(
            job.conversation_id == session.conversation_id
                && job.target_node == session.target_node
                && job.resource_id
                    == conversation_resource_in(connection, space, &session.conversation_id)?,
            "queued scheduler scope differs from its session"
        );
        if session.unresolved.is_empty() || job.kind == TriggerKind::Human {
            queued.push(job);
        }
    }
    queued.sort_by_key(|job| (job.kind != TriggerKind::Human, job.sequence));
    let mut selection = SchedulerSelection {
        job: None,
        deferred: Vec::new(),
    };
    for mut job in queued {
        match scheduler_eligibility_in(connection, policy, space, &job, now) {
            SchedulerEligibility::Ready(followup) => {
                selection.job = Some(job);
                return Ok((selection, followup));
            }
            SchedulerEligibility::Withdrawn(reason) => {
                crate::identity::admission::retire_queued_manager_in(
                    connection,
                    space,
                    &job.trigger,
                )?;
                job.state = JobState::Cancelled {
                    attempt: None,
                    evidence: serde_json::to_string(&reason)?,
                };
                session.pending.retain(|trigger| trigger != &job.trigger);
                scheduler_save(
                    connection,
                    space,
                    &scheduler_key("job", &job.conversation_id, &job.target_node, &job.trigger),
                    &job,
                )?;
            }
            SchedulerEligibility::Deferred(error) => selection.deferred.push(SchedulerDeferred {
                trigger: job.trigger,
                error,
            }),
        }
    }
    Ok((selection, None))
}

pub(crate) fn claim_next_in(
    tx: &Connection,
    policy: &crate::identity::journal::PolicyState,
    network: &str,
    conversation: &str,
    target: &str,
    boot: &str,
    sdk_config: &str,
    attempt_id: &str,
    expected_trigger: &str,
) -> Result<Option<ClaimedJob>> {
    let mut session = load_scheduler_session(tx, network, conversation, target)?;
    recover_scheduler_session(tx, network, &mut session, boot)?;
    if session.active.is_some() {
        save_scheduler_session(tx, network, &session)?;
        return Ok(None);
    }
    let (selected, policy) = select_scheduler_candidate_in(
        tx,
        policy,
        network,
        &mut session,
        &BTreeSet::new(),
        Some(expected_trigger),
        crate::runtime::now_ms() as u64,
    )?;
    let Some(mut job) = selected.job else {
        save_scheduler_session(tx, network, &session)?;
        if let Some(deferred) = selected.deferred.into_iter().next() {
            return Err(deferred.error.context("scheduler eligibility is deferred"));
        }
        return Ok(None);
    };
    if session.sdk_config.as_deref() != Some(sdk_config) {
        session.native_id = None;
        session.sdk_config = Some(sdk_config.into());
    }
    if let Some(policy) = &policy {
        for head in &policy.proof_heads {
            if !job.basis_heads.contains(head) {
                job.basis_heads.push(head.clone());
            }
        }
    }
    let read_only = !session.unresolved.is_empty()
        || policy
            .as_ref()
            .is_some_and(|policy| policy.mode != followup::FollowupMode::Continue);
    let previous_native_id = if read_only {
        None
    } else {
        session.native_id.clone()
    };
    job.state = JobState::Claimed {
        attempt: ManagerAttempt {
            id: attempt_id.into(),
            boot: boot.into(),
            read_only,
            permission_version: session.permission_version,
            followup_version: policy.map(|policy| policy.version),
        },
    };
    session.pending.retain(|trigger| trigger != &job.trigger);
    session.active = Some(job.trigger.clone());
    scheduler_save(
        tx,
        network,
        &scheduler_key("job", conversation, target, &job.trigger),
        &job,
    )?;
    save_scheduler_session(tx, network, &session)?;
    Ok(Some(ClaimedJob {
        job,
        previous_native_id,
    }))
}

fn finish_scheduler_in(
    connection: &Connection,
    space: &str,
    current: &SchedulerJob,
    outcome: &JobOutcome,
) -> Result<SchedulerJob> {
    let mut job = current.clone();
    let mut session =
        load_scheduler_session(connection, space, &job.conversation_id, &job.target_node)?;
    let attempt = job
        .state
        .attempt()
        .context("scheduler attempt missing")?
        .clone();
    job.state = match outcome {
        JobOutcome::Completed => {
            session.unresolved.remove(&job.trigger);
            JobState::Completed { attempt }
        }
        JobOutcome::Failed(detail) => JobState::Failed {
            attempt,
            detail: detail.clone(),
        },
        JobOutcome::Cancelled(evidence) => JobState::Cancelled {
            attempt: Some(attempt),
            evidence: evidence.clone(),
        },
        JobOutcome::Unknown(detail) => {
            session.unresolved.insert(job.trigger.clone());
            session.native_id = None;
            session.permission_version = session
                .permission_version
                .checked_add(1)
                .context("scheduler permission version overflow")?;
            JobState::Unknown {
                attempt,
                detail: detail.clone(),
                fenced: false,
            }
        }
    };
    if !matches!(job.state, JobState::Unknown { .. }) {
        session.active = None;
    }
    scheduler_save(
        connection,
        space,
        &scheduler_key("job", &job.conversation_id, &job.target_node, &job.trigger),
        &job,
    )?;
    save_scheduler_session(connection, space, &session)?;
    Ok(job)
}
pub(crate) fn rebuild_projections_from(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    network_id: &str,
) -> Result<()> {
    let mut events = load_events_from(connection, policy, network_id)?;
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
    bots::validate_bot_projections_from(connection, network_id, &mut events, &ordered);
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
    let policy = crate::identity::journal::PolicyState::load(connection)?;
    authorize_verified_node_from(connection, policy.verified(), network_id, node_id)
}

fn authorize_verified_node_from(
    connection: &Connection,
    policy: &vibeface_secure::authorization::VerifiedPolicySet,
    network_id: &str,
    node_id: &str,
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
    let public_key = PublicKey::parse(&key)?;
    let certificate = policy.verify_service_device(
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

fn validate_session_ownership_from(
    connection: &Connection,
    space: &str,
    target: &str,
    session: &str,
    conversation: &str,
) -> Result<bool> {
    let mut owned = false;
    let mut statement = connection.prepare(
        "SELECT request FROM operations WHERE space_id=? AND target_node=? AND session_id=?",
    )?;
    for encoded in statement.query_map(params![space, target, session], |row| {
        row.get::<_, String>(0)
    })? {
        let request: OperationRequest = serde_json::from_str(&encoded?)?;
        ensure!(
            request.conversation_id == conversation,
            "execution session belongs to another conversation"
        );
        owned = true;
    }
    let mut statement = connection.prepare(
        "SELECT value FROM local_values WHERE space_id=? AND key LIKE 'task-binding-record:%'",
    )?;
    for encoded in statement.query_map([space], |row| row.get::<_, String>(0))? {
        let proof: crate::binding::SignedBinding = serde_json::from_str(&encoded?)?;
        if proof.binding.target_node == target && proof.binding.session_id == session {
            ensure!(
                proof.binding.conversation_id == conversation,
                "execution binding belongs to another conversation"
            );
            owned = true;
        }
    }
    Ok(owned)
}
fn validate_unbound_operation_from(
    connection: &Connection,
    request: &OperationRequest,
) -> Result<()> {
    let Some(session) = request.session_id.as_deref() else {
        ensure!(
            request.action["type"] == "command",
            "only commands are sessionless"
        );
        return Ok(());
    };
    validate_session_ownership_from(
        connection,
        &request.network_id,
        &request.target_node,
        session,
        &request.conversation_id,
    )?;
    let mut statement = connection.prepare(
        "SELECT request FROM operations WHERE space_id=? AND target_node=? AND session_id=?",
    )?;
    for encoded in statement.query_map(
        params![request.network_id, request.target_node, session],
        |row| row.get::<_, String>(0),
    )? {
        let previous: OperationRequest = serde_json::from_str(&encoded?)?;
        ensure!(
            previous.task_id.is_none() && previous.binding.is_none(),
            "task-bound session cannot be used without its binding"
        );
    }
    let mut statement = connection.prepare(
        "SELECT value FROM local_values WHERE space_id=? AND key LIKE 'task-binding-record:%'",
    )?;
    for encoded in statement.query_map([&request.network_id], |row| row.get::<_, String>(0))? {
        let proof: crate::binding::SignedBinding = serde_json::from_str(&encoded?)?;
        ensure!(
            proof.binding.target_node != request.target_node || proof.binding.session_id != session,
            "task-bound session cannot be downgraded to ordinary execution"
        );
    }
    Ok(())
}

pub(crate) fn validate_operation_binding_from(
    connection: &Connection,
    request: &OperationRequest,
) -> Result<()> {
    if request.action["type"] == "command" {
        ensure!(
            request.session_id.is_none()
                && request.task_id.is_none()
                && request.binding.is_none()
                && request.expected_revision == 0,
            "command cannot carry a Task, session, binding or revision"
        );
        return Ok(());
    }
    ensure!(
        request
            .session_id
            .as_deref()
            .is_some_and(|session| !session.is_empty()),
        "session execution requires a nonempty session ID"
    );
    if let Some(session) = request.session_id.as_deref() {
        validate_session_ownership_from(
            connection,
            &request.network_id,
            &request.target_node,
            session,
            &request.conversation_id,
        )?;
    }
    match (&request.task_id, &request.binding) {
        (None, None) => return validate_unbound_operation_from(connection, request),
        (Some(_), Some(_)) => {}
        _ => bail!("task and confirmed binding must be supplied together"),
    }
    let proof = request.binding.as_ref().expect("checked binding");
    let binding = &proof.binding;
    ensure!(
        binding.network_id == request.network_id
            && binding.conversation_id == request.conversation_id
            && Some(binding.task_id.as_str()) == request.task_id.as_deref(),
        "operation does not match its bound task"
    );
    ensure!(
        binding.target_node == request.target_node
            && Some(binding.session_id.as_str()) == request.session_id.as_deref(),
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
    policy: &crate::identity::journal::PolicyState,
    network_id: &str,
) -> Result<BTreeMap<String, StoredEvent>> {
    let result = load_retained_event_rows_from(connection, network_id)?;
    for stored in result.values() {
        ensure!(
            crate::identity::event_authorization::verify_event_envelope(
                connection,
                policy,
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
                    request
                        .session_id
                        .as_deref()
                        .context("fixture session is missing")?
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
    fn node_authorization_reuses_verified_prefix_and_rechecks_certificate() -> Result<()> {
        let (network, mut store) = setup()?;
        let space = network.space.as_str();
        let node_id = network.node.node_id()?;
        let verification = vibeface_secure::signature_verification::SignatureVerification::new();
        let certificate = verification.run(|| store.authorize_node(space, &node_id, None))?;
        assert_eq!(certificate.node_id, node_id);
        assert_eq!(certificate.public_key, network.node.public_key_hex()?);
        assert!(verification.counts().actual_verifications > 0);
        assert!(verification.counts().actual_verifications <= 32);
        store.connection.execute_batch("BEGIN IMMEDIATE")?;
        assert_eq!(
            store.authorize_node(space, &node_id, None)?.node_id,
            node_id
        );
        assert!(!store.connection.is_autocommit());
        store.connection.execute_batch("ROLLBACK")?;
        let changed_key = Identity::generate()
            .map_err(anyhow::Error::msg)?
            .public_key_hex();
        store.connection.execute(
            "UPDATE node_certificates SET public_key=? WHERE space_id=? AND node_id=?",
            params![changed_key, space, node_id],
        )?;
        assert!(store.authorize_node(space, &node_id, None).is_err());
        store.connection.execute(
            "UPDATE node_certificates SET public_key=? WHERE space_id=? AND node_id=?",
            params![certificate.public_key, space, node_id],
        )?;
        store.connection.execute_batch(
            "UPDATE policy_journal SET accepted_at_ms=accepted_at_ms+1 WHERE ordinal=(SELECT MIN(ordinal) FROM policy_journal)",
        )?;
        assert!(store.authorize_node(space, &node_id, None).is_err());
        store.connection.execute_batch(
            "UPDATE policy_journal SET accepted_at_ms=accepted_at_ms-1 WHERE ordinal=(SELECT MIN(ordinal) FROM policy_journal)",
        )?;
        assert_eq!(
            store.authorize_node(space, &node_id, None)?.node_id,
            node_id
        );
        let known = store.policy.clone();
        network.revoke_node(&mut store)?;
        store.policy = known;
        assert!(store.authorize_node(space, &node_id, None).is_err());
        Ok(())
    }

    #[test]
    fn binding_task_uses_its_retained_creation_ancestors() -> Result<()> {
        let (network, mut store) = setup()?;
        let space = network.space.as_str();
        let root = network.conversation(&mut store, "conversation")?;
        let creation = network.task(&mut store, "conversation", "bound-task")?;
        let binding = network.bind(
            &mut store,
            &creation,
            "bound-task",
            "binding-session",
            &network.project,
        )?;
        let mut latest = creation.clone();
        for _ in 0..40 {
            latest = network.message(&mut store, "conversation", "later history")?;
        }
        let verification = vibeface_secure::signature_verification::SignatureVerification::new();
        verification.run(|| store.verify_historical_binding(&binding))?;
        assert!(verification.counts().actual_verifications > 0);
        assert!(verification.counts().actual_verifications <= 32);
        store.connection.execute_batch("BEGIN IMMEDIATE")?;
        store.verify_historical_binding(&binding)?;
        assert!(!store.connection.is_autocommit());
        store.connection.execute_batch("ROLLBACK")?;
        store.connection.execute(
            "UPDATE events SET status='pending' WHERE space_id=? AND hash=?",
            params![space, latest.hash],
        )?;
        store.verify_historical_binding(&binding)?;
        assert!(
            crate::identity::event_authorization::validate_retained_event_scope_in(
                &store.connection,
                &creation,
            )
            .is_err()
        );
        store.connection.execute(
            "UPDATE events SET sequence=999 WHERE space_id=? AND hash=?",
            params![space, root.hash],
        )?;
        assert!(store.verify_historical_binding(&binding).is_err());
        store.connection.execute(
            "UPDATE events SET sequence=1 WHERE space_id=? AND hash=?",
            params![space, root.hash],
        )?;
        store.verify_historical_binding(&binding)?;
        store.connection.execute(
            "UPDATE events SET status='pending' WHERE space_id=? AND hash=?",
            params![space, creation.hash],
        )?;
        assert!(store.verify_historical_binding(&binding).is_err());
        Ok(())
    }

    #[test]
    fn task_creation_uses_its_complete_retained_scope_without_replaying_policy() -> Result<()> {
        let (network, mut store) = setup()?;
        let space = network.space.as_str();
        network.conversation(&mut store, "conversation")?;
        network.conversation(&mut store, "parallel")?;
        let creation = network.task(&mut store, "conversation", "same-task")?;
        let parallel = network.task(&mut store, "parallel", "same-task")?;
        let mut latest = creation.clone();
        for _ in 0..40 {
            latest = network.message(&mut store, "conversation", "retained history")?;
        }
        let verification = vibeface_secure::signature_verification::SignatureVerification::new();
        let actual =
            verification.run(|| store.task_creation(space, "conversation", "same-task"))?;
        assert_eq!(actual, creation);
        assert_eq!(verification.counts().actual_verifications, 0);
        assert_eq!(
            store.task_creation(space, "parallel", "same-task")?,
            parallel
        );
        assert!(
            store
                .task_creation(space, "conversation", "missing")
                .is_err()
        );
        store.connection.execute_batch("BEGIN IMMEDIATE")?;
        assert_eq!(
            store.task_creation(space, "conversation", "same-task")?,
            creation
        );
        assert!(!store.connection.is_autocommit());
        store.connection.execute_batch("ROLLBACK")?;
        let other_resource_id = ResourceId::random()?;
        let other_resource = other_resource_id.as_str();
        let mut record = store
            .policy
            .verified()
            .resource(&network.space, &network.resource)
            .context("original resource is missing")?
            .payload
            .clone();
        record.resource = other_resource_id.clone();
        record.nonce = Nonce::random()?;
        let record = Signed::sign(record, &network.root)?;
        store.connection.execute(
            "INSERT INTO resource_records(space_id,resource_id,record_hash,kind,host_service_id,signed_record) SELECT space_id,?,?,kind,host_service_id,? FROM resource_records WHERE space_id=? AND resource_id=?",
            params![other_resource, record.hash()?.as_str(), serde_json::to_string(&record)?, space, network.resource.as_str()],
        )?;
        assert_eq!(
            store.connection.execute(
                "UPDATE events SET resource_id=? WHERE space_id=? AND hash=?",
                params![other_resource, space, latest.hash],
            )?,
            1
        );
        assert!(
            store
                .task_creation(space, "conversation", "same-task")
                .is_err()
        );
        assert!(
            crate::identity::event_authorization::validate_retained_event_scope_in(
                &store.connection,
                &creation,
            )
            .is_err()
        );
        store.connection.execute(
            "UPDATE events SET resource_id=? WHERE space_id=? AND hash=?",
            params![network.resource.as_str(), space, latest.hash],
        )?;
        assert_eq!(
            store.task_creation(space, "conversation", "same-task")?,
            creation
        );
        store.connection.execute(
            "UPDATE events SET sequence=999 WHERE space_id=? AND hash=?",
            params![space, parallel.hash],
        )?;
        assert_eq!(
            store.task_creation(space, "conversation", "same-task")?,
            creation
        );
        assert!(store.task_creation(space, "parallel", "same-task").is_err());
        store.connection.execute(
            "UPDATE events SET status='pending' WHERE space_id=? AND hash=?",
            params![space, creation.hash],
        )?;
        assert!(
            store
                .task_creation(space, "conversation", "same-task")
                .is_err()
        );
        Ok(())
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
        let space_id = network.space.to_string();
        let space = space_id.as_str();
        let root = network.conversation(&mut store, "conversation")?;
        let old = network.signed_message(
            &mut store,
            "retired author",
            2,
            Some(root.hash.clone()),
            vec![root.hash.clone()],
        )?;
        let old_event = store.verify_event(space, &old)?;
        let events = load_events_from(&store.connection, &store.policy, space)?;
        crate::identity::event_authorization::current_event_authorization(
            &store.policy,
            &store.connection,
            &old_event,
            &events,
            now(),
        )?;
        let (network, mut store, target) =
            crate::identity::cross_node_recovery::tests::recover_existing_source(
                network,
                store,
                old.clone(),
            )?;
        let snapshot = target
            .policy
            .accepted_journal_snapshot(&target.connection)?;
        let recovered_journal = target.policy.accepted_journal_prefix(&snapshot)?;
        assert!(
            store
                .authorize_node(space, &network.node.node_id()?, None)
                .is_ok()
        );
        assert_eq!(store.ingest_event(space, &old)?.status, "ready");
        assert!(
            network
                .message(&mut store, "conversation", "denied")
                .is_err()
        );
        assert!(store.verify_event(space, &old).is_ok());
        let events = load_events_from(&store.connection, &store.policy, space)?;
        assert!(
            crate::identity::event_authorization::current_event_authorization(
                &store.policy,
                &store.connection,
                &old_event,
                &events,
                now(),
            )
            .is_err()
        );
        let mut fresh = Store::initialize(Path::new(":memory:"))?;
        let update = fresh
            .policy
            .prepare_bootstrap(&network.pins, &recovered_journal, now())?;
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
                .is_ok()
        );
        assert!(
            network
                .message(&mut fresh, "conversation", "denied")
                .is_err()
        );
        let fresh_events = load_events_from(&fresh.connection, &fresh.policy, space)?;
        assert!(
            crate::identity::event_authorization::current_event_authorization(
                &fresh.policy,
                &fresh.connection,
                &old_event,
                &fresh_events,
                now(),
            )
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
    #[test]
    fn scheduler_withdrawal_skips_only_verified_revocation_and_defers_missing_history() -> Result<()>
    {
        let mut network = TestNetwork::new()?;
        let mut store = network.memory()?;
        network.conversation(&mut store, "conversation")?;
        let space = network.space.to_string();
        let target = network.node.node_id()?;
        let first =
            network.message(&mut store, "conversation", "queued before grant revocation")?;
        store.admit_trigger(&space, "conversation", &first.hash, &target, &target)?;

        let old_hash = network.bundle.grant_leaf.clone();
        let original = store
            .authorization_state()
            .grant(&old_hash)
            .unwrap()
            .clone();
        let mut replacement = original.payload.clone();
        replacement.nonce = Nonce::random()?;
        replacement.issued_under = store
            .authorization_state()
            .head(&PolicyDomain::Space(network.space.clone()))
            .unwrap()
            .clone();
        let signer = if original.issuer == PublicKey::of(&network.root) {
            &network.root
        } else {
            assert_eq!(original.issuer, PublicKey::of(&network.client));
            &network.client
        };
        let replacement = Signed::sign(replacement, signer)?;
        let new_hash = replacement.hash()?;
        let mut document = store
            .authorization_state()
            .space(&network.space)
            .unwrap()
            .clone();
        document.previous = store
            .authorization_state()
            .head(&PolicyDomain::Space(network.space.clone()))
            .cloned();
        document.sequence += 1;
        document.grants.retain(|hash| hash != &old_hash);
        document.grants.push(new_hash.clone());
        document.grants.sort();
        store.install_policy_journal(
            &[
                JournalEntry::Grant(replacement),
                JournalEntry::Space(Signed::sign(document, &network.root)?),
            ],
            now(),
        )?;
        network.bundle.grant_leaf = new_hash;
        let second = network.message(&mut store, "conversation", "new signed permission")?;
        store.admit_trigger(&space, "conversation", &second.hash, &target, &target)?;
        let selection = store.next_scheduler_candidate(
            &space,
            "conversation",
            &target,
            "boot",
            &BTreeSet::new(),
        )?;
        assert_eq!(selection.job.unwrap().trigger, second.hash);
        assert!(selection.deferred.is_empty());
        let retired = store
            .scheduler_job(&space, "conversation", &first.hash, &target)?
            .unwrap();
        let JobState::Cancelled {
            attempt: None,
            evidence,
        } = retired.state
        else {
            panic!("revoked queued job was not retired")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&evidence)?["kind"],
            "grant_removed"
        );

        let third =
            network.message(&mut store, "conversation", "continue past missing evidence")?;
        store.admit_trigger(&space, "conversation", &third.hash, &target, &target)?;
        let mut deferred = store
            .scheduler_job(&space, "conversation", &second.hash, &target)?
            .unwrap();
        deferred.basis_heads.push("f".repeat(64));
        scheduler_save(
            &store.connection,
            &space,
            &scheduler_key("job", "conversation", &target, &second.hash),
            &deferred,
        )?;
        let selection = store.next_scheduler_candidate(
            &space,
            "conversation",
            &target,
            "boot",
            &BTreeSet::new(),
        )?;
        assert_eq!(selection.job.unwrap().trigger, third.hash);
        assert_eq!(selection.deferred.len(), 1);
        assert_eq!(selection.deferred[0].trigger, second.hash);
        assert!(matches!(
            store
                .scheduler_job(&space, "conversation", &second.hash, &target)?
                .unwrap()
                .state,
            JobState::Queued
        ));
        Ok(())
    }
}
