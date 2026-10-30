//! Incremental projections. Each event updates its rows once, as it is stored; every rule is
//! order-independent (first/last by rank, max epoch or version), so any arrival order converges.
//! Bot settings, skills, schedules and notes keep the last change by rank; a removal is a `NULL`
//! row, so an older change arriving later cannot bring it back.
use crate::model::*;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension as _, params};

fn bounded(text: &str) -> String {
    text.chars().take(200).collect()
}

fn notify(
    connection: &Connection,
    network: &str,
    hash: &str,
    scope: &str,
    kind: &str,
    text: &str,
    rank: &str,
) -> Result<()> {
    connection.execute(
        "INSERT OR IGNORE INTO notifications(network_id,notification_id,conversation_id,kind,text,rank) VALUES(?,?,?,?,?,?)",
        params![network, hash, scope, kind, bounded(text), rank],
    )?;
    Ok(())
}

fn text(value: impl serde::Serialize) -> Result<String> {
    Ok(match serde_json::to_value(value)? {
        serde_json::Value::String(text) => text,
        other => other.to_string(),
    })
}

/// The same signed request has the same key wherever an entry Node wraps it, so a retry through
/// another entry cannot win a tie the original lost.
fn request_key(network: &str, event: &Event) -> Result<String> {
    let signer = event
        .body
        .attestation()
        .map_or(event.author.as_str(), |attestation| {
            attestation.device_id.as_str()
        });
    let mut signed = crate::crypto::attestation_payload(network, &event.scope_id, &event.body)?;
    signed.extend_from_slice(signer.as_bytes());
    Ok(crate::crypto::digest(&signed))
}

/// Every table `apply` writes. A rebuild clears exactly these; nothing else is derived from the log.
pub(crate) const TABLES: [&str; 16] = [
    "conversations",
    "messages",
    "bots",
    "bot_profiles",
    "bot_models",
    "bot_deletions",
    "skills",
    "schedules",
    "schedule_claims",
    "memories",
    "bot_changes",
    "works",
    "notifications",
    "notification_acks",
    "questions",
    "session_owners",
];

/// Who owns a session, in any order the records arrive: the latest attribution or mute by
/// rank, or else the first work bound to it.
fn own_session(
    connection: &Connection,
    network: &str,
    (node, session): (&str, &str),
    scope: &str,
    rank: &str,
    attribution: Option<String>,
    created_at_ms: i64,
) -> Result<()> {
    let wins = if attribution.is_some() {
        "attribution IS NULL OR excluded.rank>rank"
    } else {
        "attribution IS NULL AND excluded.rank<rank"
    };
    connection.execute(
        &format!("INSERT INTO session_owners(network_id,node,session_id,conversation_id,rank,attribution,muted,created_at_ms) VALUES(?,?,?,?,?,?,0,?) ON CONFLICT(network_id,node,session_id) DO UPDATE SET conversation_id=excluded.conversation_id,rank=excluded.rank,attribution=excluded.attribution,muted=0,created_at_ms=excluded.created_at_ms WHERE {wins}"),
        params![network, node, session, scope, rank, attribution, created_at_ms],
    )?;
    Ok(())
}

pub(crate) fn clear(connection: &Connection, network: &str) -> Result<()> {
    for table in TABLES {
        connection.execute(
            &format!("DELETE FROM {table} WHERE network_id=?"),
            [network],
        )?;
    }
    Ok(())
}

pub(crate) fn apply(
    connection: &Connection,
    network: &str,
    hash: &str,
    event: &Event,
    seen_at_ms: i64,
    rank: &str,
) -> Result<()> {
    let scope = event.scope_id.as_str();
    match &event.body {
        EventBody::ConversationCreated { title } => {
            connection.execute(
                "INSERT INTO conversations(network_id,conversation_id,title,title_rank) VALUES(?,?,?,?) ON CONFLICT(network_id,conversation_id) DO UPDATE SET title=excluded.title,title_rank=excluded.title_rank WHERE title_rank IS NULL OR excluded.title_rank<title_rank",
                params![network, scope, title, rank],
            )?;
        }
        EventBody::ConversationOwner {
            scheduler_node,
            epoch,
            ..
        } => {
            connection.execute(
                "INSERT INTO conversations(network_id,conversation_id,owner,owner_epoch,owner_key) VALUES(?,?,?,?,?) ON CONFLICT(network_id,conversation_id) DO UPDATE SET owner=excluded.owner,owner_epoch=excluded.owner_epoch,owner_key=excluded.owner_key WHERE owner IS NULL OR excluded.owner_epoch>owner_epoch OR (excluded.owner_epoch=owner_epoch AND excluded.owner_key<owner_key)",
                params![network, scope, scheduler_node, epoch, request_key(network, event)?],
            )?;
        }
        EventBody::BotCreated {
            bot_id,
            name,
            instructions,
            project,
            cause,
            ..
        } => {
            connection.execute(
                "INSERT INTO bots(network_id,conversation_id,bot_id,name,instructions,project,event_hash,rank,cause) VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(network_id,conversation_id) DO UPDATE SET bot_id=excluded.bot_id,name=excluded.name,instructions=excluded.instructions,project=excluded.project,event_hash=excluded.event_hash,rank=excluded.rank,cause=excluded.cause WHERE excluded.rank<rank",
                params![network, scope, bot_id, name, instructions, project.as_ref().map(serde_json::to_string).transpose()?, hash, rank, cause.as_ref().map(serde_json::to_string).transpose()?],
            )?;
        }
        EventBody::MessagePosted {
            message_id,
            role,
            text: body,
            report_results,
            in_reply_to,
            status,
            failure,
            answers,
            attachments,
            ..
        } => {
            connection.execute(
                "INSERT INTO messages(network_id,conversation_id,message_id,event_hash,rank,author,role,text,created_at_ms,in_reply_to,status,failure,report_results,answers,attachments) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(network_id,conversation_id,message_id) DO UPDATE SET event_hash=excluded.event_hash,rank=excluded.rank,author=excluded.author,role=excluded.role,text=excluded.text,created_at_ms=excluded.created_at_ms,in_reply_to=excluded.in_reply_to,status=excluded.status,failure=excluded.failure,report_results=excluded.report_results,answers=excluded.answers,attachments=excluded.attachments WHERE excluded.rank<rank",
                params![
                    network,
                    scope,
                    message_id,
                    hash,
                    rank,
                    event.body.attestation().map_or(event.author.as_str(), |attestation| attestation.device_id.as_str()),
                    text(role)?,
                    body,
                    event.created_at_ms,
                    in_reply_to,
                    status.map(text).transpose()?,
                    failure.map(text).transpose()?,
                    report_results,
                    answers,
                    serde_json::to_string(attachments)?
                ],
            )?;
            if *role == Role::Assistant {
                let kind = if *status == Some(ReplyStatus::Completed) {
                    "manager_response"
                } else {
                    "manager_attention"
                };
                notify(connection, network, hash, scope, kind, body, rank)?;
            }
        }
        EventBody::MessageSteered { message_id, reply } => {
            connection.execute(
                "UPDATE messages SET steered_into=? WHERE network_id=? AND conversation_id=? AND message_id=? AND role='user'",
                params![reply, network, scope, message_id],
            )?;
        }
        EventBody::WorkDispatched { work_id, .. } => {
            connection.execute(
                "INSERT INTO works(network_id,work_id,conversation_id,target_node,dispatch,dispatch_rank) VALUES(?,?,?,?,?,?) ON CONFLICT(network_id,work_id) DO UPDATE SET target_node=excluded.target_node,dispatch=excluded.dispatch,dispatch_rank=excluded.dispatch_rank WHERE dispatch_rank IS NULL OR excluded.dispatch_rank<dispatch_rank",
                params![network, work_id, scope, event.author, serde_json::to_string(&event.body)?, rank],
            )?;
        }
        EventBody::WorkProgress {
            work_id,
            thread_id,
            turn_id,
            ..
        } => {
            connection.execute(
                "INSERT INTO works(network_id,work_id,conversation_id,progress,progress_rank,thread_id,turn_id) VALUES(?,?,?,?,?,?,?) ON CONFLICT(network_id,work_id) DO UPDATE SET thread_id=COALESCE(thread_id,excluded.thread_id),turn_id=COALESCE(turn_id,excluded.turn_id),progress=CASE WHEN progress_rank IS NULL OR excluded.progress_rank>progress_rank THEN excluded.progress ELSE progress END,progress_rank=max(COALESCE(progress_rank,''),excluded.progress_rank)",
                params![network, work_id, scope, serde_json::to_string(&event.body)?, rank, thread_id, turn_id],
            )?;
            if let Some(thread_id) = thread_id {
                own_session(
                    connection,
                    network,
                    (&event.author, thread_id),
                    scope,
                    rank,
                    None,
                    event.created_at_ms,
                )?;
            }
        }
        EventBody::WorkFinished {
            work_id,
            outcome,
            result_text,
            attestation,
            ..
        } => {
            // Besides the target, only the user's signed settlement or the owner's record that a
            // removed device's result is unknown counts; any result of the target outranks the latter.
            let target: Option<String> = connection
                .query_row(
                    "SELECT target_node FROM works WHERE network_id=? AND work_id=?",
                    params![network, work_id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if target.as_deref() != Some(event.author.as_str())
                && attestation.is_none()
                && *outcome != WorkOutcome::Unverified
            {
                return Ok(());
            }
            let precedence = match outcome {
                WorkOutcome::Unverified => 1,
                _ => 0,
            };
            let finish_rank = format!("{precedence}:{rank}");
            connection.execute(
                "INSERT INTO works(network_id,work_id,conversation_id,finish,finish_rank) VALUES(?,?,?,?,?) ON CONFLICT(network_id,work_id) DO UPDATE SET finish=excluded.finish,finish_rank=excluded.finish_rank WHERE finish_rank IS NULL OR excluded.finish_rank<finish_rank",
                params![network, work_id, scope, serde_json::to_string(&event.body)?, finish_rank],
            )?;
            notify(
                connection,
                network,
                hash,
                scope,
                "work_finished",
                result_text,
                rank,
            )?;
        }
        EventBody::NotificationAcked {
            notification_id,
            client_id,
            ..
        } => {
            connection.execute(
                "INSERT OR IGNORE INTO notification_acks(network_id,notification_id,client_id) VALUES(?,?,?)",
                params![network, notification_id, client_id],
            )?;
        }
        EventBody::QuestionAsked {
            question_id,
            text: question,
            about,
            ..
        } => {
            connection.execute(
                "INSERT INTO questions(network_id,question_id,conversation_id,rank,author,created_at_ms,node,session_id,request_id,body) VALUES(?,?,?,?,?,?,?,?,?,?) ON CONFLICT(network_id,question_id) DO UPDATE SET conversation_id=excluded.conversation_id,rank=excluded.rank,author=excluded.author,created_at_ms=excluded.created_at_ms,node=excluded.node,session_id=excluded.session_id,request_id=excluded.request_id,body=excluded.body WHERE excluded.rank<rank",
                params![network, question_id, scope, rank, event.author, event.created_at_ms, about.node, about.session_id, about.request_id, serde_json::to_string(&event.body)?],
            )?;
            notify(connection, network, hash, scope, "question", question, rank)?;
        }
        // Follows the question in its author's log. Only the device that asked and held the
        // request closes it; an answer, if one stands, still wins when the question is read.
        EventBody::QuestionClosed {
            question_id,
            reason,
        } => {
            connection.execute(
                "UPDATE questions SET closed=? WHERE network_id=? AND question_id=? AND conversation_id=? AND author=?5 AND node=?5 AND request_id IS NOT NULL AND closed IS NULL",
                params![text(reason)?, network, question_id, scope, event.author],
            )?;
        }
        EventBody::SessionAttributed {
            node,
            agent,
            session_id,
            cwd,
            summary,
        } => {
            let attribution = serde_json::json!({"agent":agent,"cwd":cwd,"summary":summary});
            own_session(
                connection,
                network,
                (node, session_id),
                scope,
                rank,
                Some(attribution.to_string()),
                event.created_at_ms,
            )?;
        }
        // Follows the attribution it mutes in its author's log, so that row is already here.
        EventBody::SessionMuted { node, session_id } => {
            connection.execute(
                "UPDATE session_owners SET muted=1,rank=?1,created_at_ms=?2 WHERE network_id=?3 AND node=?4 AND session_id=?5 AND conversation_id=?6 AND attribution IS NOT NULL AND ?1>rank",
                params![rank, event.created_at_ms, network, node, session_id, scope],
            )?;
        }
        EventBody::BotChanged {
            change_id,
            cause,
            change,
        } => {
            connection.execute(
                "INSERT INTO bot_changes(network_id,change_id,conversation_id,event_hash,rank,cause,change,created_at_ms) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(network_id,change_id) DO UPDATE SET conversation_id=excluded.conversation_id,event_hash=excluded.event_hash,rank=excluded.rank,cause=excluded.cause,change=excluded.change,created_at_ms=excluded.created_at_ms WHERE excluded.rank<rank",
                params![network, change_id, scope, hash, rank, cause.as_ref().map(serde_json::to_string).transpose()?, serde_json::to_string(change)?, event.created_at_ms],
            )?;
            match change {
                BotChange::Profile {
                    name,
                    instructions,
                    model,
                    skills,
                    avatar,
                } => {
                    let after = |edit: &Option<Edit>| edit.as_ref().map(|edit| edit.after.clone());
                    if let Some(model) = model {
                        connection.execute(
                            "INSERT INTO bot_models(network_id,conversation_id,provider,model,rank) VALUES(?,?,?,?,?) ON CONFLICT(network_id,conversation_id,provider) DO UPDATE SET model=excluded.model,rank=excluded.rank WHERE excluded.rank>rank",
                            params![network, scope, serde_json::to_value(model.provider)?.as_str(), model.after, rank],
                        )?;
                    }
                    let fields = [
                        ("name", after(name)),
                        ("instructions", after(instructions)),
                        (
                            "skills",
                            skills
                                .as_ref()
                                .map(|edit| serde_json::to_string(&edit.after))
                                .transpose()?,
                        ),
                        (
                            "avatar",
                            avatar
                                .as_ref()
                                .map(|edit| serde_json::to_string(&edit.after))
                                .transpose()?,
                        ),
                    ];
                    for (field, value) in fields {
                        let Some(value) = value else { continue };
                        connection.execute(
                            "INSERT INTO bot_profiles(network_id,conversation_id,field,value,rank) VALUES(?,?,?,?,?) ON CONFLICT(network_id,conversation_id,field) DO UPDATE SET value=excluded.value,rank=excluded.rank WHERE excluded.rank>rank",
                            params![network, scope, field, value, rank],
                        )?;
                    }
                }
                BotChange::Skill {
                    skill_id, after, ..
                } => {
                    connection.execute(
                        "INSERT INTO skills(network_id,skill_id,skill,rank) VALUES(?,?,?,?) ON CONFLICT(network_id,skill_id) DO UPDATE SET skill=excluded.skill,rank=excluded.rank WHERE excluded.rank>rank",
                        params![network, skill_id, after.as_ref().map(serde_json::to_string).transpose()?, rank],
                    )?;
                }
                BotChange::Schedule {
                    schedule_id,
                    after: Some(after),
                    ..
                } => {
                    connection.execute(
                        "INSERT INTO schedules(network_id,schedule_id,conversation_id,schedule,rank,changed_at_ms,enabled,enabled_rank,enabled_at_ms,removed) VALUES(?1,?2,?3,?4,?5,?6,?7,?5,?6,0) ON CONFLICT(network_id,schedule_id) DO UPDATE SET schedule=excluded.schedule,rank=excluded.rank,changed_at_ms=excluded.changed_at_ms WHERE removed=0 AND conversation_id=excluded.conversation_id AND excluded.rank>rank",
                        params![network, schedule_id, scope, serde_json::to_string(after)?, rank, seen_at_ms, after.enabled],
                    )?;
                    set_schedule_enabled(
                        connection,
                        network,
                        schedule_id,
                        scope,
                        after.enabled,
                        rank,
                        seen_at_ms,
                    )?;
                }
                BotChange::Schedule {
                    schedule_id,
                    after: None,
                    ..
                } => remove_schedule(connection, network, schedule_id, scope, rank, seen_at_ms)?,
                BotChange::Memory {
                    memory_id, after, ..
                } => {
                    connection.execute(
                        "INSERT INTO memories(network_id,memory_id,conversation_id,text,rank,changed_at_ms) VALUES(?,?,?,?,?,?) ON CONFLICT(network_id,memory_id) DO UPDATE SET text=excluded.text,rank=excluded.rank,changed_at_ms=excluded.changed_at_ms WHERE conversation_id=excluded.conversation_id AND excluded.rank>rank",
                        params![network, memory_id, scope, after, rank, event.created_at_ms],
                    )?;
                }
            }
        }
        // `null` clears it; see `store::bots::MERGED_COLUMNS`.
        EventBody::BotProjectSet { project, .. } => {
            connection.execute(
                "INSERT INTO bot_profiles(network_id,conversation_id,field,value,rank) VALUES(?,?,'project',?,?) ON CONFLICT(network_id,conversation_id,field) DO UPDATE SET value=excluded.value,rank=excluded.rank WHERE excluded.rank>rank",
                params![network, scope, serde_json::to_string(project)?, rank],
            )?;
        }
        // The same row a chat change of the face writes, so the larger rank wins between them.
        EventBody::BotAvatarSet { avatar, .. } => {
            connection.execute(
                "INSERT INTO bot_profiles(network_id,conversation_id,field,value,rank) VALUES(?,?,'avatar',?,?) ON CONFLICT(network_id,conversation_id,field) DO UPDATE SET value=excluded.value,rank=excluded.rank WHERE excluded.rank>rank",
                params![network, scope, serde_json::to_string(avatar)?, rank],
            )?;
        }
        EventBody::BotDeleted { .. } => {
            connection.execute(
                "INSERT OR IGNORE INTO bot_deletions(network_id,conversation_id) VALUES(?,?)",
                params![network, scope],
            )?;
        }
        EventBody::ScheduleControlled {
            schedule_id,
            action,
            ..
        } => match action {
            ScheduleAction::Pause | ScheduleAction::Resume => set_schedule_enabled(
                connection,
                network,
                schedule_id,
                scope,
                *action == ScheduleAction::Resume,
                rank,
                seen_at_ms,
            )?,
            ScheduleAction::Delete => {
                remove_schedule(connection, network, schedule_id, scope, rank, seen_at_ms)?
            }
        },
        EventBody::ScheduleRunClaimed {
            schedule_id,
            slot_ms,
            owner_epoch,
        } => {
            connection.execute(
                "INSERT INTO schedule_claims(network_id,conversation_id,schedule_id,slot_ms,owner_epoch,author) VALUES(?,?,?,?,?,?) ON CONFLICT(network_id,conversation_id,schedule_id,slot_ms) DO UPDATE SET owner_epoch=excluded.owner_epoch,author=excluded.author WHERE excluded.owner_epoch>owner_epoch OR (excluded.owner_epoch=owner_epoch AND excluded.author<author)",
                params![network, scope, schedule_id, slot_ms, owner_epoch, event.author],
            )?;
        }
        EventBody::BotMessaged {
            message_id,
            from,
            text: body,
            ..
        } => {
            connection.execute(
                "INSERT INTO messages(network_id,conversation_id,message_id,event_hash,rank,author,role,text,created_at_ms,attachments,sender) VALUES(?,?,?,?,?,?,?,?,?,'[]',?) ON CONFLICT(network_id,conversation_id,message_id) DO UPDATE SET event_hash=excluded.event_hash,rank=excluded.rank,author=excluded.author,role=excluded.role,text=excluded.text,created_at_ms=excluded.created_at_ms,sender=excluded.sender WHERE excluded.rank<rank",
                params![network, scope, message_id, hash, rank, event.author, text(Role::Bot)?, body, event.created_at_ms, serde_json::to_string(from)?],
            )?;
        }
        // Read from the log by rank; see `store::bots::primary_bot_from`.
        EventBody::ContextCheckpoint { .. } | EventBody::PrimaryBotSet { .. } => {}
    }
    Ok(())
}

/// A schedule's pause state wins by its own rank, apart from its definition, so a pause and an
/// edit that crossed converge in any order.
fn set_schedule_enabled(
    connection: &Connection,
    network: &str,
    schedule_id: &str,
    scope: &str,
    enabled: bool,
    rank: &str,
    at: i64,
) -> Result<()> {
    connection.execute(
        "UPDATE schedules SET enabled=?,enabled_rank=?,enabled_at_ms=? WHERE network_id=? AND schedule_id=? AND conversation_id=? AND removed=0 AND ?2>enabled_rank",
        params![enabled, rank, at, network, schedule_id, scope],
    )?;
    Ok(())
}

/// Removal is final: no change of any rank brings a schedule back.
fn remove_schedule(
    connection: &Connection,
    network: &str,
    schedule_id: &str,
    scope: &str,
    rank: &str,
    at: i64,
) -> Result<()> {
    connection.execute(
        "INSERT INTO schedules(network_id,schedule_id,conversation_id,schedule,rank,changed_at_ms,enabled,enabled_rank,enabled_at_ms,removed) VALUES(?1,?2,?3,NULL,?4,?5,0,?4,?5,1) ON CONFLICT(network_id,schedule_id) DO UPDATE SET schedule=NULL,removed=1 WHERE conversation_id=excluded.conversation_id",
        params![network, schedule_id, scope, rank, at],
    )?;
    Ok(())
}

type WorkRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

pub(crate) fn works_from(
    connection: &Connection,
    network: &str,
    scope: &str,
) -> Result<Vec<WorkProjection>> {
    works_query(
        connection,
        "FROM works w WHERE w.network_id=?1 AND w.conversation_id=?2",
        params![network, scope],
    )
}

/// One conversation's works, or every conversation's when `scope` is `None`, that want the user:
/// those with no result yet or one only they can settle, and the newest ended work when it failed
/// and nothing has followed it up. A later work ending, a completed bot reply after it, or the
/// user closing it in 待处理 (an ack by [`INBOX_CLIENT`] naming the work) follows it up.
pub(crate) fn open_works_from(
    connection: &Connection,
    network: &str,
    scope: Option<&str>,
) -> Result<Vec<WorkProjection>> {
    // A failure stays until the bot answers after it: a reply to a message written after it ended,
    // or its own report turn's reply. The dispatching turn's reply can land after the failure
    // without having seen it, so a later rank alone does not count.
    // Separate statements so a scoped call can use the conversation index.
    let (only, only_w) = if scope.is_some() {
        ("conversation_id=?2", "w.conversation_id=?2")
    } else {
        ("?2 IS NULL", "?2 IS NULL")
    };
    let works = works_query(
        connection,
        &format!("FROM works w LEFT JOIN (SELECT conversation_id, max(dispatch_rank) AS newest FROM works
            WHERE network_id=?1 AND {only} AND dispatch IS NOT NULL
            AND finish IS NOT NULL AND json_extract(finish,'$.outcome')<>'unverified'
            GROUP BY conversation_id) ended ON ended.conversation_id=w.conversation_id
         WHERE w.network_id=?1 AND {only_w} AND (
            w.finish IS NULL OR json_extract(w.finish,'$.outcome')='unverified'
            OR json_extract(w.finish,'$.outcome') IN ('failed','rejected') AND w.dispatch_rank=ended.newest
            AND NOT EXISTS(SELECT 1 FROM notification_acks a WHERE a.network_id=w.network_id
                AND a.notification_id=w.work_id AND a.client_id=?3)
            AND NOT EXISTS(SELECT 1 FROM messages r JOIN messages u ON u.network_id=r.network_id
                AND u.conversation_id=r.conversation_id AND u.message_id=r.in_reply_to
                WHERE r.network_id=w.network_id AND r.conversation_id=w.conversation_id
                AND r.role='assistant' AND r.status='completed' AND u.role='user'
                AND u.rank>substr(w.finish_rank,3)))"),
        params![network, scope, INBOX_CLIENT],
    )?;
    let mut open = Vec::with_capacity(works.len());
    for work in works {
        let failed = matches!(
            work.outcome,
            Some(WorkOutcome::Failed | WorkOutcome::Rejected)
        );
        if !failed || !reported_from(connection, network, &work.conversation_id, &work.work_id)? {
            open.push(work);
        }
    }
    Ok(open)
}

/// Whether the bot's report turn for this work wrote a completed reply, on whichever owner ran it.
fn reported_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    work_id: &str,
) -> Result<bool> {
    let epoch: u64 = connection
        .query_row(
            "SELECT owner_epoch FROM conversations WHERE network_id=? AND conversation_id=?",
            params![network, conversation],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let trigger = crate::scheduling::work_trigger(work_id);
    for owner_epoch in 0..=epoch {
        let reply = super::scheduler::reply_message_id(conversation, &trigger, owner_epoch);
        let completed: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE network_id=? AND conversation_id=? AND message_id=?
                AND role='assistant' AND status='completed')",
            params![network, conversation, reply],
            |r| r.get(0),
        )?;
        if completed {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The works the `FROM … WHERE …` clause selects from `works w`, in dispatch order.
fn works_query(
    connection: &Connection,
    from: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<WorkProjection>> {
    let mut statement = connection.prepare(&format!(
        "SELECT w.work_id,w.conversation_id,w.target_node,w.dispatch,w.progress,w.thread_id,w.turn_id,w.finish {from} AND w.dispatch IS NOT NULL ORDER BY w.conversation_id,w.dispatch_rank"
    ))?;
    let rows: Vec<WorkRow> = statement
        .query_map(params, |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut works = Vec::with_capacity(rows.len());
    for (work_id, conversation_id, target_node, dispatch, progress, thread_id, turn_id, finish) in
        rows
    {
        let EventBody::WorkDispatched {
            trigger,
            dispatcher,
            kind,
            cwd,
            title,
            prompt,
            call_ref,
            continues,
            parent,
            thread,
            ..
        } = serde_json::from_str(&dispatch)?
        else {
            anyhow::bail!("work dispatch projection is corrupt");
        };
        let (phase, summary) = match progress.map(|p| serde_json::from_str(&p)).transpose()? {
            Some(EventBody::WorkProgress { phase, summary, .. }) => (Some(phase), summary),
            _ => (None, String::new()),
        };
        let (outcome, result_text, failure_reason, failure) =
            match finish.map(|f| serde_json::from_str(&f)).transpose()? {
                Some(EventBody::WorkFinished {
                    outcome,
                    result_text,
                    failure_reason,
                    failure,
                    ..
                }) => (Some(outcome), result_text, failure_reason, failure),
                _ => (None, String::new(), None, None),
            };
        works.push(WorkProjection {
            work_id,
            conversation_id,
            target_node,
            trigger,
            dispatcher,
            kind,
            cwd,
            title,
            prompt,
            call_ref,
            continues,
            parent,
            thread,
            phase,
            summary,
            thread_id,
            turn_id,
            outcome,
            result_text,
            failure_reason,
            failure,
            state: WorkStatus::of(kind, phase, outcome),
        });
    }
    Ok(works)
}

/// Whether the result that stands for this work is the user's signed settlement.
pub(crate) fn settled_by_user(
    connection: &Connection,
    network: &str,
    work_id: &str,
) -> Result<bool> {
    let finish: Option<String> = connection
        .query_row(
            "SELECT finish FROM works WHERE network_id=? AND work_id=?",
            params![network, work_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(
        match finish.map(|f| serde_json::from_str(&f)).transpose()? {
            Some(EventBody::WorkFinished { attestation, .. }) => attestation.is_some(),
            _ => false,
        },
    )
}

/// One work's projection, wherever its conversation is.
pub(crate) fn work_from(
    connection: &Connection,
    network: &str,
    work_id: &str,
) -> Result<Option<WorkProjection>> {
    Ok(works_query(
        connection,
        "FROM works w WHERE w.network_id=?1 AND w.work_id=?2",
        params![network, work_id],
    )?
    .pop())
}
