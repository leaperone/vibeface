use super::*;
use crate::scheduling::{JobState, SchedulerJob, TriggerKind, scheduler_key};
use serde::{Deserialize, Serialize};
use vibeface_secure::authorization::VerifiedPolicySet;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FollowupMode {
    Off,
    Report,
    Continue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FollowupIntentV1 {
    #[serde(rename = "space_id")]
    pub network_id: String,
    pub conversation_id: String,
    pub message_hash: String,
    pub target_node: String,
    pub mode: FollowupMode,
    pub version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskOriginV1 {
    #[serde(rename = "space_id")]
    pub network_id: String,
    pub conversation_id: String,
    pub task_id: String,
    pub task_ref: String,
    pub trigger: String,
    pub attempt_id: String,
    pub permission_version: u64,
    pub target_node: String,
    pub intent_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskFollowupGrantV1 {
    #[serde(rename = "space_id")]
    pub network_id: String,
    pub conversation_id: String,
    pub task_id: String,
    pub task_ref: String,
    pub grant_id: String,
    pub target_node: String,
    pub mode: FollowupMode,
    pub version: u64,
    pub previous_head: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum FollowupRecord {
    FollowupIntentV1(FollowupIntentV1),
    TaskOriginV1(TaskOriginV1),
    TaskFollowupGrantV1(TaskFollowupGrantV1),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FollowupPolicy {
    #[serde(rename = "space_id")]
    pub network_id: String,
    pub conversation_id: String,
    pub task_id: String,
    pub task_ref: String,
    pub grant_id: String,
    pub target_node: String,
    pub mode: FollowupMode,
    pub version: u64,
    pub proof_heads: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResultFollowupPolicy {
    pub task_id: Option<String>,
    pub target_node: String,
    pub mode: FollowupMode,
    pub version: u64,
    pub proof_heads: Vec<String>,
}

impl From<FollowupPolicy> for ResultFollowupPolicy {
    fn from(policy: FollowupPolicy) -> Self {
        Self {
            task_id: Some(policy.task_id),
            target_node: policy.target_node,
            mode: policy.mode,
            version: policy.version,
            proof_heads: policy.proof_heads,
        }
    }
}

fn grant_id(network: &str, scope: &str, task_ref: &str) -> String {
    format!(
        "task-followup:v1:{}",
        digest(&serde_json::to_vec(&(network, scope, task_ref)).expect("string tuple serializes"))
    )
}

fn field<'a>(body: &'a Value, key: &str) -> Result<&'a str> {
    body[key]
        .as_str()
        .with_context(|| format!("{key} is required"))
}

pub(crate) fn grant_body_from_request(body: &Value) -> Result<EventBody> {
    ensure!(
        body["method"] == "set_followup",
        "follow-up grant request required"
    );
    let network = field(body, "space_id")?;
    let scope = field(body, "conversation_id")?;
    let task_ref = field(body, "task_ref")?;
    let mode: FollowupMode = serde_json::from_value(body["mode"].clone())?;
    let expected = body["expected_version"]
        .as_u64()
        .context("expected_version is required")?;
    let grant = TaskFollowupGrantV1 {
        network_id: network.into(),
        conversation_id: scope.into(),
        task_id: field(body, "task_id")?.into(),
        task_ref: task_ref.into(),
        grant_id: grant_id(network, scope, task_ref),
        target_node: field(body, "scheduler_node")?.into(),
        mode,
        version: expected
            .checked_add(1)
            .context("follow-up version exhausted")?,
        previous_head: field(body, "previous_head")?.into(),
    };
    ensure!(
        !grant.target_node.is_empty(),
        "follow-up scheduler is required"
    );
    Ok(EventBody::DecisionRecorded {
        decision_id: format!("{}:{}", grant.grant_id, grant.version),
        text: "Task follow-up policy".into(),
        effective_constraints: vec![],
        tool_configuration: serde_json::to_value(FollowupRecord::TaskFollowupGrantV1(grant))?,
    })
}

fn invocation(event: &Event) -> Result<&AdmissionProof> {
    let EventCause::Invocation { admission } = &event.cause else {
        bail!("follow-up authority must come from an admitted invocation")
    };
    Ok(admission)
}

fn grant_from(event: &Event) -> Result<Option<TaskFollowupGrantV1>> {
    let EventBody::DecisionRecorded {
        tool_configuration, ..
    } = &event.body
    else {
        return Ok(None);
    };
    if tool_configuration["type"] != "task_followup_grant_v1" {
        return Ok(None);
    }
    let EventCause::Invocation { admission: proof } = &event.cause else {
        return Ok(None);
    };
    if proof.source_body["method"] != "set_followup"
        || !matches!(
            proof.invocation.payload.actor,
            vibeface_secure::authorization::Actor::Account(_)
        )
    {
        return Ok(None);
    }
    let FollowupRecord::TaskFollowupGrantV1(grant) =
        serde_json::from_value(tool_configuration.clone())?
    else {
        unreachable!()
    };
    ensure!(
        grant_body_from_request(&proof.source_body)? == event.body,
        "follow-up grant differs from its signed request"
    );
    Ok(Some(grant))
}

fn scope_events(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    network: &str,
    scope: &str,
) -> Result<BTreeMap<String, StoredEvent>> {
    let resource: String = connection
        .query_row(
            "SELECT resource_id FROM conversations WHERE space_id=? AND conversation_id=?",
            params![network, scope],
            |row| row.get(0),
        )
        .context("follow-up conversation is missing")?;
    let resource = vibeface_secure::authorization::ResourceId::parse(&resource)?;
    let mut events = load_events_from(connection, policy, network)?;
    events.retain(|_, stored| stored.event.resource_id == resource);
    let heads = heads_from_events(&events, scope);
    ensure!(
        closure_hashes(&events, &heads).iter().all(|hash| events
            .get(hash)
            .is_some_and(|event| event.status == "ready")),
        "follow-up history is incomplete or conflicting"
    );
    Ok(events)
}

fn ancestor(events: &BTreeMap<String, StoredEvent>, descendant: &str, expected: &str) -> bool {
    descendant != expected && closure_hashes(events, &[descendant.to_owned()]).contains(expected)
}

fn authority_in_view(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    state: &VerifiedPolicySet,
    events: &BTreeMap<String, StoredEvent>,
    stored: &StoredEvent,
    now_ms: u64,
) -> Result<()> {
    crate::identity::event_authorization::event_authorization_in_view(
        policy,
        connection,
        state,
        &stored.event,
        events,
        now_ms,
    )?;
    Ok(())
}

fn current_authority(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    events: &BTreeMap<String, StoredEvent>,
    stored: &StoredEvent,
) -> Result<()> {
    crate::identity::event_authorization::current_event_authorization(
        policy,
        connection,
        &stored.event,
        events,
        crate::runtime::now_ms() as u64,
    )?;
    Ok(())
}

fn scheduler_available_in_view(
    state: &VerifiedPolicySet,
    network: &str,
    target: &str,
    now_ms: u64,
) -> Result<()> {
    use vibeface_secure::authorization::{DeviceId, SpaceId};
    let space = SpaceId::parse(network)?;
    let document = state
        .space(&space)
        .context("scheduler space policy is missing")?;
    let devices = match &document.root {
        vibeface_secure::authorization::SpaceRoot::Personal { owner, .. } => {
            &state
                .account(owner)
                .context("scheduler account policy is missing")?
                .devices
        }
        vibeface_secure::authorization::SpaceRoot::Team { .. } => &document.devices,
    };
    for hash in devices {
        let Some(certificate) = state.device(hash) else {
            continue;
        };
        let key = &certificate.payload.public_key;
        if crate::crypto::fingerprint(key.as_str())? == target {
            state.verify_service_device(&space, &DeviceId::from_key(key), now_ms)?;
            return Ok(());
        }
    }
    bail!("scheduler is absent from the selected policy view")
}

fn scheduler_available(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    network: &str,
    target: &str,
) -> Result<()> {
    // Availability of the destination never supplies business write authority.
    authorize_verified_node_from(connection, policy.verified(), network, target)?;
    Ok(())
}

fn intent_from<'a>(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    network: &str,
    scope: &str,
    events: &'a BTreeMap<String, StoredEvent>,
    trigger: &str,
    check_current: bool,
) -> Result<Option<(&'a str, FollowupIntentV1)>> {
    let message = events.get(trigger).context("origin message is missing")?;
    ensure!(
        message.status == "ready" && message.event.scope_id == scope,
        "origin message is unavailable in this conversation"
    );
    let EventBody::MessagePosted {
        role,
        scheduler_node: Some(target),
        ..
    } = &message.event.body
    else {
        bail!("follow-up intent requires a user message with a fixed scheduler");
    };
    let proof = invocation(&message.event)?;
    ensure!(
        role == "user"
            && proof.source_body["method"] == "message"
            && proof.source_body["scheduler_node"].as_str() == Some(target.as_str())
            && matches!(
                proof.invocation.payload.actor,
                vibeface_secure::authorization::Actor::Account(_)
            ),
        "follow-up intent requires its original Account message"
    );
    let Some(report) = proof.source_body.get("report_results") else {
        return Ok(None);
    };
    let report = report
        .as_bool()
        .context("report_results must be a boolean")?;
    if check_current {
        current_authority(connection, policy, events, message)?;
        scheduler_available(connection, policy, network, target)?;
    }
    Ok(Some((
        events
            .get_key_value(trigger)
            .expect("message exists")
            .0
            .as_str(),
        FollowupIntentV1 {
            network_id: network.into(),
            conversation_id: scope.into(),
            message_hash: trigger.into(),
            target_node: target.clone(),
            mode: if report {
                FollowupMode::Report
            } else {
                FollowupMode::Off
            },
            version: 1,
        },
    )))
}

#[allow(clippy::too_many_arguments)]
fn intent_in_view<'a>(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    state: &VerifiedPolicySet,
    network: &str,
    scope: &str,
    events: &'a BTreeMap<String, StoredEvent>,
    trigger: &str,
    now_ms: u64,
) -> Result<Option<(&'a str, FollowupIntentV1)>> {
    let intent = intent_from(connection, policy, network, scope, events, trigger, false)?;
    if let Some((_, value)) = &intent {
        authority_in_view(
            connection,
            policy,
            state,
            events,
            events.get(trigger).context("intent source missing")?,
            now_ms,
        )?;
        scheduler_available_in_view(state, network, &value.target_node, now_ms)?;
    }
    Ok(intent)
}

pub(super) fn report_intent_head_from(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    network: &str,
    scope: &str,
    trigger: &str,
) -> Result<Option<String>> {
    let events = scope_events(connection, policy, network, scope)?;
    let source = events
        .get(trigger)
        .context("scheduler trigger is missing")?;
    if !matches!(&source.event.body, EventBody::MessagePosted { role, .. } if role == "user") {
        return Ok(None);
    }
    Ok(
        intent_from(connection, policy, network, scope, &events, trigger, true)?
            .map(|(head, _)| head.to_owned()),
    )
}

#[cfg(test)]
pub(super) fn append_report_intent_from(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    network: &str,
    scope: &str,
    message: &SignedEvent,
    target: &str,
    report_results: bool,
) -> Result<Option<String>> {
    let events = scope_events(connection, policy, network, scope)?;
    let source = events.get(&message.hash).context("message is missing")?;
    ensure!(
        source.envelope == *message,
        "intent must preserve the exact admitted message"
    );
    let (head, intent) = intent_from(
        connection,
        policy,
        network,
        scope,
        &events,
        &message.hash,
        true,
    )?
    .context("message admission has no result-report intent")?;
    ensure!(
        intent.target_node == target && (intent.mode == FollowupMode::Report) == report_results,
        "message retry cannot change its follow-up intent"
    );
    Ok(Some(head.into()))
}

fn creation_from<'a>(
    events: &'a BTreeMap<String, StoredEvent>,
    scope: &str,
    task: &str,
) -> Result<(&'a str, &'a StoredEvent)> {
    let mut found = events.iter().filter(|(_, e)| {
        e.status == "ready"
            && e.event.scope_id == scope
            && matches!(&e.event.body, EventBody::TaskCreated { task_id, .. } if task_id == task)
    });
    let (hash, event) = found.next().context("task not found")?;
    ensure!(found.next().is_none(), "task creation is ambiguous");
    Ok((hash.as_str(), event))
}

fn task_origin_from(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    network: &str,
    scope: &str,
    events: &BTreeMap<String, StoredEvent>,
    task_ref: &str,
    creation: &StoredEvent,
) -> Result<Option<TaskOriginV1>> {
    let proof = invocation(&creation.event)?;
    if matches!(
        proof.invocation.payload.actor,
        vibeface_secure::authorization::Actor::Account(_)
    ) {
        ensure!(
            proof
                .source_body
                .get("manager_authority")
                .is_none_or(Value::is_null),
            "Account task creation cannot claim a manager origin"
        );
        return Ok(None);
    }
    let authority: ManagerDispatchAuthority =
        serde_json::from_value(proof.source_body["manager_authority"].clone())?;
    let attempt = authority.attempt_id.as_str();
    ensure!(
        !attempt.is_empty()
            && authority.followup_version == 0
            && authority.scheduler_node
                == crate::crypto::fingerprint(proof.invocation.issuer.as_str())?
            && proof.invocation.payload.context.sdk_session.is_some()
            && matches!(&proof.transport, StoredTransport::LocalAgentAdapter { session, .. }
            if Some(session) == proof.invocation.payload.context.sdk_session.as_ref()),
        "task origin requires its actual writable manager invocation"
    );
    let message = events
        .get(&authority.trigger)
        .context("task origin message is missing")?;
    let parent = invocation(&message.event)?;
    ensure!(
        message.event.resource_id == creation.event.resource_id
            && matches!(
                parent.invocation.payload.actor,
                vibeface_secure::authorization::Actor::Account(_)
            )
            && proof.source_body["parent_invocation"].as_str()
                == Some(parent.invocation.hash()?.as_str())
            && ancestor(events, task_ref, &authority.trigger),
        "task origin is not derived from its exact Account message"
    );
    let intent = intent_from(
        connection,
        policy,
        network,
        scope,
        events,
        &authority.trigger,
        false,
    )?;
    if let Some((head, intent)) = &intent {
        ensure!(
            intent.target_node == authority.scheduler_node && ancestor(events, task_ref, head),
            "task creation did not consume its fixed scheduler intent"
        );
    } else {
        ensure!(
            matches!(&message.event.body, EventBody::MessagePosted {
            role, scheduler_node: Some(target), .. } if role == "user" && target == &authority.scheduler_node),
            "task origin has the wrong scheduler"
        );
    }
    let EventBody::TaskCreated { task_id, .. } = &creation.event.body else {
        unreachable!()
    };
    Ok(Some(TaskOriginV1 {
        network_id: network.into(),
        conversation_id: scope.into(),
        task_id: task_id.clone(),
        task_ref: task_ref.into(),
        trigger: authority.trigger,
        attempt_id: attempt.into(),
        permission_version: authority.permission_version,
        target_node: authority.scheduler_node,
        intent_ref: intent.map(|(head, _)| head.into()),
    }))
}

pub(super) fn scheduler_followup_evidence_in(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    space: &str,
    scope: &str,
    events: &BTreeMap<String, StoredEvent>,
    trigger: &str,
    now: u64,
) -> Result<(Option<SchedulerWithdrawal>, Vec<AdmissionProof>)> {
    let source = events
        .get(trigger)
        .context("scheduler result is unavailable")?;
    let EventBody::ExecutionObserved { task_id, .. } = &source.event.body else {
        bail!("autonomous scheduler trigger is not an execution result")
    };
    if let Some(task) = task_id {
        let (task_ref, creation) = creation_from(events, scope, task)?;
        let mut latest_status = None;
        let all = events.keys().cloned().collect();
        for hash in topological(events, &all) {
            let stored = &events[&hash];
            if stored.status == "ready"
                && stored.event.scope_id == scope
                && stored.event.resource_id == source.event.resource_id
                && ancestor(events, &hash, task_ref)
                && let EventBody::TaskChanged {
                    task_id, status, ..
                } = &stored.event.body
                && task_id == task
            {
                latest_status = Some((hash, status.clone()));
            }
        }
        if let Some((event, status)) = latest_status
            && matches!(status.as_str(), "accepted" | "cancelled")
        {
            return Ok((
                Some(SchedulerWithdrawal::TaskFinalized { event, status }),
                Vec::new(),
            ));
        }
        let current = policy_in_view(
            connection,
            policy,
            policy.verified(),
            space,
            scope,
            task,
            events,
            now,
            false,
        )?;
        let mut authorities = Vec::new();
        if current.mode == FollowupMode::Off
            && current.version == 1
            && let Some(origin) =
                task_origin_from(connection, policy, space, scope, events, task_ref, creation)?
            && let Some((head, intent)) = intent_from(
                connection,
                policy,
                space,
                scope,
                events,
                &origin.trigger,
                false,
            )?
            && intent.mode == FollowupMode::Off
        {
            return Ok((
                Some(SchedulerWithdrawal::FollowupOff { event: head.into() }),
                Vec::new(),
            ));
        }
        for head in &current.proof_heads {
            let evidence = events
                .get(head)
                .context("follow-up policy evidence is unavailable")?;
            if let Some(grant) = grant_from(&evidence.event)? {
                if grant.mode == FollowupMode::Off {
                    return Ok((
                        Some(SchedulerWithdrawal::FollowupOff {
                            event: head.clone(),
                        }),
                        Vec::new(),
                    ));
                }
                authorities.push(invocation(&evidence.event)?.clone());
            } else if matches!(&evidence.event.body, EventBody::MessagePosted { role, .. } if role == "user")
            {
                if let Some((_, intent)) =
                    intent_from(connection, policy, space, scope, events, head, false)?
                    && intent.mode == FollowupMode::Off
                {
                    return Ok((
                        Some(SchedulerWithdrawal::FollowupOff {
                            event: head.clone(),
                        }),
                        Vec::new(),
                    ));
                }
                authorities.push(invocation(&evidence.event)?.clone());
            }
        }
        Ok((None, authorities))
    } else {
        let parent = crate::identity::services::agent_account_message(events, trigger)?;
        if let Some((head, intent)) = intent_from(
            connection,
            policy,
            space,
            scope,
            events,
            &parent.envelope.hash,
            false,
        )? && intent.mode == FollowupMode::Off
        {
            return Ok((
                Some(SchedulerWithdrawal::FollowupOff { event: head.into() }),
                Vec::new(),
            ));
        }
        Ok((None, Vec::new()))
    }
}

pub(crate) fn followup_policy_from(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    network: &str,
    scope: &str,
    task: &str,
) -> Result<FollowupPolicy> {
    policy_from(connection, policy, network, scope, task, true)
}

fn policy_from(
    connection: &Connection,
    policy_state: &crate::identity::journal::PolicyState,
    network: &str,
    scope: &str,
    task: &str,
    check_current: bool,
) -> Result<FollowupPolicy> {
    let events = scope_events(connection, policy_state, network, scope)?;
    policy_in_view(
        connection,
        policy_state,
        policy_state.verified(),
        network,
        scope,
        task,
        &events,
        crate::runtime::now_ms() as u64,
        check_current,
    )
}

#[allow(clippy::too_many_arguments)]
fn policy_in_view(
    connection: &Connection,
    policy_state: &crate::identity::journal::PolicyState,
    state: &VerifiedPolicySet,
    network: &str,
    scope: &str,
    task: &str,
    events: &BTreeMap<String, StoredEvent>,
    now_ms: u64,
    check_authority: bool,
) -> Result<FollowupPolicy> {
    let (task_ref, creation) = creation_from(events, scope, task)?;
    let mut policy = FollowupPolicy {
        network_id: network.into(),
        conversation_id: scope.into(),
        task_id: task.into(),
        task_ref: task_ref.into(),
        grant_id: grant_id(network, scope, task_ref),
        target_node: String::new(),
        mode: FollowupMode::Off,
        version: 0,
        proof_heads: vec![task_ref.into()],
    };
    let mut effective_authority = None;
    if let Some(origin) = task_origin_from(
        connection,
        policy_state,
        network,
        scope,
        events,
        task_ref,
        creation,
    )? {
        policy.target_node = origin.target_node;
        if let Some((_, intent)) = intent_from(
            connection,
            policy_state,
            network,
            scope,
            events,
            &origin.trigger,
            false,
        )? {
            policy.mode = intent.mode;
            policy.version = intent.version;
            effective_authority = Some(events.get(&origin.trigger).expect("origin was verified"));
        }
    }
    let mut grants = BTreeMap::new();
    for (hash, stored) in events
        .iter()
        .filter(|(_, e)| e.status == "ready" && e.event.scope_id == scope)
    {
        let Some(grant) = grant_from(&stored.event)? else {
            continue;
        };
        if grant.task_id != task && grant.task_ref != task_ref {
            continue;
        }
        ensure!(
            grant.network_id == network
                && grant.conversation_id == scope
                && grant.task_id == task
                && grant.task_ref == task_ref
                && grant.grant_id == policy.grant_id
                && stored.event.resource_id == creation.event.resource_id
                && ancestor(events, hash, task_ref),
            "task grant has the wrong scope"
        );
        ensure!(
            grants
                .insert(grant.version, (hash, stored, grant))
                .is_none(),
            "conflicting task follow-up versions"
        );
    }
    for (version, (head, stored, grant)) in grants {
        ensure!(
            version
                == policy
                    .version
                    .checked_add(1)
                    .context("follow-up version exhausted")?
                && grant.previous_head == policy.proof_heads[0]
                && ancestor(events, head, &grant.previous_head)
                && (policy.target_node.is_empty() || grant.target_node == policy.target_node),
            "task follow-up version chain is invalid"
        );
        policy.mode = grant.mode;
        policy.version = version;
        policy.target_node = grant.target_node;
        policy.proof_heads = vec![head.clone()];
        effective_authority = Some(stored);
    }
    let closure = events.keys().cloned().collect();
    let mut status = "open";
    for hash in topological(events, &closure) {
        let stored = &events[&hash];
        if stored.status == "ready"
            && stored.event.scope_id == scope
            && stored.event.resource_id == creation.event.resource_id
            && let EventBody::TaskChanged {
                task_id,
                status: value,
                ..
            } = &stored.event.body
            && task_id == task
            && ancestor(events, &hash, task_ref)
        {
            status = value;
        }
    }
    if matches!(status, "accepted" | "cancelled") {
        policy.mode = FollowupMode::Off;
    }
    if check_authority && policy.mode != FollowupMode::Off {
        authority_in_view(
            connection,
            policy_state,
            state,
            events,
            effective_authority.context("follow-up has no Account authority")?,
            now_ms,
        )?;
        scheduler_available_in_view(state, network, &policy.target_node, now_ms)?;
    }
    Ok(policy)
}

fn result_dispatch_in_view<'a>(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    state: &VerifiedPolicySet,
    events: &'a BTreeMap<String, StoredEvent>,
    scope: &str,
    trigger: &str,
    now_ms: u64,
) -> Result<(&'a StoredEvent, OperationRequest)> {
    let result = events.get(trigger).context("follow-up result is missing")?;
    let EventBody::ExecutionObserved {
        operation_id,
        task_id,
        target_node,
        session_id,
        status,
        ..
    } = &result.event.body
    else {
        bail!("follow-up requires an execution result")
    };
    ensure!(
        result.status == "ready"
            && result.event.scope_id == scope
            && result.event.author == *target_node
            && matches!(status.as_str(), "succeeded" | "failed" | "rejected"),
        "follow-up requires its original executor's known terminal result"
    );
    let EventCause::Derived { source, .. } = &result.event.cause else {
        bail!("follow-up result has no exact admitted dispatch")
    };
    let dispatch = events
        .get(source)
        .context("follow-up dispatch is missing")?;
    let EventBody::DispatchRecorded {
        operation_id: id,
        task_id: dispatch_task,
        target_node: target,
        session_id: session,
        action,
    } = &dispatch.event.body
    else {
        bail!("follow-up source is not an execution dispatch")
    };
    let request: OperationRequest = serde_json::from_value(action.clone())?;
    ensure!(
        dispatch.status == "ready"
            && dispatch.event.scope_id == scope
            && dispatch.event.resource_id == result.event.resource_id
            && id == operation_id
            && dispatch_task == task_id
            && target == target_node
            && session == session_id
            && request.network_id == result.event.network_id
            && request.conversation_id == scope
            && request.operation_id == *operation_id
            && request.task_id == *task_id
            && request.target_node == *target_node
            && serde_json::to_value(&request.session_id)? == serde_json::to_value(session_id)?
            && ancestor(events, trigger, source),
        "execution result differs from its exact signed dispatch"
    );
    authority_in_view(connection, policy, state, events, result, now_ms)?;
    authority_in_view(connection, policy, state, events, dispatch, now_ms)?;
    Ok((dispatch, request))
}

#[allow(clippy::too_many_arguments)]
fn validate_task_result_in_view(
    connection: &Connection,
    policy_state: &crate::identity::journal::PolicyState,
    state: &VerifiedPolicySet,
    events: &BTreeMap<String, StoredEvent>,
    network: &str,
    scope: &str,
    trigger: &str,
    scheduler: &str,
    now_ms: u64,
) -> Result<FollowupPolicy> {
    let (dispatch, request) = result_dispatch_in_view(
        connection,
        policy_state,
        state,
        events,
        scope,
        trigger,
        now_ms,
    )?;
    let task = request
        .task_id
        .as_deref()
        .context("follow-up needs a task result")?;
    let policy = policy_in_view(
        connection,
        policy_state,
        state,
        network,
        scope,
        task,
        events,
        now_ms,
        true,
    )?;
    ensure!(
        policy.mode != FollowupMode::Off && policy.target_node == scheduler,
        "follow-up is disabled or belongs to another scheduler"
    );
    ensure!(
        request.action["type"] == "send"
            && ancestor(events, &dispatch.envelope.hash, &policy.task_ref),
        "task result is not derived from a task send"
    );
    let binding = request
        .binding
        .as_ref()
        .context("follow-up dispatch has no signed task binding")?;
    let creation = events
        .get(&policy.task_ref)
        .context("task creation is missing")?;
    ensure!(
        binding.binding.network_id == network
            && binding.binding.conversation_id == scope
            && binding.binding.task_id == task
            && binding.binding.task_ref == policy.task_ref
            && binding.binding.authority_node == creation.event.author
            && binding.binding.target_node == request.target_node
            && serde_json::to_value(&request.session_id)?
                == serde_json::to_value(Some(&binding.binding.session_id))?,
        "follow-up dispatch task binding mismatch"
    );
    Ok(policy)
}

#[allow(clippy::too_many_arguments)]
fn human_dispatch_source_in_view(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    state: &VerifiedPolicySet,
    network: &str,
    scope: &str,
    events: &BTreeMap<String, StoredEvent>,
    dispatch: &StoredEvent,
    request: &OperationRequest,
    now_ms: u64,
) -> Result<()> {
    let authority = request
        .manager_authority
        .as_ref()
        .context("dispatch has no manager invocation")?;
    ensure!(
        authority.followup_version == 0,
        "ordinary execution must originate in its Account manager invocation"
    );
    let source = events
        .get(&authority.trigger)
        .context("execution origin message is missing")?;
    let source_proof = invocation(&source.event)?;
    let proof = invocation(&dispatch.event)?;
    ensure!(
        matches!(
            proof.invocation.payload.actor,
            vibeface_secure::authorization::Actor::Service(_)
        ) && crate::crypto::fingerprint(proof.invocation.issuer.as_str())?
            == authority.scheduler_node
            && matches!(
                source_proof.invocation.payload.actor,
                vibeface_secure::authorization::Actor::Account(_)
            )
            && proof.source_body["parent_invocation"].as_str()
                == Some(source_proof.invocation.hash()?.as_str())
            && source.event.resource_id == dispatch.event.resource_id
            && matches!(&source.event.body, EventBody::MessagePosted { role, scheduler_node: Some(target), .. }
            if role == "user" && target == &authority.scheduler_node)
            && request.basis_heads.iter().any(
                |head| head == &authority.trigger || ancestor(events, head, &authority.trigger)
            ),
        "execution dispatch did not consume its exact original Account message"
    );
    authority_in_view(connection, policy, state, events, source, now_ms)?;
    if let Some((head, _)) = intent_in_view(
        connection,
        policy,
        state,
        network,
        scope,
        events,
        &authority.trigger,
        now_ms,
    )? {
        ensure!(
            request
                .basis_heads
                .iter()
                .any(|basis| basis == head || ancestor(events, basis, head)),
            "execution dispatch did not consume its result-report intent"
        );
    }
    Ok(())
}

fn ordinary_result_policy_from(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    network: &str,
    scope: &str,
    events: &BTreeMap<String, StoredEvent>,
    trigger: &str,
) -> Result<ResultFollowupPolicy> {
    ordinary_result_policy_in_view(
        connection,
        policy,
        policy.verified(),
        network,
        scope,
        events,
        trigger,
        crate::runtime::now_ms() as u64,
    )
}

#[allow(clippy::too_many_arguments)]
fn ordinary_result_policy_in_view(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    state: &VerifiedPolicySet,
    network: &str,
    scope: &str,
    events: &BTreeMap<String, StoredEvent>,
    trigger: &str,
    now_ms: u64,
) -> Result<ResultFollowupPolicy> {
    let (dispatch, request) =
        result_dispatch_in_view(connection, policy, state, events, scope, trigger, now_ms)?;
    ensure!(
        request.task_id.is_none()
            && request.binding.is_none()
            && (matches!(
                request.action["type"].as_str(),
                Some("command" | "send" | "steer" | "interrupt" | "respond_request")
            ) || (request.action["type"] == "open"
                && matches!(&events[trigger].event.body, EventBody::ExecutionObserved { status, .. } if status != "succeeded"))),
        "ordinary result does not match an ordinary execution"
    );
    human_dispatch_source_in_view(
        connection, policy, state, network, scope, events, dispatch, &request, now_ms,
    )?;
    let authority = request
        .manager_authority
        .as_ref()
        .context("manager invocation missing")?;
    let (head, intent) = intent_in_view(
        connection,
        policy,
        state,
        network,
        scope,
        events,
        &authority.trigger,
        now_ms,
    )?
    .context("ordinary result reporting intent is missing")?;
    ensure!(
        intent.mode == FollowupMode::Report,
        "ordinary result reporting is disabled"
    );
    Ok(ResultFollowupPolicy {
        task_id: None,
        target_node: authority.scheduler_node.clone(),
        mode: FollowupMode::Report,
        version: intent.version,
        proof_heads: vec![trigger.into(), authority.trigger.clone(), head.into()],
    })
}

pub(crate) fn validate_followup_result_from(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    network: &str,
    scope: &str,
    trigger: &str,
    scheduler: &str,
) -> Result<ResultFollowupPolicy> {
    let events = scope_events(connection, policy, network, scope)?;
    let heads = heads_from_events(&events, scope);
    validate_followup_result_in_view(
        connection,
        policy,
        policy.verified(),
        &events,
        network,
        scope,
        trigger,
        scheduler,
        &heads,
        crate::runtime::now_ms() as u64,
    )
}

/// Evaluates the selected causal snapshot with the caller's verified policy and clock.
/// It does not consult current Node identity or current task projections.
#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_followup_result_in_view(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    state: &VerifiedPolicySet,
    events: &BTreeMap<String, StoredEvent>,
    network: &str,
    scope: &str,
    trigger: &str,
    scheduler: &str,
    basis_heads: &[String],
    now_ms: u64,
) -> Result<ResultFollowupPolicy> {
    let closure = closure_hashes(events, basis_heads);
    ensure!(
        closure.contains(trigger),
        "follow-up trigger is absent from the selected history"
    );
    let source = events.get(trigger).context("follow-up result is missing")?;
    ensure!(
        source.event.network_id == network && source.event.scope_id == scope,
        "follow-up trigger belongs to another scope"
    );
    let mut selected = BTreeMap::new();
    for hash in closure {
        let stored = events
            .get(&hash)
            .context("follow-up history has missing dependencies")?;
        ensure!(
            stored.status == "ready"
                && stored.event.network_id == network
                && stored.event.resource_id == source.event.resource_id
                && u64::try_from(stored.event.created_at_ms).is_ok_and(|at| at <= now_ms),
            "follow-up history is not ready in the selected Project and clock"
        );
        selected.insert(hash, stored.clone());
    }
    let result = match &source.event.body {
        EventBody::ExecutionObserved {
            task_id: Some(_), ..
        } => validate_task_result_in_view(
            connection, policy, state, &selected, network, scope, trigger, scheduler, now_ms,
        )?
        .into(),
        EventBody::ExecutionObserved { task_id: None, .. } => ordinary_result_policy_in_view(
            connection, policy, state, network, scope, &selected, trigger, now_ms,
        )?,
        _ => bail!("follow-up requires an execution result"),
    };
    ensure!(
        result.target_node == scheduler,
        "execution result belongs to another scheduler"
    );
    Ok(result)
}

impl Store {
    pub fn report_intent_head(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
    ) -> Result<Option<String>> {
        report_intent_head_from(&self.connection, &self.policy, network, scope, trigger)
    }
    pub fn followup_policy(
        &self,
        network: &str,
        scope: &str,
        task: &str,
    ) -> Result<FollowupPolicy> {
        followup_policy_from(&self.connection, &self.policy, network, scope, task)
    }
    pub fn historical_followup_policy(
        &self,
        network: &str,
        scope: &str,
        task: &str,
    ) -> Result<FollowupPolicy> {
        policy_from(&self.connection, &self.policy, network, scope, task, false)
    }
    pub fn validate_followup_result(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
        scheduler: &str,
    ) -> Result<ResultFollowupPolicy> {
        validate_followup_result_from(
            &self.connection,
            &self.policy,
            network,
            scope,
            trigger,
            scheduler,
        )
    }
    pub fn execution_result_policy(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
    ) -> Result<ResultFollowupPolicy> {
        let events = scope_events(&self.connection, &self.policy, network, scope)?;
        let result = events.get(trigger).context("execution result is missing")?;
        match &result.event.body {
            EventBody::ExecutionObserved {
                task_id: Some(task),
                ..
            } => {
                let policy = self.followup_policy(network, scope, task)?;
                self.validate_followup_result(network, scope, trigger, &policy.target_node)
            }
            EventBody::ExecutionObserved { task_id: None, .. } => ordinary_result_policy_from(
                &self.connection,
                &self.policy,
                network,
                scope,
                &events,
                trigger,
            ),
            _ => bail!("follow-up requires an execution result"),
        }
    }
    pub fn original_message_route(
        &self,
        network: &str,
        scope: &str,
        message_id: &str,
    ) -> Result<Option<(String, bool)>> {
        let events = scope_events(&self.connection, &self.policy, network, scope)?;
        let mut found = None;
        for (hash, source) in &events {
            let EventBody::MessagePosted {
                message_id: id,
                role,
                scheduler_node,
                ..
            } = &source.event.body
            else {
                continue;
            };
            if source.event.scope_id != scope || role != "user" || id != message_id {
                continue;
            }
            ensure!(
                source.status == "ready" && found.is_none(),
                "original message is unavailable or ambiguous"
            );
            let target = scheduler_node
                .as_ref()
                .context("original message has no fixed scheduler")?;
            let report = intent_from(
                &self.connection,
                &self.policy,
                network,
                scope,
                &events,
                hash,
                false,
            )?
            .is_some_and(|(_, intent)| intent.mode == FollowupMode::Report);
            found = Some((target.clone(), report));
        }
        Ok(found)
    }
    pub fn require_followup_version(
        &self,
        network: &str,
        scope: &str,
        task: &str,
        mode: FollowupMode,
        version: u64,
        target: &str,
    ) -> Result<()> {
        let current = self.followup_policy(network, scope, task)?;
        ensure!(
            current.version == version
                && current.mode == mode
                && current.mode != FollowupMode::Off
                && current.target_node == target,
            "task follow-up permission was revoked or changed"
        );
        Ok(())
    }
    pub fn validate_latest_manager_policy(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
        target: &str,
        expected_version: u64,
        write_task: Option<&str>,
    ) -> Result<()> {
        let current = self.validate_followup_result(network, scope, trigger, target)?;
        ensure!(
            current.version == expected_version,
            "task follow-up permission was revoked or changed"
        );
        if let Some(task) = write_task {
            ensure!(
                current.mode == FollowupMode::Continue && current.task_id.as_deref() == Some(task),
                "follow-up permits no business writes outside its authorized task"
            );
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_task_followup(
        &mut self,
        identity: &LocalIdentity,
        auth: &vibeface_secure::authorization::VerifiedAuthorization,
        freshness: &crate::identity::policy::FreshnessProof,
        transport: &crate::identity::policy::RequestTransport,
        source_body: &Value,
        at: i64,
    ) -> Result<SignedEvent> {
        ensure!(
            matches!(
                auth.actor(),
                vibeface_secure::authorization::Actor::Account(_)
            ),
            "only an Account grants or revokes task follow-up"
        );
        let body = grant_body_from_request(source_body)?;
        let network = auth.resource().space.as_str();
        let scope = field(source_body, "conversation_id")?;
        let task = field(source_body, "task_id")?;
        let mode: FollowupMode = serde_json::from_value(source_body["mode"].clone())?;
        let now = u64::try_from(at).context("event creation time is negative")?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let events = scope_events(&tx, &self.policy, network, scope)?;
        if let Some(existing) = events.values().find(|stored| matches!(&stored.event.cause,
            EventCause::Invocation { admission: original } if original.invocation == *auth.signed_invocation())
            && stored.event.body == body) {
            let original = invocation(&existing.event)?;
            ensure!(
                original.source_body == *source_body
                    && original.transport == transport.stored(),
                "task follow-up retry changed its original authorization"
            );
            crate::identity::admission::validate_persisted_admission(&tx, original)?;
            return Ok(existing.envelope.clone());
        }
        let admission = crate::identity::admission::consume_request_authorization_in(
            &tx,
            &self.policy,
            auth,
            freshness,
            transport,
            source_body,
            None,
            now,
        )?;
        let current = policy_from(
            &tx,
            &self.policy,
            network,
            scope,
            task,
            mode != FollowupMode::Off,
        )?;
        ensure!(
            source_body["expected_version"].as_u64() == Some(current.version)
                && source_body["task_ref"].as_str() == Some(current.task_ref.as_str())
                && source_body["previous_head"].as_str()
                    == current.proof_heads.first().map(String::as_str),
            "task follow-up policy changed; refresh before granting"
        );
        let selected = field(source_body, "scheduler_node")?;
        ensure!(
            current.target_node.is_empty() || current.target_node == selected,
            "task follow-up must keep its original scheduler"
        );
        let creation = events
            .get(&current.task_ref)
            .context("task creation missing")?;
        ensure!(
            creation.event.resource_id == auth.resource().resource,
            "follow-up task is outside the admitted resource"
        );
        if mode != FollowupMode::Off {
            scheduler_available(&tx, &self.policy, network, selected)?;
        }
        let envelope = crate::identity::event_authorization::append_admitted_events_in(
            &tx,
            &self.policy,
            identity,
            &admission,
            vec![(scope.into(), body, vec![])],
            now,
        )?
        .remove(0);
        policy_from(
            &tx,
            &self.policy,
            network,
            scope,
            task,
            mode != FollowupMode::Off,
        )?;
        let checkpoint = crate::identity::admission::prepare_local_admission_checkpoint_in(
            &tx,
            &self.policy,
            &admission,
        )?;
        tx.commit()?;
        self.policy.local_admissions.insert(checkpoint);
        Ok(envelope)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_task_with_origin(
        &mut self,
        identity: &LocalIdentity,
        auth: &vibeface_secure::authorization::VerifiedAuthorization,
        freshness: &crate::identity::policy::FreshnessProof,
        transport: &crate::identity::policy::RequestTransport,
        source_body: &Value,
        at: i64,
    ) -> Result<SignedEvent> {
        ensure!(
            source_body["method"] == "create_task" && source_body["auto_followup"] != true,
            "task creation cannot grant follow-up permission"
        );
        let scope = field(source_body, "conversation_id")?;
        let task_id = field(source_body, "task_id")?;
        let body = EventBody::TaskCreated {
            task_id: task_id.into(),
            title: field(source_body, "title")?.into(),
            goal: field(source_body, "goal")?.into(),
            acceptance: field(source_body, "acceptance")?.into(),
        };
        let network = auth.resource().space.as_str();
        let now = u64::try_from(at).context("event creation time is negative")?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let events = scope_events(&tx, &self.policy, network, scope)?;
        let resource: String = tx.query_row(
            "SELECT resource_id FROM conversations WHERE space_id=? AND conversation_id=?",
            params![network, scope],
            |row| row.get(0),
        )?;
        ensure!(
            resource == auth.resource().resource.as_str(),
            "task conversation is outside the admitted resource"
        );
        let previous = if events.values().any(|event| {
            event.event.scope_id == scope
                && matches!(&event.event.body,
            EventBody::TaskCreated { task_id: id, .. } if id == task_id)
        }) {
            Some(creation_from(&events, scope, task_id)?.1)
        } else {
            None
        };
        if let Some(previous) = previous {
            let original = invocation(&previous.event)?;
            if original.invocation == *auth.signed_invocation() {
                ensure!(
                    previous.event.body == body
                        && previous.event.resource_id == auth.resource().resource
                        && original.source_body == *source_body
                        && original.transport == transport.stored(),
                    "task retry changed its original authorization"
                );
                crate::identity::admission::validate_persisted_admission(&tx, original)?;
                return Ok(previous.envelope.clone());
            }
        }
        let admission = crate::identity::admission::consume_request_authorization_in(
            &tx,
            &self.policy,
            auth,
            freshness,
            transport,
            source_body,
            None,
            now,
        )?;
        if let Some(previous) = previous {
            let previous_admission = invocation(&previous.event)?;
            ensure!(
                previous.event.body == body
                    && previous.event.resource_id == auth.resource().resource
                    && previous_admission.invocation.payload.actor
                        == admission.invocation.payload.actor
                    && previous_admission.invocation.payload.initiator
                        == admission.invocation.payload.initiator
                    && ["manager_authority", "parent_invocation"]
                        .iter()
                        .all(|key| previous_admission.source_body[*key]
                            == admission.source_body[*key]),
                "task retry cannot change its content or rebind its origin"
            );
            return Ok(previous.envelope.clone());
        }
        if matches!(
            auth.actor(),
            vibeface_secure::authorization::Actor::Service(_)
        ) {
            let authority: ManagerDispatchAuthority =
                serde_json::from_value(source_body["manager_authority"].clone())?;
            let attempt_id = authority.attempt_id.as_str();
            let scheduler = identity.node_id()?;
            ensure!(
                authority.scheduler_node == scheduler && authority.followup_version == 0,
                "only an Account message manager can create a task"
            );
            let session = load_scheduler_session(&tx, network, scope, &scheduler)?;
            let job: SchedulerJob = scheduler_load(
                &tx,
                network,
                &scheduler_key("job", scope, &scheduler, &authority.trigger),
            )?
            .context("task origin has no claimed invocation")?;
            let attempt = job
                .state
                .attempt()
                .context("task origin invocation has no attempt")?;
            ensure!(
                job.kind == TriggerKind::Human
                    && job.conversation_id == scope
                    && job.trigger == authority.trigger
                    && job.target_node == scheduler
                    && matches!(
                        job.state,
                        JobState::Claimed { .. } | JobState::Running { .. }
                    )
                    && attempt.id == attempt_id
                    && !attempt.read_only
                    && attempt.permission_version == session.permission_version
                    && authority.permission_version == session.permission_version
                    && session.active.as_deref() == Some(authority.trigger.as_str())
                    && session.unresolved.is_empty(),
                "task origin invocation is not currently authorized"
            );
            let source = events
                .get(&authority.trigger)
                .context("task origin message is missing")?;
            current_authority(&tx, &self.policy, &events, source)?;
        }
        let creation = crate::identity::event_authorization::append_admitted_events_in(
            &tx,
            &self.policy,
            identity,
            &admission,
            vec![(scope.into(), body, vec![])],
            now,
        )?
        .remove(0);
        let events = scope_events(&tx, &self.policy, network, scope)?;
        task_origin_from(
            &tx,
            &self.policy,
            network,
            scope,
            &events,
            &creation.hash,
            events
                .get(&creation.hash)
                .context("task creation projection missing")?,
        )?;
        let checkpoint = crate::identity::admission::prepare_local_admission_checkpoint_in(
            &tx,
            &self.policy,
            &admission,
        )?;
        tx.commit()?;
        self.policy.local_admissions.insert(checkpoint);
        Ok(creation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::test_support::{TestNetwork, now};
    use serde_json::json;
    use vibeface_secure::authorization::CheckStage;

    fn task(network: &TestNetwork, store: &mut Store) -> Result<SignedEvent> {
        let mut body = network.body("create_task", "conversation");
        body["task_id"] = json!("task");
        body["title"] = json!("Review");
        body["goal"] = json!("Review the change");
        body["acceptance"] = json!("Report findings");
        let (auth, transport) = network.authorization(store, &body, now())?;
        let freshness = network.freshness(store, &auth, CheckStage::Admit, now())?;
        store.create_task_with_origin(
            &network.node,
            &auth,
            &freshness,
            &crate::identity::policy::RequestTransport::Existing(transport),
            &body,
            now() as i64,
        )
    }

    fn grant_body(network: &TestNetwork, current: &FollowupPolicy, mode: FollowupMode) -> Value {
        let mut body = network.body("set_followup", "conversation");
        body["task_id"] = json!("task");
        body["task_ref"] = json!(current.task_ref);
        body["expected_version"] = json!(current.version);
        body["previous_head"] = json!(current.proof_heads[0]);
        body["scheduler_node"] = json!(network.node.node_id().unwrap());
        body["mode"] = serde_json::to_value(mode).unwrap();
        body
    }

    fn grant(network: &TestNetwork, store: &mut Store, body: &Value) -> Result<SignedEvent> {
        let (auth, transport) = network.authorization(store, body, now())?;
        let freshness = network.freshness(store, &auth, CheckStage::Admit, now())?;
        store.set_task_followup(
            &network.node,
            &auth,
            &freshness,
            &crate::identity::policy::RequestTransport::Existing(transport),
            body,
            now() as i64,
        )
    }

    #[test]
    fn retries_preserve_the_original_message_report_intent() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        network.conversation(&mut store, "conversation")?;
        let (mut body, event) = network.message_body("conversation", "Review this change")?;
        body["report_results"] = json!(true);
        let message = network.append(&mut store, &body, event, now())?;
        assert_eq!(
            store.original_message_route(
                network.space.as_str(),
                "conversation",
                body["message_id"].as_str().unwrap()
            )?,
            Some((network.node.node_id()?, true))
        );
        assert_eq!(
            store.report_intent_head(network.space.as_str(), "conversation", &message.hash)?,
            Some(message.hash.clone())
        );
        let events = load_events_from(&store.connection, &store.policy, network.space.as_str())?;
        assert_eq!(
            events.len(),
            2,
            "intent is part of the admitted message, not an unsigned extra event"
        );
        assert!(
            append_report_intent_from(
                &store.connection,
                &store.policy,
                network.space.as_str(),
                "conversation",
                &message,
                &network.node.node_id()?,
                false
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn late_grant_uses_original_task_and_fences_frozen_version() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        network.conversation(&mut store, "conversation")?;
        let created = task(&network, &mut store)?;
        let initial = store.followup_policy(network.space.as_str(), "conversation", "task")?;
        assert_eq!((initial.mode, initial.version), (FollowupMode::Off, 0));
        assert_eq!(initial.task_ref, created.hash);
        let request = grant_body(&network, &initial, FollowupMode::Continue);
        let granted = grant(&network, &mut store, &request)?;
        assert_eq!(grant(&network, &mut store, &request)?, granted);
        let current = store.followup_policy(network.space.as_str(), "conversation", "task")?;
        assert_eq!((current.mode, current.version), (FollowupMode::Continue, 1));
        let stale = grant_body(&network, &initial, FollowupMode::Report);
        assert!(grant(&network, &mut store, &stale).is_err());
        grant(
            &network,
            &mut store,
            &grant_body(&network, &current, FollowupMode::Off),
        )?;
        assert!(
            store
                .require_followup_version(
                    network.space.as_str(),
                    "conversation",
                    "task",
                    FollowupMode::Continue,
                    1,
                    &network.node.node_id()?
                )
                .is_err()
        );
        let final_policy =
            store.historical_followup_policy(network.space.as_str(), "conversation", "task")?;
        assert_eq!(
            (final_policy.mode, final_policy.version),
            (FollowupMode::Off, 2)
        );
        Ok(())
    }

    #[test]
    fn revoked_account_stops_current_grant_but_preserves_original_history() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        network.conversation(&mut store, "conversation")?;
        task(&network, &mut store)?;
        let initial = store.followup_policy(network.space.as_str(), "conversation", "task")?;
        grant(
            &network,
            &mut store,
            &grant_body(&network, &initial, FollowupMode::Report),
        )?;
        let original =
            store.historical_followup_policy(network.space.as_str(), "conversation", "task")?;
        network.revoke_client(&mut store)?;
        assert!(
            store
                .followup_policy(network.space.as_str(), "conversation", "task")
                .is_err()
        );
        assert_eq!(
            store.historical_followup_policy(network.space.as_str(), "conversation", "task")?,
            original
        );
        Ok(())
    }

    #[test]
    fn original_followup_view_ignores_later_revocation_and_task_status() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        network.conversation(&mut store, "conversation")?;
        task(&network, &mut store)?;
        let initial = store.followup_policy(network.space.as_str(), "conversation", "task")?;
        grant(
            &network,
            &mut store,
            &grant_body(&network, &initial, FollowupMode::Continue),
        )?;
        let original_state = store.authorization_state().clone();
        let original_events = scope_events(
            &store.connection,
            &store.policy,
            network.space.as_str(),
            "conversation",
        )?;
        let before = policy_in_view(
            &store.connection,
            &store.policy,
            &original_state,
            network.space.as_str(),
            "conversation",
            "task",
            &original_events,
            now(),
            true,
        )?;
        let mut change = network.body("update_task", "conversation");
        change["task_id"] = json!("task");
        change["status"] = json!("accepted");
        change["detail"] = json!("Accepted after the original invocation");
        network.append(
            &mut store,
            &change,
            EventBody::TaskChanged {
                task_id: "task".into(),
                status: "accepted".into(),
                detail: "Accepted after the original invocation".into(),
            },
            now(),
        )?;
        network.revoke_client(&mut store)?;
        assert_eq!(
            store
                .historical_followup_policy(network.space.as_str(), "conversation", "task")?
                .mode,
            FollowupMode::Off
        );
        assert_eq!(
            policy_in_view(
                &store.connection,
                &store.policy,
                &original_state,
                network.space.as_str(),
                "conversation",
                "task",
                &original_events,
                now(),
                true
            )?,
            before
        );
        assert_eq!(before.mode, FollowupMode::Continue);
        Ok(())
    }

    #[test]
    fn task_retry_never_rebinds_origin_and_auto_followup_is_not_a_grant() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        network.conversation(&mut store, "conversation")?;
        let original = task(&network, &mut store)?;
        assert_eq!(task(&network, &mut store)?, original);
        let event: Event = serde_json::from_str(&original.canonical_body)?;
        let original_body = &invocation(&event)?.source_body;
        let (auth, transport) = network.authorization(&store, original_body, now())?;
        let freshness = network.freshness(&mut store, &auth, CheckStage::Admit, now())?;
        assert_eq!(
            store.create_task_with_origin(
                &network.node,
                &auth,
                &freshness,
                &crate::identity::policy::RequestTransport::Existing(transport),
                original_body,
                now() as i64,
            )?,
            original
        );
        let mut body = network.body("create_task", "conversation");
        body["task_id"] = json!("task");
        body["title"] = json!("Review");
        body["goal"] = json!("Review the change");
        body["acceptance"] = json!("Report findings");
        body["auto_followup"] = json!(true);
        let (auth, transport) = network.authorization(&store, &body, now())?;
        let freshness = network.freshness(&mut store, &auth, CheckStage::Admit, now())?;
        assert!(
            store
                .create_task_with_origin(
                    &network.node,
                    &auth,
                    &freshness,
                    &crate::identity::policy::RequestTransport::Existing(transport),
                    &body,
                    now() as i64
                )
                .is_err()
        );
        assert_eq!(
            store
                .followup_policy(network.space.as_str(), "conversation", "task")?
                .mode,
            FollowupMode::Off
        );
        Ok(())
    }
}
