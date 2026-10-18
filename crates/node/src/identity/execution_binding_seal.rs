use super::{
    admission::{
        check_policy_heads_in, consume_request_authorization_in, validate_persisted_admission,
    },
    journal::PolicyState,
    policy::{FreshnessProof, RequestTransport},
};
use crate::{
    binding::{self, BindingSeal, SignedSeal},
    crypto::LocalIdentity,
    engine::{ExecutionContext, StoppedExecution},
    model::{AdmissionProof, ExecutionSession, OperationRequest, OperationState},
    store::Store,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use vibeface_secure::authorization::{
    Action, Actor, Audience, Grantee, ObjectHash, PublicKey, RequestFacts, ResourceScope,
    TransportEvidence, VerifiedAuthorization, VerifiedPolicySet,
    execution_binding_seal::{ExecutionBindingSealTarget, execution_binding_seal_facts},
    execution_handoff::SignedExecutionHandoff,
};

pub(crate) enum RemoteExecutionSealPreflight {
    Existing(SignedSeal),
    Fence(ExecutionContext),
}

fn stored_seal_from(
    connection: &Connection,
    target: &ExecutionBindingSealTarget,
) -> Result<Option<(AdmissionProof, SignedSeal)>> {
    let source = &target.session;
    let task = source
        .work
        .task_ref()
        .context("executor seal requires a Task binding")?;
    let raw:Option<(String,String,String)> = connection.query_row(
        "SELECT admission,seal,seal_invocation_hash FROM execution_binding_seals WHERE original_space_id=? AND original_resource_id=? AND task_ref=? AND target_node=? AND generation=?",
        params![source.original_project.space.as_str(),source.original_project.resource.as_str(),task.as_str(),target.binding.binding.target_node,target.binding.binding.generation],
        |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    ).optional()?;
    raw.map(|(admission, seal, hash)| {
        let admission: AdmissionProof = serde_json::from_str(&admission)?;
        let seal: SignedSeal = serde_json::from_str(&seal)?;
        ensure!(
            admission.invocation.hash()?.as_str() == hash
                && seal.seal.network_id == source.original_project.space.as_str()
                && seal.seal.binding_hash == binding::binding_hash(&target.binding.binding)?
                && seal.seal.target_node == target.binding.binding.target_node,
            "stored executor seal scope or invocation changed"
        );
        binding::verify_seal(&seal, target.target_public_key.as_str())?;
        validate_persisted_admission(connection, &admission)?;
        Ok((admission, seal))
    })
    .transpose()
}

pub(super) fn facts_from(
    connection: &Connection,
    policies: &VerifiedPolicySet,
    body: &Value,
    audience: &Audience,
    now: u64,
) -> Result<(RequestFacts, ExecutionBindingSealTarget)> {
    let record = policies.verify_transport_device(&audience.device, now)?;
    let (facts, mut target) =
        execution_binding_seal_facts(body, audience, &record.payload.public_key)?;
    target
        .usage
        .merge(&super::execution_handoff::verify_binding_seal_source_in(
            connection, &target,
        )?);
    target.usage.enforce_limits()?;
    Ok((facts, target))
}

fn authorize_in(
    connection: &Connection,
    policy: &PolicyState,
    identity: &LocalIdentity,
    auth: &VerifiedAuthorization,
    proof: &FreshnessProof,
    transport: &RequestTransport,
    body: &Value,
    now: u64,
) -> Result<ExecutionBindingSealTarget> {
    let (facts, mut target) =
        facts_from(connection, policy.verified(), body, auth.audience(), now)?;
    ensure!(
        identity.public_key_hex()? == target.target_public_key.as_str()
            && matches!(auth.actor(), Actor::Account(_))
            && auth
                .signed_invocation()
                .payload
                .context
                .sdk_session
                .is_none(),
        "executor seal requires its current Account authority"
    );
    let verified = super::policy::authorize_request_invocation_for_body(
        policy.verified(),
        auth.signed_invocation(),
        &facts,
        transport,
        body,
        now,
    )?;
    policy
        .verified()
        .verify_freshness(&verified, &proof.challenge, &proof.receipts, now)?;
    check_policy_heads_in(connection, policy.verified(), &verified, now)?;
    target.usage.record_authorization(&verified)?;
    target.usage.record_metadata(&proof.challenge)?;
    for receipt in &proof.receipts {
        target.usage.record_signed(receipt)?;
    }
    target.usage.enforce_limits()?;
    let leaf = &policy
        .verified()
        .grant(&auth.signed_invocation().payload.grant_leaf)
        .context("explicit executor seal grant disappeared")?
        .payload;
    ensure!(
        leaf.scope == ResourceScope::Exact(facts.resource)
            && leaf.grantee == Grantee::Actor(auth.actor().clone())
            && leaf.remaining_delegations == 0
            && leaf.actions.contains(&facts.action)
            && leaf.actions.iter().all(|action| matches!(
                action,
                Action::ExecutionRebind | Action::OperationStatusRead
            ))
            && leaf.expires_at_ms < u64::MAX
            && leaf.constraints.context == facts.context,
        "executor seal needs a separate exact finite Account rebind grant"
    );
    Ok(target)
}

fn quiescent_in(
    connection: &Connection,
    target: &ExecutionBindingSealTarget,
) -> Result<ExecutionSession> {
    let source = &target.session;
    let task = source
        .work
        .task_ref()
        .context("executor seal requires a Task binding")?;
    let generation = source
        .work
        .binding_generation()
        .context("executor seal requires a Task generation")?;
    let binding_hash = source
        .work
        .binding_hash()
        .context("executor seal requires a Task binding hash")?;
    let mut statement=connection.prepare(
        "SELECT execution_space_id,operation_id,handoff,state,target_node FROM execution_handoffs WHERE original_space_id=?",
    )?;
    let rows = statement.query_map(params![source.original_project.space.as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (execution, operation, wire, state, row_target) = row?;
        let wire: SignedExecutionHandoff = serde_json::from_str(&wire)?;
        let row_source = wire.handoff.source_statement()?;
        if row_source.project != source.original_project
            || wire.handoff.target_public_key != target.target_public_key
        {
            continue;
        }
        ensure!(
            wire.handoff.execution_resource.space.as_str() == execution
                && row_source.operation_id == operation
                && row_target == target.binding.binding.target_node,
            "executor operation projection changed its accepted scope"
        );
        let state: OperationState = serde_json::from_value(Value::String(state))?;
        ensure!(
            state.terminal(),
            "Project has accepted, running or unknown execution; reconcile it before sealing"
        );
        if row_source.work.task_ref() == Some(task)
            && row_source.work.binding_generation() == Some(generation)
        {
            ensure!(
                row_source.work.binding_hash() == Some(binding_hash),
                "executor contains a conflicting task binding generation"
            );
            let locks:bool=connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM execution_handoff_reservations WHERE execution_space_id=? AND original_space_id=? AND operation_id=?)",
                params![execution,source.original_project.space.as_str(),operation],|row|row.get(0))?;
            ensure!(!locks, "terminal execution still has resource reservations");
        }
    }
    let (session, binding, work) = super::execution_handoff::session_from(
        connection,
        target.session.execution_resource.space.as_str(),
        source.original_project.space.as_str(),
        source.original_project.resource.as_str(),
        &target.binding.binding.target_node,
        Some(&target.binding.binding.session_id),
    )?
    .context("executor session is missing")?;
    ensure!(
        binding.as_ref() == Some(&target.binding)
            && work == source.work
            && session.revision == target.session.revision
            && !matches!(session.state.as_str(), "running" | "unknown"),
        "executor session changed or remains uncertain"
    );
    Ok(session)
}

fn context(target: &ExecutionBindingSealTarget, expires: u64) -> ExecutionContext {
    ExecutionContext {
        space_id: target.session.original_project.space.clone(),
        resource_id: target.session.original_project.resource.clone(),
        operation_id: target.receipt.payload.operation_id.clone(),
        shared: true,
        lease_expires_at_ms: expires,
    }
}

fn project_binding_quiescent_in(
    connection: &Connection,
    binding: &crate::binding::SignedBinding,
) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT request,state,operation_id,target_node,session_id FROM operations WHERE space_id=?",
    )?;
    let rows = statement.query_map(params![binding.binding.network_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    for row in rows {
        let (request, state, operation, target, session) = row?;
        let request: OperationRequest = serde_json::from_str(&request)?;
        if request.binding.as_ref() != Some(binding) {
            continue;
        }
        ensure!(
            request.network_id == binding.binding.network_id
                && request.operation_id == operation
                && request.target_node == target
                && request.session_id.as_deref() == Some(binding.binding.session_id.as_str())
                && session.as_deref() == Some(binding.binding.session_id.as_str()),
            "Project operation metadata differs from its retained binding"
        );
        let state: OperationState = serde_json::from_str(&state)?;
        ensure!(
            state.terminal(),
            "old binding has accepted, running or unknown operations; resolve the original operation first"
        );
        crate::store::validate_released_operation_reservations_from(
            connection,
            &request.network_id,
            &request.operation_id,
        )?;
    }
    Ok(())
}

pub(super) fn ensure_handoff_generation_open_in(
    connection: &Connection,
    wire: &SignedExecutionHandoff,
) -> Result<()> {
    let source = wire.handoff.source_statement()?;
    let vibeface_secure::authorization::execution_work::ExecutionWork::TaskSession {
        task,
        binding_generation,
        ..
    } = &source.work
    else {
        return Ok(());
    };
    let target = crate::crypto::fingerprint(wire.handoff.target_public_key.as_str())?;
    let sealed:bool=connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM execution_binding_seals WHERE original_space_id=? AND original_resource_id=? AND task_ref=? AND target_node=? AND generation=?)",
        params![source.project.space.as_str(),source.project.resource.as_str(),task.as_str(),target,binding_generation],|row|row.get(0))?;
    ensure!(!sealed, "execution binding generation is sealed");
    Ok(())
}

impl Store {
    pub(crate) fn rebind_execution_dispatch_authorized(
        &mut self,
        identity: &LocalIdentity,
        dispatch: &super::execution_dispatch::VerifiedDispatchFacts,
        scoped: &super::task_execution_context::TaskScopedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        body: &Value,
        now: u64,
    ) -> Result<crate::binding::SignedBinding> {
        self.rebind_execution_dispatch_request_authorized(
            identity,
            dispatch,
            scoped,
            proof,
            &RequestTransport::Existing(transport.clone()),
            body,
            now,
        )
    }

    pub(crate) fn rebind_execution_dispatch_request_authorized(
        &mut self,
        identity: &LocalIdentity,
        dispatch: &super::execution_dispatch::VerifiedDispatchFacts,
        scoped: &super::task_execution_context::TaskScopedAuthorization,
        proof: &FreshnessProof,
        transport: &RequestTransport,
        body: &Value,
        now: u64,
    ) -> Result<crate::binding::SignedBinding> {
        let _scope = vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let auth = scoped.authorization();
        transport.check_current(auth, now)?;
        ensure!(
            body["method"] == "rebind_execution"
                && auth.action() == Action::ExecutionRebind
                && matches!(auth.actor(), Actor::Account(_))
                && auth
                    .signed_invocation()
                    .payload
                    .context
                    .sdk_session
                    .is_none()
                && auth.body_hash() == &ObjectHash::body(body)?,
            "remote rebind requires a new exact Account rebind intent"
        );
        let old: crate::binding::SignedBinding = serde_json::from_value(body["binding"].clone())?;
        let seal: SignedSeal = serde_json::from_value(body["previous_seal"].clone())?;
        let previous_key: PublicKey =
            serde_json::from_value(body["previous_target_public_key"].clone())?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        binding::verify_binding(&old, local.as_str())?;
        binding::verify_seal(&seal, previous_key.as_str())?;
        let old_hash = binding::binding_hash(&old.binding)?;
        ensure!(
            old.binding.authority_node == identity.node_id()?
                && old.binding.network_id == auth.resource().space.as_str()
                && auth.audience().device
                    == vibeface_secure::authorization::DeviceId::from_key(&local)
                && crate::crypto::fingerprint(previous_key.as_str())? == old.binding.target_node
                && seal.seal.network_id == old.binding.network_id
                && seal.seal.binding_hash == old_hash
                && seal.seal.target_node == old.binding.target_node
                && body["conversation_id"].as_str() == Some(old.binding.conversation_id.as_str())
                && body["task_ref"].as_str() == Some(old.binding.task_ref.as_str())
                && body["task_id"].as_str() == Some(old.binding.task_id.as_str()),
            "remote rebind changed its independent old executor pin or sealed task"
        );
        let cwd = super::execution_dispatch::validate_remote_resource_path(
            body["cwd"]
                .as_str()
                .context("new executor directory is required")?,
        )?;
        let executor = body["executor"]
            .as_str()
            .context("new executor kind is required")?;
        serde_json::from_value::<crate::engine::ExecutorKind>(Value::String(executor.into()))?;
        let target = body["target_node"]
            .as_str()
            .context("new executor target is required")?;
        let session = body["session_id"]
            .as_str()
            .context("new executor session is required")?;
        ensure!(
            target != old.binding.target_node || session != old.binding.session_id,
            "rebind to the same executor needs a new execution session"
        );
        let next = crate::binding::TaskBinding {
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
            target_node: target.into(),
            session_id: session.into(),
            open_spec: serde_json::json!({"type":"open","executor":executor,"cwd":cwd}),
            previous_binding_hash: Some(old_hash.clone()),
            previous_seal: Some(seal),
        };
        let signed = binding::sign_binding(identity, &next)?;
        let hash = binding::binding_hash(&next)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        let facts = dispatch.revalidate(&tx, &current, scoped, body, auth.audience(), now)?;
        super::policy::authorize_request_invocation_for_body(
            scoped.state(current.verified()),
            auth.signed_invocation(),
            &facts,
            transport,
            body,
            now,
        )?;
        let saved: String = tx.query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![
                old.binding.network_id,
                format!("task-binding-record:{old_hash}")
            ],
            |row| row.get(0),
        )?;
        ensure!(
            serde_json::from_str::<crate::binding::SignedBinding>(&saved)? == old,
            "old executor binding differs from its retained Project record"
        );
        let (task, resource, scope, status): (String, String, String, String) = tx.query_row(
            "SELECT envelope,resource_id,scope_id,status FROM events WHERE space_id=? AND hash=?",
            params![old.binding.network_id, old.binding.task_ref],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let task: crate::model::SignedEvent = serde_json::from_str(&task)?;
        let event =
            crate::crypto::verify_signed_event(&old.binding.network_id, &task, local.as_str())?;
        ensure!(
            resource == auth.resource().resource.as_str()
                && scope == old.binding.conversation_id
                && status == "ready"
                && event.resource_id == auth.resource().resource
                && event.scope_id == scope
                && event.author == old.binding.authority_node
                && matches!(event.body,crate::model::EventBody::TaskCreated{ref task_id,..} if task_id==&old.binding.task_id),
            "rebind task differs from its exact ready Project source"
        );
        project_binding_quiescent_in(&tx, &old)?;
        if super::operation_control::accepted_request_before(
            &tx, &current, auth, proof, transport, body, now,
        )? {
            let stored: String = tx.query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![next.network_id, format!("task-binding-record:{hash}")],
                |row| row.get(0),
            )?;
            ensure!(
                serde_json::from_str::<crate::binding::SignedBinding>(&stored)? == signed,
                "rebind retry changed its immutable signed binding"
            );
            tx.commit()?;
            self.policy = current;
            return Ok(signed);
        }
        let key = format!("task-binding-current:{}", next.task_ref);
        let pointer: String = tx.query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![next.network_id, key],
            |row| row.get(0),
        )?;
        ensure!(
            serde_json::from_str::<String>(&pointer)? == old_hash,
            "task binding changed before rebind"
        );
        let admitted = super::admission::consume_execution_request_dispatch_in(
            &tx, &current, scoped, proof, transport, body, None, now, dispatch,
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
                    key,
                    serde_json::to_string(&old_hash)?
                ]
            )? == 1,
            "task binding changed during rebind"
        );
        scoped.bind_in(&tx, body, Some(&signed), admitted.admitted_at_ms)?;
        tx.commit()?;
        self.policy = current;
        Ok(signed)
    }

    pub(crate) fn remote_execution_seal_facts(
        &self,
        body: &Value,
        audience: &Audience,
        now: u64,
    ) -> Result<RequestFacts> {
        facts_from(
            &self.connection,
            self.policy.verified(),
            body,
            audience,
            now,
        )
        .map(|(facts, _)| facts)
    }

    pub(crate) fn authorize_remote_execution_seal_fence(
        &self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        body: &Value,
        now: u64,
    ) -> Result<RemoteExecutionSealPreflight> {
        self.authorize_remote_execution_seal_request_fence(
            identity,
            auth,
            proof,
            &RequestTransport::Existing(transport.clone()),
            body,
            now,
        )
    }

    pub(crate) fn authorize_remote_execution_seal_request_fence(
        &self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &RequestTransport,
        body: &Value,
        now: u64,
    ) -> Result<RemoteExecutionSealPreflight> {
        let _scope = vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let tx = self.connection.unchecked_transaction()?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        let target = authorize_in(&tx, &current, identity, auth, proof, transport, body, now)?;
        ensure!(
            auth.action() == Action::ExecutionRebind,
            "seal fence requires Account rebind permission"
        );
        if let Some((admission, seal)) = stored_seal_from(&tx, &target)? {
            ensure!(
                admission.invocation == *auth.signed_invocation() && admission.source_body == *body,
                "binding was sealed by another Account intent; read its recorded seal"
            );
            tx.commit()?;
            return Ok(RemoteExecutionSealPreflight::Existing(seal));
        }
        quiescent_in(&tx, &target)?;
        tx.commit()?;
        Ok(RemoteExecutionSealPreflight::Fence(context(
            &target,
            auth.expires_at_ms().min(transport.expires_at_ms()),
        )))
    }

    pub(crate) fn seal_remote_execution_authorized(
        &mut self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        body: &Value,
        stopped: &StoppedExecution,
        now: u64,
    ) -> Result<SignedSeal> {
        self.seal_remote_execution_request_authorized(
            identity,
            auth,
            proof,
            &RequestTransport::Existing(transport.clone()),
            body,
            stopped,
            now,
        )
    }

    pub(crate) fn seal_remote_execution_request_authorized(
        &mut self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &RequestTransport,
        body: &Value,
        stopped: &StoppedExecution,
        now: u64,
    ) -> Result<SignedSeal> {
        let _scope = vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        let target = authorize_in(&tx, &current, identity, auth, proof, transport, body, now)?;
        ensure!(
            auth.action() == Action::ExecutionRebind
                && stopped.matches_context(&context(
                    &target,
                    auth.expires_at_ms().min(transport.expires_at_ms())
                )),
            "remote seal has no matching real executor fence"
        );
        if let Some((admission, seal)) = stored_seal_from(&tx, &target)? {
            ensure!(
                admission.invocation == *auth.signed_invocation() && admission.source_body == *body,
                "binding was sealed by another Account intent"
            );
            tx.commit()?;
            return Ok(seal);
        }
        let mut session = quiescent_in(&tx, &target)?;
        let source = &target.session;
        let task = source
            .work
            .task_ref()
            .context("executor seal requires a Task binding")?;
        let generation = i64::try_from(
            source
                .work
                .binding_generation()
                .context("executor seal requires a Task generation")?,
        )
        .context("binding generation exceeds SQLite range")?;
        let seal = binding::sign_seal(
            identity,
            &BindingSeal {
                network_id: source.original_project.space.to_string(),
                binding_hash: binding::binding_hash(&target.binding.binding)?,
                target_node: target.binding.binding.target_node.clone(),
                final_revision: session.revision,
            },
        )?;
        let admission = consume_request_authorization_in(
            &tx, &current, auth, proof, transport, body, None, now,
        )?;
        session.state = "closed".into();
        session.native_session_id = None;
        ensure!(tx.execute(
            "UPDATE execution_handoff_sessions SET record=? WHERE execution_space_id=? AND original_space_id=? AND original_resource_id=? AND target_node=? AND session_id=? AND revision=?",
            params![super::execution_handoff::session_record(&session, &source.work)?,target.session.execution_resource.space.as_str(),source.original_project.space.as_str(),source.original_project.resource.as_str(),session.target_node,session.session_id,session.revision],
        )?==1,"executor session changed before seal");
        tx.execute("INSERT INTO execution_binding_seals(original_space_id,original_resource_id,task_ref,target_node,generation,execution_space_id,execution_resource_id,binding_hash,seal_invocation_hash,admission,seal) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            params![source.original_project.space.as_str(),source.original_project.resource.as_str(),task.as_str(),target.binding.binding.target_node,generation,target.session.execution_resource.space.as_str(),target.session.execution_resource.resource.as_str(),seal.seal.binding_hash,auth.signed_invocation().hash()?.as_str(),serde_json::to_string(&admission)?,serde_json::to_string(&seal)?])?;
        tx.commit()?;
        self.policy = current;
        Ok(seal)
    }

    pub(crate) fn remote_execution_seal_status_authorized(
        &self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        body: &Value,
        now: u64,
    ) -> Result<Option<SignedSeal>> {
        self.remote_execution_seal_status_request_authorized(
            identity,
            auth,
            proof,
            &RequestTransport::Existing(transport.clone()),
            body,
            now,
        )
    }

    pub(crate) fn remote_execution_seal_status_request_authorized(
        &self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &RequestTransport,
        body: &Value,
        now: u64,
    ) -> Result<Option<SignedSeal>> {
        let _scope = vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let tx = self.connection.unchecked_transaction()?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        let target = authorize_in(&tx, &current, identity, auth, proof, transport, body, now)?;
        ensure!(
            auth.action() == Action::OperationStatusRead,
            "seal status requires current read permission"
        );
        let expected = ObjectHash::parse(
            body["seal_invocation_hash"]
                .as_str()
                .context("seal invocation hash is required")?,
        )?;
        let result = stored_seal_from(&tx, &target)?
            .map(|(admission, seal)| {
                ensure!(
                    admission.invocation.hash()? == expected,
                    "seal status belongs to another original invocation"
                );
                Ok(seal)
            })
            .transpose()?;
        tx.commit()?;
        Ok(result)
    }
}
