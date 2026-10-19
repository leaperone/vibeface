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

pub(super) fn validate_followup_result_from(
    connection: &Connection,
    network: &str,
    scope: &str,
    trigger: &str,
    scheduler: &str,
) -> Result<ResultFollowupPolicy> {
    let events = scope_events(connection, network, scope)?;
    let result = events.get(trigger).context("agent result is missing")?;
    let EventBody::AgentMessage { message } = &result.event.body else {
        bail!("follow-up requires an agent result");
    };
    ensure!(
        agent_messages::validate_agent_trigger_from(
            connection,
            network,
            &result.event,
            message,
            scheduler
        )? == TriggerKind::AgentResult,
        "follow-up requires a terminal agent result"
    );
    let mut source = message.origin_trigger.as_str();
    let mut visited = BTreeSet::new();
    loop {
        ensure!(visited.insert(source), "cyclic agent origin");
        let origin = events
            .get(source)
            .context("agent result origin is missing")?;
        match &origin.event.body {
            EventBody::MessagePosted { role, .. } if role == "user" => break,
            EventBody::AgentMessage { message } => source = &message.origin_trigger,
            _ => bail!("agent result origin is not a user request or agent task"),
        }
    }
    let intent = intent_from(connection, network, scope, &events, source, true)?;
    ensure!(
        intent
            .as_ref()
            .is_none_or(|(_, intent)| intent.mode != FollowupMode::Off),
        "result reporting is disabled"
    );
    Ok(ResultFollowupPolicy {
        task_id: None,
        target_node: scheduler.into(),
        mode: FollowupMode::Report,
        version: intent.as_ref().map_or(1, |(_, intent)| intent.version),
        proof_heads: std::iter::once(trigger.to_owned())
            .chain(intent.map(|(hash, _)| hash.to_owned()))
            .collect(),
    })
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

    pub fn agent_result_policy(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
    ) -> Result<ResultFollowupPolicy> {
        let events = scope_events(&self.connection, network, scope)?;
        let result = events.get(trigger).context("agent result is missing")?;
        let EventBody::AgentMessage { message } = &result.event.body else {
            bail!("follow-up requires an agent result");
        };
        validate_followup_result_from(
            &self.connection,
            network,
            scope,
            trigger,
            &message.target_node,
        )
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
    use crate::scheduling::JobOutcome;

    fn roles() -> Vec<NodeRole> {
        vec![NodeRole::Gateway, NodeRole::Replica, NodeRole::Scheduler]
    }

    fn fixture() -> Result<(Store, LocalIdentity, String, String)> {
        let identity = LocalIdentity::generate();
        let network = identity.fixture_network_id()?;
        let node = identity.peer_id()?;
        let mut store = Store::open(":memory:")?;
        store.install_trust_bundle(
            &network,
            &identity.sign_network_fixture(1, vec![identity.certificate(roles())?])?,
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
    ) -> Result<String> {
        store.enqueue_trigger(network, "chat", &message.hash, node)?;
        let claimed = store
            .claim_next(network, "chat", node, "test", "sdk")?
            .context("human invocation missing")?;
        ensure!(
            claimed.job.trigger == message.hash,
            "another invocation was claimed"
        );
        let attempt = claimed
            .job
            .state
            .attempt()
            .context("attempt missing")?
            .id
            .clone();
        store.mark_started(network, "chat", node, &message.hash, &attempt)?;
        Ok(attempt)
    }

    fn task_body(id: &str) -> EventBody {
        EventBody::TaskCreated {
            task_id: id.into(),
            title: id.into(),
            goal: "goal".into(),
            acceptance: "result".into(),
        }
    }

    fn agent_task(
        store: &mut Store,
        identity: &LocalIdentity,
        network: &str,
        target: &str,
        origin: &SignedEvent,
    ) -> Result<SignedEvent> {
        store.append_event(
            identity,
            network,
            "chat",
            EventBody::AgentMessage {
                message: AgentMessage {
                    message_id: format!("agent-task-{}", origin.hash),
                    target_node: target.into(),
                    origin_trigger: origin.hash.clone(),
                    in_reply_to: None,
                    kind: AgentMessageKind::Task,
                    text: "work".into(),
                },
            },
            vec![],
            4,
        )
    }

    fn result(
        store: &mut Store,
        identity: &LocalIdentity,
        network: &str,
        target: &str,
        task: &SignedEvent,
        kind: AgentMessageKind,
    ) -> Result<SignedEvent> {
        let EventBody::AgentMessage { message } = store.verify_event(network, task)?.body else {
            bail!("agent task fixture is missing");
        };
        store.append_event(
            identity,
            network,
            "chat",
            EventBody::AgentMessage {
                message: AgentMessage {
                    message_id: format!("agent-result-{kind:?}-{}", task.hash),
                    target_node: target.into(),
                    origin_trigger: message.origin_trigger,
                    in_reply_to: Some(task.hash.clone()),
                    kind,
                    text: "done".into(),
                },
            },
            vec![],
            5,
        )
    }

    #[test]
    fn terminal_results_are_read_only_durable_jobs_without_business_task_creation() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        let message = intent(&mut store, &identity, &network, &node, "ordinary", true)?;
        let attempt = claim(&mut store, &network, &node, &message)?;
        let task = agent_task(&mut store, &identity, &network, &node, &message)?;
        let completion = result(
            &mut store,
            &identity,
            &network,
            &node,
            &task,
            AgentMessageKind::Completed,
        )?;
        store.finish_job(
            &identity,
            &network,
            "chat",
            &node,
            &message.hash,
            &attempt,
            JobOutcome::Failed("fixture source has returned".into()),
            vec![],
            6,
        )?;
        let policy = store.agent_result_policy(&network, "chat", &completion.hash)?;
        assert_eq!(
            (policy.task_id, policy.mode, policy.version),
            (None, FollowupMode::Report, 1)
        );
        assert_eq!(policy.target_node, node);
        let first = store.enqueue_trigger(&network, "chat", &completion.hash, &node)?;
        assert_eq!(
            store.enqueue_trigger(&network, "chat", &completion.hash, &node)?,
            first
        );
        let followup = store
            .claim_next(&network, "chat", &node, "test", "sdk")?
            .context("result report missing")?;
        assert_eq!(followup.job.trigger, completion.hash);
        assert_eq!(followup.job.kind, TriggerKind::AgentResult);
        let report = followup
            .job
            .state
            .attempt()
            .context("report attempt missing")?;
        assert!(report.read_only);
        assert_eq!(report.followup_version, Some(1));
        assert!(followup.previous_native_id.is_none());
        assert!(
            store
                .claim_next(&network, "chat", &node, "test", "sdk")?
                .is_none()
        );
        assert!(store.tasks(&network, "chat")?.is_empty());
        store.validate_latest_manager_policy(&network, "chat", &completion.hash, &node, 1, None)?;
        assert!(
            store
                .validate_latest_manager_policy(
                    &network,
                    "chat",
                    &completion.hash,
                    &node,
                    1,
                    Some("anything")
                )
                .is_err()
        );
        let report_attempt = report.id.clone();
        assert!(
            store
                .create_task_with_origin(
                    &identity,
                    &network,
                    "chat",
                    task_body("forbidden"),
                    Some((&completion.hash, &report_attempt)),
                    7
                )
                .is_err()
        );
        assert!(store.tasks(&network, "chat")?.is_empty());
        Ok(())
    }

    #[test]
    fn terminal_results_require_the_original_agent_source_target_and_request() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        let peer = LocalIdentity::generate();
        let peer_node = peer.peer_id()?;
        store.install_trust_bundle(
            &network,
            &identity.sign_network_fixture(
                2,
                vec![identity.certificate(roles())?, peer.certificate(roles())?],
            )?,
        )?;
        let message = intent(&mut store, &identity, &network, &node, "request", true)?;
        let another = intent(&mut store, &identity, &network, &node, "another", true)?;
        let task = agent_task(&mut store, &identity, &network, &peer_node, &message)?;
        for kind in [
            AgentMessageKind::Completed,
            AgentMessageKind::Failed,
            AgentMessageKind::Interrupted,
        ] {
            let completion = result(&mut store, &peer, &network, &node, &task, kind)?;
            let policy = store.agent_result_policy(&network, "chat", &completion.hash)?;
            assert_eq!(
                (policy.mode, policy.version, policy.task_id),
                (FollowupMode::Report, 1, None)
            );
            assert_eq!(policy.target_node, node);
            assert_eq!(policy.proof_heads[0], completion.hash);
        }
        for (index, author, target, origin, reply, kind) in [
            (
                0,
                &identity,
                &node,
                &message.hash,
                Some(task.hash.clone()),
                AgentMessageKind::Completed,
            ),
            (
                1,
                &peer,
                &peer_node,
                &message.hash,
                Some(task.hash.clone()),
                AgentMessageKind::Completed,
            ),
            (
                2,
                &peer,
                &node,
                &another.hash,
                Some(task.hash.clone()),
                AgentMessageKind::Completed,
            ),
            (
                3,
                &peer,
                &node,
                &message.hash,
                Some(message.hash.clone()),
                AgentMessageKind::Completed,
            ),
            (
                4,
                &peer,
                &node,
                &message.hash,
                None,
                AgentMessageKind::Completed,
            ),
            (
                5,
                &peer,
                &node,
                &message.hash,
                Some(task.hash.clone()),
                AgentMessageKind::Progress,
            ),
        ] {
            let missing_reply = reply.is_none();
            let before = store.heads(&network, "chat")?;
            let forged = store.append_event(
                author,
                &network,
                "chat",
                EventBody::AgentMessage {
                    message: AgentMessage {
                        message_id: format!("forged-{index}"),
                        target_node: target.clone(),
                        origin_trigger: origin.clone(),
                        in_reply_to: reply,
                        kind,
                        text: "forged result".into(),
                    },
                },
                vec![],
                6,
            );
            if missing_reply {
                assert_eq!(
                    forged.unwrap_err().to_string(),
                    "agent reply has no task reference"
                );
                assert_eq!(store.heads(&network, "chat")?, before);
                continue;
            }
            let forged = forged?;
            assert!(
                store
                    .validate_followup_result(&network, "chat", &forged.hash, &node)
                    .is_err(),
                "forged terminal case {index} was accepted"
            );
        }
        assert!(
            store
                .validate_followup_result(&network, "other-chat", &task.hash, &node)
                .is_err()
        );
        assert!(store.tasks(&network, "chat")?.is_empty());
        Ok(())
    }

    #[test]
    fn result_reporting_defaults_to_report_and_explicit_off_stays_off() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        let default = store.append_event(
            &identity,
            &network,
            "chat",
            EventBody::MessagePosted {
                message_id: "default".into(),
                role: "user".into(),
                text: "work".into(),
                scheduler_node: Some(node.clone()),
            },
            vec![],
            2,
        )?;
        let task = agent_task(&mut store, &identity, &network, &node, &default)?;
        let completion = result(
            &mut store,
            &identity,
            &network,
            &node,
            &task,
            AgentMessageKind::Completed,
        )?;
        let policy = store.agent_result_policy(&network, "chat", &completion.hash)?;
        assert_eq!((policy.mode, policy.version), (FollowupMode::Report, 1));
        let off = intent(&mut store, &identity, &network, &node, "off", false)?;
        let task = agent_task(&mut store, &identity, &network, &node, &off)?;
        for kind in [
            AgentMessageKind::Completed,
            AgentMessageKind::Failed,
            AgentMessageKind::Interrupted,
        ] {
            let disabled = result(&mut store, &identity, &network, &node, &task, kind)?;
            assert!(
                store
                    .agent_result_policy(&network, "chat", &disabled.hash)
                    .unwrap_err()
                    .to_string()
                    .contains("result reporting is disabled")
            );
        }
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
        let before = store.heads(&network, "chat")?;
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
        assert_eq!(store.heads(&network, "chat")?, before);
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
        let attempt = claim(&mut store, &network, &node, &message)?;
        store.create_task_with_origin(
            &identity,
            &network,
            "chat",
            task_body("task"),
            Some((&message.hash, &attempt)),
            3,
        )?;
        let policy = store.followup_policy(&network, "chat", "task")?;
        assert_eq!((policy.mode, policy.version), (FollowupMode::Report, 1));
        let task = agent_task(&mut store, &identity, &network, &node, &message)?;
        let completion = result(
            &mut store,
            &identity,
            &network,
            &node,
            &task,
            AgentMessageKind::Completed,
        )?;
        store.validate_latest_manager_policy(&network, "chat", &completion.hash, &node, 1, None)?;
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
        let first_attempt = claim(&mut store, &network, &node, &first)?;
        let creation = store.create_task_with_origin(
            &identity,
            &network,
            "chat",
            task_body("task"),
            Some((&first.hash, &first_attempt)),
            3,
        )?;
        store.finish_job(
            &identity,
            &network,
            "chat",
            &node,
            &first.hash,
            &first_attempt,
            JobOutcome::Failed("fixture source has returned".into()),
            vec![],
            3,
        )?;
        let second = intent(&mut store, &identity, &network, &node, "second", true)?;
        let second_attempt = claim(&mut store, &network, &node, &second)?;
        assert_eq!(
            store
                .create_task_with_origin(
                    &identity,
                    &network,
                    "chat",
                    task_body("task"),
                    Some((&second.hash, &second_attempt)),
                    4
                )?
                .hash,
            creation.hash
        );
        assert!(
            store
                .create_task_with_origin(
                    &identity,
                    &network,
                    "chat",
                    EventBody::TaskCreated {
                        task_id: "task".into(),
                        title: "changed".into(),
                        goal: "goal".into(),
                        acceptance: "result".into(),
                    },
                    Some((&second.hash, &second_attempt)),
                    4
                )
                .is_err()
        );
        let policy = store.followup_policy(&network, "chat", "task")?;
        assert_eq!((policy.mode, policy.version), (FollowupMode::Off, 1));
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
                attempt_id: second_attempt,
                target_node: node,
                intent_ref: second_intent,
            }))?,
            vec![],
            5,
        )?;
        assert!(
            store
                .followup_policy(&network, "chat", "task")
                .unwrap_err()
                .to_string()
                .contains("task origin is not its scheduler's user invocation")
        );
        Ok(())
    }

    #[test]
    fn task_grant_versions_preserve_target_and_never_authorize_result_writes() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        store.create_task_with_origin(&identity, &network, "chat", task_body("task"), None, 2)?;
        let initial = store.followup_policy(&network, "chat", "task")?;
        assert_eq!((initial.mode, initial.version), (FollowupMode::Off, 0));
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
        let revoked = store.followup_policy(&network, "chat", "task")?;
        assert_eq!((revoked.mode, revoked.version), (FollowupMode::Off, 2));
        let before = store.heads(&network, "chat")?;
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
                    5
                )
                .is_err()
        );
        assert_eq!(store.heads(&network, "chat")?, before);
        store.set_task_followup(
            &identity,
            &network,
            "chat",
            "task",
            FollowupMode::Continue,
            2,
            None,
            5,
        )?;
        let granted = store.followup_policy(&network, "chat", "task")?;
        assert_eq!((granted.mode, granted.version), (FollowupMode::Continue, 3));
        assert_eq!(granted.target_node, node);
        assert!(
            store
                .set_task_followup(
                    &identity,
                    &network,
                    "chat",
                    "task",
                    FollowupMode::Report,
                    3,
                    Some(&LocalIdentity::generate().peer_id()?),
                    6
                )
                .is_err()
        );
        let message = intent(&mut store, &identity, &network, &node, "request", true)?;
        let task = agent_task(&mut store, &identity, &network, &node, &message)?;
        let completion = result(
            &mut store,
            &identity,
            &network,
            &node,
            &task,
            AgentMessageKind::Completed,
        )?;
        let policy = store.agent_result_policy(&network, "chat", &completion.hash)?;
        assert_eq!(
            (policy.mode, policy.version, policy.task_id),
            (FollowupMode::Report, 1, None)
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
                .validate_latest_manager_policy(&network, "chat", &completion.hash, &node, 3, None)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn forged_signature_network_and_task_reference_are_rejected() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        let message = intent(&mut store, &identity, &network, &node, "request", true)?;
        store.create_task_with_origin(&identity, &network, "chat", task_body("task"), None, 2)?;
        let task = agent_task(&mut store, &identity, &network, &node, &message)?;
        let completion = result(
            &mut store,
            &identity,
            &network,
            &node,
            &task,
            AgentMessageKind::Completed,
        )?;
        assert_eq!(
            store
                .agent_result_policy(&network, "chat", &completion.hash)?
                .mode,
            FollowupMode::Report
        );
        let wrong_network = LocalIdentity::generate().fixture_network_id()?;
        assert!(
            store
                .followup_policy(&wrong_network, "chat", "task")
                .is_err()
        );
        assert!(
            store
                .agent_result_policy(&wrong_network, "chat", &completion.hash)
                .is_err()
        );
        let mut corrupt = completion;
        corrupt.signature = "00".repeat(64);
        assert!(store.verify_event(&network, &corrupt).is_err());
        store.connection.execute(
            "UPDATE events SET envelope=? WHERE network_id=? AND hash=?",
            params![serde_json::to_string(&corrupt)?, network, corrupt.hash],
        )?;
        assert!(
            store
                .agent_result_policy(&network, "chat", &corrupt.hash)
                .is_err()
        );
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
                    identity.certificate(vec![NodeRole::Scheduler, NodeRole::Replica])?,
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
                vec![identity.certificate(vec![NodeRole::Gateway, NodeRole::Replica])?],
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
