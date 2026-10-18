use super::{
    client_policy_read::{bounded, check_heads_in, encode, metadata, signer},
    journal::{AcceptedJournalCursor, AcceptedJournalSnapshot, PolicyState},
};
use crate::{crypto::LocalIdentity, model::SignedEvent, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vibeface_secure::{
    authorization::{
        AUTH_VERSION, Action, DeviceId, MAX_PROOF_BYTES, Nonce, ObjectHash, PublicKey, Signed,
        execution_handoff::ExecutionProofUsage,
        policy_proof::policy_entry_payload_usage,
        remote_execution_policy::{
            RemoteExecutionPolicyPage, RemoteExecutionPolicyPageRequest,
            RemoteExecutionPolicyReadDelegation, RemoteExecutionPolicyReadIntent,
            RemoteExecutionPolicyReadRequest, RemoteExecutionPolicySnapshot,
            RemoteExecutionPolicyWitness, VerifiedRemoteExecutionPolicyRead,
            sign_remote_execution_policy_read_delegation,
            verify_remote_execution_policy_delegation_source, verify_remote_execution_policy_read,
        },
    },
    signature_verification::ScopedSignatureVerification,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteExecutionReadRecord {
    request: RemoteExecutionPolicyReadRequest,
    snapshot: Signed<RemoteExecutionPolicySnapshot>,
    journal_snapshot: AcceptedJournalSnapshot,
    snapshot_salt: Nonce,
    position: ReadPosition,
    witness: Option<Signed<RemoteExecutionPolicyWitness>>,
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegationRecord {
    intent: Signed<RemoteExecutionPolicyReadIntent>,
    delegation: Signed<RemoteExecutionPolicyReadDelegation>,
}

fn finish_response(input: &Value, output: Value, mut usage: ExecutionProofUsage) -> Result<Value> {
    ensure!(
        serde_json::to_vec(input)?.len() + serde_json::to_vec(&output)?.len() <= MAX_PROOF_BYTES,
        "remote execution policy input and output exceed 64 KiB"
    );
    usage.record_metadata(input)?;
    usage.record_metadata(&output)?;
    if let Some(counts) =
        vibeface_secure::signature_verification::SignatureVerification::current_counts()
    {
        ensure!(
            counts.actual_verifications <= 32,
            "remote execution policy crypto budget exceeded"
        );
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(output)
}

fn record_request_usage(
    usage: &mut ExecutionProofUsage,
    request: &RemoteExecutionPolicyReadRequest,
) -> Result<()> {
    usage.record_signed(&request.intent)?;
    usage.record_signed(&request.delegation)?;
    usage.record_metadata(request)?;
    Ok(())
}

fn delegation_key(intent: &Signed<RemoteExecutionPolicyReadIntent>) -> String {
    format!(
        "remote-execution-policy-delegation:{}:{}",
        intent.issuer, intent.payload.nonce
    )
}

fn local_value(connection: &Connection, space: &str, key: &str) -> Result<Option<String>> {
    let bytes: Option<usize> = connection
        .query_row(
            "SELECT length(CAST(value AS BLOB)) FROM local_values WHERE space_id=? AND key=?",
            params![space, key],
            |row| row.get(0),
        )
        .optional()?;
    ensure!(
        bytes.is_none_or(|bytes| bytes <= MAX_PROOF_BYTES),
        "stored execution policy Task binding is too large"
    );
    Ok(connection
        .query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![space, key],
            |row| row.get(0),
        )
        .optional()?)
}

fn check_source_task(
    connection: &Connection,
    intent: &Signed<RemoteExecutionPolicyReadIntent>,
    local: &PublicKey,
    usage: &mut ExecutionProofUsage,
) -> Result<()> {
    let value = &intent.payload;
    let project = &value.source_selection.resource;
    let context = &value.context.context;
    let task = context.task.as_ref().context("policy read has no Task")?;
    let creation: Option<(String, String, String, String, u64)> = connection
        .query_row(
            "SELECT envelope,resource_id,scope_id,author,sequence FROM events WHERE space_id=? AND hash=? AND status='ready' AND length(CAST(envelope AS BLOB))<=?",
            params![project.space.as_str(), task.as_str(), crate::model::MAX_EVENT_BYTES * 6 + 1024],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()?;
    let (creation, resource, scope, author, sequence) =
        creation.context("policy read Task is not locally ready")?;
    let creation: SignedEvent = serde_json::from_str(&creation)?;
    let event =
        crate::crypto::verify_signed_event(project.space.as_str(), &creation, local.as_str())?;
    ensure!(
        creation.hash == task.as_str()
            && event.resource_id == project.resource
            && event.resource_id.as_str() == resource
            && event.scope_id == scope
            && event.author == author
            && event.sequence == sequence
            && context.conversation.as_deref() == Some(event.scope_id.as_str()),
        "policy read Task changed its current Project, authority or conversation"
    );
    let crate::model::EventBody::TaskCreated { task_id, .. } = &event.body else {
        anyhow::bail!("policy read must reference the original TaskCreated event");
    };
    super::event_authorization::validate_retained_event_dependencies_in(connection, &creation)?;
    usage.record_retained_event(&creation)?;
    let session = context
        .execution_session
        .as_ref()
        .context("policy read has no execution session")?;
    let current = local_value(
        connection,
        project.space.as_str(),
        &format!("task-binding-current:{}", task.as_str()),
    )?;
    let Some(current) = current else {
        ensure!(
            value.expected_action == Action::ExecutionOpen
                && value.purpose
                    == vibeface_secure::authorization::remote_execution_policy::RemoteExecutionPolicyPurpose::Execution
                && value.context.binding_generation == Some(1),
            "only the first execution open may read policy for an unbound Task"
        );
        return Ok(());
    };
    let current: String = serde_json::from_str(&current)?;
    let record = local_value(
        connection,
        project.space.as_str(),
        &format!("task-binding-record:{current}"),
    )?
    .context("policy read current binding record is missing")?;
    let binding: crate::binding::SignedBinding = serde_json::from_str(&record)?;
    crate::binding::verify_binding(&binding, local.as_str())?;
    let prior = &binding.binding;
    ensure!(
        crate::binding::binding_hash(prior)? == current
            && prior.network_id == project.space.as_str()
            && prior.task_ref == task.as_str()
            && &prior.task_id == task_id
            && prior.conversation_id == event.scope_id,
        "policy read Task binding changed its retained identity"
    );
    if value.expected_action == Action::ExecutionRebind {
        ensure!(
            prior.generation.checked_add(1) == value.context.binding_generation,
            "rebind policy read must select the next binding generation"
        );
    } else {
        ensure!(
            value.context.binding_generation == Some(prior.generation)
                && prior.target_node
                    == crate::crypto::fingerprint(value.selection.target_public_key.as_str())?
                && prior.session_id == session.session_id
                && session.target == DeviceId::from_key(&value.selection.target_public_key)
                && prior.open_spec["executor"].as_str() == context.executor_kind.as_deref()
                && prior.open_spec["cwd"].as_str() == value.context.canonical_cwd.as_deref(),
            "policy read changed the current binding target, session, executor or cwd"
        );
    }
    usage.record_binding(&binding)?;
    Ok(())
}

fn context_key(id: &Nonce) -> String {
    format!("remote-execution-policy-read:{id}")
}

fn page_key(id: &Nonce) -> String {
    format!("remote-execution-policy-page:{id}")
}

fn request_key(request: &RemoteExecutionPolicyReadRequest) -> String {
    format!(
        "remote-execution-policy-request:{}:{}",
        request.intent.issuer, request.intent.payload.nonce
    )
}

fn snapshot_id(
    request: &ObjectHash,
    journal: &AcceptedJournalSnapshot,
    salt: &Nonce,
) -> Result<Nonce> {
    let hash = ObjectHash::body(&(
        "remote-execution-policy-snapshot-id-v2",
        request,
        journal,
        salt,
    ))?;
    Ok(Nonce::parse(
        hash.as_str()
            .strip_prefix("h2_")
            .context("invalid remote execution snapshot hash")?,
    )?)
}

fn verify_read(
    connection: &Connection,
    policy: &PolicyState,
    request: &RemoteExecutionPolicyReadRequest,
    local: &PublicKey,
    principal: &str,
    now: u64,
) -> Result<VerifiedRemoteExecutionPolicyRead> {
    let verified = verify_remote_execution_policy_read(
        policy.verified(),
        request,
        local,
        &PublicKey::parse(principal)?,
        now,
    )?;
    check_heads_in(connection, policy, verified.target_heads())?;
    policy.accepted_journal_snapshot(connection)?;
    Ok(verified)
}

fn cursor_hash(
    snapshot: &Signed<RemoteExecutionPolicySnapshot>,
    position: &ReadPosition,
    previous: &ObjectHash,
) -> Result<ObjectHash> {
    Ok(ObjectHash::body(&(
        "remote-execution-policy-cursor-v2",
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
    record: &RemoteExecutionReadRecord,
    local: &PublicKey,
    principal: &str,
    now: u64,
) -> Result<(
    VerifiedRemoteExecutionPolicyRead,
    Option<Signed<RemoteExecutionPolicyPage>>,
    ExecutionProofUsage,
)> {
    let verified = verify_read(connection, policy, &record.request, local, principal, now)?;
    let snapshot = &record.snapshot.payload;
    ensure!(
        record.snapshot.issuer == *local
            && snapshot.version == AUTH_VERSION
            && snapshot.request_hash == ObjectHash::body(&record.request)?
            && snapshot.saved_pins_hash
                == ObjectHash::body(&record.request.intent.payload.execution_pins)?
            && snapshot.snapshot_id
                == snapshot_id(
                    &snapshot.request_hash,
                    &record.journal_snapshot,
                    &record.snapshot_salt,
                )?
            && snapshot.target_heads == verified.target_heads()
            && snapshot.source_node_public_key == *local
            && record.request.delegation.payload.issued_at_ms <= snapshot.issued_at_ms
            && snapshot.issued_at_ms <= now
            && now < snapshot.expires_at_ms
            && snapshot.expires_at_ms == record.request.delegation.payload.expires_at_ms,
        "remote execution policy snapshot changed"
    );
    record.snapshot.verify_signature()?;
    let binding: RequestBinding = serde_json::from_str(
        &metadata(connection, &request_key(&record.request))?
            .context("remote execution policy request binding is missing")?,
    )?;
    ensure!(
        binding.request_hash == snapshot.request_hash
            && binding.snapshot_id == snapshot.snapshot_id,
        "remote execution policy request binding changed"
    );
    let mut usage = verified.proof_usage().clone();
    record_request_usage(&mut usage, &record.request)?;
    usage.record_signed(&record.snapshot)?;
    let cached: Option<Signed<RemoteExecutionPolicyPage>> =
        metadata(connection, &page_key(&snapshot.snapshot_id))?
            .map(|value| serde_json::from_str(&value))
            .transpose()?;
    if let Some(cached) = &cached {
        let page = &cached.payload;
        ensure!(
            cached.issuer == *local
                && page.version == AUTH_VERSION
                && page.request_hash == snapshot.request_hash
                && page.snapshot_id == snapshot.snapshot_id
                && page.target_heads_hash == ObjectHash::body(&snapshot.target_heads)?
                && page.page_index.checked_add(1) == Some(record.position.page_count)
                && cached.hash()? == record.position.previous_page_hash
                && page.next_cursor == record.position.cursor
                && page.complete == record.witness.is_some(),
            "remote execution policy cursor changed its signed page"
        );
        cached.verify_signature()?;
        usage.record_signed(cached)?;
        if let Some(entry) = &page.entry {
            ensure!(
                verified.allows_entry(entry)?,
                "cached execution policy page exceeds its signed reference closure"
            );
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
                "remote execution policy cursor changed its private position"
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
            "remote execution policy initial cursor changed"
        );
    }
    if let Some(witness) = &record.witness {
        let value = &witness.payload;
        ensure!(
            witness.issuer == *local
                && value.version == AUTH_VERSION
                && value.request_hash == snapshot.request_hash
                && value.snapshot_id == snapshot.snapshot_id
                && value.target_heads == snapshot.target_heads
                && value.snapshot_hash == record.snapshot.hash()?
                && value.final_page_hash == record.position.previous_page_hash
                && value.page_count == record.position.page_count
                && value.entry_count == record.position.entry_count
                && value.source_node_public_key == *local
                && snapshot.issued_at_ms <= value.issued_at_ms
                && value.issued_at_ms <= now
                && value.expires_at_ms == snapshot.expires_at_ms,
            "remote execution policy completion witness changed"
        );
        witness.verify_signature()?;
        usage.record_signed(witness)?;
    }
    Ok((verified, cached, usage))
}

impl Store {
    pub(crate) fn remote_execution_policy_delegate(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
        let intent: Signed<RemoteExecutionPolicyReadIntent> =
            serde_json::from_value(params.clone())?;
        let signer = signer(identity)?;
        let local = PublicKey::of(&signer);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let verified = verify_remote_execution_policy_delegation_source(
            self.policy.verified(),
            &intent,
            &local,
            &PublicKey::parse(principal)?,
            now,
        )?;
        check_heads_in(&tx, &self.policy, verified.source_heads())?;
        self.policy.accepted_journal_snapshot(&tx)?;
        let mut usage = verified.proof_usage().clone();
        check_source_task(&tx, &intent, &local, &mut usage)?;
        let key = delegation_key(&intent);
        if let Some(encoded) = metadata(&tx, &key)? {
            let saved: DelegationRecord = serde_json::from_str(&encoded)?;
            let value = &saved.delegation.payload;
            ensure!(
                saved.intent == intent
                    && saved.delegation.issuer == local
                    && value.version == AUTH_VERSION
                    && value.intent_hash == intent.hash()?
                    && value.source_node_public_key == local
                    && value.source_device_public_key == intent.issuer
                    && value.source_project == intent.payload.source_selection.resource
                    && Some(&value.task_ref) == intent.payload.context.context.task.as_ref()
                    && Some(&value.execution_session)
                        == intent.payload.context.context.execution_session.as_ref()
                    && intent.payload.issued_at_ms <= value.issued_at_ms
                    && value.issued_at_ms <= now
                    && value.expires_at_ms == intent.payload.expires_at_ms,
                "policy read delegation nonce or original signed metadata changed"
            );
            saved.delegation.verify_signature()?;
            usage.record_signed(&saved.delegation)?;
            usage.record_metadata(&saved)?;
            return finish_response(params, json!({"delegation":saved.delegation}), usage);
        }
        let delegation = sign_remote_execution_policy_read_delegation(&verified, &signer, now)?;
        usage.record_signed(&delegation)?;
        let saved = DelegationRecord { intent, delegation };
        usage.record_metadata(&saved)?;
        let response = finish_response(params, json!({"delegation":saved.delegation}), usage)?;
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![key, encode(&saved)?],
        )?;
        tx.commit()?;
        Ok(response)
    }

    pub(crate) fn remote_execution_policy_prepare(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
        let request: RemoteExecutionPolicyReadRequest = serde_json::from_value(params.clone())?;
        let signer = signer(identity)?;
        let local = PublicKey::of(&signer);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let nonce_key = request_key(&request);
        let hash = ObjectHash::body(&request)?;
        if let Some(binding) = metadata(&tx, &nonce_key)? {
            let binding: RequestBinding = serde_json::from_str(&binding)?;
            ensure!(
                binding.request_hash == hash,
                "remote execution policy nonce was reused with another request"
            );
            let record: RemoteExecutionReadRecord = serde_json::from_str(
                &metadata(&tx, &context_key(&binding.snapshot_id))?
                    .context("remote execution policy snapshot is missing")?,
            )?;
            ensure!(
                record.request == request
                    && record.snapshot.payload.snapshot_id == binding.snapshot_id,
                "remote execution policy request binding changed"
            );
            let (_, _, usage) = verify_record(&tx, &self.policy, &record, &local, principal, now)?;
            return finish_response(params, json!({"snapshot":record.snapshot}), usage);
        }
        let verified = verify_read(&tx, &self.policy, &request, &local, principal, now)?;
        let journal_snapshot = self.policy.accepted_journal_snapshot(&tx)?;
        let snapshot_salt = Nonce::random()?;
        let id = snapshot_id(&hash, &journal_snapshot, &snapshot_salt)?;
        let snapshot = Signed::sign(
            RemoteExecutionPolicySnapshot {
                version: AUTH_VERSION,
                request_hash: hash.clone(),
                snapshot_id: id.clone(),
                saved_pins_hash: ObjectHash::body(&request.intent.payload.execution_pins)?,
                target_heads: verified.target_heads().to_vec(),
                source_node_public_key: local,
                issued_at_ms: now,
                expires_at_ms: request.delegation.payload.expires_at_ms,
            },
            &signer,
        )?;
        let record = RemoteExecutionReadRecord {
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
        record_request_usage(&mut usage, &request)?;
        usage.record_signed(&snapshot)?;
        let response = finish_response(params, json!({"snapshot":snapshot}), usage)?;
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

    pub(crate) fn remote_execution_policy_page(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
        let request: RemoteExecutionPolicyPageRequest = serde_json::from_value(params.clone())?;
        let signer = signer(identity)?;
        let local = PublicKey::of(&signer);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = context_key(&request.snapshot_id);
        let encoded =
            metadata(&tx, &key)?.context("remote execution policy snapshot is missing")?;
        let mut record: RemoteExecutionReadRecord = serde_json::from_str(&encoded)?;
        ensure!(
            record.snapshot.payload.snapshot_id == request.snapshot_id
                && record.snapshot.payload.request_hash == request.request_hash,
            "remote execution policy page request changed"
        );
        let (verified, cached, mut usage) =
            verify_record(&tx, &self.policy, &record, &local, principal, now)?;
        if let Some(cached) = cached {
            if cached.payload.request_cursor == request.cursor {
                return finish_response(params, json!({"page":cached}), usage);
            }
        }
        ensure!(
            record.witness.is_none() && request.cursor == record.position.cursor,
            "remote execution policy cursor is stale or complete"
        );
        let scanned = self.policy.accepted_journal_page(
            &tx,
            &record.journal_snapshot,
            record.position.journal_cursor.as_ref(),
            &verified.domains().into_iter().collect(),
        )?;
        ensure!(
            scanned.scanned <= 32,
            "remote execution policy scan budget exceeded"
        );
        let entry = scanned
            .entry
            .filter(|value| verified.allows_hash(&value.entry_hash))
            .map(|value| value.entry);
        if let Some(entry) = &entry {
            usage.merge(&policy_entry_payload_usage(entry)?);
        }
        let previous_page_hash = record.position.previous_page_hash.clone();
        let page_index = record.position.page_count;
        record.position.page_count = page_index
            .checked_add(1)
            .context("remote execution policy page count overflow")?;
        record.position.entry_count = record
            .position
            .entry_count
            .checked_add(u64::from(entry.is_some()))
            .context("remote execution policy entry count overflow")?;
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
            RemoteExecutionPolicyPage {
                version: AUTH_VERSION,
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
                RemoteExecutionPolicyWitness {
                    version: AUTH_VERSION,
                    request_hash: ObjectHash::body(&record.request)?,
                    snapshot_id: record.snapshot.payload.snapshot_id.clone(),
                    target_heads: record.snapshot.payload.target_heads.clone(),
                    final_page_hash: page.hash()?,
                    page_count: record.position.page_count,
                    entry_count: record.position.entry_count,
                    snapshot_hash: record.snapshot.hash()?,
                    source_node_public_key: local,
                    issued_at_ms: now,
                    expires_at_ms: record.request.delegation.payload.expires_at_ms,
                },
                &signer,
            )?;
            usage.record_signed(&witness)?;
            record.witness = Some(witness);
        }
        let response = finish_response(params, json!({"page":page}), usage)?;
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![page_key(&record.snapshot.payload.snapshot_id), encode(&page)?],
        )?;
        ensure!(
            tx.execute(
                "UPDATE identity_meta SET value=? WHERE key=? AND value=?",
                params![encode(&record)?, key, encoded],
            )? == 1,
            "remote execution policy cursor changed concurrently"
        );
        tx.commit()?;
        Ok(response)
    }

    pub(crate) fn remote_execution_policy_finish(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
        let request: RemoteExecutionPolicyPageRequest = serde_json::from_value(params.clone())?;
        ensure!(
            request.cursor.is_none(),
            "remote execution policy witness does not accept a cursor"
        );
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: RemoteExecutionReadRecord = serde_json::from_str(
            &metadata(&tx, &context_key(&request.snapshot_id))?
                .context("remote execution policy snapshot is missing")?,
        )?;
        ensure!(
            record.snapshot.payload.snapshot_id == request.snapshot_id
                && record.snapshot.payload.request_hash == request.request_hash,
            "remote execution policy witness request changed"
        );
        let (_, _, usage) = verify_record(&tx, &self.policy, &record, &local, principal, now)?;
        finish_response(
            params,
            json!({"witness":record.witness.context("remote execution policy import is not complete")?}),
            usage,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{
        client_policy_read::verified_client_policy_for_test, execution_dispatch::tests::Fixture,
        test_support::now,
    };
    use vibeface_secure::{
        Identity,
        authorization::{
            EventRef, InvocationContext, JournalEntry, PolicyDomain, ResourceId, ResourceKind,
            ResourceRecord, ResourceRef, SessionRef,
            client_policy_import::AuthorizationSelection,
            prepare::PrepareContext,
            remote_execution_policy::{
                RemoteExecutionPolicyPurpose, build_remote_execution_policy_read_intent,
            },
        },
        signature_verification::SignatureVerification,
    };

    fn carrier<T>(action: impl FnOnce() -> Result<T>) -> Result<T> {
        let verification = SignatureVerification::new();
        let result = verification.run(action)?;
        ensure!(
            verification.counts().actual_verifications <= 32,
            "remote execution policy carrier exceeded its crypto budget"
        );
        Ok(result)
    }

    fn read_intent(fixture: &mut Fixture) -> Result<Signed<RemoteExecutionPolicyReadIntent>> {
        let gateway_network = fixture.project.same_account_node()?;
        fixture
            .store
            .install_policy_journal(&gateway_network.bundle.journal, now())?;
        let mut gateway = gateway_network.memory()?;
        let source = verified_client_policy_for_test(
            &mut gateway,
            &gateway_network.node,
            &fixture.project.client,
            &fixture.project.account,
            AuthorizationSelection {
                resource: ResourceRef {
                    space: fixture.project.space.clone(),
                    resource: fixture.project.resource.clone(),
                },
                audience: fixture.audience.clone(),
                grant_leaf: fixture.project.bundle.grant_leaf.clone(),
            },
            &fixture.project.pins,
            now(),
        )?;
        let target = PublicKey::parse(&fixture.executor.node.public_key_hex()?)?;
        let input = json!({
            "selection":{
                "execution_resource":fixture.execution,
                "target_public_key":target,
                "execution_grant":fixture.grant,
            },
            "context":PrepareContext {
                context: InvocationContext {
                    conversation:Some("conversation".into()),
                    task:Some(EventRef::parse(&fixture.task.hash)?),
                    execution_session:Some(SessionRef {
                        target:DeviceId::from_key(&target),
                        session_id:"remote-session".into(),
                    }),
                    executor_kind:Some("codex".into()),
                    workspace_resource:Some(fixture.project.resource.clone()),
                    sdk_session:None,
                },
                binding_generation:Some(1),
                canonical_cwd:Some("/workspace".into()),
                execution_target_public_key:Some(target),
            },
            "expected_action":Action::ExecutionOpen,
            "purpose":RemoteExecutionPolicyPurpose::Execution,
            "execution_pins":fixture.executor.pins,
        });
        let encoded = carrier(|| {
            build_remote_execution_policy_read_intent(
                &source,
                &serde_json::to_string(&input)?,
                &fixture.project.client,
                now() as f64,
                (now() + 60_000) as f64,
            )
            .map_err(anyhow::Error::msg)
        })?;
        Ok(serde_json::from_str(&encoded)?)
    }

    fn delegate(
        fixture: &mut Fixture,
        intent: &Signed<RemoteExecutionPolicyReadIntent>,
    ) -> Result<Signed<RemoteExecutionPolicyReadDelegation>> {
        let response = carrier(|| {
            fixture.store.remote_execution_policy_delegate(
                &fixture.project.node,
                &fixture.project.client.public_key_hex(),
                &serde_json::to_value(intent)?,
                now(),
            )
        })?;
        Ok(serde_json::from_value(response["delegation"].clone())?)
    }

    #[test]
    fn remote_policy_delegates_from_current_b_without_its_gateway_context_and_streams_only_c_references()
    -> Result<()> {
        let mut fixture = Fixture::new()?;
        let intent = read_intent(&mut fixture)?;
        assert!(
            metadata(
                &fixture.store.connection,
                &format!(
                    "client-policy-read:{}",
                    intent.payload.source_context.snapshot_id,
                )
            )?
            .is_none()
        );
        let delegation = delegate(&mut fixture, &intent)?;
        assert_eq!(delegate(&mut fixture, &intent)?, delegation);
        let request = RemoteExecutionPolicyReadRequest { intent, delegation };
        let mut target = fixture.executor.memory()?;
        target.install_policy_journal(&fixture.journal, now())?;
        assert!(
            target
                .policy
                .verified()
                .account(&fixture.project.account)
                .is_none()
        );
        let stamp = target
            .policy
            .verified()
            .head(&PolicyDomain::Space(fixture.executor.space.clone()))
            .unwrap()
            .clone();
        let unrelated = Signed::sign(
            ResourceRecord {
                version: AUTH_VERSION,
                space: fixture.executor.space.clone(),
                resource: ResourceId::from_bytes([218; 32]),
                kind: ResourceKind::Project,
                host: vibeface_secure::authorization::ServiceId::from_key(&PublicKey::parse(
                    &fixture.executor.node.public_key_hex()?,
                )?),
                issued_under: stamp,
                nonce: Nonce::random()?,
            },
            &fixture.executor.root,
        )?;
        target.install_policy_journal(&[JournalEntry::Resource(unrelated.clone())], now())?;
        let principal = fixture.project.client.public_key_hex();
        let params = serde_json::to_value(&request)?;
        let mut wrong_source = request.clone();
        wrong_source.delegation =
            Signed::sign(wrong_source.delegation.payload, &fixture.executor.root)?;
        assert!(
            target
                .remote_execution_policy_prepare(
                    &fixture.executor.node,
                    &principal,
                    &serde_json::to_value(wrong_source)?,
                    now(),
                )
                .is_err()
        );
        let mut resolver = request.clone();
        resolver.intent.payload.purpose = RemoteExecutionPolicyPurpose::Resolver;
        resolver.intent.payload.expected_action = Action::OperationResolve;
        resolver.intent = Signed::sign(resolver.intent.payload, &fixture.project.client)?;
        resolver.delegation.payload.intent_hash = resolver.intent.hash()?;
        resolver.delegation =
            Signed::sign(resolver.delegation.payload, &signer(&fixture.project.node)?)?;
        assert!(
            target
                .remote_execution_policy_prepare(
                    &fixture.executor.node,
                    &principal,
                    &serde_json::to_value(resolver)?,
                    now(),
                )
                .is_err()
        );
        let prepared = carrier(|| {
            target.remote_execution_policy_prepare(
                &fixture.executor.node,
                &principal,
                &params,
                now(),
            )
        })?;
        assert_eq!(
            carrier(|| target.remote_execution_policy_prepare(
                &fixture.executor.node,
                &principal,
                &params,
                now(),
            ))?,
            prepared
        );
        let snapshot: Signed<RemoteExecutionPolicySnapshot> =
            serde_json::from_value(prepared["snapshot"].clone())?;
        let context_key = context_key(&snapshot.payload.snapshot_id);
        let mut cursor = RemoteExecutionPolicyPageRequest {
            snapshot_id: snapshot.payload.snapshot_id.clone(),
            request_hash: ObjectHash::body(&request)?,
            cursor: None,
        };
        let initial = metadata(&target.connection, &context_key)?;
        let first_params = serde_json::to_value(&cursor)?;
        assert!(
            target
                .remote_execution_policy_page(
                    &fixture.executor.node,
                    &fixture.executor.client.public_key_hex(),
                    &first_params,
                    now(),
                )
                .is_err()
        );
        assert!(
            target
                .remote_execution_policy_finish(
                    &fixture.executor.node,
                    &principal,
                    &first_params,
                    now(),
                )
                .is_err()
        );
        assert_eq!(metadata(&target.connection, &context_key)?, initial);
        let mut page_count = 0;
        let mut entry_count = 0;
        let mut previous = snapshot.hash()?;
        let last = loop {
            ensure!(page_count < 128, "remote policy stream did not finish");
            let params = serde_json::to_value(&cursor)?;
            let response = carrier(|| {
                target.remote_execution_policy_page(
                    &fixture.executor.node,
                    &principal,
                    &params,
                    now(),
                )
            })?;
            let page: Signed<RemoteExecutionPolicyPage> =
                serde_json::from_value(response["page"].clone())?;
            assert_eq!(page.payload.request_cursor, cursor.cursor);
            assert_eq!(page.payload.page_index, page_count);
            assert_eq!(page.payload.previous_page_hash, previous);
            carrier(|| page.verify_signature().map_err(anyhow::Error::from))?;
            if let Some(JournalEntry::Resource(record)) = &page.payload.entry {
                assert_ne!(record.hash()?, unrelated.hash()?);
            }
            let after = metadata(&target.connection, &context_key)?;
            assert_eq!(
                carrier(|| target.remote_execution_policy_page(
                    &fixture.executor.node,
                    &principal,
                    &params,
                    now(),
                ))?,
                response
            );
            assert_eq!(metadata(&target.connection, &context_key)?, after);
            if page_count == 0 {
                let mut poisoned = page.clone();
                poisoned.payload.request_cursor = Some(ObjectHash::body(&"poisoned cursor")?);
                let key = page_key(&snapshot.payload.snapshot_id);
                target.connection.execute(
                    "UPDATE identity_meta SET value=? WHERE key=?",
                    params![encode(&poisoned)?, key],
                )?;
                assert!(
                    target
                        .remote_execution_policy_page(
                            &fixture.executor.node,
                            &principal,
                            &params,
                            now(),
                        )
                        .is_err()
                );
                assert_eq!(metadata(&target.connection, &context_key)?, after);
                target.connection.execute(
                    "UPDATE identity_meta SET value=? WHERE key=?",
                    params![encode(&page)?, key],
                )?;
            }
            page_count += 1;
            entry_count += u64::from(page.payload.entry.is_some());
            previous = page.hash()?;
            cursor.cursor = page.payload.next_cursor.clone();
            if page.payload.complete {
                break page;
            }
        };
        assert!(entry_count > 0);
        let finish_params = serde_json::to_value(&cursor)?;
        let finished = carrier(|| {
            target.remote_execution_policy_finish(
                &fixture.executor.node,
                &principal,
                &finish_params,
                now(),
            )
        })?;
        let witness: Signed<RemoteExecutionPolicyWitness> =
            serde_json::from_value(finished["witness"].clone())?;
        assert_eq!(witness.payload.final_page_hash, last.hash()?);
        assert_eq!(witness.payload.snapshot_hash, snapshot.hash()?);
        assert_eq!(witness.payload.page_count, page_count);
        assert_eq!(witness.payload.entry_count, entry_count);
        assert_eq!(witness.payload.target_heads, snapshot.payload.target_heads);
        carrier(|| witness.verify_signature().map_err(anyhow::Error::from))?;
        assert_eq!(
            carrier(|| target.remote_execution_policy_finish(
                &fixture.executor.node,
                &principal,
                &finish_params,
                now(),
            ))?,
            finished
        );
        let writes: u64 = target.connection.query_row(
            "SELECT (SELECT count(*) FROM authorization_nonces)+(SELECT count(*) FROM execution_handoff_nonces)+(SELECT count(*) FROM execution_handoffs)",
            [], |row| row.get(0),
        )?;
        assert_eq!(writes, 0);
        let before = metadata(&target.connection, &context_key)?;
        fixture.executor.revoke_node(&mut target)?;
        assert!(
            target
                .remote_execution_policy_finish(
                    &fixture.executor.node,
                    &principal,
                    &finish_params,
                    now() + 1,
                )
                .is_err()
        );
        assert_eq!(metadata(&target.connection, &context_key)?, before);
        Ok(())
    }

    #[test]
    fn remote_policy_source_rejects_wrong_noise_pending_task_and_oversized_current_binding()
    -> Result<()> {
        let mut fixture = Fixture::new()?;
        let intent = read_intent(&mut fixture)?;
        let params = serde_json::to_value(&intent)?;
        let key = delegation_key(&intent);
        assert!(
            fixture
                .store
                .remote_execution_policy_delegate(
                    &fixture.project.node,
                    &fixture.executor.client.public_key_hex(),
                    &params,
                    now(),
                )
                .is_err()
        );
        fixture.store.connection.execute(
            "UPDATE events SET status='pending' WHERE space_id=? AND hash=?",
            params![fixture.project.space.as_str(), fixture.task.hash],
        )?;
        assert!(delegate(&mut fixture, &intent).is_err());
        fixture.store.connection.execute(
            "UPDATE events SET status='ready' WHERE space_id=? AND hash=?",
            params![fixture.project.space.as_str(), fixture.task.hash],
        )?;
        let pointer = format!("task-binding-current:{}", fixture.task.hash);
        fixture.store.connection.execute(
            "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
            params![
                fixture.project.space.as_str(),
                pointer,
                "x".repeat(MAX_PROOF_BYTES + 1)
            ],
        )?;
        assert!(delegate(&mut fixture, &intent).is_err());
        assert!(metadata(&fixture.store.connection, &key)?.is_none());
        fixture.store.connection.execute(
            "DELETE FROM local_values WHERE space_id=? AND key=?",
            params![fixture.project.space.as_str(), pointer],
        )?;
        delegate(&mut fixture, &intent)?;
        assert!(metadata(&fixture.store.connection, &key)?.is_some());
        let mut changed = intent.payload.clone();
        changed
            .context
            .context
            .execution_session
            .as_mut()
            .unwrap()
            .session_id
            .push_str("-other");
        let changed = Signed::sign(changed, &fixture.project.client)?;
        assert!(delegate(&mut fixture, &changed).is_err());
        Ok(())
    }

    #[test]
    fn remote_policy_source_rechecks_the_current_binding_and_client_after_a_delegation()
    -> Result<()> {
        let mut fixture = Fixture::new()?;
        let mut intent = read_intent(&mut fixture)?.payload;
        intent.expected_action = Action::ExecutionSend;
        let event: crate::model::Event = serde_json::from_str(&fixture.task.canonical_body)?;
        let crate::model::EventBody::TaskCreated { task_id, .. } = event.body else {
            anyhow::bail!("fixture has no TaskCreated");
        };
        let binding = crate::binding::sign_binding(
            &fixture.project.node,
            &crate::binding::TaskBinding {
                network_id: fixture.project.space.to_string(),
                conversation_id: "conversation".into(),
                task_ref: fixture.task.hash.clone(),
                task_id,
                authority_node: fixture.project.node.node_id()?,
                generation: 1,
                target_node: fixture.executor.node.node_id()?,
                session_id: "remote-session".into(),
                open_spec: json!({"type":"open","executor":"codex","cwd":"/workspace"}),
                previous_binding_hash: None,
                previous_seal: None,
            },
        )?;
        let hash = crate::binding::binding_hash(&binding.binding)?;
        fixture.store.local_set(
            fixture.project.space.as_str(),
            &format!("task-binding-record:{hash}"),
            &serde_json::to_value(&binding)?,
        )?;
        fixture.store.local_set(
            fixture.project.space.as_str(),
            &format!("task-binding-current:{}", fixture.task.hash),
            &json!(hash),
        )?;
        let intent = Signed::sign(intent, &fixture.project.client)?;
        let original = delegate(&mut fixture, &intent)?;
        let mut changed = intent.payload.clone();
        changed.context.binding_generation = Some(2);
        let changed = Signed::sign(changed, &fixture.project.client)?;
        assert!(delegate(&mut fixture, &changed).is_err());
        let mut changed = intent.payload.clone();
        changed.context.context.executor_kind = Some("claude".into());
        let changed = Signed::sign(changed, &fixture.project.client)?;
        assert!(delegate(&mut fixture, &changed).is_err());
        assert_eq!(delegate(&mut fixture, &intent)?, original);
        let before = metadata(&fixture.store.connection, &delegation_key(&intent))?;
        fixture.project.revoke_client(&mut fixture.store)?;
        assert!(delegate(&mut fixture, &intent).is_err());
        assert_eq!(
            metadata(&fixture.store.connection, &delegation_key(&intent))?,
            before
        );
        Ok(())
    }

    #[test]
    fn remote_policy_reads_large_retained_tasks_without_metering_their_business_payload()
    -> Result<()> {
        let mut fixture = Fixture::new()?;
        let goal = "large task content ".repeat(MAX_PROOF_BYTES / 12);
        let mut body = fixture.project.body("create_task", "conversation");
        body["task_id"] = json!("large-remote-task");
        body["title"] = json!("large task");
        body["goal"] = json!(goal);
        body["acceptance"] = json!("review result");
        body["auto_followup"] = json!(false);
        fixture.task = fixture.project.append(
            &mut fixture.store,
            &body,
            crate::model::EventBody::TaskCreated {
                task_id: "large-remote-task".into(),
                title: "large task".into(),
                goal,
                acceptance: "review result".into(),
            },
            now(),
        )?;
        assert!(fixture.task.canonical_body.len() > MAX_PROOF_BYTES);
        assert!(fixture.task.canonical_body.len() < crate::model::MAX_EVENT_BYTES);
        let intent = read_intent(&mut fixture)?;
        let local = PublicKey::parse(&fixture.project.node.public_key_hex()?)?;
        let mut usage = ExecutionProofUsage::default();
        carrier(|| check_source_task(&fixture.store.connection, &intent, &local, &mut usage))?;
        assert!(usage.counts().authorization_bytes < MAX_PROOF_BYTES);
        assert_eq!(usage.counts().unique_signed_objects, 1);
        let delegated = delegate(&mut fixture, &intent)?;
        let key = delegation_key(&intent);
        let before = metadata(&fixture.store.connection, &key)?;
        let original = serde_json::to_string(&fixture.task)?;
        let mut event: crate::model::Event = serde_json::from_str(&fixture.task.canonical_body)?;
        let crate::model::EventBody::TaskCreated { goal, .. } = &mut event.body else {
            anyhow::bail!("fixture has no TaskCreated");
        };
        goal.push('!');
        let mut tampered = fixture.task.clone();
        tampered.canonical_body = serde_json::to_string(&event)?;
        fixture.store.connection.execute(
            "UPDATE events SET envelope=? WHERE space_id=? AND hash=?",
            params![
                serde_json::to_string(&tampered)?,
                fixture.project.space.as_str(),
                fixture.task.hash
            ],
        )?;
        assert!(delegate(&mut fixture, &intent).is_err());
        assert_eq!(metadata(&fixture.store.connection, &key)?, before);
        fixture.store.connection.execute(
            "UPDATE events SET envelope=? WHERE space_id=? AND hash=?",
            params![original, fixture.project.space.as_str(), fixture.task.hash],
        )?;
        let mut tampered = fixture.task.clone();
        let last = tampered.signature.pop().unwrap();
        tampered.signature.push(if last == '0' { '1' } else { '0' });
        fixture.store.connection.execute(
            "UPDATE events SET envelope=? WHERE space_id=? AND hash=?",
            params![
                serde_json::to_string(&tampered)?,
                fixture.project.space.as_str(),
                fixture.task.hash
            ],
        )?;
        assert!(delegate(&mut fixture, &intent).is_err());
        assert_eq!(metadata(&fixture.store.connection, &key)?, before);
        fixture.store.connection.execute(
            "UPDATE events SET envelope=? WHERE space_id=? AND hash=?",
            params![original, fixture.project.space.as_str(), fixture.task.hash],
        )?;
        fixture.store.connection.execute(
            "UPDATE events SET scope_id='wrong-scope' WHERE space_id=? AND hash=?",
            params![fixture.project.space.as_str(), fixture.task.hash],
        )?;
        assert!(delegate(&mut fixture, &intent).is_err());
        fixture.store.connection.execute(
            "UPDATE events SET scope_id='conversation',sequence=sequence+1 WHERE space_id=? AND hash=?",
            params![fixture.project.space.as_str(), fixture.task.hash],
        )?;
        assert!(delegate(&mut fixture, &intent).is_err());
        fixture.store.connection.execute(
            "UPDATE events SET sequence=sequence-1 WHERE space_id=? AND hash=?",
            params![fixture.project.space.as_str(), fixture.task.hash],
        )?;
        assert_eq!(delegate(&mut fixture, &intent)?, delegated);
        assert_eq!(metadata(&fixture.store.connection, &key)?, before);
        Ok(())
    }

    #[test]
    fn remote_policy_enforces_complete_wire_and_real_signature_budgets() -> Result<()> {
        let small = json!({"value":"x".repeat(100)});
        finish_response(&small, small.clone(), ExecutionProofUsage::default())?;
        let half = json!({"value":"x".repeat(MAX_PROOF_BYTES / 2)});
        assert!(finish_response(&half, half.clone(), ExecutionProofUsage::default()).is_err());
        let verification = SignatureVerification::new();
        verification.run(|| -> Result<()> {
            let identity = Identity::generate().map_err(anyhow::Error::msg)?;
            for index in 0..32 {
                let payload = index.to_string();
                let signature = identity
                    .sign("remote-policy-test", payload.as_bytes())
                    .map_err(anyhow::Error::msg)?;
                vibeface_secure::verify(
                    &identity.public_key_hex(),
                    "remote-policy-test",
                    payload.as_bytes(),
                    &signature,
                )
                .map_err(anyhow::Error::msg)?;
            }
            finish_response(&json!({}), json!({}), ExecutionProofUsage::default())?;
            let signature = identity
                .sign("remote-policy-test", b"32")
                .map_err(anyhow::Error::msg)?;
            assert_eq!(
                vibeface_secure::verify(
                    &identity.public_key_hex(),
                    "remote-policy-test",
                    b"32",
                    &signature,
                )
                .unwrap_err(),
                vibeface_secure::signature_verification::SIGNATURE_BUDGET_ERROR,
            );
            Ok(())
        })?;
        Ok(())
    }
}
