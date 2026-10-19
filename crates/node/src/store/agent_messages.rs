use super::*;
use serde_json::json;

fn event_from(connection: &Connection, network: &str, scope: &str, hash: &str) -> Result<Event> {
    let encoded: String = connection.query_row(
        "SELECT envelope FROM events WHERE network_id=? AND scope_id=? AND hash=? AND status='ready'",
        params![network, scope, hash], |row| row.get(0),
    ).optional()?.context("agent message reference is missing or incomplete")?;
    let envelope: SignedEvent = serde_json::from_str(&encoded)?;
    let event: Event = serde_json::from_str(&envelope.canonical_body)?;
    let key: String = connection.query_row(
        "SELECT public_key FROM node_certificates WHERE network_id=? AND peer_id=?",
        params![network, event.author],
        |row| row.get(0),
    )?;
    verify_signed_event(network, &envelope, &key)
}

pub(super) fn validate_agent_trigger_from(
    connection: &Connection,
    network: &str,
    event: &Event,
    message: &AgentMessage,
    target: &str,
) -> Result<TriggerKind> {
    ensure!(
        message.target_node == target,
        "agent message is addressed to another device"
    );
    authorize_node_from(
        connection,
        network,
        &event.author,
        Some(NodeRole::Scheduler),
    )?;
    let origin = event_from(
        connection,
        network,
        &event.scope_id,
        &message.origin_trigger,
    )?;
    if message.kind == AgentMessageKind::Task {
        ensure!(
            message.in_reply_to.is_none(),
            "an agent task must identify its origin trigger"
        );
        let origin_target = match origin.body {
            EventBody::MessagePosted {
                role,
                scheduler_node,
                ..
            } if role == "user" => scheduler_node,
            EventBody::AgentMessage { message } => Some(message.target_node),
            _ => None,
        };
        ensure!(
            origin_target.as_deref() == Some(event.author.as_str()),
            "task origin belongs to another agent"
        );
        return Ok(TriggerKind::AgentTask);
    }
    ensure!(
        matches!(
            message.kind,
            AgentMessageKind::Completed | AgentMessageKind::Failed | AgentMessageKind::Interrupted
        ),
        "progress cannot start an agent"
    );
    let task = event_from(
        connection,
        network,
        &event.scope_id,
        message
            .in_reply_to
            .as_deref()
            .context("agent result has no original task")?,
    )?;
    let EventBody::AgentMessage { message: original } = task.body else {
        bail!("agent result must reply to an agent task");
    };
    ensure!(
        original.kind == AgentMessageKind::Task
            && original.target_node == event.author
            && task.author == target
            && original.origin_trigger == message.origin_trigger,
        "agent result does not match its source, target or original request"
    );
    Ok(TriggerKind::AgentResult)
}

pub(super) fn result_consumed_by_parent(
    connection: &Connection,
    network: &str,
    scope: &str,
    trigger: &str,
    target: &str,
) -> Result<bool> {
    let event = event_from(connection, network, scope, trigger)?;
    let EventBody::AgentMessage { message } = event.body else {
        bail!("agent result trigger changed");
    };
    let parent: Option<SchedulerJob> = scheduler_load(
        connection,
        network,
        &scheduler_key("job", scope, target, &message.origin_trigger),
    )?;
    Ok(parent.is_some_and(|job| {
        job.kind == TriggerKind::AgentTask && matches!(job.state, JobState::Completed { .. })
    }))
}

#[expect(
    clippy::too_many_arguments,
    reason = "cancellation binds the signed task, authenticated source and terminal evidence"
)]
fn cancel_agent_task_from(
    connection: &Connection,
    identity: &LocalIdentity,
    network: &str,
    scope: &str,
    task_ref: &str,
    source_node: &str,
    evidence: &str,
    at: i64,
) -> Result<SchedulerJob> {
    let local = identity.peer_id()?;
    let task = event_from(connection, network, scope, task_ref)?;
    let EventBody::AgentMessage { message } = &task.body else {
        bail!("cancellation must reference an agent task");
    };
    ensure!(
        message.kind == AgentMessageKind::Task
            && task.author == source_node
            && message.target_node == local,
        "cancellation does not match the original source and target"
    );
    admit_scheduler_from(connection, network, scope, task_ref, &local, &local)?;
    let key = scheduler_key("job", scope, &local, task_ref);
    let mut job: SchedulerJob =
        scheduler_load(connection, network, &key)?.context("agent task admission is missing")?;
    if scheduler_publication_from(connection, network, &job)?.is_some() {
        return Ok(job);
    }
    let mut session = load_scheduler_session(connection, network, scope, &local)?;
    let terminal = match &job.state {
        JobState::Queued => {
            job.state = JobState::Cancelled {
                attempt: None,
                evidence: evidence.into(),
            };
            session.pending.retain(|pending| pending != task_ref);
            Some(JobOutcome::Cancelled(evidence.into()))
        }
        JobState::Claimed { attempt } | JobState::Running { attempt } => {
            ensure!(
                session.active.as_deref() == Some(task_ref),
                "agent task is no longer active"
            );
            job.state = JobState::Cancelling {
                attempt: attempt.clone(),
            };
            session.permission_version = session
                .permission_version
                .checked_add(1)
                .context("scheduler permission version overflow")?;
            None
        }
        _ => None,
    };
    scheduler_save(connection, network, &key, &job)?;
    save_scheduler_session(connection, network, &session)?;
    if let Some(terminal) = terminal {
        publish_agent_result_from(connection, identity, network, &job, &terminal, at)?;
    }
    Ok(job)
}

impl Store {
    pub(crate) fn admit_received_trigger(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        scope: &str,
        trigger: &str,
        at: i64,
    ) -> Result<AdmissionReceipt> {
        let local = identity.peer_id()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut receipt = admit_scheduler_from(&tx, network, scope, trigger, &local, &local)?;
        if receipt.state == "queued" {
            let event = event_from(&tx, network, scope, trigger)?;
            if let EventBody::AgentMessage { message } = &event.body
                && message.kind == AgentMessageKind::Task
                && matches!(event_from(&tx, network, scope, &message.origin_trigger)?.body,
                    EventBody::AgentMessage { message } if message.kind == AgentMessageKind::Task)
            {
                let session = load_scheduler_session(&tx, network, scope, &local)?;
                for other in session
                    .active
                    .iter()
                    .chain(&session.pending)
                    .chain(&session.unresolved)
                {
                    if other == trigger {
                        continue;
                    }
                    let job: SchedulerJob =
                        scheduler_load(&tx, network, &scheduler_key("job", scope, &local, other))?
                            .context("scheduler task admission is missing")?;
                    if job.kind == TriggerKind::AgentTask
                        && matches!(
                            job.state,
                            JobState::Queued
                                | JobState::Claimed { .. }
                                | JobState::Running { .. }
                                | JobState::Cancelling { .. }
                                | JobState::Unknown { .. }
                        )
                    {
                        receipt = cancel_agent_task_from(
                            &tx, identity, network, scope, trigger, &event.author,
                            "target agent is busy with another task in this conversation; nested task was not started",
                            at,
                        )?.receipt(network);
                        break;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(receipt)
    }

    pub fn cancel_agent_task(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        scope: &str,
        task_ref: &str,
        source_node: &str,
        at: i64,
    ) -> Result<Option<SchedulerJob>> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let job = cancel_agent_task_from(
            &tx,
            identity,
            network,
            scope,
            task_ref,
            source_node,
            "task cancelled before its agent started",
            at,
        )?;
        tx.commit()?;
        Ok(Some(job))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "reconciliation binds the exact fenced attempt and its terminal publication"
    )]
    pub(crate) fn reconcile_native_job(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        scope: &str,
        target: &str,
        trigger: &str,
        attempt_id: &str,
        outcome: JobOutcome,
        events: Vec<(EventBody, Vec<BlobRef>)>,
        at: i64,
    ) -> Result<SchedulerPublication> {
        ensure!(
            identity.peer_id()? == target,
            "native reconciliation belongs to another device"
        );
        ensure!(
            !matches!(outcome, JobOutcome::Unknown(_)),
            "native outcome is still unknown"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = scheduler_key("job", scope, target, trigger);
        let mut job: SchedulerJob =
            scheduler_load(&tx, network, &key)?.context("native job is missing")?;
        ensure!(
            job.state
                .attempt()
                .is_some_and(|attempt| attempt.id == attempt_id),
            "native reconciliation attempt changed"
        );
        if matches!(
            job.state,
            JobState::Completed { .. } | JobState::Failed { .. } | JobState::Cancelled { .. }
        ) {
            return Ok(SchedulerPublication {
                job,
                event_hashes: Vec::new(),
            });
        }
        let mut attempt = match &job.state {
            JobState::Unknown {
                attempt,
                fenced: true,
                ..
            } => attempt.clone(),
            _ => bail!("native reconciliation requires a fenced unknown attempt"),
        };
        let mut session = load_scheduler_session(&tx, network, scope, target)?;
        ensure!(
            session
                .active
                .as_deref()
                .is_none_or(|active| active == trigger),
            "another scheduler invocation is active"
        );
        attempt.read_only = true;
        attempt.permission_version = session.permission_version;
        job.state = JobState::Running { attempt };
        session.active = Some(trigger.into());
        session.unresolved.remove(trigger);
        validate_scheduler_terminal(&job, &outcome, &events)?;
        scheduler_save(&tx, network, &key, &job)?;
        save_scheduler_session(&tx, network, &session)?;
        scheduler_save(
            &tx,
            network,
            &scheduler_key("publication", scope, target, trigger),
            &SchedulerTerminalPublication {
                version: SCHEDULING_VERSION,
                job: job.clone(),
                outcome,
                events,
                created_at_ms: at,
            },
        )?;
        let publication = publish_scheduler_terminal_from(&tx, identity, network, &job)?;
        tx.commit()?;
        Ok(publication)
    }

    pub fn agent_task_for_message(
        &self,
        network: &str,
        scope: &str,
        message_id: &str,
    ) -> Result<Option<SignedEvent>> {
        let encoded: Option<String> = self.connection.query_row(
            "SELECT envelope FROM events WHERE network_id=? AND scope_id=? AND json_extract(json_extract(envelope,'$.canonical_body'),'$.body.message.message_id')=?",
            params![network, scope, message_id], |row| row.get(0),
        ).optional()?;
        optional_json(encoded)
    }

    pub fn agent_task_status(&self, network: &str, scope: &str, task_ref: &str) -> Result<Value> {
        let task = event_from(&self.connection, network, scope, task_ref)?;
        let EventBody::AgentMessage { message } = &task.body else {
            bail!("task reference is not an agent message");
        };
        ensure!(
            message.kind == AgentMessageKind::Task,
            "task reference must select a task"
        );
        let result = self
            .context(network, scope, &[])?
            .events
            .into_iter()
            .find_map(|event| {
                let EventBody::AgentMessage { message: reply } = event.body else {
                    return None;
                };
                (reply.in_reply_to.as_deref() == Some(task_ref)
                    && event.author == message.target_node
                    && reply.target_node == task.author
                    && reply.origin_trigger == message.origin_trigger
                    && matches!(
                        reply.kind,
                        AgentMessageKind::Completed
                            | AgentMessageKind::Failed
                            | AgentMessageKind::Interrupted
                    ))
                .then_some(reply)
            });
        let job = self.scheduler_job(network, scope, task_ref, &message.target_node)?;
        let local_state = job.as_ref().map(|job| job.state.name());
        let entry: Option<OutboxEntry> = scheduler_load(
            &self.connection,
            network,
            &scheduler_key("outbox", scope, &message.target_node, task_ref),
        )?;
        Ok(
            json!({"task_ref":task_ref,"task":message,"source_node":task.author,
            "state":result.as_ref().map(|result| match result.kind {
                AgentMessageKind::Completed => "completed", AgentMessageKind::Failed => "failed",
                AgentMessageKind::Interrupted => "interrupted", _ => unreachable!(),
            }).or(local_state).unwrap_or(if entry.is_some_and(|entry| entry.delivered) { "accepted" } else { "pending_delivery" }),
            "result":result,"freshness":"locally_known"}),
        )
    }

    pub fn post_agent_task(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        scope: &str,
        message: AgentMessage,
        attempt_id: &str,
        created_at_ms: i64,
    ) -> Result<(SignedEvent, AdmissionReceipt)> {
        let source = identity.peer_id()?;
        let target = message.target_node.clone();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            message.kind == AgentMessageKind::Task,
            "only tasks may be submitted by an agent"
        );
        let job: SchedulerJob = scheduler_load(
            &tx,
            network,
            &scheduler_key("job", scope, &source, &message.origin_trigger),
        )?
        .context("agent task requires an admitted source invocation")?;
        ensure!(
            matches!(&job.state, JobState::Running { attempt } if attempt.id == attempt_id && !attempt.read_only),
            "source agent is not running or cannot send tasks"
        );
        let mut origin = message.origin_trigger.clone();
        while let EventBody::AgentMessage { message: ancestor } =
            event_from(&tx, network, scope, &origin)?.body
        {
            if ancestor.kind != AgentMessageKind::Task {
                break;
            }
            ensure!(
                ancestor.target_node != target,
                "target agent is waiting for this task; run local work directly"
            );
            origin = ancestor.origin_trigger;
        }
        let body = EventBody::AgentMessage {
            message: message.clone(),
        };
        let previous: Option<String> = tx.query_row(
            "SELECT envelope FROM events WHERE network_id=? AND scope_id=? AND json_extract(json_extract(envelope,'$.canonical_body'),'$.body.message.message_id')=?",
            params![network, scope, message.message_id], |row| row.get(0),
        ).optional()?;
        let envelope = if let Some(previous) = previous {
            let envelope: SignedEvent = serde_json::from_str(&previous)?;
            let event: Event = serde_json::from_str(&envelope.canonical_body)?;
            ensure!(
                event.author == source && event.body == body,
                "agent message ID reused with different source, content or target"
            );
            envelope
        } else {
            append_event_from(&tx, identity, network, scope, body, vec![], created_at_ms)?
        };
        let receipt = admit_scheduler_from(&tx, network, scope, &envelope.hash, &source, &target)?;
        tx.commit()?;
        Ok((envelope, receipt))
    }
}

pub(super) fn publish_agent_result_from(
    connection: &Connection,
    identity: &LocalIdentity,
    network: &str,
    job: &SchedulerJob,
    outcome: &JobOutcome,
    created_at_ms: i64,
) -> Result<Option<String>> {
    if job.kind != TriggerKind::AgentTask {
        return Ok(None);
    }
    let (kind, detail) = match outcome {
        JobOutcome::Completed => (AgentMessageKind::Completed, None),
        JobOutcome::Failed(detail) => (AgentMessageKind::Failed, Some(detail.as_str())),
        JobOutcome::Cancelled(detail) => (AgentMessageKind::Interrupted, Some(detail.as_str())),
        JobOutcome::Unknown(_) => return Ok(None),
    };
    let task = event_from(connection, network, &job.conversation_id, &job.trigger)?;
    let EventBody::AgentMessage { message } = task.body else {
        bail!("agent task trigger changed");
    };
    let text = if let Some(detail) = detail {
        detail.to_owned()
    } else {
        connection.query_row(
            "SELECT text FROM messages WHERE network_id=? AND conversation_id=? AND message_id=?",
            params![network, job.conversation_id, job.message_id],
            |row| row.get(0),
        )?
    };
    let envelope = append_event_from(
        connection,
        identity,
        network,
        &job.conversation_id,
        EventBody::AgentMessage {
            message: AgentMessage {
                message_id: format!("agent-result-{}", job.trigger),
                target_node: task.author.clone(),
                origin_trigger: message.origin_trigger,
                in_reply_to: Some(job.trigger.clone()),
                kind,
                text,
            },
        },
        vec![],
        created_at_ms,
    )?;
    admit_scheduler_from(
        connection,
        network,
        &job.conversation_id,
        &envelope.hash,
        &identity.peer_id()?,
        &task.author,
    )?;
    Ok(Some(envelope.hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles() -> Vec<NodeRole> {
        vec![NodeRole::Gateway, NodeRole::Replica, NodeRole::Scheduler]
    }

    fn fixture() -> Result<(Store, LocalIdentity, LocalIdentity, String)> {
        let source = LocalIdentity::generate();
        let target = LocalIdentity::generate();
        let network = source.fixture_network_id()?;
        let path = std::env::temp_dir()
            .join(format!("vibeface-agent-messages-{}", uuid::Uuid::new_v4()))
            .join("node.sqlite3");
        let mut store = Store::open(path)?;
        store.install_trust_bundle(
            &network,
            &source.sign_network_fixture(
                1,
                vec![source.certificate(roles())?, target.certificate(roles())?],
            )?,
        )?;
        store.append_event(
            &source,
            &network,
            "chat",
            EventBody::ConversationCreated {
                title: "chat".into(),
            },
            vec![],
            1,
        )?;
        Ok((store, source, target, network))
    }

    fn cleanup(store: Store) -> Result<()> {
        let directory = store
            .path
            .parent()
            .context("test directory missing")?
            .to_owned();
        drop(store);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    fn start_next(store: &mut Store, network: &str, node: &str) -> Result<SchedulerJob> {
        let claimed = store
            .claim_next(network, "chat", node, "test-boot", "sdk")?
            .context("scheduler job missing")?;
        let attempt = claimed.job.state.attempt().context("attempt missing")?;
        store.mark_started(network, "chat", node, &claimed.job.trigger, &attempt.id)
    }

    fn start_human(
        store: &mut Store,
        identity: &LocalIdentity,
        network: &str,
    ) -> Result<(SignedEvent, SchedulerJob)> {
        let node = identity.peer_id()?;
        let (event, receipt) = store.accept_message(
            identity,
            network,
            "chat",
            EventBody::MessagePosted {
                message_id: "request".into(),
                role: "user".into(),
                text: "do the work".into(),
                scheduler_node: Some(node.clone()),
            },
            vec![],
            2,
            &node,
            &node,
            true,
        )?;
        assert_eq!(receipt.state, "queued");
        let job = start_next(store, network, &node)?;
        assert_eq!(job.trigger, event.hash);
        Ok((event, job))
    }

    fn task_message(id: &str, target: &str, origin: &SignedEvent) -> AgentMessage {
        AgentMessage {
            message_id: id.into(),
            target_node: target.into(),
            origin_trigger: origin.hash.clone(),
            in_reply_to: None,
            kind: AgentMessageKind::Task,
            text: "work on target".into(),
        }
    }

    fn final_reply(job: &SchedulerJob, text: &str) -> Vec<(EventBody, Vec<BlobRef>)> {
        vec![(
            EventBody::MessagePosted {
                message_id: job.message_id.clone(),
                role: "assistant".into(),
                text: text.into(),
                scheduler_node: None,
            },
            vec![],
        )]
    }

    fn replies(store: &Store, network: &str, task: &SignedEvent) -> Result<Vec<Event>> {
        Ok(store
            .context(network, "chat", &[])?
            .events
            .into_iter()
            .filter(|event| {
                matches!(&event.body, EventBody::AgentMessage { message }
                if message.in_reply_to.as_deref() == Some(task.hash.as_str()))
            })
            .collect())
    }

    fn result_envelope(store: &Store, network: &str, task: &SignedEvent) -> Result<SignedEvent> {
        store
            .agent_task_for_message(network, "chat", &format!("agent-result-{}", task.hash))?
            .context("agent result missing")
    }

    #[test]
    fn task_delegation_rejects_waiting_agent_ancestors() -> Result<()> {
        let (mut store, source, target, network) = fixture()?;
        let child = LocalIdentity::generate();
        store.install_trust_bundle(
            &network,
            &source.sign_network_fixture(
                2,
                vec![
                    source.certificate(roles())?,
                    target.certificate(roles())?,
                    child.certificate(roles())?,
                ],
            )?,
        )?;
        let (human, source_job) = start_human(&mut store, &source, &network)?;
        let (parent, _) = store.post_agent_task(
            &source,
            &network,
            "chat",
            task_message("parent", &target.peer_id()?, &human),
            &source_job.state.attempt().unwrap().id,
            3,
        )?;
        let target_id = target.peer_id()?;
        store.admit_trigger(&network, "chat", &parent.hash, &target_id, &target_id)?;
        let parent_job = start_next(&mut store, &network, &target_id)?;
        let heads = store.context(&network, "chat", &[])?.heads;
        assert!(
            store
                .post_agent_task(
                    &target,
                    &network,
                    "chat",
                    task_message("self", &target_id, &parent),
                    &parent_job.state.attempt().unwrap().id,
                    4,
                )
                .unwrap_err()
                .to_string()
                .contains("target agent is waiting")
        );
        assert_eq!(store.context(&network, "chat", &[])?.heads, heads);
        let child_id = child.peer_id()?;
        let (nested, _) = store.post_agent_task(
            &target,
            &network,
            "chat",
            task_message("nested", &child_id, &parent),
            &parent_job.state.attempt().unwrap().id,
            5,
        )?;
        store.admit_trigger(&network, "chat", &nested.hash, &child_id, &child_id)?;
        let child_job = start_next(&mut store, &network, &child_id)?;
        let heads = store.context(&network, "chat", &[])?.heads;
        assert!(
            store
                .post_agent_task(
                    &child,
                    &network,
                    "chat",
                    task_message("cycle", &target_id, &nested),
                    &child_job.state.attempt().unwrap().id,
                    6,
                )
                .unwrap_err()
                .to_string()
                .contains("target agent is waiting")
        );
        assert_eq!(store.context(&network, "chat", &[])?.heads, heads);
        cleanup(store)
    }

    #[test]
    fn parallel_agent_parents_interrupt_mutual_nested_deliveries_without_starting_them()
    -> Result<()> {
        for unknown in [false, true] {
            let (mut store, source, b, network) = fixture()?;
            let c = LocalIdentity::generate();
            let b_id = b.peer_id()?;
            let c_id = c.peer_id()?;
            store.install_trust_bundle(
                &network,
                &source.sign_network_fixture(
                    2,
                    vec![
                        source.certificate(roles())?,
                        b.certificate(roles())?,
                        c.certificate(roles())?,
                    ],
                )?,
            )?;
            let (human, source_job) = start_human(&mut store, &source, &network)?;
            let source_attempt = &source_job
                .state
                .attempt()
                .context("source attempt missing")?
                .id;
            let (parent_b, _) = store.post_agent_task(
                &source,
                &network,
                "chat",
                task_message("parent-b", &b_id, &human),
                source_attempt,
                3,
            )?;
            let (parent_c, _) = store.post_agent_task(
                &source,
                &network,
                "chat",
                task_message("parent-c", &c_id, &human),
                source_attempt,
                3,
            )?;
            store.admit_received_trigger(&b, &network, "chat", &parent_b.hash, 4)?;
            store.admit_received_trigger(&c, &network, "chat", &parent_c.hash, 4)?;
            let running_b = start_next(&mut store, &network, &b_id)?;
            let running_c = start_next(&mut store, &network, &c_id)?;
            let (nested_c, _) = store.post_agent_task(
                &b,
                &network,
                "chat",
                task_message("b-to-c", &c_id, &parent_b),
                &running_b.state.attempt().context("B attempt missing")?.id,
                5,
            )?;
            let (nested_b, _) = store.post_agent_task(
                &c,
                &network,
                "chat",
                task_message("c-to-b", &b_id, &parent_c),
                &running_c.state.attempt().context("C attempt missing")?.id,
                5,
            )?;
            if unknown {
                let attempt = &running_c.state.attempt().context("C attempt missing")?.id;
                store.finish_job(
                    &c,
                    &network,
                    "chat",
                    &c_id,
                    &parent_c.hash,
                    attempt,
                    JobOutcome::Unknown("parent connection lost".into()),
                    vec![],
                    6,
                )?;
                store.fence_manager_unknown(&network, "chat", &c_id, &parent_c.hash, attempt)?;
            }
            let before_c = store.scheduler_job(&network, "chat", &parent_c.hash, &c_id)?;
            store.connection.execute_batch(
                "CREATE TEMP TRIGGER reject_busy_result BEFORE INSERT ON events
                 WHEN json_extract(json_extract(NEW.envelope,'$.canonical_body'),'$.body.message.kind')='interrupted'
                 BEGIN SELECT RAISE(ABORT,'injected busy result failure'); END;",
            )?;
            assert!(
                store
                    .admit_received_trigger(&c, &network, "chat", &nested_c.hash, 7)
                    .is_err()
            );
            assert!(
                store
                    .scheduler_job(&network, "chat", &nested_c.hash, &c_id)?
                    .is_none()
            );
            assert_eq!(
                store.scheduler_job(&network, "chat", &parent_c.hash, &c_id)?,
                before_c
            );
            store
                .connection
                .execute_batch("DROP TRIGGER reject_busy_result;")?;
            for (target, task, source_id) in [(&c, &nested_c, &b_id), (&b, &nested_b, &c_id)] {
                let target_id = target.peer_id()?;
                let receipt =
                    store.admit_received_trigger(target, &network, "chat", &task.hash, 7)?;
                assert_eq!(receipt.state, "cancelled");
                assert_eq!(
                    store.admit_received_trigger(target, &network, "chat", &task.hash, 8)?,
                    receipt
                );
                assert_eq!(store.scheduler_job(&network, "chat", &task.hash, &target_id)?
                    .context("interrupted task missing")?.state,
                    JobState::Cancelled {
                        attempt: None,
                        evidence: "target agent is busy with another task in this conversation; nested task was not started".into(),
                    });
                let results = replies(&store, &network, task)?;
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].author, target_id);
                assert!(
                    matches!(&results[0].body, EventBody::AgentMessage { message }
                    if message.kind == AgentMessageKind::Interrupted
                        && &message.target_node == source_id
                        && message.text == "target agent is busy with another task in this conversation; nested task was not started")
                );
                assert!(
                    store
                        .claim_next(&network, "chat", &target_id, "test-boot", "sdk")?
                        .is_none()
                );
            }
            assert_eq!(
                store.scheduler_job(&network, "chat", &parent_b.hash, &b_id)?,
                Some(running_b)
            );
            assert_eq!(
                store.scheduler_job(&network, "chat", &parent_c.hash, &c_id)?,
                before_c
            );
            cleanup(store)?;
        }
        Ok(())
    }

    #[test]
    fn queued_parent_and_nested_admissions_serialize_without_a_wait_cycle() -> Result<()> {
        for order in ["parent-first", "nested-first", "concurrent"] {
            let (mut store, source, b, network) = fixture()?;
            let c = LocalIdentity::generate();
            let b_id = b.peer_id()?;
            let c_id = c.peer_id()?;
            store.install_trust_bundle(
                &network,
                &source.sign_network_fixture(
                    2,
                    vec![
                        source.certificate(roles())?,
                        b.certificate(roles())?,
                        c.certificate(roles())?,
                    ],
                )?,
            )?;
            let (human, source_job) = start_human(&mut store, &source, &network)?;
            let source_attempt = &source_job
                .state
                .attempt()
                .context("source attempt missing")?
                .id;
            let (parent_b, _) = store.post_agent_task(
                &source,
                &network,
                "chat",
                task_message("parent-b", &b_id, &human),
                source_attempt,
                3,
            )?;
            let (parent_c, _) = store.post_agent_task(
                &source,
                &network,
                "chat",
                task_message("parent-c", &c_id, &human),
                source_attempt,
                3,
            )?;
            store.admit_received_trigger(&b, &network, "chat", &parent_b.hash, 4)?;
            let running_b = start_next(&mut store, &network, &b_id)?;
            let (nested, _) = store.post_agent_task(
                &b,
                &network,
                "chat",
                task_message("b-to-c", &c_id, &parent_b),
                &running_b.state.attempt().context("B attempt missing")?.id,
                5,
            )?;
            let path = store.path.clone();
            drop(store);
            let mut parent_store = Store::open(&path)?;
            let mut nested_store = Store::open(&path)?;
            let ready = std::sync::Barrier::new(2);
            let committed = std::sync::Barrier::new(2);
            let (parent_receipt, nested_receipt) = std::thread::scope(|threads| -> Result<_> {
                let parent = threads.spawn(|| {
                    ready.wait();
                    if order == "nested-first" {
                        committed.wait();
                    }
                    let result = parent_store.admit_received_trigger(
                        &c,
                        &network,
                        "chat",
                        &parent_c.hash,
                        6,
                    );
                    if order == "parent-first" {
                        committed.wait();
                    }
                    result
                });
                let child = threads.spawn(|| {
                    ready.wait();
                    if order == "parent-first" {
                        committed.wait();
                    }
                    let result =
                        nested_store.admit_received_trigger(&c, &network, "chat", &nested.hash, 6);
                    if order == "nested-first" {
                        committed.wait();
                    }
                    result
                });
                Ok((
                    parent
                        .join()
                        .map_err(|_| anyhow::anyhow!("parent admission panicked"))??,
                    child
                        .join()
                        .map_err(|_| anyhow::anyhow!("nested admission panicked"))??,
                ))
            })?;
            drop(parent_store);
            drop(nested_store);
            assert_eq!(parent_receipt.state, "queued");
            let mut store = Store::open(&path)?;
            let parent = store
                .scheduler_job(&network, "chat", &parent_c.hash, &c_id)?
                .context("queued parent missing")?;
            let child = store
                .scheduler_job(&network, "chat", &nested.hash, &c_id)?
                .context("nested admission missing")?;
            assert_eq!(parent.state, JobState::Queued);
            if order == "parent-first" {
                assert_eq!(nested_receipt.state, "cancelled");
            }
            if order == "nested-first" {
                assert_eq!(nested_receipt.state, "queued");
            }
            let running = start_next(&mut store, &network, &c_id)?;
            if nested_receipt.state == "cancelled" {
                assert!(parent.sequence < child.sequence);
                assert_eq!(running.trigger, parent_c.hash);
                assert!(matches!(
                    child.state,
                    JobState::Cancelled { attempt: None, .. }
                ));
                let results = replies(&store, &network, &nested)?;
                assert_eq!(results.len(), 1);
                assert!(
                    matches!(&results[0].body, EventBody::AgentMessage { message }
                    if message.kind == AgentMessageKind::Interrupted)
                );
            } else {
                assert_eq!(nested_receipt.state, "queued");
                assert!(child.sequence < parent.sequence);
                assert_eq!(running.trigger, nested.hash);
                assert_eq!(running.kind, TriggerKind::AgentTask);
                assert!(replies(&store, &network, &nested)?.is_empty());
                assert_eq!(
                    store
                        .admit_received_trigger(&c, &network, "chat", &nested.hash, 7)?
                        .state,
                    "running"
                );
                assert_eq!(
                    store.scheduler_job(&network, "chat", &nested.hash, &c_id)?,
                    Some(running.clone())
                );
            }
            let (remote_human, _) = store.accept_message(
                &source,
                &network,
                "chat",
                EventBody::MessagePosted {
                    message_id: "remote-human".into(),
                    role: "user".into(),
                    text: "new work".into(),
                    scheduler_node: Some(c_id.clone()),
                },
                vec![],
                8,
                &source.peer_id()?,
                &c_id,
                true,
            )?;
            assert_eq!(
                store
                    .admit_received_trigger(&c, &network, "chat", &remote_human.hash, 8)?
                    .state,
                "queued"
            );
            let (ordinary, _) = store.post_agent_task(
                &source,
                &network,
                "chat",
                task_message("ordinary", &c_id, &human),
                source_attempt,
                9,
            )?;
            assert_eq!(
                store
                    .admit_received_trigger(&c, &network, "chat", &ordinary.hash, 9)?
                    .state,
                "queued"
            );
            assert_eq!(
                store.scheduler_job(&network, "chat", &running.trigger, &c_id)?,
                Some(running)
            );
            cleanup(store)?;
        }
        Ok(())
    }

    #[test]
    fn stable_task_ids_reject_changed_content_or_target() -> Result<()> {
        let (mut store, source, target, network) = fixture()?;
        let source_node = source.peer_id()?;
        let target_node = target.peer_id()?;
        let (origin, human) = start_human(&mut store, &source, &network)?;
        let attempt = &human.state.attempt().context("human attempt missing")?.id;
        let message = task_message("stable-task", &target_node, &origin);
        let first =
            store.post_agent_task(&source, &network, "chat", message.clone(), attempt, 3)?;
        let repeated =
            store.post_agent_task(&source, &network, "chat", message.clone(), attempt, 4)?;
        assert_eq!(repeated, first);
        assert_eq!(first.1.state, "pending_delivery");
        for changed in [
            AgentMessage {
                text: "changed work".into(),
                ..message.clone()
            },
            AgentMessage {
                target_node: source_node.clone(),
                ..message.clone()
            },
        ] {
            assert!(
                store
                    .post_agent_task(&source, &network, "chat", changed, attempt, 5)
                    .unwrap_err()
                    .to_string()
                    .contains("agent message ID reused")
            );
        }
        assert_eq!(store.scheduler_outbox_all(&network)?.len(), 1);
        assert_eq!(
            store.agent_task_for_message(&network, "chat", "stable-task")?,
            Some(first.0.clone())
        );
        assert_eq!(
            store.verify_event(&network, &first.0)?.body,
            EventBody::AgentMessage { message }
        );
        assert_eq!(
            store.agent_task_status(&network, "chat", &first.0.hash)?["state"],
            "pending_delivery"
        );
        assert!(
            store
                .scheduler_job(&network, "chat", &first.0.hash, &target_node)?
                .is_none()
        );
        assert_eq!(
            store.scheduler_jobs(&network, "chat", &source_node)?.len(),
            1
        );
        let receipt =
            store.admit_trigger(&network, "chat", &first.0.hash, &target_node, &target_node)?;
        assert_eq!(receipt.state, "queued");
        assert_eq!(
            store.admit_trigger(&network, "chat", &first.0.hash, &target_node, &target_node)?,
            receipt
        );
        assert_eq!(
            store.scheduler_jobs(&network, "chat", &target_node)?.len(),
            1
        );
        cleanup(store)
    }

    #[test]
    fn task_terminals_publish_one_reply_to_the_original_request() -> Result<()> {
        for (outcome, kind, state, text) in [
            (
                JobOutcome::Completed,
                AgentMessageKind::Completed,
                "completed",
                "done",
            ),
            (
                JobOutcome::Failed("failed work".into()),
                AgentMessageKind::Failed,
                "failed",
                "failed work",
            ),
            (
                JobOutcome::Cancelled("stopped work".into()),
                AgentMessageKind::Interrupted,
                "interrupted",
                "stopped work",
            ),
        ] {
            let (mut store, source, target, network) = fixture()?;
            let source_node = source.peer_id()?;
            let target_node = target.peer_id()?;
            let (origin, human) = start_human(&mut store, &source, &network)?;
            let attempt = &human.state.attempt().context("human attempt missing")?.id;
            let (task, _) = store.post_agent_task(
                &source,
                &network,
                "chat",
                task_message("task", &target_node, &origin),
                attempt,
                3,
            )?;
            store.admit_trigger(&network, "chat", &task.hash, &target_node, &target_node)?;
            let running = start_next(&mut store, &network, &target_node)?;
            assert_eq!(running.kind, TriggerKind::AgentTask);
            let target_attempt = running.state.attempt().context("target attempt missing")?;
            assert!(!target_attempt.read_only);
            if matches!(&outcome, JobOutcome::Cancelled(_)) {
                let cancelling = store
                    .cancel_agent_task(&target, &network, "chat", &task.hash, &source_node, 4)?
                    .context("cancelled task missing")?;
                assert!(matches!(cancelling.state, JobState::Cancelling { .. }));
            }
            let events = if matches!(&outcome, JobOutcome::Completed) {
                final_reply(&running, text)
            } else {
                vec![]
            };
            let terminal = store.finish_job(
                &target,
                &network,
                "chat",
                &target_node,
                &task.hash,
                &target_attempt.id,
                outcome.clone(),
                events.clone(),
                5,
            )?;
            assert_eq!(
                store.finish_job(
                    &target,
                    &network,
                    "chat",
                    &target_node,
                    &task.hash,
                    &target_attempt.id,
                    outcome,
                    events,
                    6,
                )?,
                terminal
            );
            let results = replies(&store, &network, &task)?;
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].author, target_node);
            let expected = AgentMessage {
                message_id: format!("agent-result-{}", task.hash),
                target_node: source_node,
                origin_trigger: origin.hash.clone(),
                in_reply_to: Some(task.hash.clone()),
                kind,
                text: text.into(),
            };
            assert_eq!(
                results[0].body,
                EventBody::AgentMessage {
                    message: expected.clone()
                }
            );
            assert!(results[0].parents.contains(&origin.hash));
            assert!(results[0].parents.contains(&task.hash));
            let status = store.agent_task_status(&network, "chat", &task.hash)?;
            assert_eq!(status["state"], state);
            assert_eq!(status["result"], serde_json::to_value(expected)?);
            cleanup(store)?;
        }
        Ok(())
    }

    #[test]
    fn same_node_task_terminals_preserve_the_pending_result_report() -> Result<()> {
        for cancelled in [false, true] {
            let (mut store, source, _target, network) = fixture()?;
            let node = source.peer_id()?;
            let (origin, human) = start_human(&mut store, &source, &network)?;
            let human_attempt = &human.state.attempt().context("human attempt missing")?.id;
            let (task, receipt) = store.post_agent_task(
                &source,
                &network,
                "chat",
                task_message("local-task", &node, &origin),
                human_attempt,
                3,
            )?;
            assert_eq!(receipt.state, "queued");
            assert_eq!(
                store.admit_received_trigger(&source, &network, "chat", &task.hash, 3)?,
                receipt
            );
            assert!(
                store
                    .claim_next(&network, "chat", &node, "test-boot", "sdk")?
                    .is_none()
            );
            store.finish_job(
                &source,
                &network,
                "chat",
                &node,
                &origin.hash,
                human_attempt,
                JobOutcome::Completed,
                final_reply(&human, "delegated"),
                4,
            )?;
            let kind = if cancelled {
                let terminal = store
                    .cancel_agent_task(&source, &network, "chat", &task.hash, &node, 5)?
                    .context("cancelled task missing")?;
                assert!(matches!(
                    terminal.state,
                    JobState::Cancelled { attempt: None, .. }
                ));
                AgentMessageKind::Interrupted
            } else {
                let running = start_next(&mut store, &network, &node)?;
                assert_eq!(running.trigger, task.hash);
                assert_eq!(running.kind, TriggerKind::AgentTask);
                store.finish_job(
                    &source,
                    &network,
                    "chat",
                    &node,
                    &task.hash,
                    &running.state.attempt().context("task attempt missing")?.id,
                    JobOutcome::Completed,
                    final_reply(&running, "local result"),
                    5,
                )?;
                AgentMessageKind::Completed
            };
            let result = result_envelope(&store, &network, &task)?;
            let result_event = store.verify_event(&network, &result)?;
            assert_eq!(result_event.author, node);
            assert!(
                matches!(result_event.body, EventBody::AgentMessage { message }
                if message.kind == kind && message.target_node == node
                    && message.origin_trigger == origin.hash
                    && message.in_reply_to.as_deref() == Some(task.hash.as_str()))
            );
            let path = store.path.clone();
            drop(store);
            let mut store = Store::open(path)?;
            let session = load_scheduler_session(&store.connection, &network, "chat", &node)?;
            assert_eq!(session.pending, vec![result.hash.clone()]);
            assert_eq!(session.active, None);
            let report = start_next(&mut store, &network, &node)?;
            assert_eq!(report.trigger, result.hash);
            assert_eq!(report.kind, TriggerKind::AgentResult);
            let report_attempt = report.state.attempt().context("report attempt missing")?;
            assert!(report_attempt.read_only);
            assert!(
                store
                    .claim_next(&network, "chat", &node, "test-boot", "sdk")?
                    .is_none()
            );
            store.finish_job(
                &source,
                &network,
                "chat",
                &node,
                &result.hash,
                &report_attempt.id,
                JobOutcome::Completed,
                final_reply(&report, "reported"),
                6,
            )?;
            assert_eq!(
                store
                    .admit_trigger(&network, "chat", &result.hash, &node, &node)?
                    .state,
                "completed"
            );
            assert!(
                store
                    .claim_next(&network, "chat", &node, "test-boot", "sdk")?
                    .is_none()
            );
            assert_eq!(replies(&store, &network, &task)?.len(), 1);
            cleanup(store)?;
        }
        Ok(())
    }

    #[test]
    fn cancellation_before_delivery_survives_late_admission_without_starting() -> Result<()> {
        let (mut store, source, target, network) = fixture()?;
        let source_node = source.peer_id()?;
        let target_node = target.peer_id()?;
        let (origin, human) = start_human(&mut store, &source, &network)?;
        let attempt = &human.state.attempt().context("human attempt missing")?.id;
        let (task, _) = store.post_agent_task(
            &source,
            &network,
            "chat",
            task_message("task", &target_node, &origin),
            attempt,
            3,
        )?;
        assert!(
            store
                .cancel_agent_task(&target, &network, "chat", &task.hash, &target_node, 4)
                .is_err()
        );
        assert!(
            store
                .scheduler_job(&network, "chat", &task.hash, &target_node)?
                .is_none()
        );
        let cancelled = store
            .cancel_agent_task(&target, &network, "chat", &task.hash, &source_node, 4)?
            .context("cancelled task missing")?;
        assert_eq!(
            cancelled.state,
            JobState::Cancelled {
                attempt: None,
                evidence: "task cancelled before its agent started".into(),
            }
        );
        assert_eq!(
            store.cancel_agent_task(&target, &network, "chat", &task.hash, &source_node, 5,)?,
            Some(cancelled)
        );
        let path = store.path.clone();
        drop(store);
        let mut store = Store::open(path)?;
        let receipt =
            store.admit_trigger(&network, "chat", &task.hash, &target_node, &target_node)?;
        assert_eq!(receipt.state, "cancelled");
        assert_eq!(
            store.admit_trigger(&network, "chat", &task.hash, &target_node, &target_node)?,
            receipt
        );
        assert!(
            store
                .claim_next(&network, "chat", &target_node, "restarted", "sdk")?
                .is_none()
        );
        let results = replies(&store, &network, &task)?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].author, target_node);
        assert!(
            matches!(&results[0].body, EventBody::AgentMessage { message }
            if message.kind == AgentMessageKind::Interrupted
                && message.target_node == source_node && message.origin_trigger == origin.hash)
        );
        assert_eq!(
            store.agent_task_status(&network, "chat", &task.hash)?["state"],
            "interrupted"
        );
        assert!(
            store
                .scheduler_job(&network, "chat", &task.hash, &target_node)?
                .context("cancelled admission missing")?
                .state
                .attempt()
                .is_none()
        );
        cleanup(store)
    }

    #[test]
    fn native_reconciliation_requires_the_exact_fenced_unknown_and_never_replays() -> Result<()> {
        let (mut store, source, target, network) = fixture()?;
        let target_node = target.peer_id()?;
        let (origin, human) = start_human(&mut store, &source, &network)?;
        let attempt = &human.state.attempt().context("human attempt missing")?.id;
        let (task, _) = store.post_agent_task(
            &source,
            &network,
            "chat",
            task_message("task", &target_node, &origin),
            attempt,
            3,
        )?;
        store.admit_trigger(&network, "chat", &task.hash, &target_node, &target_node)?;
        let running = start_next(&mut store, &network, &target_node)?;
        let target_attempt = &running
            .state
            .attempt()
            .context("target attempt missing")?
            .id;
        assert!(
            store
                .reconcile_native_job(
                    &target,
                    &network,
                    "chat",
                    &target_node,
                    &task.hash,
                    target_attempt,
                    JobOutcome::Completed,
                    final_reply(&running, "native result"),
                    4,
                )
                .is_err()
        );
        let unknown = store.finish_job(
            &target,
            &network,
            "chat",
            &target_node,
            &task.hash,
            target_attempt,
            JobOutcome::Unknown("native connection lost".into()),
            vec![],
            4,
        )?;
        assert!(matches!(
            unknown.state,
            JobState::Unknown { fenced: false, .. }
        ));
        assert!(
            store
                .reconcile_native_job(
                    &target,
                    &network,
                    "chat",
                    &target_node,
                    &task.hash,
                    target_attempt,
                    JobOutcome::Completed,
                    final_reply(&running, "native result"),
                    5,
                )
                .is_err()
        );
        store.fence_manager_unknown(&network, "chat", &target_node, &task.hash, target_attempt)?;
        let path = store.path.clone();
        drop(store);
        let mut store = Store::open(path)?;
        assert!(
            store
                .claim_next(&network, "chat", &target_node, "restarted", "sdk")?
                .is_none()
        );
        for (identity, attempt, outcome) in [
            (&source, target_attempt.as_str(), JobOutcome::Completed),
            (&target, "another-attempt", JobOutcome::Completed),
            (
                &target,
                target_attempt.as_str(),
                JobOutcome::Unknown("still unknown".into()),
            ),
        ] {
            assert!(
                store
                    .reconcile_native_job(
                        identity,
                        &network,
                        "chat",
                        &target_node,
                        &task.hash,
                        attempt,
                        outcome,
                        final_reply(&running, "must not publish"),
                        5,
                    )
                    .is_err()
            );
        }
        let publication = store.reconcile_native_job(
            &target,
            &network,
            "chat",
            &target_node,
            &task.hash,
            target_attempt,
            JobOutcome::Completed,
            final_reply(&running, "native result"),
            6,
        )?;
        assert!(matches!(publication.job.state, JobState::Completed { .. }));
        let completed_attempt = publication
            .job
            .state
            .attempt()
            .context("completed attempt missing")?;
        assert_eq!(&completed_attempt.id, target_attempt);
        assert!(completed_attempt.read_only);
        let result = result_envelope(&store, &network, &task)?;
        assert!(publication.event_hashes.contains(&result.hash));
        let before = store.messages(&network, "chat")?;
        let repeated = store.reconcile_native_job(
            &target,
            &network,
            "chat",
            &target_node,
            &task.hash,
            target_attempt,
            JobOutcome::Completed,
            final_reply(&running, "duplicate result"),
            7,
        )?;
        assert_eq!(repeated.job, publication.job);
        assert!(repeated.event_hashes.is_empty());
        assert_eq!(store.messages(&network, "chat")?, before);
        let results = replies(&store, &network, &task)?;
        assert_eq!(results.len(), 1);
        assert!(
            matches!(&results[0].body, EventBody::AgentMessage { message }
            if message.kind == AgentMessageKind::Completed && message.text == "native result"
                && message.target_node == source.peer_id()? && message.origin_trigger == origin.hash)
        );
        assert!(
            store
                .claim_next(&network, "chat", &target_node, "restarted", "sdk")?
                .is_none()
        );
        cleanup(store)
    }

    #[test]
    fn nested_completion_consumes_only_child_reports_of_completed_agent_parents() -> Result<()> {
        for outcome in [
            JobOutcome::Completed,
            JobOutcome::Failed("parent failed".into()),
            JobOutcome::Unknown("parent disconnected".into()),
        ] {
            let completed = matches!(&outcome, JobOutcome::Completed);
            let unknown = matches!(&outcome, JobOutcome::Unknown(_));
            let (mut store, source, parent, network) = fixture()?;
            let child = LocalIdentity::generate();
            let source_node = source.peer_id()?;
            let parent_node = parent.peer_id()?;
            let child_node = child.peer_id()?;
            store.install_trust_bundle(
                &network,
                &source.sign_network_fixture(
                    2,
                    vec![
                        source.certificate(roles())?,
                        parent.certificate(roles())?,
                        child.certificate(roles())?,
                    ],
                )?,
            )?;
            let (origin, human) = start_human(&mut store, &source, &network)?;
            let human_attempt = &human.state.attempt().context("human attempt missing")?.id;
            let (parent_task, _) = store.post_agent_task(
                &source,
                &network,
                "chat",
                task_message("parent", &parent_node, &origin),
                human_attempt,
                3,
            )?;
            store.admit_trigger(
                &network,
                "chat",
                &parent_task.hash,
                &parent_node,
                &parent_node,
            )?;
            let parent_job = start_next(&mut store, &network, &parent_node)?;
            let parent_attempt = &parent_job
                .state
                .attempt()
                .context("parent attempt missing")?
                .id;
            let (child_task, _) = store.post_agent_task(
                &parent,
                &network,
                "chat",
                task_message("child", &child_node, &parent_task),
                parent_attempt,
                4,
            )?;
            store.admit_trigger(&network, "chat", &child_task.hash, &child_node, &child_node)?;
            let child_job = start_next(&mut store, &network, &child_node)?;
            let child_attempt = &child_job
                .state
                .attempt()
                .context("child attempt missing")?
                .id;
            store.finish_job(
                &source,
                &network,
                "chat",
                &source_node,
                &origin.hash,
                human_attempt,
                JobOutcome::Completed,
                final_reply(&human, "delegated"),
                5,
            )?;
            store.finish_job(
                &child,
                &network,
                "chat",
                &child_node,
                &child_task.hash,
                child_attempt,
                JobOutcome::Completed,
                final_reply(&child_job, "child result"),
                6,
            )?;
            let child_result = result_envelope(&store, &network, &child_task)?;
            store.admit_trigger(
                &network,
                "chat",
                &child_result.hash,
                &parent_node,
                &parent_node,
            )?;
            let events = if completed {
                final_reply(&parent_job, "parent result")
            } else {
                vec![]
            };
            store.finish_job(
                &parent,
                &network,
                "chat",
                &parent_node,
                &parent_task.hash,
                parent_attempt,
                outcome,
                events,
                7,
            )?;
            if unknown {
                store.fence_manager_unknown(
                    &network,
                    "chat",
                    &parent_node,
                    &parent_task.hash,
                    parent_attempt,
                )?;
            }
            let report = store.claim_next(&network, "chat", &parent_node, "test-boot", "sdk")?;
            if completed {
                assert!(report.is_none());
                assert_eq!(
                    store
                        .scheduler_job(&network, "chat", &child_result.hash, &parent_node)?
                        .context("child result admission missing")?
                        .state,
                    JobState::Cancelled {
                        attempt: None,
                        evidence: "result consumed by parent task; no separate report needed"
                            .into(),
                    }
                );
            } else {
                let report = report.context("child result was incorrectly suppressed")?;
                assert_eq!(report.job.trigger, child_result.hash);
                assert_eq!(report.job.kind, TriggerKind::AgentResult);
                assert!(
                    report
                        .job
                        .state
                        .attempt()
                        .context("child report attempt missing")?
                        .read_only
                );
            }
            assert_eq!(
                store.event(&network, &child_task.hash)?,
                Some(child_task.clone())
            );
            assert!(matches!(
                store
                    .scheduler_job(&network, "chat", &child_task.hash, &child_node)?
                    .context("child task missing")?
                    .state,
                JobState::Completed { .. }
            ));
            let child_replies = replies(&store, &network, &child_task)?;
            assert_eq!(child_replies.len(), 1);
            assert!(
                matches!(&child_replies[0].body, EventBody::AgentMessage { message }
                if message.kind == AgentMessageKind::Completed && message.text == "child result")
            );
            if !unknown {
                let parent_result = result_envelope(&store, &network, &parent_task)?;
                store.admit_trigger(
                    &network,
                    "chat",
                    &parent_result.hash,
                    &source_node,
                    &source_node,
                )?;
                let report = store
                    .claim_next(&network, "chat", &source_node, "test-boot", "sdk")?
                    .context("human source result was incorrectly suppressed")?;
                assert_eq!(report.job.trigger, parent_result.hash);
                assert_eq!(report.job.kind, TriggerKind::AgentResult);
                let attempt = report
                    .job
                    .state
                    .attempt()
                    .context("human report attempt missing")?;
                assert!(attempt.read_only);
                assert_eq!(attempt.followup_version, Some(1));
            }
            cleanup(store)?;
        }
        Ok(())
    }
}
