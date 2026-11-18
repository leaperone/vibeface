//! Resource reservations shared by workspace assignments, Work and scheduler sessions.
use super::{
    graph::Graph,
    query::{bot_scope, read_graph},
    validation::{overlap, valid_roots},
};
use crate::{
    model::{BotProject, WorkKind, WorkProjection, WorkStatus},
    scheduling::SessionScope,
    store::Store,
    workspace::{Assignment, AssignmentState},
};
use anyhow::{Context as _, Result};
use rusqlite::{Connection, OptionalExtension as _, params};
use std::collections::BTreeSet;

pub(super) enum ResourceQuery<'a> {
    Assignment(Option<&'a str>),
    Work(&'a str),
}

pub(super) fn resource_conflicts_from(
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
        .map(|id| crate::store::reduce::work_from(connection, network, id))
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
    for work in resource_work_candidates(connection, network, node, roots)? {
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
                    && crate::store::scheduler::work_shares_native_execution_from(
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

/// Resource checks need active overlapping reservations, never historical payloads. The
/// portable overlap predicate is the same one used by resource_conflicts_from; this
/// prepass reads identity/directory facts before invoking the canonical work codec in bulk.
fn resource_work_candidates(
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
    crate::store::reduce::works_query(
        connection,
        "FROM works w WHERE w.network_id=?1 AND w.work_id IN (SELECT value FROM json_each(?2))",
        params![network, serde_json::to_string(&candidates)?],
    )
}

impl Store {
    pub fn workspace_resource_conflicts(
        &self,
        network: &str,
        node: &str,
        roots: &[String],
        except_assignment: Option<&str>,
    ) -> Result<Vec<String>> {
        let graph = read_graph(&self.connection, network)?;
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
}

pub(crate) fn assignment_resources_available_from(
    connection: &Connection,
    network: &str,
    assignment: &Assignment,
) -> Result<bool> {
    assignment_resources_available_with_graph(connection, network, assignment, || {
        read_graph(connection, network).map(std::borrow::Cow::Owned)
    })
}

pub(super) fn assignment_resources_available_with_graph<'a>(
    connection: &Connection,
    network: &str,
    assignment: &Assignment,
    graph: impl FnOnce() -> Result<std::borrow::Cow<'a, Graph>>,
) -> Result<bool> {
    let graph = graph()?;
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
    let graph = read_graph(connection, network)?;
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
