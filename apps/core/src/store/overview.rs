//! Candidate-first reads for the human workspace overview. Every collection has its own
//! SQL quota and count; a truncated conversation list is never used as the authorization set.
use super::*;
use serde_json::json;

#[derive(Clone, Copy)]
pub(crate) struct OverviewQuery<'a> {
    pub network: &'a str,
    pub scope: Option<&'a str>,
    pub local: &'a str,
    pub limit: usize,
    pub include_finished: bool,
    pub include_sessions: bool,
}

pub(crate) struct OverviewRows {
    pub rows: Vec<Value>,
    pub total: usize,
}
impl OverviewRows {
    pub fn omitted(&self) -> usize {
        self.total.saturating_sub(self.rows.len())
    }
}

/// Counts describe the full authorized snapshot, independently of each selected prefix.
pub(crate) struct OverviewSnapshot {
    pub network_epoch: u64,
    pub heads: std::collections::BTreeMap<String, u64>,
    pub conversations: OverviewRows,
    pub bots: OverviewRows,
    pub tasks: OverviewRows,
    pub questions: OverviewRows,
    pub checks: OverviewRows,
    pub jobs: OverviewRows,
    pub sessions: OverviewRows,
    pub deliveries: OverviewRows,
    pub relations: OverviewRows,
    pub remote_owners: OverviewRows,
    pub omitted_finished: usize,
}

// ?1 network, ?2 optional requested scope. A network must be present in every shared-table
// join, including EXISTS probes from local tables that have no network column.
const SCOPES: &str = "WITH scopes AS (
    SELECT c.conversation_id,c.title,c.owner,c.owner_epoch,b.bot_id,
        CASE WHEN n.rank>b.rank THEN n.value ELSE b.name END AS name
    FROM conversations c JOIN bots b ON b.network_id=c.network_id AND b.conversation_id=c.conversation_id
    LEFT JOIN bot_profiles n ON n.network_id=b.network_id AND n.conversation_id=b.conversation_id AND n.field='name'
    WHERE c.network_id=?1 AND c.title IS NOT NULL AND (?2 IS NULL OR c.conversation_id=?2)
        AND NOT EXISTS(SELECT 1 FROM bot_deletions d WHERE d.network_id=c.network_id AND d.conversation_id=c.conversation_id)
), newest_ended AS MATERIALIZED (
    SELECT e.conversation_id,max(e.dispatch_rank) AS newest
    FROM scopes s JOIN works e ON e.network_id=?1 AND e.conversation_id=s.conversation_id
    WHERE e.dispatch IS NOT NULL AND e.finish IS NOT NULL
        AND json_extract(e.finish,'$.outcome')<>'unverified'
    GROUP BY e.conversation_id
)";

// Historical payloads are examined inside SQLite; Rust only decodes the selected summaries.
// The report-ID check is supplied separately because reply identities are SHA256, not stored
// work foreign keys. It must not be approximated by an unrelated later assistant message.
const OPEN_WORK: &str = "(w.finish IS NULL OR json_extract(w.finish,'$.outcome')='unverified'
    OR json_extract(w.finish,'$.outcome') IN ('failed','rejected')
    AND w.dispatch_rank=ended.newest
    AND NOT EXISTS(SELECT 1 FROM notification_acks a WHERE a.network_id=w.network_id
        AND a.notification_id=w.work_id AND a.client_id='inbox')
    AND NOT EXISTS(SELECT 1 FROM messages r JOIN messages u ON u.network_id=r.network_id
        AND u.conversation_id=r.conversation_id AND u.message_id=r.in_reply_to
        WHERE r.network_id=w.network_id AND r.conversation_id=w.conversation_id
            AND r.role='assistant' AND r.status='completed' AND u.role='user'
            AND u.rank>substr(w.finish_rank,3)))";

const OPEN_REFUSAL: &str = "NOT EXISTS(SELECT 1 FROM works w WHERE w.network_id=?1
        AND w.work_id=d.work_id AND w.dispatch IS NOT NULL)
    AND NOT EXISTS(SELECT 1 FROM notification_acks a WHERE a.network_id=?1
        AND a.notification_id=d.work_id AND a.client_id='inbox')
    AND NOT EXISTS(SELECT 1 FROM scheduler_jobs j
        JOIN messages report ON report.network_id=?1 AND report.conversation_id=j.conversation_id AND report.message_id=j.message_id
        JOIN messages m ON m.network_id=?1 AND m.conversation_id=j.conversation_id
            AND m.role='assistant' AND m.status='completed' AND m.rank>=report.rank
        WHERE j.conversation_id=json_extract(d.spec,'$.conversation') AND j.trigger='work:'||d.work_id)
    AND NOT EXISTS(SELECT 1 FROM scheduler_jobs j
        JOIN messages turn ON turn.network_id=?1 AND turn.conversation_id=j.conversation_id AND turn.message_id=j.message_id
        JOIN messages u ON u.network_id=?1 AND u.conversation_id=j.conversation_id AND u.role='user' AND u.rank>turn.rank
        JOIN messages r ON r.network_id=?1 AND r.conversation_id=j.conversation_id
            AND r.in_reply_to=u.message_id AND r.role='assistant' AND r.status='completed'
        WHERE j.conversation_id=json_extract(d.spec,'$.conversation') AND j.trigger=json_extract(d.spec,'$.trigger')
            AND NOT EXISTS(SELECT 1 FROM scheduler_jobs report WHERE report.conversation_id=j.conversation_id
                AND report.trigger='work:'||d.work_id))";

struct Reader<'a> {
    store: &'a Store,
    query: OverviewQuery<'a>,
}
impl Reader<'_> {
    fn count(&self, from: &str, extra: &str) -> Result<usize> {
        Ok(self.store.connection.query_row(
            &format!("{SCOPES} SELECT count(*) {from}"),
            params![self.query.network, self.query.scope, extra],
            |row| row.get(0),
        )?)
    }

    fn rows(
        &self,
        select: &str,
        from: &str,
        order: &str,
        limit: usize,
        extra: &str,
    ) -> Result<Vec<Value>> {
        let mut statement = self.store.connection.prepare(&format!(
            "{SCOPES} SELECT {select} {from} ORDER BY {order} LIMIT ?4"
        ))?;
        let rows = statement.query_map(
            params![self.query.network, self.query.scope, extra, limit],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| {
            #[cfg(test)]
            DECODED_ROWS.with(|count| count.set(count.get() + 1));
            Ok(serde_json::from_str(&row?)?)
        })
        .collect()
    }

    fn collection(
        &self,
        select: &str,
        from: &str,
        order: &str,
        extra: &str,
    ) -> Result<OverviewRows> {
        Ok(OverviewRows {
            rows: self.rows(select, from, order, self.query.limit, extra)?,
            total: self.count(from, extra)?,
        })
    }

    /// Only the newest ended failure per conversation can need a report probe. Stream these
    /// lightweight identities and batch their reply-ID lookups; never hydrate historical work.
    fn reported_failures(&self) -> Result<Vec<String>> {
        if self.query.include_finished {
            return Ok(Vec::new());
        }
        let mut statement = self.store.connection.prepare(&format!(
            "{SCOPES} SELECT w.work_id,w.conversation_id,s.owner_epoch FROM works w
             JOIN scopes s ON s.conversation_id=w.conversation_id
             LEFT JOIN newest_ended ended ON ended.conversation_id=w.conversation_id
             WHERE w.network_id=?1 AND w.dispatch IS NOT NULL AND {OPEN_WORK}
                AND json_extract(w.finish,'$.outcome') IN ('failed','rejected')"
        ))?;
        let mut rows = statement.query(params![self.query.network, self.query.scope])?;
        let mut reported = std::collections::BTreeSet::new();
        let mut candidates = Vec::new();
        fn probe(
            connection: &Connection,
            network: &str,
            candidates: &mut Vec<(String, String, String)>,
            reported: &mut std::collections::BTreeSet<String>,
        ) -> Result<()> {
            if candidates.is_empty() {
                return Ok(());
            }
            let mut statement=connection.prepare(
                "SELECT DISTINCT json_extract(ids.value,'$[0]') FROM json_each(?2) ids JOIN messages m
                 ON m.network_id=?1 AND m.conversation_id=json_extract(ids.value,'$[1]') AND m.message_id=json_extract(ids.value,'$[2]')
                 WHERE m.role='assistant' AND m.status='completed'")?;
            let rows = statement.query_map(
                params![network, serde_json::to_string(candidates)?],
                |row| row.get::<_, String>(0),
            )?;
            for row in rows {
                reported.insert(row?);
            }
            candidates.clear();
            Ok(())
        }
        while let Some(row) = rows.next()? {
            let work: String = row.get(0)?;
            let conversation: String = row.get(1)?;
            let epoch: u64 = row.get(2)?;
            let trigger = crate::scheduling::work_trigger(&work);
            for epoch in 0..=epoch {
                if reported.contains(&work) {
                    break;
                }
                candidates.push((
                    work.clone(),
                    conversation.clone(),
                    super::scheduler::reply_message_id(&conversation, &trigger, epoch),
                ));
                if candidates.len() == 256 {
                    probe(
                        &self.store.connection,
                        self.query.network,
                        &mut candidates,
                        &mut reported,
                    )?;
                }
            }
        }
        probe(
            &self.store.connection,
            self.query.network,
            &mut candidates,
            &mut reported,
        )?;
        Ok(reported.into_iter().collect())
    }
}

impl Store {
    pub(crate) fn overview_snapshot(
        &self,
        query: OverviewQuery<'_>,
        mut project_job: impl FnMut(
            &Store,
            &crate::scheduling::SchedulerJob,
            u64,
            bool,
            Option<crate::scheduling::ContextProjection>,
            &super::scheduler::overview::JobFacts,
        ) -> Result<Value>,
    ) -> Result<OverviewSnapshot> {
        let reader = Reader { store: self, query };
        let conversations = reader.collection(
            "json_object('conversation_id',s.conversation_id,'title',s.title,'owner',s.owner,'owner_epoch',s.owner_epoch)",
            "FROM scopes s WHERE ?3 IS NOT NULL", "s.conversation_id", "")?;
        let bots = reader.collection(
            "json_object('bot_id',s.bot_id,'conversation_id',s.conversation_id,'name',s.name)",
            "FROM scopes s WHERE ?3 IS NOT NULL",
            "s.bot_id",
            "",
        )?;
        let remote_owners = reader.collection(
            "json_object('owner',s.owner,'reachable',NULL,'status','unknown','owner_epoch',s.owner_epoch,
                'reason',CASE WHEN s.owner IS NULL THEN 'owner_unknown' ELSE 'remote_scheduler_not_queried' END)",
            "FROM scopes s WHERE s.owner IS NULL OR s.owner<>?3", "s.conversation_id", query.local)?;
        let reported = serde_json::to_string(&reader.reported_failures()?)?;
        let work_filter = if query.include_finished {
            "?3 IS NOT NULL".to_owned()
        } else {
            format!("{OPEN_WORK} AND w.work_id NOT IN (SELECT value FROM json_each(?3))")
        };
        let ended_join = if query.include_finished {
            ""
        } else {
            "LEFT JOIN newest_ended ended ON ended.conversation_id=w.conversation_id"
        };
        let work_from = format!(
            "FROM works w JOIN scopes s ON s.conversation_id=w.conversation_id {ended_join}
            WHERE w.network_id=?1 AND w.dispatch IS NOT NULL AND {work_filter}"
        );
        let mut tasks = reader.collection(
            WORK_SUMMARY,
            &work_from,
            "w.conversation_id,w.dispatch_rank,w.rowid",
            &reported,
        )?;
        for value in &mut tasks.rows {
            finish_work_summary(value)?;
        }
        let omitted_finished = if query.include_finished {
            0
        } else {
            reader.count(
                "FROM works w JOIN scopes s ON s.conversation_id=w.conversation_id
                WHERE w.network_id=?1 AND w.dispatch IS NOT NULL AND w.finish IS NOT NULL
                    AND json_extract(w.finish,'$.outcome')<>'unverified' AND ?3 IS NOT NULL",
                "",
            )?
        };
        let dispatch_from = format!("FROM work_dispatches d JOIN scopes s ON s.conversation_id=json_extract(d.spec,'$.conversation')
            WHERE (d.state<>'accepted' OR NOT EXISTS(SELECT 1 FROM works w
                WHERE w.network_id=?1 AND w.work_id=d.work_id AND w.dispatch IS NOT NULL))
            AND (?3='all' OR d.state<>'rejected' OR ({OPEN_REFUSAL}))");
        let dispatch_mode = if query.include_finished {
            "all"
        } else {
            "open"
        };
        let mut dispatches =
            reader.collection(DISPATCH_SUMMARY, &dispatch_from, "d.work_id", dispatch_mode)?;
        for value in &mut dispatches.rows {
            finish_dispatch_summary(value)?;
        }
        tasks.total += dispatches.total;
        let remaining = query.limit.saturating_sub(tasks.rows.len());
        for value in dispatches.rows.iter().take(remaining) {
            let mut task = value.clone();
            for key in ["requested_at_ms", "deadline_ms", "reason"] {
                task.as_object_mut().unwrap().remove(key);
            }
            task["source"] = json!("local_dispatch");
            task["summary"] = Value::Null;
            task["outcome"] = Value::Null;
            if task["state"] == "dispatch_accepted" {
                task["state"] = json!("accepted_waiting_sync");
            }
            task["unknown_reason"] = match task["state"].as_str() {
                Some("accepted_waiting_sync") => json!("accepted_waiting_sync"),
                Some("dispatch_unverified") => json!("dispatch_unverified"),
                _ => Value::Null,
            };
            if value["reason"].is_string() {
                task["failure_reason"] = value["reason"].clone();
            }
            if task["failure"].is_null() {
                task.as_object_mut().unwrap().remove("failure");
            }
            tasks.rows.push(task);
        }
        let mut deliveries = dispatches;
        for value in &mut deliveries.rows {
            for key in ["trigger", "dispatcher", "parent", "continues", "thread"] {
                value.as_object_mut().unwrap().remove(key);
            }
        }
        let stop_from = "FROM work_stops t JOIN scopes s ON s.conversation_id=t.conversation_id
            LEFT JOIN works w ON w.network_id=?1 AND w.work_id=t.work_id AND w.dispatch IS NOT NULL WHERE ?3 IS NOT NULL";
        deliveries.total += reader.count(stop_from, "")?;
        let mut stops = reader.rows(
            "json_object('work_id',t.work_id,'conversation_id',t.conversation_id,'target_node',t.target_node,
                'state',CASE WHEN t.timed_out THEN 'stop_timed_out' ELSE 'stop_waiting' END,
                'requested_at_ms',t.requested_at_ms,'deadline_ms',t.deadline_ms,'cwd',json_extract(w.dispatch,'$.cwd'))",
            stop_from,"t.work_id",query.limit.saturating_sub(deliveries.rows.len()),"")?;
        for stop in &mut stops {
            if stop["cwd"].is_null() {
                stop.as_object_mut().unwrap().remove("cwd");
            }
        }
        deliveries.rows.extend(stops);
        let questions = reader.collection(
            "json_set(q.body,'$.conversation_id',q.conversation_id,'$.bot',json_object('bot_id',s.bot_id,'name',s.name),
                '$.author',q.author,'$.created_at_ms',q.created_at_ms)",
            "FROM questions q JOIN scopes s ON s.conversation_id=q.conversation_id
                LEFT JOIN messages m ON m.network_id=q.network_id AND m.conversation_id=q.conversation_id AND m.answers=q.question_id
                WHERE q.network_id=?1 AND q.closed IS NULL AND m.message_id IS NULL AND ?3 IS NOT NULL",
            "q.rank,q.rowid","")?;
        let mut checks = reader.collection(
            "json_object('attempt_id',c.attempt_id,'conversation_id',c.conversation_id,'trigger',c.trigger,'created_at_ms',c.created_at_ms,
                'unknown_reason','process_unconfirmed','codex_turn',CASE WHEN t.server IS NOT NULL AND t.thread_id IS NOT NULL
                    THEN json_object('server',t.server,'thread',t.thread_id,'turn',t.turn_id) END)",
            "FROM pending_checks c JOIN scopes s ON s.conversation_id=c.conversation_id
                LEFT JOIN manager_turns t ON t.attempt_id=c.attempt_id WHERE ?3 IS NOT NULL", "c.created_at_ms,c.rowid","")?;
        for check in &mut checks.rows {
            check["check_id"] = json!(crate::scheduling::PendingCheck::id(
                check["attempt_id"]
                    .as_str()
                    .context("missing check attempt")?
            ));
            check.as_object_mut().unwrap().remove("attempt_id");
        }
        let mut sessions = if query.include_sessions {
            reader.collection(
                "json_set(s.attribution,'$.conversation_id',s.conversation_id,'$.node',s.node,'$.session_id',s.session_id,
                    '$.muted',json(CASE WHEN s.muted THEN 'true' ELSE 'false' END),'$.created_at_ms',s.created_at_ms,'$.rank',s.rank)",
                "FROM session_owners s JOIN scopes active ON active.conversation_id=s.conversation_id
                    WHERE s.network_id=?1 AND s.attribution IS NOT NULL AND ?3 IS NOT NULL", "s.rank,s.rowid","")?
        } else {
            OverviewRows {
                rows: Vec::new(),
                total: 0,
            }
        };
        for session in &mut sessions.rows {
            session["version"] = json!(super::version(
                session["rank"].as_str().context("missing session rank")?
            ));
        }
        let relation_filter = if query.include_finished {
            ""
        } else {
            " AND (w.finish IS NULL OR json_extract(w.finish,'$.outcome')='unverified')"
        };
        let relation_from = format!("FROM (SELECT w.conversation_id,w.dispatch_rank,w.rowid AS work_rowid,w.work_id,
            json_extract(w.dispatch,'$.parent') AS parent,json_extract(w.dispatch,'$.continues') AS continues {work_from}{relation_filter}) w
            JOIN (SELECT 0 AS ordinal UNION ALL SELECT 1 UNION ALL SELECT 2) k
            WHERE (k.ordinal<2 AND w.parent IS NOT NULL) OR (k.ordinal=2 AND w.continues IS NOT NULL)");
        let relations = reader.collection(
            "json_object('from_work',CASE WHEN k.ordinal=1 THEN w.parent ELSE w.work_id END,
                'to_work',CASE k.ordinal WHEN 0 THEN w.parent WHEN 1 THEN w.work_id ELSE w.continues END,
                'kind',CASE k.ordinal WHEN 0 THEN 'parent' WHEN 1 THEN 'dispatches' ELSE 'continues' END)",
            &relation_from,"w.conversation_id,w.dispatch_rank,w.work_rowid,k.ordinal",&reported)?;
        let job_from = format!(
            "FROM scheduler_jobs j JOIN scopes s ON s.conversation_id=j.conversation_id WHERE ?3 IS NOT NULL {}",
            if query.include_finished {
                ""
            } else {
                "AND j.state IN ('queued','steering','claimed','running','cancelling','reconciling')"
            }
        );
        let metadata = "json_object('conversation_id',j.conversation_id,'trigger',j.trigger,'owner_epoch',s.owner_epoch,
            'pending',EXISTS(SELECT 1 FROM pending_checks p WHERE p.conversation_id=j.conversation_id AND p.trigger=j.trigger))";
        let job_keys = reader.collection(metadata, &job_from, "j.sequence,j.rowid", "")?;
        let triggers = tasks
            .rows
            .iter()
            .filter_map(|task| task["work_id"].as_str())
            .map(crate::scheduling::task_trigger)
            .collect::<Vec<_>>();
        let coordination = reader.rows(metadata,
            &format!("{job_from} AND j.trigger IN (SELECT value FROM json_each(?3)) AND j.rowid=(
                SELECT j2.rowid FROM scheduler_jobs j2 JOIN scopes s2 ON s2.conversation_id=j2.conversation_id
                WHERE j2.trigger=j.trigger {} ORDER BY j2.sequence,j2.rowid LIMIT 1)",
                if query.include_finished {""} else {"AND j2.state IN ('queued','steering','claimed','running','cancelling','reconciling')"}),
            "j.sequence,j.rowid",query.limit,&serde_json::to_string(&triggers)?)?;
        let key = |value: &Value| -> Result<(String, String)> {
            Ok((
                value["conversation_id"]
                    .as_str()
                    .context("missing job scope")?
                    .to_owned(),
                value["trigger"]
                    .as_str()
                    .context("missing job trigger")?
                    .to_owned(),
            ))
        };
        let top_keys = job_keys
            .rows
            .iter()
            .map(&key)
            .collect::<Result<std::collections::BTreeSet<_>>>()?;
        let mut all_keys = std::collections::BTreeMap::new();
        for metadata in job_keys.rows.iter().chain(&coordination) {
            all_keys.insert(key(metadata)?, metadata);
        }
        let selected = json!(all_keys.values().collect::<Vec<_>>());
        let hydrated = super::scheduler::overview::jobs(&self.connection, &selected)?;
        let mut contexts = super::scheduler::overview::contexts(&self.connection, &hydrated)?;
        let facts = super::scheduler::overview::JobFacts::read(
            self,
            query.network,
            query.local,
            &hydrated,
        )?;
        let mut projected = Vec::new();
        for job in hydrated {
            let key = (job.conversation_id.clone(), job.trigger.clone());
            let metadata = all_keys.get(&key).context("job metadata missing")?;
            projected.push(project_job(
                self,
                &job,
                metadata["owner_epoch"].as_u64().unwrap_or(0),
                metadata["pending"] == 1,
                contexts.remove(&key),
                &facts,
            )?);
        }
        let mut jobs = OverviewRows {
            rows: Vec::new(),
            total: job_keys.total,
        };
        for job in &projected {
            if top_keys.contains(&key(job)?) {
                jobs.rows.push(job.clone());
            }
        }
        for task in &mut tasks.rows {
            let trigger = crate::scheduling::task_trigger(
                task["work_id"].as_str().context("missing work ID")?,
            );
            task["coordination_job"] = projected
                .iter()
                .find(|job| job["trigger"] == trigger)
                .cloned()
                .unwrap_or(Value::Null);
        }
        Ok(OverviewSnapshot {
            network_epoch: self.public_state()?.project()?.epoch,
            heads: self
                .heads(query.network)?
                .into_iter()
                .map(|(author, (sequence, _))| (author, sequence))
                .collect(),
            conversations,
            bots,
            tasks,
            questions,
            checks,
            jobs,
            sessions,
            deliveries,
            relations,
            remote_owners,
            omitted_finished,
        })
    }
}

const WORK_SUMMARY: &str = "json_object(
    'work_id',w.work_id,'conversation_id',w.conversation_id,'target_node',w.target_node,
    'trigger',json_extract(w.dispatch,'$.trigger'),'dispatcher',json_extract(w.dispatch,'$.dispatcher'),
    'kind',json_extract(w.dispatch,'$.kind'),'cwd',json_extract(w.dispatch,'$.cwd'),'title',json_extract(w.dispatch,'$.title'),
    'call_ref',json_extract(w.dispatch,'$.call_ref'),'model',json_extract(w.dispatch,'$.model'),'resolved',json_extract(w.dispatch,'$.resolved'),
    'continues',json_extract(w.dispatch,'$.continues'),'parent',json_extract(w.dispatch,'$.parent'),'thread',json_extract(w.dispatch,'$.thread'),
    'phase',json_extract(w.progress,'$.phase'),'summary',coalesce(json_extract(w.progress,'$.summary'),''),
    'thread_id',w.thread_id,'turn_id',w.turn_id,'outcome',json_extract(w.finish,'$.outcome'),
    'failure_reason',json_extract(w.finish,'$.failure_reason'),'failure',json_extract(w.finish,'$.failure'),'source','dag')";

fn finish_work_summary(value: &mut Value) -> Result<()> {
    value["state"] = json!(WorkStatus::of(
        serde_json::from_value(value["kind"].clone())?,
        serde_json::from_value(value["phase"].clone())?,
        serde_json::from_value(value["outcome"].clone())?
    ));
    if value["failure_reason"].as_str().is_none_or(str::is_empty) {
        value.as_object_mut().unwrap().remove("failure_reason");
    }
    if value["failure"].is_null() {
        value.as_object_mut().unwrap().remove("failure");
    }
    if value["outcome"] == "unverified" {
        value["unknown_reason"] = json!("work_result_unverified");
    }
    Ok(())
}

fn finish_dispatch_summary(value: &mut Value) -> Result<()> {
    use crate::work::DispatchState;
    let state = match value["state"].as_str().context("missing dispatch state")? {
        "pending" => DispatchState::Pending,
        "accepted" => DispatchState::Accepted,
        "rejected" => DispatchState::Rejected {
            reason: value["reason"].as_str().unwrap_or_default().into(),
            failure: serde_json::from_value(value["failure"].clone())?,
        },
        "unverified" => DispatchState::Unverified {
            reason: value["reason"].as_str().unwrap_or_default().into(),
        },
        other => bail!("unknown dispatch state {other}"),
    };
    value["state"] = json!(state.name());
    Ok(())
}

const DISPATCH_SUMMARY: &str = "json_object('work_id',d.work_id,'conversation_id',json_extract(d.spec,'$.conversation'),
    'trigger',json_extract(d.spec,'$.trigger'),'target_node',json_extract(d.spec,'$.target'),
    'state',d.state,
    'kind',json_extract(d.spec,'$.kind'),'title',json_extract(d.spec,'$.title'),'cwd',json_extract(d.spec,'$.cwd'),
    'dispatcher',json_extract(d.spec,'$.dispatcher'),'parent',json_extract(d.spec,'$.parent'),
    'continues',json_extract(d.spec,'$.continues'),'thread',json_extract(d.spec,'$.thread'),
    'reason',CASE WHEN d.state IN ('rejected','unverified') THEN coalesce(d.reason,'') END,
    'failure',CASE WHEN d.state='rejected' THEN d.failure END)";

#[cfg(test)]
thread_local! {static DECODED_ROWS:std::cell::Cell<usize>=const {std::cell::Cell::new(0)};}
#[cfg(test)]
mod tests;
