use crate::{
    crypto::{LocalIdentity, digest, verify_signed_event},
    identity::NetworkState,
    model::*,
    scheduling::*,
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

mod agent_messages;
mod blob_reader;
pub(crate) mod blob_transfer;
pub(crate) mod bots;
pub(crate) mod followup;

const INITIAL_SCHEMA: &str = include_str!("../migrations/202610180001_initial.sql");

pub struct Store {
    blob_readers: blob_reader::BlobReaders,
    pub(crate) connection: Connection,
    path: PathBuf,
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
    for member in merged.historical_nodes()? {
        let old: Option<String> = connection
            .query_row(
                "SELECT public_key FROM node_certificates WHERE network_id=? AND peer_id=?",
                params![network_id, member.subject_id],
                |r| r.get(0),
            )
            .optional()?;
        ensure!(
            old.as_ref().is_none_or(|key| key == &member.public_key),
            "historical Node identity key changed"
        );
        connection.execute(
            "INSERT OR IGNORE INTO node_certificates(network_id,peer_id,public_key) VALUES(?,?,?)",
            params![network_id, member.subject_id, member.public_key],
        )?;
    }
    rebuild_projections_from(connection, network_id)?;
    Ok(true)
}

#[derive(Clone)]
struct StoredEvent {
    envelope: SignedEvent,
    event: Event,
    status: String,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let connection = if path == Path::new(":memory:") {
            Connection::open_in_memory()?
        } else {
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
            let canonical_path = path.canonicalize()?;
            Connection::open_with_flags(
                canonical_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?
        };
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: u32 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        match version {
            0 => {
                let tables:i64 = connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",[],|r|r.get(0))?;
                ensure!(tables == 0, "unversioned database is not supported");
                connection.execute_batch(&format!("BEGIN IMMEDIATE;{INITIAL_SCHEMA}COMMIT;"))?;
            }
            5 => {}
            _ => bail!(
                "unsupported database schema version {version}; initialize a fresh Node data directory"
            ),
        }
        let mut store = Self {
            blob_readers: Default::default(),
            connection,
            path: path.to_owned(),
            process_lock: None,
        };
        for network in store.networks()? {
            store
                .trust_bundle(&network)?
                .context("membership is missing")?
                .project()?;
        }
        store
            .connection
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")?;
        for network in store.networks()? {
            store.rebuild_projections(&network)?;
        }
        Ok(store)
    }
    pub fn install_trust_bundle(&mut self, network_id: &str, state: &NetworkState) -> Result<bool> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = install_trust_bundle_in(&tx, network_id, state)?;
        tx.commit()?;
        if changed {
            self.blob_readers.retire_network(network_id);
        }
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
            version == 5,
            "unsupported database schema version {version}"
        );
        Ok(Self {
            blob_readers: Default::default(),
            connection,
            path: path.to_owned(),
            process_lock: None,
        })
    }
    pub(crate) fn retire_identity_readers(&mut self, network: &str) {
        self.blob_readers.retire_network(network);
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
    pub fn authorize_node(
        &self,
        network_id: &str,
        peer_id: &str,
        role: Option<NodeRole>,
    ) -> Result<NodeCertificate> {
        authorize_node_from(&self.connection, network_id, peer_id, role)
    }
    pub fn save_local_identity(
        &mut self,
        network_id: &str,
        identity: &LocalIdentity,
    ) -> Result<()> {
        let peer_id = identity.peer_id()?;
        self.authorize_node(network_id, &peer_id, None)?;
        self.connection.execute("INSERT INTO local_identities(network_id,peer_id,identity) VALUES(?,?,?) ON CONFLICT(network_id,peer_id) DO UPDATE SET identity=excluded.identity",params![network_id,peer_id,serde_json::to_string(identity)?])?;
        Ok(())
    }
    pub fn local_identity(&self, network_id: &str, peer_id: &str) -> Result<Option<LocalIdentity>> {
        optional_json(
            self.connection
                .query_row(
                    "SELECT identity FROM local_identities WHERE network_id=? AND peer_id=?",
                    params![network_id, peer_id],
                    |r| r.get(0),
                )
                .optional()?,
        )
    }
    fn public_key(&self, network_id: &str, author: &str) -> Result<String> {
        self.connection
            .query_row(
                "SELECT public_key FROM node_certificates WHERE network_id=? AND peer_id=?",
                params![network_id, author],
                |r| r.get(0),
            )
            .optional()?
            .context("unknown historical event author")
    }
    pub fn verify_event(&self, network_id: &str, envelope: &SignedEvent) -> Result<Event> {
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        let key = self.public_key(network_id, &event.author)?;
        verify_signed_event(network_id, envelope, &key)
    }
    pub fn ingest_events(&mut self, network_id: &str, envelopes: &[SignedEvent]) -> Result<usize> {
        ensure!(envelopes.len() <= 1024, "event batch is too large");
        ensure!(
            envelopes
                .iter()
                .map(|event| event.canonical_body.len())
                .sum::<usize>()
                <= 8 * MAX_EVENT_BYTES,
            "event batch byte limit exceeded"
        );
        let verified = envelopes
            .iter()
            .map(|envelope| {
                Ok((
                    envelope,
                    self.verify_event(network_id, envelope)?,
                    serde_json::to_string(envelope)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut inserted = 0;
        for (envelope, event, encoded) in verified {
            let old: Option<String> = tx
                .query_row(
                    "SELECT envelope FROM events WHERE network_id=? AND hash=?",
                    params![network_id, envelope.hash],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(old) = old {
                ensure!(old == encoded, "conflicting envelope for event hash");
                continue;
            }
            tx.execute("INSERT INTO events(network_id,hash,scope_id,author,sequence,envelope,status) VALUES(?,?,?,?,?,?,'pending')",params![network_id,envelope.hash,event.scope_id,event.author,event.sequence,encoded])?;
            inserted += 1;
        }
        if inserted != 0 {
            rebuild_projections_from(&tx, network_id)?;
        }
        tx.commit()?;
        Ok(inserted)
    }
    pub fn ingest_event(
        &mut self,
        network_id: &str,
        envelope: &SignedEvent,
    ) -> Result<IngestOutcome> {
        self.ingest_events(network_id, std::slice::from_ref(envelope))?;
        let status: String = self.connection.query_row(
            "SELECT status FROM events WHERE network_id=? AND hash=?",
            params![network_id, envelope.hash],
            |row| row.get(0),
        )?;
        Ok(IngestOutcome {
            hash: envelope.hash.clone(),
            status,
            missing: self.missing_dependencies(network_id, std::slice::from_ref(&envelope.hash))?,
        })
    }
    pub fn projection_generation(&self, network_id: &str) -> Result<u64> {
        Ok(self
            .local_value(network_id, "net1:v1:projection-generation")?
            .and_then(|value| value.as_u64())
            .unwrap_or(0))
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
        let envelope = append_event_from(
            &tx,
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
    #[expect(
        clippy::too_many_arguments,
        reason = "the transaction binds its full authorization and durable record identity"
    )]
    pub fn accept_message(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        conversation: &str,
        body: EventBody,
        blobs: Vec<BlobRef>,
        created_at_ms: i64,
        local_node: &str,
        target: &str,
        report_results: bool,
    ) -> Result<(SignedEvent, AdmissionReceipt)> {
        let EventBody::MessagePosted {
            message_id,
            role,
            scheduler_node,
            ..
        } = &body
        else {
            bail!("message admission requires a user message");
        };
        ensure!(
            role == "user" && scheduler_node.as_deref() == Some(target),
            "message target or role mismatch"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize_node_from(&tx, network, &identity.peer_id()?, Some(NodeRole::Gateway))?;
        let previous: Option<String> = tx.query_row(
            "SELECT envelope FROM events WHERE network_id=? AND scope_id=? AND json_extract(envelope,'$.canonical_body') IS NOT NULL AND json_extract(json_extract(envelope,'$.canonical_body'),'$.body.message_id')=?",
            params![network, conversation, message_id], |r| r.get(0),
        ).optional()?;
        let envelope = if let Some(previous) = previous {
            let envelope: SignedEvent = serde_json::from_str(&previous)?;
            let event: Event = serde_json::from_str(&envelope.canonical_body)?;
            ensure!(
                event.body == body && event.blobs == blobs,
                "message ID reused with different content or target"
            );
            envelope
        } else {
            append_event_from(
                &tx,
                identity,
                network,
                conversation,
                body,
                blobs,
                created_at_ms,
            )?
        };
        followup::append_report_intent_from(
            &tx,
            identity,
            network,
            conversation,
            &envelope,
            target,
            report_results,
            created_at_ms,
        )?;
        let receipt = admit_scheduler_from(
            &tx,
            network,
            conversation,
            &envelope.hash,
            local_node,
            target,
            JobState::Queued,
        )?;
        tx.commit()?;
        Ok((envelope, receipt))
    }

    /// Call only after the runtime verifies autonomous continuation authority.
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
            network,
            conversation,
            trigger,
            local_node,
            target,
            JobState::Queued,
        )?;
        tx.commit()?;
        Ok(receipt)
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
            "SELECT value FROM local_values WHERE network_id=? AND key LIKE 'scheduler:v1:job:%'",
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
        let mut statement = self.connection.prepare("SELECT value FROM local_values WHERE network_id=? AND key LIKE 'scheduler:v1:outbox:%'")?;
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
            receipt.version == SCHEDULING_VERSION && receipt.network_id == network,
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
                    | "interrupted"
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
            entry.network_id == receipt.network_id
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

    pub fn claim_next(
        &mut self,
        network: &str,
        conversation: &str,
        target: &str,
        boot: &str,
        sdk_config: &str,
    ) -> Result<Option<ClaimedJob>> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = load_scheduler_session(&tx, network, conversation, target)?;
        recover_scheduler_session(&tx, network, &mut session, boot)?;
        if session.active.is_some() {
            save_scheduler_session(&tx, network, &session)?;
            tx.commit()?;
            return Ok(None);
        }
        let mut queued = Vec::new();
        for trigger in &session.pending {
            let job: SchedulerJob = scheduler_load(
                &tx,
                network,
                &scheduler_key("job", conversation, target, trigger),
            )?
            .context("missing queued scheduler job")?;
            ensure!(
                matches!(job.state, JobState::Queued),
                "pending scheduler job is not queued"
            );
            queued.push(job);
        }
        queued.sort_by_key(|job| (job.kind != TriggerKind::Human, job.sequence));
        let mut selected = None;
        for mut candidate in queued {
            if candidate.kind == TriggerKind::AgentResult
                && agent_messages::result_consumed_by_parent(
                    &tx,
                    network,
                    conversation,
                    &candidate.trigger,
                    target,
                )?
            {
                candidate.state = JobState::Cancelled {
                    attempt: None,
                    evidence: "result consumed by parent task; no separate report needed".into(),
                };
                session
                    .pending
                    .retain(|pending| pending != &candidate.trigger);
                scheduler_save(
                    &tx,
                    network,
                    &scheduler_key("job", conversation, target, &candidate.trigger),
                    &candidate,
                )?;
                continue;
            }
            let policy = if matches!(
                candidate.kind,
                TriggerKind::Autonomous | TriggerKind::AgentResult
            ) {
                match followup::validate_followup_result_from(
                    &tx,
                    network,
                    conversation,
                    &candidate.trigger,
                    target,
                )
                .and_then(followup::ResultFollowupPolicy::authorized)
                {
                    Ok(policy) => Some(policy),
                    Err(error) => {
                        candidate.state = JobState::Failed {
                            attempt: ManagerAttempt {
                                id: uuid::Uuid::new_v4().to_string(),
                                boot: boot.into(),
                                read_only: true,
                                permission_version: session.permission_version,
                                followup_version: None,
                            },
                            detail: format!(
                                "follow-up permission no longer authorizes this trigger: {error}"
                            ),
                        };
                        session
                            .pending
                            .retain(|trigger| trigger != &candidate.trigger);
                        scheduler_save(
                            &tx,
                            network,
                            &scheduler_key("job", conversation, target, &candidate.trigger),
                            &candidate,
                        )?;
                        continue;
                    }
                }
            } else {
                None
            };
            selected = Some((candidate, policy));
            break;
        }
        let Some((mut job, policy)) = selected else {
            save_scheduler_session(&tx, network, &session)?;
            tx.commit()?;
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
        let read_only = policy
            .as_ref()
            .is_some_and(|policy| policy.mode != followup::FollowupMode::Continue);
        let previous_native_id = if read_only {
            None
        } else {
            session.native_id.clone()
        };
        job.state = JobState::Claimed {
            attempt: ManagerAttempt {
                id: uuid::Uuid::new_v4().to_string(),
                boot: boot.into(),
                read_only,
                permission_version: session.permission_version,
                followup_version: policy.map(|policy| policy.version),
            },
        };
        session.pending.retain(|trigger| trigger != &job.trigger);
        session.active = Some(job.trigger.clone());
        scheduler_save(
            &tx,
            network,
            &scheduler_key("job", conversation, target, &job.trigger),
            &job,
        )?;
        save_scheduler_session(&tx, network, &session)?;
        tx.commit()?;
        Ok(Some(ClaimedJob {
            job,
            previous_native_id,
        }))
    }

    pub fn mark_started(
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
            |job, _| {
                let JobState::Claimed { attempt } = &job.state else {
                    bail!("scheduler job is not claimed");
                };
                job.state = JobState::Running {
                    attempt: attempt.clone(),
                };
                Ok(())
            },
        )
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
            && (!write || !attempt.read_only))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the transaction binds its full authorization and durable record identity"
    )]
    pub fn finish_job(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        conversation: &str,
        target: &str,
        trigger: &str,
        attempt_id: &str,
        outcome: JobOutcome,
        events: Vec<(EventBody, Vec<BlobRef>)>,
        created_at_ms: i64,
    ) -> Result<SchedulerJob> {
        ensure!(
            identity.peer_id()? == target,
            "terminal publication requires its target scheduler identity"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session = load_scheduler_session(&tx, network, conversation, target)?;
        let key = scheduler_key("job", conversation, target, trigger);
        let job: SchedulerJob =
            scheduler_load(&tx, network, &key)?.context("scheduler job missing")?;
        ensure!(
            job.state
                .attempt()
                .is_some_and(|attempt| attempt.id == attempt_id),
            "scheduler attempt does not match"
        );
        if matches!(
            job.state,
            JobState::Completed { .. }
                | JobState::Failed { .. }
                | JobState::Cancelled { .. }
                | JobState::Interrupted { .. }
        ) {
            return Ok(job);
        }
        ensure!(
            session.active.as_deref() == Some(trigger),
            "scheduler attempt is no longer active"
        );
        if scheduler_publication_from(&tx, network, &job)?.is_none() {
            validate_scheduler_terminal(&job, &outcome, &events)?;
            let marker = SchedulerTerminalPublication {
                version: SCHEDULING_VERSION,
                job: job.clone(),
                outcome,
                events,
                created_at_ms,
            };
            scheduler_save(
                &tx,
                network,
                &scheduler_key("publication", conversation, target, trigger),
                &marker,
            )?;
        }
        tx.commit()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let published = publish_scheduler_terminal_from(&tx, identity, network, &job)?;
        tx.commit()?;
        Ok(published.job)
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

    pub fn flush_scheduler_publications(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        target: &str,
    ) -> Result<SchedulerPublicationBatch> {
        ensure!(
            identity.peer_id()? == target,
            "terminal publication requires its target scheduler identity"
        );
        let mut statement = self.connection.prepare("SELECT value FROM local_values WHERE network_id=? AND key LIKE 'scheduler:v1:publication:%'")?;
        let encoded = statement
            .query_map([network], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        let mut markers = Vec::new();
        for value in encoded {
            let marker: SchedulerTerminalPublication = serde_json::from_str(&value)?;
            ensure!(
                marker.version == SCHEDULING_VERSION,
                "unsupported scheduler publication version"
            );
            if marker.job.target_node == target {
                markers.push(marker);
            }
        }
        let mut batch = SchedulerPublicationBatch::default();
        for marker in markers {
            let publication: Result<Option<SchedulerPublication>> = (|| {
                let tx = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                let job: SchedulerJob = scheduler_load(
                    &tx,
                    network,
                    &scheduler_key(
                        "job",
                        &marker.job.conversation_id,
                        target,
                        &marker.job.trigger,
                    ),
                )?
                .context("scheduler publication job missing")?;
                if scheduler_publication_from(&tx, network, &job)?.is_none() {
                    return Ok(None);
                }
                if !scheduler_scope_publishable(&tx, network, &job.conversation_id)? {
                    return Ok(None);
                }
                let publication = publish_scheduler_terminal_from(&tx, identity, network, &job)?;
                tx.commit()?;
                Ok(Some(publication))
            })();
            match publication {
                Ok(Some(publication)) => batch.published.push(publication),
                Ok(None) => {}
                Err(error) => batch.errors.push(format!(
                    "{}:{}: {error:#}",
                    marker.job.conversation_id, marker.job.trigger
                )),
            }
        }
        Ok(batch)
    }

    pub fn record_manager_process(
        &mut self,
        network: &str,
        attempt_id: &str,
        process_group: u32,
    ) -> Result<()> {
        let key = manager_process_key(attempt_id);
        let mut groups: Vec<u32> = scheduler_load::<Value>(&self.connection, network, &key)?
            .map(|value| serde_json::from_value(value["process_groups"].clone()))
            .transpose()?
            .unwrap_or_default();
        if !groups.contains(&process_group) {
            groups.push(process_group);
        }
        scheduler_save(
            &self.connection,
            network,
            &key,
            &serde_json::json!({"process_groups":groups}),
        )
    }

    pub fn pending_checks(&self, network: &str) -> Result<Vec<PendingCheck>> {
        pending_checks_from(&self.connection, network)
    }

    pub fn pending_check(&self, network: &str, check_id: &str) -> Result<Option<PendingCheck>> {
        Ok(self
            .pending_checks(network)?
            .into_iter()
            .find(|check| check.check_id == check_id))
    }

    /// Runtime verifies the user, the stopped process and linked native turns before this.
    pub fn resolve_pending_check(
        &mut self,
        network: &str,
        check_id: &str,
        evidence: &str,
        at: i64,
    ) -> Result<PendingCheck> {
        ensure!(
            !evidence.trim().is_empty(),
            "reconciliation evidence is required"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let check = pending_checks_from(&tx, network)?
            .into_iter()
            .find(|check| check.check_id == check_id)
            .context("pending check not found")?;
        let key = scheduler_key(
            "pending",
            &check.conversation_id,
            &check.target_node,
            &check.trigger,
        );
        tx.execute(
            "DELETE FROM local_values WHERE network_id=? AND key=?",
            params![network, key],
        )?;
        scheduler_save(
            &tx,
            network,
            &scheduler_key(
                "resolution",
                &check.conversation_id,
                &check.target_node,
                &check.trigger,
            ),
            &serde_json::json!({"check_id":check_id,"attempt_id":check.attempt_id,"evidence":evidence,"resolved_at_ms":at}),
        )?;
        tx.commit()?;
        Ok(check)
    }

    /// Publishes the deferred result of an interrupted agent task once nothing it started awaits reconciliation.
    pub(crate) fn publish_interrupted_task_result(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        job: &SchedulerJob,
        at: i64,
    ) -> Result<Option<String>> {
        let JobState::Interrupted { detail, .. } = &job.state else {
            return Ok(None);
        };
        if job.kind != TriggerKind::AgentTask {
            return Ok(None);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: SchedulerJob = scheduler_load(
            &tx,
            network,
            &scheduler_key("job", &job.conversation_id, &job.target_node, &job.trigger),
        )?
        .context("interrupted task job is missing")?;
        ensure!(
            current == *job,
            "interrupted task changed before publication"
        );
        if agent_messages::agent_result_published_from(&tx, network, job)?
            || pending_checks_from(&tx, network)?.iter().any(|check| {
                check.trigger == job.trigger && check.conversation_id == job.conversation_id
            })
        {
            return Ok(None);
        }
        let hash = agent_messages::publish_agent_result_from(
            &tx,
            identity,
            network,
            job,
            &JobOutcome::Cancelled(detail.clone()),
            at,
        )?;
        tx.commit()?;
        Ok(hash)
    }

    pub fn scheduler_maintenance_idle(&self, network: &str) -> Result<()> {
        for session in scheduler_sessions(&self.connection, network)? {
            ensure!(
                session.pending.is_empty() && session.active.is_none(),
                crate::maintenance::Busy(
                    "scheduler invocation requires completion or reconciliation".into()
                )
            );
        }
        ensure!(
            self.scheduler_outbox(network)?.is_empty(),
            crate::maintenance::Busy("scheduler delivery is pending".into())
        );
        ensure!(
            self.pending_checks(network)?.is_empty(),
            crate::maintenance::Busy("an interrupted invocation awaits reconciliation".into())
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
    fn load_events(&self, network_id: &str) -> Result<BTreeMap<String, StoredEvent>> {
        load_events_from(&self.connection, network_id)
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
    pub fn heads(&self, network_id: &str, scope_id: &str) -> Result<Vec<EventHash>> {
        Ok(heads_from_events(&self.load_events(network_id)?, scope_id))
    }
    pub(crate) fn inventory_heads(&self, network: &str) -> Result<Vec<(ScopeId, Vec<EventHash>)>> {
        let events = self.load_events(network)?;
        let mut scopes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (hash, entry) in &events {
            scopes
                .entry(entry.event.scope_id.clone())
                .or_default()
                .insert(hash.clone());
        }
        for entry in events.values() {
            if let Some(heads) = scopes.get_mut(&entry.event.scope_id) {
                for parent in dependencies(&entry.event) {
                    heads.remove(parent);
                }
            }
        }
        Ok(scopes
            .into_iter()
            .map(|(scope, heads)| (scope, heads.into_iter().collect()))
            .collect())
    }
    pub(crate) fn has_known_basis_gap(
        &self,
        network_id: &str,
        heads: &[EventHash],
    ) -> Result<bool> {
        let mut query = self
            .connection
            .prepare("SELECT status FROM events WHERE network_id=? AND hash=?")?;
        for hash in heads {
            let status: Option<String> = query
                .query_row(params![network_id, hash], |row| row.get(0))
                .optional()?;
            if status.as_deref() != Some("ready") {
                return Ok(true);
            }
        }
        Ok(false)
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
        for hash in &snapshot.heads {
            let envelope = snapshot
                .events
                .iter()
                .find(|event| &event.hash == hash)
                .cloned()
                .or(self.event(network_id, hash)?);
            if let Some(envelope) = envelope {
                let body: Event = serde_json::from_str(&envelope.canonical_body)?;
                ensure!(
                    body.scope_id == snapshot.scope_id,
                    "snapshot head scope mismatch"
                );
            }
        }
        let mut start = 0;
        while start < snapshot.events.len() {
            let mut end = start;
            let mut bytes = 0;
            while end < snapshot.events.len() && end - start < 1024 {
                let size = snapshot.events[end].canonical_body.len();
                if bytes + size > 8 * MAX_EVENT_BYTES {
                    break;
                }
                bytes += size;
                end += 1;
            }
            self.ingest_events(network_id, &snapshot.events[start..end])?;
            start = end;
        }
        self.missing_dependencies(network_id, &snapshot.heads)
    }
    pub fn put_blob(&mut self, network_id: &str, expected: &BlobRef, content: &[u8]) -> Result<()> {
        ensure!(
            content.len() <= MAX_BLOB_BYTES && content.len() as u64 == expected.size,
            "blob size mismatch or limit exceeded"
        );
        ensure!(digest(content) == expected.hash, "blob hash mismatch");
        if self.has_blob(network_id, expected)? {
            return Ok(());
        }
        self.blob_readers.retire_blob(network_id, &expected.hash);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO blobs(network_id,hash,size,content) VALUES(?,?,?,?) ON CONFLICT(network_id,hash) DO UPDATE SET size=excluded.size,content=excluded.content",
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
                "SELECT content FROM blobs WHERE network_id=? AND hash=?",
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
    pub(crate) fn get_blob_for_ref(
        &self,
        network: &str,
        expected: &BlobRef,
    ) -> Result<Option<Vec<u8>>> {
        get_blob_for_ref_from(&self.connection, network, expected)
    }
    pub(crate) fn context_blob_ref(
        &self,
        network: &str,
        scope: &str,
        hash: &str,
    ) -> Result<BlobRef> {
        let events = load_events_from(&self.connection, network)?;
        let reachable = closure_hashes(&events, &heads_from_events(&events, scope));
        let mut expected: Option<BlobRef> = None;
        let mut ready = false;
        for candidate in reachable.iter().filter_map(|hash| events.get(hash)) {
            if candidate.event.scope_id != scope {
                continue;
            }
            for reference in candidate
                .event
                .blobs
                .iter()
                .filter(|reference| reference.hash == hash)
            {
                ensure!(
                    expected
                        .as_ref()
                        .is_none_or(|expected| expected == reference),
                    "conflicting blob reference sizes"
                );
                expected = Some(reference.clone());
                if candidate.status == "ready" {
                    let dependencies: Vec<_> = dependencies(&candidate.event).cloned().collect();
                    ready |= closure_hashes(&events, &dependencies).iter().all(|hash| {
                        events
                            .get(hash)
                            .is_some_and(|event| event.status == "ready")
                    });
                }
            }
        }
        let expected = expected.context("blob outside this conversation")?;
        ensure!(ready, "blob reference has incomplete causal dependencies");
        ensure!(
            blob_metadata_matches(&self.connection, network, &expected)?,
            "blob not available or metadata mismatch"
        );
        Ok(expected)
    }
    pub(crate) fn attach_process_lock(&mut self, lock: Arc<std::fs::File>) {
        self.process_lock = Some(lock);
    }
    #[cfg(test)]
    pub(crate) fn test_durability(&self) -> Result<(String, u32)> {
        Ok((
            self.connection
                .query_row("PRAGMA journal_mode", [], |row| row.get(0))?,
            self.connection
                .query_row("PRAGMA synchronous", [], |row| row.get(0))?,
        ))
    }
    pub(crate) fn blob_range(
        &mut self,
        network: &str,
        local_node: &str,
        origin: &str,
        hash: &str,
        offset: u64,
        limit: usize,
    ) -> Result<Option<(u64, Vec<u8>)>> {
        self.authorize_node(network, local_node, Some(NodeRole::Replica))?;
        self.authorize_node(network, origin, Some(NodeRole::Replica))?;
        ensure!((1..=65536).contains(&limit), "invalid blob range limit");
        let Some(metadata) = blob_reader::metadata(&self.connection, network, hash)? else {
            return Ok(None);
        };
        ensure!(offset <= metadata.size, "invalid blob offset");
        let offset = usize::try_from(offset)?;
        let count = limit.min(usize::try_from(metadata.size)? - offset);
        if count == 0 {
            return Ok(Some((metadata.size, Vec::new())));
        }
        ensure!(
            self.path != Path::new(":memory:"),
            "blob ranges require a file database"
        );
        let lock = self
            .process_lock
            .as_ref()
            .context("blob reader has no data directory lock")?;
        let key = blob_reader::ReaderKey::new(network, origin, hash, metadata);
        let bytes = self
            .blob_readers
            .read(&self.path, lock, key, offset, count)?;
        Ok(Some((metadata.size, bytes)))
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
        let mut statement=self.connection.prepare("SELECT conversation_id,title FROM conversations WHERE network_id=? ORDER BY conversation_id")?;
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
    pub(crate) fn has_conversation(&self, network_id: &str, scope_id: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversations WHERE network_id=? AND conversation_id=?)",
            params![network_id, scope_id],
            |row| row.get(0),
        )?)
    }
    pub fn messages(&self, network_id: &str, scope_id: &str) -> Result<Vec<Message>> {
        let mut statement=self.connection.prepare("SELECT message_id,role,text,event_hash,created_at_ms FROM messages WHERE network_id=? AND conversation_id=? ORDER BY ordinal")?;
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
    pub fn local_value(&self, network_id: &str, key: &str) -> Result<Option<Value>> {
        optional_json(
            self.connection
                .query_row(
                    "SELECT value FROM local_values WHERE network_id=? AND key=?",
                    params![network_id, key],
                    |r| r.get(0),
                )
                .optional()?,
        )
    }
    pub fn local_set(&mut self, network_id: &str, key: &str, value: &Value) -> Result<()> {
        self.connection.execute("INSERT INTO local_values(network_id,key,value) VALUES(?,?,?) ON CONFLICT(network_id,key) DO UPDATE SET value=excluded.value",params![network_id,key,serde_json::to_string(value)?])?;
        Ok(())
    }
    pub fn local_delete(&mut self, network_id: &str, key: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM local_values WHERE network_id=? AND key=?",
            params![network_id, key],
        )?;
        Ok(())
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
                "SELECT record FROM notification_outbox WHERE network_id=? AND notification_id=?",
                params![network_id, notification.notification_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            ensure!(old == encoded, "notification ID reused with different body");
        } else {
            tx.execute(
                "INSERT INTO notification_outbox(network_id,notification_id,record) VALUES(?,?,?)",
                params![network_id, notification.notification_id, encoded],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn raise_notification_once(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        notification: Notification,
        at: i64,
    ) -> Result<Option<SignedEvent>> {
        ensure!(
            !notification.notification_id.is_empty(),
            "notification ID required"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT record FROM notification_outbox WHERE network_id=? AND notification_id=?",
                params![network, notification.notification_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            let old: Notification = serde_json::from_str(&old)?;
            ensure!(
                old.conversation_id == notification.conversation_id
                    && old.kind == notification.kind
                    && old.text == notification.text,
                "notification ID reused with different body"
            );
            return Ok(None);
        }
        let event = append_event_from(
            &tx,
            identity,
            network,
            &notification.conversation_id,
            EventBody::NotificationRaised {
                notification_id: notification.notification_id,
                kind: notification.kind,
                text: notification.text,
            },
            vec![],
            at,
        )?;
        tx.commit()?;
        Ok(Some(event))
    }
    pub fn pending_notifications(
        &self,
        network_id: &str,
        client_id: &str,
    ) -> Result<Vec<Notification>> {
        let mut statement=self.connection.prepare("SELECT n.record FROM notification_outbox n WHERE n.network_id=? AND NOT EXISTS(SELECT 1 FROM notification_acks a WHERE a.network_id=n.network_id AND a.notification_id=n.notification_id AND a.client_id=?) ORDER BY n.notification_id")?;
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
        let exists:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM notification_outbox WHERE network_id=? AND notification_id=?)",params![network_id,notification_id],|r|r.get(0))?;
        ensure!(exists, "unknown notification");
        self.connection.execute("INSERT INTO notification_acks(network_id,notification_id,client_id,event_hash) VALUES(?,?,?,'') ON CONFLICT(network_id,notification_id,client_id) DO UPDATE SET event_hash=''",params![network_id,notification_id,client_id])?;
        Ok(())
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
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT scope_id FROM events WHERE network_id=? ORDER BY scope_id")?;
        Ok(statement
            .query_map([network_id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
}
fn scheduler_publication_from(
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
    network: &str,
    conversation: &str,
) -> Result<bool> {
    let events = load_events_from(connection, network)?;
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

fn publish_scheduler_terminal_from(
    connection: &Connection,
    identity: &LocalIdentity,
    network: &str,
    expected: &SchedulerJob,
) -> Result<SchedulerPublication> {
    let key = scheduler_key(
        "job",
        &expected.conversation_id,
        &expected.target_node,
        &expected.trigger,
    );
    let mut job: SchedulerJob =
        scheduler_load(connection, network, &key)?.context("scheduler publication job missing")?;
    ensure!(
        job == *expected,
        "scheduler invocation changed before terminal publication"
    );
    let marker = scheduler_publication_from(connection, network, &job)?
        .context("scheduler publication marker missing")?;
    validate_scheduler_terminal(&job, &marker.outcome, &marker.events)?;
    let mut session =
        load_scheduler_session(connection, network, &job.conversation_id, &job.target_node)?;
    ensure!(
        session.active.as_deref() == Some(&job.trigger),
        "scheduler publication invocation is no longer active"
    );
    let attempt = job
        .state
        .attempt()
        .context("scheduler publication attempt missing")?
        .clone();
    let mut event_hashes = Vec::new();
    for (body, blobs) in marker.events {
        event_hashes.push(
            append_event_from(
                connection,
                identity,
                network,
                &job.conversation_id,
                body,
                blobs,
                marker.created_at_ms,
            )?
            .hash,
        );
    }
    let task_outcome = marker.outcome.clone();
    job.state = match marker.outcome {
        JobOutcome::Completed => JobState::Completed { attempt },
        JobOutcome::Failed(detail) => JobState::Failed { attempt, detail },
        JobOutcome::Cancelled(evidence) => JobState::Cancelled {
            attempt: Some(attempt),
            evidence,
        },
        JobOutcome::Interrupted {
            detail,
            process_unconfirmed,
        } => {
            interrupt_session(&mut session)?;
            if process_unconfirmed {
                save_pending_check(
                    connection,
                    network,
                    &job,
                    &attempt,
                    &detail,
                    marker.created_at_ms,
                )?;
            }
            JobState::Interrupted { attempt, detail }
        }
    };
    session.active = None;
    forget_manager_process(connection, network, job.state.attempt())?;
    scheduler_save(connection, network, &key, &job)?;
    save_scheduler_session(connection, network, &session)?;
    if let Some(hash) = agent_messages::publish_agent_result_from(
        connection,
        identity,
        network,
        &job,
        &task_outcome,
        marker.created_at_ms,
    )? {
        event_hashes.push(hash);
    }
    connection.execute(
        "DELETE FROM local_values WHERE network_id=? AND key=?",
        params![
            network,
            scheduler_key(
                "publication",
                &job.conversation_id,
                &job.target_node,
                &job.trigger
            )
        ],
    )?;
    Ok(SchedulerPublication { job, event_hashes })
}

fn scheduler_load<T: DeserializeOwned>(
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

fn scheduler_save(
    connection: &Connection,
    network: &str,
    key: &str,
    value: &impl serde::Serialize,
) -> Result<()> {
    connection.execute("INSERT INTO local_values(network_id,key,value) VALUES(?,?,?) ON CONFLICT(network_id,key) DO UPDATE SET value=excluded.value", params![network,key,serde_json::to_string(value)?])?;
    Ok(())
}

fn load_scheduler_session(
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

fn save_scheduler_session(
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
        "SELECT value FROM local_values WHERE network_id=? AND key LIKE 'scheduler:v1:session:%'",
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
    network: &str,
    conversation: &str,
    trigger: &str,
    target: &str,
) -> Result<TriggerKind> {
    authorize_node_from(connection, network, target, Some(NodeRole::Scheduler))?;
    let encoded: String = connection
        .query_row(
            "SELECT envelope FROM events WHERE network_id=? AND hash=? AND status='ready'",
            params![network, trigger],
            |r| r.get(0),
        )
        .optional()?
        .context("scheduler trigger is missing or not ready")?;
    let envelope: SignedEvent = serde_json::from_str(&encoded)?;
    let event: Event = serde_json::from_str(&envelope.canonical_body)?;
    ensure!(
        event.scope_id == conversation,
        "scheduler trigger belongs to another conversation"
    );
    let certificate = authorize_node_from(connection, network, &event.author, None)?;
    verify_signed_event(network, &envelope, &certificate.public_key)?;
    match &event.body {
        EventBody::MessagePosted {
            role,
            scheduler_node,
            ..
        } if role == "user" => {
            authorize_node_from(connection, network, &event.author, Some(NodeRole::Gateway))?;
            ensure!(
                scheduler_node.as_deref() == Some(target),
                "message is addressed to another scheduler"
            );
            Ok(TriggerKind::Human)
        }
        EventBody::AgentMessage { message } => agent_messages::validate_agent_trigger_from(
            connection, network, &event, message, target,
        ),
        _ => bail!("invalid scheduler trigger"),
    }
}

fn admit_scheduler_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    trigger: &str,
    local_node: &str,
    target: &str,
    state: JobState,
) -> Result<AdmissionReceipt> {
    let kind = validated_scheduler_trigger(connection, network, conversation, trigger, target)?;
    let queued = match &state {
        JobState::Queued => true,
        JobState::Cancelled {
            attempt: None,
            evidence,
        } if !evidence.is_empty() && local_node == target => false,
        _ => bail!("scheduler admission requires queued or unstarted cancelled state"),
    };
    if local_node != target {
        let key = scheduler_key("outbox", conversation, target, trigger);
        if let Some(entry) = scheduler_load::<OutboxEntry>(connection, network, &key)? {
            return Ok(entry.receipt());
        }
        let count: usize = connection.query_row("SELECT COUNT(*) FROM local_values WHERE network_id=? AND key LIKE 'scheduler:v1:outbox:%' AND json_extract(value,'$.delivered')=0", [network], |r| r.get(0))?;
        ensure!(
            count < MAX_PENDING_JOBS,
            "scheduler delivery queue is full; message was not admitted"
        );
        let entry = OutboxEntry {
            version: SCHEDULING_VERSION,
            network_id: network.into(),
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
    if queued {
        ensure!(
            session.pending.len() < MAX_PENDING_JOBS,
            "scheduler inbox is full; trigger was not admitted"
        );
    }
    let job = SchedulerJob {
        version: SCHEDULING_VERSION,
        conversation_id: conversation.into(),
        trigger: trigger.into(),
        target_node: target.into(),
        kind,
        sequence: session.next_sequence,
        basis_heads: {
            let mut heads = vec![trigger.into()];
            if let Some(head) =
                followup::report_intent_head_from(connection, network, conversation, trigger)?
            {
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
        state,
    };
    session.next_sequence = session
        .next_sequence
        .checked_add(1)
        .context("scheduler queue sequence overflow")?;
    if queued {
        session.pending.push(trigger.into());
    }
    scheduler_save(connection, network, &key, &job)?;
    save_scheduler_session(connection, network, &session)?;
    Ok(job.receipt(network))
}

fn recover_scheduler_session(
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
            let detail = "Node restarted after the SDK start boundary; this call was not replayed";
            save_pending_check(
                connection,
                network,
                &job,
                &attempt,
                detail,
                crate::runtime::now_ms(),
            )?;
            job.state = JobState::Interrupted {
                attempt,
                detail: detail.into(),
            };
            interrupt_session(session)?;
            forget_manager_process(connection, network, job.state.attempt())?;
            session.active = None;
        }
        _ => session.active = None,
    }
    scheduler_save(connection, network, &key, &job)?;
    Ok(())
}

fn interrupt_session(session: &mut SchedulerSession) -> Result<()> {
    session.native_id = None;
    session.permission_version = session
        .permission_version
        .checked_add(1)
        .context("scheduler permission version overflow")?;
    Ok(())
}

fn manager_process_key(attempt_id: &str) -> String {
    format!("manager-process:v1:{attempt_id}")
}

fn forget_manager_process(
    connection: &Connection,
    network: &str,
    attempt: Option<&ManagerAttempt>,
) -> Result<()> {
    if let Some(attempt) = attempt {
        connection.execute(
            "DELETE FROM local_values WHERE network_id=? AND key=?",
            params![network, manager_process_key(&attempt.id)],
        )?;
    }
    Ok(())
}

fn save_pending_check(
    connection: &Connection,
    network: &str,
    job: &SchedulerJob,
    attempt: &ManagerAttempt,
    detail: &str,
    at: i64,
) -> Result<()> {
    let process: Option<Value> =
        scheduler_load(connection, network, &manager_process_key(&attempt.id))?;
    let process_groups = process
        .map(|value| serde_json::from_value(value["process_groups"].clone()))
        .transpose()?
        .unwrap_or_default();
    scheduler_save(
        connection,
        network,
        &scheduler_key(
            "pending",
            &job.conversation_id,
            &job.target_node,
            &job.trigger,
        ),
        &PendingCheck {
            version: SCHEDULING_VERSION,
            check_id: PendingCheck::id(&attempt.id),
            conversation_id: job.conversation_id.clone(),
            target_node: job.target_node.clone(),
            trigger: job.trigger.clone(),
            attempt_id: attempt.id.clone(),
            detail: detail.into(),
            process_groups,
            created_at_ms: at,
        },
    )
}

fn pending_checks_from(connection: &Connection, network: &str) -> Result<Vec<PendingCheck>> {
    let mut statement = connection.prepare(
        "SELECT value FROM local_values WHERE network_id=? AND key LIKE 'scheduler:v1:pending:%'",
    )?;
    let rows = statement.query_map([network], |r| r.get::<_, String>(0))?;
    let mut checks = Vec::new();
    for row in rows {
        let check: PendingCheck = serde_json::from_str(&row?)?;
        ensure!(
            check.version == SCHEDULING_VERSION,
            "unsupported pending check version"
        );
        checks.push(check);
    }
    checks.sort_by_key(|check| check.created_at_ms);
    Ok(checks)
}

fn append_event_from(
    connection: &Connection,
    identity: &LocalIdentity,
    network_id: &str,
    scope_id: &str,
    body: EventBody,
    blobs: Vec<BlobRef>,
    created_at_ms: i64,
) -> Result<SignedEvent> {
    let author = identity.peer_id()?;
    authorize_node_from(connection, network_id, &author, None)?;
    let events = load_events_from(connection, network_id)?;
    let mut heads = heads_from_events(&events, scope_id);
    if let EventBody::AgentMessage { message } = &body {
        heads.push(message.origin_trigger.clone());
        heads.extend(message.in_reply_to.iter().cloned());
    }
    for hash in closure_hashes(&events, &heads) {
        let stored = events.get(&hash).context("scope has missing history")?;
        ensure!(
            stored.status == "ready",
            "scope has incomplete or conflicting history"
        );
        for blob in &stored.event.blobs {
            ensure!(
                has_blob_from(connection, network_id, blob)?,
                "scope has missing blob content"
            );
        }
    }
    let sequence:u64=connection.query_row("SELECT COALESCE(MAX(sequence),0)+1 FROM events WHERE network_id=? AND scope_id=? AND author=?",params![network_id,scope_id,author],|r|r.get(0))?;
    let previous: Option<String> = if sequence == 1 {
        None
    } else {
        Some(connection.query_row("SELECT hash FROM events WHERE network_id=? AND scope_id=? AND author=? AND sequence=? AND status='ready'",params![network_id,scope_id,author,sequence-1],|r|r.get(0))?)
    };
    for blob in &blobs {
        ensure!(
            has_blob_from(connection, network_id, blob)?,
            "required blob missing"
        );
    }
    let envelope = identity.sign_event(Event {
        version: WIRE_VERSION,
        network_id: network_id.into(),
        scope_id: scope_id.into(),
        author: author.clone(),
        sequence,
        previous,
        parents: heads,
        created_at_ms,
        body,
        blobs,
    })?;
    connection.execute("INSERT INTO events(network_id,hash,scope_id,author,sequence,envelope,status) VALUES(?,?,?,?,?,?,'ready')",params![network_id,envelope.hash,scope_id,author,sequence,serde_json::to_string(&envelope)?])?;
    rebuild_projections_from(connection, network_id)?;
    Ok(envelope)
}

fn rebuild_projections_from(connection: &Connection, network_id: &str) -> Result<()> {
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
        if blobs_complete {
            events.get_mut(hash).unwrap().status = "ready".into();
        }
    }
    bots::validate_bot_projections_from(connection, network_id, &mut events, &ordered);
    for (hash, e) in &events {
        connection.execute(
            "UPDATE events SET status=? WHERE network_id=? AND hash=?",
            params![e.status, network_id, hash],
        )?;
    }
    connection.execute("DELETE FROM conversations WHERE network_id=?", [network_id])?;
    connection.execute("DELETE FROM messages WHERE network_id=?", [network_id])?;
    connection.execute(
        "DELETE FROM notification_outbox WHERE network_id=? AND derived=1",
        [network_id],
    )?;
    connection.execute(
        "DELETE FROM notification_acks WHERE network_id=? AND event_hash!=''",
        [network_id],
    )?;
    for (ordinal, hash) in ordered.iter().enumerate() {
        let stored = &events[hash];
        if stored.status != "ready" {
            continue;
        }
        let event = &stored.event;
        match &event.body {
            EventBody::ConversationCreated { title } => {
                connection.execute("INSERT OR IGNORE INTO conversations(network_id,conversation_id,title) VALUES(?,?,?)",params![network_id,event.scope_id,title])?;
            }
            EventBody::MessagePosted {
                message_id,
                role,
                text,
                ..
            } => {
                connection.execute("INSERT INTO messages(network_id,conversation_id,event_hash,message_id,role,text,created_at_ms,ordinal) VALUES(?,?,?,?,?,?,?,?)",params![network_id,event.scope_id,hash,message_id,role,text,event.created_at_ms,ordinal])?;
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
                connection.execute("INSERT OR IGNORE INTO notification_outbox(network_id,notification_id,record,derived) VALUES(?,?,?,1)",params![network_id,notification_id,serde_json::to_string(&notification)?])?;
            }
            EventBody::NotificationAcknowledged {
                notification_id,
                client_id,
            } => {
                connection.execute("INSERT OR IGNORE INTO notification_acks(network_id,notification_id,client_id,event_hash) VALUES(?,?,?,?)",params![network_id,notification_id,client_id,hash])?;
            }
            _ => {}
        }
    }
    let generation: u64 = connection.query_row(
        "SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM local_values WHERE network_id=? AND key='net1:v1:projection-generation'),0)",
        [network_id], |row| row.get(0),
    )?;
    connection.execute(
        "INSERT INTO local_values(network_id,key,value) VALUES(?,'net1:v1:projection-generation',?) ON CONFLICT(network_id,key) DO UPDATE SET value=excluded.value",
        params![network_id, generation.checked_add(1).context("projection generation exhausted")?.to_string()],
    )?;
    Ok(())
}
fn authorize_node_from(
    connection: &Connection,
    network_id: &str,
    peer_id: &str,
    role: Option<NodeRole>,
) -> Result<NodeCertificate> {
    let encoded: String = connection
        .query_row(
            "SELECT bundle FROM networks WHERE network_id=?",
            [network_id],
            |r| r.get(0),
        )
        .optional()?
        .context("unknown network")?;
    let state: NetworkState = serde_json::from_str(&encoded)?;
    let view = state.project()?;
    ensure!(
        view.network_id == network_id,
        "stored membership network mismatch"
    );
    view.authorize_node(peer_id, role)?.certificate()
}

fn heads_from_events(events: &BTreeMap<String, StoredEvent>, scope_id: &str) -> Vec<String> {
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
fn load_events_from(
    connection: &Connection,
    network_id: &str,
) -> Result<BTreeMap<String, StoredEvent>> {
    let mut query = connection
        .prepare("SELECT hash,envelope,status,scope_id,author,sequence FROM events WHERE network_id=? ORDER BY hash")?;
    let rows = query.query_map([network_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, u64>(5)?,
        ))
    })?;
    let mut result = BTreeMap::new();
    for row in rows {
        let (hash, envelope, status, scope, author, sequence) = row?;
        let envelope: SignedEvent = serde_json::from_str(&envelope)?;
        ensure!(envelope.hash == hash, "stored event hash mismatch");
        let body: Event = serde_json::from_str(&envelope.canonical_body)?;
        let public_key: String = connection.query_row(
            "SELECT public_key FROM node_certificates WHERE network_id=? AND peer_id=?",
            params![network_id, body.author],
            |r| r.get(0),
        )?;
        let event = verify_signed_event(network_id, &envelope, &public_key)?;
        ensure!(
            event.scope_id == scope && event.author == author && event.sequence == sequence,
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

fn has_blob_from(connection: &Connection, network_id: &str, blob: &BlobRef) -> Result<bool> {
    if blob.size > MAX_BLOB_BYTES as u64 {
        return Ok(false);
    }
    let content: Option<Vec<u8>> = connection
        .query_row(
            "SELECT content FROM blobs WHERE network_id=? AND hash=? AND size=? AND length(content)=? AND typeof(content)='blob'",
            params![network_id, blob.hash, blob.size, blob.size],
            |r| r.get(0),
        )
        .optional()?;
    Ok(content.is_some_and(|c| {
        c.len() as u64 == blob.size && c.len() <= MAX_BLOB_BYTES && digest(&c) == blob.hash
    }))
}

fn blob_metadata_matches(
    connection: &Connection,
    network: &str,
    expected: &BlobRef,
) -> Result<bool> {
    ensure!(
        expected.size <= MAX_BLOB_BYTES as u64,
        "blob size limit exceeded"
    );
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM blobs WHERE network_id=? AND hash=? AND size=? AND length(content)=? AND typeof(content)='blob')",
        params![network, expected.hash, expected.size, expected.size], |row| row.get(0),
    )?)
}

fn get_blob_for_ref_from(
    connection: &Connection,
    network: &str,
    expected: &BlobRef,
) -> Result<Option<Vec<u8>>> {
    ensure!(
        expected.size <= MAX_BLOB_BYTES as u64,
        "blob size limit exceeded"
    );
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM blobs WHERE network_id=? AND hash=?)",
        params![network, expected.hash],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    ensure!(
        blob_metadata_matches(connection, network, expected)?,
        "stored blob metadata mismatch"
    );
    let bytes: Vec<u8> = connection.query_row(
        "SELECT content FROM blobs WHERE network_id=? AND hash=? AND size=? AND length(content)=? AND typeof(content)='blob'",
        params![network, expected.hash, expected.size, expected.size], |row| row.get(0),
    )?;
    ensure!(
        digest(&bytes) == expected.hash,
        "stored blob failed content verification"
    );
    Ok(Some(bytes))
}
fn optional_json<T: DeserializeOwned>(value: Option<String>) -> Result<Option<T>> {
    value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
}
fn dependencies(event: &Event) -> impl Iterator<Item = &String> {
    event.parents.iter().chain(event.previous.iter())
}
fn closure_hashes(events: &BTreeMap<String, StoredEvent>, heads: &[String]) -> BTreeSet<String> {
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
    use serde_json::json;
    #[test]
    fn blob_ranges_keep_isolated_cursors_and_retire_before_same_length_repairs() -> Result<()> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/network-reliability")
            .join(format!("range-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let lock = Arc::new(crate::config::lock_data_dir(&dir)?);
        let owner = LocalIdentity::generate();
        let node = owner.peer_id()?;
        let network = owner.fixture_network_id()?;
        let mut store = Store::open(dir.join("node.sqlite3"))?;
        store.attach_process_lock(lock.clone());
        store.install_trust_bundle(
            &network,
            &owner.sign_network_fixture(1, vec![owner.certificate(roles())?])?,
        )?;
        let bytes = vec![42; 256 * 1024];
        let blob = BlobRef {
            hash: digest(&bytes),
            size: bytes.len() as u64,
        };
        store.put_blob(&network, &blob, &bytes)?;
        for offset in (0..bytes.len()).step_by(65536) {
            let (size, chunk) = store
                .blob_range(&network, &node, &node, &blob.hash, offset as u64, 65536)?
                .unwrap();
            assert_eq!(size, blob.size);
            assert_eq!(chunk, bytes[offset..offset + 65536]);
            // Legal writes to the same table must not invalidate this reader's cursor/snapshot.
            let other_bytes = offset.to_le_bytes();
            let other = BlobRef {
                hash: digest(&other_bytes),
                size: other_bytes.len() as u64,
            };
            store.put_blob(&network, &other, &other_bytes)?;
        }
        assert_eq!(Arc::strong_count(&lock), 3);
        let newer = b"object committed after the first readonly snapshot";
        let second = BlobRef {
            hash: digest(newer),
            size: newer.len() as u64,
        };
        store.put_blob(&network, &second, newer)?;
        assert_eq!(
            store
                .blob_range(&network, &node, &node, &second.hash, 0, 64)?
                .unwrap()
                .1,
            newer
        );
        let third = BlobRef {
            hash: digest(b"third"),
            size: 5,
        };
        store.put_blob(&network, &third, b"third")?;
        assert!(
            store
                .blob_range(&network, &node, &node, &third.hash, 0, 5)
                .unwrap_err()
                .to_string()
                .contains("busy")
        );
        assert!(
            store
                .blob_range(&network, &node, "unknown", &blob.hash, 0, 1)
                .is_err()
        );
        assert!(
            store
                .blob_range(&network, &node, &node, &blob.hash, u64::MAX, 1)
                .is_err()
        );
        assert_eq!(
            store
                .blob_range(&network, &node, &node, &blob.hash, blob.size, 1)?
                .unwrap()
                .1,
            Vec::<u8>::new()
        );
        // External corruption is kept inside this test; put_blob must retire even same-length repairs.
        store.blob_readers.retire_blob(&network, &blob.hash);
        store.connection.execute(
            "UPDATE blobs SET content=? WHERE network_id=? AND hash=?",
            params![vec![13_u8; bytes.len()], network, blob.hash],
        )?;
        assert!(store.get_blob(&network, &blob.hash).is_err());
        assert_eq!(
            store
                .blob_range(&network, &node, &node, &blob.hash, 0, 65536)?
                .unwrap()
                .1,
            vec![13_u8; 65536],
            "the old reader actually owns a corrupt snapshot"
        );
        store.put_blob(&network, &blob, &bytes)?;
        assert_eq!(
            store
                .blob_range(&network, &node, &node, &blob.hash, 65536, 65536)?
                .unwrap()
                .1,
            bytes[65536..131072]
        );
        drop(store);
        assert_eq!(
            Arc::strong_count(&lock),
            1,
            "Store drop joins readers and releases their data-dir locks"
        );
        drop(lock);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    fn roles() -> Vec<NodeRole> {
        vec![NodeRole::Gateway, NodeRole::Replica, NodeRole::Scheduler]
    }
    fn memory(owner: &LocalIdentity, other: Option<&LocalIdentity>) -> Result<Store> {
        let mut store = Store::open(":memory:")?;
        let mut nodes = vec![owner.certificate(roles())?];
        if let Some(other) = other {
            nodes.push(other.certificate(roles())?);
        }
        store.install_trust_bundle(
            &owner.fixture_network_id()?,
            &owner.sign_network_fixture(1, nodes)?,
        )?;
        store.save_local_identity(&owner.fixture_network_id()?, owner)?;
        Ok(store)
    }
    fn conversation(store: &mut Store, owner: &LocalIdentity) -> Result<SignedEvent> {
        store.append_event(
            owner,
            &owner.fixture_network_id()?,
            "conversation",
            EventBody::ConversationCreated {
                title: "work".into(),
            },
            vec![],
            1,
        )
    }
    fn message(
        owner: &LocalIdentity,
        network: &str,
        author_sequence: u64,
        previous: Option<String>,
        parents: Vec<String>,
        text: &str,
        blobs: Vec<BlobRef>,
    ) -> Result<SignedEvent> {
        owner.sign_event(Event {
            version: 1,
            network_id: network.into(),
            scope_id: "conversation".into(),
            author: owner.peer_id()?,
            sequence: author_sequence,
            previous,
            parents,
            created_at_ms: 2,
            body: EventBody::MessagePosted {
                message_id: text.into(),
                scheduler_node: None,
                role: "user".into(),
                text: text.into(),
            },
            blobs,
        })
    }
    fn admitted_message(
        store: &mut Store,
        identity: &LocalIdentity,
        id: &str,
        target: &str,
    ) -> Result<SignedEvent> {
        Ok(store
            .accept_message(
                identity,
                &identity.fixture_network_id()?,
                "conversation",
                EventBody::MessagePosted {
                    message_id: id.into(),
                    role: "user".into(),
                    text: id.into(),
                    scheduler_node: Some(target.into()),
                },
                vec![],
                2,
                &identity.peer_id()?,
                target,
                false,
            )?
            .0)
    }

    fn completed_job_events(job: &SchedulerJob) -> Vec<(EventBody, Vec<BlobRef>)> {
        vec![(
            EventBody::MessagePosted {
                message_id: job.message_id.clone(),
                role: "assistant".into(),
                text: "done".into(),
                scheduler_node: None,
            },
            vec![],
        )]
    }

    #[test]
    fn scheduler_admission_and_final_reply_are_crash_atomic_and_idempotent() -> Result<()> {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let target = owner.peer_id()?;
        let mut store = memory(&owner, None)?;
        conversation(&mut store, &owner)?;
        store.connection.execute_batch("CREATE TRIGGER reject_scheduler_admission BEFORE INSERT ON local_values WHEN NEW.key LIKE 'scheduler:v1:job:%' BEGIN SELECT RAISE(ABORT,'injected queue failure'); END;")?;
        assert!(admitted_message(&mut store, &owner, "one", &target).is_err());
        assert!(store.messages(&network, "conversation")?.is_empty());
        store
            .connection
            .execute_batch("DROP TRIGGER reject_scheduler_admission;")?;
        let first = admitted_message(&mut store, &owner, "one", &target)?;
        assert_eq!(
            admitted_message(&mut store, &owner, "one", &target)?.hash,
            first.hash
        );
        assert_eq!(store.messages(&network, "conversation")?.len(), 1);
        assert!(store.scheduler_maintenance_idle(&network).is_err());
        let claimed = store
            .claim_next(&network, "conversation", &target, "boot", "sdk")?
            .unwrap();
        let attempt = claimed.job.state.attempt().unwrap().id.clone();
        store.mark_started(&network, "conversation", &target, &first.hash, &attempt)?;
        let mut wrong_reply = completed_job_events(&claimed.job);
        if let EventBody::MessagePosted { message_id, .. } = &mut wrong_reply[0].0 {
            *message_id = "different-reply".into();
        }
        assert!(
            store
                .finish_job(
                    &owner,
                    &network,
                    "conversation",
                    &target,
                    &first.hash,
                    &attempt,
                    JobOutcome::Completed,
                    wrong_reply,
                    3
                )
                .is_err()
        );
        let mut duplicate_reply = completed_job_events(&claimed.job);
        duplicate_reply.extend(completed_job_events(&claimed.job));
        assert!(
            store
                .finish_job(
                    &owner,
                    &network,
                    "conversation",
                    &target,
                    &first.hash,
                    &attempt,
                    JobOutcome::Completed,
                    duplicate_reply,
                    3
                )
                .is_err()
        );
        let mut incomplete = completed_job_events(&claimed.job);
        incomplete.pop();
        assert!(
            store
                .finish_job(
                    &owner,
                    &network,
                    "conversation",
                    &target,
                    &first.hash,
                    &attempt,
                    JobOutcome::Completed,
                    incomplete,
                    3
                )
                .is_err()
        );
        assert!(!store.scheduler_publication_pending(
            &network,
            "conversation",
            &target,
            &first.hash
        )?);
        store.connection.execute_batch("CREATE TRIGGER reject_scheduler_reply BEFORE INSERT ON events WHEN json_extract(json_extract(NEW.envelope,'$.canonical_body'),'$.body.role')='assistant' BEGIN SELECT RAISE(ABORT,'injected assistant failure'); END;")?;
        assert!(
            store
                .finish_job(
                    &owner,
                    &network,
                    "conversation",
                    &target,
                    &first.hash,
                    &attempt,
                    JobOutcome::Completed,
                    completed_job_events(&claimed.job),
                    3
                )
                .is_err()
        );
        assert!(matches!(
            store
                .scheduler_job(&network, "conversation", &first.hash, &target)?
                .unwrap()
                .state,
            JobState::Running { .. }
        ));
        assert!(
            !store
                .context(&network, "conversation", &[])?
                .events
                .iter()
                .any(|event| matches!(&event.body, EventBody::DecisionRecorded { decision_id, .. } if decision_id == &format!("decision-{}", claimed.job.message_id)))
        );
        assert!(store.scheduler_publication_pending(
            &network,
            "conversation",
            &target,
            &first.hash
        )?);
        assert!(!store.manager_attempt_allowed(
            &network,
            "conversation",
            &target,
            &first.hash,
            &attempt,
            true
        )?);
        assert!(matches!(
            store
                .request_manager_cancel(&network, "conversation", &target, &first.hash)?
                .unwrap()
                .state,
            JobState::Running { .. }
        ));
        assert!(
            store
                .finish_job(
                    &owner,
                    &network,
                    "conversation",
                    &target,
                    &first.hash,
                    &attempt,
                    JobOutcome::Interrupted {
                        detail: "cleanup must not erase the known reply".into(),
                        process_unconfirmed: true
                    },
                    vec![],
                    4
                )
                .is_err()
        );
        store
            .connection
            .execute_batch("DROP TRIGGER reject_scheduler_reply;")?;
        store.finish_job(
            &owner,
            &network,
            "conversation",
            &target,
            &first.hash,
            &attempt,
            JobOutcome::Completed,
            completed_job_events(&claimed.job),
            3,
        )?;
        store.finish_job(
            &owner,
            &network,
            "conversation",
            &target,
            &first.hash,
            &attempt,
            JobOutcome::Completed,
            completed_job_events(&claimed.job),
            3,
        )?;
        assert_eq!(store.messages(&network, "conversation")?.len(), 2);
        let published = store.context(&network, "conversation", &[])?;
        assert!(
            !published
                .events
                .iter()
                .any(|event| matches!(&event.body, EventBody::DecisionRecorded { decision_id, .. } if decision_id == &format!("decision-{}", claimed.job.message_id)))
        );
        assert!(published.events.iter().any(|event| matches!(
            &event.body,
            EventBody::DecisionRecorded { decision_id, tool_configuration, .. }
                if decision_id == &format!("followup-intent:v1:{}", first.hash)
                    && tool_configuration["type"] == "followup_intent_v1"
        )));
        assert!(!store.scheduler_publication_pending(
            &network,
            "conversation",
            &target,
            &first.hash
        )?);
        assert!(
            store
                .flush_scheduler_publications(&owner, &network, &target)?
                .published
                .is_empty()
        );

        assert!(
            store
                .claim_next(&network, "conversation", &target, "boot", "sdk")?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn scheduler_known_results_survive_missing_parents_and_reopen_without_sdk_replay() -> Result<()>
    {
        let dir =
            std::env::temp_dir().join(format!("vibeface-publication-{}", uuid::Uuid::new_v4()));
        let db = dir.join("state.db");
        let owner = LocalIdentity::generate();
        let peer = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let target = owner.peer_id()?;
        let mut store = Store::open(&db)?;
        store.install_trust_bundle(
            &network,
            &owner.sign_network_fixture(
                1,
                vec![owner.certificate(roles())?, peer.certificate(roles())?],
            )?,
        )?;
        store.save_local_identity(&network, &owner)?;
        let root = conversation(&mut store, &owner)?;
        let message = admitted_message(&mut store, &owner, "known-result", &target)?;
        let claimed = store
            .claim_next(&network, "conversation", &target, "boot-a", "sdk")?
            .unwrap();
        let attempt = claimed.job.state.attempt().unwrap().id.clone();
        store.mark_started(&network, "conversation", &target, &message.hash, &attempt)?;
        let parent = peer.sign_event(Event {
            version: WIRE_VERSION,
            network_id: network.clone(),
            scope_id: "conversation".into(),
            author: peer.peer_id()?,
            sequence: 1,
            previous: None,
            parents: vec![root.hash],
            created_at_ms: 3,
            body: EventBody::NotificationRaised {
                notification_id: "parent".into(),
                kind: "progress".into(),
                text: "parent".into(),
            },
            blobs: vec![],
        })?;
        let child = peer.sign_event(Event {
            version: WIRE_VERSION,
            network_id: network.clone(),
            scope_id: "conversation".into(),
            author: peer.peer_id()?,
            sequence: 2,
            previous: Some(parent.hash.clone()),
            parents: vec![parent.hash.clone()],
            created_at_ms: 4,
            body: EventBody::NotificationRaised {
                notification_id: "child".into(),
                kind: "progress".into(),
                text: "child".into(),
            },
            blobs: vec![],
        })?;
        assert_eq!(store.ingest_event(&network, &child)?.status, "pending");
        assert!(
            store
                .finish_job(
                    &owner,
                    &network,
                    "conversation",
                    &target,
                    &message.hash,
                    &attempt,
                    JobOutcome::Completed,
                    completed_job_events(&claimed.job),
                    5
                )
                .is_err()
        );
        assert!(store.scheduler_publication_pending(
            &network,
            "conversation",
            &target,
            &message.hash
        )?);
        assert!(
            store
                .finish_job(
                    &owner,
                    &network,
                    "conversation",
                    &target,
                    &message.hash,
                    &attempt,
                    JobOutcome::Interrupted {
                        detail: "failed cleanup".into(),
                        process_unconfirmed: true
                    },
                    vec![],
                    6
                )
                .is_err()
        );
        assert!(
            store
                .messages(&network, "conversation")?
                .iter()
                .all(|message| message.role != "assistant")
        );

        store.append_event(
            &owner,
            &network,
            "healthy",
            EventBody::ConversationCreated {
                title: "independent".into(),
            },
            vec![],
            1,
        )?;
        let (healthy, _) = store.accept_message(
            &owner,
            &network,
            "healthy",
            EventBody::MessagePosted {
                message_id: "other".into(),
                role: "user".into(),
                text: "other".into(),
                scheduler_node: Some(target.clone()),
            },
            vec![],
            2,
            &target,
            &target,
            false,
        )?;
        let healthy_job = store
            .claim_next(&network, "healthy", &target, "boot-a", "sdk")?
            .unwrap();
        let healthy_attempt = healthy_job.job.state.attempt().unwrap().id.clone();
        store.mark_started(
            &network,
            "healthy",
            &target,
            &healthy.hash,
            &healthy_attempt,
        )?;
        store.connection.execute_batch("CREATE TRIGGER reject_publication_reply BEFORE INSERT ON events WHEN json_extract(json_extract(NEW.envelope,'$.canonical_body'),'$.body.role')='assistant' BEGIN SELECT RAISE(ABORT,'injected publication failure'); END;")?;
        assert!(
            store
                .finish_job(
                    &owner,
                    &network,
                    "healthy",
                    &target,
                    &healthy.hash,
                    &healthy_attempt,
                    JobOutcome::Completed,
                    completed_job_events(&healthy_job.job),
                    5
                )
                .is_err()
        );
        assert!(store.scheduler_publication_pending(
            &network,
            "healthy",
            &target,
            &healthy.hash
        )?);
        drop(store);

        let mut store = Store::open(&db)?;
        store.recover_scheduler_jobs(&network, &target, "boot-b")?;
        for (scope, trigger) in [("conversation", &message.hash), ("healthy", &healthy.hash)] {
            let current = store
                .scheduler_job(&network, scope, trigger, &target)?
                .unwrap();
            assert!(matches!(current.state, JobState::Running { .. }));
            assert!(matches!(
                store
                    .request_manager_cancel(&network, scope, &target, trigger)?
                    .unwrap()
                    .state,
                JobState::Running { .. }
            ));
            assert!(
                store
                    .claim_next(&network, scope, &target, "boot-b", "sdk")?
                    .is_none()
            );
        }
        store
            .connection
            .execute_batch("DROP TRIGGER reject_publication_reply;")?;
        let independent = store.flush_scheduler_publications(&owner, &network, &target)?;
        assert!(independent.errors.is_empty());
        assert_eq!(independent.published.len(), 1);
        assert_eq!(independent.published[0].job.conversation_id, "healthy");
        assert_eq!(independent.published[0].event_hashes.len(), 1);
        assert!(store.scheduler_publication_pending(
            &network,
            "conversation",
            &target,
            &message.hash
        )?);
        assert!(!store.scheduler_publication_pending(
            &network,
            "healthy",
            &target,
            &healthy.hash
        )?);
        assert_eq!(store.ingest_event(&network, &parent)?.status, "ready");
        let flushed = store.flush_scheduler_publications(&owner, &network, &target)?;
        assert!(flushed.errors.is_empty());
        assert_eq!(flushed.published.len(), 1);
        assert_eq!(flushed.published[0].job.trigger, message.hash);
        assert!(matches!(
            flushed.published[0].job.state,
            JobState::Completed { .. }
        ));
        assert_eq!(flushed.published[0].event_hashes.len(), 1);
        assert!(!store.scheduler_publication_pending(
            &network,
            "conversation",
            &target,
            &message.hash
        )?);
        let final_messages = store.messages(&network, "conversation")?;
        assert_eq!(
            final_messages
                .iter()
                .filter(|message| message.role == "assistant")
                .count(),
            1
        );
        assert_eq!(
            final_messages
                .iter()
                .find(|message| message.role == "assistant")
                .unwrap()
                .text,
            "done"
        );
        assert!(
            store
                .flush_scheduler_publications(&owner, &network, &target)?
                .published
                .is_empty()
        );
        assert!(
            store
                .claim_next(&network, "conversation", &target, "boot-b", "sdk")?
                .is_none()
        );
        store.scheduler_maintenance_idle(&network)?;
        drop(store);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn scheduler_pending_prestart_terminals_are_not_reclaimed_after_restart() -> Result<()> {
        for cancelled in [false, true] {
            let owner = LocalIdentity::generate();
            let network = owner.fixture_network_id()?;
            let target = owner.peer_id()?;
            let mut store = memory(&owner, None)?;
            conversation(&mut store, &owner)?;
            let message = admitted_message(&mut store, &owner, "prestart", &target)?;
            let claimed = store
                .claim_next(&network, "conversation", &target, "boot-a", "sdk")?
                .unwrap();
            let attempt = claimed.job.state.attempt().unwrap().id.clone();
            if cancelled {
                store.request_manager_cancel(&network, "conversation", &target, &message.hash)?;
            }
            store.connection.execute_batch("CREATE TRIGGER reject_prestart_terminal BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'injected prestart terminal failure'); END;")?;
            let outcome = if cancelled {
                JobOutcome::Cancelled("confirmed before SDK start".into())
            } else {
                JobOutcome::Failed("SDK did not start".into())
            };
            assert!(
                store
                    .finish_job(
                        &owner,
                        &network,
                        "conversation",
                        &target,
                        &message.hash,
                        &attempt,
                        outcome,
                        vec![(
                            EventBody::NotificationRaised {
                                notification_id: "terminal".into(),
                                kind: "attention".into(),
                                text: "terminal".into()
                            },
                            vec![]
                        )],
                        3
                    )
                    .is_err()
            );
            assert!(store.scheduler_publication_pending(
                &network,
                "conversation",
                &target,
                &message.hash
            )?);
            store.recover_scheduler_jobs(&network, &target, "boot-b")?;
            let preserved = store
                .scheduler_job(&network, "conversation", &message.hash, &target)?
                .unwrap();
            assert_eq!(
                preserved.state.name(),
                if cancelled { "cancelling" } else { "claimed" }
            );
            assert!(
                store
                    .claim_next(&network, "conversation", &target, "boot-b", "sdk")?
                    .is_none()
            );
            store
                .connection
                .execute_batch("DROP TRIGGER reject_prestart_terminal;")?;
            let flushed = store.flush_scheduler_publications(&owner, &network, &target)?;
            assert!(flushed.errors.is_empty());
            assert_eq!(flushed.published.len(), 1);
            assert_eq!(
                flushed.published[0].job.state.name(),
                if cancelled { "cancelled" } else { "failed" }
            );
            assert!(
                store
                    .claim_next(&network, "conversation", &target, "boot-b", "sdk")?
                    .is_none()
            );
        }
        Ok(())
    }

    #[test]
    fn scheduler_busy_priority_and_input_boundaries_preserve_every_admitted_trigger() -> Result<()>
    {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let target = owner.peer_id()?;
        let mut store = memory(&owner, None)?;
        conversation(&mut store, &owner)?;
        let first = admitted_message(&mut store, &owner, "one", &target)?;
        let one = store
            .claim_next(&network, "conversation", &target, "boot", "sdk")?
            .unwrap();
        let task = store.append_event(
            &owner,
            &network,
            "conversation",
            EventBody::AgentMessage {
                message: AgentMessage {
                    message_id: "delegated".into(),
                    target_node: target.clone(),
                    origin_trigger: first.hash.clone(),
                    in_reply_to: None,
                    kind: AgentMessageKind::Task,
                    text: "work".into(),
                },
            },
            vec![],
            3,
        )?;
        store.enqueue_trigger(&network, "conversation", &task.hash, &target)?;
        let second = admitted_message(&mut store, &owner, "two", &target)?;
        assert!(
            store
                .claim_next(&network, "conversation", &target, "boot", "sdk")?
                .is_none()
        );
        assert_eq!(
            store
                .scheduler_status(&network, "conversation", &second.hash, &target)?
                .unwrap()
                .state,
            "queued"
        );
        store.finish_job(
            &owner,
            &network,
            "conversation",
            &target,
            &first.hash,
            &one.job.state.attempt().unwrap().id,
            JobOutcome::Failed("before SDK".into()),
            vec![],
            4,
        )?;
        let two = store
            .claim_next(&network, "conversation", &target, "boot", "sdk")?
            .unwrap();
        assert_eq!(two.job.trigger, second.hash);
        assert_eq!(two.job.kind, TriggerKind::Human);
        assert!(two.job.basis_heads.contains(&second.hash));
        let three = admitted_message(&mut store, &owner, "three", &target)?;
        let fixed = store.context(&network, "conversation", &two.job.basis_heads)?;
        assert!(!fixed.events.iter().any(|event| matches!(&event.body,EventBody::MessagePosted { message_id,.. } if message_id=="three")));
        assert_ne!(three.hash, second.hash);
        store.finish_job(
            &owner,
            &network,
            "conversation",
            &target,
            &second.hash,
            &two.job.state.attempt().unwrap().id,
            JobOutcome::Failed("before SDK".into()),
            vec![],
            5,
        )?;
        let next = store
            .claim_next(&network, "conversation", &target, "boot", "sdk")?
            .unwrap();
        assert_eq!(next.job.trigger, three.hash);
        Ok(())
    }

    #[test]
    fn scheduler_recovery_interrupts_started_attempts_and_registers_one_pending_check() -> Result<()>
    {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let target = owner.peer_id()?;
        let mut store = memory(&owner, None)?;
        conversation(&mut store, &owner)?;
        let first = admitted_message(&mut store, &owner, "one", &target)?;
        let old = store
            .claim_next(&network, "conversation", &target, "boot-a", "sdk")?
            .unwrap();
        let recovered = store
            .claim_next(&network, "conversation", &target, "boot-b", "sdk")?
            .unwrap();
        assert_eq!(old.job.trigger, recovered.job.trigger);
        assert_ne!(
            old.job.state.attempt().unwrap().id,
            recovered.job.state.attempt().unwrap().id
        );
        let attempt = recovered.job.state.attempt().unwrap().id.clone();
        store.mark_started(&network, "conversation", &target, &first.hash, &attempt)?;
        store.record_manager_process(&network, &attempt, 4242)?;
        store.recover_scheduler_jobs(&network, &target, "boot-c")?;
        assert!(matches!(
            store
                .scheduler_job(&network, "conversation", &first.hash, &target)?
                .unwrap()
                .state,
            JobState::Interrupted { .. }
        ));
        assert!(!store.manager_attempt_allowed(
            &network,
            "conversation",
            &target,
            &first.hash,
            &attempt,
            false
        )?);
        let check_id = PendingCheck::id(&attempt);
        let check = store.pending_check(&network, &check_id)?.unwrap();
        assert_eq!(
            (check.trigger.as_str(), check.process_groups.as_slice()),
            (first.hash.as_str(), &[4242][..])
        );
        // An interrupted job is terminal: later work is neither blocked nor downgraded to read-only.
        let second = admitted_message(&mut store, &owner, "two", &target)?;
        let next = store
            .claim_next(&network, "conversation", &target, "boot-c", "sdk")?
            .unwrap();
        assert_eq!(next.job.trigger, second.hash);
        let next_attempt = next.job.state.attempt().unwrap();
        assert!(!next_attempt.read_only);
        store.finish_job(
            &owner,
            &network,
            "conversation",
            &target,
            &second.hash,
            &next_attempt.id,
            JobOutcome::Failed("done".into()),
            vec![],
            5,
        )?;
        assert!(store.scheduler_maintenance_idle(&network).is_err());
        store.connection.execute_batch("CREATE TRIGGER reject_manager_resolution BEFORE INSERT ON local_values WHEN NEW.key LIKE 'scheduler:v1:resolution:%' BEGIN SELECT RAISE(ABORT,'injected evidence failure'); END;")?;
        assert!(
            store
                .resolve_pending_check(&network, &check_id, "owner checked", 6)
                .is_err()
        );
        assert!(store.pending_check(&network, &check_id)?.is_some());
        store
            .connection
            .execute_batch("DROP TRIGGER reject_manager_resolution;")?;
        store.resolve_pending_check(&network, &check_id, "owner checked", 6)?;
        store.scheduler_maintenance_idle(&network)?;
        assert!(store.pending_checks(&network)?.is_empty());
        assert!(
            store
                .resolve_pending_check(&network, &check_id, "owner checked", 7)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn scheduler_outbox_ack_retries_cannot_duplicate_or_retarget_admission() -> Result<()> {
        let owner = LocalIdentity::generate();
        let remote = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let target = remote.peer_id()?;
        let mut source = memory(&owner, Some(&remote))?;
        conversation(&mut source, &owner)?;
        let event = admitted_message(&mut source, &owner, "remote", &target)?;
        assert_eq!(source.scheduler_outbox(&network)?.len(), 1);
        assert!(source.scheduler_maintenance_idle(&network).is_err());
        assert!(
            source
                .scheduler_jobs(&network, "conversation", &target)?
                .is_empty()
        );
        let receipt = source.enqueue_trigger(&network, "conversation", &event.hash, &target)?;
        let mut forged = receipt.clone();
        forged.version = 0;
        assert!(source.acknowledge_outbox(&network, &forged).is_err());
        forged = receipt.clone();
        forged.state = "accepted".into();
        assert!(source.acknowledge_outbox(&network, &forged).is_err());
        source.acknowledge_outbox(&network, &receipt)?;
        source.acknowledge_outbox(&network, &receipt)?;
        assert!(source.scheduler_outbox(&network)?.is_empty());
        let deliveries = source.scheduler_outbox_all(&network)?;
        assert_eq!(deliveries.len(), 1);
        assert!(deliveries[0].delivered);
        assert_eq!(deliveries[0].trigger, event.hash);
        assert_eq!(
            source
                .enqueue_trigger(&network, "conversation", &event.hash, &target)?
                .state,
            "queued"
        );
        assert_eq!(
            source
                .scheduler_jobs(&network, "conversation", &target)?
                .len(),
            1
        );
        assert!(
            source
                .enqueue_trigger(&network, "conversation", &event.hash, &owner.peer_id()?)
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn scheduler_queued_cancellation_and_claim_preserve_each_serialized_winner() -> Result<()> {
        for claim_first in [false, true] {
            let owner = LocalIdentity::generate();
            let network = owner.fixture_network_id()?;
            let target = owner.peer_id()?;
            let mut store = memory(&owner, None)?;
            conversation(&mut store, &owner)?;
            let event = admitted_message(&mut store, &owner, "cancel", &target)?;
            let claimed = if claim_first {
                store.claim_next(&network, "conversation", &target, "boot", "sdk")?
            } else {
                None
            };
            let cancelled = store
                .request_manager_cancel(&network, "conversation", &target, &event.hash)?
                .unwrap();
            assert_eq!(cancelled.trigger, event.hash);
            assert_eq!(
                store.request_manager_cancel(&network, "conversation", &target, &event.hash)?,
                Some(cancelled.clone())
            );
            assert_eq!(store.budget_usage(&network, "2026-10-18", 5)?.used, 0);
            if let Some(claimed) = claimed {
                let attempt = claimed.job.state.attempt().unwrap();
                assert!(matches!(cancelled.state, JobState::Cancelling { .. }));
                assert_eq!(cancelled.state.attempt(), Some(attempt));
                assert!(!store.manager_attempt_allowed(
                    &network,
                    "conversation",
                    &target,
                    &event.hash,
                    &attempt.id,
                    true
                )?);
                assert!(
                    store
                        .mark_started(&network, "conversation", &target, &event.hash, &attempt.id)
                        .is_err()
                );
            } else {
                assert!(matches!(cancelled.state, JobState::Cancelled { .. }));
                assert!(cancelled.state.attempt().is_none());
                store.recover_scheduler_jobs(&network, &target, "restarted")?;
                store.scheduler_maintenance_idle(&network)?;
            }
            assert!(
                store
                    .claim_next(&network, "conversation", &target, "boot", "sdk")?
                    .is_none()
            );
        }
        Ok(())
    }

    #[test]
    fn scheduler_cancellation_fences_existing_credentials_before_confirming_terminal() -> Result<()>
    {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let target = owner.peer_id()?;
        let mut store = memory(&owner, None)?;
        conversation(&mut store, &owner)?;
        let event = admitted_message(&mut store, &owner, "one", &target)?;
        let claimed = store
            .claim_next(&network, "conversation", &target, "boot", "sdk")?
            .unwrap();
        let attempt = claimed.job.state.attempt().unwrap().id.clone();
        store.mark_started(&network, "conversation", &target, &event.hash, &attempt)?;
        assert!(store.manager_attempt_allowed(
            &network,
            "conversation",
            &target,
            &event.hash,
            &attempt,
            true
        )?);
        let cancelling = store
            .request_manager_cancel(&network, "conversation", &target, &event.hash)?
            .unwrap();
        assert!(matches!(cancelling.state, JobState::Cancelling { .. }));
        assert!(!store.manager_attempt_allowed(
            &network,
            "conversation",
            &target,
            &event.hash,
            &attempt,
            true
        )?);
        assert!(
            store
                .finish_job(
                    &owner,
                    &network,
                    "conversation",
                    &target,
                    &event.hash,
                    &attempt,
                    JobOutcome::Completed,
                    completed_job_events(&claimed.job),
                    3
                )
                .is_err()
        );
        assert!(
            store
                .finish_job(
                    &owner,
                    &network,
                    "conversation",
                    &target,
                    &event.hash,
                    &attempt,
                    JobOutcome::Cancelled(String::new()),
                    vec![],
                    3
                )
                .is_err()
        );
        assert!(!store.scheduler_publication_pending(
            &network,
            "conversation",
            &target,
            &event.hash
        )?);
        store.finish_job(
            &owner,
            &network,
            "conversation",
            &target,
            &event.hash,
            &attempt,
            JobOutcome::Cancelled("SDK cancellation confirmed and Node tools fenced".into()),
            vec![],
            3,
        )?;
        let cancelled = store
            .scheduler_job(&network, "conversation", &event.hash, &target)?
            .unwrap();
        let legacy = serde_json::json!({
            "state":"cancelled",
            "attempt":cancelled.state.attempt().unwrap(),
            "evidence":"SDK cancellation confirmed and Node tools fenced",
        });
        assert_eq!(serde_json::from_value::<JobState>(legacy)?, cancelled.state);
        store.scheduler_maintenance_idle(&network)?;
        assert!(
            store
                .claim_next(&network, "conversation", &target, "boot", "sdk")?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn net1_grouped_inventory_preserves_pending_forks_and_cross_scope_heads() -> Result<()> {
        fn equivalent(store: &Store, network: &str) -> Result<()> {
            let legacy = store
                .scopes(network)?
                .into_iter()
                .map(|scope| Ok((scope.clone(), store.heads(network, &scope)?)))
                .collect::<Result<Vec<_>>>()?;
            assert_eq!(store.inventory_heads(network)?, legacy);
            Ok(())
        }
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let mut store = memory(&owner, None)?;
        equivalent(&store, &network)?;
        let root = conversation(&mut store, &owner)?;
        let blob = BlobRef {
            hash: digest(b"absent"),
            size: 6,
        };
        let left = message(
            &owner,
            &network,
            2,
            Some(root.hash.clone()),
            vec![root.hash.clone()],
            "left",
            vec![blob],
        )?;
        assert_eq!(store.ingest_event(&network, &left)?.status, "pending");
        equivalent(&store, &network)?;
        assert_eq!(
            store.heads(&network, "conversation")?,
            vec![left.hash.clone()]
        );
        let right = message(
            &owner,
            &network,
            2,
            Some(root.hash.clone()),
            vec![root.hash],
            "right",
            vec![],
        )?;
        assert_eq!(store.ingest_event(&network, &right)?.status, "conflict");
        let child = message(
            &owner,
            &network,
            3,
            Some(left.hash.clone()),
            vec![left.hash],
            "child",
            vec![],
        )?;
        assert_eq!(store.ingest_event(&network, &child)?.status, "invalid");
        equivalent(&store, &network)?;
        let heads = store.heads(&network, "conversation")?;
        assert_eq!(heads.len(), 2);
        assert!(heads.contains(&right.hash) && heads.contains(&child.hash));
        let a = store.append_event(
            &owner,
            &network,
            "a",
            EventBody::ConversationCreated {
                title: "cross-scope parent".into(),
            },
            vec![],
            3,
        )?;
        let b = owner.sign_event(Event {
            version: 1,
            network_id: network.clone(),
            scope_id: "b".into(),
            author: owner.peer_id()?,
            sequence: 1,
            previous: None,
            parents: vec![a.hash.clone()],
            created_at_ms: 4,
            body: EventBody::ConversationCreated {
                title: "cross-scope child".into(),
            },
            blobs: vec![],
        })?;
        store.ingest_event(&network, &b)?;
        equivalent(&store, &network)?;
        assert_eq!(store.heads(&network, "a")?, vec![a.hash]);
        assert_eq!(store.heads(&network, "b")?, vec![b.hash.clone()]);
        let mut corrupt = b;
        corrupt.signature = "00".repeat(64);
        store.connection.execute(
            "UPDATE events SET envelope=? WHERE network_id=? AND hash=?",
            params![serde_json::to_string(&corrupt)?, network, corrupt.hash],
        )?;
        assert!(store.inventory_heads(&network).is_err());
        println!(
            "NET1_M5 grouped_inventory_legacy_equivalent=true pending_conflict_invalid_retained=true previous_fork_heads=true cross_scope_parent_retained=true corrupted_envelope_rejected=true"
        );
        Ok(())
    }

    #[test]
    fn net1_duplicate_import_does_not_write_projections() -> Result<()> {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let mut store = memory(&owner, None)?;
        let root = conversation(&mut store, &owner)?;
        store.connection.execute_batch(
            "CREATE TEMP TABLE projection_writes(count INTEGER NOT NULL);
             INSERT INTO projection_writes VALUES(0);
             CREATE TEMP TRIGGER count_conversation_delete AFTER DELETE ON conversations
             BEGIN UPDATE projection_writes SET count=count+1; END;
             CREATE TEMP TRIGGER count_conversation_insert AFTER INSERT ON conversations
             BEGIN UPDATE projection_writes SET count=count+1; END;",
        )?;
        store.ingest_event(&network, &root)?;
        let writes: u64 =
            store
                .connection
                .query_row("SELECT count FROM projection_writes", [], |row| row.get(0))?;
        assert_eq!(writes, 0);
        assert_eq!(store.conversations(&network)?.len(), 1);
        assert!(
            store
                .scheduler_jobs(&network, "conversation", &owner.peer_id()?)?
                .is_empty()
        );
        assert!(store.scheduler_outbox_all(&network)?.is_empty());
        Ok(())
    }

    #[test]
    fn net1_batch_and_snapshot_rebuild_once_and_duplicates_keep_generation() -> Result<()> {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let mut source = memory(&owner, None)?;
        let root = conversation(&mut source, &owner)?;
        let first = message(
            &owner,
            &network,
            2,
            Some(root.hash.clone()),
            vec![root.hash.clone()],
            "one",
            vec![],
        )?;
        let second = message(
            &owner,
            &network,
            3,
            Some(first.hash.clone()),
            vec![first.hash.clone()],
            "two",
            vec![],
        )?;
        let mut replica = memory(&owner, None)?;
        let generation = replica.projection_generation(&network)?;
        assert_eq!(
            replica.ingest_events(&network, &[second.clone(), root.clone(), first.clone()])?,
            3
        );
        assert_eq!(replica.projection_generation(&network)?, generation + 1);
        assert_eq!(
            replica
                .messages(&network, "conversation")?
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two"]
        );
        assert_eq!(
            replica.ingest_events(&network, &[root.clone(), first, second.clone()])?,
            0
        );
        assert_eq!(replica.projection_generation(&network)?, generation + 1);
        let snapshot = replica.export_snapshot(&network, "conversation")?;
        let mut target = memory(&owner, None)?;
        let before = target.projection_generation(&network)?;
        assert!(target.import_snapshot(&network, &snapshot)?.is_complete());
        assert_eq!(target.projection_generation(&network)?, before + 1);
        assert!(target.import_snapshot(&network, &snapshot)?.is_complete());
        assert_eq!(target.projection_generation(&network)?, before + 1);
        let mut conflicting = second;
        conflicting.signature = root.signature;
        assert!(target.ingest_event(&network, &conflicting).is_err());
        assert_eq!(target.projection_generation(&network)?, before + 1);
        assert_eq!(target.messages(&network, "conversation")?.len(), 2);
        Ok(())
    }

    #[test]
    fn causal_import_waits_for_every_dependency_and_blob_then_converges() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let mut source = memory(&a, Some(&b))?;
        let mut replica = memory(&a, Some(&b))?;
        let root = conversation(&mut source, &a)?;
        let content = b"necessary evidence";
        let blob = BlobRef {
            hash: digest(content),
            size: content.len() as u64,
        };
        let left = message(
            &a,
            &network,
            2,
            Some(root.hash.clone()),
            vec![root.hash.clone()],
            "left",
            vec![blob.clone()],
        )?;
        let right = message(
            &b,
            &network,
            1,
            None,
            vec![root.hash.clone()],
            "right",
            vec![],
        )?;
        assert_eq!(replica.ingest_event(&network, &left)?.status, "pending");
        assert!(replica.messages(&network, "conversation")?.is_empty());
        assert_eq!(
            replica
                .missing_dependencies(&network, std::slice::from_ref(&left.hash))?
                .events,
            vec![root.hash.clone()]
        );
        replica.ingest_event(&network, &right)?;
        replica.ingest_event(&network, &root)?;
        assert_eq!(replica.messages(&network, "conversation")?.len(), 1);
        assert_eq!(
            replica
                .context(&network, "conversation", &[])?
                .missing
                .blobs,
            vec![blob.clone()]
        );
        assert!(replica.put_blob(&network, &blob, b"wrong").is_err());
        replica.put_blob(&network, &blob, content)?;
        assert_eq!(replica.messages(&network, "conversation")?.len(), 2);
        replica.ingest_event(&network, &left)?;
        assert_eq!(replica.messages(&network, "conversation")?.len(), 2);
        let snapshot = replica.export_snapshot(&network, "conversation")?;
        source.import_snapshot(&network, &snapshot)?;
        source.put_blob(&network, &blob, content)?;
        assert_eq!(
            source.heads(&network, "conversation")?,
            replica.heads(&network, "conversation")?
        );
        assert_eq!(
            source.messages(&network, "conversation")?,
            replica.messages(&network, "conversation")?
        );
        assert!(
            replica
                .scheduler_jobs(&network, "conversation", &a.peer_id()?)?
                .is_empty()
        );
        assert!(
            replica
                .scheduler_jobs(&network, "conversation", &b.peer_id()?)?
                .is_empty()
        );
        assert!(replica.scheduler_outbox_all(&network)?.is_empty());
        let context = replica.context(&network, "conversation", &snapshot.heads)?;
        assert!(context.missing.is_complete());
        assert_eq!(context.events.len(), 3);
        Ok(())
    }
    #[test]
    fn corrupted_stored_blob_blocks_context_until_verified_content_is_restored() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let mut store = memory(&a, None)?;
        let root = conversation(&mut store, &a)?;
        let content = b"evidence";
        let blob = BlobRef {
            hash: digest(content),
            size: content.len() as u64,
        };
        store.put_blob(&network, &blob, content)?;
        let event = message(
            &a,
            &network,
            2,
            Some(root.hash.clone()),
            vec![root.hash],
            "attached",
            vec![blob.clone()],
        )?;
        store.ingest_event(&network, &event)?;
        store.connection.execute(
            "UPDATE blobs SET content=? WHERE network_id=? AND hash=?",
            params![b"tampered".as_slice(), network, blob.hash],
        )?;
        assert!(store.get_blob(&network, &blob.hash).is_err());
        assert_eq!(
            store.context(&network, "conversation", &[])?.missing.blobs,
            vec![blob.clone()]
        );
        store.rebuild_projections(&network)?;
        assert!(store.messages(&network, "conversation")?.is_empty());
        store.put_blob(&network, &blob, content)?;
        assert_eq!(store.messages(&network, "conversation")?.len(), 1);
        assert!(
            store
                .context(&network, "conversation", &[])?
                .missing
                .is_complete()
        );
        Ok(())
    }
    #[test]
    fn author_equivocation_removes_both_projections_and_blocks_descendants() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let mut store = memory(&a, None)?;
        let root = conversation(&mut store, &a)?;
        let left = message(
            &a,
            &network,
            2,
            Some(root.hash.clone()),
            vec![root.hash.clone()],
            "one",
            vec![],
        )?;
        let right = message(
            &a,
            &network,
            2,
            Some(root.hash.clone()),
            vec![root.hash.clone()],
            "two",
            vec![],
        )?;
        store.ingest_event(&network, &left)?;
        assert_eq!(store.messages(&network, "conversation")?.len(), 1);
        assert_eq!(store.ingest_event(&network, &right)?.status, "conflict");
        assert!(store.messages(&network, "conversation")?.is_empty());
        let child = message(
            &a,
            &network,
            3,
            Some(left.hash.clone()),
            vec![left.hash.clone()],
            "child",
            vec![],
        )?;
        assert_eq!(store.ingest_event(&network, &child)?.status, "invalid");
        assert!(
            !store
                .context(&network, "conversation", &[])?
                .missing
                .is_complete()
        );
        assert!(
            store
                .append_event(
                    &a,
                    &network,
                    "conversation",
                    EventBody::ConversationCreated {
                        title: "bad".into()
                    },
                    vec![],
                    0
                )
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn revoked_author_history_verifies_but_new_commands_are_denied() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let mut store = memory(&a, Some(&b))?;
        let root = conversation(&mut store, &a)?;
        let event = message(&b, &network, 1, None, vec![root.hash], "old", vec![])?;
        let mut revoked = b.certificate(roles())?;
        revoked.revoked = true;
        let bundle = a.sign_network_fixture(2, vec![a.certificate(roles())?, revoked])?;
        store.install_trust_bundle(&network, &bundle)?;
        assert!(store.authorize_node(&network, &b.peer_id()?, None).is_err());
        assert_eq!(store.ingest_event(&network, &event)?.status, "ready");
        assert!(
            store
                .append_event(
                    &b,
                    &network,
                    "conversation",
                    EventBody::ConversationCreated {
                        title: "bad".into()
                    },
                    vec![],
                    0
                )
                .is_err()
        );
        let old = a.sign_network_fixture(1, vec![a.certificate(roles())?])?;
        assert!(store.install_trust_bundle(&network, &old).is_err());
        Ok(())
    }
    #[test]
    fn concurrent_local_writers_serialize_author_sequences_and_projection_updates() -> Result<()> {
        let root = std::env::temp_dir().join(format!("vibeface-writers-{}", uuid::Uuid::new_v4()));
        let db = root.join("node.sqlite3");
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let mut initial = Store::open(&db)?;
        initial.install_trust_bundle(
            &network,
            &owner.sign_network_fixture(1, vec![owner.certificate(roles())?])?,
        )?;
        conversation(&mut initial, &owner)?;
        drop(initial);
        let first = Store::open(&db)?;
        let second = Store::open(&db)?;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut workers = Vec::new();
        for (writer, mut store) in [first, second].into_iter().enumerate() {
            let barrier = barrier.clone();
            let owner = owner.clone();
            let network = network.clone();
            workers.push(std::thread::spawn(move || -> Result<()> {
                barrier.wait();
                for index in 0..6 {
                    store.append_event(
                        &owner,
                        &network,
                        "conversation",
                        EventBody::MessagePosted {
                            message_id: format!("{writer}-{index}"),
                            scheduler_node: None,
                            role: "user".into(),
                            text: format!("{writer}-{index}"),
                        },
                        vec![],
                        index,
                    )?;
                }
                Ok(())
            }));
        }
        for worker in workers {
            worker.join().unwrap()?;
        }
        let final_store = Store::open(&db)?;
        assert_eq!(final_store.messages(&network, "conversation")?.len(), 12);
        let context = final_store.context(&network, "conversation", &[])?;
        assert!(context.missing.is_complete());
        assert_eq!(context.events.len(), 13);
        assert_eq!(context.heads.len(), 1);
        drop(final_store);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn corrupted_stored_event_never_becomes_a_complete_context() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let mut store = memory(&a, None)?;
        let mut event = conversation(&mut store, &a)?;
        event.canonical_body = event.canonical_body.replace("work", "tampered");
        store.connection.execute(
            "UPDATE events SET envelope=? WHERE network_id=? AND hash=?",
            params![serde_json::to_string(&event)?, network, event.hash],
        )?;
        assert!(store.context(&network, "conversation", &[]).is_err());
        assert!(store.rebuild_projections(&network).is_err());
        Ok(())
    }

    #[test]
    fn root_recovery_preserves_revoked_authors_without_restoring_authority() -> Result<()> {
        use crate::identity::{Genesis, GovernanceEvent, Member, NetworkState, RootCheckpoint};
        let root = LocalIdentity::generate();
        let admin = LocalIdentity::generate();
        let current = LocalIdentity::generate();
        let retired = LocalIdentity::generate();
        let mut state = NetworkState::new(Genesis::create(
            &root,
            vec![
                Member::client(&admin, "admin", true)?,
                Member::node(&current, "current", roles())?,
            ],
        )?)?;
        state.append(GovernanceEvent::grant(
            &admin,
            &state.project()?,
            Member::node(&retired, "retired", roles())?,
        )?)?;
        state.append(GovernanceEvent::revoke(
            &admin,
            &state.project()?,
            &retired.peer_id()?,
        )?)?;
        let event = retired.sign_event(Event {
            version: WIRE_VERSION,
            network_id: root.network_id()?,
            scope_id: "archive".into(),
            author: retired.peer_id()?,
            sequence: 1,
            previous: None,
            parents: vec![],
            created_at_ms: 0,
            body: EventBody::ConversationCreated {
                title: "retired author".into(),
            },
            blobs: vec![],
        })?;
        let members = state
            .project()?
            .members
            .into_iter()
            .filter(|member| member.subject_id != retired.peer_id().unwrap())
            .collect();
        let checkpoint =
            RootCheckpoint::create_with_history(&root, 2, members, state.historical_nodes()?)?;
        state.recover(checkpoint)?;
        let network = root.network_id()?;
        let mut fresh = Store::open(":memory:")?;
        fresh.install_trust_bundle(&network, &state)?;
        assert_eq!(fresh.ingest_event(&network, &event)?.status, "ready");
        assert!(
            fresh
                .authorize_node(&network, &retired.peer_id()?, None)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn management_clients_cannot_author_business_dag_events() -> Result<()> {
        let root = LocalIdentity::generate();
        let admin = LocalIdentity::generate();
        let node = LocalIdentity::generate();
        let state = crate::identity::NetworkState::new(crate::identity::Genesis::create(
            &root,
            vec![
                crate::identity::Member::client(&admin, "management", true)?,
                crate::identity::Member::node(&node, "Node", roles())?,
            ],
        )?)?;
        let network = root.network_id()?;
        let mut store = Store::open(":memory:")?;
        store.install_trust_bundle(&network, &state)?;
        let forged = admin.sign_event(Event {
            version: WIRE_VERSION,
            network_id: network.clone(),
            scope_id: "conversation".into(),
            author: admin.peer_id()?,
            sequence: 1,
            previous: None,
            parents: vec![],
            created_at_ms: 0,
            body: EventBody::ConversationCreated {
                title: "invalid client event".into(),
            },
            blobs: vec![],
        })?;
        assert!(store.ingest_event(&network, &forged).is_err());
        assert!(
            store
                .authorize_node(&network, &admin.peer_id()?, None)
                .is_err()
        );
        assert!(
            store
                .authorize_node(&network, &node.peer_id()?, Some(NodeRole::Replica))
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn all_replicated_and_local_ids_are_network_scoped() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let scheduler = LocalIdentity::generate();
        let target = scheduler.peer_id()?;
        let mut store = memory(&a, Some(&scheduler))?;
        let na = a.fixture_network_id()?;
        let nb = b.fixture_network_id()?;
        store.install_trust_bundle(
            &nb,
            &b.sign_network_fixture(
                1,
                vec![b.certificate(roles())?, scheduler.certificate(roles())?],
            )?,
        )?;
        let ea = conversation(&mut store, &a)?;
        let eb = conversation(&mut store, &b)?;
        assert!(store.ingest_event(&nb, &ea).is_err());
        assert!(store.event(&na, &eb.hash)?.is_none());
        assert_eq!(store.conversations(&na)?.len(), 1);
        assert_eq!(store.conversations(&nb)?.len(), 1);
        let ma = admitted_message(&mut store, &a, "same-id", &target)?;
        let mb = admitted_message(&mut store, &b, "same-id", &target)?;
        assert_eq!(
            store
                .admit_trigger(&na, "conversation", &ma.hash, &target, &target)?
                .state,
            "queued"
        );
        assert_eq!(
            store
                .admit_trigger(&nb, "conversation", &mb.hash, &target, &target)?
                .state,
            "queued"
        );
        assert_eq!(
            store
                .claim_next(&na, "conversation", &target, "boot", "sdk")?
                .unwrap()
                .job
                .trigger,
            ma.hash
        );
        assert_eq!(
            store
                .claim_next(&nb, "conversation", &target, "boot", "sdk")?
                .unwrap()
                .job
                .trigger,
            mb.hash
        );
        assert_eq!(store.scheduler_jobs(&na, "conversation", &target)?.len(), 1);
        assert_eq!(store.scheduler_jobs(&nb, "conversation", &target)?.len(), 1);
        assert!(
            store
                .scheduler_job(&na, "conversation", &mb.hash, &target)?
                .is_none()
        );
        assert!(
            store
                .scheduler_job(&nb, "conversation", &ma.hash, &target)?
                .is_none()
        );
        store.local_set(&na, "auth", &json!("a"))?;
        store.local_set(&nb, "auth", &json!("b"))?;
        assert_eq!(store.local_value(&na, "auth")?, Some(json!("a")));
        assert_eq!(store.local_value(&nb, "auth")?, Some(json!("b")));
        let blob = BlobRef {
            hash: digest(b"private"),
            size: 7,
        };
        store.put_blob(&na, &blob, b"private")?;
        assert!(store.get_blob(&nb, &blob.hash)?.is_none());
        Ok(())
    }
    #[test]
    fn notifications_and_budget_are_durable_deduped_state() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let mut store = memory(&a, None)?;
        conversation(&mut store, &a)?;
        let origin = admitted_message(&mut store, &a, "request", &a.peer_id()?)?;
        let task = store.append_event(
            &a,
            &network,
            "conversation",
            EventBody::AgentMessage {
                message: AgentMessage {
                    message_id: "task".into(),
                    target_node: a.peer_id()?,
                    origin_trigger: origin.hash.clone(),
                    in_reply_to: None,
                    kind: AgentMessageKind::Task,
                    text: "work".into(),
                },
            },
            vec![],
            2,
        )?;
        let result = store.append_event(
            &a,
            &network,
            "conversation",
            EventBody::AgentMessage {
                message: AgentMessage {
                    message_id: "result".into(),
                    target_node: a.peer_id()?,
                    origin_trigger: origin.hash,
                    in_reply_to: Some(task.hash),
                    kind: AgentMessageKind::Completed,
                    text: "done".into(),
                },
            },
            vec![],
            2,
        )?;
        let notification = Notification {
            notification_id: "notice".into(),
            conversation_id: "conversation".into(),
            kind: "turn_completed".into(),
            text: "awaiting acceptance".into(),
            event_hash: result.hash.clone(),
        };
        store.queue_notification(&network, &notification)?;
        store.queue_notification(&network, &notification)?;
        assert_eq!(store.pending_notifications(&network, "phone")?.len(), 1);
        store.ack_notification(&network, "notice", "phone")?;
        assert!(store.pending_notifications(&network, "phone")?.is_empty());
        assert_eq!(store.pending_notifications(&network, "laptop")?.len(), 1);
        assert_eq!(store.reserve_budget(&network, "2026-10-16", 2, 3)?.used, 2);
        assert!(store.reserve_budget(&network, "2026-10-16", 2, 3).is_err());
        assert_eq!(store.budget_usage(&network, "2026-10-16", 3)?.used, 2);
        store.rebuild_projections(&network)?;
        assert!(store.pending_notifications(&network, "phone")?.is_empty());
        assert_eq!(store.pending_notifications(&network, "laptop")?.len(), 1);
        assert_eq!(store.budget_usage(&network, "2026-10-16", 3)?.used, 2);
        assert_eq!(
            store
                .scheduler_jobs(&network, "conversation", &a.peer_id()?)?
                .len(),
            1
        );
        assert!(
            store
                .scheduler_job(&network, "conversation", &result.hash, &a.peer_id()?)?
                .is_none()
        );
        Ok(())
    }
    #[test]
    fn replicated_notification_and_ack_rebuild_without_scheduler_admission() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let mut source = memory(&a, None)?;
        let mut target = memory(&a, None)?;
        let origin = conversation(&mut source, &a)?;
        let notification = Notification {
            notification_id: "sync-notice".into(),
            conversation_id: "conversation".into(),
            kind: "approval".into(),
            text: "human response needed".into(),
            event_hash: origin.hash,
        };
        let raised = source
            .raise_notification_once(&a, &network, notification.clone(), 2)?
            .context("notification event missing")?;
        let heads = source.heads(&network, "conversation")?;
        assert!(
            source
                .raise_notification_once(&a, &network, notification.clone(), 3)?
                .is_none()
        );
        for changed in [
            Notification {
                text: "changed text".into(),
                ..notification.clone()
            },
            Notification {
                kind: "changed-kind".into(),
                ..notification.clone()
            },
            Notification {
                conversation_id: "another-conversation".into(),
                ..notification.clone()
            },
        ] {
            assert_eq!(
                source
                    .raise_notification_once(&a, &network, changed, 3)
                    .unwrap_err()
                    .to_string(),
                "notification ID reused with different body"
            );
        }
        assert_eq!(source.heads(&network, "conversation")?, heads);
        assert_eq!(
            source.context(&network, "conversation", &[])?.events.len(),
            2
        );
        let expected = Notification {
            event_hash: raised.hash,
            ..notification
        };
        assert_eq!(
            source.pending_notifications(&network, "phone")?,
            vec![expected.clone()]
        );
        let snapshot = source.export_snapshot(&network, "conversation")?;
        assert!(target.import_snapshot(&network, &snapshot)?.is_complete());
        assert_eq!(
            target.pending_notifications(&network, "phone")?,
            vec![expected.clone()]
        );
        assert!(target.import_snapshot(&network, &snapshot)?.is_complete());
        assert_eq!(
            target.pending_notifications(&network, "phone")?,
            vec![expected.clone()]
        );
        assert_eq!(
            target.context(&network, "conversation", &[])?.events.len(),
            2
        );
        source.append_event(
            &a,
            &network,
            "conversation",
            EventBody::NotificationAcknowledged {
                notification_id: "sync-notice".into(),
                client_id: "phone".into(),
            },
            vec![],
            3,
        )?;
        target.import_snapshot(&network, &source.export_snapshot(&network, "conversation")?)?;
        assert!(target.pending_notifications(&network, "phone")?.is_empty());
        assert_eq!(
            target.pending_notifications(&network, "laptop")?,
            vec![expected.clone()]
        );
        target.rebuild_projections(&network)?;
        assert!(target.pending_notifications(&network, "phone")?.is_empty());
        assert_eq!(
            target.pending_notifications(&network, "laptop")?,
            vec![expected]
        );
        assert!(
            target
                .scheduler_jobs(&network, "conversation", &a.peer_id()?)?
                .is_empty()
        );
        assert!(target.scheduler_outbox_all(&network)?.is_empty());
        Ok(())
    }
    #[test]
    fn incompatible_membership_is_rejected_before_rebuilding_projections() -> Result<()> {
        let dir = std::env::temp_dir().join(format!(
            "vibeface-membership-preflight-{}",
            uuid::Uuid::new_v4()
        ));
        let db = dir.join("node.sqlite3");
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let mut store = Store::open(&db)?;
        store.install_trust_bundle(
            &network,
            &owner.sign_network_fixture(1, vec![owner.certificate(roles())?])?,
        )?;
        conversation(&mut store, &owner)?;
        let target = owner.peer_id()?;
        let message = admitted_message(&mut store, &owner, "running", &target)?;
        let claimed = store
            .claim_next(&network, "conversation", &target, "boot", "sdk")?
            .context("scheduler admission missing")?;
        let key = scheduler_key("job", "conversation", &target, &message.hash);
        let expected = serde_json::to_string(&claimed.job)?;
        drop(store);
        let connection = Connection::open(&db)?;
        connection.execute_batch(
            "CREATE TABLE preflight_projection_writes(count INTEGER NOT NULL);
             INSERT INTO preflight_projection_writes VALUES(0);
             CREATE TRIGGER preflight_conversation_delete AFTER DELETE ON conversations
             BEGIN UPDATE preflight_projection_writes SET count=count+1; END;
             CREATE TRIGGER preflight_conversation_insert AFTER INSERT ON conversations
             BEGIN UPDATE preflight_projection_writes SET count=count+1; END;",
        )?;
        connection.execute("UPDATE networks SET bundle='{}'", [])?;
        drop(connection);
        assert!(Store::open(&db).is_err());
        let connection = Connection::open(&db)?;
        let state: String = connection.query_row(
            "SELECT value FROM local_values WHERE network_id=? AND key=?",
            params![network, key],
            |r| r.get(0),
        )?;
        assert_eq!(state, expected);
        let writes: u64 =
            connection.query_row("SELECT count FROM preflight_projection_writes", [], |r| {
                r.get(0)
            })?;
        assert_eq!(writes, 0);
        let conversations: u64 =
            connection.query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))?;
        assert_eq!(conversations, 1);
        drop(connection);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn reopening_preserves_conversations_and_rejects_other_schemas() -> Result<()> {
        let root = std::env::temp_dir().join(format!("vibeface-core-{}", uuid::Uuid::new_v4()));
        let db = root.join("node.sqlite3");
        let a = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        {
            let mut store = Store::open(&db)?;
            store.install_trust_bundle(
                &network,
                &a.sign_network_fixture(1, vec![a.certificate(roles())?])?,
            )?;
            conversation(&mut store, &a)?;
        }
        {
            let store = Store::open(&db)?;
            assert_eq!(store.conversations(&network)?.len(), 1);
        }
        for version in [3, 4, 6] {
            let connection = Connection::open(&db)?;
            connection.execute_batch(&format!("PRAGMA user_version={version};"))?;
            drop(connection);
            assert!(Store::open(&db).is_err());
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
