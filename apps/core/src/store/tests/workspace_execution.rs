use super::super::test_support::{
    answer_signature, assignment_state, bot_reply, complete_work, question, start_work, store,
    sync, task_state, work_spec, workspace_assigned_task_fixture, workspace_command_fixture,
    workspace_fixture, workspace_input_fixture, workspace_signed_input_fixture,
    workspace_started_job_fixture, workspace_task_command,
};
use super::super::*;
use crate::providers::Provider;
use crate::scheduling::{
    JobOutcome, JobState, SessionScope, TriggerKind, message_trigger, work_trigger,
};
use crate::work::{Signal, WorkSpec};
use vibeface_bridge::failure::{ExecFailure, FailureKind};

#[test]
fn workspace_lifecycle_waits_for_native_start_and_keeps_revision_adoption_explicit() -> Result<()> {
    use crate::scheduling::TurnExecution;
    use crate::workspace::*;
    use vibeface_bridge::failure::ProviderStage;
    let (net, mut origin) = workspace_fixture()?;
    let (_, assignment) =
        workspace_assigned_task_fixture(&net, &mut origin, "create", "task", "initial", &[], 3)?;
    let queued = origin
        .scheduler_job("c", &message_trigger("initial"))?
        .context("queued job")?;
    origin.settle_workspace_assignment(&net.a, &net.network, &queued, 6)?;
    assert_eq!(task_state(&origin, &net, "task")?, TaskState::NotStarted);
    let claimed = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "workspace-start")?
        .context("claimed job")?
        .job;
    let attempt = claimed.state.attempt().context("attempt")?.id.clone();
    origin.settle_workspace_assignment(&net.a, &net.network, &claimed, 7)?;
    origin.mark_started("c", &claimed.trigger, &attempt)?;
    origin.settle_workspace_assignment(&net.a, &net.network, &claimed, 8)?;
    assert_eq!(
        assignment_state(&origin, &net, &assignment.assignment_id)?,
        AssignmentState::Proposed
    );
    origin.update_turn_runtime(
        "c",
        &claimed.trigger,
        &attempt,
        ProviderStage::TurnStart,
        TurnExecution::Unknown,
    )?;
    origin.settle_workspace_assignment(&net.a, &net.network, &claimed, 9)?;
    assert_eq!(
        assignment_state(&origin, &net, &assignment.assignment_id)?,
        AssignmentState::Proposed,
        "an in-flight native start has no accepted turn yet"
    );
    origin.update_turn_runtime(
        "c",
        &claimed.trigger,
        &attempt,
        ProviderStage::Run,
        TurnExecution::Started,
    )?;
    origin.settle_workspace_assignment(&net.a, &net.network, &claimed, 10)?;
    let started = origin
        .workspace_assignment(&net.network, &assignment.assignment_id)?
        .unwrap();
    assert_eq!(started.state, AssignmentState::Adopted);
    assert_eq!(started.accepted_revision, Some(1));
    assert_eq!(started.adopted_revision, Some(1));
    assert!(started.completed_revision.is_none());
    assert!(started.result.is_none());
    assert_eq!(task_state(&origin, &net, "task")?, TaskState::Running);
    let recorded = origin.version_vector(&net.network)?;
    origin.settle_workspace_assignment(&net.a, &net.network, &claimed, 11)?;
    assert_eq!(
        origin.version_vector(&net.network)?,
        recorded,
        "repeated start evidence is idempotent"
    );
    workspace_command_fixture(
        &net,
        &mut origin,
        "revision",
        WorkspaceCommand::ReviseTask {
            task_id: "task".into(),
            expected_revision: 1,
            request: "new revision".into(),
            expected_result: "new result".into(),
            dependencies: vec![],
        },
        12,
    )?;
    workspace_input_fixture(
        &net,
        &mut origin,
        "correction",
        Some("task"),
        InputIntent::Continue,
        13,
    )?;
    origin.settle_workspace_assignment(&net.a, &net.network, &claimed, 14)?;
    let unchanged = origin
        .workspace_assignment(&net.network, &assignment.assignment_id)?
        .unwrap();
    assert_eq!(unchanged.started_revision, 1);
    assert_eq!(unchanged.adopted_revision, Some(1));
    assert_eq!(task_state(&origin, &net, "task")?, TaskState::UpdatePending);
    let completed = origin.finish_job(
        &net.a,
        &net.network,
        "c",
        &claimed.trigger,
        &attempt,
        JobOutcome::Completed,
        "revision one result",
        15,
    )?;
    origin.settle_workspace_assignment(&net.a, &net.network, &completed, 16)?;
    assert_eq!(
        origin
            .workspace_assignment(&net.network, &assignment.assignment_id)?
            .unwrap()
            .completed_revision,
        Some(1)
    );
    assert_eq!(task_state(&origin, &net, "task")?, TaskState::UpdatePending);
    Ok(())
}

#[test]
fn workspace_cancelled_before_native_start_does_not_claim_revision_adoption() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let (_, assignment) =
        workspace_assigned_task_fixture(&net, &mut origin, "create", "task", "initial", &[], 3)?;
    let cancelled = origin
        .request_manager_cancel(&net.a, &net.network, "c", &message_trigger("initial"))?
        .context("cancelled job")?;
    assert!(matches!(
        cancelled.state,
        JobState::Cancelled { attempt: None, .. }
    ));
    origin.settle_workspace_assignment(&net.a, &net.network, &cancelled, 6)?;
    let stopped = origin
        .workspace_assignment(&net.network, &assignment.assignment_id)?
        .unwrap();
    assert_eq!(stopped.state, AssignmentState::Stopped);
    assert!(stopped.accepted_revision.is_none());
    assert!(stopped.adopted_revision.is_none());
    assert!(stopped.completed_revision.is_none());
    Ok(())
}

#[test]
fn workspace_remote_assignment_waits_for_the_target_nodes_ordinary_work_root() -> Result<()> {
    use crate::workspace::*;
    let (net, mut a) = workspace_fixture()?;
    let node_b = net.b.device_id()?;
    let owner = net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::ConversationOwner {
            scheduler_node: node_b.clone(),
            epoch: 2,
            attestation: None,
            cause: None,
        },
    )?;
    a.append_event(&net.a, &net.network, "c", owner, vec![], 3)?;
    workspace_input_fixture(
        &net,
        &mut a,
        "ordinary-request",
        None,
        InputIntent::Continue,
        4,
    )?;
    let mut b = store(&net)?;
    sync(&a, &mut b, &net)?;
    let trigger = message_trigger("ordinary-request");
    let work_id = WorkSpec::id("c", &trigger, "ordinary");
    b.accept_work_fixture(
        &net.b,
        &net.network,
        WorkSpec {
            work_id: work_id.clone(),
            cwd: "/workspace/shared".into(),
            title: "ordinary work".into(),
            prompt: "existing write".into(),
            ..work_spec(&node_b, &trigger, "ordinary", Provider::Codex)
        },
        5,
    )?;
    start_work(
        &mut b,
        &net,
        &net.b,
        &work_id,
        ("ordinary-thread", 6),
        ("ordinary-turn", 7),
    )?;
    sync(&b, &mut a, &net)?;
    workspace_command_fixture(
        &net,
        &mut a,
        "create-task",
        workspace_task_command("remote", None),
        8,
    )?;
    workspace_command_fixture(
        &net,
        &mut a,
        "remote-assignment",
        WorkspaceCommand::AssignTask {
            task_id: "remote".into(),
            expected_revision: 1,
            assignment_id: "remote-assignment".into(),
            bot_id: "bot".into(),
            execution_node: node_b.clone(),
            bot_owner_epoch: 2,
            channel_owner_epoch: None,
            work_id: None,
            resource_roots: vec!["/workspace/shared/sub".into()],
            automatic: false,
        },
        9,
    )?;
    sync(&a, &mut b, &net)?;
    let assignment = b
        .workspace_assignment(&net.network, "remote-assignment")?
        .unwrap();
    let receipt = b
        .admit_delivery(&node_b, &net.network, "c", "assignment:remote-assignment")?
        .context("admission")?;
    assert_eq!(receipt.state, "queued");
    assert!(!workspace::assignment_resources_available_from(
        &b.connection,
        &net.network,
        &assignment
    )?);
    let key = crate::scheduling::SessionKey {
        conversation_id: "c".into(),
        scope: crate::scheduling::SessionScope::WorkspaceTask {
            task_id: "remote".into(),
        },
    };
    let config = crate::scheduling::SessionConfig {
        stable_fingerprint: "resource-test".into(),
        runtime_fingerprint: "resource-test".into(),
        provider: Provider::Codex,
    };
    assert!(
        b.claim_next_in_session(
            &net.b,
            &net.network,
            &key,
            "boot",
            &config,
            &crate::config::SchedulingLimits::default()
        )?
        .is_none()
    );
    assert_eq!(
        b.scheduler_job("c", "assignment:remote-assignment")?
            .unwrap()
            .state,
        JobState::Queued
    );
    complete_work(&mut b, &net, &net.b, &work_id, "ordinary-turn", "done", 10)?;
    assert!(workspace::assignment_resources_available_from(
        &b.connection,
        &net.network,
        &assignment
    )?);
    assert!(
        b.claim_next_in_session(
            &net.b,
            &net.network,
            &key,
            "boot",
            &config,
            &crate::config::SchedulingLimits::default()
        )?
        .is_some()
    );
    Ok(())
}

#[test]
fn an_agent_assigned_task_reports_its_real_work_without_reusing_the_main_cause() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let local = net.a.device_id()?;
    let cause = workspace_input_fixture(
        &net,
        &mut origin,
        "coordinator",
        None,
        InputIntent::Continue,
        3,
    )?;
    origin.admit_delivery(&local, &net.network, "c", "message:coordinator")?;
    let main = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "agent-child")?
        .unwrap()
        .job;
    let main_attempt = main.state.attempt().unwrap().id.clone();
    origin.mark_started("c", &main.trigger, &main_attempt)?;
    let mut child = workspace_task_command("automatic-child", None);
    if let WorkspaceCommand::CreateTask { source_events, .. } = &mut child {
        source_events.push(cause.source_event.clone());
    }
    origin.workspace_agent_command(&net.a, &net.network, &main, "create-child", child, 4)?;
    origin.workspace_agent_command(
        &net.a,
        &net.network,
        &main,
        "assign-child",
        WorkspaceCommand::AssignTask {
            task_id: "automatic-child".into(),
            expected_revision: 1,
            assignment_id: "automatic-child-assignment".into(),
            bot_id: "bot".into(),
            execution_node: local.clone(),
            bot_owner_epoch: 1,
            channel_owner_epoch: None,
            work_id: None,
            resource_roots: vec![],
            automatic: true,
        },
        5,
    )?;
    let assignment = origin
        .workspace_assignment(&net.network, "automatic-child-assignment")?
        .unwrap();
    assert_eq!(
        assignment.source_event.as_deref(),
        Some(cause.source_event.as_str())
    );
    assert_eq!(
        assignment.trigger_message_id.as_deref(),
        Some("coordinator")
    );
    assert_eq!(
        origin.workspace_queue(&net.network)?.len(),
        1,
        "the child has no separate user input"
    );
    let work = WorkSpec {
        work_id: "automatic-child-work".into(),
        cwd: "/workspace/child".into(),
        title: "child".into(),
        prompt: "child".into(),
        ..work_spec(
            &local,
            &format!("assignment:{}", assignment.assignment_id),
            "automatic-child-work",
            Provider::Codex,
        )
    };
    origin.accept_work_fixture(&net.a, &net.network, work.clone(), 6)?;
    let (_, receipt) = origin.apply_work_signal(
        &net.a,
        &net.network,
        &work.work_id,
        Signal::StartRejected(ExecFailure::new(
            FailureKind::Rejected,
            "native start rejected",
        )),
        7,
    )?;
    let receipt = receipt.context("child Work finish must admit its Task result")?;
    let result = origin.scheduler_job("c", &receipt.trigger)?.unwrap();
    assert_eq!(result.kind, TriggerKind::WorkResult);
    assert_eq!(
        result.scope,
        SessionScope::WorkspaceTask {
            task_id: "automatic-child".into()
        }
    );
    assert_eq!(result.reply_to, assignment.assignment_id);
    assert_eq!(
        workspace::workspace_assignment_for_job_from(
            &origin.connection,
            &net.network,
            &local,
            &result
        )?
        .unwrap()
        .assignment_id,
        assignment.assignment_id
    );
    origin.append_event(
        &net.a,
        &net.network,
        "c",
        bot_reply(&result.message_id, &result.reply_to),
        vec![],
        8,
    )?;
    assert_eq!(
        origin
            .main_messages(&net.network, "c")?
            .iter()
            .map(|message| message.message_id.as_str())
            .collect::<Vec<_>>(),
        ["coordinator"]
    );
    assert_eq!(
        origin
            .message(&net.network, "c", &result.message_id)?
            .unwrap()
            .target,
        Some(InputTarget::Task {
            task_id: "automatic-child".into()
        })
    );
    let unknown = WorkSpec {
        work_id: "unknown-assignment-work".into(),
        call_ref: "unknown-assignment-work".into(),
        trigger: "assignment:missing".into(),
        ..work
    };
    origin.accept_work_fixture(&net.a, &net.network, unknown.clone(), 9)?;
    assert!(
        origin
            .apply_work_signal(
                &net.a,
                &net.network,
                &unknown.work_id,
                Signal::StartRejected(ExecFailure::new(
                    FailureKind::Rejected,
                    "unbound work rejected"
                )),
                10
            )?
            .1
            .is_none()
    );
    assert!(
        origin
            .scheduler_job("c", &work_trigger(&unknown.work_id))?
            .is_none()
    );
    Ok(())
}

#[test]
fn a_replaced_assignment_cannot_reauthorize_its_running_work_result() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let local = net.a.device_id()?;
    let (_, first) = workspace_assigned_task_fixture(
        &net,
        &mut origin,
        "create",
        "task",
        "original",
        &["/workspace/a".into()],
        3,
    )?;
    let human = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "allocation-fence")?
        .unwrap()
        .job;
    let human_attempt = human.state.attempt().unwrap().id.clone();
    origin.mark_started("c", &human.trigger, &human_attempt)?;
    origin.settle_workspace_assignment(&net.a, &net.network, &human, 6)?;
    assert!(origin.manager_attempt_allowed_with_version(
        &net.network,
        &local,
        "c",
        &human.trigger,
        &human_attempt,
        human.state.attempt().unwrap().permission_version
    )?);
    origin.finish_job(
        &net.a,
        &net.network,
        "c",
        &human.trigger,
        &human_attempt,
        JobOutcome::Completed,
        "input complete",
        7,
    )?;
    let proof = origin
        .message(&net.network, "c", &human.message_id)?
        .unwrap()
        .event_hash;
    let work = WorkSpec {
        work_id: "fixed-result-work".into(),
        cwd: "/workspace/a".into(),
        title: "result".into(),
        prompt: "result".into(),
        ..work_spec(
            &local,
            &format!("assignment:{}", first.assignment_id),
            "fixed-result-work",
            Provider::Codex,
        )
    };
    origin.accept_work_fixture(&net.a, &net.network, work.clone(), 8)?;
    let (_, receipt) = origin.apply_work_signal(
        &net.a,
        &net.network,
        &work.work_id,
        Signal::StartRejected(ExecFailure::new(FailureKind::Rejected, "start rejected")),
        9,
    )?;
    let result_trigger = work_trigger(&work.work_id);
    assert_eq!(
        receipt.context("real finish admits its result")?.trigger,
        result_trigger
    );
    assert_eq!(
        origin
            .scheduler_job("c", &result_trigger)?
            .unwrap()
            .reply_to,
        "original"
    );
    let result = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "allocation-fence")?
        .unwrap()
        .job;
    assert_eq!(result.trigger, result_trigger);
    let result_attempt = result.state.attempt().unwrap().id.clone();
    origin.mark_started("c", &result.trigger, &result_attempt)?;
    assert!(
        origin.manager_attempt_allowed_with_version(
            &net.network,
            &local,
            "c",
            &result.trigger,
            &result_attempt,
            result.state.attempt().unwrap().permission_version
        )?,
        "a result turn keeps the real Work's fixed assignment"
    );
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "stop-original-allocation",
        WorkspaceCommand::AssignmentProgress {
            execution: None,
            assignment_id: first.assignment_id.clone(),
            revision: 1,
            state: AssignmentState::Stopped,
            input_event: None,
            evidence_event: Some(proof),
            result: None,
        },
        None,
        None,
        None,
        10,
    )?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "replacement",
        WorkspaceCommand::AssignTask {
            task_id: "task".into(),
            expected_revision: 1,
            assignment_id: "replacement".into(),
            bot_id: "bot".into(),
            execution_node: local.clone(),
            bot_owner_epoch: first.bot_owner_epoch,
            channel_owner_epoch: None,
            work_id: None,
            resource_roots: vec!["/workspace/b".into()],
            automatic: false,
        },
        11,
    )?;
    assert!(workspace::task_input_allowed_from(
        &origin.connection,
        &net.network,
        "task"
    )?);
    assert!(workspace::assignment_resources_available_from(
        &origin.connection,
        &net.network,
        &first
    )?);
    assert!(matches!(
        origin.scheduler_job("c", &result.trigger)?.unwrap().state,
        JobState::Running { .. }
    ));
    assert!(
        !origin.manager_attempt_allowed_with_version(
            &net.network,
            &local,
            "c",
            &result.trigger,
            &result_attempt,
            result.state.attempt().unwrap().permission_version
        )?,
        "new disjoint resources do not restore the old assignment's live input authority"
    );
    Ok(())
}

#[test]
fn workspace_release_retries_only_a_shared_proven_startup_failure_on_the_original_assignment()
-> Result<()> {
    use crate::scheduling::TurnExecution;
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let (input, assignment) =
        workspace_assigned_task_fixture(&net, &mut origin, "create", "task", "initial", &[], 3)?;
    let claimed = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "startup-retry")?
        .context("claimed job")?
        .job;
    let assertion = WorkspaceCommand::AssignmentProgress {
        assignment_id: assignment.assignment_id.clone(),
        revision: 1,
        state: AssignmentState::Stopped,
        execution: Some(TurnExecution::NotStarted),
        input_event: None,
        evidence_event: None,
        result: None,
    };
    let refused = origin
        .workspace_agent_command(
            &net.a,
            &net.network,
            &claimed,
            "agent-assertion",
            assertion,
            6,
        )
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("only Node lifecycle records execution evidence")
    );
    let attempt = claimed.state.attempt().context("attempt")?.id.clone();
    let failed = origin.finish_job(
        &net.a,
        &net.network,
        "c",
        &claimed.trigger,
        &attempt,
        JobOutcome::Failed(ExecFailure::new(
            FailureKind::MissingKey,
            "fixture has no configured key",
        )),
        "",
        7,
    )?;
    assert!(failed.retryable());
    assert!(
        !workspace::workspace_input_release_allowed_from(
            &origin.connection,
            &net.network,
            &net.a.device_id()?,
            "initial"
        )?,
        "a missing start record is not shared no-start proof"
    );
    origin.settle_workspace_assignment(&net.a, &net.network, &failed, 8)?;
    let stopped = origin
        .workspace_assignment(&net.network, &assignment.assignment_id)?
        .unwrap();
    assert_eq!(stopped.state, AssignmentState::Stopped);
    assert!(stopped.accepted_revision.is_none());
    assert!(workspace::workspace_input_release_allowed_from(
        &origin.connection,
        &net.network,
        &net.a.device_id()?,
        "initial"
    )?);
    let history = origin
        .workspace_history(&net.network, None, Some("task"), None, 100)?
        .events;
    let stop = history
        .iter()
        .find(|event| {
            matches!(
                &event.body,
                EventBody::WorkspaceCommanded {
                    command: WorkspaceCommand::AssignmentProgress {
                        state: AssignmentState::Stopped,
                        execution: Some(TurnExecution::NotStarted),
                        ..
                    },
                    ..
                }
            )
        })
        .context("shared not-started proof")?;
    let stop_hash = stop.event_hash.clone();
    let failed_reply = origin
        .message(&net.network, "c", &failed.message_id)?
        .unwrap();
    let mut replica = store(&net)?;
    sync(&origin, &mut replica, &net)?;
    let command = WorkspaceCommand::ControlInput {
        message_id: "initial".into(),
        expected_event: input.source_event.clone(),
        action: InputControl::Release,
    };
    let released =
        workspace_command_fixture(&net, &mut origin, "retry-original", command.clone(), 9)?;
    let release: Event = serde_json::from_str(
        &origin
            .event(&net.network, &released.event_hash)?
            .unwrap()
            .canonical_body,
    )?;
    assert!(
        release
            .prev
            .iter()
            .chain(&release.deps)
            .any(|hash| hash == &stop_hash)
    );
    assert!(
        release
            .prev
            .iter()
            .chain(&release.deps)
            .any(|hash| hash == &failed_reply.event_hash)
    );
    let queued = origin.scheduler_job("c", &claimed.trigger)?.unwrap();
    assert_eq!(queued.state, JobState::Queued);
    assert!(
        workspace::workspace_input_release_allowed_from(
            &origin.connection,
            &net.network,
            &net.a.device_id()?,
            "initial"
        )?,
        "the same committed Release remains valid inside its atomic apply"
    );
    assert_ne!(
        queued.message_id, failed.message_id,
        "an explicit retry keeps the old failed reply as history"
    );
    assert_eq!(
        assignment_state(&origin, &net, &assignment.assignment_id)?,
        AssignmentState::Proposed
    );
    assert_eq!(
        origin
            .workspace_task(&net.network, "task")?
            .unwrap()
            .current_assignment
            .as_deref(),
        Some(assignment.assignment_id.as_str())
    );
    sync(&origin, &mut replica, &net)?;
    assert_eq!(
        assignment_state(&replica, &net, &assignment.assignment_id)?,
        AssignmentState::Proposed
    );
    assert!(
        replica.scheduler_jobs_all()?.is_empty(),
        "importing Release cannot execute the retry"
    );
    assert!(workspace_command_fixture(&net, &mut origin, "retry-original", command, 10)?.duplicate);
    let retry = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "startup-retry")?
        .context("retry job")?
        .job;
    let retry_attempt = retry.state.attempt().context("retry attempt")?.id.clone();
    origin.update_turn_runtime(
        "c",
        &retry.trigger,
        &retry_attempt,
        vibeface_bridge::failure::ProviderStage::Run,
        TurnExecution::Started,
    )?;
    origin.settle_workspace_assignment(&net.a, &net.network, &retry, 11)?;
    let completed = origin.finish_job(
        &net.a,
        &net.network,
        "c",
        &retry.trigger,
        &retry_attempt,
        JobOutcome::Completed,
        "retry completed",
        12,
    )?;
    origin.settle_workspace_assignment(&net.a, &net.network, &completed, 13)?;
    assert_eq!(task_state(&origin, &net, "task")?, TaskState::Completed);
    assert_eq!(
        origin
            .message(&net.network, "c", &failed.message_id)?
            .unwrap()
            .status,
        Some(ReplyStatus::Failed)
    );
    assert_eq!(
        origin
            .message(&net.network, "c", &completed.message_id)?
            .unwrap()
            .status,
        Some(ReplyStatus::Completed)
    );
    Ok(())
}

#[test]
fn workspace_release_does_not_infer_no_start_or_undo_a_user_stop() -> Result<()> {
    use crate::workspace::*;
    for (explicit_proof, revise_before_release) in [(false, false), (true, false), (true, true)] {
        let (net, mut origin) = workspace_fixture()?;
        let (input, assignment) = workspace_assigned_task_fixture(
            &net,
            &mut origin,
            "create",
            "task",
            "initial",
            &[],
            3,
        )?;
        let job = origin
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "startup-retry")?
            .unwrap()
            .job;
        let attempt = job.state.attempt().unwrap().id.clone();
        let failed = origin.finish_job(
            &net.a,
            &net.network,
            "c",
            &job.trigger,
            &attempt,
            JobOutcome::Failed(ExecFailure::new(
                FailureKind::MissingKey,
                "fixture has no configured key",
            )),
            "",
            6,
        )?;
        if explicit_proof {
            origin.settle_workspace_assignment(&net.a, &net.network, &failed, 7)?;
            if revise_before_release {
                assert_eq!(
                    origin
                        .input_receipt(&net.network, &net.a.device_id()?, "initial")?
                        .context("safe before revision")?["retry_safe"],
                    true
                );
                workspace_command_fixture(
                    &net,
                    &mut origin,
                    "revision-race",
                    WorkspaceCommand::ReviseTask {
                        task_id: "task".into(),
                        expected_revision: 1,
                        request: "new revision".into(),
                        expected_result: "new result".into(),
                        dependencies: vec![],
                    },
                    8,
                )?;
                let receipt = origin
                    .input_receipt(&net.network, &net.a.device_id()?, "initial")?
                    .context("changed revision")?;
                assert_eq!(receipt["control_event"], input.source_event);
                assert_eq!(receipt["retry_safe"], false);
            } else {
                workspace_command_fixture(
                    &net,
                    &mut origin,
                    "stop",
                    WorkspaceCommand::ControlTask {
                        task_id: "task".into(),
                        expected_revision: 1,
                        action: TaskAction::Stop,
                    },
                    8,
                )?;
                workspace_command_fixture(
                    &net,
                    &mut origin,
                    "continue",
                    WorkspaceCommand::ControlTask {
                        task_id: "task".into(),
                        expected_revision: 1,
                        action: TaskAction::Continue,
                    },
                    9,
                )?;
            }
        } else {
            let reply = origin
                .message(&net.network, "c", &failed.message_id)?
                .unwrap();
            origin.apply_workspace_command(
                &net.a,
                &net.network,
                "stopped-without-classification",
                WorkspaceCommand::AssignmentProgress {
                    assignment_id: assignment.assignment_id.clone(),
                    revision: 1,
                    state: AssignmentState::Stopped,
                    execution: None,
                    input_event: None,
                    evidence_event: Some(reply.event_hash),
                    result: None,
                },
                None,
                None,
                None,
                7,
            )?;
        }
        assert!(!workspace::workspace_input_release_allowed_from(
            &origin.connection,
            &net.network,
            &net.a.device_id()?,
            "initial"
        )?);
        let before = origin.version_vector(&net.network)?;
        let refused = workspace_command_fixture(
            &net,
            &mut origin,
            "forbidden-retry",
            WorkspaceCommand::ControlInput {
                message_id: "initial".into(),
                expected_event: input.source_event,
                action: InputControl::Release,
            },
            10,
        )
        .expect_err("release qualification failed before append");
        assert_eq!(
            refused
                .downcast_ref::<WorkspaceCommandNotSaved>()
                .context("typed qualification rejection")?
                .command_id,
            "forbidden-retry"
        );
        assert!(!origin.workspace_command_recorded(&net.network, "forbidden-retry")?);
        assert_eq!(origin.version_vector(&net.network)?, before);
        assert_eq!(
            assignment_state(&origin, &net, &assignment.assignment_id)?,
            AssignmentState::Stopped
        );
        assert_eq!(
            origin.scheduler_job("c", &job.trigger)?.unwrap().state,
            failed.state
        );
    }
    Ok(())
}

#[test]
fn workspace_assignment_waits_for_native_work_and_the_last_real_summary() -> Result<()> {
    use crate::workspace::*;
    for (main_cause, work_first, reports, revise, answer_last) in [
        (false, false, true, false, false),
        (false, true, true, false, false),
        (true, false, true, false, false),
        (true, true, true, false, false),
        (false, false, false, false, false),
        (false, true, false, false, false),
        (true, false, false, false, false),
        (true, true, false, false, false),
        (false, false, true, true, false),
        (false, false, true, false, true),
    ] {
        let case = format!(
            "main_cause={main_cause} work_first={work_first} reports={reports} revise={revise} answer_last={answer_last}"
        );
        eprintln!("lifecycle case {case}");
        (|| -> Result<()> {
        let (net, mut origin) = workspace_fixture()?;
        let local = net.a.device_id()?;
        if !main_cause {
            workspace_command_fixture(
                &net,
                &mut origin,
                "create",
                workspace_task_command("task", None),
                3,
            )?;
        }
        let request = workspace_signed_input_fixture(
            &net,
            &mut origin,
            &net.a,
            InputRequest {
                message_id: "request".into(),
                route: InputRoute {
                    target: if main_cause {
                        InputTarget::BotMain {
                            bot_id: "bot".into(),
                        }
                    } else {
                        InputTarget::Task {
                            task_id: "task".into(),
                        }
                    },
                    intent: InputIntent::Continue,
                    sequence: 4,
                    expected_revision: (!main_cause).then_some(1),
                },
                text: "perform the task".into(),
                attachments: vec![],
                report_results: reports,
            },
            4,
        )?;
        origin.admit_workspace_input(&net.network, &local, "request")?;
        let assignment = if main_cause {
            let coordinator = workspace_started_job_fixture(&net, &mut origin, 5)?;
            let mut task = workspace_task_command("task", None);
            if let WorkspaceCommand::CreateTask { source_events, .. } = &mut task {
                source_events.push(request.source_event.clone());
            }
            origin.workspace_agent_command(
                &net.a,
                &net.network,
                &coordinator,
                "create-child",
                task,
                6,
            )?;
            origin.workspace_agent_command(
                &net.a,
                &net.network,
                &coordinator,
                "assign-child",
                WorkspaceCommand::AssignTask {
                    task_id: "task".into(),
                    expected_revision: 1,
                    assignment_id: "child-assignment".into(),
                    bot_id: "bot".into(),
                    execution_node: local.clone(),
                    bot_owner_epoch: 1,
                    channel_owner_epoch: None,
                    work_id: None,
                    resource_roots: vec!["/workspace".into()],
                    automatic: true,
                },
                7,
            )?;
            let ended = origin.finish_job(
                &net.a,
                &net.network,
                "c",
                &coordinator.trigger,
                &coordinator.state.attempt().unwrap().id,
                JobOutcome::Completed,
                "delegated the child",
                8,
            )?;
            origin.settle_workspace_assignment(&net.a, &net.network, &ended, 8)?;
            origin
                .workspace_assignment(&net.network, "child-assignment")?
                .unwrap()
        } else {
            origin
                .ensure_workspace_assignment(
                    &net.a,
                    &net.network,
                    "task",
                    &["/workspace".into()],
                    5,
                )?
                .unwrap()
        };
        let manager = workspace_started_job_fixture(&net, &mut origin, 9)?;
        assert_eq!(
            manager.scope,
            SessionScope::WorkspaceTask {
                task_id: "task".into()
            }
        );
        let work = WorkSpec {
            work_id: WorkSpec::id(
                "c",
                &format!("assignment:{}", assignment.assignment_id),
                "native",
            ),
            cwd: "/workspace".into(),
            title: "native task".into(),
            prompt: "perform the task".into(),
            ..work_spec(&local, &origin.scheduler_work_trigger(&net.network, &local, &manager)?, "native", Provider::Codex)
        };
        origin.accept_work_fixture(&net.a, &net.network, work.clone(), 10)?;
        assert!(origin.reserve_work_native_submission(&net.network, &work, &work.cwd, None)?);
        origin.apply_work_signal(
            &net.a,
            &net.network,
            &work.work_id,
            Signal::ThreadCreated("native-thread".into()),
            11,
        )?;
        origin.begin_reserved_work_native_input(&net.network, &work, "native-thread", &work.cwd)?;
        origin.apply_work_signal(
            &net.a,
            &net.network,
            &work.work_id,
            Signal::TurnStarted {
                turn_id: "native-turn".into(),
                epoch: 1,
            },
            12,
        )?;
        let finish_native = |origin: &mut Store,
                             uncertain: bool|
         -> Result<Option<crate::scheduling::AdmissionReceipt>> {
            let terminal = if uncertain {
                Signal::Probed(crate::work::Probe::Terminal {
                    turn_id: "native-turn".into(),
                    status: vibeface_bridge::turn::TurnStatus::Completed,
                    text: "native result".into(),
                    error: None,
                })
            } else {
                Signal::TurnCompleted {
                    turn_id: "native-turn".into(),
                    status: vibeface_bridge::turn::TurnStatus::Completed,
                    text: "native result".into(),
                    error: None,
                }
            };
            Ok(origin
                .apply_work_signal(&net.a, &net.network, &work.work_id, terminal, 20)?
                .1)
        };
        if work_first {
            assert_eq!(finish_native(&mut origin, false)?.is_some(), reports);
            assert!(
                origin
                    .claim_next_fixture(&net.a, &net.network, "c", "boot", "task-lifecycle")?
                    .is_none()
            );
        }
        let parent = origin.finish_job(
            &net.a,
            &net.network,
            "c",
            &manager.trigger,
            &manager.state.attempt().unwrap().id,
            JobOutcome::Completed,
            "work dispatched",
            13,
        )?;
        origin.settle_workspace_assignment(&net.a, &net.network, &parent, 14)?;
        let mut earlier = vec![parent];
        if !work_first || reports {
            assert_eq!(
                assignment_state(&origin, &net, &assignment.assignment_id)?,
                AssignmentState::Adopted
            );
            assert_eq!(
                task_state(&origin, &net, "task")?,
                TaskState::Running
            );
        }
        if !work_first {
            assert!(
                origin
                    .raw_work_steer_target(&net.network, &local, "c", &work.work_id)?
                    .is_some()
            );
            workspace_input_fixture(
                &net,
                &mut origin,
                "followup",
                Some("task"),
                InputIntent::Continue,
                15,
            )?;
            origin.admit_workspace_input(&net.network, &local, "followup")?;
            let queued = origin
                .scheduler_job("c", &message_trigger("followup"))?
                .unwrap();
            assert_eq!(
                workspace::workspace_assignment_for_job_from(
                    &origin.connection,
                    &net.network,
                    &local,
                    &queued
                )?
                .unwrap()
                .assignment_id,
                assignment.assignment_id
            );
            let followup = workspace_started_job_fixture(&net, &mut origin, 16)?;
            assert_eq!(followup.trigger, message_trigger("followup"));
            let old_reply = origin
                .message(&net.network, "c", &manager.message_id)?
                .context("completed dispatch reply")?;
            let recorded = origin.version_vector(&net.network)?;
            let refused = origin
                .workspace_agent_command(
                    &net.a,
                    &net.network,
                    &followup,
                    "borrow-old-reply",
                    WorkspaceCommand::AssignmentProgress {
                        execution: None,
                        assignment_id: assignment.assignment_id.clone(),
                        revision: 1,
                        state: AssignmentState::Completed,
                        input_event: None,
                        evidence_event: Some(old_reply.event_hash),
                        result: Some("ended before the native work".into()),
                    },
                    16,
                )
                .unwrap_err();
            assert!(
                refused
                    .to_string()
                    .contains("only Node lifecycle records execution evidence")
            );
            assert_eq!(origin.version_vector(&net.network)?, recorded);
            origin.workspace_agent_command(
                &net.a,
                &net.network,
                &followup,
                "confirm-adoption",
                WorkspaceCommand::AssignmentProgress {
                    execution: None,
                    assignment_id: assignment.assignment_id.clone(),
                    revision: 1,
                    state: AssignmentState::Adopted,
                    input_event: Some(assignment.event_hash.clone()),
                    evidence_event: None,
                    result: None,
                },
                16,
            )?;
            origin.workspace_agent_command(
                &net.a,
                &net.network,
                &followup,
                "submit-result",
                WorkspaceCommand::AssignmentProgress {
                    execution: None,
                    assignment_id: assignment.assignment_id.clone(),
                    revision: 1,
                    state: AssignmentState::Completed,
                    input_event: None,
                    evidence_event: None,
                    result: Some("submitted final result".into()),
                },
                16,
            )?;
            let pending = origin
                .workspace_assignment(&net.network, &assignment.assignment_id)?
                .unwrap();
            assert_eq!(pending.state, AssignmentState::Adopted);
            assert_eq!(
                pending.submitted_result.as_ref().unwrap().text,
                "submitted final result"
            );
            assert!(pending.result.is_none());
            assert_eq!(
                task_state(&origin, &net, "task")?,
                TaskState::Running
            );
            assert_eq!(
                origin.work(&net.network, &work.work_id)?.unwrap().state,
                WorkStatus::Running
            );
            assert!(matches!(
                origin.scheduler_job("c", &followup.trigger)?.unwrap().state,
                JobState::Running { .. }
            ));
            assert!(
                origin
                    .workspace_resource_conflicts(
                        &net.network,
                        &local,
                        &["/workspace".into()],
                        None,
                    )?
                    .contains(&assignment.assignment_id)
            );
            let mut pending_replica = store(&net)?;
            sync(&origin, &mut pending_replica, &net)?;
            assert_eq!(
                pending_replica.workspace_assignment(&net.network, &assignment.assignment_id)?.unwrap(),
                pending
            );
            assert_eq!(
                task_state(&pending_replica, &net, "task")?,
                TaskState::Running
            );
            assert!(pending_replica.scheduler_jobs_all()?.is_empty());
            let ended = origin.finish_job(
                &net.a,
                &net.network,
                "c",
                &followup.trigger,
                &followup.state.attempt().unwrap().id,
                JobOutcome::Completed,
                "followup recorded",
                17,
            )?;
            origin.settle_workspace_assignment(&net.a, &net.network, &ended, 18)?;
            earlier.push(ended);
            let uncertain = !main_cause && reports && !revise;
            if uncertain {
                origin.apply_work_signal(
                    &net.a,
                    &net.network,
                    &work.work_id,
                    Signal::Disconnected,
                    18,
                )?;
                origin.apply_work_signal(
                    &net.a,
                    &net.network,
                    &work.work_id,
                    Signal::Probed(crate::work::Probe::Missing),
                    19,
                )?;
                origin.settle_workspace_assignment(&net.a, &net.network, &earlier[0], 19)?;
                assert!(
                    origin
                        .raw_work_steer_target(&net.network, &local, "c", &work.work_id)?
                        .is_none()
                );
                assert_eq!(
                    assignment_state(&origin, &net, &assignment.assignment_id)?,
                    AssignmentState::Adopted
                );
                assert!(
                    !origin
                        .workspace_resource_conflicts(
                            &net.network,
                            &local,
                            &["/workspace".into()],
                            None
                        )?
                        .is_empty()
                );
            }
            assert_eq!(finish_native(&mut origin, uncertain)?.is_some(), reports);
        }
        if reports {
            for job in &earlier {
                origin.settle_workspace_assignment(&net.a, &net.network, job, 21)?;
            }
            assert_eq!(
                assignment_state(&origin, &net, &assignment.assignment_id)?,
                AssignmentState::Adopted
            );
            let result = workspace_started_job_fixture(&net, &mut origin, 22)?;
            assert_eq!(result.kind, TriggerKind::WorkResult);
            assert_eq!(
                result.reply_to,
                if main_cause {
                    assignment.assignment_id.clone()
                } else {
                    "request".into()
                }
            );
            if answer_last {
                let mut asked = question(&origin, &net, "c", "q-final", "fixture-result-session")?;
                if let EventBody::QuestionAsked { target, .. } = &mut asked {
                    *target = Some(InputTarget::Task { task_id: "task".into() });
                }
                origin.append_event(&net.a, &net.network, "c", asked, vec![], 23)?;
                origin.accept_answer(&net.a, &net.network, "q-final", "approved",
                    Some(answer_signature(&net, "c", "q-final", "approved")?), 24)?;
            }
            if revise {
                workspace_command_fixture(
                    &net,
                    &mut origin,
                    "revise",
                    WorkspaceCommand::ReviseTask {
                        task_id: "task".into(),
                        expected_revision: 1,
                        request: "revision two".into(),
                        expected_result: "revision two result".into(),
                        dependencies: vec![],
                    },
                    23,
                )?;
                workspace_input_fixture(
                    &net,
                    &mut origin,
                    "revision-two",
                    Some("task"),
                    InputIntent::Continue,
                    24,
                )?;
                origin.admit_workspace_input(&net.network, &local, "revision-two")?;
            }
            let summarized = origin.finish_job(
                &net.a,
                &net.network,
                "c",
                &result.trigger,
                &result.state.attempt().unwrap().id,
                JobOutcome::Completed,
                "actual final summary",
                25,
            )?;
            for job in &earlier {
                origin.settle_workspace_assignment(&net.a, &net.network, job, 26)?;
            }
            assert_eq!(
                assignment_state(&origin, &net, &assignment.assignment_id)?,
                AssignmentState::Adopted,
                "an older completed reply cannot steal the final settlement"
            );
            let final_job = if answer_last {
                origin.settle_workspace_assignment(&net.a, &net.network, &summarized, 27)?;
                assert_eq!(assignment_state(&origin, &net, &assignment.assignment_id)?, AssignmentState::Adopted);
                let answer = workspace_started_job_fixture(&net, &mut origin, 28)?;
                assert_eq!(answer.kind, TriggerKind::Answer);
                let answered = origin.finish_job(&net.a, &net.network, "c", &answer.trigger,
                    &answer.state.attempt().unwrap().id, JobOutcome::Completed, "answer final summary", 29)?;
                origin.settle_workspace_assignment(&net.a, &net.network, &summarized, 30)?;
                assert_eq!(assignment_state(&origin, &net, &assignment.assignment_id)?, AssignmentState::Adopted);
                answered
            } else {
                summarized
            };
            origin.settle_workspace_assignment(&net.a, &net.network, &final_job, 31)?;
            let final_assignment = origin
                .workspace_assignment(&net.network, &assignment.assignment_id)?
                .unwrap();
            if revise {
                assert_eq!(final_assignment.adopted_revision, Some(1));
                assert!(final_assignment.completed_revision.is_none());
                assert_eq!(
                    task_state(&origin, &net, "task")?,
                    TaskState::UpdatePending
                );
                assert!(
                    origin
                        .claim_next_fixture(&net.a, &net.network, "c", "boot", "task-lifecycle")?
                        .is_none()
                );
                return Ok(());
            }
            assert_eq!(
                final_assignment.result.as_deref(),
                Some(if !work_first {
                    "submitted final result"
                } else if answer_last {
                    "answer final summary"
                } else {
                    "actual final summary"
                })
            );
            assert_eq!(
                origin
                    .message(&net.network, "c", &final_job.message_id)?
                    .unwrap()
                    .target,
                Some(InputTarget::Task {
                    task_id: "task".into()
                })
            );
        } else {
            assert!(
                origin
                    .scheduler_job("c", &work_trigger(&work.work_id))?
                    .is_none()
            );
            for job in origin.scheduler_jobs_all()? {
                origin.settle_workspace_assignment(&net.a, &net.network, &job, 28)?;
            }
        }
        let final_assignment = origin
            .workspace_assignment(&net.network, &assignment.assignment_id)?
            .unwrap();
        assert_eq!(final_assignment.state, AssignmentState::Completed);
        assert_eq!(final_assignment.completed_revision, Some(1));
        assert_eq!(
            task_state(&origin, &net, "task")?,
            TaskState::Completed
        );
        let mut replica = store(&net)?;
        sync(&origin, &mut replica, &net)?;
        assert_eq!(replica.workspace_assignment(&net.network, &assignment.assignment_id)?.unwrap(), final_assignment);
        let private_binding: bool = replica.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM identity_meta WHERE key LIKE 'workspace-input-assignment:%')", [], |row| row.get(0))?;
        assert!(!private_binding, "shared terminal proofs do not require the owner's private binding");
        Ok(())
        })().with_context(|| case)?;
    }
    Ok(())
}

#[test]
fn an_owner_freezes_a_partial_dag_followup_to_its_first_assignment() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let local = net.a.device_id()?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "create",
        workspace_task_command("task", None),
        3,
    )?;
    let mut entry = store(&net)?;
    sync(&origin, &mut entry, &net)?;
    workspace_input_fixture(
        &net,
        &mut origin,
        "original",
        Some("task"),
        InputIntent::Continue,
        4,
    )?;
    let first = origin
        .ensure_workspace_assignment(&net.a, &net.network, "task", &[], 5)?
        .unwrap();
    let manager = workspace_started_job_fixture(&net, &mut origin, 6)?;
    assert!(
        entry
            .workspace_assignment(&net.network, &first.assignment_id)?
            .is_none()
    );
    workspace_signed_input_fixture(
        &net,
        &mut entry,
        &net.b,
        InputRequest {
            message_id: "partial-followup".into(),
            route: InputRoute {
                target: InputTarget::Task {
                    task_id: "task".into(),
                },
                intent: InputIntent::Continue,
                sequence: 7,
                expected_revision: Some(1),
            },
            text: "continue this task".into(),
            attachments: vec![],
            report_results: true,
        },
        7,
    )?;
    sync(&entry, &mut origin, &net)?;
    origin
        .admit_delivery(
            &local,
            &net.network,
            "c",
            &message_trigger("partial-followup"),
        )?
        .context("owner admission")?;
    let followup = origin
        .scheduler_job("c", &message_trigger("partial-followup"))?
        .unwrap();
    assert_eq!(
        workspace::workspace_assignment_for_job_from(
            &origin.connection,
            &net.network,
            &local,
            &followup
        )?
        .unwrap()
        .assignment_id,
        first.assignment_id
    );
    let ended = origin.finish_job(
        &net.a,
        &net.network,
        "c",
        &manager.trigger,
        &manager.state.attempt().unwrap().id,
        JobOutcome::Completed,
        "first turn ended",
        8,
    )?;
    origin.settle_workspace_assignment(&net.a, &net.network, &ended, 9)?;
    assert_eq!(
        assignment_state(&origin, &net, &first.assignment_id)?,
        AssignmentState::Adopted
    );
    let proof = origin
        .message(&net.network, "c", &ended.message_id)?
        .unwrap()
        .event_hash;
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "stop-first",
        WorkspaceCommand::AssignmentProgress {
            execution: None,
            assignment_id: first.assignment_id.clone(),
            revision: 1,
            state: AssignmentState::Stopped,
            input_event: None,
            evidence_event: Some(proof.clone()),
            result: Some("stopped".into()),
        },
        None,
        None,
        None,
        10,
    )?;
    workspace_input_fixture(
        &net,
        &mut origin,
        "replacement",
        Some("task"),
        InputIntent::Continue,
        11,
    )?;
    let replacement = origin
        .ensure_workspace_assignment(&net.a, &net.network, "task", &[], 12)?
        .unwrap();
    assert_ne!(replacement.assignment_id, first.assignment_id);
    let before = origin.version_vector(&net.network)?;
    let rejected = origin
        .apply_workspace_command(
            &net.a,
            &net.network,
            "borrow-old-reply",
            WorkspaceCommand::AssignmentProgress {
                execution: None,
                assignment_id: replacement.assignment_id.clone(),
                revision: 1,
                state: AssignmentState::Accepted,
                input_event: None,
                evidence_event: Some(proof),
                result: None,
            },
            None,
            None,
            None,
            13,
        )
        .expect_err("the old allocation's reply cannot prove acceptance of its replacement");
    assert!(format!("{rejected:#}").contains("assignment reply is outside this task revision"));
    assert_eq!(origin.version_vector(&net.network)?, before);
    assert_eq!(
        replacement.trigger_message_id.as_deref(),
        Some("replacement")
    );
    origin
        .admit_delivery(
            &local,
            &net.network,
            "c",
            &message_trigger("partial-followup"),
        )?
        .context("duplicate admission")?;
    let fixed = workspace::workspace_assignment_for_job_from(
        &origin.connection,
        &net.network,
        &local,
        &followup,
    )?
    .unwrap();
    assert_eq!(fixed.assignment_id, first.assignment_id);
    assert_eq!(fixed.state, AssignmentState::Stopped);
    assert!(!workspace::assignment_work_allowed_from(
        &origin.connection,
        &net.network,
        &fixed.assignment_id,
        &local,
        "c"
    )?);
    Ok(())
}
