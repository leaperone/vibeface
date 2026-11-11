use super::*;

fn task(id: &str) -> WorkspaceTask {
    WorkspaceTask {
        task_id: id.into(),
        channel_id: None,
        root_task_id: id.into(),
        parent_task_id: None,
        owner_bot: "bot".into(),
        title: id.into(),
        request: "Synthetic task".into(),
        expected_result: "Result".into(),
        source_events: vec![],
        dependencies: vec![],
        reply_only: false,
        revision: 1,
        action: TaskAction::Continue,
        request_event: "request".into(),
        event_hash: "event".into(),
        created_at_ms: 1,
        updated_at_ms: 1,
        state: TaskState::NotStarted,
        waiting_reason: None,
        current_assignment: None,
    }
}
fn assignment(id: &str, task: &str, work: Option<&str>, at: i64) -> Assignment {
    Assignment {
        submitted_result: None,
        trigger_message_id: None,
        source_event: None,
        assignment_id: id.into(),
        task_id: task.into(),
        started_revision: 1,
        bot_id: "bot".into(),
        execution_node: "node".into(),
        bot_owner_epoch: 1,
        channel_owner_epoch: None,
        work_id: work.map(str::to_owned),
        basis: VersionVector::new(),
        resource_roots: vec![],
        automatic: false,
        accepted_revision: None,
        adopted_revision: None,
        completed_revision: None,
        state: AssignmentState::Proposed,
        result: None,
        event_hash: "event".into(),
        created_at_ms: at,
    }
}
fn oracle(store: &Store, network: &str) -> Result<()> {
    let graph = Graph::read(&store.connection, network)?;
    for (id, task) in &graph.tasks {
        assert_eq!(
            workspace_task_from(&store.connection, network, id)?,
            Some(task.clone())
        );
    }
    for (id, assignment) in &graph.assignments {
        assert_eq!(
            workspace_assignment_from(&store.connection, network, id)?,
            Some(assignment.clone())
        );
        if let Some(work) = &assignment.work_id {
            let expected = graph
                .assignments
                .values()
                .filter(|item| item.work_id.as_ref() == Some(work))
                .max_by(|left, right| {
                    left.created_at_ms
                        .cmp(&right.created_at_ms)
                        .then(left.assignment_id.cmp(&right.assignment_id))
                })
                .cloned();
            assert_eq!(
                workspace_assignment_for_work_from(&store.connection, network, work)?,
                expected
            );
        }
    }
    let counts: (usize,usize) = store.connection.query_row(
        "SELECT (SELECT count(*) FROM workspace_tasks_lookup WHERE network_id=?1),(SELECT count(*) FROM workspace_assignments_lookup WHERE network_id=?1)",
        [network], |row| Ok((row.get(0)?,row.get(1)?)))?;
    assert_eq!(counts, (graph.tasks.len(), graph.assignments.len()));
    Ok(())
}
fn changes(connection: &Connection) -> Result<u64> {
    Ok(connection.query_row("SELECT total_changes()", [], |row| row.get(0))?)
}

#[test]
fn lookup_winners_and_isolation_match_the_complete_graph() -> Result<()> {
    let store = Store::open(":memory:")?;
    let mut graph = Graph {
        replace_lookups: true,
        ..Default::default()
    };
    graph.tasks.insert("first".into(), task("first"));
    graph.tasks.insert("second".into(), task("second"));
    for (id, task, work, at) in [
        ("old", "first", Some("work"), 2),
        ("a", "first", Some("work"), 3),
        ("z", "second", Some("work"), 3),
        ("work", "first", Some("different-work"), 1),
        ("unbound", "first", None, 9),
    ] {
        graph
            .assignments
            .insert(id.into(), assignment(id, task, work, at));
    }
    graph.save(&store.connection, "n")?;
    oracle(&store, "n")?;
    assert_eq!(
        workspace_assignment_for_work_from(&store.connection, "n", "work")?
            .context("winner")?
            .assignment_id,
        "z"
    );
    assert_eq!(
        workspace_assignment_from(&store.connection, "n", "work")?
            .context("exact ID")?
            .task_id,
        "first"
    );
    assert!(workspace_task_from(&store.connection, "other", "first")?.is_none());
    assert!(workspace_assignment_from(&store.connection, "other", "z")?.is_none());
    assert!(workspace_assignment_for_work_from(&store.connection, "other", "work")?.is_none());
    assert!(workspace_assignment_for_work_from(&store.connection, "n", "missing")?.is_none());
    // Initialization reconstructs from the canonical graph; stale derived records disappear.
    store.connection.execute(
        "DELETE FROM workspace_tasks_lookup WHERE network_id='n'",
        [],
    )?;
    store.connection.execute(
        "INSERT INTO workspace_assignments_lookup VALUES('n','stale','work',999,'{}')",
        [],
    )?;
    rebuild_lookup_projection(&store.connection, "n")?;
    oracle(&store, "n")?;
    assert!(workspace_assignment_from(&store.connection, "n", "stale")?.is_none());
    Ok(())
}

#[test]
fn ordinary_graph_saves_write_only_changed_lookup_rows_and_rollback_together() -> Result<()> {
    let store = Store::open(":memory:")?;
    let identity = LocalIdentity::generate();
    let network = identity.fixture_network_id()?;
    let mut graph = Graph {
        replace_lookups: true,
        ..Default::default()
    };
    for index in 0..1000 {
        let id = format!("task-{index}");
        graph.tasks.insert(id.clone(), task(&id));
    }
    graph.save(&store.connection, &network)?;
    let mut graph = Graph::read(&store.connection, &network)?;
    let mut event = Event {
        version: WIRE_VERSION,
        network_id: network.clone(),
        author: identity.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![],
        scope_id: "c".into(),
        created_at_ms: 2,
        body: EventBody::InputPosted {
            message_id: "input".into(),
            target: InputTarget::BotMain {
                bot_id: "bot".into(),
            },
            intent: InputIntent::Continue,
            sequence: 1,
            expected_revision: None,
            text: "Input".into(),
            attachments: vec![],
            report_results: Some(true),
            attestation: None,
        },
        blobs: vec![],
    };
    graph.record("input-event", &event, "rank-1", VersionVector::new())?;
    let before = changes(&store.connection)?;
    graph.save(&store.connection, &network)?;
    assert_eq!(
        changes(&store.connection)? - before,
        1,
        "an input writes only the canonical graph"
    );
    let mut graph = Graph::read(&store.connection, &network)?;
    event.scope_id = WORKSPACE_SCOPE.into();
    event.body = EventBody::WorkspaceCommanded {
        command_id: "defer".into(),
        command: WorkspaceCommand::ControlTask {
            task_id: "task-0".into(),
            expected_revision: 1,
            action: TaskAction::Defer,
        },
        attestation: None,
        cause: None,
        acting_assignment: None,
    };
    graph.record("defer-event", &event, "rank-2", VersionVector::new())?;
    assert_eq!(
        graph
            .dirty_tasks
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["task-0"]
    );
    let before = changes(&store.connection)?;
    graph.save(&store.connection, &network)?;
    assert_eq!(
        changes(&store.connection)? - before,
        2,
        "one task change writes one canonical row and one lookup row"
    );
    oracle(&store, &network)?;
    let previous: String = store.connection.query_row(
        "SELECT state FROM workspace_projection WHERE network_id=?",
        [&network],
        |row| row.get(0),
    )?;
    store.connection.execute_batch("CREATE TEMP TRIGGER reject_task_lookup BEFORE UPDATE ON workspace_tasks_lookup BEGIN SELECT RAISE(FAIL,'lookup fixture failure'); END;")?;
    let mut graph = Graph::read(&store.connection, &network)?;
    event.body = EventBody::WorkspaceCommanded {
        command_id: "resume".into(),
        command: WorkspaceCommand::ControlTask {
            task_id: "task-0".into(),
            expected_revision: 1,
            action: TaskAction::Continue,
        },
        attestation: None,
        cause: None,
        acting_assignment: None,
    };
    graph.record("resume-event", &event, "rank-3", VersionVector::new())?;
    assert!(graph.save(&store.connection, &network).is_err());
    assert_eq!(
        store.connection.query_row(
            "SELECT state FROM workspace_projection WHERE network_id=?",
            [&network],
            |row| row.get::<_, String>(0)
        )?,
        previous
    );
    oracle(&store, &network)?;
    Ok(())
}

fn fixture() -> Result<(Store, LocalIdentity, String, Vec<SignedEvent>)> {
    let identity = LocalIdentity::generate();
    let network = identity.fixture_network_id()?;
    let mut store = Store::open(":memory:")?;
    store.install_trust_bundle(
        &network,
        &identity.sign_network_fixture(1, vec![identity.device_id()?])?,
    )?;
    store.create_conversation(
        &identity,
        &network,
        "c",
        "Lookup fixture",
        identity.owner_fixture(&network, "c", &identity.device_id()?)?,
        1,
    )?;
    let bot = identity.fixture_admin().attest(
        &network,
        "c",
        EventBody::BotCreated {
            bot_id: "bot".into(),
            name: "Lookup fixture".into(),
            instructions: "Synthetic".into(),
            project: None,
            attestation: None,
            cause: None,
        },
    )?;
    store.append_event(&identity, &network, "c", bot, vec![], 2)?;
    let events = store.events_from(&network, &identity.device_id()?, 1, 100)?;
    Ok((store, identity, network, events))
}
fn create_task(id: &str) -> WorkspaceCommand {
    WorkspaceCommand::CreateTask {
        task_id: id.into(),
        channel_id: None,
        parent_task_id: None,
        owner_bot: "bot".into(),
        title: id.into(),
        request: "Synthetic".into(),
        expected_result: "Result".into(),
        source_events: vec![],
        dependencies: vec![],
        reply_only: false,
    }
}
fn command(
    store: &mut Store,
    identity: &LocalIdentity,
    network: &str,
    id: &str,
    command: WorkspaceCommand,
    at: i64,
) -> Result<WorkspaceCommandReceipt> {
    let signed = identity.fixture_admin().attest(
        network,
        WORKSPACE_SCOPE,
        EventBody::WorkspaceCommanded {
            command_id: id.into(),
            command: command.clone(),
            attestation: None,
            cause: None,
            acting_assignment: None,
        },
    )?;
    store.apply_workspace_command(
        identity,
        network,
        id,
        command,
        signed.attestation().cloned(),
        None,
        None,
        at,
    )
}

#[test]
fn signed_late_fork_and_full_projection_rebuild_keep_lookup_truth() -> Result<()> {
    let (mut origin, identity, network, base) = fixture()?;
    let created = command(
        &mut origin,
        &identity,
        &network,
        "create",
        create_task("task"),
        10,
    )?;
    oracle(&origin, &network)?;
    command(
        &mut origin,
        &identity,
        &network,
        "assign",
        WorkspaceCommand::AssignTask {
            task_id: "task".into(),
            expected_revision: 1,
            assignment_id: "assignment".into(),
            bot_id: "bot".into(),
            execution_node: identity.device_id()?,
            bot_owner_epoch: 1,
            channel_owner_epoch: None,
            work_id: None,
            resource_roots: vec![],
            automatic: false,
        },
        11,
    )?;
    oracle(&origin, &network)?;
    let events = origin.events_from(&network, &identity.device_id()?, 1, 100)?;
    let state = origin.trust_bundle(&network)?.context("membership")?;
    let mut replica = Store::open(":memory:")?;
    replica.install_trust_bundle(&network, &state)?;
    replica.ingest_events_at(&network, &base, 50)?;
    let tail = &events[base.len()..];
    for event in tail.iter().rev() {
        replica.ingest_events_at(&network, std::slice::from_ref(event), 50)?;
        oracle(&replica, &network)?;
    }
    replica.ingest_events_at(&network, tail, 50)?;
    oracle(&replica, &network)?;
    assert!(workspace_assignment_from(&replica.connection, &network, "assignment")?.is_some());
    replica.connection.execute(
        "UPDATE events SET effective=0 WHERE network_id=?",
        [&network],
    )?;
    super::super::reduce::clear(&replica.connection, &network)?;
    super::super::log::recompute(
        &replica.connection,
        &membership_from(&replica.connection, &network)?,
    )?;
    oracle(&replica, &network)?;
    let original = origin
        .event(&network, &created.event_hash)?
        .context("created event")?;
    let mut branch: Event = serde_json::from_str(&original.canonical_body)?;
    branch.body = identity.fixture_admin().attest(
        &network,
        WORKSPACE_SCOPE,
        EventBody::WorkspaceCommanded {
            command_id: "fork".into(),
            command: create_task("forked"),
            attestation: None,
            cause: None,
            acting_assignment: None,
        },
    )?;
    let branch = identity.sign_event(branch)?;
    replica.ingest_events_at(&network, &[branch], 50)?;
    oracle(&replica, &network)?;
    assert!(workspace_task_from(&replica.connection, &network, "task")?.is_none());
    assert!(workspace_task_from(&replica.connection, &network, "forked")?.is_none());
    assert!(workspace_assignment_from(&replica.connection, &network, "assignment")?.is_none());
    Ok(())
}

#[test]
fn lookup_read_plans_are_indexed_and_missing_tables_fail_closed() -> Result<()> {
    let store = Store::open(":memory:")?;
    let details=store.connection.prepare("EXPLAIN QUERY PLAN SELECT record FROM workspace_assignments_lookup WHERE network_id='n' AND work_id='w' ORDER BY created_at_ms DESC,assignment_id DESC LIMIT 1")?
        .query_map([],|row|row.get::<_,String>(3))?.collect::<rusqlite::Result<Vec<_>>>()?.join("\n");
    assert!(details.contains("workspace_assignments_work"), "{details}");
    assert!(!details.contains("TEMP B-TREE"), "{details}");
    store.connection.execute_batch(
        "DROP TABLE workspace_tasks_lookup; DROP TABLE workspace_assignments_lookup;",
    )?;
    assert!(workspace_task_from(&store.connection, "n", "task").is_err());
    assert!(workspace_assignment_from(&store.connection, "n", "assignment").is_err());
    assert!(workspace_assignment_for_work_from(&store.connection, "n", "work").is_err());
    Ok(())
}

#[test]
fn indexed_scope_resolution_preserves_assignment_id_before_work_and_never_invents_main()
-> Result<()> {
    let (store, _, network, _) = fixture()?;
    let mut graph = Graph::read(&store.connection, &network)?;
    graph.replace_lookups = true;
    graph.tasks.insert("by-id".into(), task("by-id"));
    graph.tasks.insert("by-work".into(), task("by-work"));
    graph
        .assignments
        .insert("work".into(), assignment("work", "by-id", None, 1));
    graph.assignments.insert(
        "latest".into(),
        assignment("latest", "by-work", Some("work"), 2),
    );
    graph.save(&store.connection, &network)?;
    assert_eq!(
        crate::store::scope_projection::session_target_from(
            &store.connection,
            &network,
            "c",
            &SessionScope::Task {
                work_id: "work".into()
            }
        )?,
        Some(InputTarget::Task {
            task_id: "by-id".into()
        })
    );
    assert_eq!(
        store.work_input_target(&network, "c", "work")?,
        Some(InputTarget::Task {
            task_id: "by-work".into()
        })
    );
    assert!(!crate::store::scope_projection::work_is_main_from(
        &store.connection,
        &network,
        "c",
        "work"
    )?);
    assert!(store.work_input_target(&network, "c", "unknown")?.is_none());
    assert!(
        store
            .work_input_target("wrong-network", "c", "work")?
            .is_none()
    );
    oracle(&store, &network)
}

#[test]
#[ignore = "controlled derived Graph data at 1k/10k/100k; separate from signed ingest"]
fn lookup_read_scale_preserves_graph_oracle_without_full_graph_reads() -> Result<()> {
    use std::time::Instant;
    for size in [1000, 10000, 100000] {
        let store = Store::open(":memory:")?;
        let mut graph = Graph {
            replace_lookups: true,
            ..Default::default()
        };
        for index in 0..size {
            let task_id = format!("task-{index}");
            let id = format!("assignment-{index}");
            let work = format!("work-{index}");
            graph.tasks.insert(task_id.clone(), task(&task_id));
            graph.assignments.insert(
                id.clone(),
                assignment(&id, &task_id, Some(&work), index as i64),
            );
        }
        graph.save(&store.connection, "n")?;
        for baseline in [true, false] {
            for repeat in 0..3 {
                let start = Instant::now();
                let mut results = Vec::new();
                for index in size - 100..size {
                    let work = format!("work-{index}");
                    let bound = if baseline {
                        Graph::read(&store.connection, "n")?
                            .assignments
                            .into_values()
                            .filter(|item| item.work_id.as_ref() == Some(&work))
                            .max_by(|left, right| {
                                left.created_at_ms
                                    .cmp(&right.created_at_ms)
                                    .then(left.assignment_id.cmp(&right.assignment_id))
                            })
                    } else {
                        workspace_assignment_for_work_from(&store.connection, "n", &work)?
                    };
                    let bound = bound.context("bound work")?;
                    let owner = if baseline {
                        Graph::read(&store.connection, "n")?
                            .tasks
                            .remove(&bound.task_id)
                    } else {
                        workspace_task_from(&store.connection, "n", &bound.task_id)?
                    };
                    results.push(owner.context("task")?.task_id);
                }
                let elapsed = start.elapsed();
                assert_eq!(
                    results,
                    (size - 100..size)
                        .map(|index| format!("task-{index}"))
                        .collect::<Vec<_>>()
                );
                eprintln!(
                    "controlled_graph_records={size} baseline={baseline} repeat={repeat} page=100 elapsed_ns={}",
                    elapsed.as_nanos()
                );
            }
        }
    }
    Ok(())
}
