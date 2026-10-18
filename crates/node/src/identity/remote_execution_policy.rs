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
        AUTH_VERSION, Action, DeviceId, JournalEntry, MAX_PROOF_BYTES, Nonce, ObjectHash,
        PolicyDomain, PolicyPin, PolicyStamp, PublicKey, Signed,
        execution_handoff::ExecutionProofUsage,
        node_execution_policy::{
            NodeExecutionPolicyReadRequest, VerifiedNodeExecutionPolicyRead,
            verify_node_execution_policy_read,
        },
        policy_proof::policy_entry_payload_usage,
        remote_execution_history::RemoteExecutionHistoryAcceptance,
        remote_execution_policy::{
            RemoteExecutionPolicyPage, RemoteExecutionPolicyPageRequest,
            RemoteExecutionPolicyPurpose, RemoteExecutionPolicyReadDelegation,
            RemoteExecutionPolicyReadIntent, RemoteExecutionPolicyReadRequest,
            RemoteExecutionPolicySnapshot, RemoteExecutionPolicyWitness,
            VerifiedRemoteExecutionPolicyRead, sign_remote_execution_policy_read_delegation,
            verify_remote_execution_history_policy_read,
            verify_remote_execution_policy_delegation_source, verify_remote_execution_policy_read,
        },
    },
    signature_verification::ScopedSignatureVerification,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteExecutionReadRecord {
    request: PolicyReadRequest,
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
    let mut response_metadata = output.clone();
    for field in ["delegation", "snapshot", "page", "witness"] {
        if let Some(value) = response_metadata.get_mut(field) {
            let hash = ObjectHash::body(&*value)?;
            *value = json!({"signed_object_hash":hash});
        }
    }
    usage.record_metadata(&response_metadata)?;
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

fn finish_reader_response(
    input: &Value,
    output: Value,
    usage: ExecutionProofUsage,
    node_reader: bool,
) -> Result<Value> {
    if node_reader {
        let (request_stage, response_stage, field) = if output.get("snapshot").is_some() {
            ("prepare", "snapshot", "snapshot")
        } else if output.get("page").is_some() {
            ("page", "page", "page")
        } else {
            ("finish", "witness", "witness")
        };
        let request = json!({"stage":request_stage,"request":input});
        let response = json!({"stage":response_stage,"response":output[field]});
        ensure!(
            serde_json::to_vec(&request)?.len() + serde_json::to_vec(&response)?.len()
                <= MAX_PROOF_BYTES,
            "Node policy read input and output exceed 64 KiB"
        );
    }
    finish_response(input, output, usage)
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "reader",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum PolicyReadRequest {
    Client(RemoteExecutionPolicyReadRequest),
    Node(NodeExecutionPolicyReadRequest),
}

impl PolicyReadRequest {
    fn hash(&self) -> Result<ObjectHash> {
        Ok(match self {
            Self::Client(request) => ObjectHash::body(request)?,
            Self::Node(request) => ObjectHash::body(request)?,
        })
    }
    fn issuer(&self) -> &PublicKey {
        match self {
            Self::Client(request) => &request.intent.issuer,
            Self::Node(request) => request.issuer(),
        }
    }
    fn nonce(&self) -> &Nonce {
        match self {
            Self::Client(request) => &request.intent.payload.nonce,
            Self::Node(NodeExecutionPolicyReadRequest::Current(request)) => &request.payload.nonce,
            Self::Node(NodeExecutionPolicyReadRequest::History(request)) => &request.payload.nonce,
        }
    }
    fn execution_pins(&self) -> &[PolicyPin] {
        match self {
            Self::Client(request) => &request.intent.payload.execution_pins,
            Self::Node(request) => request.execution_pins(),
        }
    }
    fn issued_at_ms(&self) -> u64 {
        match self {
            Self::Client(request) => request.delegation.payload.issued_at_ms,
            Self::Node(NodeExecutionPolicyReadRequest::Current(request)) => {
                request.payload.issued_at_ms
            }
            Self::Node(NodeExecutionPolicyReadRequest::History(request)) => {
                request.payload.issued_at_ms
            }
        }
    }
    fn expires_at_ms(&self) -> u64 {
        match self {
            Self::Client(request) => request.delegation.payload.expires_at_ms,
            Self::Node(request) => request.expires_at_ms(),
        }
    }
    fn is_node(&self) -> bool {
        matches!(self, Self::Node(_))
    }
}

enum VerifiedRead {
    Client(VerifiedRemoteExecutionPolicyRead),
    Node(VerifiedNodeExecutionPolicyRead),
}
impl VerifiedRead {
    fn target_heads(&self) -> &[PolicyStamp] {
        match self {
            Self::Client(value) => value.target_heads(),
            Self::Node(value) => value.target_heads(),
        }
    }
    fn domains(&self) -> Vec<PolicyDomain> {
        match self {
            Self::Client(value) => value.domains(),
            Self::Node(value) => value.domains(),
        }
    }
    fn allows_entry(&self, entry: &JournalEntry) -> vibeface_secure::authorization::Result<bool> {
        match self {
            Self::Client(value) => value.allows_entry(entry),
            Self::Node(value) => value.allows_entry(entry),
        }
    }
    fn proof_usage(&self) -> &ExecutionProofUsage {
        match self {
            Self::Client(value) => value.proof_usage(),
            Self::Node(value) => value.proof_usage(),
        }
    }
}

fn record_request_usage(
    usage: &mut ExecutionProofUsage,
    request: &PolicyReadRequest,
) -> Result<()> {
    match request {
        PolicyReadRequest::Client(request) => {
            usage.record_signed(&request.intent)?;
            usage.record_signed(&request.delegation)?;
        }
        PolicyReadRequest::Node(NodeExecutionPolicyReadRequest::Current(request)) => {
            usage.record_signed(request)?
        }
        PolicyReadRequest::Node(NodeExecutionPolicyReadRequest::History(request)) => {
            usage.record_signed(request)?
        }
    }
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

pub(super) fn checked_source_task_binding_in(
    connection: &Connection,
    intent: &Signed<RemoteExecutionPolicyReadIntent>,
    local: &PublicKey,
    usage: &mut ExecutionProofUsage,
) -> Result<Option<crate::binding::SignedBinding>> {
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
    let current = local_value(
        connection,
        project.space.as_str(),
        &format!("task-binding-current:{}", task.as_str()),
    )?;
    let Some(current) = current else {
        return Ok(None);
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
    usage.record_binding(&binding)?;
    Ok(Some(binding))
}

pub(super) fn check_source_task(
    connection: &Connection,
    intent: &Signed<RemoteExecutionPolicyReadIntent>,
    local: &PublicKey,
    usage: &mut ExecutionProofUsage,
) -> Result<()> {
    let value = &intent.payload;
    let context = &value.context.context;
    let session = context
        .execution_session
        .as_ref()
        .context("policy read has no execution session")?;
    let Some(binding) = checked_source_task_binding_in(connection, intent, local, usage)? else {
        ensure!(
            value.expected_action == Action::ExecutionOpen
                && value.purpose == RemoteExecutionPolicyPurpose::Execution
                && value.context.binding_generation == Some(1),
            "only the first execution open may read policy for an unbound Task"
        );
        return Ok(());
    };
    let prior = &binding.binding;
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
    Ok(())
}

fn context_key(id: &Nonce) -> String {
    format!("remote-execution-policy-read:{id}")
}

fn page_key(id: &Nonce) -> String {
    format!("remote-execution-policy-page:{id}")
}

fn request_key(request: &PolicyReadRequest) -> String {
    let reader = if request.is_node() { "node" } else { "client" };
    format!(
        "remote-execution-policy-request:{reader}:{}:{}",
        request.issuer(),
        request.nonce()
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
    request: &PolicyReadRequest,
    local: &PublicKey,
    principal: &str,
    now: u64,
) -> Result<(
    VerifiedRead,
    Option<RemoteExecutionHistoryAcceptance>,
    ExecutionProofUsage,
)> {
    let principal = PublicKey::parse(principal)?;
    let (verified, history, mut usage) = match request {
        PolicyReadRequest::Client(request) => {
            if request.intent.payload.purpose == RemoteExecutionPolicyPurpose::History {
                let reference = request
                    .intent
                    .payload
                    .history
                    .as_ref()
                    .context("history policy request has no original artifact reference")?;
                let (acceptance, usage) =
                    super::execution_handoff::remote_execution_history_acceptance_in(
                        connection, local, reference,
                    )?;
                let verified = verify_remote_execution_history_policy_read(
                    policy.verified(),
                    request,
                    &acceptance,
                    local,
                    &principal,
                    now,
                )?;
                (VerifiedRead::Client(verified), Some(acceptance), usage)
            } else {
                let verified = verify_remote_execution_policy_read(
                    policy.verified(),
                    request,
                    local,
                    &principal,
                    now,
                )?;
                (
                    VerifiedRead::Client(verified),
                    None,
                    ExecutionProofUsage::default(),
                )
            }
        }
        PolicyReadRequest::Node(request) => {
            let (history, usage) = if let Some(reference) = request.history() {
                let (acceptance, usage) =
                    super::execution_handoff::remote_execution_history_acceptance_in(
                        connection, local, reference,
                    )?;
                (Some(acceptance), usage)
            } else {
                (None, ExecutionProofUsage::default())
            };
            let verified = verify_node_execution_policy_read(
                policy.verified(),
                request,
                local,
                &principal,
                history.as_ref(),
                now,
            )?;
            (VerifiedRead::Node(verified), history, usage)
        }
    };
    usage.merge(verified.proof_usage());
    check_heads_in(connection, policy, verified.target_heads())?;
    policy.accepted_journal_snapshot(connection)?;
    Ok((verified, history, usage))
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
    VerifiedRead,
    Option<Signed<RemoteExecutionPolicyPage>>,
    ExecutionProofUsage,
)> {
    let (verified, history, mut usage) =
        verify_read(connection, policy, &record.request, local, principal, now)?;
    let snapshot = &record.snapshot.payload;
    ensure!(
        record.snapshot.issuer == *local
            && snapshot.version == AUTH_VERSION
            && snapshot.request_hash == record.request.hash()?
            && snapshot.saved_pins_hash == ObjectHash::body(&record.request.execution_pins())?
            && snapshot.snapshot_id
                == snapshot_id(
                    &snapshot.request_hash,
                    &record.journal_snapshot,
                    &record.snapshot_salt,
                )?
            && snapshot.target_heads == verified.target_heads()
            && snapshot.source_node_public_key == *local
            && snapshot.history == history
            && record.request.issued_at_ms() <= snapshot.issued_at_ms
            && snapshot.issued_at_ms <= now
            && now < snapshot.expires_at_ms
            && snapshot.expires_at_ms == record.request.expires_at_ms(),
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
        if intent.payload.purpose == RemoteExecutionPolicyPurpose::History {
            let reference = intent
                .payload
                .history
                .as_ref()
                .context("history policy request has no original artifact reference")?;
            usage.merge(
                &super::execution_dispatch::retained_execution_history_reference_in(
                    &tx, identity, reference,
                )?,
            );
        } else {
            check_source_task(&tx, &intent, &local, &mut usage)?;
        }
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
            return finish_response(params, json!({"delegation":saved.delegation}), usage);
        }
        let delegation = sign_remote_execution_policy_read_delegation(&verified, &signer, now)?;
        usage.record_signed(&delegation)?;
        let saved = DelegationRecord { intent, delegation };
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
        self.prepare_execution_policy_read(
            identity,
            principal,
            params,
            PolicyReadRequest::Client(request),
            now,
        )
    }

    pub(super) fn node_execution_policy_prepare(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        request: NodeExecutionPolicyReadRequest,
        now: u64,
    ) -> Result<Value> {
        let params = serde_json::to_value(&request)?;
        self.prepare_execution_policy_read(
            identity,
            principal,
            &params,
            PolicyReadRequest::Node(request),
            now,
        )
    }

    fn prepare_execution_policy_read(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        request: PolicyReadRequest,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
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
            return finish_reader_response(
                params,
                json!({"snapshot":record.snapshot}),
                usage,
                request.is_node(),
            );
        }
        let (verified, history, mut usage) =
            verify_read(&tx, &self.policy, &request, &local, principal, now)?;
        let journal_snapshot = self.policy.accepted_journal_snapshot(&tx)?;
        let snapshot_salt = Nonce::random()?;
        let id = snapshot_id(&hash, &journal_snapshot, &snapshot_salt)?;
        let snapshot = Signed::sign(
            RemoteExecutionPolicySnapshot {
                version: AUTH_VERSION,
                request_hash: hash.clone(),
                snapshot_id: id.clone(),
                saved_pins_hash: ObjectHash::body(&request.execution_pins())?,
                target_heads: verified.target_heads().to_vec(),
                source_node_public_key: local,
                history,
                issued_at_ms: now,
                expires_at_ms: request.expires_at_ms(),
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
        record_request_usage(&mut usage, &request)?;
        usage.record_signed(&snapshot)?;
        let response = finish_reader_response(
            params,
            json!({"snapshot":snapshot}),
            usage,
            request.is_node(),
        )?;
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
        self.execution_policy_page_for_reader(identity, principal, params, now, false)
    }

    pub(super) fn node_execution_policy_page(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        request: RemoteExecutionPolicyPageRequest,
        now: u64,
    ) -> Result<Value> {
        self.execution_policy_page_for_reader(
            identity,
            principal,
            &serde_json::to_value(request)?,
            now,
            true,
        )
    }

    fn execution_policy_page_for_reader(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
        node_reader: bool,
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
            record.request.is_node() == node_reader,
            "policy reader purpose changed"
        );
        ensure!(
            record.snapshot.payload.snapshot_id == request.snapshot_id
                && record.snapshot.payload.request_hash == request.request_hash,
            "remote execution policy page request changed"
        );
        let (verified, cached, mut usage) =
            verify_record(&tx, &self.policy, &record, &local, principal, now)?;
        usage.record_metadata(&request)?;
        if let Some(cached) = cached {
            if cached.payload.request_cursor == request.cursor {
                return finish_reader_response(params, json!({"page":cached}), usage, node_reader);
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
        let entry = if let Some(value) = scanned.entry {
            verified.allows_entry(&value.entry)?.then_some(value.entry)
        } else {
            None
        };
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
                    request_hash: record.request.hash()?,
                    snapshot_id: record.snapshot.payload.snapshot_id.clone(),
                    target_heads: record.snapshot.payload.target_heads.clone(),
                    final_page_hash: page.hash()?,
                    page_count: record.position.page_count,
                    entry_count: record.position.entry_count,
                    snapshot_hash: record.snapshot.hash()?,
                    source_node_public_key: local,
                    issued_at_ms: now,
                    expires_at_ms: record.request.expires_at_ms(),
                },
                &signer,
            )?;
            usage.record_signed(&witness)?;
            record.witness = Some(witness);
        }
        let response = finish_reader_response(params, json!({"page":page}), usage, node_reader)?;
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
        self.execution_policy_finish_for_reader(identity, principal, params, now, false)
    }

    pub(super) fn node_execution_policy_finish(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        request: RemoteExecutionPolicyPageRequest,
        now: u64,
    ) -> Result<Value> {
        self.execution_policy_finish_for_reader(
            identity,
            principal,
            &serde_json::to_value(request)?,
            now,
            true,
        )
    }

    fn execution_policy_finish_for_reader(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
        node_reader: bool,
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
            record.request.is_node() == node_reader,
            "policy reader purpose changed"
        );
        ensure!(
            record.snapshot.payload.snapshot_id == request.snapshot_id
                && record.snapshot.payload.request_hash == request.request_hash,
            "remote execution policy witness request changed"
        );
        let (_, _, mut usage) = verify_record(&tx, &self.policy, &record, &local, principal, now)?;
        usage.record_metadata(&request)?;
        finish_reader_response(
            params,
            json!({"witness":record.witness.context("remote execution policy import is not complete")?}),
            usage,
            node_reader,
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
    fn node_policy_reader_streams_c_without_b_business_membership() -> Result<()> {
        use vibeface_secure::authorization::node_execution_policy::NodeExecutionPolicyReadIntent;

        let mut fixture = Fixture::new()?;
        let original = read_intent(&mut fixture)?;
        let source = PublicKey::parse(&fixture.project.node.public_key_hex()?)?;
        let signer = Identity::from_secret_hex(&fixture.project.node.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let request = NodeExecutionPolicyReadRequest::Current(Signed::sign(
            NodeExecutionPolicyReadIntent {
                version: AUTH_VERSION,
                source_project: original.payload.source_selection.resource.clone(),
                selection: original.payload.selection.clone(),
                context: original.payload.context.clone(),
                expected_action: original.payload.expected_action,
                purpose: original.payload.purpose,
                execution_pins: original.payload.execution_pins.clone(),
                nonce: Nonce::random()?,
                issued_at_ms: now(),
                expires_at_ms: now() + 60_000,
            },
            &signer,
        )?);
        let path = fixture
            .executor
            .directory
            .join("node-policy-only-c.sqlite3");
        let mut target = fixture.executor.at(&path)?;
        target.install_policy_journal(&fixture.journal, now())?;
        assert!(
            target
                .authorize_transport_node(&fixture.project.node.node_id()?, now())
                .is_err()
        );
        assert!(
            carrier(|| target.node_execution_policy_prepare(
                &fixture.executor.node,
                &fixture.project.client.public_key_hex(),
                request.clone(),
                now(),
            ))
            .is_err()
        );
        let prepared = carrier(|| {
            target.node_execution_policy_prepare(
                &fixture.executor.node,
                source.as_str(),
                request.clone(),
                now(),
            )
        })?;
        let snapshot: Signed<RemoteExecutionPolicySnapshot> =
            serde_json::from_value(prepared["snapshot"].clone())?;
        snapshot.verify_signature()?;
        assert_eq!(snapshot.payload.request_hash, ObjectHash::body(&request)?);
        let mut cursor = None;
        let mut page_count = 0;
        let mut entry_count = 0;
        loop {
            let page_request = RemoteExecutionPolicyPageRequest {
                request_hash: snapshot.payload.request_hash.clone(),
                snapshot_id: snapshot.payload.snapshot_id.clone(),
                cursor: cursor.clone(),
            };
            let params = serde_json::to_value(&page_request)?;
            assert!(
                target
                    .remote_execution_policy_page(
                        &fixture.executor.node,
                        source.as_str(),
                        &params,
                        now(),
                    )
                    .is_err()
            );
            let response = carrier(|| {
                target.node_execution_policy_page(
                    &fixture.executor.node,
                    source.as_str(),
                    page_request.clone(),
                    now(),
                )
            })?;
            let page: Signed<RemoteExecutionPolicyPage> =
                serde_json::from_value(response["page"].clone())?;
            page.verify_signature()?;
            assert_eq!(page.payload.page_index, page_count);
            assert_eq!(page.payload.request_hash, snapshot.payload.request_hash);
            assert_eq!(page.payload.request_cursor, cursor);
            entry_count += u64::from(page.payload.entry.is_some());
            let repeated = carrier(|| {
                target.node_execution_policy_page(
                    &fixture.executor.node,
                    source.as_str(),
                    page_request,
                    now(),
                )
            })?;
            assert_eq!(repeated, response);
            page_count += 1;
            if page.payload.complete {
                break;
            }
            cursor = page.payload.next_cursor;
            if page_count == 1 {
                drop(target);
                target = Store::open(&path)?;
            }
            ensure!(
                page_count < 128,
                "Node policy read did not finish its bounded journal"
            );
        }
        let finished = carrier(|| {
            target.node_execution_policy_finish(
                &fixture.executor.node,
                source.as_str(),
                RemoteExecutionPolicyPageRequest {
                    request_hash: snapshot.payload.request_hash.clone(),
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    cursor: None,
                },
                now(),
            )
        })?;
        let witness: Signed<RemoteExecutionPolicyWitness> =
            serde_json::from_value(finished["witness"].clone())?;
        witness.verify_signature()?;
        assert_eq!(witness.payload.page_count, page_count);
        assert_eq!(witness.payload.entry_count, entry_count);
        assert_eq!(witness.payload.snapshot_hash, snapshot.hash()?);
        assert!(
            target
                .authorize_transport_node(&fixture.project.node.node_id()?, now())
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn remote_policy_three_accounts_preserve_the_gateway_presenter_and_execution_heads()
    -> Result<()> {
        use crate::identity::test_support::TestNetwork;
        use vibeface_secure::authorization::{
            Actor, AuthorizationError, Invocation, TransportEvidence,
            prepare::{PrepareIntent, verify_remote_execution},
            remote_execution_policy::begin_remote_execution_policy_import,
        };

        fn checked<T>(phase: &str, action: impl FnOnce() -> Result<T>) -> Result<T> {
            let signatures = SignatureVerification::new();
            let result = signatures.run(action);
            println!(
                "three_account_probe phase={phase} signatures={:?} accepted={}",
                signatures.counts(),
                result.is_ok(),
            );
            ensure!(
                signatures.counts().actual_verifications <= 32,
                "three-account phase {phase} exceeded its crypto budget"
            );
            result.with_context(|| format!("three-account phase {phase}"))
        }

        let mut fixture = Fixture::new()?;
        let gateway_network = TestNetwork::new()?;
        assert_ne!(gateway_network.account, fixture.project.account);
        assert_ne!(gateway_network.account, fixture.executor.account);
        assert_ne!(fixture.project.account, fixture.executor.account);
        let gateway_key = PublicKey::parse(&gateway_network.node.public_key_hex()?)?;
        let gateway_device = DeviceId::from_key(&gateway_key);
        let gateway_certificate = gateway_network
            .bundle
            .journal
            .iter()
            .find_map(|entry| match entry {
                JournalEntry::Device(certificate)
                    if certificate.payload.device == gateway_device =>
                {
                    Some(certificate)
                }
                _ => None,
            })
            .context("independent Gateway certificate is missing")?;
        assert_eq!(
            gateway_certificate.payload.owner,
            PolicyDomain::Account(gateway_network.account.clone()),
        );
        assert_eq!(
            gateway_certificate.payload.subject,
            Actor::Service(vibeface_secure::authorization::ServiceId::from_key(
                &gateway_key
            )),
        );
        let mut gateway = gateway_network.memory()?;
        fixture.project.install(&mut gateway)?;
        gateway_network.install(&mut fixture.store)?;
        let gateway_domain = PolicyDomain::Account(gateway_network.account.clone());
        let mut source_pins = fixture.project.pins.clone();
        source_pins.extend(
            gateway_network
                .pins
                .iter()
                .filter(|pin| pin.domain == gateway_domain)
                .cloned(),
        );
        source_pins.sort_by(|left, right| left.domain.cmp(&right.domain));
        let selection = AuthorizationSelection {
            resource: ResourceRef {
                space: fixture.project.space.clone(),
                resource: fixture.project.resource.clone(),
            },
            audience: fixture.audience.clone(),
            grant_leaf: fixture.project.bundle.grant_leaf.clone(),
        };
        let source = verified_client_policy_for_test(
            &mut gateway,
            &gateway_network.node,
            &fixture.project.client,
            &fixture.project.account,
            selection.clone(),
            &source_pins,
            now(),
        )
        .context("three-account phase genuine_source_on_independent_gateway")?;
        println!(
            "three_account_probe source_usage={}",
            source
                .client_policy_proof_usage_json()
                .map_err(anyhow::Error::msg)?
        );
        assert!(
            source
                .heads()
                .iter()
                .any(|head| head.domain == gateway_domain)
        );
        let target_key = PublicKey::parse(&fixture.executor.node.public_key_hex()?)?;
        let context = PrepareContext {
            context: InvocationContext {
                conversation: Some("conversation".into()),
                task: Some(EventRef::parse(&fixture.task.hash)?),
                execution_session: Some(SessionRef {
                    target: DeviceId::from_key(&target_key),
                    session_id: "remote-session".into(),
                }),
                executor_kind: Some("codex".into()),
                workspace_resource: Some(fixture.project.resource.clone()),
                sdk_session: None,
            },
            binding_generation: Some(1),
            canonical_cwd: Some(fixture.project.project.to_string_lossy().into_owned()),
            execution_target_public_key: Some(target_key.clone()),
        };
        let policy_input = json!({
            "selection":{
                "execution_resource":fixture.execution,
                "target_public_key":target_key,
                "execution_grant":fixture.grant,
            },
            "context":context,
            "expected_action":Action::ExecutionOpen,
            "purpose":RemoteExecutionPolicyPurpose::Execution,
            "execution_pins":fixture.executor.pins,
        });
        let intent = checked("policy_intent", || {
            build_remote_execution_policy_read_intent(
                &source,
                &policy_input.to_string(),
                &fixture.project.client,
                now() as f64,
                (now() + 60_000) as f64,
            )
            .map_err(anyhow::Error::msg)
        })?;
        let intent: Signed<RemoteExecutionPolicyReadIntent> = serde_json::from_str(&intent)?;
        let delegation = checked("project_delegate", || {
            fixture.store.remote_execution_policy_delegate(
                &fixture.project.node,
                &fixture.project.client.public_key_hex(),
                &serde_json::to_value(&intent)?,
                now(),
            )
        })?;
        let delegation = serde_json::from_value(delegation["delegation"].clone())?;
        let request = RemoteExecutionPolicyReadRequest { intent, delegation };
        let principal = fixture.project.client.public_key_hex();
        let mut target = fixture.executor.memory()?;
        target.install_policy_journal(&fixture.journal, now())?;
        assert!(
            target
                .policy
                .verified()
                .account(&gateway_network.account)
                .is_none()
        );
        assert!(
            target
                .remote_execution_policy_prepare(
                    &fixture.executor.node,
                    &gateway_network.client.public_key_hex(),
                    &serde_json::to_value(&request)?,
                    now(),
                )
                .is_err()
        );
        let prepared = checked("executor_policy_prepare", || {
            target.remote_execution_policy_prepare(
                &fixture.executor.node,
                &principal,
                &serde_json::to_value(&request)?,
                now(),
            )
        })?;
        let request_json = serde_json::to_string(&request)?;
        let snapshot_json = prepared["snapshot"].to_string();
        let executor_pins_json = serde_json::to_string(&fixture.executor.pins)?;
        let mut import = checked("begin_finite_execution_policy", || {
            begin_remote_execution_policy_import(
                &source,
                &request_json,
                &snapshot_json,
                &executor_pins_json,
                target_key.as_str(),
                now() as f64,
            )
            .map_err(anyhow::Error::msg)
        })?;
        let mut pages = 0;
        loop {
            ensure!(pages < 128, "three-account policy stream did not finish");
            let params =
                serde_json::from_str(&import.page_request_json().map_err(anyhow::Error::msg)?)?;
            let response = checked("executor_policy_page", || {
                target.remote_execution_policy_page(
                    &fixture.executor.node,
                    &principal,
                    &params,
                    now(),
                )
            })?;
            let page: Signed<RemoteExecutionPolicyPage> =
                serde_json::from_value(response["page"].clone())?;
            checked("append_finite_execution_page", || {
                import
                    .append_page(&serde_json::to_string(&page)?, now() as f64)
                    .map_err(anyhow::Error::msg)
            })?;
            pages += 1;
            if page.payload.complete {
                break;
            }
        }
        let params =
            serde_json::from_str(&import.page_request_json().map_err(anyhow::Error::msg)?)?;
        let response = checked("executor_policy_finish", || {
            target.remote_execution_policy_finish(
                &fixture.executor.node,
                &principal,
                &params,
                now(),
            )
        })?;
        let policy = checked("finish_finite_execution_policy", || {
            import
                .finish(&response["witness"].to_string(), now() as f64)
                .map_err(anyhow::Error::msg)
        })?;
        let remote = checked("three_argument_remote_execution", || {
            verify_remote_execution(&source, &policy, now() as f64).map_err(anyhow::Error::msg)
        })?;
        let operation_id = "three-account-bind";
        let mut body = fixture.selection("bind_execution", operation_id)?;
        body["conversation_id"] = json!("conversation");
        body["task_id"] = json!("remote-task");
        body["task_ref"] = json!(fixture.task.hash);
        body["target_node"] = json!(fixture.executor.node.node_id()?);
        body["session_id"] = json!("remote-session");
        body["binding_generation"] = json!(1);
        body["executor"] = json!("codex");
        body["cwd"] = json!(fixture.project.project);
        let signed_prepare = checked("native_signed_prepare", || {
            remote
                .prepare(
                    &source,
                    &body.to_string(),
                    &serde_json::to_string(&context)?,
                    "execution_open",
                    operation_id,
                    gateway_key.as_str(),
                    &fixture.project.client,
                    now() as f64,
                )
                .map_err(anyhow::Error::msg)
        })?;
        let signed_prepare: Signed<PrepareIntent> = serde_json::from_str(&signed_prepare)?;
        for (kind, domain) in [
            ("gateway_account", gateway_domain.clone()),
            (
                "project_account",
                PolicyDomain::Account(fixture.project.account.clone()),
            ),
            (
                "project_space",
                PolicyDomain::Space(fixture.project.space.clone()),
            ),
            (
                "executor_account",
                PolicyDomain::Account(fixture.executor.account.clone()),
            ),
            (
                "executor_space",
                PolicyDomain::Space(fixture.executor.space.clone()),
            ),
        ] {
            assert!(
                signed_prepare
                    .payload
                    .request
                    .expected_policies
                    .iter()
                    .any(|head| head.domain == domain),
                "signed prepare dropped the {kind} head",
            );
        }
        assert_eq!(signed_prepare.payload.request.presenter, gateway_device);
        assert_eq!(signed_prepare.payload.request.audience, fixture.audience);
        let peer = TransportEvidence::VerifiedPeer {
            origin: gateway_device.clone(),
        };
        let gateway_transport = TransportEvidence::GatewayNoise {
            device: DeviceId::from_key(&PublicKey::of(&fixture.project.client)),
            gateway: gateway_device.clone(),
        };
        let checked_prepare = checked("project_verify_prepare", || {
            fixture
                .store
                .policy
                .verified()
                .verify_prepare_intent(
                    &signed_prepare,
                    &body,
                    Action::ExecutionOpen,
                    &fixture.audience,
                    &peer,
                    now(),
                )
                .map_err(anyhow::Error::from)
        })?;
        assert!(checked_prepare.execution_destination().is_some());
        checked("gateway_verify_prepare", || {
            fixture
                .store
                .policy
                .verified()
                .verify_prepare_intent(
                    &signed_prepare,
                    &body,
                    Action::ExecutionOpen,
                    &fixture.audience,
                    &gateway_transport,
                    now(),
                )
                .map_err(anyhow::Error::from)
        })?;
        let invocation = checked("native_signed_invocation", || {
            remote
                .sign(
                    &source,
                    &serde_json::to_string(&signed_prepare.payload.request)?,
                    &body.to_string(),
                    "execution_open",
                    operation_id,
                    gateway_key.as_str(),
                    &fixture.project.client,
                    now() as f64,
                )
                .map_err(anyhow::Error::msg)
        })?;
        let invocation: Signed<Invocation> = serde_json::from_str(&invocation)?;
        assert_eq!(
            invocation.payload.expected_policies,
            signed_prepare.payload.request.expected_policies
        );
        let facts = fixture
            .store
            .execution_dispatch_facts(&body, &fixture.audience, now())?;
        let accepted = checked("project_body_aware_intake", || {
            fixture.store.authorize_request_for_body(
                &invocation,
                facts.facts(),
                &peer,
                &body,
                now(),
            )
        })?;
        assert_eq!(accepted.signed_invocation(), &invocation);
        checked("gateway_body_aware_intake", || {
            fixture.store.authorize_request_for_body(
                &invocation,
                facts.facts(),
                &gateway_transport,
                &body,
                now(),
            )
        })?;
        assert!(
            fixture
                .store
                .authorize_request(&invocation, facts.facts(), &peer, now(),)
                .is_err()
        );
        for wrong_transport in [
            TransportEvidence::GatewayNoise {
                device: DeviceId::from_key(&PublicKey::of(&fixture.executor.client)),
                gateway: gateway_device.clone(),
            },
            TransportEvidence::GatewayNoise {
                device: DeviceId::from_key(&PublicKey::of(&fixture.project.client)),
                gateway: DeviceId::from_key(&target_key),
            },
        ] {
            assert!(
                fixture
                    .store
                    .authorize_request_for_body(
                        &invocation,
                        facts.facts(),
                        &wrong_transport,
                        &body,
                        now(),
                    )
                    .is_err()
            );
        }
        let wrong_peer = TransportEvidence::VerifiedPeer {
            origin: DeviceId::from_key(&target_key),
        };
        assert!(
            fixture
                .store
                .authorize_request_for_body(&invocation, facts.facts(), &wrong_peer, &body, now(),)
                .is_err()
        );
        for missing_domain in [
            gateway_domain.clone(),
            PolicyDomain::Space(fixture.executor.space.clone()),
        ] {
            let mut missing = signed_prepare.payload.clone();
            missing
                .request
                .expected_policies
                .retain(|head| head.domain != missing_domain);
            assert!(matches!(
                fixture.store.policy.verified().verify_prepare_intent(
                    &Signed::sign(missing, &fixture.project.client)?,
                    &body,
                    Action::ExecutionOpen,
                    &fixture.audience,
                    &peer,
                    now(),
                ),
                Err(AuthorizationError::NeedPolicySync(_))
            ));
            let mut missing = invocation.payload.clone();
            missing
                .expected_policies
                .retain(|head| head.domain != missing_domain);
            let missing = Signed::sign(missing, &fixture.project.client)?;
            let error = fixture
                .store
                .authorize_request_for_body(&missing, facts.facts(), &peer, &body, now())
                .err()
                .context("missing signed invocation policy head was accepted")?;
            assert!(matches!(
                error.downcast_ref::<AuthorizationError>(),
                Some(AuthorizationError::NeedPolicySync(_))
            ));
        }
        let missing_gateway_pin = verified_client_policy_for_test(
            &mut gateway,
            &gateway_network.node,
            &fixture.project.client,
            &fixture.project.account,
            selection.clone(),
            &fixture.project.pins,
            now(),
        )
        .err()
        .context("missing Gateway root was accepted")?;
        ensure!(
            format!("{missing_gateway_pin:#}").contains("NeedPolicySync"),
            "missing Gateway root returned another error"
        );
        let mut missing_executor_pin = policy_input.clone();
        missing_executor_pin["execution_pins"] = serde_json::to_value(
            fixture
                .executor
                .pins
                .iter()
                .filter(|pin| pin.domain != PolicyDomain::Account(fixture.executor.account.clone()))
                .cloned()
                .collect::<Vec<_>>(),
        )?;
        let missing_intent = build_remote_execution_policy_read_intent(
            &source,
            &missing_executor_pin.to_string(),
            &fixture.project.client,
            now() as f64,
            (now() + 60_000) as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let missing_intent: Signed<RemoteExecutionPolicyReadIntent> =
            serde_json::from_str(&missing_intent)?;
        let missing_delegation = delegate(&mut fixture, &missing_intent)?;
        let missing_request = RemoteExecutionPolicyReadRequest {
            intent: missing_intent,
            delegation: missing_delegation,
        };
        let missing_executor_pin = target
            .remote_execution_policy_prepare(
                &fixture.executor.node,
                &principal,
                &serde_json::to_value(&missing_request)?,
                now(),
            )
            .err()
            .context("missing Executor root was accepted")?;
        ensure!(
            format!("{missing_executor_pin:#}").contains("NeedPolicySync"),
            "missing Executor root returned another error"
        );
        gateway_network.revoke_node(&mut gateway)?;
        assert!(
            verified_client_policy_for_test(
                &mut gateway,
                &gateway_network.node,
                &fixture.project.client,
                &fixture.project.account,
                selection,
                &source_pins,
                now() + 1,
            )
            .is_err()
        );
        let revoked_gateway = gateway
            .policy
            .verified()
            .account(&gateway_network.account)
            .context("revoked Gateway account is missing")?
            .clone();
        fixture.store.install_policy_journal(
            &[JournalEntry::Account(Signed::sign(
                revoked_gateway,
                &gateway_network.root,
            )?)],
            now() + 1,
        )?;
        assert!(
            fixture
                .store
                .authorize_request_for_body(&invocation, facts.facts(), &peer, &body, now() + 1,)
                .is_err()
        );
        fixture.project.revoke_client(&mut fixture.store)?;
        assert!(delegate(&mut fixture, &request.intent).is_err());
        Ok(())
    }

    #[test]
    fn remote_policy_history_imports_an_unapplied_result_after_writer_revocation() -> Result<()> {
        use crate::identity::foreign_execution_policy::{
            current_task_prepare_policy_in,
            tests::{UnseededFixture, import_current_policy, unseeded_fixture},
        };
        use std::collections::BTreeMap;
        use vibeface_secure::authorization::{
            CheckStage, DeviceId, Invocation, RequestFacts, TransportEvidence,
            execution_handoff::{ExecutionHandoffFreshness, ExecutionHandoffResult},
            prepare::PrepareIntent,
            remote_execution_policy::{
                begin_remote_execution_history_policy_import, begin_remote_execution_policy_import,
                build_remote_execution_history_policy_read_intent,
            },
            remote_resolution_history::verify_remote_execution_result_history,
        };

        let mut fixture = unseeded_fixture()?;
        let remote = import_current_policy(&mut fixture)?;
        let context = fixture.intent.payload.context.clone();
        let local = PublicKey::parse(&fixture.project.node.public_key_hex()?)?;
        let audience = fixture.intent.payload.source_selection.audience.clone();
        let selection = fixture.intent.payload.selection.clone();
        assert!(
            fixture
                .store
                .policy
                .verified()
                .account(&fixture.executor.account)
                .is_none()
        );
        let authorize = |fixture: &mut UnseededFixture, body: &Value| -> Result<_> {
            let operation_id = body["authorization_id"]
                .as_str()
                .context("history operation ID is missing")?;
            let prepared = carrier(|| {
                remote
                    .prepare(
                        &fixture.source,
                        &body.to_string(),
                        &serde_json::to_string(&context)?,
                        "execution_open",
                        operation_id,
                        local.as_str(),
                        &fixture.project.client,
                        now() as f64,
                    )
                    .map_err(anyhow::Error::msg)
            })?;
            let prepared: Signed<PrepareIntent> = serde_json::from_str(&prepared)?;
            carrier(|| {
                current_task_prepare_policy_in(
                    &fixture.store.connection,
                    &fixture.store.policy,
                    &local,
                    &prepared,
                    body,
                    now(),
                )?
                .context("history prepare has no completed foreign policy")
            })?;
            let invocation = carrier(|| {
                remote
                    .sign(
                        &fixture.source,
                        &serde_json::to_string(&prepared.payload.request)?,
                        &body.to_string(),
                        "execution_open",
                        operation_id,
                        local.as_str(),
                        &fixture.project.client,
                        now() as f64,
                    )
                    .map_err(anyhow::Error::msg)
            })?;
            let invocation: Signed<Invocation> = serde_json::from_str(&invocation)?;
            let signed = &invocation.payload;
            let facts = RequestFacts {
                resource: signed.resource.clone(),
                action: signed.action,
                audience: signed.audience.clone(),
                operation_id: signed.operation_id.clone(),
                body_hash: signed.body_hash.clone(),
                context: signed.context.clone(),
            };
            let transport = TransportEvidence::DirectNoise {
                device: DeviceId::from_key(&PublicKey::of(&fixture.project.client)),
            };
            let scoped = carrier(|| {
                fixture.store.authorize_scoped_request_for_body(
                    &fixture.project.node,
                    &invocation,
                    &facts,
                    &transport,
                    body,
                    now(),
                )
            })?;
            let dispatch = carrier(|| {
                fixture
                    .store
                    .execution_dispatch_facts_in_scope(&scoped, body, &audience, now())
            })?;
            let challenge = carrier(|| {
                fixture
                    .store
                    .issue_scoped_freshness(&scoped, CheckStage::Admit, now())
            })?;
            let mut receipts = carrier(|| {
                fixture
                    .store
                    .sign_current_receipts(&challenge, &fixture.project.node, now())
            })?;
            receipts.extend(carrier(|| {
                fixture
                    .target
                    .sign_current_receipts(&challenge, &fixture.executor.node, now())
            })?);
            Ok((
                invocation,
                scoped,
                dispatch,
                crate::identity::policy::FreshnessProof {
                    challenge,
                    receipts,
                },
                transport,
            ))
        };
        let bind_body = json!({
            "method":"bind_execution",
            "space_id":fixture.project.space,
            "resource_id":fixture.project.resource,
            "authorization_id":"history-bind",
            "binding_generation":1,
            "execution_resource":selection.execution_resource,
            "target_public_key":selection.target_public_key,
            "execution_grant":selection.execution_grant,
            "conversation_id":"foreign-conversation",
            "task_id":"foreign-task",
            "task_ref":context.context.task,
            "target_node":fixture.executor.node.node_id()?,
            "session_id":"foreign-session",
            "executor":"codex",
            "cwd":fixture.project.project,
        });
        let (_, scoped, dispatch, freshness, transport) = authorize(&mut fixture, &bind_body)?;
        let binding = carrier(|| {
            fixture.store.bind_execution_dispatch(
                &fixture.project.node,
                &dispatch,
                &scoped,
                &freshness,
                &transport,
                &bind_body,
                now(),
            )
        })?;
        let operation = crate::model::OperationRequest {
            operation_id: "history-result".into(),
            network_id: fixture.project.space.to_string(),
            conversation_id: "foreign-conversation".into(),
            task_id: Some("foreign-task".into()),
            target_node: fixture.executor.node.node_id()?,
            session_id: "foreign-session".into(),
            expected_revision: 0,
            basis_heads: vec![binding.binding.task_ref.clone()],
            action: binding.binding.open_spec.clone(),
            resources: vec![
                binding.binding.open_spec["cwd"]
                    .as_str()
                    .context("history binding cwd is missing")?
                    .into(),
            ],
            binding: Some(binding.clone()),
        };
        let body = json!({
            "method":"operate",
            "space_id":fixture.project.space,
            "resource_id":fixture.project.resource,
            "authorization_id":operation.operation_id,
            "execution_resource":selection.execution_resource,
            "target_public_key":selection.target_public_key,
            "execution_grant":selection.execution_grant,
            "request":operation,
        });
        let (invocation, scoped, dispatch, freshness, transport) = authorize(&mut fixture, &body)?;
        carrier(|| {
            fixture.store.accept_execution_dispatch(
                &fixture.project.node,
                &dispatch,
                &scoped,
                &freshness,
                &transport,
                &body,
                &operation,
                now(),
            )
        })?;
        let (wire, _) = carrier(|| {
            fixture.store.prepare_execution_dispatch_handoff(
                &fixture.project.node,
                fixture.project.space.as_str(),
                &operation.operation_id,
                now(),
            )
        })?;
        assert!(
            fixture
                .store
                .policy
                .verified()
                .account(&fixture.executor.account)
                .is_none()
        );
        let mut target = fixture.target;
        fixture.project.install(&mut target)?;
        target.local_set(
            fixture.executor.space.as_str(),
            &format!("resource-root:{}", selection.execution_resource.resource),
            &json!(fixture.project.project),
        )?;
        let peer = TransportEvidence::VerifiedPeer {
            origin: audience.device.clone(),
        };
        let challenge =
            target.issue_execution_handoff_challenge(&wire, &peer, CheckStage::Admit, now())?;
        let mut receipts =
            target.sign_execution_handoff_receipts(&challenge, &fixture.project.node, now())?;
        receipts.extend(target.sign_execution_handoff_receipts(
            &challenge,
            &fixture.executor.node,
            now(),
        )?);
        target.accept_execution_handoff(
            &fixture.executor.node,
            &wire,
            &ExecutionHandoffFreshness {
                challenge,
                receipts,
            },
            &peer,
            now(),
        )?;
        let accepted = target.execution_handoff_status(
            &fixture.executor.node,
            selection.execution_resource.space.as_str(),
            fixture.project.space.as_str(),
            &operation.operation_id,
            now(),
        )?;
        let actual = ExecutionHandoffResult {
            handoff: wire,
            receipt: accepted
                .receipt
                .context("accepted observation is missing")?,
            result: accepted.result,
        };
        let source = verified_client_policy_for_test(
            &mut fixture.store,
            &fixture.project.node,
            &fixture.project.client,
            &fixture.project.account,
            AuthorizationSelection {
                resource: ResourceRef {
                    space: fixture.project.space.clone(),
                    resource: fixture.project.resource.clone(),
                },
                audience: audience.clone(),
                grant_leaf: fixture.project.bundle.grant_leaf.clone(),
            },
            &fixture.project.pins,
            now(),
        )?;
        let mut document = target
            .policy
            .verified()
            .space(&fixture.executor.space)
            .context("execution space is missing")?
            .clone();
        document.previous = target
            .policy
            .verified()
            .head(&PolicyDomain::Space(fixture.executor.space.clone()))
            .cloned();
        document.sequence += 1;
        document
            .grants
            .retain(|hash| hash != &selection.execution_grant);
        target.install_policy_journal(
            &[JournalEntry::Space(Signed::sign(
                document,
                &fixture.executor.root,
            )?)],
            now() + 1,
        )?;
        let mut pins: BTreeMap<_, _> = fixture
            .project
            .pins
            .iter()
            .chain(&fixture.executor.pins)
            .cloned()
            .map(|pin| (pin.domain.clone(), pin))
            .collect();
        for pin in pins.values_mut() {
            pin.minimum_head = target.policy.verified().head(&pin.domain).cloned();
        }
        let pins: Vec<_> = pins.into_values().collect();
        let pins_json = serde_json::to_string(&pins)?;
        let actual_json = serde_json::to_string(&actual)?;
        let body_json = serde_json::to_string(&body)?;
        let invocation_json = serde_json::to_string(&invocation)?;
        let binding_json = serde_json::to_string(&binding)?;
        let intent_json = carrier(|| {
            build_remote_execution_history_policy_read_intent(
                &source,
                &json!({
                    "actual_json":actual_json,
                    "body_json":body_json,
                    "invocation_json":invocation_json,
                    "binding_json":binding_json,
                })
                .to_string(),
                &pins_json,
                &fixture.project.client,
                (now() + 1) as f64,
                (now() + 60_000) as f64,
            )
            .map_err(anyhow::Error::msg)
        })?;
        let intent: Signed<RemoteExecutionPolicyReadIntent> = serde_json::from_str(&intent_json)?;
        let mut current_context = intent.payload.context.clone();
        current_context.canonical_cwd =
            binding.binding.open_spec["cwd"].as_str().map(str::to_owned);
        let current_intent = carrier(|| {
            build_remote_execution_policy_read_intent(
                &source,
                &json!({
                    "selection":intent.payload.selection,
                    "context":current_context,
                    "expected_action":Action::ExecutionOpen,
                    "purpose":RemoteExecutionPolicyPurpose::Execution,
                    "execution_pins":pins,
                })
                .to_string(),
                &fixture.project.client,
                (now() + 1) as f64,
                (now() + 60_000) as f64,
            )
            .map_err(anyhow::Error::msg)
        })?;
        let current_intent: Signed<RemoteExecutionPolicyReadIntent> =
            serde_json::from_str(&current_intent)?;
        let current_delegation = carrier(|| {
            fixture.store.remote_execution_policy_delegate(
                &fixture.project.node,
                &fixture.project.client.public_key_hex(),
                &serde_json::to_value(&current_intent)?,
                now() + 1,
            )
        })?;
        let current_request = RemoteExecutionPolicyReadRequest {
            intent: current_intent,
            delegation: serde_json::from_value(current_delegation["delegation"].clone())?,
        };
        assert!(
            target
                .remote_execution_policy_prepare(
                    &fixture.executor.node,
                    &fixture.project.client.public_key_hex(),
                    &serde_json::to_value(&current_request)?,
                    now() + 1,
                )
                .is_err()
        );
        let delegation = carrier(|| {
            fixture.store.remote_execution_policy_delegate(
                &fixture.project.node,
                &fixture.project.client.public_key_hex(),
                &serde_json::to_value(&intent)?,
                now() + 1,
            )
        })?;
        let request = RemoteExecutionPolicyReadRequest {
            intent,
            delegation: serde_json::from_value(delegation["delegation"].clone())?,
        };
        let principal = fixture.project.client.public_key_hex();
        let request_json = serde_json::to_string(&request)?;
        let mut wrong = request.clone();
        wrong.intent.payload.history.as_mut().unwrap().actual_hash =
            ObjectHash::body(&"another result")?;
        wrong.intent = Signed::sign(wrong.intent.payload, &fixture.project.client)?;
        wrong.delegation.payload.intent_hash = wrong.intent.hash()?;
        wrong.delegation = Signed::sign(wrong.delegation.payload, &signer(&fixture.project.node)?)?;
        assert!(
            target
                .remote_execution_policy_prepare(
                    &fixture.executor.node,
                    &principal,
                    &serde_json::to_value(&wrong)?,
                    now() + 1,
                )
                .is_err()
        );
        let write_counts = |store: &Store| -> Result<u64> {
            Ok(store.connection.query_row(
                "SELECT (SELECT count(*) FROM authorization_nonces)+(SELECT count(*) FROM execution_handoff_nonces)+(SELECT count(*) FROM execution_handoffs)",
                [], |row| row.get(0),
            )?)
        };
        let before_writes = write_counts(&target)?;
        let prepared = carrier(|| {
            target.remote_execution_policy_prepare(
                &fixture.executor.node,
                &principal,
                &serde_json::to_value(&request)?,
                now() + 1,
            )
        })?;
        let snapshot_json = serde_json::to_string(&prepared["snapshot"])?;
        let snapshot: Signed<RemoteExecutionPolicySnapshot> =
            serde_json::from_value(prepared["snapshot"].clone())?;
        let target_key = fixture.executor.node.public_key_hex()?;
        assert!(
            begin_remote_execution_policy_import(
                &source,
                &request_json,
                &snapshot_json,
                &pins_json,
                &target_key,
                (now() + 1) as f64,
            )
            .is_err()
        );
        let mut import = carrier(|| {
            begin_remote_execution_history_policy_import(
                &source,
                &request_json,
                &snapshot_json,
                &pins_json,
                &target_key,
                (now() + 1) as f64,
            )
            .map_err(anyhow::Error::msg)
        })?;
        let key = context_key(&snapshot.payload.snapshot_id);
        let original_record =
            metadata(&target.connection, &key)?.context("history snapshot record is missing")?;
        let mut poisoned: RemoteExecutionReadRecord = serde_json::from_str(&original_record)?;
        let Some(RemoteExecutionHistoryAcceptance::ExecutionResult { receipt, .. }) =
            &mut poisoned.snapshot.payload.history
        else {
            anyhow::bail!("history snapshot has no original acceptance");
        };
        receipt.payload.admitted_at_ms += 1;
        *receipt = Signed::sign(receipt.payload.clone(), &signer(&fixture.executor.node)?)?;
        poisoned.snapshot =
            Signed::sign(poisoned.snapshot.payload, &signer(&fixture.executor.node)?)?;
        poisoned.position.previous_page_hash = poisoned.snapshot.hash()?;
        target.connection.execute(
            "UPDATE identity_meta SET value=? WHERE key=?",
            params![encode(&poisoned)?, &key],
        )?;
        let first_params: Value =
            serde_json::from_str(&import.page_request_json().map_err(anyhow::Error::msg)?)?;
        assert!(
            target
                .remote_execution_policy_page(
                    &fixture.executor.node,
                    &principal,
                    &first_params,
                    now() + 1,
                )
                .is_err()
        );
        let after_poison = metadata(&target.connection, &key)?.unwrap();
        assert_eq!(after_poison, encode(&poisoned)?);
        target.connection.execute(
            "UPDATE identity_meta SET value=? WHERE key=?",
            params![original_record, &key],
        )?;
        let mut pages = 0;
        loop {
            ensure!(pages < 128, "history policy stream did not finish");
            let params: Value =
                serde_json::from_str(&import.page_request_json().map_err(anyhow::Error::msg)?)?;
            let response = carrier(|| {
                target.remote_execution_policy_page(
                    &fixture.executor.node,
                    &principal,
                    &params,
                    now() + 1,
                )
            })?;
            let page: Signed<RemoteExecutionPolicyPage> =
                serde_json::from_value(response["page"].clone())?;
            carrier(|| {
                import
                    .append_page(&serde_json::to_string(&page)?, (now() + 1) as f64)
                    .map_err(anyhow::Error::msg)
            })?;
            pages += 1;
            if page.payload.complete {
                break;
            }
        }
        let finish_params = json!({
            "snapshot_id":snapshot.payload.snapshot_id,
            "request_hash":snapshot.payload.request_hash,
            "cursor":null,
        });
        let finished = carrier(|| {
            target.remote_execution_policy_finish(
                &fixture.executor.node,
                &principal,
                &finish_params,
                now() + 1,
            )
        })?;
        let history = carrier(|| {
            import
                .finish(
                    &serde_json::to_string(&finished["witness"])?,
                    (now() + 1) as f64,
                )
                .map_err(anyhow::Error::msg)
        })?;
        let verified = carrier(|| {
            verify_remote_execution_result_history(
                &source,
                &history,
                &actual_json,
                &body_json,
                &invocation_json,
                &binding_json,
                (now() + 1) as f64,
            )
            .map_err(anyhow::Error::msg)
        })?;
        let verified: Value = serde_json::from_str(&verified)?;
        assert_eq!(verified["historical"], true);
        assert_eq!(verified["state"], "accepted");
        assert_eq!(verified["actual"], serde_json::to_value(&actual)?);
        assert_eq!(write_counts(&target)?, before_writes);
        let unapplied: String = fixture.store.connection.query_row(
            "SELECT result FROM operations WHERE space_id=? AND operation_id=?",
            params![fixture.project.space.as_str(), operation.operation_id],
            |row| row.get(0),
        )?;
        assert_eq!(serde_json::from_str::<Value>(&unapplied)?, Value::Null);
        fixture.project.revoke_client(&mut fixture.store)?;
        assert!(
            fixture
                .store
                .remote_execution_policy_delegate(
                    &fixture.project.node,
                    &principal,
                    &serde_json::to_value(&request.intent)?,
                    now() + 2,
                )
                .is_err()
        );
        Ok(())
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
