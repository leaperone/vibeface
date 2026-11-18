use super::super::test_support::{
    adopt_initial_assignment, assignment_state, bot_conversation, net, say, second_bot, start_work,
    store, sync, work_spec, workspace_assigned_task_fixture, workspace_command_fixture,
    workspace_fixture, workspace_input_fixture, workspace_signed_input_fixture,
    workspace_task_command,
};
use super::super::*;
use crate::providers::Provider;
use crate::scheduling::{JobOutcome, JobState, TriggerKind, message_trigger};
use crate::work::{Signal, WorkSpec};
use vibeface_bridge::failure::{ExecFailure, FailureKind};

#[test]
fn workspace_queue_controls_preserve_signed_history_and_reject_started_or_unknown_inputs()
-> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let original =
        workspace_input_fixture(&net, &mut origin, "defer", None, InputIntent::Defer, 3)?;
    let edit = WorkspaceCommand::ControlInput {
        message_id: "defer".into(),
        expected_event: original.source_event.clone(),
        action: InputControl::Edit {
            text: "edited".into(),
            attachments: vec![],
        },
    };
    let edited = workspace_command_fixture(&net, &mut origin, "edit", edit, 4)?;
    let queue = origin
        .queued_workspace_input(&net.network, "defer")?
        .context("queued input")?;
    assert_eq!(queue.input.text, "edited");
    assert_eq!(queue.source_event, original.source_event);
    assert_eq!(queue.control_event, edited.event_hash);
    assert!(queue.deferred);
    assert_eq!(
        origin
            .workspace_input(&net.network, "defer")?
            .context("original")?
            .input
            .text,
        "input defer"
    );
    let before_cas = origin.version_vector(&net.network)?;
    let stale = workspace_command_fixture(
        &net,
        &mut origin,
        "stale-cas",
        WorkspaceCommand::ControlInput {
            message_id: "defer".into(),
            expected_event: original.source_event,
            action: InputControl::Release,
        },
        5,
    )
    .expect_err("stale controls are not appended");
    assert_eq!(
        stale
            .downcast_ref::<WorkspaceCommandNotSaved>()
            .context("typed CAS rejection")?
            .command_id,
        "stale-cas"
    );
    assert_eq!(origin.version_vector(&net.network)?, before_cas);
    assert!(!origin.workspace_command_recorded(&net.network, "stale-cas")?);
    workspace_command_fixture(
        &net,
        &mut origin,
        "release",
        WorkspaceCommand::ControlInput {
            message_id: "defer".into(),
            expected_event: queue.control_event,
            action: InputControl::Release,
        },
        6,
    )?;
    assert!(
        !origin
            .queued_workspace_input(&net.network, "defer")?
            .unwrap()
            .deferred
    );
    assert_eq!(origin.connection.execute("UPDATE scheduler_jobs SET state='reconciling',attempt_id='attempt' WHERE conversation_id='c' AND trigger='message:defer'",[])?, 1);
    let current = origin
        .queued_workspace_input(&net.network, "defer")?
        .unwrap();
    assert!(
        workspace_command_fixture(
            &net,
            &mut origin,
            "cannot-cancel",
            WorkspaceCommand::ControlInput {
                message_id: "defer".into(),
                expected_event: current.control_event,
                action: InputControl::Cancel
            },
            7
        )
        .is_err()
    );
    assert!(
        !origin
            .queued_workspace_input(&net.network, "defer")?
            .unwrap()
            .cancelled
    );
    assert_eq!(
        origin.connection.execute(
            "UPDATE scheduler_jobs SET state='running' WHERE trigger='message:defer'",
            [],
        )?,
        1
    );
    let current = origin
        .queued_workspace_input(&net.network, "defer")?
        .unwrap();
    assert!(
        workspace_command_fixture(
            &net,
            &mut origin,
            "cannot-edit",
            WorkspaceCommand::ControlInput {
                message_id: "defer".into(),
                expected_event: current.control_event,
                action: InputControl::Edit {
                    text: "unsafe rewrite".into(),
                    attachments: vec![]
                }
            },
            8
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn workspace_edit_missing_blobs_rolls_back_but_applied_duplicates_survive_blob_loss() -> Result<()>
{
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let input = workspace_input_fixture(
        &net,
        &mut origin,
        "deferred-file",
        None,
        InputIntent::Defer,
        3,
    )?;
    let bytes = b"new attachment";
    let attachment = Attachment {
        hash: crate::crypto::digest(bytes),
        size: bytes.len() as u64,
        name: "note.txt".into(),
    };
    let edit = WorkspaceCommand::ControlInput {
        message_id: "deferred-file".into(),
        expected_event: input.source_event.clone(),
        action: InputControl::Edit {
            text: "keep the whole file".into(),
            attachments: vec![attachment.clone()],
        },
    };
    let before = origin.version_vector(&net.network)?;
    assert!(workspace_command_fixture(&net, &mut origin, "edit-file", edit.clone(), 4).is_err());
    assert_eq!(origin.version_vector(&net.network)?, before);
    assert!(!origin.workspace_command_recorded(&net.network, "edit-file")?);
    assert_eq!(
        origin
            .queued_workspace_input(&net.network, "deferred-file")?
            .unwrap()
            .control_event,
        input.source_event
    );
    origin.put_blob(&attachment.blob(), bytes)?;
    let applied = workspace_command_fixture(&net, &mut origin, "edit-file", edit.clone(), 5)?;
    let current = origin
        .queued_workspace_input(&net.network, "deferred-file")?
        .unwrap();
    workspace_command_fixture(
        &net,
        &mut origin,
        "newer-edit",
        WorkspaceCommand::ControlInput {
            message_id: "deferred-file".into(),
            expected_event: current.control_event,
            action: InputControl::Edit {
                text: "newer contents".into(),
                attachments: vec![],
            },
        },
        6,
    )?;
    std::fs::remove_file(origin.blob_path(&attachment.hash)?)?;
    origin.connection.execute("INSERT INTO scheduler_jobs(conversation_id,trigger,kind,sequence,basis,reply_to,message_id,state,attempt_id,tool_calls) VALUES('c','message:deferred-file','human',1,'{}','deferred-file','reply','reconciling','attempt',0)",[])?;
    let duplicate = workspace_command_fixture(&net, &mut origin, "edit-file", edit, 7)?;
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.event_hash, applied.event_hash);
    let latest = origin
        .queued_workspace_input(&net.network, "deferred-file")?
        .unwrap();
    assert_eq!(latest.input.text, "newer contents");
    assert!(latest.input.attachments.is_empty());
    Ok(())
}

#[test]
fn workspace_active_input_fences_removed_channel_members_without_invalidating_old_work_proof()
-> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let local = net.a.device_id()?;
    origin.create_conversation(
        &net.a,
        &net.network,
        "other-c",
        "Other Bot",
        net.a.owner_fixture(&net.network, "other-c", &local)?,
        3,
    )?;
    let definition = net.a.fixture_admin().attest(
        &net.network,
        "other-c",
        EventBody::BotCreated {
            bot_id: "other".into(),
            name: "Other Bot".into(),
            instructions: "be brief".into(),
            project: None,
            attestation: None,
            cause: None,
        },
    )?;
    origin.append_event(&net.a, &net.network, "other-c", definition, vec![], 4)?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "channel",
        WorkspaceCommand::CreateChannel {
            channel_id: "channel".into(),
            title: "Channel".into(),
            purpose: "Test membership fence".into(),
            members: vec!["bot".into(), "other".into()],
            coordinator_bot: "other".into(),
            coordinator_node: local.clone(),
        },
        5,
    )?;
    let mut task = workspace_task_command("channel-task", None);
    if let WorkspaceCommand::CreateTask { channel_id, .. } = &mut task {
        *channel_id = Some("channel".into());
    }
    workspace_command_fixture(&net, &mut origin, "create-channel-task", task, 6)?;
    let input = workspace_input_fixture(
        &net,
        &mut origin,
        "channel-input",
        Some("channel-task"),
        InputIntent::Continue,
        7,
    )?;
    let assignment = origin
        .ensure_workspace_assignment(&net.a, &net.network, "channel-task", &[], 8)?
        .unwrap();
    adopt_initial_assignment(
        &mut origin,
        &net,
        &net.a,
        &assignment.assignment_id,
        input.source_event,
        ("channel-accepted", 9),
        ("channel-adopted", 10),
    )?;
    let (revision, refs) = workspace::assignment_work_source_from(
        &origin.connection,
        &net.network,
        &assignment.assignment_id,
    )?;
    assert!(workspace::task_input_allowed_from(
        &origin.connection,
        &net.network,
        "channel-task"
    )?);
    workspace_command_fixture(
        &net,
        &mut origin,
        "remove-member",
        WorkspaceCommand::ConfigureChannel {
            channel_id: "channel".into(),
            expected_revision: 1,
            title: "Channel".into(),
            purpose: "Test membership fence".into(),
            members: vec!["other".into()],
            coordinator_bot: "other".into(),
            coordinator_node: local.clone(),
            owner_epoch: 1,
        },
        11,
    )?;
    assert!(!workspace::task_input_allowed_from(
        &origin.connection,
        &net.network,
        "channel-task"
    )?);
    assert!(!workspace::assignment_work_allowed_from(
        &origin.connection,
        &net.network,
        &assignment.assignment_id,
        &local,
        "c"
    )?);
    assert!(
        workspace::assignment_work_source_matches_from(
            &origin.connection,
            &net.network,
            &assignment.assignment_id,
            revision,
            &refs
        )?,
        "the original accepted intent keeps its original membership proof"
    );
    Ok(())
}

#[test]
fn workspace_new_revision_replaces_only_a_proven_never_started_assignment() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let (_, original) =
        workspace_assigned_task_fixture(&net, &mut origin, "create", "task", "original", &[], 3)?;
    let original_job = origin
        .scheduler_job("c", &message_trigger("original"))?
        .context("original job")?;
    let original_work_trigger =
        origin.scheduler_work_trigger(&net.network, &net.a.device_id()?, &original_job)?;
    assert_eq!(
        original_work_trigger,
        format!("assignment:{}", original.assignment_id)
    );
    workspace_command_fixture(
        &net,
        &mut origin,
        "revision",
        WorkspaceCommand::ReviseTask {
            task_id: "task".into(),
            expected_revision: 1,
            request: "revision two".into(),
            expected_result: "r2 result".into(),
            dependencies: vec![],
        },
        6,
    )?;
    workspace_input_fixture(
        &net,
        &mut origin,
        "revised",
        Some("task"),
        InputIntent::Continue,
        7,
    )?;
    assert_eq!(
        origin
            .ensure_workspace_assignment(&net.a, &net.network, "task", &[], 8)?
            .unwrap()
            .assignment_id,
        original.assignment_id,
        "a projection change alone does not prove the old job stopped"
    );
    origin.admit_workspace_input(&net.network, &net.a.device_id()?, "revised")?;
    let revised_job = origin
        .scheduler_job("c", &message_trigger("revised"))?
        .context("queued revised job")?;
    assert!(
        workspace::workspace_assignment_for_job_from(
            &origin.connection,
            &net.network,
            &net.a.device_id()?,
            &revised_job
        )?
        .is_none(),
        "a new revision waits for a proven replacement instead of borrowing unstarted work"
    );
    let stopped = origin
        .request_manager_cancel(&net.a, &net.network, "c", &message_trigger("original"))?
        .context("queued original job")?;
    assert!(matches!(
        stopped.state,
        JobState::Cancelled { attempt: None, .. }
    ));
    assert_eq!(
        origin
            .message(&net.network, "c", &stopped.message_id)?
            .unwrap()
            .status,
        Some(ReplyStatus::Stopped)
    );
    let revised = origin
        .ensure_workspace_assignment(&net.a, &net.network, "task", &[], 9)?
        .context("revised assignment")?;
    assert_ne!(revised.assignment_id, original.assignment_id);
    assert_eq!(revised.started_revision, 2);
    assert_eq!(revised.trigger_message_id.as_deref(), Some("revised"));
    assert_eq!(
        origin.scheduler_work_trigger(&net.network, &net.a.device_id()?, &revised_job)?,
        format!("assignment:{}", revised.assignment_id)
    );
    assert_eq!(
        origin.scheduler_work_trigger(&net.network, &net.a.device_id()?, &original_job)?,
        original_work_trigger,
        "an old job keeps its original assignment after a replacement"
    );
    assert!(!workspace::assignment_work_allowed_from(
        &origin.connection,
        &net.network,
        &original.assignment_id,
        &net.a.device_id()?,
        "c"
    )?);
    assert_eq!(
        assignment_state(&origin, &net, &original.assignment_id)?,
        AssignmentState::Stopped
    );
    let (net, mut uncertain) = workspace_fixture()?;
    let (_, inflight) = workspace_assigned_task_fixture(
        &net,
        &mut uncertain,
        "create",
        "task",
        "inflight",
        &[],
        3,
    )?;
    let job = uncertain
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "inflight-revision")?
        .unwrap()
        .job;
    let attempt = job.state.attempt().unwrap().id.clone();
    uncertain.mark_started("c", &job.trigger, &attempt)?;
    uncertain.update_turn_runtime(
        "c",
        &job.trigger,
        &attempt,
        vibeface_bridge::failure::ProviderStage::TurnStart,
        crate::scheduling::TurnExecution::Unknown,
    )?;
    uncertain.settle_workspace_assignment(&net.a, &net.network, &job, 6)?;
    assert_eq!(
        assignment_state(&uncertain, &net, &inflight.assignment_id)?,
        AssignmentState::Proposed
    );
    workspace_command_fixture(
        &net,
        &mut uncertain,
        "revision",
        WorkspaceCommand::ReviseTask {
            task_id: "task".into(),
            expected_revision: 1,
            request: "revision two".into(),
            expected_result: "r2 result".into(),
            dependencies: vec![],
        },
        7,
    )?;
    workspace_input_fixture(
        &net,
        &mut uncertain,
        "inflight-revised",
        Some("task"),
        InputIntent::Continue,
        8,
    )?;
    uncertain.admit_workspace_input(&net.network, &net.a.device_id()?, "inflight-revised")?;
    let revised_job = uncertain
        .scheduler_job("c", &message_trigger("inflight-revised"))?
        .unwrap();
    assert_eq!(
        workspace::workspace_assignment_for_job_from(
            &uncertain.connection,
            &net.network,
            &net.a.device_id()?,
            &revised_job
        )?
        .unwrap()
        .assignment_id,
        inflight.assignment_id,
        "an in-flight unknown start cannot become a never-started replacement"
    );
    assert_eq!(
        uncertain
            .ensure_workspace_assignment(&net.a, &net.network, "task", &[], 9)?
            .unwrap()
            .assignment_id,
        inflight.assignment_id
    );
    assert!(!workspace::task_execution_allowed_from(
        &uncertain.connection,
        &net.network,
        "task"
    )?);
    Ok(())
}

#[test]
fn workspace_native_start_and_steering_recheck_current_assignment_authority() -> Result<()> {
    use crate::workspace::*;
    for started in [false, true] {
        let (net, mut origin) = workspace_fixture()?;
        let (_, assignment) =
            workspace_assigned_task_fixture(&net, &mut origin, "create", "task", "input", &[], 3)?;
        origin.apply_workspace_command(
            &net.a,
            &net.network,
            "accept",
            WorkspaceCommand::AssignmentProgress {
                execution: None,
                assignment_id: assignment.assignment_id.clone(),
                revision: 1,
                state: AssignmentState::Accepted,
                input_event: None,
                evidence_event: None,
                result: None,
            },
            None,
            None,
            None,
            6,
        )?;
        let local = net.a.device_id()?;
        let job = origin
            .scheduler_job("c", &message_trigger("input"))?
            .context("task job")?;
        let trigger = origin.scheduler_work_trigger(&net.network, &local, &job)?;
        let make = |call_ref: &str| WorkSpec {
            cwd: "/workspace".into(),
            title: "native work".into(),
            prompt: "perform the accepted task".into(),
            ..work_spec(&local, &trigger, call_ref, Provider::Codex)
        };
        let active = make(if started { "running" } else { "preparing" });
        let waiting = make("waiting");
        for work in [&active, &waiting] {
            origin.accept_work_fixture(&net.a, &net.network, work.clone(), 7)?;
        }
        assert!(origin.reserve_work_native_submission(&net.network, &active, &active.cwd, None)?);
        origin.apply_work_signal(
            &net.a,
            &net.network,
            &active.work_id,
            Signal::ThreadCreated("native-thread".into()),
            8,
        )?;
        if started {
            origin.begin_reserved_work_native_input(
                &net.network,
                &active,
                "native-thread",
                &active.cwd,
            )?;
            origin.apply_work_signal(
                &net.a,
                &net.network,
                &active.work_id,
                Signal::TurnStarted {
                    turn_id: "native-turn".into(),
                    epoch: 1,
                },
                9,
            )?;
            assert!(
                origin
                    .raw_work_steer_target(&net.network, &local, "c", &active.work_id)?
                    .is_some()
            );
        }
        workspace_command_fixture(
            &net,
            &mut origin,
            "revise",
            WorkspaceCommand::ReviseTask {
                task_id: "task".into(),
                expected_revision: 1,
                request: "revised request".into(),
                expected_result: "revised result".into(),
                dependencies: vec![],
            },
            10,
        )?;
        assert!(
            origin
                .reserve_work_native_submission(&net.network, &waiting, &waiting.cwd, None)
                .is_err()
        );
        assert!(!origin.work_native_submission_reserved(&waiting.work_id)?);
        if started {
            assert!(matches!(
                origin
                    .work_record(&active.work_id)?
                    .context("active work")?
                    .state,
                crate::work::WorkState::Running { .. }
            ));
            assert!(
                origin
                    .raw_work_steer_target(&net.network, &local, "c", &active.work_id)?
                    .is_none()
            );
        } else {
            assert!(
                origin
                    .begin_reserved_work_native_input(
                        &net.network,
                        &active,
                        "native-thread",
                        &active.cwd,
                    )
                    .is_err(),
                "a prepared process must recheck the task revision at its first input"
            );
        }
        assert!(origin.work_native_submission_reserved(&active.work_id)?);
        let terminal = if started {
            Signal::TurnCompleted {
                turn_id: "native-turn".into(),
                status: vibeface_bridge::turn::TurnStatus::Completed,
                text: "finished".into(),
                error: None,
            }
        } else {
            Signal::StartRejected(ExecFailure::new(
                FailureKind::Rejected,
                "controlled unsent fixture end",
            ))
        };
        origin.apply_work_signal(&net.a, &net.network, &active.work_id, terminal, 11)?;
        assert!(origin.work_native_resources_available(
            &net.network,
            &waiting.work_id,
            &waiting.cwd
        )?);
        assert!(
            origin
                .reserve_work_native_submission(&net.network, &waiting, &waiting.cwd, None)
                .is_err(),
            "free resources do not restore the old revision's authority"
        );
        assert!(!origin.work_native_submission_reserved(&waiting.work_id)?);
    }
    Ok(())
}

#[test]
fn only_a_new_signed_release_requeues_a_proven_not_started_input() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let input =
        workspace_input_fixture(&net, &mut origin, "retry", None, InputIntent::Continue, 3)?;
    origin.admit_workspace_input(&net.network, &net.a.device_id()?, "retry")?;
    let fail = |origin: &mut Store| -> Result<()> {
        let claimed = origin
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .context("claim")?;
        let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
        origin.finish_job(
            &net.a,
            &net.network,
            "c",
            &claimed.job.trigger,
            &attempt,
            JobOutcome::Failed(ExecFailure::new(FailureKind::Rejected, "not started")),
            "",
            4,
        )?;
        Ok(())
    };
    fail(&mut origin)?;
    assert_eq!(
        origin
            .input_receipt(&net.network, &net.a.device_id()?, "retry")?
            .context("failed receipt")?["retry_safe"],
        true
    );
    assert!(scheduler::workspace_input_releasable_from(
        &origin.connection,
        &net.network,
        "c",
        "retry"
    )?);
    assert!(!scheduler::workspace_input_editable_from(
        &origin.connection,
        &net.network,
        "c",
        "retry"
    )?);
    assert_eq!(
        origin
            .admit_workspace_input(&net.network, &net.a.device_id()?, "retry")?
            .state,
        "failed"
    );
    let release = WorkspaceCommand::ControlInput {
        message_id: "retry".into(),
        expected_event: input.source_event,
        action: InputControl::Release,
    };
    let failed_reply = origin
        .scheduler_job("c", &message_trigger("retry"))?
        .context("failed job")?
        .message_id;
    let receipt = workspace_command_fixture(&net, &mut origin, "release", release.clone(), 5)?;
    let queued = origin
        .scheduler_job("c", &message_trigger("retry"))?
        .context("queued")?;
    assert_eq!(queued.state, JobState::Queued);
    assert!(queued.runtime.is_none());
    assert_ne!(queued.message_id, failed_reply);
    assert_eq!(
        origin
            .message(&net.network, "c", &failed_reply)?
            .context("original failure")?
            .status,
        Some(ReplyStatus::Failed)
    );
    assert_eq!(
        origin
            .input_receipt(&net.network, &net.a.device_id()?, "retry")?
            .context("receipt")?["control_event"],
        receipt.event_hash
    );
    fail(&mut origin)?;
    assert!(workspace_command_fixture(&net, &mut origin, "release", release, 6)?.duplicate);
    scheduler::apply_workspace_input_control_from(
        &origin.connection,
        &net.network,
        &net.a.device_id()?,
        "retry",
    )?;
    assert_eq!(
        origin
            .scheduler_job("c", &message_trigger("retry"))?
            .context("same retry")?
            .message_id,
        queued.message_id
    );
    assert!(matches!(
        origin
            .scheduler_job("c", &message_trigger("retry"))?
            .context("still failed")?
            .state,
        JobState::Failed { .. }
    ));
    let cancelled = workspace_input_fixture(
        &net,
        &mut origin,
        "cancelled",
        None,
        InputIntent::Continue,
        7,
    )?;
    origin.admit_workspace_input(&net.network, &net.a.device_id()?, "cancelled")?;
    origin.request_manager_cancel(&net.a, &net.network, "c", &message_trigger("cancelled"))?;
    let receipt = origin
        .input_receipt(&net.network, &net.a.device_id()?, "cancelled")?
        .context("cancelled receipt")?;
    assert_eq!(receipt["stage"], "refused");
    assert_eq!(receipt["retry_safe"], false);
    assert_eq!(receipt["control_event"], cancelled.source_event);
    let deferred = workspace_input_fixture(
        &net,
        &mut origin,
        "cancel-without-job",
        None,
        InputIntent::Defer,
        8,
    )?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "cancel-unsent",
        WorkspaceCommand::ControlInput {
            message_id: "cancel-without-job".into(),
            expected_event: deferred.source_event,
            action: InputControl::Cancel,
        },
        9,
    )?;
    assert!(
        origin
            .scheduler_job("c", &message_trigger("cancel-without-job"))?
            .is_none()
    );
    let receipt = origin
        .input_receipt(&net.network, &net.a.device_id()?, "cancel-without-job")?
        .context("unsent cancelled receipt")?;
    assert_eq!(receipt["stage"], "refused");
    assert_eq!(receipt["retry_safe"], false);
    Ok(())
}

#[test]
fn a_reserved_native_input_is_consumed_once_and_a_held_stop_prevents_the_first_write() -> Result<()>
{
    let net = net()?;
    let mut origin = store(&net)?;
    bot_conversation(&mut origin, &net)?;
    say(&mut origin, &net, "source", true)?;
    let local = net.a.device_id()?;
    for (call_ref, stop) in [("write", false), ("cancelled", true)] {
        let spec = WorkSpec {
            cwd: "/work".into(),
            title: "work".into(),
            prompt: "perform work".into(),
            ..work_spec(&local, "message:source", call_ref, Provider::Pi)
        };
        origin.accept_work_fixture(&net.a, &net.network, spec.clone(), 3)?;
        assert!(origin.reserve_work_native_submission(&net.network, &spec, &spec.cwd, None)?);
        origin.apply_work_signal(
            &net.a,
            &net.network,
            &spec.work_id,
            Signal::ThreadCreated(call_ref.into()),
            4,
        )?;
        if stop {
            origin.save_stop_request(&crate::work::StopRequest {
                work_id: spec.work_id.clone(),
                conversation: "c".into(),
                target: local.clone(),
                requested_at_ms: 5,
                deadline_ms: 6,
                timed_out: false,
            })?;
            assert!(
                origin
                    .begin_reserved_work_native_input(&net.network, &spec, call_ref, &spec.cwd)
                    .is_err()
            );
        } else {
            let mut changed = spec.clone();
            changed.prompt = "another prompt".into();
            assert!(
                origin
                    .begin_reserved_work_native_input(&net.network, &changed, call_ref, &spec.cwd,)
                    .is_err()
            );
            assert!(
                origin
                    .begin_reserved_work_native_input(
                        &net.network,
                        &spec,
                        call_ref,
                        "/other-directory",
                    )
                    .is_err()
            );
            origin.begin_reserved_work_native_input(&net.network, &spec, call_ref, &spec.cwd)?;
            assert!(
                origin
                    .begin_reserved_work_native_input(&net.network, &spec, call_ref, &spec.cwd)
                    .is_err()
            );
        }
        assert!(origin.work_native_submission_reserved(&spec.work_id)?);
        origin.apply_work_signal(
            &net.a,
            &net.network,
            &spec.work_id,
            Signal::StartRejected(ExecFailure::new(FailureKind::Rejected, "controlled end")),
            7,
        )?;
    }
    Ok(())
}

#[test]
fn resource_waiting_inputs_keep_their_ids_and_share_the_first_assignment() -> Result<()> {
    use crate::scheduling::{QueueHoldReason, TurnExecution};
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let local = net.a.device_id()?;
    second_bot(&mut origin, &net, "Waiting Bot")?;
    workspace_input_fixture(&net, &mut origin, "writer", None, InputIntent::Continue, 3)?;
    let writer = WorkSpec {
        cwd: "/workspace/shared".into(),
        title: "existing writer".into(),
        prompt: "existing writer".into(),
        ..work_spec(
            &local,
            &message_trigger("writer"),
            "writer",
            Provider::Codex,
        )
    };
    origin.accept_work_fixture(&net.a, &net.network, writer.clone(), 4)?;
    start_work(
        &mut origin,
        &net,
        &net.a,
        &writer.work_id,
        ("writer-thread", 5),
        ("writer-turn", 6),
    )?;
    let mut task = workspace_task_command("waiting", None);
    if let WorkspaceCommand::CreateTask { owner_bot, .. } = &mut task {
        *owner_bot = "bot-d".into();
    }
    workspace_command_fixture(&net, &mut origin, "create-waiting", task, 7)?;
    let roots = vec!["/workspace/shared/sub".into()];
    for index in 0..crate::scheduling::MAX_PENDING_JOBS {
        let source = format!("inbox-fill-{index}");
        crate::store::scheduler::admit_from(
            &origin.connection,
            &net.network,
            "d",
            &message_trigger(&source),
            TriggerKind::Human,
            &source,
        )?;
    }
    workspace_signed_input_fixture(
        &net,
        &mut origin,
        &net.a,
        InputRequest {
            message_id: "full-refused".into(),
            route: InputRoute {
                target: InputTarget::Task {
                    task_id: "waiting".into(),
                },
                intent: InputIntent::Continue,
                sequence: 8,
                expected_revision: Some(1),
            },
            text: "full inbox".into(),
            attachments: vec![],
            report_results: true,
        },
        8,
    )?;
    assert!(
        origin
            .ensure_workspace_assignment(&net.a, &net.network, "waiting", &roots, 9)?
            .is_none()
    );
    let error = origin
        .admit_workspace_input(&net.network, &local, "full-refused")
        .expect_err("an occupied workspace does not bypass the admission capacity gate");
    assert!(error.to_string().contains("scheduler inbox is full"));
    assert!(
        origin
            .scheduler_job("d", &message_trigger("full-refused"))?
            .is_none()
    );
    let waiting: bool = origin.connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM identity_meta WHERE key=?)",
        [format!(
            "workspace-input-assignment:{}:full-refused",
            net.network
        )],
        |row| row.get(0),
    )?;
    assert!(
        !waiting,
        "a failed admission cannot leave a Waiting binding"
    );
    origin.refuse_workspace_input(&net.network, "full-refused", &error.to_string())?;
    assert_eq!(origin.connection.execute(
        "DELETE FROM scheduler_jobs WHERE conversation_id='d' AND trigger LIKE 'message:inbox-fill-%' AND state='queued' AND attempt_id IS NULL AND runtime IS NULL",
        [],
    )?, crate::scheduling::MAX_PENDING_JOBS);
    for (index, message_id) in ["m1", "m2", "refused"].into_iter().enumerate() {
        workspace_signed_input_fixture(
            &net,
            &mut origin,
            &net.a,
            InputRequest {
                message_id: message_id.into(),
                route: InputRoute {
                    target: InputTarget::Task {
                        task_id: "waiting".into(),
                    },
                    intent: InputIntent::Continue,
                    sequence: 9 + index as u64,
                    expected_revision: Some(1),
                },
                text: message_id.into(),
                attachments: vec![],
                report_results: true,
            },
            9 + index as i64,
        )?;
        if message_id == "refused" {
            origin.refuse_workspace_input(&net.network, message_id, "historical refusal")?;
        } else {
            assert!(
                origin
                    .ensure_workspace_assignment(&net.a, &net.network, "waiting", &roots, 11)?
                    .is_none()
            );
            let receipt = origin.admit_workspace_input(&net.network, &local, message_id)?;
            assert_eq!(receipt.state, "queued");
            assert!(
                origin
                    .ensure_workspace_assignment(&net.a, &net.network, "waiting", &roots, 12)?
                    .is_none()
            );
            let job = origin
                .scheduler_job("d", &message_trigger(message_id))?
                .unwrap();
            assert_eq!(job.state, JobState::Queued);
            assert!(job.runtime.is_none() && !job.tool_calls && !job.pending_check);
            assert!(
                workspace::workspace_assignment_for_job_from(
                    &origin.connection,
                    &net.network,
                    &local,
                    &job
                )?
                .is_none()
            );
            assert_eq!(
                origin
                    .input_receipt(&net.network, &local, message_id)?
                    .unwrap()["stage"],
                "owner_accepted"
            );
            assert_eq!(
                origin.scheduler_job_hold(
                    &net.network,
                    &local,
                    &job,
                    Provider::Codex,
                    &crate::config::SchedulingLimits::default()
                )?,
                Some(if message_id == "m1" {
                    QueueHoldReason::ResourceConflict
                } else {
                    QueueHoldReason::EarlierInput
                })
            );
        }
    }
    assert!(
        origin
            .claim_next_fixture(&net.a, &net.network, "d", "boot", "resource-wait")?
            .is_none()
    );
    assert!(
        origin
            .ensure_workspace_assignment(
                &net.a,
                &net.network,
                "waiting",
                &["/workspace/../invalid".into()],
                12
            )
            .is_err()
    );
    assert!(
        origin
            .ensure_workspace_assignment(&net.b, &net.network, "waiting", &roots, 12)
            .is_err()
    );
    assert!(
        workspace_command_fixture(
            &net,
            &mut origin,
            "explicit-busy",
            WorkspaceCommand::AssignTask {
                task_id: "waiting".into(),
                expected_revision: 1,
                assignment_id: "explicit-busy".into(),
                bot_id: "bot-d".into(),
                execution_node: local.clone(),
                bot_owner_epoch: 1,
                channel_owner_epoch: None,
                work_id: None,
                resource_roots: roots.clone(),
                automatic: false,
            },
            12
        )
        .is_err()
    );
    origin.apply_work_signal(
        &net.a,
        &net.network,
        &writer.work_id,
        Signal::Disconnected,
        13,
    )?;
    assert!(
        origin
            .ensure_workspace_assignment(&net.a, &net.network, "waiting", &roots, 14)?
            .is_none(),
        "unknown execution retains its resource"
    );
    origin.apply_work_signal(
        &net.a,
        &net.network,
        &writer.work_id,
        Signal::Probed(crate::work::Probe::Terminal {
            turn_id: "writer-turn".into(),
            status: vibeface_bridge::turn::TurnStatus::Completed,
            text: "writer done".into(),
            error: None,
        }),
        15,
    )?;
    let assignment = origin
        .ensure_workspace_assignment(&net.a, &net.network, "waiting", &roots, 16)?
        .context("released assignment")?;
    assert_eq!(assignment.started_revision, 1);
    for message_id in ["m1", "m2"] {
        let job = origin
            .scheduler_job("d", &message_trigger(message_id))?
            .unwrap();
        assert_eq!(
            workspace::workspace_assignment_for_job_from(
                &origin.connection,
                &net.network,
                &local,
                &job
            )?
            .unwrap()
            .assignment_id,
            assignment.assignment_id
        );
        let binding: String = origin.connection.query_row(
            "SELECT value FROM identity_meta WHERE key=?",
            [format!(
                "workspace-input-assignment:{}:{message_id}",
                net.network
            )],
            |row| row.get(0),
        )?;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&binding)?["state"],
            "bound"
        );
    }
    let mut original_jobs = Vec::new();
    for (index, expected) in ["m1", "m2"].into_iter().enumerate() {
        let job = origin
            .claim_next_fixture(&net.a, &net.network, "d", "boot", "resource-wait")?
            .context("claim released input")?
            .job;
        assert_eq!(job.reply_to, expected);
        let attempt = job.state.attempt().unwrap().id.clone();
        origin.mark_started("d", &job.trigger, &attempt)?;
        origin.update_turn_runtime(
            "d",
            &job.trigger,
            &attempt,
            vibeface_bridge::failure::ProviderStage::Run,
            TurnExecution::Started,
        )?;
        origin.settle_workspace_assignment(&net.a, &net.network, &job, 17 + index as i64)?;
        let ended = origin.finish_job(
            &net.a,
            &net.network,
            "d",
            &job.trigger,
            &attempt,
            JobOutcome::Completed,
            expected,
            20 + index as i64,
        )?;
        origin.settle_workspace_assignment(&net.a, &net.network, &ended, 22 + index as i64)?;
        assert_eq!(
            assignment_state(&origin, &net, &assignment.assignment_id)?,
            if index == 0 {
                AssignmentState::Adopted
            } else {
                AssignmentState::Completed
            }
        );
        assert_eq!(
            origin
                .admit_workspace_input(&net.network, &local, expected)?
                .state,
            "completed"
        );
        original_jobs.push(ended);
    }
    assert!(
        origin
            .claim_next_fixture(&net.a, &net.network, "d", "boot", "resource-wait")?
            .is_none()
    );
    assert!(matches!(
        origin
            .scheduler_job("d", &message_trigger("refused"))?
            .unwrap()
            .state,
        JobState::Cancelled { attempt: None, .. }
    ));
    assert_eq!(
        origin
            .input_receipt(&net.network, &local, "refused")?
            .unwrap()["reason"],
        "historical refusal"
    );
    assert!(matches!(
        origin
            .scheduler_job("d", &message_trigger("full-refused"))?
            .unwrap()
            .state,
        JobState::Cancelled { attempt: None, .. }
    ));
    let waiting: bool = origin.connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM identity_meta WHERE key=?)",
        [format!(
            "workspace-input-assignment:{}:full-refused",
            net.network
        )],
        |row| row.get(0),
    )?;
    assert!(
        !waiting,
        "the capacity refusal is never absorbed by a later assignment"
    );
    workspace_signed_input_fixture(
        &net,
        &mut origin,
        &net.a,
        InputRequest {
            message_id: "replacement".into(),
            route: InputRoute {
                target: InputTarget::Task {
                    task_id: "waiting".into(),
                },
                intent: InputIntent::Continue,
                sequence: 24,
                expected_revision: Some(1),
            },
            text: "replacement".into(),
            attachments: vec![],
            report_results: true,
        },
        24,
    )?;
    let replacement = origin
        .ensure_workspace_assignment(&net.a, &net.network, "waiting", &roots, 25)?
        .unwrap();
    assert_ne!(replacement.assignment_id, assignment.assignment_id);
    for job in original_jobs {
        assert_eq!(
            workspace::workspace_assignment_for_job_from(
                &origin.connection,
                &net.network,
                &local,
                &job
            )?
            .unwrap()
            .assignment_id,
            assignment.assignment_id,
            "bound inputs never borrow the replacement"
        );
        let replies: u64 = origin.connection.query_row("SELECT COUNT(*) FROM messages WHERE network_id=? AND conversation_id='d' AND in_reply_to=? AND role='assistant'", params![net.network, job.reply_to], |row| row.get(0))?;
        assert_eq!(replies, 1);
    }
    Ok(())
}

#[test]
fn waiting_inputs_do_not_borrow_a_replacement_after_the_owner_returns() -> Result<()> {
    use crate::workspace::*;
    for assigned_by_b in [false, true] {
        let (net, mut a) = workspace_fixture()?;
        let local = net.a.device_id()?;
        workspace_command_fixture(
            &net,
            &mut a,
            "create",
            workspace_task_command("task", None),
            3,
        )?;
        for (at, message_id) in [(4, "waiting-first"), (5, "waiting-second")] {
            workspace_input_fixture(
                &net,
                &mut a,
                message_id,
                Some("task"),
                InputIntent::Continue,
                at,
            )?;
            assert_eq!(
                a.admit_workspace_input(&net.network, &local, message_id)?
                    .state,
                "queued"
            );
        }
        let transfer = net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::ConversationOwner {
                scheduler_node: net.b.device_id()?,
                epoch: 2,
                attestation: None,
                cause: None,
            },
        )?;
        a.append_event(&net.a, &net.network, "c", transfer, vec![], 6)?;
        let mut b = store(&net)?;
        sync(&a, &mut b, &net)?;
        let first = if assigned_by_b {
            let first = b
                .ensure_workspace_assignment(&net.b, &net.network, "task", &[], 7)?
                .unwrap();
            let job = b
                .claim_next_fixture(&net.b, &net.network, "c", "boot-b", "owner-wait")?
                .unwrap()
                .job;
            let attempt = job.state.attempt().unwrap().id.clone();
            b.mark_started("c", &job.trigger, &attempt)?;
            let ended = b.finish_job(
                &net.b,
                &net.network,
                "c",
                &job.trigger,
                &attempt,
                JobOutcome::Completed,
                "owner B finished",
                8,
            )?;
            b.settle_workspace_assignment(&net.b, &net.network, &ended, 9)?;
            assert_eq!(
                assignment_state(&b, &net, &first.assignment_id)?,
                AssignmentState::Completed
            );
            Some(first)
        } else {
            None
        };
        sync(&b, &mut a, &net)?;
        let transfer = net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::ConversationOwner {
                scheduler_node: local.clone(),
                epoch: 3,
                attestation: None,
                cause: None,
            },
        )?;
        a.append_event(&net.a, &net.network, "c", transfer, vec![], 10)?;
        let execution_head: Option<String> = if assigned_by_b {
            a.connection.query_row(
                "SELECT hash FROM events WHERE network_id=? AND scope_id=? AND author=? AND effective=1 ORDER BY seq DESC LIMIT 1",
                params![net.network, WORKSPACE_SCOPE, net.b.device_id()?],
                |row| row.get(0),
            ).optional()?
        } else {
            None
        };
        let replacement_input = workspace_input_fixture(
            &net,
            &mut a,
            "replacement",
            Some("task"),
            InputIntent::Continue,
            11,
        )?;
        if let Some(head) = execution_head {
            let source =
                log::event_from(&a.connection, &net.network, &replacement_input.source_event)?
                    .unwrap();
            assert!(
                source.deps.contains(&head),
                "a new input must causally know the remote assignment's terminal progress"
            );
        }
        let replacement = a
            .ensure_workspace_assignment(&net.a, &net.network, "task", &[], 12)?
            .unwrap();
        if let Some(first) = &first {
            assert_ne!(replacement.assignment_id, first.assignment_id);
        }
        assert_eq!(
            replacement.trigger_message_id.as_deref(),
            Some("replacement")
        );
        for message_id in ["waiting-first", "waiting-second"] {
            let binding: String = a.connection.query_row(
                "SELECT value FROM identity_meta WHERE key=?",
                [format!(
                    "workspace-input-assignment:{}:{message_id}",
                    net.network
                )],
                |row| row.get(0),
            )?;
            let binding: serde_json::Value = serde_json::from_str(&binding)?;
            if let Some(first) = &first {
                assert_eq!(binding["state"], "bound");
                assert_eq!(binding["assignment_id"], first.assignment_id);
            } else {
                assert_eq!(binding["state"], "waiting");
                assert_eq!(binding["owner_epoch"], 1);
            }
            let job = a.scheduler_job("c", &message_trigger(message_id))?.unwrap();
            assert_eq!(job.state, JobState::Queued);
            assert!(job.runtime.is_none());
            assert!(
                workspace::workspace_assignment_for_job_from(
                    &a.connection,
                    &net.network,
                    &local,
                    &job
                )?
                .is_none()
            );
            assert_eq!(
                a.scheduler_job_hold(
                    &net.network,
                    &local,
                    &job,
                    Provider::Codex,
                    &crate::config::SchedulingLimits::default()
                )?,
                Some(if message_id == "waiting-first" {
                    crate::scheduling::QueueHoldReason::PendingCheck
                } else {
                    crate::scheduling::QueueHoldReason::EarlierInput
                })
            );
        }
        let replacement_job = a
            .claim_next_fixture(
                &net.a,
                &net.network,
                "c",
                "boot-a",
                "owner-return-replacement",
            )?
            .context("old waiting inputs must not block the current assignment")?
            .job;
        assert_eq!(replacement_job.reply_to, "replacement");
        assert_eq!(
            workspace::workspace_assignment_for_job_from(
                &a.connection,
                &net.network,
                &local,
                &replacement_job,
            )?
            .unwrap()
            .assignment_id,
            replacement.assignment_id
        );
    }
    Ok(())
}

#[test]
fn an_owner_rechecks_new_input_revisions_without_rewriting_existing_receipts() -> Result<()> {
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
    for (at, message_id) in [(4, "accepted"), (5, "stale")] {
        workspace_input_fixture(
            &net,
            &mut origin,
            message_id,
            Some("task"),
            InputIntent::Continue,
            at,
        )?;
    }
    let receipt = origin.admit_workspace_input(&net.network, &local, "accepted")?;
    let accepted = origin
        .scheduler_job("c", &message_trigger("accepted"))?
        .unwrap();
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
        6,
    )?;
    assert_eq!(
        origin.admit_workspace_input(&net.network, &local, "accepted")?,
        receipt
    );
    assert_eq!(
        origin
            .scheduler_job("c", &message_trigger("accepted"))?
            .unwrap(),
        accepted
    );
    let refused = origin.admit_workspace_input(&net.network, &local, "stale")?;
    assert_eq!(refused.state, "cancelled");
    assert!(matches!(
        origin.scheduler_job("c", &message_trigger("stale"))?.context("revision refusal")?.state,
        JobState::Cancelled { attempt: None, evidence } if evidence == "input task revision changed"
    ));
    assert_eq!(
        origin.admit_workspace_input(&net.network, &local, "stale")?,
        refused
    );
    assert!(
        origin
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "stale-revision")?
            .is_none()
    );
    Ok(())
}
