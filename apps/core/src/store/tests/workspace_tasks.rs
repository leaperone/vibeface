use super::super::test_support::{
    adopt_initial_assignment, answer_signature, assignment_state, bot_reply, complete_work,
    question, start_work, store, sync, task_state, work_spec, workspace_assigned_task_fixture,
    workspace_command_fixture, workspace_fixture, workspace_input_fixture,
    workspace_started_job_fixture, workspace_task_command,
};
use super::super::*;
use crate::providers::Provider;
use crate::scheduling::{JobOutcome, JobState, Shown, TriggerKind, message_trigger};
use crate::work::{Signal, WorkSpec};

#[test]
fn workspace_signed_commands_inputs_and_task_history_are_scoped_and_idempotent() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "history-channel",
        WorkspaceCommand::CreateChannel {
            channel_id: "history-channel".into(),
            title: "History".into(),
            purpose: "Keep task work in its channel".into(),
            members: vec!["bot".into()],
            coordinator_bot: "bot".into(),
            coordinator_node: net.a.device_id()?,
        },
        3,
    )?;
    let mut command = workspace_task_command("t1", None);
    if let WorkspaceCommand::CreateTask { channel_id, .. } = &mut command {
        *channel_id = Some("history-channel".into());
    }
    let created = workspace_command_fixture(&net, &mut origin, "create-t1", command.clone(), 3)?;
    let duplicate = workspace_command_fixture(&net, &mut origin, "create-t1", command, 4)?;
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.event_hash, created.event_hash);
    assert!(
        workspace_command_fixture(
            &net,
            &mut origin,
            "create-t1",
            workspace_task_command("different", None),
            5
        )
        .is_err()
    );
    workspace_command_fixture(
        &net,
        &mut origin,
        "create-t2",
        workspace_task_command("t2", None),
        6,
    )?;
    let first = workspace_input_fixture(
        &net,
        &mut origin,
        "m1",
        Some("t1"),
        InputIntent::Continue,
        7,
    )?;
    workspace_input_fixture(
        &net,
        &mut origin,
        "m2",
        Some("t2"),
        InputIntent::Continue,
        8,
    )?;
    workspace_input_fixture(&net, &mut origin, "main", None, InputIntent::Continue, 9)?;
    origin.append_event(
        &net.a,
        &net.network,
        "c",
        bot_reply("reply-t1", "m1"),
        vec![],
        10,
    )?;
    origin.append_event(
        &net.a,
        &net.network,
        "c",
        bot_reply("reply-t2", "m2"),
        vec![],
        11,
    )?;
    let resolved = origin.resolve_input_target(
        &net.network,
        &InputTarget::Task {
            task_id: "t1".into(),
        },
    )?;
    assert_eq!(
        resolved.scope,
        crate::scheduling::SessionScope::WorkspaceTask {
            task_id: "t1".into()
        }
    );
    assert_ne!(
        resolved.scope,
        origin
            .resolve_input_target(
                &net.network,
                &InputTarget::Task {
                    task_id: "t2".into()
                }
            )?
            .scope
    );
    assert_eq!(
        origin
            .main_messages(&net.network, "c")?
            .iter()
            .map(|message| message.message_id.as_str())
            .collect::<Vec<_>>(),
        ["main"]
    );
    let page = origin.workspace_history(&net.network, None, Some("t1"), None, 100)?;
    let (history, truncated) = (page.events, page.coverage.truncated);
    assert!(!truncated);
    assert!(history.iter().any(
        |event| matches!(&event.body,EventBody::InputPosted{message_id,..}if message_id=="m1")
    ));
    assert!(history.iter().any(|event|matches!(&event.body,EventBody::MessagePosted{message_id,..}if message_id=="reply-t1")));
    assert!(!history.iter().any(|event|matches!(&event.body,EventBody::InputPosted{message_id,..}|EventBody::MessagePosted{message_id,..}if message_id=="m2"||message_id=="reply-t2")));
    assert!(history.iter().any(|event| event.speaker
        == WorkspaceSpeaker::Human {
            device_id: net.a.fixture_admin().device_id().unwrap()
        }));
    assert!(history.iter().any(|event| event.speaker
        == WorkspaceSpeaker::Bot {
            bot_id: "bot".into()
        }));
    let mut changed = first.input.clone();
    changed.text = "forged text".into();
    let original = log::event_from(&origin.connection, &net.network, &first.source_event)?
        .context("original input")?;
    assert!(
        origin
            .post_workspace_input(
                &net.a,
                &net.network,
                changed,
                original.body.attestation().cloned().context("signature")?,
                12
            )
            .is_err()
    );
    assert!(
        origin
            .resolve_input_target(
                &net.network,
                &InputTarget::ChannelTask {
                    channel_id: "other".into(),
                    task_id: "t1".into(),
                    bot_id: "bot".into()
                }
            )
            .is_err()
    );
    assert!(
        origin
            .resolve_input_target(
                &net.b.fixture_network_id()?,
                &InputTarget::Task {
                    task_id: "t1".into()
                }
            )
            .is_err()
    );
    let forged_body = net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::InputPosted {
            message_id: "m1".into(),
            target: InputTarget::Task {
                task_id: "t2".into(),
            },
            intent: InputIntent::Continue,
            sequence: 99,
            expected_revision: Some(1),
            text: "changed target".into(),
            attachments: vec![],
            report_results: Some(true),
            attestation: None,
        },
    )?;
    let forged = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![first.source_event.clone()],
        scope_id: "c".into(),
        created_at_ms: 13,
        body: forged_body,
        blobs: vec![],
    })?;
    assert!(
        matches!(&origin.ingest_events(&net.network, &[forged])?.statuses[0], IngestStatus::Rejected(error) if error.contains("another signed intent"))
    );
    let changed = net.a.fixture_admin().attest(
        &net.network,
        WORKSPACE_SCOPE,
        EventBody::WorkspaceCommanded {
            acting_assignment: None,
            command_id: "create-t1".into(),
            command: workspace_task_command("forged-task", None),
            attestation: None,
            cause: None,
        },
    )?;
    let forged = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![created.event_hash.clone()],
        scope_id: WORKSPACE_SCOPE.into(),
        created_at_ms: 14,
        body: changed,
        blobs: vec![],
    })?;
    assert!(
        matches!(&origin.ingest_events(&net.network, &[forged])?.statuses[0], IngestStatus::Rejected(error) if error.contains("another signed intent"))
    );
    assert!(
        origin
            .workspace_task(&net.network, "forged-task")?
            .is_none()
    );
    let mut history_task = workspace_task_command("history-task", None);
    if let WorkspaceCommand::CreateTask { channel_id, .. } = &mut history_task {
        *channel_id = Some("history-channel".into());
    }
    workspace_command_fixture(&net, &mut origin, "create-history-task", history_task, 15)?;
    workspace_input_fixture(
        &net,
        &mut origin,
        "history-input",
        Some("history-task"),
        InputIntent::Continue,
        16,
    )?;
    let assignment = origin
        .ensure_workspace_assignment(&net.a, &net.network, "history-task", &[], 17)?
        .context("automatic assignment")?;
    assert!(assignment.work_id.is_none());
    let manager = workspace_started_job_fixture(&net, &mut origin, 18)?;
    let mut works = Vec::new();
    for (trigger, call_ref) in [
        (
            origin.scheduler_work_trigger(&net.network, &net.a.device_id()?, &manager)?,
            "task-work",
        ),
        (message_trigger("m2"), "other-task-work"),
        (message_trigger("main"), "main-work"),
    ] {
        let work = WorkSpec {
            cwd: "/workspace/history".into(),
            title: call_ref.into(),
            prompt: "report this work".into(),
            ..work_spec(&net.a.device_id()?, &trigger, call_ref, Provider::Codex)
        };
        origin.accept_work_fixture(&net.a, &net.network, work.clone(), 19)?;
        start_work(
            &mut origin,
            &net,
            &net.a,
            &work.work_id,
            (&format!("{call_ref}-thread"), 20),
            (&format!("{call_ref}-turn"), 21),
        )?;
        works.push(work);
    }
    for ended in [false, true] {
        if ended {
            for work in &works {
                complete_work(
                    &mut origin,
                    &net,
                    &net.a,
                    &work.work_id,
                    &format!("{}-turn", work.call_ref),
                    &format!("{} result", work.call_ref),
                    22,
                )?;
            }
        }
        for (channel, task, expected_work) in [
            (None, Some("history-task"), &works[0].work_id),
            (Some("history-channel"), None, &works[0].work_id),
            (None, Some("t2"), &works[1].work_id),
        ] {
            let page = origin.workspace_history(&net.network, channel, task, None, 100)?;
            let (history, truncated) = (page.events, page.coverage.truncated);
            assert!(!truncated);
            let facts: Vec<_> = history
                .iter()
                .filter(|event| event.body.work_id().is_some())
                .collect();
            assert_eq!(
                facts
                    .iter()
                    .filter_map(|event| event.body.work_id())
                    .collect::<std::collections::BTreeSet<_>>(),
                std::collections::BTreeSet::from([expected_work.as_str()])
            );
            let mut kinds: Vec<_> = facts.iter().map(|event| event.kind.as_str()).collect();
            kinds.sort_unstable();
            kinds.dedup();
            assert_eq!(
                kinds,
                if ended {
                    vec!["work_dispatched", "work_finished", "work_progress"]
                } else {
                    vec!["work_dispatched", "work_progress"]
                }
            );
        }
        let (main_history, _) = origin.main_history_page(
            &net.network,
            "c",
            &origin.version_vector(&net.network)?,
            None,
            crate::store::HistoryFilter {
                kinds: &[],
                author: None,
                keyword: None,
            },
            100,
        )?;
        assert_eq!(
            main_history
                .iter()
                .filter_map(|event| event.event.body.work_id())
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([works[2].work_id.as_str()])
        );
    }
    assert!(
        origin
            .workspace_history(&net.b.fixture_network_id()?, None, Some("t1"), None, 100)
            .is_err()
    );
    let mut replica = store(&net)?;
    sync(&origin, &mut replica, &net)?;
    assert_eq!(
        replica.workspace_task(&net.network, "t1")?,
        origin.workspace_task(&net.network, "t1")?
    );
    assert!(
        replica.scheduler_jobs_all()?.is_empty(),
        "sync must not enqueue execution"
    );
    Ok(())
}

#[test]
fn workspace_dependencies_reply_depth_and_automatic_dispatch_are_bounded() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "root",
        workspace_task_command("root", None),
        3,
    )?;
    let before_cycle = origin.version_vector(&net.network)?;
    let mut cycle = workspace_task_command("reply-cycle", Some("root"));
    if let WorkspaceCommand::CreateTask {
        dependencies,
        reply_only,
        ..
    } = &mut cycle
    {
        *dependencies = vec!["root".into()];
        *reply_only = true;
    }
    let cycle = workspace_command_fixture(&net, &mut origin, "reply-cycle", cycle, 4).unwrap_err();
    assert!(cycle.to_string().contains("cycle"));
    assert!(
        origin
            .workspace_task(&net.network, "reply-cycle")?
            .is_none()
    );
    assert!(
        origin
            .workspace_task(&net.network, "root")?
            .unwrap()
            .dependencies
            .is_empty()
    );
    assert_eq!(origin.version_vector(&net.network)?, before_cycle);
    workspace_command_fixture(
        &net,
        &mut origin,
        "child",
        workspace_task_command("child", Some("root")),
        4,
    )?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "wait-child",
        WorkspaceCommand::ReviseTask {
            task_id: "root".into(),
            expected_revision: 1,
            request: "wait for child".into(),
            expected_result: "result".into(),
            dependencies: vec!["child".into()],
        },
        5,
    )?;
    assert!(
        workspace_command_fixture(
            &net,
            &mut origin,
            "cycle",
            WorkspaceCommand::ReviseTask {
                task_id: "child".into(),
                expected_revision: 1,
                request: "cycle".into(),
                expected_result: "result".into(),
                dependencies: vec!["root".into()]
            },
            6
        )
        .is_err()
    );
    assert!(
        workspace_command_fixture(
            &net,
            &mut origin,
            "self-cycle",
            WorkspaceCommand::ReviseTask {
                task_id: "child".into(),
                expected_revision: 1,
                request: "self".into(),
                expected_result: "result".into(),
                dependencies: vec!["child".into()]
            },
            7
        )
        .is_err()
    );
    for (index, parent) in [("depth3", "child"), ("depth4", "depth3")] {
        workspace_command_fixture(
            &net,
            &mut origin,
            index,
            workspace_task_command(index, Some(parent)),
            8,
        )?;
    }
    assert!(
        workspace_command_fixture(
            &net,
            &mut origin,
            "depth5",
            workspace_task_command("depth5", Some("depth4")),
            9
        )
        .is_err()
    );
    for index in 0..3 {
        let mut command = workspace_task_command(&format!("reply{index}"), Some("root"));
        if let WorkspaceCommand::CreateTask { reply_only, .. } = &mut command {
            *reply_only = true;
        }
        let result = workspace_command_fixture(
            &net,
            &mut origin,
            &format!("reply{index}"),
            command,
            10 + index,
        );
        assert_eq!(result.is_ok(), index < 2);
    }
    for index in 0..9 {
        let id = format!("parallel{index}");
        workspace_command_fixture(
            &net,
            &mut origin,
            &format!("create-{id}"),
            workspace_task_command(&id, Some("root")),
            20 + index,
        )?;
        let result = workspace_command_fixture(
            &net,
            &mut origin,
            &format!("assign-{id}"),
            WorkspaceCommand::AssignTask {
                task_id: id.clone(),
                expected_revision: 1,
                assignment_id: format!("assignment{index}"),
                bot_id: "bot".into(),
                execution_node: net.a.device_id()?,
                bot_owner_epoch: 1,
                channel_owner_epoch: None,
                work_id: None,
                resource_roots: vec![],
                automatic: true,
            },
            40 + index,
        );
        assert_eq!(result.is_ok(), index < 8, "{result:?}");
    }
    assert_eq!(
        task_state(&origin, &net, "parallel8")?,
        TaskState::WaitingUser
    );
    Ok(())
}

#[test]
fn workspace_old_revision_results_and_unknown_assignments_do_not_release_current_work() -> Result<()>
{
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let (input, assignment) = workspace_assigned_task_fixture(
        &net,
        &mut origin,
        "task",
        "task",
        "m",
        &["/workspace/project".into()],
        3,
    )?;
    let progress = |state, input_event, evidence_event| WorkspaceCommand::AssignmentProgress {
        execution: None,
        assignment_id: assignment.assignment_id.clone(),
        revision: 1,
        state,
        input_event,
        evidence_event,
        result: Some("result r1".into()),
    };
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "accepted",
        progress(AssignmentState::Accepted, None, None),
        None,
        None,
        None,
        6,
    )?;
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "adopted",
        progress(AssignmentState::Adopted, Some(input.source_event), None),
        None,
        None,
        None,
        7,
    )?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "revise",
        WorkspaceCommand::ReviseTask {
            task_id: "task".into(),
            expected_revision: 1,
            request: "new revision".into(),
            expected_result: "new result".into(),
            dependencies: vec![],
        },
        8,
    )?;
    assert_eq!(
        origin
            .workspace_assignment(&net.network, &assignment.assignment_id)?
            .unwrap()
            .started_revision,
        1
    );
    let reply = origin.append_event(
        &net.a,
        &net.network,
        "c",
        bot_reply("reply-r1", "m"),
        vec![],
        9,
    )?;
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "completed-r1",
        progress(AssignmentState::Completed, None, Some(reply.hash)),
        None,
        None,
        None,
        10,
    )?;
    assert_eq!(task_state(&origin, &net, "task")?, TaskState::UpdatePending);
    let (_, other) = workspace_assigned_task_fixture(
        &net,
        &mut origin,
        "task2",
        "task2",
        "m2",
        &["/workspace/project/sub".into()],
        11,
    )?;
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "unknown",
        WorkspaceCommand::AssignmentProgress {
            execution: None,
            assignment_id: other.assignment_id.clone(),
            revision: 1,
            state: AssignmentState::Unknown,
            input_event: None,
            evidence_event: None,
            result: None,
        },
        None,
        None,
        None,
        14,
    )?;
    assert_eq!(
        task_state(&origin, &net, "task2")?,
        TaskState::NeedsReconciliation
    );
    assert!(
        origin
            .workspace_resource_conflicts(
                &net.network,
                &net.a.device_id()?,
                &["/workspace/project".into()],
                None
            )?
            .contains(&other.assignment_id)
    );

    assert!(
        workspace_command_fixture(
            &net,
            &mut origin,
            "unsafe-handoff",
            WorkspaceCommand::AssignTask {
                task_id: "task2".into(),
                expected_revision: 1,
                assignment_id: "new-assignment".into(),
                bot_id: "bot".into(),
                execution_node: net.a.device_id()?,
                bot_owner_epoch: 1,
                channel_owner_epoch: None,
                work_id: None,
                resource_roots: vec![],
                automatic: false
            },
            15
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn workspace_revision_submission_waits_for_real_completion_evidence() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let (initial, assignment) = workspace_assigned_task_fixture(
        &net,
        &mut origin,
        "create",
        "task",
        "m1",
        &["/workspace/revision".into()],
        3,
    )?;
    let command = |revision, state, input_event, evidence_event, result| {
        WorkspaceCommand::AssignmentProgress {
            execution: None,
            assignment_id: assignment.assignment_id.clone(),
            revision,
            state,
            input_event,
            evidence_event,
            result,
        }
    };
    adopt_initial_assignment(
        &mut origin,
        &net,
        &net.a,
        &assignment.assignment_id,
        initial.source_event,
        ("accepted", 6),
        ("adopt-r1", 7),
    )?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "revision",
        WorkspaceCommand::ReviseTask {
            task_id: "task".into(),
            expected_revision: 1,
            request: "revised request".into(),
            expected_result: "revised result".into(),
            dependencies: vec![],
        },
        8,
    )?;
    let revised = workspace_input_fixture(
        &net,
        &mut origin,
        "m2",
        Some("task"),
        InputIntent::Continue,
        9,
    )?;
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "adopt-r2",
        command(
            2,
            AssignmentState::Adopted,
            Some(revised.source_event),
            None,
            None,
        ),
        None,
        None,
        None,
        10,
    )?;
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "submit-r2",
        command(
            2,
            AssignmentState::Completed,
            None,
            None,
            Some("declared result".into()),
        ),
        None,
        None,
        None,
        11,
    )?;
    let pending = origin
        .workspace_assignment(&net.network, &assignment.assignment_id)?
        .unwrap();
    assert_eq!(pending.state, AssignmentState::Adopted);
    assert_eq!(pending.submitted_result.as_ref().unwrap().revision, 2);
    assert!(pending.result.is_none());
    assert_ne!(task_state(&origin, &net, "task")?, TaskState::Completed);
    assert!(
        origin
            .workspace_resource_conflicts(
                &net.network,
                &net.a.device_id()?,
                &["/workspace/revision".into()],
                None
            )?
            .contains(&assignment.assignment_id)
    );
    let proof = origin.append_event(
        &net.a,
        &net.network,
        "c",
        bot_reply("actual-reply", "m1"),
        vec![],
        12,
    )?;
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "complete-r2",
        command(
            2,
            AssignmentState::Completed,
            None,
            Some(proof.hash),
            Some("actual final result".into()),
        ),
        None,
        None,
        None,
        13,
    )?;
    let completed = origin
        .workspace_assignment(&net.network, &assignment.assignment_id)?
        .unwrap();
    assert_eq!(completed.result.as_deref(), Some("actual final result"));
    assert_eq!(completed.completed_revision, Some(2));
    assert_eq!(task_state(&origin, &net, "task")?, TaskState::Completed);
    Ok(())
}

#[test]
fn workspace_attached_running_work_does_not_imply_task_revision_adoption() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    workspace_input_fixture(&net, &mut origin, "seed", None, InputIntent::Continue, 3)?;
    let trigger = message_trigger("seed");
    let work_id = WorkSpec::id("c", &trigger, "existing-work");
    let spec = WorkSpec {
        work_id: work_id.clone(),
        cwd: "/workspace/work".into(),
        title: "existing work".into(),
        prompt: "original work request".into(),
        ..work_spec(
            &net.a.device_id()?,
            &trigger,
            "existing-work",
            Provider::Codex,
        )
    };
    origin.accept_work_fixture(&net.a, &net.network, spec, 4)?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "create-task",
        workspace_task_command("attached", None),
        5,
    )?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "attach",
        WorkspaceCommand::AssignTask {
            task_id: "attached".into(),
            expected_revision: 1,
            assignment_id: "attached-assignment".into(),
            bot_id: "bot".into(),
            execution_node: net.a.device_id()?,
            bot_owner_epoch: 1,
            channel_owner_epoch: None,
            work_id: Some(work_id.clone()),
            resource_roots: vec!["/workspace/work".into()],
            automatic: false,
        },
        6,
    )?;
    let assignment = origin
        .workspace_assignment(&net.network, "attached-assignment")?
        .unwrap();
    origin.settle_workspace_work_assignment(&net.a, &net.network, &assignment, 7)?;
    assert_eq!(
        assignment_state(&origin, &net, "attached-assignment")?,
        AssignmentState::Proposed
    );
    assert_eq!(
        task_state(&origin, &net, "attached")?,
        TaskState::NotStarted
    );
    origin.apply_work_signal(
        &net.a,
        &net.network,
        &work_id,
        Signal::ThreadCreated("thread".into()),
        8,
    )?;
    origin.settle_workspace_work_assignment(&net.a, &net.network, &assignment, 9)?;
    assert_eq!(
        assignment_state(&origin, &net, "attached-assignment")?,
        AssignmentState::Proposed,
        "a native locator is not an accepted turn"
    );
    origin.apply_work_signal(
        &net.a,
        &net.network,
        &work_id,
        Signal::TurnStarted {
            turn_id: "turn".into(),
            epoch: 1,
        },
        10,
    )?;
    origin.settle_workspace_work_assignment(&net.a, &net.network, &assignment, 11)?;
    let accepted = origin
        .workspace_assignment(&net.network, "attached-assignment")?
        .unwrap();
    assert_eq!(accepted.state, AssignmentState::Accepted);
    assert!(accepted.adopted_revision.is_none());
    assert!(
        origin
            .scheduler_job("c", "assignment:attached-assignment")?
            .is_none()
    );
    complete_work(
        &mut origin,
        &net,
        &net.a,
        &work_id,
        "turn",
        "original work finished",
        12,
    )?;
    origin.settle_workspace_work_assignment(&net.a, &net.network, &accepted, 13)?;
    let ended = origin
        .workspace_assignment(&net.network, "attached-assignment")?
        .unwrap();
    assert_eq!(ended.state, AssignmentState::Stopped);
    assert_eq!(ended.result.as_deref(), Some("original work finished"));
    let task = origin.workspace_task(&net.network, "attached")?.unwrap();
    assert_eq!(task.state, TaskState::WaitingUser);
    assert_eq!(
        task.waiting_reason.as_deref(),
        Some("assignment_not_adopted")
    );
    Ok(())
}

#[test]
fn workspace_concurrent_old_completion_is_preserved_after_a_new_revision() -> Result<()> {
    use crate::workspace::*;
    let (net, mut a) = workspace_fixture()?;
    let owner = net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::ConversationOwner {
            scheduler_node: net.b.device_id()?,
            epoch: 2,
            attestation: None,
            cause: None,
        },
    )?;
    a.append_event(&net.a, &net.network, "c", owner, vec![], 3)?;
    workspace_command_fixture(
        &net,
        &mut a,
        "create-task",
        workspace_task_command("task", None),
        4,
    )?;
    workspace_command_fixture(
        &net,
        &mut a,
        "create-dependency",
        workspace_task_command("pending", None),
        5,
    )?;
    workspace_command_fixture(
        &net,
        &mut a,
        "assign-b",
        WorkspaceCommand::AssignTask {
            task_id: "task".into(),
            expected_revision: 1,
            assignment_id: "assignment-b".into(),
            bot_id: "bot".into(),
            execution_node: net.b.device_id()?,
            bot_owner_epoch: 2,
            channel_owner_epoch: None,
            work_id: None,
            resource_roots: vec!["/workspace/concurrent".into()],
            automatic: false,
        },
        6,
    )?;
    let mut b = store(&net)?;
    sync(&a, &mut b, &net)?;
    let assignment = b
        .workspace_assignment(&net.network, "assignment-b")?
        .context("assignment")?;
    let progress = |state, input_event, evidence_event| WorkspaceCommand::AssignmentProgress {
        execution: None,
        assignment_id: "assignment-b".into(),
        revision: 1,
        state,
        input_event,
        evidence_event,
        result: None,
    };
    adopt_initial_assignment(
        &mut b,
        &net,
        &net.b,
        "assignment-b",
        assignment.event_hash,
        ("accepted-b", 7),
        ("adopted-b", 8),
    )?;
    workspace_command_fixture(
        &net,
        &mut a,
        "correction-a",
        WorkspaceCommand::ReviseTask {
            task_id: "task".into(),
            expected_revision: 1,
            request: "wait for the new dependency".into(),
            expected_result: "revision two".into(),
            dependencies: vec!["pending".into()],
        },
        9,
    )?;
    let result = b.append_event(
        &net.b,
        &net.network,
        "c",
        bot_reply("b-r1-result", "assignment-b"),
        vec![],
        10,
    )?;
    let completed = b.apply_workspace_command(
        &net.b,
        &net.network,
        "completed-b",
        progress(AssignmentState::Completed, None, Some(result.hash.clone())),
        None,
        None,
        None,
        11,
    )?;
    sync(&b, &mut a, &net)?;
    assert!(a.event(&net.network, &completed.event_hash)?.is_some());
    let stored = a
        .workspace_assignment(&net.network, "assignment-b")?
        .context("stored assignment")?;
    assert_eq!(stored.completed_revision, Some(1));
    assert_eq!(stored.state, AssignmentState::Completed);
    let task = a.workspace_task(&net.network, "task")?.context("task")?;
    assert_eq!(task.revision, 2);
    assert_eq!(task.dependencies, ["pending"]);
    assert_eq!(task.state, TaskState::UpdatePending);
    let history = a
        .workspace_history(&net.network, None, Some("task"), None, 100)?
        .events;
    assert!(history.iter().any(|event| event.event_hash == result.hash));
    assert!(
        history
            .iter()
            .any(|event| event.event_hash == completed.event_hash)
    );
    Ok(())
}

#[test]
fn workspace_assignment_work_proof_is_frozen_and_requires_current_adoption() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let (input, assignment) =
        workspace_assigned_task_fixture(&net, &mut origin, "create", "task", "m1", &[], 3)?;
    let progress = |revision, state, source| WorkspaceCommand::AssignmentProgress {
        execution: None,
        assignment_id: assignment.assignment_id.clone(),
        revision,
        state,
        input_event: source,
        evidence_event: None,
        result: None,
    };
    adopt_initial_assignment(
        &mut origin,
        &net,
        &net.a,
        &assignment.assignment_id,
        input.source_event,
        ("accepted", 6),
        ("adopt-r1", 7),
    )?;
    let local = net.a.device_id()?;
    assert!(workspace::assignment_work_allowed_from(
        &origin.connection,
        &net.network,
        &assignment.assignment_id,
        &local,
        "c"
    )?);
    assert!(!workspace::assignment_work_allowed_from(
        &origin.connection,
        &net.network,
        &assignment.assignment_id,
        &net.b.device_id()?,
        "c"
    )?);
    let (r1, first_refs) = workspace::assignment_work_source_from(
        &origin.connection,
        &net.network,
        &assignment.assignment_id,
    )?;
    assert_eq!(r1, 1);
    workspace_command_fixture(
        &net,
        &mut origin,
        "revise-r2",
        WorkspaceCommand::ReviseTask {
            task_id: "task".into(),
            expected_revision: 1,
            request: "revision two".into(),
            expected_result: "r2 result".into(),
            dependencies: vec![],
        },
        8,
    )?;
    assert!(
        workspace::task_input_allowed_from(&origin.connection, &net.network, "task")?,
        "active correction is allowed before adoption"
    );
    assert!(!workspace::assignment_work_allowed_from(
        &origin.connection,
        &net.network,
        &assignment.assignment_id,
        &local,
        "c"
    )?);
    let revised = workspace_input_fixture(
        &net,
        &mut origin,
        "m2",
        Some("task"),
        InputIntent::Continue,
        9,
    )?;
    let adopted = origin.apply_workspace_command(
        &net.a,
        &net.network,
        "adopt-r2",
        progress(
            2,
            AssignmentState::Adopted,
            Some(revised.source_event.clone()),
        ),
        None,
        None,
        None,
        10,
    )?;
    assert!(workspace::assignment_work_allowed_from(
        &origin.connection,
        &net.network,
        &assignment.assignment_id,
        &local,
        "c"
    )?);
    let (r2, refs) = workspace::assignment_work_source_from(
        &origin.connection,
        &net.network,
        &assignment.assignment_id,
    )?;
    assert_eq!(r2, 2);
    assert!(refs.contains(&adopted.event_hash));
    assert!(refs.contains(&revised.source_event));
    let mut missing_adoption = refs.clone();
    missing_adoption.retain(|hash| hash != &adopted.event_hash);
    assert!(!workspace::assignment_work_source_matches_from(
        &origin.connection,
        &net.network,
        &assignment.assignment_id,
        2,
        &missing_adoption
    )?);
    workspace_command_fixture(
        &net,
        &mut origin,
        "same-r2-control",
        WorkspaceCommand::ControlTask {
            task_id: "task".into(),
            expected_revision: 2,
            action: TaskAction::Continue,
        },
        11,
    )?;
    assert!(workspace::assignment_work_source_matches_from(
        &origin.connection,
        &net.network,
        &assignment.assignment_id,
        2,
        &refs
    )?);
    workspace_command_fixture(
        &net,
        &mut origin,
        "revise-r3",
        WorkspaceCommand::ReviseTask {
            task_id: "task".into(),
            expected_revision: 2,
            request: "revision three".into(),
            expected_result: "r3 result".into(),
            dependencies: vec![],
        },
        12,
    )?;
    assert!(!workspace::assignment_work_allowed_from(
        &origin.connection,
        &net.network,
        &assignment.assignment_id,
        &local,
        "c"
    )?);
    assert!(workspace::assignment_work_source_matches_from(
        &origin.connection,
        &net.network,
        &assignment.assignment_id,
        1,
        &first_refs
    )?);
    assert!(workspace::assignment_work_source_matches_from(
        &origin.connection,
        &net.network,
        &assignment.assignment_id,
        2,
        &refs
    )?);
    assert!(!workspace::assignment_work_source_matches_from(
        &origin.connection,
        &net.network,
        &assignment.assignment_id,
        3,
        &refs
    )?);
    let mut reply = workspace_task_command("reply", Some("task"));
    if let WorkspaceCommand::CreateTask { reply_only, .. } = &mut reply {
        *reply_only = true;
    }
    workspace_command_fixture(&net, &mut origin, "create-reply", reply, 13)?;
    workspace_input_fixture(
        &net,
        &mut origin,
        "reply-input",
        Some("reply"),
        InputIntent::Continue,
        14,
    )?;
    let refused = origin
        .ensure_workspace_assignment(&net.a, &net.network, "reply", &[], 15)
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("reply-only turns cannot issue workspace commands")
    );
    assert!(
        origin
            .workspace_task(&net.network, "reply")?
            .unwrap()
            .current_assignment
            .is_none()
    );
    workspace_command_fixture(
        &net,
        &mut origin,
        "assign-reply",
        WorkspaceCommand::AssignTask {
            task_id: "reply".into(),
            expected_revision: 1,
            assignment_id: "reply-assignment".into(),
            bot_id: "bot".into(),
            execution_node: local.clone(),
            bot_owner_epoch: 1,
            channel_owner_epoch: None,
            work_id: None,
            resource_roots: vec![],
            automatic: false,
        },
        15,
    )?;
    let reply = origin
        .workspace_assignment(&net.network, "reply-assignment")?
        .context("reply assignment")?;
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "accepted-reply",
        WorkspaceCommand::AssignmentProgress {
            execution: None,
            assignment_id: reply.assignment_id.clone(),
            revision: 1,
            state: AssignmentState::Accepted,
            input_event: None,
            evidence_event: None,
            result: None,
        },
        None,
        None,
        None,
        16,
    )?;
    assert!(
        !workspace::assignment_work_allowed_from(
            &origin.connection,
            &net.network,
            &reply.assignment_id,
            &local,
            "c"
        )?,
        "reply-only tasks cannot start Work"
    );
    Ok(())
}

#[test]
fn workspace_cancelled_task_cannot_restart_through_revision() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "create",
        workspace_task_command("task", None),
        3,
    )?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "cancel",
        WorkspaceCommand::ControlTask {
            task_id: "task".into(),
            expected_revision: 1,
            action: TaskAction::Cancel,
        },
        4,
    )?;
    let cancelled = origin.workspace_task(&net.network, "task")?.unwrap();
    let before = origin.version_vector(&net.network)?;
    let revised = workspace_command_fixture(
        &net,
        &mut origin,
        "revise-cancelled",
        WorkspaceCommand::ReviseTask {
            task_id: "task".into(),
            expected_revision: 1,
            request: "start again".into(),
            expected_result: "result".into(),
            dependencies: vec![],
        },
        5,
    )
    .unwrap_err();
    assert!(
        revised
            .to_string()
            .contains("cancelled tasks cannot be revised")
    );
    assert!(
        workspace_command_fixture(
            &net,
            &mut origin,
            "continue-cancelled",
            WorkspaceCommand::ControlTask {
                task_id: "task".into(),
                expected_revision: 1,
                action: TaskAction::Continue
            },
            6
        )
        .is_err()
    );
    assert_eq!(
        origin.workspace_task(&net.network, "task")?.unwrap(),
        cancelled
    );
    assert_eq!(origin.version_vector(&net.network)?, before);
    assert_eq!(cancelled.state, TaskState::Cancelled);
    assert!(origin.scheduler_jobs_all()?.is_empty());
    Ok(())
}

#[test]
fn workspace_task_answer_keeps_the_assignment_from_the_signed_question_basis() -> Result<()> {
    use crate::scheduling::TurnExecution;
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let local = net.a.device_id()?;
    let (_, first) =
        workspace_assigned_task_fixture(&net, &mut origin, "create", "task", "original", &[], 3)?;
    let running = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "question-basis")?
        .unwrap()
        .job;
    let attempt = running.state.attempt().unwrap().id.clone();
    origin.mark_started("c", &running.trigger, &attempt)?;
    origin.update_turn_runtime(
        "c",
        &running.trigger,
        &attempt,
        vibeface_bridge::failure::ProviderStage::Run,
        TurnExecution::Started,
    )?;
    origin.settle_workspace_assignment(&net.a, &net.network, &running, 6)?;
    origin.save_scheduler_native_id(
        "c",
        &running.trigger,
        &attempt,
        "task-native-session",
        "fixture-token",
        false,
    )?;
    origin.record_manager_turn(
        &attempt,
        "fixture-server",
        "task-native-session",
        Some("task-native-turn"),
        &Shown::default(),
    )?;
    for (id, session) in [
        ("q-task-question", "task-native-session"),
        ("q-wrong-session", "another-native-session"),
    ] {
        let mut asked = question(&origin, &net, "c", id, session)?;
        if let EventBody::QuestionAsked { target, .. } = &mut asked {
            *target = Some(InputTarget::Task {
                task_id: "task".into(),
            });
        }
        origin.append_event(&net.a, &net.network, "c", asked, vec![], 7)?;
        origin.accept_answer(
            &net.a,
            &net.network,
            id,
            "continue with this answer",
            Some(answer_signature(
                &net,
                "c",
                id,
                "continue with this answer",
            )?),
            8,
        )?;
    }
    let answer = origin
        .scheduler_job(
            "c",
            &message_trigger(&questions::answer_id("q-task-question")),
        )?
        .context("Task answer job")?;
    assert_eq!(answer.kind, TriggerKind::Answer);
    assert_eq!(
        answer.scope,
        crate::scheduling::SessionScope::WorkspaceTask {
            task_id: "task".into()
        }
    );
    assert_eq!(
        workspace::workspace_assignment_for_job_from(
            &origin.connection,
            &net.network,
            &local,
            &answer
        )?
        .unwrap()
        .assignment_id,
        first.assignment_id
    );
    let wrong = origin
        .scheduler_job(
            "c",
            &message_trigger(&questions::answer_id("q-wrong-session")),
        )?
        .unwrap();
    assert!(
        workspace::workspace_assignment_for_job_from(
            &origin.connection,
            &net.network,
            &local,
            &wrong
        )?
        .is_none()
    );
    let mut missing_basis = answer.clone();
    missing_basis.basis.clear();
    assert!(
        workspace::workspace_assignment_for_job_from(
            &origin.connection,
            &net.network,
            &local,
            &missing_basis
        )?
        .is_none()
    );
    let mut wrong_task = answer.clone();
    wrong_task.scope = crate::scheduling::SessionScope::WorkspaceTask {
        task_id: "another-task".into(),
    };
    assert!(
        workspace::workspace_assignment_for_job_from(
            &origin.connection,
            &net.network,
            &local,
            &wrong_task
        )?
        .is_none()
    );
    assert!(
        workspace::workspace_assignment_for_job_from(
            &origin.connection,
            &net.network,
            &net.b.device_id()?,
            &answer
        )?
        .is_none()
    );
    let completed = origin.finish_job(
        &net.a,
        &net.network,
        "c",
        &running.trigger,
        &attempt,
        JobOutcome::Completed,
        "original task ended",
        9,
    )?;
    origin.settle_workspace_assignment(&net.a, &net.network, &completed, 10)?;
    assert_eq!(
        assignment_state(&origin, &net, &first.assignment_id)?,
        AssignmentState::Adopted,
        "the accepted answer still needs its own result before the allocation can finish"
    );
    let cancelled = origin
        .request_manager_cancel(&net.a, &net.network, "c", &wrong.trigger)?
        .context("unstarted wrong-session answer")?;
    assert!(matches!(
        cancelled.state,
        JobState::Cancelled { attempt: None, .. }
    ));
    let answering = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "question-basis")?
        .context("original answer turn")?
        .job;
    assert_eq!(answering.trigger, answer.trigger);
    let answer_attempt = answering.state.attempt().unwrap().id.clone();
    origin.mark_started("c", &answering.trigger, &answer_attempt)?;
    origin.update_turn_runtime(
        "c",
        &answering.trigger,
        &answer_attempt,
        vibeface_bridge::failure::ProviderStage::Run,
        TurnExecution::Started,
    )?;
    origin.settle_workspace_assignment(&net.a, &net.network, &answering, 12)?;
    let answered = origin.finish_job(
        &net.a,
        &net.network,
        "c",
        &answering.trigger,
        &answer_attempt,
        JobOutcome::Completed,
        "answered the original question",
        13,
    )?;
    origin.settle_workspace_assignment(&net.a, &net.network, &answered, 14)?;
    workspace_input_fixture(
        &net,
        &mut origin,
        "new-request",
        Some("task"),
        InputIntent::Continue,
        15,
    )?;
    let later = origin
        .ensure_workspace_assignment(&net.a, &net.network, "task", &[], 16)?
        .context("later assignment")?;
    assert_ne!(later.assignment_id, first.assignment_id);
    assert_eq!(
        origin
            .workspace_task(&net.network, "task")?
            .unwrap()
            .current_assignment
            .as_deref(),
        Some(later.assignment_id.as_str())
    );
    let fixed = workspace::workspace_assignment_for_job_from(
        &origin.connection,
        &net.network,
        &local,
        &answer,
    )?
    .context("original question assignment")?;
    assert_eq!(fixed.assignment_id, first.assignment_id);
    assert_eq!(fixed.state, AssignmentState::Completed);
    assert!(
        !workspace::assignment_work_allowed_from(
            &origin.connection,
            &net.network,
            &fixed.assignment_id,
            &local,
            "c"
        )?,
        "a late answer cannot borrow the later allocation's write permission"
    );
    Ok(())
}
