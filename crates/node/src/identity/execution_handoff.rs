use super::{admission::check_policy_heads_in, journal::PolicyState};
use crate::{
    crypto::LocalIdentity,
    engine::{EngineAction, ExecutorKind},
    model::{Event, EventBody, ExecutionSession, OperationRequest, OperationState},
    store::{
        Store, StoredEvent, canonical_resource, check_execution_resources_in,
        check_execution_session_in, load_events_from,
    },
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use vibeface_secure::authorization::{
    AUTH_VERSION, CheckStage, DeviceId, FreshnessReceipt, Nonce, ObjectHash, PolicyDomain,
    PublicKey, ResourceId, ResourceRef, ServiceId, Signed, SpaceId, TransportEvidence,
    VerifiedAuthorization, VerifiedPolicySet, events::StoredTransport, execution_handoff::*,
    task_binding::SignedBinding,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContext {
    pub space_id: SpaceId,
    pub resource_id: ResourceId,
    pub execution_space_id: SpaceId,
    pub execution_service_resource: ResourceId,
    pub stable_original_operation_id: String,
    pub shared: bool,
    pub lease_expires_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedExecutionHandoff {
    pub original_request: OperationRequest,
    pub execution_context: ExecutionContext,
    pub execution_session: Option<ExecutionSession>,
    pub state: OperationState,
    pub result: Value,
    pub receipt: Option<Signed<ExecutionHandoffReceipt>>,
}

struct StoredHandoff {
    wire: SignedExecutionHandoff,
    request: OperationRequest,
    transport: TransportEvidence,
    admission: ExecutionHandoffFreshness,
    admitted_at_ms: u64,
    state: OperationState,
    result: Value,
    lease: Option<(ExecutionHandoffFreshness, u64, u64)>,
    receipt: Option<Signed<ExecutionHandoffReceipt>>,
}

fn scope(wire: &SignedExecutionHandoff) -> Result<(String, String, String)> {
    let dispatch: Event = serde_json::from_str(&wire.handoff.project_dispatch.canonical_body)?;
    let EventBody::DispatchRecorded { operation_id, .. } = dispatch.body else {
        anyhow::bail!("handoff source must be an admitted dispatch")
    };
    Ok((
        wire.handoff.execution_resource.space.to_string(),
        dispatch.network_id,
        operation_id,
    ))
}

fn request(wire: &SignedExecutionHandoff) -> Result<OperationRequest> {
    let event: Event = serde_json::from_str(&wire.handoff.project_dispatch.canonical_body)?;
    let EventBody::DispatchRecorded { action, .. } = event.body else {
        anyhow::bail!("handoff source must be an admitted dispatch")
    };
    Ok(serde_json::from_value(action)?)
}

fn original_authorization_from(
    _connection: &Connection,
    policy: &PolicyState,
    current: &VerifiedPolicySet,
    wire: &SignedExecutionHandoff,
    now_ms: u64,
) -> Result<VerifiedAuthorization> {
    wire.hash()?;
    current.verify_project_execution_source(&wire.handoff, now_ms)?;
    let dispatch: Event = serde_json::from_str(&wire.handoff.project_dispatch.canonical_body)?;
    let task: Event = serde_json::from_str(&wire.handoff.task_creation.canonical_body)?;
    let original = capsule_admission(&dispatch)?;
    let task_proof = capsule_admission(&task)?;
    ensure!(
        task_proof.invocation.payload.audience == original.invocation.payload.audience
            && dispatch.created_at_ms == original.admitted_at_ms as i64
            && original.admitted_at_ms >= task_proof.admitted_at_ms,
        "capsule task host or admission order changed"
    );
    let project = original.invocation.payload.resource.clone();
    let task_facts = capsule_facts(
        &task_proof,
        &project,
        &task.scope_id,
        None,
        wire.handoff.task_sdk_origin.as_ref(),
    )?;
    let crate::model::EventBody::TaskCreated {
        task_id,
        title,
        goal,
        acceptance,
    } = &task.body
    else {
        anyhow::bail!("capsule task proof is not a creation")
    };
    ensure!(
        task_proof.source_body["method"] == "create_task"
            && task_proof.source_body["task_id"].as_str() == Some(task_id.as_str())
            && task_proof.source_body["title"].as_str() == Some(title.as_str())
            && task_proof.source_body["goal"].as_str() == Some(goal.as_str())
            && task_proof.source_body["acceptance"].as_str() == Some(acceptance.as_str())
            && task.created_at_ms == task_proof.admitted_at_ms as i64,
        "task creation differs from its admitted input"
    );
    verify_capsule_admission(
        policy,
        &task_proof,
        &task_facts,
        wire.handoff.task_sdk_origin.as_ref(),
    )?;
    let operation = request(wire)?;
    let facts = capsule_facts(
        &original,
        &project,
        &dispatch.scope_id,
        Some((&operation, &wire.handoff)),
        wire.handoff.sdk_origin.as_ref(),
    )?;
    let admitted =
        verify_capsule_admission(policy, &original, &facts, wire.handoff.sdk_origin.as_ref())?;
    let auth = current.verify_operation_authorization(
        &admitted,
        &facts,
        &original.transport.clone().into_evidence(),
        now_ms,
    )?;
    verify_capsule_sdk(
        current,
        policy,
        &original,
        &facts,
        wire.handoff.sdk_origin.as_ref(),
        now_ms,
    )?;
    Ok(auth)
}

fn capsule_admission(event: &Event) -> Result<crate::model::AdmissionProof> {
    let crate::model::EventCause::Invocation { admission } = &event.cause else {
        anyhow::bail!("capsule must preserve an original invocation admission")
    };
    Ok(admission.clone())
}

fn capsule_facts(
    proof: &crate::model::AdmissionProof,
    project: &ResourceRef,
    conversation: &str,
    operation: Option<(&OperationRequest, &ExecutionHandoff)>,
    origin: Option<&CompactSdkOrigin>,
) -> Result<vibeface_secure::authorization::RequestFacts> {
    use vibeface_secure::authorization::{
        Actor, EventRef, InvocationContext, RequestFacts, SessionRef,
    };
    let body = &proof.source_body;
    ensure!(
        body["space_id"].as_str() == Some(project.space.as_str())
            && body["resource_id"].as_str() == Some(project.resource.as_str()),
        "capsule request belongs to another Project"
    );
    let mut context = InvocationContext {
        conversation: Some(conversation.to_owned()),
        workspace_resource: Some(project.resource.clone()),
        ..Default::default()
    };
    if let Some((request, handoff)) = operation {
        ensure!(
            request.conversation_id == conversation
                && request.network_id == project.space.as_str()
                && body["method"] == "operate",
            "capsule operation scope changed"
        );
        let binding = request
            .binding
            .as_ref()
            .context("capsule operation has no binding")?;
        context.task = Some(EventRef::parse(&handoff.task_creation.hash)?);
        context.execution_session = Some(SessionRef {
            target: handoff.execution_audience.device.clone(),
            session_id: request.session_id.clone(),
        });
        context.executor_kind = binding.binding.open_spec["executor"]
            .as_str()
            .map(str::to_owned);
        ensure!(
            request.action["type"] != "open" || request.action == binding.binding.open_spec,
            "capsule open input differs from its binding"
        );
    } else {
        ensure!(
            body["conversation_id"].as_str() == Some(conversation),
            "capsule task conversation changed"
        );
    }
    if let Some(origin) = origin {
        let Actor::Service(service) = &proof.invocation.payload.actor else {
            anyhow::bail!("an Account cannot claim an SDK source")
        };
        let session: SessionRef = serde_json::from_value(body["sdk_session"].clone())?;
        ensure!(
            session.target == proof.invocation.payload.device
                && session.session_id == origin.message_event.as_str()
                && body["parent_invocation"].as_str() == Some(origin.invocation.hash()?.as_str())
                && proof.transport.clone().into_evidence()
                    == TransportEvidence::LocalAgentAdapter {
                        service: service.clone(),
                        session: session.clone()
                    },
            "SDK capsule source differs from its original Account invocation"
        );
        context.sdk_session = Some(session);
    } else {
        ensure!(
            body["sdk_session"].is_null()
                && body["parent_invocation"].is_null()
                && matches!(proof.invocation.payload.actor, Actor::Account(_)),
            "capsule is missing its SDK Account source proof"
        );
    }
    Ok(RequestFacts {
        resource: project.clone(),
        action: crate::model::authorization_action(body)?,
        audience: proof.invocation.payload.audience.clone(),
        operation_id: body["authorization_id"]
            .as_str()
            .context("capsule authorization ID is missing")?
            .to_owned(),
        body_hash: ObjectHash::body(body)?,
        context,
    })
}

fn compact_parent(
    policy: &PolicyState,
    origin: &CompactSdkOrigin,
    child: &crate::model::AdmissionProof,
    facts: &vibeface_secure::authorization::RequestFacts,
) -> Result<(
    vibeface_secure::authorization::VerifiedAdmission,
    vibeface_secure::authorization::RequestFacts,
)> {
    use vibeface_secure::authorization::{Action, Actor, RequestFacts};
    let invocation = &origin.invocation.payload;
    ensure!(
        origin.challenge.stage == CheckStage::Admit
            && matches!(invocation.actor, Actor::Account(_))
            && invocation.initiator == invocation.actor
            && invocation.initiating_grant.is_none()
            && invocation.action == Action::MessageAppend
            && invocation.resource == facts.resource
            && invocation.audience == facts.audience
            && invocation.context.conversation == facts.context.conversation
            && invocation.context.workspace_resource == facts.context.workspace_resource
            && invocation.context.sdk_session.is_none()
            && invocation.context.execution_session.is_none()
            && origin.admitted_at_ms <= child.admitted_at_ms,
        "invalid opaque Account message origin"
    );
    let parent_facts = RequestFacts {
        resource: facts.resource.clone(),
        action: Action::MessageAppend,
        audience: facts.audience.clone(),
        operation_id: invocation.operation_id.clone(),
        body_hash: invocation.body_hash.clone(),
        context: invocation.context.clone(),
    };
    let historical = policy.historical(&origin.challenge.policies, origin.admitted_at_ms)?;
    let auth = historical.verify_invocation(
        &origin.invocation,
        &parent_facts,
        &origin.transport.clone().into_evidence(),
        origin.admitted_at_ms,
    )?;
    let fresh = historical.verify_freshness(
        &auth,
        &origin.challenge,
        &origin.receipts,
        origin.admitted_at_ms,
    )?;
    Ok((
        historical.admit_operation(&auth, &fresh, origin.admitted_at_ms)?,
        parent_facts,
    ))
}

fn verify_capsule_sdk(
    state: &VerifiedPolicySet,
    policy: &PolicyState,
    proof: &crate::model::AdmissionProof,
    facts: &vibeface_secure::authorization::RequestFacts,
    origin: Option<&CompactSdkOrigin>,
    now_ms: u64,
) -> Result<()> {
    use vibeface_secure::authorization::Actor;
    let Some(origin) = origin else { return Ok(()) };
    let Actor::Service(service) = &proof.invocation.payload.actor else {
        anyhow::bail!("SDK capsule is not a Service request")
    };
    let (parent, parent_facts) = compact_parent(policy, origin, proof, facts)?;
    let parent = state.verify_operation_authorization(
        &parent,
        &parent_facts,
        &origin.transport.clone().into_evidence(),
        now_ms,
    )?;
    let delegated = state.verify_service_delegation(
        &parent,
        service,
        &proof.invocation.payload.grant_leaf,
        facts,
        now_ms,
    )?;
    ensure!(
        delegated.device() == &proof.invocation.payload.device
            && delegated.initiator() == &proof.invocation.payload.initiator
            && delegated.initiating_grant() == proof.invocation.payload.initiating_grant.as_ref(),
        "capsule SDK delegation changed"
    );
    Ok(())
}

fn verify_capsule_admission(
    policy: &PolicyState,
    proof: &crate::model::AdmissionProof,
    facts: &vibeface_secure::authorization::RequestFacts,
    origin: Option<&CompactSdkOrigin>,
) -> Result<vibeface_secure::authorization::VerifiedAdmission> {
    let historical = policy.historical(&proof.challenge.policies, proof.admitted_at_ms)?;
    let admitted = super::admission::reconstruct_admission(
        policy,
        proof,
        facts,
        &proof.transport.clone().into_evidence(),
    )?;
    verify_capsule_sdk(
        &historical,
        policy,
        proof,
        facts,
        origin,
        proof.admitted_at_ms,
    )?;
    Ok(admitted)
}

fn compact_sdk_from_events(
    events: &BTreeMap<String, StoredEvent>,
    proof: &crate::model::AdmissionProof,
) -> Result<Option<CompactSdkOrigin>> {
    if proof.source_body["sdk_session"].is_null() {
        return Ok(None);
    }
    let session: vibeface_secure::authorization::SessionRef =
        serde_json::from_value(proof.source_body["sdk_session"].clone())?;
    let parent = events
        .get(&session.session_id)
        .context("SDK source message is missing")?;
    let original = capsule_admission(&parent.event)?;
    ensure!(
        matches!(&parent.event.body,EventBody::MessagePosted {role,..} if role=="user")
            && matches!(
                original.invocation.payload.actor,
                vibeface_secure::authorization::Actor::Account(_)
            )
            && proof.source_body["parent_invocation"].as_str()
                == Some(original.invocation.hash()?.as_str()),
        "SDK source is not the original Account message"
    );
    Ok(Some(CompactSdkOrigin {
        message_event: vibeface_secure::authorization::EventRef::parse(&parent.envelope.hash)?,
        invocation: original.invocation,
        challenge: original.challenge,
        receipts: original.receipts,
        admitted_at_ms: original.admitted_at_ms,
        transport: original.transport,
    }))
}

fn reconstruct_handoff(
    connection: &Connection,
    policy: &PolicyState,
    stored: &StoredHandoff,
) -> Result<VerifiedExecutionAdmission> {
    verify_challenge_record(connection, &stored.wire, &stored.admission, true)?;
    let (execution, original, operation) = scope(&stored.wire)?;
    let nonce:(String,String,String,String)=connection.query_row("SELECT device_id,nonce,invocation_hash,body_hash FROM execution_handoff_nonces WHERE execution_space_id=? AND original_space_id=? AND operation_id=?",
        params![execution,original,operation],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
    ensure!(
        nonce
            == (
                stored.wire.invocation.payload.device.to_string(),
                stored.wire.invocation.payload.nonce.to_string(),
                stored.wire.invocation.hash()?.to_string(),
                stored.wire.invocation.payload.body_hash.to_string()
            ),
        "handoff nonce ledger changed"
    );
    let historical =
        policy.historical(&stored.admission.challenge.policies, stored.admitted_at_ms)?;
    let original = original_authorization_from(
        connection,
        policy,
        &historical,
        &stored.wire,
        stored.admitted_at_ms,
    )?;
    let verified = historical.verify_execution_handoff(
        &stored.wire,
        &original,
        &stored.transport,
        None,
        stored.admitted_at_ms,
    )?;
    let freshness = historical.verify_execution_handoff_freshness(
        &verified,
        &stored.admission,
        stored.admitted_at_ms,
    )?;
    Ok(historical.admit_execution_handoff(&verified, &freshness, stored.admitted_at_ms)?)
}

fn verified_lease_deadline(
    connection: &Connection,
    policy: &PolicyState,
    stored: &StoredHandoff,
) -> Result<u64> {
    let (lease, at, deadline) = stored
        .lease
        .as_ref()
        .context("handoff has no execution lease")?;
    ensure!(
        *at >= stored.admitted_at_ms
            && matches!(lease.challenge.stage, CheckStage::Start | CheckStage::Renew),
        "invalid persisted handoff lease stage or time"
    );
    let admission = reconstruct_handoff(connection, policy, stored)?;
    let historical = policy.historical(&lease.challenge.policies, *at)?;
    let original = original_authorization_from(connection, policy, &historical, &stored.wire, *at)?;
    let auth = historical.verify_execution_handoff(
        &stored.wire,
        &original,
        &stored.transport,
        Some(&admission),
        *at,
    )?;
    let freshness = historical.verify_execution_handoff_freshness(&auth, lease, *at)?;
    verify_challenge_record(connection, &stored.wire, lease, true)?;
    ensure!(
        freshness.expires_at_ms() == *deadline,
        "persisted handoff lease deadline changed"
    );
    Ok(*deadline)
}

fn stored_from(
    connection: &Connection,
    execution: &str,
    original: &str,
    operation: &str,
) -> Result<Option<StoredHandoff>> {
    struct Raw {
        wire: String,
        request: String,
        transport: String,
        challenge: String,
        receipts: String,
        admitted: u64,
        state: String,
        result: String,
        lease_challenge: Option<String>,
        lease_receipts: Option<String>,
        lease_verified: Option<u64>,
        lease_deadline: Option<u64>,
        receipt: Option<String>,
        handoff_hash: String,
        invocation_hash: String,
        body_hash: String,
        target: String,
        session: String,
        control: bool,
    }
    let raw = connection.query_row(
        "SELECT handoff,original_request,admission_transport,admission_challenge,admission_receipts,admitted_at_ms,state,result,lease_challenge,lease_receipts,lease_verified_at_ms,lease_deadline_ms,result_receipt,handoff_hash,invocation_hash,body_hash,target_node,session_id,control FROM execution_handoffs WHERE execution_space_id=? AND original_space_id=? AND operation_id=?",
        params![execution, original, operation], |row| Ok(Raw {
            wire: row.get(0)?, request: row.get(1)?, transport: row.get(2)?, challenge: row.get(3)?, receipts: row.get(4)?,
            admitted: row.get(5)?, state: row.get(6)?, result: row.get(7)?, lease_challenge: row.get(8)?, lease_receipts: row.get(9)?,
            lease_verified: row.get(10)?, lease_deadline: row.get(11)?, receipt: row.get(12)?, handoff_hash: row.get(13)?,
            invocation_hash: row.get(14)?, body_hash: row.get(15)?, target: row.get(16)?, session: row.get(17)?, control: row.get(18)?,
        })).optional()?;
    let Some(raw) = raw else { return Ok(None) };
    let wire: SignedExecutionHandoff = serde_json::from_str(&raw.wire)?;
    let operation_request: OperationRequest = serde_json::from_str(&raw.request)?;
    ensure!(
        scope(&wire)?
            == (
                execution.to_owned(),
                original.to_owned(),
                operation.to_owned()
            )
            && request(&wire)? == operation_request
            && operation_request.network_id == original
            && operation_request.operation_id == operation
            && operation_request.target_node == raw.target
            && operation_request.session_id == raw.session
            && wire.hash()?.as_str() == raw.handoff_hash
            && wire.invocation.hash()?.as_str() == raw.invocation_hash
            && wire.invocation.payload.body_hash.as_str() == raw.body_hash
            && raw.control == is_control(&operation_request),
        "stored execution handoff scope or request changed"
    );
    let lease = match (
        raw.lease_challenge,
        raw.lease_receipts,
        raw.lease_verified,
        raw.lease_deadline,
    ) {
        (None, None, None, None) => None,
        (Some(challenge), Some(receipts), Some(verified), Some(deadline)) => Some((
            ExecutionHandoffFreshness {
                challenge: serde_json::from_str(&challenge)?,
                receipts: serde_json::from_str(&receipts)?,
            },
            verified,
            deadline,
        )),
        _ => anyhow::bail!("stored handoff lease is incomplete"),
    };
    let stored = StoredHandoff {
        wire,
        request: operation_request,
        transport: serde_json::from_str::<StoredTransport>(&raw.transport)?.into_evidence(),
        admission: ExecutionHandoffFreshness {
            challenge: serde_json::from_str(&raw.challenge)?,
            receipts: serde_json::from_str(&raw.receipts)?,
        },
        admitted_at_ms: raw.admitted,
        state: serde_json::from_value(Value::String(raw.state))?,
        result: serde_json::from_str(&raw.result)?,
        lease,
        receipt: raw
            .receipt
            .map(|receipt| serde_json::from_str(&receipt))
            .transpose()?,
    };
    ensure!(
        stored.state != OperationState::Accepted || stored.lease.is_none(),
        "accepted handoff already has an execution lease"
    );
    ensure!(
        !matches!(
            stored.state,
            OperationState::Running | OperationState::Unknown
        ) || stored.lease.is_some(),
        "started handoff has no execution lease"
    );
    if let Some(receipt) = &stored.receipt {
        verify_execution_handoff_receipt_signature(&stored.wire, receipt, &stored.result)?;
        ensure!(
            serde_json::to_value(receipt.payload.state)? == serde_json::to_value(&stored.state)?
                && receipt.payload.issued_at_ms >= stored.admitted_at_ms,
            "stored handoff result state changed"
        );
    } else {
        ensure!(
            !stored.state.terminal(),
            "terminal handoff has no signed result receipt"
        );
    }
    Ok(Some(stored))
}

fn is_control(request: &OperationRequest) -> bool {
    !matches!(request.action["type"].as_str(), Some("open" | "send"))
}

fn state_text(state: &OperationState) -> Result<String> {
    Ok(serde_json::to_value(state)?
        .as_str()
        .context("invalid operation state")?
        .to_owned())
}

fn verify_challenge_record(
    connection: &Connection,
    wire: &SignedExecutionHandoff,
    proof: &ExecutionHandoffFreshness,
    consumed: bool,
) -> Result<()> {
    let (execution, original, operation) = scope(wire)?;
    let row: (String, String, String, String, u64, bool, String) = connection.query_row(
        "SELECT original_space_id,operation_id,handoff_hash,challenge,expires_at_ms,consumed,stage FROM execution_handoff_challenges WHERE execution_space_id=? AND nonce=?",
        params![execution, proof.challenge.nonce.as_str()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
    ).context("handoff challenge was not issued by this executor")?;
    ensure!(
        row.0 == original
            && row.1 == operation
            && row.2 == wire.hash()?.as_str()
            && serde_json::from_str::<ExecutionHandoffChallenge>(&row.3)? == proof.challenge
            && row.4 == proof.challenge.expires_at_ms
            && row.5 == consumed
            && serde_json::from_value::<CheckStage>(Value::String(row.6))? == proof.challenge.stage,
        "execution handoff challenge changed or was consumed"
    );
    Ok(())
}

fn consume_freshness_in(
    tx: &Transaction<'_>,
    policy: &PolicyState,
    handoff: &VerifiedExecutionHandoff,
    proof: &ExecutionHandoffFreshness,
    stage: CheckStage,
    now_ms: u64,
) -> Result<u64> {
    ensure!(
        proof.challenge.stage == stage,
        "incorrect execution handoff stage"
    );
    check_policy_heads_in(tx, policy.verified(), handoff.original(), now_ms)?;
    check_policy_heads_in(tx, policy.verified(), handoff.submission(), now_ms)?;
    let freshness = policy
        .verified()
        .verify_execution_handoff_freshness(handoff, proof, now_ms)?;
    verify_challenge_record(tx, handoff.wire(), proof, false)?;
    ensure!(tx.execute("UPDATE execution_handoff_challenges SET consumed=1 WHERE execution_space_id=? AND nonce=? AND consumed=0",
        params![handoff.wire().handoff.execution_resource.space.as_str(), proof.challenge.nonce.as_str()])? == 1,
        "execution handoff challenge was already consumed");
    Ok(freshness.expires_at_ms())
}

fn session_from(
    connection: &Connection,
    execution: &str,
    original: &str,
    project_resource: &str,
    target: &str,
    session: &str,
) -> Result<Option<(ExecutionSession, SignedBinding)>> {
    let row: Option<(String, String, u64)> = connection.query_row(
        "SELECT record,binding,revision FROM execution_handoff_sessions WHERE execution_space_id=? AND original_space_id=? AND original_resource_id=? AND target_node=? AND session_id=?",
        params![execution, original, project_resource, target, session], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
    let Some((record, binding, revision)) = row else {
        return Ok(None);
    };
    let record: ExecutionSession = serde_json::from_str(&record)?;
    let binding: SignedBinding = serde_json::from_str(&binding)?;
    ensure!(
        record.network_id == execution
            && record.target_node == target
            && record.session_id == session
            && record.revision == revision
            && binding.binding.network_id == original
            && binding.binding.target_node == target
            && binding.binding.session_id == session,
        "executor session projection differs from its binding"
    );
    Ok(Some((record, binding)))
}

fn validate_execution_context(
    connection: &Connection,
    identity: &LocalIdentity,
    handoff: &VerifiedExecutionHandoff,
    operation: &OperationRequest,
) -> Result<(Vec<String>, ExecutionSession)> {
    let execution = &handoff.wire().handoff.execution_resource;
    ensure!(
        identity.public_key_hex()? == handoff.wire().handoff.target_public_key.as_str()
            && identity.node_id()? == operation.target_node,
        "handoff targets another executor"
    );
    let root: String = connection
        .query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![
                execution.space.as_str(),
                format!("resource-root:{}", execution.resource)
            ],
            |row| row.get(0),
        )
        .context("execution resource has no local mount mapping")?;
    let root: String = serde_json::from_str(&root)?;
    ensure!(
        canonical_resource(&root)? == root && std::path::Path::new(&root).is_dir(),
        "execution mount mapping is not canonical"
    );
    let binding = handoff.binding();
    let cwd = binding.binding.open_spec["cwd"]
        .as_str()
        .context("binding has no execution directory")?;
    ensure!(
        canonical_resource(cwd)? == cwd
            && std::path::Path::new(cwd).is_dir()
            && std::path::Path::new(cwd).starts_with(&root),
        "signed execution directory is outside the executor mount"
    );
    let resources = operation
        .resources
        .iter()
        .map(|resource| {
            let canonical = canonical_resource(resource)?;
            ensure!(
                &canonical == resource && std::path::Path::new(resource).starts_with(&root),
                "handoff resource is outside the executor mount"
            );
            Ok(canonical)
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        resources.len() <= 128,
        "too many execution resource reservations"
    );
    let existing = session_from(
        connection,
        execution.space.as_str(),
        &operation.network_id,
        handoff.original().resource().resource.as_str(),
        &operation.target_node,
        &operation.session_id,
    )?;
    let session = if operation.action["type"] == "open" {
        ensure!(
            existing.is_none()
                && operation.expected_revision == 0
                && operation.action == binding.binding.open_spec,
            "execution open conflicts with an existing session or binding"
        );
        let executor: ExecutorKind = serde_json::from_value(operation.action["executor"].clone())?;
        ExecutionSession {
            network_id: execution.space.to_string(),
            target_node: operation.target_node.clone(),
            session_id: operation.session_id.clone(),
            revision: 0,
            state: "opening".into(),
            executor: serde_json::to_value(executor)?
                .as_str()
                .context("invalid executor")?
                .to_owned(),
            cwd: cwd.to_owned(),
            native_session_id: None,
        }
    } else {
        serde_json::from_value::<EngineAction>(operation.action.clone())?.validate()?;
        let (session, previous_binding) = existing.context("executor session is not open")?;
        ensure!(
            previous_binding == *binding
                && session.revision == operation.expected_revision
                && session.cwd == cwd
                && binding.binding.open_spec["executor"].as_str()
                    == Some(session.executor.as_str()),
            "executor session revision or binding changed"
        );
        session
    };
    if !is_control(operation) {
        ensure!(
            resources.iter().any(|resource| resource == cwd),
            "execution directory must be reserved"
        );
    }
    Ok((resources, session))
}

fn signed_receipt(
    identity: &LocalIdentity,
    stored: &StoredHandoff,
    now_ms: u64,
) -> Result<Signed<ExecutionHandoffReceipt>> {
    ensure!(
        identity.public_key_hex()? == stored.wire.handoff.target_public_key.as_str(),
        "receipt signer is not this executor"
    );
    let binding = stored
        .request
        .binding
        .as_ref()
        .context("handoff binding is missing")?;
    let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
        .map_err(anyhow::Error::msg)?;
    Ok(Signed::sign(
        ExecutionHandoffReceipt {
            version: AUTH_VERSION,
            operation_id: stored.request.operation_id.clone(),
            original_space_id: SpaceId::parse(&stored.request.network_id)?,
            execution_space_id: stored.wire.handoff.execution_resource.space.clone(),
            handoff_hash: stored.wire.hash()?,
            target: stored.wire.handoff.execution_audience.device.clone(),
            session_id: stored.request.session_id.clone(),
            binding_generation: binding.binding.generation,
            state: serde_json::from_value(serde_json::to_value(&stored.state)?)?,
            result_hash: ObjectHash::body(&stored.result)?,
            issued_at_ms: now_ms,
        },
        &signer,
    )?)
}

fn dto(
    connection: &Connection,
    identity: &LocalIdentity,
    stored: &StoredHandoff,
    now_ms: u64,
) -> Result<AdmittedExecutionHandoff> {
    let execution = &stored.wire.handoff.execution_resource;
    let dispatch: Event =
        serde_json::from_str(&stored.wire.handoff.project_dispatch.canonical_body)?;
    Ok(AdmittedExecutionHandoff {
        original_request: stored.request.clone(),
        execution_context: ExecutionContext {
            space_id: SpaceId::parse(&stored.request.network_id)?,
            resource_id: dispatch.resource_id.clone(),
            execution_space_id: execution.space.clone(),
            execution_service_resource: execution.resource.clone(),
            stable_original_operation_id: stored.request.operation_id.clone(),
            shared: true,
            lease_expires_at_ms: stored.lease.as_ref().map(|lease| lease.2),
        },
        execution_session: session_from(
            connection,
            execution.space.as_str(),
            &stored.request.network_id,
            dispatch.resource_id.as_str(),
            &stored.request.target_node,
            &stored.request.session_id,
        )?
        .map(|session| session.0),
        state: stored.state.clone(),
        result: stored.result.clone(),
        receipt: Some(match &stored.receipt {
            Some(receipt) => receipt.clone(),
            None => signed_receipt(identity, stored, now_ms)?,
        }),
    })
}

impl Store {
    pub fn execution_handoff_payload(
        &self,
        identity: &LocalIdentity,
        original_space: &str,
        operation_id: &str,
        execution_resource: ResourceRef,
        execution_audience: vibeface_secure::authorization::Audience,
        target_public_key: PublicKey,
        now_ms: u64,
    ) -> Result<ExecutionHandoff> {
        let events = load_events_from(&self.connection, original_space)?;
        let mut dispatch = None;
        for event in events.values() {
            if matches!(&event.event.body, EventBody::DispatchRecorded { operation_id: candidate, .. } if candidate == operation_id)
            {
                ensure!(
                    dispatch.replace(event).is_none(),
                    "operation has conflicting admitted dispatches"
                );
            }
        }
        let dispatch = dispatch.context("operation has no admitted dispatch")?;
        super::event_authorization::current_event_authorization(
            &self.policy,
            &self.connection,
            &dispatch.event,
            &events,
            now_ms,
        )?;
        let original = capsule_admission(&dispatch.event)?;
        let operation: OperationRequest =
            serde_json::from_value(original.source_body["request"].clone())?;
        let binding = operation
            .binding
            .as_ref()
            .context("operation has no task binding")?;
        let task = events
            .get(&binding.binding.task_ref)
            .context("operation task creation is missing")?;
        let task_proof = capsule_admission(&task.event)?;
        let sdk_origin = compact_sdk_from_events(&events, &original)?;
        let task_sdk_origin = compact_sdk_from_events(&events, &task_proof)?;
        ensure!(
            identity.node_id()? == dispatch.event.author,
            "source capsule signer is not the Project host"
        );
        let mut handoff = ExecutionHandoff {
            version: AUTH_VERSION,
            project_dispatch: dispatch.envelope.clone(),
            task_creation: task.envelope.clone(),
            sdk_origin,
            task_sdk_origin,
            execution_resource,
            execution_audience,
            target_public_key,
            source_attestation: None,
        };
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        handoff.attest_source(&signer)?;
        Ok(handoff)
    }

    pub fn execution_handoff_authorization(
        &self,
        execution_space: &str,
        original_space: &str,
        operation_id: &str,
        now_ms: u64,
    ) -> Result<VerifiedExecutionHandoff> {
        let stored = stored_from(
            &self.connection,
            execution_space,
            original_space,
            operation_id,
        )?
        .context("unknown execution handoff")?;
        let admission = reconstruct_handoff(&self.connection, &self.policy, &stored)?;
        let original = original_authorization_from(
            &self.connection,
            &self.policy,
            self.policy.verified(),
            &stored.wire,
            now_ms,
        )?;
        Ok(self.policy.verified().verify_execution_handoff(
            &stored.wire,
            &original,
            &stored.transport,
            Some(&admission),
            now_ms,
        )?)
    }

    pub fn issue_execution_handoff_challenge(
        &mut self,
        wire: &SignedExecutionHandoff,
        transport: &TransportEvidence,
        stage: CheckStage,
        now_ms: u64,
    ) -> Result<ExecutionHandoffChallenge> {
        let original = original_authorization_from(
            &self.connection,
            &self.policy,
            self.policy.verified(),
            wire,
            now_ms,
        )?;
        let (execution, source, operation) = scope(wire)?;
        let admitted = if stage == CheckStage::Admit {
            None
        } else {
            let stored = stored_from(&self.connection, &execution, &source, &operation)?
                .context("handoff must be admitted before starting")?;
            ensure!(
                stored.wire == *wire && stored.transport == *transport,
                "handoff differs from its admitted request"
            );
            Some(reconstruct_handoff(
                &self.connection,
                &self.policy,
                &stored,
            )?)
        };
        let verified = self.policy.verified().verify_execution_handoff(
            wire,
            &original,
            transport,
            admitted.as_ref(),
            now_ms,
        )?;
        let challenge = ExecutionHandoffChallenge::new(
            &verified,
            stage,
            Nonce::random()?,
            now_ms,
            now_ms
                .checked_add(vibeface_secure::authorization::EXECUTION_LEASE_MS)
                .context("handoff lease expiry overflow")?
                .min(verified.expires_at_ms()),
        )?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_policy_heads_in(&tx, self.policy.verified(), verified.original(), now_ms)?;
        check_policy_heads_in(&tx, self.policy.verified(), verified.submission(), now_ms)?;
        tx.execute("INSERT INTO execution_handoff_challenges(execution_space_id,nonce,original_space_id,operation_id,handoff_hash,stage,challenge,expires_at_ms) VALUES(?,?,?,?,?,?,?,?)",
            params![execution, challenge.nonce.as_str(), source, operation, wire.hash()?.as_str(),
                serde_json::to_value(stage)?.as_str().context("invalid handoff stage")?, serde_json::to_string(&challenge)?, challenge.expires_at_ms])?;
        tx.commit()?;
        Ok(challenge)
    }

    pub fn sign_execution_handoff_receipts(
        &self,
        challenge: &ExecutionHandoffChallenge,
        identity: &LocalIdentity,
        now_ms: u64,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        ensure!(
            challenge.version == AUTH_VERSION
                && matches!(
                    challenge.stage,
                    CheckStage::Admit | CheckStage::Start | CheckStage::Renew
                )
                && challenge.issued_at_ms <= now_ms
                && now_ms < challenge.expires_at_ms
                && challenge.expires_at_ms - challenge.issued_at_ms
                    <= vibeface_secure::authorization::EXECUTION_LEASE_MS
                && challenge.policies.len() <= vibeface_secure::authorization::MAX_POLICY_DOMAINS
                && challenge
                    .policies
                    .windows(2)
                    .all(|pair| pair[0].domain < pair[1].domain),
            "invalid execution handoff freshness challenge"
        );
        let key = PublicKey::parse(&identity.public_key_hex()?)?;
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let mut receipts = Vec::new();
        for stamp in &challenge.policies {
            ensure!(
                self.policy.verified().head(&stamp.domain) == Some(stamp),
                "handoff freshness policy changed"
            );
            let authority = match &stamp.domain {
                PolicyDomain::Account(account) => {
                    &self
                        .policy
                        .verified()
                        .account(account)
                        .context("missing account authority")?
                        .authority
                }
                PolicyDomain::Space(space) => {
                    &self
                        .policy
                        .verified()
                        .space(space)
                        .context("missing space authority")?
                        .authority
                }
            };
            if authority.public_key == key {
                ensure!(
                    authority.device == DeviceId::from_key(&key)
                        && authority.service == ServiceId::from_key(&key),
                    "handoff freshness authority mismatch"
                );
                receipts.push(Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: stamp.clone(),
                        challenge_hash: challenge.hash()?,
                        issued_at_ms: now_ms,
                        expires_at_ms: challenge.expires_at_ms,
                    },
                    &signer,
                )?);
            }
        }
        Ok(receipts)
    }

    pub fn accept_execution_handoff(
        &mut self,
        identity: &LocalIdentity,
        wire: &SignedExecutionHandoff,
        proof: &ExecutionHandoffFreshness,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<AdmittedExecutionHandoff> {
        let (execution, original_space, operation_id) = scope(wire)?;
        if let Some(stored) =
            stored_from(&self.connection, &execution, &original_space, &operation_id)?
        {
            ensure!(
                stored.wire == *wire
                    && stored.transport == *transport
                    && stored.admission == *proof,
                "handoff operation ID was reused with another signed request"
            );
            self.execution_handoff_authorization(
                &execution,
                &original_space,
                &operation_id,
                now_ms,
            )?;
            verify_challenge_record(&self.connection, wire, proof, true)?;
            return dto(&self.connection, identity, &stored, now_ms);
        }
        let original = original_authorization_from(
            &self.connection,
            &self.policy,
            self.policy.verified(),
            wire,
            now_ms,
        )?;
        let verified = self
            .policy
            .verified()
            .verify_execution_handoff(wire, &original, transport, None, now_ms)?;
        let freshness = self
            .policy
            .verified()
            .verify_execution_handoff_freshness(&verified, proof, now_ms)?;
        self.policy
            .verified()
            .admit_execution_handoff(&verified, &freshness, now_ms)?;
        let operation = request(wire)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let duplicate:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM execution_handoffs WHERE original_space_id=? AND operation_id=?)",
            params![original_space,operation_id],|row|row.get(0))?;
        ensure!(
            !duplicate,
            "original operation is already admitted under another execution scope"
        );
        let (resources, session) =
            validate_execution_context(&tx, identity, &verified, &operation)?;
        check_execution_session_in(
            &tx,
            &original_space,
            &operation.target_node,
            &operation.session_id,
            is_control(&operation),
        )?;
        check_execution_resources_in(&tx, &operation.target_node, &resources)?;
        consume_freshness_in(
            &tx,
            &self.policy,
            &verified,
            proof,
            CheckStage::Admit,
            now_ms,
        )?;
        tx.execute("INSERT INTO execution_handoffs(execution_space_id,original_space_id,operation_id,target_node,session_id,original_request,handoff,handoff_hash,invocation_hash,body_hash,admission_transport,admission_challenge,admission_receipts,admitted_at_ms,state,control,result) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,'accepted',?,'null')",
            params![execution, original_space, operation_id, operation.target_node, operation.session_id, serde_json::to_string(&operation)?, serde_json::to_string(wire)?,
                wire.hash()?.as_str(), wire.invocation.hash()?.as_str(), wire.invocation.payload.body_hash.as_str(), serde_json::to_string(&StoredTransport::from(transport))?,
                serde_json::to_string(&proof.challenge)?, serde_json::to_string(&proof.receipts)?, now_ms, is_control(&operation)])?;
        tx.execute("INSERT INTO execution_handoff_nonces(execution_space_id,device_id,nonce,original_space_id,operation_id,invocation_hash,body_hash) VALUES(?,?,?,?,?,?,?)",
            params![execution, wire.invocation.payload.device.as_str(), wire.invocation.payload.nonce.as_str(), original_space, operation_id,
                wire.invocation.hash()?.as_str(), wire.invocation.payload.body_hash.as_str()])?;
        for resource in resources {
            tx.execute("INSERT INTO execution_handoff_reservations(execution_space_id,original_space_id,operation_id,target_node,resource) VALUES(?,?,?,?,?)",
                params![execution, original_space, operation_id, operation.target_node, resource])?;
        }
        if operation.action["type"] == "open" {
            tx.execute("INSERT INTO execution_handoff_sessions(execution_space_id,original_space_id,original_resource_id,target_node,session_id,revision,binding,record) VALUES(?,?,?,?,?,?,?,?)",
                params![execution, original_space, verified.original().resource().resource.as_str(), operation.target_node, operation.session_id, session.revision,
                    serde_json::to_string(verified.binding())?, serde_json::to_string(&session)?])?;
        }
        tx.commit()?;
        let stored = stored_from(&self.connection, &execution, &original_space, &operation_id)?
            .context("accepted handoff was not persisted")?;
        dto(&self.connection, identity, &stored, now_ms)
    }

    pub fn claim_execution_handoff(
        &mut self,
        identity: &LocalIdentity,
        auth: &VerifiedExecutionHandoff,
        proof: &ExecutionHandoffFreshness,
        now_ms: u64,
    ) -> Result<(AdmittedExecutionHandoff, bool, u64)> {
        let (execution, original, operation) = scope(auth.wire())?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored = stored_from(&tx, &execution, &original, &operation)?
            .context("unknown execution handoff")?;
        ensure!(
            stored.wire == *auth.wire(),
            "handoff differs from its accepted request"
        );
        if stored.state != OperationState::Accepted {
            let result = dto(&tx, identity, &stored, now_ms)?;
            let deadline = stored.lease.as_ref().map(|lease| lease.2).unwrap_or(0);
            tx.commit()?;
            return Ok((result, false, deadline));
        }
        let deadline =
            consume_freshness_in(&tx, &self.policy, auth, proof, CheckStage::Start, now_ms)?;
        save_lease_in(
            &tx, &execution, &original, &operation, proof, now_ms, deadline,
        )?;
        ensure!(tx.execute("UPDATE execution_handoffs SET state='running' WHERE execution_space_id=? AND original_space_id=? AND operation_id=? AND state='accepted'",
            params![execution, original, operation])? == 1, "execution handoff was already claimed");
        let stored = stored_from(&tx, &execution, &original, &operation)?
            .context("claimed handoff disappeared")?;
        let result = dto(&tx, identity, &stored, now_ms)?;
        tx.commit()?;
        Ok((result, true, deadline))
    }

    pub fn renew_execution_handoff(
        &mut self,
        auth: &VerifiedExecutionHandoff,
        proof: &ExecutionHandoffFreshness,
        now_ms: u64,
    ) -> Result<u64> {
        let (execution, original, operation) = scope(auth.wire())?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored = stored_from(&tx, &execution, &original, &operation)?
            .context("unknown execution handoff")?;
        ensure!(
            stored.wire == *auth.wire()
                && stored.state == OperationState::Running
                && now_ms < verified_lease_deadline(&tx, &self.policy, &stored)?,
            "only a running handoff with a live lease may renew"
        );
        let deadline =
            consume_freshness_in(&tx, &self.policy, auth, proof, CheckStage::Renew, now_ms)?;
        save_lease_in(
            &tx, &execution, &original, &operation, proof, now_ms, deadline,
        )?;
        tx.commit()?;
        Ok(deadline)
    }

    pub fn execution_handoff_session(
        &self,
        execution_space: &str,
        original_project: &ResourceRef,
        target: &str,
        session: &str,
    ) -> Result<Option<ExecutionSession>> {
        Ok(session_from(
            &self.connection,
            execution_space,
            original_project.space.as_str(),
            original_project.resource.as_str(),
            target,
            session,
        )?
        .map(|session| session.0))
    }

    pub fn persist_execution_handoff_native(
        &mut self,
        execution_space: &str,
        original_project: &ResourceRef,
        operation_id: &str,
        native_session_id: &str,
        now_ms: u64,
    ) -> Result<()> {
        ensure!(
            !native_session_id.is_empty() && native_session_id.len() <= 512,
            "invalid native execution session ID"
        );
        let auth = self.execution_handoff_authorization(
            execution_space,
            original_project.space.as_str(),
            operation_id,
            now_ms,
        )?;
        ensure!(
            auth.original().resource() == original_project,
            "native execution belongs to another Project"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored = stored_from(
            &tx,
            execution_space,
            original_project.space.as_str(),
            operation_id,
        )?
        .context("unknown execution handoff")?;
        ensure!(
            stored.state == OperationState::Running
                && now_ms < verified_lease_deadline(&tx, &self.policy, &stored)?,
            "native session update requires a running handoff and live lease"
        );
        check_policy_heads_in(&tx, self.policy.verified(), auth.original(), now_ms)?;
        check_policy_heads_in(&tx, self.policy.verified(), auth.submission(), now_ms)?;
        let (mut session, binding) = session_from(
            &tx,
            execution_space,
            original_project.space.as_str(),
            original_project.resource.as_str(),
            &stored.request.target_node,
            &stored.request.session_id,
        )?
        .context("native execution session is missing")?;
        ensure!(
            stored.request.binding.as_ref() == Some(&binding)
                && session.revision == stored.request.expected_revision,
            "native session binding or revision changed"
        );
        if let Some(previous) = &session.native_session_id {
            ensure!(
                previous == native_session_id,
                "native session ID cannot be replaced while running"
            );
        }
        session.native_session_id = Some(native_session_id.to_owned());
        tx.execute("UPDATE execution_handoff_sessions SET record=? WHERE execution_space_id=? AND original_space_id=? AND original_resource_id=? AND target_node=? AND session_id=? AND revision=?",
            params![serde_json::to_string(&session)?,execution_space,original_project.space.as_str(),original_project.resource.as_str(),stored.request.target_node,stored.request.session_id,session.revision])?;
        tx.commit()?;
        Ok(())
    }

    pub fn find_execution_handoff_by_original(
        &self,
        original_project: &ResourceRef,
        operation_id: &str,
    ) -> Result<Option<ExecutionContext>> {
        let executions=self.connection.prepare("SELECT execution_space_id FROM execution_handoffs WHERE original_space_id=? AND operation_id=?")?
            .query_map(params![original_project.space.as_str(),operation_id],|row|row.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
        ensure!(
            executions.len() <= 1,
            "original operation has ambiguous execution scopes"
        );
        let Some(execution) = executions.first() else {
            return Ok(None);
        };
        let stored = stored_from(
            &self.connection,
            execution,
            original_project.space.as_str(),
            operation_id,
        )?
        .context("execution scope disappeared")?;
        reconstruct_handoff(&self.connection, &self.policy, &stored)?;
        let event: Event =
            serde_json::from_str(&stored.wire.handoff.project_dispatch.canonical_body)?;
        ensure!(
            event.resource_id == original_project.resource,
            "operation belongs to another Project"
        );
        Ok(Some(ExecutionContext {
            space_id: original_project.space.clone(),
            resource_id: original_project.resource.clone(),
            execution_space_id: SpaceId::parse(execution)?,
            execution_service_resource: stored.wire.handoff.execution_resource.resource.clone(),
            stable_original_operation_id: operation_id.to_owned(),
            shared: true,
            lease_expires_at_ms: stored.lease.map(|lease| lease.2),
        }))
    }

    pub fn execution_handoff_status(
        &self,
        identity: &LocalIdentity,
        execution_space: &str,
        original_space: &str,
        operation_id: &str,
        now_ms: u64,
    ) -> Result<AdmittedExecutionHandoff> {
        let stored = stored_from(
            &self.connection,
            execution_space,
            original_space,
            operation_id,
        )?
        .context("unknown execution handoff")?;
        reconstruct_handoff(&self.connection, &self.policy, &stored)?;
        dto(&self.connection, identity, &stored, now_ms)
    }

    pub fn load_execution_handoffs_for_recovery(
        &mut self,
        identity: &LocalIdentity,
        now_ms: u64,
    ) -> Result<Vec<AdmittedExecutionHandoff>> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        recover_execution_handoffs_in(&tx)?;
        let keys = tx.prepare("SELECT execution_space_id,original_space_id,operation_id FROM execution_handoffs WHERE state IN ('accepted','unknown') ORDER BY execution_space_id,original_space_id,operation_id")?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let result = keys
            .into_iter()
            .map(|(execution, original, operation)| {
                let stored = stored_from(&tx, &execution, &original, &operation)?
                    .context("recovery handoff disappeared")?;
                reconstruct_handoff(&tx, &self.policy, &stored)?;
                dto(&tx, identity, &stored, now_ms)
            })
            .collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(result)
    }

    pub fn finish_execution_handoff(
        &mut self,
        identity: &LocalIdentity,
        execution: &str,
        original: &str,
        operation: &str,
        mut state: OperationState,
        mut result: Value,
        session: Option<&ExecutionSession>,
        now_ms: u64,
    ) -> Result<AdmittedExecutionHandoff> {
        ensure!(
            state.terminal() || state == OperationState::Unknown,
            "invalid execution handoff completion state"
        );
        let auth = self.execution_handoff_authorization(execution, original, operation, now_ms);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut stored = stored_from(&tx, execution, original, operation)?
            .context("unknown execution handoff")?;
        ensure!(
            identity.public_key_hex()? == stored.wire.handoff.target_public_key.as_str(),
            "completion signer is not this executor"
        );
        if stored.state.terminal() {
            ensure!(
                stored.state == state && stored.result == result,
                "terminal handoff result cannot change"
            );
            let value = dto(&tx, identity, &stored, now_ms)?;
            tx.commit()?;
            return Ok(value);
        }
        ensure!(
            stored.state == OperationState::Running
                || (stored.state == OperationState::Accepted && state == OperationState::Rejected)
                || (stored.state == OperationState::Unknown
                    && state == OperationState::Unknown
                    && stored.result == result),
            "handoff was not running or requires an explicit reconciliation decision"
        );
        if stored.state == OperationState::Running && state.terminal() {
            let lease_valid = now_ms < verified_lease_deadline(&tx, &self.policy, &stored)?;
            if let Ok(auth) = &auth {
                check_policy_heads_in(&tx, self.policy.verified(), auth.original(), now_ms)?;
                check_policy_heads_in(&tx, self.policy.verified(), auth.submission(), now_ms)?;
            }
            if auth.is_err() || !lease_valid {
                state = OperationState::Unknown;
                result = json!({"reason":"execution authorization lease is unavailable","observed_result":result});
            }
        }
        let dispatch: Event =
            serde_json::from_str(&stored.wire.handoff.project_dispatch.canonical_body)?;
        if let Some(updated) = session {
            let (previous, binding) = session_from(
                &tx,
                execution,
                original,
                dispatch.resource_id.as_str(),
                &stored.request.target_node,
                &stored.request.session_id,
            )?
            .context("execution session disappeared")?;
            ensure!(
                updated.network_id == execution
                    && updated.target_node == stored.request.target_node
                    && updated.session_id == stored.request.session_id
                    && updated.cwd == previous.cwd
                    && updated.executor == previous.executor
                    && updated.revision >= previous.revision
                    && stored.request.binding.as_ref() == Some(&binding),
                "execution completion changed its session namespace or binding"
            );
            let mut updated = updated.clone();
            if state.terminal() {
                updated.revision = updated.revision.max(
                    stored
                        .request
                        .expected_revision
                        .checked_add(1)
                        .context("execution revision overflow")?,
                );
            }
            if state == OperationState::Unknown {
                updated.state = "unknown".into();
                updated.native_session_id = None;
            }
            tx.execute("UPDATE execution_handoff_sessions SET revision=?,record=? WHERE execution_space_id=? AND original_space_id=? AND original_resource_id=? AND target_node=? AND session_id=?",
                params![updated.revision, serde_json::to_string(&updated)?, execution, original, dispatch.resource_id.as_str(), stored.request.target_node, stored.request.session_id])?;
        } else if state.terminal() {
            if let Some((mut previous, _)) = session_from(
                &tx,
                execution,
                original,
                dispatch.resource_id.as_str(),
                &stored.request.target_node,
                &stored.request.session_id,
            )? {
                previous.revision = previous.revision.max(
                    stored
                        .request
                        .expected_revision
                        .checked_add(1)
                        .context("execution revision overflow")?,
                );
                tx.execute("UPDATE execution_handoff_sessions SET revision=?,record=? WHERE execution_space_id=? AND original_space_id=? AND original_resource_id=? AND target_node=? AND session_id=?",
                    params![previous.revision, serde_json::to_string(&previous)?, execution, original, dispatch.resource_id.as_str(), stored.request.target_node, stored.request.session_id])?;
            }
        }
        stored.state = state;
        stored.result = result;
        let receipt = signed_receipt(identity, &stored, now_ms)?;
        tx.execute("UPDATE execution_handoffs SET state=?,result=?,result_receipt=? WHERE execution_space_id=? AND original_space_id=? AND operation_id=?",
            params![state_text(&stored.state)?, serde_json::to_string(&stored.result)?, serde_json::to_string(&receipt)?, execution, original, operation])?;
        if stored.state.terminal() {
            tx.execute("DELETE FROM execution_handoff_reservations WHERE execution_space_id=? AND original_space_id=? AND operation_id=?", params![execution, original, operation])?;
        }
        let value = dto(&tx, identity, &stored, now_ms)?;
        tx.commit()?;
        Ok(value)
    }
}

fn save_lease_in(
    tx: &Transaction<'_>,
    execution: &str,
    original: &str,
    operation: &str,
    proof: &ExecutionHandoffFreshness,
    now_ms: u64,
    deadline: u64,
) -> Result<()> {
    tx.execute("UPDATE execution_handoffs SET lease_challenge=?,lease_receipts=?,lease_verified_at_ms=?,lease_deadline_ms=? WHERE execution_space_id=? AND original_space_id=? AND operation_id=?",
        params![serde_json::to_string(&proof.challenge)?, serde_json::to_string(&proof.receipts)?, now_ms, deadline, execution, original, operation])?;
    Ok(())
}

pub(crate) fn recover_execution_handoffs_in(connection: &Connection) -> Result<usize> {
    Ok(connection.execute(
        "UPDATE execution_handoffs SET state='unknown' WHERE state='running'",
        [],
    )?)
}

pub(crate) fn validate_execution_handoff_storage(
    connection: &Connection,
    policy: &PolicyState,
) -> Result<()> {
    let keys = connection.prepare("SELECT execution_space_id,original_space_id,operation_id FROM execution_handoffs ORDER BY execution_space_id,original_space_id,operation_id")?
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (execution, original, operation) in keys {
        let stored = stored_from(connection, &execution, &original, &operation)?
            .context("stored handoff disappeared")?;
        let admission = reconstruct_handoff(connection, policy, &stored)?;
        verify_challenge_record(connection, &stored.wire, &stored.admission, true)?;
        let nonce: (String, String, String, String, String) = connection.query_row(
            "SELECT device_id,nonce,invocation_hash,body_hash,operation_id FROM execution_handoff_nonces WHERE execution_space_id=? AND original_space_id=? AND operation_id=?",
            params![execution, original, operation], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))?;
        ensure!(
            nonce
                == (
                    stored.wire.invocation.payload.device.to_string(),
                    stored.wire.invocation.payload.nonce.to_string(),
                    stored.wire.invocation.hash()?.to_string(),
                    stored.wire.invocation.payload.body_hash.to_string(),
                    operation.clone()
                ),
            "stored handoff invocation nonce binding changed"
        );
        if let Some((lease, at, deadline)) = &stored.lease {
            ensure!(
                *at >= stored.admitted_at_ms
                    && matches!(lease.challenge.stage, CheckStage::Start | CheckStage::Renew),
                "stored handoff lease has an invalid stage or time"
            );
            let historical = policy.historical(&lease.challenge.policies, *at)?;
            let original_auth =
                original_authorization_from(connection, policy, &historical, &stored.wire, *at)?;
            let auth = historical.verify_execution_handoff(
                &stored.wire,
                &original_auth,
                &stored.transport,
                Some(&admission),
                *at,
            )?;
            let freshness = historical.verify_execution_handoff_freshness(&auth, lease, *at)?;
            ensure!(
                freshness.expires_at_ms() == *deadline,
                "stored handoff lease deadline changed"
            );
            verify_challenge_record(connection, &stored.wire, lease, true)?;
        }
        ensure!(
            !matches!(
                stored.state,
                OperationState::Running | OperationState::Unknown
            ) || stored.lease.is_some(),
            "started handoff has no verified lease"
        );
        if let Some(receipt) = &stored.receipt {
            receipt.verify_signature()?;
            let binding = stored
                .request
                .binding
                .as_ref()
                .context("stored handoff binding is missing")?;
            let value = &receipt.payload;
            ensure!(
                receipt.issuer == stored.wire.handoff.target_public_key
                    && value.version == AUTH_VERSION
                    && value.original_space_id.as_str() == original
                    && value.execution_space_id.as_str() == execution
                    && value.operation_id == operation
                    && value.handoff_hash == stored.wire.hash()?
                    && value.target == stored.wire.handoff.execution_audience.device
                    && value.session_id == stored.request.session_id
                    && value.binding_generation == binding.binding.generation
                    && serde_json::to_value(value.state)? == serde_json::to_value(&stored.state)?
                    && value.result_hash == ObjectHash::body(&stored.result)?
                    && value.issued_at_ms >= stored.admitted_at_ms,
                "stored handoff result receipt changed"
            );
        } else {
            ensure!(
                !stored.state.terminal(),
                "terminal handoff has no signed result receipt"
            );
        }
        let actual = connection.prepare("SELECT target_node,resource FROM execution_handoff_reservations WHERE execution_space_id=? AND original_space_id=? AND operation_id=? ORDER BY resource")?
            .query_map(params![execution, original, operation], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut expected = if stored.state.terminal() {
            Vec::new()
        } else {
            stored
                .request
                .resources
                .iter()
                .map(|resource| (stored.request.target_node.clone(), resource.clone()))
                .collect::<Vec<_>>()
        };
        expected.sort();
        ensure!(
            actual == expected,
            "stored handoff resource reservation set changed"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        binding::{self, TaskBinding},
        identity::test_support::{TestNetwork, now},
        model::{AdmissionProof, EventCause, WIRE_VERSION},
    };
    use anyhow::Result;
    use vibeface_secure::{Identity, authorization::*};

    struct Fixture {
        project: TestNetwork,
        executor: TestNetwork,
        store: Store,
        wire: SignedExecutionHandoff,
        transport: TransportEvidence,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let project = TestNetwork::new()?;
            let executor = TestNetwork::new()?;
            let mut store = project.memory().context("install Project fixture")?;
            project
                .conversation(&mut store, "conversation")
                .context("Project conversation")?;
            project.task(
                &mut store,
                "conversation",
                "other-task-secret-DO-NOT-DISCLOSE",
            )?;
            let task = project
                .task(&mut store, "conversation", "task-handoff")
                .context("Project task")?;
            executor
                .install(&mut store)
                .context("install independent C")?;
            let target_key = PublicKey::parse(&executor.node.public_key_hex()?)?;
            let target = Audience {
                service: ServiceId::from_key(&target_key),
                device: DeviceId::from_key(&target_key),
            };
            let project_key = PublicKey::parse(&project.node.public_key_hex()?)?;
            let project_audience = Audience {
                service: ServiceId::from_key(&project_key),
                device: DeviceId::from_key(&project_key),
            };
            let bound = binding::sign_binding(
                &project.node,
                &TaskBinding {
                    network_id: project.space.to_string(),
                    conversation_id: "conversation".into(),
                    task_ref: task.hash.clone(),
                    task_id: "task-handoff".into(),
                    authority_node: project.node.node_id()?,
                    generation: 1,
                    target_node: executor.node.node_id()?,
                    session_id: "same-session".into(),
                    open_spec: json!({"type":"open","executor":"codex","cwd":executor.project}),
                    previous_binding_hash: None,
                    previous_seal: None,
                },
            )?;
            let operation = OperationRequest {
                operation_id: "original-operation".into(),
                network_id: project.space.to_string(),
                conversation_id: "conversation".into(),
                task_id: Some("task-handoff".into()),
                target_node: executor.node.node_id()?,
                session_id: "same-session".into(),
                expected_revision: 0,
                basis_heads: vec![task.hash.clone()],
                action: bound.binding.open_spec.clone(),
                resources: vec![executor.project.to_str().unwrap().into()],
                binding: Some(bound),
            };
            let body = json!({"method":"operate","space_id":project.space,"resource_id":project.resource,"authorization_id":operation.operation_id,"request":operation});
            let mut expected = vec![
                store
                    .policy
                    .verified()
                    .head(&PolicyDomain::Account(project.account.clone()))
                    .unwrap()
                    .clone(),
                store
                    .policy
                    .verified()
                    .head(&PolicyDomain::Account(executor.account.clone()))
                    .unwrap()
                    .clone(),
                store
                    .policy
                    .verified()
                    .head(&PolicyDomain::Space(project.space.clone()))
                    .unwrap()
                    .clone(),
            ];
            expected.sort_by(|left, right| left.domain.cmp(&right.domain));
            let context = InvocationContext {
                conversation: Some("conversation".into()),
                task: Some(EventRef::parse(&task.hash)?),
                execution_session: Some(SessionRef {
                    target: target.device.clone(),
                    session_id: operation.session_id.clone(),
                }),
                executor_kind: Some("codex".into()),
                workspace_resource: Some(project.resource.clone()),
                sdk_session: None,
            };
            let invocation = Signed::sign(
                Invocation {
                    version: AUTH_VERSION,
                    actor: Actor::Account(project.account.clone()),
                    device: DeviceId::from_key(&PublicKey::of(&project.client)),
                    initiator: Actor::Account(project.account.clone()),
                    initiating_grant: None,
                    resource: ResourceRef {
                        space: project.space.clone(),
                        resource: project.resource.clone(),
                    },
                    action: Action::ExecutionOpen,
                    audience: project_audience,
                    presenter: DeviceId::from_key(&PublicKey::of(&project.client)),
                    operation_id: operation.operation_id.clone(),
                    body_hash: ObjectHash::body(&body)?,
                    grant_leaf: project.bundle.grant_leaf.clone(),
                    expected_policies: expected,
                    context: context.clone(),
                    nonce: Nonce::random()?,
                    not_before_ms: now(),
                    expires_at_ms: now() + 100_000,
                },
                &project.client,
            )?;
            let facts = RequestFacts {
                resource: invocation.payload.resource.clone(),
                action: Action::ExecutionOpen,
                audience: invocation.payload.audience.clone(),
                operation_id: operation.operation_id.clone(),
                body_hash: ObjectHash::body(&body)?,
                context,
            };
            let source_transport = TransportEvidence::DirectNoise {
                device: invocation.payload.device.clone(),
            };
            let auth = store
                .policy
                .verified()
                .verify_invocation(&invocation, &facts, &source_transport, now())
                .context("original Project invocation")?;
            let challenge = FreshnessChallenge::new(
                &auth,
                CheckStage::Admit,
                Nonce::random()?,
                now(),
                now() + 10_000,
            )?;
            let mut receipts = store.sign_current_receipts(&challenge, &project.node, now())?;
            receipts.extend(store.sign_current_receipts(&challenge, &executor.node, now())?);
            let fresh =
                store
                    .policy
                    .verified()
                    .verify_freshness(&auth, &challenge, &receipts, now())?;
            store
                .policy
                .verified()
                .admit_operation(&auth, &fresh, now())?;
            let proof = AdmissionProof {
                invocation,
                source_body: body,
                challenge,
                receipts,
                admitted_at_ms: now(),
                transport: StoredTransport::from(&source_transport),
            };
            let dispatch = project.node.sign_event(Event {
                version: WIRE_VERSION,
                network_id: project.space.to_string(),
                resource_id: project.resource.clone(),
                cause: EventCause::Invocation { admission: proof },
                scope_id: "conversation".into(),
                author: project.node.node_id()?,
                sequence: serde_json::from_str::<Event>(&task.canonical_body)?.sequence + 1,
                previous: Some(task.hash.clone()),
                parents: vec![task.hash.clone()],
                blobs: vec![],
                created_at_ms: now() as i64,
                body: EventBody::DispatchRecorded {
                    operation_id: operation.operation_id.clone(),
                    task_id: operation.task_id.clone(),
                    target_node: operation.target_node.clone(),
                    session_id: operation.session_id.clone(),
                    action: serde_json::to_value(&operation)?,
                },
            })?;
            let execution_resource = ResourceRef {
                space: executor.space.clone(),
                resource: ResourceId::from_bytes([91; 32]),
            };
            let mut handoff = ExecutionHandoff {
                version: AUTH_VERSION,
                project_dispatch: dispatch,
                task_creation: task,
                sdk_origin: None,
                task_sdk_origin: None,
                execution_resource: execution_resource.clone(),
                execution_audience: target.clone(),
                target_public_key: target_key,
                source_attestation: None,
            };
            let project_signer = Identity::from_secret_hex(&project.node.secret_key_hex)
                .map_err(anyhow::Error::msg)?;
            handoff.attest_source(&project_signer)?;
            let original_auth = original_authorization_from(
                &store.connection,
                &store.policy,
                store.policy.verified(),
                &SignedExecutionHandoff {
                    handoff: handoff.clone(),
                    invocation: auth.signed_invocation().clone(),
                },
                now(),
            )
            .context("original dispatch history")?;
            let submission_facts = execution_submission_facts(&handoff, &original_auth)?;
            let stamp = store
                .policy
                .verified()
                .head(&PolicyDomain::Space(executor.space.clone()))
                .unwrap()
                .clone();
            let resource = Signed::sign(
                ResourceRecord {
                    version: AUTH_VERSION,
                    space: executor.space.clone(),
                    resource: execution_resource.resource.clone(),
                    kind: ResourceKind::ExecutionService,
                    host: target.service.clone(),
                    issued_under: stamp.clone(),
                    nonce: Nonce::random()?,
                },
                &executor.root,
            )?;
            let parent = Signed::sign(
                Grant {
                    version: AUTH_VERSION,
                    owner_space: executor.space.clone(),
                    scope: ResourceScope::Exact(execution_resource.clone()),
                    grantee: Grantee::Actor(Actor::Account(executor.account.clone())),
                    actions: vec![Action::ExecutionOpen, Action::ExecutionSubmit],
                    audience: target.clone(),
                    constraints: GrantConstraints {
                        context: submission_facts.context.clone(),
                        require_account_actor: false,
                        require_webauthn_uv: false,
                    },
                    parent: None,
                    remaining_delegations: 1,
                    issued_under: stamp.clone(),
                    not_before_ms: now(),
                    expires_at_ms: now() + 1_000_000,
                    nonce: Nonce::random()?,
                },
                &executor.root,
            )?;
            let mut leaf = parent.payload.clone();
            leaf.grantee = Grantee::Actor(Actor::Service(ServiceId::from_key(&project_key)));
            leaf.parent = Some(parent.hash()?);
            leaf.remaining_delegations = 0;
            leaf.nonce = Nonce::random()?;
            let mut document = store
                .policy
                .verified()
                .space(&executor.space)
                .unwrap()
                .clone();
            document.previous = Some(stamp);
            document.sequence += 1;
            document.resources.push(resource.hash()?);
            document.resources.sort();
            document.grants.push(parent.hash()?);
            document.grants.sort();
            store
                .install_policy_journal(
                    &[
                        JournalEntry::Resource(resource),
                        JournalEntry::Grant(parent),
                        JournalEntry::Space(Signed::sign(document, &executor.root)?),
                    ],
                    now(),
                )
                .context("install C root grant")?;
            let next_stamp = store
                .policy
                .verified()
                .head(&PolicyDomain::Space(executor.space.clone()))
                .unwrap()
                .clone();
            leaf.issued_under = next_stamp.clone();
            let leaf = Signed::sign(leaf, &executor.client)?;
            let mut document = store
                .policy
                .verified()
                .space(&executor.space)
                .unwrap()
                .clone();
            document.previous = Some(next_stamp);
            document.sequence += 1;
            document.grants.push(leaf.hash()?);
            document.grants.sort();
            store
                .install_policy_journal(
                    &[
                        JournalEntry::Grant(leaf.clone()),
                        JournalEntry::Space(Signed::sign(document, &executor.root)?),
                    ],
                    now(),
                )
                .context("install C delegated grant")?;
            store.local_set(
                executor.space.as_str(),
                &format!("resource-root:{}", execution_resource.resource),
                &json!(executor.project),
            )?;
            let service = store
                .policy
                .verified()
                .verify_service_grant(
                    &ServiceId::from_key(&project_key),
                    &leaf.hash()?,
                    &submission_facts,
                    now(),
                )
                .context("C delegated service grant")?;
            let signer = Identity::from_secret_hex(&project.node.secret_key_hex)
                .map_err(anyhow::Error::msg)?;
            let carrier = Signed::sign(
                Invocation {
                    version: AUTH_VERSION,
                    actor: Actor::Service(ServiceId::from_key(&project_key)),
                    device: DeviceId::from_key(&project_key),
                    initiator: service.initiator().clone(),
                    initiating_grant: service.initiating_grant().cloned(),
                    resource: submission_facts.resource,
                    action: Action::ExecutionSubmit,
                    audience: target,
                    presenter: DeviceId::from_key(&project_key),
                    operation_id: operation.operation_id,
                    body_hash: submission_facts.body_hash,
                    grant_leaf: leaf.hash()?,
                    expected_policies: service.heads().to_vec(),
                    context: submission_facts.context,
                    nonce: Nonce::random()?,
                    not_before_ms: now(),
                    expires_at_ms: now() + 100_000,
                },
                &signer,
            )?;
            let wire = SignedExecutionHandoff {
                handoff,
                invocation: carrier,
            };
            let transport = TransportEvidence::VerifiedPeer {
                origin: DeviceId::from_key(&project_key),
            };
            Ok(Self {
                project,
                executor,
                store,
                wire,
                transport,
            })
        }

        fn freshness(&mut self, stage: CheckStage, at: u64) -> Result<ExecutionHandoffFreshness> {
            let challenge = self.store.issue_execution_handoff_challenge(
                &self.wire,
                &self.transport,
                stage,
                at,
            )?;
            let mut receipts =
                self.store
                    .sign_execution_handoff_receipts(&challenge, &self.project.node, at)?;
            receipts.extend(self.store.sign_execution_handoff_receipts(
                &challenge,
                &self.executor.node,
                at,
            )?);
            Ok(ExecutionHandoffFreshness {
                challenge,
                receipts,
            })
        }

        fn accept(&mut self) -> Result<ExecutionHandoffFreshness> {
            let proof = self.freshness(CheckStage::Admit, now())?;
            self.store.accept_execution_handoff(
                &self.executor.node,
                &self.wire,
                &proof,
                &self.transport,
                now() + 1,
            )?;
            Ok(proof)
        }
    }

    #[test]
    fn independent_executor_keeps_original_scope_and_never_restarts_unknown() -> Result<()> {
        let mut fixture = Fixture::new()?;
        assert!(
            !serde_json::to_string(&fixture.wire)?.contains("other-task-secret-DO-NOT-DISCLOSE")
        );
        let admission = fixture.accept()?;
        let duplicate = fixture.store.accept_execution_handoff(
            &fixture.executor.node,
            &fixture.wire,
            &admission,
            &fixture.transport,
            now() + 2,
        )?;
        assert_eq!(
            duplicate.original_request.network_id,
            fixture.project.space.as_str()
        );
        assert_eq!(
            duplicate.execution_context.execution_space_id,
            fixture.executor.space
        );
        assert_eq!(duplicate.execution_context.space_id, fixture.project.space);
        assert_eq!(
            duplicate.execution_context.resource_id,
            fixture.project.resource
        );
        assert!(duplicate.execution_context.shared);
        let proof = fixture.freshness(CheckStage::Start, now() + 100_001)?;
        let auth = fixture.store.execution_handoff_authorization(
            fixture.executor.space.as_str(),
            fixture.project.space.as_str(),
            "original-operation",
            now() + 100_002,
        )?;
        assert_ne!(auth.original().initiator(), auth.submission().initiator());
        let (record, fresh, deadline) = fixture.store.claim_execution_handoff(
            &fixture.executor.node,
            &auth,
            &proof,
            now() + 100_002,
        )?;
        assert!(fresh);
        assert_eq!(record.state, OperationState::Running);
        assert!(deadline > now() + 100_002);
        validate_execution_handoff_storage(&fixture.store.connection, &fixture.store.policy)?;
        let recovered = fixture
            .store
            .load_execution_handoffs_for_recovery(&fixture.executor.node, now() + 100_003)?;
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].state, OperationState::Unknown);
        let (_, fresh, _) = fixture.store.claim_execution_handoff(
            &fixture.executor.node,
            &auth,
            &proof,
            now() + 100_004,
        )?;
        assert!(!fresh);
        let locks: u64 = fixture.store.connection.query_row(
            "SELECT COUNT(*) FROM execution_handoff_reservations",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(locks, 1);
        fixture
            .executor
            .conversation(&mut fixture.store, "conversation")?;
        let child = fixture.executor.project.join("child");
        std::fs::create_dir(&child)?;
        let direct = fixture.executor.request_at(
            &mut fixture.store,
            "direct-after-unknown",
            "direct-session",
            &child,
        )?;
        assert!(fixture.executor.admit(&mut fixture.store, &direct).is_err());
        Ok(())
    }

    #[test]
    fn handoff_requires_both_current_domains_and_original_body() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let mut altered = fixture.wire.clone();
        altered.handoff.execution_resource.resource = fixture.executor.resource.clone();
        assert!(
            fixture
                .store
                .issue_execution_handoff_challenge(
                    &altered,
                    &fixture.transport,
                    CheckStage::Admit,
                    now()
                )
                .is_err()
        );
        let wrong = TransportEvidence::VerifiedPeer {
            origin: DeviceId::from_key(&PublicKey::of(&fixture.project.client)),
        };
        assert!(
            fixture
                .store
                .issue_execution_handoff_challenge(&fixture.wire, &wrong, CheckStage::Admit, now())
                .is_err()
        );
        fixture.accept()?;
        fixture.project.revoke_client(&mut fixture.store)?;
        assert!(
            fixture
                .store
                .execution_handoff_authorization(
                    fixture.executor.space.as_str(),
                    fixture.project.space.as_str(),
                    "original-operation",
                    now() + 2
                )
                .is_err()
        );
        let mut fixture = Fixture::new()?;
        fixture.accept()?;
        fixture.executor.revoke_node(&mut fixture.store)?;
        assert!(
            fixture
                .store
                .execution_handoff_authorization(
                    fixture.executor.space.as_str(),
                    fixture.project.space.as_str(),
                    "original-operation",
                    now() + 2
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn direct_locks_block_handoff_and_signed_completion_releases_only_its_locks() -> Result<()> {
        let mut fixture = Fixture::new()?;
        fixture
            .executor
            .conversation(&mut fixture.store, "conversation")?;
        let child = fixture.executor.project.join("child");
        std::fs::create_dir(&child)?;
        let direct = fixture.executor.request_at(
            &mut fixture.store,
            "direct-first",
            "direct-session",
            &child,
        )?;
        fixture.executor.admit(&mut fixture.store, &direct)?;
        assert!(fixture.accept().is_err());
        fixture.store.finish_operation(
            fixture.executor.space.as_str(),
            "direct-first",
            OperationState::Rejected,
            json!({"reason":"test cleanup"}),
        )?;
        let admission = fixture.freshness(CheckStage::Admit, now() + 1)?;
        let mut missing = admission.clone();
        missing.receipts.retain(|receipt| {
            receipt.payload.current.domain != PolicyDomain::Space(fixture.executor.space.clone())
        });
        assert!(
            fixture
                .store
                .accept_execution_handoff(
                    &fixture.executor.node,
                    &fixture.wire,
                    &missing,
                    &fixture.transport,
                    now() + 2
                )
                .is_err()
        );
        fixture.store.accept_execution_handoff(
            &fixture.executor.node,
            &fixture.wire,
            &admission,
            &fixture.transport,
            now() + 2,
        )?;
        let proof = fixture.freshness(CheckStage::Start, now() + 3)?;
        let auth = fixture.store.execution_handoff_authorization(
            fixture.executor.space.as_str(),
            fixture.project.space.as_str(),
            "original-operation",
            now() + 4,
        )?;
        let (_, fresh, _) = fixture.store.claim_execution_handoff(
            &fixture.executor.node,
            &auth,
            &proof,
            now() + 4,
        )?;
        assert!(fresh);
        assert!(
            fixture
                .store
                .renew_execution_handoff(&auth, &proof, now() + 5)
                .is_err()
        );
        let result = json!({"operation_id":"original-operation","native_session_id":"provider-keeps-original-id"});
        let completed = fixture.store.finish_execution_handoff(
            &fixture.executor.node,
            fixture.executor.space.as_str(),
            fixture.project.space.as_str(),
            "original-operation",
            OperationState::Succeeded,
            result.clone(),
            None,
            now() + 6,
        )?;
        assert_eq!(completed.state, OperationState::Succeeded);
        verify_execution_handoff_receipt_signature(
            &fixture.wire,
            completed.receipt.as_ref().unwrap(),
            &result,
        )?;
        assert!(
            verify_execution_handoff_receipt_signature(
                &fixture.wire,
                completed.receipt.as_ref().unwrap(),
                &json!({"different":"result"})
            )
            .is_err()
        );
        validate_execution_handoff_storage(&fixture.store.connection, &fixture.store.policy)?;
        let locks: u64 = fixture.store.connection.query_row(
            "SELECT COUNT(*) FROM execution_handoff_reservations",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(locks, 0);
        Ok(())
    }
}
