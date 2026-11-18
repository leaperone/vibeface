//! The dependency closure needed by overview hold evaluation. This is deliberately private:
//! its graph is not a complete workspace and must never be passed to reducers or replay.
use super::{
    assignment::{
        task_execution_allowed_with_graph, workspace_assignment_for_job_with_graph,
        workspace_input_resources_blocked_with_graph,
    },
    graph::Graph,
    input::{queued_workspace_input_with_graph, resolve_input_target_with_graph},
    resources::assignment_resources_available_with_graph,
    validation::overlap,
};
use crate::{
    scheduling::{SchedulerJob, SessionScope},
    workspace::{Assignment, InputTarget, QueuedInput, ResolvedInputTarget},
};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension as _, params};

mod history;
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

#[cfg(test)]
mod tests {
    use super::super::query::save_graph;
    use super::*;
    use crate::{config::SchedulingLimits, providers::Provider, scheduling::QueueHoldReason};
    use crate::{
        crypto::LocalIdentity,
        model::{Event, EventBody, VersionVector, WIRE_VERSION},
        store::Store,
        workspace::{WORKSPACE_SCOPE, WorkspaceCommand},
    };
    use anyhow::ensure;
    use serde_json::json;

    #[test]
    fn overview_workspace_hold_matches_full_graph_for_resources_dependencies_and_authority()
    -> Result<()> {
        let identity = LocalIdentity::generate();
        let local = identity.device_id()?;
        let network = identity.fixture_network_id()?;
        let mut store = Store::open(":memory:")?;
        store.install_trust_bundle(
            &network,
            &identity.sign_network_fixture(1, vec![local.clone()])?,
        )?;
        store.connection.execute("INSERT INTO conversations(network_id,conversation_id,title,owner,owner_epoch) VALUES(?1,'c','C',?2,1)",params![network,local])?;
        store.connection.execute("INSERT INTO bots(network_id,conversation_id,bot_id,name,instructions,event_hash,rank) VALUES(?1,'c','bot','Bot','','hash','rank')",[&network])?;
        let mut graph = Graph::default();
        let mut previous = None;
        let mut sequence = 0u64;
        let mut emit = |graph: &mut Graph,
                        command_id: &str,
                        command: WorkspaceCommand|
         -> Result<String> {
            sequence += 1;
            let event = Event {
                version: WIRE_VERSION,
                network_id: network.clone(),
                author: local.clone(),
                seq: sequence,
                prev: previous.clone(),
                deps: vec![],
                scope_id: WORKSPACE_SCOPE.into(),
                created_at_ms: sequence as i64,
                body: identity.fixture_admin().attest(
                    &network,
                    WORKSPACE_SCOPE,
                    EventBody::WorkspaceCommanded {
                        command_id: command_id.into(),
                        command,
                        acting_assignment: None,
                        attestation: None,
                        cause: None,
                    },
                )?,
                blobs: vec![],
            };
            let envelope = identity.sign_event(event.clone())?;
            store.connection.execute("INSERT INTO events(network_id,author,seq,hash,scope_id,lamport,created_at_ms,seen_at_ms,envelope,main,effective,kind,causal_chain,causal_clock)
                VALUES(?1,?2,?3,?4,?6,?3,?3,?3,?5,1,1,'workspace_commanded',?2,'{}')",
                params![network,local,sequence,envelope.hash,serde_json::to_string(&envelope)?,event.scope_id])?;
            graph.record(
                &envelope.hash,
                &event,
                &format!("{sequence:08}"),
                VersionVector::new(),
            )?;
            ensure!(
                graph.commands[command_id].error.is_none(),
                "fixture command rejected: {:?}",
                graph.commands[command_id].error
            );
            previous = Some(envelope.hash.clone());
            Ok(envelope.hash)
        };
        emit(
            &mut graph,
            "channel",
            WorkspaceCommand::CreateChannel {
                channel_id: "channel".into(),
                title: "Channel".into(),
                purpose: "Purpose".into(),
                members: vec!["bot".into()],
                coordinator_bot: "bot".into(),
                coordinator_node: local.clone(),
            },
        )?;
        for id in ["wanted", "dependency"] {
            emit(
                &mut graph,
                &format!("create-{id}"),
                WorkspaceCommand::CreateTask {
                    task_id: id.into(),
                    channel_id: Some("channel".into()),
                    parent_task_id: None,
                    owner_bot: "bot".into(),
                    title: id.into(),
                    request: "request".into(),
                    expected_result: "result".into(),
                    source_events: vec![],
                    dependencies: vec![],
                    reply_only: false,
                },
            )?;
        }
        let assignment_hash = emit(
            &mut graph,
            "assign",
            WorkspaceCommand::AssignTask {
                task_id: "wanted".into(),
                expected_revision: 1,
                assignment_id: "assignment".into(),
                bot_id: "bot".into(),
                execution_node: local.clone(),
                bot_owner_epoch: 1,
                channel_owner_epoch: Some(1),
                work_id: None,
                resource_roots: vec!["/project".into()],
                automatic: false,
            },
        )?;
        save_graph(&graph, &store.connection, &network)?;
        store.connection.execute("INSERT INTO scheduler_jobs(conversation_id,scope,trigger,kind,sequence,basis,reply_to,message_id,state,tool_calls)
            VALUES('c','workspace_task:wanted','assignment:assignment','human',1,'{}','assignment','reply','queued',0)",[])?;
        let limits = SchedulingLimits {
            machine_slots: 8,
            task_limit: 4,
        };
        let compare = |expected: Option<QueueHoldReason>| -> Result<()> {
            let mut compared = 0;
            let snapshot=store.overview_snapshot(crate::store::overview::OverviewQuery {
                network:&network,scope:Some("c"),local:&local,limit:1,include_finished:false,include_sessions:false,
            },|store,job,_,_,_,facts| {
                assert_eq!(job.scope,SessionScope::WorkspaceTask {task_id:"wanted".into()});
                let full=store.scheduler_job_hold(&network,&local,job,Provider::Codex,&limits)?;
                let bounded=facts.hold(store,&network,&local,job,Provider::Codex,&limits)?;
                assert_eq!(bounded,full,"sliced versus complete graph");
                assert_eq!(bounded,expected,"literal hold precedence");
                compared+=1;
                Ok(json!({"conversation_id":job.conversation_id,"trigger":job.trigger,"hold_reason":bounded}))
            })?;
            assert_eq!(compared, 1);
            assert_eq!(snapshot.jobs.rows[0]["hold_reason"], json!(expected));
            Ok(())
        };
        compare(None)?;
        graph.tasks.get_mut("wanted").unwrap().dependencies = vec!["dependency".into()];
        save_graph(&graph, &store.connection, &network)?;
        compare(Some(QueueHoldReason::PendingCheck))?;
        graph.tasks.get_mut("wanted").unwrap().dependencies.clear();
        save_graph(&graph, &store.connection, &network)?;
        let mut dispatch = json!({"type":"work_dispatched","work_id":"foreign","trigger":"message:foreign","dispatcher":local,
            "call_ref":"foreign","kind":"task","resolved":{"provider":"codex","model":null},"cwd":"/project/subdir","title":"foreign","prompt":"private"});
        store.connection.execute("INSERT INTO works(network_id,work_id,conversation_id,target_node,dispatch,dispatch_rank) VALUES(?1,'foreign','elsewhere',?2,?3,'rank')",params![network,local,dispatch.to_string()])?;
        compare(Some(QueueHoldReason::ResourceConflict))?;
        dispatch["cwd"] = json!("/disjoint");
        store.connection.execute(
            "UPDATE works SET dispatch=? WHERE work_id='foreign'",
            [dispatch.to_string()],
        )?;
        compare(None)?;
        graph.channels.get_mut("channel").unwrap().owner_epoch = 2;
        save_graph(&graph, &store.connection, &network)?;
        compare(Some(QueueHoldReason::PendingCheck))?;
        graph.channels.get_mut("channel").unwrap().owner_epoch = 1;
        save_graph(&graph, &store.connection, &network)?;
        store.connection.execute(
            "UPDATE events SET effective=0 WHERE hash=?",
            [assignment_hash],
        )?;
        compare(Some(QueueHoldReason::PendingCheck))?;
        Ok(())
    }
}
