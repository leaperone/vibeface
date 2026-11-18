//! Human workspace summary projection and bounded query composition.
use super::*;

const WORKSPACE_OVERVIEW_SCHEMA: u32 = 1;
const WORKSPACE_OVERVIEW_DEFAULT_LIMIT: usize = 256;
const WORKSPACE_OVERVIEW_MAX_LIMIT: usize = 256;

fn workspace_bounded(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

#[cfg(test)]
fn workspace_bounded_opt(value: Option<&str>, limit: usize) -> Option<String> {
    value
        .filter(|value| !value.is_empty())
        .map(|value| workspace_bounded(value, limit))
}

pub(super) fn workspace_limit(input: &Value) -> Result<usize> {
    let limit = input
        .get("limit")
        .map(|value| value.as_u64().context("limit must be an integer"))
        .transpose()?
        .unwrap_or(WORKSPACE_OVERVIEW_DEFAULT_LIMIT as u64);
    ensure!(limit > 0, "limit must be greater than zero");
    ensure!(
        limit <= WORKSPACE_OVERVIEW_MAX_LIMIT as u64,
        "limit exceeds the workspace overview maximum"
    );
    Ok(limit as usize)
}

pub(super) fn workspace_bool(input: &Value, key: &str, default: bool) -> Result<bool> {
    input
        .get(key)
        .map(|value| value.as_bool().context(format!("{key} must be a boolean")))
        .transpose()
        .map(|value| value.unwrap_or(default))
}

#[cfg(test)]
fn workspace_work_summary(work: &WorkProjection, source: &str) -> Value {
    let mut row = json!({
        "work_id": work.work_id,
        "conversation_id": work.conversation_id,
        "trigger": work.trigger,
        "title": workspace_bounded(&work.title, 256),
        "summary": workspace_bounded(&work.summary, 512),
        "cwd": workspace_bounded(&work.cwd, 512),
        "state": work.state,
        "outcome": work.outcome,
        "kind": work.kind,
        "target_node": work.target_node,
        "dispatcher": work.dispatcher,
        "call_ref": work.call_ref,
        "model": work.model,
        "resolved": work.resolved,
        "parent": work.parent,
        "continues": work.continues,
        "thread": work.thread,
        "thread_id": work.thread_id,
        "turn_id": work.turn_id,
        "phase": work.phase,
        "source": source,
    });
    if let Some(reason) = workspace_bounded_opt(work.failure_reason.as_deref(), 512) {
        row["failure_reason"] = json!(reason);
    }
    if let Some(failure) = work.failure {
        row["failure"] = json!(failure);
    }
    if work.outcome == Some(WorkOutcome::Unverified) {
        row["unknown_reason"] = json!("work_result_unverified");
    }
    row
}

fn workspace_question_summary(question: &Value) -> Value {
    let mut row = json!({});
    for key in [
        "question_id",
        "conversation_id",
        "bot",
        "author",
        "created_at_ms",
        "severity",
        "choices",
        "attention",
    ] {
        if let Some(value) = question.get(key) {
            row[key] = value.clone();
        }
    }
    if row["bot"].is_object() {
        row["bot"]["conversation_id"] = question["conversation_id"].clone();
    }
    if let Some(text) = question.get("text").and_then(Value::as_str) {
        row["text"] = json!(workspace_bounded(text, 1024));
    }
    if let Some(about) = question.get("about").and_then(Value::as_object) {
        let mut safe = json!({});
        for key in ["node", "agent", "session_id", "request_id"] {
            if let Some(value) = about.get(key) {
                safe[key] = value.clone();
            }
        }
        for (key, limit) in [("cwd", 512), ("summary", 512)] {
            if let Some(value) = about.get(key).and_then(Value::as_str) {
                safe[key] = json!(workspace_bounded(value, limit));
            }
        }
        row["about"] = safe;
    }
    row
}

fn workspace_session_summary(session: &Value) -> Value {
    let mut row = json!({});
    for key in [
        "conversation_id",
        "node",
        "session_id",
        "agent",
        "muted",
        "created_at_ms",
        "version",
    ] {
        if let Some(value) = session.get(key) {
            row[key] = value.clone();
        }
    }
    if let Some(cwd) = session.get("cwd").and_then(Value::as_str) {
        row["cwd"] = json!(workspace_bounded(cwd, 512));
    }
    if let Some(summary) = session.get("summary").and_then(Value::as_str) {
        row["summary"] = json!(workspace_bounded(summary, 512));
    }
    row
}

fn workspace_job_summary(
    job: &crate::scheduling::SchedulerJob,
    source_node: &str,
    owner_epoch: u64,
    pending_checks: &BTreeSet<String>,
) -> Value {
    json!({
        "conversation_id": job.conversation_id,
        "trigger": job.trigger,
        "kind": job.kind,
        "lane": job.kind.lane(),
        "sequence": job.sequence,
        "state": job.state.name(),
        "message_id": job.message_id,
        "source_node": source_node,
        "owner_epoch": owner_epoch,
        "runtime": job.runtime,
        "retryable": job.retryable(),
        "unknown_reason": pending_checks.contains(&format!("{}\n{}", job.conversation_id, job.trigger))
            .then_some("pending_check"),
    })
}

#[cfg(test)]
mod workspace_overview_tests {
    use super::*;

    #[test]
    fn question_and_session_summaries_keep_their_conversation() {
        let question = workspace_question_summary(&json!({
            "question_id": "q", "conversation_id": "question-chat",
            "bot": {"bot_id": "b", "name": "Bot"}, "text": "Which format?",
            "severity": "attention", "choices": [], "created_at_ms": 1,
        }));
        assert_eq!(question["conversation_id"], "question-chat");
        assert_eq!(
            question["bot"],
            json!({
                "bot_id": "b", "name": "Bot", "conversation_id": "question-chat",
            })
        );
        let unowned = workspace_question_summary(&json!({"conversation_id": "gone", "bot": null}));
        assert!(unowned["bot"].is_null());
        let session = workspace_session_summary(&json!({
            "conversation_id": "session-chat", "node": "n", "session_id": "s",
        }));
        assert_eq!(session["conversation_id"], "session-chat");
    }

    #[test]
    fn work_summary_does_not_expose_prompt_or_raw_result() {
        let work = WorkProjection {
            work_id: "work-1".into(),
            conversation_id: "conversation-1".into(),
            target_node: "node-1".into(),
            trigger: "task:work-1".into(),
            dispatcher: "node-2".into(),
            kind: WorkKind::Task,
            cwd: "/private/project".into(),
            title: "title".into(),
            prompt: "provider secret".into(),
            call_ref: "call-ref".into(),
            model: None,
            resolved: ResolvedModel {
                provider: Provider::Codex,
                model: None,
            },
            continues: None,
            parent: None,
            thread: None,
            phase: None,
            summary: "summary".into(),
            thread_id: None,
            turn_id: None,
            outcome: Some(WorkOutcome::Completed),
            result_text: "raw provider output".into(),
            failure_reason: None,
            failure: None,
            state: WorkStatus::Finished,
        };
        let value = workspace_work_summary(&work, "dag");
        assert!(value.get("prompt").is_none());
        assert!(value.get("result_text").is_none());
        assert_eq!(value["title"], "title");
        assert_eq!(value["cwd"], "/private/project");
    }
}

pub(super) fn job_projector<'a>(
    projection: &'a super::schedule_delivery::SchedulerProjection,
    network: &'a str,
) -> impl Fn(
    &Store,
    &crate::scheduling::SchedulerJob,
    u64,
    bool,
    Option<crate::scheduling::ContextProjection>,
    &crate::store::scheduler::overview::JobFacts,
) -> Result<Value>
+ 'a {
    move |store, job, epoch, pending, context, facts| {
        let checks = if pending {
            BTreeSet::from([format!("{}\n{}", job.conversation_id, job.trigger)])
        } else {
            BTreeSet::new()
        };
        let mut value = workspace_job_summary(job, &projection.local, epoch, &checks);
        value["hold_reason"] = json!(projection.hold_reason_with(
            job,
            |work| Ok(facts.task_provider(work)),
            |provider, local, limits| facts.hold(store, network, local, job, provider, limits)
        )?);
        value["context"] = json!(context);
        Ok(value)
    }
}

pub(super) fn summary(mut snapshot: crate::store::overview::OverviewSnapshot) -> Value {
    for row in snapshot
        .tasks
        .rows
        .iter_mut()
        .chain(snapshot.deliveries.rows.iter_mut())
        .chain(snapshot.conversations.rows.iter_mut())
    {
        for (key, limit) in [
            ("title", 256),
            ("cwd", 512),
            ("summary", 512),
            ("reason", 512),
            ("failure_reason", 512),
        ] {
            if let Some(text) = row[key].as_str() {
                row[key] = json!(workspace_bounded(text, limit));
            }
        }
    }
    snapshot.questions.rows = snapshot
        .questions
        .rows
        .iter()
        .map(workspace_question_summary)
        .collect();
    snapshot.sessions.rows = snapshot
        .sessions
        .rows
        .iter()
        .map(workspace_session_summary)
        .collect();
    // The complete head frontier is snapshot metadata, not a pageable collection. Collection
    // payloads share a byte budget as well as their independent SQL row quotas. Cutting only
    // suffixes preserves ordering; original totals still report every omitted fact.
    let mut remaining = 8 * 1024 * 1024usize;
    for rows in [
        &mut snapshot.conversations,
        &mut snapshot.bots,
        &mut snapshot.tasks,
        &mut snapshot.questions,
        &mut snapshot.checks,
        &mut snapshot.jobs,
        &mut snapshot.sessions,
        &mut snapshot.deliveries,
        &mut snapshot.relations,
        &mut snapshot.remote_owners,
    ] {
        let mut keep = 0;
        for row in &rows.rows {
            let size = serde_json::to_vec(row)
                .expect("JSON value serializes")
                .len()
                + 1;
            if size > remaining {
                break;
            }
            remaining -= size;
            keep += 1;
        }
        rows.rows.truncate(keep);
    }
    let truncated = [
        &snapshot.conversations,
        &snapshot.bots,
        &snapshot.tasks,
        &snapshot.questions,
        &snapshot.checks,
        &snapshot.jobs,
        &snapshot.sessions,
        &snapshot.deliveries,
        &snapshot.relations,
        &snapshot.remote_owners,
    ]
    .iter()
    .any(|rows| rows.omitted() > 0);
    let omitted_tasks = snapshot.tasks.omitted();
    let omitted_questions = snapshot.questions.omitted();
    let omitted_jobs = snapshot.jobs.omitted();
    let omitted_checks = snapshot.checks.omitted();
    let omitted_sessions = snapshot.sessions.omitted();
    let omitted_deliveries = snapshot.deliveries.omitted();
    let omitted_relations = snapshot.relations.omitted();
    json!({
        "schema":WORKSPACE_OVERVIEW_SCHEMA,"generated_at_ms":now_ms(),"network_epoch":snapshot.network_epoch,
        "heads":snapshot.heads,"consistency":if snapshot.remote_owners.total == 0 {"local_snapshot"} else {"partial"},
        "truncated":truncated,"conversations":snapshot.conversations.rows,"bots":snapshot.bots.rows,
        "tasks":snapshot.tasks.rows,"questions":snapshot.questions.rows,"checks":snapshot.checks.rows,"jobs":snapshot.jobs.rows,
        "sessions":snapshot.sessions.rows,"deliveries":snapshot.deliveries.rows,"relations":snapshot.relations.rows,
        "coverage": {"source":"workspace_overview","complete":false,"bounded":true,"local_heads":true,
            "remote_owners":snapshot.remote_owners.rows,"omitted_finished":snapshot.omitted_finished,
            "omitted_sessions":omitted_sessions,"omitted_tasks":omitted_tasks,"omitted_questions":omitted_questions,
            "omitted_jobs":omitted_jobs,"omitted_checks":omitted_checks,"omitted_deliveries":omitted_deliveries,
            "omitted_relations":omitted_relations}
    })
}

#[cfg(test)]
mod tests;
