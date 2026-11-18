//! SQLite projections and causal history reads for the workspace DAG.
#[cfg(test)]
use super::PROJECTION_ANCESTORS;
use super::{
    graph::Graph,
    input::{queued_inputs_from, target_task},
};
use crate::{
    crypto::request_key,
    model::{Event, EventBody, Role, SignedEvent, VersionVector},
    store::{Store, log},
    workspace::{
        Assignment, AssignmentState, InputTarget, WorkspaceCommand, WorkspaceCoverage,
        WorkspaceHistoryItem, WorkspaceSpeaker, WorkspaceState, WorkspaceTask,
    },
};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OptionalExtension as _, params};
#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub(super) fn read_graph(connection: &Connection, network: &str) -> Result<Graph> {
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

pub(super) fn save_graph(graph: &Graph, connection: &Connection, network: &str) -> Result<()> {
    let state = serde_json::to_string(graph)?;
    connection.execute_batch("SAVEPOINT workspace_lookup_save")?;
    let saved = (|| {
        connection.execute("INSERT INTO workspace_projection(network_id,state) VALUES(?,?) ON CONFLICT(network_id) DO UPDATE SET state=excluded.state", params![network, state])?;
        save_lookups(graph, connection, network)
    })();
    match saved {
        Ok(()) => {
            connection.execute_batch("RELEASE workspace_lookup_save")?;
            Ok(())
        }
        Err(error) => {
            connection.execute_batch(
                "ROLLBACK TO workspace_lookup_save; RELEASE workspace_lookup_save",
            )?;
            Err(error)
        }
    }
}

fn save_lookups(graph: &Graph, connection: &Connection, network: &str) -> Result<()> {
    if graph.replace_lookups {
        connection.execute(
            "DELETE FROM workspace_tasks_lookup WHERE network_id=?",
            [network],
        )?;
        connection.execute(
            "DELETE FROM workspace_assignments_lookup WHERE network_id=?",
            [network],
        )?;
    }
    let task_ids: Box<dyn Iterator<Item = &String> + '_> = if graph.replace_lookups {
        Box::new(graph.tasks.keys())
    } else {
        Box::new(graph.dirty_tasks.iter())
    };
    let mut task_insert = connection.prepare_cached(
        "INSERT INTO workspace_tasks_lookup(network_id,task_id,record) VALUES(?,?,?)
            ON CONFLICT(network_id,task_id) DO UPDATE SET record=excluded.record",
    )?;
    for id in task_ids {
        if let Some(task) = graph.tasks.get(id) {
            task_insert.execute(params![network, id, serde_json::to_string(task)?])?;
        } else {
            connection.execute(
                "DELETE FROM workspace_tasks_lookup WHERE network_id=? AND task_id=?",
                params![network, id],
            )?;
        }
    }
    let assignment_ids: Box<dyn Iterator<Item = &String> + '_> = if graph.replace_lookups {
        Box::new(graph.assignments.keys())
    } else {
        Box::new(graph.dirty_assignments.iter())
    };
    let mut assignment_insert = connection.prepare_cached("INSERT INTO workspace_assignments_lookup(network_id,assignment_id,work_id,created_at_ms,record) VALUES(?,?,?,?,?)
            ON CONFLICT(network_id,assignment_id) DO UPDATE SET work_id=excluded.work_id,created_at_ms=excluded.created_at_ms,record=excluded.record")?;
    for id in assignment_ids {
        if let Some(assignment) = graph.assignments.get(id) {
            assignment_insert.execute(params![
                network,
                id,
                assignment.work_id,
                assignment.created_at_ms,
                serde_json::to_string(assignment)?
            ])?;
        } else {
            connection.execute(
                "DELETE FROM workspace_assignments_lookup WHERE network_id=? AND assignment_id=?",
                params![network, id],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
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
const WORKSPACE_CAUSAL_QUERY: &str = "SELECT e.hash,e.envelope,e.lamport,e.created_at_ms,e.causal_clock FROM json_each(?2) AS known CROSS JOIN events e WHERE e.network_id=?1 AND e.causal_chain=known.key AND e.seq<=json_extract(known.value,'$.seq') AND e.effective=1 AND e.kind IN ('workspace_commanded','input_posted') ORDER BY e.lamport,e.created_at_ms,e.hash";
const WORKSPACE_ORDER_QUERY: &str = "SELECT hash,envelope,lamport,created_at_ms,causal_clock FROM events WHERE network_id=? AND effective=1 AND kind IN ('workspace_commanded','input_posted') ORDER BY lamport,created_at_ms,hash";
const OWNER_CAUSAL_QUERY: &str = "SELECT e.envelope FROM json_each(?2) AS known CROSS JOIN events e WHERE e.network_id=?1 AND e.scope_id=?3 AND e.kind='conversation_owner' AND e.causal_chain=known.key AND e.seq<=json_extract(known.value,'$.seq')";

pub(super) fn graph_at(
    connection: &Connection,
    network: &str,
    known: Option<&log::CausalClock>,
) -> Result<Graph> {
    let mut graph = Graph {
        replace_lookups: true,
        ..Default::default()
    };
    let mut statement = connection.prepare(if known.is_some() {
        WORKSPACE_CAUSAL_QUERY
    } else {
        WORKSPACE_ORDER_QUERY
    })?;
    let clock = known.map(serde_json::to_string).transpose()?;
    let mut rows = match &clock {
        Some(clock) => statement.query(params![network, clock])?,
        None => statement.query([network])?,
    };
    while let Some(row) = rows.next()? {
        let hash: String = row.get(0)?;
        let envelope: SignedEvent = serde_json::from_str(&row.get::<_, String>(1)?)?;
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        let basis = projection_basis(connection, network, &event, || {
            let basis: log::CausalClock = serde_json::from_str(&row.get::<_, String>(4)?)?;
            Ok(basis.version_vector())
        })?;
        graph.record(
            &hash,
            &event,
            &crate::store::rank(row.get(2)?, row.get(3)?, &hash),
            basis,
        )?;
    }
    Ok(graph)
}

pub(super) fn projection_basis(
    connection: &Connection,
    network: &str,
    event: &Event,
    cached: impl FnOnce() -> Result<VersionVector>,
) -> Result<VersionVector> {
    #[cfg(test)]
    if log::test_metrics::legacy() {
        PROJECTION_ANCESTORS.with(|count| count.set(count.get() + 1));
        return Ok(ancestors(connection, network, event)?.values().fold(
            VersionVector::new(),
            |mut vector, event| {
                let seq = vector.entry(event.author.clone()).or_default();
                *seq = (*seq).max(event.seq);
                vector
            },
        ));
    }
    #[cfg(not(test))]
    let _ = (connection, network);
    if matches!(
        &event.body,
        EventBody::WorkspaceCommanded {
            command: WorkspaceCommand::AssignTask { .. },
            ..
        }
    ) {
        cached()
    } else {
        Ok(VersionVector::new())
    }
}
pub(crate) fn apply_projection(
    connection: &Connection,
    network: &str,
    hash: &str,
    event: &Event,
    rank: &str,
) -> Result<()> {
    let mut graph = read_graph(connection, network)?;
    if rank < graph.last_rank.as_str() {
        graph = graph_at(connection, network, None)?;
    } else {
        let basis = projection_basis(connection, network, event, || {
            Ok(log::CausalClock::stored(connection, network, hash)?.version_vector())
        })?;
        graph.record(hash, event, rank, basis)?;
    }
    save_graph(&graph, connection, network)?;
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

pub(super) fn bot_scope(connection: &Connection, network: &str, bot_id: &str) -> Result<String> {
    connection
        .query_row(
            "SELECT conversation_id FROM bots WHERE network_id=? AND bot_id=?",
            params![network, bot_id],
            |row| row.get(0),
        )
        .optional()?
        .context("Bot not found in this personal network")
}
pub(super) fn owner_at(
    connection: &Connection,
    network: &str,
    known: &log::CausalClock,
    scope: &str,
) -> Result<(String, u64)> {
    let mut owners = Vec::new();
    let mut statement = connection.prepare(OWNER_CAUSAL_QUERY)?;
    let mut rows = statement.query(params![network, serde_json::to_string(known)?, scope])?;
    while let Some(row) = rows.next()? {
        let envelope: SignedEvent = serde_json::from_str(&row.get::<_, String>(0)?)?;
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
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
#[cfg(test)]
pub(in crate::store) fn assert_causal_reads_match_recursive_history(
    connection: &Connection,
    network: &str,
    event: &Event,
) -> Result<()> {
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
            let parent =
                log::event_from(connection, network, &hash)?.context("oracle parent missing")?;
            pending.extend(parent.prev.iter().chain(&parent.deps).cloned());
            result.insert(hash, parent);
        }
        Ok(result)
    }
    fn vector(events: &BTreeMap<String, Event>) -> VersionVector {
        events
            .values()
            .fold(VersionVector::new(), |mut vector, event| {
                let seq = vector.entry(event.author.clone()).or_default();
                *seq = (*seq).max(event.seq);
                vector
            })
    }
    let known = ancestors(connection, network, event)?;
    let clock = log::CausalClock::before(connection, network, event)?;
    assert_eq!(clock.version_vector(), vector(&known));
    for (query, index, causal) in [
        (WORKSPACE_CAUSAL_QUERY, "events_workspace_causal", true),
        (WORKSPACE_ORDER_QUERY, "events_workspace_order", false),
        (OWNER_CAUSAL_QUERY, "events_owner_causal", true),
    ] {
        let mut statement = connection.prepare(&format!("EXPLAIN QUERY PLAN {query}"))?;
        let clock = serde_json::to_string(&clock)?;
        let mut rows = if query == OWNER_CAUSAL_QUERY {
            statement.query(params![network, clock, event.scope_id])?
        } else if causal {
            statement.query(params![network, clock])?
        } else {
            statement.query([network])?
        };
        let mut plan = Vec::new();
        while let Some(row) = rows.next()? {
            plan.push(row.get::<_, String>(3)?);
        }
        assert!(plan.iter().any(|step| step.contains(index)), "{plan:?}");
        if causal {
            assert!(
                plan.iter()
                    .any(|step| step.contains("causal_chain=? AND seq<?")),
                "{plan:?}"
            );
        }
    }
    let mut statement = connection.prepare("SELECT hash FROM events WHERE network_id=?")?;
    for hash in statement.query_map([network], |row| row.get::<_, String>(0))? {
        let hash = hash?;
        assert_eq!(
            clock.contains(connection, network, &hash)?,
            known.contains_key(&hash),
            "{hash}"
        );
        assert_eq!(
            clock.event(connection, network, &hash)?,
            known.get(&hash).cloned()
        );
    }
    if let Some(hash) = known.keys().next() {
        assert!(!clock.contains(connection, "other-network", hash)?);
        assert!(clock.event(connection, "other-network", hash)?.is_none());
    }
    for causal_only in [false, true] {
        let mut expected = Graph::default();
        let mut statement = connection.prepare("SELECT hash,envelope,lamport,created_at_ms FROM events WHERE network_id=? AND effective=1 AND kind IN ('workspace_commanded','input_posted') ORDER BY lamport,created_at_ms,hash")?;
        let mut rows = statement.query([network])?;
        while let Some(row) = rows.next()? {
            let hash: String = row.get(0)?;
            if causal_only && !known.contains_key(&hash) {
                continue;
            }
            let envelope: SignedEvent = serde_json::from_str(&row.get::<_, String>(1)?)?;
            let event: Event = serde_json::from_str(&envelope.canonical_body)?;
            let basis = vector(&ancestors(connection, network, &event)?);
            expected.record(
                &hash,
                &event,
                &crate::store::rank(row.get(2)?, row.get(3)?, &hash),
                basis,
            )?;
        }
        assert_eq!(
            serde_json::to_value(graph_at(
                connection,
                network,
                causal_only.then_some(&clock)
            )?)?,
            serde_json::to_value(expected)?,
        );
    }
    let scopes: BTreeSet<_> = known
        .values()
        .map(|event| event.scope_id.as_str())
        .collect();
    for scope in scopes {
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
                    request_key(network, scope, &event.body, &event.author)?,
                    scheduler_node.clone(),
                ));
            }
        }
        owners.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let expected = owners
            .first()
            .map(|(epoch, _, node)| (node.clone(), *epoch));
        assert_eq!(owner_at(connection, network, &clock, scope).ok(), expected);
    }
    Ok(())
}

pub(crate) fn workspace_assignment_from(
    connection: &Connection,
    network: &str,
    assignment_id: &str,
) -> Result<Option<Assignment>> {
    let encoded: Option<String> = connection.query_row(
        "SELECT record FROM workspace_assignments_lookup WHERE network_id=?1 AND assignment_id=?2",
        params![network, assignment_id], |row| row.get(0),
    ).optional()?;
    encoded
        .map(|encoded| serde_json::from_str(&encoded).map_err(Into::into))
        .transpose()
}

/// Reconstruct disposable lookup rows during schema initialization, in its transaction.
pub(crate) fn rebuild_lookup_projection(connection: &Connection, network: &str) -> Result<()> {
    let mut graph = read_graph(connection, network)?;
    graph.replace_lookups = true;
    save_graph(&graph, connection, network)
}

pub(crate) fn workspace_task_from(
    connection: &Connection,
    network: &str,
    task_id: &str,
) -> Result<Option<WorkspaceTask>> {
    let encoded: Option<String> = connection
        .query_row(
            "SELECT record FROM workspace_tasks_lookup WHERE network_id=?1 AND task_id=?2",
            params![network, task_id],
            |row| row.get(0),
        )
        .optional()?;
    encoded
        .map(|encoded| serde_json::from_str(&encoded).map_err(Into::into))
        .transpose()
}
pub(crate) fn workspace_assignment_for_work_from(
    connection: &Connection,
    network: &str,
    work_id: &str,
) -> Result<Option<Assignment>> {
    let encoded: Option<String> = connection
        .query_row(
            "SELECT record FROM workspace_assignments_lookup WHERE network_id=?1 AND work_id=?2
            ORDER BY created_at_ms DESC,assignment_id DESC LIMIT 1",
            params![network, work_id],
            |row| row.get(0),
        )
        .optional()?;
    encoded
        .map(|encoded| serde_json::from_str(&encoded).map_err(Into::into))
        .transpose()
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
        // Deduplicate here so SQLite can use the reply index without sorting the union.
        let replies: BTreeSet<String> = connection.prepare(
            "SELECT message_id FROM messages WHERE network_id=?1 AND conversation_id=?2 AND in_reply_to=?3 AND role='assistant' UNION ALL SELECT message_id FROM messages WHERE network_id=?1 AND conversation_id=?2 AND message_id=?3 AND role='assistant' UNION ALL SELECT steered_into FROM messages WHERE network_id=?1 AND conversation_id=?2 AND message_id=?3 AND steered_into IS NOT NULL",
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

impl Store {
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
    pub fn workspace_state(
        &self,
        network: &str,
        channel_id: Option<&str>,
        task_id: Option<&str>,
        limit: usize,
    ) -> Result<WorkspaceState> {
        let graph = read_graph(&self.connection, network)?;
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
    pub(super) fn workspace_history_events(
        &self,
        network: &str,
        channel_id: Option<&str>,
        task_id: Option<&str>,
    ) -> Result<Vec<(String, WorkspaceHistoryItem)>> {
        let graph = read_graph(&self.connection, network)?;
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
        let mut statement=self.connection.prepare("SELECT hash,envelope,lamport FROM events WHERE network_id=? AND effective=1 AND kind IN ('workspace_commanded','input_posted','message_posted','work_dispatched','work_progress','work_finished') ORDER BY lamport,created_at_ms,hash")?;
        let rows = statement.query_map([network], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u64>(2)?,
            ))
        })?;
        let mut all = Vec::new();
        for row in rows {
            let (hash, envelope, lamport) = row?;
            let envelope: SignedEvent = serde_json::from_str(&envelope)?;
            let event: Event = serde_json::from_str(&envelope.canonical_body)?;
            all.push((
                crate::store::rank(lamport, event.created_at_ms, &hash),
                hash,
                event,
            ));
        }
        let input_ids: BTreeSet<_> = all
            .iter()
            .filter_map(|(_, _, event)| match &event.body {
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
        for (rank, hash, event) in all {
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
                events.push((
                    rank,
                    WorkspaceHistoryItem {
                        speaker,
                        event_hash: hash,
                        created_at_ms: event.created_at_ms,
                        scope_id: event.scope_id,
                        kind: event.body.kind().into(),
                        body: event.body,
                    },
                ));
            }
        }
        Ok(events)
    }
}

impl Store {
    pub fn workspace_active_assignments(&self, network: &str) -> Result<Vec<Assignment>> {
        Ok(read_graph(&self.connection, network)?
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
        Ok(read_graph(&self.connection, network)?
            .commands
            .contains_key(command_id))
    }
}

pub(super) fn history_speaker(
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
            let known = log::CausalClock::before(connection, network, event)?;
            if owner_at(connection, network, &known, &event.scope_id)
                .is_ok_and(|(owner, _)| owner == event.author)
            {
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
