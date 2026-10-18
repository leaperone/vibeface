use super::{
    admission::{reconstruct_admission, reconstruct_admission_at_current_heads},
    journal::PolicyState,
    policy::FreshnessProof,
};
use crate::{
    crypto::{self, LocalIdentity},
    model::{
        AdmissionProof, BlobRef, Event, EventBody, EventCause, OperationRequest, OperationState,
        SignedEvent, WIRE_VERSION,
    },
    store::{
        Store, StoredEvent, closure_hashes, has_blob_from, heads_from_events, load_events_from,
        rebuild_admitted_projections_from,
    },
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use vibeface_secure::authorization::{
    Actor, DeviceId, DeviceUse, Invocation, InvocationContext, ObjectHash, PolicyDomain,
    PolicyStamp, PublicKey, RequestFacts, ResourceId, ResourceKind, ResourceRef, SessionRef,
    Signed, SpaceId, TransportEvidence, VerifiedAuthorization, VerifiedPolicySet,
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

fn policy_devices<'a>(
    state: &'a VerifiedPolicySet,
    stamp: &PolicyStamp,
) -> Option<&'a [ObjectHash]> {
    match &stamp.domain {
        PolicyDomain::Account(id) => state
            .account_history(id)
            .and_then(|history| {
                history
                    .iter()
                    .find(|document| document.hash().ok().as_ref() == Some(&stamp.head))
            })
            .map(|document| document.payload.devices.as_slice()),
        PolicyDomain::Space(id) => state
            .space_history(id)
            .and_then(|history| {
                history
                    .iter()
                    .find(|document| document.hash().ok().as_ref() == Some(&stamp.head))
            })
            .map(|document| document.payload.devices.as_slice()),
    }
}

fn audience_key(state: &VerifiedPolicySet, proof: &AdmissionProof) -> Result<PublicKey> {
    let audience = &proof.invocation.payload.audience;
    for stamp in &proof.challenge.policies {
        let devices = policy_devices(state, stamp);
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

fn execution_device(
    state: &VerifiedPolicySet,
    proof: &AdmissionProof,
    node: &str,
) -> Result<DeviceId> {
    for stamp in &proof.challenge.policies {
        for hash in policy_devices(state, stamp).into_iter().flatten() {
            if let Some(certificate) = state.device(hash)
                && crypto::fingerprint(certificate.payload.public_key.as_str())? == node
            {
                ensure!(
                    matches!(certificate.payload.subject, Actor::Service(_))
                        && certificate.payload.uses.contains(&DeviceUse::Noise),
                    "execution target is not a registered service device"
                );
                return Ok(DeviceId::from_key(&certificate.payload.public_key));
            }
        }
    }
    bail!("execution target is absent from admitted policy devices")
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
    invocation: &Signed<Invocation>,
    transport: &TransportEvidence,
) -> Result<Vec<(EventBody, Vec<BlobRef>)>> {
    let actor = &invocation.payload.actor;
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
        "resolve_unknown" => {
            ensure!(
                matches!(actor, Actor::Account(_)),
                "operation resolution requires an account actor"
            );
            let status = text(body, "outcome")?;
            ensure!(
                matches!(status, "succeeded" | "failed" | "rejected"),
                "invalid operation resolution outcome"
            );
            let task_id: Option<String> = serde_json::from_value(
                body.get("task_id").context("task_id is required")?.clone(),
            )?;
            vec![plain(EventBody::ExecutionObserved {
                operation_id: text(body, "operation_id")?.into(),
                task_id,
                target_node: text(body, "target_node")?.into(),
                session_id: text(body, "session_id")?.into(),
                status: status.into(),
                detail: json!({"resolver":actor,"invocation_hash":invocation.hash()?,"evidence":text(body,"evidence")?}),
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
        ensure!(
            prior.event.resource_id == event.resource_id,
            "event dependency belongs to another resource"
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
    let mut resolution_request = None;
    if text(body, "method")? == "resolve_unknown" {
        let mut dispatches = ancestors.iter().filter_map(|hash| {
            let prior = &events[hash].event;
            (prior.scope_id == scope && matches!(&prior.body, EventBody::DispatchRecorded { operation_id, .. } if body["operation_id"].as_str() == Some(operation_id.as_str())))
                .then_some(prior)
        });
        let Some(dispatch) = dispatches.next() else {
            return Ok(None);
        };
        ensure!(
            dispatches.next().is_none(),
            "operation resolution has conflicting dispatches"
        );
        let EventBody::DispatchRecorded { action, .. } = &dispatch.body else {
            bail!("resolution source is not a dispatch")
        };
        let original: OperationRequest = serde_json::from_value(action.clone())?;
        ensure!(
            dispatch.resource_id == resource
                && original.network_id == space.as_str()
                && original.conversation_id == scope
                && original.operation_id == text(body, "operation_id")?
                && original.target_node == text(body, "target_node")?
                && original.session_id == text(body, "session_id")?
                && serde_json::to_value(&original.task_id)?
                    == *body.get("task_id").context("task_id is required")?,
            "resolution scope differs from admitted operation"
        );
        ensure!(
            original.target_node == crypto::fingerprint(audience_key(state, proof)?.as_str())?,
            "resolution targets another service device"
        );
        resolution_request = Some(original);
    }
    let mut context = InvocationContext {
        conversation: Some(scope.into()),
        workspace_resource: Some(resource.clone()),
        ..Default::default()
    };
    let task = request
        .as_ref()
        .and_then(|request| request.task_id.as_deref())
        .or_else(|| body["task_id"].as_str());
    let mut task_creation = None;
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
        context.task = Some(vibeface_secure::authorization::EventRef::parse(
            &creation.envelope.hash,
        )?);
        task_creation = Some(creation);
    }
    if let Some(request) = request {
        let creation = task_creation.context("execution requires an admitted task creation")?;
        let binding = request
            .binding
            .as_ref()
            .context("execution requires a confirmed task binding")?;
        let b = &binding.binding;
        ensure!(
            b.network_id == request.network_id
                && b.conversation_id == scope
                && b.task_ref == creation.envelope.hash
                && Some(b.task_id.as_str()) == request.task_id.as_deref()
                && b.authority_node == creation.event.author
                && b.target_node == request.target_node
                && b.session_id == request.session_id,
            "execution binding differs from its task creation or target"
        );
        crate::binding::verify_binding(
            binding,
            audience_key(state, admission(&creation.event))?.as_str(),
        )?;
        if request.action["type"] == "open" {
            ensure!(
                request.action == b.open_spec,
                "execution open differs from confirmed task binding"
            );
        }
        ensure!(
            request.network_id == space.as_str()
                && request.operation_id == text(body, "authorization_id")?,
            "operation authorization scope mismatch"
        );
        context.execution_session = Some(SessionRef {
            target: execution_device(state, proof, &request.target_node)?,
            session_id: request.session_id.clone(),
        });
        context.executor_kind = request.action["executor"].as_str().map(str::to_owned);
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
    } else if let Some(original) = resolution_request {
        let binding = original
            .binding
            .as_ref()
            .context("resolved operation has no task binding")?;
        context.execution_session = Some(SessionRef {
            target: execution_device(state, proof, &original.target_node)?,
            session_id: original.session_id,
        });
        context.executor_kind = binding.binding.open_spec["executor"]
            .as_str()
            .map(str::to_owned);
        ensure!(
            context.executor_kind.is_some(),
            "resolved operation binding has no executor"
        );
    } else if matches!(text(body, "method")?, "bind_execution" | "rebind_execution") {
        context.execution_session = Some(SessionRef {
            target: execution_device(state, proof, text(body, "target_node")?)?,
            session_id: text(body, "session_id")?.into(),
        });
        context.executor_kind = Some(text(body, "executor")?.into());
    }
    if !body["sdk_session"].is_null() {
        let Actor::Service(service) = &proof.invocation.payload.actor else {
            bail!("account invocation cannot claim an SDK session")
        };
        let session: SessionRef = serde_json::from_value(body["sdk_session"].clone())?;
        ensure!(
            session.target == proof.invocation.payload.device,
            "SDK session targets another invocation device"
        );
        ensure!(
            proof.transport.clone().into_evidence()
                == TransportEvidence::LocalAgentAdapter {
                    service: service.clone(),
                    session: session.clone()
                },
            "SDK session does not have an observed service adapter"
        );
        let parent_hash = ObjectHash::parse(text(body, "parent_invocation")?)?;
        let Some(trigger) = events.get(&session.session_id) else {
            return Ok(None);
        };
        let parent = super::services::agent_account_message(events, &session.session_id)?;
        ensure!(
            ancestors.contains(&trigger.envelope.hash),
            "SDK trigger is not causal to this event"
        );
        ensure!(
            ancestors.contains(&parent.envelope.hash)
                && parent.event.scope_id == scope
                && parent.event.resource_id == resource
                && matches!(&parent.event.body,EventBody::MessagePosted { role, .. } if role == "user")
                && matches!(&parent.event.cause, EventCause::Invocation { .. })
                && matches!(
                    admission(&parent.event).invocation.payload.actor,
                    Actor::Account(_)
                )
                && admission(&parent.event).invocation.hash()? == parent_hash,
            "SDK session has no matching causal account message admission"
        );
        context.sdk_session = Some(session);
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

fn validate_sdk_parent(
    policy: &PolicyState,
    connection: &Connection,
    state: &VerifiedPolicySet,
    event: &Event,
    events: &BTreeMap<String, StoredEvent>,
    facts: &RequestFacts,
) -> Result<()> {
    let Some(session) = &facts.context.sdk_session else {
        return Ok(());
    };
    let proof = admission(event);
    let Actor::Service(service) = &proof.invocation.payload.actor else {
        bail!("SDK session requires a service actor")
    };
    let heads = if proof.source_body["basis_heads"].is_null() {
        vec![session.session_id.clone()]
    } else {
        serde_json::from_value(proof.source_body["basis_heads"].clone())?
    };
    let origin = super::services::resolve_agent_origin(
        policy,
        connection,
        state,
        events,
        &session.session_id,
        &heads,
        proof.admitted_at_ms,
    )?;
    ensure!(
        origin.parent.invocation_hash().as_str() == text(&proof.source_body, "parent_invocation")?,
        "SDK parent invocation differs from its causal account origin"
    );
    if let Some((_, task)) = &origin.task {
        ensure!(
            facts.context.task.as_ref() == Some(task)
                && proof.source_body["method"] != "create_task",
            "continuation SDK invocation escaped its original task"
        );
    }
    let delegated = state.verify_service_delegation(
        &origin.parent,
        service,
        &proof.invocation.payload.grant_leaf,
        facts,
        proof.admitted_at_ms,
    )?;
    ensure!(
        delegated.device() == &proof.invocation.payload.device
            && delegated.initiator() == &proof.invocation.payload.initiator
            && delegated.initiating_grant() == proof.invocation.payload.initiating_grant.as_ref(),
        "SDK service invocation differs from its admitted account delegation"
    );
    Ok(())
}

fn validate_derived(
    policy: &PolicyState,
    connection: &Connection,
    event: &Event,
    source: &StoredEvent,
) -> Result<()> {
    let source_envelope = &source.envelope;
    let source = &source.event;
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
            if target_node != &event.author {
                use vibeface_secure::authorization::execution_handoff::{
                    ExecutionHandoffResult, verify_execution_handoff_receipt_signature,
                };
                let result = if detail.get("result_blob").is_some() {
                    let blob: BlobRef = serde_json::from_value(detail["result_blob"].clone())?;
                    let bytes: Vec<u8> = connection.query_row(
                        "SELECT content FROM blobs WHERE space_id=? AND hash=?",
                        params![event.network_id, blob.hash],
                        |row| row.get(0),
                    )?;
                    serde_json::from_slice::<Value>(&bytes)?
                } else {
                    detail.clone()
                };
                let (handoff, receipt) = if result.get("reconciliation").is_some() {
                    use vibeface_secure::authorization::execution_handoff::ExecutionHandoffTerminalReconciliationResult;
                    let signed: ExecutionHandoffTerminalReconciliationResult =
                        serde_json::from_value(result)?;
                    verify_terminal_reconciliation_in(
                        policy,
                        connection,
                        &signed,
                        event.created_at_ms.try_into()?,
                    )?;
                    (
                        signed.reconciliation.resolution.observation.handoff,
                        signed.reconciliation.resolution.observation.receipt,
                    )
                } else if result.get("resolution").is_some() {
                    use vibeface_secure::authorization::execution_handoff::{
                        ExecutionHandoffResolutionResult,
                        verify_execution_resolution_receipt_signature,
                    };
                    let signed: ExecutionHandoffResolutionResult = serde_json::from_value(result)?;
                    verify_execution_resolution_receipt_signature(&signed)?;
                    verify_resolution_observation_in(policy, connection, &signed)?;
                    (
                        signed.resolution.resolution.observation.handoff,
                        signed.handoff_receipt,
                    )
                } else {
                    let signed: ExecutionHandoffResult = serde_json::from_value(result)?;
                    verify_execution_handoff_receipt_signature(
                        &signed.handoff,
                        &signed.receipt,
                        &signed.result,
                    )?;
                    (signed.handoff, signed.receipt)
                };
                ensure!(
                    handoff.handoff.project_dispatch == *source_envelope
                        && crypto::fingerprint(handoff.handoff.target_public_key.as_str())?
                            == *target_node
                        && serde_json::to_value(receipt.payload.state)?.as_str()
                            == Some(status.as_str())
                        && receipt.payload.issued_at_ms <= event.created_at_ms as u64
                        && receipt.payload.issued_at_ms >= admission(source).admitted_at_ms,
                    "delegated execution result differs from its admitted source or state"
                );
            }
            ensure!(
                !(detail.get("resolver").is_some() && detail.get("invocation_hash").is_some()),
                "resolution result requires the current resolver admission"
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
    let state = policy.historical(&proof.challenge.policies, proof.admitted_at_ms)?;
    validate_event_evidence_in_state(policy, connection, &state, event, events, false)
}

fn validate_local_event_evidence(
    policy: &PolicyState,
    connection: &Connection,
    event: &Event,
    events: &BTreeMap<String, StoredEvent>,
) -> Result<EventEvidenceStatus> {
    let state = policy.verified();
    if admission(event)
        .challenge
        .policies
        .iter()
        .all(|head| state.head(&head.domain) == Some(head))
    {
        validate_event_evidence_in_state(policy, connection, state, event, events, true)
    } else {
        validate_event_evidence(policy, connection, event, events)
    }
}

fn validate_event_evidence_in_state(
    policy: &PolicyState,
    connection: &Connection,
    state: &VerifiedPolicySet,
    event: &Event,
    events: &BTreeMap<String, StoredEvent>,
    current_heads: bool,
) -> Result<EventEvidenceStatus> {
    let proof = admission(event);
    let now = u64::try_from(event.created_at_ms).context("event creation time is negative")?;
    ensure!(now >= proof.admitted_at_ms, "event predates admission");
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
    if current_heads {
        reconstruct_admission_at_current_heads(state, proof, &facts, &transport)?
            .context("local event admission policy changed")?;
    } else {
        reconstruct_admission(policy, proof, &facts, &transport)?;
    }
    validate_sdk_parent(policy, connection, state, event, events, &facts)?;
    match &event.cause {
        EventCause::Invocation { .. } => {
            ensure!(
                now == proof.admitted_at_ms,
                "invocation event time differs from admission"
            );
            ensure!(
                expected_events(&proof.source_body, &proof.invocation, &transport)?
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
            for blob in &event.blobs {
                if !has_blob_from(connection, &event.network_id, blob)? {
                    return Ok(EventEvidenceStatus::Pending);
                }
            }
            validate_derived(policy, connection, event, source)?;
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
    if matches!(&event.cause, EventCause::Derived { .. })
        && let EventBody::ExecutionObserved { detail, .. } = &event.body
        && let Some(blob) = detail.get("result_blob")
    {
        let blob: BlobRef = serde_json::from_value(blob.clone())?;
        let bytes: Vec<u8> = connection.query_row(
            "SELECT content FROM blobs WHERE space_id=? AND hash=?",
            params![event.network_id, blob.hash],
            |r| r.get(0),
        )?;
        let result: Value = serde_json::from_slice(&bytes)?;
        ensure!(
            !(result.get("resolver").is_some() && result.get("invocation_hash").is_some()),
            "resolution attachment requires the current resolver admission"
        );
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
    event_authorization_in_view(policy, connection, policy.verified(), event, events, now_ms)
}

pub(crate) fn event_authorization_in_view(
    policy: &PolicyState,
    connection: &Connection,
    state: &VerifiedPolicySet,
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
    Ok(state.verify_operation_authorization(&admitted, &facts, &transport, now_ms)?)
}

pub(crate) fn persisted_admission_facts(
    connection: &Connection,
    policy: &PolicyState,
    proof: &AdmissionProof,
) -> Result<Option<RequestFacts>> {
    let events = load_events_from(connection, proof.invocation.payload.resource.space.as_str())?;
    let mut facts = None;
    for stored in events.values() {
        if !matches!(&stored.event.cause,EventCause::Invocation { admission } if admission == proof)
        {
            continue;
        }
        ensure!(
            verify_event_envelope(policy, &stored.event.network_id, &stored.envelope)?
                == stored.event,
            "accepted event evidence differs from signed envelope"
        );
        let expected = expected_events(
            &proof.source_body,
            &proof.invocation,
            &proof.transport.clone().into_evidence(),
        )?;
        ensure!(
            expected
                .iter()
                .any(|(body, blobs)| body == &stored.event.body && blobs == &stored.event.blobs),
            "accepted event business content differs from signed request"
        );
        let state = policy.historical(&proof.challenge.policies, proof.admitted_at_ms)?;
        let ancestors = dependency_closure(&stored.event, &events)?
            .context("accepted event causal history is missing")?;
        let current = historical_facts(&state, &stored.event, &events, &ancestors)?
            .context("accepted event resource history is missing")?;
        validate_sdk_parent(policy, connection, &state, &stored.event, &events, &current)?;
        ensure!(
            facts.as_ref().is_none_or(|facts| facts == &current),
            "accepted events disagree about original request facts"
        );
        facts = Some(current);
    }
    Ok(facts)
}

pub(super) fn retained_scope_events_in(
    connection: &Connection,
    space: &str,
    resource: &ResourceId,
    scope: &str,
) -> Result<BTreeMap<String, StoredEvent>> {
    let rows = connection.prepare(
        "SELECT envelope,status FROM events WHERE space_id=? AND resource_id=? AND scope_id=? ORDER BY hash",
    )?.query_map(params![space, resource.as_str(), scope], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut events = BTreeMap::new();
    for (encoded, status) in rows {
        let envelope: SignedEvent = serde_json::from_str(&encoded)?;
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        events.insert(
            envelope.hash.clone(),
            StoredEvent {
                envelope,
                event,
                status,
            },
        );
    }
    if let Some(source) = events.values().next() {
        validate_retained_event_scope_in(connection, &source.envelope)?;
    }
    Ok(events)
}

fn append_admitted_events_in(
    connection: &Connection,
    policy: &PolicyState,
    identity: &LocalIdentity,
    proof: &AdmissionProof,
    batch: Vec<(String, EventBody, Vec<BlobRef>)>,
    now_ms: u64,
) -> Result<Vec<SignedEvent>> {
    super::admission::validate_persisted_admission(connection, proof)?;
    let transport = proof.transport.clone().into_evidence();
    let mut expected = expected_events(&proof.source_body, &proof.invocation, &transport)?;
    ensure!(
        !batch.is_empty(),
        "empty event batch has no authorized mutation"
    );
    if proof.source_body["method"] == "create_task" {
        let scope = source_scope(&proof.source_body)?;
        let space = proof.invocation.payload.resource.space.as_str();
        let resource = &proof.invocation.payload.resource.resource;
        let events = retained_scope_events_in(connection, space, resource, scope)?;
        let task_id = text(&proof.source_body, "task_id")?;
        let existing=events.values().filter(|stored| stored.status=="ready"
            && stored.event.scope_id==scope && &stored.event.resource_id==resource
            && matches!(&stored.event.body,EventBody::TaskCreated{task_id:id,..} if id==task_id)).collect::<Vec<_>>();
        ensure!(existing.len() <= 1, "task has ambiguous creation history");
        if let Some(created) = existing.first() {
            let (requested, blobs) = expected
                .first()
                .context("task creation request has no event")?;
            ensure!(
                &created.event.body == requested && &created.event.blobs == blobs,
                "task retry differs from its original creation"
            );
            ensure!(
                validate_event_evidence(policy, connection, &created.event, &events)?
                    == EventEvidenceStatus::Valid,
                "task creation has incomplete authorization evidence"
            );
            ensure!(
                verify_event_envelope(policy, space, &created.envelope)? == created.event,
                "task creation envelope differs from its admitted event"
            );
            expected.remove(0);
            let mut missing = Vec::new();
            for (body, blobs) in expected {
                let mut authorized = false;
                for prior in events.values().filter(|stored| {
                    stored.status == "ready"
                        && stored.event.scope_id == scope
                        && &stored.event.resource_id == resource
                        && stored.event.body == body
                        && stored.event.blobs == blobs
                }) {
                    let EventCause::Invocation { admission } = &prior.event.cause else {
                        continue;
                    };
                    let old = &admission.source_body;
                    if !matches!(admission.invocation.payload.actor, Actor::Account(_))
                        || old["method"] != "create_task"
                        || old["auto_followup"] != true
                        || ["task_id", "title", "goal", "acceptance"]
                            .iter()
                            .any(|key| old[*key] != proof.source_body[*key])
                    {
                        continue;
                    }
                    ensure!(
                        verify_event_envelope(policy, space, &prior.envelope)? == prior.event
                            && validate_event_evidence(policy, connection, &prior.event, &events)?
                                == EventEvidenceStatus::Valid,
                        "existing task decision has incomplete authorization evidence"
                    );
                    authorized |= current_event_authorization(
                        policy,
                        connection,
                        &prior.event,
                        &events,
                        now_ms,
                    )
                    .is_ok();
                }
                if !authorized {
                    missing.push((body, blobs));
                }
            }
            expected = missing;
        }
    }
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
    rebuild_admitted_projections_from(
        connection,
        proof.invocation.payload.resource.space.as_str(),
    )?;
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
    let events = retained_scope_events_in(
        connection,
        space,
        &proof.invocation.payload.resource.resource,
        scope,
    )?;
    let scoped_events = events
        .iter()
        .filter(|(_, stored)| {
            stored.event.resource_id == proof.invocation.payload.resource.resource
        })
        .map(|(hash, stored)| (hash.clone(), stored.clone()))
        .collect();
    let mut heads = heads_from_events(&scoped_events, scope);
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
        ensure!(
            stored.event.resource_id == proof.invocation.payload.resource.resource,
            "scope includes another resource history"
        );
        for blob in &stored.event.blobs {
            ensure!(
                has_blob_from(connection, space, blob)?,
                "scope has missing blob content"
            );
        }
    }
    let sequence: u64 = connection.query_row("SELECT COALESCE(MAX(sequence),0)+1 FROM events WHERE space_id=? AND resource_id=? AND scope_id=? AND author=?", params![space,proof.invocation.payload.resource.resource.as_str(),scope,author], |r| r.get(0))?;
    let previous = if sequence == 1 {
        None
    } else {
        Some(connection.query_row("SELECT hash FROM events WHERE space_id=? AND resource_id=? AND scope_id=? AND author=? AND sequence=? AND status='ready'", params![space,proof.invocation.payload.resource.resource.as_str(),scope,author,sequence-1], |r| r.get(0))?)
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
        validate_local_event_evidence(policy, connection, &event, &events)?
            == EventEvidenceStatus::Valid,
        "event has incomplete authorization evidence"
    );
    connection.execute("INSERT INTO events(space_id,resource_id,hash,scope_id,author,sequence,envelope,status) VALUES(?,?,?,?,?,?,?,'ready')",
        params![space,event.resource_id.as_str(),envelope.hash,scope,author,sequence,serde_json::to_string(&envelope)?])?;
    Ok(envelope)
}

pub(crate) fn append_admitted_dispatch_in(
    tx: &Transaction<'_>,
    policy: &PolicyState,
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
    let body = expected_events(
        &proof.source_body,
        &proof.invocation,
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

pub(crate) fn append_admitted_resolution_in(
    tx: &Transaction<'_>,
    policy: &PolicyState,
    identity: &LocalIdentity,
    proof: &AdmissionProof,
    request: &OperationRequest,
    state: &OperationState,
    result: &Value,
    now_ms: u64,
) -> Result<SignedEvent> {
    ensure!(
        text(&proof.source_body, "method")? == "resolve_unknown",
        "resolution requires a resolver admission"
    );
    let expected = expected_events(
        &proof.source_body,
        &proof.invocation,
        &proof.transport.clone().into_evidence(),
    )?
    .remove(0);
    let EventBody::ExecutionObserved {
        operation_id,
        task_id,
        target_node,
        session_id,
        status,
        detail,
    } = &expected.0
    else {
        bail!("resolution did not produce an observation")
    };
    ensure!(
        request.network_id == proof.invocation.payload.resource.space.as_str()
            && request.conversation_id == source_scope(&proof.source_body)?
            && &request.operation_id == operation_id
            && &request.task_id == task_id
            && &request.target_node == target_node
            && &request.session_id == session_id
            && serde_json::to_value(state)?.as_str() == Some(status.as_str())
            && result == detail,
        "resolution result differs from signed resolver intent"
    );
    let persisted: (String, String, String) = tx.query_row(
        "SELECT request,state,result FROM operations WHERE space_id=? AND operation_id=?",
        params![request.network_id, request.operation_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    ensure!(
        serde_json::from_str::<OperationRequest>(&persisted.0)? == *request
            && serde_json::from_str::<OperationState>(&persisted.1)? == *state
            && serde_json::from_str::<Value>(&persisted.2)? == *result,
        "resolution audit differs from durable operation"
    );
    Ok(append_admitted_events_in(
        tx,
        &policy,
        identity,
        proof,
        vec![(request.conversation_id.clone(), expected.0, expected.1)],
        now_ms,
    )?
    .remove(0))
}

pub(crate) fn append_execution_handoff_observation_in(
    connection: &Connection,
    policy: &PolicyState,
    identity: &LocalIdentity,
    source: &SignedEvent,
    result: &vibeface_secure::authorization::execution_handoff::ExecutionHandoffResult,
    verified: &vibeface_secure::authorization::execution_handoff::VerifiedExecutionHandoffResult,
    now_ms: u64,
) -> Result<SignedEvent> {
    ensure!(
        verified.result() == result && result.handoff.handoff.project_dispatch == *source,
        "handoff acknowledgement belongs to another dispatch"
    );
    check_execution_observation_authorizations_in(
        connection,
        policy,
        identity,
        verified.original(),
        verified.submission(),
        now_ms,
    )?;
    verified.proof_usage().enforce_limits()?;
    append_checkpointed_execution_observation_in(
        connection,
        identity,
        source,
        &serde_json::to_value(result)?,
        result.receipt.payload.state,
        now_ms,
    )
}

fn verify_resolution_observation_in(
    policy: &PolicyState,
    _connection: &Connection,
    result: &vibeface_secure::authorization::execution_handoff::ExecutionHandoffResolutionResult,
) -> Result<()> {
    let at = result.receipt.payload.issued_at_ms;
    let historical = policy.historical(&result.freshness.challenge.policies, at)?;
    let resolver = super::execution_handoff::resolution_resolver_from(
        policy,
        &historical,
        &result.resolution,
        at,
    )?;
    let auth = historical.verify_execution_resolution_ack(result, &resolver, at)?;
    historical.verify_execution_resolution_freshness(&auth, &result.freshness, at)?;
    Ok(())
}

fn verify_terminal_reconciliation_in(
    policy: &PolicyState,
    _connection: &Connection,
    result: &vibeface_secure::authorization::execution_handoff::ExecutionHandoffTerminalReconciliationResult,
    now_ms: u64,
) -> Result<()> {
    let historical = policy.historical(&result.freshness.challenge.policies, now_ms)?;
    let resolver = super::execution_handoff::resolution_resolver_from(
        policy,
        &historical,
        &result.reconciliation,
        now_ms,
    )?;
    historical.verify_execution_terminal_reconciliation(result, &resolver, now_ms)?;
    Ok(())
}

pub(crate) fn append_execution_terminal_reconciliation_observation_in(
    connection: &Connection,
    policy: &PolicyState,
    identity: &LocalIdentity,
    source: &SignedEvent,
    result: &vibeface_secure::authorization::execution_handoff::ExecutionHandoffTerminalReconciliationResult,
    verified: &vibeface_secure::authorization::execution_handoff::VerifiedExecutionTerminalReconciliation,
    now_ms: u64,
) -> Result<SignedEvent> {
    ensure!(
        verified.wire() == &result.reconciliation
            && verified.freshness_hash() == &ObjectHash::body(&result.freshness)?
            && result
                .reconciliation
                .resolution
                .observation
                .handoff
                .handoff
                .project_dispatch
                == *source,
        "terminal reconciliation belongs to another dispatch"
    );
    check_execution_observation_authorizations_in(
        connection,
        policy,
        identity,
        verified.resolver(),
        verified.submission(),
        now_ms,
    )?;
    verified.proof_usage().enforce_limits()?;
    append_checkpointed_execution_observation_in(
        connection,
        identity,
        source,
        &serde_json::to_value(result)?,
        result
            .reconciliation
            .resolution
            .observation
            .receipt
            .payload
            .state,
        now_ms,
    )
}

pub(crate) fn append_execution_resolution_observation_in(
    connection: &Connection,
    policy: &PolicyState,
    identity: &LocalIdentity,
    source: &SignedEvent,
    result: &vibeface_secure::authorization::execution_handoff::ExecutionHandoffResolutionResult,
    verified: &vibeface_secure::authorization::execution_handoff::VerifiedExecutionResolution,
    now_ms: u64,
) -> Result<SignedEvent> {
    use vibeface_secure::authorization::execution_handoff::verify_execution_resolution_receipt_signature;
    ensure!(
        verified.wire() == &result.resolution
            && result
                .resolution
                .resolution
                .observation
                .handoff
                .handoff
                .project_dispatch
                == *source,
        "resolution acknowledgement belongs to another dispatch"
    );
    verify_execution_resolution_receipt_signature(result)?;
    check_execution_observation_authorizations_in(
        connection,
        policy,
        identity,
        verified.resolver(),
        verified.submission(),
        now_ms,
    )?;
    policy
        .verified()
        .verify_execution_resolution_freshness(verified, &result.freshness, now_ms)?;
    verified.proof_usage().enforce_limits()?;
    append_checkpointed_execution_observation_in(
        connection,
        identity,
        source,
        &serde_json::to_value(result)?,
        result.handoff_receipt.payload.state,
        now_ms,
    )
}

fn check_execution_observation_authorizations_in(
    connection: &Connection,
    policy: &PolicyState,
    identity: &LocalIdentity,
    resolver: &VerifiedAuthorization,
    submission: &VerifiedAuthorization,
    now_ms: u64,
) -> Result<()> {
    super::admission::check_policy_heads_in(connection, policy.verified(), resolver, now_ms)?;
    super::admission::check_policy_heads_in(connection, policy.verified(), submission, now_ms)?;
    ensure!(
        resolver.audience().device
            == DeviceId::from_key(&PublicKey::parse(&identity.public_key_hex()?)?),
        "execution observation must be appended by its current Project host"
    );
    Ok(())
}

struct ObservationCausalFact {
    author: String,
    sequence: u64,
    previous: Option<String>,
}

struct ObservationScopeCheckpoint {
    heads: Vec<String>,
    previous: Option<(u64, String)>,
}

pub(super) fn validate_retained_execution_scope_in(
    connection: &Connection,
    identity: &LocalIdentity,
    source: &SignedEvent,
) -> Result<()> {
    ready_observation_scope_in(connection, source, &identity.node_id()?, "")?;
    Ok(())
}

pub(crate) fn validate_retained_event_scope_in(
    connection: &Connection,
    source: &SignedEvent,
) -> Result<()> {
    ready_observation_scope_in(connection, source, "", "")?;
    Ok(())
}

fn ready_observation_scope_in(
    connection: &Connection,
    source: &SignedEvent,
    local_author: &str,
    observation_slot: &str,
) -> Result<ObservationScopeCheckpoint> {
    let expected: Event = serde_json::from_str(&source.canonical_body)?;
    let invocation_hash = admission(&expected).invocation.hash()?;
    let row_count: u64 = connection.query_row(
        "SELECT COUNT(*) FROM events WHERE space_id=? AND resource_id=? AND scope_id=?",
        params![
            expected.network_id,
            expected.resource_id.as_str(),
            expected.scope_id
        ],
        |row| row.get(0),
    )?;
    let heads = connection.prepare(
        "WITH scoped AS (SELECT hash,json_extract(envelope,'$.canonical_body') AS body FROM events WHERE space_id=?1 AND resource_id=?2 AND scope_id=?3), dependencies AS (SELECT value AS hash FROM scoped,json_each(scoped.body,'$.parents') UNION SELECT json_extract(body,'$.previous') FROM scoped WHERE json_extract(body,'$.previous') IS NOT NULL) SELECT hash FROM scoped WHERE hash NOT IN (SELECT hash FROM dependencies) ORDER BY hash",
    )?.query_map(params![expected.network_id, expected.resource_id.as_str(), expected.scope_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut lookup = connection.prepare(
        "SELECT envelope,status,scope_id,author,sequence,resource_id FROM events WHERE space_id=? AND hash=?",
    )?;
    let mut pending = heads
        .iter()
        .chain(std::iter::once(&source.hash))
        .map(|hash| (hash.clone(), false))
        .collect::<Vec<_>>();
    let mut visiting = BTreeSet::new();
    let mut completed = BTreeSet::new();
    let mut facts = BTreeMap::<String, ObservationCausalFact>::new();
    let mut sequences = BTreeSet::new();
    let mut previous = None::<(u64, String)>;
    while let Some((hash, exiting)) = pending.pop() {
        if exiting {
            visiting.remove(&hash);
            completed.insert(hash);
            continue;
        }
        if completed.contains(&hash) {
            continue;
        }
        ensure!(
            visiting.insert(hash.clone()),
            "execution observation scope has cyclic causal history"
        );
        let row: (String, String, String, String, u64, String) = lookup
            .query_row(params![expected.network_id, hash], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .context("execution observation scope has missing causal history")?;
        let envelope: SignedEvent = serde_json::from_str(&row.0)?;
        ensure!(
            envelope.canonical_body.len() <= crate::model::MAX_EVENT_BYTES,
            "execution observation event is too large"
        );
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        ensure!(
            row.1 == "ready"
                && envelope.hash == hash
                && crypto::digest(envelope.canonical_body.as_bytes()) == hash
                && serde_json::to_string(&event)? == envelope.canonical_body
                && event.network_id == expected.network_id
                && event.resource_id == expected.resource_id
                && event.resource_id.as_str() == row.5
                && event.scope_id == expected.scope_id
                && event.scope_id == row.2
                && event.author == row.3
                && event.sequence == row.4
                && sequences.insert((event.author.clone(), event.sequence)),
            "execution observation causal checkpoint changed"
        );
        crypto::validate_event_shape(&event)?;
        if hash == source.hash {
            ensure!(
                envelope == *source,
                "execution observation has no exact local ready Dispatch checkpoint"
            );
        }
        for blob in &event.blobs {
            ensure!(
                has_blob_from(connection, &event.network_id, blob)?,
                "execution observation scope has missing blob content"
            );
        }
        ensure!(
            admission(&event).invocation.hash()? != invocation_hash
                || event_slot(&event.body) != observation_slot,
            "execution observation conflicts with its original invocation slot"
        );
        if event.author == local_author
            && previous
                .as_ref()
                .is_none_or(|(sequence, _)| event.sequence > *sequence)
        {
            previous = Some((event.sequence, hash.clone()));
        }
        pending.push((hash.clone(), true));
        pending.extend(
            event
                .parents
                .iter()
                .chain(&event.previous)
                .map(|dependency| (dependency.clone(), false)),
        );
        facts.insert(
            hash,
            ObservationCausalFact {
                author: event.author,
                sequence: event.sequence,
                previous: event.previous,
            },
        );
    }
    ensure!(
        facts.len() as u64 == row_count,
        "execution observation scope has unreachable causal history"
    );
    for fact in facts.values() {
        if let Some(previous) = &fact.previous {
            let prior = facts
                .get(previous)
                .context("execution observation has missing author history")?;
            ensure!(
                prior.author == fact.author && prior.sequence.checked_add(1) == Some(fact.sequence),
                "execution observation scope author chain changed"
            );
        }
    }
    Ok(ObservationScopeCheckpoint { heads, previous })
}

fn append_checkpointed_execution_observation_in(
    connection: &Connection,
    identity: &LocalIdentity,
    source: &SignedEvent,
    result: &Value,
    state: vibeface_secure::authorization::execution_handoff::ExecutionHandoffState,
    now_ms: u64,
) -> Result<SignedEvent> {
    let source_event: Event = serde_json::from_str(&source.canonical_body)?;
    crypto::verify_signed_event(
        &source_event.network_id,
        source,
        &identity.public_key_hex()?,
    )?;
    let EventBody::DispatchRecorded {
        operation_id,
        task_id,
        target_node,
        session_id,
        ..
    } = &source_event.body
    else {
        bail!("execution observation checkpoint is not a Dispatch")
    };
    let state: OperationState = serde_json::from_value(serde_json::to_value(state)?)?;
    ensure!(
        state != OperationState::Accepted,
        "accepted handoff has no execution observation"
    );
    let (stored_state, stored_result): (String, String) = connection.query_row(
        "SELECT state,result FROM operations WHERE space_id=? AND operation_id=?",
        params![source_event.network_id, operation_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        serde_json::from_str::<OperationState>(&stored_state)? == state
            && serde_json::from_str::<Value>(&stored_result)? == *result,
        "execution observation differs from its durable signed acknowledgement"
    );
    let proof = admission(&source_event);
    ensure!(
        now_ms >= proof.admitted_at_ms,
        "execution observation predates its original admission"
    );
    super::admission::validate_persisted_admission(connection, proof)?;
    let bytes = serde_json::to_vec(result)?;
    ensure!(
        bytes.len() <= crate::model::MAX_BLOB_BYTES,
        "execution observation proof is too large"
    );
    let blob = BlobRef {
        hash: crypto::digest(&bytes),
        size: bytes.len() as u64,
    };
    let body = EventBody::ExecutionObserved {
        operation_id: operation_id.clone(),
        task_id: task_id.clone(),
        target_node: target_node.clone(),
        session_id: session_id.clone(),
        status: serde_json::to_value(state)?
            .as_str()
            .context("invalid execution observation state")?
            .to_owned(),
        detail: json!({"result_blob":blob}),
    };
    let author = identity.node_id()?;
    let checkpoint = ready_observation_scope_in(connection, source, &author, &event_slot(&body))?;
    connection.execute(
        "INSERT OR IGNORE INTO blobs(space_id,hash,size,content) VALUES(?,?,?,?)",
        params![source_event.network_id, blob.hash, blob.size, bytes],
    )?;
    ensure!(
        has_blob_from(connection, &source_event.network_id, &blob)?,
        "execution observation blob conflicts with stored bytes"
    );
    let sequence = checkpoint
        .previous
        .as_ref()
        .map_or(Ok(1), |(sequence, _)| {
            sequence
                .checked_add(1)
                .context("execution observation author sequence overflow")
        })?;
    let mut parents = checkpoint.heads;
    parents.push(source.hash.clone());
    parents.sort();
    parents.dedup();
    let envelope = identity.sign_event(Event {
        version: WIRE_VERSION,
        network_id: source_event.network_id.clone(),
        resource_id: source_event.resource_id.clone(),
        cause: EventCause::Derived {
            admission: proof.clone(),
            source: source.hash.clone(),
        },
        scope_id: source_event.scope_id.clone(),
        author: author.clone(),
        sequence,
        previous: checkpoint.previous.map(|(_, hash)| hash),
        parents,
        created_at_ms: i64::try_from(now_ms)?,
        body,
        blobs: vec![blob],
    })?;
    connection.execute("INSERT INTO events(space_id,resource_id,hash,scope_id,author,sequence,envelope,status) VALUES(?,?,?,?,?,?,?,'ready')", params![source_event.network_id, source_event.resource_id.as_str(), envelope.hash, source_event.scope_id, author, sequence, serde_json::to_string(&envelope)?])?;
    Ok(envelope)
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
            !matches!(text(source_body, "method")?, "operate" | "resolve_unknown"),
            "operation events require the operation state transaction"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let admitted = super::admission::consume_authorization_in(
            &tx,
            &self.policy,
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
        let encoded: String = tx.query_row(
            "SELECT envelope FROM events WHERE space_id=? AND hash=?",
            params![space, source],
            |row| row.get(0),
        )?;
        let source_envelope: SignedEvent = serde_json::from_str(&encoded)?;
        let source_body: Event = serde_json::from_str(&source_envelope.canonical_body)?;
        let events =
            retained_scope_events_in(&tx, space, &source_body.resource_id, &source_body.scope_id)?;
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
        super::admission::validate_persisted_admission(&tx, proof)?;
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
        rebuild_admitted_projections_from(&tx, space)?;
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
            Action::ExecutionOpen,
            Action::OperationResolve,
        ];
        grant.scope = vibeface_secure::authorization::ResourceScope::WholeSpace(space.clone());
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
        let scope = source_scope(&fixture.body)?.to_owned();
        let batch = expected_events(
            &fixture.body,
            fixture.auth.signed_invocation(),
            &fixture.transport,
        )?
        .into_iter()
        .map(|(body, blobs)| (scope.clone(), body, blobs))
        .collect();
        let tx = fixture
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let admission = consume_authorization_in(
            &tx,
            &fixture.policy,
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
            target_recovery_import: None,
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
        let shared = vibeface_secure::authorization::events::verify_signed_event(
            &serde_json::to_string(&event)?,
            fixture.auth.resource().space.as_str(),
            fixture.auth.resource().resource.as_str(),
            &fixture.node.public_key_hex()?,
        )
        .map_err(anyhow::Error::msg)?;
        assert_eq!(shared.hash(), event.hash);
        assert_eq!(shared.context_hash(), &ObjectHash::body(&event)?);
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
        assert_eq!(
            facts.context.task,
            Some(vibeface_secure::authorization::EventRef::parse(&task.hash)?)
        );
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
        fixture
            .connection
            .execute("UPDATE events SET status='conflict'", [])?;
        fixture.connection.execute("DELETE FROM tasks", [])?;
        fixture
            .connection
            .execute("DELETE FROM conversations", [])?;
        assert_eq!(
            persisted_admission_facts(&fixture.connection, &fixture.policy, admission(&event))?,
            Some(facts)
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

    #[test]
    fn resolution_keeps_the_current_resolver_and_signed_evidence() -> Result<()> {
        let mut fixture = Fixture::new()?;
        grant_event_actions(&mut fixture)?;
        append_fixture(&mut fixture)?;
        fixture.body = json!({"method":"create_task","space_id":fixture.auth.resource().space,"resource_id":fixture.auth.resource().resource,
            "conversation_id":"chat","authorization_id":"task_1","task_id":"t1","title":"title","goal":"goal","acceptance":"acceptance"});
        resign(&mut fixture)?;
        let task = append_fixture(&mut fixture)?.remove(0);
        let target = fixture.node.node_id()?;
        let execution_root = std::fs::canonicalize(std::env::temp_dir())?
            .to_string_lossy()
            .into_owned();
        fixture.connection.execute(
            "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
            params![
                fixture.auth.resource().space.as_str(),
                format!("resource-root:{}", fixture.auth.resource().resource),
                serde_json::to_string(&execution_root)?
            ],
        )?;
        let open_spec = json!({"type":"open","executor":"codex","cwd":execution_root});
        let binding = crate::binding::sign_binding(
            &fixture.node,
            &crate::binding::TaskBinding {
                network_id: fixture.auth.resource().space.as_str().into(),
                conversation_id: "chat".into(),
                task_ref: task.hash.clone(),
                task_id: "t1".into(),
                authority_node: target.clone(),
                generation: 1,
                target_node: target.clone(),
                session_id: "s1".into(),
                open_spec: open_spec.clone(),
                previous_binding_hash: None,
                previous_seal: None,
            },
        )?;
        let request = OperationRequest {
            operation_id: "open_1".into(),
            network_id: fixture.auth.resource().space.as_str().into(),
            conversation_id: "chat".into(),
            task_id: Some("t1".into()),
            target_node: target.clone(),
            session_id: "s1".into(),
            expected_revision: 0,
            basis_heads: heads_from_events(
                &load_events_from(&fixture.connection, fixture.auth.resource().space.as_str())?,
                "chat",
            ),
            action: open_spec,
            resources: vec![execution_root],
            binding: Some(binding),
        };
        fixture.body = json!({"method":"operate","space_id":fixture.auth.resource().space,"resource_id":fixture.auth.resource().resource,"authorization_id":"open_1","request":request});
        resign(&mut fixture)?;
        let original_invocation = fixture.auth.invocation_hash().clone();
        let original_freshness = fixture.proof()?;
        let tx = fixture
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let original = consume_authorization_in(
            &tx,
            &fixture.policy,
            &fixture.auth,
            &original_freshness,
            &fixture.transport,
            &fixture.body,
            Some(&request.operation_id),
            NOW,
        )?;
        tx.execute("INSERT INTO operations(space_id,operation_id,target_node,session_id,request,state,control,result) VALUES(?,?,?,?,?,'\"unknown\"',0,'null')",
            params![request.network_id,request.operation_id,request.target_node,request.session_id,serde_json::to_string(&request)?])?;
        let dispatch = append_admitted_dispatch_in(
            &tx,
            &fixture.policy,
            &fixture.node,
            &original,
            &request,
            NOW,
        )?;
        tx.commit()?;
        let dispatch_event =
            verify_event_envelope(&fixture.policy, &request.network_id, &dispatch)?;
        let dispatch_events = load_events_from(&fixture.connection, &request.network_id)?;
        let historical = fixture
            .policy
            .historical(&original.challenge.policies, NOW)?;
        let dispatch_facts = historical_facts(
            &historical,
            &dispatch_event,
            &dispatch_events,
            &dependency_closure(&dispatch_event, &dispatch_events)?.unwrap(),
        )?
        .unwrap();
        assert!(dispatch_facts.context.sdk_session.is_none());
        assert_eq!(
            dispatch_facts.context.execution_session,
            Some(SessionRef {
                target: DeviceId::from_key(&PublicKey::parse(&fixture.node.public_key_hex()?)?),
                session_id: request.session_id.clone(),
            })
        );
        let mut account_claiming_sdk = dispatch_event.clone();
        match &mut account_claiming_sdk.cause {
            EventCause::Invocation { admission } => {
                admission.source_body["sdk_session"] =
                    json!({"target":fixture.auth.device(),"session_id":"forged-sdk"})
            }
            _ => bail!("expected admitted dispatch"),
        }
        assert!(
            historical_facts(
                &historical,
                &account_claiming_sdk,
                &dispatch_events,
                &dependency_closure(&dispatch_event, &dispatch_events)?.unwrap()
            )
            .is_err()
        );
        fixture.body = json!({"method":"resolve_unknown","space_id":fixture.auth.resource().space,"resource_id":fixture.auth.resource().resource,"authorization_id":"resolve_1",
            "conversation_id":"chat","operation_id":"open_1","target_node":target,"session_id":"s1","task_id":"t1","task_ref":task.hash,
            "outcome":"succeeded","evidence":"verified external process and side effects"});
        resign(&mut fixture)?;
        let proof = fixture.proof()?;
        let result = json!({"resolver":fixture.auth.actor(),"invocation_hash":fixture.auth.invocation_hash(),"evidence":fixture.body["evidence"]});
        {
            let tx = fixture
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let resolution = consume_authorization_in(
                &tx,
                &fixture.policy,
                &fixture.auth,
                &proof,
                &fixture.transport,
                &fixture.body,
                None,
                NOW,
            )?;
            tx.execute(
                "UPDATE operations SET state='\"succeeded\"',result=?",
                [serde_json::to_string(&result)?],
            )?;
            assert!(
                append_admitted_resolution_in(
                    &tx,
                    &fixture.policy,
                    &fixture.node,
                    &resolution,
                    &request,
                    &OperationState::Succeeded,
                    &json!({"evidence":"substituted"}),
                    NOW
                )
                .is_err()
            );
        }
        assert_eq!(
            fixture
                .connection
                .query_row("SELECT state FROM operations", [], |r| r
                    .get::<_, String>(0))?,
            "\"unknown\""
        );
        let tx = fixture
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let resolution = consume_authorization_in(
            &tx,
            &fixture.policy,
            &fixture.auth,
            &proof,
            &fixture.transport,
            &fixture.body,
            None,
            NOW,
        )?;
        tx.execute(
            "UPDATE operations SET state='\"succeeded\"',result=?",
            [serde_json::to_string(&result)?],
        )?;
        let resolved = append_admitted_resolution_in(
            &tx,
            &fixture.policy,
            &fixture.node,
            &resolution,
            &request,
            &OperationState::Succeeded,
            &result,
            NOW,
        )?;
        tx.commit()?;
        let event = verify_event_envelope(&fixture.policy, &request.network_id, &resolved)?;
        assert!(matches!(&event.cause, EventCause::Invocation { .. }));
        assert_eq!(
            admission(&event).invocation.hash()?,
            *fixture.auth.invocation_hash()
        );
        assert_ne!(admission(&event).invocation.hash()?, original_invocation);
        assert!(
            matches!(&event.body,EventBody::ExecutionObserved { detail, .. } if detail == &result)
        );
        let receiver = Connection::open_in_memory()?;
        receiver.execute_batch(include_str!("../../migrations/202610180001_initial.sql"))?;
        let events = load_events_from(&fixture.connection, &request.network_id)?;
        assert_eq!(
            validate_event_evidence(&fixture.policy, &receiver, &event, &events)?,
            EventEvidenceStatus::Valid
        );
        let mut store = Store {
            connection: fixture.connection,
            policy: fixture.policy,
            target_recovery_import: None,
        };
        assert!(
            store
                .append_derived_event(
                    &fixture.node,
                    &request.network_id,
                    &dispatch.hash,
                    event.body.clone(),
                    vec![],
                    NOW as i64
                )
                .is_err()
        );
        let bytes = serde_json::to_vec(&result)?;
        let blob = BlobRef {
            hash: crypto::digest(&bytes),
            size: bytes.len() as u64,
        };
        store.put_blob(&request.network_id, &blob, &bytes)?;
        let mut hidden_resolution = event.body.clone();
        if let EventBody::ExecutionObserved { detail, .. } = &mut hidden_resolution {
            *detail = json!({"result_blob":blob});
        }
        assert!(
            store
                .append_derived_event(
                    &fixture.node,
                    &request.network_id,
                    &dispatch.hash,
                    hidden_resolution,
                    vec![blob],
                    NOW as i64
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn project_event_parents_never_require_another_project() -> Result<()> {
        let mut fixture = Fixture::new()?;
        grant_event_actions(&mut fixture)?;
        let original_resource = fixture.auth.resource().resource.clone();
        let foreign_resource = ResourceId::from_bytes([42; 32]);
        let root = Identity::from_secret_hex(&hex::encode([1; 32])).map_err(anyhow::Error::msg)?;
        let space = fixture.auth.resource().space.clone();
        let mut record = fixture
            .state
            .resource(&space, &original_resource)
            .unwrap()
            .payload
            .clone();
        record.resource = foreign_resource.clone();
        record.issued_under = fixture
            .state
            .head(&PolicyDomain::Space(space.clone()))
            .unwrap()
            .clone();
        record.nonce = Nonce::random()?;
        let record = Signed::sign(record, &root)?;
        let record_hash = fixture.state.install_resource(record.clone())?;
        let mut document = fixture.state.space(&space).unwrap().clone();
        document.previous = fixture
            .state
            .head(&PolicyDomain::Space(space.clone()))
            .cloned();
        document.sequence += 1;
        document.resources.push(record_hash.clone());
        document.resources.sort();
        let document = Signed::sign(document, &root)?;
        fixture.state.install_space(document.clone())?;
        let prepared = fixture.policy.prepare_network_update(
            &[
                JournalEntry::Resource(record.clone()),
                JournalEntry::Space(document),
            ],
            NOW,
        )?;
        let tx = fixture
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        fixture.policy = super::super::journal::commit_in(&tx, &prepared)?;
        tx.execute("INSERT INTO resource_records(space_id,resource_id,record_hash,kind,host_service_id,signed_record) VALUES(?,?,?,'project',?,?)",
            params![space.as_str(),foreign_resource.as_str(),record_hash.as_str(),record.payload.host.as_str(),serde_json::to_string(&record)?])?;
        tx.commit()?;
        fixture.body = json!({"method":"create_conversation","space_id":space,"resource_id":foreign_resource,"conversation_id":"foreign","authorization_id":"create_foreign","title":"foreign"});
        resign(&mut fixture)?;
        let foreign = append_fixture(&mut fixture)?.remove(0);
        fixture.body = json!({"method":"create_conversation","space_id":space,"resource_id":original_resource,"conversation_id":"chat","authorization_id":"create_chat","title":"chat"});
        resign(&mut fixture)?;
        let local = append_fixture(&mut fixture)?.remove(0);
        let local_event: Event = serde_json::from_str(&local.canonical_body)?;
        assert!(local_event.parents.is_empty());
        fixture.body = json!({"method":"message","space_id":space,"resource_id":original_resource,"conversation_id":"chat","authorization_id":"local_message",
            "message_id":"m1","text":"local only","scheduler_node":fixture.node.node_id()?});
        resign(&mut fixture)?;
        let message = append_fixture(&mut fixture)?.remove(0);
        let mut event = verify_event_envelope(&fixture.policy, space.as_str(), &message)?;
        let all = load_events_from(&fixture.connection, space.as_str())?;
        let project_events = all
            .iter()
            .filter(|(_, e)| e.event.resource_id == original_resource)
            .map(|(h, e)| (h.clone(), e.clone()))
            .collect();
        assert!(!event.parents.contains(&foreign.hash));
        assert_eq!(
            validate_event_evidence(
                &fixture.policy,
                &fixture.connection,
                &event,
                &project_events
            )?,
            EventEvidenceStatus::Valid
        );
        event.parents.push(foreign.hash);
        event.parents.sort();
        assert!(
            validate_event_evidence(&fixture.policy, &fixture.connection, &event, &all).is_err()
        );
        Ok(())
    }

    #[test]
    fn execution_device_uses_registered_target_key_instead_of_audience() -> Result<()> {
        use vibeface_secure::authorization::{DeviceCertificate, ServiceId};
        let mut fixture = Fixture::new()?;
        let root = Identity::from_secret_hex(&hex::encode([1; 32])).map_err(anyhow::Error::msg)?;
        let target =
            Identity::from_secret_hex(&hex::encode([7; 32])).map_err(anyhow::Error::msg)?;
        let account = match fixture.auth.actor() {
            Actor::Account(account) => account.clone(),
            _ => bail!("fixture needs account actor"),
        };
        let domain = PolicyDomain::Account(account.clone());
        let mut certificate = DeviceCertificate {
            version: vibeface_secure::authorization::AUTH_VERSION,
            owner: domain.clone(),
            subject: Actor::Service(ServiceId::from_key(&PublicKey::of(&target))),
            device: DeviceId::from_key(&PublicKey::of(&target)),
            public_key: PublicKey::of(&target),
            uses: vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
            possession: None,
            enrollment_grant: None,
            issued_under: fixture.state.head(&domain).unwrap().clone(),
            nonce: Nonce::random()?,
            not_before_ms: 0,
            expires_at_ms: 1_000_000,
        };
        certificate.prove_possession(&target)?;
        let certificate = Signed::sign(certificate, &root)?;
        let hash = fixture.state.install_device(certificate.clone(), NOW)?;
        let mut document = fixture.state.account(&account).unwrap().clone();
        document.previous = fixture.state.head(&domain).cloned();
        document.sequence += 1;
        document.devices.push(hash);
        document.devices.sort();
        let document = Signed::sign(document, &root)?;
        fixture.state.install_account(document.clone())?;
        let prepared = fixture.policy.prepare_network_update(
            &[
                JournalEntry::Device(certificate),
                JournalEntry::Account(document),
            ],
            NOW,
        )?;
        let tx = fixture
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        fixture.policy = super::super::journal::commit_in(&tx, &prepared)?;
        tx.commit()?;
        resign(&mut fixture)?;
        let freshness = fixture.proof()?;
        let tx = fixture
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let admitted = consume_authorization_in(
            &tx,
            &fixture.policy,
            &fixture.auth,
            &freshness,
            &fixture.transport,
            &fixture.body,
            None,
            NOW,
        )?;
        let device = execution_device(&fixture.state, &admitted, &target.node_id())?;
        assert_eq!(device, DeviceId::from_key(&PublicKey::of(&target)));
        assert_ne!(device, fixture.auth.audience().device);
        assert!(
            execution_device(
                &fixture.state,
                &admitted,
                &LocalIdentity::generate().node_id()?
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn sdk_event_reconstructs_the_original_account_message_and_service_delegation() -> Result<()> {
        use crate::identity::test_support::{TestNetwork, now};
        use vibeface_secure::authorization::{AUTH_VERSION, CheckStage, ServiceId};
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        network.conversation(&mut store, "sdk")?;
        let trigger = network.message(&mut store, "sdk", "perform bounded work")?;
        let node_key = PublicKey::parse(&network.node.public_key_hex()?)?;
        let service = ServiceId::from_key(&node_key);
        let session = SessionRef {
            target: DeviceId::from_key(&node_key),
            session_id: trigger.hash.clone(),
        };
        let descriptor = store.agent_grant(
            network.space.as_str(),
            "sdk",
            &service,
            &session,
            &trigger.hash,
            now(),
        )?;
        let mut body = network.body("agent_notification", "sdk");
        body["sdk_session"] = serde_json::to_value(&session)?;
        body["parent_invocation"] = json!(descriptor.parent_invocation);
        body["notification_id"] = json!("bounded-sdk-notice");
        body["kind"] = json!("manager_attention");
        body["text"] = json!("bounded SDK result");
        let audience = vibeface_secure::authorization::Audience {
            service: service.clone(),
            device: session.target.clone(),
        };
        let facts = store.request_facts(&body, &audience)?;
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Service(service.clone()),
                device: descriptor.device.clone(),
                initiator: descriptor.initiator,
                initiating_grant: descriptor.initiating_grant,
                resource: facts.resource.clone(),
                action: facts.action,
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
            &Identity::from_secret_hex(&network.node.secret_key_hex).map_err(anyhow::Error::msg)?,
        )?;
        let transport = TransportEvidence::LocalAgentAdapter {
            service,
            session: session.clone(),
        };
        let auth = store.authorize_request(&invocation, &facts, &transport, now())?;
        let freshness = network.freshness(&mut store, &auth, CheckStage::Admit, now())?;
        let checked = store.authorization_state().verify_freshness(
            &auth,
            &freshness.challenge,
            &freshness.receipts,
            now(),
        )?;
        store
            .authorization_state()
            .admit_operation(&auth, &checked, now())?;
        let proof = AdmissionProof {
            invocation,
            source_body: body,
            challenge: freshness.challenge,
            receipts: freshness.receipts,
            admitted_at_ms: now(),
            transport: crate::model::StoredTransport::from(&transport),
        };
        let parent: Event = serde_json::from_str(&trigger.canonical_body)?;
        let envelope = network.node.sign_event(Event {
            version: WIRE_VERSION,
            network_id: network.space.to_string(),
            resource_id: network.resource.clone(),
            cause: EventCause::Invocation { admission: proof },
            scope_id: "sdk".into(),
            author: network.node.node_id()?,
            sequence: parent.sequence + 1,
            previous: Some(trigger.hash.clone()),
            parents: vec![trigger.hash],
            created_at_ms: now() as i64,
            body: EventBody::NotificationRaised {
                notification_id: "bounded-sdk-notice".into(),
                kind: "manager_attention".into(),
                text: "bounded SDK result".into(),
            },
            blobs: vec![],
        })?;
        let event = verify_event_envelope(&store.policy, network.space.as_str(), &envelope)?;
        let events = load_events_from(&store.connection, network.space.as_str())?;
        assert_eq!(
            validate_event_evidence(&store.policy, &store.connection, &event, &events)?,
            EventEvidenceStatus::Valid
        );
        assert!(
            persisted_admission_facts(&store.connection, &store.policy, admission(&event))?
                .is_none()
        );
        assert_eq!(facts.context.sdk_session, Some(session));
        assert!(facts.context.execution_session.is_none());
        let historical = store
            .policy
            .historical(&admission(&event).challenge.policies, now())?;
        let ancestors = dependency_closure(&event, &events)?.unwrap();
        let mut changed = event.clone();
        match &mut changed.cause {
            EventCause::Invocation { admission } => {
                admission.source_body["parent_invocation"] =
                    json!(ObjectHash::body(&"wrong parent")?)
            }
            _ => bail!("expected SDK invocation event"),
        }
        assert!(historical_facts(&historical, &changed, &events, &ancestors).is_err());
        let mut tampered_parent_events = events.clone();
        let trigger_event = tampered_parent_events
            .get_mut(&facts.context.sdk_session.as_ref().unwrap().session_id)
            .unwrap();
        if let EventBody::MessagePosted { text, .. } = &mut trigger_event.event.body {
            *text = "replaced trigger content".into();
        }
        assert!(
            validate_event_evidence(
                &store.policy,
                &store.connection,
                &event,
                &tampered_parent_events
            )
            .is_err()
        );
        Ok(())
    }
}
