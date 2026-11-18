use super::super::test_support::{
    adopt_initial_assignment, bot_conversation, bot_reply, complete_work, listed, net, prompt_data,
    say, signed_user, start_work, store, work_spec, workspace_assigned_task_fixture,
    workspace_fixture,
};
use super::super::*;
use crate::providers::Provider;
use crate::scheduling::{TriggerKind, message_trigger, work_trigger};
use crate::work::{RequestedWorkKind, Signal, WorkRequest, WorkSpec};
use serde_json::json;
use vibeface_bridge::failure::{ExecFailure, FailureKind};

#[test]
fn native_start_reservations_cover_initial_resume_and_reject_cancelled_work() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    say(&mut store, &net, "source", true)?;
    let local = net.a.device_id()?;
    for (reference, thread, stopped) in [
        ("fresh", None, false),
        ("resume", Some("known-thread"), false),
        ("stopped", None, true),
    ] {
        let spec = WorkSpec {
            cwd: "/work".into(),
            title: "work".into(),
            prompt: "read only".into(),
            thread: thread.map(str::to_owned),
            ..work_spec(&local, "message:source", reference, Provider::Codex)
        };
        store.accept_work_fixture(&net.a, &net.network, spec.clone(), 3)?;
        if stopped {
            store.save_stop_request(&crate::work::StopRequest {
                work_id: spec.work_id.clone(),
                conversation: "c".into(),
                target: local.clone(),
                requested_at_ms: 4,
                deadline_ms: 5,
                timed_out: false,
            })?;
            assert!(
                store
                    .reserve_work_native_submission(&net.network, &spec, &spec.cwd, None)
                    .is_err()
            );
            assert!(!store.work_native_submission_reserved(&spec.work_id)?);
        } else {
            assert!(store.reserve_work_native_submission(&net.network, &spec, &spec.cwd, None)?);
            assert!(!store.reserve_work_native_submission(&net.network, &spec, &spec.cwd, None)?);
            assert!(store.work_native_submission_reserved(&spec.work_id)?);
            let record = store.work_record(&spec.work_id)?.context("work")?;
            assert!(matches!(
                record.state,
                crate::work::WorkState::Accepted | crate::work::WorkState::Starting { .. }
            ));
            store.apply_work_signal(
                &net.a,
                &net.network,
                &spec.work_id,
                Signal::StartRejected(ExecFailure::new(
                    FailureKind::Rejected,
                    "controlled unsent fixture end",
                )),
                6,
            )?;
        }
    }
    Ok(())
}

#[test]
fn work_retries_never_start_twice_and_results_report_back() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    store.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m1", "do it", true)?,
        2,
    )?;
    let trigger = message_trigger("m1");
    let spec = work_spec(&net.a.device_id()?, &trigger, "call-1", Provider::Codex);
    assert!(
        store
            .accept_work_fixture(&net.a, &net.network, spec.clone(), 3)?
            .1
    );
    assert!(
        !store
            .accept_work_fixture(&net.a, &net.network, spec.clone(), 4)?
            .1
    );
    let moved = WorkSpec {
        thread: Some("other".into()),
        ..spec.clone()
    };
    assert!(
        store
            .accept_work_fixture(&net.a, &net.network, moved, 4)
            .is_err(),
        "the same work ID into another thread is a different request"
    );
    let id = spec.work_id.clone();
    start_work(&mut store, &net, &net.a, &id, ("t", 5), ("u", 6))?;
    let receipt = complete_work(&mut store, &net, &net.a, &id, "u", "done", 9)?;
    assert!(receipt.is_some());
    let job = store
        .scheduler_job("c", &work_trigger(&id))?
        .context("result job")?;
    assert_eq!(
        (job.kind, job.reply_to.as_str()),
        (TriggerKind::WorkResult, "m1")
    );
    let works = store.works(&net.network, "c")?;
    assert_eq!(
        (works.len(), works[0].outcome, works[0].result_text.as_str()),
        (1, Some(WorkOutcome::Completed), "done")
    );
    Ok(())
}

/// A dispatch the target never took into the DAG is still the bot's to see: waiting ones are
/// in its prompt, and a refusal or a removed target's unknown answer wakes it once.
#[test]
fn dispatches_the_dag_lacks_reach_the_bot_once() -> Result<()> {
    use crate::work::{Dispatch, DispatchState};
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    store.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m1", "go", true)?,
        2,
    )?;
    let (local, trigger) = (net.a.device_id()?, message_trigger("m1"));
    let dispatch = |call: &str| -> Result<Dispatch> {
        Ok(Dispatch {
            spec: WorkRequest {
                work_id: WorkSpec::id("c", &trigger, call),
                conversation: "c".into(),
                trigger: trigger.clone(),
                target: net.b.device_id()?,
                dispatcher: local.clone(),
                call_ref: call.into(),
                kind: RequestedWorkKind::Task,
                cwd: "/tmp".into(),
                title: "t".into(),
                prompt: "p".into(),
                continues: None,
                parent: None,
                thread: None,

                model: None,
            },
            state: DispatchState::Pending,
        })
    };
    let (refused, lost) = (dispatch("refused")?, dispatch("lost")?);
    store.save_dispatch(&refused)?;
    store.save_dispatch(&lost)?;
    let render = |store: &Store, trigger: &str| -> Result<Value> {
        let job = store.scheduler_job("c", trigger)?.context("job")?;
        let history = store.turn_history(&net.network, "c", &job, None)?;
        let works = store.works(&net.network, "c")?;
        let dispatches = store.undelivered_dispatches(&net.network, "c")?;
        let (prompt, _) = crate::scheduler::materialize(
            &history,
            &job,
            &works,
            &dispatches,
            json!({}),
            None,
            &[],
            None,
        )?;
        prompt_data(&prompt)
    };
    let shown = render(&store, &trigger)?;
    assert_eq!(
        listed(&shown, "/works", "state"),
        vec![json!("dispatch_waiting"), json!("dispatch_waiting")],
        "the bot sees what it already sent, so it does not send it again"
    );

    let no = DispatchState::Rejected {
        reason: "no bot".into(),
        failure: None,
    };
    let woke = |store: &mut Store, state: &DispatchState| {
        store
            .answer_dispatch(
                &net.network,
                &local,
                &refused.spec.work_id,
                state.clone(),
                true,
            )
            .map(|(_, receipt)| receipt.is_some())
    };
    assert!(woke(&mut store, &no)?, "a refusal wakes the bot");
    assert!(
        !woke(&mut store, &no)?,
        "a repeated refusal wakes it no more"
    );
    assert!(
        !woke(&mut store, &DispatchState::Accepted)?,
        "the first answer stands"
    );
    let report = work_trigger(&refused.spec.work_id);
    let job = store.scheduler_job("c", &report)?.context("refusal job")?;
    assert_eq!(
        (job.kind, job.reply_to.as_str()),
        (TriggerKind::WorkResult, "m1")
    );
    let shown = render(&store, &report)?;
    assert_eq!(
        shown["trigger"]["event"]["body"]["state"],
        "dispatch_rejected"
    );
    assert_eq!(shown["trigger"]["event"]["body"]["reason"], "no bot");

    let removed = [net.b.device_id()?];
    assert_eq!(store.drop_requests_to(&net.network, &local, &removed)?, 1);
    assert_eq!(store.drop_requests_to(&net.network, &local, &removed)?, 0);
    assert_eq!(
        store
            .dispatch(&lost.spec.work_id)?
            .context("lost")?
            .state
            .name(),
        "dispatch_unverified",
        "a removed target may have taken it; it is not written off as refused"
    );
    assert!(
        store
            .scheduler_job("c", &work_trigger(&lost.spec.work_id))?
            .is_some()
    );
    Ok(())
}

#[test]
fn removed_targets_leave_results_unknown_and_signed_choices_settle() -> Result<()> {
    let mut net = net()?;
    let third = LocalIdentity::generate();
    net.state = net.a.sign_network_fixture(
        1,
        vec![net.a.device_id()?, net.b.device_id()?, third.device_id()?],
    )?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    store.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m1", "go", true)?,
        2,
    )?;
    let trigger = message_trigger("m1");
    let spec = |call: &str, target: &LocalIdentity| -> Result<WorkSpec> {
        Ok(WorkSpec {
            target: target.device_id()?,
            ..work_spec(&net.a.device_id()?, &trigger, call, Provider::Codex)
        })
    };
    let outcome = |store: &Store, id: &str| -> Result<Option<WorkOutcome>> {
        Ok(store
            .works(&net.network, "c")?
            .into_iter()
            .find(|w| w.work_id == id)
            .context("work")?
            .outcome)
    };
    let remote = spec("remote", &net.b)?;
    store.accept_work_fixture(&net.b, &net.network, remote.clone(), 3)?;
    let refused = store
        .append_event(
            &third,
            &net.network,
            "c",
            EventBody::WorkFinished {
                work_id: remote.work_id.clone(),
                outcome: WorkOutcome::Unverified,
                result_text: "not this member's report".into(),
                failure_reason: None,
                failure: None,
                attestation: None,
            },
            vec![],
            4,
        )
        .unwrap_err();
    assert!(refused.to_string().contains("scheduling Node"), "{refused}");
    assert_eq!(outcome(&store, &remote.work_id)?, None);
    let unknown = store.unverify_removed_works(&net.a, &net.network, &[net.b.device_id()?], 4)?;
    assert_eq!(unknown, vec![("c".to_owned(), remote.work_id.clone())]);
    let work = store
        .works(&net.network, "c")?
        .into_iter()
        .find(|w| w.work_id == remote.work_id)
        .context("work")?;
    assert_eq!(
        (work.outcome, work.state, work.failure_reason.as_deref()),
        (
            Some(WorkOutcome::Unverified),
            WorkStatus::Unverified,
            Some("设备已移除，结果无法确认。")
        ),
        "a removed device's work is not written off as never run"
    );
    assert!(
        !store
            .scheduler_jobs("c")?
            .iter()
            .any(|job| job.trigger == crate::scheduling::work_trigger(&remote.work_id)),
        "an unknown result waits for the user"
    );
    assert!(
        store
            .unverify_removed_works(&net.a, &net.network, &[net.b.device_id()?], 5)?
            .is_empty()
    );
    let concluded = |outcome| Signal::Concluded {
        outcome,
        text: "late".into(),
        reason: Some("lost".into()),
        failure: None,
    };
    store.apply_work_signal(
        &net.b,
        &net.network,
        &remote.work_id,
        concluded(WorkOutcome::Completed),
        6,
    )?;
    assert_eq!(
        outcome(&store, &remote.work_id)?,
        Some(WorkOutcome::Completed),
        "a real result beats the unknown one"
    );

    let local = spec("local", &net.a)?;
    store.accept_work_fixture(&net.a, &net.network, local.clone(), 7)?;
    let (_, receipt) = store.apply_work_signal(
        &net.a,
        &net.network,
        &local.work_id,
        concluded(WorkOutcome::Unverified),
        8,
    )?;
    assert!(receipt.is_none(), "an unverified result waits for the user");
    let unsigned = EventBody::WorkFinished {
        work_id: local.work_id.clone(),
        outcome: WorkOutcome::Completed,
        result_text: String::new(),
        failure_reason: None,
        attestation: None,
        failure: None,
    };
    store.append_event(&net.b, &net.network, "c", unsigned, vec![], 9)?;
    assert_eq!(
        outcome(&store, &local.work_id)?,
        Some(WorkOutcome::Unverified),
        "only the target or the user settles"
    );
    let EventBody::WorkFinished {
        attestation: Some(signed),
        ..
    } = net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::WorkFinished {
            work_id: local.work_id.clone(),
            outcome: WorkOutcome::Completed,
            result_text: "checked".into(),
            failure_reason: None,
            attestation: None,
            failure: None,
        },
    )?
    else {
        unreachable!()
    };
    store
        .settle_work(
            &net.b,
            &net.network,
            "c",
            &local.work_id,
            WorkOutcome::Completed,
            "checked",
            signed.clone(),
            false,
            10,
        )?
        .context("the settled result reports back")?;
    assert_eq!(
        outcome(&store, &local.work_id)?,
        Some(WorkOutcome::Completed)
    );
    assert!(
        store
            .work_record(&local.work_id)?
            .context("record")?
            .state
            .finished()
    );
    assert!(
        store
            .settle_work(
                &net.b,
                &net.network,
                "c",
                &local.work_id,
                WorkOutcome::Completed,
                "checked",
                signed,
                false,
                11
            )
            .is_err()
    );

    let running = spec("running", &net.a)?;
    store.accept_work_fixture(&net.a, &net.network, running.clone(), 12)?;
    for signal in [
        Signal::ThreadCreated("th".into()),
        Signal::TurnStarted {
            turn_id: "tu".into(),
            epoch: 1,
        },
    ] {
        store.apply_work_signal(&net.a, &net.network, &running.work_id, signal, 13)?;
    }
    let EventBody::WorkFinished {
        attestation: Some(signed),
        ..
    } = net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::WorkFinished {
            work_id: running.work_id.clone(),
            outcome: WorkOutcome::Interrupted,
            result_text: String::new(),
            failure_reason: None,
            attestation: None,
            failure: None,
        },
    )?
    else {
        unreachable!()
    };
    let mut settle = |unvouched| {
        store.settle_work(
            &net.b,
            &net.network,
            "c",
            &running.work_id,
            WorkOutcome::Interrupted,
            "",
            signed.clone(),
            unvouched,
            14,
        )
    };
    assert!(
        settle(false).is_err(),
        "a device that can report is waited for"
    );
    settle(true)?.context("the settled result reports back")?;
    let receipt = complete_work(&mut store, &net, &net.a, &running.work_id, "tu", "late", 15)?;
    assert!(receipt.is_none(), "a late result does not report twice");
    assert_eq!(
        outcome(&store, &running.work_id)?,
        Some(WorkOutcome::Interrupted),
        "the user's settlement stands"
    );
    let target_report = spec("target-unverified", &net.b)?;
    store.accept_work_fixture(&net.b, &net.network, target_report.clone(), 16)?;
    store.apply_work_signal(
        &net.b,
        &net.network,
        &target_report.work_id,
        concluded(WorkOutcome::Unverified),
        17,
    )?;
    assert_eq!(
        outcome(&store, &target_report.work_id)?,
        Some(WorkOutcome::Unverified)
    );
    Ok(())
}

#[test]
fn open_works_list_only_what_still_wants_the_user() -> Result<()> {
    use crate::work::{Dispatch, DispatchState};
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    store.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m1", "go", true)?,
        2,
    )?;
    let local = net.a.device_id()?;
    let mut at = 2;
    let mut work = |store: &mut Store, id: &str, outcome: Option<WorkOutcome>| -> Result<()> {
        at += 1;
        let dispatched = EventBody::WorkDispatched {
            work_id: id.into(),
            trigger: message_trigger("m1"),
            dispatcher: local.clone(),
            call_ref: id.into(),
            kind: WorkKind::Task,
            cwd: "/w".into(),
            title: id.into(),
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
        store.append_event(&net.a, &net.network, "c", dispatched, vec![], at)?;
        if let Some(outcome) = outcome {
            let finished = EventBody::WorkFinished {
                work_id: id.into(),
                outcome,
                result_text: String::new(),
                failure_reason: None,
                failure: None,
                attestation: None,
            };
            store.append_event(&net.a, &net.network, "c", finished, vec![], at)?;
        }
        Ok(())
    };
    let open = |store: &Store| -> Result<Vec<String>> {
        Ok(store
            .open_works(&net.network, None)?
            .into_iter()
            .map(|work| work.work_id)
            .collect())
    };
    for n in 0..40 {
        work(
            &mut store,
            &format!("done-{n}"),
            Some(WorkOutcome::Completed),
        )?;
    }
    work(&mut store, "failed", Some(WorkOutcome::Failed))?;
    work(&mut store, "running", None)?;
    assert_eq!(open(&store)?, ["failed", "running"]);
    assert_eq!(
        store.open_works(&net.network, Some("other"))?,
        vec![],
        "another conversation has none"
    );

    let mut failed_reply = bot_reply("r0", "m1");
    if let EventBody::MessagePosted { status, .. } = &mut failed_reply {
        *status = Some(ReplyStatus::Failed);
    }
    store.append_event(&net.a, &net.network, "c", failed_reply, vec![], 50)?;
    assert_eq!(
        open(&store)?,
        ["failed", "running"],
        "a reply that failed follows nothing up"
    );
    store.append_event(&net.a, &net.network, "c", bot_reply("r1", "m1"), vec![], 51)?;
    assert_eq!(
        open(&store)?,
        ["failed", "running"],
        "the dispatching turn's reply landed later without having seen it"
    );
    let report = crate::store::scheduler::reply_message_id("c", &work_trigger("failed"), 0);
    store.append_event(
        &net.a,
        &net.network,
        "c",
        bot_reply(&report, "m1"),
        vec![],
        52,
    )?;
    assert_eq!(open(&store)?, ["running"], "the bot reported on it");

    work(&mut store, "failed-4", Some(WorkOutcome::Failed))?;
    store.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m2", "retry", true)?,
        55,
    )?;
    assert_eq!(open(&store)?, ["running", "failed-4"]);
    store.append_event(&net.a, &net.network, "c", bot_reply("r2", "m2"), vec![], 56)?;
    assert_eq!(
        open(&store)?,
        ["running"],
        "the bot answered a message written after it"
    );

    work(&mut store, "failed-2", Some(WorkOutcome::Rejected))?;
    assert_eq!(open(&store)?, ["running", "failed-2"]);
    let close = |work_id: &str| {
        net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::NotificationAcked {
                notification_id: work_id.into(),
                client_id: INBOX_CLIENT.into(),
                attestation: None,
            },
        )
    };
    assert!(store.failed_work(&net.network, "c", "failed-2")?);
    assert!(
        !store.failed_work(&net.network, "c", "running")?,
        "only a failure can be closed"
    );
    assert!(!store.failed_work(&net.network, "other", "failed-2")?);
    assert!(!store.closed_in_inbox(&net.network, "failed-2")?);
    store.append_event(&net.a, &net.network, "c", close("failed-2")?, vec![], 60)?;
    assert!(store.closed_in_inbox(&net.network, "failed-2")?);
    assert_eq!(open(&store)?, ["running"], "the user closed it");

    work(&mut store, "failed-3", Some(WorkOutcome::Failed))?;
    work(&mut store, "done-late", Some(WorkOutcome::Completed))?;
    assert_eq!(
        open(&store)?,
        ["running"],
        "a later work ending follows it up"
    );

    let trigger = message_trigger("m1");
    let refused = |call: &str| -> Result<Dispatch> {
        Ok(Dispatch {
            spec: WorkRequest {
                work_id: WorkSpec::id("c", &trigger, call),
                conversation: "c".into(),
                trigger: trigger.clone(),
                target: net.b.device_id()?,
                dispatcher: net.a.device_id()?,
                call_ref: call.into(),
                kind: RequestedWorkKind::Task,
                cwd: "/w".into(),
                title: call.into(),
                prompt: "p".into(),
                continues: None,
                parent: None,
                thread: None,

                model: None,
            },
            state: DispatchState::Pending,
        })
    };
    let refusals = |store: &Store| -> Result<Vec<String>> {
        Ok(store
            .open_dispatches(&net.network, Some("c"))?
            .into_iter()
            .map(|dispatch| dispatch.spec.title)
            .collect())
    };
    let no = DispatchState::Rejected {
        reason: "no bot".into(),
        failure: None,
    };
    for call in ["answered", "closed"] {
        let dispatch = refused(call)?;
        store.save_dispatch(&dispatch)?;
        assert!(
            !refusals(&store)?.contains(&call.to_owned()),
            "a waiting dispatch is not a refusal"
        );
        store.answer_dispatch(
            &net.network,
            &local,
            &dispatch.spec.work_id,
            no.clone(),
            true,
        )?;
    }
    assert_eq!(refusals(&store)?, ["answered", "closed"]);
    assert!(store.failed_work(&net.network, "c", &refused("closed")?.spec.work_id)?);
    let report = store
        .scheduler_job("c", &work_trigger(&refused("answered")?.spec.work_id))?
        .context("report job")?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        bot_reply(&report.message_id, "m1"),
        vec![],
        70,
    )?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        close(&refused("closed")?.spec.work_id)?,
        vec![],
        71,
    )?;
    assert!(
        refusals(&store)?.is_empty(),
        "one answered by its report, one closed"
    );

    let inline = refused("inline")?;
    store.save_dispatch(&inline)?;
    store.answer_dispatch(
        &net.network,
        &local,
        &inline.spec.work_id,
        no.clone(),
        false,
    )?;
    assert_eq!(refusals(&store)?, ["inline"]);
    let turn = store
        .scheduler_job("c", &trigger)?
        .context("dispatching turn")?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        bot_reply(&turn.message_id, "m1"),
        vec![],
        72,
    )?;
    assert_eq!(
        refusals(&store)?,
        ["inline"],
        "the turn that saw the refusal inline has no report to follow it up"
    );
    store.post_input_fixture(
        &net.a,
        &net.network,
        signed_user(&net, "m3", "again", true)?,
        73,
    )?;
    store.append_event(&net.a, &net.network, "c", bot_reply("r3", "m3"), vec![], 74)?;
    assert!(
        refusals(&store)?.is_empty(),
        "the bot answered a message written after the refusing turn"
    );
    let late = refused("late")?;
    store.save_dispatch(&late)?;
    store.answer_dispatch(&net.network, &local, &late.spec.work_id, no, true)?;
    assert_eq!(
        refusals(&store)?,
        ["late"],
        "a refusal arriving after the chat moved on waits for its report"
    );
    Ok(())
}

#[test]
fn two_accepted_ordinary_works_compete_for_one_native_reservation() -> Result<()> {
    for (thread, winner_cwd) in [
        (None, "/same-workspace"),
        (Some("existing-thread"), "/same-workspace"),
        (None, "/aliased-workspace"),
        (Some("existing-thread"), "/aliased-workspace"),
    ] {
        let net = net()?;
        let mut origin = store(&net)?;
        bot_conversation(&mut origin, &net)?;
        say(&mut origin, &net, "source", true)?;
        let local = net.a.device_id()?;
        let make = |call_ref: &str, cwd: &str| WorkSpec {
            cwd: cwd.into(),
            title: "work".into(),
            prompt: "do the work".into(),
            thread: thread.map(str::to_owned),
            ..work_spec(&local, "message:source", call_ref, Provider::Pi)
        };
        let winner = make("winner", winner_cwd);
        let waiting = make("waiting", "/same-workspace");
        let initial = match thread {
            Some(thread) => crate::work::WorkState::Starting {
                thread: Some(thread.to_owned()),
            },
            None => crate::work::WorkState::Accepted,
        };
        for work in [&winner, &waiting] {
            origin.accept_work_fixture(&net.a, &net.network, work.clone(), 3)?;
            assert_eq!(
                origin
                    .work_record(&work.work_id)?
                    .context("accepted")?
                    .state,
                initial
            );
            assert!(!origin.work_native_submission_reserved(&work.work_id)?);
            let projection = origin
                .work(&net.network, &work.work_id)?
                .context("projection")?;
            assert!(projection.thread_id.is_none() && projection.turn_id.is_none());
        }
        assert!(origin.reserve_work_native_submission(
            &net.network,
            &winner,
            &waiting.cwd,
            None
        )?);
        let blocked = origin
            .reserve_work_native_submission(&net.network, &waiting, &waiting.cwd, None)
            .expect_err("one reservation owns this root");
        assert_eq!(
            blocked
                .downcast_ref::<ExecFailure>()
                .context("typed conflict")?
                .kind,
            FailureKind::WriterBusy
        );
        assert!(format!("{blocked:#}").contains("resource_conflict"));
        assert!(origin.work_native_submission_reserved(&winner.work_id)?);
        assert!(!origin.work_native_submission_reserved(&waiting.work_id)?);
        let actual_thread = thread.unwrap_or("winner-thread");
        if thread.is_none() {
            origin.apply_work_signal(
                &net.a,
                &net.network,
                &winner.work_id,
                Signal::ThreadCreated(actual_thread.into()),
                4,
            )?;
        }
        origin.begin_reserved_work_native_input(
            &net.network,
            &winner,
            actual_thread,
            &waiting.cwd,
        )?;
        assert_eq!(
            origin
                .work_record(&waiting.work_id)?
                .context("waiting")?
                .state,
            initial
        );
        origin.apply_work_signal(
            &net.a,
            &net.network,
            &winner.work_id,
            Signal::TurnStarted {
                turn_id: "winner-turn".into(),
                epoch: 1,
            },
            5,
        )?;
        assert!(
            origin
                .reserve_work_native_submission(&net.network, &waiting, &waiting.cwd, None)
                .is_err()
        );
        assert!(!origin.work_native_submission_reserved(&waiting.work_id)?);
        complete_work(
            &mut origin,
            &net,
            &net.a,
            &winner.work_id,
            "winner-turn",
            "done",
            6,
        )?;
        assert!(origin.reserve_work_native_submission(
            &net.network,
            &waiting,
            &waiting.cwd,
            None
        )?);
    }
    Ok(())
}

#[test]
fn assignment_works_reserve_overlapping_directories_individually() -> Result<()> {
    let (net, mut origin) = workspace_fixture()?;
    let (input, assignment) = workspace_assigned_task_fixture(
        &net,
        &mut origin,
        "create",
        "task",
        "input",
        &["/workspace".into()],
        3,
    )?;
    adopt_initial_assignment(
        &mut origin,
        &net,
        &net.a,
        &assignment.assignment_id,
        input.source_event,
        ("accepted", 6),
        ("adopted", 7),
    )?;
    let assignment = origin
        .workspace_assignment(&net.network, &assignment.assignment_id)?
        .context("adopted assignment")?;
    let local = net.a.device_id()?;
    let trigger = format!("assignment:{}", assignment.assignment_id);
    let make = |call_ref: &str, cwd: &str| WorkSpec {
        cwd: cwd.into(),
        title: "work".into(),
        prompt: "do the work".into(),
        ..work_spec(&local, &trigger, call_ref, Provider::Pi)
    };
    let winner = make("winner", "/workspace/shared");
    let waiting = make("waiting", "/workspace/shared");
    let separate = make("separate", "/workspace/other");
    for work in [&winner, &waiting, &separate] {
        origin.accept_work_fixture(&net.a, &net.network, work.clone(), 8)?;
    }
    assert!(origin.reserve_work_native_submission(&net.network, &winner, &winner.cwd, None)?);
    assert!(origin.reserve_work_native_submission(&net.network, &separate, &separate.cwd, None)?);
    for running in [false, true] {
        if running {
            origin.apply_work_signal(
                &net.a,
                &net.network,
                &winner.work_id,
                Signal::ThreadCreated("winner-thread".into()),
                9,
            )?;
            origin.begin_reserved_work_native_input(
                &net.network,
                &winner,
                "winner-thread",
                &winner.cwd,
            )?;
            origin.apply_work_signal(
                &net.a,
                &net.network,
                &winner.work_id,
                Signal::TurnStarted {
                    turn_id: "winner-turn".into(),
                    epoch: 1,
                },
                10,
            )?;
        }
        let blocked = origin
            .reserve_work_native_submission(&net.network, &waiting, &waiting.cwd, None)
            .expect_err("one assignment still serializes overlapping work");
        assert_eq!(
            blocked
                .downcast_ref::<ExecFailure>()
                .context("typed conflict")?
                .kind,
            FailureKind::WriterBusy
        );
        assert!(format!("{blocked:#}").contains("resource_conflict"));
        assert!(!origin.work_native_submission_reserved(&waiting.work_id)?);
        assert!(workspace::assignment_resources_available_from(
            &origin.connection,
            &net.network,
            &assignment
        )?);
    }
    complete_work(
        &mut origin,
        &net,
        &net.a,
        &winner.work_id,
        "winner-turn",
        "done",
        11,
    )?;
    assert!(origin.reserve_work_native_submission(&net.network, &waiting, &waiting.cwd, None)?);
    Ok(())
}

#[test]
fn codex_shared_reservations_require_a_verified_turn_and_the_same_work_scope() -> Result<()> {
    use crate::work::NativeExecution;
    for scenario in [
        "same",
        "hint",
        "missing-id",
        "turn",
        "thread",
        "unknown",
        "stopped",
        "scope",
    ] {
        let net = net()?;
        let mut origin = store(&net)?;
        bot_conversation(&mut origin, &net)?;
        say(&mut origin, &net, "source", true)?;
        let local = net.a.device_id()?;
        let make = |call_ref: &str| WorkSpec {
            cwd: "/shared".into(),
            title: "work".into(),
            thread: Some("native".into()),
            ..work_spec(&local, "message:source", call_ref, Provider::Codex)
        };
        let first = make("first");
        origin.accept_work_fixture(&net.a, &net.network, first.clone(), 3)?;
        assert!(origin.reserve_work_native_submission(&net.network, &first, &first.cwd, None)?);
        origin.apply_work_signal(
            &net.a,
            &net.network,
            &first.work_id,
            Signal::TurnStarted {
                turn_id: "turn".into(),
                epoch: 1,
            },
            4,
        )?;
        if scenario == "unknown" {
            origin.apply_work_signal(
                &net.a,
                &net.network,
                &first.work_id,
                Signal::Disconnected,
                5,
            )?;
        }
        if scenario == "stopped" {
            origin.save_stop_request(&crate::work::StopRequest {
                work_id: first.work_id.clone(),
                conversation: "c".into(),
                target: local.clone(),
                requested_at_ms: 5,
                deadline_ms: 6,
                timed_out: false,
            })?;
        }
        let mut second = make("second");
        if scenario == "scope" {
            second.parent = Some("another-task".into());
        }
        if scenario == "thread" {
            second.thread = Some("another-thread".into());
        }
        origin.accept_work_fixture(&net.a, &net.network, second.clone(), 6)?;
        let execution = NativeExecution {
            thread_id: second.thread.clone().unwrap(),
            turn_id: if scenario == "turn" {
                "another-turn"
            } else {
                "turn"
            }
            .into(),
            work_ids: if scenario == "missing-id" {
                vec![]
            } else {
                vec![first.work_id.clone()]
            },
        };
        let reserved = origin.reserve_work_native_submission(
            &net.network,
            &second,
            &second.cwd,
            (scenario != "hint").then_some(&execution),
        );
        if scenario != "same" {
            assert_eq!(
                reserved
                    .expect_err(scenario)
                    .downcast_ref::<ExecFailure>()
                    .context("typed conflict")?
                    .kind,
                FailureKind::WriterBusy,
                "{scenario}"
            );
            assert!(
                !origin.work_native_submission_reserved(&second.work_id)?,
                "{scenario}"
            );
            continue;
        }
        assert!(reserved?);
        assert!(!origin.reserve_work_native_submission(
            &net.network,
            &second,
            &second.cwd,
            Some(&execution)
        )?);
        assert!(origin.work_native_write_allowed(&net.network, &first.work_id)?);
        assert!(origin.work_native_write_allowed(&net.network, &second.work_id)?);
        origin.apply_work_signal(
            &net.a,
            &net.network,
            &second.work_id,
            Signal::TurnStarted {
                turn_id: "turn".into(),
                epoch: 1,
            },
            7,
        )?;
        origin.apply_work_signal(
            &net.a,
            &net.network,
            &second.work_id,
            Signal::Disconnected,
            8,
        )?;
        let third = make("third");
        origin.accept_work_fixture(&net.a, &net.network, third.clone(), 9)?;
        let blocked = origin
            .reserve_work_native_submission(&net.network, &third, &third.cwd, Some(&execution))
            .expect_err("an unknown Work keeps its directory even with a shared-turn permit");
        assert_eq!(
            blocked
                .downcast_ref::<ExecFailure>()
                .context("typed conflict")?
                .kind,
            FailureKind::WriterBusy
        );
        assert!(!origin.work_native_submission_reserved(&third.work_id)?);
        assert!(origin.work_native_submission_reserved(&second.work_id)?);
        origin.apply_work_signal(
            &net.a,
            &net.network,
            &second.work_id,
            Signal::Probed(crate::work::Probe::Running {
                turn_id: "turn".into(),
                epoch: 1,
            }),
            10,
        )?;
        assert!(matches!(
            origin
                .work_record(&second.work_id)?
                .context("readback restored Work")?
                .state,
            crate::work::WorkState::Running { .. }
        ));
        assert!(origin.reserve_work_native_submission(
            &net.network,
            &third,
            &third.cwd,
            Some(&execution)
        )?);
        let first_projection = origin
            .work(&net.network, &first.work_id)?
            .context("first projection")?;
        let mut other_assignment = origin
            .work(&net.network, &second.work_id)?
            .context("second projection")?;
        other_assignment.trigger = "assignment:other".into();
        assert!(!origin.work_shares_native_execution(
            &net.network,
            &first_projection,
            &other_assignment
        )?);
    }
    Ok(())
}
