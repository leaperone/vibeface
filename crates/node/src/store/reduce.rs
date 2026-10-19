//! Incremental projections. Each event updates its rows once, as it is stored; every rule is
//! order-independent (first/last by rank, max epoch or version), so any arrival order converges.
use crate::model::*;
use anyhow::Result;
use rusqlite::{Connection, params};

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
        } => {
            connection.execute(
                "INSERT INTO conversations(network_id,conversation_id,owner,owner_epoch,owner_hash) VALUES(?,?,?,?,?) ON CONFLICT(network_id,conversation_id) DO UPDATE SET owner=excluded.owner,owner_epoch=excluded.owner_epoch,owner_hash=excluded.owner_hash WHERE owner IS NULL OR excluded.owner_epoch>owner_epoch OR (excluded.owner_epoch=owner_epoch AND excluded.owner_hash<owner_hash)",
                params![network, scope, scheduler_node, epoch, hash],
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
                "INSERT INTO approvals(network_id,request_id,work_id,decision,answer_rank) VALUES(?,?,?,?,?) ON CONFLICT(network_id,request_id) DO UPDATE SET decision=excluded.decision,answer_rank=excluded.answer_rank WHERE answer_rank IS NULL OR excluded.answer_rank<answer_rank",
                params![network, request_id, work_id, text(decision)?, rank],
            )?;
        }
        EventBody::WorkFinished {
            work_id,
            outcome,
            result_text,
            ..
        } => {
            // A proven outcome beats an unverified one regardless of order.
            let finish_rank = format!("{}:{rank}", u8::from(*outcome == WorkOutcome::Unverified));
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
                "INSERT INTO autonomy(network_id,conversation_id,mode,strictness,version,event_hash) VALUES(?,?,?,?,?,?) ON CONFLICT(network_id,conversation_id) DO UPDATE SET mode=excluded.mode,strictness=excluded.strictness,version=excluded.version,event_hash=excluded.event_hash WHERE excluded.version>version OR (excluded.version=version AND (excluded.strictness>strictness OR (excluded.strictness=strictness AND excluded.event_hash<event_hash)))",
                params![network, scope, text(mode)?, strictness(*mode), version, hash],
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
