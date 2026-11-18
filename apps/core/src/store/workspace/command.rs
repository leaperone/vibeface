//! Local command transactions and commands scoped to a running Agent attempt.
use super::{
    assignment::workspace_assignment_for_job_from,
    authorization::command_in_task,
    input::{
        input_from_event, queued_inputs_from, resolve_input_target_from, workspace_input_from,
    },
    query::{bot_scope, read_graph},
    resources::{ResourceQuery, resource_conflicts_from},
    validation::valid_id,
};
use crate::{
    crypto::{LocalIdentity, request_key},
    model::{Attachment, Attestation, Cause, EventBody},
    scheduling::SessionScope,
    store::{Store, log, membership_from},
    workspace::{
        AssignmentState, InputControl, WORKSPACE_SCOPE, WorkspaceCommand, WorkspaceCommandReceipt,
    },
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::TransactionBehavior;

impl Store {
    #[expect(
        clippy::too_many_arguments,
        reason = "the signed workspace command and its authority"
    )]
    pub fn apply_workspace_command(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        command_id: &str,
        command: WorkspaceCommand,
        attestation: Option<Attestation>,
        cause: Option<Cause>,
        acting_assignment: Option<String>,
        created_at_ms: i64,
    ) -> Result<WorkspaceCommandReceipt> {
        valid_id(command_id)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let body = EventBody::WorkspaceCommanded {
            command_id: command_id.into(),
            command,
            acting_assignment,
            attestation,
            cause,
        };
        if let Some(attestation) = body.attestation() {
            membership_from(&tx, network)?.client(&attestation.device_id)?;
            crate::crypto::verify_attestation(network, WORKSPACE_SCOPE, &body)?;
        }
        let graph = read_graph(&tx, network)?;
        let key = request_key(network, WORKSPACE_SCOPE, &body, &identity.device_id()?)?;
        if !graph.commands.contains_key(command_id) {
            if let EventBody::WorkspaceCommanded {
                command:
                    WorkspaceCommand::ControlInput {
                        message_id,
                        expected_event,
                        action,
                    },
                ..
            } = &body
            {
                let original =
                    workspace_input_from(&tx, network, message_id)?.context("input not found")?;
                ensure!(
                    resolve_input_target_from(&tx, network, &original.input.route.target)?
                        .owner_node
                        == identity.device_id()?,
                    "only the target owner accepts input controls"
                );
                let current = graph
                    .inputs
                    .get(message_id)
                    .context("input projection is missing")?;
                if current.control_event != *expected_event {
                    return Err(crate::workspace::WorkspaceCommandNotSaved::new(
                        command_id,
                        "input control version changed",
                    )
                    .into());
                }
                let editable = match action {
                    InputControl::Release => {
                        crate::store::scheduler::workspace_input_releasable_from(
                            &tx,
                            network,
                            &original.conversation_id,
                            message_id,
                        )?
                    }
                    _ => crate::store::scheduler::workspace_input_editable_from(
                        &tx,
                        network,
                        &original.conversation_id,
                        message_id,
                    )?,
                };
                if !editable {
                    return Err(crate::workspace::WorkspaceCommandNotSaved::new(
                        command_id,
                        "input has started or needs reconciliation",
                    )
                    .into());
                }
            }
            if let EventBody::WorkspaceCommanded {
                command:
                    WorkspaceCommand::AssignTask {
                        execution_node,
                        resource_roots,
                        work_id,
                        ..
                    },
                ..
            } = &body
                && *execution_node == identity.device_id()?
            {
                ensure!(
                    resource_conflicts_from(
                        &tx,
                        network,
                        execution_node,
                        resource_roots,
                        None,
                        &graph,
                        ResourceQuery::Assignment(work_id.as_deref())
                    )?
                    .is_empty(),
                    "workspace root is occupied"
                );
            }
        }
        let controlled_input = match &body {
            EventBody::WorkspaceCommanded {
                command: WorkspaceCommand::ControlInput { message_id, .. },
                ..
            } => Some(message_id.clone()),
            _ => None,
        };
        let dispatch_assignment = match &body {
            EventBody::WorkspaceCommanded {
                command: WorkspaceCommand::AssignTask { assignment_id, .. },
                ..
            } => Some(assignment_id.clone()),
            _ => None,
        };
        let (previous, duplicate) = if let Some(existing) = graph.commands.get(command_id) {
            ensure!(
                existing.request_key == key,
                "command ID already has another signed intent"
            );
            if let Some(error) = &existing.error {
                bail!("{error}")
            }
            (existing.event_hash.clone(), true)
        } else {
            let blobs = match &body {
                EventBody::WorkspaceCommanded {
                    command:
                        WorkspaceCommand::PostChannelMessage { attachments, .. }
                        | WorkspaceCommand::ControlInput {
                            action: InputControl::Edit { attachments, .. },
                            ..
                        },
                    ..
                } => attachments.iter().map(Attachment::blob).collect(),
                _ => vec![],
            };
            let event = log::append_from(
                &tx,
                &self.blob_dir,
                identity,
                network,
                WORKSPACE_SCOPE,
                body,
                blobs,
                created_at_ms,
            )?;
            (event.hash, false)
        };
        let graph = read_graph(&tx, network)?;
        if let Some(error) = graph
            .commands
            .get(command_id)
            .and_then(|result| result.error.as_ref())
        {
            bail!("{error}")
        }
        if !duplicate && let Some(id) = dispatch_assignment {
            let assignment = graph
                .assignments
                .get(&id)
                .context("assignment projection missing")?;
            let local = identity.device_id()?;
            if assignment.execution_node == local {
                crate::store::scheduler::admit_assignment_from(&tx, network, &local, assignment)?;
            } else {
                let scope = bot_scope(&tx, network, &assignment.bot_id)?;
                crate::store::mailbox::queue(
                    &tx,
                    network,
                    &scope,
                    &format!("assignment:{id}"),
                    created_at_ms,
                )?;
            }
        }
        if !duplicate && let Some(message_id) = controlled_input {
            crate::store::scheduler::apply_workspace_input_control_from(
                &tx,
                network,
                &identity.device_id()?,
                &message_id,
            )?;
        }
        let inputs = queued_inputs_from(&tx, network, &graph)?;
        let receipt = WorkspaceCommandReceipt {
            inputs,
            command_id: command_id.into(),
            event_hash: previous,
            duplicate,
            channels: graph.channels.into_values().collect(),
            tasks: graph.tasks.into_values().collect(),
            assignments: graph.assignments.into_values().collect(),
        };
        tx.commit()?;
        Ok(receipt)
    }
    pub fn workspace_agent_command(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        job: &crate::scheduling::SchedulerJob,
        command_id: &str,
        command: WorkspaceCommand,
        at: i64,
    ) -> Result<WorkspaceCommandReceipt> {
        if let WorkspaceCommand::AssignmentProgress {
            execution,
            evidence_event,
            ..
        } = &command
        {
            ensure!(
                execution.is_none() && evidence_event.is_none(),
                "only Node lifecycle records execution evidence"
            );
        }
        let stored = self
            .scheduler_job(&job.conversation_id, &job.trigger)?
            .context("workspace tool has no current job")?;
        ensure!(
            stored.state.attempt() == job.state.attempt()
                && matches!(stored.state, crate::scheduling::JobState::Running { .. }),
            "workspace tool belongs to an inactive attempt"
        );
        let graph = read_graph(&self.connection, network)?;
        let acting_assignment = if matches!(job.session_scope(), SessionScope::WorkspaceTask { .. })
        {
            Some(
                workspace_assignment_for_job_from(
                    &self.connection,
                    network,
                    &identity.device_id()?,
                    job,
                )?
                .context("current job has no matching assignment")?
                .assignment_id,
            )
        } else {
            None
        };
        if let SessionScope::WorkspaceTask { task_id } = job.session_scope() {
            let task = graph
                .tasks
                .get(&task_id)
                .context("current task not found")?;
            ensure!(
                matches!(command, WorkspaceCommand::AssignmentProgress { .. }) || !task.reply_only,
                "reply-only turns cannot issue workspace commands"
            );
            ensure!(
                command_in_task(&graph, &task_id, &command),
                "workspace command is outside the current task"
            );
        }
        if let WorkspaceCommand::AssignmentProgress {
            assignment_id,
            revision,
            state,
            input_event,
            ..
        } = &command
        {
            let assignment = self
                .workspace_assignment(network, assignment_id)?
                .context("assignment not found")?;
            ensure!(
                job.session_scope()
                    == SessionScope::WorkspaceTask {
                        task_id: assignment.task_id.clone()
                    }
                    && assignment.execution_node == identity.device_id()?,
                "assignment tool is outside its task"
            );
            if *state == AssignmentState::Adopted && *revision > assignment.started_revision {
                let hash = input_event.as_ref().context("adoption needs its input")?;
                let input = log::event_from(&self.connection, network, hash)?
                    .map(|event| input_from_event(hash, event))
                    .transpose()?
                    .flatten()
                    .context("adoption input not found")?;
                ensure!(
                    self.workspace_input_accepted(
                        network,
                        &input.conversation_id,
                        &input.input.message_id
                    )?,
                    "Provider acceptance has not been proven for this revision input"
                );
            }
            self.apply_workspace_command(
                identity, network, command_id, command, None, None, None, at,
            )
        } else {
            let source = if let Some(id) = acting_assignment.as_deref() {
                let assignment = graph
                    .assignments
                    .get(id)
                    .filter(|assignment| {
                        assignment.execution_node == identity.device_id().unwrap_or_default()
                    })
                    .context("workspace tool has no active assignment")?;
                let source = assignment
                    .source_event
                    .as_ref()
                    .context("assignment has no signed input source")?;
                log::event_from(&self.connection, network, source)?
                    .map(|event| input_from_event(source, event))
                    .transpose()?
                    .flatten()
                    .context("delegated workspace commands require the original signed input")?
            } else {
                let source = self
                    .workspace_input(network, &job.reply_to)?
                    .context("workspace commands require the user's signed input")?;
                ensure!(
                    source.conversation_id == job.conversation_id,
                    "workspace tool source is outside its conversation"
                );
                source
            };
            self.apply_workspace_command(
                identity,
                network,
                command_id,
                command,
                None,
                Some(Cause {
                    conversation_id: source.conversation_id,
                    message_id: source.input.message_id,
                }),
                acting_assignment,
                at,
            )
        }
    }
}
