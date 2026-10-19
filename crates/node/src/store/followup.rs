use super::*;
use crate::scheduling::{JobState, SchedulerJob, TriggerKind, scheduler_key};
use serde::{Deserialize, Serialize};

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
    pub network_id: String,
    pub conversation_id: String,
    pub task_id: String,
    pub task_ref: String,
    pub trigger: String,
    pub attempt_id: String,
    pub target_node: String,
    pub intent_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskFollowupGrantV1 {
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

fn record_body(record: FollowupRecord) -> Result<EventBody> {
    let id = match &record {
        FollowupRecord::FollowupIntentV1(v) => format!("followup-intent:v1:{}", v.message_hash),
        FollowupRecord::TaskOriginV1(v) => format!("task-origin:v1:{}", v.task_ref),
        FollowupRecord::TaskFollowupGrantV1(v) => format!("{}:{}", v.grant_id, v.version),
    };
    Ok(EventBody::DecisionRecorded {
        decision_id: id,
        text: "Task follow-up policy".into(),
        effective_constraints: vec![],
        tool_configuration: serde_json::to_value(record)?,
    })
}

fn record(event: &Event) -> Result<Option<FollowupRecord>> {
    let EventBody::DecisionRecorded {
        decision_id,
        tool_configuration,
        ..
    } = &event.body
    else {
        return Ok(None);
    };
    if !matches!(
        tool_configuration["type"].as_str(),
        Some("followup_intent_v1" | "task_origin_v1" | "task_followup_grant_v1")
    ) {
        return Ok(None);
    }
    let value: FollowupRecord = serde_json::from_value(tool_configuration.clone())?;
    let EventBody::DecisionRecorded {
        decision_id: expected,
        ..
    } = record_body(value.clone())?
    else {
        unreachable!()
    };
    ensure!(
        *decision_id == expected,
        "follow-up record identity mismatch"
    );
    Ok(Some(value))
}

fn scope_events(
    connection: &Connection,
    network: &str,
    scope: &str,
) -> Result<BTreeMap<String, StoredEvent>> {
    let events = load_events_from(connection, network)?;
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

fn intent_from<'a>(
    connection: &Connection,
    network: &str,
    scope: &str,
    events: &'a BTreeMap<String, StoredEvent>,
    trigger: &str,
    check_roles: bool,
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
    ensure!(role == "user", "follow-up intent requires a user message");
    if check_roles {
        authorize_node_from(
            connection,
            network,
            &message.event.author,
            Some(NodeRole::Gateway),
        )?;
    }
    let mut found = None;
    for (hash, stored) in events
        .iter()
        .filter(|(_, e)| e.status == "ready" && e.event.scope_id == scope)
    {
        let Some(FollowupRecord::FollowupIntentV1(intent)) = record(&stored.event)? else {
            continue;
        };
        if intent.message_hash != trigger {
            continue;
        }
        ensure!(found.is_none(), "conflicting message follow-up intent");
        ensure!(
            intent.network_id == network
                && intent.conversation_id == scope
                && intent.version == 1
                && intent.mode != FollowupMode::Continue
                && intent.target_node == *target
                && stored.event.author == message.event.author
                && ancestor(events, hash, trigger),
            "invalid message follow-up intent"
        );
        if check_roles {
            authorize_node_from(
                connection,
                network,
                &stored.event.author,
                Some(NodeRole::Gateway),
            )?;
            authorize_node_from(connection, network, target, Some(NodeRole::Scheduler))?;
        }
        found = Some((hash.as_str(), intent));
    }
    Ok(found)
}

pub(super) fn report_intent_head_from(
    connection: &Connection,
    network: &str,
    scope: &str,
    trigger: &str,
) -> Result<Option<String>> {
    let events = scope_events(connection, network, scope)?;
    let source = events
        .get(trigger)
        .context("scheduler trigger is missing")?;
    if !matches!(&source.event.body, EventBody::MessagePosted { role, .. } if role == "user") {
        return Ok(None);
    }
    Ok(
        intent_from(connection, network, scope, &events, trigger, true)?
            .map(|(head, _)| head.to_owned()),
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "the transaction binds its full authorization and durable record identity"
)]
pub(super) fn append_report_intent_from(
    connection: &Connection,
    identity: &LocalIdentity,
    network: &str,
    scope: &str,
    message: &SignedEvent,
    target: &str,
    report_results: bool,
    at: i64,
) -> Result<Option<String>> {
    let author = identity.peer_id()?;
    authorize_node_from(connection, network, &author, Some(NodeRole::Gateway))?;
    authorize_node_from(connection, network, target, Some(NodeRole::Scheduler))?;
    let events = load_events_from(connection, network)?;
    let source = events.get(&message.hash).context("message is missing")?;
    ensure!(
        source.envelope == *message && source.event.author == author,
        "intent must be signed by the original message gateway"
    );
    ensure!(
        matches!(&source.event.body, EventBody::MessagePosted { role, scheduler_node: Some(selected), .. } if role == "user" && selected == target),
        "intent must preserve the message's scheduler"
    );
    let mode = if report_results {
        FollowupMode::Report
    } else {
        FollowupMode::Off
    };
    if let Some((hash, intent)) =
        intent_from(connection, network, scope, &events, &message.hash, true)?
    {
        ensure!(
            intent.mode == mode && intent.target_node == target,
            "message retry cannot change its follow-up intent"
        );
        return Ok(Some(hash.to_owned()));
    }
    scope_events(connection, network, scope)?;
    let intent = FollowupIntentV1 {
        network_id: network.into(),
        conversation_id: scope.into(),
        message_hash: message.hash.clone(),
        target_node: target.into(),
        mode,
        version: 1,
    };
    let envelope = append_event_from(
        connection,
        identity,
        network,
        scope,
        record_body(FollowupRecord::FollowupIntentV1(intent))?,
        vec![],
        at,
    )?;
    Ok(Some(envelope.hash))
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

pub(super) fn followup_policy_from(
    connection: &Connection,
    network: &str,
    scope: &str,
    task: &str,
) -> Result<FollowupPolicy> {
    policy_from(connection, network, scope, task, true)
}

fn policy_from(
    connection: &Connection,
    network: &str,
    scope: &str,
    task: &str,
    check_roles: bool,
) -> Result<FollowupPolicy> {
    let events = scope_events(connection, network, scope)?;
    let (task_ref, creation) = creation_from(&events, scope, task)?;
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
    let mut origins = Vec::new();
    let mut authorities = Vec::new();
    let mut grants = BTreeMap::new();
    for (hash, stored) in events
        .iter()
        .filter(|(_, e)| e.status == "ready" && e.event.scope_id == scope)
    {
        match record(&stored.event)? {
            Some(FollowupRecord::TaskOriginV1(origin))
                if origin.task_id == task || origin.task_ref == task_ref =>
            {
                ensure!(
                    origin.network_id == network
                        && origin.conversation_id == scope
                        && origin.task_id == task
                        && origin.task_ref == task_ref
                        && origin.target_node == stored.event.author
                        && origin.target_node == creation.event.author
                        && !origin.attempt_id.is_empty()
                        && ancestor(&events, hash, task_ref),
                    "task origin does not match its signed creation"
                );
                authorities.push((origin.target_node.clone(), NodeRole::Scheduler));
                let message = events
                    .get(&origin.trigger)
                    .context("task origin message is missing")?;
                ensure!(
                    ancestor(&events, task_ref, &origin.trigger)
                        && matches!(&message.event.body, EventBody::MessagePosted { role, scheduler_node: Some(target), .. } if role == "user" && *target == origin.target_node),
                    "task origin is not its scheduler's user invocation"
                );
                authorities.push((message.event.author.clone(), NodeRole::Gateway));
                let intent =
                    intent_from(connection, network, scope, &events, &origin.trigger, false)?;
                ensure!(
                    origin.intent_ref.as_deref() == intent.as_ref().map(|(head, _)| *head),
                    "task origin intent proof mismatch"
                );
                if let Some((head, _)) = &intent {
                    ensure!(
                        ancestor(&events, task_ref, head),
                        "task creation did not consume the intent proof"
                    );
                }
                origins.push((hash, origin, intent));
            }
            Some(FollowupRecord::TaskFollowupGrantV1(grant))
                if grant.task_id == task || grant.task_ref == task_ref =>
            {
                ensure!(
                    grant.network_id == network
                        && grant.conversation_id == scope
                        && grant.task_id == task
                        && grant.task_ref == task_ref
                        && grant.grant_id == policy.grant_id
                        && ancestor(&events, hash, task_ref),
                    "task grant has the wrong scope"
                );
                authorities.push((stored.event.author.clone(), NodeRole::Gateway));
                authorities.push((grant.target_node.clone(), NodeRole::Scheduler));
                ensure!(
                    grants.insert(grant.version, (hash, grant)).is_none(),
                    "conflicting task follow-up versions"
                );
            }
            _ => {}
        }
    }
    ensure!(origins.len() <= 1, "task origin cannot be rebound");
    if let Some((head, origin, intent)) = origins.pop() {
        policy.target_node = origin.target_node;
        policy.proof_heads = vec![head.clone()];
        if let Some((_, intent)) = intent {
            policy.mode = intent.mode;
            policy.version = intent.version;
        }
    }
    for (version, (head, grant)) in grants {
        ensure!(
            version
                == policy
                    .version
                    .checked_add(1)
                    .context("follow-up version exhausted")?
                && grant.previous_head == policy.proof_heads[0]
                && ancestor(&events, head, &grant.previous_head)
                && (policy.target_node.is_empty() || grant.target_node == policy.target_node),
            "task follow-up version chain is invalid"
        );
        policy.mode = grant.mode;
        policy.version = version;
        policy.target_node = grant.target_node;
        policy.proof_heads = vec![head.clone()];
    }
    let status: String = connection.query_row(
        "SELECT status FROM tasks WHERE network_id=? AND conversation_id=? AND task_id=?",
        params![network, scope, task],
        |row| row.get(0),
    )?;
    if matches!(status.as_str(), "accepted" | "cancelled") {
        policy.mode = FollowupMode::Off;
    }
    if check_roles && policy.mode != FollowupMode::Off {
        for (author, role) in authorities {
            authorize_node_from(connection, network, &author, Some(role))?;
        }
    }
    Ok(policy)
}

fn validate_task_result_from(
    connection: &Connection,
    network: &str,
    scope: &str,
    trigger: &str,
    scheduler: &str,
) -> Result<FollowupPolicy> {
    let events = scope_events(connection, network, scope)?;
    let result = events.get(trigger).context("follow-up result is missing")?;
    ensure!(
        result.status == "ready" && result.event.scope_id == scope,
        "follow-up result has the wrong conversation"
    );
    let EventBody::ExecutionObserved {
        operation_id,
        task_id: Some(task),
        target_node,
        session_id,
        status,
        ..
    } = &result.event.body
    else {
        bail!("follow-up needs an execution task result")
    };
    ensure!(
        result.event.author == *target_node
            && matches!(status.as_str(), "succeeded" | "failed" | "rejected"),
        "follow-up requires the original executor's known terminal result"
    );
    authorize_node_from(connection, network, target_node, Some(NodeRole::Executor))?;
    let policy = followup_policy_from(connection, network, scope, task)?;
    ensure!(
        policy.mode != FollowupMode::Off && policy.target_node == scheduler,
        "follow-up is disabled or belongs to another scheduler"
    );
    let mut original = None;
    for hash in closure_hashes(&events, &[trigger.into()]) {
        let Some(dispatch) = events.get(&hash) else {
            continue;
        };
        let EventBody::DispatchRecorded {
            operation_id: id,
            task_id: Some(dispatch_task),
            target_node: target,
            session_id: session,
            action,
        } = &dispatch.event.body
        else {
            continue;
        };
        if id != operation_id {
            continue;
        }
        let request: OperationRequest = serde_json::from_value(action.clone())?;
        ensure!(
            dispatch.event.scope_id == scope
                && dispatch_task == task
                && target == target_node
                && session == session_id
                && request.network_id == network
                && request.conversation_id == scope
                && request.operation_id == *operation_id
                && request.task_id.as_ref() == Some(task)
                && request.target_node == *target_node
                && request.session_id == *session_id
                && request.action["type"] == "send"
                && ancestor(&events, &hash, &policy.task_ref),
            "execution result does not match its original dispatch"
        );
        let certificate = authorize_node_from(connection, network, &dispatch.event.author, None)?;
        ensure!(
            certificate.roles.contains(&NodeRole::Gateway)
                || certificate.roles.contains(&NodeRole::Scheduler),
            "dispatch author is not authorized"
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
                && binding.binding.task_id == *task
                && binding.binding.task_ref == policy.task_ref
                && binding.binding.authority_node == creation.event.author
                && binding.binding.target_node == *target_node
                && Some(binding.binding.session_id.as_str()) == session_id.as_deref(),
            "follow-up dispatch task binding mismatch"
        );
        let key: String = connection.query_row(
            "SELECT public_key FROM node_certificates WHERE network_id=? AND peer_id=?",
            params![network, binding.binding.authority_node],
            |row| row.get(0),
        )?;
        crate::binding::verify_binding(binding, &key)?;
        if let Some(original) = &original {
            ensure!(
                original == &request,
                "execution result has conflicting dispatch bodies"
            );
        } else {
            original = Some(request);
        }
    }
    ensure!(original.is_some(), "execution result dispatch is missing");
    Ok(policy)
}

fn human_dispatch_source_from(
    connection: &Connection,
    network: &str,
    scope: &str,
    events: &BTreeMap<String, StoredEvent>,
    request: &OperationRequest,
) -> Result<()> {
    let authority = request
        .manager_authority
        .as_ref()
        .context("execution dispatch has no original manager invocation")?;
    ensure!(
        authority.followup_version == 0,
        "ordinary execution must originate in a human invocation"
    );
    let source = events
        .get(&authority.trigger)
        .context("execution origin message is missing")?;
    ensure!(
        source.status == "ready"
            && source.event.scope_id == scope
            && matches!(&source.event.body,
                EventBody::MessagePosted { role, scheduler_node: Some(target), .. }
                if role == "user" && target == &authority.scheduler_node),
        "execution origin is not its fixed scheduler's user message"
    );
    authorize_node_from(
        connection,
        network,
        &source.event.author,
        Some(NodeRole::Gateway),
    )?;
    authorize_node_from(
        connection,
        network,
        &authority.scheduler_node,
        Some(NodeRole::Scheduler),
    )?;
    ensure!(
        request.basis_heads.iter().any(|head| {
            head == &authority.trigger || ancestor(events, head, &authority.trigger)
        }),
        "execution dispatch did not consume its original human message"
    );
    let intent = intent_from(connection, network, scope, events, &authority.trigger, true)?;
    if let Some((head, _)) = &intent {
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
    network: &str,
    scope: &str,
    events: &BTreeMap<String, StoredEvent>,
    trigger: &str,
) -> Result<ResultFollowupPolicy> {
    let result = events.get(trigger).context("execution result is missing")?;
    let EventBody::ExecutionObserved {
        operation_id,
        task_id: None,
        target_node,
        session_id,
        status,
        ..
    } = &result.event.body
    else {
        bail!("ordinary result requires an execution without a task")
    };
    ensure!(
        result.status == "ready"
            && result.event.scope_id == scope
            && result.event.author == *target_node
            && matches!(status.as_str(), "succeeded" | "failed" | "rejected"),
        "ordinary result requires its executor's known terminal result"
    );
    authorize_node_from(connection, network, target_node, Some(NodeRole::Executor))?;
    let mut original = None;
    for hash in closure_hashes(events, &[trigger.into()]) {
        let Some(dispatch) = events.get(&hash) else {
            continue;
        };
        let EventBody::DispatchRecorded {
            operation_id: id,
            task_id,
            target_node: target,
            session_id: session,
            action,
        } = &dispatch.event.body
        else {
            continue;
        };
        if id != operation_id {
            continue;
        }
        let request: OperationRequest = serde_json::from_value(action.clone())?;
        let authority = request
            .manager_authority
            .as_ref()
            .context("ordinary result has no original manager invocation")?;
        ensure!(
            dispatch.status == "ready"
                && dispatch.event.scope_id == scope
                && dispatch.event.author == authority.scheduler_node
                && task_id.is_none()
                && request.task_id.is_none()
                && request.binding.is_none()
                && target == target_node
                && session == session_id
                && request.network_id == network
                && request.conversation_id == scope
                && request.operation_id == *operation_id
                && request.target_node == *target_node
                && request.session_id == *session_id
                && (matches!(
                    request.action["type"].as_str(),
                    Some("command" | "send" | "steer" | "interrupt" | "respond_request")
                ) || (request.action["type"] == "open" && status != "succeeded")),
            "ordinary result does not match its original signed dispatch"
        );
        human_dispatch_source_from(connection, network, scope, events, &request)?;
        if let Some(original) = &original {
            ensure!(
                original == &request,
                "execution result has conflicting dispatch bodies"
            );
        } else {
            original = Some(request);
        }
    }
    let request = original.context("ordinary result dispatch is missing")?;
    let authority = request
        .manager_authority
        .as_ref()
        .context("manager invocation missing")?;
    let (head, intent) = intent_from(connection, network, scope, events, &authority.trigger, true)?
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

pub(super) fn validate_followup_result_from(
    connection: &Connection,
    network: &str,
    scope: &str,
    trigger: &str,
    scheduler: &str,
) -> Result<ResultFollowupPolicy> {
    let events = scope_events(connection, network, scope)?;
    let result = events.get(trigger).context("follow-up result is missing")?;
    let policy = match &result.event.body {
        EventBody::ExecutionObserved {
            task_id: Some(_), ..
        } => validate_task_result_from(connection, network, scope, trigger, scheduler)?.into(),
        EventBody::ExecutionObserved { task_id: None, .. } => {
            ordinary_result_policy_from(connection, network, scope, &events, trigger)?
        }
        _ => bail!("follow-up requires an execution result"),
    };
    ensure!(
        policy.target_node == scheduler,
        "execution result belongs to another scheduler"
    );
    Ok(policy)
}

pub(super) fn validate_manager_dispatch_from(
    connection: &Connection,
    network: &str,
    request: &OperationRequest,
) -> Result<()> {
    let Some(authority) = &request.manager_authority else {
        return Ok(());
    };
    let events = scope_events(connection, network, &request.conversation_id)?;
    if matches!(events.get(&authority.trigger).map(|source| &source.event.body),
        Some(EventBody::MessagePosted { role, .. }) if role == "user")
    {
        human_dispatch_source_from(
            connection,
            network,
            &request.conversation_id,
            &events,
            request,
        )?;
        return Ok(());
    }
    let policy = validate_followup_result_from(
        connection,
        network,
        &request.conversation_id,
        &authority.trigger,
        &authority.scheduler_node,
    )?;
    ensure!(
        policy.version == authority.followup_version
            && policy.mode == FollowupMode::Continue
            && request.task_id.is_some()
            && request.task_id == policy.task_id,
        "autonomous dispatch permission was revoked or changed"
    );
    Ok(())
}

impl Store {
    pub fn report_intent_head(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
    ) -> Result<Option<String>> {
        report_intent_head_from(&self.connection, network, scope, trigger)
    }

    pub fn followup_policy(
        &self,
        network: &str,
        scope: &str,
        task: &str,
    ) -> Result<FollowupPolicy> {
        followup_policy_from(&self.connection, network, scope, task)
    }

    pub fn historical_followup_policy(
        &self,
        network: &str,
        scope: &str,
        task: &str,
    ) -> Result<FollowupPolicy> {
        policy_from(&self.connection, network, scope, task, false)
    }

    pub fn validate_followup_result(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
        scheduler: &str,
    ) -> Result<ResultFollowupPolicy> {
        validate_followup_result_from(&self.connection, network, scope, trigger, scheduler)
    }

    pub fn execution_result_policy(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
    ) -> Result<ResultFollowupPolicy> {
        let events = scope_events(&self.connection, network, scope)?;
        let result = events.get(trigger).context("execution result is missing")?;
        match &result.event.body {
            EventBody::ExecutionObserved {
                task_id: Some(task),
                ..
            } => {
                let policy = self.followup_policy(network, scope, task)?;
                self.validate_followup_result(network, scope, trigger, &policy.target_node)
            }
            EventBody::ExecutionObserved { task_id: None, .. } => {
                ordinary_result_policy_from(&self.connection, network, scope, &events, trigger)
            }
            _ => bail!("follow-up requires an execution result"),
        }
    }

    pub fn original_message_route(
        &self,
        network: &str,
        scope: &str,
        message_id: &str,
    ) -> Result<Option<(String, bool)>> {
        let events = load_events_from(&self.connection, network)?;
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
            let report = intent_from(&self.connection, network, scope, &events, hash, false)?
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

    #[expect(
        clippy::too_many_arguments,
        reason = "the transaction binds its full authorization and durable record identity"
    )]
    pub fn set_task_followup(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        scope: &str,
        task: &str,
        mode: FollowupMode,
        expected_version: u64,
        target: Option<&str>,
        at: i64,
    ) -> Result<SignedEvent> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize_node_from(&tx, network, &identity.peer_id()?, Some(NodeRole::Gateway))?;
        let current = policy_from(&tx, network, scope, task, mode != FollowupMode::Off)?;
        ensure!(
            current.version == expected_version,
            "task follow-up policy changed; refresh before granting"
        );
        let selected = target
            .filter(|value| !value.is_empty())
            .unwrap_or(&current.target_node);
        ensure!(
            !selected.is_empty()
                && (current.target_node.is_empty() || current.target_node == selected),
            "task follow-up must keep its original scheduler"
        );
        if mode != FollowupMode::Off {
            authorize_node_from(&tx, network, selected, Some(NodeRole::Scheduler))?;
        }
        let grant = TaskFollowupGrantV1 {
            network_id: network.into(),
            conversation_id: scope.into(),
            task_id: task.into(),
            task_ref: current.task_ref,
            grant_id: current.grant_id,
            target_node: selected.into(),
            mode,
            version: current
                .version
                .checked_add(1)
                .context("follow-up version exhausted")?,
            previous_head: current.proof_heads[0].clone(),
        };
        let envelope = append_event_from(
            &tx,
            identity,
            network,
            scope,
            record_body(FollowupRecord::TaskFollowupGrantV1(grant))?,
            vec![],
            at,
        )?;
        policy_from(&tx, network, scope, task, mode != FollowupMode::Off)?;
        tx.commit()?;
        Ok(envelope)
    }

    pub fn create_task_with_origin(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        scope: &str,
        body: EventBody,
        origin: Option<(&str, &str)>,
        at: i64,
    ) -> Result<SignedEvent> {
        let EventBody::TaskCreated { task_id, .. } = &body else {
            bail!("task creation body is required")
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let events = scope_events(&tx, network, scope)?;
        if events.values().any(|event| event.event.scope_id == scope && matches!(&event.event.body, EventBody::TaskCreated { task_id: id, .. } if id == task_id)) {
            let (_, previous) = creation_from(&events, scope, task_id)?;
            ensure!(previous.event.body == body, "task ID reused with different content");
            return Ok(previous.envelope.clone());
        }
        let scheduler = identity.peer_id()?;
        let verified_origin = if let Some((trigger, attempt_id)) = origin {
            authorize_node_from(&tx, network, &scheduler, Some(NodeRole::Scheduler))?;
            let session = load_scheduler_session(&tx, network, scope, &scheduler)?;
            let job: SchedulerJob = scheduler_load(
                &tx,
                network,
                &scheduler_key("job", scope, &scheduler, trigger),
            )?
            .context("task origin has no claimed invocation")?;
            let attempt = job
                .state
                .attempt()
                .context("task origin invocation has no attempt")?;
            ensure!(
                job.kind == TriggerKind::Human
                    && job.conversation_id == scope
                    && job.trigger == trigger
                    && job.target_node == scheduler
                    && matches!(
                        job.state,
                        JobState::Claimed { .. } | JobState::Running { .. }
                    )
                    && attempt.id == attempt_id
                    && !attempt.read_only
                    && attempt.permission_version == session.permission_version
                    && session.active.as_deref() == Some(trigger)
                    && session.unresolved.is_empty(),
                "task origin invocation is not currently authorized"
            );
            let intent = intent_from(&tx, network, scope, &events, trigger, true)?;
            if let Some((head, _)) = &intent {
                ensure!(
                    job.basis_heads
                        .iter()
                        .any(|basis| basis == head || ancestor(&events, basis, head)),
                    "claimed invocation did not include its intent proof"
                );
            }
            Some((
                trigger.to_owned(),
                attempt_id.to_owned(),
                intent.map(|(head, _)| head.to_owned()),
            ))
        } else {
            authorize_node_from(&tx, network, &scheduler, Some(NodeRole::Gateway))?;
            None
        };
        let task = task_id.clone();
        let creation = append_event_from(&tx, identity, network, scope, body, vec![], at)?;
        if let Some((trigger, attempt_id, intent_ref)) = verified_origin {
            let origin = TaskOriginV1 {
                network_id: network.into(),
                conversation_id: scope.into(),
                task_id: task,
                task_ref: creation.hash.clone(),
                trigger,
                attempt_id,
                target_node: scheduler,
                intent_ref,
            };
            append_event_from(
                &tx,
                identity,
                network,
                scope,
                record_body(FollowupRecord::TaskOriginV1(origin))?,
                vec![],
                at,
            )?;
        }
        tx.commit()?;
        Ok(creation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        binding::{TaskBinding, sign_binding},
        scheduling::{ManagerAttempt, SchedulerSession},
    };
    use serde_json::json;

    fn fixture() -> Result<(Store, LocalIdentity, String, String)> {
        let identity = LocalIdentity::generate();
        let network = identity.fixture_network_id()?;
        let node = identity.peer_id()?;
        let mut store = Store::open(":memory:")?;
        let roles = vec![
            NodeRole::Gateway,
            NodeRole::Replica,
            NodeRole::Scheduler,
            NodeRole::Executor,
        ];
        store.install_trust_bundle(
            &network,
            &identity.sign_network_fixture(1, vec![identity.certificate(roles)?])?,
        )?;
        store.append_event(
            &identity,
            &network,
            "chat",
            EventBody::ConversationCreated {
                title: "chat".into(),
            },
            vec![],
            1,
        )?;
        Ok((store, identity, network, node))
    }

    fn intent(
        store: &mut Store,
        identity: &LocalIdentity,
        network: &str,
        node: &str,
        id: &str,
        report: bool,
    ) -> Result<SignedEvent> {
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let message = append_event_from(
            &tx,
            identity,
            network,
            "chat",
            EventBody::MessagePosted {
                message_id: id.into(),
                role: "user".into(),
                text: id.into(),
                scheduler_node: Some(node.into()),
            },
            vec![],
            2,
        )?;
        append_report_intent_from(&tx, identity, network, "chat", &message, node, report, 2)?;
        tx.commit()?;
        Ok(message)
    }

    fn claim(
        store: &mut Store,
        network: &str,
        node: &str,
        message: &SignedEvent,
        attempt_id: &str,
    ) -> Result<()> {
        let mut session = SchedulerSession::new("chat", node);
        session.active = Some(message.hash.clone());
        let job = SchedulerJob {
            version: 1,
            conversation_id: "chat".into(),
            trigger: message.hash.clone(),
            target_node: node.into(),
            kind: TriggerKind::Human,
            sequence: 1,
            basis_heads: store.heads(network, "chat")?,
            message_id: "assistant".into(),
            state: JobState::Running {
                attempt: ManagerAttempt {
                    id: attempt_id.into(),
                    boot: "test".into(),
                    read_only: false,
                    permission_version: 1,
                    followup_version: None,
                },
            },
        };
        scheduler_save(
            &store.connection,
            network,
            &scheduler_key("job", "chat", node, &message.hash),
            &job,
        )?;
        save_scheduler_session(&store.connection, network, &session)
    }

    fn task_body(id: &str) -> EventBody {
        EventBody::TaskCreated {
            task_id: id.into(),
            title: id.into(),
            goal: "goal".into(),
            acceptance: "result".into(),
        }
    }

    fn result(
        store: &mut Store,
        identity: &LocalIdentity,
        network: &str,
        node: &str,
        task: &str,
    ) -> Result<SignedEvent> {
        let creation = store.task_creation(network, "chat", task)?;
        let binding = sign_binding(
            identity,
            &TaskBinding {
                network_id: network.into(),
                conversation_id: "chat".into(),
                task_ref: creation.hash,
                task_id: task.into(),
                authority_node: node.into(),
                generation: 1,
                target_node: node.into(),
                session_id: "execution".into(),
                open_spec: json!({"type":"open","executor":"codex","cwd":"/tmp"}),
                previous_binding_hash: None,
                previous_seal: None,
            },
        )?;
        let request = OperationRequest {
            operation_id: "send".into(),
            network_id: network.into(),
            conversation_id: "chat".into(),
            task_id: Some(task.into()),
            target_node: node.into(),
            session_id: Some("execution".into()),
            expected_revision: 1,
            basis_heads: store.heads(network, "chat")?,
            action: json!({"type":"send","text":"work"}),
            resources: vec![],
            binding: Some(binding),
            manager_authority: None,
        };
        store.append_event(
            identity,
            network,
            "chat",
            EventBody::DispatchRecorded {
                operation_id: "send".into(),
                task_id: Some(task.into()),
                target_node: node.into(),
                session_id: Some("execution".into()),
                action: serde_json::to_value(request)?,
            },
            vec![],
            4,
        )?;
        store.append_event(
            identity,
            network,
            "chat",
            EventBody::ExecutionObserved {
                operation_id: "send".into(),
                task_id: Some(task.into()),
                target_node: node.into(),
                session_id: Some("execution".into()),
                status: "succeeded".into(),
                detail: json!({"text":"done"}),
            },
            vec![],
            5,
        )
    }

    fn ordinary_request(
        store: &Store,
        network: &str,
        node: &str,
        message: &SignedEvent,
        operation: &str,
    ) -> Result<OperationRequest> {
        Ok(OperationRequest {
            operation_id: operation.into(),
            network_id: network.into(),
            conversation_id: "chat".into(),
            task_id: None,
            target_node: node.into(),
            session_id: None,
            expected_revision: 0,
            basis_heads: store.heads(network, "chat")?,
            action: json!({"type":"command","argv":["pwd"],"cwd":"/tmp","timeout_secs":1}),
            resources: vec![],
            binding: None,
            manager_authority: Some(ManagerDispatchAuthority {
                scheduler_node: node.into(),
                trigger: message.hash.clone(),
                followup_version: 0,
            }),
        })
    }

    fn ordinary_result(
        store: &mut Store,
        identity: &LocalIdentity,
        request: &OperationRequest,
        result_session: Option<String>,
    ) -> Result<SignedEvent> {
        store.append_event(
            identity,
            &request.network_id,
            &request.conversation_id,
            EventBody::DispatchRecorded {
                operation_id: request.operation_id.clone(),
                task_id: None,
                target_node: request.target_node.clone(),
                session_id: request.session_id.clone(),
                action: serde_json::to_value(request)?,
            },
            vec![],
            4,
        )?;
        store.append_event(
            identity,
            &request.network_id,
            &request.conversation_id,
            EventBody::ExecutionObserved {
                operation_id: request.operation_id.clone(),
                task_id: None,
                target_node: request.target_node.clone(),
                session_id: result_session,
                status: "succeeded".into(),
                detail: json!({"exit_code":0,"stdout":"/tmp"}),
            },
            vec![],
            5,
        )
    }

    #[test]
    fn ordinary_results_are_read_only_durable_jobs_without_task_creation() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        let (message, _) = store.accept_message(
            &identity,
            &network,
            "chat",
            EventBody::MessagePosted {
                message_id: "ordinary".into(),
                role: "user".into(),
                text: "where am I".into(),
                scheduler_node: Some(node.clone()),
            },
            vec![],
            2,
            &node,
            &node,
            true,
        )?;
        let human = store
            .claim_next(&network, "chat", &node, "test", "sdk")?
            .context("human invocation missing")?;
        let attempt = human.job.state.attempt().context("human attempt missing")?;
        let request = ordinary_request(&store, &network, &node, &message, "ordinary-command")?;
        validate_manager_dispatch_from(&store.connection, &network, &request)?;
        let result = ordinary_result(&mut store, &identity, &request, None)?;
        store.finish_job(
            &identity,
            &network,
            "chat",
            &node,
            &message.hash,
            &attempt.id,
            crate::scheduling::JobOutcome::Failed("fixture manager has returned".into()),
            vec![],
            6,
        )?;
        let policy = store.validate_followup_result(&network, "chat", &result.hash, &node)?;
        assert_eq!(policy.task_id, None);
        assert_eq!(policy.mode, FollowupMode::Report);
        let first = store.enqueue_trigger(&network, "chat", &result.hash, &node)?;
        assert_eq!(
            store.enqueue_trigger(&network, "chat", &result.hash, &node)?,
            first
        );
        let followup = store
            .claim_next(&network, "chat", &node, "test", "sdk")?
            .context("read-only result report missing")?;
        assert!(
            followup
                .job
                .state
                .attempt()
                .context("report attempt missing")?
                .read_only
        );
        assert!(followup.previous_native_id.is_none());
        assert!(
            store
                .claim_next(&network, "chat", &node, "test", "sdk")?
                .is_none()
        );
        assert!(store.tasks(&network, "chat")?.is_empty());
        assert!(
            store
                .context(&network, "chat", &[])?
                .events
                .iter()
                .all(|event| !matches!(event.body, EventBody::TaskCreated { .. }))
        );
        store.validate_latest_manager_policy(&network, "chat", &result.hash, &node, 1, None)?;
        assert!(
            store
                .validate_latest_manager_policy(
                    &network,
                    "chat",
                    &result.hash,
                    &node,
                    1,
                    Some("anything")
                )
                .is_err()
        );
        let mut forged = request;
        forged.operation_id = "report-cannot-continue".into();
        forged.basis_heads = store.heads(&network, "chat")?;
        forged.manager_authority = Some(ManagerDispatchAuthority {
            scheduler_node: node,
            trigger: result.hash,
            followup_version: 1,
        });
        assert!(validate_manager_dispatch_from(&store.connection, &network, &forged).is_err());
        Ok(())
    }

    #[test]
    fn ordinary_results_require_the_original_human_dispatch_scope_and_report_intent() -> Result<()>
    {
        let (mut store, identity, network, node) = fixture()?;
        let message = intent(&mut store, &identity, &network, &node, "ordinary", true)?;
        let request = ordinary_request(&store, &network, &node, &message, "mismatched-session")?;
        let mismatch = ordinary_result(
            &mut store,
            &identity,
            &request,
            Some("injected-session".into()),
        )?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &mismatch.hash, &node)
                .is_err()
        );
        let mut wrong_source = ordinary_request(&store, &network, &node, &message, "wrong-origin")?;
        wrong_source
            .manager_authority
            .as_mut()
            .context("authority missing")?
            .trigger = mismatch.hash;
        let mismatch = ordinary_result(&mut store, &identity, &wrong_source, None)?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &mismatch.hash, &node)
                .is_err()
        );
        let off = intent(&mut store, &identity, &network, &node, "off", false)?;
        let request = ordinary_request(&store, &network, &node, &off, "report-disabled")?;
        let disabled = ordinary_result(&mut store, &identity, &request, None)?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &disabled.hash, &node)
                .is_err()
        );
        let mut request =
            ordinary_request(&store, &network, &node, &message, "missing-origin-proof")?;
        request.basis_heads.clear();
        let missing = ordinary_result(&mut store, &identity, &request, None)?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &missing.hash, &node)
                .is_err()
        );
        let other = LocalIdentity::generate();
        store.install_trust_bundle(
            &network,
            &identity.sign_network_fixture(
                2,
                vec![
                    identity.certificate(vec![
                        NodeRole::Gateway,
                        NodeRole::Replica,
                        NodeRole::Scheduler,
                        NodeRole::Executor,
                    ])?,
                    other.certificate(vec![
                        NodeRole::Gateway,
                        NodeRole::Replica,
                        NodeRole::Executor,
                    ])?,
                ],
            )?,
        )?;
        let mut borrowed = ordinary_request(&store, &network, &node, &message, "borrowed-manager")?;
        borrowed.target_node = other.peer_id()?;
        let forged = ordinary_result(&mut store, &other, &borrowed, None)?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &forged.hash, &node)
                .is_err()
        );
        assert!(store.tasks(&network, "chat")?.is_empty());
        Ok(())
    }

    #[test]
    fn message_retries_recover_the_original_route_and_report_mode() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        intent(&mut store, &identity, &network, &node, "report", true)?;
        intent(&mut store, &identity, &network, &node, "off", false)?;
        assert_eq!(
            store.original_message_route(&network, "chat", "report")?,
            Some((node.clone(), true))
        );
        assert_eq!(
            store.original_message_route(&network, "chat", "off")?,
            Some((node, false))
        );
        assert_eq!(
            store.original_message_route(&network, "chat", "missing")?,
            None
        );
        assert_eq!(
            store.original_message_route(&network, "another-chat", "report")?,
            None
        );
        Ok(())
    }

    #[test]
    fn report_intent_is_immutable_and_cannot_grant_continue() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        let message = intent(&mut store, &identity, &network, &node, "request", true)?;
        assert!(
            append_report_intent_from(
                &store.connection,
                &identity,
                &network,
                "chat",
                &message,
                &node,
                false,
                3
            )
            .is_err()
        );
        assert!(
            store
                .create_task_with_origin(
                    &identity,
                    &network,
                    "chat",
                    task_body("task"),
                    Some((&message.hash, "not-claimed")),
                    3
                )
                .is_err()
        );
        claim(&mut store, &network, &node, &message, "attempt")?;
        store.create_task_with_origin(
            &identity,
            &network,
            "chat",
            task_body("task"),
            Some((&message.hash, "attempt")),
            3,
        )?;
        let policy = store.followup_policy(&network, "chat", "task")?;
        assert_eq!((policy.mode, policy.version), (FollowupMode::Report, 1));
        let completion = result(&mut store, &identity, &network, &node, "task")?;
        assert!(
            store
                .validate_latest_manager_policy(&network, "chat", &completion.hash, &node, 1, None)
                .is_ok()
        );
        assert!(
            store
                .validate_latest_manager_policy(
                    &network,
                    "chat",
                    &completion.hash,
                    &node,
                    1,
                    Some("task")
                )
                .is_err()
        );
        assert!(
            store
                .set_task_followup(
                    &identity,
                    &network,
                    "chat",
                    "missing",
                    FollowupMode::Continue,
                    0,
                    Some(&node),
                    6
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn task_retry_never_rebinds_origin_and_forged_origin_is_rejected() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        let first = intent(&mut store, &identity, &network, &node, "first", false)?;
        claim(&mut store, &network, &node, &first, "first-attempt")?;
        let creation = store.create_task_with_origin(
            &identity,
            &network,
            "chat",
            task_body("task"),
            Some((&first.hash, "first-attempt")),
            3,
        )?;
        let second = intent(&mut store, &identity, &network, &node, "second", true)?;
        claim(&mut store, &network, &node, &second, "second-attempt")?;
        assert_eq!(
            store
                .create_task_with_origin(
                    &identity,
                    &network,
                    "chat",
                    task_body("task"),
                    Some((&second.hash, "second-attempt")),
                    4
                )?
                .hash,
            creation.hash
        );
        assert_eq!(
            store.followup_policy(&network, "chat", "task")?.mode,
            FollowupMode::Off
        );
        let second_intent =
            report_intent_head_from(&store.connection, &network, "chat", &second.hash)?;
        store.append_event(
            &identity,
            &network,
            "chat",
            record_body(FollowupRecord::TaskOriginV1(TaskOriginV1 {
                network_id: network.clone(),
                conversation_id: "chat".into(),
                task_id: "task".into(),
                task_ref: creation.hash,
                trigger: second.hash,
                attempt_id: "second-attempt".into(),
                target_node: node,
                intent_ref: second_intent,
            }))?,
            vec![],
            5,
        )?;
        assert!(store.followup_policy(&network, "chat", "task").is_err());
        Ok(())
    }

    #[test]
    fn identical_dispatch_retries_preserve_followup_but_changed_bodies_are_rejected() -> Result<()>
    {
        let (mut store, identity, network, node) = fixture()?;
        store.create_task_with_origin(&identity, &network, "chat", task_body("task"), None, 2)?;
        store.set_task_followup(
            &identity,
            &network,
            "chat",
            "task",
            FollowupMode::Report,
            0,
            Some(&node),
            3,
        )?;
        let completion = result(&mut store, &identity, &network, &node, "task")?;
        let result_body = store.verify_event(&network, &completion)?.body;
        let mut dispatch = store.context(&network, "chat", &[])?.events.into_iter()
            .find_map(|event| matches!(&event.body, EventBody::DispatchRecorded { operation_id, .. } if operation_id == "send").then_some(event.body))
            .context("dispatch missing")?;
        store.append_event(&identity, &network, "chat", dispatch.clone(), vec![], 6)?;
        let repeated =
            store.append_event(&identity, &network, "chat", result_body.clone(), vec![], 7)?;
        assert_eq!(
            store
                .validate_followup_result(&network, "chat", &repeated.hash, &node)?
                .mode,
            FollowupMode::Report
        );
        if let EventBody::DispatchRecorded { action, .. } = &mut dispatch {
            action["action"]["text"] = json!("changed request");
        }
        store.append_event(&identity, &network, "chat", dispatch, vec![], 8)?;
        let conflicting =
            store.append_event(&identity, &network, "chat", result_body, vec![], 9)?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &conflicting.hash, &node)
                .unwrap_err()
                .to_string()
                .contains("conflicting dispatch bodies")
        );
        Ok(())
    }

    #[test]
    fn late_grant_uses_original_result_and_revocation_fences_frozen_version() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        store.create_task_with_origin(&identity, &network, "chat", task_body("task"), None, 2)?;
        let completion = result(&mut store, &identity, &network, &node, "task")?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &completion.hash, &node)
                .is_err()
        );
        store.set_task_followup(
            &identity,
            &network,
            "chat",
            "task",
            FollowupMode::Continue,
            0,
            Some(&node),
            6,
        )?;
        assert_eq!(
            store
                .validate_followup_result(&network, "chat", &completion.hash, &node)?
                .version,
            1
        );
        assert!(
            store
                .validate_latest_manager_policy(
                    &network,
                    "chat",
                    &completion.hash,
                    &node,
                    1,
                    Some("task")
                )
                .is_ok()
        );
        assert!(
            store
                .validate_latest_manager_policy(
                    &network,
                    "chat",
                    &completion.hash,
                    &node,
                    1,
                    Some("another-task")
                )
                .is_err()
        );
        let mut accepted: OperationRequest = store
            .context(&network, "chat", &[])?
            .events
            .into_iter()
            .find_map(|event| match event.body {
                EventBody::DispatchRecorded { action, .. } => serde_json::from_value(action).ok(),
                _ => None,
            })
            .context("original dispatch missing")?;
        store.install_binding(accepted.binding.as_ref().unwrap())?;
        accepted.operation_id = "accepted-before-revocation".into();
        accepted.expected_revision = 0;
        accepted.basis_heads = store.heads(&network, "chat")?;
        accepted.manager_authority = Some(ManagerDispatchAuthority {
            scheduler_node: node.clone(),
            trigger: completion.hash.clone(),
            followup_version: 1,
        });
        assert_eq!(
            store.accept_operation(&network, &accepted)?.state,
            OperationState::Accepted
        );
        store.set_task_followup(
            &identity,
            &network,
            "chat",
            "task",
            FollowupMode::Off,
            1,
            None,
            7,
        )?;
        assert!(
            store
                .validate_latest_manager_policy(
                    &network,
                    "chat",
                    &completion.hash,
                    &node,
                    1,
                    Some("task")
                )
                .is_err()
        );
        assert!(
            store
                .set_task_followup(
                    &identity,
                    &network,
                    "chat",
                    "task",
                    FollowupMode::Continue,
                    1,
                    None,
                    8
                )
                .is_err()
        );
        assert_eq!(
            store.accept_operation(&network, &accepted)?.state,
            OperationState::Accepted
        );
        let mut delayed = accepted.clone();
        delayed.operation_id = "new-arrival-after-revocation".into();
        delayed.expected_revision = 1;
        let rejected = store.accept_operation(&network, &delayed).unwrap_err();
        assert!(rejected.to_string().contains("follow-up"), "{rejected:#}");
        assert!(store.operation(&network, &delayed.operation_id)?.is_none());
        assert_eq!(store.followup_policy(&network, "chat", "task")?.version, 2);
        assert!(store.event(&network, &completion.hash)?.is_some());
        Ok(())
    }

    #[test]
    fn forged_signature_network_and_dispatch_are_rejected() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        store.create_task_with_origin(&identity, &network, "chat", task_body("task"), None, 2)?;
        store.set_task_followup(
            &identity,
            &network,
            "chat",
            "task",
            FollowupMode::Report,
            0,
            Some(&node),
            3,
        )?;
        let bad = store.append_event(
            &identity,
            &network,
            "chat",
            EventBody::ExecutionObserved {
                operation_id: "forged".into(),
                task_id: Some("task".into()),
                target_node: node.clone(),
                session_id: Some("wrong".into()),
                status: "succeeded".into(),
                detail: json!({}),
            },
            vec![],
            4,
        )?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &bad.hash, &node)
                .is_err()
        );
        assert!(
            store
                .followup_policy(
                    &LocalIdentity::generate().fixture_network_id()?,
                    "chat",
                    "task"
                )
                .is_err()
        );
        let mut corrupt = bad.clone();
        corrupt.signature = "00".repeat(64);
        assert!(store.verify_event(&network, &corrupt).is_err());
        store.connection.execute(
            "UPDATE events SET envelope=? WHERE network_id=? AND hash=?",
            params![serde_json::to_string(&corrupt)?, network, bad.hash],
        )?;
        assert!(store.followup_policy(&network, "chat", "task").is_err());
        Ok(())
    }

    #[test]
    fn rejected_grant_does_not_commit_an_unusable_policy_version() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        store.create_task_with_origin(&identity, &network, "chat", task_body("task"), None, 2)?;
        store.set_task_followup(
            &identity,
            &network,
            "chat",
            "task",
            FollowupMode::Off,
            0,
            Some(&node),
            3,
        )?;
        let next_gateway = LocalIdentity::generate();
        store.install_trust_bundle(
            &network,
            &identity.sign_network_fixture(
                2,
                vec![
                    identity.certificate(vec![
                        NodeRole::Scheduler,
                        NodeRole::Replica,
                        NodeRole::Executor,
                    ])?,
                    next_gateway.certificate(vec![NodeRole::Gateway, NodeRole::Replica])?,
                ],
            )?,
        )?;
        let before = store.heads(&network, "chat")?;
        assert!(
            store
                .set_task_followup(
                    &next_gateway,
                    &network,
                    "chat",
                    "task",
                    FollowupMode::Continue,
                    1,
                    None,
                    4
                )
                .is_err()
        );
        let policy = store.followup_policy(&network, "chat", "task")?;
        assert_eq!((policy.mode, policy.version), (FollowupMode::Off, 1));
        assert_eq!(store.heads(&network, "chat")?, before);
        Ok(())
    }

    #[test]
    fn removed_scheduler_role_does_not_prevent_persistent_revocation() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        store.create_task_with_origin(&identity, &network, "chat", task_body("task"), None, 2)?;
        store.set_task_followup(
            &identity,
            &network,
            "chat",
            "task",
            FollowupMode::Report,
            0,
            Some(&node),
            3,
        )?;
        store.install_trust_bundle(
            &network,
            &identity.sign_network_fixture(
                2,
                vec![identity.certificate(vec![
                    NodeRole::Gateway,
                    NodeRole::Replica,
                    NodeRole::Executor,
                ])?],
            )?,
        )?;
        assert!(store.followup_policy(&network, "chat", "task").is_err());
        store.set_task_followup(
            &identity,
            &network,
            "chat",
            "task",
            FollowupMode::Off,
            1,
            None,
            4,
        )?;
        let policy = store.followup_policy(&network, "chat", "task")?;
        assert_eq!((policy.mode, policy.version), (FollowupMode::Off, 2));
        Ok(())
    }
}
