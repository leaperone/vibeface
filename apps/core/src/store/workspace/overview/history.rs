//! Historical task reads retain the complete influence component, then use Graph::record.
//! The prepass reads identities and resource roots only, never event text/prompts/results.
use super::super::*;

#[derive(Default)]
struct Influence {
    hash: String,
    writes: BTreeSet<String>,
    reads: BTreeSet<String>,
    assignment: Option<String>,
    resources: Option<(String, Vec<String>)>,
}
fn identity(kind: &str, id: &str) -> String {
    format!("{kind}:{id}")
}
fn read(item: &mut Influence, kind: &str, id: Option<&str>) {
    if let Some(id) = id {
        item.reads.insert(identity(kind, id));
    }
}
fn write(item: &mut Influence, kind: &str, id: Option<&str>) {
    if let Some(id) = id {
        let key = identity(kind, id);
        item.reads.insert(key.clone());
        item.writes.insert(key);
    }
}

pub(in crate::store::workspace) fn graph_at_task(
    connection: &Connection,
    network: &str,
    known: &log::CausalClock,
    task: &str,
) -> Result<Graph> {
    let mut statement=connection.prepare(
        "WITH source AS (SELECT e.hash,json_extract(json_extract(e.envelope,'$.canonical_body'),'$.body') AS body
            FROM json_each(?2) known CROSS JOIN events e WHERE e.network_id=?1 AND e.causal_chain=known.key
                AND e.seq<=json_extract(known.value,'$.seq') AND e.effective=1 AND e.kind IN ('workspace_commanded','input_posted'))
         SELECT hash,json_extract(body,'$.type'),json_extract(body,'$.command_id'),json_extract(body,'$.command.kind'),
            json_extract(body,'$.command.task_id'),json_extract(body,'$.command.assignment_id'),
            json_extract(body,'$.command.channel_id'),json_extract(body,'$.command.parent_task_id'),
            json_extract(body,'$.command.dependencies'),json_extract(body,'$.command.execution_node'),
            json_extract(body,'$.command.resource_roots'),json_extract(body,'$.command.work_id'),
            json_extract(body,'$.cause.message_id'),json_extract(body,'$.message_id'),json_extract(body,'$.command.message_id'),
            json_extract(body,'$.command.action.before_message_id') FROM source")?;
    let mut rows = statement.query(params![network, serde_json::to_string(known)?])?;
    let mut items = Vec::new();
    let mut assignment_tasks: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let mut item = Influence {
            hash: row.get(0)?,
            ..Default::default()
        };
        let kind: String = row.get(1)?;
        if kind == "input_posted" {
            let input: String = row.get(13)?;
            write(&mut item, "input", Some(&input));
        } else {
            ensure!(
                kind == "workspace_commanded",
                "unknown historical workspace influence"
            );
            let command: String = row.get(2)?;
            let action: String = row.get(3)?;
            let task: Option<String> = row.get(4)?;
            let assignment: Option<String> = row.get(5)?;
            let channel: Option<String> = row.get(6)?;
            let parent: Option<String> = row.get(7)?;
            let dependencies: Option<String> = row.get(8)?;
            let node: Option<String> = row.get(9)?;
            let roots: Option<String> = row.get(10)?;
            let work: Option<String> = row.get(11)?;
            let cause: Option<String> = row.get(12)?;
            let input: Option<String> = row.get(14)?;
            let before: Option<String> = row.get(15)?;
            write(&mut item, "command", Some(&command));
            read(&mut item, "input", cause.as_deref());
            match action.as_str() {
                "create_channel" | "configure_channel" => write(
                    &mut item,
                    "channel",
                    Some(channel.as_deref().context("historical channel missing")?),
                ),
                "post_channel_message" => {
                    read(&mut item, "channel", channel.as_deref());
                    read(&mut item, "task", task.as_deref());
                }
                "control_input" => {
                    let input = input
                        .as_deref()
                        .context("historical input control missing")?;
                    write(&mut item, "input", Some(input));
                    read(&mut item, "input_assignment", Some(input));
                    read(&mut item, "input", before.as_deref());
                }
                "create_task" | "revise_task" | "control_task" | "assign_task" => {
                    let task = task.as_deref().context("historical task missing")?;
                    write(&mut item, "task", Some(task));
                    // A root's descendants share reply/automatic-dispatch budgets. Both sides
                    // of parent creation participate, including a rejected child creation.
                    read(&mut item, "family", Some(task));
                    if action == "create_task" {
                        write(&mut item, "family", Some(task));
                        write(&mut item, "family", parent.as_deref());
                        read(&mut item, "task", parent.as_deref());
                        read(&mut item, "family", parent.as_deref());
                    }
                    read(&mut item, "channel", channel.as_deref());
                    if let Some(dependencies) = dependencies {
                        for dependency in serde_json::from_str::<Vec<String>>(&dependencies)? {
                            read(&mut item, "task", Some(&dependency));
                        }
                    }
                    if action == "assign_task" {
                        let assignment = assignment
                            .as_deref()
                            .context("historical assignment missing")?;
                        write(&mut item, "assignment", Some(assignment));
                        write(&mut item, "work_reservation", work.as_deref());
                        write(&mut item, "input_assignment", cause.as_deref());
                        assignment_tasks
                            .entry(assignment.into())
                            .or_default()
                            .insert(task.into());
                        item.resources = Some((
                            node.context("historical assignment node missing")?,
                            serde_json::from_str(
                                &roots.context("historical assignment roots missing")?,
                            )?,
                        ));
                    }
                }
                "assignment_progress" => write(
                    &mut item,
                    "assignment",
                    Some(
                        assignment
                            .as_deref()
                            .context("historical assignment progress missing")?,
                    ),
                ),
                _ => bail!("unclassified historical workspace command {action}"),
            }
            item.assignment = assignment;
        }
        items.push(item);
    }
    // Progress changes task state; duplicate assignment IDs can name different task attempts.
    for item in &mut items {
        if let Some(assignment) = &item.assignment {
            for task in assignment_tasks.get(assignment).into_iter().flatten() {
                let key = identity("task", task);
                item.writes.insert(key.clone());
                item.reads.insert(key);
            }
        }
    }
    // Resolve resource influence lazily from the selected component. Unrelated assignment
    // histories require one metadata scan, never an all-pairs resource comparison.
    let mut by_executor: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, item) in items.iter().enumerate() {
        if let Some((node, _)) = &item.resources {
            by_executor.entry(node).or_default().push(index);
        }
    }
    let mut writers: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, item) in items.iter().enumerate() {
        for key in &item.writes {
            writers.entry(key.clone()).or_default().push(index);
        }
    }
    let mut pending = vec![identity("task", task)];
    let mut seen = BTreeSet::new();
    let mut selected = BTreeSet::new();
    while let Some(key) = pending.pop() {
        if !seen.insert(key.clone()) {
            continue;
        }
        for &index in writers.get(&key).into_iter().flatten() {
            if selected.insert(index) {
                pending.extend(items[index].reads.iter().cloned());
                if let Some((node, roots)) = &items[index].resources {
                    for &other in by_executor.get(node.as_str()).into_iter().flatten() {
                        let (_, other_roots) =
                            items[other].resources.as_ref().expect("indexed resource");
                        if roots
                            .iter()
                            .any(|root| other_roots.iter().any(|other| overlap(root, other)))
                        {
                            pending.push(identity(
                                "assignment",
                                items[other]
                                    .assignment
                                    .as_deref()
                                    .context("resource assignment missing")?,
                            ));
                        }
                    }
                }
            }
        }
    }
    let hashes = selected
        .into_iter()
        .map(|index| items[index].hash.clone())
        .collect::<Vec<_>>();
    let mut graph = Graph {
        replace_lookups: true,
        ..Default::default()
    };
    let mut statement=connection.prepare(
        "SELECT e.hash,e.envelope,e.lamport,e.created_at_ms,e.causal_clock FROM json_each(?2) ids
         JOIN events e ON e.network_id=?1 AND e.hash=ids.value ORDER BY e.lamport,e.created_at_ms,e.hash")?;
    let mut rows = statement.query(params![network, serde_json::to_string(&hashes)?])?;
    while let Some(row) = rows.next()? {
        let hash: String = row.get(0)?;
        #[cfg(test)]
        DECODED_EVENTS.with(|count| count.set(count.get() + 1));
        let envelope: SignedEvent = serde_json::from_str(&row.get::<_, String>(1)?)?;
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        let basis = projection_basis(connection, network, &event, || {
            let clock: log::CausalClock = serde_json::from_str(&row.get::<_, String>(4)?)?;
            Ok(clock.version_vector())
        })?;
        graph.record(
            &hash,
            &event,
            &super::super::super::rank(row.get(2)?, row.get(3)?, &hash),
            basis,
        )?;
    }
    Ok(graph)
}

#[cfg(test)]
thread_local! { static DECODED_EVENTS: std::cell::Cell<usize> = const {std::cell::Cell::new(0)}; }

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct History {
        db: Connection,
        identity: LocalIdentity,
        network: String,
        author: String,
        sequence: u64,
        previous: Option<String>,
    }
    impl History {
        fn new() -> Result<Self> {
            let db = Connection::open_in_memory()?;
            db.execute_batch(super::super::super::super::INITIAL_SCHEMA)?;
            let identity = LocalIdentity::generate();
            let author = identity.device_id()?;
            let network = identity.fixture_network_id()?;
            db.execute(
                "INSERT INTO networks(network_id,bundle) VALUES(?,'{}')",
                [&network],
            )?;
            Ok(Self {
                db,
                identity,
                network,
                author,
                sequence: 0,
                previous: None,
            })
        }
        fn clock(&self) -> Result<log::CausalClock> {
            Ok(serde_json::from_value(
                json!({(self.author.clone()):{"author":self.author,"seq":self.sequence}}),
            )?)
        }
        fn append(&mut self, body: EventBody) -> Result<String> {
            self.append_dependent(body, vec![])
        }
        fn append_dependent(&mut self, body: EventBody, deps: Vec<String>) -> Result<String> {
            let clock = serde_json::to_string(&self.clock()?)?;
            self.sequence += 1;
            let scope = if matches!(&body, EventBody::WorkspaceCommanded { .. }) {
                WORKSPACE_SCOPE
            } else {
                "scope"
            };
            let body = match &body {
                EventBody::WorkspaceCommanded {
                    command: WorkspaceCommand::AssignmentProgress { .. },
                    ..
                } => body,
                EventBody::WorkspaceCommanded { cause: None, .. }
                | EventBody::InputPosted { .. } => {
                    self.identity
                        .fixture_admin()
                        .attest(&self.network, scope, body)?
                }
                _ => body,
            };
            let event = Event {
                version: WIRE_VERSION,
                network_id: self.network.clone(),
                author: self.author.clone(),
                seq: self.sequence,
                prev: self.previous.clone(),
                deps: deps
                    .into_iter()
                    .filter(|hash| Some(hash) != self.previous.as_ref())
                    .collect(),
                scope_id: scope.into(),
                created_at_ms: self.sequence as i64,
                body,
                blobs: vec![],
            };
            let envelope = self.identity.sign_event(event.clone())?;
            self.db.execute("INSERT INTO events(network_id,author,seq,hash,scope_id,lamport,created_at_ms,seen_at_ms,envelope,main,effective,kind,causal_chain,causal_clock)
                VALUES(?1,?2,?3,?4,?8,?3,?3,?3,?5,1,1,?6,?2,?7)",
                params![self.network,self.author,self.sequence,envelope.hash,serde_json::to_string(&envelope)?,
                    match &event.body {EventBody::InputPosted {..}=>"input_posted",EventBody::MessagePosted {..}=>"message_posted",_=>"workspace_commanded"},clock,event.scope_id])?;
            self.previous = Some(envelope.hash.clone());
            Ok(envelope.hash)
        }
        fn command(&mut self, id: &str, command: WorkspaceCommand) -> Result<String> {
            self.append(EventBody::WorkspaceCommanded {
                command_id: id.into(),
                command,
                acting_assignment: None,
                attestation: None,
                cause: None,
            })
        }
        fn create(
            &mut self,
            id: &str,
            parent: Option<&str>,
            dependencies: Vec<String>,
        ) -> Result<String> {
            self.command(
                &format!("create-{id}"),
                WorkspaceCommand::CreateTask {
                    task_id: id.into(),
                    channel_id: None,
                    parent_task_id: parent.map(str::to_owned),
                    owner_bot: "bot".into(),
                    title: id.into(),
                    request: "request".into(),
                    expected_result: "result".into(),
                    source_events: vec![],
                    dependencies,
                    reply_only: false,
                },
            )
        }
        fn assign(&mut self, id: &str, task: &str, root: &str) -> Result<String> {
            self.command(
                &format!("assign-{id}"),
                WorkspaceCommand::AssignTask {
                    task_id: task.into(),
                    expected_revision: 1,
                    assignment_id: id.into(),
                    bot_id: "bot".into(),
                    execution_node: self.author.clone(),
                    bot_owner_epoch: 1,
                    channel_owner_epoch: None,
                    work_id: None,
                    resource_roots: vec![root.into()],
                    automatic: true,
                },
            )
        }
        fn compare(&self, task: &str) -> Result<usize> {
            let full = graph_at(&self.db, &self.network, Some(&self.clock()?))?;
            DECODED_EVENTS.with(|count| count.set(0));
            let bounded = graph_at_task(&self.db, &self.network, &self.clock()?, task)?;
            assert_eq!(
                serde_json::to_value(full.tasks.get(task))?,
                serde_json::to_value(bounded.tasks.get(task))?
            );
            if let Some(task) = full.tasks.get(task) {
                if let Some(id) = &task.current_assignment {
                    assert_eq!(
                        serde_json::to_value(bounded.assignments.get(id))?,
                        serde_json::to_value(full.assignments.get(id))?,
                        "current assignment must be present"
                    );
                }
                if let Some(id) = &task.channel_id {
                    assert_eq!(
                        serde_json::to_value(bounded.channels.get(id))?,
                        serde_json::to_value(full.channels.get(id))?,
                        "current channel must be present"
                    );
                }
                for id in &task.dependencies {
                    assert_eq!(
                        serde_json::to_value(bounded.tasks.get(id))?,
                        serde_json::to_value(full.tasks.get(id))?,
                        "dependency must be present"
                    );
                }
            }
            for (id, actual) in &bounded.inputs {
                assert_eq!(
                    serde_json::to_value(actual)?,
                    serde_json::to_value(full.inputs.get(id).context("extra input")?)?
                );
            }
            assert_eq!(
                bounded.queue_order,
                full.queue_order
                    .iter()
                    .filter(|id| bounded.inputs.contains_key(*id))
                    .cloned()
                    .collect::<Vec<_>>()
            );
            for (id, actual) in &bounded.assignments {
                assert_eq!(
                    serde_json::to_value(actual)?,
                    serde_json::to_value(full.assignments.get(id).context("extra assignment")?)?
                );
            }
            for (id, actual) in &bounded.commands {
                assert_eq!(
                    serde_json::to_value(actual)?,
                    serde_json::to_value(full.commands.get(id).context("extra command")?)?
                );
            }
            Ok(DECODED_EVENTS.with(std::cell::Cell::get))
        }
    }

    #[test]
    fn historical_closure_keeps_conflicts_rejections_dedup_dependencies_and_controls() -> Result<()>
    {
        let mut history = History::new()?;
        history.create("other", None, vec![])?;
        history.assign("occupied", "other", "/shared")?;
        history.create("wanted", None, vec!["other".into()])?;
        history.assign("refused", "wanted", "/shared/subdir")?;
        history.create("child", Some("wanted"), vec![])?;
        history.command(
            "same-command",
            WorkspaceCommand::ControlTask {
                task_id: "other".into(),
                expected_revision: 1,
                action: TaskAction::Defer,
            },
        )?;
        history.command(
            "same-command",
            WorkspaceCommand::ControlTask {
                task_id: "wanted".into(),
                expected_revision: 1,
                action: TaskAction::Cancel,
            },
        )?;
        let input = history.append(EventBody::InputPosted {
            message_id: "input".into(),
            target: InputTarget::Task {
                task_id: "wanted".into(),
            },
            intent: InputIntent::Defer,
            sequence: 1,
            expected_revision: Some(1),
            text: "input".into(),
            attachments: vec![],
            report_results: Some(true),
            attestation: None,
        })?;
        history.command(
            "edit-input",
            WorkspaceCommand::ControlInput {
                message_id: "input".into(),
                expected_event: input,
                action: InputControl::Edit {
                    text: "edited".into(),
                    attachments: vec![],
                },
            },
        )?;
        history.command(
            "edit-input",
            WorkspaceCommand::ControlTask {
                task_id: "wanted".into(),
                expected_revision: 1,
                action: TaskAction::Cancel,
            },
        )?;
        let full = graph_at(&history.db, &history.network, Some(&history.clock()?))?;
        assert!(
            full.commands["assign-refused"]
                .error
                .as_ref()
                .is_some_and(|error| error.contains("occupied"))
        );
        assert_eq!(full.tasks["wanted"].action, TaskAction::Continue);
        let before = history.compare("wanted")?;
        for index in 0..200 {
            let task = format!("unrelated-{index}");
            history.create(&task, None, vec![])?;
            history.assign(
                &format!("isolated-{index}"),
                &task,
                &format!("/isolated/{index}"),
            )?;
        }
        assert_eq!(
            history.compare("wanted")?,
            before,
            "disjoint historical events must not be decoded as domain payloads"
        );
        Ok(())
    }

    #[test]
    fn historical_closure_preserves_family_automatic_and_member_reply_budgets() -> Result<()> {
        let mut history = History::new()?;
        history.create("root", None, vec![])?;
        for index in 0..MAX_AUTOMATIC_ASSIGNMENTS {
            let id = format!("child-{index}");
            history.create(&id, Some("root"), vec![])?;
            history.assign(
                &format!("execution-{index}"),
                &id,
                &format!("/root/{index}"),
            )?;
        }
        history.create("wanted", Some("root"), vec![])?;
        history.assign("over-budget", "wanted", "/wanted")?;
        let full = graph_at(&history.db, &history.network, Some(&history.clock()?))?;
        assert!(
            full.commands["assign-over-budget"]
                .error
                .as_ref()
                .is_some_and(|error| error.contains("automatic dispatch budget"))
        );
        assert_eq!(
            full.tasks["wanted"].waiting_reason.as_deref(),
            Some("dispatch_budget")
        );
        history.compare("wanted")?;
        for index in 0..=MAX_MEMBER_REPLIES {
            let id = format!("reply-{index}");
            history.command(
                &format!("create-{id}"),
                WorkspaceCommand::CreateTask {
                    task_id: id.clone(),
                    channel_id: None,
                    parent_task_id: Some("root".into()),
                    owner_bot: "bot".into(),
                    title: id,
                    request: "reply request".into(),
                    expected_result: "reply result".into(),
                    source_events: vec![],
                    dependencies: vec![],
                    reply_only: true,
                },
            )?;
        }
        let full = graph_at(&history.db, &history.network, Some(&history.clock()?))?;
        assert_eq!(full.tasks["root"].dependencies, vec!["reply-0", "reply-1"]);
        assert!(
            full.commands["create-reply-2"]
                .error
                .as_ref()
                .is_some_and(|error| error.contains("member reply budget"))
        );
        history.compare("root")?;
        Ok(())
    }

    #[test]
    fn historical_closure_preserves_stopped_release_move_and_channel_epochs() -> Result<()> {
        use crate::scheduling::TurnExecution;
        use vibeface_bridge::failure::FailureKind;
        let mut history = History::new()?;
        history.command(
            "channel",
            WorkspaceCommand::CreateChannel {
                channel_id: "channel".into(),
                title: "Channel".into(),
                purpose: "Purpose".into(),
                members: vec!["bot".into()],
                coordinator_bot: "bot".into(),
                coordinator_node: history.author.clone(),
            },
        )?;
        history.command(
            "wanted",
            WorkspaceCommand::CreateTask {
                task_id: "wanted".into(),
                channel_id: Some("channel".into()),
                parent_task_id: None,
                owner_bot: "bot".into(),
                title: "Wanted".into(),
                request: "request".into(),
                expected_result: "result".into(),
                source_events: vec![],
                dependencies: vec![],
                reply_only: false,
            },
        )?;
        let mut input_hash = String::new();
        for id in ["before", "input"] {
            let hash = history.append(EventBody::InputPosted {
                message_id: id.into(),
                target: InputTarget::Task {
                    task_id: "wanted".into(),
                },
                intent: InputIntent::Defer,
                sequence: 1,
                expected_revision: Some(1),
                text: id.into(),
                attachments: vec![],
                report_results: Some(true),
                attestation: None,
            })?;
            if id == "input" {
                input_hash = hash;
            }
        }
        history.append(EventBody::WorkspaceCommanded {
            command_id: "assign".into(),
            command: WorkspaceCommand::AssignTask {
                task_id: "wanted".into(),
                expected_revision: 1,
                assignment_id: "assignment".into(),
                bot_id: "bot".into(),
                execution_node: history.author.clone(),
                bot_owner_epoch: 1,
                channel_owner_epoch: Some(1),
                work_id: None,
                resource_roots: vec!["/project".into()],
                automatic: false,
            },
            acting_assignment: None,
            attestation: None,
            cause: Some(Cause {
                conversation_id: "scope".into(),
                message_id: "input".into(),
            }),
        })?;
        let evidence = history.append(EventBody::MessagePosted {
            message_id: "failed-reply".into(),
            role: Role::Assistant,
            text: "not started".into(),
            attachments: vec![],
            report_results: None,
            in_reply_to: Some("input".into()),
            status: Some(ReplyStatus::Failed),
            failure: Some(FailureKind::MissingBinary),
            answers: None,
            attestation: None,
        })?;
        let stopped = history.command(
            "stopped",
            WorkspaceCommand::AssignmentProgress {
                assignment_id: "assignment".into(),
                revision: 1,
                state: AssignmentState::Stopped,
                result: None,
                evidence_event: Some(evidence.clone()),
                execution: Some(TurnExecution::NotStarted),
                input_event: None,
            },
        )?;
        history.compare("wanted")?;
        let released = history.append_dependent(
            EventBody::WorkspaceCommanded {
                command_id: "release".into(),
                command: WorkspaceCommand::ControlInput {
                    message_id: "input".into(),
                    expected_event: input_hash,
                    action: InputControl::Release,
                },
                acting_assignment: None,
                attestation: None,
                cause: None,
            },
            vec![stopped, evidence],
        )?;
        history.command(
            "move",
            WorkspaceCommand::ControlInput {
                message_id: "input".into(),
                expected_event: released,
                action: InputControl::Move {
                    before_message_id: Some("before".into()),
                },
            },
        )?;
        let full = graph_at(&history.db, &history.network, Some(&history.clock()?))?;
        assert_eq!(
            full.assignments["assignment"].state,
            AssignmentState::Proposed
        );
        assert!(!full.inputs["input"].deferred);
        assert_eq!(full.queue_order, vec!["input", "before"]);
        assert!(full.commands["release"].error.is_none());
        assert!(full.commands["move"].error.is_none());
        history.compare("wanted")?;
        let coordinator = LocalIdentity::generate().device_id()?;
        history.command(
            "channel-new-epoch",
            WorkspaceCommand::ConfigureChannel {
                channel_id: "channel".into(),
                expected_revision: 1,
                title: "Moved".into(),
                purpose: "Purpose".into(),
                members: vec!["bot".into()],
                coordinator_bot: "bot".into(),
                coordinator_node: coordinator,
                owner_epoch: 2,
            },
        )?;
        let full = graph_at(&history.db, &history.network, Some(&history.clock()?))?;
        assert_eq!(full.channels["channel"].owner_epoch, 2);
        assert_eq!(full.assignments["assignment"].channel_owner_epoch, Some(1));
        history.compare("wanted")?;
        Ok(())
    }
}
