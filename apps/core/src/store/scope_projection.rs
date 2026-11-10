//! Read-model destinations follow signed inputs and verified execution bindings.
use super::{Store, bots::bot_for_conversation_from, log, reduce, work, workspace};
use crate::{
    model::{Event, EventBody, Role, Subject},
    scheduling::{SessionScope, parse_schedule_trigger},
    workspace::InputTarget,
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension as _, params};
use std::collections::BTreeSet;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Message(String),
    Work(String),
    Trigger(String),
    Question(String),
}

enum Destination {
    Main(Option<String>),
    Scoped(InputTarget),
    Unknown,
}

impl Destination {
    fn from_target(target: InputTarget) -> Self {
        match target {
            InputTarget::BotMain { bot_id } => Self::Main(Some(bot_id)),
            target => Self::Scoped(target),
        }
    }

    fn target(self) -> Option<InputTarget> {
        match self {
            Self::Main(bot_id) => bot_id.map(|bot_id| InputTarget::BotMain { bot_id }),
            Self::Scoped(target) => Some(target),
            Self::Unknown => None,
        }
    }
}

fn main_target_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
) -> Result<Destination> {
    Ok(Destination::Main(
        bot_for_conversation_from(connection, network, conversation)?.map(|bot| bot.bot_id),
    ))
}

fn attributed_session_target_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    about: &Subject,
    before_rank: Option<&str>,
) -> Result<Option<InputTarget>> {
    let Some(agent) = about.agent else {
        return Ok(None);
    };
    let attributed: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_owners s
         WHERE s.network_id=?1 AND s.node=?2 AND s.session_id=?3 AND s.conversation_id=?4
            AND s.attribution IS NOT NULL AND json_extract(s.attribution,'$.agent')=?5
            AND s.muted=0 AND (?6 IS NULL OR s.rank<=?6)
            AND NOT EXISTS(SELECT 1 FROM bot_deletions d
                WHERE d.network_id=s.network_id AND d.conversation_id=s.conversation_id)
            AND NOT EXISTS(SELECT 1 FROM works w
                WHERE w.network_id=s.network_id AND w.target_node=s.node
                    AND (w.thread_id=s.session_id OR json_extract(w.dispatch,'$.thread')=s.session_id)
                    AND json_extract(w.dispatch,'$.resolved.provider')=?5
                    AND (?6 IS NULL OR w.dispatch_rank<=?6)))",
        params![network, about.node, about.session_id, conversation, agent.name(), before_rank],
        |row| row.get(0),
    )?;
    if attributed {
        Ok(main_target_from(connection, network, conversation)?.target())
    } else {
        Ok(None)
    }
}

fn task_target_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    task_id: &str,
) -> Result<Option<InputTarget>> {
    let Some(task) = workspace::workspace_task_from(connection, network, task_id)? else {
        return Ok(None);
    };
    let Some(bot) = bot_for_conversation_from(connection, network, conversation)? else {
        return Ok(None);
    };
    Ok(Some(match task.channel_id {
        Some(channel_id) => InputTarget::ChannelTask {
            channel_id,
            task_id: task.task_id,
            bot_id: bot.bot_id,
        },
        None => InputTarget::Task {
            task_id: task.task_id,
        },
    }))
}

pub(crate) fn session_target_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    scope: &SessionScope,
) -> Result<Option<InputTarget>> {
    match scope {
        SessionScope::Main => Ok(main_target_from(connection, network, conversation)?.target()),
        SessionScope::WorkspaceTask { task_id } => {
            task_target_from(connection, network, conversation, task_id)
        }
        SessionScope::Task { work_id } => {
            if let Some(assignment) =
                workspace::workspace_assignment_from(connection, network, work_id)?.or(
                    workspace::workspace_assignment_for_work_from(connection, network, work_id)?,
                )
            {
                return task_target_from(connection, network, conversation, &assignment.task_id);
            }
            Ok(reduce::work_from(connection, network, work_id)?
                .filter(|work| work.conversation_id == conversation)
                .map(|_| InputTarget::Work {
                    work_id: work_id.clone(),
                }))
        }
        SessionScope::Routine { trigger } => Ok(bot_for_conversation_from(
            connection,
            network,
            conversation,
        )?
        .map(|bot| InputTarget::Routine {
            bot_id: bot.bot_id,
            trigger: trigger.clone(),
        })),
    }
}

fn target_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    mut source: Source,
) -> Result<Destination> {
    let mut conversation = conversation.to_owned();
    let mut visited = BTreeSet::new();
    while visited.insert((conversation.clone(), source.clone())) {
        source = match source {
            Source::Message(message_id) => {
                if let Some(target) =
                    workspace::message_target_from(connection, network, &conversation, &message_id)?
                {
                    return Ok(Destination::from_target(target));
                }
                let hash: Option<String> = connection
                    .query_row(
                        "SELECT event_hash FROM messages WHERE network_id=? AND conversation_id=? AND message_id=?",
                        params![network, conversation, message_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                let event = hash
                    .map(|hash| log::event_from(connection, network, &hash))
                    .transpose()?
                    .flatten();
                match event.map(|event| event.body) {
                    Some(EventBody::InputPosted { target, .. }) => {
                        return Ok(Destination::from_target(target));
                    }
                    Some(EventBody::MessagePosted {
                        answers: Some(question),
                        ..
                    }) => Source::Question(question),
                    Some(EventBody::BotMessaged { .. }) => {
                        return main_target_from(connection, network, &conversation);
                    }
                    Some(EventBody::MessagePosted {
                        role: Role::Assistant,
                        in_reply_to: Some(reply_to),
                        ..
                    }) => {
                        if let Some(assignment) =
                            workspace::workspace_assignment_from(connection, network, &reply_to)?
                        {
                            return Ok(task_target_from(
                                connection,
                                network,
                                &conversation,
                                &assignment.task_id,
                            )?
                            .map_or(Destination::Unknown, Destination::from_target));
                        }
                        if parse_schedule_trigger(&reply_to).is_some() {
                            Source::Trigger(reply_to)
                        } else {
                            Source::Message(reply_to)
                        }
                    }
                    _ => return Ok(Destination::Unknown),
                }
            }
            Source::Question(question_id) => {
                let question: Option<(String, String)> = connection
                    .query_row(
                        "SELECT body,rank FROM questions WHERE network_id=? AND conversation_id=? AND question_id=?",
                        params![network, conversation, question_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let Some((body, rank)) = question else {
                    return Ok(Destination::Unknown);
                };
                let EventBody::QuestionAsked { target, about, .. } = serde_json::from_str(&body)?
                else {
                    return Ok(Destination::Unknown);
                };
                let target = match target {
                    Some(target) => Some(target),
                    // A later answer may bind this thread to Work without changing the question's scope.
                    None => attributed_session_target_from(
                        connection,
                        network,
                        &conversation,
                        &about,
                        Some(&rank),
                    )?,
                };
                return Ok(target.map_or(Destination::Unknown, Destination::from_target));
            }
            Source::Work(work_id) => {
                if let Some(assignment) =
                    workspace::workspace_assignment_for_work_from(connection, network, &work_id)?
                {
                    return Ok(task_target_from(
                        connection,
                        network,
                        &conversation,
                        &assignment.task_id,
                    )?
                    .map_or(Destination::Unknown, Destination::from_target));
                }
                let origin = match reduce::work_from(connection, network, &work_id)? {
                    Some(work) => Some((work.conversation_id, work.trigger)),
                    None => work::dispatch_from(connection, &work_id)?
                        .map(|dispatch| (dispatch.spec.conversation, dispatch.spec.trigger)),
                };
                let Some((scope, trigger)) = origin else {
                    return Ok(Destination::Unknown);
                };
                conversation = scope;
                Source::Trigger(trigger)
            }
            Source::Trigger(trigger) => match trigger.split_once(':') {
                Some(("message" | "bot_message", message)) => Source::Message(message.into()),
                Some(("work", work_id)) => Source::Work(work_id.into()),
                Some(("assignment", assignment_id)) => {
                    let Some(assignment) =
                        workspace::workspace_assignment_from(connection, network, assignment_id)?
                    else {
                        return Ok(Destination::Unknown);
                    };
                    if bot_for_conversation_from(connection, network, &conversation)?
                        .is_none_or(|bot| bot.bot_id != assignment.bot_id)
                    {
                        return Ok(Destination::Unknown);
                    }
                    return Ok(task_target_from(
                        connection,
                        network,
                        &conversation,
                        &assignment.task_id,
                    )?
                    .map_or(Destination::Unknown, Destination::from_target));
                }
                Some(("task", work_id)) => {
                    return Ok(session_target_from(
                        connection,
                        network,
                        &conversation,
                        &SessionScope::Task {
                            work_id: work_id.into(),
                        },
                    )?
                    .map_or(Destination::Unknown, Destination::from_target));
                }
                Some(("schedule", _)) if parse_schedule_trigger(&trigger).is_some() => {
                    return Ok(session_target_from(
                        connection,
                        network,
                        &conversation,
                        &SessionScope::Routine { trigger },
                    )?
                    .map_or(Destination::Unknown, Destination::from_target));
                }
                _ => return Ok(Destination::Unknown),
            },
        };
    }
    Ok(Destination::Unknown)
}

pub(crate) fn message_is_main_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    message_id: &str,
) -> Result<bool> {
    Ok(matches!(
        target_from(
            connection,
            network,
            conversation,
            Source::Message(message_id.into())
        )?,
        Destination::Main(_)
    ))
}

pub(crate) fn trigger_input_target_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    trigger: &str,
) -> Result<Option<InputTarget>> {
    Ok(target_from(
        connection,
        network,
        conversation,
        Source::Trigger(trigger.into()),
    )?
    .target())
}

pub(crate) fn question_input_target_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    question_id: &str,
) -> Result<Option<InputTarget>> {
    Ok(target_from(
        connection,
        network,
        conversation,
        Source::Question(question_id.into()),
    )?
    .target())
}

pub(crate) fn question_event_input_target_from(
    connection: &Connection,
    network: &str,
    event: &Event,
) -> Result<Option<InputTarget>> {
    let EventBody::QuestionAsked {
        question_id,
        target,
        ..
    } = &event.body
    else {
        return Ok(None);
    };
    if let Some(target) = target {
        return Ok(Some(target.clone()));
    }
    if !super::event_about_from(
        connection,
        network,
        &event.scope_id,
        "question_asked",
        question_id,
    )?
    .is_some_and(|question| question.event == *event)
    {
        return Ok(None);
    }
    question_input_target_from(connection, network, &event.scope_id, question_id)
}

pub(crate) fn trigger_is_main_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    trigger: &str,
) -> Result<bool> {
    Ok(matches!(
        target_from(
            connection,
            network,
            conversation,
            Source::Trigger(trigger.into())
        )?,
        Destination::Main(_)
    ))
}

pub(crate) fn work_is_main_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    work_id: &str,
) -> Result<bool> {
    Ok(matches!(
        target_from(
            connection,
            network,
            conversation,
            Source::Work(work_id.into())
        )?,
        Destination::Main(_)
    ))
}

pub(crate) fn notification_target_from(
    connection: &Connection,
    network: &str,
    hash: &str,
) -> Result<Option<InputTarget>> {
    let Some(event) = log::event_from(connection, network, hash)? else {
        return Ok(None);
    };
    match event.body {
        EventBody::MessagePosted { message_id, .. } | EventBody::BotMessaged { message_id, .. } => {
            Ok(target_from(
                connection,
                network,
                &event.scope_id,
                Source::Message(message_id),
            )?
            .target())
        }
        EventBody::WorkFinished { work_id, .. } => {
            Ok(target_from(connection, network, &event.scope_id, Source::Work(work_id))?.target())
        }
        EventBody::QuestionAsked { .. } => {
            question_event_input_target_from(connection, network, &event)
        }
        EventBody::SessionAttributed { .. } => {
            Ok(main_target_from(connection, network, &event.scope_id)?.target())
        }
        _ => Ok(None),
    }
}

impl Store {
    pub(crate) fn work_source_events(
        &self,
        network: &str,
        conversation: &str,
        work_id: &str,
    ) -> Result<Vec<String>> {
        let mut query = self.connection.prepare(
            "SELECT hash FROM events WHERE network_id=? AND scope_id=? AND subject=?
             AND effective=1 AND kind IN ('work_dispatched','work_finished')
             ORDER BY lamport,author,seq LIMIT 64",
        )?;
        Ok(query
            .query_map(params![network, conversation, work_id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn work_input_target(
        &self,
        network: &str,
        conversation: &str,
        work_id: &str,
    ) -> Result<Option<InputTarget>> {
        Ok(target_from(
            &self.connection,
            network,
            conversation,
            Source::Work(work_id.into()),
        )?
        .target())
    }

    pub(crate) fn trigger_input_target(
        &self,
        network: &str,
        conversation: &str,
        trigger: &str,
    ) -> Result<Option<InputTarget>> {
        trigger_input_target_from(&self.connection, network, conversation, trigger)
    }

    pub(crate) fn native_input_target(
        &self,
        network: &str,
        conversation: &str,
        thread: &str,
        turn: &str,
    ) -> Result<Option<InputTarget>> {
        let mut target = None;
        for candidate in self.native_input_targets(network, conversation, thread, turn)? {
            ensure!(
                target.as_ref().is_none_or(|target| target == &candidate),
                "native session has conflicting input targets"
            );
            target.get_or_insert(candidate);
        }
        Ok(target)
    }

    pub(crate) fn attributed_session_input_target(
        &self,
        network: &str,
        conversation: &str,
        about: &Subject,
    ) -> Result<Option<InputTarget>> {
        attributed_session_target_from(&self.connection, network, conversation, about, None)
    }

    pub(super) fn native_input_targets(
        &self,
        network: &str,
        conversation: &str,
        thread: &str,
        turn: &str,
    ) -> Result<Vec<InputTarget>> {
        let foreign: bool = self.connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM scheduler_sessions WHERE native_id=?1 AND conversation_id<>?2
                UNION ALL SELECT 1 FROM manager_turns t JOIN scheduler_jobs j ON j.attempt_id=t.attempt_id
                    WHERE t.thread_id=?1 AND j.conversation_id<>?2)",
            params![thread, conversation],
            |row| row.get(0),
        )?;
        ensure!(!foreign, "native session belongs to another conversation");
        let bindings = self
            .connection
            .prepare(
                "SELECT r.work_id,COALESCE(r.turn_id,w.turn_id) FROM work_runs r LEFT JOIN works w
                ON w.network_id=?1 AND w.work_id=r.work_id
             WHERE r.thread_id=?2 OR w.thread_id=?2 OR json_extract(w.dispatch,'$.thread')=?2
             ORDER BY r.work_id",
            )?
            .query_map(params![network, thread], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut reassigned = false;
        let mut work_ids = Vec::new();
        for (work_id, work_turn) in bindings {
            let work = self
                .work_record(&work_id)?
                .context("native work binding is missing")?;
            let owner: Option<String> = self
                .connection
                .query_row(
                    "SELECT s.conversation_id FROM session_owners s JOIN works w
                    ON w.network_id=s.network_id AND w.work_id=?3 AND w.target_node=s.node
                 WHERE s.network_id=?1 AND s.session_id=?2 AND s.attribution IS NOT NULL
                    AND s.muted=0 AND s.rank>w.dispatch_rank
                    AND NOT EXISTS(SELECT 1 FROM bot_deletions d
                        WHERE d.network_id=s.network_id AND d.conversation_id=s.conversation_id)",
                    params![network, thread, work.work_id],
                    |row| row.get(0),
                )
                .optional()?;
            if work.conversation != conversation {
                ensure!(
                    work.state.finished() && owner.as_deref() == Some(conversation),
                    "native session belongs to another conversation"
                );
                reassigned = true;
            } else if work.state.finished() {
                ensure!(
                    owner.as_deref().is_none_or(|owner| owner == conversation),
                    "native session was handed to another conversation"
                );
            }
            if work.conversation == conversation
                && (turn.is_empty()
                    || work_turn
                        .as_deref()
                        .is_none_or(|work_turn| work_turn == turn))
            {
                work_ids.push(work_id);
            }
        }
        let mut statement = self.connection.prepare(
            "SELECT j.scope FROM manager_turns t JOIN scheduler_jobs j ON j.attempt_id=t.attempt_id
             WHERE j.conversation_id=?1 AND t.thread_id=?2 AND (?3='' OR t.turn_id=?3 OR t.turn_id IS NULL)
             UNION SELECT scope FROM scheduler_sessions WHERE conversation_id=?1 AND native_id=?2",
        )?;
        let scopes = statement
            .query_map(params![conversation, thread, turn], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut candidates = Vec::new();
        if reassigned {
            candidates.push(
                main_target_from(&self.connection, network, conversation)?
                    .target()
                    .context("the native session's new owner has no Bot")?,
            );
        }
        for scope in scopes {
            candidates.push(
                session_target_from(
                    &self.connection,
                    network,
                    conversation,
                    &SessionScope::from_storage_key(&scope)?,
                )?
                .context("native session has an unknown scope target")?,
            );
        }
        for work_id in work_ids {
            candidates.push(InputTarget::Work { work_id });
        }
        let mut targets = Vec::new();
        let mut scope = None;
        for candidate in candidates {
            let candidate = match candidate {
                InputTarget::Work { work_id } => match target_from(
                    &self.connection,
                    network,
                    conversation,
                    Source::Work(work_id.clone()),
                )? {
                    Destination::Scoped(target) => target,
                    Destination::Main(_) => InputTarget::Work { work_id },
                    Destination::Unknown => bail!("native work has an unknown input target"),
                },
                candidate => candidate,
            };
            let candidate_scope = match &candidate {
                InputTarget::BotMain { .. } | InputTarget::ChannelMember { .. } => {
                    SessionScope::Main
                }
                InputTarget::Task { task_id } | InputTarget::ChannelTask { task_id, .. } => {
                    SessionScope::WorkspaceTask {
                        task_id: task_id.clone(),
                    }
                }
                InputTarget::Work { work_id } => {
                    if work_is_main_from(&self.connection, network, conversation, work_id)? {
                        SessionScope::Main
                    } else {
                        SessionScope::Task {
                            work_id: work_id.clone(),
                        }
                    }
                }
                InputTarget::Routine { trigger, .. } => SessionScope::Routine {
                    trigger: trigger.clone(),
                },
            };
            ensure!(
                scope.as_ref().is_none_or(|scope| scope == &candidate_scope),
                "native session has conflicting scope bindings"
            );
            scope = Some(candidate_scope);
            targets.push(candidate);
        }
        Ok(targets)
    }
}
