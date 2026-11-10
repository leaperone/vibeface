//! Workspace rows are disposable projections of the existing signed DAG.
use super::{Store, conversation_from, log, membership_from};
use crate::{
    crypto::{LocalIdentity, request_key},
    model::*,
    scheduling::{SessionScope, TriggerKind},
    workspace::*,
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Graph {
    channels: BTreeMap<String, Channel>,
    tasks: BTreeMap<String, WorkspaceTask>,
    assignments: BTreeMap<String, Assignment>,
    commands: BTreeMap<String, CommandResult>,
    inputs: BTreeMap<String, InputControlState>,
    queue_order: Vec<String>,
    last_rank: String,
    #[serde(default)]
    stopped: BTreeMap<String, AssignmentStopProof>,
    #[serde(default)]
    retry_blocked: BTreeSet<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommandResult {
    event_hash: String,
    request_key: String,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InputControlState {
    source_event: String,
    control_event: String,
    target: InputTarget,
    sequence: u64,
    deferred: bool,
    cancelled: bool,
    text: Option<String>,
    attachments: Option<Vec<Attachment>>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum InputAssignmentBinding {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AssignmentStopProof {
    event_hash: String,
    evidence_event: String,
    source_event: String,
    revision: u64,
}

fn valid_id(value: &str) -> Result<()> {
    vibeface_protocol::validate_id(value).map_err(anyhow::Error::msg)
}
fn valid_hash(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid hash or device ID"
    );
    Ok(())
}
fn valid_text(value: &str, max: usize, field: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= max,
        "invalid {field}"
    );
    Ok(())
}
#[derive(Debug, PartialEq, Eq)]
struct LexicalRoot {
    family: RootFamily,
    anchor: String,
    parts: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum RootFamily {
    Posix,
    Windows,
}

// Shared workspace metadata is replayed on different host OSes. Parse its spelling
// as data instead of asking the current host's std::path implementation.
fn lexical_root(path: &str) -> Option<LexicalRoot> {
    if path.is_empty() || path.len() > 4096 || path.chars().any(char::is_control) {
        return None;
    }
    let verbatim = path.starts_with("\\\\?\\");
    let (family, anchor, body, separator, root_only) = if let Some(body) = path.strip_prefix('/') {
        if path.starts_with("//") {
            return None;
        }
        (RootFamily::Posix, "/".to_owned(), body, '/', path == "/")
    } else if let Some(rest) = path
        .strip_prefix("\\\\?\\UNC\\")
        .or_else(|| path.strip_prefix("\\\\?\\unc\\"))
    {
        let mut it = rest.split('\\');
        let server = it.next()?;
        let share = it.next()?;
        if !valid_unc_anchor(server) || !valid_unc_anchor(share) {
            return None;
        }
        let consumed = server.len() + 1 + share.len();
        let body = &rest[consumed..];
        (
            RootFamily::Windows,
            format!("\\\\{server}\\{share}"),
            body.strip_prefix('\\').unwrap_or(body),
            '\\',
            body.is_empty() || body == "\\",
        )
    } else if let Some(rest) = path.strip_prefix("\\\\?\\") {
        if rest.len() < 3
            || !rest.as_bytes()[0].is_ascii_alphabetic()
            || rest.as_bytes()[1] != b':'
            || rest.as_bytes()[2] != b'\\'
        {
            return None;
        }
        (
            RootFamily::Windows,
            format!("{}:", (rest.as_bytes()[0] as char).to_ascii_uppercase()),
            &rest[3..],
            '\\',
            rest.len() == 3,
        )
    } else if let Some(rest) = path.strip_prefix("\\\\") {
        let mut it = rest.split('\\');
        let server = it.next()?;
        let share = it.next()?;
        if !valid_unc_anchor(server) || !valid_unc_anchor(share) {
            return None;
        }
        let consumed = server.len() + 1 + share.len();
        let body = &rest[consumed..];
        (
            RootFamily::Windows,
            format!("\\\\{server}\\{share}"),
            body.strip_prefix('\\').unwrap_or(body),
            '\\',
            body.is_empty() || body == "\\",
        )
    } else if path.len() >= 3
        && path.as_bytes()[0].is_ascii_alphabetic()
        && path.as_bytes()[1] == b':'
        && matches!(path.as_bytes()[2], b'/' | b'\\')
    {
        let sep = path.as_bytes()[2] as char;
        (
            RootFamily::Windows,
            format!("{}:", (path.as_bytes()[0] as char).to_ascii_uppercase()),
            &path[3..],
            sep,
            path.len() == 3,
        )
    } else {
        return None;
    };
    if root_only {
        return Some(LexicalRoot {
            family,
            anchor,
            parts: Vec::new(),
        });
    }
    if body.is_empty()
        || body.ends_with(separator)
        || (family == RootFamily::Windows
            && body.contains(if separator == '\\' { '/' } else { '\\' }))
    {
        return None;
    }
    let parts: Vec<String> = body.split(separator).map(str::to_owned).collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || part == "." || part == "..")
        || (family == RootFamily::Windows
            && !verbatim
            && parts.iter().any(|part| part.ends_with(['.', ' '])))
    {
        return None;
    }
    Some(LexicalRoot {
        family,
        anchor,
        parts,
    })
}

fn valid_unc_anchor(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && !part.contains(['/', ':'])
        && !part.ends_with(['.', ' '])
}

fn valid_roots(roots: &[String]) -> Result<()> {
    ensure!(roots.len() <= 16, "too many workspace roots");
    for root in roots {
        ensure!(
            lexical_root(root).is_some(),
            "workspace roots must be normalized absolute paths"
        );
    }
    ensure!(
        roots.iter().collect::<BTreeSet<_>>().len() == roots.len(),
        "duplicate workspace root"
    );
    Ok(())
}
fn overlap(left: &str, right: &str) -> bool {
    let (Some(left), Some(right)) = (lexical_root(left), lexical_root(right)) else {
        return false;
    };
    if left.family != right.family || !eq_root_part(&left.family, &left.anchor, &right.anchor) {
        return false;
    }
    let common = left.parts.len().min(right.parts.len());
    left.parts[..common]
        .iter()
        .zip(&right.parts[..common])
        .all(|(a, b)| eq_root_part(&left.family, a, b))
}

fn eq_root_part(family: &RootFamily, left: &str, right: &str) -> bool {
    match family {
        RootFamily::Posix => left == right,
        RootFamily::Windows => left
            .chars()
            .map(simple_uppercase)
            .eq(right.chars().map(simple_uppercase)),
    }
}

fn simple_uppercase(character: char) -> char {
    let mut uppercase = character.to_uppercase();
    let first = uppercase.next().unwrap_or(character);
    if uppercase.next().is_none() {
        first
    } else {
        character
    }
}

#[cfg(test)]
mod root_path_tests {
    use super::{overlap, valid_roots};

    #[test]
    fn accepts_normalized_absolute_paths_for_each_platform() {
        for path in [
            "/srv/work",
            "/srv/项目",
            "C:\\Users\\dev\\work",
            "C:\\资料\\项目",
            "D:/work",
            "\\\\server\\share\\work",
            "\\\\?\\C:\\work",
            "\\\\?\\C:\\work.",
            "\\\\?\\C:\\work ",
            "\\\\?\\UNC\\server\\share\\work",
        ] {
            assert!(valid_roots(&[path.to_owned()]).is_ok(), "{path}");
        }
    }

    #[test]
    fn rejects_relative_and_noncanonical_paths_for_each_platform() {
        for path in [
            "relative/work",
            "/srv/./work",
            "/srv/../work",
            "/srv//work",
            "/srv/work/",
            "C:work",
            "C:\\work\\..\\other",
            "C:\\work/../other",
            "C:/work\\..\\other",
            "C:\\work\\",
            "C:\\work.",
            "C:\\work ",
            "C:/work./repo",
            "\\\\.\\share\\work",
            "\\\\server\\..\\work",
            "\\\\server\\share\\..\\work",
            "\\\\server\\share\\work\\\\x",
            "\\\\server\\share\\work.",
            "\\\\server.\\share\\work",
            "/srv/wo\u{7}rk",
        ] {
            assert!(valid_roots(&[path.to_owned()]).is_err(), "{path:?}");
        }
    }

    #[test]
    fn overlap_uses_portable_components() {
        assert!(overlap("C:\\Work\\repo", "c:\\work"));
        assert!(overlap("\\\\?\\C:\\Work\\repo", "C:/work"));
        assert!(!overlap("\\\\?\\C:\\work.", "C:\\work"));
        assert!(!overlap("\\\\?\\C:\\work ", "C:\\work"));
        assert!(!overlap("C:\\Work", "D:\\Work"));
        assert!(overlap("/srv/work", "/srv/work/repo"));
        assert!(!overlap("/srv/work", "/srv/worker"));
        assert!(overlap("\\\\server\\share\\work", "\\\\server\\share"));
        assert!(overlap(
            "\\\\?\\UNC\\server\\share\\work",
            "\\\\server\\share"
        ));
        assert!(!overlap(
            "\\\\server\\share\\work",
            "\\\\other\\share\\work"
        ));
    }

    #[test]
    fn windows_unicode_case_aliases_overlap_without_normalization_or_expansion() {
        assert!(overlap("C:\\Ä\\repo", "c:\\ä"));
        assert!(overlap("\\\\SÉRVER\\SHÄRE\\repo", "\\\\sérver\\shäre"));
        assert!(overlap("\\\\?\\C:\\Ä\\repo", "C:\\ä"));
        assert!(overlap("C:\\Σ\\repo", "c:\\ς"));

        assert!(!overlap("C:\\Straße", "c:\\STRASSE"));
        assert!(!overlap("C:\\Ä", "C:\\A\u{0308}"));
        assert!(!overlap("/srv/Ä", "/srv/ä"));
        assert!(!overlap("C:\\ä", "C:\\ä-repo"));
    }
}

pub(crate) fn validate_target(target: &InputTarget) -> Result<()> {
    match target {
        InputTarget::BotMain { bot_id } => valid_id(bot_id),
        InputTarget::Task { task_id } => valid_id(task_id),
        InputTarget::Work { work_id } => valid_id(work_id),
        InputTarget::Routine { bot_id, trigger } => {
            valid_id(bot_id)?;
            ensure!(
                crate::scheduling::parse_schedule_trigger(trigger).is_some(),
                "invalid routine trigger"
            );
            Ok(())
        }
        InputTarget::ChannelMember { channel_id, bot_id } => {
            valid_id(channel_id)?;
            valid_id(bot_id)
        }
        InputTarget::ChannelTask {
            channel_id,
            task_id,
            bot_id,
        } => {
            valid_id(channel_id)?;
            valid_id(task_id)?;
            valid_id(bot_id)
        }
    }
}

pub(crate) fn validate_command(command: &WorkspaceCommand) -> Result<()> {
    match command {
        WorkspaceCommand::ControlInput {
            message_id,
            expected_event,
            action,
        } => {
            valid_id(message_id)?;
            valid_hash(expected_event)?;
            match action {
                InputControl::Edit { text, attachments } => {
                    ensure!(
                        text.len() <= 65536 && (!text.trim().is_empty() || !attachments.is_empty()),
                        "invalid edited input"
                    );
                    validate_attachments(attachments)?;
                }
                InputControl::Move {
                    before_message_id: Some(id),
                } => valid_id(id)?,
                _ => {}
            }
        }
        WorkspaceCommand::CreateChannel {
            channel_id,
            title,
            purpose,
            members,
            coordinator_bot,
            coordinator_node,
        }
        | WorkspaceCommand::ConfigureChannel {
            channel_id,
            title,
            purpose,
            members,
            coordinator_bot,
            coordinator_node,
            ..
        } => {
            valid_id(channel_id)?;
            valid_id(coordinator_bot)?;
            valid_hash(coordinator_node)?;
            valid_text(title, 512, "channel title")?;
            valid_text(purpose, 16384, "channel purpose")?;
            ensure!(
                !members.is_empty() && members.len() <= 32,
                "a channel needs 1-32 Bots"
            );
            for member in members {
                valid_id(member)?;
            }
            ensure!(
                members.iter().collect::<BTreeSet<_>>().len() == members.len()
                    && members.contains(coordinator_bot),
                "invalid channel membership"
            );
        }
        WorkspaceCommand::PostChannelMessage {
            channel_id,
            message_id,
            text,
            attachments,
            recipient_bot,
            task_id,
            reply_to,
        } => {
            valid_id(channel_id)?;
            valid_id(message_id)?;
            ensure!(
                !text.trim().is_empty() || !attachments.is_empty(),
                "empty channel message"
            );
            ensure!(text.len() <= 65536, "channel message too long");
            for id in [recipient_bot, task_id, reply_to].into_iter().flatten() {
                valid_id(id)?;
            }
            validate_attachments(attachments)?;
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
            ..
        } => {
            valid_id(task_id)?;
            valid_id(owner_bot)?;
            for id in [channel_id, parent_task_id].into_iter().flatten() {
                valid_id(id)?;
            }
            valid_text(title, 512, "task title")?;
            valid_text(request, 65536, "task request")?;
            valid_text(expected_result, 16384, "expected result")?;
            ensure!(source_events.len() <= 128, "too many task sources");
            for hash in source_events {
                valid_hash(hash)?;
            }
            validate_dependencies(dependencies)?;
        }
        WorkspaceCommand::ReviseTask {
            task_id,
            expected_revision,
            request,
            expected_result,
            dependencies,
        } => {
            valid_id(task_id)?;
            ensure!(*expected_revision > 0, "invalid task revision");
            valid_text(request, 65536, "task request")?;
            valid_text(expected_result, 16384, "expected result")?;
            validate_dependencies(dependencies)?;
        }
        WorkspaceCommand::ControlTask {
            task_id,
            expected_revision,
            ..
        } => {
            valid_id(task_id)?;
            ensure!(*expected_revision > 0, "invalid task revision");
        }
        WorkspaceCommand::AssignTask {
            task_id,
            expected_revision,
            assignment_id,
            bot_id,
            execution_node,
            resource_roots,
            ..
        } => {
            valid_id(task_id)?;
            valid_id(assignment_id)?;
            valid_id(bot_id)?;
            valid_hash(execution_node)?;
            ensure!(*expected_revision > 0, "invalid task revision");
            valid_roots(resource_roots)?;
        }
        WorkspaceCommand::AssignmentProgress {
            execution,
            assignment_id,
            revision,
            state,
            input_event,
            evidence_event,
            result,
        } => {
            valid_id(assignment_id)?;
            ensure!(
                *execution != Some(crate::scheduling::TurnExecution::NotStarted)
                    || *state == AssignmentState::Stopped,
                "not-started evidence belongs to a stopped assignment"
            );
            ensure!(*revision > 0, "invalid assignment revision");
            ensure!(
                *state != AssignmentState::Proposed,
                "an assignment cannot return to proposed"
            );
            for hash in [input_event, evidence_event].into_iter().flatten() {
                valid_hash(hash)?;
            }
            if let Some(result) = result {
                ensure!(result.len() <= 65536, "assignment result too long");
            }
        }
    }
    Ok(())
}
fn validate_dependencies(dependencies: &[String]) -> Result<()> {
    ensure!(
        dependencies.len() <= 32
            && dependencies.iter().collect::<BTreeSet<_>>().len() == dependencies.len(),
        "invalid task dependencies"
    );
    for dependency in dependencies {
        valid_id(dependency)?;
    }
    Ok(())
}
pub(crate) fn validate_attachments(attachments: &[Attachment]) -> Result<()> {
    ensure!(
        attachments.len() <= 128
            && attachments
                .windows(2)
                .all(|pair| pair[0].hash < pair[1].hash),
        "attachments must be sorted and unique"
    );
    for attachment in attachments {
        valid_hash(&attachment.hash)?;
        ensure!(
            attachment.size <= MAX_BLOB_BYTES as u64 && valid_attachment_name(&attachment.name),
            "invalid attachment"
        );
    }
    Ok(())
}

impl Graph {
    fn read(connection: &Connection, network: &str) -> Result<Self> {
        let encoded: Option<String> = connection
            .query_row(
                "SELECT state FROM workspace_projection WHERE network_id=?",
                [network],
                |row| row.get(0),
            )
            .optional()?;
        encoded
            .map(|encoded| serde_json::from_str(&encoded).map_err(Into::into))
            .transpose()
            .map(Option::unwrap_or_default)
    }
    fn save(&self, connection: &Connection, network: &str) -> Result<()> {
        connection.execute("INSERT INTO workspace_projection(network_id,state) VALUES(?,?) ON CONFLICT(network_id) DO UPDATE SET state=excluded.state", params![network, serde_json::to_string(self)?])?;
        Ok(())
    }
    fn task(&self, task_id: &str, expected: u64) -> Result<&WorkspaceTask> {
        let task = self.tasks.get(task_id).context("task not found")?;
        ensure!(task.revision == expected, "task revision changed");
        Ok(task)
    }
    fn channel(&self, channel_id: &str) -> Result<&Channel> {
        self.channels.get(channel_id).context("channel not found")
    }
    fn work_ready(&self, assignment: &Assignment, task: &WorkspaceTask) -> bool {
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
    fn startup_retry_assignment(
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
    fn releasable_assignment(&self, message_id: &str, event: &Event) -> Result<Option<String>> {
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
    fn apply(&mut self, hash: &str, event: &Event, basis: VersionVector) -> Result<()> {
        let EventBody::WorkspaceCommanded { command, .. } = &event.body else {
            return Ok(());
        };
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
                let (trigger_message_id, source_event) = match &event.body {
                    EventBody::WorkspaceCommanded {
                        cause: Some(cause), ..
                    } => (
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
    fn record(
        &mut self,
        hash: &str,
        event: &Event,
        rank: &str,
        basis: impl FnOnce() -> Result<VersionVector>,
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
        // Only an assignment stores its causal basis, and computing one walks the whole history.
        let basis = if matches!(command, WorkspaceCommand::AssignTask { .. }) {
            basis()?
        } else {
            VersionVector::new()
        };
        let mut next = self.clone();
        let error = next
            .apply(hash, event, basis)
            .err()
            .map(|error| error.to_string());
        if error.is_none() {
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

fn ancestors(
    connection: &Connection,
    network: &str,
    event: &Event,
) -> Result<BTreeMap<String, Event>> {
    let mut pending: Vec<_> = event.prev.iter().chain(&event.deps).cloned().collect();
    let mut result = BTreeMap::new();
    while let Some(hash) = pending.pop() {
        if result.contains_key(&hash) {
            continue;
        }
        let ancestor = log::event_from(connection, network, &hash)?
            .context("workspace dependency is missing")?;
        pending.extend(ancestor.prev.iter().chain(&ancestor.deps).cloned());
        result.insert(hash, ancestor);
    }
    Ok(result)
}
fn basis(connection: &Connection, network: &str, event: &Event) -> Result<VersionVector> {
    Ok(ancestors(connection, network, event)?.values().fold(
        VersionVector::new(),
        |mut vector, event| {
            let seq = vector.entry(event.author.clone()).or_default();
            *seq = (*seq).max(event.seq);
            vector
        },
    ))
}
fn graph_at(
    connection: &Connection,
    network: &str,
    known: Option<&BTreeMap<String, Event>>,
) -> Result<Graph> {
    let mut graph = Graph::default();
    let mut statement=connection.prepare("SELECT hash,envelope,lamport,created_at_ms FROM events WHERE network_id=? AND effective=1 AND kind IN ('workspace_commanded','input_posted') ORDER BY lamport,created_at_ms,hash")?;
    let rows = statement.query_map([network], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, u64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (hash, envelope, lamport, created) = row?;
        if known.is_some_and(|known| !known.contains_key(&hash)) {
            continue;
        }
        let envelope: SignedEvent = serde_json::from_str(&envelope)?;
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        graph.record(&hash, &event, &super::rank(lamport, created, &hash), || {
            basis(connection, network, &event)
        })?;
    }
    Ok(graph)
}
pub(crate) fn apply_projection(
    connection: &Connection,
    network: &str,
    hash: &str,
    event: &Event,
    rank: &str,
) -> Result<()> {
    let mut graph = Graph::read(connection, network)?;
    if rank < graph.last_rank.as_str() {
        graph = graph_at(connection, network, None)?;
    } else {
        graph.record(hash, event, rank, || basis(connection, network, event))?;
    }
    graph.save(connection, network)?;
    if let EventBody::WorkspaceCommanded {
        command: WorkspaceCommand::AssignTask { assignment_id, .. },
        ..
    } = &event.body
        && let Some(assignment) = graph.assignments.get(assignment_id)
    {
        let scope = bot_scope(connection, network, &assignment.bot_id)?;
        refresh_reply_targets(connection, network, &scope, assignment_id)?;
    }
    Ok(())
}

fn bot_scope(connection: &Connection, network: &str, bot_id: &str) -> Result<String> {
    connection
        .query_row(
            "SELECT conversation_id FROM bots WHERE network_id=? AND bot_id=?",
            params![network, bot_id],
            |row| row.get(0),
        )
        .optional()?
        .context("Bot not found in this personal network")
}
fn owner_at(known: &BTreeMap<String, Event>, scope: &str) -> Result<(String, u64)> {
    let mut owners = Vec::new();
    for event in known.values().filter(|event| event.scope_id == scope) {
        if let EventBody::ConversationOwner {
            scheduler_node,
            epoch,
            ..
        } = &event.body
        {
            owners.push((
                *epoch,
                request_key(&event.network_id, scope, &event.body, &event.author)?,
                scheduler_node.clone(),
            ));
        }
    }
    owners.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    owners
        .first()
        .map(|(epoch, _, node)| (node.clone(), *epoch))
        .context("Bot has no causally known owner")
}
fn direct_parent(event: &Event, hash: &str) -> bool {
    event.prev.as_deref() == Some(hash) || event.deps.iter().any(|parent| parent == hash)
}

fn assignment_reply_source_matches_from(
    connection: &Connection,
    network: &str,
    assignment: &Assignment,
    revision: u64,
    input: &str,
    proof: &Event,
) -> Result<bool> {
    let known = ancestors(connection, network, proof)?;
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
            && known.get(&source.source_event).is_some_and(|event| {
                matches!(
                    &event.body,
                    EventBody::InputPosted {
                        attestation: Some(_),
                        ..
                    }
                )
            }));
    }
    let Some(answer) = super::message_from(connection, network, &proof.scope_id, input)?
        .filter(|answer| answer.role == Role::User)
    else {
        return Ok(false);
    };
    let Some(question) = answer.answers.as_deref() else {
        return Ok(false);
    };
    if !known.get(&answer.event_hash).is_some_and(|event| {
        matches!(&event.body, EventBody::MessagePosted { role: Role::User, answers: Some(id), attestation: Some(_), .. } if id == question)
    }) {
        return Ok(false);
    }
    let Some(asked) = super::event_about_from(
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
    if !known.contains_key(&asked.hash)
        || asked.event.author != assignment.execution_node
        || about.node != assignment.execution_node
        || target_task(target) != Some(assignment.task_id.as_str())
    {
        return Ok(false);
    }
    let question_known = ancestors(connection, network, &asked.event)?;
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

pub(crate) fn authorize_event(connection: &Connection, network: &str, event: &Event) -> Result<()> {
    if !matches!(
        event.body,
        EventBody::WorkspaceCommanded { .. } | EventBody::InputPosted { .. }
    ) {
        return Ok(());
    }
    let known = ancestors(connection, network, event)?;
    let mut graph = graph_at(connection, network, Some(&known))?;
    let projected = Graph::read(connection, network)?;
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
        EventBody::InputPosted {
            target,
            expected_revision,
            intent,
            ..
        } => {
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
            let resolved = resolve_with_graph(connection, network, target, &graph)?;
            ensure!(
                resolved.conversation_id == event.scope_id,
                "input target belongs to another conversation"
            );
            ensure!(
                resolved.task_revision == *expected_revision,
                "input task revision changed"
            );
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
                        .get(source)
                        .context("original task input is missing")?;
                    ensure!(
                        matches!(&original.body, EventBody::InputPosted { message_id, target, expected_revision, .. } if assignment.trigger_message_id.as_ref() == Some(message_id) && target_task(target) == Some(assignment.task_id.as_str()) && *expected_revision == Some(assignment.started_revision)),
                        "not-started evidence is outside the original task input"
                    );
                    let proof = evidence_event
                        .as_ref()
                        .and_then(|hash| known.get(hash))
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
                    let input = known.get(input).context("adopted input is missing")?;
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
                    let proof = known.get(proof).context("assignment evidence is missing")?;
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
                                    connection, network, assignment, *revision, input, proof,
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
                let source = known
                    .iter()
                    .find(|(hash, parent)| {
                        direct_parent(event, hash)
                            && parent.scope_id == cause.conversation_id
                            && match &parent.body {
                                EventBody::InputPosted {
                                    message_id,
                                    attestation: Some(_),
                                    ..
                                } => message_id == &cause.message_id,
                                _ => false,
                            }
                    })
                    .map(|(_, event)| event)
                    .context("Bot command must directly follow the user's signed request")?;
                let cause_hash = known
                    .iter()
                    .find(|(_, candidate)| std::ptr::eq(*candidate, source))
                    .map(|(hash, _)| hash.as_str())
                    .context("command cause is missing")?;
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
                                && assignment.source_event.as_deref() == Some(cause_hash),
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
                        let (owner, epoch) = owner_at(&known, &scope)?;
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
                        || owner_at(&known, &cause.conversation_id)?.0 == event.author,
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
                        .get(&input.source_event)
                        .context("input control source is missing")?;
                    let owner = match &input.target {
                        InputTarget::Work { work_id } => {
                            super::reduce::work_from(connection, network, work_id)?
                                .context("controlled Work is missing")?
                                .target_node
                        }
                        _ => owner_at(&known, &source.scope_id)?.0,
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
                            direct_parent(event, hash) && known.contains_key(hash),
                            "task source must be a known direct dependency"
                        );
                        if attestation.is_none() {
                            let source = &known[hash];
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
                    let (owner, epoch) = owner_at(&known, &scope)?;
                    ensure!(epoch == *bot_owner_epoch, "Bot owner epoch changed");
                    if let Some(work_id) = work_id {
                        let work = super::reduce::work_from(connection, network, work_id)?
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
                _ => {}
            }
            graph.apply("pending", event, VersionVector::new())?;
        }
        _ => {}
    }
    Ok(())
}

fn target_task(target: &InputTarget) -> Option<&str> {
    match target {
        InputTarget::Task { task_id } | InputTarget::ChannelTask { task_id, .. } => Some(task_id),
        _ => None,
    }
}
fn resolve_with_graph(
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
            let found = super::reduce::work_from(connection, network, work_id)?
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
        let schedule = super::bot_settings::schedule_from(connection, network, schedule_id)?
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
    let graph = Graph::read(connection, network)?;
    let resolved = resolve_with_graph(connection, network, target, &graph)?;
    ensure!(
        !super::bots::bot_deleted_from(connection, network, &resolved.conversation_id)?,
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
fn input_from_event(hash: &str, event: Event) -> Option<WorkspaceInput> {
    match event.body {
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
                report_results,
            },
            conversation_id: event.scope_id,
            source_event: hash.into(),
            created_at_ms: event.created_at_ms,
        }),
        _ => None,
    }
}
fn workspace_input_from(
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
            && let Some(input) = input_from_event(&hash, event)
        {
            ensure!(found.is_none(), "input ID is ambiguous in this network");
            found = Some(input);
        }
    }
    Ok(found)
}
#[derive(Clone, Copy)]
enum ResourceQuery<'a> {
    Assignment(Option<&'a str>),
    Work(&'a str),
}

fn resource_conflicts_from(
    connection: &Connection,
    network: &str,
    node: &str,
    roots: &[String],
    except_assignment: Option<&str>,
    graph: &Graph,
    query: ResourceQuery<'_>,
) -> Result<Vec<String>> {
    valid_roots(roots)?;
    let except_work = match query {
        ResourceQuery::Assignment(work) => work,
        ResourceQuery::Work(work) => Some(work),
    };
    let requesting_work = except_work
        .map(|id| super::reduce::work_from(connection, network, id))
        .transpose()?
        .flatten();
    let mut conflicts = BTreeSet::new();
    for assignment in graph.assignments.values().filter(|assignment| {
        assignment.execution_node == node
            && Some(assignment.assignment_id.as_str()) != except_assignment
            && !matches!(
                assignment.state,
                AssignmentState::Completed | AssignmentState::Stopped
            )
    }) {
        if roots.iter().any(|root| {
            assignment
                .resource_roots
                .iter()
                .any(|occupied| overlap(root, occupied))
        }) {
            conflicts.insert(assignment.assignment_id.clone());
        }
    }
    for work in super::reduce::open_works_from(connection, network, None)? {
        let accepted_without_reservation: bool = if work.state == WorkStatus::Accepted
            && matches!(work.kind, WorkKind::Agent(_))
            && work.thread_id.is_none()
            && work.turn_id.is_none()
        {
            connection.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM work_runs
                    WHERE work_id=?1 AND conversation_id=?2
                      AND (state='accepted' OR
                           (state='starting' AND ?3 IS NOT NULL AND thread_id=?3))
                      AND turn_id IS NULL AND codex_epoch IS NULL
                      AND outcome IS NULL AND evidence IS NULL AND probe IS NULL
                      AND result_text='' AND progress='' AND progress_at_ms=0
                 ) AND NOT EXISTS(SELECT 1 FROM identity_meta WHERE key=?4)",
                params![
                    work.work_id,
                    work.conversation_id,
                    work.thread,
                    format!("work-native-submission:{}", work.work_id)
                ],
                |row| row.get(0),
            )?
        } else {
            false
        };
        if accepted_without_reservation {
            continue;
        }
        let own_assignment_work = matches!(query, ResourceQuery::Assignment(_))
            && except_assignment
                .and_then(|id| graph.assignments.get(id))
                .is_some_and(|assignment| {
                    work.trigger == format!("assignment:{}", assignment.assignment_id)
                        && work.dispatcher == assignment.execution_node
                        && bot_scope(connection, network, &assignment.bot_id)
                            .is_ok_and(|scope| scope == work.conversation_id)
                });
        if work.target_node == node
            && !own_assignment_work
            && work.state != WorkStatus::Finished
            && Some(work.work_id.as_str()) != except_work
        {
            let reservation: Option<String> = connection
                .query_row(
                    "SELECT value FROM identity_meta WHERE key=?",
                    [format!("work-native-submission:{}", work.work_id)],
                    |row| row.get(0),
                )
                .optional()?;
            let occupied = match reservation {
                Some(reservation) => {
                    let reservation: serde_json::Value = serde_json::from_str(&reservation)?;
                    let cwd = reservation["native_cwd"]
                        .as_str()
                        .context("native work reservation has no directory")?
                        .to_owned();
                    valid_roots(std::slice::from_ref(&cwd))?;
                    cwd
                }
                None => work.cwd.clone(),
            };
            if roots.iter().any(|root| overlap(root, &occupied)) {
                if matches!(query, ResourceQuery::Work(_))
                    && let Some(requesting) = &requesting_work
                    && super::scheduler::work_shares_native_execution_from(
                        connection, network, requesting, &work,
                    )?
                {
                    continue;
                }
                conflicts.insert(format!("work:{}", work.work_id));
            }
        }
    }
    let mut statement=connection.prepare("SELECT conversation_id,scope,trigger FROM scheduler_jobs WHERE state IN ('claimed','running','cancelling','reconciling')")?;
    let sessions = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (conversation, scope, trigger) in sessions {
        let own_requesting_job = requesting_work.as_ref().is_some_and(|work| {
            work.dispatcher == node
                && work.target_node == node
                && work.conversation_id == conversation
                && work.trigger == trigger
        });
        if own_requesting_job {
            continue;
        }
        let this_assignment = except_assignment
            .and_then(|id| graph.assignments.get(id))
            .is_some_and(|assignment| {
                bot_scope(connection, network, &assignment.bot_id)
                    .is_ok_and(|bot| bot == conversation)
                    && scope
                        == SessionScope::WorkspaceTask {
                            task_id: assignment.task_id.clone(),
                        }
                        .storage_key()
            });
        if this_assignment {
            continue;
        }
        let project: Option<String> = connection
            .query_row(
                "SELECT project FROM bots WHERE network_id=? AND conversation_id=?",
                params![network, conversation],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        if let Some(project) =
            project.and_then(|project| serde_json::from_str::<BotProject>(&project).ok())
            && project.target_node == node
            && roots.iter().any(|root| overlap(root, &project.cwd))
        {
            conflicts.insert(format!("session:{conversation}:{scope}"));
        }
    }
    Ok(conflicts.into_iter().collect())
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
    let graph = Graph::read(connection, network)?;
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
                let message = super::message_from(
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
                    super::message_from(connection, network, &source.conversation_id, reply)?
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
    pub fn workspace_task(&self, network: &str, task_id: &str) -> Result<Option<WorkspaceTask>> {
        workspace_task_from(&self.connection, network, task_id)
    }
    pub fn workspace_assignment(
        &self,
        network: &str,
        assignment_id: &str,
    ) -> Result<Option<Assignment>> {
        workspace_assignment_from(&self.connection, network, assignment_id)
    }
    pub fn workspace_resource_conflicts(
        &self,
        network: &str,
        node: &str,
        roots: &[String],
        except_assignment: Option<&str>,
    ) -> Result<Vec<String>> {
        let graph = Graph::read(&self.connection, network)?;
        let except_work = except_assignment
            .and_then(|id| graph.assignments.get(id))
            .and_then(|assignment| assignment.work_id.as_deref());
        resource_conflicts_from(
            &self.connection,
            network,
            node,
            roots,
            except_assignment,
            &graph,
            ResourceQuery::Assignment(except_work),
        )
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
        let body = EventBody::InputPosted {
            message_id: input.message_id.clone(),
            target: input.route.target.clone(),
            intent: input.route.intent,
            sequence: input.route.sequence,
            expected_revision: input.route.expected_revision,
            text: input.text.clone(),
            attachments: input.attachments.clone(),
            report_results: input.report_results,
            attestation: Some(attestation),
        };
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
            resolved.task_revision == input.route.expected_revision,
            "input task revision changed"
        );
        if let Some(task) = resolved
            .task_id
            .as_ref()
            .and_then(|id| Graph::read(&tx, network).ok()?.tasks.remove(id))
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
        tx.commit()?;
        Ok(stored)
    }
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
        let graph = Graph::read(&tx, network)?;
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
                    InputControl::Release => super::scheduler::workspace_input_releasable_from(
                        &tx,
                        network,
                        &original.conversation_id,
                        message_id,
                    )?,
                    _ => super::scheduler::workspace_input_editable_from(
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
        let graph = Graph::read(&tx, network)?;
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
                super::scheduler::admit_assignment_from(&tx, network, &local, assignment)?;
            } else {
                let scope = bot_scope(&tx, network, &assignment.bot_id)?;
                super::mailbox::queue(
                    &tx,
                    network,
                    &scope,
                    &format!("assignment:{id}"),
                    created_at_ms,
                )?;
            }
        }
        if !duplicate && let Some(message_id) = controlled_input {
            super::scheduler::apply_workspace_input_control_from(
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
    pub fn workspace_state(
        &self,
        network: &str,
        channel_id: Option<&str>,
        task_id: Option<&str>,
        limit: usize,
    ) -> Result<WorkspaceState> {
        let graph = Graph::read(&self.connection, network)?;
        if let Some(channel_id) = channel_id {
            graph.channel(channel_id)?;
        }
        if let Some(task_id) = task_id {
            ensure!(graph.tasks.contains_key(task_id), "task not found");
        }
        let all_inputs = queued_inputs_from(&self.connection, network, &graph)?;
        let mut tasks: Vec<_> = graph
            .tasks
            .into_values()
            .filter(|task| {
                channel_id.is_none_or(|id| task.channel_id.as_deref() == Some(id))
                    && task_id.is_none_or(|id| task.task_id == id)
            })
            .collect();
        tasks.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then(left.task_id.cmp(&right.task_id))
        });
        let truncated = tasks.len() > limit;
        tasks.truncate(limit);
        let ids: BTreeSet<_> = tasks.iter().map(|task| task.task_id.as_str()).collect();
        let mut channels: Vec<_> = graph
            .channels
            .into_values()
            .filter(|channel| {
                channel_id.is_none_or(|id| channel.channel_id == id)
                    && task_id.is_none_or(|_| {
                        tasks.iter().any(|task| {
                            task.channel_id.as_deref() == Some(channel.channel_id.as_str())
                        })
                    })
            })
            .collect();
        let mut assignments: Vec<_> = graph
            .assignments
            .into_values()
            .filter(|assignment| ids.contains(assignment.task_id.as_str()))
            .collect();
        channels.truncate(limit);
        assignments.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then(left.assignment_id.cmp(&right.assignment_id))
        });
        let inputs = all_inputs
            .into_iter()
            .filter(|input| {
                target_task(&input.input.route.target).is_some_and(|id| ids.contains(id))
                    || (task_id.is_none()
                        && channel_id.is_none()
                        && target_task(&input.input.route.target).is_none())
            })
            .collect();
        Ok(WorkspaceState {
            inputs,
            channels,
            tasks,
            assignments,
            coverage: WorkspaceCoverage { limit, truncated },
        })
    }
    pub fn workspace_history(
        &self,
        network: &str,
        channel_id: Option<&str>,
        task_id: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<WorkspaceHistoryItem>, bool)> {
        let graph = Graph::read(&self.connection, network)?;
        if let Some(channel_id) = channel_id {
            graph.channel(channel_id)?;
        }
        if let Some(task_id) = task_id {
            let task = graph.tasks.get(task_id).context("task not found")?;
            ensure!(
                channel_id.is_none_or(|id| task.channel_id.as_deref() == Some(id)),
                "task is outside the channel"
            );
        }
        let task_matches = |id: &str| {
            graph.tasks.get(id).is_some_and(|task| {
                task_id.is_none_or(|wanted| id == wanted)
                    && channel_id.is_none_or(|wanted| task.channel_id.as_deref() == Some(wanted))
            })
        };
        let mut statement=self.connection.prepare("SELECT hash,envelope FROM events WHERE network_id=? AND effective=1 AND kind IN ('workspace_commanded','input_posted','message_posted','work_dispatched','work_progress','work_finished') ORDER BY lamport,created_at_ms,hash")?;
        let rows = statement.query_map([network], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut all = Vec::new();
        for row in rows {
            let (hash, envelope) = row?;
            let envelope: SignedEvent = serde_json::from_str(&envelope)?;
            let event: Event = serde_json::from_str(&envelope.canonical_body)?;
            all.push((hash, event));
        }
        let input_ids: BTreeSet<_> = all
            .iter()
            .filter_map(|(_, event)| match &event.body {
                EventBody::InputPosted {
                    message_id, target, ..
                } if target_task(target).is_some_and(task_matches) => {
                    Some((event.scope_id.clone(), message_id.clone()))
                }
                _ => None,
            })
            .collect();
        let mut reply_ids = BTreeSet::new();
        for (scope, input) in &input_ids {
            let steered:Option<String>=self.connection.query_row("SELECT steered_into FROM messages WHERE network_id=? AND conversation_id=? AND message_id=?",params![network,scope,input],|row|row.get(0)).optional()?.flatten();
            if let Some(reply) = steered {
                reply_ids.insert((scope.clone(), reply));
            }
        }
        let work_ids: BTreeSet<_> = graph
            .assignments
            .values()
            .filter(|assignment| task_matches(&assignment.task_id))
            .filter_map(|assignment| assignment.work_id.as_deref())
            .collect();
        let mut events = Vec::new();
        for (hash, event) in all {
            let matches = match &event.body {
                EventBody::InputPosted { target, .. } => {
                    target_task(target).is_some_and(task_matches)
                }
                EventBody::MessagePosted { message_id, .. } => {
                    message_target_from(&self.connection, network, &event.scope_id, message_id)?
                        .as_ref()
                        .and_then(target_task)
                        .is_some_and(task_matches)
                        || reply_ids.contains(&(event.scope_id.clone(), message_id.clone()))
                }
                EventBody::WorkspaceCommanded {
                    command,
                    command_id,
                    ..
                } => {
                    let effective = graph
                        .commands
                        .get(command_id)
                        .is_some_and(|result| result.event_hash == hash && result.error.is_none());
                    effective
                        && match command {
                            WorkspaceCommand::ControlInput { message_id, .. } => {
                                graph.inputs.get(message_id).is_some_and(|input| {
                                    target_task(&input.target).is_some_and(task_matches)
                                })
                            }
                            WorkspaceCommand::AssignmentProgress { assignment_id, .. } => graph
                                .assignments
                                .get(assignment_id)
                                .is_some_and(|assignment| task_matches(&assignment.task_id)),
                            _ => {
                                if let Some(task) = command.task_id() {
                                    task_matches(task)
                                } else {
                                    task_id.is_none()
                                        && channel_id
                                            .is_none_or(|id| command.channel_id() == Some(id))
                                }
                            }
                        }
                }
                body => {
                    if let Some(id) = body.work_id()
                        && self
                            .work(network, id)?
                            .is_some_and(|work| work.conversation_id == event.scope_id)
                    {
                        work_ids.contains(id)
                            || self
                                .work_input_target(network, &event.scope_id, id)?
                                .as_ref()
                                .and_then(target_task)
                                .is_some_and(task_matches)
                    } else {
                        false
                    }
                }
            };
            if matches {
                let speaker = history_speaker(&self.connection, network, &graph, &event)?;
                events.push(WorkspaceHistoryItem {
                    speaker,
                    event_hash: hash,
                    created_at_ms: event.created_at_ms,
                    scope_id: event.scope_id,
                    kind: event.body.kind().into(),
                    body: event.body,
                });
            }
        }
        let truncated = events.len() > limit;
        if truncated {
            events.drain(..events.len() - limit);
        }
        Ok((events, truncated))
    }
}

pub(crate) fn message_target_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    message_id: &str,
) -> Result<Option<InputTarget>> {
    let target: Option<String> = connection
        .query_row(
            "SELECT target FROM messages WHERE network_id=? AND conversation_id=? AND message_id=?",
            params![network, conversation, message_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    target
        .map(|target| serde_json::from_str(&target).map_err(Into::into))
        .transpose()
}
pub(crate) fn refresh_reply_targets(
    connection: &Connection,
    network: &str,
    conversation: &str,
    message_id: &str,
) -> Result<()> {
    connection.execute(
        "UPDATE messages AS answer SET target=(SELECT json_extract(question.body,'$.target') FROM questions AS question WHERE question.network_id=answer.network_id AND question.conversation_id=answer.conversation_id AND question.question_id=answer.answers AND json_type(question.body,'$.target')='object') WHERE answer.network_id=? AND answer.conversation_id=? AND answer.message_id=? AND answer.role='user' AND answer.answers IS NOT NULL",
        params![network, conversation, message_id],
    )?;
    let mut changed = BTreeSet::from([message_id.to_owned()]);
    while let Some(message_id) = changed.pop_first() {
        let replies: Vec<String> = connection.prepare(
            "SELECT message_id FROM messages WHERE network_id=?1 AND conversation_id=?2 AND in_reply_to=?3 AND role='assistant' UNION SELECT message_id FROM messages WHERE network_id=?1 AND conversation_id=?2 AND message_id=?3 AND role='assistant' UNION SELECT steered_into FROM messages WHERE network_id=?1 AND conversation_id=?2 AND message_id=?3 AND steered_into IS NOT NULL",
        )?.query_map(params![network, conversation, message_id], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
        for reply in replies {
            if refresh_reply_target(connection, network, conversation, &reply)? {
                changed.insert(reply);
            }
        }
    }
    Ok(())
}

fn refresh_reply_target(
    connection: &Connection,
    network: &str,
    conversation: &str,
    message_id: &str,
) -> Result<bool> {
    let Some((author, in_reply_to, previous)) = connection.query_row(
        "SELECT author,in_reply_to,target FROM messages WHERE network_id=? AND conversation_id=? AND message_id=? AND role='assistant'",
        params![network, conversation, message_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<String>>(2)?)),
    ).optional()? else { return Ok(false) };
    let mut target: Option<String> = connection.query_row(
        "SELECT COALESCE((SELECT target FROM messages WHERE network_id=?1 AND conversation_id=?2 AND message_id=?4),(SELECT target FROM messages WHERE network_id=?1 AND conversation_id=?2 AND steered_into=?3 AND role='user' AND target IS NOT NULL ORDER BY rank LIMIT 1))",
        params![network, conversation, message_id, in_reply_to],
        |row| row.get(0),
    )?;
    if let Some(in_reply_to) = in_reply_to
        && let Some(assignment) = workspace_assignment_from(connection, network, &in_reply_to)?
        && assignment.execution_node == author
        && bot_scope(connection, network, &assignment.bot_id)? == conversation
    {
        target = Some(serde_json::to_string(&InputTarget::Task {
            task_id: assignment.task_id,
        })?);
    }
    if target == previous {
        return Ok(false);
    }
    connection.execute(
        "UPDATE messages SET target=? WHERE network_id=? AND conversation_id=? AND message_id=?",
        params![target, network, conversation, message_id],
    )?;
    Ok(true)
}

pub(crate) fn refresh_question_targets(
    connection: &Connection,
    network: &str,
    question_id: &str,
) -> Result<()> {
    let answers: Vec<(String, String)> = connection.prepare(
        "SELECT conversation_id,message_id FROM messages WHERE network_id=? AND answers=? AND role='user'",
    )?.query_map(params![network, question_id], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    for (conversation, message_id) in answers {
        refresh_reply_targets(connection, network, &conversation, &message_id)?;
    }
    Ok(())
}

fn queued_inputs_from(
    connection: &Connection,
    network: &str,
    graph: &Graph,
) -> Result<Vec<QueuedInput>> {
    graph
        .queue_order
        .iter()
        .enumerate()
        .map(|(index, message_id)| queued_input_at(connection, network, graph, index, message_id))
        .collect()
}
fn queued_input_at(
    connection: &Connection,
    network: &str,
    graph: &Graph,
    index: usize,
    message_id: &str,
) -> Result<QueuedInput> {
    let control = graph
        .inputs
        .get(message_id)
        .context("queue input projection is missing")?;
    let original = log::event_from(connection, network, &control.source_event)?
        .and_then(|event| input_from_event(&control.source_event, event))
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
pub(crate) fn queued_workspace_input_from(
    connection: &Connection,
    network: &str,
    message_id: &str,
) -> Result<Option<QueuedInput>> {
    let graph = Graph::read(connection, network)?;
    graph
        .queue_order
        .iter()
        .position(|id| id == message_id)
        .map(|index| queued_input_at(connection, network, &graph, index, message_id))
        .transpose()
}

impl Store {
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
            &Graph::read(&self.connection, network)?,
        )
    }
}

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
                            format!("assignment:{}", existing.assignment_id),
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
        let graph = Graph::read(&self.connection, network)?;
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
                || super::scheduler::workspace_assignment_followups_pending_from(
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
        let graph = Graph::read(&self.connection, network)?;
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
                    .and_then(|event| input_from_event(hash, event))
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
                    .and_then(|event| input_from_event(source, event))
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

fn history_speaker(
    connection: &Connection,
    network: &str,
    graph: &Graph,
    event: &Event,
) -> Result<WorkspaceSpeaker> {
    if let Some(attestation) = event.body.attestation() {
        return Ok(WorkspaceSpeaker::Human {
            device_id: attestation.device_id.clone(),
        });
    }
    let bot = match &event.body {
        EventBody::WorkspaceCommanded {
            command: WorkspaceCommand::AssignmentProgress { assignment_id, .. },
            ..
        } => graph
            .assignments
            .get(assignment_id)
            .filter(|assignment| assignment.execution_node == event.author)
            .map(|assignment| assignment.bot_id.clone()),
        EventBody::WorkspaceCommanded {
            acting_assignment: Some(id),
            ..
        } => graph
            .assignments
            .get(id)
            .filter(|assignment| assignment.execution_node == event.author)
            .map(|assignment| assignment.bot_id.clone()),
        EventBody::WorkspaceCommanded {
            cause: Some(cause), ..
        } => connection
            .query_row(
                "SELECT bot_id FROM bots WHERE network_id=? AND conversation_id=?",
                params![network, cause.conversation_id],
                |row| row.get(0),
            )
            .optional()?,
        EventBody::MessagePosted {
            role: Role::Assistant,
            ..
        } => {
            let known = ancestors(connection, network, event)?;
            if owner_at(&known, &event.scope_id).is_ok_and(|(owner, _)| owner == event.author) {
                connection
                    .query_row(
                        "SELECT bot_id FROM bots WHERE network_id=? AND conversation_id=?",
                        params![network, event.scope_id],
                        |row| row.get(0),
                    )
                    .optional()?
            } else {
                None
            }
        }
        body => body
            .work_id()
            .and_then(|work| {
                graph.assignments.values().find(|assignment| {
                    assignment.work_id.as_deref() == Some(work)
                        && assignment.execution_node == event.author
                })
            })
            .map(|assignment| assignment.bot_id.clone()),
    };
    Ok(bot.map_or_else(
        || WorkspaceSpeaker::Node {
            device_id: event.author.clone(),
        },
        |bot_id| WorkspaceSpeaker::Bot { bot_id },
    ))
}

pub(crate) fn task_execution_allowed_from(
    connection: &Connection,
    network: &str,
    task_id: &str,
) -> Result<bool> {
    let graph = Graph::read(connection, network)?;
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
    if super::bots::bot_deleted_from(connection, network, &scope)? {
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
    let graph = Graph::read(connection, network)?;
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
    let graph = Graph::read(connection, network)?;
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
    let graph = Graph::read(connection, network)?;
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
    let mut known = BTreeMap::new();
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
        let Some(event) = log::event_from(connection, network, hash)? else {
            return Ok(false);
        };
        known.extend(ancestors(connection, network, &event)?);
        known.insert(hash.clone(), event);
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
    let (owner, epoch) = owner_at(&known, &scope)?;
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

pub(crate) fn workspace_assignment_from(
    connection: &Connection,
    network: &str,
    assignment_id: &str,
) -> Result<Option<Assignment>> {
    let encoded: Option<String> = connection
        .query_row(
            "SELECT json_extract(state, '$.assignments.' || json_quote(?2)) FROM workspace_projection WHERE network_id=?1",
            params![network, assignment_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    encoded
        .map(|encoded| serde_json::from_str(&encoded).map_err(Into::into))
        .transpose()
}

pub(crate) fn assignment_admission_allowed_from(
    connection: &Connection,
    network: &str,
    assignment: &Assignment,
) -> Result<bool> {
    let graph = Graph::read(connection, network)?;
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

fn command_in_task(graph: &Graph, task_id: &str, command: &WorkspaceCommand) -> bool {
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

pub(crate) fn workspace_task_from(
    connection: &Connection,
    network: &str,
    task_id: &str,
) -> Result<Option<WorkspaceTask>> {
    Ok(Graph::read(connection, network)?.tasks.remove(task_id))
}
pub(crate) fn workspace_assignment_for_work_from(
    connection: &Connection,
    network: &str,
    work_id: &str,
) -> Result<Option<Assignment>> {
    Ok(Graph::read(connection, network)?
        .assignments
        .into_values()
        .filter(|assignment| assignment.work_id.as_deref() == Some(work_id))
        .max_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then(left.assignment_id.cmp(&right.assignment_id))
        }))
}

impl Store {
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

impl Store {
    pub fn workspace_active_assignments(&self, network: &str) -> Result<Vec<Assignment>> {
        Ok(Graph::read(&self.connection, network)?
            .assignments
            .into_values()
            .filter(|assignment| {
                !matches!(
                    assignment.state,
                    AssignmentState::Completed | AssignmentState::Stopped
                )
            })
            .collect())
    }
}

impl Store {
    pub fn workspace_command_recorded(&self, network: &str, command_id: &str) -> Result<bool> {
        Ok(Graph::read(&self.connection, network)?
            .commands
            .contains_key(command_id))
    }
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
    let direct = format!("assignment:{}", assignment.assignment_id);
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

fn input_assignment_binding_from(
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

fn assignment_for_input_from(
    connection: &Connection,
    network: &str,
    input: &WorkspaceInput,
    graph: &Graph,
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
    let known = ancestors(connection, network, &source)?;
    let at_input = graph_at(connection, network, Some(&known))?;
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
    let graph = Graph::read(connection, network)?;
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
    let graph = Graph::read(connection, network)?;
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
            input_from_event(&source_event, source).context("waiting source is not an input")?;
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

pub(crate) fn workspace_input_resources_blocked_from(
    connection: &Connection,
    network: &str,
    local: &str,
    job: &crate::scheduling::SchedulerJob,
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
    let graph = Graph::read(connection, network)?;
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
    let SessionScope::WorkspaceTask { task_id } = job.session_scope() else {
        return Ok(None);
    };
    if job.kind == TriggerKind::WorkResult {
        let Some(work_id) = job.trigger.strip_prefix("work:") else {
            return Ok(None);
        };
        let (conversation, dispatcher, trigger) =
            if let Some(work) = super::reduce::work_from(connection, network, work_id)? {
                (work.conversation_id, work.dispatcher, work.trigger)
            } else if let Some(dispatch) = super::work::dispatch_from(connection, work_id)?
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
        let (reply_to, kind) = match trigger.split_once(':') {
            Some(("assignment", id)) => (id.to_owned(), TriggerKind::Human),
            Some(("message", id)) => {
                let Some(message) = super::message_from(connection, network, &conversation, id)?
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
        return workspace_assignment_for_job_from(connection, network, local, &origin);
    }
    let graph = Graph::read(connection, network)?;
    if let Some(id) = job.trigger.strip_prefix("assignment:") {
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
            super::message_from(connection, network, &job.conversation_id, &job.reply_to)?
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
        let Some(asked) = super::event_about_from(
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
        let known = ancestors(connection, network, &asked.event)?;
        let at_question = graph_at(connection, network, Some(&known))?;
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
            || format!("assignment:{}", original.assignment_id),
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
    let assignment = assignment_for_input_from(connection, network, &input, &graph)?;
    Ok(assignment.filter(|assignment| {
        assignment.execution_node == local
            && bot_scope(connection, network, &assignment.bot_id)
                .is_ok_and(|scope| scope == job.conversation_id)
    }))
}

pub(crate) fn assignment_resources_available_from(
    connection: &Connection,
    network: &str,
    assignment: &Assignment,
) -> Result<bool> {
    let graph = Graph::read(connection, network)?;
    let Some(stored) = graph.assignments.get(&assignment.assignment_id) else {
        return Ok(false);
    };
    if stored.event_hash != assignment.event_hash
        || stored.execution_node != assignment.execution_node
        || stored.resource_roots != assignment.resource_roots
        || stored.work_id != assignment.work_id
    {
        return Ok(false);
    }
    Ok(resource_conflicts_from(
        connection,
        network,
        &stored.execution_node,
        &stored.resource_roots,
        Some(&stored.assignment_id),
        &graph,
        ResourceQuery::Assignment(stored.work_id.as_deref()),
    )?
    .is_empty())
}

pub(crate) fn work_resources_available_from(
    connection: &Connection,
    network: &str,
    work: &WorkProjection,
) -> Result<bool> {
    if work.cwd.is_empty() {
        return Ok(false);
    }
    let graph = Graph::read(connection, network)?;
    let assignment = work
        .trigger
        .strip_prefix("assignment:")
        .and_then(|id| graph.assignments.get(id))
        .filter(|assignment| {
            work.dispatcher == assignment.execution_node
                && bot_scope(connection, network, &assignment.bot_id)
                    .is_ok_and(|scope| scope == work.conversation_id)
        });
    Ok(resource_conflicts_from(
        connection,
        network,
        &work.target_node,
        std::slice::from_ref(&work.cwd),
        assignment.map(|assignment| assignment.assignment_id.as_str()),
        &graph,
        ResourceQuery::Work(&work.work_id),
    )?
    .is_empty())
}

pub(crate) fn workspace_input_release_allowed_from(
    connection: &Connection,
    network: &str,
    local: &str,
    message_id: &str,
) -> Result<bool> {
    let graph = Graph::read(connection, network)?;
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
