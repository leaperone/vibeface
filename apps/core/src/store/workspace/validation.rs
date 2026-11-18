//! Shape and portable path validation for signed workspace records.
use crate::{
    model::{Attachment, MAX_BLOB_BYTES, valid_attachment_name},
    workspace::{AssignmentState, InputControl, InputTarget, WorkspaceCommand},
};
use anyhow::{Result, ensure};
use std::collections::BTreeSet;

pub(super) fn valid_id(value: &str) -> Result<()> {
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

pub(super) fn valid_roots(roots: &[String]) -> Result<()> {
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
pub(super) fn overlap(left: &str, right: &str) -> bool {
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
