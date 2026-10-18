use super::{
    event_authorization::append_admitted_dispatch_in,
    journal::PolicyState,
    policy::{FreshnessProof, RequestTransport, request_facts_from},
    task_execution_context::{OriginalAdmissionView, TaskScopedAuthorization},
};
use crate::{
    crypto::LocalIdentity,
    engine::{EngineAction, ExecutorKind},
    model::{
        AdmissionProof, AgentSession, EventBody, EventCause, ExecutionSession, MAX_EVENT_BYTES,
        OperationRecord, OperationRequest, OperationState, StoredTransport,
    },
    scheduler,
    scheduling::{JobState, ManagerAttempt, SchedulerJob, scheduler_key},
    store::{
        Store, canonical_resource, closure_hashes, has_blob_from, heads_from_events,
        load_events_from, paths_overlap, validate_operation_binding_from,
    },
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::BTreeMap;
use vibeface_secure::authorization::{
    Action, Actor, CheckStage, InvocationContext, ObjectHash, PolicyDomain, RequestFacts,
    ResourceId, ResourceRef, SessionRef, SpaceId, TransportEvidence, VerifiedAdmission,
    VerifiedAuthorization, VerifiedPolicySet,
};

#[derive(Clone, Default)]
pub(crate) struct AdmissionCheckpoints {
    entries: BTreeMap<(String, String), LocalAdmissionCheckpoint>,
}

#[derive(Clone)]
pub(crate) struct LocalAdmissionCheckpoint {
    proof: AdmissionProof,
    facts: RequestFacts,
    facts_hash: ObjectHash,
    operation_id: Option<String>,
    anchors: Vec<crate::model::SignedEvent>,
    admitted: VerifiedAdmission,
    admission_expires_at_ms: u64,
    proof_usage: vibeface_secure::authorization::execution_handoff::ExecutionProofUsage,
}

fn admission_facts_hash(facts: &RequestFacts) -> Result<ObjectHash> {
    Ok(ObjectHash::body(&(
        &facts.resource,
        facts.action,
        &facts.audience,
        &facts.operation_id,
        &facts.body_hash,
        &facts.context,
    ))?)
}

fn admission_operation_index_in(
    connection: &Connection,
    proof: &AdmissionProof,
) -> Result<Option<String>> {
    let invocation = &proof.invocation.payload;
    let operation: Option<String> = connection.query_row(
        "SELECT operation_id FROM accepted_authorizations WHERE space_id=? AND authorization_id=?",
        params![invocation.resource.space.as_str(), invocation.operation_id],
        |row| row.get(0),
    )?;
    if let Some(operation) = &operation {
        let encoded: String = connection.query_row(
            "SELECT request FROM operations WHERE space_id=? AND operation_id=?",
            params![invocation.resource.space.as_str(), operation],
            |row| row.get(0),
        )?;
        let request: OperationRequest = serde_json::from_str(&encoded)?;
        ensure!(
            proof.source_body["method"] == "operate"
                && request.operation_id == *operation
                && request.network_id == invocation.resource.space.as_str()
                && serde_json::from_value::<OperationRequest>(
                    proof.source_body["request"].clone()
                )? == request,
            "cached admission operation differs from its original request"
        );
    } else {
        ensure!(
            proof.source_body["method"] != "operate",
            "cached operation has no admission index"
        );
    }
    Ok(operation)
}

fn admission_anchors_in(
    connection: &Connection,
    proof: &AdmissionProof,
) -> Result<Vec<crate::model::SignedEvent>> {
    let anchors = admission_anchor_envelopes_in(connection, proof)?;
    for envelope in &anchors {
        super::event_authorization::validate_retained_event_dependencies_in(connection, envelope)?;
    }
    Ok(anchors)
}

fn admission_anchor_envelopes_in(
    connection: &Connection,
    proof: &AdmissionProof,
) -> Result<Vec<crate::model::SignedEvent>> {
    let invocation = &proof.invocation.payload;
    let method = manager_field(&proof.source_body, "method")?;
    if matches!(
        method,
        "bind_execution" | "seal_execution" | "rebind_execution"
    ) {
        return Ok(Vec::new());
    }
    let encoded = if method == "agent_start" {
        vec![connection.query_row(
            "SELECT envelope FROM events WHERE space_id=? AND hash=?",
            params![
                invocation.resource.space.as_str(),
                manager_field(&proof.source_body, "trigger")?
            ],
            |row| row.get::<_, String>(0),
        )?]
    } else {
        let scope = if method == "operate" {
            manager_field(&proof.source_body["request"], "conversation_id")?
        } else {
            manager_field(&proof.source_body, "conversation_id")?
        };
        connection.prepare(
            "SELECT envelope FROM events WHERE space_id=? AND resource_id=? AND scope_id=? AND json_extract(json_extract(envelope,'$.canonical_body'),'$.cause.kind')='invocation' AND json_extract(json_extract(envelope,'$.canonical_body'),'$.cause.admission.invocation.payload.operation_id')=? ORDER BY hash",
        )?.query_map(params![invocation.resource.space.as_str(), invocation.resource.resource.as_str(), scope, invocation.operation_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    ensure!(
        !encoded.is_empty(),
        "accepted admission has no retained ready checkpoint"
    );
    encoded.into_iter().map(|encoded| {
        let envelope: crate::model::SignedEvent = serde_json::from_str(&encoded)?;
        let event: crate::model::Event = serde_json::from_str(&envelope.canonical_body)?;
        ensure!(event.network_id == invocation.resource.space.as_str()
            && event.resource_id == invocation.resource.resource,
            "admission checkpoint belongs to another resource");
        if method != "agent_start" {
            ensure!(matches!(&event.cause, crate::model::EventCause::Invocation { admission } if admission == proof),
                "admission checkpoint changed its original proof");
        }
        Ok(envelope)
    }).collect()
}

impl AdmissionCheckpoints {
    pub(crate) fn insert(&mut self, checkpoint: LocalAdmissionCheckpoint) {
        let invocation = &checkpoint.proof.invocation.payload;
        self.entries.insert(
            (
                invocation.resource.space.to_string(),
                invocation.operation_id.clone(),
            ),
            checkpoint,
        );
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    fn admitted_in(
        &self,
        connection: &Connection,
        policy: &PolicyState,
        proof: &AdmissionProof,
        facts: &RequestFacts,
        allow_nonready_history: bool,
    ) -> Result<Option<(VerifiedAdmission, u64)>> {
        let invocation = &proof.invocation.payload;
        let Some(checkpoint) = self.entries.get(&(
            invocation.resource.space.to_string(),
            invocation.operation_id.clone(),
        )) else {
            return Ok(None);
        };
        let snapshot = if connection.is_autocommit() {
            Some(connection.unchecked_transaction()?)
        } else {
            None
        };
        let key = super::event_authorization::audience_key(policy.verified(), proof)?;
        let _original = super::foreign_execution_policy::original_task_execution_policy_in(
            connection, policy, &key, proof,
        )?;
        validate_persisted_admission(connection, proof)?;
        ensure!(
            checkpoint.proof == *proof
                && checkpoint.facts == *facts
                && checkpoint.facts_hash == admission_facts_hash(facts)?
                && checkpoint.operation_id == admission_operation_index_in(connection, proof)?
                && checkpoint.admitted.signed_invocation() == &proof.invocation
                && checkpoint.admitted.admitted_at_ms() == proof.admitted_at_ms,
            "retained admission differs from its verified local checkpoint"
        );
        let anchors = if allow_nonready_history {
            admission_anchor_envelopes_in(connection, proof)?
        } else {
            admission_anchors_in(connection, proof)?
        };
        ensure!(
            anchors == checkpoint.anchors,
            "retained admission changed its ready canonical checkpoint"
        );
        if allow_nonready_history {
            for anchor in &anchors {
                let event: crate::model::Event = serde_json::from_str(&anchor.canonical_body)?;
                let dependencies = super::event_authorization::historical_event_dependencies_in(
                    connection,
                    policy,
                    &event.network_id,
                    &event.resource_id,
                    &event.scope_id,
                    std::slice::from_ref(&anchor.hash),
                )?;
                if dependencies.values().any(|stored| stored.status != "ready") {
                    return Ok(None);
                }
            }
        }
        checkpoint.proof_usage.enforce_limits()?;
        if let Some(snapshot) = snapshot {
            snapshot.commit()?;
        }
        Ok(Some((
            checkpoint.admitted.clone(),
            checkpoint.admission_expires_at_ms,
        )))
    }
}

fn checkpoint_from_verified_in(
    connection: &Connection,
    policy: &PolicyState,
    proof: &AdmissionProof,
    facts: RequestFacts,
    admitted: VerifiedAdmission,
    allow_nonready_history: bool,
) -> Result<Option<LocalAdmissionCheckpoint>> {
    validate_persisted_admission(connection, proof)?;
    let operation_id = admission_operation_index_in(connection, proof)?;
    let anchors = admission_anchor_envelopes_in(connection, proof)?;
    if anchors.is_empty() {
        return Ok(None);
    }
    let mut all_ready = true;
    for envelope in &anchors {
        let event: crate::model::Event = serde_json::from_str(&envelope.canonical_body)?;
        ensure!(
            super::event_authorization::verify_event_envelope(
                connection,
                policy,
                &event.network_id,
                envelope
            )? == event,
            "local admission checkpoint has an invalid event signature"
        );
        if allow_nonready_history {
            let dependencies = super::event_authorization::historical_event_dependencies_in(
                connection,
                policy,
                &event.network_id,
                &event.resource_id,
                &event.scope_id,
                std::slice::from_ref(&envelope.hash),
            )?;
            all_ready &= dependencies.values().all(|stored| stored.status == "ready");
        } else {
            super::event_authorization::validate_retained_event_dependencies_in(
                connection, envelope,
            )?;
        }
    }
    if !all_ready {
        return Ok(None);
    }
    let proof_usage = admitted.proof_usage().clone();
    proof_usage.enforce_limits()?;
    let admission_expires_at_ms = admitted.admission_expires_at_ms();
    Ok(Some(LocalAdmissionCheckpoint {
        proof: proof.clone(),
        facts_hash: admission_facts_hash(&facts)?,
        facts,
        operation_id,
        anchors,
        admitted,
        admission_expires_at_ms,
        proof_usage,
    }))
}

pub(crate) fn prepare_local_admission_checkpoint_in(
    connection: &Connection,
    policy: &PolicyState,
    proof: &AdmissionProof,
) -> Result<LocalAdmissionCheckpoint> {
    let facts = persisted_admission_facts(connection, policy, proof)?;
    let admitted = reconstruct_stored_admission_uncached(connection, policy, proof, &facts)?;
    checkpoint_from_verified_in(connection, policy, proof, facts, admitted, false)?
        .context("local admission has no retained event checkpoint")
}

pub(crate) fn check_policy_heads_in(
    connection: &Connection,
    state: &VerifiedPolicySet,
    auth: &VerifiedAuthorization,
    now_ms: u64,
) -> Result<()> {
    state.check_current(auth, now_ms)?;
    for expected in auth.dependencies() {
        let (kind, id) = match &expected.domain {
            PolicyDomain::Account(id) => ("account", id.as_str()),
            PolicyDomain::Space(id) => ("space", id.as_str()),
        };
        let actual: Option<(u64, u64, String)> = connection.query_row(
            "SELECT authority_epoch,sequence,head_hash FROM current_policy_heads WHERE domain_kind=? AND domain_id=?",
            params![kind, id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        ensure!(
            actual
                == Some((
                    expected.authority_epoch,
                    expected.sequence,
                    expected.head.to_string()
                )),
            "authorization policy changed before commit"
        );
    }
    Ok(())
}

fn consume_challenge_in(
    tx: &Transaction<'_>,
    state: &VerifiedPolicySet,
    auth: &VerifiedAuthorization,
    proof: &FreshnessProof,
    stage: CheckStage,
    now_ms: u64,
) -> Result<u64> {
    ensure!(
        proof.challenge.stage == stage,
        "incorrect authorization freshness stage"
    );
    check_policy_heads_in(tx, state, auth, now_ms)?;
    let freshness = state.verify_freshness(auth, &proof.challenge, &proof.receipts, now_ms)?;
    consume_challenge_record_in(tx, auth, proof)?;
    Ok(freshness.expires_at_ms().min(auth.expires_at_ms()))
}

fn consume_challenge_record_in(
    tx: &Transaction<'_>,
    auth: &VerifiedAuthorization,
    proof: &FreshnessProof,
) -> Result<()> {
    check_challenge_record(tx, auth, proof, false)?;
    ensure!(tx.execute(
        "UPDATE authorization_challenges SET consumed=1 WHERE space_id=? AND nonce=? AND consumed=0",
        params![auth.resource().space.as_str(), proof.challenge.nonce.as_str()],
    )? == 1, "freshness challenge was already consumed");
    Ok(())
}

fn check_challenge_record(
    connection: &Connection,
    auth: &VerifiedAuthorization,
    proof: &FreshnessProof,
    consumed: bool,
) -> Result<()> {
    let stored: (String, String, String, u64, bool) = connection.query_row(
        "SELECT invocation_hash,stage,challenge,expires_at_ms,consumed FROM authorization_challenges WHERE space_id=? AND nonce=?",
        params![auth.resource().space.as_str(), proof.challenge.nonce.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    ).context("freshness challenge was not issued by this node")?;
    ensure!(
        stored.0 == auth.invocation_hash().as_str()
            && stored.1
                == serde_json::to_value(proof.challenge.stage)?
                    .as_str()
                    .context("invalid freshness stage")?
            && serde_json::from_str::<vibeface_secure::authorization::FreshnessChallenge>(
                &stored.2
            )? == proof.challenge
            && stored.3 == proof.challenge.expires_at_ms
            && stored.4 == consumed,
        "freshness challenge is consumed or does not match persisted proof"
    );
    Ok(())
}

fn source_matches(auth: &VerifiedAuthorization, source_body: &Value) -> Result<()> {
    ensure!(
        ObjectHash::body(source_body)? == *auth.body_hash(),
        "authorization body changed"
    );
    ensure!(
        source_body["authorization_id"].as_str() == Some(auth.operation_id()),
        "authorization ID does not match signed request"
    );
    ensure!(
        source_body["space_id"].as_str() == Some(auth.resource().space.as_str()),
        "authorization space does not match signed request"
    );
    ensure!(
        serde_json::to_vec(source_body)?.len() <= MAX_EVENT_BYTES,
        "authorized request is too large"
    );
    Ok(())
}

pub(crate) fn consume_authorization_in(
    tx: &Transaction<'_>,
    policy: &PolicyState,
    auth: &VerifiedAuthorization,
    proof: &FreshnessProof,
    transport: &TransportEvidence,
    source_body: &Value,
    operation_id: Option<&str>,
    now_ms: u64,
) -> Result<AdmissionProof> {
    consume_request_authorization_in(
        tx,
        policy,
        auth,
        proof,
        &RequestTransport::Existing(transport.clone()),
        source_body,
        operation_id,
        now_ms,
    )
}

pub(crate) fn consume_request_authorization_in(
    tx: &Transaction<'_>,
    policy: &PolicyState,
    auth: &VerifiedAuthorization,
    proof: &FreshnessProof,
    transport: &RequestTransport,
    source_body: &Value,
    operation_id: Option<&str>,
    now_ms: u64,
) -> Result<AdmissionProof> {
    let facts = request_facts_from(tx, policy.verified(), source_body, auth.audience())?;
    consume_authorization_with_facts_in(
        tx,
        policy,
        auth,
        proof,
        transport,
        source_body,
        operation_id,
        now_ms,
        &facts,
        None,
    )
}

pub(crate) fn consume_execution_dispatch_in(
    tx: &Transaction<'_>,
    policy: &PolicyState,
    scoped: &TaskScopedAuthorization,
    proof: &FreshnessProof,
    transport: &TransportEvidence,
    source_body: &Value,
    operation_id: Option<&str>,
    now_ms: u64,
    dispatch: &super::execution_dispatch::VerifiedDispatchFacts,
) -> Result<AdmissionProof> {
    consume_execution_request_dispatch_in(
        tx,
        policy,
        scoped,
        proof,
        &RequestTransport::Existing(transport.clone()),
        source_body,
        operation_id,
        now_ms,
        dispatch,
    )
}

pub(crate) fn consume_execution_request_dispatch_in(
    tx: &Transaction<'_>,
    policy: &PolicyState,
    scoped: &TaskScopedAuthorization,
    proof: &FreshnessProof,
    transport: &RequestTransport,
    source_body: &Value,
    operation_id: Option<&str>,
    now_ms: u64,
    dispatch: &super::execution_dispatch::VerifiedDispatchFacts,
) -> Result<AdmissionProof> {
    let auth = scoped.authorization();
    let facts = dispatch.revalidate(tx, policy, scoped, source_body, auth.audience(), now_ms)?;
    consume_authorization_with_facts_in(
        tx,
        policy,
        auth,
        proof,
        transport,
        source_body,
        operation_id,
        now_ms,
        &facts,
        Some(scoped),
    )
}

fn consume_authorization_with_facts_in(
    tx: &Transaction<'_>,
    policy: &PolicyState,
    auth: &VerifiedAuthorization,
    proof: &FreshnessProof,
    transport: &RequestTransport,
    source_body: &Value,
    operation_id: Option<&str>,
    now_ms: u64,
    facts: &RequestFacts,
    scoped: Option<&TaskScopedAuthorization>,
) -> Result<AdmissionProof> {
    let state = scoped.map_or(policy.verified(), |scope| scope.state(policy.verified()));
    source_matches(auth, source_body)?;
    if source_body["method"] != "agent_start" {
        check_request_manager_tool_in(tx, policy, auth, transport, now_ms)?;
        if let Some(TransportEvidence::LocalAgentAdapter { session, .. }) = transport.existing()
            && auth.action() == Action::AgentStart
        {
            let space = auth.resource().space.as_str();
            let manager_id = active_manager_id(tx, space, &session.session_id)?;
            if now_ms >= manager_lease_from(tx, policy, space, &manager_id)? {
                let (claim, _, job) = load_manager_from(tx, space, &manager_id)?;
                let known: crate::scheduling::SchedulerKnownResult = crate::store::scheduler_load(
                    tx,
                    space,
                    &scheduler_key(
                        "known-result",
                        &claim.conversation_id,
                        &claim.target_node,
                        &claim.trigger,
                    ),
                )?
                .context("expired manager has no retained terminal result")?;
                let mut known_job = known.job.clone();
                known_job.state = job.state.clone();
                ensure!(
                    known_job == job
                        && known.job.state.attempt() == job.state.attempt()
                        && source_body["method"] == "agent_result"
                        && source_body["message_id"].as_str() == Some(job.message_id.as_str())
                        && source_body["decision_id"].as_str()
                            == Some(format!("decision:{}", job.message_id).as_str())
                        && source_body["output"].as_str() == Some(known.output.as_str())
                        && source_body["text"].as_str() == Some(known.output.as_str())
                        && source_body["blobs"] == serde_json::to_value(&known.blobs)?,
                    "expired manager can only publish its retained terminal result"
                );
            }
        }
    }
    let signed = auth.signed_invocation();
    let checked = super::policy::authorize_request_invocation_for_body(
        state,
        signed,
        facts,
        transport,
        source_body,
        now_ms,
    )?;
    ensure!(
        checked.invocation_hash() == auth.invocation_hash(),
        "authorization proof changed"
    );
    if let Some(scope) = scoped {
        scope.check_current_in(tx, policy.verified(), facts, now_ms)?;
    } else {
        check_policy_heads_in(tx, state, auth, now_ms)?;
    }
    ensure!(
        proof.challenge.stage == CheckStage::Admit,
        "incorrect authorization freshness stage"
    );
    let freshness = state.verify_freshness(auth, &proof.challenge, &proof.receipts, now_ms)?;
    state.admit_operation(auth, &freshness, now_ms)?;
    consume_challenge_record_in(tx, auth, proof)?;
    let (actor_kind, actor_id) = match auth.actor() {
        Actor::Account(id) => ("account", id.as_str()),
        Actor::Service(id) => ("service", id.as_str()),
    };
    let action = serde_json::to_value(auth.action())?;
    tx.execute(
        "INSERT INTO accepted_authorizations(space_id,authorization_id,invocation_hash,invocation_body_hash,actor_kind,actor_id,device_id,resource_id,action,grant_leaf,signed_invocation,source_body,transport,admission_challenge,admission_receipts,admitted_at_ms,operation_id) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![auth.resource().space.as_str(), auth.operation_id(), auth.invocation_hash().as_str(),
            auth.body_hash().as_str(), actor_kind, actor_id, auth.device().as_str(),
            auth.resource().resource.as_str(), action.as_str().context("invalid action")?,
            signed.payload.grant_leaf.as_str(), serde_json::to_string(signed)?,
            serde_json::to_string(source_body)?, serde_json::to_string(&transport.stored())?,
            serde_json::to_string(&proof.challenge)?, serde_json::to_string(&proof.receipts)?,
            now_ms, operation_id],
    )?;
    tx.execute(
        "INSERT INTO authorization_nonces(space_id,device_id,nonce,authorization_id,body_hash) VALUES(?,?,?,?,?)",
        params![auth.resource().space.as_str(), auth.device().as_str(), auth.nonce().as_str(),
            auth.operation_id(), auth.body_hash().as_str()],
    )?;
    Ok(AdmissionProof {
        transport: transport.stored(),
        invocation: signed.clone(),
        source_body: source_body.clone(),
        challenge: proof.challenge.clone(),
        receipts: proof.receipts.clone(),
        admitted_at_ms: now_ms,
    })
}

pub(crate) fn reconstruct_admission(
    connection: &Connection,
    policy: &PolicyState,
    proof: &AdmissionProof,
    facts: &RequestFacts,
    transport: &TransportEvidence,
) -> Result<VerifiedAdmission> {
    validate_admission_evidence(proof, transport)?;
    if let Some((admitted, _)) = policy
        .local_admissions
        .admitted_in(connection, policy, proof, facts, false)?
    {
        return Ok(admitted);
    }
    reconstruct_stored_admission_uncached(connection, policy, proof, facts)
}

pub(crate) fn reconstruct_stored_admission(
    connection: &Connection,
    policy: &PolicyState,
    proof: &AdmissionProof,
    facts: &RequestFacts,
) -> Result<VerifiedAdmission> {
    if let Some((admitted, _)) = policy
        .local_admissions
        .admitted_in(connection, policy, proof, facts, true)?
    {
        return Ok(admitted);
    }
    reconstruct_stored_admission_uncached(connection, policy, proof, facts)
}

pub(crate) fn reconstruct_admission_uncached(
    connection: &Connection,
    policy: &PolicyState,
    proof: &AdmissionProof,
    facts: &RequestFacts,
    transport: &TransportEvidence,
) -> Result<VerifiedAdmission> {
    validate_admission_evidence(proof, transport)?;
    reconstruct_stored_admission_uncached(connection, policy, proof, facts)
}

pub(crate) fn reconstruct_stored_admission_uncached(
    connection: &Connection,
    policy: &PolicyState,
    proof: &AdmissionProof,
    facts: &RequestFacts,
) -> Result<VerifiedAdmission> {
    let original = OriginalAdmissionView::load(connection, policy, proof)?;
    if let Some(task) = original.task_policy() {
        let invocation = &proof.invocation.payload;
        ensure!(
            facts.resource == invocation.resource
                && facts.action == invocation.action
                && facts.audience == invocation.audience
                && facts.operation_id == invocation.operation_id
                && facts.body_hash == invocation.body_hash
                && facts.context == invocation.context
                && ObjectHash::body(&proof.source_body)? == invocation.body_hash,
            "original task admission differs from its parsed request facts"
        );
        return Ok(task.admission().clone());
    }
    Ok(original.state().verify_original_admission(proof, facts)?)
}

pub(super) fn reconstruct_admission_at_current_heads(
    policy: &PolicyState,
    proof: &AdmissionProof,
    facts: &RequestFacts,
    transport: &TransportEvidence,
) -> Result<Option<VerifiedAdmission>> {
    validate_admission_evidence(proof, transport)?;
    if !policy.current_heads_at(&proof.challenge.policies, proof.admitted_at_ms)? {
        return Ok(None);
    }
    Ok(Some(
        policy.verified().verify_original_admission(proof, facts)?,
    ))
}

fn validate_admission_evidence(
    proof: &AdmissionProof,
    transport: &TransportEvidence,
) -> Result<()> {
    ensure!(
        proof.challenge.stage == CheckStage::Admit,
        "stored admission has incorrect freshness stage"
    );
    ensure!(
        proof.transport.existing_evidence()? == *transport,
        "admission transport differs from captured evidence"
    );
    Ok(())
}

struct StoredAdmission {
    proof: AdmissionProof,
    transport: StoredTransport,
}

fn load_admission_from(
    connection: &Connection,
    space: &str,
    operation_id: &str,
) -> Result<StoredAdmission> {
    let ids = connection.prepare("SELECT authorization_id FROM accepted_authorizations WHERE space_id=? AND operation_id=?")?
        .query_map(params![space, operation_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        ids.len() == 1,
        "operation must have exactly one accepted authorization"
    );
    load_authorization_from(connection, space, &ids[0])
}

pub(super) fn admission_for_operation_from(
    connection: &Connection,
    space: &str,
    operation_id: &str,
) -> Result<AdmissionProof> {
    Ok(load_admission_from(connection, space, operation_id)?.proof)
}

pub(super) fn load_admission_in(
    connection: &Connection,
    space: &str,
    authorization_id: &str,
) -> Result<AdmissionProof> {
    Ok(load_authorization_from(connection, space, authorization_id)?.proof)
}

fn load_authorization_from(
    connection: &Connection,
    space: &str,
    authorization_id: &str,
) -> Result<StoredAdmission> {
    let mut statement = connection.prepare(
        "SELECT authorization_id,invocation_hash,invocation_body_hash,actor_kind,actor_id,device_id,resource_id,action,grant_leaf,signed_invocation,source_body,transport,admission_challenge,admission_receipts,admitted_at_ms FROM accepted_authorizations WHERE space_id=? AND authorization_id=?",
    )?;
    let rows = statement
        .query_map(params![space, authorization_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, String>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, String>(13)?,
                row.get::<_, u64>(14)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        rows.len() == 1,
        "operation must have exactly one accepted authorization"
    );
    let (
        authorization_id,
        invocation_hash,
        body_hash,
        actor_kind,
        actor_id,
        device_id,
        resource_id,
        action,
        grant_leaf,
        invocation,
        source_body,
        transport,
        challenge,
        receipts,
        admitted_at_ms,
    ) = rows.into_iter().next().unwrap();
    let transport: StoredTransport = serde_json::from_str(&transport)?;
    let proof = AdmissionProof {
        transport: transport.clone(),
        invocation: serde_json::from_str(&invocation)?,
        source_body: serde_json::from_str(&source_body)?,
        challenge: serde_json::from_str(&challenge)?,
        receipts: serde_json::from_str(&receipts)?,
        admitted_at_ms,
    };
    let signed = &proof.invocation;
    let invocation = &signed.payload;
    let (expected_kind, expected_id) = match &invocation.actor {
        Actor::Account(id) => ("account", id.as_str()),
        Actor::Service(id) => ("service", id.as_str()),
    };
    ensure!(
        invocation.resource.space.as_str() == space
            && invocation.operation_id == authorization_id
            && signed.hash()?.as_str() == invocation_hash
            && invocation.body_hash.as_str() == body_hash
            && ObjectHash::body(&proof.source_body)? == invocation.body_hash
            && actor_kind == expected_kind
            && actor_id == expected_id
            && invocation.device.as_str() == device_id
            && invocation.resource.resource.as_str() == resource_id
            && invocation.grant_leaf.as_str() == grant_leaf
            && serde_json::to_value(invocation.action)?.as_str() == Some(action.as_str()),
        "stored authorization metadata does not match signed invocation"
    );
    let nonces = connection.prepare(
        "SELECT device_id,nonce,body_hash FROM authorization_nonces WHERE space_id=? AND authorization_id=?",
    )?.query_map(params![space, authorization_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        nonces == vec![(device_id, invocation.nonce.to_string(), body_hash)],
        "stored invocation nonce does not match accepted authorization"
    );
    let challenge_row: (String, String, String, u64, bool) = connection.query_row(
        "SELECT invocation_hash,stage,challenge,expires_at_ms,consumed FROM authorization_challenges WHERE space_id=? AND nonce=?",
        params![space, proof.challenge.nonce.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    )?;
    ensure!(
        challenge_row.0 == invocation_hash
            && challenge_row.1 == "admit"
            && serde_json::from_str::<vibeface_secure::authorization::FreshnessChallenge>(
                &challenge_row.2
            )? == proof.challenge
            && challenge_row.3 == proof.challenge.expires_at_ms
            && challenge_row.4,
        "stored admission challenge does not match consumed challenge"
    );
    Ok(StoredAdmission { proof, transport })
}

pub(crate) fn validate_persisted_admission(
    connection: &Connection,
    proof: &AdmissionProof,
) -> Result<()> {
    let stored = load_authorization_from(
        connection,
        proof.invocation.payload.resource.space.as_str(),
        &proof.invocation.payload.operation_id,
    )?;
    ensure!(
        stored.proof == *proof,
        "persisted admission differs from event authorization"
    );
    Ok(())
}

pub(super) fn persisted_admission_facts(
    connection: &Connection,
    policy: &PolicyState,
    proof: &AdmissionProof,
) -> Result<RequestFacts> {
    if let Some(facts) =
        super::event_authorization::persisted_admission_facts(connection, policy, proof)?
    {
        return Ok(facts);
    }
    if let Some(facts) =
        super::execution_dispatch_resolution::persisted_resolution_facts(connection, proof)?
    {
        return Ok(facts);
    }
    let body = &proof.source_body;
    let method = manager_field(body, "method")?;
    let manager_output = matches!(method, "agent_result" | "agent_notification");
    let manager_control = matches!(method, "cancel_manager" | "resolve_manager_unknown");
    if manager_control {
        let saved: crate::scheduling::ManagerControlReceipt = crate::store::scheduler_load(
            connection,
            proof.invocation.payload.resource.space.as_str(),
            &format!("manager-control:{}", proof.invocation.payload.operation_id),
        )?
        .context("manager control has no durable receipt")?;
        ensure!(
            saved.admission == *proof,
            "manager control receipt differs from its admission"
        );
    }
    ensure!(
        manager_output
            || manager_control
            || matches!(
                method,
                "agent_start" | "bind_execution" | "seal_execution" | "rebind_execution"
            ),
        "accepted authorization has no original business event"
    );
    if manager_output {
        let space = proof.invocation.payload.resource.space.as_str();
        let scope = manager_field(body, "conversation_id")?;
        let authority = &body["manager_authority"];
        let trigger = manager_field(authority, "trigger")?;
        let target = manager_field(authority, "scheduler_node")?;
        let marker: crate::scheduling::SchedulerTerminalPublication = crate::store::scheduler_load(
            connection,
            space,
            &scheduler_key("publication", scope, target, trigger),
        )?
        .context("manager output has no durable pending publication")?;
        ensure!(
            marker.admission == *proof
                && marker.job.resource_id == proof.invocation.payload.resource.resource,
            "pending manager output differs from its admission"
        );
        let current: SchedulerJob = crate::store::scheduler_load(
            connection,
            space,
            &scheduler_key("job", scope, target, trigger),
        )?
        .context("pending manager output job is missing")?;
        ensure!(
            current == marker.job,
            "pending manager output changed its attempt"
        );
    }
    let space = SpaceId::parse(manager_field(body, "space_id")?)?;
    let resource = ResourceId::parse(manager_field(body, "resource_id")?)?;
    let scope = manager_field(body, "conversation_id")?;
    let original = OriginalAdmissionView::load(connection, policy, proof)?;
    let state = original.state();
    let record = state
        .resource(&space, &resource)
        .context("historical resource is missing")?;
    ensure!(
        record.payload.host == proof.invocation.payload.audience.service,
        "historical request targets another resource host"
    );
    let reference = if method == "agent_start" {
        manager_field(body, "trigger")?
    } else if manager_control {
        manager_field(body, "trigger")?
    } else if manager_output {
        manager_field(&body["sdk_session"], "session_id")?
    } else {
        manager_field(body, "task_ref")?
    };
    let mut roots = vec![reference.to_owned()];
    if (method == "agent_start" || manager_output) && !body["basis_heads"].is_null() {
        roots.extend(serde_json::from_value::<Vec<String>>(
            body["basis_heads"].clone(),
        )?);
    }
    let events = super::event_authorization::historical_event_dependencies_in(
        connection,
        policy,
        space.as_str(),
        &resource,
        scope,
        &roots,
    )?;
    let source = events
        .get(reference)
        .context("historical command source is missing")?;
    ensure!(
        source.event.scope_id == scope && source.event.resource_id == resource,
        "historical command source belongs to another conversation or resource"
    );
    let closure = closure_hashes(&events, &[reference.to_owned()]);
    ensure!(
        closure.iter().all(|hash| events.contains_key(hash)),
        "historical command source has incomplete context"
    );
    let creations: Vec<_> = closure
        .iter()
        .filter_map(|hash| {
            let event = &events[hash].event;
            (event.scope_id == scope && matches!(event.body, EventBody::ConversationCreated { .. }))
                .then_some(event)
        })
        .collect();
    ensure!(
        creations.len() == 1 && creations[0].resource_id == resource,
        "historical command has no unique causal conversation creation"
    );
    let mut context = InvocationContext {
        conversation: Some(scope.into()),
        workspace_resource: Some(resource.clone()),
        ..Default::default()
    };
    if manager_control {
        ensure!(
            matches!(proof.invocation.payload.actor, Actor::Account(_)),
            "historical manager control requires Account"
        );
    } else if method == "agent_start" || manager_output {
        let heads: Vec<String> = if body["basis_heads"].is_null() {
            vec![reference.to_owned()]
        } else {
            serde_json::from_value(body["basis_heads"].clone())?
        };
        let origin = super::services::resolve_agent_origin(
            policy,
            connection,
            &state,
            &events,
            reference,
            &heads,
            proof.admitted_at_ms,
        )?;
        ensure!(
            origin.parent.invocation_hash().as_str() == manager_field(body, "parent_invocation")?
                && origin.parent.actor() == &proof.invocation.payload.initiator
                && (manager_output
                    || proof.invocation.payload.operation_id
                        == format!("manager:{reference}:{}", manager_field(body, "attempt_id")?)),
            "historical manager delegation differs from its account origin"
        );
        context.task = origin.task.as_ref().map(|(_, reference)| reference.clone());
        let session: SessionRef = serde_json::from_value(body["sdk_session"].clone())?;
        ensure!(
            session.session_id == reference && session.target == proof.invocation.payload.device,
            "historical manager session differs from its admitted trigger"
        );
        context.sdk_session = Some(session);
    } else {
        ensure!(
            matches!(&source.event.body, EventBody::TaskCreated {task_id,..}
            if body["task_id"].as_str() == Some(task_id.as_str())),
            "historical task command differs from its task creation"
        );
        context.task = Some(vibeface_secure::authorization::EventRef::parse(
            &source.envelope.hash,
        )?);
        let (target, session_id, executor) = if method == "seal_execution" {
            let signed: crate::binding::SignedBinding =
                serde_json::from_value(body["binding"].clone())?;
            ensure!(
                signed.binding.task_ref == reference
                    && signed.binding.conversation_id == scope
                    && signed.binding.network_id == space.as_str(),
                "seal refers to another historical task binding"
            );
            (
                signed.binding.target_node,
                signed.binding.session_id,
                signed.binding.open_spec["executor"]
                    .as_str()
                    .context("sealed binding has no executor")?
                    .to_owned(),
            )
        } else {
            (
                manager_field(body, "target_node")?.to_owned(),
                manager_field(body, "session_id")?.to_owned(),
                manager_field(body, "executor")?.to_owned(),
            )
        };
        let key: String = if method == "bind_execution" && body.get("execution_resource").is_some()
        {
            use vibeface_secure::authorization::{DeviceId, PublicKey};
            let _: ResourceRef = serde_json::from_value(body["execution_resource"].clone())?;
            let _: ObjectHash = serde_json::from_value(body["execution_grant"].clone())?;
            let key: PublicKey = serde_json::from_value(body["target_public_key"].clone())?;
            let certificate =
                state.verify_transport_device(&DeviceId::from_key(&key), proof.admitted_at_ms)?;
            ensure!(
                certificate.payload.public_key == key,
                "historical execution target differs from its independent certificate"
            );
            key.to_string()
        } else {
            connection.query_row(
                "SELECT public_key FROM node_certificates WHERE space_id=? AND node_id=?",
                params![space.as_str(), target],
                |row| row.get(0),
            )?
        };
        ensure!(
            crate::crypto::fingerprint(&key)? == target,
            "historical execution target key mismatch"
        );
        context.execution_session = Some(SessionRef {
            target: vibeface_secure::authorization::DeviceId::from_key(
                &vibeface_secure::authorization::PublicKey::parse(&key)?,
            ),
            session_id,
        });
        context.executor_kind = Some(executor);
        if !body["sdk_session"].is_null() {
            context.sdk_session = Some(serde_json::from_value(body["sdk_session"].clone())?);
        }
    }
    let facts = RequestFacts {
        resource: ResourceRef { space, resource },
        action: crate::model::authorization_action(body)?,
        audience: proof.invocation.payload.audience.clone(),
        operation_id: manager_field(body, "authorization_id")?.into(),
        body_hash: ObjectHash::body(body)?,
        context,
    };
    if let Some(session) = &facts.context.sdk_session {
        let Actor::Service(service) = &proof.invocation.payload.actor else {
            anyhow::bail!("historical SDK session requires a service actor");
        };
        let heads: Vec<String> = if body["basis_heads"].is_null() {
            vec![session.session_id.clone()]
        } else {
            serde_json::from_value(body["basis_heads"].clone())?
        };
        let origin = super::services::resolve_agent_origin(
            policy,
            connection,
            &state,
            &events,
            &session.session_id,
            &heads,
            proof.admitted_at_ms,
        )?;
        ensure!(
            origin.parent.invocation_hash().as_str() == manager_field(body, "parent_invocation")?,
            "historical SDK source differs from its causal account origin"
        );
        if let Some((_, task)) = &origin.task {
            ensure!(
                facts.context.task.as_ref() == Some(task),
                "historical SDK escaped its original task"
            );
        }
        let delegated = state.verify_service_delegation(
            &origin.parent,
            service,
            &proof.invocation.payload.grant_leaf,
            &facts,
            proof.admitted_at_ms,
        )?;
        ensure!(
            delegated.device() == &proof.invocation.payload.device
                && delegated.initiator() == &proof.invocation.payload.initiator
                && delegated.initiating_grant()
                    == proof.invocation.payload.initiating_grant.as_ref(),
            "historical SDK invocation differs from its verified delegation"
        );
    }
    Ok(facts)
}

pub(crate) fn validate_persisted_authorizations(
    connection: &Connection,
    policy: &PolicyState,
) -> Result<()> {
    verified_admission_checkpoints_from(connection, policy).map(|_| ())
}

pub(crate) fn verified_admission_checkpoints_from(
    connection: &Connection,
    policy: &PolicyState,
) -> Result<AdmissionCheckpoints> {
    let mut uncached = policy.clone();
    uncached.local_admissions = AdmissionCheckpoints::default();
    let policy = &uncached;
    let mut checkpoints = AdmissionCheckpoints::default();
    let rows = connection.prepare("SELECT space_id,authorization_id,operation_id FROM accepted_authorizations ORDER BY space_id,authorization_id")?
        .query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,Option<String>>(2)?)))?
        .collect::<std::result::Result<Vec<_>,_>>()?;
    let mut operations = BTreeMap::new();
    for (space, id, operation_id) in rows {
        let stored = load_authorization_from(connection, &space, &id)?;
        let facts = persisted_admission_facts(connection, policy, &stored.proof)?;
        let admitted =
            reconstruct_stored_admission_uncached(connection, policy, &stored.proof, &facts)?;
        match operation_id {
            Some(operation_id) => {
                ensure!(
                    stored.proof.source_body["method"] == "operate",
                    "non-operation authorization is indexed as an operation"
                );
                let operation = operation_from(connection, &space, &operation_id)?
                    .context("accepted operation has no ledger record")?;
                ensure!(
                    serde_json::from_value::<OperationRequest>(
                        stored.proof.source_body["request"].clone()
                    )? == operation.request,
                    "operation ledger differs from its original authorization"
                );
                ensure!(
                    operations
                        .insert((space.clone(), operation_id.clone()), id.clone())
                        .is_none(),
                    "operation has multiple accepted authorizations"
                );
                let reservations = connection.prepare("SELECT target_node,resource FROM resource_reservations WHERE space_id=? AND operation_id=? ORDER BY resource")?
                    .query_map(params![space,operation_id],|row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?
                    .collect::<std::result::Result<Vec<_>,_>>()?;
                let expected: Vec<_> = if operation.state.terminal() {
                    Vec::new()
                } else {
                    operation
                        .request
                        .resources
                        .iter()
                        .map(|resource| (operation.request.target_node.clone(), resource.clone()))
                        .collect()
                };
                ensure!(
                    reservations == expected,
                    "operation reservations differ from its ledger state"
                );
                let has_lease: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM local_values WHERE space_id=? AND key=?)",
                    params![space, format!("lease:{operation_id}")],
                    |row| row.get(0),
                )?;
                if operation.state == OperationState::Running && !has_lease {
                    super::execution_dispatch::validate_retained_remote_dispatch_in(
                        connection,
                        policy,
                        &stored.proof,
                        &admitted,
                        &facts,
                        &operation,
                    )
                    .context("running operation has neither a verified local lease nor a retained remote dispatch")?;
                }
                if has_lease {
                    lease_deadline_from(connection, policy, &space, &operation_id)?;
                }
            }
            None => ensure!(
                stored.proof.source_body["method"] != "operate",
                "operation authorization has no operation index"
            ),
        }
        if let Some(checkpoint) =
            checkpoint_from_verified_in(connection, policy, &stored.proof, facts, admitted, true)?
        {
            checkpoints.insert(checkpoint);
        }
        if stored.proof.source_body["method"] == "agent_start" {
            let (claim, original, job) = load_manager_from(connection, &space, &id)?;
            ensure!(
                original.proof == stored.proof,
                "manager index points to another admission"
            );
            if !claim.retired_before_start {
                let session = crate::store::load_scheduler_session(
                    connection,
                    &space,
                    &claim.conversation_id,
                    &claim.target_node,
                )?;
                ensure!(
                    !matches!(
                        job.state,
                        JobState::Claimed { .. }
                            | JobState::Running { .. }
                            | JobState::Cancelling { .. }
                    ) || session.active.as_deref() == Some(&claim.trigger),
                    "active manager job is absent from its session"
                );
            }
            manager_lease_from(connection, policy, &space, &id)?;
        }
    }

    let ledger = connection
        .prepare("SELECT space_id,operation_id FROM operations")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        ledger.len() == operations.len() && ledger.iter().all(|key| operations.contains_key(key)),
        "operation ledger has no unique accepted authorization"
    );
    let orphan_nonce: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM authorization_nonces n LEFT JOIN accepted_authorizations a ON a.space_id=n.space_id AND a.authorization_id=n.authorization_id WHERE a.authorization_id IS NULL)",[], |row| row.get(0))?;
    ensure!(
        !orphan_nonce,
        "invocation nonce has no accepted authorization"
    );
    let locals = connection.prepare("SELECT space_id,key,value FROM local_values WHERE key LIKE 'manager-admission:%' OR key LIKE 'lease:%'")?
        .query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))?
        .collect::<std::result::Result<Vec<_>,_>>()?;
    for (space, key, encoded) in locals {
        if let Some(manager_id) = key.strip_prefix("manager-admission:") {
            let claim: ManagerClaim = serde_json::from_str(&encoded)?;
            ensure!(
                claim.manager_id == manager_id,
                "manager claim is indexed by another attempt"
            );
            load_manager_from(connection, &space, manager_id)?;
        } else if let Some(id) = key.strip_prefix("lease:") {
            if operations.contains_key(&(space.clone(), id.to_owned())) {
                lease_deadline_from(connection, policy, &space, id)?;
            } else {
                manager_lease_from(connection, policy, &space, id)?;
            }
        }
    }
    Ok(checkpoints)
}

fn operation_from(
    connection: &Connection,
    space: &str,
    operation_id: &str,
) -> Result<Option<OperationRecord>> {
    let row: Option<(String, String, String, String, Option<String>)> = connection
        .query_row(
            "SELECT request,state,result,target_node,session_id FROM operations WHERE space_id=? AND operation_id=?",
            params![space, operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()?;
    row.map(|(request, state, result, target, session)| {
        let request: OperationRequest = serde_json::from_str(&request)?;
        ensure!(
            request.network_id == space
                && request.operation_id == operation_id
                && request.target_node == target
                && request.session_id == session,
            "operation metadata does not match request"
        );
        Ok(OperationRecord {
            request,
            state: serde_json::from_str(&state)?,
            result: serde_json::from_str(&result)?,
        })
    })
    .transpose()
}

pub(crate) fn validate_basis_in(
    connection: &Connection,
    request: &OperationRequest,
    resource: &ResourceId,
) -> Result<()> {
    validate_operation_binding_from(connection, request)?;
    let heads = if request.basis_heads.is_empty() {
        let events = super::event_authorization::retained_scope_events_in(
            connection,
            &request.network_id,
            resource,
            &request.conversation_id,
        )?;
        heads_from_events(&events, &request.conversation_id)
    } else {
        request.basis_heads.clone()
    };
    for hash in heads {
        let encoded: String = connection
            .query_row(
                "SELECT envelope FROM events WHERE space_id=? AND hash=?",
                params![request.network_id, hash],
                |row| row.get(0),
            )
            .context("operation basis is incomplete")?;
        let source: crate::model::SignedEvent = serde_json::from_str(&encoded)?;
        let event: crate::model::Event = serde_json::from_str(&source.canonical_body)?;
        ensure!(
            source.hash == hash
                && event.network_id == request.network_id
                && event.resource_id == *resource
                && event.scope_id == request.conversation_id,
            "operation basis is outside its conversation or resource"
        );
        super::event_authorization::validate_retained_event_dependencies_in(connection, &source)?;
    }
    Ok(())
}

fn checked_session_from(
    connection: &Connection,
    request: &OperationRequest,
) -> Result<Option<ExecutionSession>> {
    let Some(session_id) = request.session_id.as_deref() else {
        ensure!(
            request.action["type"] == "command" && request.expected_revision == 0,
            "only commands may omit an execution session"
        );
        return Ok(None);
    };
    let row: Option<(u64, String)> = connection.query_row(
        "SELECT revision,record FROM execution_sessions WHERE space_id=? AND target_node=? AND session_id=?",
        params![request.network_id, request.target_node, request.session_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    let session = row
        .map(|(revision, encoded)| -> Result<_> {
            let session: ExecutionSession = serde_json::from_str(&encoded)?;
            ensure!(
                session.network_id == request.network_id
                    && session.target_node == request.target_node
                    && session.session_id == session_id
                    && session.revision == revision,
                "execution session metadata does not match record"
            );
            Ok(session)
        })
        .transpose()?;
    ensure!(
        session.as_ref().map_or(0, |session| session.revision) == request.expected_revision,
        "stale execution session revision"
    );
    Ok(session)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LeaseRecord {
    proof: FreshnessProof,
    checked_at_ms: u64,
    deadline: u64,
}

fn save_lease_in(
    tx: &Transaction<'_>,
    space: &str,
    operation_id: &str,
    proof: &FreshnessProof,
    checked_at_ms: u64,
    deadline: u64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?) ON CONFLICT(space_id,key) DO UPDATE SET value=excluded.value",
        params![space, format!("lease:{operation_id}"), serde_json::to_string(&LeaseRecord { proof: proof.clone(), checked_at_ms, deadline })?],
    )?;
    Ok(())
}

fn check_operation_authorization_in(
    connection: &Connection,
    state: &VerifiedPolicySet,
    auth: &VerifiedAuthorization,
    operation: &OperationRecord,
) -> Result<()> {
    let stored = load_admission_from(
        connection,
        &operation.request.network_id,
        &operation.request.operation_id,
    )?;
    ensure!(
        stored.proof.invocation == *auth.signed_invocation()
            && serde_json::from_value::<OperationRequest>(
                stored.proof.source_body["request"].clone()
            )? == operation.request,
        "operation authorization changed before commit"
    );
    let facts = request_facts_from(
        connection,
        state,
        &stored.proof.source_body,
        auth.audience(),
    )?;
    let invocation = &auth.signed_invocation().payload;
    ensure!(
        facts.resource == invocation.resource
            && facts.action == invocation.action
            && facts.audience == invocation.audience
            && facts.operation_id == invocation.operation_id
            && facts.body_hash == invocation.body_hash
            && facts.context == invocation.context,
        "operation request facts changed before commit"
    );
    check_operation_reservations_in(connection, operation)
}

pub(super) fn check_operation_reservations_in(
    connection: &Connection,
    operation: &OperationRecord,
) -> Result<()> {
    let reservations = connection.prepare(
        "SELECT target_node,resource FROM resource_reservations WHERE space_id=? AND operation_id=? ORDER BY resource",
    )?.query_map(params![operation.request.network_id, operation.request.operation_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let expected: Vec<_> = operation
        .request
        .resources
        .iter()
        .map(|resource| (operation.request.target_node.clone(), resource.clone()))
        .collect();
    ensure!(
        reservations == expected,
        "operation resource reservations changed before commit"
    );
    Ok(())
}

fn lease_deadline_from(
    connection: &Connection,
    policy: &PolicyState,
    space: &str,
    operation_id: &str,
) -> Result<u64> {
    let encoded: String = connection.query_row(
        "SELECT value FROM local_values WHERE space_id=? AND key=?",
        params![space, format!("lease:{operation_id}")],
        |row| row.get(0),
    )?;
    let lease: LeaseRecord = serde_json::from_str(&encoded)?;
    ensure!(
        matches!(
            lease.proof.challenge.stage,
            CheckStage::Start | CheckStage::Renew
        ),
        "persisted execution lease has invalid stage"
    );
    let stored = load_admission_from(connection, space, operation_id)?;
    verify_lease_record(connection, policy, &stored, &lease)
}

fn verify_lease_record(
    connection: &Connection,
    policy: &PolicyState,
    stored: &StoredAdmission,
    lease: &LeaseRecord,
) -> Result<u64> {
    let facts = persisted_admission_facts(connection, policy, &stored.proof)?;
    if lease.proof.challenge.stage == CheckStage::Admit {
        ensure!(
            lease.checked_at_ms == stored.proof.admitted_at_ms
                && lease.proof.challenge == stored.proof.challenge
                && lease.proof.receipts == stored.proof.receipts,
            "initial manager lease differs from its original admission"
        );
        if let Some((_, deadline)) =
            policy
                .local_admissions
                .admitted_in(connection, policy, &stored.proof, &facts, true)?
        {
            ensure!(
                lease.deadline == deadline,
                "persisted lease deadline differs from verified local admission"
            );
            return Ok(deadline);
        }
    }
    let admission = reconstruct_stored_admission(connection, policy, &stored.proof, &facts)?;
    if lease.proof.challenge.stage == CheckStage::Admit {
        let deadline = admission.admission_expires_at_ms();
        ensure!(
            lease.deadline == deadline,
            "persisted lease deadline differs from its original admission"
        );
        return Ok(deadline);
    }
    let current = policy.verified();
    let retained =
        if policy.current_heads_at(&lease.proof.challenge.policies, lease.checked_at_ms)? {
            None
        } else {
            Some(policy.historical(&lease.proof.challenge.policies, lease.checked_at_ms)?)
        };
    let historical = retained.as_ref().unwrap_or(current);
    let auth = historical.verify_admitted_operation_authorization(
        &admission,
        &facts,
        lease.checked_at_ms,
    )?;
    let freshness = historical.verify_freshness(
        &auth,
        &lease.proof.challenge,
        &lease.proof.receipts,
        lease.checked_at_ms,
    )?;
    ensure!(
        lease.deadline == freshness.expires_at_ms().min(auth.expires_at_ms()),
        "persisted lease deadline differs from verified receipts"
    );
    check_challenge_record(connection, &auth, &lease.proof, true)?;
    Ok(lease.deadline)
}

pub(crate) struct ManagerAdmission {
    pub(crate) manager_id: String,
    pub(crate) job: SchedulerJob,
    pub(crate) previous_native_id: Option<String>,
    pub(crate) fresh: bool,
    pub(crate) lease_expires_at_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagerClaim {
    manager_id: String,
    conversation_id: String,
    trigger: String,
    target_node: String,
    attempt: ManagerAttempt,
    retired_before_start: bool,
}

fn local_from<T: DeserializeOwned>(
    connection: &Connection,
    space: &str,
    key: &str,
) -> Result<Option<T>> {
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![space, key],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| Ok(serde_json::from_str(&value)?))
        .transpose()
}

fn set_local_in(
    tx: &Transaction<'_>,
    space: &str,
    key: &str,
    value: &impl Serialize,
) -> Result<()> {
    tx.execute("INSERT INTO local_values(space_id,key,value) VALUES(?,?,?) ON CONFLICT(space_id,key) DO UPDATE SET value=excluded.value",
        params![space,key,serde_json::to_string(value)?])?;
    Ok(())
}

fn manager_field<'a>(body: &'a Value, field: &str) -> Result<&'a str> {
    let value = body[field]
        .as_str()
        .with_context(|| format!("manager {field} is required"))?;
    ensure!(
        !value.is_empty() && value.len() <= 1024,
        "invalid manager field"
    );
    Ok(value)
}

fn check_manager_parent(
    connection: &Connection,
    policy: &PolicyState,
    auth: &VerifiedAuthorization,
    body: &Value,
    now_ms: u64,
) -> Result<()> {
    ensure!(
        body["method"] == "agent_start"
            && auth.action() == Action::AgentStart
            && matches!(auth.actor(), Actor::Service(_)),
        "manager requires an admitted service agent-start invocation"
    );
    let space = auth.resource().space.as_str();
    let conversation = manager_field(body, "conversation_id")?;
    let trigger = manager_field(body, "trigger")?;
    ensure!(
        auth.operation_id() == format!("manager:{trigger}:{}", manager_field(body, "attempt_id")?),
        "manager authorization does not identify its trigger"
    );
    let sdk_session: SessionRef = serde_json::from_value(body["sdk_session"].clone())?;
    ensure!(
        sdk_session.session_id == trigger,
        "manager SDK session does not identify its trigger"
    );
    let descriptor: AgentSession = local_from(
        connection,
        space,
        &format!("agent-session:{}", sdk_session.session_id),
    )?
    .context("manager has no persisted service delegation")?;
    ensure!(
        descriptor.space_id == auth.resource().space
            && descriptor.resource_id == auth.resource().resource
            && descriptor.sdk_session == sdk_session
            && descriptor.device == *auth.device()
            && auth.actor() == &Actor::Service(descriptor.service.clone())
            && descriptor.conversation_id == conversation
            && descriptor.grant_leaf == auth.signed_invocation().payload.grant_leaf
            && descriptor.initiator == *auth.initiator()
            && descriptor.initiating_grant == auth.signed_invocation().payload.initiating_grant
            && descriptor.parent_invocation.as_str() == manager_field(body, "parent_invocation")?
            && now_ms < descriptor.expires_at_ms,
        "manager invocation differs from its persisted delegation"
    );
    let events = super::event_authorization::retained_scope_events_in(
        connection,
        space,
        &auth.resource().resource,
        conversation,
    )?;
    let origin = super::services::resolve_current_agent_origin(
        policy, connection, &events, trigger, now_ms,
    )?;
    if let Some(request) = &origin.execution {
        crate::store::validate_operation_binding_from(connection, request)?;
    }
    let parent = origin.parent;
    ensure!(
        matches!(parent.actor(), Actor::Account(_))
            && parent.resource() == auth.resource()
            && parent.actor() == auth.initiator()
            && parent.invocation_hash() == &descriptor.parent_invocation
            && auth
                .grant_path()
                .contains(&parent.signed_invocation().payload.grant_leaf)
            && auth.signed_invocation().payload.context.task
                == origin.task.as_ref().map(|(_, reference)| reference.clone()),
        "manager delegation differs from its current account origin or task"
    );
    Ok(())
}

fn load_manager_from(
    connection: &Connection,
    space: &str,
    manager_id: &str,
) -> Result<(ManagerClaim, StoredAdmission, SchedulerJob)> {
    let claim: ManagerClaim = local_from(
        connection,
        space,
        &format!("manager-admission:{manager_id}"),
    )?
    .context("manager has no admitted attempt claim")?;
    ensure!(
        claim.manager_id == manager_id
            && manager_id == format!("manager:{}:{}", claim.trigger, claim.attempt.id),
        "manager admission mapping mismatch"
    );
    let stored = load_authorization_from(connection, space, manager_id)?;
    let body = &stored.proof.source_body;
    ensure!(
        manager_field(body, "conversation_id")? == claim.conversation_id
            && manager_field(body, "trigger")? == claim.trigger
            && manager_field(body, "scheduler_node")? == claim.target_node
            && manager_field(body, "attempt_id")? == claim.attempt.id,
        "manager admission source mismatch"
    );
    let job: SchedulerJob = crate::store::scheduler_load(
        connection,
        space,
        &scheduler_key(
            "job",
            &claim.conversation_id,
            &claim.target_node,
            &claim.trigger,
        ),
    )?
    .context("manager job is missing")?;
    ensure!(
        job.resource_id == stored.proof.invocation.payload.resource.resource,
        "manager job resource differs from admission"
    );
    ensure!(
        claim.retired_before_start
            || job
                .state
                .attempt()
                .is_some_and(|attempt| attempt.id == claim.attempt.id),
        "manager job no longer matches its admitted attempt"
    );
    Ok((claim, stored, job))
}

fn active_manager_id(connection: &Connection, space: &str, trigger: &str) -> Result<String> {
    local_from(connection, space, &format!("manager-active:{trigger}"))?
        .context("SDK session has no active admitted manager")
}

pub(crate) fn retire_queued_manager_in(
    connection: &Connection,
    space: &str,
    trigger: &str,
) -> Result<()> {
    let Some(manager_id) =
        local_from::<String>(connection, space, &format!("manager-active:{trigger}"))?
    else {
        return Ok(());
    };
    let key = format!("manager-admission:{manager_id}");
    let mut claim: ManagerClaim =
        local_from(connection, space, &key)?.context("queued manager claim is missing")?;
    ensure!(
        claim.trigger == trigger
            && claim.manager_id == manager_id
            && manager_id == format!("manager:{trigger}:{}", claim.attempt.id),
        "queued manager claim differs from its trigger"
    );
    let job: SchedulerJob = crate::store::scheduler_load(
        connection,
        space,
        &scheduler_key("job", &claim.conversation_id, &claim.target_node, trigger),
    )?
    .context("queued manager job is missing")?;
    ensure!(
        matches!(job.state, JobState::Queued),
        "a started manager requires reconciliation"
    );
    claim.retired_before_start = true;
    connection.execute(
        "UPDATE local_values SET value=? WHERE space_id=? AND key=?",
        params![serde_json::to_string(&claim)?, space, key],
    )?;
    Ok(())
}

fn check_manager_attempt_in(
    connection: &Connection,
    policy: &PolicyState,
    space: &str,
    claim: &ManagerClaim,
    job: &SchedulerJob,
    write: bool,
) -> Result<()> {
    ensure!(
        !claim.retired_before_start,
        "manager attempt was retired before SDK start"
    );
    let session = crate::store::load_scheduler_session(
        connection,
        space,
        &claim.conversation_id,
        &claim.target_node,
    )?;
    let attempt = job.state.attempt().context("manager job has no attempt")?;
    ensure!(
        session.active.as_deref() == Some(&claim.trigger)
            && attempt.id == claim.attempt.id
            && attempt.permission_version == session.permission_version
            && matches!(
                job.state,
                JobState::Claimed { .. } | JobState::Running { .. }
            )
            && (!write || (!attempt.read_only && session.unresolved.is_empty())),
        "manager attempt has been fenced or cannot write"
    );
    if let Some(version) = attempt.followup_version {
        let current = crate::store::followup::validate_followup_result_from(
            connection,
            policy,
            space,
            &claim.conversation_id,
            &claim.trigger,
            &claim.target_node,
        )?;
        ensure!(
            current.version == version,
            "manager follow-up version changed"
        );
        if write {
            ensure!(
                current.mode == crate::store::followup::FollowupMode::Continue,
                "report-only manager cannot write"
            );
        }
    }
    Ok(())
}

pub(crate) fn validate_manager_start_in(
    connection: &Connection,
    policy: &PolicyState,
    space: &str,
    trigger: &str,
    attempt_id: &str,
    now: u64,
) -> Result<()> {
    let id = active_manager_id(connection, space, trigger)?;
    let (claim, _, job) = load_manager_from(connection, space, &id)?;
    ensure!(
        claim.attempt.id == attempt_id && matches!(job.state, JobState::Claimed { .. }),
        "manager start attempt changed"
    );
    manager_authorization_from(connection, policy, space, &id, now)?;
    check_manager_attempt_in(connection, policy, space, &claim, &job, false)?;
    ensure!(
        now < manager_lease_from(connection, policy, space, &id)?,
        "manager start lease expired"
    );
    Ok(())
}

fn manager_lease_from(
    connection: &Connection,
    policy: &PolicyState,
    space: &str,
    manager_id: &str,
) -> Result<u64> {
    let snapshot = if connection.is_autocommit() {
        Some(connection.unchecked_transaction()?)
    } else {
        None
    };
    let (_, stored, _) = load_manager_from(connection, space, manager_id)?;
    let lease: LeaseRecord = local_from(connection, space, &format!("lease:{manager_id}"))?
        .context("manager lease is missing")?;
    ensure!(
        matches!(
            lease.proof.challenge.stage,
            CheckStage::Admit | CheckStage::Renew
        ),
        "manager lease has incorrect stage"
    );
    let deadline = verify_lease_record(connection, policy, &stored, &lease)?;
    if let Some(snapshot) = snapshot {
        snapshot.commit()?;
    }
    Ok(deadline)
}

fn manager_authorization_from(
    connection: &Connection,
    policy: &PolicyState,
    space: &str,
    manager_id: &str,
    now_ms: u64,
) -> Result<VerifiedAuthorization> {
    let (_, stored, _) = load_manager_from(connection, space, manager_id)?;
    let facts = request_facts_from(
        connection,
        policy.verified(),
        &stored.proof.source_body,
        &stored.proof.invocation.payload.audience,
    )?;
    let admitted = reconstruct_stored_admission(connection, policy, &stored.proof, &facts)?;
    let auth = policy
        .verified()
        .verify_admitted_operation_authorization(&admitted, &facts, now_ms)?;
    check_policy_heads_in(connection, policy.verified(), &auth, now_ms)?;
    check_manager_parent(connection, policy, &auth, &stored.proof.source_body, now_ms)?;
    Ok(auth)
}

fn manager_tool_authorization_from(
    connection: &Connection,
    policy: &PolicyState,
    space: &str,
    manager_id: &str,
    now_ms: u64,
) -> Result<VerifiedAuthorization> {
    let auth = manager_authorization_from(connection, policy, space, manager_id, now_ms)?;
    let (claim, stored, job) = load_manager_from(connection, space, manager_id)?;
    check_manager_attempt_in(connection, policy, space, &claim, &job, false)?;
    ensure!(
        matches!(job.state, JobState::Running { .. }),
        "manager is not running"
    );
    let session = crate::store::load_scheduler_session(
        connection,
        space,
        &claim.conversation_id,
        &claim.target_node,
    )?;
    ensure!(
        session.sdk_config.as_deref()
            == Some(manager_field(&stored.proof.source_body, "sdk_config")?),
        "manager execution configuration changed"
    );
    ensure!(
        now_ms < manager_lease_from(connection, policy, space, manager_id)?,
        "manager lease already expired"
    );
    Ok(auth)
}

pub(super) fn check_manager_tool_in(
    connection: &Connection,
    policy: &PolicyState,
    auth: &VerifiedAuthorization,
    transport: &TransportEvidence,
    now_ms: u64,
) -> Result<()> {
    let TransportEvidence::LocalAgentAdapter { service, session } = transport else {
        return Ok(());
    };
    let manager_id = active_manager_id(
        connection,
        auth.resource().space.as_str(),
        &session.session_id,
    )?;
    let manager = if auth.action() == Action::AgentStart {
        let (claim, _, job) =
            load_manager_from(connection, auth.resource().space.as_str(), &manager_id)?;
        ensure!(
            !claim.retired_before_start
                && (matches!(
                    job.state,
                    JobState::Running { .. } | JobState::Cancelling { .. }
                ) || (matches!(job.state, JobState::Unknown { .. })
                    && crate::store::scheduler_load::<crate::scheduling::SchedulerKnownResult>(
                        connection,
                        auth.resource().space.as_str(),
                        &scheduler_key(
                            "known-result",
                            &claim.conversation_id,
                            &claim.target_node,
                            &claim.trigger
                        )
                    )?
                    .is_some())),
            "manager output has no started attempt"
        );
        manager_authorization_from(
            connection,
            policy,
            auth.resource().space.as_str(),
            &manager_id,
            now_ms,
        )?
    } else {
        manager_tool_authorization_from(
            connection,
            policy,
            auth.resource().space.as_str(),
            &manager_id,
            now_ms,
        )?
    };
    ensure!(
        manager.actor() == &Actor::Service(service.clone())
            && manager.resource() == auth.resource()
            && manager
                .signed_invocation()
                .payload
                .context
                .sdk_session
                .as_ref()
                == Some(session)
            && manager.initiator() == auth.initiator(),
        "tool invocation differs from its admitted manager"
    );
    let manager_id = active_manager_id(
        connection,
        auth.resource().space.as_str(),
        &session.session_id,
    )?;
    let (claim, _, job) =
        load_manager_from(connection, auth.resource().space.as_str(), &manager_id)?;
    let write = !auth.action().is_read() && auth.action() != Action::AgentStart;
    if auth.action() != Action::AgentStart {
        check_manager_attempt_in(
            connection,
            policy,
            auth.resource().space.as_str(),
            &claim,
            &job,
            write,
        )?;
    }
    if let Some(task) = &manager.signed_invocation().payload.context.task {
        ensure!(
            auth.signed_invocation().payload.context.task.as_ref() == Some(task)
                && auth.action() != Action::TaskCreate,
            "continuation tool invocation escaped its original task"
        );
    }
    Ok(())
}

pub(crate) fn check_request_manager_tool_in(
    connection: &Connection,
    policy: &PolicyState,
    auth: &VerifiedAuthorization,
    transport: &RequestTransport,
    now_ms: u64,
) -> Result<()> {
    transport.check_current(auth, now_ms)?;
    match transport.existing() {
        Some(existing) => check_manager_tool_in(connection, policy, auth, existing, now_ms),
        None => {
            ensure!(
                matches!(auth.actor(), Actor::Account(_))
                    && auth
                        .signed_invocation()
                        .payload
                        .context
                        .sdk_session
                        .is_none(),
                "directed gateway cannot authorize a Manager service"
            );
            Ok(())
        }
    }
}

pub(super) fn check_stored_manager_tool_in(
    connection: &Connection,
    policy: &PolicyState,
    auth: &VerifiedAuthorization,
    transport: &StoredTransport,
    now_ms: u64,
) -> Result<()> {
    match transport {
        StoredTransport::DirectedGateway { .. } => {
            ensure!(
                matches!(auth.actor(), Actor::Account(_)),
                "directed gateway cannot authorize a Manager service"
            );
            Ok(())
        }
        _ => check_manager_tool_in(
            connection,
            policy,
            auth,
            &transport.existing_evidence()?,
            now_ms,
        ),
    }
}

pub(crate) fn sdk_policy_source_provenance_in(
    connection: &Connection,
    policy: &PolicyState,
    source: &vibeface_secure::authorization::PublicKey,
    service: &vibeface_secure::authorization::ServiceId,
    service_grant: &ObjectHash,
    facts: &RequestFacts,
    now_ms: u64,
) -> Result<vibeface_secure::authorization::node_execution_policy::SdkPolicySourceProvenance> {
    use vibeface_secure::authorization::{
        AUTH_VERSION, DeviceId, ServiceId,
        node_execution_policy::{
            SdkPolicyLease, SdkPolicySession, SdkPolicySourceFacts, SdkPolicySourceProvenance,
            sdk_policy_source_heads,
        },
    };
    let snapshot = if connection.is_autocommit() {
        Some(connection.unchecked_transaction()?)
    } else {
        None
    };
    let sdk_session = facts
        .context
        .sdk_session
        .as_ref()
        .context("SDK policy source has no SDK session")?;
    let descriptor: AgentSession = local_from(
        connection,
        facts.resource.space.as_str(),
        &format!("agent-session:{}", sdk_session.session_id),
    )?
    .context("SDK policy source has no persisted AgentSession")?;
    ensure!(
        descriptor.space_id == facts.resource.space
            && descriptor.resource_id == facts.resource.resource
            && descriptor.service == *service
            && *service == ServiceId::from_key(source)
            && descriptor.device == DeviceId::from_key(source)
            && descriptor.sdk_session == *sdk_session
            && sdk_session.target == descriptor.device
            && facts.context.conversation.as_deref() == Some(descriptor.conversation_id.as_str())
            && facts.audience.service == *service
            && facts.audience.device == descriptor.device
            && now_ms < descriptor.expires_at_ms,
        "SDK policy source differs from its persisted service session"
    );
    let manager_id = active_manager_id(
        connection,
        facts.resource.space.as_str(),
        &sdk_session.session_id,
    )?;
    let manager = manager_tool_authorization_from(
        connection,
        policy,
        facts.resource.space.as_str(),
        &manager_id,
        now_ms,
    )?;
    ensure!(
        manager.actor() == &Actor::Service(service.clone())
            && manager.resource() == &facts.resource
            && manager.initiator() == &descriptor.initiator,
        "SDK policy source differs from its current admitted Manager"
    );
    let (claim, _, job) =
        load_manager_from(connection, facts.resource.space.as_str(), &manager_id)?;
    check_manager_attempt_in(
        connection,
        policy,
        facts.resource.space.as_str(),
        &claim,
        &job,
        !facts.action.is_read() && facts.action != Action::AgentStart,
    )?;
    if let Some(task) = &manager.signed_invocation().payload.context.task {
        ensure!(
            facts.context.task.as_ref() == Some(task),
            "SDK policy source escaped its Manager task"
        );
    }
    let (_, stored, _) = load_manager_from(connection, facts.resource.space.as_str(), &manager_id)?;
    let lease: LeaseRecord = local_from(
        connection,
        facts.resource.space.as_str(),
        &format!("lease:{manager_id}"),
    )?
    .context("SDK policy source has no persisted Manager lease")?;
    ensure!(
        lease.checked_at_ms <= now_ms && now_ms < lease.deadline,
        "SDK policy source Manager lease is not current"
    );
    let events = super::services::retained_agent_events(
        connection,
        facts.resource.space.as_str(),
        &sdk_session.session_id,
    )?;
    let origin = super::services::resolve_current_agent_origin(
        policy,
        connection,
        &events,
        &sdk_session.session_id,
        now_ms,
    )?;
    ensure!(
        origin.parent.invocation_hash() == &descriptor.parent_invocation
            && origin.parent.actor() == &descriptor.initiator
            && origin.parent.resource() == &facts.resource
            && origin.parent.audience() == &facts.audience,
        "SDK policy source changed its current Account origin"
    );
    if let Some(request) = &origin.execution {
        validate_operation_binding_from(connection, request)?;
    }
    let compact = super::execution_handoff::compact_sdk_from_events(&events, &stored.proof)?
        .context("SDK policy source has no original Account message proof")?;
    ensure!(
        compact.invocation == *origin.parent.signed_invocation()
            && compact.sdk_trigger.as_str() == sdk_session.session_id,
        "SDK policy source changed its compact signed origin"
    );
    let parent_event = super::services::agent_account_message(&events, &sdk_session.session_id)?;
    let EventCause::Invocation { admission: parent } = &parent_event.event.cause else {
        anyhow::bail!("SDK policy source Account message has no original admission");
    };
    ensure!(
        parent.invocation == compact.invocation,
        "SDK policy source changed its original Account body provenance"
    );
    let origin_source_body = parent.source_body.clone();
    let task = facts
        .context
        .task
        .as_ref()
        .context("SDK policy source has no Task")?;
    let creation = events
        .get(task.as_str())
        .context("SDK policy source Task is missing")?;
    let EventBody::TaskCreated { task_id, .. } = &creation.event.body else {
        anyhow::bail!("SDK policy source does not identify a TaskCreated event");
    };
    ensure!(
        creation.status == "ready"
            && creation.event.network_id == facts.resource.space.as_str()
            && creation.event.resource_id == facts.resource.resource
            && creation.event.scope_id == descriptor.conversation_id
            && super::event_authorization::verify_event_envelope(
                connection,
                policy,
                facts.resource.space.as_str(),
                &creation.envelope,
            )? == creation.event,
        "SDK policy source Task changed its current resource or envelope"
    );
    super::event_authorization::validate_retained_event_dependencies_in(
        connection,
        &creation.envelope,
    )?;
    let current_binding = if let Some(hash) = local_from::<String>(
        connection,
        facts.resource.space.as_str(),
        &format!("task-binding-current:{}", task.as_str()),
    )? {
        let binding: crate::binding::SignedBinding = local_from(
            connection,
            facts.resource.space.as_str(),
            &format!("task-binding-record:{hash}"),
        )?
        .context("SDK policy source current binding record is missing")?;
        crate::binding::verify_binding(&binding, source.as_str())?;
        ensure!(
            crate::binding::binding_hash(&binding.binding)? == hash
                && binding.binding.network_id == facts.resource.space.as_str()
                && binding.binding.task_ref == task.as_str()
                && &binding.binding.task_id == task_id
                && binding.binding.conversation_id == descriptor.conversation_id,
            "SDK policy source changed its exact current Task binding"
        );
        Some(binding)
    } else {
        None
    };
    let source_heads = sdk_policy_source_heads(
        policy.verified(),
        &origin.parent,
        service,
        service_grant,
        facts,
        now_ms,
    )?;
    super::client_policy_read::check_heads_in(connection, policy, &source_heads)?;
    let provenance = SdkPolicySourceProvenance {
        version: AUTH_VERSION,
        origin: compact,
        origin_source_body,
        session: SdkPolicySession {
            space_id: descriptor.space_id,
            resource_id: descriptor.resource_id,
            service: descriptor.service,
            device: descriptor.device,
            conversation_id: descriptor.conversation_id,
            sdk_session: descriptor.sdk_session,
            grant_leaf: descriptor.grant_leaf,
            initiator: descriptor.initiator,
            initiating_grant: descriptor.initiating_grant,
            parent_invocation: descriptor.parent_invocation,
            expected_policies: descriptor.expected_policies,
            expires_at_ms: descriptor.expires_at_ms,
        },
        manager_admission: stored.proof,
        lease: SdkPolicyLease {
            challenge: lease.proof.challenge,
            receipts: lease.proof.receipts,
            checked_at_ms: lease.checked_at_ms,
            deadline: lease.deadline,
        },
        current_binding,
        child_facts: SdkPolicySourceFacts::from(facts),
        service_grant: service_grant.clone(),
        source_heads,
    };
    if let Some(snapshot) = snapshot {
        snapshot.commit()?;
    }
    Ok(provenance)
}

impl Store {
    pub(crate) fn active_manager_authorization_id(
        &self,
        space: &str,
        trigger: &str,
    ) -> Result<String> {
        active_manager_id(&self.connection, space, trigger)
    }

    pub(crate) fn existing_manager_admission(
        &mut self,
        space: &str,
        conversation: &str,
        trigger: &str,
        target: &str,
        boot: &str,
    ) -> Result<Option<ManagerAdmission>> {
        self.recover_scheduler_jobs(space, target, boot)?;
        let Some(job) = self.scheduler_job(space, conversation, trigger, target)? else {
            return Ok(None);
        };
        let Some(attempt) = job.state.attempt() else {
            return Ok(None);
        };
        if matches!(job.state, JobState::Queued) {
            return Ok(None);
        }
        let manager_id = format!("manager:{trigger}:{}", attempt.id);
        let deadline = manager_lease_from(&self.connection, &self.policy, space, &manager_id)?;
        Ok(Some(ManagerAdmission {
            manager_id,
            job,
            previous_native_id: None,
            fresh: false,
            lease_expires_at_ms: deadline,
        }))
    }

    pub(crate) fn claim_authorized_manager(
        &mut self,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        source_body: &Value,
        boot: &str,
        sdk_config: &str,
        now_ms: u64,
    ) -> Result<ManagerAdmission> {
        source_matches(auth, source_body)?;
        ensure!(
            !boot.is_empty()
                && boot.len() <= 1024
                && !sdk_config.is_empty()
                && manager_field(source_body, "sdk_config")? == sdk_config,
            "manager execution configuration differs from signed request"
        );
        ensure!(
            matches!(transport, TransportEvidence::LocalAgentAdapter { .. }),
            "manager start requires a local service adapter"
        );
        let facts = self.request_facts(source_body, auth.audience())?;
        let checked = self.policy.verified().verify_invocation(
            auth.signed_invocation(),
            &facts,
            transport,
            now_ms,
        )?;
        ensure!(
            checked.invocation_hash() == auth.invocation_hash(),
            "manager invocation changed"
        );
        let freshness = self.policy.verified().verify_freshness(
            auth,
            &proof.challenge,
            &proof.receipts,
            now_ms,
        )?;
        self.policy
            .verified()
            .admit_operation(auth, &freshness, now_ms)?;
        let space = auth.resource().space.as_str();
        let conversation = manager_field(source_body, "conversation_id")?;
        let trigger = manager_field(source_body, "trigger")?;
        let target = manager_field(source_body, "scheduler_node")?;
        let attempt_id = manager_field(source_body, "attempt_id")?;
        let target_key: String = self.connection.query_row(
            "SELECT public_key FROM node_certificates WHERE space_id=? AND node_id=?",
            params![space, target],
            |row| row.get(0),
        )?;
        ensure!(
            vibeface_secure::authorization::DeviceId::from_key(
                &vibeface_secure::authorization::PublicKey::parse(&target_key)?
            ) == *auth.device(),
            "scheduler queue target differs from its admitted service"
        );
        let manager_key = format!("manager-admission:{}", auth.operation_id());
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_policy_heads_in(&tx, self.policy.verified(), auth, now_ms)?;
        check_manager_parent(&tx, &self.policy, auth, source_body, now_ms)?;
        if local_from::<ManagerClaim>(&tx, space, &manager_key)?.is_some() {
            let (_, stored, job) = load_manager_from(&tx, space, auth.operation_id())?;
            ensure!(
                stored.proof.source_body == *source_body
                    && stored.transport == StoredTransport::from(transport),
                "manager attempt was reused with a different request"
            );
            let deadline = manager_lease_from(&tx, &self.policy, space, auth.operation_id())?;
            tx.commit()?;
            return Ok(ManagerAdmission {
                manager_id: auth.operation_id().into(),
                job,
                previous_native_id: None,
                fresh: false,
                lease_expires_at_ms: deadline,
            });
        }
        if let Some(previous_id) =
            local_from::<String>(&tx, space, &format!("manager-active:{trigger}"))?
        {
            let mut previous: ManagerClaim =
                local_from(&tx, space, &format!("manager-admission:{previous_id}"))?
                    .context("previous manager claim missing")?;
            let previous_job: SchedulerJob = crate::store::scheduler_load(
                &tx,
                space,
                &scheduler_key("job", conversation, target, trigger),
            )?
            .context("previous job missing")?;
            ensure!(
                matches!(previous_job.state, JobState::Queued),
                "previous manager attempt requires reconciliation"
            );
            previous.retired_before_start = true;
            set_local_in(
                &tx,
                space,
                &format!("manager-admission:{previous_id}"),
                &previous,
            )?;
        }
        let claimed = crate::store::claim_next_in(
            &tx,
            &self.policy,
            space,
            conversation,
            target,
            boot,
            sdk_config,
            attempt_id,
            trigger,
        )?
        .context(scheduler::ManagerBusy)?;
        ensure!(
            claimed.job.trigger == trigger && claimed.job.resource_id == auth.resource().resource,
            "scheduler queue selected another trigger or resource"
        );
        let attempt = claimed
            .job
            .state
            .attempt()
            .context("claimed job has no attempt")?
            .clone();
        let admitted = consume_authorization_in(
            &tx,
            &self.policy,
            auth,
            proof,
            transport,
            source_body,
            None,
            now_ms,
        )?;
        set_local_in(
            &tx,
            space,
            &manager_key,
            &ManagerClaim {
                manager_id: auth.operation_id().into(),
                conversation_id: conversation.into(),
                trigger: trigger.into(),
                target_node: target.into(),
                attempt,
                retired_before_start: false,
            },
        )?;
        set_local_in(
            &tx,
            space,
            &format!("manager-active:{trigger}"),
            &auth.operation_id(),
        )?;
        let deadline = freshness.expires_at_ms().min(auth.expires_at_ms());
        save_lease_in(&tx, space, auth.operation_id(), proof, now_ms, deadline)?;
        let checkpoint = prepare_local_admission_checkpoint_in(&tx, &self.policy, &admitted)?;
        tx.commit()?;
        self.policy.local_admissions.insert(checkpoint);
        Ok(ManagerAdmission {
            manager_id: auth.operation_id().into(),
            job: claimed.job,
            previous_native_id: claimed.previous_native_id,
            fresh: true,
            lease_expires_at_ms: deadline,
        })
    }

    pub(crate) fn manager_authorization(
        &self,
        space: &str,
        manager_id: &str,
        now_ms: u64,
    ) -> Result<VerifiedAuthorization> {
        manager_authorization_from(&self.connection, &self.policy, space, manager_id, now_ms)
    }

    pub(crate) fn manager_tool_authorization(
        &self,
        space: &str,
        manager_id: &str,
        now_ms: u64,
    ) -> Result<VerifiedAuthorization> {
        manager_tool_authorization_from(&self.connection, &self.policy, space, manager_id, now_ms)
    }

    pub(crate) fn renew_authorized_manager(
        &mut self,
        space: &str,
        manager_id: &str,
        proof: &FreshnessProof,
        now_ms: u64,
    ) -> Result<u64> {
        let auth = self.manager_authorization(space, manager_id, now_ms)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (claim, stored, job) = load_manager_from(&tx, space, manager_id)?;
        ensure!(
            matches!(job.state, JobState::Running { .. })
                && stored.proof.invocation == *auth.signed_invocation(),
            "only the original running manager can renew its lease"
        );
        check_manager_attempt_in(&tx, &self.policy, space, &claim, &job, false)?;
        ensure!(
            now_ms < manager_lease_from(&tx, &self.policy, space, manager_id)?,
            "manager lease already expired"
        );
        check_manager_parent(&tx, &self.policy, &auth, &stored.proof.source_body, now_ms)?;
        let deadline = consume_challenge_in(
            &tx,
            self.policy.verified(),
            &auth,
            proof,
            CheckStage::Renew,
            now_ms,
        )?;
        save_lease_in(&tx, space, manager_id, proof, now_ms, deadline)?;
        tx.commit()?;
        Ok(deadline)
    }

    pub fn authorize_read(
        &self,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<()> {
        self.authorize_request_read(
            auth,
            proof,
            &RequestTransport::Existing(transport.clone()),
            now_ms,
        )
    }

    pub(crate) fn authorize_request_read(
        &self,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &RequestTransport,
        now_ms: u64,
    ) -> Result<()> {
        ensure!(
            proof.challenge.stage == CheckStage::RemoteRead && auth.action().is_read(),
            "read requires remote-read freshness"
        );
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        check_request_manager_tool_in(&tx, &self.policy, auth, transport, now_ms)?;
        check_policy_heads_in(&tx, self.policy.verified(), auth, now_ms)?;
        self.policy
            .verified()
            .verify_freshness(auth, &proof.challenge, &proof.receipts, now_ms)?;
        check_challenge_record(&tx, auth, proof, false)?;
        tx.commit()?;
        Ok(())
    }

    pub fn accept_authorized_operation(
        &mut self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        source_body: &Value,
        request: &OperationRequest,
        now_ms: u64,
    ) -> Result<OperationRecord> {
        self.accept_request_authorized_operation(
            identity,
            auth,
            proof,
            &RequestTransport::Existing(transport.clone()),
            source_body,
            request,
            now_ms,
        )
    }

    pub(crate) fn accept_request_authorized_operation(
        &mut self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &RequestTransport,
        source_body: &Value,
        request: &OperationRequest,
        now_ms: u64,
    ) -> Result<OperationRecord> {
        source_matches(auth, source_body)?;
        ensure!(
            source_body["method"] == "operate"
                && serde_json::from_value::<OperationRequest>(source_body["request"].clone())?
                    == *request,
            "operation differs from signed request"
        );
        ensure!(
            request.network_id == auth.resource().space.as_str(),
            "operation space mismatch"
        );
        ensure!(
            !request.operation_id.is_empty()
                && request
                    .session_id
                    .as_ref()
                    .is_none_or(|session| !session.is_empty()),
            "operation and supplied session IDs must be nonempty"
        );
        ensure!(
            request.expected_revision < i64::MAX as u64,
            "revision overflow"
        );
        ensure!(
            request.resources.len() <= 128,
            "too many resource reservations"
        );
        ensure!(
            serde_json::to_vec(request)?.len() <= MAX_EVENT_BYTES,
            "operation too large"
        );
        self.validate_operation_binding(request)?;
        let facts = self.request_facts(source_body, auth.audience())?;
        let checked = super::policy::authorize_request_invocation_for_body(
            self.policy.verified(),
            auth.signed_invocation(),
            &facts,
            transport,
            source_body,
            now_ms,
        )?;
        ensure!(
            checked.invocation_hash() == auth.invocation_hash(),
            "operation authorization mismatch"
        );
        let action = request.action["type"]
            .as_str()
            .context("operation action is required")?;
        let control = !matches!(action, "open" | "send" | "command");
        let command = if action == "command" {
            ensure!(
                request.session_id.is_none()
                    && request.task_id.is_none()
                    && request.binding.is_none()
                    && request.expected_revision == 0,
                "command cannot carry a Task, session, binding or revision"
            );
            let mut fields = request.action.clone();
            fields
                .as_object_mut()
                .context("invalid command action")?
                .remove("type");
            let command: crate::command::CommandSpec = serde_json::from_value(fields)?;
            command.validate()?;
            let cwd = command
                .cwd
                .to_str()
                .context("command directory must be UTF-8")?;
            ensure!(
                canonical_resource(cwd)? == cwd && command.cwd.is_dir(),
                "command directory is not canonical"
            );
            Some(command)
        } else {
            None
        };
        if !matches!(action, "open" | "command") {
            serde_json::from_value::<EngineAction>(request.action.clone())?.validate()?;
        }
        let resources = request
            .resources
            .iter()
            .map(|resource| canonical_resource(resource))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            resources == request.resources && resources.windows(2).all(|pair| pair[0] < pair[1]),
            "operation resources are not canonical and uniquely ordered"
        );
        ensure!(
            !control || resources.is_empty(),
            "control operations cannot reserve resources"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_policy_heads_in(&tx, self.policy.verified(), auth, now_ms)?;
        if let Some(old) = operation_from(&tx, &request.network_id, &request.operation_id)? {
            let stored = load_admission_from(&tx, &request.network_id, &request.operation_id)?;
            ensure!(
                old.request == *request
                    && stored.proof.invocation == *auth.signed_invocation()
                    && stored.proof.source_body == *source_body,
                "operation ID reused with different authorization or body"
            );
            let freshness = self.policy.verified().verify_freshness(
                auth,
                &proof.challenge,
                &proof.receipts,
                now_ms,
            )?;
            self.policy
                .verified()
                .admit_operation(auth, &freshness, now_ms)?;
            if stored.proof.challenge != proof.challenge {
                consume_challenge_in(
                    &tx,
                    self.policy.verified(),
                    auth,
                    proof,
                    CheckStage::Admit,
                    now_ms,
                )?;
            }
            tx.commit()?;
            return Ok(old);
        }
        validate_basis_in(&tx, request, &auth.resource().resource)?;
        let session = checked_session_from(&tx, request)?;
        let session = if action == "command" {
            ensure!(session.is_none(), "command cannot use an execution session");
            None
        } else if action == "open" {
            ensure!(session.is_none(), "execution session already exists");
            let fields = request.action.as_object().context("invalid open action")?;
            ensure!(
                fields.len() == 3
                    && fields
                        .keys()
                        .all(|key| matches!(key.as_str(), "type" | "executor" | "cwd")),
                "unexpected open action field"
            );
            let executor: ExecutorKind =
                serde_json::from_value(request.action["executor"].clone())?;
            let cwd = request.action["cwd"]
                .as_str()
                .context("execution directory is required")?;
            ensure!(
                canonical_resource(cwd)? == cwd && std::path::Path::new(cwd).is_dir(),
                "execution directory is not canonical"
            );
            Some(ExecutionSession {
                network_id: request.network_id.clone(),
                target_node: request.target_node.clone(),
                session_id: request
                    .session_id
                    .clone()
                    .context("open requires a session")?,
                revision: 0,
                state: "opening".into(),
                executor: serde_json::to_value(executor)?
                    .as_str()
                    .context("invalid executor")?
                    .into(),
                cwd: cwd.into(),
                native_session_id: None,
            })
        } else {
            Some(session.context("execution session is not open")?)
        };
        if !control {
            let cwd = match (&session, &command) {
                (Some(session), None) => session.cwd.as_str(),
                (None, Some(command)) => command
                    .cwd
                    .to_str()
                    .context("command directory must be UTF-8")?,
                _ => anyhow::bail!("operation has no execution directory"),
            };
            ensure!(
                resources.iter().any(|resource| resource == cwd),
                "execution directory is not reserved"
            );
        }
        if let Some(session_id) = request.session_id.as_deref() {
            crate::store::check_execution_session_in(
                &tx,
                &request.network_id,
                &request.target_node,
                session_id,
                control,
            )?;
        }
        crate::store::check_execution_resources_in(&tx, &request.target_node, &resources)?;
        let admission = consume_request_authorization_in(
            &tx,
            &self.policy,
            auth,
            proof,
            transport,
            source_body,
            Some(&request.operation_id),
            now_ms,
        )?;
        tx.execute(
            "INSERT INTO operations(space_id,operation_id,target_node,session_id,request,state,control,result) VALUES(?,?,?,?,?,'\"accepted\"',?,'null')",
            params![request.network_id, request.operation_id, request.target_node, request.session_id, serde_json::to_string(request)?, control],
        )?;
        for resource in &resources {
            tx.execute("INSERT INTO resource_reservations(space_id,operation_id,target_node,resource) VALUES(?,?,?,?)",
                params![request.network_id, request.operation_id, request.target_node, resource])?;
        }
        if action == "open" {
            let session = session.as_ref().context("open session is missing")?;
            tx.execute("INSERT INTO execution_sessions(space_id,target_node,session_id,revision,record) VALUES(?,?,?,?,?)",
                params![request.network_id, request.target_node, request.session_id, session.revision, serde_json::to_string(&session)?])?;
        }
        append_admitted_dispatch_in(&tx, &self.policy, identity, &admission, request, now_ms)?;
        let checkpoint = prepare_local_admission_checkpoint_in(&tx, &self.policy, &admission)?;
        tx.commit()?;
        self.policy.local_admissions.insert(checkpoint);
        Ok(OperationRecord {
            request: request.clone(),
            state: OperationState::Accepted,
            result: Value::Null,
        })
    }

    pub fn operation_authorization(
        &self,
        space: &str,
        operation_id: &str,
        now_ms: u64,
    ) -> Result<VerifiedAuthorization> {
        let stored = load_admission_from(&self.connection, space, operation_id)?;
        let operation =
            operation_from(&self.connection, space, operation_id)?.context("unknown operation")?;
        ensure!(
            serde_json::from_value::<OperationRequest>(
                stored.proof.source_body["request"].clone()
            )? == operation.request,
            "persisted operation differs from admitted request"
        );
        let facts = self.request_facts(
            &stored.proof.source_body,
            &stored.proof.invocation.payload.audience,
        )?;
        let admission =
            reconstruct_stored_admission(&self.connection, &self.policy, &stored.proof, &facts)?;
        Ok(self
            .policy
            .verified()
            .verify_admitted_operation_authorization(&admission, &facts, now_ms)?)
    }

    pub fn operation_admission_proof(
        &self,
        space: &str,
        operation_id: &str,
    ) -> Result<AdmissionProof> {
        let stored = load_admission_from(&self.connection, space, operation_id)?;
        let operation =
            operation_from(&self.connection, space, operation_id)?.context("unknown operation")?;
        ensure!(
            serde_json::from_value::<OperationRequest>(
                stored.proof.source_body["request"].clone()
            )? == operation.request,
            "persisted operation differs from admitted request"
        );
        Ok(stored.proof)
    }

    pub fn claim_authorized_operation(
        &mut self,
        space: &str,
        operation_id: &str,
        proof: &FreshnessProof,
        now_ms: u64,
    ) -> Result<(OperationRecord, u64)> {
        let auth = self.operation_authorization(space, operation_id, now_ms)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut operation =
            operation_from(&tx, space, operation_id)?.context("unknown operation")?;
        ensure!(
            operation.state == OperationState::Accepted,
            "operation is not accepted or was already claimed"
        );
        check_operation_authorization_in(&tx, self.policy.verified(), &auth, &operation)?;
        validate_basis_in(&tx, &operation.request, &auth.resource().resource)?;
        validate_operation_binding_from(&tx, &operation.request)?;
        if let Some(session) = checked_session_from(&tx, &operation.request)? {
            ensure!(
                canonical_resource(&session.cwd)? == session.cwd,
                "execution directory changed before claim"
            );
        } else {
            ensure!(
                operation.request.session_id.is_none()
                    && operation.request.action["type"] == "command",
                "accepted operation has no execution session"
            );
        }
        for resource in &operation.request.resources {
            ensure!(
                canonical_resource(resource)? == *resource,
                "reserved path changed before claim"
            );
        }
        let deadline = consume_challenge_in(
            &tx,
            self.policy.verified(),
            &auth,
            proof,
            CheckStage::Start,
            now_ms,
        )?;
        ensure!(tx.execute("UPDATE operations SET state='\"running\"' WHERE space_id=? AND operation_id=? AND state='\"accepted\"'", params![space, operation_id])? == 1,
            "operation was already claimed");
        save_lease_in(&tx, space, operation_id, proof, now_ms, deadline)?;
        tx.commit()?;
        operation.state = OperationState::Running;
        Ok((operation, deadline))
    }

    pub fn renew_authorized_operation(
        &mut self,
        space: &str,
        operation_id: &str,
        proof: &FreshnessProof,
        now_ms: u64,
    ) -> Result<u64> {
        let auth = self.operation_authorization(space, operation_id, now_ms)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let operation = operation_from(&tx, space, operation_id)?.context("unknown operation")?;
        ensure!(
            operation.state == OperationState::Running,
            "only a running operation can renew its lease"
        );
        check_operation_authorization_in(&tx, self.policy.verified(), &auth, &operation)?;
        ensure!(
            now_ms < lease_deadline_from(&tx, &self.policy, space, operation_id)?,
            "execution lease already expired"
        );
        validate_operation_binding_from(&tx, &operation.request)?;
        let deadline = consume_challenge_in(
            &tx,
            self.policy.verified(),
            &auth,
            proof,
            CheckStage::Renew,
            now_ms,
        )?;
        save_lease_in(&tx, space, operation_id, proof, now_ms, deadline)?;
        tx.commit()?;
        Ok(deadline)
    }

    pub fn operation_lease_deadline(&self, space: &str, operation_id: &str) -> Result<u64> {
        let operation =
            operation_from(&self.connection, space, operation_id)?.context("unknown operation")?;
        ensure!(
            operation.state == OperationState::Running,
            "operation is not running"
        );
        lease_deadline_from(&self.connection, &self.policy, space, operation_id)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use anyhow::Result;
    use serde_json::json;
    use vibeface_secure::{Identity, authorization::*};

    const NOW: u64 = 1_000;

    pub(crate) struct Fixture {
        pub(crate) connection: Connection,
        pub(crate) state: VerifiedPolicySet,
        pub(crate) policy: PolicyState,
        pub(crate) authority: Identity,
        pub(crate) node: LocalIdentity,
        pub(crate) auth: VerifiedAuthorization,
        pub(crate) transport: TransportEvidence,
        pub(crate) body: Value,
    }

    impl Fixture {
        pub(crate) fn new() -> Result<Self> {
            let key = |byte| {
                Identity::from_secret_hex(&hex::encode([byte; 32])).map_err(anyhow::Error::msg)
            };
            let root = key(1)?;
            let recovery = key(2)?;
            let authority = key(3)?;
            let user = key(4)?;
            let node = key(5)?;
            let account = AccountId::from_root(&PublicKey::of(&root));
            let space = SpaceId::personal(&account);
            let audience = Audience {
                service: ServiceId::from_key(&PublicKey::of(&node)),
                device: DeviceId::from_key(&PublicKey::of(&node)),
            };
            let resource = ResourceRef {
                space: space.clone(),
                resource: ResourceId::from_bytes([10; 32]),
            };
            let recovery = RecoveryPolicy {
                generation: 0,
                keys: vec![PublicKey::of(&recovery)],
                threshold: 1,
            };
            let mut state = VerifiedPolicySet::new();
            let account_genesis = state.trust_account_genesis(
                &PublicKey::of(&root),
                Signed::sign(
                    AccountDocument {
                        version: AUTH_VERSION,
                        account: account.clone(),
                        genesis_root: PublicKey::of(&root),
                        current_root: PublicKey::of(&root),
                        root_generation: 0,
                        previous: None,
                        authority_epoch: 0,
                        sequence: 0,
                        authority: AuthorityBinding::from_key(PublicKey::of(&authority)),
                        recovery: recovery.clone(),
                        enrollment: vec![],
                        devices: vec![],
                        revoked_devices: vec![],
                        authenticators: vec![],
                        authenticator_approval: None,
                        device_enrollment: None,
                        root_acceptance: None,
                        recovery_approvals: vec![],
                    },
                    &root,
                )?,
            )?;
            let mut devices = Vec::new();
            for (identity, subject, uses) in [
                (
                    &user,
                    Actor::Account(account.clone()),
                    vec![DeviceUse::Noise, DeviceUse::AccountInvocation],
                ),
                (
                    &node,
                    Actor::Service(audience.service.clone()),
                    vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
                ),
            ] {
                let mut certificate = DeviceCertificate {
                    version: AUTH_VERSION,
                    owner: PolicyDomain::Account(account.clone()),
                    subject,
                    device: DeviceId::from_key(&PublicKey::of(identity)),
                    public_key: PublicKey::of(identity),
                    uses,
                    possession: None,
                    enrollment_grant: None,
                    issued_under: account_genesis.clone(),
                    nonce: Nonce::random()?,
                    not_before_ms: 0,
                    expires_at_ms: 1_000_000,
                };
                certificate.prove_possession(identity)?;
                devices.push(state.install_device(Signed::sign(certificate, &root)?, NOW)?);
            }
            devices.sort();
            let mut account_document = state.account(&account).unwrap().clone();
            account_document.previous = Some(account_genesis);
            account_document.sequence = 1;
            account_document.devices = devices;
            state.install_account(Signed::sign(account_document, &root)?)?;
            let genesis = state.trust_space_genesis(
                &PublicKey::of(&root),
                Signed::sign(
                    SpaceDocument {
                        version: AUTH_VERSION,
                        space: space.clone(),
                        root: SpaceRoot::Personal {
                            owner: account.clone(),
                            genesis_root: PublicKey::of(&root),
                        },
                        current_root: PublicKey::of(&root),
                        root_generation: 0,
                        previous: None,
                        authority_epoch: 0,
                        sequence: 0,
                        authority: AuthorityBinding::from_key(PublicKey::of(&authority)),
                        recovery,
                        devices: vec![],
                        revoked_devices: vec![],
                        memberships: vec![],
                        grants: vec![],
                        resources: vec![],
                        governance_grant: None,
                        root_acceptance: None,
                        recovery_approvals: vec![],
                    },
                    &root,
                )?,
            )?;
            let resource_hash = state.install_resource(Signed::sign(
                ResourceRecord {
                    version: AUTH_VERSION,
                    space: space.clone(),
                    resource: resource.resource.clone(),
                    kind: ResourceKind::Project,
                    host: audience.service.clone(),
                    issued_under: genesis.clone(),
                    nonce: Nonce::random()?,
                },
                &root,
            )?)?;
            let grant_hash = state.install_grant(
                Signed::sign(
                    Grant {
                        execution_source: None,
                        version: AUTH_VERSION,
                        owner_space: space.clone(),
                        scope: ResourceScope::Exact(resource.clone()),
                        grantee: Grantee::Actor(Actor::Account(account.clone())),
                        actions: vec![Action::ConversationRead, Action::ConversationCreate],
                        audience: audience.clone(),
                        constraints: GrantConstraints::default(),
                        parent: None,
                        remaining_delegations: 0,
                        issued_under: genesis.clone(),
                        not_before_ms: 0,
                        expires_at_ms: 1_000_000,
                        nonce: Nonce::random()?,
                    },
                    &root,
                )?,
                NOW,
            )?;
            let mut document = state.space(&space).unwrap().clone();
            document.previous = Some(genesis);
            document.sequence = 1;
            document.resources = vec![resource_hash];
            document.grants = vec![grant_hash.clone()];
            state.install_space(Signed::sign(document, &root)?)?;
            let account_history = state.account_history(&account).unwrap();
            let space_history = state.space_history(&space).unwrap();
            let mut journal = vec![JournalEntry::AccountGenesis(account_history[0].clone())];
            journal.extend(
                account_history[1]
                    .payload
                    .devices
                    .iter()
                    .map(|hash| JournalEntry::Device(state.device(hash).unwrap().clone())),
            );
            journal.push(JournalEntry::Account(account_history[1].clone()));
            journal.push(JournalEntry::SpaceGenesis(space_history[0].clone()));
            journal.push(JournalEntry::Resource(
                state.resource(&space, &resource.resource).unwrap().clone(),
            ));
            journal.push(JournalEntry::Grant(
                state.grant(&grant_hash).unwrap().clone(),
            ));
            journal.push(JournalEntry::Space(space_history[1].clone()));
            let pins = vec![
                PolicyPin {
                    domain: PolicyDomain::Account(account.clone()),
                    genesis_root: PublicKey::of(&root),
                    minimum_head: None,
                },
                PolicyPin {
                    domain: PolicyDomain::Space(space.clone()),
                    genesis_root: PublicKey::of(&root),
                    minimum_head: None,
                },
            ];
            let body = json!({"method":"create_conversation", "space_id":space, "resource_id":resource.resource,
                "conversation_id":"chat", "title":"test", "authorization_id":"write_1"});
            let mut connection = Connection::open_in_memory()?;
            connection.execute_batch(include_str!("../../migrations/202610180001_initial.sql"))?;
            connection.pragma_update(None, "foreign_keys", true)?;
            connection.execute(
                "INSERT INTO spaces(space_id,revision,bundle) VALUES(?,1,'{}')",
                [space.as_str()],
            )?;
            let prepared = PolicyState::default().prepare_bootstrap(&pins, &journal, NOW)?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let policy = crate::identity::journal::commit_in(&tx, &prepared)?;
            tx.commit()?;
            let node_identity = LocalIdentity {
                secret_key_hex: node.secret_hex(),
            };
            connection.execute(
                "INSERT INTO node_certificates(space_id,node_id,public_key) VALUES(?,?,?)",
                params![
                    space.as_str(),
                    node_identity.node_id()?,
                    node_identity.public_key_hex()?
                ],
            )?;
            let record = state.resource(&space, &resource.resource).unwrap();
            connection.execute("INSERT INTO resource_records(space_id,resource_id,record_hash,kind,host_service_id,signed_record) VALUES(?,?,?,'project',?,?)",
                params![space.as_str(), resource.resource.as_str(), record.hash()?.as_str(), record.payload.host.as_str(), serde_json::to_string(record)?])?;
            let facts = request_facts_from(&connection, &state, &body, &audience)?;
            let expected_policies = vec![
                state
                    .head(&PolicyDomain::Account(account.clone()))
                    .unwrap()
                    .clone(),
                state.head(&PolicyDomain::Space(space)).unwrap().clone(),
            ];
            let device = DeviceId::from_key(&PublicKey::of(&user));
            let signed = Signed::sign(
                Invocation {
                    version: AUTH_VERSION,
                    actor: Actor::Account(account.clone()),
                    device: device.clone(),
                    initiator: Actor::Account(account),
                    initiating_grant: None,
                    resource,
                    action: facts.action,
                    audience,
                    presenter: device.clone(),
                    operation_id: facts.operation_id.clone(),
                    body_hash: facts.body_hash.clone(),
                    grant_leaf: grant_hash,
                    expected_policies,
                    context: facts.context.clone(),
                    nonce: Nonce::random()?,
                    not_before_ms: 0,
                    expires_at_ms: 90_000,
                },
                &user,
            )?;
            let transport = TransportEvidence::DirectNoise { device };
            let auth = state.verify_invocation(&signed, &facts, &transport, NOW)?;
            Ok(Self {
                connection,
                state,
                policy,
                authority,
                node: node_identity,
                auth,
                transport,
                body,
            })
        }

        pub(crate) fn proof(&self) -> Result<FreshnessProof> {
            let challenge = vibeface_secure::authorization::FreshnessChallenge::new(
                &self.auth,
                CheckStage::Admit,
                Nonce::random()?,
                NOW,
                NOW + 10_000,
            )?;
            self.connection.execute("INSERT INTO authorization_challenges(space_id,nonce,invocation_hash,stage,challenge,expires_at_ms) VALUES(?,?,?,'admit',?,?)",
                params![self.auth.resource().space.as_str(), challenge.nonce.as_str(), self.auth.invocation_hash().as_str(), serde_json::to_string(&challenge)?, challenge.expires_at_ms])?;
            let receipts = vec![Signed::sign(
                FreshnessReceipt {
                    version: AUTH_VERSION,
                    currents: self.auth.dependencies().to_vec(),
                    challenge_hash: challenge.hash()?,
                    issued_at_ms: NOW,
                    expires_at_ms: challenge.expires_at_ms,
                },
                &self.authority,
            )?];
            Ok(FreshnessProof {
                challenge,
                receipts,
            })
        }
    }

    fn local_cache_fixture(
        persistent: bool,
    ) -> Result<(
        crate::identity::test_support::TestNetwork,
        Store,
        crate::model::SignedEvent,
        crate::model::SignedEvent,
        AdmissionProof,
        RequestFacts,
    )> {
        use crate::identity::test_support::TestNetwork;
        let network = TestNetwork::new()?;
        let mut store = if persistent {
            network.at(&network.directory.join("admission-cache.sqlite3"))?
        } else {
            network.memory()?
        };
        let ancestor = network.conversation(&mut store, "conversation")?;
        let anchor = network.message(&mut store, "conversation", "cached admission")?;
        let event: crate::model::Event = serde_json::from_str(&anchor.canonical_body)?;
        assert!(event.parents.contains(&ancestor.hash));
        let EventCause::Invocation { admission } = event.cause else {
            anyhow::bail!("expected an admitted message");
        };
        let facts = persisted_admission_facts(&store.connection, &store.policy, &admission)?;
        assert!(store.policy.local_admissions.entries.contains_key(&(
            network.space.to_string(),
            admission.invocation.payload.operation_id.clone(),
        )));
        Ok((network, store, ancestor, anchor, admission, facts))
    }

    pub(super) fn cached_without_crypto(
        store: &Store,
        proof: &AdmissionProof,
        facts: &RequestFacts,
    ) -> Result<VerifiedAdmission> {
        let signatures = vibeface_secure::signature_verification::SignatureVerification::new();
        let admitted = signatures.run(|| {
            reconstruct_admission(
                &store.connection,
                &store.policy,
                proof,
                facts,
                &proof.transport.clone().existing_evidence()?,
            )
        })?;
        assert_eq!(signatures.counts().actual_verifications, 0);
        assert_eq!(admitted.signed_invocation(), &proof.invocation);
        assert_eq!(admitted.admitted_at_ms(), proof.admitted_at_ms);
        Ok(admitted)
    }

    #[test]
    fn local_admission_cache_reuses_committed_proof_and_rebuilds_on_reopen() -> Result<()> {
        let (network, store, _, _, proof, facts) = local_cache_fixture(true)?;
        let signatures = vibeface_secure::signature_verification::SignatureVerification::new();
        signatures.run(|| {
            reconstruct_admission_uncached(
                &store.connection,
                &store.policy,
                &proof,
                &facts,
                &proof.transport.clone().existing_evidence()?,
            )
        })?;
        assert!(signatures.counts().actual_verifications > 0);
        cached_without_crypto(&store, &proof, &facts)?;
        cached_without_crypto(&store, &proof, &facts)?;
        let cache_len = store.policy.local_admissions.len();
        assert_eq!(
            verified_admission_checkpoints_from(&store.connection, &store.policy)?.len(),
            cache_len
        );
        drop(store);
        let reopened = Store::open(network.directory.join("admission-cache.sqlite3"))?;
        assert_eq!(reopened.policy.local_admissions.len(), cache_len);
        cached_without_crypto(&reopened, &proof, &facts)?;
        Ok(())
    }

    #[test]
    fn local_admission_cache_rejects_changed_rows_proof_facts_and_ready_ancestors() -> Result<()> {
        let (network, store, ancestor, anchor, proof, facts) = local_cache_fixture(false)?;
        let id = &proof.invocation.payload.operation_id;
        for (sql, parameter) in [
            (
                "UPDATE accepted_authorizations SET actor_id='changed' WHERE authorization_id=?",
                id.as_str(),
            ),
            (
                "UPDATE authorization_nonces SET body_hash='changed' WHERE authorization_id=?",
                id.as_str(),
            ),
            (
                "UPDATE authorization_challenges SET consumed=0 WHERE nonce=?",
                proof.challenge.nonce.as_str(),
            ),
            (
                "UPDATE authorization_challenges SET challenge='{}' WHERE nonce=?",
                proof.challenge.nonce.as_str(),
            ),
            (
                "UPDATE events SET resource_id='changed' WHERE hash=?",
                anchor.hash.as_str(),
            ),
            (
                "UPDATE events SET scope_id='changed' WHERE hash=?",
                anchor.hash.as_str(),
            ),
            (
                "UPDATE events SET status='conflict' WHERE hash=?",
                anchor.hash.as_str(),
            ),
            (
                "UPDATE events SET status='conflict' WHERE hash=?",
                ancestor.hash.as_str(),
            ),
        ] {
            let tx = store.connection.unchecked_transaction()?;
            tx.pragma_update(None, "defer_foreign_keys", true)?;
            assert_eq!(tx.execute(sql, [parameter])?, 1);
            assert!(
                reconstruct_admission(
                    &tx,
                    &store.policy,
                    &proof,
                    &facts,
                    &proof.transport.clone().existing_evidence()?
                )
                .is_err(),
                "{sql}"
            );
            if sql == "UPDATE events SET status='conflict' WHERE hash=?" {
                let checkpoints = verified_admission_checkpoints_from(&tx, &store.policy)?;
                assert!(
                    !checkpoints
                        .entries
                        .contains_key(&(network.space.to_string(), id.clone()))
                );
            } else {
                assert!(
                    verified_admission_checkpoints_from(&tx, &store.policy).is_err(),
                    "{sql}"
                );
            }
            tx.rollback()?;
            cached_without_crypto(&store, &proof, &facts)?;
        }
        for original in [&anchor, &ancestor] {
            let mut changed = original.clone();
            changed.canonical_body.push(' ');
            let tx = store.connection.unchecked_transaction()?;
            assert_eq!(
                tx.execute(
                    "UPDATE events SET envelope=? WHERE hash=?",
                    params![serde_json::to_string(&changed)?, original.hash]
                )?,
                1
            );
            assert!(
                reconstruct_admission(
                    &tx,
                    &store.policy,
                    &proof,
                    &facts,
                    &proof.transport.clone().existing_evidence()?
                )
                .is_err()
            );
            tx.rollback()?;
            cached_without_crypto(&store, &proof, &facts)?;
        }
        let mut event: crate::model::Event = serde_json::from_str(&anchor.canonical_body)?;
        let EventCause::Invocation { admission } = &mut event.cause else {
            unreachable!()
        };
        admission
            .invocation
            .payload
            .operation_id
            .push_str("-changed");
        let mut changed_anchor = anchor.clone();
        changed_anchor.canonical_body = serde_json::to_string(&event)?;
        let tx = store.connection.unchecked_transaction()?;
        assert_eq!(
            tx.execute(
                "UPDATE events SET envelope=? WHERE hash=?",
                params![serde_json::to_string(&changed_anchor)?, anchor.hash]
            )?,
            1
        );
        assert!(
            reconstruct_admission(
                &tx,
                &store.policy,
                &proof,
                &facts,
                &proof.transport.clone().existing_evidence()?
            )
            .is_err()
        );
        tx.rollback()?;
        let mut changed = proof.clone();
        changed.receipts.clear();
        assert!(
            reconstruct_admission(
                &store.connection,
                &store.policy,
                &changed,
                &facts,
                &proof.transport.clone().existing_evidence()?
            )
            .is_err()
        );
        let mut changed_facts = facts.clone();
        changed_facts.context.conversation = Some("changed".into());
        assert!(
            reconstruct_admission(
                &store.connection,
                &store.policy,
                &proof,
                &changed_facts,
                &proof.transport.clone().existing_evidence()?
            )
            .is_err()
        );
        let changed_transport = TransportEvidence::DirectNoise {
            device: DeviceId::from_key(&PublicKey::of(&network.root)),
        };
        assert!(
            reconstruct_admission(
                &store.connection,
                &store.policy,
                &proof,
                &facts,
                &changed_transport
            )
            .is_err()
        );
        let mut changed_facts = facts.clone();
        changed_facts.operation_id.push_str("-changed");
        assert!(
            reconstruct_admission(
                &store.connection,
                &store.policy,
                &proof,
                &changed_facts,
                &proof.transport.clone().existing_evidence()?
            )
            .is_err()
        );
        cached_without_crypto(&store, &proof, &facts)?;
        Ok(())
    }

    #[test]
    fn local_admission_cache_is_not_published_when_sql_commit_fails() -> Result<()> {
        use crate::identity::test_support::{TestNetwork, now};
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        network.conversation(&mut store, "conversation")?;
        let cache_len = store.policy.local_admissions.len();
        let (body, event) = network.message_body("conversation", "retry after rollback")?;
        let (auth, transport) = network.authorization(&store, &body, now())?;
        let freshness = network.freshness(&mut store, &auth, CheckStage::Admit, now())?;
        assert!(
            store
                .connection
                .pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))?
        );
        store.connection.execute_batch(
            "CREATE TABLE cache_commit_parent(id INTEGER PRIMARY KEY);
             CREATE TABLE cache_commit_guard(id INTEGER REFERENCES cache_commit_parent(id) DEFERRABLE INITIALLY DEFERRED);
             CREATE TRIGGER reject_admission_commit AFTER INSERT ON events BEGIN INSERT INTO cache_commit_guard VALUES(1); END;",
        )?;
        let failure = store.append_authorized_event(
            &network.node,
            &auth,
            &freshness,
            &transport,
            &body,
            "conversation",
            event.clone(),
            vec![],
            now() as i64,
        );
        let failure = failure.expect_err("deferred foreign key must reject COMMIT");
        assert!(format!("{failure:#}").contains("FOREIGN KEY constraint failed"));
        assert_eq!(store.policy.local_admissions.len(), cache_len);
        assert!(
            !store
                .policy
                .local_admissions
                .entries
                .contains_key(&(network.space.to_string(), auth.operation_id().to_owned()))
        );
        assert_eq!(
            store.connection.query_row(
                "SELECT (SELECT count(*) FROM accepted_authorizations WHERE authorization_id=?1),
                    (SELECT count(*) FROM authorization_nonces WHERE authorization_id=?1),
                    (SELECT count(*) FROM cache_commit_guard)",
                [auth.operation_id()],
                |row| Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?
                ))
            )?,
            (0, 0, 0)
        );
        assert!(!store.connection.query_row(
            "SELECT consumed FROM authorization_challenges WHERE nonce=?",
            [freshness.challenge.nonce.as_str()],
            |row| row.get::<_, bool>(0)
        )?);
        store.connection.execute_batch("DROP TRIGGER reject_admission_commit; DROP TABLE cache_commit_guard; DROP TABLE cache_commit_parent;")?;
        let accepted = store.append_authorized_event(
            &network.node,
            &auth,
            &freshness,
            &transport,
            &body,
            "conversation",
            event,
            vec![],
            now() as i64,
        )?;
        assert_eq!(store.policy.local_admissions.len(), cache_len + 1);
        let accepted: crate::model::Event = serde_json::from_str(&accepted.canonical_body)?;
        let EventCause::Invocation { admission } = accepted.cause else {
            unreachable!()
        };
        let facts = persisted_admission_facts(&store.connection, &store.policy, &admission)?;
        cached_without_crypto(&store, &admission, &facts)?;
        Ok(())
    }

    #[test]
    fn local_admission_cache_keeps_policy_time_and_current_revocation_gates() -> Result<()> {
        let (mut network, mut store, _, _, _, _) = local_cache_fixture(false)?;
        network.enroll_node(LocalIdentity::generate())?;
        store.install_policy_journal(
            &network.bundle.journal,
            crate::identity::test_support::now(),
        )?;
        let anchor = network.message(
            &mut store,
            "conversation",
            "after the accepted policy commit",
        )?;
        let event: crate::model::Event = serde_json::from_str(&anchor.canonical_body)?;
        let EventCause::Invocation { admission: proof } = event.cause else {
            unreachable!()
        };
        let facts = persisted_admission_facts(&store.connection, &store.policy, &proof)?;
        let transport = proof.transport.clone().existing_evidence()?;
        let admitted = cached_without_crypto(&store, &proof, &facts)?;
        let mut earlier = proof.clone();
        let accepted_at = network
            .bundle
            .journal
            .iter()
            .filter_map(|entry| match entry {
                vibeface_secure::authorization::JournalEntry::AccountCommit(value) => {
                    Some(value.commit.payload.accepted_at_ms)
                }
                _ => None,
            })
            .max()
            .context("fixture has no accepted policy commit")?;
        earlier.admitted_at_ms = accepted_at
            .checked_sub(1)
            .context("fixture policy predates the clock")?;
        assert!(
            store
                .policy
                .historical(&proof.challenge.policies, earlier.admitted_at_ms)
                .is_err()
        );
        assert!(
            reconstruct_admission_uncached(
                &store.connection,
                &store.policy,
                &earlier,
                &facts,
                &transport
            )
            .is_err()
        );
        assert!(
            reconstruct_admission(
                &store.connection,
                &store.policy,
                &earlier,
                &facts,
                &transport
            )
            .is_err()
        );
        assert!(
            store
                .authorization_state()
                .verify_operation_authorization(
                    &admitted,
                    &facts,
                    &transport,
                    admitted.admitted_at_ms() - 1
                )
                .is_err()
        );
        let after_expiry = proof.invocation.payload.expires_at_ms + 1;
        store.authorization_state().verify_operation_authorization(
            &admitted,
            &facts,
            &transport,
            after_expiry,
        )?;
        let retained = store.policy.local_admissions.len();
        network.revoke_client(&mut store)?;
        assert_eq!(store.policy.local_admissions.len(), retained);
        let historical = cached_without_crypto(&store, &proof, &facts)?;
        assert!(
            store
                .authorization_state()
                .verify_operation_authorization(&historical, &facts, &transport, after_expiry)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn local_admission_cache_binds_operation_rows_and_excludes_binding_commands() -> Result<()> {
        use crate::identity::test_support::{TestNetwork, now};
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        network.conversation(&mut store, "conversation")?;
        let request = network.request(&mut store, "operation-cache", "binding-session")?;
        let binding = request.binding.clone().context("binding required")?;
        let retained = store.policy.local_admissions.len();
        let mut seal_body = network.body("seal_execution", "conversation");
        seal_body["task_id"] = json!(request.task_id);
        seal_body["task_ref"] = json!(binding.binding.task_ref);
        seal_body["binding"] = json!(binding);
        let (auth, transport) = network.authorization(&store, &seal_body, now())?;
        let freshness = network.freshness(&mut store, &auth, CheckStage::Admit, now())?;
        let seal = store.seal_task_authorized(
            &network.node,
            &auth,
            &freshness,
            &transport,
            &seal_body,
            now(),
        )?;
        let mut rebind = network.body("rebind_execution", "conversation");
        rebind["task_id"] = json!(request.task_id);
        rebind["task_ref"] = json!(binding.binding.task_ref);
        rebind["binding"] = json!(binding);
        rebind["previous_seal"] = json!(seal);
        rebind["target_node"] = json!(network.node.node_id()?);
        rebind["session_id"] = json!("replacement-session");
        rebind["executor"] = json!("codex");
        rebind["cwd"] = json!(network.project);
        let (auth, transport) = network.authorization(&store, &rebind, now())?;
        let freshness = network.freshness(&mut store, &auth, CheckStage::Admit, now())?;
        store.rebind_task_authorized(
            &network.node,
            &auth,
            &freshness,
            &transport,
            &rebind,
            now(),
        )?;
        assert_eq!(store.policy.local_admissions.len(), retained);
        let commands = store.connection.prepare(
            "SELECT authorization_id FROM accepted_authorizations WHERE json_extract(source_body,'$.method') IN ('bind_execution','seal_execution','rebind_execution') ORDER BY authorization_id",
        )?.query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(commands.len(), 3);
        for command in commands {
            let stored =
                load_authorization_from(&store.connection, network.space.as_str(), &command)?;
            assert!(admission_anchors_in(&store.connection, &stored.proof)?.is_empty());
            assert!(
                !store
                    .policy
                    .local_admissions
                    .entries
                    .contains_key(&(network.space.to_string(), command))
            );
        }
        let request = network.request(&mut store, "operation-cache", "operation-session")?;
        network.admit(&mut store, &request)?;
        let stored = load_admission_from(
            &store.connection,
            network.space.as_str(),
            &request.operation_id,
        )?;
        let facts = persisted_admission_facts(&store.connection, &store.policy, &stored.proof)?;
        cached_without_crypto(&store, &stored.proof, &facts)?;
        for sql in [
            "UPDATE accepted_authorizations SET operation_id=NULL WHERE authorization_id=?",
            "UPDATE operations SET request=json_set(request,'$.session_id','changed') WHERE operation_id=?",
        ] {
            let tx = store.connection.unchecked_transaction()?;
            assert_eq!(tx.execute(sql, [&request.operation_id])?, 1);
            assert!(
                reconstruct_admission(
                    &tx,
                    &store.policy,
                    &stored.proof,
                    &facts,
                    &stored.transport.existing_evidence()?
                )
                .is_err()
            );
            tx.rollback()?;
            cached_without_crypto(&store, &stored.proof, &facts)?;
        }
        Ok(())
    }

    #[test]
    fn admission_persists_and_checks_nonce_and_challenge_relationships() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let proof = fixture.proof()?;
        let space = fixture.auth.resource().space.to_string();
        let tx = fixture
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let admitted = consume_authorization_in(
            &tx,
            &fixture.policy,
            &fixture.auth,
            &proof,
            &fixture.transport,
            &fixture.body,
            Some("operation_1"),
            NOW,
        )?;
        tx.commit()?;
        assert_eq!(
            load_admission_from(&fixture.connection, &space, "operation_1")?.proof,
            admitted
        );
        validate_persisted_admission(&fixture.connection, &admitted)?;
        fixture.connection.execute(
            "UPDATE authorization_nonces SET body_hash=?",
            [ObjectHash::body(&"tampered")?.as_str()],
        )?;
        assert!(load_admission_from(&fixture.connection, &space, "operation_1").is_err());
        assert!(validate_persisted_admission(&fixture.connection, &admitted).is_err());
        fixture.connection.execute(
            "UPDATE authorization_nonces SET body_hash=?",
            [fixture.auth.body_hash().as_str()],
        )?;
        fixture
            .connection
            .execute("UPDATE authorization_challenges SET consumed=0", [])?;
        assert!(load_admission_from(&fixture.connection, &space, "operation_1").is_err());
        Ok(())
    }

    #[test]
    fn authorization_failure_rolls_back_consumption_and_prevents_replay() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let proof = fixture.proof()?;
        {
            let tx = fixture
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut changed = fixture.body.clone();
            changed["title"] = json!("changed");
            assert!(
                consume_authorization_in(
                    &tx,
                    &fixture.policy,
                    &fixture.auth,
                    &proof,
                    &fixture.transport,
                    &changed,
                    None,
                    NOW
                )
                .is_err()
            );
        }
        assert!(!fixture.connection.query_row(
            "SELECT consumed FROM authorization_challenges",
            [],
            |row| row.get::<_, bool>(0)
        )?);
        {
            let tx = fixture
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            consume_authorization_in(
                &tx,
                &fixture.policy,
                &fixture.auth,
                &proof,
                &fixture.transport,
                &fixture.body,
                None,
                NOW,
            )?;
            tx.commit()?;
        }
        {
            let tx = fixture
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            assert!(
                consume_authorization_in(
                    &tx,
                    &fixture.policy,
                    &fixture.auth,
                    &proof,
                    &fixture.transport,
                    &fixture.body,
                    None,
                    NOW
                )
                .is_err()
            );
        }
        assert_eq!(
            fixture.connection.query_row(
                "SELECT count(*) FROM accepted_authorizations",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            1
        );
        assert_eq!(
            fixture.connection.query_row(
                "SELECT count(*) FROM authorization_nonces",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            1
        );
        Ok(())
    }

    #[test]
    fn changed_policy_head_prevents_consumption() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let proof = fixture.proof()?;
        fixture
            .connection
            .execute("UPDATE current_policy_heads SET sequence=sequence+1", [])?;
        {
            let tx = fixture
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            assert!(
                consume_authorization_in(
                    &tx,
                    &fixture.policy,
                    &fixture.auth,
                    &proof,
                    &fixture.transport,
                    &fixture.body,
                    None,
                    NOW
                )
                .is_err()
            );
        }
        assert!(!fixture.connection.query_row(
            "SELECT consumed FROM authorization_challenges",
            [],
            |row| row.get::<_, bool>(0)
        )?);
        assert_eq!(
            fixture.connection.query_row(
                "SELECT count(*) FROM accepted_authorizations",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            0
        );
        Ok(())
    }

    #[test]
    fn historical_admission_survives_invocation_expiry_but_current_revocation_stops_work()
    -> Result<()> {
        let mut fixture = Fixture::new()?;
        let proof = fixture.proof()?;
        let tx = fixture
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let admitted = consume_authorization_in(
            &tx,
            &fixture.policy,
            &fixture.auth,
            &proof,
            &fixture.transport,
            &fixture.body,
            None,
            NOW,
        )?;
        tx.commit()?;
        let facts = request_facts_from(
            &fixture.connection,
            &fixture.state,
            &fixture.body,
            fixture.auth.audience(),
        )?;
        let historical = reconstruct_admission(
            &fixture.connection,
            &fixture.policy,
            &admitted,
            &facts,
            &fixture.transport,
        )?;
        validate_persisted_admission(&fixture.connection, &admitted)?;
        assert_eq!(
            historical.signed_invocation(),
            fixture.auth.signed_invocation()
        );
        assert!(
            fixture
                .state
                .verify_operation_authorization(&historical, &facts, &fixture.transport, 91_000)
                .is_ok()
        );
        let space = fixture.auth.resource().space.clone();
        let mut revoked = fixture.state.space(&space).unwrap().clone();
        revoked.previous = fixture.state.head(&PolicyDomain::Space(space)).cloned();
        revoked.sequence += 1;
        revoked.grants.clear();
        let root = Identity::from_secret_hex(&hex::encode([1; 32])).map_err(anyhow::Error::msg)?;
        fixture.state.install_space(Signed::sign(revoked, &root)?)?;
        assert!(
            fixture
                .state
                .verify_operation_authorization(&historical, &facts, &fixture.transport, 91_000)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn persisted_execution_lease_cannot_outlive_its_signed_receipts() -> Result<()> {
        use crate::identity::test_support::{TestNetwork, now};
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        network.conversation(&mut store, "conversation")?;
        let request = network.request(&mut store, "operation_1", "session_1")?;
        network.admit(&mut store, &request)?;
        let auth = store.operation_authorization(network.space.as_str(), "operation_1", now())?;
        let mut proof = network.freshness(&mut store, &auth, CheckStage::Start, now())?;
        let signer =
            Identity::from_secret_hex(&network.node.secret_key_hex).map_err(anyhow::Error::msg)?;
        let expected = now() + 15_000;
        for receipt in &mut proof.receipts {
            let mut payload = receipt.payload.clone();
            payload.expires_at_ms = expected;
            *receipt = Signed::sign(payload, &signer)?;
        }
        let (_, deadline) = store.claim_authorized_operation(
            network.space.as_str(),
            "operation_1",
            &proof,
            now(),
        )?;
        assert_eq!(deadline, expected);
        assert_eq!(
            store.operation_lease_deadline(network.space.as_str(), "operation_1")?,
            expected
        );
        store.connection.execute("UPDATE local_values SET value=json_set(value,'$.deadline',?) WHERE space_id=? AND key='lease:operation_1'",
            params![proof.challenge.expires_at_ms, network.space.as_str()])?;
        assert!(
            store
                .operation_lease_deadline(network.space.as_str(), "operation_1")
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn remote_reads_reuse_exact_proof_without_consuming_write_nonces() -> Result<()> {
        let fixture = Fixture::new()?;
        let body = json!({"method":"conversations","space_id":fixture.auth.resource().space,
            "resource_id":fixture.auth.resource().resource,"authorization_id":"read_1"});
        let facts = request_facts_from(
            &fixture.connection,
            &fixture.state,
            &body,
            fixture.auth.audience(),
        )?;
        let mut invocation = fixture.auth.signed_invocation().payload.clone();
        invocation.action = facts.action;
        invocation.context = facts.context.clone();
        invocation.operation_id = facts.operation_id.clone();
        invocation.body_hash = facts.body_hash.clone();
        invocation.nonce = Nonce::random()?;
        let user = Identity::from_secret_hex(&hex::encode([4; 32])).map_err(anyhow::Error::msg)?;
        let auth = fixture.state.verify_invocation(
            &Signed::sign(invocation, &user)?,
            &facts,
            &fixture.transport,
            NOW,
        )?;
        let challenge = FreshnessChallenge::new(
            &auth,
            CheckStage::RemoteRead,
            Nonce::random()?,
            NOW,
            NOW + 60_000,
        )?;
        let receipts = vec![Signed::sign(
            FreshnessReceipt {
                version: AUTH_VERSION,
                currents: auth.dependencies().to_vec(),
                challenge_hash: challenge.hash()?,
                issued_at_ms: NOW,
                expires_at_ms: NOW + 60_000,
            },
            &fixture.authority,
        )?];
        fixture.connection.execute("INSERT INTO authorization_challenges(space_id,nonce,invocation_hash,stage,challenge,expires_at_ms) VALUES(?,?,?,'remote_read',?,?)",
            params![auth.resource().space.as_str(), challenge.nonce.as_str(), auth.invocation_hash().as_str(), serde_json::to_string(&challenge)?, challenge.expires_at_ms])?;
        let proof = FreshnessProof {
            challenge,
            receipts,
        };
        let store = Store {
            connection: fixture.connection,
            policy: fixture.policy,
            target_recovery_import: None,
            account_node_enrollment_import: None,
            foreign_execution_policy: Default::default(),
        };
        store.authorize_read(&auth, &proof, &fixture.transport, NOW)?;
        store.authorize_read(&auth, &proof, &fixture.transport, NOW + 1)?;
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM authorization_nonces", [], |row| row
                    .get::<_, u64>(
                    0
                ))?,
            0
        );
        assert!(!store.connection.query_row(
            "SELECT consumed FROM authorization_challenges",
            [],
            |row| row.get::<_, bool>(0)
        )?);
        let mut wrong_stage = proof.clone();
        wrong_stage.challenge.stage = CheckStage::Admit;
        assert!(
            store
                .authorize_read(&auth, &wrong_stage, &fixture.transport, NOW)
                .is_err()
        );
        store
            .connection
            .execute("UPDATE current_policy_heads SET sequence=sequence+1", [])?;
        assert!(
            store
                .authorize_read(&auth, &proof, &fixture.transport, NOW + 1)
                .is_err()
        );
        Ok(())
    }
}

#[cfg(test)]
mod manager_tests {
    use super::*;
    use crate::identity::test_support::{TestNetwork, now};
    use crate::scheduling::SchedulerSession;
    use serde_json::json;
    use vibeface_secure::{
        Identity,
        authorization::{Grantee, JournalEntry, Nonce, PublicKey, Signed, SpaceId},
    };

    struct Fixture {
        network: TestNetwork,
        store: Store,
        auth: VerifiedAuthorization,
        transport: TransportEvidence,
        body: Value,
        proof: FreshnessProof,
        service_grant: ObjectHash,
        sdk_config: String,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            Self::new_at(false)
        }

        fn new_at(persistent: bool) -> Result<Self> {
            let network = TestNetwork::new()?;
            let mut store = if persistent {
                network.at(&network.directory.join("node.sqlite3"))?
            } else {
                network.memory()?
            };
            network.conversation(&mut store, "conversation")?;
            let service = vibeface_secure::authorization::ServiceId::from_key(&PublicKey::parse(
                &network.node.public_key_hex()?,
            )?);
            let mut child = store
                .authorization_state()
                .grant(&network.bundle.grant_leaf)
                .unwrap()
                .payload
                .clone();
            child.parent = Some(network.bundle.grant_leaf.clone());
            child.grantee = Grantee::Actor(Actor::Service(service.clone()));
            child.actions = vec![Action::ConversationRead, Action::AgentStart];
            child.remaining_delegations -= 1;
            child.issued_under = store
                .authorization_state()
                .head(&PolicyDomain::Space(network.space.clone()))
                .unwrap()
                .clone();
            child.nonce = Nonce::random()?;
            let child = Signed::sign(child, &network.client)?;
            let service_grant = child.hash()?;
            let mut document = store
                .authorization_state()
                .space(&network.space)
                .unwrap()
                .clone();
            document.previous = store
                .authorization_state()
                .head(&PolicyDomain::Space(network.space.clone()))
                .cloned();
            document.sequence += 1;
            document.grants.push(service_grant.clone());
            document.grants.sort();
            store.install_policy_journal(
                &[
                    JournalEntry::Grant(child),
                    JournalEntry::Space(Signed::sign(document, &network.root)?),
                ],
                now(),
            )?;
            let trigger = network.message(&mut store, "conversation", "please work on this")?;
            let sdk_session = SessionRef {
                target: vibeface_secure::authorization::DeviceId::from_key(&PublicKey::parse(
                    &network.node.public_key_hex()?,
                )?),
                session_id: trigger.hash.clone(),
            };
            let descriptor = store.agent_grant(
                network.space.as_str(),
                "conversation",
                &service,
                &sdk_session,
                &trigger.hash,
                now(),
            )?;
            let service_grant = descriptor.grant_leaf.clone();
            let sdk_config = crate::crypto::digest(b"actual manager configuration");
            let mut body = network.body("agent_start", "conversation");
            body["attempt_id"] = json!("fixture-attempt");
            body["scheduler_node"] = json!(network.node.node_id()?);
            body["authorization_id"] = json!(format!("manager:{}:fixture-attempt", trigger.hash));
            store.admit_trigger(
                network.space.as_str(),
                "conversation",
                &trigger.hash,
                &network.node.node_id()?,
                &network.node.node_id()?,
            )?;
            body["trigger"] = json!(trigger.hash);
            body["sdk_session"] = json!(descriptor.sdk_session);
            body["parent_invocation"] = json!(descriptor.parent_invocation);
            body["sdk_config"] = json!(sdk_config);
            let key = PublicKey::parse(&network.node.public_key_hex()?)?;
            let audience = vibeface_secure::authorization::Audience {
                service: service.clone(),
                device: vibeface_secure::authorization::DeviceId::from_key(&key),
            };
            let facts = store.request_facts(&body, &audience)?;
            let signer = Identity::from_secret_hex(&network.node.secret_key_hex)
                .map_err(anyhow::Error::msg)?;
            let signed = Signed::sign(
                vibeface_secure::authorization::Invocation {
                    version: vibeface_secure::authorization::AUTH_VERSION,
                    actor: Actor::Service(service.clone()),
                    device: descriptor.device.clone(),
                    initiator: descriptor.initiator,
                    initiating_grant: descriptor.initiating_grant,
                    resource: facts.resource.clone(),
                    action: Action::AgentStart,
                    audience,
                    presenter: descriptor.device,
                    operation_id: facts.operation_id.clone(),
                    body_hash: facts.body_hash.clone(),
                    grant_leaf: descriptor.grant_leaf,
                    expected_policies: descriptor.expected_policies,
                    context: facts.context.clone(),
                    nonce: Nonce::random()?,
                    not_before_ms: now(),
                    expires_at_ms: now() + 10_000,
                },
                &signer,
            )?;
            let transport = TransportEvidence::LocalAgentAdapter {
                service,
                session: sdk_session,
            };
            let auth = store.authorize_request(&signed, &facts, &transport, now())?;
            let proof = network.freshness(&mut store, &auth, CheckStage::Admit, now())?;
            Ok(Self {
                network,
                store,
                auth,
                transport,
                body,
                proof,
                service_grant,
                sdk_config,
            })
        }

        fn claim(&mut self, boot: &str) -> Result<ManagerAdmission> {
            self.store.recover_scheduler_jobs(
                self.network.space.as_str(),
                &self.network.node.node_id()?,
                boot,
            )?;
            let mut admitted = self.store.claim_authorized_manager(
                &self.auth,
                &self.proof,
                &self.transport,
                &self.body,
                boot,
                &self.sdk_config,
                now(),
            )?;
            if admitted.fresh {
                let attempt = admitted.job.state.attempt().unwrap().id.clone();
                admitted.job = self.store.mark_started(
                    self.network.space.as_str(),
                    "conversation",
                    &self.network.node.node_id()?,
                    &admitted.job.trigger,
                    &attempt,
                )?;
            }
            Ok(admitted)
        }
    }

    #[test]
    fn local_admission_cache_pins_the_manager_trigger_and_ready_ancestors() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let first = fixture.claim("cache-boot")?;
        let stored = load_authorization_from(
            &fixture.store.connection,
            fixture.network.space.as_str(),
            &first.manager_id,
        )?;
        let facts = persisted_admission_facts(
            &fixture.store.connection,
            &fixture.store.policy,
            &stored.proof,
        )?;
        let trigger = manager_field(&fixture.body, "trigger")?;
        let key = (fixture.network.space.to_string(), first.manager_id.clone());
        let checkpoint = &fixture.store.policy.local_admissions.entries[&key];
        assert_eq!(checkpoint.anchors.len(), 1);
        assert_eq!(checkpoint.anchors[0].hash, trigger);
        super::tests::cached_without_crypto(&fixture.store, &stored.proof, &facts)?;
        super::tests::cached_without_crypto(&fixture.store, &stored.proof, &facts)?;
        assert_eq!(
            manager_lease_from(
                &fixture.store.connection,
                &fixture.store.policy,
                fixture.network.space.as_str(),
                &first.manager_id,
            )?,
            first.lease_expires_at_ms
        );
        for (path, value) in [
            ("$.deadline", json!(first.lease_expires_at_ms + 1)),
            ("$.checked_at_ms", json!(stored.proof.admitted_at_ms + 1)),
            ("$.proof.challenge.stage", json!("renew")),
            ("$.proof.receipts[0].signature", json!("00".repeat(64))),
        ] {
            let tx = fixture.store.connection.unchecked_transaction()?;
            assert_eq!(tx.execute(
                "UPDATE local_values SET value=json_set(value,?,json(?)) WHERE space_id=? AND key=?",
                params![path, serde_json::to_string(&value)?, fixture.network.space.as_str(), format!("lease:{}", first.manager_id)],
            )?, 1);
            assert!(
                manager_lease_from(
                    &tx,
                    &fixture.store.policy,
                    fixture.network.space.as_str(),
                    &first.manager_id,
                )
                .is_err(),
                "{path}"
            );
            tx.rollback()?;
        }
        let trigger_envelope = fixture
            .store
            .event(fixture.network.space.as_str(), trigger)?
            .context("manager trigger missing")?;
        let trigger_event: crate::model::Event =
            serde_json::from_str(&trigger_envelope.canonical_body)?;
        let ancestor = trigger_event
            .parents
            .first()
            .context("trigger ancestor missing")?;
        let ancestor = fixture
            .store
            .event(fixture.network.space.as_str(), ancestor)?
            .context("manager trigger ancestor missing")?;
        for original in [&trigger_envelope, &ancestor] {
            let mut changed = original.clone();
            changed.canonical_body.push(' ');
            let tx = fixture.store.connection.unchecked_transaction()?;
            assert_eq!(
                tx.execute(
                    "UPDATE events SET envelope=? WHERE hash=?",
                    params![serde_json::to_string(&changed)?, original.hash]
                )?,
                1
            );
            assert!(
                reconstruct_admission(
                    &tx,
                    &fixture.store.policy,
                    &stored.proof,
                    &facts,
                    &stored.transport.existing_evidence()?
                )
                .is_err()
            );
            tx.rollback()?;
            super::tests::cached_without_crypto(&fixture.store, &stored.proof, &facts)?;
        }
        let tx = fixture.store.connection.unchecked_transaction()?;
        assert_eq!(
            tx.execute(
                "UPDATE events SET status='conflict' WHERE hash=?",
                [trigger]
            )?,
            1
        );
        assert!(
            reconstruct_admission(
                &tx,
                &fixture.store.policy,
                &stored.proof,
                &facts,
                &stored.transport.existing_evidence()?
            )
            .is_err()
        );
        tx.rollback()?;
        let mut changed = stored.proof.clone();
        changed.source_body["trigger"] = json!(ancestor.hash);
        assert!(
            reconstruct_admission(
                &fixture.store.connection,
                &fixture.store.policy,
                &changed,
                &facts,
                &stored.transport.existing_evidence()?
            )
            .is_err()
        );
        super::tests::cached_without_crypto(&fixture.store, &stored.proof, &facts)?;
        Ok(())
    }

    #[test]
    fn manager_notification_conflict_preserves_an_independent_candidate_and_acknowledgment()
    -> Result<()> {
        let mut fixture = Fixture::new()?;
        fixture.claim("boot")?;
        let mut notices = Vec::new();
        for text in ["first candidate", "second candidate"] {
            let mut body = fixture.body.clone();
            body["method"] = json!("agent_notification");
            body["authorization_id"] = json!(format!("notification:{}", Nonce::random()?));
            body["notification_id"] = json!("shared-notification");
            body["kind"] = json!("needs_attention");
            body["text"] = json!(text);
            let facts = fixture
                .store
                .request_facts(&body, fixture.auth.audience())?;
            let mut invocation = fixture.auth.signed_invocation().payload.clone();
            invocation.operation_id = facts.operation_id.clone();
            invocation.body_hash = facts.body_hash.clone();
            invocation.context = facts.context.clone();
            invocation.nonce = Nonce::random()?;
            let signer = Identity::from_secret_hex(&fixture.network.node.secret_key_hex)
                .map_err(anyhow::Error::msg)?;
            let auth = fixture.store.authorize_request(
                &Signed::sign(invocation, &signer)?,
                &facts,
                &fixture.transport,
                now(),
            )?;
            let proof =
                fixture
                    .network
                    .freshness(&mut fixture.store, &auth, CheckStage::Admit, now())?;
            notices.push(fixture.store.append_authorized_event(
                &fixture.network.node,
                &auth,
                &proof,
                &fixture.transport,
                &body,
                "conversation",
                EventBody::NotificationRaised {
                    notification_id: "shared-notification".into(),
                    kind: "needs_attention".into(),
                    text: text.into(),
                },
                vec![],
                now() as i64,
            )?);
        }
        let mut ack = fixture.network.body("ack", "conversation");
        ack["notification_id"] = json!("shared-notification");
        ack["client_id"] = json!("phone");
        fixture.network.append(
            &mut fixture.store,
            &ack,
            EventBody::NotificationAcknowledged {
                notification_id: "shared-notification".into(),
                client_id: "phone".into(),
            },
            now(),
        )?;
        let mut replica = fixture.network.memory()?;
        let journal = fixture
            .store
            .connection
            .prepare("SELECT entry FROM policy_journal ORDER BY ordinal")?
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str::<JournalEntry>(&row?)?))
            .collect::<Result<Vec<_>>>()?;
        replica.install_policy_journal(&journal, now())?;
        replica.import_snapshot(
            fixture.network.space.as_str(),
            &fixture
                .store
                .export_snapshot(fixture.network.space.as_str(), "conversation")?,
        )?;
        let second: crate::model::Event = serde_json::from_str(&notices[1].canonical_body)?;
        let fork = fixture.network.signed_message(
            &mut fixture.store,
            "conflicting second notification",
            second.sequence,
            second.previous,
            second.parents,
        )?;
        fixture
            .store
            .ingest_event(fixture.network.space.as_str(), &fork)?;
        replica.ingest_event(fixture.network.space.as_str(), &fork)?;
        for store in [&fixture.store, &replica] {
            let pending = store.pending_notifications(fixture.network.space.as_str(), "laptop")?;
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].notification_id, "shared-notification");
            assert_eq!(pending[0].text, "first candidate");
            assert_eq!(pending[0].event_hash, notices[0].hash);
            assert!(
                store
                    .pending_notifications(fixture.network.space.as_str(), "phone")?
                    .is_empty()
            );
        }
        Ok(())
    }

    #[test]
    fn reopening_running_manager_preserves_unknown_work_without_replaying_admission() -> Result<()>
    {
        let mut fixture = Fixture::new_at(true)?;
        let first = fixture.claim("boot-before-restart")?;
        let key = scheduler_key(
            "session",
            "conversation",
            &fixture.network.node.node_id()?,
            "",
        );
        let mut session: SchedulerSession = local_from(
            &fixture.store.connection,
            fixture.network.space.as_str(),
            &key,
        )?
        .unwrap();
        session.native_id = Some("native-before-restart".into());
        fixture.store.local_set(
            fixture.network.space.as_str(),
            &key,
            &serde_json::to_value(&session)?,
        )?;
        let counts: (u64,u64) = fixture.store.connection.query_row(
            "SELECT (SELECT count(*) FROM accepted_authorizations),(SELECT count(*) FROM authorization_nonces)",[],|row| Ok((row.get(0)?,row.get(1)?)))?;
        let Fixture {
            network,
            store,
            auth,
            transport,
            body,
            proof,
            sdk_config,
            ..
        } = fixture;
        drop(store);
        let mut reopened = Store::open(network.directory.join("node.sqlite3"))?;
        reopened.recover_scheduler_jobs(
            network.space.as_str(),
            &network.node.node_id()?,
            "boot-after-restart",
        )?;
        let session: SchedulerSession =
            local_from(&reopened.connection, network.space.as_str(), &key)?.unwrap();
        assert!(session.native_id.is_none());
        assert!(
            session
                .active
                .as_ref()
                .is_some_and(|active| active == &first.job.trigger)
        );
        let replay = reopened.claim_authorized_manager(
            &auth,
            &proof,
            &transport,
            &body,
            "boot-after-restart",
            &sdk_config,
            now(),
        )?;
        assert!(!replay.fresh && matches!(replay.job.state, JobState::Unknown { .. }));
        assert!(replay.previous_native_id.is_none());
        assert_eq!(reopened.connection.query_row(
            "SELECT (SELECT count(*) FROM accepted_authorizations),(SELECT count(*) FROM authorization_nonces)",[],|row| Ok((row.get::<_,u64>(0)?,row.get::<_,u64>(1)?)))?,counts);
        assert!(reopened.operations(network.space.as_str())?.is_empty());
        Ok(())
    }

    #[test]
    fn startup_validation_keeps_revoked_history_and_rejects_lease_tampering() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let first = fixture.claim("boot")?;
        validate_persisted_authorizations(&fixture.store.connection, &fixture.store.policy)?;
        fixture.network.revoke_client(&mut fixture.store)?;
        assert!(
            fixture
                .store
                .manager_tool_authorization(
                    fixture.network.space.as_str(),
                    &first.manager_id,
                    first.lease_expires_at_ms + 1
                )
                .is_err()
        );
        validate_persisted_authorizations(&fixture.store.connection, &fixture.store.policy)?;
        fixture.store.connection.execute_batch(
            "UPDATE events SET status='conflict'; DELETE FROM tasks; DELETE FROM conversations;",
        )?;
        validate_persisted_authorizations(&fixture.store.connection, &fixture.store.policy)?;
        assert_eq!(
            verified_admission_checkpoints_from(&fixture.store.connection, &fixture.store.policy,)?
                .len(),
            0
        );
        let stored = load_authorization_from(
            &fixture.store.connection,
            fixture.network.space.as_str(),
            &first.manager_id,
        )?;
        let mut uncached = fixture.store.policy.clone();
        uncached.local_admissions = AdmissionCheckpoints::default();
        let facts = persisted_admission_facts(&fixture.store.connection, &uncached, &stored.proof)?;
        assert!(
            reconstruct_admission(
                &fixture.store.connection,
                &fixture.store.policy,
                &stored.proof,
                &facts,
                &stored.transport.existing_evidence()?,
            )
            .is_err()
        );
        let (hash, original): (String, String) = fixture.store.connection.query_row(
            "SELECT hash,envelope FROM events ORDER BY hash LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        fixture.store.connection.execute(
            "UPDATE events SET envelope=json_set(envelope,'$.signature',?) WHERE hash=?",
            params!["00".repeat(64), hash],
        )?;
        assert!(
            verified_admission_checkpoints_from(&fixture.store.connection, &fixture.store.policy,)
                .is_err()
        );
        fixture.store.connection.execute(
            "UPDATE events SET envelope=? WHERE hash=?",
            params![original, hash],
        )?;
        validate_persisted_authorizations(&fixture.store.connection, &fixture.store.policy)?;
        fixture.store.connection.execute("UPDATE local_values SET value=json_set(value,'$.deadline',?) WHERE space_id=? AND key=?",
            params![first.lease_expires_at_ms+1,fixture.network.space.as_str(),format!("lease:{}",first.manager_id)])?;
        assert!(
            validate_persisted_authorizations(&fixture.store.connection, &fixture.store.policy)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn startup_validation_rejects_changed_authorization_nonce_and_challenge_records() -> Result<()>
    {
        let mut fixture = Fixture::new()?;
        let first = fixture.claim("boot")?;
        fixture.store.connection.execute(
            "UPDATE accepted_authorizations SET actor_id='changed' WHERE authorization_id=?",
            [&first.manager_id],
        )?;
        assert!(
            validate_persisted_authorizations(&fixture.store.connection, &fixture.store.policy)
                .is_err()
        );
        let Actor::Service(service) = fixture.auth.actor() else {
            unreachable!()
        };
        fixture.store.connection.execute(
            "UPDATE accepted_authorizations SET actor_id=? WHERE authorization_id=?",
            params![service.as_str(), first.manager_id],
        )?;
        fixture.store.connection.execute(
            "UPDATE authorization_nonces SET body_hash=? WHERE authorization_id=?",
            params![ObjectHash::body(&"changed")?.as_str(), first.manager_id],
        )?;
        assert!(
            validate_persisted_authorizations(&fixture.store.connection, &fixture.store.policy)
                .is_err()
        );
        fixture.store.connection.execute(
            "UPDATE authorization_nonces SET body_hash=? WHERE authorization_id=?",
            params![fixture.auth.body_hash().as_str(), first.manager_id],
        )?;
        fixture.store.connection.execute(
            "UPDATE authorization_challenges SET consumed=0 WHERE nonce=?",
            [fixture.proof.challenge.nonce.as_str()],
        )?;
        assert!(
            validate_persisted_authorizations(&fixture.store.connection, &fixture.store.policy)
                .is_err()
        );
        fixture.store.connection.execute(
            "UPDATE authorization_challenges SET consumed=1 WHERE nonce=?",
            [fixture.proof.challenge.nonce.as_str()],
        )?;
        validate_persisted_authorizations(&fixture.store.connection, &fixture.store.policy)?;
        Ok(())
    }

    #[test]
    fn startup_validation_binds_operation_rows_and_reservations_to_real_admissions() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        network.conversation(&mut store, "conversation")?;
        let request = network.request(&mut store, "startup-operation", "startup-session")?;
        network.admit(&mut store, &request)?;
        validate_persisted_authorizations(&store.connection, &store.policy)?;
        store.connection.execute(
            "UPDATE accepted_authorizations SET operation_id=NULL WHERE authorization_id=?",
            [&request.operation_id],
        )?;
        assert!(validate_persisted_authorizations(&store.connection, &store.policy).is_err());
        store.connection.execute(
            "UPDATE accepted_authorizations SET operation_id=? WHERE authorization_id=?",
            params![request.operation_id, request.operation_id],
        )?;
        network.claim(&mut store, &request.operation_id)?;
        validate_persisted_authorizations(&store.connection, &store.policy)?;
        let lease_key = format!("lease:{}", request.operation_id);
        let lease: String = store.connection.query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![network.space.as_str(), lease_key],
            |row| row.get(0),
        )?;
        store.connection.execute(
            "DELETE FROM local_values WHERE space_id=? AND key=?",
            params![network.space.as_str(), lease_key],
        )?;
        assert!(validate_persisted_authorizations(&store.connection, &store.policy).is_err());
        let handoff_key = format!("project-execution-handoff:{}", request.operation_id);
        store.connection.execute(
            "INSERT INTO local_values(space_id,key,value) VALUES(?,?,'{}')",
            params![network.space.as_str(), handoff_key],
        )?;
        assert!(validate_persisted_authorizations(&store.connection, &store.policy).is_err());
        store.connection.execute(
            "DELETE FROM local_values WHERE space_id=? AND key=?",
            params![network.space.as_str(), handoff_key],
        )?;
        store.connection.execute(
            "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
            params![network.space.as_str(), lease_key, lease],
        )?;
        validate_persisted_authorizations(&store.connection, &store.policy)?;
        store.connection.execute(
            "UPDATE resource_reservations SET resource=resource || '/different'",
            [],
        )?;
        assert!(validate_persisted_authorizations(&store.connection, &store.policy).is_err());
        store.connection.execute(
            "UPDATE resource_reservations SET resource=?",
            [&request.resources[0]],
        )?;
        store.finish_operation(
            network.space.as_str(),
            &request.operation_id,
            OperationState::Succeeded,
            serde_json::json!({"done":true}),
        )?;
        let result = store.seal_local_execution_result(
            &network.node,
            network.space.as_str(),
            &request.operation_id,
            now() + 2,
        )?;
        let dispatch = store
            .operation_dispatch(network.space.as_str(), &request.operation_id)?
            .context("dispatch missing")?;
        store.append_derived_event(
            &network.node,
            network.space.as_str(),
            &dispatch.hash,
            EventBody::ExecutionObserved {
                operation_id: request.operation_id.clone(),
                task_id: request.task_id.clone(),
                target_node: request.target_node.clone(),
                session_id: request.session_id.clone(),
                status: "succeeded".into(),
                detail: serde_json::to_value(result)?,
            },
            vec![],
            (now() + 2) as i64,
        )?;
        validate_persisted_authorizations(&store.connection, &store.policy)?;
        Ok(())
    }

    #[test]
    fn manager_tools_require_running_matching_session_and_unexpired_lease() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let first = fixture.claim("boot")?;
        let space = fixture.network.space.as_str();
        fixture
            .store
            .manager_tool_authorization(space, &first.manager_id, now())?;
        assert!(
            fixture
                .store
                .manager_tool_authorization(space, &first.manager_id, first.lease_expires_at_ms)
                .is_err()
        );
        let key = scheduler_key(
            "session",
            "conversation",
            &fixture.network.node.node_id()?,
            "",
        );
        let mut session: SchedulerSession =
            local_from(&fixture.store.connection, space, &key)?.unwrap();
        session.sdk_config = Some("different-configuration".into());
        fixture
            .store
            .local_set(space, &key, &serde_json::to_value(&session)?)?;
        assert!(
            fixture
                .store
                .manager_tool_authorization(space, &first.manager_id, now())
                .is_err()
        );
        session.sdk_config = Some(fixture.sdk_config.clone());
        session.permission_version += 1;
        fixture
            .store
            .local_set(space, &key, &serde_json::to_value(&session)?)?;
        assert!(
            fixture
                .store
                .manager_tool_authorization(space, &first.manager_id, now())
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn manager_expiry_blocks_inflight_read_and_atomic_write_with_valid_freshness() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let first = fixture.claim("boot")?;
        let mut proofs = Vec::new();
        for (method, stage) in [
            ("agent_result", CheckStage::Admit),
            ("conversation", CheckStage::RemoteRead),
        ] {
            let mut body = fixture.body.clone();
            body["method"] = json!(method);
            body["authorization_id"] = json!(format!("tool:{}", Nonce::random()?));
            if method == "agent_result" {
                body["message_id"] = json!(first.job.message_id);
                body["decision_id"] = json!(format!("decision:{}", first.job.message_id));
                body["output"] = json!("retained completed output");
                body["text"] = body["output"].clone();
                body["blobs"] = json!([]);
                body["effective_constraints"] = json!([]);
                body["tool_configuration"] = json!({});
            }
            let facts = fixture
                .store
                .request_facts(&body, fixture.auth.audience())?;
            let mut invocation = fixture.auth.signed_invocation().payload.clone();
            invocation.operation_id = facts.operation_id.clone();
            invocation.action = facts.action;
            invocation.body_hash = facts.body_hash.clone();
            invocation.context = facts.context.clone();
            invocation.nonce = Nonce::random()?;
            invocation.expires_at_ms = now() + 60_000;
            let signer = Identity::from_secret_hex(&fixture.network.node.secret_key_hex)
                .map_err(anyhow::Error::msg)?;
            let signed = Signed::sign(invocation, &signer)?;
            let auth =
                fixture
                    .store
                    .authorize_request(&signed, &facts, &fixture.transport, now())?;
            let proof = fixture
                .network
                .freshness(&mut fixture.store, &auth, stage, now())?;
            proofs.push((body, auth, proof));
        }
        let (_, read, proof) = &proofs[1];
        fixture
            .store
            .authorize_read(read, proof, &fixture.transport, now())?;
        assert!(
            fixture
                .store
                .authorize_read(read, proof, &fixture.transport, first.lease_expires_at_ms)
                .is_err()
        );
        let (body, auth, proof) = &proofs[0];
        {
            let tx = fixture
                .store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            assert!(
                consume_authorization_in(
                    &tx,
                    &fixture.store.policy,
                    auth,
                    proof,
                    &fixture.transport,
                    body,
                    None,
                    first.lease_expires_at_ms
                )
                .is_err()
            );
        }
        assert!(!fixture.store.connection.query_row(
            "SELECT consumed FROM authorization_challenges WHERE space_id=? AND nonce=?",
            params![
                fixture.network.space.as_str(),
                proof.challenge.nonce.as_str()
            ],
            |row| row.get::<_, bool>(0)
        )?);
        assert_eq!(
            fixture.store.connection.query_row(
                "SELECT count(*) FROM accepted_authorizations WHERE authorization_id=?",
                [auth.operation_id()],
                |row| row.get::<_, u64>(0)
            )?,
            0
        );
        fixture.store.retain_scheduler_result(
            fixture.network.space.as_str(),
            &first.job,
            "retained completed output",
            &[],
        )?;
        assert!(
            fixture
                .store
                .manager_tool_authorization(
                    fixture.network.space.as_str(),
                    &first.manager_id,
                    first.lease_expires_at_ms,
                )
                .is_err()
        );
        {
            let tx = fixture
                .store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            assert_eq!(tx.execute(
                "UPDATE local_values SET value=json_set(value,'$.output','changed output') WHERE space_id=? AND key=?",
                params![fixture.network.space.as_str(), scheduler_key("known-result", &first.job.conversation_id, &first.job.target_node, &first.job.trigger)],
            )?, 1);
            assert!(
                consume_authorization_in(
                    &tx,
                    &fixture.store.policy,
                    auth,
                    proof,
                    &fixture.transport,
                    body,
                    None,
                    first.lease_expires_at_ms,
                )
                .is_err()
            );
            tx.rollback()?;
        }
        let tx = fixture
            .store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        consume_authorization_in(
            &tx,
            &fixture.store.policy,
            auth,
            proof,
            &fixture.transport,
            body,
            None,
            first.lease_expires_at_ms,
        )?;
        tx.rollback()?;
        Ok(())
    }

    #[test]
    fn manager_claim_is_once_and_a_new_boot_preserves_unknown_work() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let first = fixture.claim("boot-a")?;
        assert!(first.fresh && first.previous_native_id.is_none());
        assert_eq!(first.lease_expires_at_ms, now() + 10_000);
        let duplicate = fixture.claim("boot-a")?;
        assert!(
            !duplicate.fresh
                && duplicate.manager_id == first.manager_id
                && matches!(duplicate.job.state, JobState::Running { .. })
        );
        let space = fixture.network.space.as_str();
        let session_key = scheduler_key(
            "session",
            "conversation",
            &fixture.network.node.node_id()?,
            "",
        );
        let mut session: SchedulerSession =
            local_from(&fixture.store.connection, space, &session_key)?.unwrap();
        session.native_id = Some("native-manager".into());
        fixture
            .store
            .local_set(space, &session_key, &serde_json::to_value(session)?)?;
        let resumed = fixture.claim("boot-b")?;
        assert!(!resumed.fresh && matches!(resumed.job.state, JobState::Unknown { .. }));
        let session: SchedulerSession = local_from(
            &fixture.store.connection,
            fixture.network.space.as_str(),
            &session_key,
        )?
        .unwrap();
        assert!(session.native_id.is_none() && session.active.is_some());
        assert_eq!(
            fixture.store.connection.query_row(
                "SELECT count(*) FROM accepted_authorizations WHERE action='agent_start'",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            1
        );
        assert!(
            fixture
                .store
                .operations(fixture.network.space.as_str())?
                .is_empty()
        );
        let auth = fixture.store.manager_authorization(
            fixture.network.space.as_str(),
            &first.manager_id,
            now() + 1,
        )?;
        let proof =
            fixture
                .network
                .freshness(&mut fixture.store, &auth, CheckStage::Renew, now() + 1)?;
        assert!(
            fixture
                .store
                .renew_authorized_manager(
                    fixture.network.space.as_str(),
                    &first.manager_id,
                    &proof,
                    now() + 1
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn manager_claim_rolls_back_nonce_and_session_if_any_persisted_part_fails() -> Result<()> {
        let mut fixture = Fixture::new()?;
        fixture.store.recover_scheduler_jobs(
            fixture.network.space.as_str(),
            &fixture.network.node.node_id()?,
            "boot",
        )?;
        let scheduler_before = fixture
            .store
            .connection
            .prepare("SELECT key,value FROM local_values ORDER BY key")?
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert!(
            fixture
                .store
                .claim_authorized_manager(
                    &fixture.auth,
                    &fixture.proof,
                    &fixture.transport,
                    &fixture.body,
                    "boot",
                    "different-sdk",
                    now()
                )
                .is_err()
        );
        fixture.store.connection.execute_batch("CREATE TRIGGER reject_manager_mapping BEFORE INSERT ON local_values WHEN NEW.key LIKE 'manager-admission:%' BEGIN SELECT RAISE(ABORT,'manager mapping failure'); END")?;
        assert!(fixture.claim("boot").is_err());
        assert_eq!(
            fixture.store.connection.query_row(
                "SELECT count(*) FROM accepted_authorizations WHERE action='agent_start'",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            0
        );
        let scheduler_after = fixture
            .store
            .connection
            .prepare("SELECT key,value FROM local_values ORDER BY key")?
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(scheduler_after, scheduler_before);
        assert_eq!(
            fixture.store.connection.query_row(
                "SELECT count(*) FROM authorization_nonces WHERE authorization_id=?",
                [fixture.auth.operation_id()],
                |row| row.get::<_, u64>(0)
            )?,
            0
        );
        assert!(!fixture.store.connection.query_row(
            "SELECT consumed FROM authorization_challenges WHERE space_id=? AND nonce=?",
            params![
                fixture.network.space.as_str(),
                fixture.proof.challenge.nonce.as_str()
            ],
            |row| row.get::<_, bool>(0)
        )?);
        fixture
            .store
            .connection
            .execute_batch("DROP TRIGGER reject_manager_mapping")?;
        assert!(fixture.claim("boot")?.fresh);
        Ok(())
    }

    #[test]
    fn manager_renewal_uses_original_admission_and_stops_after_grant_revocation() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let first = fixture.claim("boot")?;
        let space = fixture.network.space.as_str();
        let auth = fixture
            .store
            .manager_authorization(space, &first.manager_id, now() + 5_000)?;
        let proof = fixture.network.freshness(
            &mut fixture.store,
            &auth,
            CheckStage::Renew,
            now() + 5_000,
        )?;
        let deadline = fixture.store.renew_authorized_manager(
            space,
            &first.manager_id,
            &proof,
            now() + 5_000,
        )?;
        assert_eq!(deadline, now() + 35_000);
        assert!(
            fixture
                .store
                .manager_authorization(space, &first.manager_id, now() + 10_001)
                .is_ok()
        );
        let mut document = fixture
            .store
            .authorization_state()
            .space(&SpaceId::parse(space)?)
            .unwrap()
            .clone();
        document.previous = fixture
            .store
            .authorization_state()
            .head(&PolicyDomain::Space(SpaceId::parse(space)?))
            .cloned();
        document.sequence += 1;
        document
            .grants
            .retain(|hash| hash != &fixture.service_grant);
        fixture.store.install_policy_journal(
            &[JournalEntry::Space(Signed::sign(
                document,
                &fixture.network.root,
            )?)],
            now() + 6_000,
        )?;
        assert!(
            fixture
                .store
                .manager_authorization(space, &first.manager_id, now() + 10_001)
                .is_err()
        );
        assert_eq!(
            fixture.store.connection.query_row(
                "SELECT count(*) FROM accepted_authorizations WHERE action='agent_start'",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            1
        );
        Ok(())
    }
}
