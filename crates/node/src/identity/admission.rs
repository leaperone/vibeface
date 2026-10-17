use super::{
    event_authorization::{append_admitted_dispatch_in, current_event_authorization},
    journal::PolicyState,
    policy::{FreshnessProof, request_facts_from},
};
use crate::{
    crypto::LocalIdentity,
    engine::{EngineAction, ExecutorKind},
    model::{
        AdmissionProof, AgentSession, ExecutionSession, MAX_EVENT_BYTES, OperationRecord,
        OperationRequest, OperationState, StoredTransport,
    },
    scheduler::{self, Invocation as SchedulerInvocation, InvocationState, LocalSession},
    store::{
        Store, canonical_resource, closure_hashes, has_blob_from, heads_from_events,
        load_events_from, paths_overlap, validate_operation_binding_from,
    },
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use vibeface_secure::authorization::{
    Action, Actor, CheckStage, ObjectHash, PolicyDomain, RequestFacts, SessionRef,
    TransportEvidence, VerifiedAdmission, VerifiedAuthorization, VerifiedPolicySet,
};

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
    check_challenge_record(tx, auth, proof, false)?;
    ensure!(tx.execute(
        "UPDATE authorization_challenges SET consumed=1 WHERE space_id=? AND nonce=? AND consumed=0",
        params![auth.resource().space.as_str(), proof.challenge.nonce.as_str()],
    )? == 1, "freshness challenge was already consumed");
    Ok(freshness.expires_at_ms().min(auth.expires_at_ms()))
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
    let state = policy.verified();
    source_matches(auth, source_body)?;
    if source_body["method"] != "agent_start" {
        check_manager_tool_in(tx, policy, auth, transport, now_ms)?;
    }
    let signed = auth.signed_invocation();
    let facts = request_facts_from(tx, state, source_body, auth.audience())?;
    let checked = state.verify_invocation(signed, &facts, transport, now_ms)?;
    ensure!(
        checked.invocation_hash() == auth.invocation_hash(),
        "authorization proof changed"
    );
    let freshness = state.verify_freshness(auth, &proof.challenge, &proof.receipts, now_ms)?;
    state.admit_operation(auth, &freshness, now_ms)?;
    consume_challenge_in(tx, state, auth, proof, CheckStage::Admit, now_ms)?;
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
            serde_json::to_string(source_body)?, serde_json::to_string(&StoredTransport::from(transport))?,
            serde_json::to_string(&proof.challenge)?, serde_json::to_string(&proof.receipts)?,
            now_ms, operation_id],
    )?;
    tx.execute(
        "INSERT INTO authorization_nonces(space_id,device_id,nonce,authorization_id,body_hash) VALUES(?,?,?,?,?)",
        params![auth.resource().space.as_str(), auth.device().as_str(), auth.nonce().as_str(),
            auth.operation_id(), auth.body_hash().as_str()],
    )?;
    Ok(AdmissionProof {
        transport: StoredTransport::from(transport),
        invocation: signed.clone(),
        source_body: source_body.clone(),
        challenge: proof.challenge.clone(),
        receipts: proof.receipts.clone(),
        admitted_at_ms: now_ms,
    })
}

pub(crate) fn reconstruct_admission(
    policy: &PolicyState,
    proof: &AdmissionProof,
    facts: &RequestFacts,
    transport: &TransportEvidence,
) -> Result<VerifiedAdmission> {
    ensure!(
        proof.challenge.stage == CheckStage::Admit,
        "stored admission has incorrect freshness stage"
    );
    ensure!(
        proof.transport.clone().into_evidence() == *transport,
        "admission transport differs from captured evidence"
    );
    let historical = policy.historical(&proof.challenge.policies, proof.admitted_at_ms)?;
    let auth =
        historical.verify_invocation(&proof.invocation, facts, transport, proof.admitted_at_ms)?;
    source_matches(&auth, &proof.source_body)?;
    let freshness = historical.verify_freshness(
        &auth,
        &proof.challenge,
        &proof.receipts,
        proof.admitted_at_ms,
    )?;
    Ok(historical.admit_operation(&auth, &freshness, proof.admitted_at_ms)?)
}

struct StoredAdmission {
    proof: AdmissionProof,
    transport: TransportEvidence,
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
    Ok(StoredAdmission {
        proof,
        transport: transport.into_evidence(),
    })
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

fn operation_from(
    connection: &Connection,
    space: &str,
    operation_id: &str,
) -> Result<Option<OperationRecord>> {
    let row: Option<(String, String, String, String, String)> = connection
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

fn validate_basis_in(connection: &Connection, request: &OperationRequest) -> Result<()> {
    validate_operation_binding_from(connection, request)?;
    let events = load_events_from(connection, &request.network_id)?;
    let heads = if request.basis_heads.is_empty() {
        heads_from_events(&events, &request.conversation_id)
    } else {
        request.basis_heads.clone()
    };
    for head in &heads {
        ensure!(
            events
                .get(head)
                .is_some_and(|event| event.event.scope_id == request.conversation_id),
            "operation basis is outside its conversation"
        );
    }
    for hash in closure_hashes(&events, &heads) {
        let event = events.get(&hash).context("operation basis is incomplete")?;
        ensure!(
            event.status == "ready",
            "operation basis is incomplete or conflicting"
        );
        for blob in &event.event.blobs {
            ensure!(
                has_blob_from(connection, &request.network_id, blob)?,
                "operation basis content is incomplete"
            );
        }
    }
    Ok(())
}

fn checked_session_from(
    connection: &Connection,
    request: &OperationRequest,
) -> Result<Option<ExecutionSession>> {
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
                    && session.session_id == request.session_id
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
    let historical = policy.historical(&lease.proof.challenge.policies, lease.checked_at_ms)?;
    let facts = request_facts_from(
        connection,
        &historical,
        &stored.proof.source_body,
        &stored.proof.invocation.payload.audience,
    )?;
    let auth = if lease.proof.challenge.stage == CheckStage::Admit {
        ensure!(
            lease.checked_at_ms == stored.proof.admitted_at_ms
                && lease.proof.challenge == stored.proof.challenge
                && lease.proof.receipts == stored.proof.receipts,
            "initial manager lease differs from its original admission"
        );
        historical.verify_invocation(
            &stored.proof.invocation,
            &facts,
            &stored.transport,
            lease.checked_at_ms,
        )?
    } else {
        let admission = reconstruct_admission(policy, &stored.proof, &facts, &stored.transport)?;
        historical.verify_operation_authorization(
            &admission,
            &facts,
            &stored.transport,
            lease.checked_at_ms,
        )?
    };
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
    pub(crate) invocation: SchedulerInvocation,
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
        auth.operation_id() == format!("manager:{trigger}"),
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
    let events = load_events_from(connection, space)?;
    let event = events.get(trigger).context("manager trigger is missing")?;
    ensure!(
        event.status == "ready"
            && event.event.scope_id == conversation
            && event.event.resource_id == auth.resource().resource
            && matches!(&event.event.body, crate::model::EventBody::MessagePosted { role, text, .. } if role == "user" && !text.trim().is_empty()),
        "manager trigger is not a ready account message in its resource"
    );
    let parent = current_event_authorization(policy, connection, &event.event, &events, now_ms)?;
    ensure!(
        matches!(parent.actor(), Actor::Account(_))
            && parent.resource() == auth.resource()
            && parent.actor() == auth.initiator()
            && parent.invocation_hash() == &descriptor.parent_invocation
            && auth
                .grant_path()
                .contains(&parent.signed_invocation().payload.grant_leaf),
        "manager delegation differs from its current account parent"
    );
    Ok(())
}

fn load_manager_from(
    connection: &Connection,
    space: &str,
    manager_id: &str,
) -> Result<(ManagerClaim, StoredAdmission, SchedulerInvocation)> {
    let trigger = manager_id
        .strip_prefix("manager:")
        .context("invalid manager authorization ID")?;
    let claim: ManagerClaim =
        local_from(connection, space, &format!("manager-admission:{trigger}"))?
            .context("manager has no admitted trigger claim")?;
    ensure!(
        claim.manager_id == manager_id && claim.trigger == trigger,
        "manager admission mapping mismatch"
    );
    let stored = load_authorization_from(connection, space, manager_id)?;
    ensure!(
        manager_field(&stored.proof.source_body, "conversation_id")? == claim.conversation_id
            && manager_field(&stored.proof.source_body, "trigger")? == claim.trigger,
        "manager admission source mismatch"
    );
    let invocation: SchedulerInvocation = local_from(
        connection,
        space,
        &scheduler::key("invocation", &claim.conversation_id, &claim.trigger),
    )?
    .context("manager invocation record is missing")?;
    ensure!(
        invocation.trigger == claim.trigger
            && invocation.message_id
                == format!(
                    "scheduler-message-{}",
                    crate::crypto::digest(&serde_json::to_vec(&(
                        space,
                        &claim.conversation_id,
                        &claim.trigger
                    ))?)
                ),
        "manager invocation metadata does not identify its admitted trigger"
    );
    Ok((claim, stored, invocation))
}

fn manager_lease_from(
    connection: &Connection,
    policy: &PolicyState,
    space: &str,
    manager_id: &str,
) -> Result<u64> {
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
    verify_lease_record(connection, policy, &stored, &lease)
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
    let admitted = reconstruct_admission(policy, &stored.proof, &facts, &stored.transport)?;
    let auth = policy.verified().verify_operation_authorization(
        &admitted,
        &facts,
        &stored.transport,
        now_ms,
    )?;
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
    let (claim, stored, invocation) = load_manager_from(connection, space, manager_id)?;
    ensure!(
        invocation.state == InvocationState::Running,
        "manager is no longer running"
    );
    let session: LocalSession = local_from(
        connection,
        space,
        &scheduler::key("session", &claim.conversation_id, ""),
    )?
    .context("manager session is missing")?;
    ensure!(
        session
            .active
            .as_ref()
            .is_some_and(|active| active.trigger == invocation.trigger
                && active.boot == invocation.boot
                && active.message_id == invocation.message_id
                && active.state == InvocationState::Running)
            && session.sdk_config.as_deref()
                == Some(manager_field(&stored.proof.source_body, "sdk_config")?),
        "manager execution session changed before tool authorization"
    );
    ensure!(
        now_ms < manager_lease_from(connection, policy, space, manager_id)?,
        "manager lease already expired"
    );
    Ok(auth)
}

fn check_manager_tool_in(
    connection: &Connection,
    policy: &PolicyState,
    auth: &VerifiedAuthorization,
    transport: &TransportEvidence,
    now_ms: u64,
) -> Result<()> {
    let TransportEvidence::LocalAgentAdapter { service, session } = transport else {
        return Ok(());
    };
    let manager = manager_tool_authorization_from(
        connection,
        policy,
        auth.resource().space.as_str(),
        &format!("manager:{}", session.session_id),
        now_ms,
    )?;
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
    Ok(())
}

impl Store {
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
        let invocation_key = scheduler::key("invocation", conversation, trigger);
        let session_key = scheduler::key("session", conversation, "");
        let manager_key = format!("manager-admission:{trigger}");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_policy_heads_in(&tx, self.policy.verified(), auth, now_ms)?;
        check_manager_parent(&tx, &self.policy, auth, source_body, now_ms)?;
        if local_from::<ManagerClaim>(&tx, space, &manager_key)?.is_some() {
            let (claim, stored, mut previous) = load_manager_from(&tx, space, auth.operation_id())?;
            ensure!(
                stored.proof.source_body == *source_body && stored.transport == *transport,
                "manager trigger was reused with a different request"
            );
            check_challenge_record(&tx, auth, proof, stored.proof.challenge == proof.challenge)?;
            let deadline = manager_lease_from(&tx, &self.policy, space, &claim.manager_id)?;
            if previous.state == InvocationState::Running && previous.boot != boot {
                previous.state = InvocationState::Unknown;
                set_local_in(&tx, space, &invocation_key, &previous)?;
                let mut session: LocalSession =
                    local_from(&tx, space, &session_key)?.context("manager session is missing")?;
                if session
                    .active
                    .as_ref()
                    .is_some_and(|active| active.trigger == trigger)
                {
                    session.active = Some(previous.clone());
                    session.native_id = None;
                    set_local_in(&tx, space, &session_key, &session)?;
                }
            }
            tx.commit()?;
            return Ok(ManagerAdmission {
                manager_id: claim.manager_id,
                invocation: previous,
                previous_native_id: None,
                fresh: false,
                lease_expires_at_ms: deadline,
            });
        }
        ensure!(
            local_from::<SchedulerInvocation>(&tx, space, &invocation_key)?.is_none(),
            "manager trigger exists without admitted authorization"
        );
        let mut session: LocalSession = local_from(&tx, space, &session_key)?.unwrap_or_default();
        if let Some(active) = session.active.take() {
            let old_id = format!("manager:{}", active.trigger);
            let (_, _, mut old) = load_manager_from(&tx, space, &old_id)?;
            if old.state == InvocationState::Running {
                ensure!(
                    old.boot != boot,
                    "a manager invocation is already running for this conversation"
                );
                old.state = InvocationState::Unknown;
                set_local_in(
                    &tx,
                    space,
                    &scheduler::key("invocation", conversation, &old.trigger),
                    &old,
                )?;
            }
            if old.state == InvocationState::Unknown {
                session.native_id = None;
            }
        }
        if session.sdk_config.as_deref() != Some(sdk_config) {
            session.native_id = None;
            session.sdk_config = Some(sdk_config.into());
        }
        consume_authorization_in(
            &tx,
            &self.policy,
            auth,
            proof,
            transport,
            source_body,
            None,
            now_ms,
        )?;
        let invocation = SchedulerInvocation {
            state: InvocationState::Running,
            boot: boot.into(),
            trigger: trigger.into(),
            message_id: format!(
                "scheduler-message-{}",
                crate::crypto::digest(&serde_json::to_vec(&(space, conversation, trigger))?)
            ),
        };
        let previous_native_id = session.native_id.clone();
        session.active = Some(invocation.clone());
        set_local_in(&tx, space, &invocation_key, &invocation)?;
        set_local_in(&tx, space, &session_key, &session)?;
        set_local_in(
            &tx,
            space,
            &manager_key,
            &ManagerClaim {
                manager_id: auth.operation_id().into(),
                conversation_id: conversation.into(),
                trigger: trigger.into(),
            },
        )?;
        let deadline = freshness.expires_at_ms().min(auth.expires_at_ms());
        save_lease_in(&tx, space, auth.operation_id(), proof, now_ms, deadline)?;
        tx.commit()?;
        Ok(ManagerAdmission {
            manager_id: auth.operation_id().into(),
            invocation,
            previous_native_id,
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
        let (claim, stored, invocation) = load_manager_from(&tx, space, manager_id)?;
        ensure!(
            invocation.state == InvocationState::Running
                && stored.proof.invocation == *auth.signed_invocation(),
            "only the original running manager can renew its lease"
        );
        let session: LocalSession = local_from(
            &tx,
            space,
            &scheduler::key("session", &claim.conversation_id, ""),
        )?
        .context("manager session is missing")?;
        ensure!(
            session
                .active
                .as_ref()
                .is_some_and(|active| active.trigger == invocation.trigger
                    && active.boot == invocation.boot
                    && active.state == InvocationState::Running)
                && session.sdk_config.as_deref()
                    == Some(manager_field(&stored.proof.source_body, "sdk_config")?),
            "manager execution session changed before renewal"
        );
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
        ensure!(
            proof.challenge.stage == CheckStage::RemoteRead && auth.action().is_read(),
            "read requires remote-read freshness"
        );
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        check_manager_tool_in(&tx, &self.policy, auth, transport, now_ms)?;
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
            !request.operation_id.is_empty() && !request.session_id.is_empty(),
            "operation and session IDs are required"
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
        let checked = self.policy.verified().verify_invocation(
            auth.signed_invocation(),
            &facts,
            transport,
            now_ms,
        )?;
        ensure!(
            checked.invocation_hash() == auth.invocation_hash(),
            "operation authorization mismatch"
        );
        let action = request.action["type"]
            .as_str()
            .context("operation action is required")?;
        let control = !matches!(action, "open" | "send");
        if action != "open" {
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
        validate_basis_in(&tx, request)?;
        let session = checked_session_from(&tx, request)?;
        let session = if action == "open" {
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
            ExecutionSession {
                network_id: request.network_id.clone(),
                target_node: request.target_node.clone(),
                session_id: request.session_id.clone(),
                revision: 0,
                state: "opening".into(),
                executor: serde_json::to_value(executor)?
                    .as_str()
                    .context("invalid executor")?
                    .into(),
                cwd: cwd.into(),
                native_session_id: None,
            }
        } else {
            session.context("execution session is not open")?
        };
        if !control {
            ensure!(
                resources.iter().any(|resource| resource == &session.cwd),
                "execution directory is not reserved"
            );
        }
        let busy: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE space_id=? AND target_node=? AND session_id=? AND state IN ('\"accepted\"','\"running\"','\"unknown\"') AND (?=0 OR control=1))",
            params![request.network_id, request.target_node, request.session_id, control], |row| row.get(0),
        )?;
        ensure!(!busy, "execution session has an unresolved operation");
        let reserved = tx
            .prepare("SELECT resource FROM resource_reservations WHERE target_node=?")?
            .query_map([&request.target_node], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            !resources.iter().any(|resource| reserved
                .iter()
                .any(|existing| paths_overlap(resource, existing))),
            "resource overlaps an unresolved operation on this device"
        );
        let admission = consume_authorization_in(
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
            tx.execute("INSERT INTO execution_sessions(space_id,target_node,session_id,revision,record) VALUES(?,?,?,?,?)",
                params![request.network_id, request.target_node, request.session_id, session.revision, serde_json::to_string(&session)?])?;
        }
        append_admitted_dispatch_in(&tx, identity, &admission, request, now_ms)?;
        tx.commit()?;
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
            reconstruct_admission(&self.policy, &stored.proof, &facts, &stored.transport)?;
        Ok(self.policy.verified().verify_operation_authorization(
            &admission,
            &facts,
            &stored.transport,
            now_ms,
        )?)
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
        validate_basis_in(&tx, &operation.request)?;
        let session = checked_session_from(&tx, &operation.request)?
            .context("accepted operation has no execution session")?;
        ensure!(
            canonical_resource(&session.cwd)? == session.cwd,
            "execution directory changed before claim"
        );
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
            let receipts = self
                .auth
                .dependencies()
                .iter()
                .map(|current| {
                    Ok(Signed::sign(
                        FreshnessReceipt {
                            version: AUTH_VERSION,
                            current: current.clone(),
                            challenge_hash: challenge.hash()?,
                            issued_at_ms: NOW,
                            expires_at_ms: challenge.expires_at_ms,
                        },
                        &self.authority,
                    )?)
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(FreshnessProof {
                challenge,
                receipts,
            })
        }
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
        let historical =
            reconstruct_admission(&fixture.policy, &admitted, &facts, &fixture.transport)?;
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
            Some("operation_1"),
            NOW,
        )?;
        tx.commit()?;
        let facts = request_facts_from(
            &fixture.connection,
            &fixture.state,
            &fixture.body,
            fixture.auth.audience(),
        )?;
        let admission =
            reconstruct_admission(&fixture.policy, &admitted, &facts, &fixture.transport)?;
        let auth = fixture.state.verify_operation_authorization(
            &admission,
            &facts,
            &fixture.transport,
            NOW + 1,
        )?;
        let challenge = FreshnessChallenge::new(
            &auth,
            CheckStage::Start,
            Nonce::random()?,
            NOW + 1,
            NOW + 20_000,
        )?;
        let receipts = auth
            .dependencies()
            .iter()
            .map(|current| {
                Ok(Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: current.clone(),
                        challenge_hash: challenge.hash()?,
                        issued_at_ms: NOW + 1,
                        expires_at_ms: NOW + 15_000,
                    },
                    &fixture.authority,
                )?)
            })
            .collect::<Result<Vec<_>>>()?;
        let proof = FreshnessProof {
            challenge,
            receipts,
        };
        let space = auth.resource().space.as_str();
        fixture.connection.execute("INSERT INTO authorization_challenges(space_id,nonce,invocation_hash,stage,challenge,expires_at_ms) VALUES(?,?,?,'start',?,?)",
            params![space, proof.challenge.nonce.as_str(), auth.invocation_hash().as_str(), serde_json::to_string(&proof.challenge)?, proof.challenge.expires_at_ms])?;
        let tx = fixture
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deadline = consume_challenge_in(
            &tx,
            &fixture.state,
            &auth,
            &proof,
            CheckStage::Start,
            NOW + 1,
        )?;
        assert_eq!(deadline, NOW + 15_000);
        save_lease_in(&tx, space, "operation_1", &proof, NOW + 1, deadline)?;
        tx.commit()?;
        assert_eq!(
            lease_deadline_from(&fixture.connection, &fixture.policy, space, "operation_1")?,
            NOW + 15_000
        );
        fixture.connection.execute("UPDATE local_values SET value=json_set(value,'$.deadline',?) WHERE space_id=? AND key='lease:operation_1'", params![NOW + 20_000, space])?;
        assert!(
            lease_deadline_from(&fixture.connection, &fixture.policy, space, "operation_1")
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
        let receipts = auth
            .dependencies()
            .iter()
            .map(|current| {
                Ok(Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: current.clone(),
                        challenge_hash: challenge.hash()?,
                        issued_at_ms: NOW,
                        expires_at_ms: NOW + 60_000,
                    },
                    &fixture.authority,
                )?)
            })
            .collect::<Result<Vec<_>>>()?;
        fixture.connection.execute("INSERT INTO authorization_challenges(space_id,nonce,invocation_hash,stage,challenge,expires_at_ms) VALUES(?,?,?,'remote_read',?,?)",
            params![auth.resource().space.as_str(), challenge.nonce.as_str(), auth.invocation_hash().as_str(), serde_json::to_string(&challenge)?, challenge.expires_at_ms])?;
        let proof = FreshnessProof {
            challenge,
            receipts,
        };
        let store = Store {
            connection: fixture.connection,
            policy: fixture.policy,
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
            let network = TestNetwork::new()?;
            let mut store = network.memory()?;
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
            body["authorization_id"] = json!(format!("manager:{}", trigger.hash));
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
            self.store.claim_authorized_manager(
                &self.auth,
                &self.proof,
                &self.transport,
                &self.body,
                boot,
                &self.sdk_config,
                now(),
            )
        }
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
        let key = scheduler::key("session", "conversation", "");
        let mut session: LocalSession =
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
        session.active.as_mut().unwrap().state = InvocationState::Unknown;
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
                && duplicate.invocation.state == InvocationState::Running
        );
        let space = fixture.network.space.as_str();
        let session_key = scheduler::key("session", "conversation", "");
        let mut session: LocalSession =
            local_from(&fixture.store.connection, space, &session_key)?.unwrap();
        session.native_id = Some("native-manager".into());
        fixture
            .store
            .local_set(space, &session_key, &serde_json::to_value(session)?)?;
        let resumed = fixture.claim("boot-b")?;
        assert!(!resumed.fresh && resumed.invocation.state == InvocationState::Unknown);
        let session: LocalSession = local_from(
            &fixture.store.connection,
            fixture.network.space.as_str(),
            &session_key,
        )?
        .unwrap();
        assert!(
            session.native_id.is_none()
                && session.active.unwrap().state == InvocationState::Unknown
        );
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
        assert_eq!(fixture.store.connection.query_row("SELECT count(*) FROM local_values WHERE key LIKE 'scheduler:%' OR key LIKE 'manager-admission:%'", [], |row| row.get::<_,u64>(0))?,0);
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
