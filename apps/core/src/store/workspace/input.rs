//! Input routing, signed posting and the saved queue.
use super::{
    graph::{Graph, InputControlState, direct_parent},
    query::{bot_scope, read_graph},
    validation::{valid_id, validate_target},
};
use crate::{
    crypto::{LocalIdentity, request_key},
    model::{Attachment, Attestation, Event, EventBody, SignedEvent},
    scheduling::SessionScope,
    store::{Store, conversation_from, log, membership_from},
    workspace::{
        InputControl, InputIntent, InputRequest, InputRoute, InputTarget, QueuedInput,
        ResolvedInputTarget, TaskAction, WorkspaceCommand, WorkspaceInput,
    },
};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};

pub(super) fn target_task(target: &InputTarget) -> Option<&str> {
    match target {
        InputTarget::Task { task_id } | InputTarget::ChannelTask { task_id, .. } => Some(task_id),
        _ => None,
    }
}
pub(super) fn resolve_with_graph(
    connection: &Connection,
    network: &str,
    target: &InputTarget,
    graph: &Graph,
) -> Result<ResolvedInputTarget> {
    validate_target(target)?;
    let mut task = None;
    let mut channel = None;
    let mut work = None;
    let bot_id = match target {
        InputTarget::BotMain { bot_id } | InputTarget::Routine { bot_id, .. } => bot_id.clone(),
        InputTarget::Task { task_id } | InputTarget::ChannelTask { task_id, .. } => {
            let found = graph
                .tasks
                .get(task_id)
                .context("task not found in this network")?;
            if let InputTarget::ChannelTask {
                channel_id, bot_id, ..
            } = target
            {
                ensure!(
                    found.channel_id.as_ref() == Some(channel_id) && found.owner_bot == *bot_id,
                    "task target is outside this channel or Bot"
                );
            }
            if let Some(channel_id) = &found.channel_id {
                channel = Some(graph.channel(channel_id)?);
            }
            task = Some(found);
            found.owner_bot.clone()
        }
        InputTarget::ChannelMember { channel_id, bot_id } => {
            let found = graph.channel(channel_id)?;
            ensure!(
                found.members.contains(bot_id),
                "Bot is not a channel member"
            );
            channel = Some(found);
            bot_id.clone()
        }
        InputTarget::Work { work_id } => {
            let found = crate::store::reduce::work_from(connection, network, work_id)?
                .context("Work not found in this network")?;
            let bot = connection
                .query_row(
                    "SELECT bot_id FROM bots WHERE network_id=? AND conversation_id=?",
                    params![network, found.conversation_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .context("Work is not attributed to a Bot")?;
            work = Some(found);
            bot
        }
    };
    let scope_id = bot_scope(connection, network, &bot_id)?;
    let conversation =
        conversation_from(connection, network, &scope_id)?.context("Bot conversation not found")?;
    let owner = conversation.owner.context("Bot has no owner")?;
    let scope = if let Some(task) = task {
        SessionScope::WorkspaceTask {
            task_id: task.task_id.clone(),
        }
    } else if let Some(work) = &work {
        SessionScope::Task {
            work_id: work.work_id.clone(),
        }
    } else if let InputTarget::Routine { trigger, .. } = target {
        let (schedule_id, _) = crate::scheduling::parse_schedule_trigger(trigger)
            .context("invalid routine trigger")?;
        let schedule = crate::store::bot_settings::schedule_from(connection, network, schedule_id)?
            .context("routine not found")?;
        ensure!(
            schedule.conversation_id == scope_id,
            "routine belongs to another Bot"
        );
        SessionScope::Routine {
            trigger: trigger.clone(),
        }
    } else {
        SessionScope::Main
    };
    if let Some(channel) = channel {
        ensure!(
            channel.members.contains(&bot_id),
            "task Bot is no longer a channel member"
        );
    }
    let assignment = task
        .and_then(|task| task.current_assignment.as_ref())
        .and_then(|id| graph.assignments.get(id));
    Ok(ResolvedInputTarget {
        target: target.clone(),
        conversation_id: scope_id,
        bot_id,
        owner_node: work
            .as_ref()
            .map_or_else(|| owner.clone(), |work| work.target_node.clone()),
        owner_epoch: conversation.owner_epoch,
        scope,
        task_id: task.map(|task| task.task_id.clone()),
        task_revision: task.map(|task| task.revision),
        assignment_id: assignment.map(|assignment| assignment.assignment_id.clone()),
        channel_id: channel.map(|channel| channel.channel_id.clone()),
        channel_owner_epoch: channel.map(|channel| channel.owner_epoch),
        reply_only: task.is_some_and(|task| task.reply_only),
    })
}

pub(crate) fn resolve_input_target_from(
    connection: &Connection,
    network: &str,
    target: &InputTarget,
) -> Result<ResolvedInputTarget> {
    resolve_input_target_with_graph(connection, network, target, || {
        read_graph(connection, network).map(std::borrow::Cow::Owned)
    })
}

pub(super) fn resolve_input_target_with_graph<'a>(
    connection: &Connection,
    network: &str,
    target: &InputTarget,
    graph: impl FnOnce() -> Result<std::borrow::Cow<'a, Graph>>,
) -> Result<ResolvedInputTarget> {
    let graph = graph()?;
    let resolved = resolve_with_graph(connection, network, target, &graph)?;
    ensure!(
        !crate::store::bots::bot_deleted_from(connection, network, &resolved.conversation_id)?,
        "Bot was deleted"
    );
    Ok(resolved)
}
pub(crate) fn input_route_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    message_id: &str,
) -> Result<Option<InputRoute>> {
    let source:Option<String>=connection.query_row("SELECT event_hash FROM messages WHERE network_id=? AND conversation_id=? AND message_id=?",params![network,conversation,message_id],|row|row.get(0)).optional()?;
    let Some(source) = source else {
        return Ok(None);
    };
    Ok(
        match log::event_from(connection, network, &source)?.map(|event| event.body) {
            Some(EventBody::InputPosted {
                target,
                intent,
                sequence,
                expected_revision,
                ..
            }) => Some(InputRoute {
                target,
                intent,
                sequence,
                expected_revision,
            }),
            _ => None,
        },
    )
}
pub(super) fn input_from_event(hash: &str, event: Event) -> Result<Option<WorkspaceInput>> {
    Ok(match event.body {
        EventBody::InputPosted {
            message_id,
            target,
            intent,
            sequence,
            expected_revision,
            text,
            attachments,
            report_results,
            ..
        } => Some(WorkspaceInput {
            input: InputRequest {
                message_id,
                route: InputRoute {
                    target,
                    intent,
                    sequence,
                    expected_revision,
                },
                text,
                attachments,
                report_results: report_results
                    .context("stored input must specify report_results")?,
            },
            conversation_id: event.scope_id,
            source_event: hash.into(),
            created_at_ms: event.created_at_ms,
        }),
        _ => None,
    })
}
fn input_evidence_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    message_id: &str,
) -> Result<bool> {
    let retained: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE network_id=?1 AND kind='input_posted' AND subject=?2)
         OR EXISTS(SELECT 1 FROM scheduler_jobs WHERE conversation_id=?3 AND (message_id=?2 OR trigger=?4))
         OR EXISTS(SELECT 1 FROM identity_meta WHERE key=?5)",
        params![network, message_id, conversation, crate::scheduling::message_trigger(message_id), format!("workspace-input-admission:{network}:{message_id}")],
        |row| row.get(0),
    )?;
    if retained
        || crate::store::admission::input_delivery_evidence_from(
            connection,
            network,
            conversation,
            message_id,
        )?
    {
        return Ok(true);
    }
    let mut query = connection.prepare("SELECT envelope FROM pending_events WHERE network_id=?")?;
    for row in query.query_map([network], |row| row.get::<_, String>(0))? {
        let signed: SignedEvent = serde_json::from_str(&row?)?;
        let event: Event = serde_json::from_str(&signed.canonical_body)?;
        if matches!(event.body, EventBody::InputPosted { message_id: id, .. } if id == message_id) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn workspace_input_from(
    connection: &Connection,
    network: &str,
    message_id: &str,
) -> Result<Option<WorkspaceInput>> {
    let mut statement = connection.prepare(
        "SELECT event_hash FROM messages WHERE network_id=? AND message_id=? AND role='user'",
    )?;
    let hashes = statement
        .query_map(params![network, message_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut found = None;
    for hash in hashes {
        if let Some(event) = log::event_from(connection, network, &hash)?
            && let Some(input) = input_from_event(&hash, event)?
        {
            ensure!(found.is_none(), "input ID is ambiguous in this network");
            found = Some(input);
        }
    }
    Ok(found)
}

fn queued_input_from_control(
    connection: &Connection,
    network: &str,
    index: usize,
    control: &InputControlState,
) -> Result<QueuedInput> {
    let original = log::event_from(connection, network, &control.source_event)?
        .map(|event| input_from_event(&control.source_event, event))
        .transpose()?
        .flatten()
        .context("queue input source is missing")?;
    let mut input = original.input;
    if let Some(text) = &control.text {
        input.text = text.clone();
    }
    if let Some(attachments) = &control.attachments {
        input.attachments = attachments.clone();
    }
    if !control.deferred && input.route.intent == InputIntent::Defer {
        input.route.intent = InputIntent::Continue;
    }
    Ok(QueuedInput {
        input,
        conversation_id: original.conversation_id,
        source_event: control.source_event.clone(),
        control_event: control.control_event.clone(),
        effective_order: index as u64 + 1,
        deferred: control.deferred,
        cancelled: control.cancelled,
    })
}

pub(super) fn queued_inputs_from(
    connection: &Connection,
    network: &str,
    graph: &Graph,
) -> Result<Vec<QueuedInput>> {
    graph
        .queue_order
        .iter()
        .enumerate()
        .map(|(index, message_id)| {
            let control = graph
                .inputs
                .get(message_id)
                .context("queue input projection is missing")?;
            queued_input_from_control(connection, network, index, control)
        })
        .collect()
}
pub(crate) fn queued_workspace_input_from(
    connection: &Connection,
    network: &str,
    message_id: &str,
) -> Result<Option<QueuedInput>> {
    queued_workspace_input_with_graph(connection, network, message_id, || {
        read_graph(connection, network).map(std::borrow::Cow::Owned)
    })
}

pub(super) fn queued_workspace_input_with_graph<'a>(
    connection: &Connection,
    network: &str,
    message_id: &str,
    graph: impl FnOnce() -> Result<std::borrow::Cow<'a, Graph>>,
) -> Result<Option<QueuedInput>> {
    let graph = graph()?;
    let Some(index) = graph.queue_order.iter().position(|id| id == message_id) else {
        return Ok(None);
    };
    let control = graph
        .inputs
        .get(message_id)
        .context("queue input projection is missing")?;
    Ok(Some(queued_input_from_control(
        connection, network, index, control,
    )?))
}

impl Store {
    pub fn resolve_input_target(
        &self,
        network: &str,
        target: &InputTarget,
    ) -> Result<ResolvedInputTarget> {
        resolve_input_target_from(&self.connection, network, target)
    }
    pub fn input_route(
        &self,
        network: &str,
        conversation: &str,
        message_id: &str,
    ) -> Result<Option<InputRoute>> {
        input_route_from(&self.connection, network, conversation, message_id)
    }
    pub fn workspace_input(
        &self,
        network: &str,
        message_id: &str,
    ) -> Result<Option<WorkspaceInput>> {
        workspace_input_from(&self.connection, network, message_id)
    }
    pub(crate) fn input_evidence(
        &self,
        network: &str,
        conversation: &str,
        message_id: &str,
    ) -> Result<bool> {
        input_evidence_from(&self.connection, network, conversation, message_id)
    }
    pub fn post_workspace_input(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        input: InputRequest,
        attestation: Attestation,
        created_at_ms: i64,
    ) -> Result<WorkspaceInput> {
        valid_id(&input.message_id)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let resolved = resolve_input_target_from(&tx, network, &input.route.target)?;
        let body = crate::workspace::input_event_body(&input, Some(attestation));
        crate::crypto::verify_attestation(network, &resolved.conversation_id, &body)?;
        membership_from(&tx, network)?.client(
            &body
                .attestation()
                .context("input signature is missing")?
                .device_id,
        )?;
        if let Some(existing) = workspace_input_from(&tx, network, &input.message_id)? {
            ensure!(
                existing.input == input && existing.conversation_id == resolved.conversation_id,
                "input ID already has another signed intent"
            );
            let source = log::event_from(&tx, network, &existing.source_event)?
                .context("input event is missing")?;
            ensure!(
                request_key(network, &source.scope_id, &source.body, &source.author)?
                    == request_key(
                        network,
                        &resolved.conversation_id,
                        &body,
                        &identity.device_id()?
                    )?,
                "input ID belongs to another signer"
            );
            tx.commit()?;
            return Ok(existing);
        }
        ensure!(
            !input_evidence_from(&tx, network, &resolved.conversation_id, &input.message_id)?,
            "input has retained history or delivery evidence; reconcile it before retrying"
        );
        ensure!(
            resolved.task_revision == input.route.expected_revision,
            "input task revision changed"
        );
        if let Some(task) = resolved
            .task_id
            .as_ref()
            .and_then(|id| read_graph(&tx, network).ok()?.tasks.remove(id))
        {
            ensure!(
                !matches!(task.action, TaskAction::Stop | TaskAction::Cancel),
                "task is stopped or cancelled"
            );
        }
        let blobs = input.attachments.iter().map(Attachment::blob).collect();
        let envelope = log::append_from(
            &tx,
            &self.blob_dir,
            identity,
            network,
            &resolved.conversation_id,
            body,
            blobs,
            created_at_ms,
        )?;
        let stored = workspace_input_from(&tx, network, &input.message_id)?
            .context("input projection is missing")?;
        ensure!(
            stored.source_event == envelope.hash,
            "input ID is already used by another message"
        );
        crate::store::scheduler::queue_workspace_input_admission_from(
            &tx,
            network,
            &input.message_id,
        )?;
        tx.commit()?;
        Ok(stored)
    }
    pub fn queued_workspace_input(
        &self,
        network: &str,
        message_id: &str,
    ) -> Result<Option<QueuedInput>> {
        queued_workspace_input_from(&self.connection, network, message_id)
    }
    pub fn workspace_queue(&self, network: &str) -> Result<Vec<QueuedInput>> {
        queued_inputs_from(
            &self.connection,
            network,
            &read_graph(&self.connection, network)?,
        )
    }
}

pub(crate) fn workspace_input_release_allowed_from(
    connection: &Connection,
    network: &str,
    local: &str,
    message_id: &str,
) -> Result<bool> {
    let graph = read_graph(connection, network)?;
    let Some(input) = graph.inputs.get(message_id) else {
        return Ok(false);
    };
    if input.cancelled {
        return Ok(false);
    }
    let Some(original) = workspace_input_from(connection, network, message_id)? else {
        return Ok(false);
    };
    let resolved = resolve_with_graph(connection, network, &input.target, &graph)?;
    if resolved.owner_node != local
        || resolved.task_revision != original.input.route.expected_revision
    {
        return Ok(false);
    }
    if resolved.task_id.is_none() {
        return Ok(true);
    }
    if resolved
        .task_id
        .as_ref()
        .and_then(|id| graph.tasks.get(id))
        .is_none_or(|task| task.action != TaskAction::Continue)
    {
        return Ok(false);
    }
    if graph
        .startup_retry_assignment(message_id, local, false)?
        .is_some()
    {
        return Ok(true);
    }
    let Some(id) = graph.startup_retry_assignment(message_id, local, true)? else {
        return Ok(false);
    };
    let proof = graph
        .stopped
        .get(&id)
        .context("retry stop proof is missing")?;
    let Some(release) = log::event_from(connection, network, &input.control_event)? else {
        return Ok(false);
    };
    Ok(release.author == local
        && direct_parent(&release, &proof.event_hash)
        && direct_parent(&release, &proof.evidence_event)
        && matches!(&release.body, EventBody::WorkspaceCommanded { command_id, command: WorkspaceCommand::ControlInput { message_id: released, action: InputControl::Release, .. }, attestation: Some(_), .. }
            if released == message_id && graph.commands.get(command_id).is_some_and(|command| command.event_hash == input.control_event && command.error.is_none())))
}
