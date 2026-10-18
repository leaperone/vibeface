use super::{
    admission::{consume_authorization_in, validate_persisted_admission},
    policy::FreshnessProof,
};
use crate::{
    binding::{self, BindingSeal, SignedBinding, SignedSeal, TaskBinding},
    crypto::LocalIdentity,
    model::{
        AdmissionProof, EventBody, ExecutionSession, OperationRecord, OperationRequest,
        OperationState, StoredTransport,
    },
    store::{Store, canonical_resource},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use vibeface_secure::authorization::{
    Action, Actor, DeviceId, PublicKey, ServiceId, TransportEvidence, VerifiedAuthorization,
};

fn text<'a>(body: &'a Value, field: &str) -> Result<&'a str> {
    let value = body[field]
        .as_str()
        .with_context(|| format!("{field} is required"))?;
    ensure!(!value.is_empty(), "{field} is empty");
    Ok(value)
}

fn local_account_action(
    identity: &LocalIdentity,
    auth: &VerifiedAuthorization,
    body: &Value,
    action: Action,
    method: &str,
) -> Result<()> {
    ensure!(
        auth.action() == action
            && body["method"] == method
            && matches!(auth.actor(), Actor::Account(_)),
        "operation control requires the matching account action"
    );
    ensure!(
        vibeface_secure::authorization::ObjectHash::body(body)? == *auth.body_hash()
            && body["authorization_id"].as_str() == Some(auth.operation_id())
            && body["space_id"].as_str() == Some(auth.resource().space.as_str())
            && body["resource_id"].as_str() == Some(auth.resource().resource.as_str()),
        "operation control source differs from its invocation"
    );
    let key = PublicKey::parse(&identity.public_key_hex()?)?;
    ensure!(
        auth.audience().service == ServiceId::from_key(&key)
            && auth.audience().device == DeviceId::from_key(&key),
        "operation control targets another service audience"
    );
    Ok(())
}

pub(super) fn accepted_before(
    connection: &Connection,
    auth: &VerifiedAuthorization,
    proof: &FreshnessProof,
    transport: &TransportEvidence,
    body: &Value,
) -> Result<bool> {
    let at: Option<u64> = connection.query_row(
        "SELECT admitted_at_ms FROM accepted_authorizations WHERE space_id=? AND authorization_id=?",
        params![auth.resource().space.as_str(), auth.operation_id()], |row| row.get(0),
    ).optional()?;
    let Some(admitted_at_ms) = at else {
        return Ok(false);
    };
    validate_persisted_admission(
        connection,
        &AdmissionProof {
            invocation: auth.signed_invocation().clone(),
            source_body: body.clone(),
            challenge: proof.challenge.clone(),
            receipts: proof.receipts.clone(),
            admitted_at_ms,
            transport: StoredTransport::from(transport),
        },
    )?;
    Ok(true)
}

fn operation_from(connection: &Connection, space: &str, id: &str) -> Result<OperationRecord> {
    let (request, state, result, target, session): (String, String, String, String, String) = connection.query_row(
        "SELECT request,state,result,target_node,session_id FROM operations WHERE space_id=? AND operation_id=?",
        params![space, id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    ).context("operation is missing")?;
    let request: OperationRequest = serde_json::from_str(&request)?;
    ensure!(
        request.network_id == space
            && request.operation_id == id
            && request.target_node == target
            && request.session_id == session,
        "operation metadata differs from its request"
    );
    Ok(OperationRecord {
        request,
        state: serde_json::from_str(&state)?,
        result: serde_json::from_str(&result)?,
    })
}

fn value_from(connection: &Connection, space: &str, key: &str) -> Result<Option<Value>> {
    let encoded: Option<String> = connection
        .query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![space, key],
            |row| row.get(0),
        )
        .optional()?;
    encoded
        .map(|value| Ok(serde_json::from_str(&value)?))
        .transpose()
}

fn current_binding_in(connection: &Connection, expected: &SignedBinding) -> Result<String> {
    let binding = &expected.binding;
    let hash = binding::binding_hash(binding)?;
    let pointer = value_from(
        connection,
        &binding.network_id,
        &format!("task-binding-current:{}", binding.task_ref),
    )?
    .context("task has no current binding")?;
    ensure!(
        pointer.as_str() == Some(&hash),
        "task binding was superseded"
    );
    let record = value_from(
        connection,
        &binding.network_id,
        &format!("task-binding-record:{hash}"),
    )?
    .context("current binding record is missing")?;
    ensure!(
        serde_json::from_value::<SignedBinding>(record)? == *expected,
        "stored binding differs from the signed source binding"
    );
    let ready: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE space_id=? AND hash=? AND status='ready')",
        params![binding.network_id, binding.task_ref],
        |row| row.get(0),
    )?;
    ensure!(ready, "bound task creation is no longer ready");
    Ok(hash)
}

fn check_binding_source(
    binding: &TaskBinding,
    auth: &VerifiedAuthorization,
    body: &Value,
) -> Result<()> {
    ensure!(
        binding.network_id == auth.resource().space.as_str()
            && binding.conversation_id == text(body, "conversation_id")?
            && binding.task_id == text(body, "task_id")?
            && binding.task_ref == text(body, "task_ref")?,
        "source binding belongs to another space, conversation, or task"
    );
    Ok(())
}

impl Store {
    pub fn resolve_authorized_operation(
        &mut self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        body: &Value,
        now_ms: u64,
    ) -> Result<OperationRecord> {
        local_account_action(
            identity,
            auth,
            body,
            Action::OperationResolve,
            "resolve_unknown",
        )?;
        let space = auth.resource().space.as_str();
        let id = text(body, "operation_id")?;
        ensure!(
            auth.operation_id() != id,
            "resolution needs a distinct authorization ID"
        );
        let evidence = text(body, "evidence")?;
        ensure!(
            evidence.trim().len() >= 12,
            "resolution needs evidence of the process and external effects"
        );
        let state = match text(body, "outcome")? {
            "succeeded" => OperationState::Succeeded,
            "failed" => OperationState::Failed,
            _ => anyhow::bail!("resolution outcome must be succeeded or failed"),
        };
        let result = json!({"resolver":auth.actor(),"invocation_hash":auth.invocation_hash(),"evidence":evidence});
        let original = self.operation_admission_proof(space, id)?;
        ensure!(
            original.invocation.payload.resource == *auth.resource(),
            "resolution belongs to another admitted resource"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let operation = operation_from(&tx, space, id)?;
        ensure!(
            operation.request.target_node == identity.node_id()?
                && operation.request.target_node == text(body, "target_node")?,
            "resolution targets another executor"
        );
        ensure!(
            operation.request.conversation_id == text(body, "conversation_id")?
                && operation.request.session_id == text(body, "session_id")?
                && serde_json::to_value(&operation.request.task_id)? == body["task_id"],
            "resolution differs from the original operation context"
        );
        if let Some(binding) = &operation.request.binding {
            ensure!(
                text(body, "task_ref")? == binding.binding.task_ref,
                "resolution references another bound task creation"
            );
        }
        ensure!(
            serde_json::from_value::<OperationRequest>(original.source_body["request"].clone())?
                == operation.request,
            "operation differs from its original admission"
        );
        validate_persisted_admission(&tx, &original)?;
        if accepted_before(&tx, auth, proof, transport, body)? {
            ensure!(
                operation.state == state && operation.result == result,
                "accepted resolution differs from the terminal ledger"
            );
            return Ok(operation);
        }
        ensure!(
            operation.state == OperationState::Unknown,
            "only an unknown operation can be resolved"
        );
        let admission = consume_authorization_in(
            &tx,
            &self.policy,
            auth,
            proof,
            transport,
            body,
            None,
            now_ms,
        )?;
        ensure!(tx.execute("UPDATE operations SET state=?,result=? WHERE space_id=? AND operation_id=? AND state='\"unknown\"' AND request=?",
            params![serde_json::to_string(&state)?, serde_json::to_string(&result)?, space, id, serde_json::to_string(&operation.request)?])? == 1,
            "operation changed before resolution");
        tx.execute(
            "DELETE FROM resource_reservations WHERE space_id=? AND operation_id=?",
            params![space, id],
        )?;
        let encoded: Option<String> = tx.query_row("SELECT record FROM execution_sessions WHERE space_id=? AND target_node=? AND session_id=?",
            params![space, operation.request.target_node, operation.request.session_id], |row| row.get(0)).optional()?;
        if let Some(encoded) = encoded {
            let mut session: ExecutionSession = serde_json::from_str(&encoded)?;
            let revision: u64 = tx.query_row("SELECT revision FROM execution_sessions WHERE space_id=? AND target_node=? AND session_id=?",
                params![space, operation.request.target_node, operation.request.session_id], |row| row.get(0))?;
            ensure!(
                session.network_id == space
                    && session.target_node == operation.request.target_node
                    && session.session_id == operation.request.session_id
                    && session.revision == revision,
                "execution session metadata changed"
            );
            session.revision = revision.max(
                operation
                    .request
                    .expected_revision
                    .checked_add(1)
                    .context("execution revision overflow")?,
            );
            let pending: Vec<String> = tx.prepare("SELECT state FROM operations WHERE space_id=? AND target_node=? AND session_id=? AND state IN ('\"accepted\"','\"running\"','\"unknown\"')")?
                .query_map(params![space, operation.request.target_node, operation.request.session_id], |row| row.get(0))?.collect::<std::result::Result<_, _>>()?;
            session.state = if pending.iter().any(|state| state == "\"unknown\"") {
                "unknown"
            } else if !pending.is_empty() {
                "running"
            } else {
                "idle"
            }
            .into();
            ensure!(tx.execute("UPDATE execution_sessions SET revision=?,record=? WHERE space_id=? AND target_node=? AND session_id=? AND revision=? AND record=?",
                params![session.revision,serde_json::to_string(&session)?,space,session.target_node,session.session_id,revision,encoded])? == 1,
                "execution session changed before resolution");
        }
        super::event_authorization::append_admitted_resolution_in(
            &tx,
            &self.policy,
            identity,
            &admission,
            &operation.request,
            &state,
            &result,
            now_ms,
        )?;
        tx.execute(
            "DELETE FROM local_values WHERE space_id=? AND key IN (?,?)",
            params![space, format!("completion:{id}"), format!("lease:{id}")],
        )?;
        tx.commit()?;
        Ok(OperationRecord {
            request: operation.request,
            state,
            result,
        })
    }

    pub fn seal_task_authorized(
        &mut self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        body: &Value,
        now_ms: u64,
    ) -> Result<SignedSeal> {
        local_account_action(
            identity,
            auth,
            body,
            Action::ExecutionRebind,
            "seal_execution",
        )?;
        let binding: SignedBinding = serde_json::from_value(body["binding"].clone())?;
        check_binding_source(&binding.binding, auth, body)?;
        self.verify_binding_creation(&binding)?;
        let task = self.verify_event(
            &binding.binding.network_id,
            &self
                .event(&binding.binding.network_id, &binding.binding.task_ref)?
                .context("bound task creation is missing")?,
        )?;
        ensure!(
            task.resource_id == auth.resource().resource
                && binding.binding.target_node == identity.node_id()?,
            "seal targets another resource or executor"
        );
        let hash = binding::binding_hash(&binding.binding)?;
        let key = format!("task-binding-seal:{hash}");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if accepted_before(&tx, auth, proof, transport, body)? {
            let prior: SignedSeal = serde_json::from_value(
                value_from(&tx, &binding.binding.network_id, &key)?
                    .context("accepted seal is missing")?,
            )?;
            binding::verify_seal(&prior, &identity.public_key_hex()?)?;
            ensure!(
                prior.seal.binding_hash == hash,
                "accepted seal closes another binding"
            );
            return Ok(prior);
        }
        current_binding_in(&tx, &binding)?;
        let busy: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE space_id=? AND target_node=? AND session_id=? AND state IN ('\"accepted\"','\"running\"','\"unknown\"'))",
            params![binding.binding.network_id,binding.binding.target_node,binding.binding.session_id], |row| row.get(0))?;
        ensure!(
            !busy,
            "execution has accepted, running, or unknown operations"
        );
        let pending: Vec<(String,String)> = tx.prepare("SELECT o.operation_id,o.state FROM operations o JOIN local_values v ON v.space_id=o.space_id AND v.key='completion:' || o.operation_id WHERE o.space_id=? AND o.target_node=? AND o.session_id=?")?
            .query_map(params![binding.binding.network_id,binding.binding.target_node,binding.binding.session_id],|row| Ok((row.get(0)?,row.get(1)?)))?.collect::<std::result::Result<_,_>>()?;
        if !pending.is_empty() {
            let events = super::event_authorization::retained_scope_events_in(
                &tx,
                &binding.binding.network_id,
                &auth.resource().resource,
                &binding.binding.conversation_id,
            )?;
            for (id, state) in pending {
                let state: OperationState = serde_json::from_str(&state)?;
                let status = serde_json::to_value(&state)?;
                let completions = events.values().filter(|stored| stored.status == "ready" && stored.event.author == binding.binding.target_node
                    && stored.event.scope_id == binding.binding.conversation_id
                    && matches!(&stored.event.body, EventBody::ExecutionObserved {operation_id,target_node,session_id,status:actual,..}
                        if operation_id == &id && target_node == &binding.binding.target_node && session_id == &binding.binding.session_id && Some(actual.as_str()) == status.as_str())).collect::<Vec<_>>();
                ensure!(completions.len() == 1,
                    "execution completion is not durable");
                let completion = completions[0];
                ensure!(
                    super::event_authorization::verify_event_envelope(
                        &tx,
                        &self.policy,
                        &binding.binding.network_id,
                        &completion.envelope,
                    )? == completion.event,
                    "execution completion signature differs from its retained event"
                );
            }
        }
        let session: Option<(u64,String)> = tx.query_row("SELECT revision,record FROM execution_sessions WHERE space_id=? AND target_node=? AND session_id=?",
            params![binding.binding.network_id,binding.binding.target_node,binding.binding.session_id], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
        let revision = if let Some((revision, encoded)) = session {
            let session: ExecutionSession = serde_json::from_str(&encoded)?;
            ensure!(
                session.network_id == binding.binding.network_id
                    && session.target_node == binding.binding.target_node
                    && session.session_id == binding.binding.session_id
                    && session.revision == revision
                    && matches!(session.state.as_str(), "idle" | "closed"),
                "execution session is not idle or has changed"
            );
            revision
        } else {
            0
        };
        consume_authorization_in(
            &tx,
            &self.policy,
            auth,
            proof,
            transport,
            body,
            None,
            now_ms,
        )?;
        let signed = binding::sign_seal(
            identity,
            &BindingSeal {
                network_id: binding.binding.network_id.clone(),
                binding_hash: hash,
                target_node: binding.binding.target_node.clone(),
                final_revision: revision,
            },
        )?;
        if let Some(existing) = value_from(&tx, &binding.binding.network_id, &key)? {
            ensure!(
                serde_json::from_value::<SignedSeal>(existing)? == signed,
                "existing seal differs from the final execution fence"
            );
        } else {
            tx.execute(
                "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
                params![
                    binding.binding.network_id,
                    key,
                    serde_json::to_string(&signed)?
                ],
            )?;
        }
        tx.commit()?;
        Ok(signed)
    }

    pub fn rebind_task_authorized(
        &mut self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        body: &Value,
        now_ms: u64,
    ) -> Result<SignedBinding> {
        local_account_action(
            identity,
            auth,
            body,
            Action::ExecutionRebind,
            "rebind_execution",
        )?;
        let old: SignedBinding = serde_json::from_value(body["binding"].clone())?;
        check_binding_source(&old.binding, auth, body)?;
        self.verify_historical_binding(&old)?;
        let creation = self.verify_event(
            &old.binding.network_id,
            &self
                .event(&old.binding.network_id, &old.binding.task_ref)?
                .context("task creation is missing")?,
        )?;
        ensure!(
            creation.author == identity.node_id()?
                && old.binding.authority_node == creation.author
                && creation.resource_id == auth.resource().resource,
            "rebind targets another task authority or resource"
        );
        let old_hash = binding::binding_hash(&old.binding)?;
        let seal: SignedSeal = serde_json::from_value(body["previous_seal"].clone())?;
        let target_key: String = self.connection.query_row(
            "SELECT public_key FROM node_certificates WHERE space_id=? AND node_id=?",
            params![old.binding.network_id, old.binding.target_node],
            |row| row.get(0),
        )?;
        binding::verify_seal(&seal, &target_key)?;
        ensure!(
            seal.seal.network_id == old.binding.network_id
                && seal.seal.binding_hash == old_hash
                && seal.seal.target_node == old.binding.target_node,
            "previous seal does not close the source binding"
        );
        let cwd = text(body, "cwd")?;
        ensure!(
            canonical_resource(cwd)? == cwd,
            "new execution directory is not canonical"
        );
        let next = TaskBinding {
            network_id: old.binding.network_id.clone(),
            conversation_id: old.binding.conversation_id.clone(),
            task_ref: old.binding.task_ref.clone(),
            task_id: old.binding.task_id.clone(),
            authority_node: old.binding.authority_node.clone(),
            generation: old
                .binding
                .generation
                .checked_add(1)
                .context("binding generation overflow")?,
            target_node: text(body, "target_node")?.into(),
            session_id: text(body, "session_id")?.into(),
            open_spec: json!({"type":"open","executor":text(body,"executor")?,"cwd":cwd}),
            previous_binding_hash: Some(old_hash.clone()),
            previous_seal: Some(seal),
        };
        self.verify_binding_task(&next, true)?;
        let signed = binding::sign_binding(identity, &next)?;
        let hash = binding::binding_hash(&next)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if accepted_before(&tx, auth, proof, transport, body)? {
            let stored: SignedBinding = serde_json::from_value(
                value_from(
                    &tx,
                    &next.network_id,
                    &format!("task-binding-record:{hash}"),
                )?
                .context("accepted rebind is missing")?,
            )?;
            ensure!(
                stored == signed,
                "accepted rebind differs from the signed source request"
            );
            return Ok(stored);
        }
        current_binding_in(&tx, &old)?;
        consume_authorization_in(
            &tx,
            &self.policy,
            auth,
            proof,
            transport,
            body,
            None,
            now_ms,
        )?;
        tx.execute(
            "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
            params![
                next.network_id,
                format!("task-binding-record:{hash}"),
                serde_json::to_string(&signed)?
            ],
        )?;
        ensure!(
            tx.execute(
                "UPDATE local_values SET value=? WHERE space_id=? AND key=? AND value=?",
                params![
                    serde_json::to_string(&hash)?,
                    next.network_id,
                    format!("task-binding-current:{}", next.task_ref),
                    serde_json::to_string(&old_hash)?
                ]
            )? == 1,
            "task binding changed before rebind"
        );
        tx.commit()?;
        Ok(signed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::{admission::tests::Fixture, policy::FreshnessProof},
        model::SignedEvent,
    };
    use vibeface_secure::{
        Identity,
        authorization::{
            AUTH_VERSION, Audience, CheckStage, JournalEntry, Nonce, ObjectHash, PolicyDomain,
            ResourceRef, Signed,
        },
    };

    const NOW: u64 = 1_000;

    struct ControlFixture {
        store: Store,
        user: Identity,
        authority: Identity,
        node: LocalIdentity,
        actor: Actor,
        audience: Audience,
        resource: ResourceRef,
        grant: ObjectHash,
        binding: SignedBinding,
        cwd: String,
    }

    impl ControlFixture {
        fn new() -> Result<Self> {
            let fixture = Fixture::new()?;
            let freshness = fixture.proof()?;
            let actor = fixture.auth.actor().clone();
            let audience = fixture.auth.audience().clone();
            let resource = fixture.auth.resource().clone();
            let mut store = Store {
                connection: fixture.connection,
                policy: fixture.policy,
                target_recovery_import: None,
            account_node_enrollment_import: None,
            };
            store.append_authorized_event(
                &fixture.node,
                &fixture.auth,
                &freshness,
                &fixture.transport,
                &fixture.body,
                "chat",
                EventBody::ConversationCreated {
                    title: "test".into(),
                },
                vec![],
                NOW as i64,
            )?;
            let root =
                Identity::from_secret_hex(&hex::encode([1; 32])).map_err(anyhow::Error::msg)?;
            let user =
                Identity::from_secret_hex(&hex::encode([4; 32])).map_err(anyhow::Error::msg)?;
            let node_signer = Identity::from_secret_hex(&fixture.node.secret_key_hex)
                .map_err(anyhow::Error::msg)?;
            let mut device_entries = Vec::new();
            let account = match &actor {
                Actor::Account(account) => account,
                _ => unreachable!(),
            };
            let mut document = store
                .authorization_state()
                .account(account)
                .unwrap()
                .clone();
            let mut devices = Vec::new();
            for hash in &document.devices {
                let mut certificate = store
                    .authorization_state()
                    .device(hash)
                    .unwrap()
                    .payload
                    .clone();
                certificate.expires_at_ms = 9_007_199_254_000_000;
                certificate.nonce = Nonce::random()?;
                let key = match &certificate.subject {
                    Actor::Account(_) => &user,
                    Actor::Service(_) => &node_signer,
                };
                certificate.prove_possession(key)?;
                let signed = Signed::sign(certificate, &root)?;
                devices.push(signed.hash()?);
                device_entries.push(JournalEntry::Device(signed));
            }
            devices.sort();
            document.previous = store
                .authorization_state()
                .head(&PolicyDomain::Account(account.clone()))
                .cloned();
            document.sequence += 1;
            document.devices = devices;
            device_entries.push(JournalEntry::Account(Signed::sign(document, &root)?));
            store.install_policy_journal(&device_entries, NOW)?;
            let mut grant = store
                .authorization_state()
                .grant(&fixture.auth.signed_invocation().payload.grant_leaf)
                .unwrap()
                .payload
                .clone();
            grant.actions = vec![
                Action::ConversationCreate,
                Action::MessageAppend,
                Action::TaskCreate,
                Action::ExecutionOpen,
                Action::ExecutionRebind,
                Action::OperationResolve,
            ];
            grant.nonce = Nonce::random()?;
            let grant = Signed::sign(grant, &root)?;
            let hash = grant.hash()?;
            let mut space = store
                .authorization_state()
                .space(&resource.space)
                .unwrap()
                .clone();
            space.previous = store
                .authorization_state()
                .head(&PolicyDomain::Space(resource.space.clone()))
                .cloned();
            space.sequence += 1;
            space.grants.push(hash.clone());
            space.grants.sort();
            store.install_policy_journal(
                &[
                    JournalEntry::Grant(grant),
                    JournalEntry::Space(Signed::sign(space, &root)?),
                ],
                NOW,
            )?;
            let cwd = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR"))?
                .to_string_lossy()
                .into_owned();
            store.local_set(
                resource.space.as_str(),
                &format!("resource-root:{}", resource.resource),
                &json!(cwd),
            )?;
            let placeholder = TaskBinding {
                network_id: resource.space.to_string(),
                conversation_id: "chat".into(),
                task_ref: "0".repeat(64),
                task_id: "task".into(),
                authority_node: fixture.node.node_id()?,
                generation: 1,
                target_node: fixture.node.node_id()?,
                session_id: "old".into(),
                open_spec: json!({"type":"open","executor":"codex","cwd":cwd}),
                previous_binding_hash: None,
                previous_seal: None,
            };
            let mut result = Self {
                store,
                user,
                authority: fixture.authority,
                node: fixture.node,
                actor,
                audience,
                resource,
                grant: hash,
                binding: SignedBinding {
                    binding: placeholder,
                    signature: String::new(),
                },
                cwd,
            };
            let task_body = json!({"method":"create_task","space_id":result.resource.space,"resource_id":result.resource.resource,"conversation_id":"chat",
                "task_id":"task","title":"task","goal":"goal","acceptance":"acceptance","authorization_id":"task-create"});
            let (auth, proof) = result.authorization(&task_body)?;
            let task = result.store.append_authorized_event(
                &result.node,
                &auth,
                &proof,
                &result.transport(),
                &task_body,
                "chat",
                EventBody::TaskCreated {
                    task_id: "task".into(),
                    title: "task".into(),
                    goal: "goal".into(),
                    acceptance: "acceptance".into(),
                },
                vec![],
                NOW as i64,
            )?;
            let bind_body = json!({"method":"bind_execution","space_id":result.resource.space,"resource_id":result.resource.resource,"conversation_id":"chat",
                "task_id":"task","task_ref":task.hash,"target_node":result.node.node_id()?,"session_id":"old","executor":"codex","cwd":result.cwd,"authorization_id":"bind-initial"});
            let (auth, proof) = result.authorization(&bind_body)?;
            result.binding = result.store.bind_task_authorized(
                &result.node,
                &auth,
                &proof,
                &result.transport(),
                &bind_body,
                NOW,
            )?;
            Ok(result)
        }

        fn transport(&self) -> TransportEvidence {
            TransportEvidence::DirectNoise {
                device: DeviceId::from_key(&PublicKey::of(&self.user)),
            }
        }

        fn authorization(
            &mut self,
            body: &Value,
        ) -> Result<(VerifiedAuthorization, FreshnessProof)> {
            let facts = self.store.request_facts(body, &self.audience)?;
            let device = DeviceId::from_key(&PublicKey::of(&self.user));
            let heads = self
                .store
                .trusted_policy_pins()
                .iter()
                .filter_map(|pin| self.store.authorization_state().head(&pin.domain).cloned())
                .collect();
            let invocation = Signed::sign(
                vibeface_secure::authorization::Invocation {
                    version: AUTH_VERSION,
                    actor: self.actor.clone(),
                    device: device.clone(),
                    initiator: self.actor.clone(),
                    initiating_grant: None,
                    resource: facts.resource,
                    action: facts.action,
                    audience: facts.audience,
                    presenter: device,
                    operation_id: facts.operation_id,
                    body_hash: facts.body_hash,
                    grant_leaf: self.grant.clone(),
                    expected_policies: heads,
                    context: facts.context,
                    nonce: Nonce::random()?,
                    not_before_ms: NOW,
                    expires_at_ms: NOW + 10_000,
                },
                &self.user,
            )?;
            let auth = self.store.authorize_request(
                &invocation,
                &self.store.request_facts(body, &self.audience)?,
                &self.transport(),
                NOW,
            )?;
            let challenge = self.store.issue_freshness(&auth, CheckStage::Admit, NOW)?;
            let receipts = self.store.sign_current_receipts(
                &challenge,
                &LocalIdentity {
                    secret_key_hex: self.authority.secret_hex(),
                },
                NOW,
            )?;
            Ok((
                auth,
                FreshnessProof {
                    challenge,
                    receipts,
                },
            ))
        }

        fn seal_body(&self, id: &str) -> Value {
            json!({"method":"seal_execution","space_id":self.resource.space,"resource_id":self.resource.resource,"conversation_id":"chat",
            "task_id":"task","task_ref":self.binding.binding.task_ref,"binding":self.binding,"authorization_id":id})
        }

        fn operation(&mut self) -> Result<(OperationRequest, SignedEvent)> {
            let request = OperationRequest {
                network_id: self.resource.space.to_string(),
                operation_id: "operation".into(),
                conversation_id: "chat".into(),
                task_id: Some("task".into()),
                target_node: self.node.node_id()?,
                session_id: "old".into(),
                expected_revision: 0,
                basis_heads: self.store.heads(self.resource.space.as_str(), "chat")?,
                action: self.binding.binding.open_spec.clone(),
                resources: vec![self.cwd.clone()],
                binding: Some(self.binding.clone()),
            };
            let body = json!({"method":"operate","space_id":self.resource.space,"resource_id":self.resource.resource,"authorization_id":"operation","request":request});
            let (auth, proof) = self.authorization(&body)?;
            self.store.accept_authorized_operation(
                &self.node,
                &auth,
                &proof,
                &self.transport(),
                &body,
                &request,
                NOW,
            )?;
            let event=self.store.context(self.resource.space.as_str(),"chat",&[])?.events.into_iter().find(|event|matches!(&event.body,EventBody::DispatchRecorded {operation_id,..} if operation_id=="operation")).context("dispatch missing")?;
            let envelope = self
                .store
                .event(
                    self.resource.space.as_str(),
                    &crate::crypto::digest(&serde_json::to_vec(&event)?),
                )?
                .context("dispatch envelope missing")?;
            self.store.connection.execute("UPDATE operations SET state='\"unknown\"' WHERE space_id=? AND operation_id='operation'",[self.resource.space.as_str()])?;
            Ok((request, envelope))
        }
    }

    #[test]
    fn signed_seal_and_rebind_are_atomic_idempotent_and_fence_the_previous_generation() -> Result<()>
    {
        let mut fixture = ControlFixture::new()?;
        let body = fixture.seal_body("seal");
        let (auth, proof) = fixture.authorization(&body)?;
        let seal = fixture.store.seal_task_authorized(
            &fixture.node,
            &auth,
            &proof,
            &fixture.transport(),
            &body,
            NOW,
        )?;
        assert_eq!(
            fixture.store.seal_task_authorized(
                &fixture.node,
                &auth,
                &proof,
                &fixture.transport(),
                &body,
                NOW
            )?,
            seal
        );
        let rebind = json!({"method":"rebind_execution","space_id":fixture.resource.space,"resource_id":fixture.resource.resource,"conversation_id":"chat","task_id":"task","task_ref":fixture.binding.binding.task_ref,
            "binding":fixture.binding,"previous_seal":seal,"target_node":fixture.node.node_id()?,"session_id":"new","executor":"codex","cwd":fixture.cwd,"authorization_id":"rebind"});
        let (auth, proof) = fixture.authorization(&rebind)?;
        let next = fixture.store.rebind_task_authorized(
            &fixture.node,
            &auth,
            &proof,
            &fixture.transport(),
            &rebind,
            NOW,
        )?;
        assert_eq!(next.binding.generation, 2);
        assert_eq!(next.binding.session_id, "new");
        assert_eq!(
            fixture.store.rebind_task_authorized(
                &fixture.node,
                &auth,
                &proof,
                &fixture.transport(),
                &rebind,
                NOW
            )?,
            next
        );
        assert_eq!(
            fixture.store.task_binding(
                fixture.resource.space.as_str(),
                &fixture.binding.binding.task_ref
            )?,
            Some(next)
        );
        Ok(())
    }

    #[test]
    fn unresolved_operation_blocks_sealing_without_consuming_its_authorization() -> Result<()> {
        let mut fixture = ControlFixture::new()?;
        fixture.operation()?;
        let body = fixture.seal_body("seal-busy");
        let (auth, proof) = fixture.authorization(&body)?;
        assert!(
            fixture
                .store
                .seal_task_authorized(
                    &fixture.node,
                    &auth,
                    &proof,
                    &fixture.transport(),
                    &body,
                    NOW
                )
                .is_err()
        );
        let consumed: bool = fixture.store.connection.query_row(
            "SELECT consumed FROM authorization_challenges WHERE space_id=? AND nonce=?",
            params![
                fixture.resource.space.as_str(),
                proof.challenge.nonce.as_str()
            ],
            |row| row.get(0),
        )?;
        assert!(!consumed);
        assert!(
            fixture
                .store
                .local_value(
                    fixture.resource.space.as_str(),
                    &format!(
                        "task-binding-seal:{}",
                        binding::binding_hash(&fixture.binding.binding)?
                    )
                )?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn resolution_keeps_its_own_admission_and_releases_only_the_unknown_operation() -> Result<()> {
        let mut fixture = ControlFixture::new()?;
        let (request, _) = fixture.operation()?;
        let body = json!({"method":"resolve_unknown","space_id":fixture.resource.space,"resource_id":fixture.resource.resource,"conversation_id":"chat","task_id":"task","task_ref":fixture.binding.binding.task_ref,
            "target_node":fixture.node.node_id()?,"session_id":"old","operation_id":"operation","authorization_id":"resolve-1","outcome":"failed","evidence":"external process stopped and effects reconciled"});
        let (auth, proof) = fixture.authorization(&body)?;
        let resolved = fixture.store.resolve_authorized_operation(
            &fixture.node,
            &auth,
            &proof,
            &fixture.transport(),
            &body,
            NOW,
        )?;
        assert_eq!(resolved.state, OperationState::Failed);
        assert_eq!(
            resolved.result,
            json!({"resolver":fixture.actor,"invocation_hash":auth.invocation_hash(),"evidence":body["evidence"]})
        );
        assert_eq!(
            fixture.store.resolve_authorized_operation(
                &fixture.node,
                &auth,
                &proof,
                &fixture.transport(),
                &body,
                NOW
            )?,
            resolved
        );
        let reservations: u64 = fixture.store.connection.query_row(
            "SELECT count(*) FROM resource_reservations WHERE space_id=? AND operation_id=?",
            params![fixture.resource.space.as_str(), request.operation_id],
            |row| row.get(0),
        )?;
        assert_eq!(reservations, 0);
        let session = fixture
            .store
            .session(
                fixture.resource.space.as_str(),
                &request.target_node,
                &request.session_id,
            )?
            .unwrap();
        assert_eq!(session.revision, 1);
        assert_eq!(session.state, "idle");
        let observation=fixture.store.context(fixture.resource.space.as_str(),"chat",&[])?.events.into_iter().find(|event|matches!(&event.body,EventBody::ExecutionObserved {status,..} if status=="failed")).context("resolution event missing")?;
        let admission = match observation.cause {
            crate::model::EventCause::Invocation { admission } => admission,
            _ => anyhow::bail!("resolution reused the original operation cause"),
        };
        assert_eq!(admission.invocation.hash()?, *auth.invocation_hash());
        assert_eq!(admission.source_body, body);
        Ok(())
    }

    #[test]
    fn resolution_audit_failure_rolls_back_the_ledger_reservation_and_nonce() -> Result<()> {
        let mut fixture = ControlFixture::new()?;
        fixture.operation()?;
        let body = json!({"method":"resolve_unknown","space_id":fixture.resource.space,"resource_id":fixture.resource.resource,"conversation_id":"chat","task_id":"task","task_ref":fixture.binding.binding.task_ref,
            "target_node":fixture.node.node_id()?,"session_id":"old","operation_id":"operation","authorization_id":"resolve-rollback","outcome":"failed","evidence":"external process stopped and effects reconciled"});
        let (auth, proof) = fixture.authorization(&body)?;
        fixture.store.connection.execute_batch("CREATE TRIGGER reject_resolution_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'injected audit failure'); END")?;
        assert!(
            fixture
                .store
                .resolve_authorized_operation(
                    &fixture.node,
                    &auth,
                    &proof,
                    &fixture.transport(),
                    &body,
                    NOW
                )
                .is_err()
        );
        assert_eq!(
            fixture
                .store
                .operation(fixture.resource.space.as_str(), "operation")?
                .unwrap()
                .state,
            OperationState::Unknown
        );
        let reservations:u64=fixture.store.connection.query_row("SELECT count(*) FROM resource_reservations WHERE space_id=? AND operation_id='operation'",[fixture.resource.space.as_str()],|row|row.get(0))?;
        assert_eq!(reservations, 1);
        let accepted:u64=fixture.store.connection.query_row("SELECT count(*) FROM accepted_authorizations WHERE space_id=? AND authorization_id='resolve-rollback'",[fixture.resource.space.as_str()],|row|row.get(0))?;
        assert_eq!(accepted, 0);
        let consumed: bool = fixture.store.connection.query_row(
            "SELECT consumed FROM authorization_challenges WHERE space_id=? AND nonce=?",
            params![
                fixture.resource.space.as_str(),
                proof.challenge.nonce.as_str()
            ],
            |row| row.get(0),
        )?;
        assert!(!consumed);
        Ok(())
    }
}
