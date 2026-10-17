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
    admission_receipt: Signed<ExecutionHandoffAdmissionReceipt>,
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
    _policy: &PolicyState,
    current: &VerifiedPolicySet,
    wire: &SignedExecutionHandoff,
    now_ms: u64,
) -> Result<VerifiedAuthorization> {
    wire.hash()?;
    original_capsule_authorization(current, &wire.handoff, now_ms)
}

fn original_capsule_authorization(
    current: &VerifiedPolicySet,
    handoff: &ExecutionHandoff,
    now_ms: u64,
) -> Result<VerifiedAuthorization> {
    let source = current.verify_project_execution_source(&handoff, now_ms)?;
    let dispatch: Event = serde_json::from_str(&handoff.project_dispatch.canonical_body)?;
    let task: Event = serde_json::from_str(&handoff.task_creation.canonical_body)?;
    let original = capsule_admission(&dispatch)?;
    let task_proof = capsule_admission(&task)?;
    ensure!(
        task_proof.invocation.payload.audience == original.invocation.payload.audience
            && dispatch.created_at_ms == original.admitted_at_ms as i64
            && original.admitted_at_ms >= task_proof.admitted_at_ms,
        "capsule task host or admission order changed"
    );
    let project = original.invocation.payload.resource.clone();
    capsule_facts(
        &task_proof,
        &project,
        &task.scope_id,
        None,
        handoff.task_sdk_origin.as_ref(),
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
    let operation: OperationRequest =
        serde_json::from_value(original.source_body["request"].clone())?;
    let facts = capsule_facts(
        &original,
        &project,
        &dispatch.scope_id,
        Some((&operation, &handoff)),
        handoff.sdk_origin.as_ref(),
    )?;
    let admitted = source.original_admission(&handoff, now_ms)?;
    let auth = current.verify_operation_authorization(
        &admitted,
        &facts,
        &original.transport.clone().into_evidence(),
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
    let fields = body
        .as_object()
        .context("capsule source must be an object")?;
    ensure!(
        fields.keys().all(|key| if operation.is_some() {
            matches!(
                key.as_str(),
                "method"
                    | "space_id"
                    | "resource_id"
                    | "authorization_id"
                    | "conversation_id"
                    | "request"
                    | "sdk_session"
                    | "parent_invocation"
                    | "task_ref"
                    | "execution_resource"
                    | "target_public_key"
                    | "execution_grant"
                    | "execution_session_receipt"
            )
        } else {
            matches!(
                key.as_str(),
                "method"
                    | "space_id"
                    | "resource_id"
                    | "authorization_id"
                    | "conversation_id"
                    | "task_id"
                    | "title"
                    | "goal"
                    | "acceptance"
                    | "auto_followup"
                    | "sdk_session"
                    | "parent_invocation"
            )
        }),
        "task capsule cannot disclose unrelated source fields"
    );
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
            serde_json::from_value::<ResourceRef>(body["execution_resource"].clone())?
                == handoff.execution_resource
                && serde_json::from_value::<PublicKey>(body["target_public_key"].clone())?
                    == handoff.target_public_key,
            "original request selected another execution destination"
        );
        ObjectHash::parse(
            body["execution_grant"]
                .as_str()
                .context("original execution grant is missing")?,
        )?;
        if !body["execution_session_receipt"].is_null() {
            let receipt: Signed<ExecutionHandoffReceipt> =
                serde_json::from_value(body["execution_session_receipt"].clone())?;
            receipt.verify_signature()?;
            let snapshot = receipt
                .payload
                .session
                .as_ref()
                .context("execution state receipt has no signed snapshot")?;
            let confirmed = request
                .binding
                .as_ref()
                .context("execution state has no task binding")?;
            ensure!(
                receipt.issuer == handoff.target_public_key
                    && snapshot.original_project == *project
                    && snapshot.execution_resource == handoff.execution_resource
                    && snapshot.target == handoff.execution_audience.device
                    && snapshot.session_id == request.session_id
                    && snapshot.revision == request.expected_revision
                    && snapshot.task.as_str() == confirmed.binding.task_ref
                    && snapshot.binding_hash == crate::binding::binding_hash(&confirmed.binding)?
                    && snapshot.binding_generation == confirmed.binding.generation
                    && confirmed.binding.open_spec["executor"].as_str()
                        == Some(snapshot.executor.as_str())
                    && confirmed.binding.open_spec["cwd"].as_str() == Some(snapshot.cwd.as_str())
                    && receipt.payload.original_space_id == project.space
                    && receipt.payload.execution_space_id == handoff.execution_resource.space
                    && receipt.payload.target == handoff.execution_audience.device
                    && receipt.payload.session_id == request.session_id
                    && receipt.payload.issued_at_ms <= proof.admitted_at_ms
                    && proof.admitted_at_ms < snapshot.expires_at_ms
                    && snapshot.expires_at_ms - receipt.payload.issued_at_ms
                        <= vibeface_secure::authorization::EXECUTION_LEASE_MS,
                "original execution state receipt belongs to another session"
            );
        } else {
            ensure!(
                request.action["type"] == "open",
                "non-open operation requires signed executor session state"
            );
        }
        ensure!(
            request.conversation_id == conversation
                && (body["conversation_id"].is_null()
                    || body["conversation_id"].as_str() == Some(conversation))
                && request.network_id == project.space.as_str()
                && body["method"] == "operate",
            "capsule operation scope changed"
        );
        let binding = request
            .binding
            .as_ref()
            .context("capsule operation has no binding")?;
        ensure!(
            body["task_ref"].is_null()
                || body["task_ref"].as_str() == Some(handoff.task_creation.hash.as_str()),
            "capsule references another task"
        );
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
                && session.session_id == origin.sdk_trigger.as_str()
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
    let parent = super::services::agent_account_message(events, &session.session_id)?;
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
        sdk_trigger: vibeface_secure::authorization::EventRef::parse(&session.session_id)?,
        invocation: original.invocation,
        challenge: original.challenge,
        receipts: original.receipts,
        admitted_at_ms: original.admitted_at_ms,
        transport: original.transport,
    }))
}

fn verify_local_handoff_checkpoint(connection: &Connection, stored: &StoredHandoff) -> Result<()> {
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
    if let Some((lease, _, _)) = &stored.lease {
        verify_challenge_record(connection, &stored.wire, lease, true)?;
    }
    verify_execution_handoff_admission_checkpoint(
        &stored.wire,
        &stored.admission_receipt,
        &stored.admission,
        &StoredTransport::from(&stored.transport),
        stored.admitted_at_ms,
    )?;
    Ok(())
}

fn reconstruct_handoff(
    connection: &Connection,
    _policy: &PolicyState,
    stored: &StoredHandoff,
) -> Result<VerifiedExecutionAdmission> {
    verify_local_handoff_checkpoint(connection, stored)?;
    Ok(verify_execution_handoff_admission_checkpoint(
        &stored.wire,
        &stored.admission_receipt,
        &stored.admission,
        &StoredTransport::from(&stored.transport),
        stored.admitted_at_ms,
    )?)
}

fn lease_checkpoint_key(
    wire: &SignedExecutionHandoff,
    proof: &ExecutionHandoffFreshness,
) -> Result<String> {
    Ok(format!(
        "execution-handoff-lease:{}:{}",
        wire.hash()?,
        proof.challenge.hash()?
    ))
}

fn verified_lease_deadline(
    connection: &Connection,
    _policy: &PolicyState,
    stored: &StoredHandoff,
    current: Option<&VerifiedExecutionHandoff>,
    next: Option<&ExecutionHandoffFreshness>,
) -> Result<u64> {
    let (lease, at, deadline) = stored
        .lease
        .as_ref()
        .context("handoff has no execution lease")?;
    verify_local_handoff_checkpoint(connection, stored)?;
    let encoded: String = connection
        .query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![
                stored.wire.handoff.execution_resource.space.as_str(),
                lease_checkpoint_key(&stored.wire, lease)?
            ],
            |row| row.get(0),
        )
        .context("executor lease has no immutable checkpoint")?;
    let receipt: Signed<ExecutionHandoffLeaseReceipt> = serde_json::from_str(&encoded)?;
    let mut usage = verify_execution_handoff_lease_checkpoint(
        &stored.wire,
        &stored.admission_receipt,
        lease,
        *at,
        *deadline,
        &receipt,
    )?;
    let admitted = verify_execution_handoff_admission_checkpoint(
        &stored.wire,
        &stored.admission_receipt,
        &stored.admission,
        &StoredTransport::from(&stored.transport),
        stored.admitted_at_ms,
    )?;
    usage.merge(admitted.proof_usage());
    if let Some(current) = current {
        usage.merge(current.proof_usage());
    }
    if let Some(next) = next {
        usage.record_metadata(&next.challenge)?;
        for receipt in &next.receipts {
            usage.record_signed(receipt)?;
        }
    }
    usage.enforce_limits()?;
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
        admission_receipt: String,
    }
    let raw = connection.query_row(
        "SELECT handoff,original_request,admission_transport,admission_challenge,admission_receipts,admitted_at_ms,state,result,lease_challenge,lease_receipts,lease_verified_at_ms,lease_deadline_ms,result_receipt,handoff_hash,invocation_hash,body_hash,target_node,session_id,control,admission_receipt FROM execution_handoffs WHERE execution_space_id=? AND original_space_id=? AND operation_id=?",
        params![execution, original, operation], |row| Ok(Raw {
            wire: row.get(0)?, request: row.get(1)?, transport: row.get(2)?, challenge: row.get(3)?, receipts: row.get(4)?,
            admitted: row.get(5)?, state: row.get(6)?, result: row.get(7)?, lease_challenge: row.get(8)?, lease_receipts: row.get(9)?,
            lease_verified: row.get(10)?, lease_deadline: row.get(11)?, receipt: row.get(12)?, handoff_hash: row.get(13)?,
            invocation_hash: row.get(14)?, body_hash: row.get(15)?, target: row.get(16)?, session: row.get(17)?, control: row.get(18)?, admission_receipt: row.get(19)?,
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
        admission_receipt: serde_json::from_str(&raw.admission_receipt)?,
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
                && session.state != "resolved"
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
    connection: &Connection,
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
    let dispatch: Event =
        serde_json::from_str(&stored.wire.handoff.project_dispatch.canonical_body)?;
    let session = session_from(
        connection,
        stored.wire.handoff.execution_resource.space.as_str(),
        &stored.request.network_id,
        dispatch.resource_id.as_str(),
        &stored.request.target_node,
        &stored.request.session_id,
    )?
    .map(|(session, _)| -> Result<ExecutionSessionSnapshot> {
        Ok(ExecutionSessionSnapshot {
            original_project: ResourceRef {
                space: SpaceId::parse(&stored.request.network_id)?,
                resource: dispatch.resource_id.clone(),
            },
            execution_resource: stored.wire.handoff.execution_resource.clone(),
            task: vibeface_secure::authorization::EventRef::parse(&binding.binding.task_ref)?,
            binding_hash: crate::binding::binding_hash(&binding.binding)?,
            binding_generation: binding.binding.generation,
            target: stored.wire.handoff.execution_audience.device.clone(),
            session_id: session.session_id,
            revision: session.revision,
            executor: session.executor,
            cwd: session.cwd,
            state: session.state,
            expires_at_ms: now_ms
                .checked_add(vibeface_secure::authorization::EXECUTION_LEASE_MS)
                .context("session snapshot expiry overflow")?,
        })
    })
    .transpose()?;
    let receipt = Signed::sign(
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
            session,
            issued_at_ms: now_ms,
        },
        &signer,
    )?;
    verify_execution_handoff_receipt_signature(&stored.wire, &receipt, &stored.result)?;
    Ok(receipt)
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
        receipt: Some(signed_receipt(connection, identity, stored, now_ms)?),
    })
}

impl Store {
    pub fn execution_handoff_proof_usage(
        &self,
        wire: &SignedExecutionHandoff,
        transport: &TransportEvidence,
        freshness: Option<&ExecutionHandoffFreshness>,
        now_ms: u64,
    ) -> Result<ExecutionProofUsage> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let current = self.policy.verified();
        let original =
            original_authorization_from(&self.connection, &self.policy, current, wire, now_ms)?;
        let (execution, space, operation) = scope(wire)?;
        let admission = stored_from(&self.connection, &execution, &space, &operation)?
            .map(|stored| reconstruct_handoff(&self.connection, &self.policy, &stored))
            .transpose()?;
        let verified = current.verify_execution_handoff(
            wire,
            &original,
            transport,
            admission.as_ref(),
            now_ms,
        )?;
        let mut usage = match freshness {
            Some(proof) => current
                .verify_execution_handoff_freshness(&verified, proof, now_ms)?
                .proof_usage()
                .clone(),
            None => verified.proof_usage().clone(),
        };
        usage.record_actual_signature_counts(_signature_scope.counts());
        Ok(usage)
    }

    pub fn execution_handoff_resolution_proof_usage(
        &self,
        wire: &SignedExecutionHandoffResolution,
        freshness: Option<&ExecutionHandoffResolutionFreshness>,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<ExecutionProofUsage> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let auth = self.execution_handoff_resolution_authorization(wire, transport, now_ms)?;
        let mut usage = auth.proof_usage().clone();
        if let Some(proof) = freshness {
            self.policy
                .verified()
                .verify_execution_resolution_freshness(&auth, proof, now_ms)?;
            usage.record_metadata(&proof.challenge)?;
            for receipt in &proof.receipts {
                usage.record_signed(receipt)?;
            }
        }
        usage.enforce_limits()?;
        usage.record_actual_signature_counts(_signature_scope.counts());
        Ok(usage)
    }
}

fn ready_execution_source_event(
    connection: &Connection,
    space: &str,
    hash: &str,
    expected_resource: Option<&ResourceId>,
) -> Result<StoredEvent> {
    let row: (String, String, String, u64, String) = connection.query_row(
        "SELECT envelope,scope_id,author,sequence,resource_id FROM events WHERE space_id=? AND hash=? AND status='ready'",
        params![space, hash], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
    ).context("execution source is not a locally accepted ready event")?;
    let envelope: crate::model::SignedEvent = serde_json::from_str(&row.0)?;
    ensure!(
        envelope.canonical_body.len() <= crate::model::MAX_EVENT_BYTES
            && envelope.hash == hash
            && crate::crypto::digest(envelope.canonical_body.as_bytes()) == hash,
        "ready execution source hash changed"
    );
    let event: Event = serde_json::from_str(&envelope.canonical_body)?;
    ensure!(
        serde_json::to_string(&event)? == envelope.canonical_body
            && event.network_id == space
            && event.scope_id == row.1
            && event.author == row.2
            && event.sequence == row.3
            && event.resource_id.as_str() == row.4
            && expected_resource.is_none_or(|resource| resource == &event.resource_id),
        "ready execution source metadata changed"
    );
    crate::crypto::validate_event_shape(&event)?;
    Ok(StoredEvent {
        envelope,
        event,
        status: "ready".into(),
    })
}

fn ready_execution_source_events(
    connection: &Connection,
    space: &str,
    operation: &str,
) -> Result<BTreeMap<String, StoredEvent>> {
    let hashes = connection.prepare("SELECT hash FROM events WHERE space_id=? AND status='ready' AND json_extract(json_extract(envelope,'$.canonical_body'),'$.body.type')='dispatch_recorded' AND json_extract(json_extract(envelope,'$.canonical_body'),'$.body.operation_id')=? LIMIT 2")?
        .query_map(params![space, operation], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        hashes.len() == 1,
        "operation has missing or ambiguous ready dispatches"
    );
    let dispatch = ready_execution_source_event(connection, space, &hashes[0], None)?;
    let project = dispatch.event.resource_id.clone();
    let conversation = dispatch.event.scope_id.clone();
    let proof = capsule_admission(&dispatch.event)?;
    let request: OperationRequest = serde_json::from_value(proof.source_body["request"].clone())?;
    let binding = request
        .binding
        .as_ref()
        .context("dispatch has no Task binding")?;
    let task =
        ready_execution_source_event(connection, space, &binding.binding.task_ref, Some(&project))?;
    let task_proof = capsule_admission(&task.event)?;
    let mut events = BTreeMap::from([
        (dispatch.envelope.hash.clone(), dispatch),
        (task.envelope.hash.clone(), task),
    ]);
    let mut loaded_bytes = events
        .values()
        .map(|stored| stored.envelope.canonical_body.len())
        .sum::<usize>();
    for proof in [&proof, &task_proof] {
        if proof.source_body["sdk_session"].is_null() {
            continue;
        }
        let session: vibeface_secure::authorization::SessionRef =
            serde_json::from_value(proof.source_body["sdk_session"].clone())?;
        let expected = proof.source_body["parent_invocation"]
            .as_str()
            .context("SDK source has no Account parent")?;
        let mut pending = std::collections::VecDeque::from([session.session_id.clone()]);
        let mut visited = std::collections::BTreeSet::new();
        let mut found = false;
        while let Some(hash) = pending.pop_front() {
            if !visited.insert(hash.clone()) {
                continue;
            }
            ensure!(
                visited.len() <= 128 && events.len() <= 128,
                "SDK source ancestry is too large"
            );
            if !events.contains_key(&hash) {
                let stored =
                    ready_execution_source_event(connection, space, &hash, Some(&project))?;
                loaded_bytes += stored.envelope.canonical_body.len();
                ensure!(
                    loaded_bytes <= MAX_EXECUTION_HANDOFF_BYTES,
                    "SDK source ancestry is too large"
                );
                events.insert(hash.clone(), stored);
            }
            let stored = &events[&hash];
            ensure!(
                stored.event.scope_id == conversation,
                "SDK source crossed a conversation"
            );
            if matches!(&stored.event.body, EventBody::MessagePosted { role, .. } if role == "user")
            {
                let admission = capsule_admission(&stored.event)?;
                if admission.invocation.hash()?.as_str() == expected {
                    ensure!(
                        ObjectHash::body(&admission.source_body)?
                            == admission.invocation.payload.body_hash,
                        "SDK Account Message source body changed"
                    );
                    found = true;
                    break;
                }
            }
            pending.extend(stored.event.parents.iter().cloned());
            pending.extend(stored.event.previous.iter().cloned());
            if let crate::model::EventCause::Derived { source, .. } = &stored.event.cause {
                pending.push_back(source.clone());
            }
        }
        ensure!(found, "SDK source has no bounded causal Account message");
    }
    Ok(events)
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
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let events = ready_execution_source_events(&self.connection, original_space, operation_id)?;
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
        original_capsule_authorization(self.policy.verified(), &handoff, now_ms)?;
        Ok(handoff)
    }

    pub fn execution_handoff_authorization(
        &self,
        execution_space: &str,
        original_space: &str,
        operation_id: &str,
        now_ms: u64,
    ) -> Result<VerifiedExecutionHandoff> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
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
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
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
                    CheckStage::Admit
                        | CheckStage::Start
                        | CheckStage::Renew
                        | CheckStage::RemoteRead
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
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
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
        let reused: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM execution_handoff_resolutions WHERE execution_space_id=? AND submission_device=? AND submission_nonce=?)",
            params![execution, wire.invocation.payload.device.as_str(), wire.invocation.payload.nonce.as_str()],
            |row| row.get(0),
        )?;
        ensure!(!reused, "handoff reused a prior executor resolution nonce");
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
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let admission_receipt = Signed::sign(
            ExecutionHandoffAdmissionReceipt::new(
                wire,
                proof,
                &StoredTransport::from(transport),
                now_ms,
            )?,
            &signer,
        )?;
        tx.execute("INSERT INTO execution_handoffs(execution_space_id,original_space_id,operation_id,target_node,session_id,original_request,handoff,handoff_hash,invocation_hash,body_hash,admission_transport,admission_challenge,admission_receipts,admission_receipt,admitted_at_ms,state,control,result) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,'accepted',?,'null')",
            params![execution, original_space, operation_id, operation.target_node, operation.session_id, serde_json::to_string(&operation)?, serde_json::to_string(wire)?,
                wire.hash()?.as_str(), wire.invocation.hash()?.as_str(), wire.invocation.payload.body_hash.as_str(), serde_json::to_string(&StoredTransport::from(transport))?,
                serde_json::to_string(&proof.challenge)?, serde_json::to_string(&proof.receipts)?, serde_json::to_string(&admission_receipt)?, now_ms, is_control(&operation)])?;
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

    pub fn authorize_execution_handoff_read(
        &self,
        auth: &VerifiedExecutionHandoff,
        proof: &ExecutionHandoffFreshness,
        now_ms: u64,
    ) -> Result<()> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        ensure!(
            proof.challenge.stage == CheckStage::RemoteRead,
            "handoff read requires its own freshness purpose"
        );
        let (execution, original, operation) = scope(auth.wire())?;
        let stored = stored_from(&self.connection, &execution, &original, &operation)?
            .context("handoff read has no prior acceptance")?;
        ensure!(
            stored.wire == *auth.wire(),
            "handoff read changed its original request"
        );
        reconstruct_handoff(&self.connection, &self.policy, &stored)?;
        check_policy_heads_in(
            &self.connection,
            self.policy.verified(),
            auth.original(),
            now_ms,
        )?;
        check_policy_heads_in(
            &self.connection,
            self.policy.verified(),
            auth.submission(),
            now_ms,
        )?;
        self.policy
            .verified()
            .verify_execution_handoff_freshness(auth, proof, now_ms)?;
        verify_challenge_record(&self.connection, auth.wire(), proof, false)
    }

    pub fn claim_execution_handoff(
        &mut self,
        identity: &LocalIdentity,
        auth: &VerifiedExecutionHandoff,
        proof: &ExecutionHandoffFreshness,
        now_ms: u64,
    ) -> Result<(AdmittedExecutionHandoff, bool, u64)> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
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
        save_lease_in(&tx, identity, auth, proof, now_ms, deadline)?;
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
        identity: &LocalIdentity,
        auth: &VerifiedExecutionHandoff,
        proof: &ExecutionHandoffFreshness,
        now_ms: u64,
    ) -> Result<u64> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let (execution, original, operation) = scope(auth.wire())?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored = stored_from(&tx, &execution, &original, &operation)?
            .context("unknown execution handoff")?;
        ensure!(
            stored.wire == *auth.wire()
                && stored.state == OperationState::Running
                && now_ms
                    < verified_lease_deadline(&tx, &self.policy, &stored, Some(auth), Some(proof))?,
            "only a running handoff with a live lease may renew"
        );
        let deadline =
            consume_freshness_in(&tx, &self.policy, auth, proof, CheckStage::Renew, now_ms)?;
        save_lease_in(&tx, identity, auth, proof, now_ms, deadline)?;
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
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
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
                && now_ms < verified_lease_deadline(&tx, &self.policy, &stored, Some(&auth), None)?,
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
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
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
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
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
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
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
            let lease_valid = now_ms
                < verified_lease_deadline(&tx, &self.policy, &stored, auth.as_ref().ok(), None)?;
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
        let receipt = signed_receipt(&tx, identity, &stored, now_ms)?;
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

struct StoredResolution {
    wire: SignedExecutionHandoffResolution,
    transport: TransportEvidence,
    freshness: ExecutionHandoffResolutionFreshness,
    resolved_at_ms: u64,
    result: ExecutionHandoffResolutionResult,
}

pub(super) fn resolution_resolver_from(
    policy: &PolicyState,
    current: &VerifiedPolicySet,
    wire: &SignedExecutionHandoffResolution,
    now_ms: u64,
) -> Result<VerifiedAuthorization> {
    let facts = execution_resolution_account_facts(&wire.resolution)?;
    let proof = &wire.resolution.account_admission;
    let admitted = verify_capsule_admission(policy, proof, &facts, None)?;
    Ok(current.verify_operation_authorization(
        &admitted,
        &facts,
        &proof.transport.clone().into_evidence(),
        now_ms,
    )?)
}

fn stored_resolution_from(
    connection: &Connection,
    wire: &SignedExecutionHandoffResolution,
) -> Result<Option<StoredResolution>> {
    let (execution, original, operation) = scope(&wire.resolution.observation.handoff)?;
    let row = connection.query_row(
        "SELECT request,admission_transport,freshness,resolved_at_ms,result_receipt,resolution_id,resolution_hash,resolver_device,resolver_nonce,resolver_invocation_hash,resolver_body_hash,submission_device,submission_nonce,submission_invocation_hash,submission_body_hash FROM execution_handoff_resolutions WHERE execution_space_id=? AND original_space_id=? AND operation_id=?",
        params![execution, original, operation], |row| {
            Ok((row.get::<_,String>(0)?, row.get::<_,String>(1)?, row.get::<_,String>(2)?,
                row.get::<_,u64>(3)?, row.get::<_,String>(4)?,
                (5..15).map(|index|row.get::<_,String>(index)).collect::<rusqlite::Result<Vec<_>>>()?))
        }).optional()?;
    let Some((request, transport, freshness, resolved_at_ms, result, metadata)) = row else {
        return Ok(None);
    };
    let saved: SignedExecutionHandoffResolution = serde_json::from_str(&request)?;
    ensure!(
        saved == *wire,
        "original operation already has another resolution request"
    );
    let account = &saved.resolution.account_admission.invocation;
    let submission = &saved.invocation;
    let expected = vec![
        account.payload.operation_id.clone(),
        saved.hash()?.to_string(),
        account.payload.device.to_string(),
        account.payload.nonce.to_string(),
        account.hash()?.to_string(),
        account.payload.body_hash.to_string(),
        submission.payload.device.to_string(),
        submission.payload.nonce.to_string(),
        submission.hash()?.to_string(),
        submission.payload.body_hash.to_string(),
    ];
    ensure!(
        metadata == expected,
        "stored resolution nonce, scope or request hash changed"
    );
    let result: ExecutionHandoffResolutionResult = serde_json::from_str(&result)?;
    verify_execution_resolution_receipt_signature(&result)?;
    ensure!(
        result.resolution == saved && result.receipt.payload.issued_at_ms == resolved_at_ms,
        "stored resolution receipt differs from its request or acceptance clock"
    );
    Ok(Some(StoredResolution {
        wire: saved,
        transport: serde_json::from_str::<StoredTransport>(&transport)?.into_evidence(),
        freshness: serde_json::from_str(&freshness)?,
        resolved_at_ms,
        result,
    }))
}

fn verify_resolution_challenge_record(
    connection: &Connection,
    wire: &SignedExecutionHandoffResolution,
    proof: &ExecutionHandoffResolutionFreshness,
    consumed: bool,
) -> Result<()> {
    let (execution, original, operation) = scope(&wire.resolution.observation.handoff)?;
    let saved: (String,String,String,String,String,u64,bool) = connection.query_row(
        "SELECT original_space_id,operation_id,resolution_id,resolution_hash,challenge,expires_at_ms,consumed FROM execution_handoff_resolution_challenges WHERE execution_space_id=? AND nonce=?",
        params![execution, proof.challenge.nonce.as_str()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)))
        .context("resolution challenge was not issued by this executor")?;
    ensure!(
        saved.0 == original
            && saved.1 == operation
            && saved.2
                == wire
                    .resolution
                    .account_admission
                    .invocation
                    .payload
                    .operation_id
            && saved.3 == wire.hash()?.as_str()
            && serde_json::from_str::<ExecutionHandoffResolutionChallenge>(&saved.4)?
                == proof.challenge
            && saved.5 == proof.challenge.expires_at_ms
            && saved.6 == consumed,
        "resolution challenge is consumed or differs from its durable purpose"
    );
    Ok(())
}

fn reconstruct_resolution(
    connection: &Connection,
    _policy: &PolicyState,
    saved: &StoredResolution,
) -> Result<VerifiedExecutionResolutionAdmission> {
    verify_resolution_challenge_record(connection, &saved.wire, &saved.freshness, true)?;
    Ok(verify_execution_resolution_admission_checkpoint(
        &saved.result,
        &saved.freshness,
        &StoredTransport::from(&saved.transport),
        saved.resolved_at_ms,
    )?)
}

fn resolution_original_observation_from(
    connection: &Connection,
    _policy: &PolicyState,
    wire: &SignedExecutionHandoffResolution,
) -> Result<StoredHandoff> {
    let (execution, original, operation) = scope(&wire.resolution.observation.handoff)?;
    let stored = stored_from(connection, &execution, &original, &operation)?
        .context("resolution has no executor acceptance")?;
    ensure!(
        stored.wire == wire.resolution.observation.handoff,
        "resolution changed the executor's accepted carrier"
    );
    verify_local_handoff_checkpoint(connection, &stored)?;
    verify_execution_handoff_receipt_signature(
        &stored.wire,
        &wire.resolution.observation.receipt,
        &stored.result,
    )?;
    let actual = connection.prepare("SELECT target_node,resource FROM execution_handoff_reservations WHERE execution_space_id=? AND original_space_id=? AND operation_id=? ORDER BY resource")?
        .query_map(params![execution, original, operation], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
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
        "execution checkpoint reservations changed"
    );
    ensure!(
        (match wire.resolution.purpose {
            ExecutionResolutionPurpose::ResolveUnknown => stored.state == OperationState::Unknown,
            ExecutionResolutionPurpose::ReconcileTerminal =>
                stored.state.terminal()
                    && serde_json::to_value(&stored.state)?
                        == serde_json::to_value(wire.resolution.observation.receipt.payload.state)?,
        }) && stored.result == wire.resolution.observation.result
            && wire.resolution.observation.receipt.payload.issued_at_ms >= stored.admitted_at_ms,
        "resolution does not match the executor's current unknown state"
    );
    let dispatch: Event =
        serde_json::from_str(&stored.wire.handoff.project_dispatch.canonical_body)?;
    let (session, binding) = session_from(
        connection,
        &execution,
        &original,
        dispatch.resource_id.as_str(),
        &stored.request.target_node,
        &stored.request.session_id,
    )?
    .context("unknown execution session disappeared")?;
    let snapshot = wire
        .resolution
        .observation
        .receipt
        .payload
        .session
        .as_ref()
        .context("unknown receipt has no signed session state")?;
    ensure!(
        stored.request.binding.as_ref() == Some(&binding)
            && snapshot.revision == session.revision
            && snapshot.session_id == session.session_id
            && snapshot.target == stored.wire.handoff.execution_audience.device
            && snapshot.cwd == session.cwd
            && snapshot.executor == session.executor
            && snapshot.state == session.state,
        "unknown receipt differs from the executor's current session revision or binding"
    );
    Ok(stored)
}

impl Store {
    pub fn execution_handoff_resolution_authorization(
        &self,
        wire: &SignedExecutionHandoffResolution,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<VerifiedExecutionResolution> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let resolver = self
            .policy
            .verified()
            .verify_execution_resolution_account_checkpoint(wire, transport, now_ms)?;
        let admission = if wire.resolution.purpose == ExecutionResolutionPurpose::ReconcileTerminal
        {
            resolution_original_observation_from(&self.connection, &self.policy, wire)?;
            None
        } else if let Some(saved) = stored_resolution_from(&self.connection, wire)? {
            ensure!(
                saved.transport == *transport,
                "resolution retry changed its authenticated transport"
            );
            Some(reconstruct_resolution(
                &self.connection,
                &self.policy,
                &saved,
            )?)
        } else {
            resolution_original_observation_from(&self.connection, &self.policy, wire)?;
            None
        };
        let verified = self.policy.verified().verify_execution_resolution(
            wire,
            &resolver,
            transport,
            admission.as_ref(),
            now_ms,
        )?;
        let (execution, original, operation) = scope(&wire.resolution.observation.handoff)?;
        let stored = stored_from(&self.connection, &execution, &original, &operation)?
            .context("resolution acceptance checkpoint disappeared")?;
        Ok(verified.with_admission_checkpoint(
            &stored.admission_receipt,
            &stored.admission,
            &StoredTransport::from(&stored.transport),
            stored.admitted_at_ms,
        )?)
    }

    pub fn execution_handoff_resolution_status(
        &self,
        wire: &SignedExecutionHandoffResolution,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<Option<ExecutionHandoffResolutionResult>> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        if wire.resolution.purpose == ExecutionResolutionPurpose::ReconcileTerminal {
            return Ok(None);
        }
        let Some(saved) = stored_resolution_from(&self.connection, wire)? else {
            return Ok(None);
        };
        let auth = self.execution_handoff_resolution_authorization(wire, transport, now_ms)?;
        check_policy_heads_in(
            &self.connection,
            self.policy.verified(),
            auth.resolver(),
            now_ms,
        )?;
        check_policy_heads_in(
            &self.connection,
            self.policy.verified(),
            auth.submission(),
            now_ms,
        )?;
        let (execution, original, operation) = scope(&wire.resolution.observation.handoff)?;
        let original = stored_from(&self.connection, &execution, &original, &operation)?
            .context("resolved handoff disappeared")?;
        ensure!(
            original.state.terminal()
                && original.result == saved.result.result
                && original.receipt.as_ref() == Some(&saved.result.handoff_receipt),
            "resolved handoff differs from its immutable resolution receipt"
        );
        Ok(Some(saved.result))
    }

    pub fn issue_execution_handoff_resolution_challenge(
        &mut self,
        wire: &SignedExecutionHandoffResolution,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<ExecutionHandoffResolutionChallenge> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        ensure!(
            wire.resolution.purpose == ExecutionResolutionPurpose::ReconcileTerminal
                || stored_resolution_from(&self.connection, wire)?.is_none(),
            "resolution is already committed"
        );
        let auth = self.execution_handoff_resolution_authorization(wire, transport, now_ms)?;
        let challenge = ExecutionHandoffResolutionChallenge::new(
            &auth,
            Nonce::random()?,
            now_ms,
            now_ms
                .checked_add(vibeface_secure::authorization::EXECUTION_LEASE_MS)
                .context("resolution clock overflow")?
                .min(auth.expires_at_ms()),
        )?;
        let (execution, original, operation) = scope(&wire.resolution.observation.handoff)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_policy_heads_in(&tx, self.policy.verified(), auth.resolver(), now_ms)?;
        check_policy_heads_in(&tx, self.policy.verified(), auth.submission(), now_ms)?;
        resolution_original_observation_from(&tx, &self.policy, wire)?;
        tx.execute("INSERT INTO execution_handoff_resolution_challenges(execution_space_id,nonce,original_space_id,operation_id,resolution_id,resolution_hash,challenge,expires_at_ms) VALUES(?,?,?,?,?,?,?,?)",
            params![execution,challenge.nonce.as_str(),original,operation,auth.resolver().operation_id(),auth.hash().as_str(),serde_json::to_string(&challenge)?,challenge.expires_at_ms])?;
        tx.commit()?;
        Ok(challenge)
    }

    pub fn sign_execution_handoff_resolution_receipts(
        &self,
        challenge: &ExecutionHandoffResolutionChallenge,
        identity: &LocalIdentity,
        now_ms: u64,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        ensure!(
            challenge.version == AUTH_VERSION
                && challenge.issued_at_ms <= now_ms
                && now_ms < challenge.expires_at_ms
                && challenge.expires_at_ms - challenge.issued_at_ms
                    <= vibeface_secure::authorization::EXECUTION_LEASE_MS
                && !challenge.policies.is_empty()
                && challenge.policies.len() <= vibeface_secure::authorization::MAX_POLICY_DOMAINS
                && challenge
                    .policies
                    .windows(2)
                    .all(|p| p[0].domain < p[1].domain),
            "invalid resolution freshness challenge"
        );
        let key = PublicKey::parse(&identity.public_key_hex()?)?;
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let mut receipts = Vec::new();
        for stamp in &challenge.policies {
            ensure!(
                self.policy.verified().head(&stamp.domain) == Some(stamp),
                "resolution freshness policy changed"
            );
            let authority = match &stamp.domain {
                PolicyDomain::Account(id) => {
                    &self
                        .policy
                        .verified()
                        .account(id)
                        .context("missing resolution account")?
                        .authority
                }
                PolicyDomain::Space(id) => {
                    &self
                        .policy
                        .verified()
                        .space(id)
                        .context("missing resolution space")?
                        .authority
                }
            };
            if authority.public_key == key {
                ensure!(
                    authority.device == DeviceId::from_key(&key)
                        && authority.service == ServiceId::from_key(&key),
                    "resolution authority mismatch"
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

    pub fn authorize_execution_handoff_resolution_fence(
        &self,
        auth: &VerifiedExecutionResolution,
        proof: &ExecutionHandoffResolutionFreshness,
        now_ms: u64,
    ) -> Result<()> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        ensure!(
            auth.wire().resolution.purpose == ExecutionResolutionPurpose::ResolveUnknown,
            "only an explicit unknown resolution can fence execution"
        );
        resolution_original_observation_from(&self.connection, &self.policy, auth.wire())?;
        check_policy_heads_in(
            &self.connection,
            self.policy.verified(),
            auth.resolver(),
            now_ms,
        )?;
        check_policy_heads_in(
            &self.connection,
            self.policy.verified(),
            auth.submission(),
            now_ms,
        )?;
        self.policy
            .verified()
            .verify_execution_resolution_freshness(auth, proof, now_ms)?;
        verify_resolution_challenge_record(&self.connection, auth.wire(), proof, false)
    }

    pub fn authorize_execution_handoff_terminal_reconciliation(
        &self,
        auth: &VerifiedExecutionResolution,
        proof: &ExecutionHandoffResolutionFreshness,
        now_ms: u64,
    ) -> Result<ExecutionHandoffTerminalReconciliationResult> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        ensure!(
            auth.wire().resolution.purpose == ExecutionResolutionPurpose::ReconcileTerminal,
            "terminal reconciliation cannot resolve an unknown executor operation"
        );
        resolution_original_observation_from(&self.connection, &self.policy, auth.wire())?;
        check_policy_heads_in(
            &self.connection,
            self.policy.verified(),
            auth.resolver(),
            now_ms,
        )?;
        check_policy_heads_in(
            &self.connection,
            self.policy.verified(),
            auth.submission(),
            now_ms,
        )?;
        self.policy
            .verified()
            .verify_execution_resolution_freshness(auth, proof, now_ms)?;
        verify_resolution_challenge_record(&self.connection, auth.wire(), proof, false)?;
        Ok(ExecutionHandoffTerminalReconciliationResult {
            reconciliation: auth.wire().clone(),
            freshness: proof.clone(),
        })
    }

    pub(crate) fn resolve_execution_handoff(
        &mut self,
        identity: &LocalIdentity,
        wire: &SignedExecutionHandoffResolution,
        proof: &ExecutionHandoffResolutionFreshness,
        transport: &TransportEvidence,
        stopped: &crate::engine::StoppedExecution,
        now_ms: u64,
    ) -> Result<ExecutionHandoffResolutionResult> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        ensure!(
            wire.resolution.purpose == ExecutionResolutionPurpose::ResolveUnknown,
            "terminal reconciliation cannot mutate executor state"
        );
        if let Some(result) = self.execution_handoff_resolution_status(wire, transport, now_ms)? {
            return Ok(result);
        }
        let auth = self.execution_handoff_resolution_authorization(wire, transport, now_ms)?;
        self.policy
            .verified()
            .admit_execution_resolution(&auth, proof, now_ms)?;
        let (execution, original, operation) = scope(&wire.resolution.observation.handoff)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut stored = resolution_original_observation_from(&tx, &self.policy, wire)?;
        ensure!(
            identity.public_key_hex()? == stored.wire.handoff.target_public_key.as_str(),
            "resolution signer is not this executor"
        );
        let dispatch: Event =
            serde_json::from_str(&stored.wire.handoff.project_dispatch.canonical_body)?;
        ensure!(
            stopped.matches_context(&crate::engine::ExecutionContext {
                space_id: SpaceId::parse(&original)?,
                resource_id: dispatch.resource_id.clone(),
                operation_id: operation.clone(),
                shared: true,
                lease_expires_at_ms: now_ms,
            }),
            "resolution has no matching confirmed execution fence"
        );
        check_policy_heads_in(&tx, self.policy.verified(), auth.resolver(), now_ms)?;
        check_policy_heads_in(&tx, self.policy.verified(), auth.submission(), now_ms)?;
        self.policy
            .verified()
            .verify_execution_resolution_freshness(&auth, proof, now_ms)?;
        verify_resolution_challenge_record(&tx, wire, proof, false)?;
        ensure!(tx.execute("UPDATE execution_handoff_resolution_challenges SET consumed=1 WHERE execution_space_id=? AND nonce=? AND consumed=0",
            params![execution,proof.challenge.nonce.as_str()])?==1,"resolution challenge was already consumed");
        let reused:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM execution_handoff_nonces WHERE execution_space_id=? AND device_id=? AND nonce=?)",
            params![execution,wire.invocation.payload.device.as_str(),wire.invocation.payload.nonce.as_str()],|row|row.get(0))?;
        ensure!(
            !reused,
            "resolution reused a prior executor invocation nonce"
        );
        stored.state = serde_json::from_value(
            wire.resolution.account_admission.source_body["outcome"].clone(),
        )?;
        stored.result = execution_resolution_result(&wire.resolution)?;
        ensure!(tx.execute("UPDATE execution_handoffs SET state=?,result=? WHERE execution_space_id=? AND original_space_id=? AND operation_id=? AND state='unknown' AND handoff_hash=?",
            params![state_text(&stored.state)?,serde_json::to_string(&stored.result)?,execution,original,operation,stored.wire.hash()?.as_str()])?==1,
            "unknown handoff changed before the explicit resolution");
        let (mut session, binding) = session_from(
            &tx,
            &execution,
            &original,
            dispatch.resource_id.as_str(),
            &stored.request.target_node,
            &stored.request.session_id,
        )?
        .context("resolved session disappeared")?;
        ensure!(
            stored.request.binding.as_ref() == Some(&binding),
            "resolved session binding changed"
        );
        session.revision = session.revision.max(
            stored
                .request
                .expected_revision
                .checked_add(1)
                .context("resolved session revision overflow")?,
        );
        session.native_session_id = None;
        session.state = "resolved".into();
        tx.execute("UPDATE execution_handoff_sessions SET revision=?,record=? WHERE execution_space_id=? AND original_space_id=? AND original_resource_id=? AND target_node=? AND session_id=?",
            params![session.revision,serde_json::to_string(&session)?,execution,original,dispatch.resource_id.as_str(),session.target_node,session.session_id])?;
        let handoff_receipt = signed_receipt(&tx, identity, &stored, now_ms)?;
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let receipt = Signed::sign(
            ExecutionHandoffResolutionReceipt {
                version: AUTH_VERSION,
                resolution_hash: wire.hash()?,
                freshness_hash: ObjectHash::body(proof)?,
                handoff_receipt_hash: handoff_receipt.hash()?,
                issued_at_ms: now_ms,
            },
            &signer,
        )?;
        let result = ExecutionHandoffResolutionResult {
            resolution: wire.clone(),
            freshness: proof.clone(),
            handoff_receipt,
            result: stored.result.clone(),
            receipt,
        };
        verify_execution_resolution_receipt_signature(&result)?;
        let account = &wire.resolution.account_admission.invocation;
        let submission = &wire.invocation;
        tx.execute("INSERT INTO execution_handoff_resolutions(execution_space_id,original_space_id,operation_id,resolution_id,resolution_hash,resolver_device,resolver_nonce,resolver_invocation_hash,resolver_body_hash,submission_device,submission_nonce,submission_invocation_hash,submission_body_hash,request,admission_transport,freshness,resolved_at_ms,result_receipt) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![execution,original,operation,account.payload.operation_id,wire.hash()?.as_str(),account.payload.device.as_str(),account.payload.nonce.as_str(),account.hash()?.as_str(),account.payload.body_hash.as_str(),
                submission.payload.device.as_str(),submission.payload.nonce.as_str(),submission.hash()?.as_str(),submission.payload.body_hash.as_str(),serde_json::to_string(wire)?,serde_json::to_string(&StoredTransport::from(transport))?,serde_json::to_string(proof)?,now_ms,serde_json::to_string(&result)?])?;
        tx.execute("UPDATE execution_handoffs SET result_receipt=? WHERE execution_space_id=? AND original_space_id=? AND operation_id=?",
            params![serde_json::to_string(&result.handoff_receipt)?,execution,original,operation])?;
        tx.execute("DELETE FROM execution_handoff_reservations WHERE execution_space_id=? AND original_space_id=? AND operation_id=?",params![execution,original,operation])?;
        tx.commit()?;
        Ok(result)
    }
}

fn save_lease_in(
    tx: &Transaction<'_>,
    identity: &LocalIdentity,
    auth: &VerifiedExecutionHandoff,
    proof: &ExecutionHandoffFreshness,
    now_ms: u64,
    deadline: u64,
) -> Result<()> {
    let (execution, original, operation) = scope(auth.wire())?;
    let stored = stored_from(tx, &execution, &original, &operation)?
        .context("lease acceptance checkpoint disappeared")?;
    ensure!(
        identity.public_key_hex()? == stored.wire.handoff.target_public_key.as_str(),
        "lease signer is not this executor"
    );
    let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
        .map_err(anyhow::Error::msg)?;
    let receipt = Signed::sign(
        ExecutionHandoffLeaseReceipt::new(
            &stored.wire,
            &stored.admission_receipt,
            proof,
            now_ms,
            deadline,
        )?,
        &signer,
    )?;
    let mut usage = verify_execution_handoff_lease_checkpoint(
        &stored.wire,
        &stored.admission_receipt,
        proof,
        now_ms,
        deadline,
        &receipt,
    )?;
    usage.merge(auth.proof_usage());
    usage.record_metadata(&proof.challenge)?;
    for receipt in &proof.receipts {
        usage.record_signed(receipt)?;
    }
    usage.enforce_limits()?;
    tx.execute(
        "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
        params![
            execution,
            lease_checkpoint_key(&stored.wire, proof)?,
            serde_json::to_string(&receipt)?
        ],
    )?;
    ensure!(tx.execute("UPDATE execution_handoffs SET lease_challenge=?,lease_receipts=?,lease_verified_at_ms=?,lease_deadline_ms=? WHERE execution_space_id=? AND original_space_id=? AND operation_id=?",
        params![serde_json::to_string(&proof.challenge)?, serde_json::to_string(&proof.receipts)?, now_ms, deadline, execution, original, operation])? == 1,
        "lease operation disappeared");
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
        if stored.lease.is_some() {
            verified_lease_deadline(connection, policy, &stored, None, None)?;
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
    let requests = connection.prepare("SELECT request FROM execution_handoff_resolutions ORDER BY execution_space_id,original_space_id,operation_id")?
        .query_map([], |row| row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for request in requests {
        let wire: SignedExecutionHandoffResolution = serde_json::from_str(&request)?;
        let saved =
            stored_resolution_from(connection, &wire)?.context("stored resolution disappeared")?;
        reconstruct_resolution(connection, policy, &saved)?;
        let (execution, original, operation) = scope(&wire.resolution.observation.handoff)?;
        let handoff = stored_from(connection, &execution, &original, &operation)?
            .context("resolved handoff disappeared")?;
        ensure!(
            handoff.wire == wire.resolution.observation.handoff
                && handoff.state.terminal()
                && handoff.result == saved.result.result
                && handoff.receipt.as_ref() == Some(&saved.result.handoff_receipt),
            "resolution audit differs from its terminal execution ledger"
        );
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        binding::{self, TaskBinding},
        identity::test_support::{TestNetwork, now},
        model::{AdmissionProof, EventCause, WIRE_VERSION},
    };
    use anyhow::Result;
    use vibeface_secure::{Identity, authorization::*};

    pub(crate) struct Fixture {
        pub(crate) project: TestNetwork,
        pub(crate) executor: TestNetwork,
        pub(crate) store: Store,
        pub(crate) wire: SignedExecutionHandoff,
        pub(crate) transport: TransportEvidence,
    }

    impl Fixture {
        pub(crate) fn new() -> Result<Self> {
            Self::with_executor("codex")
        }
        pub(crate) fn with_executor(executor_kind: &str) -> Result<Self> {
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
                    open_spec: json!({"type":"open","executor":executor_kind,"cwd":executor.project}),
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
                executor_kind: Some(executor_kind.into()),
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
            let original_admitted =
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
            let original_auth = store.policy.verified().verify_operation_authorization(
                &original_admitted,
                &facts,
                &source_transport,
                now(),
            )?;
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
            let mut event: Event = serde_json::from_str(&handoff.project_dispatch.canonical_body)?;
            let mut proof = capsule_admission(&event)?;
            proof.source_body["execution_resource"] = serde_json::to_value(&execution_resource)?;
            proof.source_body["target_public_key"] =
                serde_json::to_value(&handoff.target_public_key)?;
            proof.source_body["execution_grant"] = json!(leaf.hash()?);
            let mut payload = proof.invocation.payload.clone();
            payload.body_hash = ObjectHash::body(&proof.source_body)?;
            payload.nonce = Nonce::random()?;
            proof.invocation = Signed::sign(payload, &project.client)?;
            let mut final_facts = facts.clone();
            final_facts.body_hash = ObjectHash::body(&proof.source_body)?;
            let final_auth = store.policy.verified().verify_invocation(
                &proof.invocation,
                &final_facts,
                &source_transport,
                now(),
            )?;
            proof.challenge = FreshnessChallenge::new(
                &final_auth,
                CheckStage::Admit,
                Nonce::random()?,
                now(),
                now() + 10_000,
            )?;
            proof.receipts = store.sign_current_receipts(&proof.challenge, &project.node, now())?;
            proof.receipts.extend(store.sign_current_receipts(
                &proof.challenge,
                &executor.node,
                now(),
            )?);
            let final_fresh = store.policy.verified().verify_freshness(
                &final_auth,
                &proof.challenge,
                &proof.receipts,
                now(),
            )?;
            let final_admitted =
                store
                    .policy
                    .verified()
                    .admit_operation(&final_auth, &final_fresh, now())?;
            let original_auth = store.policy.verified().verify_operation_authorization(
                &final_admitted,
                &final_facts,
                &source_transport,
                now(),
            )?;
            event.cause = EventCause::Invocation { admission: proof };
            handoff.project_dispatch = project.node.sign_event(event)?;
            handoff.attest_source(&project_signer)?;
            let submission_facts = execution_submission_facts(&handoff, &original_auth)?;
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

        fn sdk() -> Result<Self> {
            let mut fixture = Self::new()?;
            let message = fixture.project.message(
                &mut fixture.store,
                "conversation",
                "private-user-message-DO-NOT-DISCLOSE",
            )?;
            let parent_event: Event = serde_json::from_str(&message.canonical_body)?;
            let parent = capsule_admission(&parent_event)?;
            let project_key = PublicKey::parse(&fixture.project.node.public_key_hex()?)?;
            let service = ServiceId::from_key(&project_key);
            let session = SessionRef {
                target: DeviceId::from_key(&project_key),
                session_id: message.hash.clone(),
            };
            let descriptor = fixture.store.agent_grant(
                fixture.project.space.as_str(),
                "conversation",
                &service,
                &session,
                &message.hash,
                now(),
            )?;
            let mut dispatch: Event =
                serde_json::from_str(&fixture.wire.handoff.project_dispatch.canonical_body)?;
            let mut proof = capsule_admission(&dispatch)?;
            proof.source_body["conversation_id"] = json!("conversation");
            proof.source_body["sdk_session"] = serde_json::to_value(&session)?;
            proof.source_body["parent_invocation"] = json!(parent.invocation.hash()?);
            let mut invocation = proof.invocation.payload.clone();
            invocation.actor = Actor::Service(service.clone());
            invocation.device = descriptor.device.clone();
            invocation.presenter = descriptor.device;
            invocation.initiator = descriptor.initiator;
            invocation.initiating_grant = descriptor.initiating_grant;
            invocation.grant_leaf = descriptor.grant_leaf;
            invocation.context.sdk_session = Some(session.clone());
            invocation.body_hash = ObjectHash::body(&proof.source_body)?;
            invocation.nonce = Nonce::random()?;
            invocation.expected_policies = descriptor.expected_policies;
            invocation.expected_policies.push(
                fixture
                    .store
                    .policy
                    .verified()
                    .head(&PolicyDomain::Account(fixture.executor.account.clone()))
                    .unwrap()
                    .clone(),
            );
            invocation
                .expected_policies
                .sort_by(|a, b| a.domain.cmp(&b.domain));
            invocation
                .expected_policies
                .dedup_by(|a, b| a.domain == b.domain);
            let signer = Identity::from_secret_hex(&fixture.project.node.secret_key_hex)
                .map_err(anyhow::Error::msg)?;
            proof.invocation = Signed::sign(invocation.clone(), &signer)?;
            let transport = TransportEvidence::LocalAgentAdapter { service, session };
            proof.transport = StoredTransport::from(&transport);
            let facts = RequestFacts {
                resource: invocation.resource,
                action: invocation.action,
                audience: invocation.audience,
                operation_id: invocation.operation_id,
                body_hash: invocation.body_hash,
                context: invocation.context,
            };
            let auth = fixture.store.policy.verified().verify_invocation(
                &proof.invocation,
                &facts,
                &transport,
                now(),
            )?;
            proof.challenge = FreshnessChallenge::new(
                &auth,
                CheckStage::Admit,
                Nonce::random()?,
                now(),
                now() + 10_000,
            )?;
            proof.receipts = fixture.store.sign_current_receipts(
                &proof.challenge,
                &fixture.project.node,
                now(),
            )?;
            proof.receipts.extend(fixture.store.sign_current_receipts(
                &proof.challenge,
                &fixture.executor.node,
                now(),
            )?);
            dispatch.cause = EventCause::Invocation { admission: proof };
            dispatch.sequence = parent_event.sequence + 1;
            dispatch.previous = Some(message.hash.clone());
            dispatch.parents = vec![message.hash.clone()];
            fixture.wire.handoff.project_dispatch = fixture.project.node.sign_event(dispatch)?;
            fixture.wire.handoff.sdk_origin = Some(CompactSdkOrigin {
                message_event: EventRef::parse(&message.hash)?,
                sdk_trigger: EventRef::parse(&message.hash)?,
                invocation: parent.invocation,
                challenge: parent.challenge,
                receipts: parent.receipts,
                admitted_at_ms: parent.admitted_at_ms,
                transport: parent.transport,
            });
            fixture.wire.handoff.attest_source(&signer)?;
            let original = original_authorization_from(
                &fixture.store.connection,
                &fixture.store.policy,
                fixture.store.policy.verified(),
                &fixture.wire,
                now(),
            )?;
            let facts = execution_submission_facts(&fixture.wire.handoff, &original)?;
            let mut carrier = fixture.wire.invocation.payload.clone();
            carrier.body_hash = facts.body_hash;
            carrier.context = facts.context;
            carrier.nonce = Nonce::random()?;
            fixture.wire.invocation = Signed::sign(carrier, &signer)?;
            Ok(fixture)
        }

        fn verify_builder_and_isolate_executor(&mut self) -> Result<()> {
            let outcome = self.store.ingest_event(
                self.project.space.as_str(),
                &self.wire.handoff.project_dispatch,
            )?;
            assert_eq!(outcome.status, "ready");
            let built = self.store.execution_handoff_payload(
                &self.project.node,
                self.project.space.as_str(),
                "original-operation",
                self.wire.handoff.execution_resource.clone(),
                self.wire.handoff.execution_audience.clone(),
                self.wire.handoff.target_public_key.clone(),
                now(),
            )?;
            assert_eq!(built, self.wire.handoff);
            let bytes = serde_json::to_string(&built)?;
            assert!(!bytes.contains("other-task-secret-DO-NOT-DISCLOSE"));
            assert!(!bytes.contains("private-user-message-DO-NOT-DISCLOSE"));
            let journal = self
                .store
                .connection
                .prepare("SELECT entry FROM policy_journal ORDER BY ordinal")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter()
                .map(|entry| serde_json::from_str::<JournalEntry>(&entry))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let pins = self.store.policy.pins().to_vec();
            let mut executor = Store::initialize(std::path::Path::new(":memory:"))?;
            let update = executor.policy.prepare_bootstrap(&pins, &journal, now())?;
            let tx = executor
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let state = super::super::journal::commit_in(&tx, &update)?;
            super::super::policy::project_policy_in(&tx, &state)?;
            tx.commit()?;
            executor.policy = state;
            executor.save_local_device(self.executor.space.as_str(), &self.executor.node)?;
            for resource in [
                &self.executor.resource,
                &self.wire.handoff.execution_resource.resource,
            ] {
                executor.local_set(
                    self.executor.space.as_str(),
                    &format!("resource-root:{resource}"),
                    &json!(self.executor.project),
                )?;
            }
            self.store = executor;
            let events: u64 =
                self.store
                    .connection
                    .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
            assert_eq!(events, 0);
            Ok(())
        }

        fn grant(
            &mut self,
            owner_project: bool,
            grantee: Actor,
            actions: Vec<Action>,
            resource: ResourceRef,
            audience: Audience,
            context: InvocationContext,
        ) -> Result<Signed<Grant>> {
            let owner = if owner_project {
                &self.project
            } else {
                &self.executor
            };
            let stamp = self
                .store
                .policy
                .verified()
                .head(&PolicyDomain::Space(owner.space.clone()))
                .unwrap()
                .clone();
            let grant = Signed::sign(
                Grant {
                    version: AUTH_VERSION,
                    owner_space: owner.space.clone(),
                    scope: ResourceScope::Exact(resource),
                    grantee: Grantee::Actor(grantee),
                    actions,
                    audience,
                    constraints: GrantConstraints {
                        context,
                        ..Default::default()
                    },
                    parent: None,
                    remaining_delegations: 0,
                    issued_under: stamp.clone(),
                    not_before_ms: now(),
                    expires_at_ms: now() + 1_000_000,
                    nonce: Nonce::random()?,
                },
                &owner.root,
            )?;
            let mut document = self
                .store
                .policy
                .verified()
                .space(&owner.space)
                .unwrap()
                .clone();
            document.previous = Some(stamp);
            document.sequence += 1;
            document.grants.push(grant.hash()?);
            document.grants.sort();
            self.store.install_policy_journal(
                &[
                    JournalEntry::Grant(grant.clone()),
                    JournalEntry::Space(Signed::sign(document, &owner.root)?),
                ],
                now() + 40,
            )?;
            Ok(grant)
        }

        pub(crate) fn resolution(
            &mut self,
            purpose: ExecutionResolutionPurpose,
        ) -> Result<SignedExecutionHandoffResolution> {
            let observed = self.store.execution_handoff_status(
                &self.executor.node,
                self.executor.space.as_str(),
                self.project.space.as_str(),
                "original-operation",
                now() + 41,
            )?;
            let observation = ExecutionHandoffResult {
                handoff: self.wire.clone(),
                receipt: observed.receipt.unwrap(),
                result: observed.result,
            };
            let dispatch: Event =
                serde_json::from_str(&self.wire.handoff.project_dispatch.canonical_body)?;
            let original = capsule_admission(&dispatch)?;
            let mut context = original.invocation.payload.context.clone();
            context.sdk_session = None;
            let project = original.invocation.payload.resource.clone();
            let audience = original.invocation.payload.audience.clone();
            let resolve_grant = self.grant(
                true,
                Actor::Account(self.executor.account.clone()),
                vec![Action::OperationResolve],
                project.clone(),
                audience.clone(),
                context.clone(),
            )?;
            let service = Actor::Service(audience.service.clone());
            let mut execution_context = context.clone();
            execution_context.workspace_resource =
                Some(self.wire.handoff.execution_resource.resource.clone());
            let carrier_grant = self.grant(
                false,
                service,
                vec![Action::ExecutionResolve, Action::OperationResolve],
                self.wire.handoff.execution_resource.clone(),
                self.wire.handoff.execution_audience.clone(),
                execution_context,
            )?;
            let outcome = if purpose == ExecutionResolutionPurpose::ResolveUnknown {
                json!("succeeded")
            } else {
                serde_json::to_value(observation.receipt.payload.state)?
            };
            let body = json!({"method":"resolve_unknown","resolution_purpose":purpose,"space_id":project.space,"resource_id":project.resource,
                "authorization_id":"explicit-resolution","operation_id":"original-operation","conversation_id":"conversation",
                "task_id":"task-handoff","task_ref":self.wire.handoff.task_creation.hash,"target_node":self.executor.node.node_id()?,
                "session_id":"same-session","outcome":outcome,"evidence":"verified stopped process and external side effects",
                "execution_resource":self.wire.handoff.execution_resource,"target_public_key":self.wire.handoff.target_public_key,
                "execution_grant":carrier_grant.hash()?,"original_handoff_hash":self.wire.hash()?,
                "original_body_hash":original.invocation.payload.body_hash,"unknown_receipt_hash":observation.receipt.hash()?,"execution_observation":observation});
            let mut account = original.invocation.payload.clone();
            account.actor = Actor::Account(self.executor.account.clone());
            account.initiator = account.actor.clone();
            account.initiating_grant = None;
            account.device = DeviceId::from_key(&PublicKey::of(&self.executor.client));
            account.presenter = account.device.clone();
            account.action = Action::OperationResolve;
            account.operation_id = "explicit-resolution".into();
            account.grant_leaf = resolve_grant.hash()?;
            account.body_hash = ObjectHash::body(&body)?;
            account.context = context;
            account.expected_policies = vec![];
            account.nonce = Nonce::random()?;
            account.not_before_ms = now() + 50;
            account.expires_at_ms = now() + 100_050;
            let transport = TransportEvidence::DirectNoise {
                device: account.device.clone(),
            };
            let mut proof = original;
            proof.source_body = body;
            proof.invocation = Signed::sign(account.clone(), &self.executor.client)?;
            proof.transport = StoredTransport::from(&transport);
            proof.admitted_at_ms = now() + 51;
            let mut resolution = ExecutionHandoffResolution {
                version: AUTH_VERSION,
                purpose,
                observation,
                account_admission: proof,
            };
            let facts = execution_resolution_account_facts(&resolution)?;
            let policy = self.store.policy.verified();
            let first = policy.verify_invocation(
                &resolution.account_admission.invocation,
                &facts,
                &transport,
                now() + 50,
            );
            let auth = match first {
                Err(AuthorizationError::NeedPolicySync(expected)) => {
                    account.expected_policies = expected;
                    resolution.account_admission.invocation =
                        Signed::sign(account, &self.executor.client)?;
                    policy.verify_invocation(
                        &resolution.account_admission.invocation,
                        &facts,
                        &transport,
                        now() + 50,
                    )?
                }
                other => other?,
            };
            let challenge = FreshnessChallenge::new(
                &auth,
                CheckStage::Admit,
                Nonce::random()?,
                now() + 50,
                now() + 10_050,
            )?;
            let mut receipts =
                self.store
                    .sign_current_receipts(&challenge, &self.project.node, now() + 50)?;
            receipts.extend(self.store.sign_current_receipts(
                &challenge,
                &self.executor.node,
                now() + 50,
            )?);
            let fresh = policy.verify_freshness(&auth, &challenge, &receipts, now() + 51)?;
            let admitted = policy.admit_operation(&auth, &fresh, now() + 51)?;
            resolution.account_admission.challenge = challenge;
            resolution.account_admission.receipts = receipts;
            let resolver =
                policy.verify_operation_authorization(&admitted, &facts, &transport, now() + 51)?;
            let facts = execution_resolution_facts(&resolution, &resolver)?;
            let descriptor = policy.verify_service_grant(
                &audience.service,
                &carrier_grant.hash()?,
                &facts,
                now() + 52,
            )?;
            let signer = Identity::from_secret_hex(&self.project.node.secret_key_hex)
                .map_err(anyhow::Error::msg)?;
            let invocation = Signed::sign(
                Invocation {
                    version: AUTH_VERSION,
                    actor: Actor::Service(audience.service),
                    device: descriptor.device().clone(),
                    initiator: descriptor.initiator().clone(),
                    initiating_grant: descriptor.initiating_grant().cloned(),
                    resource: facts.resource,
                    action: Action::ExecutionResolve,
                    audience: facts.audience,
                    presenter: descriptor.device().clone(),
                    operation_id: facts.operation_id,
                    body_hash: facts.body_hash,
                    grant_leaf: carrier_grant.hash()?,
                    expected_policies: descriptor.heads().to_vec(),
                    context: facts.context,
                    nonce: Nonce::random()?,
                    not_before_ms: now() + 52,
                    expires_at_ms: now() + 100_052,
                },
                &signer,
            )?;
            Ok(SignedExecutionHandoffResolution {
                resolution,
                invocation,
            })
        }

        fn resign_resolution_account(
            &self,
            wire: &mut SignedExecutionHandoffResolution,
        ) -> Result<()> {
            let proof = &mut wire.resolution.account_admission;
            let mut invocation = proof.invocation.payload.clone();
            invocation.operation_id = proof.source_body["authorization_id"]
                .as_str()
                .unwrap()
                .into();
            invocation.body_hash = ObjectHash::body(&proof.source_body)?;
            invocation.nonce = Nonce::random()?;
            proof.invocation = Signed::sign(invocation.clone(), &self.executor.client)?;
            let facts = RequestFacts {
                resource: invocation.resource,
                action: invocation.action,
                audience: invocation.audience,
                operation_id: invocation.operation_id,
                body_hash: invocation.body_hash,
                context: invocation.context,
            };
            let auth = self.store.policy.verified().verify_invocation(
                &proof.invocation,
                &facts,
                &proof.transport.clone().into_evidence(),
                now() + 50,
            )?;
            proof.challenge = FreshnessChallenge::new(
                &auth,
                CheckStage::Admit,
                Nonce::random()?,
                now() + 50,
                now() + 10_050,
            )?;
            proof.receipts = self.store.sign_current_receipts(
                &proof.challenge,
                &self.project.node,
                now() + 50,
            )?;
            proof.receipts.extend(self.store.sign_current_receipts(
                &proof.challenge,
                &self.executor.node,
                now() + 50,
            )?);
            self.store.policy.verified().verify_freshness(
                &auth,
                &proof.challenge,
                &proof.receipts,
                now() + 51,
            )?;
            let signer = Identity::from_secret_hex(&self.project.node.secret_key_hex)
                .map_err(anyhow::Error::msg)?;
            let mut carrier = wire.invocation.payload.clone();
            carrier.operation_id = proof.invocation.payload.operation_id.clone();
            carrier.body_hash = ObjectHash::body(&wire.resolution)?;
            carrier.nonce = Nonce::random()?;
            wire.invocation = Signed::sign(carrier, &signer)?;
            Ok(())
        }

        pub(crate) fn resolution_freshness(
            &mut self,
            wire: &SignedExecutionHandoffResolution,
            at: u64,
        ) -> Result<ExecutionHandoffResolutionFreshness> {
            let challenge = self.store.issue_execution_handoff_resolution_challenge(
                wire,
                &self.transport,
                at,
            )?;
            let mut receipts = self.store.sign_execution_handoff_resolution_receipts(
                &challenge,
                &self.project.node,
                at,
            )?;
            receipts.extend(self.store.sign_execution_handoff_resolution_receipts(
                &challenge,
                &self.executor.node,
                at,
            )?);
            Ok(ExecutionHandoffResolutionFreshness {
                challenge,
                receipts,
            })
        }

        async fn fence(&self, operation: &str) -> Result<crate::engine::StoppedExecution> {
            use std::os::unix::fs::PermissionsExt;
            let docker = self.executor.directory.join("fence-adapter.sh");
            std::fs::write(
                &docker,
                "#!/bin/sh\nprintf 'No such container\\n' >&2\nexit 1\n",
            )?;
            std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700))?;
            let (events, _) = tokio::sync::mpsc::unbounded_channel();
            let engine = crate::engine::Engine::new(
                crate::engine::ExecutorConfig {
                    execution_isolation: Some(crate::engine::ExecutionIsolationConfig {
                        image: format!("sha256:{}", "a".repeat(64)),
                        docker_bin: docker,
                        egress: None,
                    }),
                    data_dir: Some(self.executor.directory.clone()),
                    ..Default::default()
                },
                events,
            );
            let context = crate::engine::ExecutionContext {
                space_id: self.project.space.clone(),
                resource_id: self.project.resource.clone(),
                operation_id: operation.into(),
                shared: true,
                lease_expires_at_ms: now() + 1_000,
            };
            Ok(engine.fence_operation(&context).await?.0)
        }

        fn print_handoff_proof_counts(&mut self, label: &str) -> Result<()> {
            let proof = self.freshness(CheckStage::Admit, now())?;
            let usage = self.store.execution_handoff_proof_usage(
                &self.wire,
                &self.transport,
                Some(&proof),
                now(),
            )?;
            println!(
                "HANDOFF_PROOF_USAGE {}",
                serde_json::json!({"case":label,"capsule_transport_bytes":serde_json::to_vec(&self.wire)?.len(),
                "submit_rpc_json_bytes":serde_json::to_vec(&json!({"type":"execution_handoff_submit","handoff":self.wire}))?.len(),
                "freshness_json_bytes":serde_json::to_vec(&proof)?.len(),"proof":usage.counts(),"within_ordinary_limits":usage.enforce_limits().is_ok()})
            );
            Ok(())
        }
        pub(crate) fn start_unknown(&mut self) -> Result<()> {
            self.accept()?;
            let proof = self.freshness(CheckStage::Start, now() + 3)?;
            let auth = self.store.execution_handoff_authorization(
                self.executor.space.as_str(),
                self.project.space.as_str(),
                "original-operation",
                now() + 4,
            )?;
            let (_, fresh, _) = self.store.claim_execution_handoff(
                &self.executor.node,
                &auth,
                &proof,
                now() + 4,
            )?;
            assert!(fresh);
            self.store
                .load_execution_handoffs_for_recovery(&self.executor.node, now() + 5)?;
            Ok(())
        }

        pub(crate) fn freshness(
            &mut self,
            stage: CheckStage,
            at: u64,
        ) -> Result<ExecutionHandoffFreshness> {
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

        pub(crate) fn accept(&mut self) -> Result<ExecutionHandoffFreshness> {
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

    #[tokio::test]
    async fn independent_account_resolves_unknown_after_original_actor_revocation() -> Result<()> {
        let mut fixture = Fixture::new()?;
        fixture.start_unknown()?;
        let direct =
            fixture
                .project
                .request(&mut fixture.store, "unrelated-direct", "unrelated-session")?;
        fixture.project.admit(&mut fixture.store, &direct)?;
        fixture.project.revoke_client(&mut fixture.store)?;
        assert!(
            fixture
                .store
                .execution_handoff_authorization(
                    fixture.executor.space.as_str(),
                    fixture.project.space.as_str(),
                    "original-operation",
                    now() + 55
                )
                .is_err()
        );
        let wire = fixture.resolution(ExecutionResolutionPurpose::ResolveUnknown)?;
        let proof = fixture.resolution_freshness(&wire, now() + 55)?;
        let usage = fixture.store.execution_handoff_resolution_proof_usage(
            &wire,
            Some(&proof),
            &fixture.transport,
            now() + 55,
        )?;
        println!(
            "HANDOFF_PROOF_USAGE {}",
            json!({"case":"account_resolution_after_revocation","capsule_transport_bytes":serde_json::to_vec(&wire)?.len(),
            "submit_rpc_json_bytes":serde_json::to_vec(&json!({"type":"execution_handoff_resolve","resolution":wire}))?.len(),
            "freshness_json_bytes":serde_json::to_vec(&proof)?.len(),"proof":usage.counts(),"within_ordinary_limits":usage.enforce_limits().is_ok()})
        );
        let wrong_fence = fixture.fence("another-operation").await?;
        assert!(
            fixture
                .store
                .resolve_execution_handoff(
                    &fixture.executor.node,
                    &wire,
                    &proof,
                    &fixture.transport,
                    &wrong_fence,
                    now() + 56
                )
                .is_err()
        );
        let stopped = fixture.fence("original-operation").await?;
        let result = fixture.store.resolve_execution_handoff(
            &fixture.executor.node,
            &wire,
            &proof,
            &fixture.transport,
            &stopped,
            now() + 56,
        )?;
        assert_eq!(
            result.handoff_receipt.payload.state,
            ExecutionHandoffState::Succeeded
        );
        assert_eq!(
            result.result["original_actor"],
            json!(Actor::Account(fixture.project.account.clone()))
        );
        assert_eq!(
            result.result["resolver"],
            json!(Actor::Account(fixture.executor.account.clone()))
        );
        assert_ne!(result.result["original_actor"], result.result["resolver"]);
        verify_execution_resolution_receipt_signature(&result)?;
        let resolver = resolution_resolver_from(
            &fixture.store.policy,
            fixture.store.policy.verified(),
            &wire,
            now() + 200_000,
        )?;
        fixture
            .store
            .policy
            .verified()
            .verify_execution_resolution_ack(&result, &resolver, now() + 200_000)?;
        let before = fixture.store.connection.total_changes();
        let retry = fixture
            .store
            .execution_handoff_resolution_status(&wire, &fixture.transport, now() + 200_000)?
            .unwrap();
        assert_eq!(retry, result);
        assert_eq!(before, fixture.store.connection.total_changes());
        assert_eq!(
            fixture.store.connection.query_row(
                "SELECT COUNT(*) FROM execution_handoff_reservations",
                [],
                |r| r.get::<_, u64>(0)
            )?,
            0
        );
        assert_eq!(
            fixture.store.connection.query_row(
                "SELECT COUNT(*) FROM resource_reservations WHERE operation_id='unrelated-direct'",
                [],
                |r| r.get::<_, u64>(0)
            )?,
            1
        );
        let snapshot = result.handoff_receipt.payload.session.as_ref().unwrap();
        assert_eq!(snapshot.state, "resolved");
        assert_eq!(snapshot.revision, 1);
        let session = fixture
            .store
            .execution_handoff_session(
                fixture.executor.space.as_str(),
                &ResourceRef {
                    space: fixture.project.space.clone(),
                    resource: fixture.project.resource.clone(),
                },
                &fixture.executor.node.node_id()?,
                "same-session",
            )?
            .unwrap();
        assert!(session.native_session_id.is_none());
        validate_execution_handoff_storage(&fixture.store.connection, &fixture.store.policy)?;
        let mut altered = wire.clone();
        altered.resolution.account_admission.source_body["evidence"] =
            json!("changed external outcome evidence");
        assert!(
            fixture
                .store
                .execution_handoff_resolution_status(&altered, &fixture.transport, now() + 200_001)
                .is_err()
        );
        fixture.executor.revoke_client(&mut fixture.store)?;
        assert!(
            fixture
                .store
                .execution_handoff_resolution_status(&wire, &fixture.transport, now() + 200_002)
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolution_failure_keeps_unknown_nonce_and_reservations_atomic() -> Result<()> {
        let mut fixture = Fixture::new()?;
        fixture.start_unknown()?;
        let wire = fixture.resolution(ExecutionResolutionPurpose::ResolveUnknown)?;
        let proof = fixture.resolution_freshness(&wire, now() + 55)?;
        let stopped = fixture.fence("original-operation").await?;
        let auth = fixture.store.execution_handoff_resolution_authorization(
            &wire,
            &fixture.transport,
            now() + 56,
        )?;
        let mut missing = proof.clone();
        missing.receipts.pop();
        let before = fixture.store.connection.total_changes();
        assert!(
            fixture
                .store
                .authorize_execution_handoff_resolution_fence(&auth, &missing, now() + 56)
                .is_err()
        );
        fixture
            .store
            .authorize_execution_handoff_resolution_fence(&auth, &proof, now() + 56)?;
        assert!(
            fixture
                .store
                .authorize_execution_handoff_resolution_fence(
                    &auth,
                    &proof,
                    proof.challenge.expires_at_ms
                )
                .is_err()
        );
        assert_eq!(before, fixture.store.connection.total_changes());
        let before = fixture.store.connection.total_changes();
        assert!(
            fixture
                .store
                .resolve_execution_handoff(
                    &fixture.executor.node,
                    &wire,
                    &missing,
                    &fixture.transport,
                    &stopped,
                    now() + 56
                )
                .is_err()
        );
        assert_eq!(before, fixture.store.connection.total_changes());
        fixture.store.connection.execute_batch("CREATE TRIGGER reject_handoff_resolution BEFORE INSERT ON execution_handoff_resolutions BEGIN SELECT RAISE(ABORT,'injected audit failure'); END")?;
        assert!(
            fixture
                .store
                .resolve_execution_handoff(
                    &fixture.executor.node,
                    &wire,
                    &proof,
                    &fixture.transport,
                    &stopped,
                    now() + 56
                )
                .is_err()
        );
        let state: String = fixture.store.connection.query_row(
            "SELECT state FROM execution_handoffs",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(state, "unknown");
        assert_eq!(
            fixture.store.connection.query_row(
                "SELECT COUNT(*) FROM execution_handoff_resolutions",
                [],
                |r| r.get::<_, u64>(0)
            )?,
            0
        );
        assert_eq!(
            fixture.store.connection.query_row(
                "SELECT consumed FROM execution_handoff_resolution_challenges",
                [],
                |r| r.get::<_, bool>(0)
            )?,
            false
        );
        assert_eq!(
            fixture.store.connection.query_row(
                "SELECT COUNT(*) FROM execution_handoff_reservations",
                [],
                |r| r.get::<_, u64>(0)
            )?,
            1
        );
        fixture
            .store
            .connection
            .execute_batch("DROP TRIGGER reject_handoff_resolution")?;
        fixture.store.resolve_execution_handoff(
            &fixture.executor.node,
            &wire,
            &proof,
            &fixture.transport,
            &stopped,
            now() + 57,
        )?;
        validate_execution_handoff_storage(&fixture.store.connection, &fixture.store.policy)?;
        Ok(())
    }

    #[tokio::test]
    async fn fully_signed_resolution_rejects_changed_tuple_and_only_one_decision_can_win()
    -> Result<()> {
        let mut fixture = Fixture::new()?;
        fixture.start_unknown()?;
        let wire = fixture.resolution(ExecutionResolutionPurpose::ResolveUnknown)?;
        let wrong_values = vec![
            ("operation_id", json!("another-operation")),
            ("session_id", json!("another-session")),
            ("task_ref", json!("f".repeat(64))),
            (
                "original_body_hash",
                json!(format!("h2_{}", "f".repeat(64))),
            ),
            ("execution_grant", json!(format!("h2_{}", "f".repeat(64)))),
            ("conversation_id", json!("another-conversation")),
            (
                "unknown_receipt_hash",
                json!(format!("h2_{}", "f".repeat(64))),
            ),
        ];
        for (field, value) in wrong_values {
            let mut altered = wire.clone();
            altered.resolution.account_admission.source_body[field] = value;
            fixture.resign_resolution_account(&mut altered)?;
            let before = fixture.store.connection.total_changes();
            assert!(
                fixture
                    .store
                    .issue_execution_handoff_resolution_challenge(
                        &altered,
                        &fixture.transport,
                        now() + 55
                    )
                    .is_err(),
                "changed {field} must be rejected"
            );
            assert_eq!(
                before,
                fixture.store.connection.total_changes(),
                "changed {field} must write nothing"
            );
        }
        let mut other = wire.clone();
        other.resolution.account_admission.source_body["authorization_id"] =
            json!("opposite-resolution");
        other.resolution.account_admission.source_body["outcome"] = json!("failed");
        fixture.resign_resolution_account(&mut other)?;
        let proof = fixture.resolution_freshness(&wire, now() + 55)?;
        let other_proof = fixture.resolution_freshness(&other, now() + 55)?;
        let stopped = fixture.fence("original-operation").await?;
        fixture.store.resolve_execution_handoff(
            &fixture.executor.node,
            &wire,
            &proof,
            &fixture.transport,
            &stopped,
            now() + 56,
        )?;
        let before = fixture.store.connection.total_changes();
        assert!(
            fixture
                .store
                .resolve_execution_handoff(
                    &fixture.executor.node,
                    &other,
                    &other_proof,
                    &fixture.transport,
                    &stopped,
                    now() + 57
                )
                .is_err()
        );
        assert_eq!(before, fixture.store.connection.total_changes());
        assert_eq!(
            fixture.store.connection.query_row(
                "SELECT state FROM execution_handoffs",
                [],
                |r| r.get::<_, String>(0)
            )?,
            "succeeded"
        );
        assert_eq!(
            fixture.store.connection.query_row(
                "SELECT COUNT(*) FROM execution_handoff_resolutions",
                [],
                |r| r.get::<_, u64>(0)
            )?,
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolution_rechecks_both_current_devices_and_preserves_unknown_after_revocation()
    -> Result<()> {
        for revoke_project in [true, false] {
            let mut fixture = Fixture::new()?;
            fixture.start_unknown()?;
            let wire = fixture.resolution(ExecutionResolutionPurpose::ResolveUnknown)?;
            let proof = fixture.resolution_freshness(&wire, now() + 55)?;
            let stopped = fixture.fence("original-operation").await?;
            if revoke_project {
                fixture.project.revoke_node(&mut fixture.store)?;
            } else {
                fixture.executor.revoke_node(&mut fixture.store)?;
            }
            let before = fixture.store.connection.total_changes();
            assert!(
                fixture
                    .store
                    .resolve_execution_handoff(
                        &fixture.executor.node,
                        &wire,
                        &proof,
                        &fixture.transport,
                        &stopped,
                        now() + 56
                    )
                    .is_err()
            );
            assert_eq!(before, fixture.store.connection.total_changes());
            assert_eq!(
                fixture.store.connection.query_row(
                    "SELECT state FROM execution_handoffs",
                    [],
                    |r| r.get::<_, String>(0)
                )?,
                "unknown"
            );
            assert_eq!(
                fixture.store.connection.query_row(
                    "SELECT COUNT(*) FROM execution_handoff_reservations",
                    [],
                    |r| r.get::<_, u64>(0)
                )?,
                1
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn terminal_reconciliation_is_purpose_bound_and_never_mutates_executor_state()
    -> Result<()> {
        let mut fixture = Fixture::new()?;
        fixture.accept()?;
        let proof = fixture.freshness(CheckStage::Start, now() + 3)?;
        let auth = fixture.store.execution_handoff_authorization(
            fixture.executor.space.as_str(),
            fixture.project.space.as_str(),
            "original-operation",
            now() + 4,
        )?;
        fixture
            .store
            .claim_execution_handoff(&fixture.executor.node, &auth, &proof, now() + 4)?;
        fixture.store.finish_execution_handoff(
            &fixture.executor.node,
            fixture.executor.space.as_str(),
            fixture.project.space.as_str(),
            "original-operation",
            OperationState::Succeeded,
            json!({"provider":"completed before response was lost"}),
            None,
            now() + 5,
        )?;
        fixture.project.revoke_client(&mut fixture.store)?;
        let wire = fixture.resolution(ExecutionResolutionPurpose::ReconcileTerminal)?;
        let proof = fixture.resolution_freshness(&wire, now() + 55)?;
        let auth = fixture.store.execution_handoff_resolution_authorization(
            &wire,
            &fixture.transport,
            now() + 56,
        )?;
        let before = fixture.store.connection.total_changes();
        let result = fixture
            .store
            .authorize_execution_handoff_terminal_reconciliation(&auth, &proof, now() + 56)?;
        assert_eq!(before, fixture.store.connection.total_changes());
        let branded = fixture
            .store
            .policy
            .verified()
            .verify_execution_terminal_reconciliation(&result, auth.resolver(), now() + 56)?;
        assert_eq!(
            branded.resolver().actor(),
            &Actor::Account(fixture.executor.account.clone())
        );
        assert_eq!(
            fixture.store.connection.query_row(
                "SELECT COUNT(*) FROM execution_handoff_resolutions",
                [],
                |r| r.get::<_, u64>(0)
            )?,
            0
        );
        let stopped = fixture.fence("original-operation").await?;
        assert!(
            fixture
                .store
                .resolve_execution_handoff(
                    &fixture.executor.node,
                    &wire,
                    &proof,
                    &fixture.transport,
                    &stopped,
                    now() + 56
                )
                .is_err()
        );
        let before = fixture.store.connection.total_changes();
        let mut opposite = wire.clone();
        opposite.resolution.account_admission.source_body["outcome"] = json!("failed");
        assert!(
            fixture
                .store
                .issue_execution_handoff_resolution_challenge(
                    &opposite,
                    &fixture.transport,
                    now() + 57
                )
                .is_err()
        );
        assert_eq!(before, fixture.store.connection.total_changes());
        Ok(())
    }

    #[test]
    fn independent_executor_keeps_original_scope_and_never_restarts_unknown() -> Result<()> {
        let mut fixture = Fixture::new()?;
        fixture.verify_builder_and_isolate_executor()?;
        fixture.print_handoff_proof_counts("account_production_builder")?;
        assert!(
            !serde_json::to_string(&fixture.wire)?.contains("other-task-secret-DO-NOT-DISCLOSE")
        );
        let admission = fixture.accept()?;
        let checkpoint = stored_from(
            &fixture.store.connection,
            fixture.executor.space.as_str(),
            fixture.project.space.as_str(),
            "original-operation",
        )?
        .unwrap()
        .admission_receipt;
        let signer = Identity::from_secret_hex(&fixture.executor.node.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        for field in [
            "handoff_hash",
            "submission_invocation_hash",
            "original_invocation_hash",
            "original_request_hash",
            "admission_hash",
            "transport_hash",
            "admitted_at_ms",
            "version",
        ] {
            let mut payload = serde_json::to_value(&checkpoint.payload)?;
            payload[field] = if field == "version" {
                json!(AUTH_VERSION + 1)
            } else if field == "admitted_at_ms" {
                json!(checkpoint.payload.admitted_at_ms + 1)
            } else {
                json!(ObjectHash::body(&field)?)
            };
            let altered = Signed::sign(
                serde_json::from_value::<ExecutionHandoffAdmissionReceipt>(payload)?,
                &signer,
            )?;
            fixture.store.connection.execute(
                "UPDATE execution_handoffs SET admission_receipt=?",
                [serde_json::to_string(&altered)?],
            )?;
            let before = fixture.store.connection.total_changes();
            assert!(
                fixture
                    .store
                    .execution_handoff_authorization(
                        fixture.executor.space.as_str(),
                        fixture.project.space.as_str(),
                        "original-operation",
                        now() + 2
                    )
                    .is_err(),
                "accepted replaced checkpoint field {field}"
            );
            assert_eq!(before, fixture.store.connection.total_changes());
        }
        fixture.store.connection.execute(
            "UPDATE execution_handoffs SET admission_receipt=?",
            [serde_json::to_string(&checkpoint)?],
        )?;
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
        let read = fixture.freshness(CheckStage::RemoteRead, now() + 3)?;
        let read_auth = fixture.store.execution_handoff_authorization(
            fixture.executor.space.as_str(),
            fixture.project.space.as_str(),
            "original-operation",
            now() + 4,
        )?;
        let before = fixture.store.connection.total_changes();
        fixture
            .store
            .authorize_execution_handoff_read(&read_auth, &read, now() + 4)?;
        assert_eq!(before, fixture.store.connection.total_changes());
        assert!(
            fixture
                .store
                .claim_execution_handoff(&fixture.executor.node, &read_auth, &read, now() + 4)
                .is_err()
        );
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
    fn sdk_capsule_preserves_account_origin_without_replicating_private_messages() -> Result<()> {
        let mut fixture = Fixture::sdk()?;
        fixture.verify_builder_and_isolate_executor()?;
        fixture.print_handoff_proof_counts("sdk_production_builder")?;
        let bytes = serde_json::to_string(&fixture.wire)?;
        assert!(!bytes.contains("private-user-message-DO-NOT-DISCLOSE"));
        assert!(!bytes.contains("other-task-secret-DO-NOT-DISCLOSE"));
        fixture.accept()?;
        let proof = fixture.freshness(CheckStage::Start, now() + 3)?;
        let auth = fixture.store.execution_handoff_authorization(
            fixture.executor.space.as_str(),
            fixture.project.space.as_str(),
            "original-operation",
            now() + 4,
        )?;
        assert!(matches!(auth.original().actor(), Actor::Service(_)));
        assert_eq!(
            auth.original().initiator(),
            &Actor::Account(fixture.project.account.clone())
        );
        assert_eq!(
            auth.submission().initiator(),
            &Actor::Account(fixture.executor.account.clone())
        );
        fixture
            .store
            .claim_execution_handoff(&fixture.executor.node, &auth, &proof, now() + 4)?;
        let project = auth.original().resource().clone();
        fixture.store.persist_execution_handoff_native(
            fixture.executor.space.as_str(),
            &project,
            "original-operation",
            "native-stable",
            now() + 5,
        )?;
        assert_eq!(
            fixture
                .store
                .execution_handoff_session(
                    fixture.executor.space.as_str(),
                    &project,
                    &fixture.executor.node.node_id()?,
                    "same-session"
                )?
                .unwrap()
                .native_session_id
                .as_deref(),
            Some("native-stable")
        );
        let renewal = fixture.freshness(CheckStage::Renew, now() + 6)?;
        let stored = stored_from(
            &fixture.store.connection,
            fixture.executor.space.as_str(),
            fixture.project.space.as_str(),
            "original-operation",
        )?
        .unwrap();
        let lease = &stored.lease.as_ref().unwrap().0;
        let key = lease_checkpoint_key(&fixture.wire, lease)?;
        let encoded: String = fixture.store.connection.query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![fixture.executor.space.as_str(), key],
            |row| row.get(0),
        )?;
        let original_checkpoint: Signed<ExecutionHandoffLeaseReceipt> =
            serde_json::from_str(&encoded)?;
        let signer = Identity::from_secret_hex(&fixture.executor.node.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        for field in [
            "wrong_c",
            "handoff_hash",
            "admission_receipt_hash",
            "freshness_hash",
            "verified_at_ms",
            "deadline_ms",
        ] {
            let mut payload = serde_json::to_value(&original_checkpoint.payload)?;
            if field.ends_with("_hash") {
                payload[field] = json!(ObjectHash::body(&field)?);
            }
            if field == "verified_at_ms" {
                payload[field] = json!(original_checkpoint.payload.verified_at_ms + 1);
            }
            if field == "deadline_ms" {
                payload[field] = json!(original_checkpoint.payload.deadline_ms + 1);
            }
            let altered = Signed::sign(
                serde_json::from_value::<ExecutionHandoffLeaseReceipt>(payload)?,
                if field == "wrong_c" {
                    &fixture.project.client
                } else {
                    &signer
                },
            )?;
            fixture.store.connection.execute(
                "UPDATE local_values SET value=? WHERE space_id=? AND key=?",
                params![
                    serde_json::to_string(&altered)?,
                    fixture.executor.space.as_str(),
                    key
                ],
            )?;
            let before = fixture.store.connection.total_changes();
            assert!(
                fixture
                    .store
                    .renew_execution_handoff(&fixture.executor.node, &auth, &renewal, now() + 7)
                    .is_err(),
                "accepted replaced lease checkpoint field {field}"
            );
            assert_eq!(before, fixture.store.connection.total_changes());
        }
        fixture.store.connection.execute(
            "UPDATE local_values SET value=? WHERE space_id=? AND key=?",
            params![encoded, fixture.executor.space.as_str(), key],
        )?;
        fixture.store.connection.execute(
            "UPDATE execution_handoffs SET lease_deadline_ms=lease_deadline_ms+1",
            [],
        )?;
        assert!(
            fixture
                .store
                .renew_execution_handoff(&fixture.executor.node, &auth, &renewal, now() + 7)
                .is_err()
        );
        fixture.store.connection.execute(
            "UPDATE execution_handoffs SET lease_deadline_ms=lease_deadline_ms-1",
            [],
        )?;
        fixture.store.renew_execution_handoff(
            &fixture.executor.node,
            &auth,
            &renewal,
            now() + 7,
        )?;
        let new_key = lease_checkpoint_key(&fixture.wire, &renewal)?;
        let new_checkpoint: String = fixture.store.connection.query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![fixture.executor.space.as_str(), new_key],
            |row| row.get(0),
        )?;
        fixture.store.connection.execute(
            "UPDATE local_values SET value=? WHERE space_id=? AND key=?",
            params![
                serde_json::to_string(&original_checkpoint)?,
                fixture.executor.space.as_str(),
                new_key
            ],
        )?;
        let before = fixture.store.connection.total_changes();
        assert!(
            fixture
                .store
                .persist_execution_handoff_native(
                    fixture.executor.space.as_str(),
                    &project,
                    "original-operation",
                    "native-stable",
                    now() + 8
                )
                .is_err()
        );
        assert_eq!(before, fixture.store.connection.total_changes());
        fixture.store.connection.execute(
            "UPDATE local_values SET value=? WHERE space_id=? AND key=?",
            params![new_checkpoint, fixture.executor.space.as_str(), new_key],
        )?;
        validate_execution_handoff_storage(&fixture.store.connection, &fixture.store.policy)?;
        Ok(())
    }

    #[test]
    fn task_capsule_rejects_cross_task_missing_origin_and_changed_inputs() -> Result<()> {
        let mut fixture = Fixture::sdk()?;
        let signer = Identity::from_secret_hex(&fixture.project.node.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let resign_carrier = |wire: &mut SignedExecutionHandoff| -> Result<()> {
            let mut carrier = wire.invocation.payload.clone();
            carrier.body_hash = ObjectHash::body(&wire.handoff)?;
            wire.invocation = Signed::sign(carrier, &signer)?;
            Ok(())
        };
        let mut altered = fixture.wire.clone();
        altered.handoff.task_creation = fixture.store.task_creation(
            fixture.project.space.as_str(),
            "conversation",
            "other-task-secret-DO-NOT-DISCLOSE",
        )?;
        altered.handoff.attest_source(&signer)?;
        let mut carrier = altered.invocation.payload.clone();
        carrier.body_hash = ObjectHash::body(&altered.handoff)?;
        altered.invocation = Signed::sign(carrier, &signer)?;
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
        let mut altered = fixture.wire.clone();
        altered.handoff.sdk_origin = None;
        altered.handoff.attest_source(&signer)?;
        resign_carrier(&mut altered)?;
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
        let mut altered = fixture.wire.clone();
        altered.handoff.source_attestation = None;
        resign_carrier(&mut altered)?;
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
        let mut altered = fixture.wire.clone();
        let mut task: Event = serde_json::from_str(&altered.handoff.task_creation.canonical_body)?;
        if let EventBody::TaskCreated { goal, .. } = &mut task.body {
            *goal = "changed task input".into();
        }
        altered.handoff.task_creation = fixture.project.node.sign_event(task)?;
        altered.handoff.attest_source(&signer)?;
        resign_carrier(&mut altered)?;
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
        let mut historical = fixture.wire.clone();
        historical.handoff.sdk_origin.as_mut().unwrap().receipts[0]
            .payload
            .expires_at_ms += 1;
        historical.handoff.attest_source(&signer)?;
        resign_carrier(&mut historical)?;
        fixture.store.issue_execution_handoff_challenge(
            &historical,
            &fixture.transport,
            CheckStage::Admit,
            now(),
        )?;
        let mut altered = fixture.wire.clone();
        altered
            .handoff
            .sdk_origin
            .as_mut()
            .unwrap()
            .invocation
            .payload
            .body_hash = ObjectHash::body(&"forged Account message")?;
        altered.handoff.attest_source(&signer)?;
        resign_carrier(&mut altered)?;
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
        let mut altered = fixture.wire.clone();
        altered.handoff.attest_source(&fixture.project.client)?;
        resign_carrier(&mut altered)?;
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
        let mut altered = fixture.wire.clone();
        let mut event: Event =
            serde_json::from_str(&altered.handoff.project_dispatch.canonical_body)?;
        let EventCause::Invocation { admission } = &mut event.cause else {
            unreachable!()
        };
        admission.source_body["request"]["resources"] = json!([]);
        let EventBody::DispatchRecorded { action, .. } = &mut event.body else {
            unreachable!()
        };
        *action = admission.source_body["request"].clone();
        altered.handoff.project_dispatch = fixture.project.node.sign_event(event)?;
        altered.handoff.attest_source(&signer)?;
        resign_carrier(&mut altered)?;
        altered.invocation.verify_signature()?;
        altered
            .handoff
            .source_attestation
            .as_ref()
            .unwrap()
            .verify_signature()?;
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
        let mut altered = fixture.wire.clone();
        let mut event: Event =
            serde_json::from_str(&altered.handoff.project_dispatch.canonical_body)?;
        let EventCause::Invocation { admission } = &mut event.cause else {
            unreachable!()
        };
        admission.source_body["conversation_id"] = json!("another-conversation");
        let mut invocation = admission.invocation.payload.clone();
        invocation.body_hash = ObjectHash::body(&admission.source_body)?;
        invocation.nonce = Nonce::random()?;
        admission.invocation = Signed::sign(invocation.clone(), &signer)?;
        let facts = RequestFacts {
            resource: invocation.resource,
            action: invocation.action,
            audience: invocation.audience,
            operation_id: invocation.operation_id,
            body_hash: invocation.body_hash,
            context: invocation.context,
        };
        let auth = fixture.store.policy.verified().verify_invocation(
            &admission.invocation,
            &facts,
            &admission.transport.clone().into_evidence(),
            now(),
        )?;
        admission.challenge = FreshnessChallenge::new(
            &auth,
            CheckStage::Admit,
            Nonce::random()?,
            now(),
            now() + 10_000,
        )?;
        admission.receipts = fixture.store.sign_current_receipts(
            &admission.challenge,
            &fixture.project.node,
            now(),
        )?;
        admission
            .receipts
            .extend(fixture.store.sign_current_receipts(
                &admission.challenge,
                &fixture.executor.node,
                now(),
            )?);
        fixture.store.policy.verified().verify_freshness(
            &auth,
            &admission.challenge,
            &admission.receipts,
            now(),
        )?;
        altered.handoff.project_dispatch = fixture.project.node.sign_event(event)?;
        altered.handoff.attest_source(&signer)?;
        resign_carrier(&mut altered)?;
        let error = fixture
            .store
            .issue_execution_handoff_challenge(
                &altered,
                &fixture.transport,
                CheckStage::Admit,
                now(),
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("capsule operation scope changed")
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
                .renew_execution_handoff(&fixture.executor.node, &auth, &proof, now() + 5)
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
        let wrapper = ExecutionHandoffResult {
            handoff: fixture.wire.clone(),
            receipt: completed.receipt.clone().unwrap(),
            result: result.clone(),
        };
        let source_event: Event =
            serde_json::from_str(&fixture.wire.handoff.project_dispatch.canonical_body)?;
        let mut events =
            load_events_from(&fixture.store.connection, fixture.project.space.as_str())?;
        events.insert(
            fixture.wire.handoff.project_dispatch.hash.clone(),
            StoredEvent {
                envelope: fixture.wire.handoff.project_dispatch.clone(),
                event: source_event.clone(),
                status: "ready".into(),
            },
        );
        let observation = Event {
            version: WIRE_VERSION,
            network_id: fixture.project.space.to_string(),
            resource_id: fixture.project.resource.clone(),
            cause: EventCause::Derived {
                admission: capsule_admission(&source_event)?,
                source: fixture.wire.handoff.project_dispatch.hash.clone(),
            },
            scope_id: "conversation".into(),
            author: fixture.project.node.node_id()?,
            sequence: source_event.sequence + 1,
            previous: Some(fixture.wire.handoff.project_dispatch.hash.clone()),
            parents: vec![fixture.wire.handoff.project_dispatch.hash.clone()],
            blobs: vec![],
            created_at_ms: (now() + 7) as i64,
            body: EventBody::ExecutionObserved {
                operation_id: "original-operation".into(),
                task_id: Some("task-handoff".into()),
                target_node: fixture.executor.node.node_id()?,
                session_id: "same-session".into(),
                status: "succeeded".into(),
                detail: serde_json::to_value(&wrapper)?,
            },
        };
        assert_eq!(
            super::super::event_authorization::validate_event_evidence(
                &fixture.store.policy,
                &fixture.store.connection,
                &observation,
                &events
            )?,
            super::super::event_authorization::EventEvidenceStatus::Valid
        );
        let mut altered = observation.clone();
        if let EventBody::ExecutionObserved { detail, .. } = &mut altered.body {
            detail["result"] = json!({"forged":"result"});
        }
        assert!(
            super::super::event_authorization::validate_event_evidence(
                &fixture.store.policy,
                &fixture.store.connection,
                &altered,
                &events
            )
            .is_err()
        );
        fixture
            .store
            .policy
            .verified()
            .verify_execution_handoff_ack(
                &fixture.wire,
                auth.original(),
                completed.receipt.as_ref().unwrap(),
                &result,
                now() + 7,
            )?;
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
