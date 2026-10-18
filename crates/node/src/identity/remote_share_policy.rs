use super::{
    client_policy_read::{bounded, check_heads_in, encode, finish_response, metadata, signer},
    journal::{AcceptedJournalCursor, AcceptedJournalSnapshot, PolicyState},
};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vibeface_secure::{
    authorization::{
        AUTH_VERSION, MAX_PROOF_BYTES, Nonce, ObjectHash, PublicKey, Signed,
        execution_handoff::ExecutionProofUsage,
        policy_proof::policy_entry_payload_usage,
        remote_share::{
            ConversationOpenEvidence, RemoteSharePolicyPage, RemoteSharePolicyPageRequest,
            RemoteSharePolicyReadRequest, RemoteSharePolicySnapshot, RemoteSharePolicyWitness,
            RemoteShareWork, VerifiedRemoteSharePolicyRead, verify_remote_share_policy_read,
        },
    },
    signature_verification::ScopedSignatureVerification,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteShareReadRecord {
    request: Signed<RemoteSharePolicyReadRequest>,
    snapshot: Signed<RemoteSharePolicySnapshot>,
    journal_snapshot: AcceptedJournalSnapshot,
    snapshot_salt: Nonce,
    position: ReadPosition,
    witness: Option<Signed<RemoteSharePolicyWitness>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadPosition {
    cursor: Option<ObjectHash>,
    journal_cursor: Option<AcceptedJournalCursor>,
    page_count: u64,
    entry_count: u64,
    previous_page_hash: ObjectHash,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestBinding {
    request_hash: ObjectHash,
    snapshot_id: Nonce,
}

fn context_key(id: &Nonce) -> String {
    format!("remote-share-policy-read:{id}")
}

fn page_key(id: &Nonce) -> String {
    format!("remote-share-policy-page:{id}")
}

fn request_key(request: &Signed<RemoteSharePolicyReadRequest>) -> String {
    format!(
        "remote-share-policy-request:{}:{}",
        request.payload.reader_public_key, request.payload.nonce
    )
}

fn snapshot_id(
    request: &ObjectHash,
    journal: &AcceptedJournalSnapshot,
    salt: &Nonce,
) -> Result<Nonce> {
    let hash = ObjectHash::body(&("remote-share-policy-snapshot-id-v2", request, journal, salt))?;
    Ok(Nonce::parse(
        hash.as_str()
            .strip_prefix("h2_")
            .context("invalid remote share snapshot hash")?,
    )?)
}

fn check_source_work(
    connection: &Connection,
    request: &Signed<RemoteSharePolicyReadRequest>,
) -> Result<()> {
    let intent = request.payload.intent.intent();
    let context = &request.payload.intent.invocation().payload.context;
    let task = match &intent.work {
        RemoteShareWork::TaskSession { task_creation, .. } => &task_creation.payload,
        RemoteShareWork::ConversationSession { source, .. }
        | RemoteShareWork::Command { source } => {
            let statement = &source.payload;
            let operation: crate::model::OperationRequest =
                serde_json::from_value(statement.request.clone())?;
            ensure!(
                statement.project == intent.project
                    && statement.source_node_public_key == intent.source_node_public_key
                    && operation.network_id == intent.project.space.as_str()
                    && context.conversation.as_deref() == Some(operation.conversation_id.as_str())
                    && context.task.is_none(),
                "remote share operation changed its Project or scope"
            );
            let resource: Option<String> = connection
                .query_row(
                    "SELECT resource_id FROM conversations WHERE space_id=? AND conversation_id=?",
                    params![intent.project.space.as_str(), operation.conversation_id],
                    |row| row.get(0),
                )
                .optional()?;
            ensure!(
                resource.as_deref() == Some(intent.project.resource.as_str()),
                "remote share operation differs from its retained Project conversation"
            );
            if let RemoteShareWork::ConversationSession {
                open: ConversationOpenEvidence::Accepted { open },
                ..
            } = &intent.work
            {
                let accepted = super::execution_handoff::accepted_conversation_open_in(
                    connection,
                    &intent.project,
                    &operation,
                )?;
                let session = accepted
                    .receipt
                    .payload
                    .session
                    .as_ref()
                    .context("remote share accepted Open has no created session")?;
                ensure!(
                    !matches!(
                        session.state.as_str(),
                        "opening" | "unknown" | "resolved" | "failed" | "closed"
                    ),
                    "remote share accepted Open did not create a usable session"
                );
                ensure!(
                    accepted == **open,
                    "remote share conversation changed its retained accepted Open"
                );
            }
            return Ok(());
        }
    };
    ensure!(
        task.project == intent.project
            && task.source_node_public_key == intent.source_node_public_key
            && context.conversation.as_deref() == Some(task.conversation_id.as_str())
            && context.task.as_ref() == Some(&task.task),
        "remote share selected Task changed its Project or scope"
    );
    let ready: Option<bool> = connection
        .query_row(
            "WITH selected AS (
                SELECT resource_id,scope_id,author,sequence,
                    json_extract(envelope,'$.hash') AS envelope_hash,
                    json_extract(envelope,'$.canonical_body') AS body
                FROM events WHERE space_id=?1 AND hash=?2 AND status='ready'
            ) SELECT 1 FROM selected
                WHERE resource_id=?3 AND scope_id=?4 AND author=?5 AND sequence=?6
                AND envelope_hash=?2
                AND json_extract(body,'$.space_id')=?1
                AND json_extract(body,'$.resource_id')=?3
                AND json_extract(body,'$.scope_id')=?4
                AND json_extract(body,'$.author')=?5
                AND json_extract(body,'$.sequence')=?6
                AND json_extract(body,'$.body.type')='task_created'
                AND json_extract(body,'$.body.task_id')=?7",
            params![
                task.project.space.as_str(),
                task.task.as_str(),
                task.project.resource.as_str(),
                task.conversation_id,
                task.author,
                task.sequence,
                task.task_id,
            ],
            |row| row.get(0),
        )
        .optional()?;
    ensure!(
        ready == Some(true),
        "remote share selected Task differs from its retained ready row"
    );
    if let Some(binding) = &intent.old_binding {
        let hash = crate::binding::binding_hash(&binding.binding)?;
        let stored: Option<(String, String)> = connection
            .query_row(
                "SELECT current.value,record.value FROM local_values AS current JOIN local_values AS record ON record.space_id=current.space_id AND record.key=? WHERE current.space_id=? AND current.key=? AND length(CAST(current.value AS BLOB))<=? AND length(CAST(record.value AS BLOB))<=?",
                params![
                    format!("task-binding-record:{hash}"),
                    intent.project.space.as_str(),
                    format!("task-binding-current:{}", task.task),
                    MAX_PROOF_BYTES,
                    MAX_PROOF_BYTES,
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (current, record) = stored.context("remote share old Task binding is not retained")?;
        ensure!(
            serde_json::from_str::<String>(&current)? == hash
                && serde_json::from_str::<crate::binding::SignedBinding>(&record)? == *binding,
            "remote share old Task binding is no longer current"
        );
    }
    Ok(())
}

fn verify_read(
    connection: &Connection,
    policy: &PolicyState,
    request: &Signed<RemoteSharePolicyReadRequest>,
    local: &PublicKey,
    principal: &str,
    now: u64,
) -> Result<VerifiedRemoteSharePolicyRead> {
    let verified = verify_remote_share_policy_read(
        policy.verified(),
        request,
        local,
        &PublicKey::parse(principal)?,
        now,
    )?;
    check_heads_in(connection, policy, verified.heads())?;
    check_source_work(connection, request)?;
    Ok(verified)
}

fn cursor_hash(
    snapshot: &Signed<RemoteSharePolicySnapshot>,
    position: &ReadPosition,
    previous: &ObjectHash,
) -> Result<ObjectHash> {
    Ok(ObjectHash::body(&(
        "remote-share-policy-cursor-v2",
        snapshot.hash()?,
        &position.journal_cursor,
        position.page_count,
        position.entry_count,
        previous,
    ))?)
}

fn verify_record(
    connection: &Connection,
    policy: &PolicyState,
    record: &RemoteShareReadRecord,
    local: &PublicKey,
    principal: &str,
    now: u64,
) -> Result<(
    VerifiedRemoteSharePolicyRead,
    Option<Signed<RemoteSharePolicyPage>>,
    ExecutionProofUsage,
)> {
    let verified = verify_read(connection, policy, &record.request, local, principal, now)?;
    let snapshot = &record.snapshot.payload;
    let intent_hash = record.request.payload.intent.hash()?;
    ensure!(
        record.snapshot.issuer == *local
            && snapshot.version == AUTH_VERSION
            && snapshot.intent_hash == intent_hash
            && snapshot.request_hash == record.request.hash()?
            && snapshot.reader_public_key == record.request.payload.reader_public_key
            && snapshot.saved_pins_hash == ObjectHash::body(&record.request.payload.saved_pins)?
            && snapshot.snapshot_id
                == snapshot_id(
                    &snapshot.request_hash,
                    &record.journal_snapshot,
                    &record.snapshot_salt,
                )?
            && snapshot.target_heads == verified.heads()
            && snapshot.source_node_public_key == *local
            && record.request.payload.issued_at_ms <= snapshot.issued_at_ms
            && snapshot.issued_at_ms <= now
            && now < snapshot.expires_at_ms
            && snapshot.expires_at_ms == record.request.payload.expires_at_ms,
        "remote share policy snapshot changed"
    );
    record.snapshot.verify_signature()?;
    let binding: RequestBinding = serde_json::from_str(
        &metadata(connection, &request_key(&record.request))?
            .context("remote share policy request binding is missing")?,
    )?;
    ensure!(
        binding.request_hash == snapshot.request_hash
            && binding.snapshot_id == snapshot.snapshot_id,
        "remote share policy request binding changed"
    );
    let mut usage = verified.proof_usage().clone();
    usage.record_signed(&record.request)?;
    usage.record_signed(&record.snapshot)?;
    let cached: Option<Signed<RemoteSharePolicyPage>> =
        metadata(connection, &page_key(&snapshot.snapshot_id))?
            .map(|value| serde_json::from_str(&value))
            .transpose()?;
    if let Some(cached) = &cached {
        let page = &cached.payload;
        ensure!(
            cached.issuer == *local
                && page.version == AUTH_VERSION
                && page.intent_hash == intent_hash
                && page.request_hash == snapshot.request_hash
                && page.snapshot_id == snapshot.snapshot_id
                && page.target_heads_hash == ObjectHash::body(&snapshot.target_heads)?
                && page.page_index.checked_add(1) == Some(record.position.page_count)
                && cached.hash()? == record.position.previous_page_hash
                && page.next_cursor == record.position.cursor
                && page.complete == record.witness.is_some(),
            "remote share policy cursor changed its signed page"
        );
        cached.verify_signature()?;
        usage.record_signed(cached)?;
        if let Some(entry) = &page.entry {
            usage.merge(&policy_entry_payload_usage(entry)?);
        }
        if !page.complete {
            ensure!(
                page.next_cursor.as_ref()
                    == Some(&cursor_hash(
                        &record.snapshot,
                        &record.position,
                        &page.previous_page_hash,
                    )?),
                "remote share policy cursor changed its private position"
            );
        }
    } else {
        ensure!(
            record.position.cursor.is_none()
                && record.position.journal_cursor.is_none()
                && record.position.page_count == 0
                && record.position.entry_count == 0
                && record.position.previous_page_hash == record.snapshot.hash()?
                && record.witness.is_none(),
            "remote share policy initial cursor changed"
        );
    }
    if let Some(witness) = &record.witness {
        let value = &witness.payload;
        ensure!(
            witness.issuer == *local
                && value.version == AUTH_VERSION
                && value.intent_hash == intent_hash
                && value.request_hash == snapshot.request_hash
                && value.snapshot_id == snapshot.snapshot_id
                && value.target_heads == snapshot.target_heads
                && value.snapshot_hash == record.snapshot.hash()?
                && value.final_page_hash == record.position.previous_page_hash
                && value.page_count == record.position.page_count
                && value.entry_count == record.position.entry_count
                && value.reader_public_key == snapshot.reader_public_key
                && value.source_node_public_key == *local
                && snapshot.issued_at_ms <= value.issued_at_ms
                && value.issued_at_ms <= now
                && value.expires_at_ms == snapshot.expires_at_ms,
            "remote share policy completion witness changed"
        );
        witness.verify_signature()?;
        usage.record_signed(witness)?;
    }
    Ok((verified, cached, usage))
}

impl Store {
    pub(crate) fn execution_share_policy_prepare(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
        let request: Signed<RemoteSharePolicyReadRequest> = serde_json::from_value(params.clone())?;
        let signer = signer(identity)?;
        let local = PublicKey::of(&signer);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let nonce_key = request_key(&request);
        let hash = request.hash()?;
        if let Some(binding) = metadata(&tx, &nonce_key)? {
            let binding: RequestBinding = serde_json::from_str(&binding)?;
            ensure!(
                binding.request_hash == hash,
                "remote share policy nonce was reused with another request"
            );
            let record: RemoteShareReadRecord = serde_json::from_str(
                &metadata(&tx, &context_key(&binding.snapshot_id))?
                    .context("remote share policy snapshot is missing")?,
            )?;
            ensure!(
                record.request == request
                    && record.snapshot.payload.snapshot_id == binding.snapshot_id,
                "remote share policy request binding changed"
            );
            let (_, _, usage) = verify_record(&tx, &self.policy, &record, &local, principal, now)?;
            return finish_response(json!({"snapshot":record.snapshot}), usage);
        }
        let verified = verify_read(&tx, &self.policy, &request, &local, principal, now)?;
        let journal_snapshot = self.policy.accepted_journal_snapshot(&tx)?;
        let snapshot_salt = Nonce::random()?;
        let id = snapshot_id(&hash, &journal_snapshot, &snapshot_salt)?;
        let snapshot = Signed::sign(
            RemoteSharePolicySnapshot {
                version: AUTH_VERSION,
                intent_hash: request.payload.intent.hash()?,
                request_hash: hash.clone(),
                snapshot_id: id.clone(),
                reader_public_key: request.payload.reader_public_key.clone(),
                saved_pins_hash: ObjectHash::body(&request.payload.saved_pins)?,
                target_heads: verified.heads().to_vec(),
                source_node_public_key: local,
                issued_at_ms: now,
                expires_at_ms: request.payload.expires_at_ms,
            },
            &signer,
        )?;
        let record = RemoteShareReadRecord {
            request: request.clone(),
            journal_snapshot,
            snapshot_salt,
            position: ReadPosition {
                cursor: None,
                journal_cursor: None,
                page_count: 0,
                entry_count: 0,
                previous_page_hash: snapshot.hash()?,
            },
            snapshot: snapshot.clone(),
            witness: None,
        };
        let mut usage = verified.proof_usage().clone();
        usage.record_signed(&request)?;
        usage.record_signed(&snapshot)?;
        let response = finish_response(json!({"snapshot":snapshot}), usage)?;
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![context_key(&id), encode(&record)?],
        )?;
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![
                nonce_key,
                encode(&RequestBinding {
                    request_hash: hash,
                    snapshot_id: id,
                })?,
            ],
        )?;
        tx.commit()?;
        Ok(response)
    }

    pub(crate) fn execution_share_policy_page(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
        let request: RemoteSharePolicyPageRequest = serde_json::from_value(params.clone())?;
        let signer = signer(identity)?;
        let local = PublicKey::of(&signer);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = context_key(&request.snapshot_id);
        let encoded = metadata(&tx, &key)?.context("remote share policy snapshot is missing")?;
        let mut record: RemoteShareReadRecord = serde_json::from_str(&encoded)?;
        ensure!(
            record.snapshot.payload.snapshot_id == request.snapshot_id
                && record.snapshot.payload.request_hash == request.request_hash,
            "remote share policy page request changed"
        );
        let (verified, cached, mut usage) =
            verify_record(&tx, &self.policy, &record, &local, principal, now)?;
        if let Some(cached) = cached {
            if cached.payload.request_cursor == request.cursor {
                return finish_response(json!({"page":cached}), usage);
            }
        }
        ensure!(
            record.witness.is_none() && request.cursor == record.position.cursor,
            "remote share policy cursor is stale or complete"
        );
        let scanned = self.policy.accepted_journal_page(
            &tx,
            &record.journal_snapshot,
            record.position.journal_cursor.as_ref(),
            &verified.domains(),
        )?;
        ensure!(
            scanned.scanned <= 32,
            "remote share policy scan budget exceeded"
        );
        let entry = scanned.entry.map(|value| value.entry);
        if let Some(entry) = &entry {
            usage.merge(&policy_entry_payload_usage(entry)?);
        }
        let previous_page_hash = record.position.previous_page_hash.clone();
        let page_index = record.position.page_count;
        record.position.page_count = page_index
            .checked_add(1)
            .context("remote share policy page count overflow")?;
        record.position.entry_count = record
            .position
            .entry_count
            .checked_add(u64::from(entry.is_some()))
            .context("remote share policy entry count overflow")?;
        record.position.journal_cursor = Some(scanned.next_cursor);
        let next_cursor = if scanned.complete {
            None
        } else {
            Some(cursor_hash(
                &record.snapshot,
                &record.position,
                &previous_page_hash,
            )?)
        };
        let page = Signed::sign(
            RemoteSharePolicyPage {
                version: AUTH_VERSION,
                intent_hash: record.request.payload.intent.hash()?,
                request_hash: request.request_hash,
                snapshot_id: request.snapshot_id,
                target_heads_hash: ObjectHash::body(&record.snapshot.payload.target_heads)?,
                page_index,
                request_cursor: request.cursor,
                previous_page_hash,
                entry,
                next_cursor: next_cursor.clone(),
                complete: scanned.complete,
            },
            &signer,
        )?;
        usage.record_signed(&page)?;
        record.position.cursor = next_cursor;
        record.position.previous_page_hash = page.hash()?;
        if scanned.complete {
            let witness = Signed::sign(
                RemoteSharePolicyWitness {
                    version: AUTH_VERSION,
                    intent_hash: record.request.payload.intent.hash()?,
                    request_hash: record.request.hash()?,
                    snapshot_id: record.snapshot.payload.snapshot_id.clone(),
                    target_heads: record.snapshot.payload.target_heads.clone(),
                    final_page_hash: page.hash()?,
                    page_count: record.position.page_count,
                    entry_count: record.position.entry_count,
                    snapshot_hash: record.snapshot.hash()?,
                    reader_public_key: record.snapshot.payload.reader_public_key.clone(),
                    source_node_public_key: local,
                    issued_at_ms: now,
                    expires_at_ms: record.request.payload.expires_at_ms,
                },
                &signer,
            )?;
            usage.record_signed(&witness)?;
            record.witness = Some(witness);
        }
        let response = finish_response(json!({"page":page}), usage)?;
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![page_key(&record.snapshot.payload.snapshot_id), encode(&page)?],
        )?;
        ensure!(
            tx.execute(
                "UPDATE identity_meta SET value=? WHERE key=? AND value=?",
                params![encode(&record)?, key, encoded],
            )? == 1,
            "remote share policy cursor changed concurrently"
        );
        tx.commit()?;
        Ok(response)
    }

    pub(crate) fn execution_share_policy_finish(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
        let request: RemoteSharePolicyPageRequest = serde_json::from_value(params.clone())?;
        ensure!(
            request.cursor.is_none(),
            "remote share policy witness does not accept a cursor"
        );
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: RemoteShareReadRecord = serde_json::from_str(
            &metadata(&tx, &context_key(&request.snapshot_id))?
                .context("remote share policy snapshot is missing")?,
        )?;
        ensure!(
            record.snapshot.payload.snapshot_id == request.snapshot_id
                && record.snapshot.payload.request_hash == request.request_hash,
            "remote share policy witness request changed"
        );
        let (_, _, usage) = verify_record(&tx, &self.policy, &record, &local, principal, now)?;
        finish_response(
            json!({"witness":record.witness.context("remote share policy import is not complete")?}),
            usage,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::test_support::{TestNetwork, now},
        model::SignedEvent,
    };
    use vibeface_secure::{
        Identity,
        authorization::{
            Action, Actor, Audience, DeviceId, EventRef, Invocation, InvocationContext,
            PolicyDomain, ResourceRef, ServiceId,
            remote_share::{
                RemoteShareIntent, RemoteSharePurpose, RemoteShareTaskAttestation,
                SignedRemoteShareIntent,
            },
        },
        signature_verification::SignatureVerification,
    };

    fn carrier<T>(action: impl FnOnce() -> Result<T>) -> Result<T> {
        let verification = SignatureVerification::new();
        let result = verification.run(action)?;
        ensure!(
            verification.counts().actual_verifications <= 32,
            "remote share carrier exceeded its crypto budget"
        );
        Ok(result)
    }

    fn read_request(
        network: &TestNetwork,
        store: &Store,
        task: SignedEvent,
        reader: &Identity,
    ) -> Result<Signed<RemoteSharePolicyReadRequest>> {
        let source = PublicKey::parse(&network.node.public_key_hex()?)?;
        let task_event: crate::model::Event = serde_json::from_str(&task.canonical_body)?;
        let crate::model::EventBody::TaskCreated { task_id, .. } = &task_event.body else {
            anyhow::bail!("test sharing requires a TaskCreated event");
        };
        let project = ResourceRef {
            space: network.space.clone(),
            resource: network.resource.clone(),
        };
        let task_creation = Signed::sign(
            RemoteShareTaskAttestation {
                version: AUTH_VERSION,
                project: project.clone(),
                task: EventRef::parse(&task.hash)?,
                task_id: task_id.clone(),
                conversation_id: task_event.scope_id.clone(),
                author: task_event.author.clone(),
                sequence: task_event.sequence,
                source_node_public_key: source.clone(),
                issued_at_ms: now(),
                expires_at_ms: now() + 90_000,
            },
            &signer(&network.node)?,
        )?;
        let mut heads = [
            PolicyDomain::Account(network.account.clone()),
            PolicyDomain::Space(network.space.clone()),
        ]
        .iter()
        .map(|domain| {
            store
                .policy
                .verified()
                .head(domain)
                .cloned()
                .context("source policy head is missing")
        })
        .collect::<Result<Vec<_>>>()?;
        heads.sort_by(|a, b| a.domain.cmp(&b.domain));
        let intent = RemoteShareIntent {
            version: AUTH_VERSION,
            purpose: RemoteSharePurpose::Execution,
            project,
            work: RemoteShareWork::TaskSession {
                task_creation,
                session_id: "shared-session".into(),
                executor_kind: "codex".into(),
                binding_generation: 1,
            },
            source_node_public_key: source.clone(),
            requester: network.account.clone(),
            requester_device: DeviceId::from_key(&PublicKey::of(&network.client)),
            saved_pins: network.pins.clone(),
            policy_heads: heads.clone(),
            execution_resource: None,
            target_public_key: None,
            old_binding: None,
            old_execution_receipt: None,
            nonce: Nonce::random()?,
            issued_at_ms: now(),
            expires_at_ms: now() + 90_000,
        };
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Account(network.account.clone()),
                device: intent.requester_device.clone(),
                initiator: Actor::Account(network.account.clone()),
                initiating_grant: None,
                resource: intent.project.clone(),
                action: Action::ConversationRead,
                audience: Audience {
                    service: ServiceId::from_key(&source),
                    device: DeviceId::from_key(&source),
                },
                presenter: intent.requester_device.clone(),
                operation_id: intent.nonce.to_string(),
                body_hash: ObjectHash::body(&intent)?,
                grant_leaf: network.bundle.grant_leaf.clone(),
                expected_policies: heads,
                context: InvocationContext {
                    conversation: Some("conversation".into()),
                    task: Some(EventRef::parse(&task.hash)?),
                    workspace_resource: Some(network.resource.clone()),
                    ..Default::default()
                },
                nonce: intent.nonce.clone(),
                not_before_ms: intent.issued_at_ms,
                expires_at_ms: intent.expires_at_ms,
            },
            &network.client,
        )?;
        Ok(Signed::sign(
            RemoteSharePolicyReadRequest {
                version: AUTH_VERSION,
                intent: SignedRemoteShareIntent { intent, invocation },
                reader_public_key: PublicKey::of(reader),
                source_node_public_key: source,
                saved_pins: network.pins.clone(),
                nonce: Nonce::random()?,
                issued_at_ms: now(),
                expires_at_ms: now() + 90_000,
            },
            reader,
        )?)
    }

    #[test]
    fn remote_share_policy_binds_external_noise_reader_and_signed_cursor_retries() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        network.conversation(&mut store, "conversation")?;
        let task = network.task(&mut store, "conversation", "shared-task")?;
        let reader = Identity::generate().map_err(anyhow::Error::msg)?;
        let other_reader = Identity::generate().map_err(anyhow::Error::msg)?;
        let request = read_request(&network, &store, task.clone(), &reader)?;
        let principal = reader.public_key_hex();
        let params = serde_json::to_value(&request)?;
        let prepared = carrier(|| {
            store.execution_share_policy_prepare(&network.node, &principal, &params, now())
        })?;
        assert_eq!(
            carrier(|| {
                store.execution_share_policy_prepare(&network.node, &principal, &params, now())
            })?,
            prepared,
        );
        let snapshot: Signed<RemoteSharePolicySnapshot> =
            serde_json::from_value(prepared["snapshot"].clone())?;
        assert_eq!(snapshot.payload.reader_public_key, PublicKey::of(&reader));
        assert_eq!(snapshot.payload.intent_hash, request.payload.intent.hash()?);
        let key = context_key(&snapshot.payload.snapshot_id);
        let initial = metadata(&store.connection, &key)?;
        let mut changed = request.payload.clone();
        changed.expires_at_ms -= 1;
        let changed = serde_json::to_value(Signed::sign(changed, &reader)?)?;
        assert!(
            store
                .execution_share_policy_prepare(&network.node, &principal, &changed, now())
                .is_err()
        );
        let mut page_request = RemoteSharePolicyPageRequest {
            snapshot_id: snapshot.payload.snapshot_id.clone(),
            request_hash: request.hash()?,
            cursor: None,
        };
        let first_params = serde_json::to_value(&page_request)?;
        assert!(
            store
                .execution_share_policy_page(
                    &network.node,
                    &other_reader.public_key_hex(),
                    &first_params,
                    now(),
                )
                .is_err()
        );
        assert!(
            store
                .execution_share_policy_finish(&network.node, &principal, &first_params, now())
                .is_err()
        );
        assert_eq!(metadata(&store.connection, &key)?, initial);
        let first_response = carrier(|| {
            store.execution_share_policy_page(&network.node, &principal, &first_params, now())
        })?;
        let first: Signed<RemoteSharePolicyPage> =
            serde_json::from_value(first_response["page"].clone())?;
        assert!(!first.payload.complete);
        let after_first = metadata(&store.connection, &key)?;
        let mut poisoned = first.clone();
        poisoned.payload.request_cursor = Some(ObjectHash::body(&"changed request cursor")?);
        store.connection.execute(
            "UPDATE identity_meta SET value=? WHERE key=?",
            params![encode(&poisoned)?, page_key(&snapshot.payload.snapshot_id)],
        )?;
        assert!(
            store
                .execution_share_policy_page(&network.node, &principal, &first_params, now())
                .is_err()
        );
        assert_eq!(metadata(&store.connection, &key)?, after_first);
        store.connection.execute(
            "UPDATE identity_meta SET value=? WHERE key=?",
            params![encode(&first)?, page_key(&snapshot.payload.snapshot_id)],
        )?;
        assert_eq!(
            carrier(|| {
                store.execution_share_policy_page(&network.node, &principal, &first_params, now())
            })?,
            first_response,
        );
        page_request.cursor = Some(ObjectHash::body(&"stale cursor")?);
        assert!(
            store
                .execution_share_policy_page(
                    &network.node,
                    &principal,
                    &serde_json::to_value(&page_request)?,
                    now(),
                )
                .is_err()
        );
        page_request.cursor = first.payload.next_cursor.clone();
        let next_params = serde_json::to_value(&page_request)?;
        assert!(
            store
                .execution_share_policy_page(
                    &network.node,
                    &principal,
                    &next_params,
                    request.payload.expires_at_ms,
                )
                .is_err()
        );
        store.connection.execute(
            "UPDATE events SET status='pending' WHERE space_id=? AND hash=?",
            params![network.space.as_str(), task.hash],
        )?;
        assert!(
            store
                .execution_share_policy_page(&network.node, &principal, &next_params, now())
                .is_err()
        );
        assert_eq!(metadata(&store.connection, &key)?, after_first);
        store.connection.execute(
            "UPDATE events SET status='ready' WHERE space_id=? AND hash=?",
            params![network.space.as_str(), task.hash],
        )?;
        let mut pages = 1;
        let mut entries = u64::from(first.payload.entry.is_some());
        let final_page = loop {
            assert!(pages < 128);
            let response = carrier(|| {
                store.execution_share_policy_page(
                    &network.node,
                    &principal,
                    &serde_json::to_value(&page_request)?,
                    now(),
                )
            })?;
            let page: Signed<RemoteSharePolicyPage> =
                serde_json::from_value(response["page"].clone())?;
            assert_eq!(page.payload.page_index, pages);
            assert_eq!(page.payload.request_cursor, page_request.cursor);
            pages += 1;
            entries += u64::from(page.payload.entry.is_some());
            page_request.cursor = page.payload.next_cursor.clone();
            if page.payload.complete {
                break page;
            }
        };
        assert!(entries > 0);
        let finish_params = serde_json::to_value(&page_request)?;
        let response = carrier(|| {
            store.execution_share_policy_finish(&network.node, &principal, &finish_params, now())
        })?;
        let witness: Signed<RemoteSharePolicyWitness> =
            serde_json::from_value(response["witness"].clone())?;
        assert_eq!(witness.payload.final_page_hash, final_page.hash()?);
        assert_eq!(witness.payload.page_count, pages);
        assert_eq!(witness.payload.entry_count, entries);
        assert_eq!(witness.payload.snapshot_hash, snapshot.hash()?);
        assert_eq!(witness.payload.reader_public_key, PublicKey::of(&reader));
        assert_eq!(witness.payload.target_heads, snapshot.payload.target_heads);
        carrier(|| witness.verify_signature().map_err(anyhow::Error::from))?;
        assert_eq!(
            carrier(|| {
                store.execution_share_policy_finish(
                    &network.node,
                    &principal,
                    &finish_params,
                    now(),
                )
            })?,
            response,
        );
        Ok(())
    }
}
