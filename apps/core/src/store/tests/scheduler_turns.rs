use super::super::test_support::{
    Net, all_accepted, bot_conversation, bot_with_message, conversation, net, owner, question, say,
    signed_answer, signed_user, store, sync, user,
};
use super::super::*;
use crate::providers::Provider;
use crate::scheduling::{
    JobOutcome, JobState, SchedulerJob, SteerOutcome, TriggerKind, message_trigger,
};
use vibeface_bridge::failure::{ExecFailure, FailureKind};

#[test]
fn the_same_message_id_queues_one_job() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    let (_, first) = store.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m1", "hi", true)?,
        2,
    )?;
    let (_, second) = store.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m1", "hi", true)?,
        3,
    )?;
    assert_eq!(first, second);
    assert!(
        store
            .post_input_fixture(
                &net.a,
                &net.network,
                signed_user(&net, "m1", "other", true)?,
                4
            )
            .is_err()
    );
    assert_eq!(store.scheduler_jobs("c")?.len(), 1);
    assert_eq!(store.messages(&net.network, "c")?.len(), 1);
    Ok(())
}

#[test]
fn stopped_and_interrupted_turns_leave_a_status_reply() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    store.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m1", "first", true)?,
        2,
    )?;
    store.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m2", "second", true)?,
        3,
    )?;
    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot-1", "cfg")?
        .context("job")?;
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    store.mark_started("c", &claimed.job.trigger, &attempt)?;
    store.request_manager_cancel(&net.a, &net.network, "c", &claimed.job.trigger)?;
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &claimed.job.trigger,
        &attempt,
        JobOutcome::Cancelled("user".into()),
        "partial",
        4,
    )?;
    let reply = store
        .message(&net.network, "c", &claimed.job.message_id)?
        .context("reply")?;
    assert_eq!(
        (
            reply.status,
            reply.text.as_str(),
            reply.in_reply_to.as_deref()
        ),
        (Some(ReplyStatus::Stopped), "partial", Some("m1"))
    );

    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot-1", "cfg")?
        .context("job")?;
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    store.mark_started("c", &claimed.job.trigger, &attempt)?;
    store.recover_scheduler_jobs(&net.a, &net.network, "boot-2")?;
    assert!(
        store
            .message(&net.network, "c", &claimed.job.message_id)?
            .is_none(),
        "an unknown restart has no terminal reply"
    );
    assert_eq!(store.pending_checks()?.len(), 1);
    assert!(matches!(
        store
            .scheduler_job("c", &message_trigger("m2"))?
            .context("job")?
            .state,
        JobState::Reconciling { .. }
    ));
    let check = store
        .pending_checks()?
        .into_iter()
        .find(|check| check.attempt_id == attempt)
        .context("pending check")?;
    store.confirm_manager_stopped(&net.a, &net.network, &check, 5)?;
    let reply = store
        .message(&net.network, "c", &claimed.job.message_id)?
        .context("confirmed reply")?;
    assert_eq!(reply.status, Some(ReplyStatus::Interrupted));
    assert_eq!(store.pending_checks()?.len(), 0);
    assert!(matches!(
        store
            .scheduler_job("c", &message_trigger("m2"))?
            .context("job")?
            .state,
        JobState::Interrupted { .. }
    ));
    Ok(())
}

#[test]
fn confirming_a_stopped_main_turn_skips_a_work_of_unknown_scope() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let mut unknown = question(&store, &net, "c", "q-unknown", "external-thread")?;
    if let EventBody::QuestionAsked { target, .. } = &mut unknown {
        *target = None;
    }
    store.ask(&net.a, &net.network, "c", unknown, 4)?;
    let answer = signed_answer(&net, "c", "q-unknown", "go on")?;
    store.append_event(&net.a, &net.network, "c", answer, vec![], 5)?;
    let dispatched = EventBody::WorkDispatched {
        work_id: "work-unknown-scope".into(),
        trigger: message_trigger("a-q-unknown"),
        dispatcher: net.a.device_id()?,
        call_ref: "a-q-unknown".into(),
        kind: WorkKind::Task,
        cwd: "/w".into(),
        title: "unknown".into(),
        prompt: "p".into(),
        continues: None,
        parent: None,
        thread: None,
        model: None,
        resolved: ResolvedModel {
            provider: Provider::Codex,
            model: None,
        },
    };
    store.append_event(&net.a, &net.network, "c", dispatched, vec![], 6)?;
    assert_eq!(
        store.trigger_input_target(&net.network, "c", &message_trigger("a-q-unknown"))?,
        None
    );

    store.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m2", "status?", true)?,
        7,
    )?;
    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot-1", "cfg")?
        .context("job")?;
    assert_eq!(claimed.job.trigger, message_trigger("m2"));
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    store.mark_started("c", &claimed.job.trigger, &attempt)?;
    store.recover_scheduler_jobs(&net.a, &net.network, "boot-2")?;
    let check = store
        .pending_checks()?
        .into_iter()
        .find(|check| check.attempt_id == attempt)
        .context("pending check")?;
    store.confirm_manager_stopped(&net.a, &net.network, &check, 8)?;
    assert_eq!(
        store
            .message(&net.network, "c", &claimed.job.message_id)?
            .context("confirmed reply")?
            .status,
        Some(ReplyStatus::Interrupted)
    );
    assert!(store.pending_checks()?.is_empty());
    assert!(matches!(
        store
            .scheduler_job("c", &message_trigger("m2"))?
            .context("job")?
            .state,
        JobState::Interrupted { .. }
    ));
    Ok(())
}

#[test]
fn a_reply_stopped_before_it_started_is_recorded_once() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    say(&mut store, &net, "m1", true)?;
    let trigger = message_trigger("m1");
    let job = store.scheduler_job("c", &trigger)?.context("job")?;
    for _ in 0..2 {
        store.request_manager_cancel(&net.a, &net.network, "c", &trigger)?;
    }
    assert!(matches!(
        store.scheduler_job("c", &trigger)?.context("job")?.state,
        JobState::Cancelled { attempt: None, .. }
    ));
    let replies: Vec<_> = store
        .messages(&net.network, "c")?
        .into_iter()
        .filter(|message| message.in_reply_to.as_deref() == Some("m1"))
        .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(
        (replies[0].message_id.as_str(), replies[0].status),
        (job.message_id.as_str(), Some(ReplyStatus::Stopped))
    );
    Ok(())
}

#[test]
fn a_moved_conversation_hands_queued_turns_to_its_new_owner() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    store.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m1", "go", true)?,
        2,
    )?;
    let moved = net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::ConversationOwner {
            scheduler_node: net.b.device_id()?,
            epoch: 2,
            attestation: None,
            cause: None,
        },
    )?;
    store.append_event(&net.a, &net.network, "c", moved, vec![], 3)?;
    assert!(
        store
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .is_none()
    );
    let trigger = message_trigger("m1");
    assert!(store.scheduler_job("c", &trigger)?.is_none());
    let outbox = store.scheduler_outbox()?;
    assert_eq!(
        outbox.iter().map(|e| e.trigger.clone()).collect::<Vec<_>>(),
        vec![trigger]
    );
    Ok(())
}

fn answered(store: &mut Store, net: &Net, at: i64) -> Result<SchedulerJob> {
    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let trigger = claimed.job.trigger.clone();
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    store.mark_started("c", &trigger, &attempt)?;
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &attempt,
        JobOutcome::Completed,
        "done",
        at,
    )
}

fn replied_elsewhere(store: &mut Store, net: &Net, identity: &LocalIdentity) -> Result<()> {
    assert!(
        store
            .claim_next_fixture(identity, &net.network, "c", "boot", "cfg")?
            .is_none(),
        "a message answered in the shared record does not run again"
    );
    let job = store
        .scheduler_job("c", &message_trigger("m1"))?
        .context("job")?;
    assert!(
        matches!(&job.state, JobState::Cancelled { evidence, .. } if evidence == "已在别处回复"),
        "{:?}",
        job.state
    );
    let replies = store
        .messages(&net.network, "c")?
        .into_iter()
        .filter(|m| m.in_reply_to.as_deref() == Some("m1"))
        .count();
    assert_eq!(replies, 1);
    Ok(())
}

#[test]
fn a_restored_backup_does_not_rerun_a_turn_answered_since() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    bot_conversation(&mut origin, &net)?;
    origin.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m1", "go", true)?,
        2,
    )?;
    let dir = std::env::temp_dir()
        .canonicalize()?
        .join(format!("vibeface-restore-{}", uuid::Uuid::new_v4()));
    crate::config::private_dir(&dir)?;
    let backup = dir.join("backup.sqlite3");
    origin
        .connection
        .execute("VACUUM INTO ?", [backup.to_str().context("path")?])?;
    answered(&mut origin, &net, 3)?;

    let mut restored = Store::open(&backup)?;
    let events = origin.events_from(&net.network, &net.a.device_id()?, 1, 500)?;
    restored.ingest_events(&net.network, &events)?;
    let result = replied_elsewhere(&mut restored, &net, &net.a);
    drop(restored);
    std::fs::remove_dir_all(dir)?;
    result
}

#[test]
fn a_new_owner_does_not_rerun_a_delivery_the_former_owner_answered() -> Result<()> {
    let net = net()?;
    let mut former = store(&net)?;
    bot_conversation(&mut former, &net)?;
    former.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m1", "go", true)?,
        2,
    )?;
    answered(&mut former, &net, 3)?;

    let mut next = store(&net)?;
    all_accepted(&mut next, &net, &former)?;
    next.append_event(
        &net.b,
        &net.network,
        "c",
        owner(&net, &net.b, 2)?,
        vec![],
        4,
    )?;
    let receipt = next
        .admit_delivery(
            &net.b.device_id()?,
            &net.network,
            "c",
            &message_trigger("m1"),
        )?
        .context("the entry retries the delivery whose receipt was lost")?;
    assert_eq!(receipt.state, "queued");
    replied_elsewhere(&mut next, &net, &net.b)
}

#[test]
fn a_move_fences_the_old_owners_running_turn() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    let local = net.a.device_id()?;
    store.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m1", "go", true)?,
        2,
    )?;
    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let trigger = claimed.job.trigger.clone();
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    let permission_version = claimed
        .job
        .state
        .attempt()
        .context("attempt")?
        .permission_version;
    store.mark_started("c", &trigger, &attempt)?;
    assert!(store.manager_attempt_allowed_with_version(
        &net.network,
        &local,
        "c",
        &trigger,
        &attempt,
        permission_version
    )?);
    assert!(store.fence_moved_turns(&net.network, &local)?.is_empty());

    store.append_event(
        &net.a,
        &net.network,
        "c",
        owner(&net, &net.b, 2)?,
        vec![],
        3,
    )?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        owner(&net, &net.a, 3)?,
        vec![],
        4,
    )?;
    assert!(!store.manager_attempt_allowed_with_version(
        &net.network,
        &local,
        "c",
        &trigger,
        &attempt,
        permission_version
    )?);
    assert_eq!(
        store.fence_moved_turns(&net.network, &local)?,
        vec![("c".to_owned(), trigger.clone(), attempt.clone())]
    );
    assert!(matches!(
        store.scheduler_job("c", &trigger)?.context("job")?.state,
        JobState::Cancelling { .. }
    ));
    assert!(store.fence_moved_turns(&net.network, &local)?.is_empty());
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &attempt,
        JobOutcome::Cancelled("会话已移到另一台设备".into()),
        "partial",
        5,
    )?;
    let reply = store
        .message(&net.network, "c", &claimed.job.message_id)?
        .context("reply")?;
    assert_eq!(reply.status, Some(ReplyStatus::Stopped));
    Ok(())
}

#[test]
fn a_failed_turn_shares_its_kind_and_keeps_the_raw_detail_local() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    let raw = "CURSOR_API_KEY is required for the Cursor SDK manager";
    let failure = ExecFailure::new(FailureKind::MissingKey, raw);
    for (id, outcome, status) in [
        (
            "m1",
            JobOutcome::Failed(failure.clone()),
            ReplyStatus::Failed,
        ),
        (
            "m2",
            JobOutcome::Interrupted {
                failure: failure.clone(),
                process_unconfirmed: true,
            },
            ReplyStatus::Interrupted,
        ),
    ] {
        store.post_input_fixture(&net.a, &net.network, signed_user(&net, id, "hi", true)?, 2)?;
        let claimed = store
            .claim_next_fixture(&net.a, &net.network, "c", "boot-1", "cfg")?
            .context("job")?;
        let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
        store.mark_started("c", &claimed.job.trigger, &attempt)?;
        let unknown = matches!(
            &outcome,
            JobOutcome::Interrupted {
                process_unconfirmed: true,
                ..
            }
        );
        let mut job = store.finish_job(
            &net.a,
            &net.network,
            "c",
            &claimed.job.trigger,
            &attempt,
            outcome,
            "",
            3,
        )?;
        if unknown {
            assert!(
                store
                    .message(&net.network, "c", &claimed.job.message_id)?
                    .is_none(),
                "an unknown result has no terminal reply"
            );
            assert!(matches!(job.state, JobState::Reconciling { .. }));
            let check = store
                .pending_checks()?
                .into_iter()
                .find(|check| check.attempt_id == attempt)
                .context("pending check")?;
            job = store.confirm_manager_stopped(&net.a, &net.network, &check, 4)?;
        }
        let reply = store
            .message(&net.network, "c", &claimed.job.message_id)?
            .context("confirmed reply")?;
        assert_eq!(
            (reply.status, reply.failure, reply.text.as_str()),
            (Some(status), Some(FailureKind::MissingKey), ""),
            "the shared reply names the kind, never the raw cause"
        );
        assert!(
            matches!(&job.state, JobState::Failed { failure: kept, .. } | JobState::Interrupted { failure: kept, .. } if *kept == failure),
            "the raw cause stays on the local job"
        );
    }
    Ok(())
}

#[test]
fn a_bot_thinks_until_its_reply_runs_and_rests_once_it_ends() -> Result<()> {
    use crate::scheduling::Busy;
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    assert_eq!(store.busy_conversations()?.get("c"), None);
    for (id, at) in [("m1", 2), ("m2", 3)] {
        store.post_input_fixture(&net.a, &net.network, signed_user(&net, id, "hi", true)?, at)?;
    }
    assert_eq!(store.busy_conversations()?.get("c"), Some(&Busy::Thinking));

    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot-1", "cfg")?
        .context("job")?;
    assert_eq!(
        store.busy_conversations()?.get("c"),
        Some(&Busy::Thinking),
        "claimed, nothing written yet"
    );
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    store.mark_started("c", &claimed.job.trigger, &attempt)?;
    assert_eq!(
        store.busy_conversations()?.get("c"),
        Some(&Busy::Replying),
        "a running reply outranks the queued message"
    );

    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &claimed.job.trigger,
        &attempt,
        JobOutcome::Completed,
        "ok",
        4,
    )?;
    assert_eq!(
        store.busy_conversations()?.get("c"),
        Some(&Busy::Thinking),
        "the second message still waits"
    );
    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot-1", "cfg")?
        .context("job")?;
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    store.mark_started("c", &claimed.job.trigger, &attempt)?;
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &claimed.job.trigger,
        &attempt,
        JobOutcome::Completed,
        "ok",
        5,
    )?;
    assert_eq!(store.busy_conversations()?.get("c"), None);
    Ok(())
}

#[test]
fn a_task_running_in_a_bots_conversation_does_not_make_the_bot_reply() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    crate::store::scheduler::admit_from(
        &store.connection,
        &net.network,
        "c",
        "task:w1",
        TriggerKind::AgentTask,
        "w1",
    )?;
    assert_eq!(store.busy_conversations()?.get("c"), None);
    Ok(())
}

#[test]
fn a_queued_turn_reads_the_reply_ahead_of_it() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    for (id, at) in [("m1", 2), ("m2", 3)] {
        let signed = signed_user(&net, id, "question", true)?;
        store.post_input_fixture(&net.a, &net.network, signed, at)?;
    }
    let first = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("first turn")?;
    let attempt = first.job.state.attempt().context("attempt")?.id.clone();
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &first.job.trigger,
        &attempt,
        JobOutcome::Completed,
        "first answer",
        4,
    )?;
    let second = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("second turn")?;
    assert_eq!(second.job.trigger, message_trigger("m2"));
    let read = context_after(
        &store.connection,
        &net.network,
        "c",
        Some(&second.job.basis),
        &VersionVector::new(),
    )?;
    assert!(read.events.iter().any(|event| matches!(
        &event.event.body,
        EventBody::MessagePosted { role: Role::Assistant, text, .. } if text == "first answer"
    )));
    assert_eq!(
        store
            .scheduler_job("c", &second.job.trigger)?
            .context("job")?
            .basis,
        second.job.basis,
        "the claimed basis is stored"
    );
    Ok(())
}

#[test]
fn a_refused_turn_start_leaves_its_messages_for_the_next_turn() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    let turn =
        |store: &mut Store, id: &str, started: bool| -> Result<crate::scheduling::ClaimedJob> {
            say(store, &net, id, true)?;
            let claimed = store
                .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
                .context("job")?;
            let trigger = claimed.job.trigger.clone();
            let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
            store.mark_started("c", &trigger, &attempt)?;
            store.save_scheduler_native_id("c", &trigger, &attempt, "thread", "token", false)?;
            store.record_manager_turn(&attempt, "system", "thread", None, &Default::default())?;
            let outcome = if started {
                store.record_manager_turn(
                    &attempt,
                    "system",
                    "thread",
                    Some("turn"),
                    &Default::default(),
                )?;
                JobOutcome::Completed
            } else {
                JobOutcome::Failed(ExecFailure::new(
                    FailureKind::NoModel,
                    "the model is unknown",
                ))
            };
            store.finish_job(
                &net.a,
                &net.network,
                "c",
                &trigger,
                &attempt,
                outcome,
                "",
                3,
            )?;
            Ok(claimed)
        };
    turn(&mut store, "m1", false)?;
    let second = turn(&mut store, "m2", true)?;
    assert_eq!(second.previous_native_id.as_deref(), Some("thread"));
    assert_eq!(
        second.covers, None,
        "the refused turn's message is sent again"
    );
    say(&mut store, &net, "m3", true)?;
    let third = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    assert_eq!(third.previous_native_id.as_deref(), Some("thread"));
    assert_eq!(
        third.covers.as_ref(),
        Some(&second.job.basis),
        "the started turn's messages are not repeated"
    );
    Ok(())
}

#[test]
fn a_move_hands_the_queue_on_without_a_claim() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    let local = net.a.device_id()?;
    say(&mut store, &net, "m1", true)?;
    assert!(store.fence_moved_turns(&net.network, &local)?.is_empty());
    let trigger = message_trigger("m1");
    assert!(store.scheduler_job("c", &trigger)?.is_some());
    assert!(store.scheduler_outbox()?.is_empty());
    store.append_event(
        &net.a,
        &net.network,
        "c",
        owner(&net, &net.b, 2)?,
        vec![],
        3,
    )?;
    assert!(store.fence_moved_turns(&net.network, &local)?.is_empty());
    assert!(store.scheduler_job("c", &trigger)?.is_none());
    let handed = |store: &Store| -> Result<Vec<String>> {
        Ok(store
            .scheduler_outbox()?
            .into_iter()
            .map(|e| e.trigger)
            .collect())
    };
    assert_eq!(handed(&store)?, std::slice::from_ref(&trigger));
    assert!(
        store
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .is_none()
    );
    store.fence_moved_turns(&net.network, &local)?;
    assert_eq!(handed(&store)?, [trigger]);
    Ok(())
}

#[test]
fn a_new_owner_replies_to_what_the_old_one_left_unanswered_once() -> Result<()> {
    let net = net()?;
    let mut a_side = store(&net)?;
    bot_conversation(&mut a_side, &net)?;
    let local = net.a.device_id()?;
    say(&mut a_side, &net, "m1", true)?;
    let first = a_side
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let (t1, a1) = (
        first.job.trigger.clone(),
        first.job.state.attempt().context("attempt")?.id.clone(),
    );
    a_side.mark_started("c", &t1, &a1)?;
    a_side.record_manager_turn(&a1, "system", "thread", Some("turn"), &Default::default())?;
    say(&mut a_side, &net, "m2", true)?;
    let into = a_side
        .steer_candidates(&net.network, &local, "boot")?
        .pop()
        .context("steer candidate")?
        .into;
    let m2 = message_trigger("m2");
    assert!(a_side.begin_steer(&net.network, &local, "boot", ("c", &m2), &into, None)?);
    a_side.finish_job(
        &net.a,
        &net.network,
        "c",
        &t1,
        &a1,
        JobOutcome::Completed,
        "both",
        3,
    )?;
    let answered = JobState::Steered {
        into,
        receipt: None,
        outcome: SteerOutcome::Answered,
    };
    a_side.settle_steer(&net.a, &net.network, "c", &m2, answered.clone())?;
    a_side.settle_steer(&net.a, &net.network, "c", &m2, answered)?;
    let markers: i64 = a_side.connection.query_row(
        "SELECT count(*) FROM events WHERE instr(envelope,'message_steered')>0",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(markers, 1, "a settled steer is recorded once");
    say(&mut a_side, &net, "m3", true)?;
    let third = a_side
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let (t3, a3) = (
        third.job.trigger.clone(),
        third.job.state.attempt().context("attempt")?.id.clone(),
    );
    a_side.mark_started("c", &t3, &a3)?;
    a_side.finish_job(
        &net.a,
        &net.network,
        "c",
        &t3,
        &a3,
        JobOutcome::Cancelled("stopped".into()),
        "",
        4,
    )?;
    say(&mut a_side, &net, "m4", true)?;
    assert!(
        a_side
            .unanswered_messages(&net.network, &local, "c")?
            .is_empty()
    );

    let mut b_side = store(&net)?;
    sync(&a_side, &mut b_side, &net)?;
    b_side.append_event(
        &net.b,
        &net.network,
        "c",
        owner(&net, &net.b, 2)?,
        vec![],
        5,
    )?;
    let b = net.b.device_id()?;
    assert_eq!(
        b_side.unanswered_messages(&net.network, &b, "c")?,
        ["m3", "m4"]
    );
    sync(&b_side, &mut a_side, &net)?;
    assert!(
        a_side
            .unanswered_messages(&net.network, &local, "c")?
            .is_empty()
    );

    let asked: Vec<String> = ["m1", "m2", "m3", "m4"].map(String::from).into();
    let receipts = b_side.reply_unanswered(&net.network, &b, "c", &asked)?;
    assert_eq!(
        receipts
            .iter()
            .map(|r| (r.trigger.as_str(), r.state.as_str()))
            .collect::<Vec<_>>(),
        [
            (message_trigger("m3").as_str(), "queued"),
            (message_trigger("m4").as_str(), "queued"),
        ]
    );
    let stopped = a_side
        .scheduler_job("c", &t3)?
        .context("old job")?
        .message_id;
    assert!(b_side.message(&net.network, "c", &stopped)?.is_some());
    let retried = b_side.scheduler_job("c", &t3)?.context("new job")?;
    assert_ne!(retried.message_id, stopped);
    assert!(
        b_side
            .message(&net.network, "c", &retried.message_id)?
            .is_none()
    );
    assert!(
        b_side
            .reply_unanswered(&net.network, &b, "c", &asked)?
            .is_empty()
    );
    assert!(
        b_side
            .unanswered_messages(&net.network, &b, "c")?
            .is_empty()
    );
    Ok(())
}

#[test]
fn a_new_owner_declines_what_the_old_one_left_unanswered_once() -> Result<()> {
    let net = net()?;
    let mut a_side = store(&net)?;
    bot_conversation(&mut a_side, &net)?;
    a_side.append_event(&net.a, &net.network, "c", user(&net, "m1", "x"), vec![], 2)?;
    a_side.append_event(&net.a, &net.network, "c", user(&net, "m2", "y"), vec![], 3)?;
    let mut b_side = store(&net)?;
    sync(&a_side, &mut b_side, &net)?;
    b_side.append_event(
        &net.b,
        &net.network,
        "c",
        owner(&net, &net.b, 2)?,
        vec![],
        4,
    )?;
    let b = net.b.device_id()?;
    assert_eq!(
        b_side.unanswered_messages(&net.network, &b, "c")?,
        ["m1", "m2"]
    );

    let declined = ["m1".to_owned()];
    let receipts = b_side.dismiss_unanswered(&net.network, &b, "c", &declined)?;
    assert_eq!(
        receipts
            .iter()
            .map(|r| (r.trigger.as_str(), r.state.as_str()))
            .collect::<Vec<_>>(),
        [(message_trigger("m1").as_str(), "cancelled")]
    );
    assert!(
        b_side
            .dismiss_unanswered(&net.network, &b, "c", &declined)?
            .is_empty()
    );
    assert_eq!(b_side.unanswered_messages(&net.network, &b, "c")?, ["m2"]);

    let both = ["m1".to_owned(), "m2".to_owned()];
    let queued = b_side.reply_unanswered(&net.network, &b, "c", &both)?;
    assert_eq!(
        queued
            .iter()
            .map(|r| r.trigger.as_str())
            .collect::<Vec<_>>(),
        [message_trigger("m2").as_str()]
    );
    let claimed = b_side
        .claim_next_fixture(&net.b, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    assert_eq!(claimed.job.trigger, message_trigger("m2"));
    assert!(matches!(
        b_side
            .scheduler_job("c", &message_trigger("m1"))?
            .context("declined job")?
            .state,
        JobState::Cancelled { attempt: None, .. }
    ));
    Ok(())
}
