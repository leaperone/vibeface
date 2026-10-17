use super::{admission::reconstruct_admission, journal::PolicyState, policy::FreshnessProof};
use crate::{
    crypto::{self, LocalIdentity},
    model::{
        AdmissionProof, BlobRef, Event, EventBody, EventCause, OperationRequest, SignedEvent,
        WIRE_VERSION,
    },
    store::{
        Store, StoredEvent, closure_hashes, has_blob_from, heads_from_events, load_events_from,
        rebuild_projections_from,
    },
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use vibeface_secure::authorization::{
    Actor, DeviceUse, InvocationContext, ObjectHash, PolicyDomain, PublicKey, RequestFacts,
    ResourceId, ResourceKind, ResourceRef, SessionRef, SpaceId, TransportEvidence,
    VerifiedAuthorization, VerifiedPolicySet,
};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EventEvidenceStatus {
    Valid,
    Pending,
}

fn admission(event: &Event) -> &AdmissionProof {
    match &event.cause {
        EventCause::Invocation { admission } | EventCause::Derived { admission, .. } => admission,
    }
}

fn text<'a>(body: &'a Value, field: &str) -> Result<&'a str> {
    body[field]
        .as_str()
        .with_context(|| format!("{field} is required"))
}

fn audience_key(state: &VerifiedPolicySet, proof: &AdmissionProof) -> Result<PublicKey> {
    let audience = &proof.invocation.payload.audience;
    for stamp in &proof.challenge.policies {
        let devices = match &stamp.domain {
            PolicyDomain::Account(id) => state
                .account(id)
                .map(|document| document.devices.as_slice()),
            PolicyDomain::Space(id) => state.space(id).map(|document| document.devices.as_slice()),
        };
        for hash in devices.into_iter().flatten() {
            if let Some(certificate) = state.device(hash)
                && certificate.payload.device == audience.device
            {
                ensure!(
                    certificate.payload.subject == Actor::Service(audience.service.clone())
                        && certificate.payload.uses.contains(&DeviceUse::Noise),
                    "event audience is not a registered service device"
                );
                return Ok(certificate.payload.public_key.clone());
            }
        }
    }
    bail!("event audience device is missing from admitted policy")
}

pub(crate) fn verify_event_envelope(
    policy: &PolicyState,
    space: &str,
    envelope: &SignedEvent,
) -> Result<Event> {
    ensure!(
        envelope.canonical_body.len() <= crate::model::MAX_EVENT_BYTES,
        "event too large"
    );
    let event: Event = serde_json::from_str(&envelope.canonical_body)?;
    let proof = admission(&event);
    let historical = policy.historical(&proof.challenge.policies, proof.admitted_at_ms)?;
    crypto::verify_signed_event(space, envelope, audience_key(&historical, proof)?.as_str())
}

fn source_scope(body: &Value) -> Result<&str> {
    if text(body, "method")? == "operate" {
        text(&body["request"], "conversation_id")
    } else {
        text(body, "conversation_id")
    }
}

fn expected_events(
    body: &Value,
    actor: &Actor,
    transport: &TransportEvidence,
) -> Result<Vec<(EventBody, Vec<BlobRef>)>> {
    let plain = |event| (event, Vec::new());
    Ok(match text(body, "method")? {
        "create_conversation" => vec![plain(EventBody::ConversationCreated {
            title: text(body, "title")?.into(),
        })],
        "message" => vec![plain(EventBody::MessagePosted {
            message_id: text(body, "message_id")?.into(),
            role: "user".into(),
            text: text(body, "text")?.into(),
            scheduler_node: Some(text(body, "scheduler_node")?.into()),
        })],
        "create_task" => {
            let task = text(body, "task_id")?;
            let mut events = vec![plain(EventBody::TaskCreated {
                task_id: task.into(),
                title: text(body, "title")?.into(),
                goal: text(body, "goal")?.into(),
                acceptance: text(body, "acceptance")?.into(),
            })];
            if body["auto_followup"].as_bool() == Some(true) {
                ensure!(
                    matches!(actor, Actor::Account(_)),
                    "automatic follow-up requires an account actor"
                );
                events.push(plain(EventBody::DecisionRecorded {
                    decision_id: format!("auto:{task}"),
                    text: format!("User enabled bounded automatic follow-up for task {task}"),
                    effective_constraints: vec![format!("auto_followup:{task}")],
                    tool_configuration: json!({"auto_followup":task}),
                }));
            }
            events
        }
        "update_task" => vec![plain(EventBody::TaskChanged {
            task_id: text(body, "task_id")?.into(),
            status: text(body, "status")?.into(),
            detail: text(body, "detail")?.into(),
        })],
        "ack" => vec![plain(EventBody::NotificationAcknowledged {
            notification_id: text(body, "notification_id")?.into(),
            client_id: text(body, "client_id")?.into(),
        })],
        "operate" => {
            let request: OperationRequest = serde_json::from_value(body["request"].clone())?;
            vec![plain(EventBody::DispatchRecorded {
                operation_id: request.operation_id.clone(),
                task_id: request.task_id.clone(),
                target_node: request.target_node.clone(),
                session_id: request.session_id.clone(),
                action: serde_json::to_value(request)?,
            })]
        }
        "agent_result" => {
            ensure!(
                matches!((actor, transport), (Actor::Service(actor), TransportEvidence::LocalAgentAdapter { service, .. }) if actor == service),
                "manager output requires a service adapter admission"
            );
            text(body, "parent_invocation")?;
            let blobs: Vec<BlobRef> = serde_json::from_value(body["blobs"].clone())?;
            vec![
                (
                    EventBody::DecisionRecorded {
                        decision_id: text(body, "decision_id")?.into(),
                        text: text(body, "text")?.into(),
                        effective_constraints: serde_json::from_value(
                            body["effective_constraints"].clone(),
                        )?,
                        tool_configuration: body
                            .get("tool_configuration")
                            .context("tool_configuration is required")?
                            .clone(),
                    },
                    blobs.clone(),
                ),
                (
                    EventBody::MessagePosted {
                        message_id: text(body, "message_id")?.into(),
                        role: "assistant".into(),
                        text: text(body, "output")?.into(),
                        scheduler_node: None,
                    },
                    blobs,
                ),
            ]
        }
        "agent_notification" => {
            ensure!(
                matches!((actor, transport), (Actor::Service(actor), TransportEvidence::LocalAgentAdapter { service, .. }) if actor == service),
                "manager notification requires a service adapter admission"
            );
            text(body, "parent_invocation")?;
            vec![plain(EventBody::NotificationRaised {
                notification_id: text(body, "notification_id")?.into(),
                kind: text(body, "kind")?.into(),
                text: text(body, "text")?.into(),
            })]
        }
        _ => bail!("request does not authorize a business event"),
    })
}

fn dependency_closure(
    event: &Event,
    events: &BTreeMap<String, StoredEvent>,
) -> Result<Option<BTreeSet<String>>> {
    let mut pending = event.parents.clone();
    pending.extend(event.previous.iter().cloned());
    let mut closure = BTreeSet::new();
    while let Some(hash) = pending.pop() {
        if !closure.insert(hash.clone()) {
            continue;
        }
        let Some(prior) = events.get(&hash) else {
            return Ok(None);
        };
        ensure!(
            prior.event.network_id == event.network_id,
            "event dependency belongs to another space"
        );
        pending.extend(prior.event.parents.iter().cloned());
        pending.extend(prior.event.previous.iter().cloned());
    }
    Ok(Some(closure))
}

fn historical_facts(
    state: &VerifiedPolicySet,
    event: &Event,
    events: &BTreeMap<String, StoredEvent>,
    ancestors: &BTreeSet<String>,
) -> Result<Option<RequestFacts>> {
    let proof = admission(event);
    let body = &proof.source_body;
    let space = SpaceId::parse(text(body, "space_id")?)?;
    let resource = ResourceId::parse(text(body, "resource_id")?)?;
    ensure!(
        space.as_str() == event.network_id && resource == event.resource_id,
        "event resource differs from signed request"
    );
    let scope = source_scope(body)?;
    ensure!(
        scope == event.scope_id,
        "event conversation differs from signed request"
    );
    let record = state
        .resource(&space, &resource)
        .context("event resource is not registered by admitted space policy")?;
    ensure!(
        record.payload.kind == ResourceKind::Project,
        "conversation events require a project resource"
    );
    ensure!(
        record.payload.host == proof.invocation.payload.audience.service,
        "event targets another resource host"
    );
    let mut creations = ancestors.iter().filter_map(|hash| {
        let prior = &events[hash];
        (prior.event.scope_id == scope
            && matches!(prior.event.body, EventBody::ConversationCreated { .. }))
        .then_some(&prior.event)
    });
    if text(body, "method")? == "create_conversation" {
        ensure!(
            creations.next().is_none(),
            "conversation already has a causal creation"
        );
    } else {
        let Some(creation) = creations.next() else {
            return Ok(None);
        };
        ensure!(
            creations.next().is_none(),
            "conversation has conflicting creations"
        );
        ensure!(
            creation.resource_id == resource,
            "conversation resource differs from signed request"
        );
    }
    let request = (text(body, "method")? == "operate")
        .then(|| serde_json::from_value::<OperationRequest>(body["request"].clone()))
        .transpose()?;
    let mut context = InvocationContext {
        conversation: Some(scope.into()),
        workspace_resource: Some(resource.clone()),
        ..Default::default()
    };
    let task = request
        .as_ref()
        .and_then(|request| request.task_id.as_deref())
        .or_else(|| body["task_id"].as_str());
    if let Some(task) = task
        && text(body, "method")? != "create_task"
    {
        let mut tasks = ancestors.iter().filter_map(|hash| {
            let prior = &events[hash];
            (prior.event.scope_id == scope && matches!(&prior.event.body, EventBody::TaskCreated { task_id, .. } if task_id == task)).then_some(prior)
        });
        let Some(creation) = tasks.next() else {
            return Ok(None);
        };
        ensure!(tasks.next().is_none(), "task has conflicting creations");
        ensure!(
            creation.event.resource_id == resource,
            "task belongs to another resource"
        );
        if let Some(reference) = body["task_ref"].as_str() {
            ensure!(
                reference == creation.envelope.hash,
                "task creation reference mismatch"
            );
        }
        context.task = Some(ObjectHash::body(&creation.envelope)?);
    }
    if let Some(request) = request {
        ensure!(
            request.network_id == space.as_str()
                && request.operation_id == text(body, "authorization_id")?,
            "operation authorization scope mismatch"
        );
        context.sdk_session = Some(SessionRef {
            target: proof.invocation.payload.audience.device.clone(),
            session_id: request.session_id.clone(),
        });
        context.executor_kind = request.action["executor"].as_str().map(str::to_owned);
        ensure!(
            request.target_node == crypto::fingerprint(audience_key(state, proof)?.as_str())?,
            "execution targets another service device"
        );
        if context.executor_kind.is_none() {
            let executors: BTreeSet<_> = ancestors
                .iter()
                .filter_map(|hash| {
                    let prior = &events[hash].event;
                    let EventBody::DispatchRecorded {
                        target_node,
                        session_id,
                        action,
                        ..
                    } = &prior.body
                    else {
                        return None;
                    };
                    (prior.scope_id == scope
                        && target_node == &request.target_node
                        && session_id == &request.session_id
                        && action["action"]["type"].as_str() == Some("open"))
                    .then(|| action["action"]["executor"].as_str().map(str::to_owned))
                    .flatten()
                })
                .collect();
            ensure!(
                executors.len() <= 1,
                "execution session has conflicting executors"
            );
            context.executor_kind = executors.into_iter().next();
            if context.executor_kind.is_none() {
                return Ok(None);
            }
        }
    } else if !body["sdk_session"].is_null() {
        context.sdk_session = Some(serde_json::from_value(body["sdk_session"].clone())?);
    }
    Ok(Some(RequestFacts {
        resource: ResourceRef { space, resource },
        action: crate::model::authorization_action(body)?,
        audience: proof.invocation.payload.audience.clone(),
        operation_id: text(body, "authorization_id")?.into(),
        body_hash: ObjectHash::body(body)?,
        context,
    }))
}

fn event_slot(body: &EventBody) -> String {
    match body {
        EventBody::ExecutionObserved {
            operation_id,
            status,
            ..
        } => format!("execution:{operation_id}:{status}"),
        EventBody::NotificationRaised {
            notification_id, ..
        } => format!("notification:{notification_id}"),
        EventBody::ConversationCreated { .. } => "conversation_created".into(),
        EventBody::MessagePosted { .. } => "message_posted".into(),
        EventBody::TaskCreated { .. } => "task_created".into(),
        EventBody::TaskChanged { .. } => "task_changed".into(),
        EventBody::DispatchRecorded { .. } => "dispatch_recorded".into(),
        EventBody::DecisionRecorded { .. } => "decision_recorded".into(),
        EventBody::NotificationAcknowledged { .. } => "notification_acknowledged".into(),
        EventBody::ContextCheckpoint { .. } => "context_checkpoint".into(),
    }
}

fn validate_derived(event: &Event, source: &Event) -> Result<()> {
    ensure!(
        event.scope_id == source.scope_id
            && event.resource_id == source.resource_id
            && admission(event) == admission(source),
        "derived event does not preserve its source admission"
    );
    match (&event.body, &source.body) {
        (
            EventBody::ExecutionObserved {
                operation_id,
                task_id,
                target_node,
                session_id,
                status,
                detail,
            },
            EventBody::DispatchRecorded {
                operation_id: source_operation,
                task_id: source_task,
                target_node: source_target,
                session_id: source_session,
                ..
            },
        ) => {
            ensure!(
                operation_id == source_operation
                    && task_id == source_task
                    && target_node == source_target
                    && session_id == source_session,
                "execution result differs from admitted dispatch"
            );
            ensure!(
                matches!(
                    status.as_str(),
                    "running" | "succeeded" | "failed" | "rejected" | "unknown"
                ),
                "execution observation has no durable result state"
            );
            ensure!(
                target_node == &event.author,
                "execution result must be signed by its target"
            );
            if detail.get("result_blob").is_some() {
                let blob: BlobRef = serde_json::from_value(detail["result_blob"].clone())?;
                ensure!(
                    event.blobs == vec![blob.clone()] && *detail == json!({"result_blob":blob}),
                    "execution result attachment differs from signed event"
                );
            } else {
                ensure!(
                    event.blobs.is_empty(),
                    "inline execution result cannot attach unrelated content"
                );
            }
        }
        (
            EventBody::NotificationRaised {
                notification_id,
                kind,
                text,
            },
            EventBody::ExecutionObserved {
                operation_id,
                session_id,
                status,
                ..
            },
        ) => {
            ensure!(
                notification_id == &format!("result:{operation_id}:{status}")
                    && kind == status
                    && text
                        == &format!(
                            "执行会话 {session_id} 的本轮执行状态为 {status}。工单验收仍需本人确认。"
                        ),
                "notification differs from its durable execution result"
            );
        }
        _ => bail!("event cannot be derived from this source"),
    }
    Ok(())
}

pub(crate) fn validate_event_evidence(
    policy: &PolicyState,
    connection: &Connection,
    event: &Event,
    events: &BTreeMap<String, StoredEvent>,
) -> Result<EventEvidenceStatus> {
    let proof = admission(event);
    let now = u64::try_from(event.created_at_ms).context("event creation time is negative")?;
    ensure!(now >= proof.admitted_at_ms, "event predates admission");
    let state = policy.historical(&proof.challenge.policies, proof.admitted_at_ms)?;
    ensure!(
        event.author == crypto::fingerprint(audience_key(&state, proof)?.as_str())?,
        "event author is not the admitted audience"
    );
    let Some(ancestors) = dependency_closure(event, events)? else {
        return Ok(EventEvidenceStatus::Pending);
    };
    let Some(facts) = historical_facts(&state, event, events, &ancestors)? else {
        return Ok(EventEvidenceStatus::Pending);
    };
    let transport = proof.transport.clone().into_evidence();
    reconstruct_admission(policy, proof, &facts, &transport)?;
    match &event.cause {
        EventCause::Invocation { .. } => {
            ensure!(
                now == proof.admitted_at_ms,
                "invocation event time differs from admission"
            );
            ensure!(
                expected_events(
                    &proof.source_body,
                    &proof.invocation.payload.actor,
                    &transport
                )?
                .iter()
                .any(|(body, blobs)| body == &event.body && blobs == &event.blobs),
                "event business content differs from signed request"
            );
        }
        EventCause::Derived { source, .. } => {
            let Some(source) = events.get(source) else {
                return Ok(EventEvidenceStatus::Pending);
            };
            ensure!(
                ancestors.contains(&source.envelope.hash),
                "derived source is not a causal dependency"
            );
            validate_derived(event, &source.event)?;
        }
    }
    let invocation_hash = proof.invocation.hash()?;
    let slot = event_slot(&event.body);
    for prior in events.values() {
        if &prior.event == event {
            continue;
        }
        ensure!(
            admission(&prior.event).invocation.hash()? != invocation_hash
                || event_slot(&prior.event.body) != slot,
            "invocation already has a conflicting event in this business slot"
        );
    }
    for blob in &event.blobs {
        if !has_blob_from(connection, &event.network_id, blob)? {
            return Ok(EventEvidenceStatus::Pending);
        }
    }
    Ok(EventEvidenceStatus::Valid)
}

pub(crate) fn current_event_authorization(
    policy: &PolicyState,
    connection: &Connection,
    event: &Event,
    events: &BTreeMap<String, StoredEvent>,
    now_ms: u64,
) -> Result<VerifiedAuthorization> {
    ensure!(
        validate_event_evidence(policy, connection, event, events)? == EventEvidenceStatus::Valid,
        "event has incomplete authorization evidence"
    );
    let proof = admission(event);
    let historical = policy.historical(&proof.challenge.policies, proof.admitted_at_ms)?;
    let ancestors = dependency_closure(event, events)?.context("event history is incomplete")?;
    let facts = historical_facts(&historical, event, events, &ancestors)?
        .context("event resource history is incomplete")?;
    let transport = proof.transport.clone().into_evidence();
    let admitted = reconstruct_admission(policy, proof, &facts, &transport)?;
    Ok(policy
        .verified()
        .verify_operation_authorization(&admitted, &facts, &transport, now_ms)?)
}

fn ensure_persisted_admission(connection: &Connection, proof: &AdmissionProof) -> Result<()> {
    let i = &proof.invocation.payload;
    let stored: (String, String, String, String, String, u64) = connection.query_row(
        "SELECT signed_invocation,source_body,transport,admission_challenge,admission_receipts,admitted_at_ms FROM accepted_authorizations WHERE space_id=? AND authorization_id=?",
        params![i.resource.space.as_str(), i.operation_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)),
    )?;
    ensure!(
        serde_json::from_str::<Value>(&stored.0)? == serde_json::to_value(&proof.invocation)?
            && serde_json::from_str::<Value>(&stored.1)? == proof.source_body
            && serde_json::from_str::<Value>(&stored.2)? == serde_json::to_value(&proof.transport)?
            && serde_json::from_str::<Value>(&stored.3)? == serde_json::to_value(&proof.challenge)?
            && serde_json::from_str::<Value>(&stored.4)? == serde_json::to_value(&proof.receipts)?
            && stored.5 == proof.admitted_at_ms,
        "event admission differs from durable authorization"
    );
    Ok(())
}

fn append_admitted_events_in(
    connection: &Connection,
    policy: &PolicyState,
    identity: &LocalIdentity,
    proof: &AdmissionProof,
    batch: Vec<(String, EventBody, Vec<BlobRef>)>,
    now_ms: u64,
) -> Result<Vec<SignedEvent>> {
    ensure_persisted_admission(connection, proof)?;
    let transport = proof.transport.clone().into_evidence();
    let expected = expected_events(
        &proof.source_body,
        &proof.invocation.payload.actor,
        &transport,
    )?;
    ensure!(
        batch.len() == expected.len()
            && batch.iter().zip(&expected).all(
                |((scope, body, blobs), (expected_body, expected_blobs))| source_scope(
                    &proof.source_body
                )
                .ok()
                    == Some(scope.as_str())
                    && body == expected_body
                    && blobs == expected_blobs
            ),
        "event batch differs from signed request"
    );
    let mut signed = Vec::new();
    for (scope, body, blobs) in batch {
        signed.push(append_event_in(
            connection,
            policy,
            identity,
            proof,
            &scope,
            EventCause::Invocation {
                admission: proof.clone(),
            },
            body,
            blobs,
            now_ms,
        )?);
    }
    rebuild_projections_from(connection, proof.invocation.payload.resource.space.as_str())?;
    Ok(signed)
}

#[allow(clippy::too_many_arguments)]
fn append_event_in(
    connection: &Connection,
    policy: &PolicyState,
    identity: &LocalIdentity,
    proof: &AdmissionProof,
    scope: &str,
    cause: EventCause,
    body: EventBody,
    blobs: Vec<BlobRef>,
    now_ms: u64,
) -> Result<SignedEvent> {
    let space = proof.invocation.payload.resource.space.as_str();
    let author = identity.node_id()?;
    let events = load_events_from(connection, space)?;
    let mut heads = heads_from_events(&events, scope);
    if let EventCause::Derived { source, .. } = &cause {
        heads.push(source.clone());
        heads.sort();
        heads.dedup();
    }
    for hash in closure_hashes(&events, &heads) {
        let stored = events.get(&hash).context("scope has missing history")?;
        ensure!(
            stored.status == "ready",
            "scope has incomplete or conflicting history"
        );
        for blob in &stored.event.blobs {
            ensure!(
                has_blob_from(connection, space, blob)?,
                "scope has missing blob content"
            );
        }
    }
    let sequence: u64 = connection.query_row("SELECT COALESCE(MAX(sequence),0)+1 FROM events WHERE space_id=? AND scope_id=? AND author=?", params![space,scope,author], |r| r.get(0))?;
    let previous = if sequence == 1 {
        None
    } else {
        Some(connection.query_row("SELECT hash FROM events WHERE space_id=? AND scope_id=? AND author=? AND sequence=? AND status='ready'", params![space,scope,author,sequence-1], |r| r.get(0))?)
    };
    let event = Event {
        version: WIRE_VERSION,
        network_id: space.into(),
        resource_id: proof.invocation.payload.resource.resource.clone(),
        cause,
        scope_id: scope.into(),
        author: author.clone(),
        sequence,
        previous,
        parents: heads,
        created_at_ms: i64::try_from(now_ms)?,
        body,
        blobs,
    };
    let envelope = identity.sign_event(event)?;
    let event: Event = serde_json::from_str(&envelope.canonical_body)?;
    ensure!(
        validate_event_evidence(policy, connection, &event, &events)? == EventEvidenceStatus::Valid,
        "event has incomplete authorization evidence"
    );
    connection.execute("INSERT INTO events(space_id,resource_id,hash,scope_id,author,sequence,envelope,status) VALUES(?,?,?,?,?,?,?,'ready')",
        params![space,event.resource_id.as_str(),envelope.hash,scope,author,sequence,serde_json::to_string(&envelope)?])?;
    Ok(envelope)
}

pub(crate) fn append_admitted_dispatch_in(
    tx: &Transaction<'_>,
    identity: &LocalIdentity,
    proof: &AdmissionProof,
    request: &OperationRequest,
    now_ms: u64,
) -> Result<SignedEvent> {
    ensure!(
        text(&proof.source_body, "method")? == "operate"
            && proof.source_body["request"] == serde_json::to_value(request)?,
        "dispatch differs from admitted request"
    );
    let persisted: String = tx.query_row(
        "SELECT request FROM operations WHERE space_id=? AND operation_id=?",
        params![request.network_id, request.operation_id],
        |r| r.get(0),
    )?;
    ensure!(
        serde_json::from_str::<OperationRequest>(&persisted)? == *request,
        "dispatch differs from durable operation"
    );
    let policy = PolicyState::load(tx)?;
    let body = expected_events(
        &proof.source_body,
        &proof.invocation.payload.actor,
        &proof.transport.clone().into_evidence(),
    )?
    .remove(0);
    Ok(append_admitted_events_in(
        tx,
        &policy,
        identity,
        proof,
        vec![(request.conversation_id.clone(), body.0, body.1)],
        now_ms,
    )?
    .remove(0))
}

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub fn append_authorized_events(
        &mut self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        source_body: &Value,
        events: Vec<(String, EventBody, Vec<BlobRef>)>,
        now: i64,
    ) -> Result<Vec<SignedEvent>> {
        let now_ms = u64::try_from(now).context("event creation time is negative")?;
        ensure!(
            text(source_body, "method")? != "operate",
            "operations must be admitted with their dispatch in one transaction"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let admitted = super::admission::consume_authorization_in(
            &tx,
            self.policy.verified(),
            auth,
            proof,
            transport,
            source_body,
            None,
            now_ms,
        )?;
        let signed =
            append_admitted_events_in(&tx, &self.policy, identity, &admitted, events, now_ms)?;
        tx.commit()?;
        Ok(signed)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_authorized_event(
        &mut self,
        identity: &LocalIdentity,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        source_body: &Value,
        scope: &str,
        body: EventBody,
        blobs: Vec<BlobRef>,
        now: i64,
    ) -> Result<SignedEvent> {
        Ok(self
            .append_authorized_events(
                identity,
                auth,
                proof,
                transport,
                source_body,
                vec![(scope.into(), body, blobs)],
                now,
            )?
            .remove(0))
    }

    pub fn operation_dispatch(
        &self,
        space: &str,
        operation_id: &str,
    ) -> Result<Option<SignedEvent>> {
        let mut found = None;
        for stored in load_events_from(&self.connection, space)?.values() {
            if stored.status == "ready"
                && matches!(&stored.event.body, EventBody::DispatchRecorded { operation_id: id, .. } if id == operation_id)
            {
                ensure!(found.is_none(), "operation has conflicting dispatches");
                found = Some(stored.envelope.clone());
            }
        }
        Ok(found)
    }

    pub fn append_derived_event(
        &mut self,
        identity: &LocalIdentity,
        space: &str,
        source: &str,
        body: EventBody,
        blobs: Vec<BlobRef>,
        now: i64,
    ) -> Result<SignedEvent> {
        let now_ms = u64::try_from(now).context("event creation time is negative")?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let events = load_events_from(&tx, space)?;
        let source_event = events.get(source).context("derived source is missing")?;
        ensure!(
            source_event.status == "ready",
            "derived source is not ready"
        );
        if let EventBody::ExecutionObserved {
            operation_id,
            status,
            detail,
            ..
        } = &body
        {
            let (state, result): (String, String) = tx.query_row(
                "SELECT state,result FROM operations WHERE space_id=? AND operation_id=?",
                params![space, operation_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            ensure!(
                serde_json::from_str::<String>(&state)? == *status,
                "execution observation differs from durable operation state"
            );
            let result: Value = if status == "running" {
                let stored: String = tx.query_row(
                    "SELECT value FROM local_values WHERE space_id=? AND key=?",
                    params![space, format!("turn:{operation_id}")],
                    |r| r.get(0),
                )?;
                serde_json::from_str(&stored)?
            } else {
                serde_json::from_str(&result)?
            };
            if detail.get("result_blob").is_some() {
                let blob: BlobRef = serde_json::from_value(detail["result_blob"].clone())?;
                ensure!(
                    blobs == vec![blob.clone()] && has_blob_from(&tx, space, &blob)?,
                    "execution result blob is missing"
                );
                let bytes: Vec<u8> = tx.query_row(
                    "SELECT content FROM blobs WHERE space_id=? AND hash=?",
                    params![space, blob.hash],
                    |r| r.get(0),
                )?;
                ensure!(
                    serde_json::from_slice::<Value>(&bytes)? == result,
                    "execution result blob differs from durable operation result"
                );
                ensure!(
                    *detail == json!({"result_blob":blob}),
                    "execution result contains unsigned extra fields"
                );
            } else {
                ensure!(
                    *detail == result && blobs.is_empty(),
                    "execution observation differs from durable operation result"
                );
            }
        } else {
            ensure!(
                blobs.is_empty(),
                "derived notification cannot attach content"
            );
        }
        let proof = admission(&source_event.event);
        ensure_persisted_admission(&tx, proof)?;
        let signed = append_event_in(
            &tx,
            &self.policy,
            identity,
            proof,
            &source_event.event.scope_id,
            EventCause::Derived {
                admission: proof.clone(),
                source: source.into(),
            },
            body,
            blobs,
            now_ms,
        )?;
        rebuild_projections_from(&tx, space)?;
        tx.commit()?;
        Ok(signed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::admission::{consume_authorization_in, tests::Fixture};
    use vibeface_secure::{
        Identity,
        authorization::{Action, JournalEntry, Nonce, Signed},
    };

    const NOW: u64 = 1_000;

    fn resign(fixture: &mut Fixture) -> Result<()> {
        let user = Identity::from_secret_hex(&hex::encode([4; 32])).map_err(anyhow::Error::msg)?;
        let facts = super::super::policy::request_facts_from(
            &fixture.connection,
            &fixture.state,
            &fixture.body,
            fixture.auth.audience(),
        )?;
        let mut invocation = fixture.auth.signed_invocation().payload.clone();
        invocation.action = facts.action;
        invocation.resource = facts.resource;
        invocation.context = facts.context;
        invocation.operation_id = facts.operation_id;
        invocation.body_hash = facts.body_hash;
        invocation.nonce = Nonce::random()?;
        for head in &mut invocation.expected_policies {
            *head = fixture.state.head(&head.domain).unwrap().clone();
        }
        fixture.auth = fixture.state.verify_invocation(
            &Signed::sign(invocation, &user)?,
            &super::super::policy::request_facts_from(
                &fixture.connection,
                &fixture.state,
                &fixture.body,
                fixture.auth.audience(),
            )?,
            &fixture.transport,
            NOW,
        )?;
        Ok(())
    }

    fn grant_event_actions(fixture: &mut Fixture) -> Result<()> {
        let root = Identity::from_secret_hex(&hex::encode([1; 32])).map_err(anyhow::Error::msg)?;
        let space = fixture.auth.resource().space.clone();
        let mut grant = fixture
            .state
            .grant(&fixture.auth.signed_invocation().payload.grant_leaf)
            .unwrap()
            .payload
            .clone();
        grant.issued_under = fixture
            .state
            .head(&PolicyDomain::Space(space.clone()))
            .unwrap()
            .clone();
        grant.actions = vec![
            Action::ConversationCreate,
            Action::MessageAppend,
            Action::TaskCreate,
            Action::TaskProgress,
        ];
        grant.nonce = Nonce::random()?;
        let signed_grant = Signed::sign(grant, &root)?;
        let grant_hash = fixture.state.install_grant(signed_grant.clone(), NOW)?;
        let mut document = fixture.state.space(&space).unwrap().clone();
        document.previous = fixture.state.head(&PolicyDomain::Space(space)).cloned();
        document.sequence += 1;
        document.grants = vec![grant_hash.clone()];
        let document = Signed::sign(document, &root)?;
        fixture.state.install_space(document.clone())?;
        let prepared = fixture.policy.prepare_network_update(
            &[
                JournalEntry::Grant(signed_grant),
                JournalEntry::Space(document),
            ],
            NOW,
        )?;
        let tx = fixture
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        fixture.policy = super::super::journal::commit_in(&tx, &prepared)?;
        tx.commit()?;
        let user = Identity::from_secret_hex(&hex::encode([4; 32])).map_err(anyhow::Error::msg)?;
        let mut invocation = fixture.auth.signed_invocation().payload.clone();
        invocation.grant_leaf = grant_hash;
        for head in &mut invocation.expected_policies {
            *head = fixture.state.head(&head.domain).unwrap().clone();
        }
        let facts = super::super::policy::request_facts_from(
            &fixture.connection,
            &fixture.state,
            &fixture.body,
            fixture.auth.audience(),
        )?;
        fixture.auth = fixture.state.verify_invocation(
            &Signed::sign(invocation, &user)?,
            &facts,
            &fixture.transport,
            NOW,
        )?;
        Ok(())
    }

    fn append_fixture(fixture: &mut Fixture) -> Result<Vec<SignedEvent>> {
        let proof = fixture.proof()?;
        let batch = expected_events(&fixture.body, fixture.auth.actor(), &fixture.transport)?
            .into_iter()
            .map(|(body, blobs)| ("chat".into(), body, blobs))
            .collect();
        let tx = fixture
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let admission = consume_authorization_in(
            &tx,
            &fixture.state,
            &fixture.auth,
            &proof,
            &fixture.transport,
            &fixture.body,
            None,
            NOW,
        )?;
        let events =
            append_admitted_events_in(&tx, &fixture.policy, &fixture.node, &admission, batch, NOW)?;
        tx.commit()?;
        Ok(events)
    }

    #[test]
    fn exact_business_content_and_atomic_nonce_consumption() -> Result<()> {
        let fixture = Fixture::new()?;
        let proof = fixture.proof()?;
        let mut store = Store {
            connection: fixture.connection,
            policy: fixture.policy,
        };
        assert!(
            store
                .append_authorized_event(
                    &fixture.node,
                    &fixture.auth,
                    &proof,
                    &fixture.transport,
                    &fixture.body,
                    "chat",
                    EventBody::ConversationCreated {
                        title: "changed after signing".into()
                    },
                    vec![],
                    NOW as i64
                )
                .is_err()
        );
        assert_eq!(
            store.connection.query_row(
                "SELECT count(*) FROM accepted_authorizations",
                [],
                |r| r.get::<_, u64>(0)
            )?,
            0
        );
        assert!(!store.connection.query_row(
            "SELECT consumed FROM authorization_challenges",
            [],
            |r| r.get::<_, bool>(0)
        )?);
        let event = store.append_authorized_event(
            &fixture.node,
            &fixture.auth,
            &proof,
            &fixture.transport,
            &fixture.body,
            "chat",
            EventBody::ConversationCreated {
                title: "test".into(),
            },
            vec![],
            NOW as i64,
        )?;
        assert_eq!(
            store.connection.query_row(
                "SELECT resource_id FROM conversations WHERE conversation_id='chat'",
                [],
                |r| r.get::<_, String>(0)
            )?,
            fixture.auth.resource().resource.as_str()
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM authorization_nonces", [], |r| r
                    .get::<_, u64>(0))?,
            1
        );
        assert!(
            store
                .append_authorized_event(
                    &fixture.node,
                    &fixture.auth,
                    &proof,
                    &fixture.transport,
                    &fixture.body,
                    "chat",
                    EventBody::ConversationCreated {
                        title: "test".into()
                    },
                    vec![],
                    NOW as i64
                )
                .is_err()
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT hash FROM events", [], |r| r.get::<_, String>(0))?,
            event.hash
        );
        Ok(())
    }

    #[test]
    fn signed_admission_sync_needs_no_receiver_nonce_and_missing_parents_stay_pending() -> Result<()>
    {
        let mut fixture = Fixture::new()?;
        grant_event_actions(&mut fixture)?;
        let creation = append_fixture(&mut fixture)?.remove(0);
        fixture.body = json!({"method":"message","space_id":fixture.auth.resource().space,"resource_id":fixture.auth.resource().resource,
            "conversation_id":"chat","authorization_id":"message_1","message_id":"m1","text":"signed text","scheduler_node":fixture.node.node_id()?});
        resign(&mut fixture)?;
        let message = append_fixture(&mut fixture)?.remove(0);
        let event = verify_event_envelope(
            &fixture.policy,
            fixture.auth.resource().space.as_str(),
            &message,
        )?;
        let receiver = Connection::open_in_memory()?;
        receiver.execute_batch(include_str!("../../migrations/202610180001_initial.sql"))?;
        assert_eq!(
            validate_event_evidence(&fixture.policy, &receiver, &event, &BTreeMap::new())?,
            EventEvidenceStatus::Pending
        );
        let mut events = BTreeMap::new();
        events.insert(
            creation.hash.clone(),
            StoredEvent {
                event: serde_json::from_str(&creation.canonical_body)?,
                envelope: creation,
                status: "ready".into(),
            },
        );
        assert_eq!(
            validate_event_evidence(&fixture.policy, &receiver, &event, &events)?,
            EventEvidenceStatus::Valid
        );
        assert_eq!(
            receiver.query_row("SELECT count(*) FROM authorization_nonces", [], |r| r
                .get::<_, u64>(0))?,
            0
        );
        let mut changed = event.clone();
        if let EventBody::MessagePosted { text, .. } = &mut changed.body {
            *text = "node substituted this text".into();
        }
        assert!(validate_event_evidence(&fixture.policy, &receiver, &changed, &events).is_err());
        changed = event.clone();
        changed.resource_id = ResourceId::from_bytes([99; 32]);
        assert!(validate_event_evidence(&fixture.policy, &receiver, &changed, &events).is_err());
        let outsider = LocalIdentity::generate();
        changed = event.clone();
        changed.author = outsider.node_id()?;
        let forged = outsider.sign_event(changed)?;
        assert!(
            verify_event_envelope(
                &fixture.policy,
                fixture.auth.resource().space.as_str(),
                &forged
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn task_context_uses_the_verified_creation_envelope() -> Result<()> {
        let mut fixture = Fixture::new()?;
        grant_event_actions(&mut fixture)?;
        append_fixture(&mut fixture)?;
        fixture.body = json!({"method":"create_task","space_id":fixture.auth.resource().space,"resource_id":fixture.auth.resource().resource,
            "conversation_id":"chat","authorization_id":"task_1","task_id":"t1","title":"title","goal":"goal","acceptance":"acceptance"});
        resign(&mut fixture)?;
        let task = append_fixture(&mut fixture)?.remove(0);
        fixture.body = json!({"method":"update_task","space_id":fixture.auth.resource().space,"resource_id":fixture.auth.resource().resource,
            "conversation_id":"chat","authorization_id":"task_2","task_id":"t1","status":"working","detail":"started"});
        resign(&mut fixture)?;
        let changed = append_fixture(&mut fixture)?.remove(0);
        let event: Event = serde_json::from_str(&changed.canonical_body)?;
        let events = load_events_from(&fixture.connection, &event.network_id)?;
        let historical = fixture
            .policy
            .historical(&admission(&event).challenge.policies, NOW)?;
        let facts = historical_facts(
            &historical,
            &event,
            &events,
            &dependency_closure(&event, &events)?.unwrap(),
        )?
        .unwrap();
        assert_eq!(facts.context.task, Some(ObjectHash::body(&task)?));
        assert_eq!(
            current_event_authorization(
                &fixture.policy,
                &fixture.connection,
                &event,
                &events,
                91_000
            )?
            .action(),
            Action::TaskProgress
        );
        let mut forged = event.clone();
        forged.cause = EventCause::Derived {
            admission: admission(&event).clone(),
            source: task.hash,
        };
        assert!(
            validate_event_evidence(&fixture.policy, &fixture.connection, &forged, &events)
                .is_err()
        );
        Ok(())
    }
}
