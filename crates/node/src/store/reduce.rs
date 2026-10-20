//! Incremental projections. Each event updates its rows once, as it is stored; every rule is
//! order-independent (first/last by rank, max epoch or version), so any arrival order converges.
//! Assistant settings, skills and routines keep the last change by rank; a removal is a `NULL`
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

/// A refusal outranks consent, so concurrent conflicting answers settle the same way everywhere.
fn answer_strictness(decision: ApprovalDecision) -> u8 {
    match decision {
        ApprovalDecision::Cancel => 0,
        ApprovalDecision::Decline => 1,
        ApprovalDecision::Accept | ApprovalDecision::Answered => 2,
    }
}

pub(crate) fn strictness(mode: AutonomyMode) -> u8 {
    match mode {
        AutonomyMode::Continue => 0,
        AutonomyMode::Report => 1,
        AutonomyMode::Off => 2,
    }
}

pub(crate) fn apply(
    connection: &Connection,
    network: &str,
    hash: &str,
    event: &Event,
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
            ..
        } => {
            connection.execute(
                "INSERT INTO bots(network_id,conversation_id,bot_id,name,instructions,project,event_hash,rank) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(network_id,conversation_id) DO UPDATE SET bot_id=excluded.bot_id,name=excluded.name,instructions=excluded.instructions,project=excluded.project,event_hash=excluded.event_hash,rank=excluded.rank WHERE excluded.rank<rank",
                params![network, scope, bot_id, name, instructions, project.as_ref().map(serde_json::to_string).transpose()?, hash, rank],
            )?;
        }
        EventBody::MessagePosted {
            message_id,
            role,
            text: body,
            report_results,
            in_reply_to,
            status,
            ..
        } => {
            connection.execute(
                "INSERT INTO messages(network_id,conversation_id,message_id,event_hash,rank,author,role,text,created_at_ms,in_reply_to,status,report_results,blobs) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(network_id,conversation_id,message_id) DO UPDATE SET event_hash=excluded.event_hash,rank=excluded.rank,author=excluded.author,role=excluded.role,text=excluded.text,created_at_ms=excluded.created_at_ms,in_reply_to=excluded.in_reply_to,status=excluded.status,report_results=excluded.report_results,blobs=excluded.blobs WHERE excluded.rank<rank",
                params![
                    network,
                    scope,
                    message_id,
                    hash,
                    rank,
                    event.author,
                    text(role)?,
                    body,
                    event.created_at_ms,
                    in_reply_to,
                    status.map(text).transpose()?,
                    report_results,
                    serde_json::to_string(&event.blobs)?
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
        }
        EventBody::ApprovalRequested {
            work_id,
            request_id,
            kind,
            summary,
        } => {
            connection.execute(
                "INSERT INTO approvals(network_id,request_id,work_id,kind,summary,request_rank) VALUES(?,?,?,?,?,?) ON CONFLICT(network_id,request_id) DO UPDATE SET kind=excluded.kind,summary=excluded.summary,request_rank=excluded.request_rank WHERE request_rank IS NULL OR excluded.request_rank<request_rank",
                params![network, request_id, work_id, kind, summary, rank],
            )?;
            notify(
                connection,
                network,
                hash,
                scope,
                "needs_input",
                summary,
                rank,
            )?;
        }
        EventBody::ApprovalAnswered {
            work_id,
            request_id,
            decision,
            ..
        } => {
            connection.execute(
                "INSERT INTO approvals(network_id,request_id,work_id,decision,answer_key) VALUES(?,?,?,?,?) ON CONFLICT(network_id,request_id) DO UPDATE SET decision=excluded.decision,answer_key=excluded.answer_key WHERE answer_key IS NULL OR excluded.answer_key<answer_key",
                params![network, request_id, work_id, text(decision)?, format!("{}:{}", answer_strictness(*decision), request_key(network, event)?)],
            )?;
        }
        EventBody::WorkFinished {
            work_id,
            outcome,
            result_text,
            attestation,
            ..
        } => {
            // Besides the target, only the user's signed settlement or the owner's abandonment counts.
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
                && *outcome != WorkOutcome::Abandoned
            {
                return Ok(());
            }
            // A proven outcome beats an unverified one, and both beat an abandonment, in any order.
            let precedence = match outcome {
                WorkOutcome::Unverified => 1,
                WorkOutcome::Abandoned => 2,
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
        EventBody::AutonomySet { mode, version, .. } => {
            connection.execute(
                "INSERT INTO autonomy(network_id,conversation_id,mode,strictness,version,request_key) VALUES(?,?,?,?,?,?) ON CONFLICT(network_id,conversation_id) DO UPDATE SET mode=excluded.mode,strictness=excluded.strictness,version=excluded.version,request_key=excluded.request_key WHERE excluded.version>version OR (excluded.version=version AND (excluded.strictness>strictness OR (excluded.strictness=strictness AND excluded.request_key<request_key)))",
                params![network, scope, text(mode)?, strictness(*mode), version, request_key(network, event)?],
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
        EventBody::AssistantChanged {
            change_id,
            cause,
            change,
        } => {
            connection.execute(
                "INSERT INTO assistant_changes(network_id,change_id,conversation_id,event_hash,rank,cause,change,created_at_ms) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(network_id,change_id) DO UPDATE SET conversation_id=excluded.conversation_id,event_hash=excluded.event_hash,rank=excluded.rank,cause=excluded.cause,change=excluded.change,created_at_ms=excluded.created_at_ms WHERE excluded.rank<rank",
                params![network, change_id, scope, hash, rank, cause, serde_json::to_string(change)?, event.created_at_ms],
            )?;
            match change {
                AssistantChange::Profile { name, instructions } => {
                    for (field, edit) in [("name", name), ("instructions", instructions)] {
                        let Some(edit) = edit else { continue };
                        connection.execute(
                            "INSERT INTO assistant_profiles(network_id,conversation_id,field,value,rank) VALUES(?,?,?,?,?) ON CONFLICT(network_id,conversation_id,field) DO UPDATE SET value=excluded.value,rank=excluded.rank WHERE excluded.rank>rank",
                            params![network, scope, field, edit.after, rank],
                        )?;
                    }
                }
                AssistantChange::Skill {
                    skill_id, after, ..
                } => {
                    connection.execute(
                        "INSERT INTO skills(network_id,skill_id,skill,rank) VALUES(?,?,?,?) ON CONFLICT(network_id,skill_id) DO UPDATE SET skill=excluded.skill,rank=excluded.rank WHERE excluded.rank>rank",
                        params![network, skill_id, after.as_ref().map(serde_json::to_string).transpose()?, rank],
                    )?;
                }
                AssistantChange::Routine {
                    routine_id, after, ..
                } => {
                    connection.execute(
                        "INSERT INTO routines(network_id,routine_id,conversation_id,routine,rank,changed_at_ms) VALUES(?,?,?,?,?,?) ON CONFLICT(network_id,routine_id) DO UPDATE SET conversation_id=excluded.conversation_id,routine=excluded.routine,rank=excluded.rank,changed_at_ms=excluded.changed_at_ms WHERE excluded.rank>rank",
                        params![network, routine_id, scope, after.as_ref().map(serde_json::to_string).transpose()?, rank, event.created_at_ms],
                    )?;
                }
            }
        }
        EventBody::ContextCheckpoint { .. } => {}
    }
    Ok(())
}

type WorkRow = (
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
    let mut statement = connection.prepare(
        "SELECT work_id,target_node,dispatch,progress,thread_id,turn_id,finish FROM works WHERE network_id=? AND conversation_id=? AND dispatch IS NOT NULL ORDER BY dispatch_rank",
    )?;
    let rows: Vec<WorkRow> = statement
        .query_map(params![network, scope], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut approvals = connection.prepare(
        "SELECT request_id,kind,summary,decision FROM approvals WHERE network_id=? AND work_id=? AND request_rank IS NOT NULL ORDER BY request_rank",
    )?;
    let mut works = Vec::with_capacity(rows.len());
    for (work_id, target_node, dispatch, progress, thread_id, turn_id, finish) in rows {
        let EventBody::WorkDispatched {
            trigger,
            dispatcher,
            executor,
            cwd,
            title,
            prompt,
            ..
        } = serde_json::from_str(&dispatch)?
        else {
            anyhow::bail!("work dispatch projection is corrupt");
        };
        let (phase, summary) = match progress.map(|p| serde_json::from_str(&p)).transpose()? {
            Some(EventBody::WorkProgress { phase, summary, .. }) => (Some(phase), summary),
            _ => (None, String::new()),
        };
        let (outcome, result_text, failure_reason) =
            match finish.map(|f| serde_json::from_str(&f)).transpose()? {
                Some(EventBody::WorkFinished {
                    outcome,
                    result_text,
                    failure_reason,
                    ..
                }) => (Some(outcome), result_text, failure_reason),
                _ => (None, String::new(), None),
            };
        let mut open_approvals = Vec::new();
        let mut answered_approvals = Vec::new();
        let rows = approvals.query_map(params![network, work_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (request_id, kind, summary, decision) = row?;
            match decision {
                Some(decision) => answered_approvals.push(AnsweredApproval {
                    request_id,
                    kind,
                    summary,
                    decision: serde_json::from_value(serde_json::Value::String(decision))?,
                }),
                // A finished work has nothing left to answer.
                None if outcome.is_none() => open_approvals.push(ApprovalProjection {
                    request_id,
                    kind,
                    summary,
                }),
                None => {}
            }
        }
        works.push(WorkProjection {
            work_id,
            conversation_id: scope.into(),
            target_node,
            trigger,
            dispatcher,
            executor,
            cwd,
            title,
            prompt,
            phase,
            summary,
            thread_id,
            turn_id,
            outcome,
            result_text,
            failure_reason,
            open_approvals,
            answered_approvals,
        });
    }
    Ok(works)
}

/// One work's projection, wherever its conversation is.
pub(crate) fn work_from(
    connection: &Connection,
    network: &str,
    work_id: &str,
) -> Result<Option<WorkProjection>> {
    let scope: Option<String> = connection
        .query_row(
            "SELECT conversation_id FROM works WHERE network_id=? AND work_id=? AND dispatch IS NOT NULL",
            params![network, work_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(scope) = scope else {
        return Ok(None);
    };
    Ok(works_from(connection, network, &scope)?
        .into_iter()
        .find(|work| work.work_id == work_id))
}
