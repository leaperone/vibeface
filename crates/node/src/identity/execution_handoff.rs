use super::{admission::check_policy_heads_in, journal::PolicyState};
use crate::{
    crypto::LocalIdentity,
    engine::{EngineAction, ExecutorKind},
    model::{Event, EventBody, ExecutionSession, OperationRequest, OperationState},
    store::{
        Store, StoredEvent, canonical_resource, check_execution_resources_in,
        check_execution_session_in,
    },
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use vibeface_secure::authorization::{
    AUTH_VERSION, CheckStage, DeviceId, FreshnessReceipt, Nonce, ObjectHash, PolicyDomain,
    PolicyStamp, PublicKey, ResourceId, ResourceRef, ServiceId, Signed, SpaceId, TransportEvidence,
    VerifiedAuthorization, VerifiedPolicySet,
    events::StoredTransport,
    execution_handoff::*,
    execution_work::ExecutionWork,
    remote_execution_history::RemoteExecutionHistoryAcceptance,
    task_binding::SignedBinding,
    task_business_policy::{
        TaskBusinessScopedView, TaskBusinessWire, original::VerifiedTaskBusinessOriginalSource,
        original_operation::VerifiedTaskBusinessOperationCurrent,
    },
};

#[path = "execution_observation.rs"]
mod observation;

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

#[derive(Clone, Copy)]
enum HandoffPolicy<'a> {
    Primary,
    TaskBusiness(&'a TaskBusinessScopedView),
}

impl HandoffPolicy<'_> {
    fn state<'a>(&'a self, primary: &'a PolicyState) -> &'a VerifiedPolicySet {
        match self {
            Self::Primary => primary.verified(),
            Self::TaskBusiness(view) => view.state(),
        }
    }

    fn check_business_wire_in(
        &self,
        connection: &Connection,
        primary: &PolicyState,
        wire: &TaskBusinessWire,
        now_ms: u64,
    ) -> Result<()> {
        if let Self::TaskBusiness(view) = self {
            super::task_business_policy::check_task_business_policy_view_in(
                connection, primary, view, wire, now_ms,
            )?;
        }
        Ok(())
    }

    fn check_wire_in(
        &self,
        connection: &Connection,
        primary: &PolicyState,
        wire: &SignedExecutionHandoff,
        now_ms: u64,
    ) -> Result<()> {
        self.check_business_wire_in(
            connection,
            primary,
            &TaskBusinessWire::Submit(wire.clone()),
            now_ms,
        )
    }

    fn check_heads_in(
        &self,
        connection: &Connection,
        primary: &PolicyState,
        wire: &TaskBusinessWire,
        heads: &[PolicyStamp],
        now_ms: u64,
    ) -> Result<()> {
        match self {
            Self::Primary => observation::check_heads_in(connection, primary.verified(), heads),
            Self::TaskBusiness(view) => {
                self.check_business_wire_in(connection, primary, wire, now_ms)?;
                ensure!(
                    heads
                        .iter()
                        .all(
                            |expected| view.state().head(&expected.domain) == Some(expected)
                                && (view.primary_heads().contains(expected)
                                    || view.foreign_heads().contains(expected))
                        ),
                    "Task business authorization escaped its exact source policy heads"
                );
                Ok(())
            }
        }
    }

    fn bind_original_in(
        &self,
        connection: &Connection,
        primary: &PolicyState,
        wire: &TaskBusinessWire,
        acceptance: &RemoteExecutionHistoryAcceptance,
        now_ms: u64,
    ) -> Result<()> {
        if let Self::TaskBusiness(view) = self {
            super::task_business_policy::bind_task_business_original_in(
                connection, primary, view, wire, acceptance, now_ms,
            )?;
        }
        Ok(())
    }

    fn check_resolution_authorizations_in(
        &self,
        connection: &Connection,
        primary: &PolicyState,
        resolution: &VerifiedExecutionResolution,
        now_ms: u64,
    ) -> Result<()> {
        match self {
            Self::Primary => {
                check_policy_heads_in(
                    connection,
                    primary.verified(),
                    resolution.resolver(),
                    now_ms,
                )?;
                check_policy_heads_in(
                    connection,
                    primary.verified(),
                    resolution.submission(),
                    now_ms,
                )?;
            }
            Self::TaskBusiness(_) => {
                self.state(primary)
                    .check_current(resolution.resolver(), now_ms)?;
                self.state(primary)
                    .check_current(resolution.submission(), now_ms)?;
                self.check_heads_in(
                    connection,
                    primary,
                    &TaskBusinessWire::Resolve(resolution.wire().clone()),
                    resolution.dependencies(),
                    now_ms,
                )?;
            }
        }
        Ok(())
    }

    fn check_authorizations_in(
        &self,
        connection: &Connection,
        primary: &PolicyState,
        handoff: &VerifiedExecutionHandoff,
        now_ms: u64,
    ) -> Result<()> {
        match self {
            Self::Primary => {
                check_policy_heads_in(connection, primary.verified(), handoff.original(), now_ms)?;
                check_policy_heads_in(
                    connection,
                    primary.verified(),
                    handoff.submission(),
                    now_ms,
                )?;
            }
            Self::TaskBusiness(view) => {
                self.check_wire_in(connection, primary, handoff.wire(), now_ms)?;
                for auth in [handoff.original(), handoff.submission()] {
                    view.state().check_current(auth, now_ms)?;
                    ensure!(
                        auth.dependencies()
                            .iter()
                            .all(|expected| view.primary_heads().contains(expected)
                                || view.foreign_heads().contains(expected)),
                        "handoff authorization escaped its exact Task business source"
                    );
                }
            }
        }
        Ok(())
    }

    fn resolution_usage(
        &self,
        auth: &VerifiedExecutionResolution,
        proof: &ExecutionHandoffResolutionFreshness,
    ) -> Result<ExecutionProofUsage> {
        let mut usage = auth.proof_usage().clone();
        usage.record_metadata(&proof.challenge)?;
        for receipt in &proof.receipts {
            usage.record_signed(receipt)?;
        }
        self.check_usage(usage)
    }

    fn check_response_usage(
        &self,
        mut usage: ExecutionProofUsage,
        proof: &ExecutionHandoffFreshness,
        stored: &StoredHandoff,
        result: &AdmittedExecutionHandoff,
    ) -> Result<()> {
        if matches!(self, Self::TaskBusiness(_)) {
            usage.record_metadata(&proof.challenge)?;
            for receipt in &proof.receipts {
                usage.record_signed(receipt)?;
            }
            usage.record_signed(&stored.admission_receipt)?;
            if let Some(receipt) = &result.receipt {
                usage.record_signed(receipt)?;
            }
            self.check_usage(usage)?;
        }
        Ok(())
    }

    fn check_usage(&self, mut usage: ExecutionProofUsage) -> Result<ExecutionProofUsage> {
        if let Self::TaskBusiness(view) = self {
            usage.merge(view.proof_usage());
            if let Some(counts) =
                vibeface_secure::signature_verification::SignatureVerification::current_counts()
            {
                ensure!(
                    counts.actual_verifications <= 32,
                    "Task business handoff exceeded its crypto budget"
                );
                usage.record_actual_signature_counts(counts);
            }
            usage.enforce_limits()?;
        }
        Ok(usage)
    }
}

#[derive(Clone, Copy)]
enum HandoffRunningAuthorization<'a> {
    Primary(&'a VerifiedExecutionHandoff),
    OperationCurrent(&'a VerifiedTaskBusinessOperationCurrent),
}

impl HandoffRunningAuthorization<'_> {
    fn authorization(&self) -> &VerifiedExecutionHandoff {
        match self {
            Self::Primary(value) => value,
            Self::OperationCurrent(value) => value.handoff_authorization(),
        }
    }

    fn check_accepted(&self, stored: &StoredHandoff) -> Result<()> {
        ensure!(
            stored.wire == *self.authorization().wire(),
            "running authorization changed its accepted wire"
        );
        if let Self::OperationCurrent(cap) = self {
            let acceptance = RemoteExecutionHistoryAcceptance::ExecutionResult {
                receipt: stored.admission_receipt.clone(),
                freshness: stored.admission.clone(),
                transport: StoredTransport::from(&stored.transport),
            };
            ensure!(
                cap.admission_receipt()? == &stored.admission_receipt
                    && cap.accepted_at_ms() == stored.admitted_at_ms
                    && cap.acceptance_envelope_hash()? == ObjectHash::body(&acceptance)?,
                "operation capability changed its original executor acceptance"
            );
        }
        Ok(())
    }

    fn check_current_in(
        &self,
        connection: &Connection,
        primary: &PolicyState,
        now_ms: u64,
    ) -> Result<ExecutionProofUsage> {
        match self {
            Self::Primary(auth) => {
                check_policy_heads_in(connection, primary.verified(), auth.original(), now_ms)?;
                check_policy_heads_in(connection, primary.verified(), auth.submission(), now_ms)?;
                Ok(auth.proof_usage().clone())
            }
            Self::OperationCurrent(cap) => {
                super::task_business_policy::check_task_business_operation_current_in(
                    connection, primary, cap, now_ms,
                )
            }
        }
    }

    fn check_unconsumed_in(
        &self,
        connection: &Connection,
        primary: &PolicyState,
        proof: &ExecutionHandoffFreshness,
        stage: CheckStage,
        now_ms: u64,
    ) -> Result<ExecutionProofUsage> {
        if let Self::OperationCurrent(cap) = self {
            ensure!(
                proof == cap.freshness() && proof.challenge.stage == stage,
                "operation capability changed its freshness purpose"
            );
            let usage = self.check_current_in(connection, primary, now_ms)?;
            check_execution_handoff_operation_challenge_in(connection, cap.wire(), proof, false)?;
            Ok(usage)
        } else {
            Ok(self.authorization().proof_usage().clone())
        }
    }

    fn consume_in(
        &self,
        tx: &Transaction<'_>,
        primary: &PolicyState,
        proof: &ExecutionHandoffFreshness,
        stage: CheckStage,
        now_ms: u64,
    ) -> Result<u64> {
        match self {
            Self::Primary(auth) => consume_freshness_in(tx, primary, auth, proof, stage, now_ms),
            Self::OperationCurrent(cap) => {
                ensure!(
                    proof == cap.freshness() && proof.challenge.stage == stage,
                    "operation capability changed its freshness purpose"
                );
                consume_execution_handoff_operation_challenge_in(tx, cap.wire(), proof)?;
                Ok(cap.deadline_ms())
            }
        }
    }

    fn check_latest_lease_in(&self, connection: &Connection, stored: &StoredHandoff) -> Result<()> {
        if let Self::OperationCurrent(cap) = self {
            self.check_accepted(stored)?;
            let (proof, verified_at, deadline) = stored
                .lease
                .as_ref()
                .context("operation capability has no consumed execution lease")?;
            ensure!(
                proof == cap.freshness()
                    && *deadline == cap.deadline_ms()
                    && *verified_at >= cap.checked_at_ms(),
                "operation capability differs from the latest executor lease"
            );
            check_execution_handoff_operation_challenge_in(connection, cap.wire(), proof, true)?;
        }
        Ok(())
    }

    fn check_usage_in(
        &self,
        connection: &Connection,
        stored: &StoredHandoff,
        mut usage: ExecutionProofUsage,
        result: Option<&AdmittedExecutionHandoff>,
    ) -> Result<()> {
        if let Self::OperationCurrent(cap) = self {
            usage.merge(cap.proof_usage());
            check_handoff_checkpoint_usage_in(connection, stored, usage, result)?;
        }
        Ok(())
    }
}

fn check_handoff_checkpoint_usage_in(
    connection: &Connection,
    stored: &StoredHandoff,
    mut usage: ExecutionProofUsage,
    result: Option<&AdmittedExecutionHandoff>,
) -> Result<ExecutionProofUsage> {
    verify_local_handoff_checkpoint(connection, stored)?;
    usage.merge(
        verify_execution_handoff_admission_checkpoint(
            &stored.wire,
            &stored.admission_receipt,
            &stored.admission,
            &StoredTransport::from(&stored.transport),
            stored.admitted_at_ms,
        )?
        .proof_usage(),
    );
    if let Some((proof, verified_at, deadline)) = &stored.lease {
        let encoded: String = connection
            .query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![
                    stored.wire.handoff.execution_resource.space.as_str(),
                    lease_checkpoint_key(&stored.wire, proof)?
                ],
                |row| row.get(0),
            )
            .context("operation lease lost its immutable checkpoint")?;
        let receipt: Signed<ExecutionHandoffLeaseReceipt> = serde_json::from_str(&encoded)?;
        usage.merge(&verify_execution_handoff_lease_checkpoint(
            &stored.wire,
            &stored.admission_receipt,
            proof,
            *verified_at,
            *deadline,
            &receipt,
        )?);
    }
    if let Some(receipt) = &stored.receipt {
        usage.record_signed(receipt)?;
    }
    if let Some(receipt) = result.and_then(|value| value.receipt.as_ref()) {
        usage.record_signed(receipt)?;
    }
    if let Some(counts) =
        vibeface_secure::signature_verification::SignatureVerification::current_counts()
    {
        ensure!(
            counts.actual_verifications <= 32,
            "Task running operation exceeded its crypto budget"
        );
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(usage)
}

fn clear_unknown_native_session_in(connection: &Connection, stored: &StoredHandoff) -> Result<()> {
    if stored.request.session_id.is_none() {
        ensure!(
            matches!(
                stored.wire.handoff.source_statement()?.work,
                ExecutionWork::Command { .. }
            ),
            "session work has no session identifier"
        );
        return Ok(());
    }
    let (execution, original, _) = scope(&stored.wire)?;
    let dispatch: Event =
        serde_json::from_str(&stored.wire.handoff.project_dispatch.canonical_body)?;
    let (mut session, binding, work) = session_from(
        connection,
        &execution,
        &original,
        dispatch.resource_id.as_str(),
        &stored.request.target_node,
        stored.request.session_id.as_deref(),
    )?
    .context("Unknown execution session disappeared")?;
    ensure!(
        stored.request.binding == binding && work == stored.wire.handoff.source_statement()?.work,
        "Unknown execution session changed its accepted binding"
    );
    session.state = "unknown".into();
    session.native_session_id = None;
    ensure!(connection.execute("UPDATE execution_handoff_sessions SET record=? WHERE execution_space_id=? AND original_space_id=? AND original_resource_id=? AND target_node=? AND session_id=? AND revision=?",
        params![session_record(&session, &stored.wire.handoff.source_statement()?.work)?,execution,original,dispatch.resource_id.as_str(),session.target_node,session.session_id,session.revision])? == 1,
        "Unknown execution session changed before commit");
    Ok(())
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
    let original = capsule_admission(&dispatch)?;
    let project = original.invocation.payload.resource.clone();
    if let ExecutionOrigin::TaskSession { task_creation } = &handoff.origin {
        let task: Event = serde_json::from_str(&task_creation.canonical_body)?;
        let task_proof = capsule_admission(&task)?;
        ensure!(
            task_proof.invocation.payload.audience == original.invocation.payload.audience
                && dispatch.created_at_ms == original.admitted_at_ms as i64
                && original.admitted_at_ms >= task_proof.admitted_at_ms,
            "capsule task host or admission order changed"
        );
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
    } else {
        ensure!(
            handoff.task_sdk_origin.is_none(),
            "non-Task work cannot carry a Task SDK origin"
        );
    }
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
    let auth = current.verify_admitted_operation_authorization(&admitted, &facts, now_ms)?;
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
                    | "task_id"
                    | "task_ref"
                    | "basis_heads"
                    | "execution_resource"
                    | "target_public_key"
                    | "execution_grant"
                    | "execution_session_receipt"
                    | "manager_authority"
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
                    | "manager_authority"
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
        let source = handoff.source_statement()?;
        if !body["execution_session_receipt"].is_null() {
            let receipt: Signed<ExecutionHandoffReceipt> =
                serde_json::from_value(body["execution_session_receipt"].clone())?;
            receipt.verify_signature()?;
            let snapshot = receipt
                .payload
                .session
                .as_ref()
                .context("execution state receipt has no signed snapshot")?;
            ensure!(
                receipt.issuer == handoff.target_public_key
                    && snapshot.original_project == *project
                    && snapshot.execution_resource == handoff.execution_resource
                    && snapshot.target == handoff.execution_audience.device
                    && Some(snapshot.session_id.as_str()) == request.session_id.as_deref()
                    && snapshot.revision == request.expected_revision
                    && snapshot.work == source.work
                    && receipt.payload.work == source.work
                    && receipt.payload.original_space_id == project.space
                    && receipt.payload.execution_space_id == handoff.execution_resource.space
                    && receipt.payload.target == handoff.execution_audience.device
                    && receipt.payload.issued_at_ms <= proof.admitted_at_ms
                    && proof.admitted_at_ms < snapshot.expires_at_ms
                    && snapshot.expires_at_ms - receipt.payload.issued_at_ms
                        <= vibeface_secure::authorization::EXECUTION_LEASE_MS,
                "original execution state receipt belongs to another session"
            );
        } else {
            ensure!(
                request.action["type"] == "open"
                    || matches!(source.work, ExecutionWork::Command { .. }),
                "non-open session operation requires signed executor session state"
            );
        }
        ensure!(
            request.conversation_id == conversation
                && (body["conversation_id"].is_null()
                    || body["conversation_id"].as_str() == Some(conversation))
                && request.network_id == project.space.as_str()
                && body["method"] == "operate"
                && (body.get("task_id").is_none()
                    || body["task_id"] == serde_json::to_value(&request.task_id)?)
                && (body.get("basis_heads").is_none()
                    || body["basis_heads"] == serde_json::to_value(&request.basis_heads)?),
            "capsule operation scope changed"
        );
        ensure!(
            body["task_ref"].is_null()
                || body["task_ref"].as_str() == source.work.task_ref().map(|task| task.as_str()),
            "capsule references another task"
        );
        context = handoff.execution_context()?;
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
                && proof.transport.existing_evidence()?
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

pub(super) fn compact_sdk_from_events(
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
    let source = stored.wire.handoff.source_statement()?;
    match session_from(
        connection,
        stored.wire.handoff.execution_resource.space.as_str(),
        source.project.space.as_str(),
        source.project.resource.as_str(),
        &stored.request.target_node,
        stored.request.session_id.as_deref(),
    )? {
        Some((session, binding, work)) => {
            ensure!(
                work == source.work,
                "stored session changed its original work"
            );
            check_session_work(
                &work,
                &stored.wire.handoff.target_public_key,
                &stored.request,
                &session,
                &binding,
            )?;
        }
        None => ensure!(
            matches!(source.work, ExecutionWork::Command { .. }),
            "stored session work has no executor session"
        ),
    }
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
        session: Option<String>,
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
        transport: serde_json::from_str::<StoredTransport>(&raw.transport)?.existing_evidence()?,
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
        ) || stored.lease.is_some()
            || (stored.state == OperationState::Unknown && stored.receipt.is_some()),
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
            !stored.state.terminal() && stored.result.is_null(),
            "handoff result has no signed result receipt"
        );
    }
    Ok(Some(stored))
}

fn is_control(request: &OperationRequest) -> bool {
    !matches!(
        request.action["type"].as_str(),
        Some("open" | "send" | "command")
    )
}

pub(super) fn verify_binding_seal_source_in(
    connection: &Connection,
    target: &vibeface_secure::authorization::execution_binding_seal::ExecutionBindingSealTarget,
) -> Result<ExecutionProofUsage> {
    let session = &target.session;
    let stored = stored_from(
        connection,
        session.execution_resource.space.as_str(),
        session.original_project.space.as_str(),
        &target.receipt.payload.operation_id,
    )?
    .context("executor has not accepted the sealed operation")?;
    verify_local_handoff_checkpoint(connection, &stored)?;
    let admission = verify_execution_handoff_admission_checkpoint(
        &stored.wire,
        &stored.admission_receipt,
        &stored.admission,
        &StoredTransport::from(&stored.transport),
        stored.admitted_at_ms,
    )?;
    verify_execution_handoff_receipt_signature(&stored.wire, &target.receipt, &stored.result)?;
    let source = stored.wire.handoff.source_statement()?;
    ensure!(
        stored.request.binding.as_ref() == Some(&target.binding)
            && source.project == session.original_project
            && source.work == session.work
            && stored.wire.handoff.execution_resource == session.execution_resource
            && stored.wire.handoff.target_public_key == target.target_public_key
            && serde_json::to_value(&stored.state)?
                == serde_json::to_value(target.receipt.payload.state)?,
        "seal receipt differs from the executor's exact accepted operation",
    );
    Ok(admission.proof_usage().clone())
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

pub(crate) fn persist_execution_handoff_operation_challenge_in(
    connection: &Connection,
    wire: &SignedExecutionHandoff,
    challenge: &ExecutionHandoffChallenge,
) -> Result<()> {
    let signatures = vibeface_secure::signature_verification::ScopedSignatureVerification::new();
    ensure!(
        matches!(challenge.stage, CheckStage::Start | CheckStage::Renew),
        "operation challenge requires Start or Renew"
    );
    let (execution, original, operation) = scope(wire)?;
    let stored = stored_from(connection, &execution, &original, &operation)?
        .context("operation challenge has no executor acceptance")?;
    ensure!(
        stored.wire == *wire,
        "operation challenge changed its accepted wire"
    );
    verify_local_handoff_checkpoint(connection, &stored)?;
    let admitted = verify_execution_handoff_admission_checkpoint(
        wire,
        &stored.admission_receipt,
        &stored.admission,
        &StoredTransport::from(&stored.transport),
        stored.admitted_at_ms,
    )?;
    let mut expected = stored.admission.challenge.clone();
    expected.stage = challenge.stage;
    expected.policies = challenge.policies.clone();
    expected.nonce = challenge.nonce.clone();
    expected.issued_at_ms = challenge.issued_at_ms;
    expected.expires_at_ms = challenge.expires_at_ms;
    ensure!(
        expected == *challenge
            && challenge.issued_at_ms >= stored.admitted_at_ms
            && challenge.expires_at_ms > challenge.issued_at_ms
            && challenge.expires_at_ms - challenge.issued_at_ms
                <= vibeface_secure::authorization::EXECUTION_LEASE_MS
            && !challenge.policies.is_empty()
            && challenge.policies.len() <= vibeface_secure::authorization::MAX_POLICY_DOMAINS
            && challenge
                .policies
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain),
        "operation challenge changed its immutable accepted scope or lifetime"
    );
    super::execution_binding_seal::ensure_handoff_generation_open_in(connection, wire)?;
    let mut usage = admitted.proof_usage().clone();
    usage.record_metadata(challenge)?;
    usage.record_actual_signature_counts(signatures.counts());
    usage.enforce_limits()?;
    connection.execute("INSERT INTO execution_handoff_challenges(execution_space_id,nonce,original_space_id,operation_id,handoff_hash,stage,challenge,expires_at_ms) VALUES(?,?,?,?,?,?,?,?)",
        params![execution,challenge.nonce.as_str(),original,operation,wire.hash()?.as_str(),
            serde_json::to_value(challenge.stage)?.as_str().context("invalid operation challenge stage")?,
            serde_json::to_string(challenge)?,challenge.expires_at_ms])?;
    Ok(())
}

pub(crate) fn check_execution_handoff_operation_challenge_in(
    connection: &Connection,
    wire: &SignedExecutionHandoff,
    proof: &ExecutionHandoffFreshness,
    consumed: bool,
) -> Result<()> {
    ensure!(
        matches!(proof.challenge.stage, CheckStage::Start | CheckStage::Renew),
        "operation challenge requires Start or Renew"
    );
    verify_challenge_record(connection, wire, proof, consumed)
}

pub(crate) fn consume_execution_handoff_operation_challenge_in(
    connection: &Connection,
    wire: &SignedExecutionHandoff,
    proof: &ExecutionHandoffFreshness,
) -> Result<()> {
    check_execution_handoff_operation_challenge_in(connection, wire, proof, false)?;
    ensure!(connection.execute(
        "UPDATE execution_handoff_challenges SET consumed=1 WHERE execution_space_id=? AND nonce=? AND consumed=0",
        params![wire.handoff.execution_resource.space.as_str(),proof.challenge.nonce.as_str()],
    )? == 1, "operation challenge was already consumed");
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
    consume_freshness_with_policy_in(
        tx,
        policy,
        HandoffPolicy::Primary,
        handoff,
        proof,
        stage,
        now_ms,
    )
}

fn consume_freshness_with_policy_in(
    tx: &Transaction<'_>,
    policy: &PolicyState,
    current: HandoffPolicy<'_>,
    handoff: &VerifiedExecutionHandoff,
    proof: &ExecutionHandoffFreshness,
    stage: CheckStage,
    now_ms: u64,
) -> Result<u64> {
    ensure!(
        proof.challenge.stage == stage,
        "incorrect execution handoff stage"
    );
    current.check_authorizations_in(tx, policy, handoff, now_ms)?;
    let freshness = current
        .state(policy)
        .verify_execution_handoff_freshness(handoff, proof, now_ms)?;
    current.check_usage(freshness.proof_usage().clone())?;
    verify_challenge_record(tx, handoff.wire(), proof, false)?;
    ensure!(tx.execute("UPDATE execution_handoff_challenges SET consumed=1 WHERE execution_space_id=? AND nonce=? AND consumed=0",
        params![handoff.wire().handoff.execution_resource.space.as_str(), proof.challenge.nonce.as_str()])? == 1,
        "execution handoff challenge was already consumed");
    Ok(freshness.expires_at_ms())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredExecutionSession {
    #[serde(flatten)]
    session: ExecutionSession,
    work: ExecutionWork,
}

pub(super) fn session_record(session: &ExecutionSession, work: &ExecutionWork) -> Result<String> {
    Ok(serde_json::to_string(&StoredExecutionSession {
        session: session.clone(),
        work: work.clone(),
    })?)
}

pub(super) fn session_from(
    connection: &Connection,
    execution: &str,
    original: &str,
    project_resource: &str,
    target: &str,
    session: Option<&str>,
) -> Result<Option<(ExecutionSession, Option<SignedBinding>, ExecutionWork)>> {
    let Some(session) = session else {
        return Ok(None);
    };
    let row: Option<(String, Option<String>, u64)> = connection.query_row(
        "SELECT record,binding,revision FROM execution_handoff_sessions WHERE execution_space_id=? AND original_space_id=? AND original_resource_id=? AND target_node=? AND session_id=?",
        params![execution, original, project_resource, target, session], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
    let Some((record, binding, revision)) = row else {
        return Ok(None);
    };
    let StoredExecutionSession {
        session: record,
        work,
    } = serde_json::from_str(&record)?;
    let binding: Option<SignedBinding> = binding
        .map(|value| serde_json::from_str(&value))
        .transpose()?;
    ensure!(
        record.network_id == execution
            && record.target_node == target
            && record.session_id == session
            && record.revision == revision
            && binding.as_ref().is_none_or(|binding| {
                binding.binding.network_id == original
                    && binding.binding.target_node == target
                    && binding.binding.session_id == session
            }),
        "executor session projection differs from its original scope"
    );
    let expected_session = work
        .execution_session()
        .context("command cannot own a session row")?;
    ensure!(
        expected_session.session_id == session,
        "stored session work changed its session"
    );
    match &work {
        ExecutionWork::TaskSession {
            binding_hash,
            binding_generation,
            ..
        } => {
            let binding = binding
                .as_ref()
                .context("Task session has no persisted binding")?;
            ensure!(
                crate::binding::binding_hash(&binding.binding)? == *binding_hash
                    && binding.binding.generation == *binding_generation,
                "stored Task session work changed its binding"
            );
        }
        ExecutionWork::ConversationSession {
            executor,
            canonical_cwd,
            ..
        } => {
            ensure!(
                binding.is_none() && record.executor == *executor && record.cwd == *canonical_cwd,
                "stored conversation session work changed its Open"
            );
        }
        ExecutionWork::Command { .. } => anyhow::bail!("command cannot own a session row"),
    }
    Ok(Some((record, binding, work)))
}

fn check_session_work(
    work: &ExecutionWork,
    target_public_key: &PublicKey,
    operation: &OperationRequest,
    session: &ExecutionSession,
    binding: &Option<SignedBinding>,
) -> Result<()> {
    let expected = work
        .execution_session()
        .context("command cannot have an execution session")?;
    let target_node = vibeface_secure::authorization::node_fingerprint(target_public_key.as_str())
        .map_err(anyhow::Error::msg)?;
    ensure!(
        operation.session_id.as_deref() == Some(expected.session_id.as_str())
            && expected.target == DeviceId::from_key(target_public_key)
            && operation.target_node == target_node
            && session.target_node == target_node
            && session.session_id == expected.session_id
            && operation.binding == *binding,
        "execution session differs from its original work"
    );
    match work {
        ExecutionWork::TaskSession {
            binding_hash,
            binding_generation,
            ..
        } => {
            let binding = binding.as_ref().context("Task session has no binding")?;
            ensure!(
                crate::binding::binding_hash(&binding.binding)? == *binding_hash
                    && binding.binding.generation == *binding_generation
                    && binding.binding.open_spec["executor"].as_str()
                        == Some(session.executor.as_str())
                    && binding.binding.open_spec["cwd"].as_str() == Some(session.cwd.as_str()),
                "Task session changed its exact binding"
            );
        }
        ExecutionWork::ConversationSession {
            executor,
            canonical_cwd,
            ..
        } => {
            ensure!(
                binding.is_none() && session.executor == *executor && session.cwd == *canonical_cwd,
                "conversation session changed its original Open"
            );
        }
        ExecutionWork::Command { .. } => anyhow::bail!("command cannot have an execution session"),
    }
    Ok(())
}

fn validate_execution_context(
    connection: &Connection,
    identity: &LocalIdentity,
    handoff: &VerifiedExecutionHandoff,
    operation: &OperationRequest,
) -> Result<(Vec<String>, Option<ExecutionSession>)> {
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
    let binding = match handoff.work() {
        ExecutionWork::TaskSession { .. } => Some(handoff.binding()?),
        _ => None,
    };
    let cwd = match handoff.work() {
        ExecutionWork::TaskSession { .. } => binding.as_ref().unwrap().binding.open_spec["cwd"]
            .as_str()
            .context("binding has no execution directory")?,
        ExecutionWork::ConversationSession { canonical_cwd, .. }
        | ExecutionWork::Command { canonical_cwd, .. } => canonical_cwd,
    };
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
    if !is_control(operation) {
        ensure!(
            resources.iter().any(|resource| resource == cwd),
            "execution directory must be reserved"
        );
    }
    if matches!(handoff.work(), ExecutionWork::Command { .. }) {
        ensure!(
            operation.session_id.is_none()
                && operation.task_id.is_none()
                && operation.binding.is_none()
                && operation.expected_revision == 0,
            "command cannot carry a session, Task or revision"
        );
        serde_json::from_value::<EngineAction>(operation.action.clone())?.validate()?;
        return Ok((resources, None));
    }
    let session_id = operation
        .session_id
        .as_deref()
        .context("session work has no session")?;
    let existing = session_from(
        connection,
        execution.space.as_str(),
        &operation.network_id,
        handoff.original().resource().resource.as_str(),
        &operation.target_node,
        Some(session_id),
    )?;
    let session = if operation.action["type"] == "open" {
        ensure!(
            existing.is_none()
                && operation.expected_revision == 0
                && binding
                    .as_ref()
                    .is_none_or(|binding| operation.action == binding.binding.open_spec),
            "execution open conflicts with an existing session or Task binding"
        );
        let executor: ExecutorKind = serde_json::from_value(operation.action["executor"].clone())?;
        let session = ExecutionSession {
            network_id: execution.space.to_string(),
            target_node: operation.target_node.clone(),
            session_id: session_id.to_owned(),
            revision: 0,
            state: "opening".into(),
            executor: serde_json::to_value(executor)?
                .as_str()
                .context("invalid executor")?
                .to_owned(),
            cwd: cwd.to_owned(),
            native_session_id: None,
        };
        check_session_work(
            handoff.work(),
            &handoff.wire().handoff.target_public_key,
            operation,
            &session,
            &binding,
        )?;
        session
    } else {
        serde_json::from_value::<EngineAction>(operation.action.clone())?.validate()?;
        let (session, previous_binding, work) = existing.context("executor session is not open")?;
        ensure!(work == *handoff.work(), "session changed its original work");
        check_session_work(
            handoff.work(),
            &handoff.wire().handoff.target_public_key,
            operation,
            &session,
            &previous_binding,
        )?;
        ensure!(
            session.state != "resolved" && session.revision == operation.expected_revision,
            "executor session revision changed"
        );
        session
    };
    Ok((resources, Some(session)))
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
    let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
        .map_err(anyhow::Error::msg)?;
    let source = stored.wire.handoff.source_statement()?;
    let session = session_from(
        connection,
        stored.wire.handoff.execution_resource.space.as_str(),
        &stored.request.network_id,
        source.project.resource.as_str(),
        &stored.request.target_node,
        stored.request.session_id.as_deref(),
    )?
    .map(
        |(session, binding, work)| -> Result<ExecutionSessionSnapshot> {
            ensure!(work == source.work, "session changed its original work");
            check_session_work(
                &source.work,
                &stored.wire.handoff.target_public_key,
                &stored.request,
                &session,
                &binding,
            )?;
            Ok(ExecutionSessionSnapshot {
                original_project: source.project.clone(),
                execution_resource: stored.wire.handoff.execution_resource.clone(),
                work: source.work.clone(),
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
        },
    )
    .transpose()?;
    ensure!(
        matches!(source.work, ExecutionWork::Command { .. }) == session.is_none(),
        "session work has no durable executor session"
    );
    let receipt = Signed::sign(
        ExecutionHandoffReceipt {
            version: AUTH_VERSION,
            operation_id: stored.request.operation_id.clone(),
            original_space_id: SpaceId::parse(&stored.request.network_id)?,
            execution_space_id: stored.wire.handoff.execution_resource.space.clone(),
            handoff_hash: stored.wire.hash()?,
            target: stored.wire.handoff.execution_audience.device.clone(),
            work: source.work,
            state: serde_json::from_value(serde_json::to_value(&stored.state)?)?,
            result_hash: ObjectHash::body(&stored.result)?,
            session,
            control: None,
            issued_at_ms: now_ms,
        },
        &signer,
    )?;
    verify_execution_handoff_receipt_signature(&stored.wire, &receipt, &stored.result)?;
    Ok(receipt)
}

pub(crate) fn public_execution_result(mut value: Value, native: Option<&str>) -> Value {
    fn clean(value: &mut Value, native: Option<&str>) {
        match value {
            Value::Object(fields) => {
                for key in ["native_session_id", "threadId", "thread_id"] {
                    fields.remove(key);
                }
                for value in fields.values_mut() {
                    clean(value, native);
                }
            }
            Value::Array(values) => {
                for value in values {
                    clean(value, native);
                }
            }
            Value::String(text) if native.is_some_and(|native| native == text) => {
                *value = Value::Null
            }
            Value::String(text) => {
                if let Some(native) = native.filter(|native| !native.is_empty()) {
                    *text = text.replace(native, "[private session]");
                }
            }
            _ => {}
        }
    }
    clean(&mut value, native);
    value
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
            stored.request.session_id.as_deref(),
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
        self.execution_handoff_proof_usage_with_policy(
            HandoffPolicy::Primary,
            wire,
            transport,
            freshness,
            now_ms,
        )
    }

    pub(crate) fn execution_handoff_proof_usage_scoped(
        &self,
        view: &TaskBusinessScopedView,
        wire: &SignedExecutionHandoff,
        transport: &TransportEvidence,
        freshness: Option<&ExecutionHandoffFreshness>,
        now_ms: u64,
    ) -> Result<ExecutionProofUsage> {
        self.execution_handoff_proof_usage_with_policy(
            HandoffPolicy::TaskBusiness(view),
            wire,
            transport,
            freshness,
            now_ms,
        )
    }

    fn execution_handoff_proof_usage_with_policy(
        &self,
        current: HandoffPolicy<'_>,
        wire: &SignedExecutionHandoff,
        transport: &TransportEvidence,
        freshness: Option<&ExecutionHandoffFreshness>,
        now_ms: u64,
    ) -> Result<ExecutionProofUsage> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        current.check_wire_in(&self.connection, &self.policy, wire, now_ms)?;
        let state = current.state(&self.policy);
        let original =
            original_authorization_from(&self.connection, &self.policy, state, wire, now_ms)?;
        let (execution, space, operation) = scope(wire)?;
        let admission = stored_from(&self.connection, &execution, &space, &operation)?
            .map(|stored| reconstruct_handoff(&self.connection, &self.policy, &stored))
            .transpose()?;
        let verified = state.verify_execution_handoff(
            wire,
            &original,
            transport,
            admission.as_ref(),
            now_ms,
        )?;
        let mut usage = match freshness {
            Some(proof) => state
                .verify_execution_handoff_freshness(&verified, proof, now_ms)?
                .proof_usage()
                .clone(),
            None => verified.proof_usage().clone(),
        };
        usage.record_actual_signature_counts(_signature_scope.counts());
        current.check_usage(usage)
    }

    pub fn execution_handoff_resolution_proof_usage(
        &self,
        wire: &SignedExecutionHandoffResolution,
        freshness: Option<&ExecutionHandoffResolutionFreshness>,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<ExecutionProofUsage> {
        self.execution_handoff_resolution_proof_usage_with_policy(
            HandoffPolicy::Primary,
            wire,
            freshness,
            transport,
            now_ms,
        )
    }

    pub(crate) fn execution_handoff_resolution_proof_usage_scoped(
        &self,
        view: &TaskBusinessScopedView,
        wire: &SignedExecutionHandoffResolution,
        freshness: Option<&ExecutionHandoffResolutionFreshness>,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<ExecutionProofUsage> {
        self.execution_handoff_resolution_proof_usage_with_policy(
            HandoffPolicy::TaskBusiness(view),
            wire,
            freshness,
            transport,
            now_ms,
        )
    }

    fn execution_handoff_resolution_proof_usage_with_policy(
        &self,
        current: HandoffPolicy<'_>,
        wire: &SignedExecutionHandoffResolution,
        freshness: Option<&ExecutionHandoffResolutionFreshness>,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<ExecutionProofUsage> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let auth = self.execution_handoff_resolution_authorization_with_policy(
            current, wire, transport, now_ms,
        )?;
        let mut usage = auth.proof_usage().clone();
        if let Some(proof) = freshness {
            current
                .state(&self.policy)
                .verify_execution_resolution_freshness(&auth, proof, now_ms)?;
            usage.record_metadata(&proof.challenge)?;
            for receipt in &proof.receipts {
                usage.record_signed(receipt)?;
            }
        }
        usage.enforce_limits()?;
        usage.record_actual_signature_counts(_signature_scope.counts());
        current.check_usage(usage)
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
    let mut proofs = vec![proof];
    let mut events = BTreeMap::from([(dispatch.envelope.hash.clone(), dispatch)]);
    if let Some(binding) = &request.binding {
        let task = ready_execution_source_event(
            connection,
            space,
            &binding.binding.task_ref,
            Some(&project),
        )?;
        proofs.push(capsule_admission(&task.event)?);
        events.insert(task.envelope.hash.clone(), task);
    }
    let mut loaded_bytes = events
        .values()
        .map(|stored| stored.envelope.canonical_body.len())
        .sum::<usize>();
    for proof in &proofs {
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

pub(crate) fn accepted_conversation_open_in(
    connection: &Connection,
    project: &ResourceRef,
    operation: &OperationRequest,
) -> Result<ExecutionHandoffResult> {
    let session = operation
        .session_id
        .as_deref()
        .context("conversation work has no session")?;
    let rows = connection.prepare("SELECT result FROM operations WHERE space_id=? AND target_node=? AND session_id=? AND json_extract(request,'$.conversation_id')=? AND json_extract(request,'$.task_id') IS NULL AND json_extract(request,'$.action.type')='open' LIMIT 2")?
        .query_map(params![project.space.as_str(), operation.target_node, session, operation.conversation_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        rows.len() == 1,
        "conversation session has missing or ambiguous accepted Open"
    );
    let accepted: ExecutionHandoffResult = serde_json::from_str(&rows[0])?;
    verify_execution_handoff_receipt_signature(
        &accepted.handoff,
        &accepted.receipt,
        &accepted.result,
    )?;
    let source = accepted.handoff.handoff.source_statement()?;
    let snapshot = accepted
        .receipt
        .payload
        .session
        .as_ref()
        .context("successful conversation Open has no session snapshot")?;
    ensure!(
        snapshot.session_id == session
            && snapshot.work == source.work
            && snapshot.revision > 0
            && !matches!(
                snapshot.state.as_str(),
                "opening" | "unknown" | "resolved" | "failed" | "closed"
            ),
        "conversation Open did not create an available execution session"
    );
    ensure!(
        matches!(
            accepted.handoff.handoff.origin,
            ExecutionOrigin::ConversationOpen
        ) && matches!(
            accepted.receipt.payload.state,
            ExecutionHandoffState::Succeeded
        ) && source.project == *project
            && source.conversation == operation.conversation_id
            && source
                .work
                .execution_session()
                .is_some_and(|value| value.session_id == session),
        "conversation session source is not its successful original Open"
    );
    Ok(accepted)
}

fn execution_handoff_payload_from(
    connection: &Connection,
    state: &VerifiedPolicySet,
    scope: Option<&super::task_execution_context::TaskScopedAuthorization>,
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
    let events = ready_execution_source_events(connection, original_space, operation_id)?;
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
    if let Some(scope) = scope {
        ensure!(
            scope.authorization().invocation_hash() == &original.invocation.hash()?
                && scope.authorization().body_hash() == &ObjectHash::body(&original.source_body)?,
            "handoff source differs from its current scoped authorization"
        );
    }
    let operation: OperationRequest =
        serde_json::from_value(original.source_body["request"].clone())?;
    let (origin, task_sdk_origin) = if let Some(binding) = &operation.binding {
        let task = events
            .get(&binding.binding.task_ref)
            .context("operation task creation is missing")?;
        let task_proof = capsule_admission(&task.event)?;
        (
            ExecutionOrigin::TaskSession {
                task_creation: task.envelope.clone(),
            },
            compact_sdk_from_events(&events, &task_proof)?,
        )
    } else if operation.action["type"] == "command" {
        (ExecutionOrigin::Command, None)
    } else if operation.action["type"] == "open" {
        (ExecutionOrigin::ConversationOpen, None)
    } else {
        let accepted_open = accepted_conversation_open_in(
            connection,
            &original.invocation.payload.resource,
            &operation,
        )?;
        (
            ExecutionOrigin::ConversationSession {
                accepted_open: Box::new(accepted_open),
            },
            None,
        )
    };
    let sdk_origin = compact_sdk_from_events(&events, &original)?;
    ensure!(
        identity.node_id()? == dispatch.event.author,
        "source capsule signer is not the Project host"
    );
    let mut handoff = ExecutionHandoff {
        version: AUTH_VERSION,
        project_dispatch: dispatch.envelope.clone(),
        origin,
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
    original_capsule_authorization(state, &handoff, now_ms)?;
    Ok(handoff)
}

pub(crate) fn execution_handoff_payload_in(
    connection: &Connection,
    policy: &PolicyState,
    scope: &super::task_execution_context::TaskScopedAuthorization,
    identity: &LocalIdentity,
    original_space: &str,
    operation_id: &str,
    execution_resource: ResourceRef,
    execution_audience: vibeface_secure::authorization::Audience,
    target_public_key: PublicKey,
    now_ms: u64,
) -> Result<ExecutionHandoff> {
    execution_handoff_payload_from(
        connection,
        scope.state(policy.verified()),
        Some(scope),
        identity,
        original_space,
        operation_id,
        execution_resource,
        execution_audience,
        target_public_key,
        now_ms,
    )
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
        execution_handoff_payload_from(
            &self.connection,
            self.policy.verified(),
            None,
            identity,
            original_space,
            operation_id,
            execution_resource,
            execution_audience,
            target_public_key,
            now_ms,
        )
    }

    pub(crate) fn execution_handoff_original_acceptance(
        &self,
        execution_space: &str,
        original_space: &str,
        operation_id: &str,
    ) -> Result<(SignedExecutionHandoff, RemoteExecutionHistoryAcceptance)> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let tx = self.connection.unchecked_transaction()?;
        let stored = stored_from(&tx, execution_space, original_space, operation_id)?
            .context("unknown execution handoff")?;
        verify_local_handoff_checkpoint(&tx, &stored)?;
        let acceptance = RemoteExecutionHistoryAcceptance::ExecutionResult {
            receipt: stored.admission_receipt,
            freshness: stored.admission,
            transport: StoredTransport::from(&stored.transport),
        };
        tx.commit()?;
        Ok((stored.wire, acceptance))
    }

    pub fn execution_handoff_authorization(
        &self,
        execution_space: &str,
        original_space: &str,
        operation_id: &str,
        now_ms: u64,
    ) -> Result<VerifiedExecutionHandoff> {
        self.execution_handoff_authorization_with_policy(
            HandoffPolicy::Primary,
            execution_space,
            original_space,
            operation_id,
            now_ms,
        )
    }

    pub(crate) fn execution_handoff_authorization_scoped(
        &self,
        view: &TaskBusinessScopedView,
        execution_space: &str,
        original_space: &str,
        operation_id: &str,
        now_ms: u64,
    ) -> Result<VerifiedExecutionHandoff> {
        self.execution_handoff_authorization_with_policy(
            HandoffPolicy::TaskBusiness(view),
            execution_space,
            original_space,
            operation_id,
            now_ms,
        )
    }

    fn execution_handoff_authorization_with_policy(
        &self,
        current: HandoffPolicy<'_>,
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
        current.check_wire_in(&self.connection, &self.policy, &stored.wire, now_ms)?;
        let admission = reconstruct_handoff(&self.connection, &self.policy, &stored)?;
        let original = original_authorization_from(
            &self.connection,
            &self.policy,
            current.state(&self.policy),
            &stored.wire,
            now_ms,
        )?;
        let auth = current.state(&self.policy).verify_execution_handoff(
            &stored.wire,
            &original,
            &stored.transport,
            Some(&admission),
            now_ms,
        )?;
        if matches!(current, HandoffPolicy::TaskBusiness(_)) {
            current.check_authorizations_in(&self.connection, &self.policy, &auth, now_ms)?;
        }
        current.check_usage(auth.proof_usage().clone())?;
        Ok(auth)
    }

    pub fn issue_execution_handoff_challenge(
        &mut self,
        wire: &SignedExecutionHandoff,
        transport: &TransportEvidence,
        stage: CheckStage,
        now_ms: u64,
    ) -> Result<ExecutionHandoffChallenge> {
        self.issue_execution_handoff_challenge_with_policy(
            HandoffPolicy::Primary,
            wire,
            transport,
            stage,
            now_ms,
        )
    }

    pub(crate) fn issue_execution_handoff_challenge_scoped(
        &mut self,
        view: &TaskBusinessScopedView,
        wire: &SignedExecutionHandoff,
        transport: &TransportEvidence,
        stage: CheckStage,
        now_ms: u64,
    ) -> Result<ExecutionHandoffChallenge> {
        ensure!(
            matches!(stage, CheckStage::Admit | CheckStage::RemoteRead),
            "Task business source permits initial admission or current read only"
        );
        self.issue_execution_handoff_challenge_with_policy(
            HandoffPolicy::TaskBusiness(view),
            wire,
            transport,
            stage,
            now_ms,
        )
    }

    fn issue_execution_handoff_challenge_with_policy(
        &mut self,
        current: HandoffPolicy<'_>,
        wire: &SignedExecutionHandoff,
        transport: &TransportEvidence,
        stage: CheckStage,
        now_ms: u64,
    ) -> Result<ExecutionHandoffChallenge> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        current.check_wire_in(&self.connection, &self.policy, wire, now_ms)?;
        let original = original_authorization_from(
            &self.connection,
            &self.policy,
            current.state(&self.policy),
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
        let verified = current.state(&self.policy).verify_execution_handoff(
            wire,
            &original,
            transport,
            admitted.as_ref(),
            now_ms,
        )?;
        current.check_usage(verified.proof_usage().clone())?;
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
        current.check_authorizations_in(&tx, &self.policy, &verified, now_ms)?;
        super::execution_binding_seal::ensure_handoff_generation_open_in(&tx, wire)?;
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
        self.sign_execution_handoff_receipts_with_policy(
            HandoffPolicy::Primary,
            challenge,
            identity,
            now_ms,
        )
    }

    pub(crate) fn sign_execution_handoff_receipts_scoped(
        &self,
        view: &TaskBusinessScopedView,
        wire: &SignedExecutionHandoff,
        challenge: &ExecutionHandoffChallenge,
        identity: &LocalIdentity,
        now_ms: u64,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        ensure!(
            matches!(challenge.stage, CheckStage::Admit | CheckStage::RemoteRead),
            "Task business source permits initial admission or current read only"
        );
        ensure!(
            identity.public_key_hex()? == wire.handoff.target_public_key.as_str(),
            "Task business receipt signer is not this executor"
        );
        let current = HandoffPolicy::TaskBusiness(view);
        let tx = self.connection.unchecked_transaction()?;
        current.check_wire_in(&tx, &self.policy, wire, now_ms)?;
        verify_challenge_record(
            &tx,
            wire,
            &ExecutionHandoffFreshness {
                challenge: challenge.clone(),
                receipts: Vec::new(),
            },
            false,
        )?;
        let receipts =
            self.sign_execution_handoff_receipts_with_policy(current, challenge, identity, now_ms)?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_metadata(challenge)?;
        for receipt in &receipts {
            usage.record_signed(receipt)?;
        }
        current.check_usage(usage)?;
        tx.commit()?;
        Ok(receipts)
    }

    fn sign_execution_handoff_receipts_with_policy(
        &self,
        current: HandoffPolicy<'_>,
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
        let mut currents = Vec::new();
        for stamp in &challenge.policies {
            ensure!(
                current.state(&self.policy).head(&stamp.domain) == Some(stamp),
                "handoff freshness policy changed"
            );
            if let HandoffPolicy::TaskBusiness(view) = current {
                if !view.primary_heads().contains(stamp) {
                    continue;
                }
                observation::check_heads_in(
                    &self.connection,
                    self.policy.verified(),
                    std::slice::from_ref(stamp),
                )?;
            }
            let signer_policy = match current {
                HandoffPolicy::Primary => current.state(&self.policy),
                HandoffPolicy::TaskBusiness(_) => self.policy.verified(),
            };
            let authority = match &stamp.domain {
                PolicyDomain::Account(account) => {
                    &signer_policy
                        .account(account)
                        .context("missing account authority")?
                        .authority
                }
                PolicyDomain::Space(space) => {
                    &signer_policy
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
                currents.push(stamp.clone());
            }
        }
        if currents.is_empty() {
            return Ok(Vec::new());
        }
        currents.sort_by(|left, right| left.domain.cmp(&right.domain));
        let receipt = FreshnessReceipt {
            version: AUTH_VERSION,
            currents,
            challenge_hash: challenge.hash()?,
            issued_at_ms: now_ms,
            expires_at_ms: challenge.expires_at_ms,
        };
        receipt.validate_currents()?;
        Ok(vec![Signed::sign(receipt, &signer)?])
    }

    pub fn accept_execution_handoff(
        &mut self,
        identity: &LocalIdentity,
        wire: &SignedExecutionHandoff,
        proof: &ExecutionHandoffFreshness,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<AdmittedExecutionHandoff> {
        self.accept_execution_handoff_with_policy(
            HandoffPolicy::Primary,
            identity,
            wire,
            proof,
            transport,
            now_ms,
        )
    }

    pub(crate) fn accept_execution_handoff_scoped(
        &mut self,
        view: &TaskBusinessScopedView,
        identity: &LocalIdentity,
        wire: &SignedExecutionHandoff,
        proof: &ExecutionHandoffFreshness,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<AdmittedExecutionHandoff> {
        self.accept_execution_handoff_with_policy(
            HandoffPolicy::TaskBusiness(view),
            identity,
            wire,
            proof,
            transport,
            now_ms,
        )
    }

    fn accept_execution_handoff_with_policy(
        &mut self,
        current: HandoffPolicy<'_>,
        identity: &LocalIdentity,
        wire: &SignedExecutionHandoff,
        proof: &ExecutionHandoffFreshness,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<AdmittedExecutionHandoff> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        current.check_wire_in(&self.connection, &self.policy, wire, now_ms)?;
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
            let auth = self.execution_handoff_authorization_with_policy(
                current,
                &execution,
                &original_space,
                &operation_id,
                now_ms,
            )?;
            verify_challenge_record(&self.connection, wire, proof, true)?;
            let result = dto(&self.connection, identity, &stored, now_ms)?;
            current.check_response_usage(auth.proof_usage().clone(), proof, &stored, &result)?;
            return Ok(result);
        }
        let original = original_authorization_from(
            &self.connection,
            &self.policy,
            current.state(&self.policy),
            wire,
            now_ms,
        )?;
        let verified = current
            .state(&self.policy)
            .verify_execution_handoff(wire, &original, transport, None, now_ms)?;
        let freshness = current
            .state(&self.policy)
            .verify_execution_handoff_freshness(&verified, proof, now_ms)?;
        current.check_usage(freshness.proof_usage().clone())?;
        current
            .state(&self.policy)
            .admit_execution_handoff(&verified, &freshness, now_ms)?;
        let operation = request(wire)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let duplicate:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM execution_handoffs WHERE original_space_id=? AND operation_id=?)",
            params![original_space,operation_id],|row|row.get(0))?;
        super::execution_binding_seal::ensure_handoff_generation_open_in(&tx, wire)?;
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
        if let Some(session_id) = operation.session_id.as_deref() {
            check_execution_session_in(
                &tx,
                &original_space,
                &operation.target_node,
                session_id,
                is_control(&operation),
            )?;
        }
        check_execution_resources_in(&tx, &operation.target_node, &resources)?;
        consume_freshness_with_policy_in(
            &tx,
            &self.policy,
            current,
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
        if let Some(session) = session.filter(|_| operation.action["type"] == "open") {
            tx.execute("INSERT INTO execution_handoff_sessions(execution_space_id,original_space_id,original_resource_id,target_node,session_id,revision,binding,record) VALUES(?,?,?,?,?,?,?,?)",
                params![execution, original_space, verified.original().resource().resource.as_str(), operation.target_node, operation.session_id, session.revision,
                    operation.binding.as_ref().map(serde_json::to_string).transpose()?, session_record(&session, verified.work())?])?;
        }
        if matches!(current, HandoffPolicy::TaskBusiness(_)) {
            current.bind_original_in(
                &tx,
                &self.policy,
                &TaskBusinessWire::Submit(wire.clone()),
                &RemoteExecutionHistoryAcceptance::ExecutionResult {
                    receipt: admission_receipt.clone(),
                    freshness: proof.clone(),
                    transport: StoredTransport::from(transport),
                },
                now_ms,
            )?;
            let stored = stored_from(&tx, &execution, &original_space, &operation_id)?
                .context("accepted handoff was not persisted")?;
            let result = dto(&tx, identity, &stored, now_ms)?;
            current.check_response_usage(
                freshness.proof_usage().clone(),
                proof,
                &stored,
                &result,
            )?;
            tx.commit()?;
            return Ok(result);
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
        self.authorize_execution_handoff_read_with_policy(
            HandoffPolicy::Primary,
            auth,
            proof,
            now_ms,
        )
    }

    pub(crate) fn authorize_execution_handoff_read_scoped(
        &self,
        view: &TaskBusinessScopedView,
        auth: &VerifiedExecutionHandoff,
        proof: &ExecutionHandoffFreshness,
        now_ms: u64,
    ) -> Result<()> {
        self.authorize_execution_handoff_read_with_policy(
            HandoffPolicy::TaskBusiness(view),
            auth,
            proof,
            now_ms,
        )
    }

    fn authorize_execution_handoff_read_with_policy(
        &self,
        current: HandoffPolicy<'_>,
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
        current.check_authorizations_in(&self.connection, &self.policy, auth, now_ms)?;
        let freshness = current
            .state(&self.policy)
            .verify_execution_handoff_freshness(auth, proof, now_ms)?;
        current.check_usage(freshness.proof_usage().clone())?;
        verify_challenge_record(&self.connection, auth.wire(), proof, false)
    }

    pub fn claim_execution_handoff(
        &mut self,
        identity: &LocalIdentity,
        auth: &VerifiedExecutionHandoff,
        proof: &ExecutionHandoffFreshness,
        now_ms: u64,
    ) -> Result<(AdmittedExecutionHandoff, bool, u64)> {
        self.claim_execution_handoff_with_authority(
            HandoffRunningAuthorization::Primary(auth),
            identity,
            proof,
            now_ms,
        )
    }

    pub(crate) fn claim_execution_handoff_operation_current(
        &mut self,
        identity: &LocalIdentity,
        cap: &VerifiedTaskBusinessOperationCurrent,
        now_ms: u64,
    ) -> Result<(AdmittedExecutionHandoff, bool, u64)> {
        self.claim_execution_handoff_with_authority(
            HandoffRunningAuthorization::OperationCurrent(cap),
            identity,
            cap.freshness(),
            now_ms,
        )
    }

    fn claim_execution_handoff_with_authority(
        &mut self,
        authority: HandoffRunningAuthorization<'_>,
        identity: &LocalIdentity,
        proof: &ExecutionHandoffFreshness,
        now_ms: u64,
    ) -> Result<(AdmittedExecutionHandoff, bool, u64)> {
        let auth = authority.authorization();
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let (execution, original, operation) = scope(auth.wire())?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored = stored_from(&tx, &execution, &original, &operation)?
            .context("unknown execution handoff")?;
        authority.check_accepted(&stored)?;
        let usage =
            authority.check_unconsumed_in(&tx, &self.policy, proof, CheckStage::Start, now_ms)?;
        ensure!(
            stored.wire == *auth.wire(),
            "handoff differs from its accepted request"
        );
        if stored.state != OperationState::Accepted {
            let result = dto(&tx, identity, &stored, now_ms)?;
            let deadline = stored.lease.as_ref().map(|lease| lease.2).unwrap_or(0);
            authority.check_usage_in(&tx, &stored, usage, Some(&result))?;
            tx.commit()?;
            return Ok((result, false, deadline));
        }
        super::execution_binding_seal::ensure_handoff_generation_open_in(&tx, auth.wire())?;
        let deadline = authority
            .consume_in(&tx, &self.policy, proof, CheckStage::Start, now_ms)
            .context("verify executor start freshness")?;
        save_lease_in(&tx, identity, auth, proof, now_ms, deadline)
            .context("persist executor start lease")?;
        ensure!(tx.execute("UPDATE execution_handoffs SET state='running' WHERE execution_space_id=? AND original_space_id=? AND operation_id=? AND state='accepted'",
            params![execution, original, operation])? == 1, "execution handoff was already claimed");
        let stored = stored_from(&tx, &execution, &original, &operation)?
            .context("claimed handoff disappeared")?;
        let result = dto(&tx, identity, &stored, now_ms)?;
        authority.check_latest_lease_in(&tx, &stored)?;
        authority.check_usage_in(&tx, &stored, usage, Some(&result))?;
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
        self.renew_execution_handoff_with_authority(
            HandoffRunningAuthorization::Primary(auth),
            identity,
            proof,
            now_ms,
        )
    }

    pub(crate) fn renew_execution_handoff_operation_current(
        &mut self,
        identity: &LocalIdentity,
        cap: &VerifiedTaskBusinessOperationCurrent,
        now_ms: u64,
    ) -> Result<u64> {
        self.renew_execution_handoff_with_authority(
            HandoffRunningAuthorization::OperationCurrent(cap),
            identity,
            cap.freshness(),
            now_ms,
        )
    }

    fn renew_execution_handoff_with_authority(
        &mut self,
        authority: HandoffRunningAuthorization<'_>,
        identity: &LocalIdentity,
        proof: &ExecutionHandoffFreshness,
        now_ms: u64,
    ) -> Result<u64> {
        let auth = authority.authorization();
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let (execution, original, operation) = scope(auth.wire())?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored = stored_from(&tx, &execution, &original, &operation)?
            .context("unknown execution handoff")?;
        authority.check_accepted(&stored)?;
        let mut usage =
            authority.check_unconsumed_in(&tx, &self.policy, proof, CheckStage::Renew, now_ms)?;
        if let HandoffRunningAuthorization::OperationCurrent(cap) = authority {
            usage.merge(cap.proof_usage());
            usage = check_handoff_checkpoint_usage_in(&tx, &stored, usage, None)?;
        }
        super::execution_binding_seal::ensure_handoff_generation_open_in(&tx, auth.wire())?;
        ensure!(
            stored.wire == *auth.wire()
                && stored.state == OperationState::Running
                && now_ms
                    < verified_lease_deadline(&tx, &self.policy, &stored, Some(auth), Some(proof))?,
            "only a running handoff with a live lease may renew"
        );
        let deadline = authority.consume_in(&tx, &self.policy, proof, CheckStage::Renew, now_ms)?;
        save_lease_in(&tx, identity, auth, proof, now_ms, deadline)?;
        if matches!(authority, HandoffRunningAuthorization::OperationCurrent(_)) {
            let updated = stored_from(&tx, &execution, &original, &operation)?
                .context("renewed handoff disappeared")?;
            authority.check_latest_lease_in(&tx, &updated)?;
            authority.check_usage_in(&tx, &updated, usage, None)?;
        }
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
            Some(session),
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
        self.persist_execution_handoff_native_with_authority(
            HandoffRunningAuthorization::Primary(&auth),
            native_session_id,
            now_ms,
        )
    }

    pub(crate) fn persist_execution_handoff_native_operation_current(
        &mut self,
        cap: &VerifiedTaskBusinessOperationCurrent,
        native_session_id: &str,
        now_ms: u64,
    ) -> Result<()> {
        self.persist_execution_handoff_native_with_authority(
            HandoffRunningAuthorization::OperationCurrent(cap),
            native_session_id,
            now_ms,
        )
    }

    fn persist_execution_handoff_native_with_authority(
        &mut self,
        authority: HandoffRunningAuthorization<'_>,
        native_session_id: &str,
        now_ms: u64,
    ) -> Result<()> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        ensure!(
            !native_session_id.is_empty() && native_session_id.len() <= 512,
            "invalid native execution session ID"
        );
        let auth = authority.authorization();
        let (execution, _, operation) = scope(auth.wire())?;
        let execution_space = execution.as_str();
        let operation_id = operation.as_str();
        let original_project = auth.original().resource();
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
        authority.check_accepted(&stored)?;
        authority.check_latest_lease_in(&tx, &stored)?;
        let usage = authority.check_current_in(&tx, &self.policy, now_ms)?;
        if matches!(authority, HandoffRunningAuthorization::OperationCurrent(_)) {
            super::execution_binding_seal::ensure_handoff_generation_open_in(&tx, auth.wire())?;
        }
        let (mut session, binding, work) = session_from(
            &tx,
            execution_space,
            original_project.space.as_str(),
            original_project.resource.as_str(),
            &stored.request.target_node,
            stored.request.session_id.as_deref(),
        )?
        .context("native execution session is missing")?;
        ensure!(
            stored.request.binding == binding
                && work == stored.wire.handoff.source_statement()?.work
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
            params![session_record(&session, &stored.wire.handoff.source_statement()?.work)?,execution_space,original_project.space.as_str(),original_project.resource.as_str(),stored.request.target_node,stored.request.session_id,session.revision])?;
        authority.check_usage_in(&tx, &stored, usage, None)?;
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

    pub(crate) fn execution_handoff_status_scoped(
        &self,
        view: &TaskBusinessScopedView,
        identity: &LocalIdentity,
        auth: &VerifiedExecutionHandoff,
        proof: &ExecutionHandoffFreshness,
        now_ms: u64,
    ) -> Result<AdmittedExecutionHandoff> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let tx = self.connection.unchecked_transaction()?;
        self.authorize_execution_handoff_read_scoped(view, auth, proof, now_ms)?;
        let (execution, original, operation) = scope(auth.wire())?;
        let stored = stored_from(&tx, &execution, &original, &operation)?
            .context("handoff read has no prior acceptance")?;
        ensure!(
            stored.wire == *auth.wire(),
            "handoff status changed its accepted request"
        );
        let result = dto(&tx, identity, &stored, now_ms)?;
        HandoffPolicy::TaskBusiness(view).check_response_usage(
            auth.proof_usage().clone(),
            proof,
            &stored,
            &result,
        )?;
        tx.commit()?;
        Ok(result)
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
                let verification =
                    vibeface_secure::signature_verification::SignatureVerification::new();
                verification.run(|| {
                    let stored = stored_from(&tx, &execution, &original, &operation)?
                        .context("recovery handoff disappeared")?;
                    super::execution_binding_seal::ensure_handoff_generation_open_in(
                        &tx,
                        &stored.wire,
                    )?;
                    reconstruct_handoff(&tx, &self.policy, &stored)?;
                    dto(&tx, identity, &stored, now_ms)
                })
            })
            .collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn mark_task_business_handoff_unknown(
        &mut self,
        identity: &LocalIdentity,
        source: &VerifiedTaskBusinessOriginalSource,
        reason: &str,
        now_ms: u64,
    ) -> Result<AdmittedExecutionHandoff> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let TaskBusinessWire::Submit(wire) = source.wire() else {
            anyhow::bail!("Unknown state requires an original accepted Submit")
        };
        let (execution, original, operation) = scope(wire)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::task_business_policy::check_task_business_original_source_in(
            &tx,
            &self.policy,
            source,
            source.wire(),
            source.acceptance(),
        )?;
        let mut stored = stored_from(&tx, &execution, &original, &operation)?
            .context("Unknown state has no original executor acceptance")?;
        let acceptance = RemoteExecutionHistoryAcceptance::ExecutionResult {
            receipt: stored.admission_receipt.clone(),
            freshness: stored.admission.clone(),
            transport: StoredTransport::from(&stored.transport),
        };
        ensure!(
            stored.wire == *wire
                && source.acceptance() == &acceptance
                && identity.public_key_hex()? == stored.wire.handoff.target_public_key.as_str(),
            "Unknown state changed its original executor acceptance or signer"
        );
        verify_local_handoff_checkpoint(&tx, &stored)?;
        ensure!(
            now_ms >= stored.admitted_at_ms,
            "Unknown state clock predates its acceptance"
        );
        ensure!(
            matches!(
                stored.state,
                OperationState::Accepted | OperationState::Running | OperationState::Unknown
            ),
            "terminal handoff cannot become Unknown"
        );
        if stored.state == OperationState::Unknown {
            clear_unknown_native_session_in(&tx, &stored)?;
            let value = dto(&tx, identity, &stored, now_ms)?;
            check_handoff_checkpoint_usage_in(
                &tx,
                &stored,
                source.proof_usage().clone(),
                Some(&value),
            )?;
            tx.commit()?;
            return Ok(value);
        }
        let dispatch: Event = serde_json::from_str(&wire.handoff.project_dispatch.canonical_body)?;
        stored.state = OperationState::Unknown;
        stored.result = json!({"reason":reason});
        if let Some((mut session, binding, work)) = session_from(
            &tx,
            &execution,
            &original,
            dispatch.resource_id.as_str(),
            &stored.request.target_node,
            stored.request.session_id.as_deref(),
        )? {
            check_session_work(
                &work,
                &stored.wire.handoff.target_public_key,
                &stored.request,
                &session,
                &binding,
            )?;
            ensure!(
                work == stored.wire.handoff.source_statement()?.work,
                "Unknown execution changed its original work"
            );
            session.state = "unknown".into();
            session.native_session_id = None;
            ensure!(tx.execute("UPDATE execution_handoff_sessions SET record=? WHERE execution_space_id=? AND original_space_id=? AND original_resource_id=? AND target_node=? AND session_id=? AND revision=?",
                params![session_record(&session, &work)?,execution,original,dispatch.resource_id.as_str(),session.target_node,session.session_id,session.revision])? == 1,
                "Unknown execution session changed before commit");
        }
        let receipt = signed_receipt(&tx, identity, &stored, now_ms)?;
        ensure!(tx.execute("UPDATE execution_handoffs SET state='unknown',result=?,result_receipt=? WHERE execution_space_id=? AND original_space_id=? AND operation_id=? AND state IN ('accepted','running') AND handoff_hash=?",
            params![serde_json::to_string(&stored.result)?,serde_json::to_string(&receipt)?,execution,original,operation,wire.hash()?.as_str()])? == 1,
            "handoff changed before its protective Unknown state");
        let value = dto(&tx, identity, &stored, now_ms)?;
        check_handoff_checkpoint_usage_in(
            &tx,
            &stored,
            source.proof_usage().clone(),
            Some(&value),
        )?;
        tx.commit()?;
        Ok(value)
    }

    pub fn finish_execution_handoff(
        &mut self,
        identity: &LocalIdentity,
        execution: &str,
        original: &str,
        operation: &str,
        state: OperationState,
        result: Value,
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
        self.finish_execution_handoff_with_authority(
            auth.as_ref().ok().map(HandoffRunningAuthorization::Primary),
            identity,
            execution,
            original,
            operation,
            state,
            result,
            session,
            now_ms,
        )
    }

    pub(crate) fn finish_execution_handoff_operation_current(
        &mut self,
        identity: &LocalIdentity,
        cap: &VerifiedTaskBusinessOperationCurrent,
        state: OperationState,
        result: Value,
        session: Option<&ExecutionSession>,
        now_ms: u64,
    ) -> Result<AdmittedExecutionHandoff> {
        let (execution, original, operation) = scope(cap.wire())?;
        self.finish_execution_handoff_with_authority(
            Some(HandoffRunningAuthorization::OperationCurrent(cap)),
            identity,
            &execution,
            &original,
            &operation,
            state,
            result,
            session,
            now_ms,
        )
    }

    fn finish_execution_handoff_with_authority(
        &mut self,
        authority: Option<HandoffRunningAuthorization<'_>>,
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
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut stored = stored_from(&tx, execution, original, operation)?
            .context("unknown execution handoff")?;
        ensure!(
            identity.public_key_hex()? == stored.wire.handoff.target_public_key.as_str(),
            "completion signer is not this executor"
        );
        let mut usage = ExecutionProofUsage::default();
        if let Some(value @ HandoffRunningAuthorization::OperationCurrent(cap)) = authority {
            value.check_accepted(&stored)?;
            ensure!(
                now_ms >= stored.admitted_at_ms,
                "operation completion clock predates its acceptance"
            );
            let original_source = cap.original_source();
            super::task_business_policy::check_task_business_original_source_in(
                &tx,
                &self.policy,
                original_source,
                original_source.wire(),
                original_source.acceptance(),
            )?;
            verify_local_handoff_checkpoint(&tx, &stored)?;
            value.check_latest_lease_in(&tx, &stored)?;
            usage.merge(cap.proof_usage());
            usage.merge(original_source.proof_usage());
        }
        let dispatch: Event =
            serde_json::from_str(&stored.wire.handoff.project_dispatch.canonical_body)?;
        let local_session = session_from(
            &tx,
            execution,
            original,
            dispatch.resource_id.as_str(),
            &stored.request.target_node,
            stored.request.session_id.as_deref(),
        )?;
        let native = session
            .and_then(|session| session.native_session_id.as_deref())
            .or_else(|| {
                local_session
                    .as_ref()
                    .and_then(|(session, _, _)| session.native_session_id.as_deref())
            });
        result = public_execution_result(result, native);
        if stored.state.terminal() {
            ensure!(
                stored.state == state && stored.result == result,
                "terminal handoff result cannot change"
            );
            let value = dto(&tx, identity, &stored, now_ms)?;
            if let Some(authority) = authority {
                authority.check_usage_in(&tx, &stored, usage, Some(&value))?;
            }
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
            let current_auth = authority.as_ref().map(|value| value.authorization());
            let lease_valid =
                now_ms < verified_lease_deadline(&tx, &self.policy, &stored, current_auth, None)?;
            let authorized = match authority {
                Some(value @ HandoffRunningAuthorization::Primary(_)) => {
                    value.check_current_in(&tx, &self.policy, now_ms)?;
                    true
                }
                Some(value @ HandoffRunningAuthorization::OperationCurrent(_)) => {
                    match value.check_current_in(&tx, &self.policy, now_ms) {
                        Ok(current_usage) => {
                            usage.merge(&current_usage);
                            super::execution_binding_seal::ensure_handoff_generation_open_in(
                                &tx,
                                value.authorization().wire(),
                            )
                            .is_ok()
                        }
                        Err(_) => false,
                    }
                }
                None => false,
            };
            if !authorized || !lease_valid {
                state = OperationState::Unknown;
                result = json!({"reason":"execution authorization lease is unavailable","observed_result":result});
            }
        }
        if let Some(updated) = session {
            ensure!(
                stored.request.session_id.is_some(),
                "command completion cannot create a session"
            );
            let (previous, binding, work) = session_from(
                &tx,
                execution,
                original,
                dispatch.resource_id.as_str(),
                &stored.request.target_node,
                stored.request.session_id.as_deref(),
            )?
            .context("execution session disappeared")?;
            ensure!(
                updated.network_id == execution
                    && updated.target_node == stored.request.target_node
                    && Some(updated.session_id.as_str()) == stored.request.session_id.as_deref()
                    && updated.cwd == previous.cwd
                    && updated.executor == previous.executor
                    && updated.revision >= previous.revision
                    && stored.request.binding == binding
                    && work == stored.wire.handoff.source_statement()?.work,
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
                params![updated.revision, session_record(&updated, &stored.wire.handoff.source_statement()?.work)?, execution, original, dispatch.resource_id.as_str(), stored.request.target_node, stored.request.session_id])?;
        } else if state.terminal() {
            if let Some((mut previous, _, _)) = session_from(
                &tx,
                execution,
                original,
                dispatch.resource_id.as_str(),
                &stored.request.target_node,
                stored.request.session_id.as_deref(),
            )? {
                previous.revision = previous.revision.max(
                    stored
                        .request
                        .expected_revision
                        .checked_add(1)
                        .context("execution revision overflow")?,
                );
                tx.execute("UPDATE execution_handoff_sessions SET revision=?,record=? WHERE execution_space_id=? AND original_space_id=? AND original_resource_id=? AND target_node=? AND session_id=?",
                    params![previous.revision, session_record(&previous, &stored.wire.handoff.source_statement()?.work)?, execution, original, dispatch.resource_id.as_str(), stored.request.target_node, stored.request.session_id])?;
            }
        }
        if state == OperationState::Unknown
            && session.is_none()
            && matches!(
                authority,
                Some(HandoffRunningAuthorization::OperationCurrent(_))
            )
        {
            clear_unknown_native_session_in(&tx, &stored)?;
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
        if let Some(authority) = authority {
            authority.check_usage_in(&tx, &stored, usage, Some(&value))?;
        }
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
    connection: &Connection,
    policy: &PolicyState,
    current: &VerifiedPolicySet,
    wire: &SignedExecutionHandoffResolution,
    now_ms: u64,
) -> Result<VerifiedAuthorization> {
    let facts = execution_resolution_account_facts(&wire.resolution)?;
    let proof = &wire.resolution.account_admission;
    let admitted =
        super::admission::reconstruct_stored_admission_uncached(connection, policy, proof, &facts)?;
    Ok(current.verify_admitted_operation_authorization(&admitted, &facts, now_ms)?)
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
        transport: serde_json::from_str::<StoredTransport>(&transport)?.existing_evidence()?,
        freshness: serde_json::from_str(&freshness)?,
        resolved_at_ms,
        result,
    }))
}

pub(super) fn remote_execution_history_acceptance_in(
    connection: &Connection,
    local: &PublicKey,
    reference: &vibeface_secure::authorization::remote_execution_history::RemoteExecutionHistoryReference,
) -> Result<(
    vibeface_secure::authorization::remote_execution_history::RemoteExecutionHistoryAcceptance,
    ExecutionProofUsage,
)> {
    use vibeface_secure::authorization::remote_execution_history::{
        RemoteExecutionHistoryAcceptance, RemoteExecutionHistoryKind,
        RemoteExecutionHistoryReference, derive_remote_execution_history_reference,
    };
    let stored = stored_from(
        connection,
        reference.execution_resource().space.as_str(),
        reference.source.payload.project.space.as_str(),
        &reference.source.payload.operation_id,
    )?
    .context("history has no original executor acceptance")?;
    verify_local_handoff_checkpoint(connection, &stored)?;
    ensure!(
        &stored.wire.handoff.target_public_key == local
            && reference.target_public_key() == local
            && stored.wire.hash()? == reference.handoff_hash
            && stored.wire.handoff.execution_resource == *reference.execution_resource()
            && stored.wire.handoff.source_attestation.as_ref() == Some(&reference.source)
            && stored.wire.handoff.source_statement()? == reference.source.payload
            && stored.wire.handoff.project_dispatch.hash == reference.dispatch_ref.as_str()
            && ObjectHash::body(&stored.wire.handoff.project_dispatch)?
                == reference.dispatch_envelope_hash,
        "history reference changed the executor's original wire or source checkpoint"
    );
    let mut usage = verify_execution_handoff_admission_checkpoint(
        &stored.wire,
        &stored.admission_receipt,
        &stored.admission,
        &StoredTransport::from(&stored.transport),
        stored.admitted_at_ms,
    )?
    .proof_usage()
    .clone();
    let acceptance = match reference.kind {
        RemoteExecutionHistoryKind::ExecutionResult => {
            verify_execution_handoff_receipt_signature(
                &stored.wire,
                &reference.observation_receipt,
                &stored.result,
            )?;
            let actual = ExecutionHandoffResult {
                handoff: stored.wire.clone(),
                receipt: reference.observation_receipt.clone(),
                result: stored.result.clone(),
            };
            let dispatch: Event =
                serde_json::from_str(&stored.wire.handoff.project_dispatch.canonical_body)?;
            let account = capsule_admission(&dispatch)?;
            let derived: RemoteExecutionHistoryReference = serde_json::from_str(
                &derive_remote_execution_history_reference(
                    &serde_json::to_string(&actual)?,
                    &serde_json::to_string(&account.source_body)?,
                    &serde_json::to_string(&account.invocation)?,
                    &serde_json::to_string(&stored.request.binding)?,
                )
                .map_err(anyhow::Error::msg)?,
            )?;
            ensure!(
                derived == *reference,
                "history changed its original execution result"
            );
            usage.record_signed(&reference.observation_receipt)?;
            RemoteExecutionHistoryAcceptance::ExecutionResult {
                receipt: stored.admission_receipt,
                freshness: stored.admission,
                transport: StoredTransport::from(&stored.transport),
            }
        }
        RemoteExecutionHistoryKind::UnknownResolution => {
            let encoded: String = connection.query_row(
                "SELECT request FROM execution_handoff_resolutions WHERE execution_space_id=? AND original_space_id=? AND operation_id=?",
                params![reference.execution_resource().space.as_str(), reference.source.payload.project.space.as_str(), reference.source.payload.operation_id],
                |row| row.get(0),
            )?;
            let wire: SignedExecutionHandoffResolution = serde_json::from_str(&encoded)?;
            let saved = stored_resolution_from(connection, &wire)?
                .context("history has no original executor resolution")?;
            let account = &saved.wire.resolution.account_admission;
            let derived: RemoteExecutionHistoryReference = serde_json::from_str(
                &derive_remote_execution_history_reference(
                    &serde_json::to_string(&saved.result)?,
                    &serde_json::to_string(&account.source_body)?,
                    &serde_json::to_string(&account.invocation)?,
                    "null",
                )
                .map_err(anyhow::Error::msg)?,
            )?;
            ensure!(
                derived == *reference,
                "history changed its original resolution result"
            );
            verify_execution_resolution_admission_checkpoint(
                &saved.result,
                &saved.freshness,
                &StoredTransport::from(&saved.transport),
                saved.resolved_at_ms,
            )?;
            usage.record_signed(&saved.result.receipt)?;
            usage.record_signed(&saved.result.handoff_receipt)?;
            usage.record_metadata(&saved.freshness)?;
            usage.record_metadata(&StoredTransport::from(&saved.transport))?;
            usage.record_signed(&reference.observation_receipt)?;
            RemoteExecutionHistoryAcceptance::UnknownResolution {
                receipt: saved.result.receipt,
                freshness: saved.freshness,
            }
        }
        RemoteExecutionHistoryKind::TerminalReconciliation => {
            ensure!(
                stored.state.terminal(),
                "history has no terminal executor observation"
            );
            verify_execution_handoff_receipt_signature(
                &stored.wire,
                &reference.observation_receipt,
                &stored.result,
            )?;
            usage.record_signed(&reference.observation_receipt)?;
            RemoteExecutionHistoryAcceptance::TerminalReconciliation {
                freshness: reference
                    .terminal_freshness
                    .clone()
                    .context("terminal history has no original signed freshness proof")?,
                observation_receipt: reference.observation_receipt.clone(),
            }
        }
    };
    usage.enforce_limits()?;
    Ok((acceptance, usage))
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
    let session = session_from(
        connection,
        &execution,
        &original,
        dispatch.resource_id.as_str(),
        &stored.request.target_node,
        stored.request.session_id.as_deref(),
    )?;
    let snapshot = wire.resolution.observation.receipt.payload.session.as_ref();
    match (session, snapshot) {
        (Some((session, binding, work)), Some(snapshot)) => {
            ensure!(
                work == stored.wire.handoff.source_statement()?.work && snapshot.work == work,
                "unknown receipt changed its original work"
            );
            check_session_work(
                &work,
                &stored.wire.handoff.target_public_key,
                &stored.request,
                &session,
                &binding,
            )?;
            ensure!(
                snapshot.revision == session.revision
                    && snapshot.session_id == session.session_id
                    && snapshot.target == stored.wire.handoff.execution_audience.device
                    && snapshot.cwd == session.cwd
                    && snapshot.executor == session.executor
                    && snapshot.state == session.state,
                "unknown receipt differs from the executor's current session"
            );
        }
        (None, None) => ensure!(
            matches!(
                stored.wire.handoff.source_statement()?.work,
                ExecutionWork::Command { .. }
            ),
            "session work lost its signed session state"
        ),
        _ => anyhow::bail!("unknown receipt changed its session presence"),
    }
    Ok(stored)
}

impl Store {
    pub fn execution_handoff_resolution_authorization(
        &self,
        wire: &SignedExecutionHandoffResolution,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<VerifiedExecutionResolution> {
        self.execution_handoff_resolution_authorization_with_policy(
            HandoffPolicy::Primary,
            wire,
            transport,
            now_ms,
        )
    }

    pub(crate) fn execution_handoff_resolution_authorization_scoped(
        &self,
        view: &TaskBusinessScopedView,
        wire: &SignedExecutionHandoffResolution,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<VerifiedExecutionResolution> {
        self.execution_handoff_resolution_authorization_with_policy(
            HandoffPolicy::TaskBusiness(view),
            wire,
            transport,
            now_ms,
        )
    }

    fn execution_handoff_resolution_authorization_with_policy(
        &self,
        current: HandoffPolicy<'_>,
        wire: &SignedExecutionHandoffResolution,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<VerifiedExecutionResolution> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        current.check_business_wire_in(
            &self.connection,
            &self.policy,
            &TaskBusinessWire::Resolve(wire.clone()),
            now_ms,
        )?;
        let resolver = current
            .state(&self.policy)
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
        let verified = current.state(&self.policy).verify_execution_resolution(
            wire,
            &resolver,
            transport,
            admission.as_ref(),
            now_ms,
        )?;
        let (execution, original, operation) = scope(&wire.resolution.observation.handoff)?;
        let stored = stored_from(&self.connection, &execution, &original, &operation)?
            .context("resolution acceptance checkpoint disappeared")?;
        let verified = verified.with_admission_checkpoint(
            &stored.admission_receipt,
            &stored.admission,
            &StoredTransport::from(&stored.transport),
            stored.admitted_at_ms,
        )?;
        if matches!(current, HandoffPolicy::TaskBusiness(_)) {
            current.check_resolution_authorizations_in(
                &self.connection,
                &self.policy,
                &verified,
                now_ms,
            )?;
            current.check_usage(verified.proof_usage().clone())?;
        }
        Ok(verified)
    }

    pub fn execution_handoff_resolution_status(
        &self,
        wire: &SignedExecutionHandoffResolution,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<Option<ExecutionHandoffResolutionResult>> {
        self.execution_handoff_resolution_status_with_policy(
            HandoffPolicy::Primary,
            wire,
            transport,
            now_ms,
        )
    }

    pub(crate) fn execution_handoff_resolution_status_scoped(
        &self,
        view: &TaskBusinessScopedView,
        wire: &SignedExecutionHandoffResolution,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<Option<ExecutionHandoffResolutionResult>> {
        self.execution_handoff_resolution_status_with_policy(
            HandoffPolicy::TaskBusiness(view),
            wire,
            transport,
            now_ms,
        )
    }

    fn execution_handoff_resolution_status_with_policy(
        &self,
        current: HandoffPolicy<'_>,
        wire: &SignedExecutionHandoffResolution,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<Option<ExecutionHandoffResolutionResult>> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let read_tx = if matches!(current, HandoffPolicy::TaskBusiness(_)) {
            Some(self.connection.unchecked_transaction()?)
        } else {
            None
        };
        let result = (|| -> Result<Option<ExecutionHandoffResolutionResult>> {
            current.check_business_wire_in(
                &self.connection,
                &self.policy,
                &TaskBusinessWire::Resolve(wire.clone()),
                now_ms,
            )?;
            if wire.resolution.purpose == ExecutionResolutionPurpose::ReconcileTerminal {
                return Ok(None);
            }
            let Some(saved) = stored_resolution_from(&self.connection, wire)? else {
                if matches!(current, HandoffPolicy::TaskBusiness(_)) {
                    self.execution_handoff_resolution_authorization_with_policy(
                        current, wire, transport, now_ms,
                    )?;
                }
                return Ok(None);
            };
            let auth = self.execution_handoff_resolution_authorization_with_policy(
                current, wire, transport, now_ms,
            )?;
            current.check_resolution_authorizations_in(
                &self.connection,
                &self.policy,
                &auth,
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
            let mut usage = current.resolution_usage(&auth, &saved.freshness)?;
            usage.record_signed(&saved.result.receipt)?;
            usage.record_signed(&saved.result.handoff_receipt)?;
            current.check_usage(usage)?;
            Ok(Some(saved.result))
        })()?;
        if let Some(tx) = read_tx {
            tx.commit()?;
        }
        Ok(result)
    }

    pub fn issue_execution_handoff_resolution_challenge(
        &mut self,
        wire: &SignedExecutionHandoffResolution,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<ExecutionHandoffResolutionChallenge> {
        self.issue_execution_handoff_resolution_challenge_with_policy(
            HandoffPolicy::Primary,
            wire,
            transport,
            now_ms,
        )
    }

    pub(crate) fn issue_execution_handoff_resolution_challenge_scoped(
        &mut self,
        view: &TaskBusinessScopedView,
        wire: &SignedExecutionHandoffResolution,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<ExecutionHandoffResolutionChallenge> {
        self.issue_execution_handoff_resolution_challenge_with_policy(
            HandoffPolicy::TaskBusiness(view),
            wire,
            transport,
            now_ms,
        )
    }

    fn issue_execution_handoff_resolution_challenge_with_policy(
        &mut self,
        current: HandoffPolicy<'_>,
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
        let auth = self.execution_handoff_resolution_authorization_with_policy(
            current, wire, transport, now_ms,
        )?;
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
        current.check_resolution_authorizations_in(&tx, &self.policy, &auth, now_ms)?;
        resolution_original_observation_from(&tx, &self.policy, wire)?;
        let mut usage = auth.proof_usage().clone();
        usage.record_metadata(&challenge)?;
        current.check_usage(usage)?;
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
        self.sign_execution_handoff_resolution_receipts_with_policy(
            HandoffPolicy::Primary,
            challenge,
            identity,
            now_ms,
        )
    }

    pub(crate) fn sign_execution_handoff_resolution_receipts_scoped(
        &self,
        view: &TaskBusinessScopedView,
        wire: &SignedExecutionHandoffResolution,
        transport: &TransportEvidence,
        challenge: &ExecutionHandoffResolutionChallenge,
        identity: &LocalIdentity,
        now_ms: u64,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let current = HandoffPolicy::TaskBusiness(view);
        let tx = self.connection.unchecked_transaction()?;
        let auth = self.execution_handoff_resolution_authorization_with_policy(
            current, wire, transport, now_ms,
        )?;
        ensure!(
            identity.public_key_hex()?
                == wire
                    .resolution
                    .observation
                    .handoff
                    .handoff
                    .target_public_key
                    .as_str(),
            "resolution receipt signer is not this executor"
        );
        current.check_resolution_authorizations_in(&tx, &self.policy, &auth, now_ms)?;
        verify_resolution_challenge_record(
            &tx,
            wire,
            &ExecutionHandoffResolutionFreshness {
                challenge: challenge.clone(),
                receipts: Vec::new(),
            },
            false,
        )?;
        let receipts = self.sign_execution_handoff_resolution_receipts_with_policy(
            current, challenge, identity, now_ms,
        )?;
        let mut usage = auth.proof_usage().clone();
        usage.record_metadata(challenge)?;
        for receipt in &receipts {
            usage.record_signed(receipt)?;
        }
        current.check_usage(usage)?;
        tx.commit()?;
        Ok(receipts)
    }

    fn sign_execution_handoff_resolution_receipts_with_policy(
        &self,
        current: HandoffPolicy<'_>,
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
        let mut currents = Vec::new();
        for stamp in &challenge.policies {
            ensure!(
                current.state(&self.policy).head(&stamp.domain) == Some(stamp),
                "resolution freshness policy changed"
            );
            if let HandoffPolicy::TaskBusiness(view) = current {
                if !view.primary_heads().contains(stamp) {
                    continue;
                }
                observation::check_heads_in(
                    &self.connection,
                    self.policy.verified(),
                    std::slice::from_ref(stamp),
                )?;
            }
            let signer_policy = match current {
                HandoffPolicy::Primary => current.state(&self.policy),
                HandoffPolicy::TaskBusiness(_) => self.policy.verified(),
            };
            let authority = match &stamp.domain {
                PolicyDomain::Account(id) => {
                    &signer_policy
                        .account(id)
                        .context("missing resolution account")?
                        .authority
                }
                PolicyDomain::Space(id) => {
                    &signer_policy
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
                currents.push(stamp.clone());
            }
        }
        if currents.is_empty() {
            return Ok(Vec::new());
        }
        currents.sort_by(|left, right| left.domain.cmp(&right.domain));
        let receipt = FreshnessReceipt {
            version: AUTH_VERSION,
            currents,
            challenge_hash: challenge.hash()?,
            issued_at_ms: now_ms,
            expires_at_ms: challenge.expires_at_ms,
        };
        receipt.validate_currents()?;
        Ok(vec![Signed::sign(receipt, &signer)?])
    }

    pub fn authorize_execution_handoff_resolution_fence(
        &self,
        auth: &VerifiedExecutionResolution,
        proof: &ExecutionHandoffResolutionFreshness,
        now_ms: u64,
    ) -> Result<()> {
        self.authorize_execution_handoff_resolution_fence_with_policy(
            HandoffPolicy::Primary,
            auth,
            proof,
            now_ms,
        )
    }

    pub(crate) fn authorize_execution_handoff_resolution_fence_scoped(
        &self,
        view: &TaskBusinessScopedView,
        auth: &VerifiedExecutionResolution,
        proof: &ExecutionHandoffResolutionFreshness,
        now_ms: u64,
    ) -> Result<()> {
        self.authorize_execution_handoff_resolution_fence_with_policy(
            HandoffPolicy::TaskBusiness(view),
            auth,
            proof,
            now_ms,
        )
    }

    fn authorize_execution_handoff_resolution_fence_with_policy(
        &self,
        current: HandoffPolicy<'_>,
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
        current.check_resolution_authorizations_in(
            &self.connection,
            &self.policy,
            &auth,
            now_ms,
        )?;
        current
            .state(&self.policy)
            .verify_execution_resolution_freshness(auth, proof, now_ms)?;
        current.resolution_usage(auth, proof)?;
        verify_resolution_challenge_record(&self.connection, auth.wire(), proof, false)
    }

    pub fn authorize_execution_handoff_terminal_reconciliation(
        &self,
        auth: &VerifiedExecutionResolution,
        proof: &ExecutionHandoffResolutionFreshness,
        now_ms: u64,
    ) -> Result<ExecutionHandoffTerminalReconciliationResult> {
        self.authorize_execution_handoff_terminal_reconciliation_with_policy(
            HandoffPolicy::Primary,
            auth,
            proof,
            now_ms,
        )
    }

    pub(crate) fn authorize_execution_handoff_terminal_reconciliation_scoped(
        &self,
        view: &TaskBusinessScopedView,
        auth: &VerifiedExecutionResolution,
        proof: &ExecutionHandoffResolutionFreshness,
        now_ms: u64,
    ) -> Result<ExecutionHandoffTerminalReconciliationResult> {
        self.authorize_execution_handoff_terminal_reconciliation_with_policy(
            HandoffPolicy::TaskBusiness(view),
            auth,
            proof,
            now_ms,
        )
    }

    fn authorize_execution_handoff_terminal_reconciliation_with_policy(
        &self,
        current: HandoffPolicy<'_>,
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
        current.check_resolution_authorizations_in(
            &self.connection,
            &self.policy,
            &auth,
            now_ms,
        )?;
        current
            .state(&self.policy)
            .verify_execution_resolution_freshness(auth, proof, now_ms)?;
        current.resolution_usage(auth, proof)?;
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
        self.resolve_execution_handoff_with_policy(
            HandoffPolicy::Primary,
            identity,
            wire,
            proof,
            transport,
            stopped,
            now_ms,
        )
    }

    pub(crate) fn resolve_execution_handoff_scoped(
        &mut self,
        view: &TaskBusinessScopedView,
        identity: &LocalIdentity,
        wire: &SignedExecutionHandoffResolution,
        proof: &ExecutionHandoffResolutionFreshness,
        transport: &TransportEvidence,
        stopped: &crate::engine::StoppedExecution,
        now_ms: u64,
    ) -> Result<ExecutionHandoffResolutionResult> {
        self.resolve_execution_handoff_with_policy(
            HandoffPolicy::TaskBusiness(view),
            identity,
            wire,
            proof,
            transport,
            stopped,
            now_ms,
        )
    }

    fn resolve_execution_handoff_with_policy(
        &mut self,
        current: HandoffPolicy<'_>,
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
        if let Some(result) =
            self.execution_handoff_resolution_status_with_policy(current, wire, transport, now_ms)?
        {
            return Ok(result);
        }
        let auth = self.execution_handoff_resolution_authorization_with_policy(
            current, wire, transport, now_ms,
        )?;
        current
            .state(&self.policy)
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
        current.check_resolution_authorizations_in(&tx, &self.policy, &auth, now_ms)?;
        current
            .state(&self.policy)
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
        if let Some((mut session, binding, work)) = session_from(
            &tx,
            &execution,
            &original,
            dispatch.resource_id.as_str(),
            &stored.request.target_node,
            stored.request.session_id.as_deref(),
        )? {
            ensure!(
                stored.request.binding == binding
                    && work == stored.wire.handoff.source_statement()?.work,
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
            params![session.revision,session_record(&session, &stored.wire.handoff.source_statement()?.work)?,execution,original,dispatch.resource_id.as_str(),session.target_node,session.session_id])?;
        }
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
        if matches!(current, HandoffPolicy::TaskBusiness(_)) {
            current.bind_original_in(
                &tx,
                &self.policy,
                &TaskBusinessWire::Resolve(wire.clone()),
                &RemoteExecutionHistoryAcceptance::UnknownResolution {
                    receipt: result.receipt.clone(),
                    freshness: proof.clone(),
                },
                now_ms,
            )?;
            let mut usage = current.resolution_usage(&auth, proof)?;
            usage.record_signed(&result.receipt)?;
            usage.record_signed(&result.handoff_receipt)?;
            current.check_usage(usage)?;
        }
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
    usage
        .enforce_limits()
        .with_context(|| format!("executor lease proof budget {:?}", usage.counts()))?;
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
        reconstruct_handoff(connection, policy, &stored)?;
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
            ) || stored.lease.is_some()
                || (stored.state == OperationState::Unknown && stored.receipt.is_some()),
            "started handoff has no verified lease"
        );
        if let Some(receipt) = &stored.receipt {
            receipt.verify_signature()?;
            let value = &receipt.payload;
            ensure!(
                receipt.issuer == stored.wire.handoff.target_public_key
                    && value.version == AUTH_VERSION
                    && value.original_space_id.as_str() == original
                    && value.execution_space_id.as_str() == execution
                    && value.operation_id == operation
                    && value.handoff_hash == stored.wire.hash()?
                    && value.target == stored.wire.handoff.execution_audience.device
                    && value.work == stored.wire.handoff.source_statement()?.work
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
        store::load_events_from,
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
                session_id: Some("same-session".into()),
                expected_revision: 0,
                basis_heads: vec![task.hash.clone()],
                action: bound.binding.open_spec.clone(),
                resources: vec![executor.project.to_str().unwrap().into()],
                binding: Some(bound),
                manager_authority: None,
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
                    session_id: operation
                        .session_id
                        .clone()
                        .context("fixture session is missing")?,
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
                origin: ExecutionOrigin::TaskSession {
                    task_creation: task,
                },
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
                    execution_source: None,
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
            let destination = store.policy.verified().verify_execution_destination(
                &facts.resource,
                &facts.audience,
                &execution_resource,
                &handoff.target_public_key,
                &leaf.hash()?,
                &facts.context,
                facts.action,
                now(),
            )?;
            payload
                .expected_policies
                .extend_from_slice(destination.heads());
            payload
                .expected_policies
                .sort_by(|left, right| left.domain.cmp(&right.domain));
            payload
                .expected_policies
                .dedup_by(|left, right| left.domain == right.domain);
            proof.invocation = Signed::sign(payload, &project.client)?;
            let mut final_facts = facts.clone();
            final_facts.body_hash = ObjectHash::body(&proof.source_body)?;
            let final_auth = store
                .policy
                .verified()
                .verify_invocation_with_execution_destination(
                    &proof.invocation,
                    &final_facts,
                    &source_transport,
                    &destination,
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
            let original_auth = store
                .policy
                .verified()
                .verify_admitted_operation_authorization(&final_admitted, &final_facts, now())?;
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
            invocation
                .expected_policies
                .extend(descriptor.expected_policies);
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
            let destination = crate::identity::policy::execution_destination_for_body(
                fixture.store.policy.verified(),
                &facts,
                &proof.source_body,
                now(),
            )?
            .context("SDK fixture execution destination")?;
            let auth = fixture
                .store
                .policy
                .verified()
                .verify_invocation_with_execution_destination(
                    &proof.invocation,
                    &facts,
                    &transport,
                    &destination,
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
            )
            .context("SDK fixture original source")?;
            let facts = execution_submission_facts(&fixture.wire.handoff, &original)?;
            let mut carrier = fixture.wire.invocation.payload.clone();
            carrier.body_hash = facts.body_hash;
            carrier.context = facts.context;
            carrier.nonce = Nonce::random()?;
            fixture.wire.invocation = Signed::sign(carrier, &signer)?;
            Ok(fixture)
        }

        fn retain_source_dispatch(&mut self) -> Result<()> {
            use vibeface_secure::authorization::{
                client_policy_import::AuthorizationSelection,
                prepare::{PrepareContext, PrepareIntent},
                remote_execution_policy::{
                    RemoteExecutionPolicyPurpose, RemoteExecutionPolicyReadIntent,
                    build_remote_execution_policy_read_intent,
                },
            };
            let original: Event =
                serde_json::from_str(&self.wire.handoff.project_dispatch.canonical_body)?;
            let original = capsule_admission(&original)?;
            let operation: OperationRequest =
                serde_json::from_value(original.source_body["request"].clone())?;
            let mut context = original.invocation.payload.context.clone();
            context.sdk_session = None;
            let context = PrepareContext {
                context,
                binding_generation: Some(1),
                canonical_cwd: Some(self.executor.project.to_string_lossy().into_owned()),
                execution_target_public_key: Some(self.wire.handoff.target_public_key.clone()),
            };
            let source = super::super::client_policy_read::verified_client_policy_for_test(
                &mut self.store,
                &self.project.node,
                &self.project.client,
                &self.project.account,
                AuthorizationSelection {
                    resource: original.invocation.payload.resource.clone(),
                    audience: original.invocation.payload.audience.clone(),
                    grant_leaf: self.project.bundle.grant_leaf.clone(),
                },
                &self.project.pins,
                now(),
            )?;
            let intent = build_remote_execution_policy_read_intent(
                &source,
                &json!({
                    "work":self.wire.handoff.source_statement()?.work.intent(),
                    "selection":{
                        "execution_resource":self.wire.handoff.execution_resource,
                        "target_public_key":self.wire.handoff.target_public_key,
                        "execution_grant":original.source_body["execution_grant"],
                    },
                    "context":context,
                    "expected_action":Action::ExecutionOpen,
                    "purpose":RemoteExecutionPolicyPurpose::Execution,
                    "execution_pins":self.executor.pins,
                })
                .to_string(),
                &self.project.client,
                now() as f64,
                (now() + 60_000) as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let intent: Signed<RemoteExecutionPolicyReadIntent> = serde_json::from_str(&intent)?;
            let snapshot = self
                .store
                .policy
                .accepted_journal_snapshot(&self.store.connection)?;
            let domains = std::collections::BTreeSet::from([
                PolicyDomain::Account(self.executor.account.clone()),
                PolicyDomain::Space(self.executor.space.clone()),
            ]);
            let mut cursor = None;
            let mut journal = vec![];
            loop {
                let page = self.store.policy.accepted_journal_page(
                    &self.store.connection,
                    &snapshot,
                    cursor.as_ref(),
                    &domains,
                )?;
                if let Some(entry) = page.entry {
                    journal.push(entry.entry);
                }
                if page.complete {
                    break;
                }
                cursor = Some(page.next_cursor);
            }
            let mut target = self.executor.memory()?;
            target.install_policy_journal(&journal, now())?;
            let remote = super::super::foreign_execution_policy::tests::import_current_policy_in(
                &self.project,
                &self.executor,
                &mut self.store,
                &mut target,
                &source,
                &intent,
                None,
            )?;
            let bind = json!({
                "method":"bind_execution","space_id":self.project.space,
                "resource_id":self.project.resource,"authorization_id":"handoff-bind",
                "binding_generation":1,"conversation_id":operation.conversation_id,
                "task_id":operation.task_id,"task_ref":context.context.task,
                "target_node":operation.target_node,"session_id":operation.session_id,
                "executor":context.context.executor_kind,"cwd":self.executor.project,
                "execution_resource":self.wire.handoff.execution_resource,
                "target_public_key":self.wire.handoff.target_public_key,
                "execution_grant":original.source_body["execution_grant"],
            });
            for binding in [true, false] {
                let body = if binding {
                    &bind
                } else {
                    &original.source_body
                };
                let invocation = if binding {
                    let prepared = remote
                        .prepare(
                            &source,
                            &body.to_string(),
                            &serde_json::to_string(&context)?,
                            "execution_open",
                            "handoff-bind",
                            &self.project.node.public_key_hex()?,
                            &self.project.client,
                            now() as f64,
                        )
                        .map_err(anyhow::Error::msg)?;
                    let prepared: Signed<PrepareIntent> = serde_json::from_str(&prepared)?;
                    let invocation = remote
                        .sign(
                            &source,
                            &serde_json::to_string(&prepared.payload.request)?,
                            &body.to_string(),
                            "execution_open",
                            "handoff-bind",
                            &self.project.node.public_key_hex()?,
                            &self.project.client,
                            now() as f64,
                        )
                        .map_err(anyhow::Error::msg)?;
                    serde_json::from_str(&invocation)?
                } else {
                    original.invocation.clone()
                };
                let value = &invocation.payload;
                let facts = RequestFacts {
                    resource: value.resource.clone(),
                    action: value.action,
                    audience: value.audience.clone(),
                    operation_id: value.operation_id.clone(),
                    body_hash: value.body_hash.clone(),
                    context: value.context.clone(),
                };
                let transport = if binding {
                    TransportEvidence::DirectNoise {
                        device: DeviceId::from_key(&PublicKey::of(&self.project.client)),
                    }
                } else {
                    original.transport.existing_evidence()?
                };
                let scoped = self.store.authorize_scoped_request_for_body(
                    &self.project.node,
                    &invocation,
                    &facts,
                    &transport,
                    body,
                    now(),
                )?;
                let dispatch = self.store.execution_dispatch_facts_in_scope(
                    &scoped,
                    body,
                    scoped.audience(),
                    now(),
                )?;
                let challenge =
                    self.store
                        .issue_scoped_freshness(&scoped, CheckStage::Admit, now())?;
                let mut receipts =
                    self.store
                        .sign_current_receipts(&challenge, &self.project.node, now())?;
                receipts.extend(target.sign_current_receipts(
                    &challenge,
                    &self.executor.node,
                    now(),
                )?);
                let proof = super::super::policy::FreshnessProof {
                    challenge,
                    receipts,
                };
                if binding {
                    let bound = self.store.bind_execution_dispatch(
                        &self.project.node,
                        &dispatch,
                        &scoped,
                        &proof,
                        &transport,
                        body,
                        now(),
                    )?;
                    assert_eq!(Some(&bound), operation.binding.as_ref());
                } else {
                    self.store.accept_execution_dispatch(
                        &self.project.node,
                        &dispatch,
                        &scoped,
                        &proof,
                        &transport,
                        body,
                        &operation,
                        now(),
                    )?;
                }
            }
            self.wire = self
                .store
                .prepare_execution_dispatch_handoff(
                    &self.project.node,
                    self.project.space.as_str(),
                    "original-operation",
                    now(),
                )?
                .0;
            Ok(())
        }

        fn verify_builder_and_isolate_executor(&mut self) -> Result<()> {
            self.retain_source_dispatch()?;
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
                    execution_source: None,
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
                "task_id":"task-handoff","task_ref":self.wire.handoff.source_statement()?.work.task_ref().context("fixture Task missing")?,"target_node":self.executor.node.node_id()?,
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
            let destination = super::super::policy::execution_destination_for_body(
                policy,
                &facts,
                &resolution.account_admission.source_body,
                now() + 50,
            )?
            .context("resolution fixture destination")?;
            let first = policy.verify_invocation_with_execution_destination(
                &resolution.account_admission.invocation,
                &facts,
                &transport,
                &destination,
                now() + 50,
            );
            let auth = match first {
                Err(AuthorizationError::NeedPolicySync(expected)) => {
                    account.expected_policies = expected;
                    resolution.account_admission.invocation =
                        Signed::sign(account, &self.executor.client)?;
                    policy.verify_invocation_with_execution_destination(
                        &resolution.account_admission.invocation,
                        &facts,
                        &transport,
                        &destination,
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

        fn observation(
            &mut self,
        ) -> Result<(
            execution_observation::SignedExecutionObservation,
            execution_observation::ExecutionObservationFreshness,
        )> {
            use execution_observation::*;
            let original: Event =
                serde_json::from_str(&self.wire.handoff.project_dispatch.canonical_body)?;
            let original = capsule_admission(&original)?;
            let mut context = original.invocation.payload.context.clone();
            context.sdk_session = None;
            let account = self.grant(
                true,
                Actor::Account(self.executor.account.clone()),
                vec![Action::OperationResolve],
                original.invocation.payload.resource.clone(),
                original.invocation.payload.audience.clone(),
                context.clone(),
            )?;
            context.workspace_resource =
                Some(self.wire.handoff.execution_resource.resource.clone());
            let execution = self.grant(
                false,
                Actor::Service(original.invocation.payload.audience.service.clone()),
                vec![Action::ExecutionResolve, Action::OperationResolve],
                self.wire.handoff.execution_resource.clone(),
                self.wire.handoff.execution_audience.clone(),
                context,
            )?;
            let request = self
                .store
                .policy
                .verified()
                .build_execution_observation_request(
                    &self.wire,
                    &self.executor.client,
                    &account.hash()?,
                    &execution.hash()?,
                    "explicit-observation",
                    now() + 50,
                    now() + 10_050,
                )?;
            let signer = Identity::from_secret_hex(&self.project.node.secret_key_hex)
                .map_err(anyhow::Error::msg)?;
            let wire = self.store.policy.verified().sign_execution_observation(
                &request,
                &self.wire,
                &signer,
                now() + 51,
                now() + 10_050,
            )?;
            let challenge = self.store.issue_execution_observation_challenge(
                &self.executor.node,
                &wire,
                &self.transport,
                now() + 52,
            )?;
            let mut receipts = self.store.sign_execution_observation_receipts(
                &challenge,
                &self.project.node,
                now() + 52,
            )?;
            receipts.extend(self.store.sign_execution_observation_receipts(
                &challenge,
                &self.executor.node,
                now() + 52,
            )?);
            Ok((
                wire,
                ExecutionObservationFreshness {
                    challenge,
                    receipts,
                },
            ))
        }

        fn deep_grant(
            &mut self,
            owner_project: bool,
            template: Grant,
            last: Actor,
            intermediaries: &[TestNetwork],
        ) -> Result<ObjectHash> {
            let owner = if owner_project {
                &self.project
            } else {
                &self.executor
            };
            let mut parent = None;
            for depth in 0..MAX_DELEGATION_DEPTH {
                let stamp = self
                    .store
                    .policy
                    .verified()
                    .head(&PolicyDomain::Space(owner.space.clone()))
                    .unwrap()
                    .clone();
                let mut grant = template.clone();
                grant.issued_under = stamp.clone();
                grant.parent = parent;
                grant.remaining_delegations = (MAX_DELEGATION_DEPTH - depth) as u8;
                if depth + 1 == MAX_DELEGATION_DEPTH && matches!(last, Actor::Service(_)) {
                    grant.remaining_delegations = 0;
                }
                grant.grantee = Grantee::Actor(if depth + 1 == MAX_DELEGATION_DEPTH {
                    last.clone()
                } else if let Some(intermediary) = intermediaries.get(depth) {
                    Actor::Account(intermediary.account.clone())
                } else {
                    Actor::Account(owner.account.clone())
                });
                grant.nonce = Nonce::random()?;
                let signed = Signed::sign(
                    grant,
                    if depth == 0 {
                        &owner.root
                    } else if let Some(intermediary) = intermediaries.get(depth - 1) {
                        &intermediary.client
                    } else {
                        &owner.client
                    },
                )?;
                let hash = signed.hash()?;
                let mut document = self
                    .store
                    .policy
                    .verified()
                    .space(&owner.space)
                    .unwrap()
                    .clone();
                document.previous = Some(stamp);
                document.sequence += 1;
                document.grants.push(hash.clone());
                document.grants.sort();
                self.store.install_policy_journal(
                    &[
                        JournalEntry::Grant(signed),
                        JournalEntry::Space(Signed::sign(document, &owner.root)?),
                    ],
                    now(),
                )?;
                parent = Some(hash);
            }
            Ok(parent.unwrap())
        }

        fn resign_resolution_account(
            &self,
            wire: &mut SignedExecutionHandoffResolution,
            bind_destination: bool,
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
                resource: invocation.resource.clone(),
                action: invocation.action,
                audience: invocation.audience.clone(),
                operation_id: invocation.operation_id.clone(),
                body_hash: invocation.body_hash.clone(),
                context: invocation.context.clone(),
            };
            let policy = self.store.policy.verified();
            let destination = if bind_destination {
                super::super::policy::execution_destination_for_body(
                    policy,
                    &facts,
                    &proof.source_body,
                    now() + 50,
                )?
            } else {
                None
            };
            let transport = proof.transport.existing_evidence()?;
            let authorize = |signed| match &destination {
                Some(destination) => policy.verify_invocation_with_execution_destination(
                    signed,
                    &facts,
                    &transport,
                    destination,
                    now() + 50,
                ),
                None => policy.verify_invocation(signed, &facts, &transport, now() + 50),
            };
            let auth = match authorize(&proof.invocation) {
                Err(AuthorizationError::NeedPolicySync(expected)) => {
                    invocation.expected_policies = expected;
                    proof.invocation = Signed::sign(invocation, &self.executor.client)?;
                    authorize(&proof.invocation)?
                }
                other => other?,
            };
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
            self.accept().context("admit Unknown fixture")?;
            let proof = self
                .freshness(CheckStage::Start, now() + 3)
                .context("refresh Unknown fixture")?;
            let auth = self
                .store
                .execution_handoff_authorization(
                    self.executor.space.as_str(),
                    self.project.space.as_str(),
                    "original-operation",
                    now() + 4,
                )
                .context("authorize Unknown fixture")?;
            let (_, fresh, _) = self
                .store
                .claim_execution_handoff(&self.executor.node, &auth, &proof, now() + 4)
                .context("claim Unknown fixture")?;
            assert!(fresh);
            self.store
                .load_execution_handoffs_for_recovery(&self.executor.node, now() + 5)
                .context("recover Unknown fixture")?;
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

        fn remote_seal_body(&self) -> Result<Value> {
            let observed = self.store.execution_handoff_status(
                &self.executor.node,
                self.executor.space.as_str(),
                self.project.space.as_str(),
                "original-operation",
                now() + 20,
            )?;
            Ok(
                json!({"method":"seal_remote_execution","space_id":self.executor.space,
                "resource_id":self.wire.handoff.execution_resource.resource,
                "authorization_id":"seal-generation-one","original_space_id":self.project.space,
                "operation_id":"original-operation","binding":request(&self.wire)?.binding,
                "execution_receipt":observed.receipt.context("C receipt is missing")?}),
            )
        }

        fn executor_seal_grant(&mut self, body: &Value) -> Result<ObjectHash> {
            let key = PublicKey::parse(&self.executor.node.public_key_hex()?)?;
            let (facts, _) = execution_binding_seal::execution_binding_seal_facts(
                body,
                &self.wire.handoff.execution_audience,
                &key,
            )?;
            let previous = self
                .store
                .policy
                .verified()
                .head(&PolicyDomain::Space(self.executor.space.clone()))
                .unwrap()
                .clone();
            let grant = Signed::sign(
                Grant {
                    execution_source: None,
                    version: AUTH_VERSION,
                    owner_space: self.executor.space.clone(),
                    scope: ResourceScope::Exact(facts.resource),
                    grantee: Grantee::Actor(Actor::Account(self.executor.account.clone())),
                    actions: vec![Action::ExecutionRebind, Action::OperationStatusRead],
                    audience: facts.audience,
                    constraints: GrantConstraints {
                        context: facts.context,
                        ..Default::default()
                    },
                    parent: None,
                    remaining_delegations: 0,
                    issued_under: previous.clone(),
                    not_before_ms: now(),
                    expires_at_ms: now() + 100_000,
                    nonce: Nonce::random()?,
                },
                &self.executor.root,
            )?;
            let leaf = grant.hash()?;
            let mut document = self
                .store
                .policy
                .verified()
                .space(&self.executor.space)
                .unwrap()
                .clone();
            document.previous = Some(previous);
            document.sequence += 1;
            document.grants.push(leaf.clone());
            document.grants.sort();
            self.store.install_policy_journal(
                &[
                    JournalEntry::Grant(grant),
                    JournalEntry::Space(Signed::sign(document, &self.executor.root)?),
                ],
                now() + 20,
            )?;
            Ok(leaf)
        }

        fn authorize_executor_seal(
            &mut self,
            body: &Value,
            leaf: &ObjectHash,
        ) -> Result<(
            VerifiedAuthorization,
            super::super::policy::FreshnessProof,
            TransportEvidence,
        )> {
            let audience = self.wire.handoff.execution_audience.clone();
            let facts = self
                .store
                .remote_execution_seal_facts(body, &audience, now() + 21)?;
            let device = DeviceId::from_key(&PublicKey::of(&self.executor.client));
            let mut heads = [
                PolicyDomain::Account(self.executor.account.clone()),
                PolicyDomain::Space(self.executor.space.clone()),
            ]
            .iter()
            .map(|domain| self.store.policy.verified().head(domain).unwrap().clone())
            .collect::<Vec<_>>();
            heads.sort_by(|left, right| left.domain.cmp(&right.domain));
            let invocation = Signed::sign(
                Invocation {
                    version: AUTH_VERSION,
                    actor: Actor::Account(self.executor.account.clone()),
                    device: device.clone(),
                    initiator: Actor::Account(self.executor.account.clone()),
                    initiating_grant: None,
                    resource: facts.resource.clone(),
                    action: facts.action,
                    audience,
                    presenter: device.clone(),
                    operation_id: facts.operation_id.clone(),
                    body_hash: facts.body_hash.clone(),
                    grant_leaf: leaf.clone(),
                    expected_policies: heads,
                    context: facts.context.clone(),
                    nonce: Nonce::random()?,
                    not_before_ms: now() + 21,
                    expires_at_ms: now() + 10_000,
                },
                &self.executor.client,
            )?;
            let transport = TransportEvidence::DirectNoise { device };
            let auth = self
                .store
                .authorize_request(&invocation, &facts, &transport, now() + 21)?;
            let challenge = self
                .store
                .issue_freshness(&auth, CheckStage::Admit, now() + 21)?;
            let receipts =
                self.store
                    .sign_current_receipts(&challenge, &self.executor.node, now() + 21)?;
            Ok((
                auth,
                super::super::policy::FreshnessProof {
                    challenge,
                    receipts,
                },
                transport,
            ))
        }
    }

    #[tokio::test]
    async fn compact_remote_seal_requires_current_exact_authority_and_persists_one_generation_fence()
    -> Result<()> {
        use super::super::execution_binding_seal::RemoteExecutionSealPreflight;
        let mut fixture = Fixture::new()?;
        fixture.accept()?;
        let start = fixture.freshness(CheckStage::Start, now() + 3)?;
        let execution = fixture.store.execution_handoff_authorization(
            fixture.executor.space.as_str(),
            fixture.project.space.as_str(),
            "original-operation",
            now() + 4,
        )?;
        fixture.store.claim_execution_handoff(
            &fixture.executor.node,
            &execution,
            &start,
            now() + 4,
        )?;
        let project = ResourceRef {
            space: fixture.project.space.clone(),
            resource: fixture.project.resource.clone(),
        };
        let mut session = fixture
            .store
            .execution_handoff_session(
                fixture.executor.space.as_str(),
                &project,
                &fixture.executor.node.node_id()?,
                "same-session",
            )?
            .context("C session is missing")?;
        session.state = "idle".into();
        session.native_session_id = Some("private-native-session".into());
        fixture.store.finish_execution_handoff(
            &fixture.executor.node,
            fixture.executor.space.as_str(),
            fixture.project.space.as_str(),
            "original-operation",
            OperationState::Succeeded,
            json!({"done":true}),
            Some(&session),
            now() + 5,
        )?;
        let body = fixture.remote_seal_body()?;
        assert!(body.get("execution_observation").is_none());
        let broad = fixture.executor.bundle.grant_leaf.clone();
        let (auth, proof, transport) = fixture.authorize_executor_seal(&body, &broad)?;
        let before = fixture.store.connection.total_changes();
        assert!(
            fixture
                .store
                .authorize_remote_execution_seal_fence(
                    &fixture.executor.node,
                    &auth,
                    &proof,
                    &transport,
                    &body,
                    now() + 22
                )
                .is_err()
        );
        assert_eq!(fixture.store.connection.total_changes(), before);
        let leaf = fixture.executor_seal_grant(&body)?;
        let (auth, proof, transport) = fixture.authorize_executor_seal(&body, &leaf)?;
        fixture.store.connection.execute("INSERT INTO execution_handoff_reservations(execution_space_id,original_space_id,operation_id,target_node,resource) VALUES(?,?,?,?,?)",
            params![fixture.executor.space.as_str(),fixture.project.space.as_str(),"original-operation",fixture.executor.node.node_id()?,fixture.executor.project.to_str().unwrap()])?;
        let before = fixture.store.connection.total_changes();
        assert!(
            fixture
                .store
                .authorize_remote_execution_seal_fence(
                    &fixture.executor.node,
                    &auth,
                    &proof,
                    &transport,
                    &body,
                    now() + 22
                )
                .is_err()
        );
        assert_eq!(fixture.store.connection.total_changes(), before);
        fixture.store.connection.execute(
            "DELETE FROM execution_handoff_reservations WHERE operation_id='original-operation'",
            [],
        )?;
        assert!(matches!(
            fixture.store.authorize_remote_execution_seal_fence(
                &fixture.executor.node,
                &auth,
                &proof,
                &transport,
                &body,
                now() + 22
            )?,
            RemoteExecutionSealPreflight::Fence(_)
        ));
        let wrong_stop = fixture.fence("another-operation").await?;
        assert!(
            fixture
                .store
                .seal_remote_execution_authorized(
                    &fixture.executor.node,
                    &auth,
                    &proof,
                    &transport,
                    &body,
                    &wrong_stop,
                    now() + 22
                )
                .is_err()
        );
        let stopped = fixture.fence("original-operation").await?;
        fixture.store.connection.execute_batch("CREATE TRIGGER reject_generation_seal BEFORE INSERT ON execution_binding_seals BEGIN SELECT RAISE(ABORT,'injected seal failure'); END")?;
        let failure = fixture
            .store
            .seal_remote_execution_authorized(
                &fixture.executor.node,
                &auth,
                &proof,
                &transport,
                &body,
                &stopped,
                now() + 22,
            )
            .expect_err("seal persistence must roll back");
        assert!(
            format!("{failure:#}").contains("injected seal failure"),
            "{failure:#}"
        );
        let retained = fixture
            .store
            .execution_handoff_session(
                fixture.executor.space.as_str(),
                &project,
                &fixture.executor.node.node_id()?,
                "same-session",
            )?
            .unwrap();
        assert_eq!(retained.state, "idle");
        assert_eq!(
            retained.native_session_id.as_deref(),
            Some("private-native-session")
        );
        assert_eq!(
            fixture.store.connection.query_row(
                "SELECT COUNT(*) FROM execution_binding_seals",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            0
        );
        fixture
            .store
            .connection
            .execute_batch("DROP TRIGGER reject_generation_seal")?;
        let usage = vibeface_secure::signature_verification::SignatureVerification::new();
        usage.run(|| -> Result<()> {
            let facts = fixture.store.remote_execution_seal_facts(
                &body,
                &fixture.wire.handoff.execution_audience,
                now() + 22,
            )?;
            fixture.store.authorize_request(
                auth.signed_invocation(),
                &facts,
                &transport,
                now() + 22,
            )?;
            assert!(matches!(
                fixture.store.authorize_remote_execution_seal_fence(
                    &fixture.executor.node,
                    &auth,
                    &proof,
                    &transport,
                    &body,
                    now() + 22
                )?,
                RemoteExecutionSealPreflight::Fence(_)
            ));
            Ok(())
        })?;
        let seal = usage.run(|| {
            fixture.store.seal_remote_execution_authorized(
                &fixture.executor.node,
                &auth,
                &proof,
                &transport,
                &body,
                &stopped,
                now() + 22,
            )
        })?;
        println!(
            "REMOTE_SEAL_SIGNATURE_USAGE {}",
            serde_json::to_string(&usage.counts())?
        );
        binding::verify_seal(&seal, &fixture.executor.node.public_key_hex()?)?;
        let verify = |revision: &str, project_key: &str, execution_key: &str| {
            execution_binding_seal::verify_remote_execution_seal(
                &body["binding"].to_string(),
                &serde_json::to_string(&seal).unwrap(),
                project_key,
                execution_key,
                revision,
            )
        };
        let project_key = fixture.project.node.public_key_hex()?;
        let execution_key = fixture.executor.node.public_key_hex()?;
        assert_eq!(
            serde_json::from_str::<binding::SignedSeal>(
                &verify(
                    &seal.seal.final_revision.to_string(),
                    &project_key,
                    &execution_key
                )
                .map_err(anyhow::Error::msg)?
            )?,
            seal
        );
        assert!(
            verify(
                &(seal.seal.final_revision + 1).to_string(),
                &project_key,
                &execution_key
            )
            .is_err()
        );
        assert!(
            verify(
                &seal.seal.final_revision.to_string(),
                &execution_key,
                &execution_key
            )
            .is_err()
        );
        assert!(
            verify(
                &seal.seal.final_revision.to_string(),
                &project_key,
                &project_key
            )
            .is_err()
        );
        assert!(verify("9007199254740992", &project_key, &execution_key).is_err());
        assert!(verify("1.5", &project_key, &execution_key).is_err());
        let closed = fixture
            .store
            .execution_handoff_session(
                fixture.executor.space.as_str(),
                &project,
                &fixture.executor.node.node_id()?,
                "same-session",
            )?
            .unwrap();
        assert_eq!(closed.state, "closed");
        assert_eq!(closed.native_session_id, None);
        let before = fixture.store.connection.total_changes();
        assert!(
            matches!(fixture.store.authorize_remote_execution_seal_fence(&fixture.executor.node,
            &auth,&proof,&transport,&body,now()+23)?,RemoteExecutionSealPreflight::Existing(ref saved) if saved==&seal)
        );
        assert_eq!(fixture.store.connection.total_changes(), before);
        let (replacement, replacement_proof, replacement_transport) =
            fixture.authorize_executor_seal(&body, &leaf)?;
        assert!(
            fixture
                .store
                .authorize_remote_execution_seal_fence(
                    &fixture.executor.node,
                    &replacement,
                    &replacement_proof,
                    &replacement_transport,
                    &body,
                    now() + 23
                )
                .is_err()
        );
        let mut status = body.clone();
        status["method"] = json!("remote_execution_seal_status");
        status["authorization_id"] = json!("read-seal-generation-one");
        status["seal_invocation_hash"] = json!(auth.signed_invocation().hash()?);
        let (reader, reader_proof, reader_transport) =
            fixture.authorize_executor_seal(&status, &leaf)?;
        assert_eq!(
            fixture.store.remote_execution_seal_status_authorized(
                &fixture.executor.node,
                &reader,
                &reader_proof,
                &reader_transport,
                &status,
                now() + 23
            )?,
            Some(seal.clone())
        );
        let mut wrong_status = status.clone();
        wrong_status["seal_invocation_hash"] = json!(replacement.signed_invocation().hash()?);
        let (wrong_reader, wrong_proof, wrong_transport) =
            fixture.authorize_executor_seal(&wrong_status, &leaf)?;
        assert!(
            fixture
                .store
                .remote_execution_seal_status_authorized(
                    &fixture.executor.node,
                    &wrong_reader,
                    &wrong_proof,
                    &wrong_transport,
                    &wrong_status,
                    now() + 23
                )
                .is_err()
        );
        let mut replacement = fixture.wire.clone();
        let mut invocation = replacement.invocation.payload.clone();
        for head in &mut invocation.expected_policies {
            *head = fixture
                .store
                .policy
                .verified()
                .head(&head.domain)
                .unwrap()
                .clone();
        }
        invocation.nonce = Nonce::random()?;
        let signer = Identity::from_secret_hex(&fixture.project.node.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        replacement.invocation = Signed::sign(invocation, &signer)?;
        let failure = fixture
            .store
            .issue_execution_handoff_challenge(
                &replacement,
                &fixture.transport,
                CheckStage::Admit,
                now() + 23,
            )
            .expect_err("sealed generation cannot issue another challenge");
        assert!(
            format!("{failure:#}").contains("generation is sealed"),
            "{failure:#}"
        );
        assert!(
            fixture
                .store
                .renew_execution_handoff(&fixture.executor.node, &execution, &start, now() + 23)
                .is_err()
        );
        fixture.executor.revoke_client(&mut fixture.store)?;
        let before = fixture.store.connection.total_changes();
        assert!(
            fixture
                .store
                .remote_execution_seal_status_authorized(
                    &fixture.executor.node,
                    &reader,
                    &reader_proof,
                    &reader_transport,
                    &status,
                    now() + 23
                )
                .is_err()
        );
        assert_eq!(fixture.store.connection.total_changes(), before);

        let mut uncertain = Fixture::new()?;
        uncertain.start_unknown()?;
        let body = uncertain.remote_seal_body()?;
        let leaf = uncertain.executor_seal_grant(&body)?;
        let (auth, proof, transport) = uncertain.authorize_executor_seal(&body, &leaf)?;
        let before = uncertain.store.connection.total_changes();
        assert!(
            uncertain
                .store
                .authorize_remote_execution_seal_fence(
                    &uncertain.executor.node,
                    &auth,
                    &proof,
                    &transport,
                    &body,
                    now() + 22
                )
                .is_err()
        );
        assert_eq!(uncertain.store.connection.total_changes(), before);
        assert_eq!(
            uncertain.store.connection.query_row(
                "SELECT COUNT(*) FROM execution_binding_seals",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            0
        );
        Ok(())
    }

    #[test]
    fn independent_current_observation_survives_original_revocation_without_operation_writes()
    -> Result<()> {
        for sdk in [false, true] {
            let mut fixture = if sdk {
                Fixture::sdk().context("SDK fixture setup")?
            } else {
                Fixture::new()?
            };
            fixture
                .start_unknown()
                .with_context(|| format!("start Unknown, sdk={sdk}"))?;
            fixture.project.revoke_client(&mut fixture.store)?;
            let mut revoked_grants = vec![(
                &fixture.executor,
                fixture.wire.invocation.payload.grant_leaf.clone(),
            )];
            if sdk {
                let event: Event =
                    serde_json::from_str(&fixture.wire.handoff.project_dispatch.canonical_body)?;
                revoked_grants.push((
                    &fixture.project,
                    capsule_admission(&event)?
                        .invocation
                        .payload
                        .grant_leaf
                        .clone(),
                ));
            }
            for (owner, grant) in revoked_grants {
                let stamp = fixture
                    .store
                    .policy
                    .verified()
                    .head(&PolicyDomain::Space(owner.space.clone()))
                    .unwrap()
                    .clone();
                let mut document = fixture
                    .store
                    .policy
                    .verified()
                    .space(&owner.space)
                    .unwrap()
                    .clone();
                document.previous = Some(stamp);
                document.sequence += 1;
                document.grants.retain(|hash| hash != &grant);
                fixture.store.install_policy_journal(
                    &[JournalEntry::Space(Signed::sign(document, &owner.root)?)],
                    now() + 7,
                )?;
            }
            let (wire, proof) = fixture
                .observation()
                .with_context(|| format!("prepare Observation, sdk={sdk}"))?;
            let changes = fixture.store.connection.total_changes();
            let verification =
                vibeface_secure::signature_verification::SignatureVerification::new();
            let result = verification
                .run(|| {
                    fixture.store.observe_execution_handoff(
                        &fixture.executor.node,
                        &wire,
                        &proof,
                        &fixture.transport,
                        now() + 53,
                    )
                })?
                .context("accepted operation was not observed")?;
            assert_eq!(result.receipt.payload.state, ExecutionHandoffState::Unknown);
            verify_execution_handoff_receipt_signature(
                &fixture.wire,
                &result.receipt,
                &result.result,
            )?;
            assert_eq!(changes, fixture.store.connection.total_changes());
            let mut missing = proof.clone();
            missing.receipts.pop();
            assert!(
                fixture
                    .store
                    .observe_execution_handoff(
                        &fixture.executor.node,
                        &wire,
                        &missing,
                        &fixture.transport,
                        now() + 53,
                    )
                    .is_err()
            );
            let wrong_peer = TransportEvidence::VerifiedPeer {
                origin: DeviceId::from_key(&PublicKey::of(&fixture.executor.client)),
            };
            assert!(
                fixture
                    .store
                    .observe_execution_handoff(
                        &fixture.executor.node,
                        &wire,
                        &proof,
                        &wrong_peer,
                        now() + 53,
                    )
                    .is_err()
            );
            assert!(
                fixture
                    .store
                    .observe_execution_handoff(
                        &fixture.executor.node,
                        &wire,
                        &proof,
                        &fixture.transport,
                        proof.challenge.expires_at_ms,
                    )
                    .is_err()
            );
            let mut unissued = proof.clone();
            unissued.challenge.nonce = Nonce::random()?;
            unissued.receipts = fixture.store.sign_execution_observation_receipts(
                &unissued.challenge,
                &fixture.project.node,
                now() + 52,
            )?;
            unissued
                .receipts
                .extend(fixture.store.sign_execution_observation_receipts(
                    &unissued.challenge,
                    &fixture.executor.node,
                    now() + 52,
                )?);
            assert!(
                fixture
                    .store
                    .observe_execution_handoff(
                        &fixture.executor.node,
                        &wire,
                        &unissued,
                        &fixture.transport,
                        now() + 53,
                    )
                    .is_err()
            );
            assert_eq!(changes, fixture.store.connection.total_changes());
            for mutation in [
                "DELETE FROM execution_handoff_reservations",
                "UPDATE execution_handoff_sessions SET binding=json_set(binding,'$.binding.generation',2)",
                "UPDATE execution_handoffs SET result='{\"native_session_id\":\"private-provider-id\"}'",
                "DELETE FROM execution_handoff_nonces",
                "UPDATE local_values SET value=json_set(value,'$.stage','admit') WHERE key GLOB 'execution-observation:*'",
                "UPDATE local_values SET value=json_set(value,'$.payload.deadline_ms',0) WHERE key GLOB 'execution-handoff-lease:*'",
            ] {
                fixture
                    .store
                    .connection
                    .execute_batch("SAVEPOINT observation_tamper")?;
                fixture.store.connection.execute(mutation, [])?;
                let tampered_changes = fixture.store.connection.total_changes();
                assert!(
                    fixture
                        .store
                        .observe_execution_handoff(
                            &fixture.executor.node,
                            &wire,
                            &proof,
                            &fixture.transport,
                            now() + 53,
                        )
                        .is_err(),
                    "{mutation}"
                );
                assert_eq!(tampered_changes, fixture.store.connection.total_changes());
                fixture
                    .store
                    .connection
                    .execute_batch("ROLLBACK TO observation_tamper; RELEASE observation_tamper")?;
            }
            println!(
                "OBSERVATION_STORE_USAGE {}",
                json!({"case": if sdk {"sdk_actor_revoked"} else {"account_actor_revoked"}, "actual":verification.counts()})
            );
            fixture.executor.revoke_client(&mut fixture.store)?;
            let changes = fixture.store.connection.total_changes();
            assert!(
                fixture
                    .store
                    .observe_execution_handoff(
                        &fixture.executor.node,
                        &wire,
                        &proof,
                        &fixture.transport,
                        now() + 54,
                    )
                    .is_err()
            );
            assert_eq!(changes, fixture.store.connection.total_changes());
        }
        Ok(())
    }

    #[test]
    fn observation_requires_local_checkpoint_and_current_freshness_even_when_absent() -> Result<()>
    {
        let mut fixture = Fixture::new()?;
        let (wire, proof) = fixture.observation()?;
        let changes = fixture.store.connection.total_changes();
        assert!(
            fixture
                .store
                .observe_execution_handoff(
                    &fixture.executor.node,
                    &wire,
                    &proof,
                    &fixture.transport,
                    now() + 53,
                )?
                .is_none()
        );
        let mut missing = proof.clone();
        missing.receipts.pop();
        assert!(
            fixture
                .store
                .observe_execution_handoff(
                    &fixture.executor.node,
                    &wire,
                    &missing,
                    &fixture.transport,
                    now() + 53,
                )
                .is_err()
        );
        assert_eq!(changes, fixture.store.connection.total_changes());
        Ok(())
    }

    #[test]
    fn production_admission_rejects_oversized_metadata_and_excess_historical_receipts_without_writes()
    -> Result<()> {
        let mut fixture = Fixture::new()?;
        let signer = Identity::from_secret_hex(&fixture.project.node.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        for oversized_resources in [false, true] {
            let mut wire = fixture.wire.clone();
            let mut event: Event =
                serde_json::from_str(&wire.handoff.project_dispatch.canonical_body)?;
            let EventCause::Invocation { admission } = &mut event.cause else {
                unreachable!()
            };
            if oversized_resources {
                let root = fixture.executor.project.to_str().unwrap();
                let mut resources = vec![root.to_owned()];
                resources.extend(
                    (1..128).map(|index| format!("{root}/resource-{index}-{}", "x".repeat(320))),
                );
                admission.source_body["request"]["resources"] = json!(resources);
                let mut invocation = admission.invocation.payload.clone();
                invocation.body_hash = ObjectHash::body(&admission.source_body)?;
                invocation.nonce = Nonce::random()?;
                admission.invocation = Signed::sign(invocation.clone(), &fixture.project.client)?;
                let facts = RequestFacts {
                    resource: invocation.resource,
                    action: invocation.action,
                    audience: invocation.audience,
                    operation_id: invocation.operation_id,
                    body_hash: invocation.body_hash,
                    context: invocation.context,
                };
                let auth = super::super::policy::authorize_invocation_for_body(
                    fixture.store.policy.verified(),
                    &admission.invocation,
                    &facts,
                    &admission.transport.existing_evidence()?,
                    &admission.source_body,
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
                let EventBody::DispatchRecorded { action, .. } = &mut event.body else {
                    unreachable!()
                };
                *action = admission.source_body["request"].clone();
            } else {
                let mut receipt = admission.receipts[0].payload.clone();
                for index in 0..=MAX_POLICY_DOMAINS {
                    receipt.issued_at_ms = now() + index as u64;
                    admission
                        .receipts
                        .push(Signed::sign(receipt.clone(), &signer)?);
                }
            }
            wire.handoff.project_dispatch = fixture.project.node.sign_event(event)?;
            wire.handoff.attest_source(&signer)?;
            let mut invocation = wire.invocation.payload.clone();
            invocation.body_hash = ObjectHash::body(&wire.handoff)?;
            wire.invocation = Signed::sign(invocation, &signer)?;
            wire.invocation.verify_signature()?;
            wire.handoff
                .source_attestation
                .as_ref()
                .unwrap()
                .verify_signature()?;
            let before = fixture.store.connection.total_changes();
            let error = fixture
                .store
                .issue_execution_handoff_challenge(
                    &wire,
                    &fixture.transport,
                    CheckStage::Admit,
                    now(),
                )
                .unwrap_err();
            if oversized_resources {
                assert!(
                    matches!(
                        error.downcast_ref::<AuthorizationError>(),
                        Some(AuthorizationError::BudgetExceeded)
                    ),
                    "{error:#}"
                );
            } else {
                assert!(
                    error
                        .to_string()
                        .contains("too many historical admission items"),
                    "{error:#}"
                );
            }
            assert_eq!(before, fixture.store.connection.total_changes());
        }
        Ok(())
    }

    #[test]
    fn production_admission_rejects_union_of_individually_legal_depth_eight_grants_without_writes()
    -> Result<()> {
        for intermediary in [false, true] {
            let mut fixture = Fixture::new()?;
            let intermediaries = if intermediary {
                let networks = vec![
                    TestNetwork::new()?,
                    TestNetwork::new()?,
                    TestNetwork::new()?,
                ];
                for network in &networks {
                    network.install(&mut fixture.store)?;
                }
                networks
            } else {
                Vec::new()
            };
            let project_template = fixture
                .store
                .policy
                .verified()
                .grant(&fixture.project.bundle.grant_leaf)
                .unwrap()
                .payload
                .clone();
            let execution_template = fixture
                .store
                .policy
                .verified()
                .grant(&fixture.wire.invocation.payload.grant_leaf)
                .unwrap()
                .payload
                .clone();
            let project_leaf = fixture.deep_grant(
                true,
                project_template,
                Actor::Account(fixture.project.account.clone()),
                &[],
            )?;
            let execution_leaf = fixture.deep_grant(
                false,
                execution_template,
                fixture.wire.invocation.payload.actor.clone(),
                &intermediaries,
            )?;
            let mut event: Event =
                serde_json::from_str(&fixture.wire.handoff.project_dispatch.canonical_body)?;
            let EventCause::Invocation { admission } = &mut event.cause else {
                unreachable!()
            };
            admission.source_body["execution_grant"] = json!(execution_leaf);
            let mut invocation = admission.invocation.payload.clone();
            invocation.grant_leaf = project_leaf.clone();
            invocation.body_hash = ObjectHash::body(&admission.source_body)?;
            invocation.expected_policies.clear();
            invocation.nonce = Nonce::random()?;
            let facts = RequestFacts {
                resource: invocation.resource.clone(),
                action: invocation.action,
                audience: invocation.audience.clone(),
                operation_id: invocation.operation_id.clone(),
                body_hash: invocation.body_hash.clone(),
                context: invocation.context.clone(),
            };
            let transport = admission.transport.existing_evidence()?;
            admission.invocation = Signed::sign(invocation.clone(), &fixture.project.client)?;
            let policy = fixture.store.policy.verified();
            let auth =
                match policy.verify_invocation(&admission.invocation, &facts, &transport, now()) {
                    Err(AuthorizationError::NeedPolicySync(expected)) => {
                        invocation.expected_policies = expected;
                        admission.invocation = Signed::sign(invocation, &fixture.project.client)?;
                        policy.verify_invocation(
                            &admission.invocation,
                            &facts,
                            &transport,
                            now(),
                        )?
                    }
                    result => result?,
                };
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
            let fresh =
                policy.verify_freshness(&auth, &admission.challenge, &admission.receipts, now())?;
            let admitted = policy.admit_operation(&auth, &fresh, now())?;
            let original =
                policy.verify_operation_authorization(&admitted, &facts, &transport, now())?;
            let mut original_usage = ExecutionProofUsage::default();
            original_usage.record_authorization(&original)?;
            original_usage.enforce_limits()?;
            assert_eq!(
                original_usage.counts().max_chain_depth,
                MAX_DELEGATION_DEPTH
            );
            fixture.wire.handoff.project_dispatch = fixture.project.node.sign_event(event)?;
            let signer = Identity::from_secret_hex(&fixture.project.node.secret_key_hex)
                .map_err(anyhow::Error::msg)?;
            fixture.wire.handoff.attest_source(&signer)?;
            let facts = execution_submission_facts(&fixture.wire.handoff, &original)?;
            let Actor::Service(service) = &fixture.wire.invocation.payload.actor else {
                unreachable!()
            };
            let descriptor =
                policy.verify_service_grant(service, &execution_leaf, &facts, now())?;
            let mut invocation = fixture.wire.invocation.payload.clone();
            invocation.grant_leaf = execution_leaf;
            invocation.body_hash = facts.body_hash.clone();
            invocation.expected_policies = descriptor.heads().to_vec();
            invocation.initiator = descriptor.initiator().clone();
            invocation.initiating_grant = descriptor.initiating_grant().cloned();
            invocation.nonce = Nonce::random()?;
            fixture.wire.invocation = Signed::sign(invocation, &signer)?;
            let submission = policy.verify_invocation(
                &fixture.wire.invocation,
                &facts,
                &fixture.transport,
                now(),
            )?;
            let mut submission_usage = ExecutionProofUsage::default();
            submission_usage.record_authorization(&submission)?;
            submission_usage.enforce_limits()?;
            assert_eq!(
                submission_usage.counts().max_chain_depth,
                MAX_DELEGATION_DEPTH
            );
            let mut union_usage = original_usage.clone();
            union_usage.merge(&submission_usage);
            union_usage.record_handoff(&fixture.wire)?;
            let before = fixture.store.connection.total_changes();
            let verification =
                vibeface_secure::signature_verification::SignatureVerification::new();
            let error = verification
                .run(|| {
                    fixture.store.issue_execution_handoff_challenge(
                        &fixture.wire,
                        &fixture.transport,
                        CheckStage::Admit,
                        now(),
                    )
                })
                .unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<AuthorizationError>(),
                    Some(AuthorizationError::BudgetExceeded)
                ),
                "{error:#}"
            );
            assert_eq!(before, fixture.store.connection.total_changes());
            println!(
                "HANDOFF_OVERBUDGET_USAGE {}",
                json!({"original":original_usage.counts(),"submission":submission_usage.counts(),"union":union_usage.counts(),"actual":verification.counts()})
            );
            if !intermediaries.is_empty() {
                continue;
            }
            let mut ninth = fixture
                .store
                .policy
                .verified()
                .grant(&project_leaf)
                .unwrap()
                .payload
                .clone();
            ninth.parent = Some(project_leaf);
            ninth.remaining_delegations = 0;
            ninth.issued_under = fixture
                .store
                .policy
                .verified()
                .head(&PolicyDomain::Space(fixture.project.space.clone()))
                .unwrap()
                .clone();
            ninth.nonce = Nonce::random()?;
            let ninth = Signed::sign(ninth, &fixture.project.client)?;
            let mut document = fixture
                .store
                .policy
                .verified()
                .space(&fixture.project.space)
                .unwrap()
                .clone();
            document.previous = Some(
                fixture
                    .store
                    .policy
                    .verified()
                    .head(&PolicyDomain::Space(fixture.project.space.clone()))
                    .unwrap()
                    .clone(),
            );
            document.sequence += 1;
            document.grants.push(ninth.hash()?);
            document.grants.sort();
            let before = fixture.store.connection.total_changes();
            let error = fixture
                .store
                .install_policy_journal(
                    &[
                        JournalEntry::Grant(ninth),
                        JournalEntry::Space(Signed::sign(document, &fixture.project.root)?),
                    ],
                    now(),
                )
                .unwrap_err();
            assert!(error.to_string().contains("depth"), "{error:#}");
            assert_eq!(before, fixture.store.connection.total_changes());
        }
        Ok(())
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
        let resolver = fixture
            .store
            .policy
            .verified()
            .verify_execution_resolution_account_checkpoint(
                &wire,
                &fixture.transport,
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
            fixture.resign_resolution_account(&mut altered, field != "execution_grant")?;
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
        fixture.resign_resolution_account(&mut other, true)?;
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
    fn protective_unknown_requires_a_signed_executor_receipt_without_fabricating_a_lease()
    -> Result<()> {
        let mut fixture = Fixture::new()?;
        fixture.accept()?;
        let (execution, original, operation) = scope(&fixture.wire)?;
        let mut accepted =
            stored_from(&fixture.store.connection, &execution, &original, &operation)?
                .context("fixture has no original acceptance")?;
        assert_eq!(accepted.state, OperationState::Accepted);
        assert!(accepted.lease.is_none());
        let reservations: u64 = fixture.store.connection.query_row(
            "SELECT count(*) FROM execution_handoff_reservations",
            [],
            |row| row.get(0),
        )?;
        assert!(reservations > 0);
        accepted.state = OperationState::Unknown;
        accepted.result = json!({"reason":"stopped before Start"});
        fixture.store.connection.execute(
            "UPDATE execution_handoffs SET state='unknown',result=?,result_receipt=NULL",
            [serde_json::to_string(&accepted.result)?],
        )?;
        assert!(
            fixture
                .store
                .execution_handoff_status(
                    &fixture.executor.node,
                    &execution,
                    &original,
                    &operation,
                    now() + 2
                )
                .is_err()
        );
        let receipt = signed_receipt(
            &fixture.store.connection,
            &fixture.executor.node,
            &accepted,
            now() + 2,
        )?;
        fixture.store.connection.execute(
            "UPDATE execution_handoffs SET result_receipt=?",
            [serde_json::to_string(&receipt)?],
        )?;
        let status = fixture.store.execution_handoff_status(
            &fixture.executor.node,
            &execution,
            &original,
            &operation,
            now() + 2,
        )?;
        assert_eq!(status.state, OperationState::Unknown);
        assert_eq!(status.execution_context.lease_expires_at_ms, None);
        let lease_rows: u64 = fixture.store.connection.query_row(
            "SELECT count(*) FROM execution_handoffs WHERE lease_challenge IS NOT NULL OR lease_deadline_ms IS NOT NULL",
            [], |row| row.get(0))?;
        assert_eq!(lease_rows, 0);
        let held: u64 = fixture.store.connection.query_row(
            "SELECT count(*) FROM execution_handoff_reservations",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(held, reservations);
        validate_execution_handoff_storage(&fixture.store.connection, &fixture.store.policy)?;
        let mut changed = receipt.clone();
        changed.payload.handoff_hash = ObjectHash::body(&"another accepted operation")?;
        fixture.store.connection.execute(
            "UPDATE execution_handoffs SET result_receipt=?",
            [serde_json::to_string(&changed)?],
        )?;
        assert!(
            fixture
                .store
                .execution_handoff_status(
                    &fixture.executor.node,
                    &execution,
                    &original,
                    &operation,
                    now() + 2
                )
                .is_err()
        );
        fixture.store.connection.execute(
            "UPDATE execution_handoffs SET state='running',result_receipt=?",
            [serde_json::to_string(&receipt)?],
        )?;
        assert!(
            fixture
                .store
                .execution_handoff_status(
                    &fixture.executor.node,
                    &execution,
                    &original,
                    &operation,
                    now() + 2
                )
                .is_err()
        );
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
        fixture.print_handoff_proof_counts("sdk_capsule")?;
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
        altered.handoff.origin = ExecutionOrigin::TaskSession {
            task_creation: fixture.store.task_creation(
                fixture.project.space.as_str(),
                "conversation",
                "other-task-secret-DO-NOT-DISCLOSE",
            )?,
        };
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
        let ExecutionOrigin::TaskSession { task_creation } = &mut altered.handoff.origin else {
            anyhow::bail!("fixture has no Task origin")
        };
        let mut task: Event = serde_json::from_str(&task_creation.canonical_body)?;
        if let EventBody::TaskCreated { goal, .. } = &mut task.body {
            *goal = "changed task input".into();
        }
        *task_creation = fixture.project.node.sign_event(task)?;
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
        let receipt = &mut historical.handoff.sdk_origin.as_mut().unwrap().receipts[0];
        receipt.payload.expires_at_ms -= 1;
        *receipt = Signed::sign(receipt.payload.clone(), &signer)?;
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
        let auth = super::super::policy::authorize_invocation_for_body(
            fixture.store.policy.verified(),
            &admission.invocation,
            &facts,
            &admission.transport.existing_evidence()?,
            &admission.source_body,
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
        fixture.retain_source_dispatch()?;
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
            !receipt.payload.currents.iter().any(|current| {
                current.domain == PolicyDomain::Space(fixture.executor.space.clone())
            })
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
        fixture.store.persist_execution_handoff_native(
            fixture.executor.space.as_str(),
            &fixture.wire.handoff.source_statement()?.project,
            "original-operation",
            "provider-keeps-original-id",
            now() + 5,
        )?;
        let result = json!({"operation_id":"original-operation","native_session_id":"provider-keeps-original-id","message":"process provider-keeps-original-id completed","events":[{"threadId":"provider-keeps-original-id","text":"done"}]});
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
        assert!(
            verify_execution_handoff_receipt_signature(
                &fixture.wire,
                completed.receipt.as_ref().unwrap(),
                &result
            )
            .is_err()
        );
        let result = completed.result.clone();
        assert_eq!(
            result,
            json!({"operation_id":"original-operation","message":"process [private session] completed","events":[{"text":"done"}]})
        );
        assert!(!serde_json::to_string(&result)?.contains("provider-keeps-original-id"));
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
        let mut events = load_events_from(
            &fixture.store.connection,
            &fixture.store.policy,
            fixture.project.space.as_str(),
        )?;
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
                session_id: Some("same-session".into()),
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
