//! The dependency closure needed by overview hold evaluation. This is deliberately private:
//! its graph is not a complete workspace and must never be passed to reducers or replay.
use super::*;
mod history;
use crate::scheduling::SchedulerJob;
pub(super) use history::graph_at_task;

pub(crate) struct OverviewFacts {
    graph: Graph,
}
impl OverviewFacts {
    pub(crate) fn read(
        connection: &Connection,
        network: &str,
        local: &str,
        jobs: &[SchedulerJob],
    ) -> Result<Self> {
        let selected=jobs.iter().filter(|job|job.state==crate::scheduling::JobState::Queued)
            .map(|job|serde_json::json!({"conversation":job.conversation_id,"scope":job.scope.storage_key(),
                "sequence":job.sequence,"reply":job.reply_to,"task":match &job.scope {
                    SessionScope::WorkspaceTask {task_id}=>Some(task_id),_=>None
                }})).collect::<Vec<_>>();
        if selected.is_empty() {
            return Ok(Self {
                graph: Graph::default(),
            });
        }
        let selected_json = serde_json::to_string(&selected)?;
        let active = overlapping_assignments(connection, network, local, &selected_json)?;
        let encoded:Option<String>=connection.query_row(
            "WITH RECURSIVE selected AS (
                SELECT json_extract(value,'$.conversation') AS conversation,json_extract(value,'$.scope') AS scope,
                    json_extract(value,'$.sequence') AS sequence,json_extract(value,'$.reply') AS reply,
                    json_extract(value,'$.task') AS task FROM json_each(?3)),
             inputs AS (SELECT reply AS id FROM selected UNION SELECT j.reply_to FROM selected k JOIN scheduler_jobs j
                ON j.conversation_id=k.conversation AND j.scope=k.scope AND j.sequence<k.sequence AND j.state='queued'),
             wanted(task_id) AS (
                SELECT task FROM selected WHERE task IS NOT NULL
                UNION SELECT json_extract(i.value,'$.target.task_id') FROM workspace_projection p,json_each(p.state,'$.inputs') i
                    WHERE p.network_id=?1 AND i.key IN (SELECT id FROM inputs) AND json_extract(i.value,'$.target.task_id') IS NOT NULL
                UNION SELECT d.value FROM wanted w JOIN workspace_tasks_lookup t ON t.network_id=?1 AND t.task_id=w.task_id,
                    json_each(t.record,'$.dependencies') d),
             tasks AS (SELECT t.task_id,t.record FROM workspace_tasks_lookup t WHERE t.network_id=?1 AND t.task_id IN (SELECT task_id FROM wanted)),
             channels AS (SELECT json_extract(record,'$.channel_id') AS id FROM tasks
                UNION SELECT json_extract(i.value,'$.target.channel_id') FROM workspace_projection p,json_each(p.state,'$.inputs') i
                    WHERE p.network_id=?1 AND i.key IN (SELECT id FROM inputs))
             SELECT json_object(
                'tasks',json((SELECT json_group_object(task_id,json(record)) FROM tasks)),
                'assignments',json((SELECT json_group_object(a.assignment_id,json(a.record)) FROM workspace_assignments_lookup a
                    WHERE a.network_id=?1 AND (json_extract(a.record,'$.task_id') IN (SELECT task FROM selected WHERE task IS NOT NULL)
                        OR a.assignment_id IN (SELECT value FROM json_each(?4))))),
                'channels',json((SELECT json_group_object(c.key,json(c.value)) FROM json_each(p.state,'$.channels') c WHERE c.key IN (SELECT id FROM channels))),
                'inputs',json((SELECT json_group_object(i.key,json(i.value)) FROM json_each(p.state,'$.inputs') i WHERE i.key IN (SELECT id FROM inputs))),
                'queue_order',json((SELECT json_group_array(q.value) FROM json_each(p.state,'$.queue_order') q WHERE q.value IN (SELECT id FROM inputs))),
                'commands',json('{}'),'last_rank','') FROM workspace_projection p WHERE p.network_id=?1",
            params![network,local,selected_json,serde_json::to_string(&active)?],|row|row.get(0)).optional()?;
        Ok(Self {
            graph: encoded
                .map(|value| serde_json::from_str(&value))
                .transpose()?
                .unwrap_or_default(),
        })
    }
    pub(crate) fn assignment_for_job(
        &self,
        connection: &Connection,
        network: &str,
        local: &str,
        job: &SchedulerJob,
    ) -> Result<Option<Assignment>> {
        workspace_assignment_for_job_with_graph(
            connection,
            network,
            local,
            job,
            || Ok(std::borrow::Cow::Borrowed(&self.graph)),
            true,
        )
    }
    pub(crate) fn task_execution_allowed(
        &self,
        connection: &Connection,
        network: &str,
        task: &str,
    ) -> Result<bool> {
        task_execution_allowed_with_graph(connection, network, task, || {
            Ok(std::borrow::Cow::Borrowed(&self.graph))
        })
    }
    pub(crate) fn input_resources_blocked(
        &self,
        connection: &Connection,
        network: &str,
        local: &str,
        job: &SchedulerJob,
    ) -> Result<bool> {
        workspace_input_resources_blocked_with_graph(connection, network, local, job, || {
            Ok(std::borrow::Cow::Borrowed(&self.graph))
        })
    }
    pub(crate) fn queued_input(
        &self,
        connection: &Connection,
        network: &str,
        input: &str,
    ) -> Result<Option<QueuedInput>> {
        queued_workspace_input_with_graph(connection, network, input, || {
            Ok(std::borrow::Cow::Borrowed(&self.graph))
        })
    }
    pub(crate) fn assignment_resources_available(
        &self,
        connection: &Connection,
        network: &str,
        assignment: &Assignment,
    ) -> Result<bool> {
        assignment_resources_available_with_graph(connection, network, assignment, || {
            Ok(std::borrow::Cow::Borrowed(&self.graph))
        })
    }
    pub(crate) fn resolve_target(
        &self,
        connection: &Connection,
        network: &str,
        target: &InputTarget,
    ) -> Result<ResolvedInputTarget> {
        resolve_input_target_with_graph(connection, network, target, || {
            Ok(std::borrow::Cow::Borrowed(&self.graph))
        })
    }
}

/// Resource checks need active overlapping reservations, never historical payloads. The
/// portable overlap predicate is the same one used below by resource_conflicts_from; this
/// prepass reads identity/directory facts before invoking the canonical work codec in bulk.
pub(super) fn resource_work_candidates(
    connection: &Connection,
    network: &str,
    node: &str,
    roots: &[String],
) -> Result<Vec<WorkProjection>> {
    let mut statement=connection.prepare(
        "SELECT w.work_id,json_extract(w.dispatch,'$.cwd'),r.value IS NOT NULL,json_extract(r.value,'$.native_cwd')
         FROM works w LEFT JOIN identity_meta r ON r.key='work-native-submission:'||w.work_id
         WHERE w.network_id=?1 AND w.target_node=?2 AND w.dispatch IS NOT NULL
            AND (w.finish IS NULL OR json_extract(w.finish,'$.outcome')='unverified')")?;
    let mut rows = statement.query(params![network, node])?;
    let mut candidates = Vec::new();
    while let Some(row) = rows.next()? {
        let reserved: bool = row.get(2)?;
        let occupied = if reserved {
            let cwd: Option<String> = row.get(3)?;
            let cwd = cwd.context("native work reservation has no directory")?;
            valid_roots(std::slice::from_ref(&cwd))?;
            cwd
        } else {
            row.get::<_, String>(1)?
        };
        if roots.iter().any(|root| overlap(root, &occupied)) {
            candidates.push(row.get::<_, String>(0)?);
        }
    }
    super::super::reduce::works_query(
        connection,
        "FROM works w WHERE w.network_id=?1 AND w.work_id IN (SELECT value FROM json_each(?2))",
        params![network, serde_json::to_string(&candidates)?],
    )
}

fn overlapping_assignments(
    connection: &Connection,
    network: &str,
    local: &str,
    selected: &str,
) -> Result<Vec<String>> {
    let mut roots = Vec::<String>::new();
    let mut statement=connection.prepare(
        "SELECT json_extract(a.record,'$.resource_roots') FROM workspace_assignments_lookup a WHERE a.network_id=?1
            AND json_extract(a.record,'$.task_id') IN (SELECT json_extract(value,'$.task') FROM json_each(?2))
         UNION ALL SELECT json_extract(i.value,'$.resource_roots') FROM json_each(?2) k JOIN identity_meta i
            ON i.key='workspace-input-assignment:'||?1||':'||json_extract(k.value,'$.reply')
            WHERE json_type(i.value,'$.resource_roots')='array'")?;
    let mut rows = statement.query(params![network, selected])?;
    while let Some(row) = rows.next()? {
        roots.extend(serde_json::from_str::<Vec<String>>(
            &row.get::<_, String>(0)?,
        )?);
    }
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement=connection.prepare(
        "SELECT assignment_id,json_extract(record,'$.resource_roots') FROM workspace_assignments_lookup
         WHERE network_id=?1 AND json_extract(record,'$.execution_node')=?2 AND json_extract(record,'$.state') NOT IN ('completed','stopped')")?;
    let mut rows = statement.query(params![network, local])?;
    let mut ids = Vec::new();
    while let Some(row) = rows.next()? {
        let occupied: Vec<String> = serde_json::from_str(&row.get::<_, String>(1)?)?;
        if roots
            .iter()
            .any(|root| occupied.iter().any(|other| overlap(root, other)))
        {
            ids.push(row.get(0)?);
        }
    }
    Ok(ids)
}
