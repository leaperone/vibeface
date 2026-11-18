//! Freeze a task input against its assignment and the original authority snapshot.
use super::super::{
    graph::Graph,
    input::{input_from_event, resolve_with_graph, target_task, workspace_input_from},
    overview,
    query::{bot_scope, graph_at, read_graph},
    validation::valid_roots,
};
use crate::{
    scheduling::SessionScope,
    store::log,
    workspace::{Assignment, AssignmentState, InputTarget, WorkspaceInput},
};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum InputAssignmentBinding {
    Waiting {
        source_event: String,
        task_id: String,
        revision: u64,
        bot_id: String,
        owner_node: String,
        owner_epoch: u64,
        channel_owner_epoch: Option<u64>,
        resource_roots: Option<Vec<String>>,
    },
    Bound {
        source_event: String,
        assignment_id: String,
        assignment_event: String,
    },
}

fn input_requires_new_assignment_from(
    connection: &Connection,
    network: &str,
    input: &WorkspaceInput,
    assignment: &Assignment,
) -> Result<bool> {
    if !(assignment.state == AssignmentState::Proposed
        && assignment.accepted_revision.is_none()
        && assignment.adopted_revision.is_none()
        && input
            .input
            .route
            .expected_revision
            .is_some_and(|revision| revision > assignment.started_revision))
    {
        return Ok(false);
    }
    let scope = SessionScope::WorkspaceTask {
        task_id: assignment.task_id.clone(),
    }
    .storage_key();
    let direct = crate::scheduling::assignment_trigger(&assignment.assignment_id);
    let source = assignment
        .trigger_message_id
        .as_ref()
        .map(|input| crate::scheduling::message_trigger(input))
        .unwrap_or_else(|| direct.clone());
    Ok(connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM scheduler_jobs AS job
            WHERE conversation_id=?1 AND scope=?2 AND trigger IN (?3,?4)
              AND state IN ('queued','cancelled') AND attempt_id IS NULL AND tool_calls=0
              AND (runtime IS NULL OR json_extract(runtime,'$.execution')='not_started')
              AND NOT EXISTS(SELECT 1 FROM pending_checks AS pending
                  WHERE pending.conversation_id=job.conversation_id AND pending.trigger=job.trigger)
         )",
        params![
            bot_scope(connection, network, &assignment.bot_id)?,
            scope,
            direct,
            source
        ],
        |row| row.get(0),
    )?)
}

pub(super) fn input_assignment_binding_from(
    connection: &Connection,
    network: &str,
    input: &WorkspaceInput,
) -> Result<Option<InputAssignmentBinding>> {
    let binding: Option<String> = connection
        .query_row(
            "SELECT value FROM identity_meta WHERE key=?",
            [format!(
                "workspace-input-assignment:{network}:{}",
                input.input.message_id
            )],
            |row| row.get(0),
        )
        .optional()?;
    let binding = binding
        .map(|binding| serde_json::from_str::<InputAssignmentBinding>(&binding))
        .transpose()?;
    if let Some(binding) = &binding {
        let source = match binding {
            InputAssignmentBinding::Waiting { source_event, .. }
            | InputAssignmentBinding::Bound { source_event, .. } => source_event,
        };
        ensure!(
            *source == input.source_event,
            "input assignment source changed"
        );
        if let InputAssignmentBinding::Waiting {
            task_id, revision, ..
        } = binding
        {
            ensure!(
                target_task(&input.input.route.target) == Some(task_id.as_str())
                    && input.input.route.expected_revision == Some(*revision),
                "waiting input task revision changed"
            );
        }
    }
    Ok(binding)
}

fn first_assignment_after_input_from(
    connection: &Connection,
    network: &str,
    input: &WorkspaceInput,
    graph: &Graph,
) -> Result<Option<Assignment>> {
    let Some(task_id) = target_task(&input.input.route.target) else {
        return Ok(None);
    };
    let source = log::event_from(connection, network, &input.source_event)?
        .context("task input source is missing")?;
    let mut candidates = Vec::new();
    for assignment in graph.assignments.values().filter(|assignment| {
        assignment.task_id == task_id
            && Some(assignment.started_revision) == input.input.route.expected_revision
            && assignment.basis.get(&source.author).copied().unwrap_or(0) >= source.seq
    }) {
        let event = log::event_from(connection, network, &assignment.event_hash)?
            .context("input assignment event is missing")?;
        candidates.push((assignment, event));
    }
    let mut found = None;
    for (assignment, _) in &candidates {
        if candidates.iter().any(|(earlier, event)| {
            earlier.assignment_id != assignment.assignment_id
                && assignment.basis.get(&event.author).copied().unwrap_or(0) >= event.seq
        }) {
            continue;
        }
        ensure!(found.is_none(), "input has multiple original assignments");
        found = Some((*assignment).clone());
    }
    Ok(found)
}

pub(super) fn assignment_for_input_from(
    connection: &Connection,
    network: &str,
    input: &WorkspaceInput,
    graph: &Graph,
) -> Result<Option<Assignment>> {
    assignment_for_input_with_history(connection, network, input, graph, false)
}

pub(super) fn assignment_for_input_with_history(
    connection: &Connection,
    network: &str,
    input: &WorkspaceInput,
    graph: &Graph,
    bounded_history: bool,
) -> Result<Option<Assignment>> {
    let Some(task_id) = target_task(&input.input.route.target) else {
        return Ok(None);
    };
    match input_assignment_binding_from(connection, network, input)? {
        Some(InputAssignmentBinding::Bound {
            assignment_id,
            assignment_event,
            ..
        }) => {
            return Ok(graph
                .assignments
                .get(&assignment_id)
                .filter(|assignment| {
                    assignment.event_hash == assignment_event && assignment.task_id == task_id
                })
                .cloned());
        }
        Some(InputAssignmentBinding::Waiting {
            bot_id,
            owner_node,
            owner_epoch,
            channel_owner_epoch,
            ..
        }) => {
            return Ok(
                first_assignment_after_input_from(connection, network, input, graph)?.filter(
                    |assignment| {
                        assignment.bot_id == bot_id
                            && assignment.execution_node == owner_node
                            && assignment.bot_owner_epoch == owner_epoch
                            && assignment.channel_owner_epoch == channel_owner_epoch
                    },
                ),
            );
        }
        None => {}
    }
    let source = log::event_from(connection, network, &input.source_event)?
        .context("task input source is missing")?;
    let known = log::CausalClock::before(connection, network, &source)?;
    let at_input = if bounded_history {
        overview::graph_at_task(connection, network, &known, task_id)?
    } else {
        graph_at(connection, network, Some(&known))?
    };
    if let Some(task) = at_input.tasks.get(task_id) {
        if input.input.route.expected_revision != Some(task.revision) {
            return Ok(None);
        }
        if let InputTarget::ChannelTask {
            channel_id, bot_id, ..
        } = &input.input.route.target
            && (task.channel_id.as_ref() != Some(channel_id) || task.owner_bot != *bot_id)
        {
            return Ok(None);
        }
        if let Some(original) = task
            .current_assignment
            .as_ref()
            .and_then(|id| at_input.assignments.get(id))
            .filter(|assignment| {
                !matches!(
                    assignment.state,
                    AssignmentState::Completed | AssignmentState::Stopped
                )
            })
            && !input_requires_new_assignment_from(connection, network, input, original)?
        {
            return Ok(graph
                .assignments
                .get(&original.assignment_id)
                .filter(|assignment| {
                    assignment.event_hash == original.event_hash
                        && assignment.bot_id == original.bot_id
                        && assignment.execution_node == original.execution_node
                })
                .cloned());
        }
    }
    let mut found = None;
    for assignment in graph.assignments.values().filter(|assignment| {
        assignment.task_id == task_id
            && assignment.trigger_message_id.as_deref() == Some(input.input.message_id.as_str())
            && assignment.source_event.as_ref() == Some(&input.source_event)
            && Some(assignment.started_revision) == input.input.route.expected_revision
    }) {
        ensure!(found.is_none(), "input has multiple original assignments");
        found = Some(assignment.clone());
    }
    Ok(found)
}

pub(crate) fn freeze_workspace_input_assignment_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    message_id: &str,
    roots: Option<&[String]>,
) -> Result<()> {
    let Some(input) = workspace_input_from(connection, network, message_id)? else {
        return Ok(());
    };
    let Some(task_id) = target_task(&input.input.route.target) else {
        return Ok(());
    };
    ensure!(
        input.conversation_id == conversation,
        "input conversation mismatch"
    );
    if let Some(mut binding) = input_assignment_binding_from(connection, network, &input)? {
        if let InputAssignmentBinding::Waiting { resource_roots, .. } = &mut binding
            && let Some(roots) = roots
        {
            valid_roots(roots)?;
            *resource_roots = Some(roots.to_vec());
            connection.execute(
                "UPDATE identity_meta SET value=? WHERE key=?",
                params![
                    serde_json::to_string(&binding)?,
                    format!("workspace-input-assignment:{network}:{message_id}")
                ],
            )?;
        }
        return Ok(());
    }
    let graph = read_graph(connection, network)?;
    let original = assignment_for_input_from(connection, network, &input, &graph)?;
    let resolved = resolve_with_graph(connection, network, &input.input.route.target, &graph)?;
    let current = graph
        .tasks
        .get(task_id)
        .filter(|task| input.input.route.expected_revision == Some(task.revision))
        .and_then(|task| task.current_assignment.as_ref())
        .and_then(|id| graph.assignments.get(id))
        .filter(|assignment| {
            !matches!(
                assignment.state,
                AssignmentState::Completed | AssignmentState::Stopped
            )
        });
    let current = match current {
        Some(current)
            if input_requires_new_assignment_from(connection, network, &input, current)? =>
        {
            None
        }
        current => current,
    };
    let binding = if let Some(assignment) = original.as_ref().or(current) {
        ensure!(
            bot_scope(connection, network, &assignment.bot_id)? == conversation,
            "input assignment belongs to another Bot"
        );
        InputAssignmentBinding::Bound {
            source_event: input.source_event,
            assignment_id: assignment.assignment_id.clone(),
            assignment_event: assignment.event_hash.clone(),
        }
    } else {
        let revision = input
            .input
            .route
            .expected_revision
            .context("task input revision is missing")?;
        ensure!(
            resolved.task_revision == Some(revision),
            "input task revision changed"
        );
        InputAssignmentBinding::Waiting {
            source_event: input.source_event,
            task_id: task_id.into(),
            revision,
            bot_id: resolved.bot_id,
            owner_node: resolved.owner_node,
            owner_epoch: resolved.owner_epoch,
            channel_owner_epoch: resolved.channel_owner_epoch,
            resource_roots: roots.map(<[String]>::to_vec),
        }
    };
    connection.execute(
        "INSERT INTO identity_meta(key,value) VALUES(?,?)",
        params![
            format!("workspace-input-assignment:{network}:{message_id}"),
            serde_json::to_string(&binding)?
        ],
    )?;
    Ok(())
}

pub(crate) fn bind_waiting_workspace_inputs_from(
    connection: &Connection,
    network: &str,
    assignment: &Assignment,
) -> Result<()> {
    let mut statement =
        connection.prepare("SELECT key,value FROM identity_meta WHERE key>=?1 AND key<?2")?;
    let bindings = statement
        .query_map(
            [
                format!("workspace-input-assignment:{network}:"),
                format!("workspace-input-assignment:{network};"),
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let graph = read_graph(connection, network)?;
    for (key, value) in bindings {
        let InputAssignmentBinding::Waiting {
            source_event,
            task_id,
            revision,
            bot_id,
            owner_node,
            owner_epoch,
            channel_owner_epoch,
            ..
        } = serde_json::from_str(&value)?
        else {
            continue;
        };
        if task_id != assignment.task_id || revision != assignment.started_revision {
            continue;
        }
        let source = log::event_from(connection, network, &source_event)?
            .context("waiting input source is missing")?;
        let input =
            input_from_event(&source_event, source)?.context("waiting source is not an input")?;
        ensure!(
            target_task(&input.input.route.target) == Some(task_id.as_str())
                && input.input.route.expected_revision == Some(revision)
                && key
                    == format!(
                        "workspace-input-assignment:{network}:{}",
                        input.input.message_id
                    ),
            "waiting input identity changed"
        );
        let original = first_assignment_after_input_from(connection, network, &input, &graph)?;
        let Some(original) = original else {
            continue;
        };
        if original.assignment_id == assignment.assignment_id
            && (bot_id != assignment.bot_id
                || owner_node != assignment.execution_node
                || owner_epoch != assignment.bot_owner_epoch
                || channel_owner_epoch != assignment.channel_owner_epoch)
        {
            continue;
        }
        let binding = InputAssignmentBinding::Bound {
            source_event,
            assignment_id: original.assignment_id,
            assignment_event: original.event_hash,
        };
        connection.execute(
            "UPDATE identity_meta SET value=? WHERE key=? AND value=?",
            params![serde_json::to_string(&binding)?, key, value],
        )?;
    }
    Ok(())
}
