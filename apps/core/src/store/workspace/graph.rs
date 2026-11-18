//! In-memory reduction of signed workspace history.
use super::validation::{overlap, validate_command};
use crate::{
    crypto::request_key,
    model::{Attachment, Cause, Event, EventBody, VersionVector},
    workspace::{
        Assignment, AssignmentState, AssignmentSubmission, Channel, InputControl, InputIntent,
        InputTarget, MAX_AUTOMATIC_ASSIGNMENTS, MAX_MEMBER_REPLIES, MAX_TASK_DEPTH, TaskAction,
        TaskState, WorkspaceCommand, WorkspaceTask,
    },
};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct Graph {
    pub(super) channels: BTreeMap<String, Channel>,
    pub(super) tasks: BTreeMap<String, WorkspaceTask>,
    pub(super) assignments: BTreeMap<String, Assignment>,
    pub(super) commands: BTreeMap<String, CommandResult>,
    pub(super) inputs: BTreeMap<String, InputControlState>,
    pub(super) queue_order: Vec<String>,
    pub(super) last_rank: String,
    #[serde(skip)]
    pub(super) dirty_tasks: BTreeSet<String>,
    #[serde(skip)]
    pub(super) dirty_assignments: BTreeSet<String>,
    #[serde(skip)]
    pub(super) replace_lookups: bool,
    #[serde(default)]
    pub(super) stopped: BTreeMap<String, AssignmentStopProof>,
    #[serde(default)]
    pub(super) retry_blocked: BTreeSet<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CommandResult {
    pub(super) event_hash: String,
    pub(super) request_key: String,
    pub(super) error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct InputControlState {
    pub(super) source_event: String,
    pub(super) control_event: String,
    pub(super) target: InputTarget,
    sequence: u64,
    pub(super) deferred: bool,
    pub(super) cancelled: bool,
    pub(super) text: Option<String>,
    pub(super) attachments: Option<Vec<Attachment>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct AssignmentStopProof {
    pub(super) event_hash: String,
    pub(super) evidence_event: String,
    source_event: String,
    revision: u64,
}

impl Graph {
    fn task(&self, task_id: &str, expected: u64) -> Result<&WorkspaceTask> {
        let task = self.tasks.get(task_id).context("task not found")?;
        ensure!(task.revision == expected, "task revision changed");
        Ok(task)
    }
    pub(super) fn channel(&self, channel_id: &str) -> Result<&Channel> {
        self.channels.get(channel_id).context("channel not found")
    }
    pub(super) fn work_ready(&self, assignment: &Assignment, task: &WorkspaceTask) -> bool {
        task.action == TaskAction::Continue
            && !task.reply_only
            && matches!(
                assignment.state,
                AssignmentState::Accepted | AssignmentState::Adopted
            )
            && (task.revision == assignment.started_revision
                || assignment.adopted_revision == Some(task.revision))
            && task.dependencies.iter().all(|id| {
                self.tasks
                    .get(id)
                    .is_some_and(|task| task.state == TaskState::Completed)
            })
            && (!assignment.automatic
                || self
                    .assignments
                    .values()
                    .filter(|other| {
                        other.automatic
                            && self
                                .tasks
                                .get(&other.task_id)
                                .is_some_and(|other| other.root_task_id == task.root_task_id)
                    })
                    .count()
                    <= MAX_AUTOMATIC_ASSIGNMENTS)
    }
    fn dependencies(&self, task_id: &str, dependencies: &[String]) -> Result<()> {
        for dependency in dependencies {
            ensure!(dependency != task_id, "a task cannot depend on itself");
            ensure!(
                self.tasks.contains_key(dependency),
                "dependency task not found"
            );
            let mut pending = vec![dependency.as_str()];
            let mut visited = BTreeSet::new();
            while let Some(id) = pending.pop() {
                ensure!(id != task_id, "task dependencies contain a cycle");
                if visited.insert(id)
                    && let Some(task) = self.tasks.get(id)
                {
                    pending.extend(task.dependencies.iter().map(String::as_str));
                }
            }
        }
        Ok(())
    }
    pub(super) fn startup_retry_assignment(
        &self,
        message_id: &str,
        owner: &str,
        rearmed: bool,
    ) -> Result<Option<String>> {
        let Some(input) = self.inputs.get(message_id) else {
            return Ok(None);
        };
        let mut found = None;
        for assignment in self.assignments.values().filter(|assignment| {
            assignment.trigger_message_id.as_deref() == Some(message_id)
                && assignment.source_event.as_ref() == Some(&input.source_event)
        }) {
            let Some(proof) = self.stopped.get(&assignment.assignment_id) else {
                continue;
            };
            let Some(task) = self.tasks.get(&assignment.task_id) else {
                continue;
            };
            if !(assignment.state == AssignmentState::Stopped
                || (rearmed && assignment.state == AssignmentState::Proposed))
                || self.retry_blocked.contains(&assignment.assignment_id)
                || assignment.execution_node != owner
                || assignment.accepted_revision.is_some()
                || assignment.adopted_revision.is_some()
                || assignment.completed_revision.is_some()
                || assignment.submitted_result.is_some()
                || task.current_assignment.as_deref() != Some(assignment.assignment_id.as_str())
                || task.action != TaskAction::Continue
                || task.revision != assignment.started_revision
                || proof.revision != assignment.started_revision
                || proof.source_event != input.source_event
            {
                continue;
            }
            ensure!(found.is_none(), "input has multiple retry assignments");
            found = Some(assignment.assignment_id.clone());
        }
        Ok(found)
    }
    pub(super) fn releasable_assignment(
        &self,
        message_id: &str,
        event: &Event,
    ) -> Result<Option<String>> {
        let Some(id) = self.startup_retry_assignment(message_id, &event.author, false)? else {
            return Ok(None);
        };
        let proof = self
            .stopped
            .get(&id)
            .context("retry stop proof is missing")?;
        Ok(
            (direct_parent(event, &proof.event_hash)
                && direct_parent(event, &proof.evidence_event))
            .then_some(id),
        )
    }
    fn refresh(&mut self) {
        for task in self.tasks.values_mut() {
            task.waiting_reason = None;
            let assignment = task
                .current_assignment
                .as_ref()
                .and_then(|id| self.assignments.get(id));
            task.state = match assignment {
                Some(assignment) if assignment.state == AssignmentState::Unknown => {
                    TaskState::NeedsReconciliation
                }
                Some(assignment)
                    if matches!(task.action, TaskAction::Stop | TaskAction::Cancel)
                        && !matches!(
                            assignment.state,
                            AssignmentState::Stopped | AssignmentState::Completed
                        ) =>
                {
                    TaskState::StopPending
                }
                _ if task.action == TaskAction::Cancel => TaskState::Cancelled,
                _ if task.action == TaskAction::Stop => TaskState::NotStarted,
                _ if task.action == TaskAction::Defer => TaskState::Deferred,
                Some(assignment)
                    if assignment.state == AssignmentState::Completed
                        && assignment.completed_revision == Some(task.revision) =>
                {
                    TaskState::Completed
                }
                Some(assignment)
                    if assignment.started_revision < task.revision
                        && assignment.adopted_revision != Some(task.revision) =>
                {
                    TaskState::UpdatePending
                }
                Some(assignment)
                    if !matches!(
                        assignment.state,
                        AssignmentState::Proposed
                            | AssignmentState::Stopped
                            | AssignmentState::Completed
                    ) =>
                {
                    TaskState::Running
                }
                _ => TaskState::NotStarted,
            };
            if task.state == TaskState::NotStarted
                && assignment.is_some_and(|assignment| {
                    assignment.work_id.is_some()
                        && assignment.state == AssignmentState::Stopped
                        && assignment.adopted_revision.is_none()
                })
            {
                task.state = TaskState::WaitingUser;
                task.waiting_reason = Some("assignment_not_adopted".into());
            }
        }
        fn complete(
            id: &str,
            tasks: &BTreeMap<String, WorkspaceTask>,
            visited: &mut BTreeSet<String>,
        ) -> bool {
            if !visited.insert(id.into()) {
                return false;
            }
            let result = tasks.get(id).is_some_and(|task| {
                task.state == TaskState::Completed
                    && task
                        .dependencies
                        .iter()
                        .all(|id| complete(id, tasks, visited))
            });
            visited.remove(id);
            result
        }
        let completed: BTreeSet<_> = self
            .tasks
            .keys()
            .filter(|id| complete(id, &self.tasks, &mut BTreeSet::new()))
            .cloned()
            .collect();
        let task_roots: BTreeMap<_, _> = self
            .tasks
            .values()
            .map(|task| (task.task_id.clone(), task.root_task_id.clone()))
            .collect();
        for task in self.tasks.values_mut() {
            if matches!(
                task.state,
                TaskState::NotStarted | TaskState::Running | TaskState::Completed
            ) && task.dependencies.iter().any(|id| !completed.contains(id))
            {
                task.state = TaskState::WaitingDependencies;
                task.waiting_reason = Some("dependencies_incomplete".into());
            }
            if matches!(task.state, TaskState::NotStarted)
                && self
                    .assignments
                    .values()
                    .filter(|assignment| {
                        assignment.automatic
                            && task_roots.get(&assignment.task_id) == Some(&task.root_task_id)
                    })
                    .count()
                    >= MAX_AUTOMATIC_ASSIGNMENTS
            {
                task.state = TaskState::WaitingUser;
                task.waiting_reason = Some("dispatch_budget".into());
            }
        }
    }
    pub(super) fn apply_command(
        &mut self,
        hash: &str,
        event: &Event,
        command: &WorkspaceCommand,
        cause: Option<&Cause>,
        basis: VersionVector,
    ) -> Result<()> {
        validate_command(command)?;
        match command {
            WorkspaceCommand::ControlInput {
                message_id,
                expected_event,
                action,
            } => {
                let input = self.inputs.get(message_id).context("input not found")?;
                ensure!(
                    input.control_event == *expected_event,
                    "input control version changed"
                );
                ensure!(!input.cancelled, "input was cancelled");
                if let InputControl::Move {
                    before_message_id: Some(before),
                } = action
                {
                    let other = self
                        .inputs
                        .get(before)
                        .context("queue destination not found")?;
                    ensure!(
                        before != message_id && other.target == input.target && !other.cancelled,
                        "queue destination is outside the target"
                    );
                }
                let retry = if *action == InputControl::Release {
                    self.releasable_assignment(message_id, event)?
                } else {
                    None
                };
                let input = self.inputs.get_mut(message_id).expect("validated input");
                match action {
                    InputControl::Release => input.deferred = false,
                    InputControl::Cancel => input.cancelled = true,
                    InputControl::Edit { text, attachments } => {
                        input.text = Some(text.clone());
                        input.attachments = Some(attachments.clone());
                    }
                    InputControl::Move { .. } => {}
                }
                input.control_event = hash.into();
                if let Some(id) = retry {
                    let assignment = self
                        .assignments
                        .get_mut(&id)
                        .expect("validated retry assignment");
                    assignment.state = AssignmentState::Proposed;
                    assignment.result = None;
                }
                if let InputControl::Move { before_message_id } = action {
                    self.queue_order.retain(|id| id != message_id);
                    let position = before_message_id
                        .as_ref()
                        .and_then(|before| self.queue_order.iter().position(|id| id == before))
                        .unwrap_or(self.queue_order.len());
                    self.queue_order.insert(position, message_id.clone());
                }
            }
            WorkspaceCommand::CreateChannel {
                channel_id,
                title,
                purpose,
                members,
                coordinator_bot,
                coordinator_node,
            } => {
                ensure!(
                    !self.channels.contains_key(channel_id),
                    "channel ID already exists"
                );
                self.channels.insert(
                    channel_id.clone(),
                    Channel {
                        channel_id: channel_id.clone(),
                        title: title.clone(),
                        purpose: purpose.clone(),
                        members: members.clone(),
                        coordinator_bot: coordinator_bot.clone(),
                        coordinator_node: coordinator_node.clone(),
                        owner_epoch: 1,
                        revision: 1,
                        event_hash: hash.into(),
                        created_at_ms: event.created_at_ms,
                    },
                );
            }
            WorkspaceCommand::ConfigureChannel {
                channel_id,
                expected_revision,
                title,
                purpose,
                members,
                coordinator_bot,
                coordinator_node,
                owner_epoch,
            } => {
                let channel = self
                    .channels
                    .get_mut(channel_id)
                    .context("channel not found")?;
                ensure!(
                    channel.revision == *expected_revision,
                    "channel revision changed"
                );
                ensure!(
                    *owner_epoch
                        == channel.owner_epoch
                            + u64::from(channel.coordinator_node != *coordinator_node),
                    "invalid channel owner epoch"
                );
                channel.title = title.clone();
                channel.purpose = purpose.clone();
                channel.members = members.clone();
                channel.coordinator_bot = coordinator_bot.clone();
                channel.coordinator_node = coordinator_node.clone();
                channel.owner_epoch = *owner_epoch;
                channel.revision += 1;
                channel.event_hash = hash.into();
            }
            WorkspaceCommand::PostChannelMessage {
                channel_id,
                recipient_bot,
                task_id,
                ..
            } => {
                let channel = self.channel(channel_id)?;
                if let Some(bot) = recipient_bot {
                    ensure!(channel.members.contains(bot), "Bot is not a channel member");
                }
                if let Some(task_id) = task_id {
                    ensure!(
                        self.tasks
                            .get(task_id)
                            .is_some_and(|task| task.channel_id.as_ref() == Some(channel_id)),
                        "task is outside the channel"
                    );
                }
            }
            WorkspaceCommand::CreateTask {
                task_id,
                channel_id,
                parent_task_id,
                owner_bot,
                title,
                request,
                expected_result,
                source_events,
                dependencies,
                reply_only,
            } => {
                ensure!(!self.tasks.contains_key(task_id), "task ID already exists");
                if let Some(channel) = channel_id {
                    ensure!(
                        self.channel(channel)?.members.contains(owner_bot),
                        "task Bot is outside the channel"
                    );
                }
                self.dependencies(task_id, dependencies)?;
                let mut root = task_id.clone();
                if let Some(parent_id) = parent_task_id {
                    let parent = self.tasks.get(parent_id).context("parent task not found")?;
                    ensure!(
                        !parent.reply_only,
                        "a reply-only task cannot create children"
                    );
                    ensure!(
                        parent.channel_id == *channel_id,
                        "parent task is outside the channel"
                    );
                    root = parent.root_task_id.clone();
                    let mut depth = 1;
                    let mut ancestor = Some(parent);
                    while let Some(task) = ancestor {
                        depth += 1;
                        ancestor = task
                            .parent_task_id
                            .as_ref()
                            .and_then(|id| self.tasks.get(id));
                    }
                    ensure!(depth <= MAX_TASK_DEPTH, "task depth budget reached");
                }
                if *reply_only {
                    ensure!(parent_task_id.is_some(), "a reply task needs its caller");
                    ensure!(
                        self.tasks
                            .values()
                            .filter(|task| task.root_task_id == root
                                && task.owner_bot == *owner_bot
                                && task.reply_only)
                            .count()
                            < MAX_MEMBER_REPLIES,
                        "member reply budget reached"
                    );
                }
                self.tasks.insert(
                    task_id.clone(),
                    WorkspaceTask {
                        task_id: task_id.clone(),
                        channel_id: channel_id.clone(),
                        root_task_id: root,
                        parent_task_id: parent_task_id.clone(),
                        owner_bot: owner_bot.clone(),
                        title: title.clone(),
                        request: request.clone(),
                        expected_result: expected_result.clone(),
                        source_events: source_events.clone(),
                        dependencies: dependencies.clone(),
                        reply_only: *reply_only,
                        revision: 1,
                        action: TaskAction::Continue,
                        request_event: hash.into(),
                        event_hash: hash.into(),
                        created_at_ms: event.created_at_ms,
                        updated_at_ms: event.created_at_ms,
                        state: TaskState::NotStarted,
                        waiting_reason: None,
                        current_assignment: None,
                    },
                );
                if *reply_only {
                    let parent = self
                        .tasks
                        .get_mut(parent_task_id.as_ref().expect("validated parent"))
                        .expect("validated parent");
                    parent.dependencies.push(task_id.clone());
                    let parent_id = parent_task_id.as_ref().expect("validated parent");
                    self.dependencies(parent_id, &self.tasks[parent_id].dependencies)?;
                }
            }
            WorkspaceCommand::ReviseTask {
                task_id,
                expected_revision,
                request,
                expected_result,
                dependencies,
            } => {
                ensure!(
                    self.task(task_id, *expected_revision)?.action != TaskAction::Cancel,
                    "cancelled tasks cannot be revised"
                );
                self.dependencies(task_id, dependencies)?;
                let task = self.tasks.get_mut(task_id).expect("validated task");
                task.revision += 1;
                task.request = request.clone();
                task.expected_result = expected_result.clone();
                task.dependencies = dependencies.clone();
                task.action = TaskAction::Continue;
                task.event_hash = hash.into();
                task.updated_at_ms = event.created_at_ms;
            }
            WorkspaceCommand::ControlTask {
                task_id,
                expected_revision,
                action,
            } => {
                self.task(task_id, *expected_revision)?;
                let task = self.tasks.get_mut(task_id).expect("validated task");
                ensure!(
                    task.action != TaskAction::Cancel || *action == TaskAction::Cancel,
                    "cancelled tasks cannot restart"
                );
                if matches!(action, TaskAction::Stop | TaskAction::Cancel)
                    && let Some(id) = &task.current_assignment
                {
                    self.retry_blocked.insert(id.clone());
                }
                task.action = *action;
                task.event_hash = hash.into();
                task.updated_at_ms = event.created_at_ms;
            }
            WorkspaceCommand::AssignTask {
                task_id,
                expected_revision,
                assignment_id,
                bot_id,
                execution_node,
                bot_owner_epoch,
                channel_owner_epoch,
                work_id,
                resource_roots,
                automatic,
            } => {
                let task = self.task(task_id, *expected_revision)?;
                ensure!(
                    task.action == TaskAction::Continue,
                    "task is not ready to run"
                );
                ensure!(
                    task.owner_bot == *bot_id,
                    "assignment must use the task's Bot"
                );
                ensure!(
                    !self.assignments.contains_key(assignment_id),
                    "assignment ID already exists"
                );
                if let Some(previous) = task
                    .current_assignment
                    .as_ref()
                    .and_then(|id| self.assignments.get(id))
                {
                    ensure!(
                        matches!(
                            previous.state,
                            AssignmentState::Stopped | AssignmentState::Completed
                        ),
                        "previous assignment must be proven stopped before handoff"
                    );
                }
                if let Some(channel_id) = &task.channel_id {
                    ensure!(
                        Some(self.channel(channel_id)?.owner_epoch) == *channel_owner_epoch,
                        "channel owner epoch changed"
                    );
                } else {
                    ensure!(
                        channel_owner_epoch.is_none(),
                        "standalone task has no channel epoch"
                    );
                }
                if *automatic {
                    ensure!(
                        self.assignments
                            .values()
                            .filter(|assignment| assignment.automatic
                                && self
                                    .tasks
                                    .get(&assignment.task_id)
                                    .is_some_and(|other| other.root_task_id == task.root_task_id))
                            .count()
                            < MAX_AUTOMATIC_ASSIGNMENTS,
                        "automatic dispatch budget reached"
                    );
                }
                if let Some(work) = work_id {
                    ensure!(
                        !self.assignments.values().any(|other| other.work_id.as_ref()
                            == Some(work)
                            && !matches!(
                                other.state,
                                AssignmentState::Completed | AssignmentState::Stopped
                            )),
                        "Work already belongs to an active assignment"
                    );
                }
                for other in self.assignments.values().filter(|other| {
                    other.execution_node == *execution_node
                        && !matches!(
                            other.state,
                            AssignmentState::Completed | AssignmentState::Stopped
                        )
                }) {
                    ensure!(
                        !resource_roots.iter().any(|root| other
                            .resource_roots
                            .iter()
                            .any(|other| overlap(root, other))),
                        "workspace root is occupied by assignment {}",
                        other.assignment_id
                    );
                }
                let (trigger_message_id, source_event) = match cause {
                    Some(cause) => (
                        Some(cause.message_id.clone()),
                        self.inputs
                            .get(&cause.message_id)
                            .map(|input| input.source_event.clone()),
                    ),
                    _ => (None, Some(task.request_event.clone())),
                };
                self.assignments.insert(
                    assignment_id.clone(),
                    Assignment {
                        submitted_result: None,
                        trigger_message_id,
                        source_event,
                        assignment_id: assignment_id.clone(),
                        task_id: task_id.clone(),
                        started_revision: *expected_revision,
                        bot_id: bot_id.clone(),
                        execution_node: execution_node.clone(),
                        bot_owner_epoch: *bot_owner_epoch,
                        channel_owner_epoch: *channel_owner_epoch,
                        work_id: work_id.clone(),
                        basis,
                        resource_roots: resource_roots.clone(),
                        automatic: *automatic,
                        accepted_revision: None,
                        adopted_revision: None,
                        completed_revision: None,
                        state: AssignmentState::Proposed,
                        result: None,
                        event_hash: hash.into(),
                        created_at_ms: event.created_at_ms,
                    },
                );
                self.tasks
                    .get_mut(task_id)
                    .expect("validated task")
                    .current_assignment = Some(assignment_id.clone());
            }
            WorkspaceCommand::AssignmentProgress {
                assignment_id,
                revision,
                state,
                result,
                evidence_event,
                execution,
                ..
            } => {
                let assignment = self
                    .assignments
                    .get(assignment_id)
                    .context("assignment not found")?;
                ensure!(
                    assignment.execution_node == event.author,
                    "only the executing Node records assignment evidence"
                );
                let task = self
                    .tasks
                    .get(&assignment.task_id)
                    .context("assignment task not found")?;
                ensure!(
                    *revision >= assignment.started_revision && *revision <= task.revision,
                    "assignment revision is outside this task"
                );
                ensure!(
                    !matches!(
                        assignment.state,
                        AssignmentState::Completed | AssignmentState::Stopped
                    ),
                    "assignment already ended"
                );
                ensure!(
                    assignment.state != AssignmentState::Unknown
                        || evidence_event.is_some()
                        || matches!(
                            state,
                            AssignmentState::Unknown
                                | AssignmentState::Completed
                                | AssignmentState::Stopped
                        ),
                    "unknown execution needs a proven terminal result"
                );
                ensure!(
                    *state != AssignmentState::Accepted
                        || assignment.state == AssignmentState::Proposed
                        || evidence_event.is_some(),
                    "assignment acceptance cannot regress adoption"
                );
                ensure!(
                    *state != AssignmentState::Adopted
                        || assignment
                            .adopted_revision
                            .is_none_or(|adopted| *revision >= adopted),
                    "assignment adoption cannot move backwards"
                );
                if *state == AssignmentState::Completed {
                    ensure!(
                        assignment.adopted_revision == Some(*revision),
                        "assignment must explicitly adopt the result revision"
                    );
                    ensure!(
                        *revision != task.revision
                            || task.dependencies.iter().all(|id| self
                                .tasks
                                .get(id)
                                .is_some_and(|task| task.state == TaskState::Completed)),
                        "required task results are incomplete"
                    );
                }
                if *state == AssignmentState::Completed && evidence_event.is_none() {
                    let text = result
                        .as_ref()
                        .filter(|result| !result.trim().is_empty())
                        .context("result submission needs its text")?;
                    ensure!(
                        assignment.state != AssignmentState::Unknown,
                        "unknown execution needs a proven terminal result"
                    );
                    self.assignments
                        .get_mut(assignment_id)
                        .expect("validated assignment")
                        .submitted_result = Some(AssignmentSubmission {
                        revision: *revision,
                        text: text.clone(),
                        event_hash: hash.into(),
                    });
                    self.refresh();
                    return Ok(());
                }
                let assignment = self
                    .assignments
                    .get_mut(assignment_id)
                    .expect("validated assignment");
                match state {
                    AssignmentState::Accepted => {
                        ensure!(
                            *revision == assignment.started_revision,
                            "acceptance pins the started revision"
                        );
                        assignment.accepted_revision = Some(*revision);
                    }
                    AssignmentState::Adopted => {
                        ensure!(
                            assignment.accepted_revision.is_some(),
                            "assignment is not accepted"
                        );
                        assignment.adopted_revision = Some(*revision);
                    }
                    AssignmentState::Completed => assignment.completed_revision = Some(*revision),
                    _ => {}
                }
                assignment.state = *state;
                if result.is_some() {
                    assignment.result = result.clone();
                }
                if *state == AssignmentState::Unknown {
                    self.retry_blocked.insert(assignment_id.clone());
                }
                if *state == AssignmentState::Stopped
                    && *execution == Some(crate::scheduling::TurnExecution::NotStarted)
                {
                    self.stopped.insert(
                        assignment_id.clone(),
                        AssignmentStopProof {
                            event_hash: hash.into(),
                            evidence_event: evidence_event
                                .clone()
                                .context("not-started stop needs its failed reply")?,
                            source_event: assignment
                                .source_event
                                .clone()
                                .context("not-started stop needs its original input")?,
                            revision: *revision,
                        },
                    );
                }
            }
        }
        self.refresh();
        Ok(())
    }
    pub(super) fn record(
        &mut self,
        hash: &str,
        event: &Event,
        rank: &str,
        basis: VersionVector,
    ) -> Result<()> {
        if let EventBody::InputPosted {
            message_id,
            target,
            intent,
            sequence,
            ..
        } = &event.body
        {
            self.inputs
                .entry(message_id.clone())
                .or_insert_with(|| InputControlState {
                    source_event: hash.into(),
                    control_event: hash.into(),
                    target: target.clone(),
                    sequence: *sequence,
                    deferred: *intent == InputIntent::Defer,
                    cancelled: false,
                    text: None,
                    attachments: None,
                });
            if !self.queue_order.contains(message_id) {
                self.queue_order.push(message_id.clone());
            }
            self.last_rank = rank.into();
            return Ok(());
        }
        let EventBody::WorkspaceCommanded {
            command_id,
            command,
            cause,
            ..
        } = &event.body
        else {
            return Ok(());
        };
        let key = request_key(
            &event.network_id,
            &event.scope_id,
            &event.body,
            &event.author,
        )?;
        if self.commands.contains_key(command_id) {
            return Ok(());
        }
        let mut next = self.clone();
        let error = next
            .apply_command(hash, event, command, cause.as_ref(), basis)
            .err()
            .map(|error| error.to_string());
        if error.is_none() {
            if !next.replace_lookups {
                let changed_tasks: BTreeSet<_> = self
                    .tasks
                    .keys()
                    .chain(next.tasks.keys())
                    .filter(|id| self.tasks.get(*id) != next.tasks.get(*id))
                    .cloned()
                    .collect();
                let changed_assignments: BTreeSet<_> = self
                    .assignments
                    .keys()
                    .chain(next.assignments.keys())
                    .filter(|id| self.assignments.get(*id) != next.assignments.get(*id))
                    .cloned()
                    .collect();
                next.dirty_tasks.extend(changed_tasks);
                next.dirty_assignments.extend(changed_assignments);
            }
            *self = next;
        }
        self.commands.insert(
            command_id.clone(),
            CommandResult {
                event_hash: hash.into(),
                request_key: key,
                error,
            },
        );
        self.last_rank = rank.into();
        Ok(())
    }
}

pub(super) fn direct_parent(event: &Event, hash: &str) -> bool {
    event.prev.as_deref() == Some(hash) || event.deps.iter().any(|parent| parent == hash)
}
