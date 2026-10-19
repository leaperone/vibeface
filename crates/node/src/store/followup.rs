use super::*;
use crate::scheduling::TriggerKind;
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
pub struct ConversationAutonomyV1 {
    pub network_id: String,
    pub conversation_id: String,
    pub target_node: String,
    pub mode: FollowupMode,
    pub version: u64,
    pub previous_head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum FollowupRecord {
    FollowupIntentV1(FollowupIntentV1),
    ConversationAutonomyV1(ConversationAutonomyV1),
}

/// Version 0 is the unrecorded default: results are reported read-only as each message's intent allows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutonomyPolicy {
    pub network_id: String,
    pub conversation_id: String,
    pub target_node: String,
    pub mode: FollowupMode,
    pub version: u64,
    pub proof_heads: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResultFollowupPolicy {
    pub target_node: String,
    pub mode: FollowupMode,
    pub version: u64,
    pub proof_heads: Vec<String>,
}

impl ResultFollowupPolicy {
    pub fn authorized(self) -> Result<Self> {
        ensure!(
            self.mode != FollowupMode::Off,
            "follow-up is turned off for this conversation or message"
        );
        Ok(self)
    }
}

fn record_body(record: FollowupRecord) -> Result<EventBody> {
    let (id, text) = match &record {
        FollowupRecord::FollowupIntentV1(v) => (
            format!("followup-intent:v1:{}", v.message_hash),
            "Message follow-up intent",
        ),
        FollowupRecord::ConversationAutonomyV1(v) => (
            format!("conversation-autonomy:v1:{}", v.version),
            "Conversation autonomy",
        ),
    };
    Ok(EventBody::DecisionRecorded {
        decision_id: id,
        text: text.into(),
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
        Some("followup_intent_v1" | "conversation_autonomy_v1")
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

fn autonomy_in(
    connection: &Connection,
    network: &str,
    scope: &str,
    events: &BTreeMap<String, StoredEvent>,
    check_roles: bool,
) -> Result<AutonomyPolicy> {
    let mut records = BTreeMap::new();
    for (hash, stored) in events
        .iter()
        .filter(|(_, e)| e.status == "ready" && e.event.scope_id == scope)
    {
        let Some(FollowupRecord::ConversationAutonomyV1(record)) = record(&stored.event)? else {
            continue;
        };
        ensure!(
            record.network_id == network && record.conversation_id == scope,
            "conversation autonomy has the wrong scope"
        );
        ensure!(
            records
                .insert(record.version, (hash, stored, record))
                .is_none(),
            "conflicting conversation autonomy versions"
        );
    }
    let mut policy = AutonomyPolicy {
        network_id: network.into(),
        conversation_id: scope.into(),
        target_node: String::new(),
        mode: FollowupMode::Report,
        version: 0,
        proof_heads: vec![],
    };
    let mut author = None;
    for (version, (head, stored, record)) in records {
        ensure!(
            version
                == policy
                    .version
                    .checked_add(1)
                    .context("conversation autonomy version exhausted")?
                && record.previous_head.as_ref() == policy.proof_heads.first()
                && record
                    .previous_head
                    .as_ref()
                    .is_none_or(|previous| ancestor(events, head, previous)),
            "conversation autonomy version chain is invalid"
        );
        policy.mode = record.mode;
        policy.version = version;
        policy.target_node = record.target_node;
        policy.proof_heads = vec![head.clone()];
        author = Some(stored.event.author.clone());
    }
    if check_roles && policy.mode != FollowupMode::Off {
        if let Some(author) = author {
            authorize_node_from(connection, network, &author, Some(NodeRole::Gateway))?;
        }
        if !policy.target_node.is_empty() {
            authorize_node_from(
                connection,
                network,
                &policy.target_node,
                Some(NodeRole::Scheduler),
            )?;
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
    let autonomy = autonomy_in(connection, network, scope, &events, true)?;
    let mode = if intent
        .as_ref()
        .is_some_and(|(_, intent)| intent.mode == FollowupMode::Off)
    {
        FollowupMode::Off
    } else if autonomy.mode == FollowupMode::Continue && autonomy.target_node != scheduler {
        FollowupMode::Report
    } else {
        autonomy.mode
    };
    Ok(ResultFollowupPolicy {
        target_node: scheduler.into(),
        mode,
        version: autonomy.version,
        proof_heads: std::iter::once(trigger.to_owned())
            .chain(intent.map(|(hash, _)| hash.to_owned()))
            .chain(autonomy.proof_heads)
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

    pub fn conversation_autonomy(&self, network: &str, scope: &str) -> Result<AutonomyPolicy> {
        let events = scope_events(&self.connection, network, scope)?;
        autonomy_in(&self.connection, network, scope, &events, false)
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

    /// Rechecked before every tool call of a follow-up turn, so a revoked or changed grant stops further writes.
    pub fn validate_latest_manager_policy(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
        target: &str,
        expected_version: u64,
        write: bool,
    ) -> Result<()> {
        let current = self
            .validate_followup_result(network, scope, trigger, target)?
            .authorized()?;
        ensure!(
            current.version == expected_version,
            "conversation autonomy was revoked or changed"
        );
        ensure!(
            !write || current.mode == FollowupMode::Continue,
            "follow-up writes require continue autonomy for this conversation"
        );
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the transaction binds its full authorization and durable record identity"
    )]
    pub fn set_conversation_autonomy(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        scope: &str,
        mode: FollowupMode,
        expected_version: u64,
        target: Option<&str>,
        at: i64,
    ) -> Result<SignedEvent> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize_node_from(&tx, network, &identity.peer_id()?, Some(NodeRole::Gateway))?;
        let events = scope_events(&tx, network, scope)?;
        let current = autonomy_in(&tx, network, scope, &events, false)?;
        ensure!(
            current.version == expected_version,
            "conversation autonomy changed; refresh before changing it"
        );
        let selected = target
            .filter(|value| !value.is_empty())
            .unwrap_or(&current.target_node);
        if mode != FollowupMode::Off {
            ensure!(
                !selected.is_empty(),
                "conversation autonomy needs a scheduler"
            );
            authorize_node_from(&tx, network, selected, Some(NodeRole::Scheduler))?;
        }
        let record = ConversationAutonomyV1 {
            network_id: network.into(),
            conversation_id: scope.into(),
            target_node: selected.into(),
            mode,
            version: current
                .version
                .checked_add(1)
                .context("conversation autonomy version exhausted")?,
            previous_head: current.proof_heads.into_iter().next(),
        };
        let envelope = append_event_from(
            &tx,
            identity,
            network,
            scope,
            record_body(FollowupRecord::ConversationAutonomyV1(record))?,
            vec![],
            at,
        )?;
        let events = scope_events(&tx, network, scope)?;
        autonomy_in(&tx, network, scope, &events, mode != FollowupMode::Off)?;
        tx.commit()?;
        Ok(envelope)
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

    fn set(
        store: &mut Store,
        identity: &LocalIdentity,
        network: &str,
        mode: FollowupMode,
        expected: u64,
        target: Option<&str>,
    ) -> Result<SignedEvent> {
        store.set_conversation_autonomy(identity, network, "chat", mode, expected, target, 3)
    }

    fn completed(
        store: &mut Store,
        identity: &LocalIdentity,
        network: &str,
        node: &str,
        id: &str,
    ) -> Result<SignedEvent> {
        let message = intent(store, identity, network, node, id, true)?;
        let task = agent_task(store, identity, network, node, &message)?;
        result(
            store,
            identity,
            network,
            node,
            &task,
            AgentMessageKind::Completed,
        )
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
    fn terminal_results_are_read_only_durable_jobs_by_default() -> Result<()> {
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
        assert_eq!((policy.mode, policy.version), (FollowupMode::Report, 0));
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
        assert_eq!(report.followup_version, Some(0));
        assert!(followup.previous_native_id.is_none());
        assert!(
            store
                .claim_next(&network, "chat", &node, "test", "sdk")?
                .is_none()
        );
        store.validate_latest_manager_policy(
            &network,
            "chat",
            &completion.hash,
            &node,
            0,
            false,
        )?;
        assert!(
            store
                .validate_latest_manager_policy(&network, "chat", &completion.hash, &node, 0, true)
                .is_err()
        );
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
            assert_eq!((policy.mode, policy.version), (FollowupMode::Report, 0));
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
        assert_eq!((policy.mode, policy.version), (FollowupMode::Report, 0));
        let off = intent(&mut store, &identity, &network, &node, "off", false)?;
        let task = agent_task(&mut store, &identity, &network, &node, &off)?;
        for kind in [
            AgentMessageKind::Completed,
            AgentMessageKind::Failed,
            AgentMessageKind::Interrupted,
        ] {
            let disabled = result(&mut store, &identity, &network, &node, &task, kind)?;
            let policy = store.agent_result_policy(&network, "chat", &disabled.hash)?;
            assert_eq!(policy.mode, FollowupMode::Off);
            assert!(policy.authorized().is_err());
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
        let task = agent_task(&mut store, &identity, &network, &node, &message)?;
        let completion = result(
            &mut store,
            &identity,
            &network,
            &node,
            &task,
            AgentMessageKind::Completed,
        )?;
        store.validate_latest_manager_policy(
            &network,
            "chat",
            &completion.hash,
            &node,
            0,
            false,
        )?;
        assert!(
            store
                .validate_latest_manager_policy(&network, "chat", &completion.hash, &node, 0, true)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn conversation_autonomy_versions_are_monotonic_and_revocation_stops_writes() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        let initial = store.conversation_autonomy(&network, "chat")?;
        assert_eq!((initial.mode, initial.version), (FollowupMode::Report, 0));
        set(
            &mut store,
            &identity,
            &network,
            FollowupMode::Continue,
            0,
            Some(&node),
        )?;
        let completion = completed(&mut store, &identity, &network, &node, "request")?;
        let policy = store.agent_result_policy(&network, "chat", &completion.hash)?;
        assert_eq!((policy.mode, policy.version), (FollowupMode::Continue, 1));
        store.enqueue_trigger(&network, "chat", &completion.hash, &node)?;
        let claimed = store
            .claim_next(&network, "chat", &node, "test", "sdk")?
            .context("continuation missing")?;
        let attempt = claimed.job.state.attempt().context("attempt missing")?;
        assert!(!attempt.read_only);
        assert_eq!(attempt.followup_version, Some(1));
        store.validate_latest_manager_policy(&network, "chat", &completion.hash, &node, 1, true)?;

        let before = store.heads(&network, "chat")?;
        assert!(
            set(
                &mut store,
                &identity,
                &network,
                FollowupMode::Continue,
                0,
                None
            )
            .unwrap_err()
            .to_string()
            .contains("refresh")
        );
        assert_eq!(store.heads(&network, "chat")?, before);

        set(&mut store, &identity, &network, FollowupMode::Off, 1, None)?;
        let revoked = store.conversation_autonomy(&network, "chat")?;
        assert_eq!((revoked.mode, revoked.version), (FollowupMode::Off, 2));
        for (version, write) in [(1, true), (1, false), (2, false)] {
            assert!(
                store
                    .validate_latest_manager_policy(
                        &network,
                        "chat",
                        &completion.hash,
                        &node,
                        version,
                        write
                    )
                    .is_err()
            );
        }
        assert!(
            store
                .agent_result_policy(&network, "chat", &completion.hash)?
                .authorized()
                .is_err()
        );

        set(
            &mut store,
            &identity,
            &network,
            FollowupMode::Report,
            2,
            None,
        )?;
        store.validate_latest_manager_policy(
            &network,
            "chat",
            &completion.hash,
            &node,
            3,
            false,
        )?;
        assert!(
            store
                .validate_latest_manager_policy(&network, "chat", &completion.hash, &node, 3, true)
                .is_err()
        );

        let other = LocalIdentity::generate();
        store.install_trust_bundle(
            &network,
            &identity.sign_network_fixture(
                2,
                vec![identity.certificate(roles())?, other.certificate(roles())?],
            )?,
        )?;
        set(
            &mut store,
            &identity,
            &network,
            FollowupMode::Continue,
            3,
            Some(&other.peer_id()?),
        )?;
        let policy = store.agent_result_policy(&network, "chat", &completion.hash)?;
        assert_eq!((policy.mode, policy.version), (FollowupMode::Report, 4));
        Ok(())
    }

    #[test]
    fn forged_autonomy_records_break_the_chain_and_fail_closed() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        set(
            &mut store,
            &identity,
            &network,
            FollowupMode::Report,
            0,
            Some(&node),
        )?;
        let completion = completed(&mut store, &identity, &network, &node, "request")?;
        store.append_event(
            &identity,
            &network,
            "chat",
            record_body(FollowupRecord::ConversationAutonomyV1(
                ConversationAutonomyV1 {
                    network_id: network.clone(),
                    conversation_id: "chat".into(),
                    target_node: node.clone(),
                    mode: FollowupMode::Continue,
                    version: 3,
                    previous_head: None,
                },
            ))?,
            vec![],
            5,
        )?;
        assert!(
            store
                .conversation_autonomy(&network, "chat")
                .unwrap_err()
                .to_string()
                .contains("version chain is invalid")
        );
        assert!(
            store
                .agent_result_policy(&network, "chat", &completion.hash)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn forged_signature_and_network_are_rejected() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        let message = intent(&mut store, &identity, &network, &node, "request", true)?;
        set(
            &mut store,
            &identity,
            &network,
            FollowupMode::Report,
            0,
            Some(&node),
        )?;
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
                .conversation_autonomy(&wrong_network, "chat")?
                .proof_heads
                .is_empty()
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
        assert!(store.conversation_autonomy(&network, "chat").is_err());
        Ok(())
    }

    #[test]
    fn rejected_grant_does_not_commit_an_unusable_policy_version() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        set(
            &mut store,
            &identity,
            &network,
            FollowupMode::Off,
            0,
            Some(&node),
        )?;
        let gateway_only = LocalIdentity::generate();
        store.install_trust_bundle(
            &network,
            &identity.sign_network_fixture(
                2,
                vec![
                    identity.certificate(roles())?,
                    gateway_only.certificate(vec![NodeRole::Gateway, NodeRole::Replica])?,
                ],
            )?,
        )?;
        let before = store.heads(&network, "chat")?;
        assert!(
            set(
                &mut store,
                &identity,
                &network,
                FollowupMode::Continue,
                1,
                Some(&gateway_only.peer_id()?)
            )
            .is_err()
        );
        let policy = store.conversation_autonomy(&network, "chat")?;
        assert_eq!((policy.mode, policy.version), (FollowupMode::Off, 1));
        assert_eq!(store.heads(&network, "chat")?, before);
        Ok(())
    }

    #[test]
    fn removed_scheduler_role_does_not_prevent_persistent_revocation() -> Result<()> {
        let (mut store, identity, network, node) = fixture()?;
        set(
            &mut store,
            &identity,
            &network,
            FollowupMode::Report,
            0,
            Some(&node),
        )?;
        store.install_trust_bundle(
            &network,
            &identity.sign_network_fixture(
                2,
                vec![identity.certificate(vec![NodeRole::Gateway, NodeRole::Replica])?],
            )?,
        )?;
        let events = scope_events(&store.connection, &network, "chat")?;
        assert!(autonomy_in(&store.connection, &network, "chat", &events, true).is_err());
        assert!(
            set(
                &mut store,
                &identity,
                &network,
                FollowupMode::Continue,
                1,
                None
            )
            .is_err()
        );
        set(&mut store, &identity, &network, FollowupMode::Off, 1, None)?;
        let policy = store.conversation_autonomy(&network, "chat")?;
        assert_eq!((policy.mode, policy.version), (FollowupMode::Off, 2));
        Ok(())
    }
}
