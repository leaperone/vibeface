//! Assignment selection, admission, execution evidence and settlement.
mod binding;

use super::{
    graph::Graph,
    input::{resolve_with_graph, target_task, workspace_input_from},
    overview,
    query::{bot_scope, graph_at, owner_at, read_graph},
    resources::{ResourceQuery, resource_conflicts_from},
    validation::valid_roots,
};
use crate::{
    crypto::LocalIdentity,
    model::{
        Cause, Event, EventBody, EventHash, FailureKind, ReplyStatus, Role, SignedEvent,
        WorkOutcome,
    },
    scheduling::{SessionScope, TriggerKind},
    store::{Store, conversation_from, log, membership_from},
    workspace::{
        Assignment, AssignmentState, InputTarget, TaskAction, TaskState, WorkspaceCommand,
    },
};
use anyhow::{Context as _, Result, ensure};
use binding::{
    InputAssignmentBinding, assignment_for_input_from, assignment_for_input_with_history,
    input_assignment_binding_from,
};
pub(crate) use binding::{
    bind_waiting_workspace_inputs_from, freeze_workspace_input_assignment_from,
};
use rusqlite::{Connection, OptionalExtension as _, params};
use std::collections::BTreeSet;

impl Store {
    pub fn ensure_workspace_assignment(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        task_id: &str,
        roots: &[String],
        at: i64,
    ) -> Result<Option<Assignment>> {
        let task = self
            .workspace_task(network, task_id)?
            .context("task not found")?;
        if task.action != TaskAction::Continue
            || task.dependencies.iter().any(|id| {
                self.workspace_task(network, id)
                    .ok()
                    .flatten()
                    .is_none_or(|task| task.state != TaskState::Completed)
            })
        {
            return Ok(None);
        }
        if let Some(existing) = task
            .current_assignment
            .as_ref()
            .map(|id| self.workspace_assignment(network, id))
            .transpose()?
            .flatten()
        {
            if existing.state == AssignmentState::Proposed
                && existing.started_revision < task.revision
            {
                let scope = bot_scope(&self.connection, network, &existing.bot_id)?;
                let (trigger, reply_to) = existing.trigger_message_id.as_ref().map_or_else(
                    || {
                        (
                            crate::scheduling::assignment_trigger(&existing.assignment_id),
                            existing.assignment_id.clone(),
                        )
                    },
                    |input| (crate::scheduling::message_trigger(input), input.clone()),
                );
                let cancelled = self.scheduler_job(&scope, &trigger)?.filter(|job| {
                    matches!(
                        job.state,
                        crate::scheduling::JobState::Cancelled { attempt: None, .. }
                    ) && !job.tool_calls
                        && !job.pending_check
                        && job.runtime.as_ref().is_none_or(|runtime| {
                            runtime.execution == crate::scheduling::TurnExecution::NotStarted
                        })
                });
                let proof = cancelled
                    .as_ref()
                    .map(|job| self.message(network, &scope, &job.message_id))
                    .transpose()?
                    .flatten()
                    .filter(|reply| {
                        reply.status == Some(ReplyStatus::Stopped)
                            && reply.in_reply_to.as_deref() == Some(reply_to.as_str())
                            && reply.author == existing.execution_node
                    });
                if let Some(proof) = proof {
                    self.record_assignment_state(
                        identity,
                        network,
                        &existing.assignment_id,
                        existing.started_revision,
                        AssignmentState::Stopped,
                        None,
                        Some(proof.event_hash),
                        Some(proof.text),
                        at,
                    )?;
                } else {
                    return Ok(Some(existing));
                }
            } else if !matches!(
                existing.state,
                AssignmentState::Completed | AssignmentState::Stopped
            ) {
                return Ok(Some(existing));
            }
        }
        let graph = read_graph(&self.connection, network)?;
        let used_inputs: BTreeSet<_> = graph
            .assignments
            .values()
            .filter(|assignment| assignment.task_id == task_id)
            .filter_map(|assignment| assignment.trigger_message_id.clone())
            .collect();
        let mut inputs = self.workspace_queue(network)?;
        inputs.retain(|input| {
            target_task(&input.input.route.target) == Some(task_id)
                && input.input.route.expected_revision == Some(task.revision)
                && !input.deferred
                && !input.cancelled
                && !used_inputs.contains(&input.input.message_id)
        });
        let mut eligible = Vec::new();
        for input in inputs {
            let original =
                workspace_input_from(&self.connection, network, &input.input.message_id)?
                    .context("task input is missing")?;
            if assignment_for_input_from(&self.connection, network, &original, &graph)?.is_some() {
                continue;
            }
            if let Some(InputAssignmentBinding::Waiting {
                bot_id,
                owner_node,
                owner_epoch,
                channel_owner_epoch,
                ..
            }) = input_assignment_binding_from(&self.connection, network, &original)?
            {
                let resolved = self.resolve_input_target(network, &original.input.route.target)?;
                if resolved.bot_id != bot_id
                    || resolved.owner_node != owner_node
                    || resolved.owner_epoch != owner_epoch
                    || resolved.channel_owner_epoch != channel_owner_epoch
                {
                    continue;
                }
            }
            let job = self.scheduler_job(
                &input.conversation_id,
                &crate::scheduling::message_trigger(&input.input.message_id),
            )?;
            if job.is_none_or(|job| {
                job.state == crate::scheduling::JobState::Queued
                    && job.runtime.is_none()
                    && !job.tool_calls
                    && !job.pending_check
            }) {
                eligible.push(input);
            }
        }
        let Some(input) = eligible.into_iter().next() else {
            return Ok(None);
        };
        let resolved = self.resolve_input_target(network, &input.input.route.target)?;
        ensure!(
            resolved.owner_node == identity.device_id()?,
            "only the task owner creates its execution assignment"
        );
        membership_from(&self.connection, network)?.node(&resolved.owner_node)?;
        ensure!(
            !task.reply_only,
            "reply-only turns cannot issue workspace commands"
        );
        valid_roots(roots)?;
        if !resource_conflicts_from(
            &self.connection,
            network,
            &resolved.owner_node,
            roots,
            None,
            &graph,
            ResourceQuery::Assignment(None),
        )?
        .is_empty()
        {
            let original =
                workspace_input_from(&self.connection, network, &input.input.message_id)?
                    .context("task input is missing")?;
            if input_assignment_binding_from(&self.connection, network, &original)?.is_some() {
                freeze_workspace_input_assignment_from(
                    &self.connection,
                    network,
                    &input.conversation_id,
                    &input.input.message_id,
                    Some(roots),
                )?;
            }
            return Ok(None);
        }
        let assignment_id = format!(
            "assignment-{}",
            crate::crypto::digest(
                format!("{task_id}:{}:{}", task.revision, input.source_event).as_bytes()
            )
        );
        let command = WorkspaceCommand::AssignTask {
            task_id: task_id.into(),
            expected_revision: task.revision,
            assignment_id: assignment_id.clone(),
            bot_id: task.owner_bot,
            execution_node: resolved.owner_node,
            bot_owner_epoch: resolved.owner_epoch,
            channel_owner_epoch: resolved.channel_owner_epoch,
            work_id: None,
            resource_roots: roots.to_vec(),
            automatic: true,
        };
        self.apply_workspace_command(
            identity,
            network,
            &assignment_id,
            command,
            None,
            Some(Cause {
                conversation_id: input.conversation_id,
                message_id: input.input.message_id,
            }),
            None,
            at,
        )?;
        self.workspace_assignment(network, &assignment_id)
    }
    pub fn settle_workspace_assignment(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        job: &crate::scheduling::SchedulerJob,
        at: i64,
    ) -> Result<()> {
        use crate::scheduling::{JobState, TurnExecution};
        let Some(recorded) = self.scheduler_job(&job.conversation_id, &job.trigger)? else {
            return Ok(());
        };
        if recorded.scope != job.scope
            || recorded.reply_to != job.reply_to
            || recorded.message_id != job.message_id
            || recorded.state.attempt().map(|attempt| attempt.id.as_str())
                != job.state.attempt().map(|attempt| attempt.id.as_str())
        {
            return Ok(());
        }
        let job = &recorded;
        if matches!(
            job.state,
            JobState::Steering { .. } | JobState::Steered { .. }
        ) {
            return Ok(());
        }
        let SessionScope::WorkspaceTask { task_id } = job.session_scope() else {
            return Ok(());
        };
        let local = identity.device_id()?;
        let Some(mut assignment) =
            workspace_assignment_for_job_from(&self.connection, network, &local, job)?
        else {
            return Ok(());
        };
        if matches!(
            assignment.state,
            AssignmentState::Completed | AssignmentState::Stopped
        ) {
            return Ok(());
        }
        let id = assignment.assignment_id.clone();
        let (initial_revision, adoption_source) = if let Some(source) = self
            .workspace_input(network, &job.reply_to)?
            .filter(|input| {
                input.conversation_id == job.conversation_id
                    && target_task(&input.input.route.target) == Some(task_id.as_str())
            }) {
            (
                source
                    .input
                    .route
                    .expected_revision
                    .context("task input lacks revision")?,
                source.source_event,
            )
        } else {
            (assignment.started_revision, assignment.event_hash.clone())
        };
        let revision = assignment
            .submitted_result
            .as_ref()
            .map_or(initial_revision, |submitted| submitted.revision);
        let evidence = self.message(network, &job.conversation_id, &job.message_id)?;
        let terminal = evidence.as_ref().filter(|reply| {
            let ended = matches!(
                reply.status,
                Some(ReplyStatus::Completed | ReplyStatus::Stopped)
            ) || (reply.status == Some(ReplyStatus::Failed)
                && job
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.execution != TurnExecution::Unknown)
                && !matches!(
                    reply.failure,
                    Some(FailureKind::Unknown | FailureKind::Lost | FailureKind::Restarted)
                ));
            ended
                && reply.in_reply_to.as_deref() == Some(job.reply_to.as_str())
                && reply.author == identity.device_id().unwrap_or_default()
        });
        let has_started = job
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.execution == TurnExecution::Started)
            || terminal.is_some_and(|reply| reply.status == Some(ReplyStatus::Completed));
        if (assignment.state == AssignmentState::Proposed
            || (assignment.state == AssignmentState::Unknown
                && assignment.accepted_revision.is_none()
                && terminal.is_some()))
            && has_started
        {
            self.record_assignment_state(
                identity,
                network,
                &id,
                initial_revision,
                AssignmentState::Accepted,
                None,
                terminal.map(|reply| reply.event_hash.clone()),
                None,
                at,
            )?;
            assignment = self
                .workspace_assignment(network, &id)?
                .context("assignment missing")?;
        }
        if initial_revision == assignment.started_revision
            && has_started
            && assignment.adopted_revision.is_none()
        {
            self.record_assignment_state(
                identity,
                network,
                &id,
                initial_revision,
                AssignmentState::Adopted,
                Some(adoption_source),
                terminal.map(|reply| reply.event_hash.clone()),
                None,
                at,
            )?;
            assignment = self
                .workspace_assignment(network, &id)?
                .context("assignment missing")?;
        }
        if terminal.is_none()
            && (matches!(job.state, JobState::Reconciling { .. })
                || (job.state.terminal()
                    && job
                        .runtime
                        .as_ref()
                        .is_some_and(|runtime| runtime.execution == TurnExecution::Unknown)))
        {
            if assignment.state != AssignmentState::Unknown {
                self.record_assignment_state(
                    identity,
                    network,
                    &id,
                    revision,
                    AssignmentState::Unknown,
                    None,
                    None,
                    None,
                    at,
                )?;
            }
            return Ok(());
        }
        if !job.state.terminal() {
            return Ok(());
        }
        if let Some(reply) = terminal {
            let Some(task) = self.workspace_task(network, &task_id)? else {
                return Ok(());
            };
            if task.current_assignment.as_deref() != Some(id.as_str())
                || crate::store::scheduler::workspace_assignment_followups_pending_from(
                    &self.connection,
                    network,
                    &local,
                    &assignment,
                    job,
                )?
            {
                return Ok(());
            }
            let state = if reply.status == Some(ReplyStatus::Completed)
                && task.action == TaskAction::Continue
            {
                AssignmentState::Completed
            } else {
                AssignmentState::Stopped
            };
            if state == AssignmentState::Completed && assignment.adopted_revision != Some(revision)
            {
                return Ok(());
            }
            if state == AssignmentState::Completed
                && self.workspace_task(network, &task_id)?.is_some_and(|task| {
                    task.dependencies.iter().any(|id| {
                        self.workspace_task(network, id)
                            .ok()
                            .flatten()
                            .is_none_or(|task| task.state != TaskState::Completed)
                    })
                })
            {
                return Ok(());
            }
            self.record_assignment_state(
                identity,
                network,
                &id,
                revision,
                state,
                None,
                Some(reply.event_hash.clone()),
                Some(
                    assignment
                        .submitted_result
                        .as_ref()
                        .filter(|submitted| submitted.revision == revision)
                        .map_or_else(|| reply.text.clone(), |submitted| submitted.text.clone()),
                ),
                at,
            )?;
        } else if !matches!(job.state, JobState::Queued)
            && assignment.state != AssignmentState::Unknown
        {
            self.record_assignment_state(
                identity,
                network,
                &id,
                revision,
                AssignmentState::Unknown,
                None,
                None,
                None,
                at,
            )?;
        }
        Ok(())
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "the assignment evidence and its source references"
    )]
    fn record_assignment_state(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        assignment_id: &str,
        revision: u64,
        state: AssignmentState,
        input_event: Option<String>,
        evidence_event: Option<String>,
        result: Option<String>,
        at: i64,
    ) -> Result<()> {
        let execution = if state == AssignmentState::Stopped {
            let assignment = self
                .workspace_assignment(network, assignment_id)?
                .context("assignment not found")?;
            if let Some(source) = &assignment.trigger_message_id {
                let scope = bot_scope(&self.connection, network, &assignment.bot_id)?;
                let job =
                    self.scheduler_job(&scope, &crate::scheduling::message_trigger(source))?;
                let outbox: bool = self.connection.query_row("SELECT EXISTS(SELECT 1 FROM scheduler_outbox WHERE conversation_id=? AND trigger=?)", params![scope, crate::scheduling::message_trigger(source)], |row| row.get(0))?;
                let original = job
                    .as_ref()
                    .map(|job| {
                        workspace_assignment_for_job_from(
                            &self.connection,
                            network,
                            &identity.device_id()?,
                            job,
                        )
                    })
                    .transpose()?
                    .flatten();
                if !outbox
                    && original
                        .as_ref()
                        .is_some_and(|original| original.assignment_id == assignment.assignment_id)
                    && assignment.accepted_revision.is_none()
                    && assignment.adopted_revision.is_none()
                    && job.as_ref().is_some_and(|job| {
                        job.retryable()
                            && job.runtime.as_ref().is_some_and(|runtime| {
                                runtime.execution == crate::scheduling::TurnExecution::NotStarted
                            })
                    })
                {
                    Some(crate::scheduling::TurnExecution::NotStarted)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        let command = WorkspaceCommand::AssignmentProgress {
            execution,
            assignment_id: assignment_id.into(),
            revision,
            state,
            input_event,
            evidence_event,
            result,
        };
        let encoded = serde_json::to_vec(&command)?;
        let command_id = format!("assignment-state-{}", crate::crypto::digest(&encoded));
        self.apply_workspace_command(
            identity,
            network,
            &command_id,
            command,
            None,
            None,
            None,
            at,
        )?;
        Ok(())
    }
    pub fn settle_workspace_work_assignment(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        snapshot: &Assignment,
        at: i64,
    ) -> Result<()> {
        let Some(mut assignment) = self.workspace_assignment(network, &snapshot.assignment_id)?
        else {
            return Ok(());
        };
        if assignment.execution_node != identity.device_id()?
            || matches!(
                assignment.state,
                AssignmentState::Completed | AssignmentState::Stopped
            )
        {
            return Ok(());
        }
        let Some(work_id) = assignment.work_id.as_deref() else {
            return Ok(());
        };
        let Some(work) = self.work(network, work_id)? else {
            return Ok(());
        };
        ensure!(
            work.target_node == assignment.execution_node,
            "assignment Work belongs to another Node"
        );
        let journal = self.work_record(work_id)?;
        if work.outcome == Some(WorkOutcome::Unverified)
            || (work.outcome.is_none()
                && journal.as_ref().is_some_and(|record| {
                    matches!(record.state, crate::work::WorkState::Unverified { .. })
                }))
        {
            if assignment.state != AssignmentState::Unknown {
                self.record_assignment_state(
                    identity,
                    network,
                    &assignment.assignment_id,
                    assignment.started_revision,
                    AssignmentState::Unknown,
                    None,
                    None,
                    None,
                    at,
                )?;
            }
            return Ok(());
        }
        if work.outcome.is_none() {
            let started = journal.as_ref().is_some_and(|record| {
                matches!(
                    record.state,
                    crate::work::WorkState::Running { .. }
                        | crate::work::WorkState::Interrupting { .. }
                )
            });
            if assignment.state == AssignmentState::Proposed && started {
                self.record_assignment_state(
                    identity,
                    network,
                    &assignment.assignment_id,
                    assignment.started_revision,
                    AssignmentState::Accepted,
                    None,
                    None,
                    None,
                    at,
                )?;
            }
            return Ok(());
        }
        let proof:Option<String>=self.connection.query_row("SELECT hash FROM events WHERE network_id=? AND subject=? AND kind='work_finished' AND author=? AND effective=1 ORDER BY lamport DESC,created_at_ms DESC,hash DESC LIMIT 1",params![network,work_id,assignment.execution_node],|row|row.get(0)).optional()?;
        let Some(proof) = proof else { return Ok(()) };
        let terminal = log::event_from(&self.connection, network, &proof)?
            .context("Work result evidence is missing")?;
        let EventBody::WorkFinished { outcome, .. } = terminal.body else {
            return Ok(());
        };
        if outcome == WorkOutcome::Unverified {
            return Ok(());
        }
        assignment = self
            .workspace_assignment(network, &assignment.assignment_id)?
            .context("assignment missing")?;
        let revision = assignment
            .submitted_result
            .as_ref()
            .map_or(assignment.started_revision, |submitted| submitted.revision);
        let state =
            if outcome == WorkOutcome::Completed && assignment.adopted_revision == Some(revision) {
                AssignmentState::Completed
            } else {
                AssignmentState::Stopped
            };
        let result = assignment
            .submitted_result
            .as_ref()
            .filter(|submitted| submitted.revision == revision)
            .map_or_else(
                || work.result_text.clone(),
                |submitted| submitted.text.clone(),
            );
        self.record_assignment_state(
            identity,
            network,
            &assignment.assignment_id,
            revision,
            state,
            None,
            Some(proof),
            Some(result),
            at,
        )
    }
}

pub(crate) fn task_execution_allowed_from(
    connection: &Connection,
    network: &str,
    task_id: &str,
) -> Result<bool> {
    task_execution_allowed_with_graph(connection, network, task_id, || {
        read_graph(connection, network).map(std::borrow::Cow::Owned)
    })
}

pub(super) fn task_execution_allowed_with_graph<'a>(
    connection: &Connection,
    network: &str,
    task_id: &str,
    graph: impl FnOnce() -> Result<std::borrow::Cow<'a, Graph>>,
) -> Result<bool> {
    let graph = graph()?;
    let task = graph.tasks.get(task_id).context("task not found")?;
    if task.action != TaskAction::Continue
        || task.dependencies.iter().any(|id| {
            graph
                .tasks
                .get(id)
                .is_none_or(|task| task.state != TaskState::Completed)
        })
    {
        return Ok(false);
    }
    match task
        .current_assignment
        .as_ref()
        .and_then(|id| graph.assignments.get(id))
    {
        Some(assignment) => Ok(assignment_snapshot_allowed_from(
            connection, network, &graph, assignment,
        )? && (task.revision == assignment.started_revision
            || assignment.adopted_revision == Some(task.revision))),
        None => Ok(true),
    }
}

fn assignment_snapshot_allowed_from(
    connection: &Connection,
    network: &str,
    graph: &Graph,
    assignment: &Assignment,
) -> Result<bool> {
    let Some(task) = graph.tasks.get(&assignment.task_id) else {
        return Ok(false);
    };
    if task.current_assignment.as_deref() != Some(assignment.assignment_id.as_str())
        || task.owner_bot != assignment.bot_id
        || task.action != TaskAction::Continue
        || matches!(
            assignment.state,
            AssignmentState::Unknown | AssignmentState::Stopped | AssignmentState::Completed
        )
        || membership_from(connection, network)?
            .node(&assignment.execution_node)
            .is_err()
    {
        return Ok(false);
    }
    let scope = bot_scope(connection, network, &assignment.bot_id)?;
    if crate::store::bots::bot_deleted_from(connection, network, &scope)? {
        return Ok(false);
    }
    let Some(conversation) = conversation_from(connection, network, &scope)? else {
        return Ok(false);
    };
    if conversation.owner_epoch != assignment.bot_owner_epoch
        || (assignment.work_id.is_none()
            && conversation.owner.as_deref() != Some(assignment.execution_node.as_str()))
    {
        return Ok(false);
    }
    if let Some(channel) = &task.channel_id {
        let Some(channel) = graph.channels.get(channel) else {
            return Ok(false);
        };
        if Some(channel.owner_epoch) != assignment.channel_owner_epoch
            || !channel.members.contains(&assignment.bot_id)
            || membership_from(connection, network)?
                .node(&channel.coordinator_node)
                .is_err()
        {
            return Ok(false);
        }
    } else if assignment.channel_owner_epoch.is_some() {
        return Ok(false);
    }
    let Some(source) = &assignment.source_event else {
        return Ok(false);
    };
    for hash in [&assignment.event_hash, source] {
        if connection
            .query_row(
                "SELECT 1 FROM events WHERE network_id=? AND hash=? AND effective=1",
                params![network, hash],
                |_| Ok(()),
            )
            .optional()?
            .is_none()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn task_input_allowed_from(
    connection: &Connection,
    network: &str,
    task_id: &str,
) -> Result<bool> {
    let graph = read_graph(connection, network)?;
    let task = graph.tasks.get(task_id).context("task not found")?;
    if task.action != TaskAction::Continue {
        return Ok(false);
    }
    match task
        .current_assignment
        .as_ref()
        .and_then(|id| graph.assignments.get(id))
    {
        Some(assignment) => {
            assignment_snapshot_allowed_from(connection, network, &graph, assignment)
        }
        None => Ok(true),
    }
}

pub(crate) fn assignment_work_allowed_from(
    connection: &Connection,
    network: &str,
    assignment_id: &str,
    dispatcher: &str,
    conversation: &str,
) -> Result<bool> {
    let graph = read_graph(connection, network)?;
    let Some(assignment) = graph.assignments.get(assignment_id) else {
        return Ok(false);
    };
    let Some(task) = graph.tasks.get(&assignment.task_id) else {
        return Ok(false);
    };
    Ok(
        assignment_snapshot_allowed_from(connection, network, &graph, assignment)?
            && assignment.execution_node == dispatcher
            && bot_scope(connection, network, &assignment.bot_id)? == conversation
            && graph.work_ready(assignment, task),
    )
}

fn assignment_proof_events(
    connection: &Connection,
    network: &str,
    graph: &Graph,
    assignment: &Assignment,
    revision: u64,
) -> Result<Vec<(String, Event)>> {
    let mut statement = connection.prepare("SELECT hash,envelope FROM events WHERE network_id=? AND effective=1 AND kind='workspace_commanded' ORDER BY lamport,created_at_ms,hash")?;
    let rows = statement.query_map([network], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut proofs = Vec::new();
    for row in rows {
        let (hash, envelope) = row?;
        let envelope: SignedEvent = serde_json::from_str(&envelope)?;
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        let EventBody::WorkspaceCommanded {
            command_id,
            command,
            ..
        } = &event.body
        else {
            continue;
        };
        if !graph
            .commands
            .get(command_id)
            .is_some_and(|result| result.error.is_none() && result.event_hash == hash)
        {
            continue;
        }
        let relevant = match command {
            WorkspaceCommand::CreateTask { task_id, .. } => {
                task_id == &assignment.task_id && revision == 1
            }
            WorkspaceCommand::ReviseTask {
                task_id,
                expected_revision,
                ..
            } => {
                task_id == &assignment.task_id && expected_revision.checked_add(1) == Some(revision)
            }
            WorkspaceCommand::AssignmentProgress {
                assignment_id,
                state,
                revision: adopted,
                ..
            } => {
                assignment_id == &assignment.assignment_id
                    && (matches!(state, AssignmentState::Accepted)
                        || (matches!(state, AssignmentState::Adopted | AssignmentState::Completed)
                            && *adopted == revision))
            }
            _ => false,
        };
        if relevant {
            proofs.push((hash, event));
        }
    }
    Ok(proofs)
}

pub(crate) fn assignment_work_source_from(
    connection: &Connection,
    network: &str,
    assignment_id: &str,
) -> Result<(u64, Vec<EventHash>)> {
    let graph = read_graph(connection, network)?;
    let assignment = graph
        .assignments
        .get(assignment_id)
        .context("assignment not found")?;
    let task = graph
        .tasks
        .get(&assignment.task_id)
        .context("task not found")?;
    let scope = bot_scope(connection, network, &assignment.bot_id)?;
    ensure!(
        assignment_work_allowed_from(
            connection,
            network,
            assignment_id,
            &assignment.execution_node,
            &scope
        )?,
        "assignment may not dispatch Work"
    );
    let mut refs = BTreeSet::from([
        assignment.event_hash.clone(),
        task.event_hash.clone(),
        assignment
            .source_event
            .clone()
            .context("assignment source is missing")?,
    ]);
    fn add_proofs(
        connection: &Connection,
        network: &str,
        graph: &Graph,
        assignment: &Assignment,
        revision: u64,
        refs: &mut BTreeSet<String>,
    ) -> Result<()> {
        refs.insert(assignment.event_hash.clone());
        refs.extend(assignment.source_event.iter().cloned());
        for (hash, event) in
            assignment_proof_events(connection, network, graph, assignment, revision)?
        {
            refs.insert(hash);
            if let EventBody::WorkspaceCommanded {
                command:
                    WorkspaceCommand::AssignmentProgress {
                        input_event,
                        evidence_event,
                        ..
                    },
                ..
            } = event.body
            {
                refs.extend(input_event);
                refs.extend(evidence_event);
            }
        }
        Ok(())
    }
    add_proofs(
        connection,
        network,
        &graph,
        assignment,
        task.revision,
        &mut refs,
    )?;
    let mut dependencies = task.dependencies.clone();
    let mut seen = BTreeSet::new();
    while let Some(id) = dependencies.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let dependency = graph.tasks.get(&id).context("required task is missing")?;
        ensure!(
            dependency.state == TaskState::Completed,
            "required task results are incomplete"
        );
        refs.insert(dependency.event_hash.clone());
        let assignment = dependency
            .current_assignment
            .as_ref()
            .and_then(|id| graph.assignments.get(id))
            .context("dependency completion has no assignment")?;
        add_proofs(
            connection,
            network,
            &graph,
            assignment,
            dependency.revision,
            &mut refs,
        )?;
        dependencies.extend(dependency.dependencies.iter().cloned());
    }
    if let Some(channel) = task
        .channel_id
        .as_ref()
        .and_then(|id| graph.channels.get(id))
    {
        refs.insert(channel.event_hash.clone());
    }
    let mut statement = connection.prepare("SELECT hash FROM events WHERE network_id=? AND scope_id=? AND kind='conversation_owner' AND effective=1")?;
    refs.extend(
        statement
            .query_map(params![network, scope], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    );
    let refs: Vec<_> = refs.into_iter().collect();
    ensure!(
        refs.len() <= 1024
            && assignment_work_source_matches_from(
                connection,
                network,
                assignment_id,
                task.revision,
                &refs
            )?,
        "assignment Work source is incomplete"
    );
    Ok((task.revision, refs))
}

pub(crate) fn assignment_work_source_matches_from(
    connection: &Connection,
    network: &str,
    assignment_id: &str,
    revision: u64,
    refs: &[EventHash],
) -> Result<bool> {
    if revision == 0
        || refs.is_empty()
        || refs.len() > 1024
        || refs.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Ok(false);
    }
    let mut known = log::CausalClock::default();
    for hash in refs {
        if connection
            .query_row(
                "SELECT 1 FROM events WHERE network_id=? AND hash=? AND effective=1",
                params![network, hash],
                |_| Ok(()),
            )
            .optional()?
            .is_none()
        {
            return Ok(false);
        }
        known.include(connection, network, hash)?;
    }
    let graph = graph_at(connection, network, Some(&known))?;
    let Some(assignment) = graph.assignments.get(assignment_id) else {
        return Ok(false);
    };
    let Some(task) = graph.tasks.get(&assignment.task_id) else {
        return Ok(false);
    };
    if task.revision != revision
        || task.current_assignment.as_deref() != Some(assignment_id)
        || !graph.work_ready(assignment, task)
        || !refs.contains(&assignment.event_hash)
        || !assignment
            .source_event
            .as_ref()
            .is_some_and(|source| refs.contains(source))
    {
        return Ok(false);
    }
    let scope = bot_scope(connection, network, &assignment.bot_id)?;
    let (owner, epoch) = owner_at(connection, network, &known, &scope)?;
    if epoch != assignment.bot_owner_epoch
        || (assignment.work_id.is_none() && owner != assignment.execution_node)
    {
        return Ok(false);
    }
    if let Some(channel) = &task.channel_id {
        let Some(channel) = graph.channels.get(channel) else {
            return Ok(false);
        };
        if Some(channel.owner_epoch) != assignment.channel_owner_epoch
            || !channel.members.contains(&assignment.bot_id)
        {
            return Ok(false);
        }
    } else if assignment.channel_owner_epoch.is_some() {
        return Ok(false);
    }
    let proofs = assignment_proof_events(connection, network, &graph, assignment, revision)?;
    let revision_proven = proofs.iter().any(|(hash, event)| {
        refs.contains(hash)
            && matches!(
                &event.body,
                EventBody::WorkspaceCommanded {
                    command: WorkspaceCommand::CreateTask { .. }
                        | WorkspaceCommand::ReviseTask { .. },
                    ..
                }
            )
    });
    let adoption_proven = revision == assignment.started_revision || proofs.iter().any(|(hash,event)| refs.contains(hash)
        && matches!(&event.body, EventBody::WorkspaceCommanded { command: WorkspaceCommand::AssignmentProgress { state: AssignmentState::Adopted, input_event: Some(input), .. }, .. } if refs.contains(input)));
    Ok(revision_proven && adoption_proven)
}

pub(crate) fn assignment_admission_allowed_from(
    connection: &Connection,
    network: &str,
    assignment: &Assignment,
) -> Result<bool> {
    let graph = read_graph(connection, network)?;
    let Some(current) = graph.assignments.get(&assignment.assignment_id) else {
        return Ok(false);
    };
    let Some(task) = graph.tasks.get(&assignment.task_id) else {
        return Ok(false);
    };
    if current.event_hash != assignment.event_hash
        || task.current_assignment.as_deref() != Some(assignment.assignment_id.as_str())
        || task.revision != assignment.started_revision
        || task.action != TaskAction::Continue
        || matches!(
            current.state,
            AssignmentState::Unknown | AssignmentState::Completed | AssignmentState::Stopped
        )
    {
        return Ok(false);
    }
    let scope = bot_scope(connection, network, &assignment.bot_id)?;
    let conversation =
        conversation_from(connection, network, &scope)?.context("assignment Bot is missing")?;
    if conversation.owner_epoch != assignment.bot_owner_epoch
        || (assignment.work_id.is_none()
            && conversation.owner.as_deref() != Some(assignment.execution_node.as_str()))
        || membership_from(connection, network)?
            .node(&assignment.execution_node)
            .is_err()
    {
        return Ok(false);
    }
    if let Some(channel) = &task.channel_id {
        let channel = graph.channel(channel)?;
        if Some(channel.owner_epoch) != assignment.channel_owner_epoch
            || !channel.members.contains(&assignment.bot_id)
        {
            return Ok(false);
        }
    } else if assignment.channel_owner_epoch.is_some() {
        return Ok(false);
    }
    let source = assignment
        .source_event
        .as_ref()
        .context("assignment has no authorization source")?;
    Ok(connection
        .query_row(
            "SELECT 1 FROM events WHERE network_id=? AND hash=? AND effective=1",
            params![network, source],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(crate) fn workspace_input_resources_blocked_from(
    connection: &Connection,
    network: &str,
    local: &str,
    job: &crate::scheduling::SchedulerJob,
) -> Result<bool> {
    workspace_input_resources_blocked_with_graph(connection, network, local, job, || {
        read_graph(connection, network).map(std::borrow::Cow::Owned)
    })
}

pub(super) fn workspace_input_resources_blocked_with_graph<'a>(
    connection: &Connection,
    network: &str,
    local: &str,
    job: &crate::scheduling::SchedulerJob,
    graph: impl FnOnce() -> Result<std::borrow::Cow<'a, Graph>>,
) -> Result<bool> {
    let Some(input) = workspace_input_from(connection, network, &job.reply_to)? else {
        return Ok(false);
    };
    let Some(InputAssignmentBinding::Waiting {
        revision,
        bot_id,
        owner_node,
        owner_epoch,
        channel_owner_epoch,
        resource_roots: Some(roots),
        ..
    }) = input_assignment_binding_from(connection, network, &input)?
    else {
        return Ok(false);
    };
    let graph = graph()?;
    let resolved = resolve_with_graph(connection, network, &input.input.route.target, &graph)?;
    if input.conversation_id != job.conversation_id
        || resolved.task_revision != Some(revision)
        || resolved.bot_id != bot_id
        || resolved.owner_node != owner_node
        || owner_node != local
        || resolved.owner_epoch != owner_epoch
        || resolved.channel_owner_epoch != channel_owner_epoch
    {
        return Ok(false);
    }
    Ok(!resource_conflicts_from(
        connection,
        network,
        local,
        &roots,
        None,
        &graph,
        ResourceQuery::Assignment(None),
    )?
    .is_empty())
}

pub(crate) fn workspace_assignment_for_job_from(
    connection: &Connection,
    network: &str,
    local: &str,
    job: &crate::scheduling::SchedulerJob,
) -> Result<Option<Assignment>> {
    workspace_assignment_for_job_with_graph(
        connection,
        network,
        local,
        job,
        || read_graph(connection, network).map(std::borrow::Cow::Owned),
        false,
    )
}

pub(super) fn workspace_assignment_for_job_with_graph<'a>(
    connection: &Connection,
    network: &str,
    local: &str,
    job: &crate::scheduling::SchedulerJob,
    graph: impl FnOnce() -> Result<std::borrow::Cow<'a, Graph>>,
    bounded_history: bool,
) -> Result<Option<Assignment>> {
    let SessionScope::WorkspaceTask { task_id } = job.session_scope() else {
        return Ok(None);
    };
    if job.kind == TriggerKind::WorkResult {
        let Ok(crate::scheduling::Trigger::Work(work_id)) =
            crate::scheduling::Trigger::decode(&job.trigger)
        else {
            return Ok(None);
        };
        let (conversation, dispatcher, trigger) =
            if let Some(work) = crate::store::reduce::work_from(connection, network, work_id)? {
                (work.conversation_id, work.dispatcher, work.trigger)
            } else if let Some(dispatch) = crate::store::work::dispatch_from(connection, work_id)?
                && dispatch.state.ended()
            {
                (
                    dispatch.spec.conversation,
                    dispatch.spec.dispatcher,
                    dispatch.spec.trigger,
                )
            } else {
                return Ok(None);
            };
        if conversation != job.conversation_id || dispatcher != local {
            return Ok(None);
        }
        let (reply_to, kind) = match crate::scheduling::Trigger::decode(&trigger) {
            Ok(crate::scheduling::Trigger::Assignment(id)) => (id.to_owned(), TriggerKind::Human),
            Ok(crate::scheduling::Trigger::Message(id)) => {
                let Some(message) =
                    crate::store::message_from(connection, network, &conversation, id)?
                        .filter(|message| message.role == Role::User)
                else {
                    return Ok(None);
                };
                let kind = if message.answers.is_some() {
                    TriggerKind::Answer
                } else {
                    TriggerKind::Human
                };
                (id.to_owned(), kind)
            }
            _ => return Ok(None),
        };
        let mut origin = job.clone();
        origin.trigger = trigger;
        origin.reply_to = reply_to;
        origin.kind = kind;
        return workspace_assignment_for_job_with_graph(
            connection,
            network,
            local,
            &origin,
            graph,
            bounded_history,
        );
    }
    let graph = graph()?;
    if let Ok(crate::scheduling::Trigger::Assignment(id)) =
        crate::scheduling::Trigger::decode(&job.trigger)
    {
        let Some(assignment) = graph.assignments.get(id) else {
            return Ok(None);
        };
        return Ok((assignment.assignment_id == job.reply_to
            && assignment.task_id == task_id
            && assignment.execution_node == local
            && bot_scope(connection, network, &assignment.bot_id)? == job.conversation_id)
            .then(|| assignment.clone()));
    }
    if job.trigger != crate::scheduling::message_trigger(&job.reply_to) {
        return Ok(None);
    }
    if job.kind == crate::scheduling::TriggerKind::Answer {
        let Some(answer) =
            crate::store::message_from(connection, network, &job.conversation_id, &job.reply_to)?
        else {
            return Ok(None);
        };
        let Some(question) = answer
            .answers
            .as_deref()
            .filter(|_| answer.role == Role::User)
        else {
            return Ok(None);
        };
        let Some(asked) = crate::store::event_about_from(
            connection,
            network,
            &job.conversation_id,
            "question_asked",
            question,
        )?
        else {
            return Ok(None);
        };
        let EventBody::QuestionAsked {
            target: Some(target),
            about,
            ..
        } = &asked.event.body
        else {
            return Ok(None);
        };
        if asked.event.author != local
            || about.node != local
            || target_task(target) != Some(task_id.as_str())
            || job.basis.get(&asked.event.author).copied().unwrap_or(0) < asked.event.seq
        {
            return Ok(None);
        }
        let known = log::CausalClock::before(connection, network, &asked.event)?;
        let at_question = if bounded_history {
            overview::graph_at_task(connection, network, &known, &task_id)?
        } else {
            graph_at(connection, network, Some(&known))?
        };
        let Some(task) = at_question.tasks.get(&task_id) else {
            return Ok(None);
        };
        if let InputTarget::ChannelTask {
            channel_id, bot_id, ..
        } = target
            && (task.channel_id.as_ref() != Some(channel_id) || task.owner_bot != *bot_id)
        {
            return Ok(None);
        }
        let Some(original) = task
            .current_assignment
            .as_ref()
            .and_then(|id| at_question.assignments.get(id))
        else {
            return Ok(None);
        };
        if original.execution_node != local
            || original.bot_id != task.owner_bot
            || !matches!(
                original.state,
                AssignmentState::Accepted | AssignmentState::Adopted
            )
            || bot_scope(connection, network, &original.bot_id)? != job.conversation_id
        {
            return Ok(None);
        }
        let trigger = original.trigger_message_id.as_ref().map_or_else(
            || crate::scheduling::assignment_trigger(&original.assignment_id),
            |input| crate::scheduling::message_trigger(input),
        );
        let native: Option<String> = connection.query_row("SELECT t.thread_id FROM manager_turns t JOIN scheduler_jobs j ON j.attempt_id=t.attempt_id WHERE j.conversation_id=? AND j.trigger=?", params![job.conversation_id,trigger], |row| row.get(0)).optional()?;
        if native.is_some_and(|thread| thread != about.session_id) {
            return Ok(None);
        }
        return Ok(graph
            .assignments
            .get(&original.assignment_id)
            .filter(|current| {
                current.event_hash == original.event_hash
                    && current.execution_node == local
                    && current.bot_id == original.bot_id
            })
            .cloned());
    }
    let Some(input) = workspace_input_from(connection, network, &job.reply_to)? else {
        return Ok(None);
    };
    if input.conversation_id != job.conversation_id
        || target_task(&input.input.route.target) != Some(task_id.as_str())
    {
        return Ok(None);
    }
    let source = log::event_from(connection, network, &input.source_event)?
        .context("task input source is missing")?;
    if job.basis.get(&source.author).copied().unwrap_or(0) < source.seq {
        return Ok(None);
    }
    let assignment =
        assignment_for_input_with_history(connection, network, &input, &graph, bounded_history)?;
    Ok(assignment.filter(|assignment| {
        assignment.execution_node == local
            && bot_scope(connection, network, &assignment.bot_id)
                .is_ok_and(|scope| scope == job.conversation_id)
    }))
}
