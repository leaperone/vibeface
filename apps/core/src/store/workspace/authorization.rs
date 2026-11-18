//! Causal authorization for workspace events and the dependencies that supply its proofs.
#[cfg(test)]
use super::{AUTHORIZATION_ANCESTORS, FRESH_MAIN_INPUTS};
use super::{
    graph::{Graph, direct_parent},
    input::{resolve_with_graph, target_task, workspace_input_from},
    query::{bot_scope, graph_at, owner_at, read_graph},
    validation::validate_target,
};
use crate::{
    crypto::request_key,
    model::{Event, EventBody, FailureKind, ReplyStatus, Role, VersionVector, WorkOutcome},
    store::{log, membership_from},
    workspace::{
        Assignment, AssignmentState, InputControl, InputIntent, InputTarget, TaskAction,
        WORKSPACE_SCOPE, WorkspaceCommand,
    },
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension as _, params};
use std::collections::BTreeSet;

fn assignment_reply_source_matches_from(
    connection: &Connection,
    network: &str,
    assignment: &Assignment,
    revision: u64,
    input: &str,
    proof: &Event,
) -> Result<bool> {
    let known = log::CausalClock::before(connection, network, proof)?;
    let at_reply = graph_at(connection, network, Some(&known))?;
    let Some(task) = at_reply.tasks.get(&assignment.task_id) else {
        return Ok(false);
    };
    if task.current_assignment.as_deref() != Some(assignment.assignment_id.as_str())
        || task.owner_bot != assignment.bot_id
        || bot_scope(connection, network, &assignment.bot_id)? != proof.scope_id
        || !at_reply
            .assignments
            .get(&assignment.assignment_id)
            .is_some_and(|original| {
                original.event_hash == assignment.event_hash
                    && original.execution_node == assignment.execution_node
                    && original.bot_id == assignment.bot_id
            })
    {
        return Ok(false);
    }
    let submitted = assignment
        .submitted_result
        .as_ref()
        .is_some_and(|submitted| submitted.revision == revision);
    if input == assignment.assignment_id {
        return Ok(revision == assignment.started_revision || submitted);
    }
    if let Some(source) = workspace_input_from(connection, network, input)? {
        return Ok(source.conversation_id == proof.scope_id
            && target_task(&source.input.route.target) == Some(assignment.task_id.as_str())
            && (source.input.route.expected_revision == Some(revision) || submitted)
            && known
                .event(connection, network, &source.source_event)?
                .is_some_and(|event| {
                    matches!(
                        &event.body,
                        EventBody::InputPosted {
                            attestation: Some(_),
                            ..
                        }
                    )
                }));
    }
    let Some(answer) = crate::store::message_from(connection, network, &proof.scope_id, input)?
        .filter(|answer| answer.role == Role::User)
    else {
        return Ok(false);
    };
    let Some(question) = answer.answers.as_deref() else {
        return Ok(false);
    };
    if !known.event(connection, network, &answer.event_hash)?.is_some_and(|event| {
        matches!(&event.body, EventBody::MessagePosted { role: Role::User, answers: Some(id), attestation: Some(_), .. } if id == question)
    }) {
        return Ok(false);
    }
    let Some(asked) = crate::store::event_about_from(
        connection,
        network,
        &proof.scope_id,
        "question_asked",
        question,
    )?
    else {
        return Ok(false);
    };
    let EventBody::QuestionAsked {
        target: Some(target),
        about,
        ..
    } = &asked.event.body
    else {
        return Ok(false);
    };
    if !known.contains(connection, network, &asked.hash)?
        || asked.event.author != assignment.execution_node
        || about.node != assignment.execution_node
        || target_task(target) != Some(assignment.task_id.as_str())
    {
        return Ok(false);
    }
    let question_known = log::CausalClock::before(connection, network, &asked.event)?;
    let at_question = graph_at(connection, network, Some(&question_known))?;
    let Some(question_task) = at_question.tasks.get(&assignment.task_id) else {
        return Ok(false);
    };
    if let InputTarget::ChannelTask {
        channel_id, bot_id, ..
    } = target
        && (question_task.channel_id.as_ref() != Some(channel_id)
            || question_task.owner_bot != *bot_id)
    {
        return Ok(false);
    }
    let Some(original) = question_task
        .current_assignment
        .as_ref()
        .and_then(|id| at_question.assignments.get(id))
    else {
        return Ok(false);
    };
    Ok(original.assignment_id == assignment.assignment_id
        && original.event_hash == assignment.event_hash
        && original.execution_node == assignment.execution_node
        && original.bot_id == assignment.bot_id
        && matches!(
            original.state,
            AssignmentState::Accepted | AssignmentState::Adopted
        )
        && (original
            .adopted_revision
            .unwrap_or(original.started_revision)
            == revision
            || submitted))
}

/// The log boundary has already checked the event shape, client attestation and membership.
/// A new BotMain input has no task/channel/assignment dependency. Without any stored input
/// for this ID, neither the causal nor current Graph can contain its input-control state.
/// Callers additionally require an unlimited, unforked author extending its current head.
/// Every non-fresh ID retains the full causal authorization path.
pub(crate) fn authorize_fresh_bot_main(
    connection: &Connection,
    network: &str,
    event: &Event,
) -> Result<bool> {
    let EventBody::InputPosted {
        message_id,
        target: InputTarget::BotMain { .. },
        expected_revision: None,
        intent: InputIntent::Continue | InputIntent::Defer,
        ..
    } = &event.body
    else {
        return Ok(false);
    };
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE network_id=?1 AND kind='input_posted' AND subject=?2)
            OR EXISTS(SELECT 1 FROM messages WHERE network_id=?1 AND conversation_id=?3 AND message_id=?2)",
        params![network,message_id,event.scope_id], |row| row.get(0),
    )?;
    if exists {
        return Ok(false);
    }
    authorize_input_target_at_graph(connection, network, event, &Graph::default())?;
    #[cfg(test)]
    FRESH_MAIN_INPUTS.with(|count| count.set(count.get() + 1));
    Ok(true)
}

fn authorize_input_target_at_graph(
    connection: &Connection,
    network: &str,
    event: &Event,
    graph: &Graph,
) -> Result<()> {
    let EventBody::InputPosted {
        target,
        expected_revision,
        intent,
        ..
    } = &event.body
    else {
        bail!("input authorization needs an input");
    };
    validate_target(target)?;
    ensure!(
        !matches!(target, InputTarget::ChannelMember { .. }),
        "channel input must first name its stable task"
    );
    ensure!(
        *intent != InputIntent::NewTask
            || matches!(
                target,
                InputTarget::Task { .. } | InputTarget::ChannelTask { .. }
            ),
        "new task input requires a stable task"
    );
    let resolved = resolve_with_graph(connection, network, target, graph)?;
    ensure!(
        resolved.conversation_id == event.scope_id,
        "input target belongs to another conversation"
    );
    ensure!(
        resolved.task_revision == *expected_revision,
        "input task revision changed"
    );
    Ok(())
}

pub(crate) fn authorize_event(connection: &Connection, network: &str, event: &Event) -> Result<()> {
    if !matches!(
        event.body,
        EventBody::WorkspaceCommanded { .. } | EventBody::InputPosted { .. }
    ) {
        return Ok(());
    }
    #[cfg(test)]
    AUTHORIZATION_ANCESTORS.with(|count| count.set(count.get() + 1));
    let known = log::CausalClock::before(connection, network, event)?;
    let mut graph = graph_at(connection, network, Some(&known))?;
    let projected = read_graph(connection, network)?;
    match &event.body {
        EventBody::InputPosted { message_id, .. } => {
            if let Some(existing) = graph
                .inputs
                .get(message_id)
                .or_else(|| projected.inputs.get(message_id))
            {
                let original = log::event_from(connection, network, &existing.source_event)?
                    .context("original input is missing")?;
                ensure!(
                    request_key(
                        network,
                        &original.scope_id,
                        &original.body,
                        &original.author
                    )? == request_key(network, &event.scope_id, &event.body, &event.author)?,
                    "input ID already has another signed intent"
                );
            }
        }
        EventBody::WorkspaceCommanded { command_id, .. } => {
            if let Some(existing) = graph
                .commands
                .get(command_id)
                .or_else(|| projected.commands.get(command_id))
            {
                ensure!(
                    existing.request_key
                        == request_key(network, &event.scope_id, &event.body, &event.author)?,
                    "command ID already has another signed intent"
                );
            }
        }
        _ => {}
    }
    match &event.body {
        EventBody::InputPosted { .. } => {
            authorize_input_target_at_graph(connection, network, event, &graph)?
        }
        EventBody::WorkspaceCommanded {
            command,
            acting_assignment,
            attestation,
            cause,
            ..
        } => {
            ensure!(
                event.scope_id == WORKSPACE_SCOPE,
                "workspace commands use the workspace scope"
            );
            if let WorkspaceCommand::AssignmentProgress {
                assignment_id,
                revision,
                state,
                input_event,
                evidence_event,
                execution,
                ..
            } = command
            {
                ensure!(
                    attestation.is_none() && cause.is_none(),
                    "assignment evidence belongs to its execution Node"
                );
                let assignment = graph
                    .assignments
                    .get(assignment_id)
                    .context("assignment not found")?;
                ensure!(
                    assignment.execution_node == event.author,
                    "only the executing Node records assignment evidence"
                );
                if *execution == Some(crate::scheduling::TurnExecution::NotStarted) {
                    ensure!(
                        *state == AssignmentState::Stopped
                            && assignment.accepted_revision.is_none()
                            && assignment.adopted_revision.is_none(),
                        "not-started evidence cannot erase execution"
                    );
                    let source = assignment
                        .source_event
                        .as_ref()
                        .context("not-started evidence has no original input")?;
                    ensure!(
                        direct_parent(event, source),
                        "not-started evidence must name its original source"
                    );
                    let original = known
                        .event(connection, network, source)?
                        .context("original task input is missing")?;
                    ensure!(
                        matches!(&original.body, EventBody::InputPosted { message_id, target, expected_revision, .. } if assignment.trigger_message_id.as_ref() == Some(message_id) && target_task(target) == Some(assignment.task_id.as_str()) && *expected_revision == Some(assignment.started_revision)),
                        "not-started evidence is outside the original task input"
                    );
                    let proof = evidence_event
                        .as_ref()
                        .context("not-started evidence needs its failed reply")?;
                    let proof = known
                        .event(connection, network, proof)?
                        .context("not-started evidence needs its failed reply")?;
                    ensure!(
                        proof.author == assignment.execution_node
                            && proof.scope_id == original.scope_id
                            && matches!(&proof.body, EventBody::MessagePosted { role: Role::Assistant, status: Some(ReplyStatus::Failed), in_reply_to, failure: Some(failure), .. } if in_reply_to.as_ref() == assignment.trigger_message_id.as_ref() && !matches!(failure, FailureKind::Unknown | FailureKind::Lost | FailureKind::Restarted)),
                        "not-started evidence is not the original failed reply"
                    );
                }
                if *state == AssignmentState::Adopted {
                    let input = input_event
                        .as_ref()
                        .context("adoption needs its signed input")?;
                    ensure!(
                        direct_parent(event, input),
                        "adopted input must be a direct dependency"
                    );
                    let input = known
                        .event(connection, network, input)?
                        .context("adopted input is missing")?;
                    let valid = match &input.body {
                        EventBody::InputPosted {
                            target,
                            expected_revision,
                            ..
                        } => {
                            target_task(target) == Some(assignment.task_id.as_str())
                                && *expected_revision == Some(*revision)
                        }
                        EventBody::WorkspaceCommanded {
                            command:
                                WorkspaceCommand::AssignTask {
                                    assignment_id: source,
                                    ..
                                },
                            ..
                        } => source == assignment_id && *revision == assignment.started_revision,
                        _ => false,
                    };
                    ensure!(valid, "adoption input is outside the assignment revision");
                }
                if *state == AssignmentState::Stopped || evidence_event.is_some() {
                    let proof = evidence_event
                        .as_ref()
                        .context("ending an assignment needs execution evidence")?;
                    ensure!(
                        direct_parent(event, proof),
                        "assignment evidence must be a direct dependency"
                    );
                    let proof = known
                        .event(connection, network, proof)?
                        .context("assignment evidence is missing")?;
                    ensure!(
                        proof.author == assignment.execution_node,
                        "assignment evidence comes from another Node"
                    );
                    match &proof.body {
                        EventBody::WorkFinished {
                            work_id, outcome, ..
                        } => {
                            ensure!(
                                assignment.work_id.as_ref() == Some(work_id),
                                "assignment evidence names another Work"
                            );
                            ensure!(
                                if *state == AssignmentState::Completed {
                                    *outcome == WorkOutcome::Completed
                                } else {
                                    matches!(
                                        outcome,
                                        WorkOutcome::Completed
                                            | WorkOutcome::Failed
                                            | WorkOutcome::Interrupted
                                            | WorkOutcome::Rejected
                                    )
                                },
                                "assignment result is not proven ended"
                            );
                        }
                        EventBody::MessagePosted {
                            role: Role::Assistant,
                            in_reply_to: Some(input),
                            status: Some(status),
                            ..
                        } => {
                            ensure!(
                                if *state == AssignmentState::Completed {
                                    *status == ReplyStatus::Completed
                                } else {
                                    matches!(
                                        status,
                                        ReplyStatus::Completed
                                            | ReplyStatus::Stopped
                                            | ReplyStatus::Failed
                                    )
                                },
                                "assignment reply is not proven ended"
                            );
                            ensure!(
                                assignment_reply_source_matches_from(
                                    connection, network, assignment, *revision, input, &proof,
                                )?,
                                "assignment reply is outside this task revision"
                            );
                        }
                        _ => bail!("invalid assignment execution evidence"),
                    }
                }
            } else if attestation.is_some() {
                ensure!(
                    cause.is_none() && acting_assignment.is_none(),
                    "human commands cannot claim a Bot cause or acting assignment"
                );
            } else {
                ensure!(
                    !matches!(
                        command,
                        WorkspaceCommand::CreateChannel { .. }
                            | WorkspaceCommand::ConfigureChannel { .. }
                            | WorkspaceCommand::ControlInput { .. }
                    ),
                    "only the user configures channels"
                );
                let cause = cause
                    .as_ref()
                    .context("Bot commands need the user's signed request")?;
                let mut source = None;
                for hash in event.prev.iter().chain(&event.deps) {
                    let parent = known
                        .event(connection, network, hash)?
                        .context("command dependency is missing")?;
                    if parent.scope_id == cause.conversation_id
                        && matches!(&parent.body, EventBody::InputPosted { message_id, attestation: Some(_), .. } if message_id == &cause.message_id)
                        && source.as_ref().is_none_or(|(prior, _)| hash < *prior)
                    {
                        source = Some((hash, parent));
                    }
                }
                let (cause_hash, source) =
                    source.context("Bot command must directly follow the user's signed request")?;
                let delegated = acting_assignment
                    .as_ref()
                    .map(|id| {
                        let assignment = graph
                            .assignments
                            .get(id)
                            .context("acting assignment is missing")?;
                        let task = graph
                            .tasks
                            .get(&assignment.task_id)
                            .context("acting task is missing")?;
                        ensure!(
                            direct_parent(event, &assignment.event_hash)
                                && assignment.execution_node == event.author
                                && assignment.source_event.as_deref() == Some(cause_hash.as_str()),
                            "acting assignment is outside the command source or Node"
                        );
                        ensure!(
                            task.current_assignment.as_ref() == Some(id)
                                && task.action == TaskAction::Continue
                                && !matches!(
                                    assignment.state,
                                    AssignmentState::Unknown
                                        | AssignmentState::Stopped
                                        | AssignmentState::Completed
                                ),
                            "acting assignment is no longer active"
                        );
                        let scope = bot_scope(connection, network, &assignment.bot_id)?;
                        let (owner, epoch) = owner_at(connection, network, &known, &scope)?;
                        ensure!(
                            epoch == assignment.bot_owner_epoch
                                && (assignment.work_id.is_some()
                                    || owner == assignment.execution_node),
                            "acting Bot owner changed"
                        );
                        if let Some(channel) = &task.channel_id {
                            let channel = graph.channel(channel)?;
                            ensure!(
                                Some(channel.owner_epoch) == assignment.channel_owner_epoch
                                    && channel.members.contains(&assignment.bot_id),
                                "acting channel permission changed"
                            );
                        }
                        Ok::<_, anyhow::Error>(assignment)
                    })
                    .transpose()?;
                let source_task = match &source.body {
                    EventBody::InputPosted { target, .. } => target_task(target),
                    _ => None,
                };
                let acting_task = delegated
                    .map(|assignment| assignment.task_id.as_str())
                    .or(source_task);
                ensure!(
                    delegated.is_some()
                        || owner_at(connection, network, &known, &cause.conversation_id)?.0
                            == event.author,
                    "only the request Bot's owner or its delegated assignment issues commands"
                );
                if let Some(task_id) = acting_task {
                    let task = graph.tasks.get(task_id).context("source task not found")?;
                    ensure!(
                        !task.reply_only,
                        "reply-only turns cannot issue workspace commands"
                    );
                    ensure!(
                        command_in_task(&graph, task_id, command),
                        "Bot command is outside its task"
                    );
                }
            }
            match command {
                WorkspaceCommand::ControlInput {
                    message_id,
                    expected_event,
                    action,
                    ..
                } => {
                    ensure!(
                        attestation.is_some() && cause.is_none(),
                        "only the user controls saved input"
                    );
                    let input = graph.inputs.get(message_id).context("input not found")?;
                    ensure!(
                        input.control_event == *expected_event
                            && direct_parent(event, expected_event),
                        "input control version changed"
                    );
                    let source = known
                        .event(connection, network, &input.source_event)?
                        .context("input control source is missing")?;
                    let owner = match &input.target {
                        InputTarget::Work { work_id } => {
                            crate::store::reduce::work_from(connection, network, work_id)?
                                .context("controlled Work is missing")?
                                .target_node
                        }
                        _ => owner_at(connection, network, &known, &source.scope_id)?.0,
                    };
                    ensure!(
                        owner == event.author,
                        "only the target owner accepts input controls"
                    );
                    if *action == InputControl::Release {
                        let ended = graph.assignments.values().any(|assignment| {
                            assignment.trigger_message_id.as_deref() == Some(message_id)
                                && assignment.source_event.as_ref() == Some(&input.source_event)
                                && matches!(
                                    assignment.state,
                                    AssignmentState::Stopped
                                        | AssignmentState::Unknown
                                        | AssignmentState::Completed
                                )
                        });
                        ensure!(
                            !ended || graph.releasable_assignment(message_id, event)?.is_some(),
                            "input has no proven startup failure to release"
                        );
                    }
                }
                WorkspaceCommand::CreateChannel {
                    members,
                    coordinator_node,
                    ..
                }
                | WorkspaceCommand::ConfigureChannel {
                    members,
                    coordinator_node,
                    ..
                } => {
                    membership_from(connection, network)?.node(coordinator_node)?;
                    for bot in members {
                        bot_scope(connection, network, bot)?;
                    }
                }
                WorkspaceCommand::CreateTask {
                    owner_bot,
                    source_events,
                    ..
                } => {
                    bot_scope(connection, network, owner_bot)?;
                    for hash in source_events {
                        ensure!(
                            direct_parent(event, hash)
                                && known.contains(connection, network, hash)?,
                            "task source must be a known direct dependency"
                        );
                        if attestation.is_none() {
                            let source = known
                                .event(connection, network, hash)?
                                .context("task source is missing")?;
                            let acting = cause.as_ref().expect("validated cause");
                            let cause_input =
                                workspace_input_from(connection, network, &acting.message_id)?;
                            let acting_task = acting_assignment
                                .as_ref()
                                .and_then(|id| graph.assignments.get(id))
                                .map(|assignment| assignment.task_id.as_str())
                                .or_else(|| {
                                    cause_input
                                        .as_ref()
                                        .and_then(|input| target_task(&input.input.route.target))
                                });
                            let allowed = match &source.body {
                                EventBody::InputPosted {
                                    message_id, target, ..
                                } => {
                                    source.scope_id == acting.conversation_id
                                        && (message_id == &acting.message_id
                                            || target_task(target) == acting_task)
                                }
                                EventBody::WorkspaceCommanded { command, .. } => {
                                    command.task_id().is_some_and(|id| Some(id) == acting_task)
                                }
                                EventBody::MessagePosted {
                                    role: Role::Assistant,
                                    in_reply_to: Some(input),
                                    ..
                                } => workspace_input_from(connection, network, input)?.is_some_and(
                                    |input| target_task(&input.input.route.target) == acting_task,
                                ),
                                _ => false,
                            };
                            ensure!(allowed, "Bot task source is outside its authorized scope");
                        }
                    }
                }
                WorkspaceCommand::AssignTask {
                    bot_id,
                    execution_node,
                    bot_owner_epoch,
                    work_id,
                    ..
                } => {
                    membership_from(connection, network)?.node(execution_node)?;
                    let scope = bot_scope(connection, network, bot_id)?;
                    let (owner, epoch) = owner_at(connection, network, &known, &scope)?;
                    ensure!(epoch == *bot_owner_epoch, "Bot owner epoch changed");
                    if let Some(work_id) = work_id {
                        let work = crate::store::reduce::work_from(connection, network, work_id)?
                            .context("assignment Work not found")?;
                        ensure!(
                            work.conversation_id == scope && work.target_node == *execution_node,
                            "assignment Work belongs to another target"
                        );
                    } else {
                        ensure!(
                            owner == *execution_node,
                            "a coordination assignment runs on its Bot's owner"
                        );
                    }
                }
                WorkspaceCommand::PostChannelMessage { .. }
                | WorkspaceCommand::ReviseTask { .. }
                | WorkspaceCommand::ControlTask { .. }
                | WorkspaceCommand::AssignmentProgress { .. } => {}
            }
            // Use the same reducer that records this command during projection.
            graph.apply_command(
                "pending",
                event,
                command,
                cause.as_ref(),
                VersionVector::new(),
            )?;
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn command_in_task(graph: &Graph, task_id: &str, command: &WorkspaceCommand) -> bool {
    let Some(task) = graph.tasks.get(task_id) else {
        return false;
    };
    match command {
        WorkspaceCommand::CreateTask {
            parent_task_id,
            channel_id,
            ..
        } => parent_task_id.as_deref() == Some(task_id) && *channel_id == task.channel_id,
        WorkspaceCommand::AssignmentProgress { assignment_id, .. } => graph
            .assignments
            .get(assignment_id)
            .is_some_and(|assignment| assignment.task_id == task_id),
        WorkspaceCommand::PostChannelMessage {
            channel_id,
            task_id: target,
            ..
        } => {
            target.as_deref() == Some(task_id)
                && task.channel_id.as_deref() == Some(channel_id.as_str())
        }
        WorkspaceCommand::ReviseTask {
            task_id: target, ..
        }
        | WorkspaceCommand::ControlTask {
            task_id: target, ..
        }
        | WorkspaceCommand::AssignTask {
            task_id: target, ..
        } => {
            target == task_id
                || graph
                    .tasks
                    .get(target)
                    .is_some_and(|task| task.parent_task_id.as_deref() == Some(task_id))
        }
        _ => false,
    }
}

pub(crate) fn additional_dependencies(
    connection: &Connection,
    network: &str,
    body: &EventBody,
) -> Result<Vec<String>> {
    if !matches!(
        body,
        EventBody::InputPosted { .. }
            | EventBody::WorkspaceCommanded { .. }
            | EventBody::MessagePosted {
                in_reply_to: Some(_),
                role: Role::Assistant,
                ..
            }
            | EventBody::MessageSteered { .. }
    ) {
        return Ok(Vec::new());
    }
    let graph = read_graph(connection, network)?;
    let mut hashes = BTreeSet::new();
    let mut scopes = BTreeSet::new();
    fn add_task(graph: &Graph, hashes: &mut BTreeSet<String>, task_id: &str) {
        if let Some(task) = graph.tasks.get(task_id) {
            hashes.insert(task.request_event.clone());
            hashes.insert(task.event_hash.clone());
        }
    }
    match body {
        EventBody::InputPosted { target, .. } => {
            if let Some(task) = target_task(target) {
                add_task(&graph, &mut hashes, task);
                if let Some(assignment) = graph
                    .tasks
                    .get(task)
                    .and_then(|task| task.current_assignment.as_ref())
                    .and_then(|id| graph.assignments.get(id))
                {
                    hashes.insert(assignment.event_hash.clone());
                    let head: Option<String> = connection.query_row(
                        "SELECT hash FROM events WHERE network_id=? AND scope_id=? AND author=? AND effective=1 ORDER BY seq DESC LIMIT 1",
                        params![network, WORKSPACE_SCOPE, assignment.execution_node],
                        |row| row.get(0),
                    ).optional()?;
                    hashes.extend(head);
                }
            }
            let resolved = resolve_with_graph(connection, network, target, &graph)?;
            scopes.insert(resolved.conversation_id);
            if let Some(channel) = resolved.channel_id.and_then(|id| graph.channels.get(&id)) {
                hashes.insert(channel.event_hash.clone());
            }
        }
        EventBody::WorkspaceCommanded {
            command,
            cause,
            acting_assignment,
            ..
        } => {
            if let Some(id) = acting_assignment {
                let actor = graph
                    .assignments
                    .get(id)
                    .context("acting assignment is missing")?;
                hashes.insert(actor.event_hash.clone());
                scopes.insert(bot_scope(connection, network, &actor.bot_id)?);
            }
            if let Some(task) = command.task_id() {
                add_task(&graph, &mut hashes, task);
            }
            match command {
                WorkspaceCommand::ControlInput {
                    message_id,
                    expected_event,
                    ..
                } => {
                    hashes.insert(expected_event.clone());
                    let input = workspace_input_from(connection, network, message_id)?
                        .context("input not found")?;
                    hashes.insert(input.source_event.clone());
                    for assignment in graph.assignments.values().filter(|assignment| {
                        assignment.trigger_message_id.as_deref() == Some(message_id)
                            && assignment.source_event.as_ref() == Some(&input.source_event)
                    }) {
                        if let Some(proof) = graph.stopped.get(&assignment.assignment_id) {
                            hashes.insert(proof.event_hash.clone());
                            hashes.insert(proof.evidence_event.clone());
                        }
                    }
                    scopes.insert(input.conversation_id);
                }
                WorkspaceCommand::CreateChannel { members, .. }
                | WorkspaceCommand::ConfigureChannel { members, .. } => {
                    for bot in members {
                        scopes.insert(bot_scope(connection, network, bot)?);
                    }
                }
                WorkspaceCommand::CreateTask {
                    owner_bot,
                    parent_task_id,
                    dependencies,
                    source_events,
                    ..
                } => {
                    if let Some(task) = parent_task_id {
                        add_task(&graph, &mut hashes, task);
                    }
                    for task in dependencies {
                        add_task(&graph, &mut hashes, task);
                    }
                    hashes.extend(source_events.iter().cloned());
                    scopes.insert(bot_scope(connection, network, owner_bot)?);
                }
                WorkspaceCommand::ReviseTask { dependencies, .. } => {
                    for task in dependencies {
                        add_task(&graph, &mut hashes, task);
                    }
                }
                WorkspaceCommand::AssignTask {
                    bot_id, work_id, ..
                } => {
                    scopes.insert(bot_scope(connection, network, bot_id)?);
                    if let Some(work) = work_id {
                        let mut statement=connection.prepare("SELECT hash FROM events WHERE network_id=? AND subject=? AND kind IN ('work_dispatched','work_progress','work_finished') AND effective=1")?;
                        hashes.extend(
                            statement
                                .query_map(params![network, work], |row| row.get::<_, String>(0))?
                                .collect::<rusqlite::Result<Vec<_>>>()?,
                        );
                    }
                }
                WorkspaceCommand::AssignmentProgress {
                    assignment_id,
                    input_event,
                    evidence_event,
                    ..
                } => {
                    if let Some(assignment) = graph.assignments.get(assignment_id) {
                        hashes.insert(assignment.event_hash.clone());
                        hashes.extend(assignment.source_event.iter().cloned());
                    }
                    hashes.extend(input_event.iter().chain(evidence_event.iter()).cloned());
                }
                _ => {}
            }
            if let Some(channel) = command.channel_id().and_then(|id| graph.channels.get(id)) {
                hashes.insert(channel.event_hash.clone());
            }
            if let Some(cause) = cause {
                let message = crate::store::message_from(
                    connection,
                    network,
                    &cause.conversation_id,
                    &cause.message_id,
                )?
                .context("command cause is missing")?;
                hashes.insert(message.event_hash);
                scopes.insert(cause.conversation_id.clone());
            }
        }
        EventBody::MessagePosted {
            in_reply_to: Some(input),
            role: Role::Assistant,
            ..
        } => {
            let source = workspace_input_from(connection, network, input)?;
            if let Some(source) = source {
                hashes.insert(source.source_event);
                scopes.insert(source.conversation_id);
            } else if let Some(assignment) = graph.assignments.get(input) {
                hashes.insert(assignment.event_hash.clone());
                if let Some(source) = &assignment.source_event {
                    hashes.insert(source.clone());
                }
                scopes.insert(bot_scope(connection, network, &assignment.bot_id)?);
            }
        }
        EventBody::MessageSteered { message_id, reply } => {
            if let Some(source) = workspace_input_from(connection, network, message_id)? {
                hashes.insert(source.source_event);
                scopes.insert(source.conversation_id.clone());
                if let Some(reply) =
                    crate::store::message_from(connection, network, &source.conversation_id, reply)?
                {
                    hashes.insert(reply.event_hash);
                }
            }
        }
        _ => return Ok(Vec::new()),
    }
    for scope in scopes {
        let bot: Option<String> = connection
            .query_row(
                "SELECT event_hash FROM bots WHERE network_id=? AND conversation_id=?",
                params![network, scope],
                |row| row.get(0),
            )
            .optional()?;
        hashes.extend(bot);
        let mut statement=connection.prepare("SELECT hash FROM events WHERE network_id=? AND scope_id=? AND kind='conversation_owner' AND effective=1")?;
        hashes.extend(
            statement
                .query_map(params![network, scope], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    Ok(hashes.into_iter().collect())
}
