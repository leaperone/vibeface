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
    path::{Component, Path},
};

pub(crate) mod bots;
pub(crate) mod followup;

const INITIAL_SCHEMA: &str = include_str!("../migrations/202610160001_initial.sql");

pub struct Store {
    connection: Connection,
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
        if path != Path::new(":memory:")
            && let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        {
            let existed = parent.exists();
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            if !existed {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        let connection = Connection::open(path)?;
        #[cfg(unix)]
        if path != Path::new(":memory:") {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: u32 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        match version {
            0 => {
                let tables:i64 = connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",[],|r|r.get(0))?;
                ensure!(tables == 0, "unversioned database is not supported");
                connection.execute_batch(&format!("BEGIN IMMEDIATE;{INITIAL_SCHEMA}COMMIT;"))?;
            }
            3 => {}
            _ => bail!(
                "unsupported database schema version {version}; initialize a fresh Node data directory"
            ),
        }
        let mut store = Self { connection };
        // Reject incompatible membership before recovery changes operations or rebuilds projections.
        for network in store.networks()? {
            store
                .trust_bundle(&network)?
                .context("membership is missing")?
                .project()?;
        }
        store
            .connection
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")?;
        store.recover_running()?;
        for network in store.networks()? {
            store.rebuild_projections(&network)?;
        }
        Ok(store)
    }
    pub fn install_trust_bundle(&mut self, network_id: &str, state: &NetworkState) -> Result<bool> {
        ensure!(
            state.genesis.network_id == network_id,
            "membership belongs to another personal network"
        );
        state.project()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<String> = tx
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
        tx.execute("INSERT INTO networks(network_id,revision,bundle) VALUES(?,?,?) ON CONFLICT(network_id) DO UPDATE SET revision=excluded.revision,bundle=excluded.bundle",params![network_id,merged.epoch(),encoded])?;
        for member in merged.historical_nodes()? {
            let old: Option<String> = tx
                .query_row(
                    "SELECT public_key FROM node_certificates WHERE network_id=? AND node_id=?",
                    params![network_id, member.subject_id],
                    |r| r.get(0),
                )
                .optional()?;
            ensure!(
                old.as_ref().is_none_or(|key| key == &member.public_key),
                "historical Node identity key changed"
            );
            tx.execute("INSERT OR IGNORE INTO node_certificates(network_id,node_id,public_key) VALUES(?,?,?)",params![network_id,member.subject_id,member.public_key])?;
        }
        tx.commit()?;
        Ok(true)
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
        node_id: &str,
        role: Option<NodeRole>,
    ) -> Result<NodeCertificate> {
        authorize_node_from(&self.connection, network_id, node_id, role)
    }
    pub fn save_local_identity(
        &mut self,
        network_id: &str,
        identity: &LocalIdentity,
    ) -> Result<()> {
        let node_id = identity.node_id()?;
        self.authorize_node(network_id, &node_id, None)?;
        self.connection.execute("INSERT INTO local_identities(network_id,node_id,identity) VALUES(?,?,?) ON CONFLICT(network_id,node_id) DO UPDATE SET identity=excluded.identity",params![network_id,node_id,serde_json::to_string(identity)?])?;
        Ok(())
    }
    pub fn local_identity(&self, network_id: &str, node_id: &str) -> Result<Option<LocalIdentity>> {
        optional_json(
            self.connection
                .query_row(
                    "SELECT identity FROM local_identities WHERE network_id=? AND node_id=?",
                    params![network_id, node_id],
                    |r| r.get(0),
                )
                .optional()?,
        )
    }
    fn public_key(&self, network_id: &str, author: &str) -> Result<String> {
        self.connection
            .query_row(
                "SELECT public_key FROM node_certificates WHERE network_id=? AND node_id=?",
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
                "SELECT envelope FROM events WHERE network_id=? AND hash=?",
                params![network_id, envelope.hash],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            ensure!(old == encoded, "conflicting envelope for event hash");
        } else {
            tx.execute("INSERT INTO events(network_id,hash,scope_id,author,sequence,envelope,status) VALUES(?,?,?,?,?,?,'pending')",params![network_id,envelope.hash,event.scope_id,event.author,event.sequence,encoded])?;
        }
        rebuild_projections_from(&tx, network_id)?;
        tx.commit()?;
        let status: String = self.connection.query_row(
            "SELECT status FROM events WHERE network_id=? AND hash=?",
            params![network_id, envelope.hash],
            |r| r.get(0),
        )?;
        Ok(IngestOutcome {
            hash: envelope.hash.clone(),
            status,
            missing: self.missing_dependencies(network_id, std::slice::from_ref(&envelope.hash))?,
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
        authorize_node_from(&tx, network, &identity.node_id()?, Some(NodeRole::Gateway))?;
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
        let receipt =
            admit_scheduler_from(&tx, network, conversation, trigger, local_node, target)?;
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
        let mut legacy_statement = tx.prepare("SELECT value FROM local_values WHERE network_id=? AND key LIKE 'scheduler:invocation:%'")?;
        let legacy = legacy_statement
            .query_map([network], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(legacy_statement);
        for encoded in legacy {
            let value: Value = serde_json::from_str(&encoded)?;
            let Some(trigger) = value["trigger"].as_str() else {
                continue;
            };
            let event: Option<String> = tx
                .query_row(
                    "SELECT envelope FROM events WHERE network_id=? AND hash=?",
                    params![network, trigger],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(event) = event else {
                continue;
            };
            let envelope: SignedEvent = serde_json::from_str(&event)?;
            let event: Event = serde_json::from_str(&envelope.canonical_body)?;
            if let EventBody::MessagePosted { scheduler_node, .. } = &event.body
                && scheduler_node.as_deref() != Some(target)
            {
                continue;
            }
            import_legacy_scheduler_job(&tx, network, &event.scope_id, trigger, target)?;
        }
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
            if session.unresolved.is_empty() || job.kind == TriggerKind::Human {
                queued.push(job);
            }
        }
        queued.sort_by_key(|job| (job.kind != TriggerKind::Human, job.sequence));
        let mut selected = None;
        for mut candidate in queued {
            let policy = if candidate.kind == TriggerKind::Autonomous {
                match followup::validate_followup_result_from(
                    &tx,
                    network,
                    conversation,
                    &candidate.trigger,
                    target,
                ) {
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
            && (!write || (!attempt.read_only && session.unresolved.is_empty())))
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
            identity.node_id()? == target,
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
                | JobState::Unknown { .. }
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
            identity.node_id()? == target,
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

    /// Runtime calls this only after old credentials are fenced and in-flight tools are accounted for.
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

    /// Runtime verifies owner identity and reconciliation evidence before resolving the write freeze.
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
    pub fn tasks(&self, network_id: &str, scope_id: &str) -> Result<Vec<Task>> {
        let mut statement=self.connection.prepare("SELECT task_id,conversation_id,title,goal,acceptance,status,detail,event_hash FROM tasks WHERE network_id=? AND conversation_id=? ORDER BY task_id")?;
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
    pub fn session(
        &self,
        network_id: &str,
        target_node: &str,
        session_id: &str,
    ) -> Result<Option<ExecutionSession>> {
        optional_json(self.connection.query_row("SELECT record FROM execution_sessions WHERE network_id=? AND target_node=? AND session_id=?",params![network_id,target_node,session_id],|r|r.get(0)).optional()?)
    }
    pub fn sessions(&self, network_id: &str) -> Result<Vec<ExecutionSession>> {
        let mut statement=self.connection.prepare("SELECT record FROM execution_sessions WHERE network_id=? ORDER BY target_node,session_id")?;
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
        let changed=self.connection.execute("INSERT INTO execution_sessions(network_id,target_node,session_id,revision,record) VALUES(?,?,?,?,?) ON CONFLICT(network_id,target_node,session_id) DO UPDATE SET revision=excluded.revision,record=excluded.record WHERE excluded.revision>=execution_sessions.revision",params![network_id,session.target_node,session.session_id,session.revision,serde_json::to_string(session)?])?;
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
                "SELECT request,state,result FROM operations WHERE network_id=? AND operation_id=?",
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
            "SELECT operation_id FROM operations WHERE network_id=? ORDER BY operation_id",
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

    fn verify_binding_creation(&self, proof: &crate::binding::SignedBinding) -> Result<()> {
        let binding = &proof.binding;
        let key = self.public_key(&binding.network_id, &binding.authority_node)?;
        crate::binding::verify_binding(proof, &key)?;
        self.verify_binding_task(binding, true)
    }

    fn verify_historical_binding(&self, proof: &crate::binding::SignedBinding) -> Result<()> {
        let binding = &proof.binding;
        let key = self.public_key(&binding.network_id, &binding.authority_node)?;
        crate::binding::verify_binding(proof, &key)?;
        self.verify_binding_task(binding, false)
    }

    fn verify_binding_task(
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

    pub fn bind_task(
        &mut self,
        identity: &LocalIdentity,
        binding: &crate::binding::TaskBinding,
    ) -> Result<crate::binding::SignedBinding> {
        ensure!(
            binding.authority_node == identity.node_id()?,
            "this Node is not the task binding authority"
        );
        let authority = self.authorize_node(&binding.network_id, &binding.authority_node, None)?;
        ensure!(
            authority.roles.contains(&NodeRole::Gateway)
                || authority.roles.contains(&NodeRole::Scheduler),
            "task authority cannot bind execution"
        );
        self.verify_binding_task(binding, true)?;
        let hash = crate::binding::binding_hash(binding)?;
        let record_key = format!("task-binding-record:{hash}");
        let current_key = format!("task-binding-current:{}", binding.task_ref);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ready: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM events WHERE network_id=? AND hash=? AND status='ready')",
            params![binding.network_id, binding.task_ref],
            |r| r.get(0),
        )?;
        ensure!(ready, "task creation changed before binding confirmation");
        if !validate_session_ownership_from(
            &tx,
            &binding.network_id,
            &binding.target_node,
            &binding.session_id,
            &binding.conversation_id,
        )? {
            validate_local_session_intents_from(
                &tx,
                &binding.network_id,
                &binding.target_node,
                &binding.session_id,
                &binding.conversation_id,
                &BTreeSet::from([identity.node_id()?]),
            )?;
        }
        let current: Option<String> = tx
            .query_row(
                "SELECT value FROM local_values WHERE network_id=? AND key=?",
                params![binding.network_id, current_key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(current) = current {
            let current: String = serde_json::from_str(&current)?;
            if current != hash {
                ensure!(
                    binding.previous_binding_hash.as_ref() == Some(&current),
                    "task is already bound to another execution; query its existing binding"
                );
                ensure!(
                    binding.previous_seal.is_some(),
                    "changing executor requires a sealed previous binding"
                );
            }
        } else {
            ensure!(
                binding.generation == 1 && binding.previous_binding_hash.is_none(),
                "initial binding must be generation one"
            );
        }
        let unsigned = serde_json::json!({"binding":binding,"signature":null});
        tx.execute(
            "INSERT OR IGNORE INTO local_values(network_id,key,value) VALUES(?,?,?)",
            params![
                binding.network_id,
                record_key,
                serde_json::to_string(&unsigned)?
            ],
        )?;
        tx.execute("INSERT INTO local_values(network_id,key,value) VALUES(?,?,?) ON CONFLICT(network_id,key) DO UPDATE SET value=excluded.value",params![binding.network_id,current_key,serde_json::to_string(&hash)?])?;
        tx.commit()?;
        // No binding proof leaves this Node before its unique value is durable.
        let proof = crate::binding::sign_binding(identity, binding)?;
        self.local_set(
            &binding.network_id,
            &record_key,
            &serde_json::to_value(&proof)?,
        )?;
        Ok(proof)
    }

    pub fn install_binding(&mut self, proof: &crate::binding::SignedBinding) -> Result<()> {
        self.verify_binding_creation(proof)?;
        self.install_verified_binding(proof)
    }

    fn install_verified_binding(&mut self, proof: &crate::binding::SignedBinding) -> Result<()> {
        let binding = &proof.binding;
        let hash = crate::binding::binding_hash(binding)?;
        let current_key = format!("task-binding-current:{}", binding.task_ref);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !validate_session_ownership_from(
            &tx,
            &binding.network_id,
            &binding.target_node,
            &binding.session_id,
            &binding.conversation_id,
        )? {
            let mut query =
                tx.prepare("SELECT node_id FROM local_identities WHERE network_id=?")?;
            let local_authors = query
                .query_map([&binding.network_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<BTreeSet<_>>>()?;
            validate_local_session_intents_from(
                &tx,
                &binding.network_id,
                &binding.target_node,
                &binding.session_id,
                &binding.conversation_id,
                &local_authors,
            )?;
        }
        let current: Option<String> = tx
            .query_row(
                "SELECT value FROM local_values WHERE network_id=? AND key=?",
                params![binding.network_id, current_key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(current) = current {
            let current: String = serde_json::from_str(&current)?;
            if current != hash {
                let record: String = tx.query_row(
                    "SELECT value FROM local_values WHERE network_id=? AND key=?",
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
                        "INSERT OR IGNORE INTO local_values(network_id,key,value) VALUES(?,?,?)",
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
        tx.execute("INSERT INTO local_values(network_id,key,value) VALUES(?,?,?) ON CONFLICT(network_id,key) DO UPDATE SET value=excluded.value",params![binding.network_id,format!("task-binding-record:{hash}"),serde_json::to_string(proof)?])?;
        tx.execute("INSERT INTO local_values(network_id,key,value) VALUES(?,?,?) ON CONFLICT(network_id,key) DO UPDATE SET value=excluded.value",params![binding.network_id,current_key,serde_json::to_string(&hash)?])?;
        if let Some(seal) = &binding.previous_seal {
            tx.execute(
                "INSERT OR IGNORE INTO local_values(network_id,key,value) VALUES(?,?,?)",
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

    pub fn seal_binding(
        &mut self,
        identity: &LocalIdentity,
        proof: &crate::binding::SignedBinding,
    ) -> Result<crate::binding::SignedSeal> {
        self.verify_binding_creation(proof)?;
        let binding = &proof.binding;
        ensure!(
            binding.target_node == identity.node_id()?,
            "only the bound executor can seal execution"
        );
        let hash = crate::binding::binding_hash(binding)?;
        let key = format!("task-binding-seal:{hash}");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT value FROM local_values WHERE network_id=? AND key=?",
                params![binding.network_id, key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            let value: Value = serde_json::from_str(&existing)?;
            let seal: crate::binding::BindingSeal = serde_json::from_value(value["seal"].clone())?;
            tx.commit()?;
            let signed = crate::binding::sign_seal(identity, &seal)?;
            self.local_set(&binding.network_id, &key, &serde_json::to_value(&signed)?)?;
            return Ok(signed);
        }
        let busy:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE network_id=? AND target_node=? AND session_id=? AND state IN ('\"accepted\"','\"running\"','\"unknown\"'))",params![binding.network_id,binding.target_node,binding.session_id],|r|r.get(0))?;
        ensure!(
            !busy,
            "cannot seal an unresolved execution; reconcile unknown results first"
        );
        let pending = {
            let mut statement=tx.prepare("SELECT o.operation_id,o.state FROM operations o JOIN local_values v ON v.network_id=o.network_id AND v.key='completion:' || o.operation_id WHERE o.network_id=? AND o.target_node=? AND o.session_id=?")?;
            statement
                .query_map(
                    params![binding.network_id, binding.target_node, binding.session_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        if !pending.is_empty() {
            let events = load_events_from(&tx, &binding.network_id)?;
            for (operation, state) in pending {
                let state: OperationState = serde_json::from_str(&state)?;
                let expected = serde_json::to_value(state)?
                    .as_str()
                    .context("invalid operation state")?
                    .to_owned();
                ensure!(events.values().any(|stored|stored.status == "ready" && stored.event.author == binding.target_node
                    && stored.event.scope_id == binding.conversation_id
                    && matches!(&stored.event.body,EventBody::ExecutionObserved{operation_id,target_node,session_id,status,..}
                        if operation_id == &operation && target_node == &binding.target_node && session_id.as_deref() == Some(binding.session_id.as_str()) && status == &expected)),
                    "execution completion must be durable before sealing");
            }
        }
        let session:Option<String>=tx.query_row("SELECT record FROM execution_sessions WHERE network_id=? AND target_node=? AND session_id=?",params![binding.network_id,binding.target_node,binding.session_id],|r|r.get(0)).optional()?;
        let revision = if let Some(session) = session {
            let session: ExecutionSession = serde_json::from_str(&session)?;
            ensure!(
                matches!(session.state.as_str(), "idle" | "closed"),
                "native execution session is not idle"
            );
            session.revision
        } else {
            0
        };
        let seal = crate::binding::BindingSeal {
            network_id: binding.network_id.clone(),
            binding_hash: hash,
            target_node: binding.target_node.clone(),
            final_revision: revision,
        };
        tx.execute(
            "INSERT INTO local_values(network_id,key,value) VALUES(?,?,?)",
            params![
                binding.network_id,
                key,
                serde_json::to_string(&serde_json::json!({"seal":seal,"signature":null}))?
            ],
        )?;
        tx.commit()?;
        let signed = crate::binding::sign_seal(identity, &seal)?;
        self.local_set(&binding.network_id, &key, &serde_json::to_value(&signed)?)?;
        Ok(signed)
    }

    pub fn validate_operation_binding(&self, request: &OperationRequest) -> Result<()> {
        if let Some(proof) = &request.binding {
            self.verify_binding_creation(proof)?;
        }
        validate_operation_binding_from(&self.connection, request)
    }

    #[cfg(test)]
    pub(crate) fn bind_test_operation(
        &mut self,
        identity: &LocalIdentity,
        request: &mut OperationRequest,
    ) -> Result<()> {
        let task_id = request
            .task_id
            .get_or_insert_with(|| format!("fixture-{}", request.operation_id))
            .clone();
        let creation =
            match self.task_creation(&request.network_id, &request.conversation_id, &task_id) {
                Ok(event) => event,
                Err(_) => self.append_event(
                    identity,
                    &request.network_id,
                    &request.conversation_id,
                    EventBody::TaskCreated {
                        task_id: task_id.clone(),
                        title: "fixture".into(),
                        goal: "fixture".into(),
                        acceptance: "fixture".into(),
                    },
                    vec![],
                    2,
                )?,
            };
        if let Some(proof) = self.task_binding(&request.network_id, &creation.hash)? {
            request.binding = Some(proof);
            return Ok(());
        }
        let open_spec =
            if request.action["type"] == "open" && request.action["executor"].is_string() {
                request.action.clone()
            } else if let Some(session) = self.session(
                &request.network_id,
                &request.target_node,
                request
                    .session_id
                    .as_deref()
                    .context("execution session is required")?,
            )? {
                serde_json::json!({"type":"open","executor":session.executor,"cwd":session.cwd})
            } else {
                serde_json::json!({"type":"open","executor":"codex","cwd":"/tmp"})
            };
        let binding = crate::binding::TaskBinding {
            network_id: request.network_id.clone(),
            conversation_id: request.conversation_id.clone(),
            task_ref: creation.hash,
            task_id,
            authority_node: identity.node_id()?,
            generation: 1,
            target_node: request.target_node.clone(),
            session_id: request
                .session_id
                .clone()
                .context("execution session is required")?,
            open_spec,
            previous_binding_hash: None,
            previous_seal: None,
        };
        request.binding = Some(self.bind_task(identity, &binding)?);
        Ok(())
    }

    pub fn accept_operation(
        &mut self,
        network_id: &str,
        request: &OperationRequest,
    ) -> Result<OperationRecord> {
        self.admit_operation(network_id, request, false)
    }
    pub fn accept_control_operation(
        &mut self,
        network_id: &str,
        request: &OperationRequest,
    ) -> Result<OperationRecord> {
        ensure!(
            request.resources.is_empty(),
            "control operations cannot reserve resources"
        );
        ensure!(
            matches!(
                request.action.get("type").and_then(Value::as_str),
                Some("read" | "steer" | "interrupt" | "respond_request")
            ),
            "invalid control operation action"
        );
        self.admit_operation(network_id, request, true)
    }
    fn admit_operation(
        &mut self,
        network_id: &str,
        request: &OperationRequest,
        control: bool,
    ) -> Result<OperationRecord> {
        ensure!(
            request.network_id == network_id,
            "operation network mismatch"
        );
        ensure!(!request.operation_id.is_empty(), "operation ID is required");
        ensure!(
            if request.action["type"] == "command" {
                request.session_id.is_none() && request.expected_revision == 0 && !control
            } else {
                request
                    .session_id
                    .as_deref()
                    .is_some_and(|id| !id.is_empty())
            },
            "SDK operations require a session and commands must be sessionless"
        );
        ensure!(
            request.expected_revision < i64::MAX as u64,
            "revision overflow"
        );
        ensure!(
            request.resources.len() <= 128,
            "too many resource reservations"
        );
        self.validate_operation_binding(request)?;
        let encoded = serde_json::to_string(request)?;
        ensure!(encoded.len() <= MAX_EVENT_BYTES, "operation too large");
        if let Some(old) = self.operation(network_id, &request.operation_id)? {
            ensure!(
                old.request == *request,
                "operation ID reused with different body"
            );
            return Ok(old);
        }
        self.authorize_node(network_id, &request.target_node, Some(NodeRole::Executor))?;
        let complete = self.context(network_id, &request.conversation_id, &request.basis_heads)?;
        ensure!(
            complete.missing.is_complete(),
            "required context is incomplete or conflicting"
        );
        let mut resources = request
            .resources
            .iter()
            .map(|r| canonical_resource(r))
            .collect::<Result<Vec<_>>>()?;
        resources.sort();
        resources.dedup();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<(String, String, String)> = tx
            .query_row(
                "SELECT request,state,result FROM operations WHERE network_id=? AND operation_id=?",
                params![network_id, request.operation_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((old, state, result)) = old {
            ensure!(old == encoded, "operation ID reused with different body");
            return Ok(OperationRecord {
                request: request.clone(),
                state: serde_json::from_str(&state)?,
                result: serde_json::from_str(&result)?,
            });
        }
        followup::validate_manager_dispatch_from(&tx, network_id, request)?;
        validate_operation_binding_from(&tx, request)?;
        for event in &complete.events {
            let hash = digest(serde_json::to_string(event)?.as_bytes());
            let ready:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE network_id=? AND hash=? AND status='ready')",params![network_id,hash],|r|r.get(0))?;
            ensure!(ready, "context changed during admission");
        }
        let revision:Option<u64>=tx.query_row("SELECT revision FROM execution_sessions WHERE network_id=? AND target_node=? AND session_id=?",params![network_id,request.target_node,request.session_id],|r|r.get(0)).optional()?;
        ensure!(
            revision.unwrap_or(0) == request.expected_revision,
            "stale execution session revision"
        );
        let busy:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE network_id=? AND target_node=? AND session_id=? AND state IN ('\"accepted\"','\"running\"','\"unknown\"') AND (state='\"unknown\"' OR ?=0 OR control=1))",params![network_id,request.target_node,request.session_id,control],|r|r.get(0))?;
        ensure!(!busy, "execution session has an unresolved operation");
        {
            let mut statement =
                tx.prepare("SELECT resource FROM resource_reservations WHERE target_node=?")?;
            let existing =
                statement.query_map(params![request.target_node], |r| r.get::<_, String>(0))?;
            for row in existing {
                let existing = row?;
                ensure!(
                    !resources.iter().any(|r| paths_overlap(r, &existing)),
                    "resource overlaps an unresolved operation on this device"
                );
            }
        }
        tx.execute("INSERT INTO operations(network_id,operation_id,target_node,session_id,request,state,control,result) VALUES(?,?,?,?,?,'\"accepted\"',?,'null')",params![network_id,request.operation_id,request.target_node,request.session_id,encoded,control])?;
        for resource in resources {
            tx.execute("INSERT INTO resource_reservations(network_id,operation_id,target_node,resource) VALUES(?,?,?,?)",params![network_id,request.operation_id,request.target_node,resource])?;
        }
        tx.commit()?;
        Ok(OperationRecord {
            request: request.clone(),
            state: OperationState::Accepted,
            result: Value::Null,
        })
    }
    pub fn claim_operation(
        &mut self,
        network_id: &str,
        operation_id: &str,
    ) -> Result<OperationRecord> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let encoded: String = tx.query_row(
            "SELECT request FROM operations WHERE network_id=? AND operation_id=?",
            params![network_id, operation_id],
            |r| r.get(0),
        )?;
        let request: OperationRequest = serde_json::from_str(&encoded)?;
        validate_operation_binding_from(&tx, &request)?;
        followup::validate_manager_dispatch_from(&tx, network_id, &request)?;
        authorize_node_from(
            &tx,
            network_id,
            &request.target_node,
            Some(NodeRole::Executor),
        )?;
        let revision:Option<u64>=tx.query_row("SELECT revision FROM execution_sessions WHERE network_id=? AND target_node=? AND session_id=?",params![network_id,request.target_node,request.session_id],|r|r.get(0)).optional()?;
        ensure!(
            revision.unwrap_or(0) == request.expected_revision,
            "execution session changed before claim"
        );
        let events = load_events_from(&tx, network_id)?;
        let heads = if request.basis_heads.is_empty() {
            heads_from_events(&events, &request.conversation_id)
        } else {
            request.basis_heads.clone()
        };
        for hash in closure_hashes(&events, &heads) {
            let event = events
                .get(&hash)
                .context("operation basis is missing before claim")?;
            ensure!(
                event.status == "ready",
                "operation basis became incomplete or conflicting"
            );
            for blob in &event.event.blobs {
                ensure!(
                    has_blob_from(&tx, network_id, blob)?,
                    "operation basis content became incomplete"
                );
            }
        }
        let changed=tx.execute("UPDATE operations SET state='\"running\"' WHERE network_id=? AND operation_id=? AND state='\"accepted\"'",params![network_id,operation_id])?;
        ensure!(
            changed == 1,
            "operation is not accepted or has already been claimed"
        );
        tx.commit()?;
        self.operation(network_id, operation_id)?
            .context("operation disappeared")
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
                "SELECT request,state,result FROM operations WHERE network_id=? AND operation_id=?",
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
            "UPDATE operations SET state=?,result=? WHERE network_id=? AND operation_id=?",
            params![
                serde_json::to_string(&state)?,
                serde_json::to_string(&result)?,
                network_id,
                operation_id
            ],
        )?;
        if state.terminal() {
            tx.execute(
                "DELETE FROM resource_reservations WHERE network_id=? AND operation_id=?",
                params![network_id, operation_id],
            )?;
            let session:Option<String>=tx.query_row("SELECT record FROM execution_sessions WHERE network_id=? AND target_node=? AND session_id=?",params![network_id,request.target_node,request.session_id],|r|r.get(0)).optional()?;
            if let Some(session) = session {
                let mut session: ExecutionSession = serde_json::from_str(&session)?;
                session.revision = session.revision.max(request.expected_revision + 1);
                tx.execute("UPDATE execution_sessions SET revision=?,record=? WHERE network_id=? AND target_node=? AND session_id=?",params![session.revision,serde_json::to_string(&session)?,network_id,request.target_node,request.session_id])?;
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
        let count = self.connection.execute(
            "UPDATE operations SET state='\"unknown\"' WHERE state='\"running\"'",
            [],
        )?;
        self.connection.execute("UPDATE continuation_claims SET record=json_set(record,'$.state','unknown') WHERE json_extract(record,'$.state')='running'",[])?;
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
            self.local_identity(network_id, &event.author)?.is_some(),
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
        let changed=tx.execute("INSERT OR IGNORE INTO continuation_claims(network_id,result_event_hash,policy_version,record) VALUES(?,?,?,?)",params![network_id,result_event_hash,policy_version,serde_json::to_string(&claim)?])?;
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
        optional_json(self.connection.query_row("SELECT record FROM continuation_claims WHERE network_id=? AND result_event_hash=? AND policy_version=?",params![network_id,result_event_hash,policy_version],|r|r.get(0)).optional()?)
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
        let record:String=tx.query_row("SELECT record FROM continuation_claims WHERE network_id=? AND result_event_hash=? AND policy_version=?",params![network_id,result_event_hash,policy_version],|r|r.get(0))?;
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
        tx.execute("UPDATE continuation_claims SET record=? WHERE network_id=? AND result_event_hash=? AND policy_version=?",params![serde_json::to_string(&claim)?,network_id,result_event_hash,policy_version])?;
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
    job.state = match marker.outcome {
        JobOutcome::Completed => JobState::Completed { attempt },
        JobOutcome::Failed(detail) => JobState::Failed { attempt, detail },
        JobOutcome::Cancelled(evidence) => JobState::Cancelled {
            attempt: Some(attempt),
            evidence,
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
                detail,
                fenced: false,
            }
        }
    };
    if !matches!(job.state, JobState::Unknown { .. }) {
        session.active = None;
    }
    scheduler_save(connection, network, &key, &job)?;
    save_scheduler_session(connection, network, &session)?;
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
    match event.body {
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
        EventBody::ExecutionObserved { status, .. }
            if matches!(
                status.as_str(),
                "completed" | "succeeded" | "failed" | "rejected" | "interrupted"
            ) =>
        {
            Ok(TriggerKind::Autonomous)
        }
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
) -> Result<AdmissionReceipt> {
    let kind = validated_scheduler_trigger(connection, network, conversation, trigger, target)?;
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
    import_legacy_scheduler_job(connection, network, conversation, trigger, target)?;
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

fn import_legacy_scheduler_job(
    connection: &Connection,
    network: &str,
    conversation: &str,
    trigger: &str,
    target: &str,
) -> Result<()> {
    let key = scheduler_key("job", conversation, target, trigger);
    if scheduler_load::<SchedulerJob>(connection, network, &key)?.is_some() {
        return Ok(());
    }
    let legacy_key = format!(
        "scheduler:invocation:{}",
        digest(&serde_json::to_vec(&(conversation, trigger))?)
    );
    let Some(value) = scheduler_load::<Value>(connection, network, &legacy_key)? else {
        return Ok(());
    };
    ensure!(
        value["trigger"].as_str() == Some(trigger),
        "legacy invocation trigger does not match its ledger key"
    );
    let encoded: String = connection
        .query_row(
            "SELECT envelope FROM events WHERE network_id=? AND hash=? AND status='ready'",
            params![network, trigger],
            |row| row.get(0),
        )
        .optional()?
        .context("legacy scheduler trigger is missing or not ready")?;
    let envelope: SignedEvent = serde_json::from_str(&encoded)?;
    let event: Event = serde_json::from_str(&envelope.canonical_body)?;
    let public_key: String = connection
        .query_row(
            "SELECT public_key FROM node_certificates WHERE network_id=? AND node_id=?",
            params![network, event.author],
            |row| row.get(0),
        )
        .optional()?
        .context("legacy scheduler author has no historical Node identity")?;
    let event = verify_signed_event(network, &envelope, &public_key)?;
    ensure!(
        envelope.hash == trigger && event.scope_id == conversation,
        "legacy scheduler trigger belongs to another conversation or identity"
    );
    // The existing local ledger proves prior admission; importing a terminal never grants new authority.
    let kind = match event.body {
        EventBody::MessagePosted {
            role,
            scheduler_node,
            ..
        } if role == "user" => {
            ensure!(
                scheduler_node.as_deref() == Some(target),
                "legacy message is addressed to another scheduler"
            );
            TriggerKind::Human
        }
        EventBody::ExecutionObserved { status, .. }
            if matches!(
                status.as_str(),
                "completed" | "succeeded" | "failed" | "rejected" | "interrupted"
            ) =>
        {
            TriggerKind::Autonomous
        }
        _ => bail!("invalid legacy scheduler trigger"),
    };
    let mut session = load_scheduler_session(connection, network, conversation, target)?;
    let attempt = ManagerAttempt {
        id: format!("legacy-{}", digest(legacy_key.as_bytes())),
        boot: value["boot"].as_str().unwrap_or("legacy").into(),
        read_only: false,
        permission_version: session.permission_version,
        followup_version: None,
    };
    let state = match value["state"].as_str() {
        Some("completed") => JobState::Completed { attempt },
        Some("failed") => JobState::Failed {
            attempt,
            detail: "imported legacy failed invocation".into(),
        },
        Some("running" | "unknown") => {
            session.unresolved.insert(trigger.into());
            if session.active.is_none() {
                session.active = Some(trigger.into());
            }
            session.native_id = None;
            JobState::Unknown {
                attempt,
                detail: "legacy management invocation requires reconciliation; it was not replayed"
                    .into(),
                fenced: false,
            }
        }
        _ => bail!("unsupported legacy scheduler invocation state"),
    };
    let job = SchedulerJob {
        version: SCHEDULING_VERSION,
        conversation_id: conversation.into(),
        trigger: trigger.into(),
        target_node: target.into(),
        kind,
        sequence: session.next_sequence,
        basis_heads: vec![trigger.into()],
        message_id: value["message_id"]
            .as_str()
            .context("legacy invocation message ID missing")?
            .into(),
        state,
    };
    session.next_sequence = session
        .next_sequence
        .checked_add(1)
        .context("scheduler sequence overflow")?;
    scheduler_save(connection, network, &key, &job)?;
    save_scheduler_session(connection, network, &session)?;
    Ok(())
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

fn append_event_from(
    connection: &Connection,
    identity: &LocalIdentity,
    network_id: &str,
    scope_id: &str,
    body: EventBody,
    blobs: Vec<BlobRef>,
    created_at_ms: i64,
) -> Result<SignedEvent> {
    let author = identity.node_id()?;
    authorize_node_from(connection, network_id, &author, None)?;
    let events = load_events_from(connection, network_id)?;
    let heads = heads_from_events(&events, scope_id);
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
    connection.execute("DELETE FROM tasks WHERE network_id=?", [network_id])?;
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
            EventBody::TaskCreated {
                task_id,
                title,
                goal,
                acceptance,
            } => {
                connection.execute("INSERT OR IGNORE INTO tasks(network_id,conversation_id,task_id,title,goal,acceptance,status,detail,event_hash) VALUES(?,?,?,?,?,?,'open','',?)",params![network_id,event.scope_id,task_id,title,goal,acceptance,hash])?;
            }
            EventBody::TaskChanged {
                task_id,
                status,
                detail,
            } => {
                connection.execute("UPDATE tasks SET status=?,detail=?,event_hash=? WHERE network_id=? AND conversation_id=? AND task_id=?",params![status,detail,hash,network_id,event.scope_id,task_id])?;
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
    Ok(())
}
fn authorize_node_from(
    connection: &Connection,
    network_id: &str,
    node_id: &str,
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
    view.authorize_node(node_id, role)?.certificate()
}
fn validate_operation_binding_from(
    connection: &Connection,
    request: &OperationRequest,
) -> Result<()> {
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
        "SELECT EXISTS(SELECT 1 FROM events WHERE network_id=? AND hash=? AND status='ready')",
        params![binding.network_id, binding.task_ref],
        |r| r.get(0),
    )?;
    ensure!(ready, "task creation is incomplete or conflicting");
    let hash = crate::binding::binding_hash(binding)?;
    let current: Option<String> = connection
        .query_row(
            "SELECT value FROM local_values WHERE network_id=? AND key=?",
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
        "SELECT EXISTS(SELECT 1 FROM local_values WHERE network_id=? AND key=?)",
        params![request.network_id, format!("task-binding-seal:{hash}")],
        |r| r.get(0),
    )?;
    ensure!(
        !sealed,
        "execution binding is sealed; old commands cannot resume it"
    );
    Ok(())
}

fn validate_session_ownership_from(
    connection: &Connection,
    network: &str,
    target: &str,
    session: &str,
    conversation: &str,
) -> Result<bool> {
    let mut owned = false;
    let mut query = connection.prepare(
        "SELECT request FROM operations WHERE network_id=? AND target_node=? AND session_id=?",
    )?;
    for encoded in query.query_map(params![network, target, session], |row| {
        row.get::<_, String>(0)
    })? {
        let previous: OperationRequest = serde_json::from_str(&encoded?)?;
        ensure!(
            previous.conversation_id == conversation,
            "execution session belongs to another conversation"
        );
        owned = true;
    }
    let mut query = connection.prepare(
        "SELECT value FROM local_values WHERE network_id=? AND key LIKE 'task-binding-record:%'",
    )?;
    for encoded in query.query_map([network], |row| row.get::<_, String>(0))? {
        let value: Value = serde_json::from_str(&encoded?)?;
        let binding: crate::binding::TaskBinding =
            serde_json::from_value(value["binding"].clone())?;
        if binding.target_node == target && binding.session_id == session {
            ensure!(
                binding.conversation_id == conversation,
                "execution session binding belongs to another conversation"
            );
            owned = true;
        }
    }
    let events = load_events_from(connection, network)?;
    for (hash, stored) in &events {
        if stored.status != "ready" {
            continue;
        }
        let EventBody::ExecutionObserved {
            operation_id,
            task_id,
            target_node,
            session_id,
            status,
            ..
        } = &stored.event.body
        else {
            continue;
        };
        if stored.event.author != target
            || target_node != target
            || session_id.as_deref() != Some(session)
            || !matches!(
                status.as_str(),
                "running"
                    | "unknown"
                    | "succeeded"
                    | "failed"
                    | "rejected"
                    | "needs_input"
                    | "progress"
                    | "completed"
                    | "interrupted"
            )
        {
            continue;
        }
        let acknowledged = closure_hashes(&events, std::slice::from_ref(hash))
            .iter()
            .any(|ancestor| {
                let Some(dispatch) = events.get(ancestor) else {
                    return false;
                };
                let EventBody::DispatchRecorded {
                    operation_id: original_id,
                    task_id: original_task,
                    target_node: original_target,
                    session_id: original_session,
                    action,
                } = &dispatch.event.body
                else {
                    return false;
                };
                if dispatch.status != "ready"
                    || dispatch.event.scope_id != stored.event.scope_id
                    || original_id != operation_id
                    || original_task != task_id
                    || original_target != target_node
                    || original_session != session_id
                {
                    return false;
                }
                let Ok(request) = serde_json::from_value::<OperationRequest>(action.clone()) else {
                    return false;
                };
                request.network_id == network
                    && request.conversation_id == stored.event.scope_id
                    && request.operation_id == *operation_id
                    && request.task_id == *task_id
                    && request.target_node == target
                    && request.session_id.as_deref() == Some(session)
            });
        if acknowledged {
            ensure!(
                stored.event.scope_id == conversation,
                "execution session acknowledged by its executor belongs to another conversation"
            );
            owned = true;
        }
    }
    Ok(owned)
}

fn validate_local_session_intents_from(
    connection: &Connection,
    network: &str,
    target: &str,
    session: &str,
    conversation: &str,
    local_authors: &BTreeSet<String>,
) -> Result<()> {
    for stored in load_events_from(connection, network)?.values() {
        if stored.status != "ready" || !local_authors.contains(&stored.event.author) {
            continue;
        }
        let EventBody::DispatchRecorded {
            operation_id,
            task_id,
            target_node,
            session_id,
            action,
        } = &stored.event.body
        else {
            continue;
        };
        let Ok(request) = serde_json::from_value::<OperationRequest>(action.clone()) else {
            continue;
        };
        if target_node == target
            && session_id.as_deref() == Some(session)
            && request.network_id == network
            && request.conversation_id == stored.event.scope_id
            && request.operation_id == *operation_id
            && request.task_id == *task_id
            && request.target_node == *target_node
            && request.session_id == *session_id
        {
            ensure!(
                request.conversation_id == conversation,
                "execution session has a local dispatch for another conversation; reconcile its receipt before binding"
            );
        }
    }
    let mut query = connection.prepare(
        "SELECT value FROM local_values WHERE network_id=? AND key LIKE 'agent-calls:v1:%'",
    )?;
    for encoded in query.query_map([network], |row| row.get::<_, String>(0))? {
        let calls: Value = serde_json::from_str(&encoded?)?;
        for call in calls
            .as_object()
            .context("invalid prepared call index")?
            .values()
        {
            let scope = call["conversation_id"]
                .as_str()
                .context("invalid prepared call conversation")?;
            for request in call["requests"]
                .as_array()
                .context("invalid prepared call requests")?
            {
                let request: OperationRequest = serde_json::from_value(request.clone())?;
                ensure!(
                    request.network_id == network && request.conversation_id == scope,
                    "prepared call request has a different ownership scope"
                );
                if request.target_node == target && request.session_id.as_deref() == Some(session) {
                    ensure!(
                        scope == conversation,
                        "execution session has a local prepared call for another conversation; reconcile its receipt before binding"
                    );
                }
            }
        }
    }
    Ok(())
}

fn validate_unbound_operation_from(
    connection: &Connection,
    request: &OperationRequest,
) -> Result<()> {
    let Some(session) = request.session_id.as_deref() else {
        ensure!(
            request.action["type"] == "command",
            "sessionless operation must be a command"
        );
        return Ok(());
    };
    let mut query = connection.prepare(
        "SELECT request FROM operations WHERE network_id=? AND target_node=? AND session_id=?",
    )?;
    for encoded in query.query_map(
        params![request.network_id, request.target_node, session],
        |r| r.get::<_, String>(0),
    )? {
        let previous: OperationRequest = serde_json::from_str(&encoded?)?;
        ensure!(
            previous.conversation_id == request.conversation_id,
            "execution session belongs to another conversation"
        );
        ensure!(
            previous.task_id.is_none() && previous.binding.is_none(),
            "task-bound session cannot be used without its binding"
        );
    }
    let mut query = connection.prepare(
        "SELECT value FROM local_values WHERE network_id=? AND key LIKE 'task-binding-record:%'",
    )?;
    for value in query.query_map([&request.network_id], |r| r.get::<_, String>(0))? {
        let value: Value = serde_json::from_str(&value?)?;
        let binding: crate::binding::TaskBinding =
            serde_json::from_value(value["binding"].clone())?;
        ensure!(
            binding.target_node != request.target_node || binding.session_id != session,
            "task-bound session cannot be downgraded to ordinary execution"
        );
    }
    Ok(())
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
            "SELECT public_key FROM node_certificates WHERE network_id=? AND node_id=?",
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
    let content: Option<Vec<u8>> = connection
        .query_row(
            "SELECT content FROM blobs WHERE network_id=? AND hash=? AND size=?",
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
fn canonical_resource(resource: &str) -> Result<String> {
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
fn paths_overlap(left: &str, right: &str) -> bool {
    let left = Path::new(left);
    let right = Path::new(right);
    left.starts_with(right) || right.starts_with(left)
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
    fn roles() -> Vec<NodeRole> {
        vec![
            NodeRole::Gateway,
            NodeRole::Replica,
            NodeRole::Scheduler,
            NodeRole::Executor,
        ]
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
            author: owner.node_id()?,
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
                &identity.node_id()?,
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
        let target = owner.node_id()?;
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
                    JobOutcome::Unknown("cleanup must not erase the known reply".into()),
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
        let target = owner.node_id()?;
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
            author: peer.node_id()?,
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
            author: peer.node_id()?,
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
                    JobOutcome::Unknown("failed cleanup".into()),
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
            let target = owner.node_id()?;
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
        let target = owner.node_id()?;
        let mut store = memory(&owner, None)?;
        conversation(&mut store, &owner)?;
        let first = admitted_message(&mut store, &owner, "one", &target)?;
        let one = store
            .claim_next(&network, "conversation", &target, "boot", "sdk")?
            .unwrap();
        let autonomous = store.append_event(
            &owner,
            &network,
            "conversation",
            EventBody::ExecutionObserved {
                operation_id: "op".into(),
                task_id: None,
                target_node: target.clone(),
                session_id: Some("session".into()),
                status: "completed".into(),
                detail: json!({}),
            },
            vec![],
            3,
        )?;
        store.enqueue_trigger(&network, "conversation", &autonomous.hash, &target)?;
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
    fn scheduler_recovers_only_claimed_and_unknown_requires_fence_and_owner_resolution()
    -> Result<()> {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let target = owner.node_id()?;
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
        store.recover_scheduler_jobs(&network, &target, "boot-c")?;
        assert!(matches!(
            store
                .scheduler_job(&network, "conversation", &first.hash, &target)?
                .unwrap()
                .state,
            JobState::Unknown { .. }
        ));
        assert!(!store.manager_attempt_allowed(
            &network,
            "conversation",
            &target,
            &first.hash,
            &attempt,
            true
        )?);
        let second = admitted_message(&mut store, &owner, "two", &target)?;
        assert!(
            store
                .claim_next(&network, "conversation", &target, "boot-c", "sdk")?
                .is_none()
        );
        assert!(
            store
                .resolve_manager_unknown(
                    &network,
                    "conversation",
                    &target,
                    &first.hash,
                    &attempt,
                    "owner checked"
                )
                .is_err()
        );
        store.fence_manager_unknown(&network, "conversation", &target, &first.hash, &attempt)?;
        let read = store
            .claim_next(&network, "conversation", &target, "boot-c", "sdk")?
            .unwrap();
        assert_eq!(read.job.trigger, second.hash);
        let read_attempt = read.job.state.attempt().unwrap();
        assert!(read_attempt.read_only && read.previous_native_id.is_none());
        assert!(store.manager_attempt_allowed(
            &network,
            "conversation",
            &target,
            &second.hash,
            &read_attempt.id,
            false
        )?);
        assert!(!store.manager_attempt_allowed(
            &network,
            "conversation",
            &target,
            &second.hash,
            &read_attempt.id,
            true
        )?);
        store.finish_job(
            &owner,
            &network,
            "conversation",
            &target,
            &second.hash,
            &read_attempt.id,
            JobOutcome::Failed("done reading".into()),
            vec![],
            5,
        )?;
        assert!(store.scheduler_maintenance_idle(&network).is_err());
        store.connection.execute_batch("CREATE TRIGGER reject_manager_resolution BEFORE INSERT ON local_values WHEN NEW.key LIKE 'scheduler:v1:resolution:%' BEGIN SELECT RAISE(ABORT,'injected evidence failure'); END;")?;
        assert!(
            store
                .resolve_manager_unknown(
                    &network,
                    "conversation",
                    &target,
                    &first.hash,
                    &attempt,
                    "owner checked"
                )
                .is_err()
        );
        assert!(store.scheduler_maintenance_idle(&network).is_err());
        store
            .connection
            .execute_batch("DROP TRIGGER reject_manager_resolution;")?;
        store.resolve_manager_unknown(
            &network,
            "conversation",
            &target,
            &first.hash,
            &attempt,
            "owner checked",
        )?;
        store.scheduler_maintenance_idle(&network)?;
        assert!(
            store
                .claim_next(&network, "conversation", &target, "boot-c", "sdk")?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn scheduler_outbox_ack_retries_cannot_duplicate_or_retarget_admission() -> Result<()> {
        let owner = LocalIdentity::generate();
        let remote = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let target = remote.node_id()?;
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
                .enqueue_trigger(&network, "conversation", &event.hash, &owner.node_id()?)
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn scheduler_queued_cancellation_and_claim_preserve_each_serialized_winner() -> Result<()> {
        for claim_first in [false, true] {
            let owner = LocalIdentity::generate();
            let network = owner.fixture_network_id()?;
            let target = owner.node_id()?;
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
        let target = owner.node_id()?;
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
    fn legacy_scheduler_calls_are_imported_without_replaying_completed_or_unknown_work()
    -> Result<()> {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let target = owner.node_id()?;
        let mut store = memory(&owner, None)?;
        conversation(&mut store, &owner)?;
        for (message_id, state) in [("done", "completed"), ("uncertain", "running")] {
            let event = store.append_event(
                &owner,
                &network,
                "conversation",
                EventBody::MessagePosted {
                    message_id: message_id.into(),
                    role: "user".into(),
                    text: message_id.into(),
                    scheduler_node: Some(target.clone()),
                },
                vec![],
                2,
            )?;
            let legacy_key = format!(
                "scheduler:invocation:{}",
                digest(&serde_json::to_vec(&("conversation", &event.hash))?)
            );
            store.local_set(&network,&legacy_key,&json!({"state":state,"boot":"old-boot","trigger":event.hash,"message_id":format!("legacy-{message_id}")}))?;
            let receipt = store.enqueue_trigger(&network, "conversation", &event.hash, &target)?;
            assert_eq!(
                receipt.state,
                if state == "completed" {
                    "completed"
                } else {
                    "unknown"
                }
            );
        }
        store.recover_scheduler_jobs(&network, &target, "new-boot")?;
        assert!(
            store
                .claim_next(&network, "conversation", &target, "new-boot", "sdk")?
                .is_none()
        );
        assert!(store.scheduler_maintenance_idle(&network).is_err());
        assert_eq!(
            store
                .scheduler_jobs(&network, "conversation", &target)?
                .len(),
            2
        );
        Ok(())
    }

    #[test]
    fn legacy_scheduler_migration_survives_gateway_revocation_or_role_removal() -> Result<()> {
        for revoked in [true, false] {
            let current = LocalIdentity::generate();
            let retired = LocalIdentity::generate();
            let network = current.fixture_network_id()?;
            let target = current.node_id()?;
            let mut store = memory(&current, Some(&retired))?;
            conversation(&mut store, &current)?;
            let mut legacy = Vec::new();
            for state in ["completed", "failed", "running", "unknown"] {
                let event = store.append_event(
                    &retired,
                    &network,
                    "conversation",
                    EventBody::MessagePosted {
                        message_id: state.into(),
                        role: "user".into(),
                        text: state.into(),
                        scheduler_node: Some(target.clone()),
                    },
                    vec![],
                    2,
                )?;
                let key = format!(
                    "scheduler:invocation:{}",
                    digest(&serde_json::to_vec(&("conversation", &event.hash))?)
                );
                store.local_set(&network, &key, &json!({"state":state,"boot":"old-boot","trigger":event.hash,"message_id":format!("legacy-{state}")}))?;
                legacy.push((event.hash, state));
            }
            let mut retired_certificate = retired.certificate(vec![NodeRole::Replica])?;
            retired_certificate.revoked = revoked;
            store.install_trust_bundle(
                &network,
                &current.sign_network_fixture(
                    2,
                    vec![current.certificate(roles())?, retired_certificate],
                )?,
            )?;
            assert!(
                store
                    .authorize_node(&network, &retired.node_id()?, Some(NodeRole::Gateway))
                    .is_err()
            );
            assert!(
                store
                    .enqueue_trigger(&network, "conversation", &legacy[0].0, &target)
                    .is_err()
            );
            let new_message =
                admitted_message(&mut store, &current, "new-authorized-message", &target)?;
            store.recover_scheduler_jobs(&network, &target, "new-boot")?;
            for (trigger, original) in legacy {
                let job = store
                    .scheduler_job(&network, "conversation", &trigger, &target)?
                    .unwrap();
                assert_eq!(
                    job.state.name(),
                    if matches!(original, "running" | "unknown") {
                        "unknown"
                    } else {
                        original
                    }
                );
                if matches!(job.state, JobState::Unknown { .. }) {
                    store.fence_manager_unknown(
                        &network,
                        "conversation",
                        &target,
                        &trigger,
                        &job.state.attempt().unwrap().id,
                    )?;
                }
            }
            let claimed = store
                .claim_next(&network, "conversation", &target, "new-boot", "sdk")?
                .unwrap();
            assert_eq!(claimed.job.trigger, new_message.hash);
            assert!(claimed.job.state.attempt().unwrap().read_only);
            assert_eq!(
                store
                    .scheduler_jobs(&network, "conversation", &target)?
                    .iter()
                    .filter(|job| matches!(job.state, JobState::Queued))
                    .count(),
                0
            );
        }
        Ok(())
    }

    fn request(
        store: &mut Store,
        owner: &LocalIdentity,
        id: &str,
        heads: Vec<String>,
    ) -> Result<OperationRequest> {
        let mut request = OperationRequest {
            operation_id: id.into(),
            network_id: owner.fixture_network_id()?,
            conversation_id: "conversation".into(),
            task_id: None,
            target_node: owner.node_id()?,
            session_id: Some("session".into()),
            expected_revision: 0,
            basis_heads: heads,
            action: json!({"type":"open","executor":"codex","cwd":"/tmp"}),
            resources: vec![],
            binding: None,
            manager_authority: None,
        };
        store.bind_test_operation(owner, &mut request)?;
        Ok(request)
    }
    fn session(owner: &LocalIdentity, id: &str) -> Result<ExecutionSession> {
        Ok(ExecutionSession {
            network_id: owner.fixture_network_id()?,
            target_node: owner.node_id()?,
            session_id: id.into(),
            revision: 0,
            state: "idle".into(),
            executor: "codex".into(),
            cwd: "/tmp".into(),
            native_session_id: None,
        })
    }
    #[test]
    fn net1_duplicate_import_projection_write_baseline() -> Result<()> {
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
        assert_eq!(writes, 2);
        assert_eq!(store.conversations(&network)?.len(), 1);
        assert!(store.operations(&network)?.is_empty());
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
        assert!(replica.operations(&network)?.is_empty());
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
        assert!(store.authorize_node(&network, &b.node_id()?, None).is_err());
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
    fn queued_operation_cannot_start_after_known_executor_revocation() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let mut store = memory(&a, None)?;
        let event = conversation(&mut store, &a)?;
        let request = request(&mut store, &a, "queued", vec![event.hash])?;
        store.accept_operation(&network, &request)?;
        let mut certificate = a.certificate(roles())?;
        certificate.revoked = true;
        store.install_trust_bundle(&network, &a.sign_network_fixture(2, vec![certificate])?)?;
        assert!(store.claim_operation(&network, "queued").is_err());
        assert_eq!(
            store.operation(&network, "queued")?.unwrap().state,
            OperationState::Accepted
        );
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
    fn task_binding_cas_rejects_competing_targets_and_recovers_unsigned_commit() -> Result<()> {
        let owner = LocalIdentity::generate();
        let executor = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let dir =
            std::env::temp_dir().join(format!("vibeface-binding-cas-{}", uuid::Uuid::new_v4()));
        let db = dir.join("node.sqlite3");
        let mut store = Store::open(&db)?;
        store.install_trust_bundle(
            &network,
            &owner.sign_network_fixture(
                1,
                vec![owner.certificate(roles())?, executor.certificate(roles())?],
            )?,
        )?;
        conversation(&mut store, &owner)?;
        let creation = store.append_event(
            &owner,
            &network,
            "conversation",
            EventBody::TaskCreated {
                task_id: "task".into(),
                title: "task".into(),
                goal: "task".into(),
                acceptance: "task".into(),
            },
            vec![],
            2,
        )?;
        let first = crate::binding::TaskBinding {
            network_id: network.clone(),
            conversation_id: "conversation".into(),
            task_ref: creation.hash,
            task_id: "task".into(),
            authority_node: owner.node_id()?,
            generation: 1,
            target_node: owner.node_id()?,
            session_id: "first".into(),
            open_spec: json!({"type":"open","executor":"codex","cwd":"/tmp"}),
            previous_binding_hash: None,
            previous_seal: None,
        };
        let mut second = first.clone();
        second.target_node = executor.node_id()?;
        second.session_id = "second".into();
        drop(store);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut threads = vec![];
        for proposal in [first, second] {
            let mut store = Store::open(&db)?;
            let owner = owner.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                store.bind_task(&owner, &proposal)
            }));
        }
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let proof = results.into_iter().find_map(Result::ok).unwrap();
        let mut store = Store::open(&db)?;
        let hash = crate::binding::binding_hash(&proof.binding)?;
        // Simulate commit succeeding while the process exits before its signature is persisted.
        store.local_set(
            &network,
            &format!("task-binding-record:{hash}"),
            &json!({"binding":proof.binding,"signature":null}),
        )?;
        assert!(
            store
                .task_binding(&network, &proof.binding.task_ref)?
                .is_none()
        );
        assert_eq!(store.bind_task(&owner, &proof.binding)?, proof);
        drop(store);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn unknown_binding_cannot_move_and_seal_fences_delayed_commands() -> Result<()> {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let mut store = memory(&owner, None)?;
        let root = conversation(&mut store, &owner)?;
        let request = request(&mut store, &owner, "work", vec![root.hash])?;
        let proof = request.binding.as_ref().unwrap();
        let mut unbound = request.clone();
        unbound.binding = None;
        assert!(store.accept_operation(&network, &unbound).is_err());
        let mut wrong = request.clone();
        wrong.target_node = "0".repeat(64);
        assert!(store.accept_operation(&network, &wrong).is_err());
        store.accept_operation(&network, &request)?;
        store.claim_operation(&network, "work")?;
        store.finish_operation(
            &network,
            "work",
            OperationState::Unknown,
            json!({"reason":"lost connection"}),
        )?;
        assert!(store.seal_binding(&owner, proof).is_err());
        let mut replacement = proof.binding.clone();
        replacement.generation = 2;
        replacement.session_id = "replacement".into();
        replacement.previous_binding_hash = Some(crate::binding::binding_hash(&proof.binding)?);
        assert!(store.bind_task(&owner, &replacement).is_err());
        store.finish_operation(
            &network,
            "work",
            OperationState::Failed,
            json!({"verified":"external execution stopped"}),
        )?;
        store.local_set(
            &network,
            "completion:work",
            &json!({"state":"failed","result":"done"}),
        )?;
        assert!(store.seal_binding(&owner, proof).is_err());
        store.append_event(
            &owner,
            &network,
            "conversation",
            EventBody::ExecutionObserved {
                operation_id: "work".into(),
                task_id: request.task_id.clone(),
                target_node: request.target_node.clone(),
                session_id: request.session_id.clone(),
                status: "failed".into(),
                detail: json!({"verified":"external execution stopped"}),
            },
            vec![],
            3,
        )?;
        let seal = store.seal_binding(&owner, proof)?;
        assert_eq!(store.seal_binding(&owner, proof)?, seal);
        let mut delayed = request.clone();
        delayed.operation_id = "delayed".into();
        assert!(store.accept_operation(&network, &delayed).is_err());
        replacement.previous_seal = Some(seal);
        let next = store.bind_task(&owner, &replacement)?;
        assert_eq!(next.binding.generation, 2);
        assert!(store.accept_operation(&network, &delayed).is_err());
        store.install_binding(proof)?;
        assert_eq!(
            store
                .task_binding(&network, &proof.binding.task_ref)?
                .unwrap(),
            next
        );
        Ok(())
    }

    #[test]
    fn two_scheduler_requests_at_one_revision_admit_only_one_side_effect() -> Result<()> {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let dir =
            std::env::temp_dir().join(format!("vibeface-session-cas-{}", uuid::Uuid::new_v4()));
        let db = dir.join("node.sqlite3");
        let mut store = Store::open(&db)?;
        store.install_trust_bundle(
            &network,
            &owner.sign_network_fixture(1, vec![owner.certificate(roles())?])?,
        )?;
        let root = conversation(&mut store, &owner)?;
        let request = request(&mut store, &owner, "first", vec![root.hash])?;
        store.upsert_session(&network, &session(&owner, "session")?)?;
        drop(store);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut threads = vec![];
        for id in ["scheduler-a", "scheduler-c"] {
            let mut store = Store::open(&db)?;
            let mut request = request.clone();
            request.operation_id = id.into();
            let barrier = barrier.clone();
            let network = network.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                store.accept_operation(&network, &request)
            }));
        }
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let store = Store::open(&db)?;
        assert_eq!(store.operations(&network)?.len(), 1);
        drop(store);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn operation_cas_reserves_resources_and_reconciles_unknown_without_replay() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let mut store = memory(&a, None)?;
        let root = conversation(&mut store, &a)?;
        store.upsert_session(&network, &session(&a, "session")?)?;
        let mut first = request(&mut store, &a, "operation", vec![root.hash])?;
        first.resources = vec!["/tmp/vibeface-lock/repo".into()];
        assert_eq!(
            store.accept_operation(&network, &first)?.state,
            OperationState::Accepted
        );
        assert_eq!(
            store.accept_operation(&network, &first)?.state,
            OperationState::Accepted
        );
        let mut changed = first.clone();
        changed.action = json!({"type":"send"});
        assert!(store.accept_operation(&network, &changed).is_err());
        let mut overlap = first.clone();
        overlap.operation_id = "other".into();
        overlap.session_id = Some("second".into());
        overlap.resources = vec!["/tmp/vibeface-lock/repo/child".into()];
        overlap.task_id = None;
        overlap.binding = None;
        store.bind_test_operation(&a, &mut overlap)?;
        assert!(store.accept_operation(&network, &overlap).is_err());
        let mut sibling = overlap.clone();
        sibling.resources = vec!["/tmp/vibeface-lock/repo-two".into()];
        store.accept_operation(&network, &sibling)?;
        store.claim_operation(&network, "operation")?;
        assert!(store.claim_operation(&network, "operation").is_err());
        assert_eq!(store.recover_running()?, 1);
        assert_eq!(
            store.operation(&network, "operation")?.unwrap().state,
            OperationState::Unknown
        );
        assert!(store.claim_operation(&network, "operation").is_err());
        assert!(store.accept_operation(&network, &overlap).is_err());
        store.finish_operation(
            &network,
            "operation",
            OperationState::Succeeded,
            json!({"actual":"reconciled"}),
        )?;
        assert_eq!(
            store
                .session(&network, &a.node_id()?, "session")?
                .unwrap()
                .revision,
            1
        );
        let mut stale = first.clone();
        stale.operation_id = "stale".into();
        assert!(store.accept_operation(&network, &stale).is_err());
        stale.expected_revision = 1;
        store.accept_operation(&network, &stale)?;
        assert!(
            store
                .finish_operation(&network, "operation", OperationState::Failed, Value::Null)
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn running_send_allows_deduped_controls_but_blocks_other_sends_and_controls() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let mut store = memory(&a, None)?;
        let root = conversation(&mut store, &a)?;
        store.upsert_session(&network, &session(&a, "session")?)?;
        let mut send = request(&mut store, &a, "send", vec![root.hash])?;
        send.action = json!({"type":"send","text":"work"});
        send.resources = vec!["/tmp/vibeface-active-turn".into()];
        store.accept_operation(&network, &send)?;
        store.claim_operation(&network, "send")?;
        let mut another = send.clone();
        another.operation_id = "send-again".into();
        assert!(store.accept_operation(&network, &another).is_err());
        assert!(store.accept_control_operation(&network, &another).is_err());
        let mut control = send.clone();
        control.operation_id = "interrupt".into();
        control.action = json!({"type":"interrupt"});
        control.resources.clear();
        assert_eq!(
            store.accept_control_operation(&network, &control)?.state,
            OperationState::Accepted
        );
        store.claim_operation(&network, "interrupt")?;
        assert_eq!(
            store.accept_control_operation(&network, &control)?.state,
            OperationState::Running
        );
        let mut second = control.clone();
        second.operation_id = "steer".into();
        second.action = json!({"type":"steer","text":"more"});
        assert!(store.accept_control_operation(&network, &second).is_err());
        store.finish_operation(
            &network,
            "interrupt",
            OperationState::Succeeded,
            Value::Null,
        )?;
        assert!(store.accept_operation(&network, &another).is_err());
        second.expected_revision = 1;
        store.accept_control_operation(&network, &second)?;
        assert_eq!(
            store.operation(&network, "send")?.unwrap().state,
            OperationState::Running
        );
        let mut newer = store.session(&network, &a.node_id()?, "session")?.unwrap();
        newer.revision = 2;
        store.upsert_session(&network, &newer)?;
        assert!(store.claim_operation(&network, "steer").is_err());
        Ok(())
    }
    #[test]
    fn unknown_send_blocks_controls_until_reconciliation() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let mut store = memory(&a, None)?;
        let root = conversation(&mut store, &a)?;
        store.upsert_session(&network, &session(&a, "session")?)?;
        let mut send = request(&mut store, &a, "send", vec![root.hash])?;
        send.action = json!({"type":"send","text":"work"});
        store.accept_operation(&network, &send)?;
        store.claim_operation(&network, "send")?;
        assert_eq!(store.recover_running()?, 1);
        assert_eq!(
            store.operation(&network, "send")?.unwrap().state,
            OperationState::Unknown
        );
        for (operation_id, action) in [
            ("interrupt", json!({"type":"interrupt"})),
            ("steer", json!({"type":"steer","text":"more"})),
            (
                "respond",
                json!({"type":"respond_request","request_id":"request","response":"no"}),
            ),
        ] {
            let mut control = send.clone();
            control.operation_id = operation_id.into();
            control.action = action;
            control.resources.clear();
            assert!(store.accept_control_operation(&network, &control).is_err());
        }
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
            &retired.node_id()?,
        )?)?;
        let event = retired.sign_event(Event {
            version: WIRE_VERSION,
            network_id: root.network_id()?,
            scope_id: "archive".into(),
            author: retired.node_id()?,
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
            .filter(|member| member.subject_id != retired.node_id().unwrap())
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
                .authorize_node(&network, &retired.node_id()?, None)
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
            author: admin.node_id()?,
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
                .authorize_node(&network, &admin.node_id()?, None)
                .is_err()
        );
        assert!(
            store
                .authorize_node(&network, &node.node_id()?, Some(NodeRole::Replica))
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn all_replicated_and_local_ids_are_network_scoped() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let mut store = memory(&a, None)?;
        let na = a.fixture_network_id()?;
        let nb = b.fixture_network_id()?;
        store.install_trust_bundle(
            &nb,
            &b.sign_network_fixture(1, vec![b.certificate(roles())?])?,
        )?;
        let ea = conversation(&mut store, &a)?;
        let eb = conversation(&mut store, &b)?;
        assert!(store.ingest_event(&nb, &ea).is_err());
        assert!(store.event(&na, &eb.hash)?.is_none());
        assert_eq!(store.conversations(&na)?.len(), 1);
        assert_eq!(store.conversations(&nb)?.len(), 1);
        let ra = request(&mut store, &a, "same-id", vec![ea.hash])?;
        let rb = request(&mut store, &b, "same-id", vec![eb.hash])?;
        store.accept_operation(&na, &ra)?;
        store.accept_operation(&nb, &rb)?;
        assert_eq!(
            store
                .operation(&na, "same-id")?
                .unwrap()
                .request
                .target_node,
            a.node_id()?
        );
        assert_eq!(
            store
                .operation(&nb, "same-id")?
                .unwrap()
                .request
                .target_node,
            b.node_id()?
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
    fn same_device_resources_are_reserved_across_networks_without_sharing_records() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let na = a.fixture_network_id()?;
        let nb = b.fixture_network_id()?;
        let mut store = memory(&a, None)?;
        store.install_trust_bundle(
            &nb,
            &b.sign_network_fixture(1, vec![a.certificate(roles())?, b.certificate(roles())?])?,
        )?;
        let ea = conversation(&mut store, &a)?;
        let eb = conversation(&mut store, &b)?;
        let mut ra = request(&mut store, &a, "same-op", vec![ea.hash])?;
        ra.resources = vec!["/tmp/vibeface-shared-device".into()];
        let mut rb = request(&mut store, &b, "same-op", vec![eb.hash])?;
        rb.target_node = a.node_id()?;
        rb.task_id = Some("other-device-task".into());
        rb.binding = None;
        store.bind_test_operation(&b, &mut rb)?;
        rb.resources = ra.resources.clone();
        store.accept_operation(&na, &ra)?;
        assert!(store.accept_operation(&nb, &rb).is_err());
        assert!(store.operation(&nb, "same-op")?.is_none());
        rb.resources = vec!["/tmp/vibeface-separate-device".into()];
        store.accept_operation(&nb, &rb)?;
        Ok(())
    }
    #[test]
    fn notifications_budget_and_continuation_claims_are_durable_deduped_state() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let mut store = memory(&a, None)?;
        conversation(&mut store, &a)?;
        let result = store.append_event(
            &a,
            &network,
            "conversation",
            EventBody::ExecutionObserved {
                operation_id: "operation".into(),
                task_id: None,
                target_node: a.node_id()?,
                session_id: Some("session".into()),
                status: "completed".into(),
                detail: json!({"turn":"finished"}),
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
        store.claim_continuation(&network, &result.hash, "v1", &a.node_id()?)?;
        assert!(
            store
                .claim_continuation(&network, &result.hash, "v1", &a.node_id()?)
                .is_err()
        );
        store.recover_running()?;
        assert_eq!(
            store
                .continuation(&network, &result.hash, "v1")?
                .unwrap()
                .state,
            "unknown"
        );
        assert!(
            store
                .claim_continuation(&network, &result.hash, "v1", &a.node_id()?)
                .is_err()
        );
        store.finish_continuation(
            &network,
            &result.hash,
            "v1",
            &a.node_id()?,
            "completed",
            json!({"next":"ask_user"}),
        )?;
        store.rebuild_projections(&network)?;
        assert_eq!(store.pending_notifications(&network, "laptop")?.len(), 1);
        assert!(store.operations(&network)?.is_empty());
        Ok(())
    }
    #[test]
    fn replicated_notification_and_ack_rebuild_without_model_or_operation() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        let mut source = memory(&a, None)?;
        let mut target = memory(&a, None)?;
        conversation(&mut source, &a)?;
        source.append_event(
            &a,
            &network,
            "conversation",
            EventBody::NotificationRaised {
                notification_id: "sync-notice".into(),
                kind: "approval".into(),
                text: "human response needed".into(),
            },
            vec![],
            2,
        )?;
        target.import_snapshot(&network, &source.export_snapshot(&network, "conversation")?)?;
        assert_eq!(target.pending_notifications(&network, "phone")?.len(), 1);
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
        assert_eq!(target.pending_notifications(&network, "laptop")?.len(), 1);
        target.rebuild_projections(&network)?;
        assert!(target.pending_notifications(&network, "phone")?.is_empty());
        assert!(target.operations(&network)?.is_empty());
        Ok(())
    }
    #[test]
    fn incompatible_membership_is_rejected_before_recovery_mutates_operations() -> Result<()> {
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
        let root = conversation(&mut store, &owner)?;
        let request = request(&mut store, &owner, "running", vec![root.hash])?;
        store.accept_operation(&network, &request)?;
        store.claim_operation(&network, "running")?;
        drop(store);
        let connection = Connection::open(&db)?;
        connection.execute("UPDATE networks SET bundle='{}'", [])?;
        drop(connection);
        assert!(Store::open(&db).is_err());
        let connection = Connection::open(&db)?;
        let state: String = connection.query_row(
            "SELECT state FROM operations WHERE operation_id='running'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(state, "\"running\"");
        let conversations: u64 =
            connection.query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))?;
        assert_eq!(conversations, 1);
        drop(connection);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn reopening_keeps_unknown_effects_reserved_and_rejects_future_schema() -> Result<()> {
        let root = std::env::temp_dir().join(format!("vibeface-core-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root)?;
        let db = root.join("node.sqlite3");
        let a = LocalIdentity::generate();
        let network = a.fixture_network_id()?;
        {
            let mut store = Store::open(&db)?;
            store.install_trust_bundle(
                &network,
                &a.sign_network_fixture(1, vec![a.certificate(roles())?])?,
            )?;
            let event = conversation(&mut store, &a)?;
            let request = request(&mut store, &a, "durable", vec![event.hash])?;
            store.accept_operation(&network, &request)?;
            store.claim_operation(&network, "durable")?;
        }
        {
            let store = Store::open(&db)?;
            assert_eq!(
                store.operation(&network, "durable")?.unwrap().state,
                OperationState::Unknown
            );
            assert_eq!(store.conversations(&network)?.len(), 1);
        }
        let connection = Connection::open(&db)?;
        connection.execute_batch("PRAGMA user_version=4;")?;
        drop(connection);
        assert!(Store::open(&db).is_err());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn ordinary_operations_keep_session_ownership_without_creating_tasks() -> Result<()> {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let mut store = memory(&owner, None)?;
        let creation = conversation(&mut store, &owner)?;
        let open = OperationRequest {
            operation_id: "ordinary-open".into(),
            network_id: network.clone(),
            conversation_id: "conversation".into(),
            task_id: None,
            target_node: owner.node_id()?,
            session_id: Some("ordinary-session".into()),
            expected_revision: 0,
            basis_heads: vec![creation.hash],
            action: json!({"type":"open","executor":"codex","cwd":"/tmp"}),
            resources: vec![],
            binding: None,
            manager_authority: None,
        };
        store.accept_operation(&network, &open)?;
        store.upsert_session(&network, &session(&owner, "ordinary-session")?)?;
        store.claim_operation(&network, &open.operation_id)?;
        store.finish_operation(
            &network,
            &open.operation_id,
            OperationState::Succeeded,
            json!({"opened":true}),
        )?;
        assert!(store.tasks(&network, "conversation")?.is_empty());
        assert!(
            !store
                .context(&network, "conversation", &[])?
                .events
                .iter()
                .any(|event| matches!(event.body, EventBody::TaskCreated { .. }))
        );
        let mut send = open.clone();
        send.operation_id = "ordinary-send".into();
        send.expected_revision = 1;
        send.action = json!({"type":"send","text":"continue"});
        store.accept_operation(&network, &send)?;
        let mut foreign = send.clone();
        foreign.operation_id = "foreign".into();
        foreign.conversation_id = "other".into();
        assert!(
            store
                .accept_operation(&network, &foreign)
                .unwrap_err()
                .to_string()
                .contains("another conversation")
        );
        let mut bound = send.clone();
        bound.operation_id = "legacy-task".into();
        store.bind_test_operation(&owner, &mut bound)?;
        assert!(
            store
                .claim_operation(&network, &send.operation_id)
                .unwrap_err()
                .to_string()
                .contains("task-bound session")
        );
        Ok(())
    }

    #[test]
    fn task_bindings_cannot_capture_another_conversations_ordinary_session() -> Result<()> {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let mut store = memory(&owner, None)?;
        let created = conversation(&mut store, &owner)?;
        let open = OperationRequest {
            operation_id: "ordinary".into(),
            network_id: network.clone(),
            conversation_id: "conversation".into(),
            task_id: None,
            target_node: owner.node_id()?,
            session_id: Some("private-native".into()),
            expected_revision: 0,
            basis_heads: vec![created.hash],
            action: json!({"type":"open","executor":"codex","cwd":"/tmp"}),
            resources: vec![],
            binding: None,
            manager_authority: None,
        };
        store.accept_operation(&network, &open)?;
        store.upsert_session(&network, &session(&owner, "private-native")?)?;
        store.claim_operation(&network, "ordinary")?;
        store.finish_operation(
            &network,
            "ordinary",
            OperationState::Succeeded,
            json!({"opened":true}),
        )?;
        store.append_event(
            &owner,
            &network,
            "other-chat",
            EventBody::ConversationCreated {
                title: "Other Bot".into(),
            },
            vec![],
            2,
        )?;
        let task = store.append_event(
            &owner,
            &network,
            "other-chat",
            EventBody::TaskCreated {
                task_id: "task".into(),
                title: "Task".into(),
                goal: "Goal".into(),
                acceptance: "Check".into(),
            },
            vec![],
            3,
        )?;
        let binding = crate::binding::TaskBinding {
            network_id: network.clone(),
            conversation_id: "other-chat".into(),
            task_ref: task.hash.clone(),
            task_id: "task".into(),
            authority_node: owner.node_id()?,
            generation: 1,
            target_node: owner.node_id()?,
            session_id: "private-native".into(),
            open_spec: open.action.clone(),
            previous_binding_hash: None,
            previous_seal: None,
        };
        assert!(
            store
                .bind_task(&owner, &binding)
                .unwrap_err()
                .to_string()
                .contains("another conversation")
        );
        let proof = crate::binding::sign_binding(&owner, &binding)?;
        assert!(
            store
                .install_binding(&proof)
                .unwrap_err()
                .to_string()
                .contains("another conversation")
        );
        assert!(store.task_binding(&network, &task.hash)?.is_none());
        let mut attempted = open.clone();
        attempted.operation_id = "capture".into();
        attempted.conversation_id = "other-chat".into();
        attempted.task_id = Some("task".into());
        attempted.binding = Some(proof);
        attempted.expected_revision = 1;
        attempted.basis_heads = store.heads(&network, "other-chat")?;
        attempted.action = json!({"type":"send","text":"private Bot B request"});
        assert!(
            store
                .accept_operation(&network, &attempted)
                .unwrap_err()
                .to_string()
                .contains("another conversation")
        );
        store.append_event(
            &owner,
            &network,
            "other-chat",
            EventBody::DispatchRecorded {
                operation_id: attempted.operation_id.clone(),
                task_id: attempted.task_id.clone(),
                target_node: attempted.target_node.clone(),
                session_id: attempted.session_id.clone(),
                action: json!(attempted),
            },
            vec![],
            4,
        )?;
        let mut legitimate = open.clone();
        legitimate.operation_id = "legitimate-after-rejection".into();
        legitimate.expected_revision = 1;
        legitimate.action = json!({"type":"send","text":"continue original Bot A"});
        assert_eq!(
            store.accept_operation(&network, &legitimate)?.state,
            OperationState::Accepted
        );
        store.connection.execute(r#"INSERT INTO operations(network_id,operation_id,target_node,session_id,request,state,control,result) VALUES(?,?,?,?,?,'"accepted"',0,'null')"#,params![network,attempted.operation_id,attempted.target_node,attempted.session_id,serde_json::to_string(&attempted)?])?;
        assert!(
            store
                .claim_operation(&network, "capture")
                .unwrap_err()
                .to_string()
                .contains("another conversation")
        );
        assert_eq!(
            store
                .session(&network, &open.target_node, "private-native")?
                .unwrap()
                .revision,
            1
        );
        Ok(())
    }

    #[test]
    fn local_remote_execution_intents_block_foreign_bindings_before_ack() -> Result<()> {
        for (prepared_only, install_first) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let gateway = LocalIdentity::generate();
            let executor = LocalIdentity::generate();
            let foreign_gateway = LocalIdentity::generate();
            let network = gateway.fixture_network_id()?;
            let target = executor.node_id()?;
            let mut entry = Store::open(":memory:")?;
            entry.install_trust_bundle(
                &network,
                &gateway.sign_network_fixture(
                    1,
                    vec![
                        gateway.certificate(roles())?,
                        executor.certificate(roles())?,
                        foreign_gateway.certificate(roles())?,
                    ],
                )?,
            )?;
            entry.save_local_identity(&network, &gateway)?;
            let creation = conversation(&mut entry, &gateway)?;
            let original = OperationRequest {
                operation_id: "pending-open".into(),
                network_id: network.clone(),
                conversation_id: "conversation".into(),
                task_id: None,
                target_node: target.clone(),
                session_id: Some("pending-native".into()),
                expected_revision: 0,
                basis_heads: vec![creation.hash],
                action: json!({"type":"open","executor":"codex","cwd":"/tmp"}),
                resources: vec![],
                binding: None,
                manager_authority: None,
            };
            if prepared_only {
                entry.local_set(
                    &network,
                    "agent-calls:v1:attempt-a",
                    &json!({"call-a":{
                        "conversation_id":"conversation",
                        "name":"delegate",
                        "intent":{"text":"Bot A work"},
                        "requests":[original.clone()]
                    }}),
                )?;
            } else {
                entry.append_event(
                    &gateway,
                    &network,
                    "conversation",
                    EventBody::DispatchRecorded {
                        operation_id: original.operation_id.clone(),
                        task_id: None,
                        target_node: target.clone(),
                        session_id: original.session_id.clone(),
                        action: json!(original),
                    },
                    vec![],
                    2,
                )?;
            }
            assert!(entry.operations(&network)?.is_empty());
            assert!(entry.sessions(&network)?.is_empty());
            entry.append_event(
                &gateway,
                &network,
                "other-chat",
                EventBody::ConversationCreated {
                    title: "Bot B".into(),
                },
                vec![],
                3,
            )?;
            let task = entry.append_event(
                &gateway,
                &network,
                "other-chat",
                EventBody::TaskCreated {
                    task_id: "task-b".into(),
                    title: "B".into(),
                    goal: "B".into(),
                    acceptance: "Check".into(),
                },
                vec![],
                4,
            )?;
            let mut binding = crate::binding::TaskBinding {
                network_id: network.clone(),
                conversation_id: "other-chat".into(),
                task_ref: task.hash.clone(),
                task_id: "task-b".into(),
                authority_node: gateway.node_id()?,
                generation: 1,
                target_node: target,
                session_id: "pending-native".into(),
                open_spec: original.action.clone(),
                previous_binding_hash: None,
                previous_seal: None,
            };
            assert!(
                entry
                    .bind_task(&gateway, &binding)
                    .unwrap_err()
                    .to_string()
                    .contains("another conversation")
            );
            let proof = crate::binding::sign_binding(&gateway, &binding)?;
            assert!(
                entry
                    .install_binding(&proof)
                    .unwrap_err()
                    .to_string()
                    .contains("another conversation")
            );
            assert!(entry.task_binding(&network, &task.hash)?.is_none());
            let mut rejected = original.clone();
            rejected.operation_id = "unadmitted-b".into();
            rejected.conversation_id = "other-chat".into();
            rejected.basis_heads = entry.heads(&network, "other-chat")?;
            let foreign = foreign_gateway.sign_event(Event {
                version: 1,
                network_id: network.clone(),
                scope_id: "other-chat".into(),
                author: foreign_gateway.node_id()?,
                sequence: 1,
                previous: None,
                parents: rejected.basis_heads.clone(),
                created_at_ms: 5,
                body: EventBody::DispatchRecorded {
                    operation_id: rejected.operation_id.clone(),
                    task_id: None,
                    target_node: rejected.target_node.clone(),
                    session_id: rejected.session_id.clone(),
                    action: json!(rejected),
                },
                blobs: vec![],
            })?;
            assert_eq!(entry.ingest_event(&network, &foreign)?.status, "ready");
            entry.append_event(
                &gateway,
                &network,
                "other-chat",
                EventBody::DispatchRecorded {
                    operation_id: "mismatched-b".into(),
                    task_id: None,
                    target_node: rejected.target_node.clone(),
                    session_id: rejected.session_id.clone(),
                    action: json!(rejected),
                },
                vec![],
                6,
            )?;
            let own_task = entry.append_event(
                &gateway,
                &network,
                "conversation",
                EventBody::TaskCreated {
                    task_id: "task-a".into(),
                    title: "A".into(),
                    goal: "A".into(),
                    acceptance: "Check".into(),
                },
                vec![],
                7,
            )?;
            binding.conversation_id = "conversation".into();
            binding.task_ref = own_task.hash;
            binding.task_id = "task-a".into();
            let own_proof = if install_first {
                let proof = crate::binding::sign_binding(&gateway, &binding)?;
                entry.install_binding(&proof)?;
                proof
            } else {
                entry.bind_task(&gateway, &binding)?
            };
            assert_eq!(
                entry.task_binding(&network, &binding.task_ref)?,
                Some(own_proof)
            );
        }
        Ok(())
    }

    #[test]
    fn remote_executor_acknowledgement_prevents_gateway_binding_pollution() -> Result<()> {
        let gateway = LocalIdentity::generate();
        let executor = LocalIdentity::generate();
        let foreign_gateway = LocalIdentity::generate();
        let network = gateway.fixture_network_id()?;
        let target = executor.node_id()?;
        let mut entry = Store::open(":memory:")?;
        entry.install_trust_bundle(
            &network,
            &gateway.sign_network_fixture(
                1,
                vec![
                    gateway.certificate(roles())?,
                    executor.certificate(roles())?,
                    foreign_gateway.certificate(roles())?,
                ],
            )?,
        )?;
        entry.save_local_identity(&network, &gateway)?;
        let mut execution = Store::open(":memory:")?;
        execution.install_trust_bundle(&network, &entry.trust_bundle(&network)?.unwrap())?;
        execution.save_local_identity(&network, &executor)?;
        let creation = conversation(&mut entry, &gateway)?;
        execution.ingest_event(&network, &creation)?;
        let mut original = OperationRequest {
            operation_id: "remote-open".into(),
            network_id: network.clone(),
            conversation_id: "conversation".into(),
            task_id: None,
            target_node: target.clone(),
            session_id: Some("remote-native".into()),
            expected_revision: 0,
            basis_heads: entry.heads(&network, "conversation")?,
            action: json!({"type":"open","executor":"codex","cwd":"/tmp"}),
            resources: vec![],
            binding: None,
            manager_authority: None,
        };
        for (operation_id, revision, action) in [
            (
                "remote-open",
                0,
                json!({"type":"open","executor":"codex","cwd":"/tmp"}),
            ),
            ("remote-send", 1, json!({"type":"send","text":"Bot A work"})),
        ] {
            original.operation_id = operation_id.into();
            original.expected_revision = revision;
            original.action = action;
            original.basis_heads = entry.heads(&network, "conversation")?;
            let dispatch = entry.append_event(
                &gateway,
                &network,
                "conversation",
                EventBody::DispatchRecorded {
                    operation_id: original.operation_id.clone(),
                    task_id: None,
                    target_node: target.clone(),
                    session_id: original.session_id.clone(),
                    action: json!(original),
                },
                vec![],
                3,
            )?;
            execution.ingest_event(&network, &dispatch)?;
            execution.accept_operation(&network, &original)?;
            if revision == 0 {
                execution.upsert_session(
                    &network,
                    &ExecutionSession {
                        network_id: network.clone(),
                        target_node: target.clone(),
                        session_id: "remote-native".into(),
                        revision: 0,
                        state: "idle".into(),
                        executor: "codex".into(),
                        cwd: "/tmp".into(),
                        native_session_id: Some("actual-native".into()),
                    },
                )?;
            }
            execution.claim_operation(&network, operation_id)?;
            execution.finish_operation(
                &network,
                operation_id,
                OperationState::Succeeded,
                json!({"done":true}),
            )?;
            let acknowledgement = execution.append_event(
                &executor,
                &network,
                "conversation",
                EventBody::ExecutionObserved {
                    operation_id: operation_id.into(),
                    task_id: None,
                    target_node: target.clone(),
                    session_id: original.session_id.clone(),
                    status: "succeeded".into(),
                    detail: json!({"done":true}),
                },
                vec![],
                4,
            )?;
            entry.ingest_event(&network, &acknowledgement)?;
        }
        assert!(entry.operations(&network)?.is_empty());
        assert!(entry.sessions(&network)?.is_empty());
        entry.append_event(
            &gateway,
            &network,
            "other-chat",
            EventBody::ConversationCreated {
                title: "Bot B".into(),
            },
            vec![],
            5,
        )?;
        let task = entry.append_event(
            &gateway,
            &network,
            "other-chat",
            EventBody::TaskCreated {
                task_id: "task-b".into(),
                title: "B".into(),
                goal: "B".into(),
                acceptance: "Check".into(),
            },
            vec![],
            6,
        )?;
        let binding = crate::binding::TaskBinding {
            network_id: network.clone(),
            conversation_id: "other-chat".into(),
            task_ref: task.hash.clone(),
            task_id: "task-b".into(),
            authority_node: gateway.node_id()?,
            generation: 1,
            target_node: target.clone(),
            session_id: "remote-native".into(),
            open_spec: json!({"type":"open","executor":"codex","cwd":"/tmp"}),
            previous_binding_hash: None,
            previous_seal: None,
        };
        assert!(
            entry
                .bind_task(&gateway, &binding)
                .unwrap_err()
                .to_string()
                .contains("another conversation")
        );
        let proof = crate::binding::sign_binding(&gateway, &binding)?;
        assert!(
            entry
                .install_binding(&proof)
                .unwrap_err()
                .to_string()
                .contains("another conversation")
        );
        assert!(entry.task_binding(&network, &task.hash)?.is_none());
        let mut rejected = original.clone();
        rejected.operation_id = "rejected-b".into();
        rejected.conversation_id = "other-chat".into();
        rejected.task_id = Some("task-b".into());
        rejected.binding = Some(proof.clone());
        rejected.expected_revision = 2;
        rejected.basis_heads = entry.heads(&network, "other-chat")?;
        entry.append_event(
            &gateway,
            &network,
            "other-chat",
            EventBody::DispatchRecorded {
                operation_id: rejected.operation_id.clone(),
                task_id: rejected.task_id.clone(),
                target_node: target.clone(),
                session_id: rejected.session_id.clone(),
                action: json!(rejected),
            },
            vec![],
            7,
        )?;
        for envelope in entry.export_snapshot(&network, "other-chat")?.events {
            execution.ingest_event(&network, &envelope)?;
        }
        assert!(execution.install_binding(&proof).is_err());
        assert!(execution.accept_operation(&network, &rejected).is_err());
        original.operation_id = "continue-a".into();
        original.expected_revision = 2;
        original.basis_heads = entry.heads(&network, "conversation")?;
        original.action = json!({"type":"send","text":"continue Bot A"});
        entry.validate_operation_binding(&original)?;
        let continuation = entry.append_event(
            &gateway,
            &network,
            "conversation",
            EventBody::DispatchRecorded {
                operation_id: original.operation_id.clone(),
                task_id: None,
                target_node: target,
                session_id: original.session_id.clone(),
                action: json!(original),
            },
            vec![],
            8,
        )?;
        execution.ingest_event(&network, &continuation)?;
        assert_eq!(
            execution.accept_operation(&network, &original)?.state,
            OperationState::Accepted
        );
        let foreign = foreign_gateway.sign_event(Event {
            version: 1,
            network_id: network.clone(),
            scope_id: "other-chat".into(),
            author: foreign_gateway.node_id()?,
            sequence: 1,
            previous: None,
            parents: entry.heads(&network, "other-chat")?,
            created_at_ms: 9,
            body: EventBody::DispatchRecorded {
                operation_id: rejected.operation_id.clone(),
                task_id: rejected.task_id.clone(),
                target_node: rejected.target_node.clone(),
                session_id: rejected.session_id.clone(),
                action: json!(rejected),
            },
            blobs: vec![],
        })?;
        entry.ingest_event(&network, &foreign)?;
        entry.local_set(
            &network,
            "agent-calls:v1:rejected-b",
            &json!({"call-b":{
                "conversation_id":"other-chat",
                "name":"delegate",
                "intent":{"text":"B"},
                "requests":[rejected]
            }}),
        )?;
        let own_task = entry.append_event(
            &gateway,
            &network,
            "conversation",
            EventBody::TaskCreated {
                task_id: "task-a".into(),
                title: "A".into(),
                goal: "A".into(),
                acceptance: "Check".into(),
            },
            vec![],
            10,
        )?;
        let mut own_binding = binding;
        own_binding.conversation_id = "conversation".into();
        own_binding.task_ref = own_task.hash;
        own_binding.task_id = "task-a".into();
        let own_proof = entry.bind_task(&gateway, &own_binding)?;
        entry.install_binding(&own_proof)?;
        assert_eq!(
            entry.task_binding(&network, &own_binding.task_ref)?,
            Some(own_proof)
        );
        Ok(())
    }

    #[test]
    fn sessionless_commands_share_reservations_without_creating_native_sessions() -> Result<()> {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let mut store = memory(&owner, None)?;
        let event = conversation(&mut store, &owner)?;
        let command = OperationRequest {
            operation_id: "command".into(),
            network_id: network.clone(),
            conversation_id: "conversation".into(),
            task_id: None,
            target_node: owner.node_id()?,
            session_id: None,
            expected_revision: 0,
            basis_heads: vec![event.hash],
            action: json!({"type":"command","command":"pwd","cwd":"/tmp","timeout_secs":10}),
            resources: vec!["/tmp".into()],
            binding: None,
            manager_authority: None,
        };
        store.accept_operation(&network, &command)?;
        assert_eq!(
            store
                .accept_operation(&network, &command)?
                .request
                .session_id,
            None
        );
        store.claim_operation(&network, &command.operation_id)?;
        assert!(store.sessions(&network)?.is_empty());
        assert!(store.tasks(&network, "conversation")?.is_empty());
        let mut competitor = command.clone();
        competitor.operation_id = "competitor".into();
        assert!(
            store
                .accept_operation(&network, &competitor)
                .unwrap_err()
                .to_string()
                .contains("resource overlaps")
        );
        store.recover_running()?;
        assert_eq!(
            store.operation(&network, "command")?.unwrap().state,
            OperationState::Unknown
        );
        assert!(store.claim_operation(&network, "command").is_err());
        assert!(store.accept_operation(&network, &competitor).is_err());
        store.finish_operation(
            &network,
            "command",
            OperationState::Succeeded,
            json!({"exit_code":0,"stdout":"/tmp\n","stderr":"","truncated":false}),
        )?;
        assert_eq!(
            store.accept_operation(&network, &competitor)?.state,
            OperationState::Accepted
        );
        assert!(store.sessions(&network)?.is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn resource_normalization_follows_symlinks_before_parent_components() -> Result<()> {
        let root = std::env::temp_dir().join(format!("vibeface-resource-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("real/child"))?;
        std::os::unix::fs::symlink(root.join("real/child"), root.join("link"))?;
        let resource = canonical_resource(root.join("link/..").to_str().unwrap())?;
        assert_eq!(
            resource,
            std::fs::canonicalize(root.join("real"))?.to_str().unwrap()
        );
        assert!(canonical_resource(root.join("absent/../other").to_str().unwrap()).is_err());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
