use super::super::bot_settings::ChangeRequest;
use super::super::test_support::{
    Net, all_accepted, bot_conversation, bot_with_message, net, owner, rename, save_schedule,
    scheduled_on_both, store, sync, user,
};
use super::super::*;
use crate::providers::Provider;
use crate::scheduling::{JobOutcome, JobState, TriggerKind, message_trigger, schedule_trigger};

#[test]
fn a_schedule_runs_its_latest_time_once() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    store.append_event(&net.a, &net.network, "c", user(&net, "m1", "x"), vec![], 2)?;
    let (saved, _) = store.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "r1",
        save_schedule(None, "Digest", 5),
        Provider::Codex,
        3,
    )?;
    let BotChange::Schedule { schedule_id, .. } = saved.change else {
        panic!("schedule change")
    };
    let every = 5 * 60_000;
    assert!(
        store
            .admit_due_schedules(&net.a, &net.network, 999)?
            .is_empty()
    );
    let due = store.admit_due_schedules(&net.a, &net.network, 1_000 + 3 * every + 10)?;
    let trigger = schedule_trigger(&schedule_id, 1_000 + 3 * every);
    assert_eq!(due, vec![("c".to_owned(), trigger.clone())]);
    assert!(
        store
            .admit_due_schedules(&net.a, &net.network, 1_000 + 3 * every + 20)?
            .is_empty()
    );
    assert!(
        store
            .admit_due_schedules(&net.a, &net.network, 1_000 + 4 * every)?
            .is_empty()
    );

    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("schedule job")?;
    assert_eq!(
        (claimed.job.kind, claimed.job.trigger.as_str()),
        (TriggerKind::Schedule, trigger.as_str())
    );
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &attempt,
        JobOutcome::Completed,
        "done",
        4,
    )?;
    let reply = store
        .message(&net.network, "c", &claimed.job.message_id)?
        .context("reply")?;
    assert_eq!(reply.in_reply_to.as_deref(), Some(trigger.as_str()));

    let next = store.admit_due_schedules(&net.a, &net.network, 1_000 + 5 * every)?;
    assert_eq!(next.len(), 1);
    store.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "r2",
        ChangeRequest::SaveSchedule {
            schedule_id: Some(schedule_id),
            name: "Digest".into(),
            prompt: "Summarize yesterday's commits; say so if there are none.".into(),
            first_run_at_ms: 1_000,
            every_minutes: Some(5),
            enabled: Some(false),
        },
        Provider::Codex,
        5,
    )?;
    assert!(
        store
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .is_none()
    );
    assert!(matches!(
        store.scheduler_job("c", &next[0].1)?.context("job")?.state,
        JobState::Cancelled { .. }
    ));
    assert!(
        store
            .admit_due_schedules(&net.a, &net.network, 1_000 + 9 * every)?
            .is_empty()
    );
    Ok(())
}

#[test]
fn a_schedule_uses_the_receiver_seen_time_when_the_writer_clock_is_fast() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    bot_conversation(&mut origin, &net)?;
    origin.append_event(&net.a, &net.network, "c", user(&net, "m1", "x"), vec![], 2)?;
    let (saved, _) = origin.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "r1",
        save_schedule(None, "Digest", 5),
        Provider::Codex,
        3_600_000,
    )?;
    let BotChange::Schedule { schedule_id, .. } = saved.change else {
        panic!("schedule change")
    };
    let events = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
    let mut received = store(&net)?;
    received.ingest_events_at(&net.network, &events, 10_000)?;
    let seen: i64 = received.connection.query_row(
        "SELECT seen_at_ms FROM events WHERE network_id=? AND subject IS NULL AND kind='bot_changed'",
        [&net.network],
        |row| row.get(0),
    )?;
    assert_eq!(seen, 10_000);

    let every = 5 * 60_000;
    let slot = 1_000 + 3 * every;
    assert_eq!(
        received.admit_due_schedules(&net.a, &net.network, slot + 10)?,
        vec![("c".to_owned(), schedule_trigger(&schedule_id, slot))]
    );
    received.ingest_events_at(&net.network, &events, 20_000)?;
    let stored_seen: i64 = received.connection.query_row(
        "SELECT seen_at_ms FROM events WHERE network_id=? AND subject IS NULL AND kind='bot_changed'",
        [&net.network],
        |row| row.get(0),
    )?;
    assert_eq!(
        stored_seen, 10_000,
        "duplicate sync must not reset first-seen time"
    );
    Ok(())
}

#[test]
fn a_takeover_grace_uses_the_receiver_seen_time_when_the_writer_clock_is_slow() -> Result<()> {
    let net = net()?;
    let (mut a_side, mut b_side, schedule_id) = scheduled_on_both(&net, 5)?;
    let handover = a_side.append_event(
        &net.a,
        &net.network,
        "c",
        owner(&net, &net.b, 2)?,
        vec![],
        0,
    )?;
    b_side.ingest_events_at(&net.network, &[handover], 600_000)?;
    let slot = 1_000 + 2 * 5 * 60_000;
    assert!(
        b_side
            .admit_due_schedules(&net.b, &net.network, slot + 10)?
            .is_empty(),
        "a slow writer timestamp must not consume the receiver's takeover grace"
    );
    assert!(b_side.scheduler_jobs("c")?.is_empty());
    assert_eq!(
        b_side.admit_due_schedules(
            &net.b,
            &net.network,
            600_000 + crate::store::scheduler::TAKEOVER_GRACE_MS + 1,
        )?,
        vec![("c".to_owned(), schedule_trigger(&schedule_id, slot))]
    );
    Ok(())
}

#[test]
fn a_reminder_runs_once_and_never_again_after_restart_or_sync() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    let now = 10_000;
    let (saved, _) = origin.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "r1",
        ChangeRequest::Remind {
            note: "Check the deploy".into(),
            in_minutes: Some(1),
            at_ms: None,
        },
        Provider::Codex,
        now,
    )?;
    let BotChange::Schedule {
        schedule_id,
        after: Some(reminder),
        ..
    } = saved.change
    else {
        panic!("reminder")
    };
    assert_eq!((reminder.every_minutes, reminder.depth), (None, 1));
    let at = now + 60_000;
    assert!(
        origin
            .admit_due_schedules(&net.a, &net.network, at - 1)?
            .is_empty()
    );
    let trigger = schedule_trigger(&schedule_id, at);
    assert_eq!(
        origin.admit_due_schedules(&net.a, &net.network, at + 5)?,
        vec![("c".to_owned(), trigger.clone())]
    );
    let claimed = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("reminder job")?;
    assert_eq!(claimed.job.kind, TriggerKind::Schedule);
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    origin.finish_job(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &attempt,
        JobOutcome::Completed,
        "done",
        at + 10,
    )?;
    for later in [at + 60_000, at + 86_400_000] {
        assert!(
            origin
                .admit_due_schedules(&net.a, &net.network, later)?
                .is_empty()
        );
    }
    let mut restored = store(&net)?;
    all_accepted(&mut restored, &net, &origin)?;
    assert!(
        restored
            .admit_due_schedules(&net.a, &net.network, at + 120_000)?
            .is_empty()
    );
    assert!(restored.scheduler_jobs("c")?.is_empty());
    assert!(
        restored
            .schedules(&net.network, "c")?
            .iter()
            .all(|entry| !crate::scheduling::ahead(&entry.schedule, at + 1))
    );
    Ok(())
}

fn claim_winner(store: &Store, net: &Net, schedule_id: &str, slot: i64) -> Result<(u64, String)> {
    Ok(store.connection.query_row(
        "SELECT owner_epoch,author FROM schedule_claims WHERE network_id=? AND conversation_id='c' AND schedule_id=? AND slot_ms=?",
        params![net.network, schedule_id, slot],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

#[test]
fn a_schedule_time_runs_once_across_owners() -> Result<()> {
    let net = net()?;
    let (mut a_side, mut b_side, schedule_id) = scheduled_on_both(&net, 5)?;
    let every = 5 * 60_000;
    let slot = 1_000 + 3 * every;
    let trigger = schedule_trigger(&schedule_id, slot);
    let due = vec![("c".to_owned(), trigger.clone())];
    b_side.append_event(
        &net.b,
        &net.network,
        "c",
        owner(&net, &net.b, 2)?,
        vec![],
        4,
    )?;
    assert_eq!(
        a_side.admit_due_schedules(&net.a, &net.network, slot + 10)?,
        due
    );
    assert_eq!(
        b_side.admit_due_schedules(&net.b, &net.network, slot + 20)?,
        due
    );
    let running = b_side
        .claim_next_fixture(&net.b, &net.network, "c", "boot", "cfg")?
        .context("b runs the time")?;
    assert_eq!(running.job.trigger, trigger);
    b_side.append_event(
        &net.b,
        &net.network,
        "c",
        owner(&net, &net.a, 3)?,
        vec![],
        5,
    )?;

    let a_log = a_side.events_from(&net.network, &net.a.device_id()?, 1, 500)?;
    let b_log = b_side.events_from(&net.network, &net.b.device_id()?, 1, 500)?;
    let mut winners = Vec::new();
    for order in [[&a_log, &b_log], [&b_log, &a_log]] {
        let mut replica = store(&net)?;
        for log in order {
            replica.ingest_events(&net.network, log)?;
        }
        winners.push(claim_winner(&replica, &net, &schedule_id, slot)?);
    }
    sync(&b_side, &mut a_side, &net)?;
    winners.push(claim_winner(&a_side, &net, &schedule_id, slot)?);
    assert_eq!(winners, vec![(2, net.b.device_id()?); 3]);

    assert!(
        a_side
            .admit_due_schedules(&net.a, &net.network, slot + 30)?
            .is_empty()
    );
    assert!(
        a_side
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .is_none()
    );
    assert!(matches!(
        a_side.scheduler_job("c", &trigger)?.context("job")?.state,
        JobState::Cancelled { ref evidence, .. } if evidence.contains("另一台设备")
    ));
    Ok(())
}

#[test]
fn a_new_owner_leaves_times_due_before_the_handover_to_the_former_owner() -> Result<()> {
    let net = net()?;
    let (mut a_side, mut b_side, schedule_id) = scheduled_on_both(&net, 5)?;
    let every = 5 * 60_000;
    let slot = 1_000 + 3 * every;
    b_side.append_event(
        &net.b,
        &net.network,
        "c",
        owner(&net, &net.b, 2)?,
        vec![],
        slot + 30,
    )?;
    assert!(
        b_side
            .admit_due_schedules(&net.b, &net.network, slot + 40)?
            .is_empty(),
        "the time A may have run while cut off is not run again"
    );
    assert!(b_side.scheduler_jobs("c")?.is_empty());
    let next = slot + every;
    let grace_over = slot + 30 + crate::store::scheduler::TAKEOVER_GRACE_MS;
    assert_eq!(
        b_side.admit_due_schedules(&net.b, &net.network, grace_over + 10)?,
        vec![("c".to_owned(), schedule_trigger(&schedule_id, next))]
    );

    sync(&b_side, &mut a_side, &net)?;
    assert!(
        a_side
            .admit_due_schedules(&net.a, &net.network, grace_over + 20)?
            .is_empty(),
        "the former owner stops once it sees the newer epoch"
    );
    Ok(())
}

#[test]
fn a_new_owner_waits_out_the_grace_before_taking_a_time_after_the_handover() -> Result<()> {
    let net = net()?;
    let every = 5 * 60_000;
    let slot = 1_000 + 3 * every;
    let handover = slot - 60_000;
    let grace_over = handover + crate::store::scheduler::TAKEOVER_GRACE_MS;
    assert!(grace_over < slot + every);

    let (mut a_side, mut b_side, schedule_id) = scheduled_on_both(&net, 5)?;
    b_side.append_event(
        &net.b,
        &net.network,
        "c",
        owner(&net, &net.b, 2)?,
        vec![],
        handover,
    )?;
    let ran_unaware = vec![("c".to_owned(), schedule_trigger(&schedule_id, slot))];
    assert_eq!(
        a_side.admit_due_schedules(&net.a, &net.network, slot + 10)?,
        ran_unaware,
        "the former owner has not seen the handover and runs the time"
    );
    assert!(
        b_side
            .admit_due_schedules(&net.b, &net.network, slot + 20)?
            .is_empty(),
        "the new owner waits instead of running the same time a second time"
    );
    sync(&a_side, &mut b_side, &net)?;
    assert!(
        b_side
            .admit_due_schedules(&net.b, &net.network, grace_over + 10)?
            .is_empty(),
        "the former owner's claim arrived within the grace, so the time is not run again"
    );
    assert!(b_side.scheduler_jobs("c")?.is_empty());
    let next = slot + every;
    assert_eq!(
        b_side.admit_due_schedules(&net.b, &net.network, next + 10)?,
        vec![("c".to_owned(), schedule_trigger(&schedule_id, next))],
        "after the grace the new owner runs times on time"
    );
    sync(&b_side, &mut a_side, &net)?;
    assert!(
        a_side
            .admit_due_schedules(&net.a, &net.network, next + 20)?
            .is_empty(),
        "back in touch, the former owner does not run a time the new owner took"
    );

    let (_, mut b_side, schedule_id) = scheduled_on_both(&net, 5)?;
    b_side.append_event(
        &net.b,
        &net.network,
        "c",
        owner(&net, &net.b, 2)?,
        vec![],
        handover,
    )?;
    assert!(
        b_side
            .admit_due_schedules(&net.b, &net.network, slot + 20)?
            .is_empty()
    );
    assert_eq!(
        b_side.admit_due_schedules(&net.b, &net.network, grace_over + 10)?,
        vec![("c".to_owned(), schedule_trigger(&schedule_id, slot))],
        "no claim came from the former owner, so the new owner runs the time late, not never"
    );
    assert_eq!(
        claim_winner(&b_side, &net, &schedule_id, slot)?,
        (2, net.b.device_id()?)
    );
    Ok(())
}

#[test]
fn a_skipped_late_time_is_not_run_by_the_next_owner() -> Result<()> {
    let net = net()?;
    let (mut a_side, mut b_side, schedule_id) = scheduled_on_both(&net, 60)?;
    let every = 60 * 60_000;
    let slot = 1_000 + 3 * every;
    let late = slot + crate::store::scheduler::MAX_LATE_MS + 1;
    assert!(
        a_side
            .admit_due_schedules(&net.a, &net.network, late)?
            .is_empty()
    );
    assert!(a_side.scheduler_jobs("c")?.is_empty());
    assert_eq!(
        claim_winner(&a_side, &net, &schedule_id, slot)?,
        (1, net.a.device_id()?)
    );

    let stale = b_side
        .append_event(
            &net.b,
            &net.network,
            "c",
            EventBody::ScheduleRunClaimed {
                schedule_id: schedule_id.clone(),
                slot_ms: slot,
                owner_epoch: 1,
            },
            vec![],
            4,
        )
        .unwrap_err();
    assert!(stale.to_string().contains("scheduling Node"), "{stale}");
    sync(&a_side, &mut b_side, &net)?;
    b_side.append_event(
        &net.b,
        &net.network,
        "c",
        owner(&net, &net.b, 2)?,
        vec![],
        5,
    )?;
    let misnamed = b_side
        .append_event(
            &net.b,
            &net.network,
            "c",
            EventBody::ScheduleRunClaimed {
                schedule_id: schedule_id.clone(),
                slot_ms: slot + every,
                owner_epoch: 1,
            },
            vec![],
            6,
        )
        .unwrap_err();
    assert!(misnamed.to_string().contains("owner epoch"), "{misnamed}");
    assert!(
        b_side
            .admit_due_schedules(&net.b, &net.network, slot + 60_000)?
            .is_empty()
    );
    assert!(b_side.scheduler_jobs("c")?.is_empty());
    Ok(())
}

#[test]
fn reminders_chain_boundedly_and_only_they_need_no_cause() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    let remind = |note: String| ChangeRequest::Remind {
        note,
        in_minutes: Some(5),
        at_ms: None,
    };
    let mut trigger = message_trigger("m1");
    let mut now = 10_000;
    for depth in 1..=MAX_CHAIN_DEPTH {
        let (saved, _) = origin.change_bot(
            &net.a,
            &net.network,
            "c",
            &trigger,
            "r",
            remind(format!("step {depth}")),
            Provider::Codex,
            now,
        )?;
        assert_eq!(saved.cause.is_some(), depth == 1);
        let BotChange::Schedule {
            schedule_id,
            after: Some(reminder),
            ..
        } = saved.change
        else {
            panic!("reminder")
        };
        assert_eq!(reminder.depth, depth);
        now = reminder.first_run_at_ms;
        trigger = schedule_trigger(&schedule_id, now);
    }
    let error = origin
        .change_bot(
            &net.a,
            &net.network,
            "c",
            &trigger,
            "r",
            remind("again".into()),
            Provider::Codex,
            now,
        )
        .unwrap_err();
    assert!(error.to_string().contains("automatic wakes"), "{error}");
    let error = origin
        .change_bot(
            &net.a,
            &net.network,
            "c",
            &trigger,
            "r2",
            rename("Nova"),
            Provider::Codex,
            now,
        )
        .unwrap_err();
    assert!(error.to_string().contains(bot_settings::NOT_FROM_USER));

    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    let forged = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![],
        scope_id: "c".into(),
        created_at_ms: now,
        body: EventBody::BotChanged {
            change_id: "change-x".into(),
            cause: None,
            change: BotChange::Profile {
                name: Some(Edit {
                    before: "Helper".into(),
                    after: "Nova".into(),
                }),
                instructions: None,
                model: None,
                skills: None,
                avatar: None,

                reasoning_effort: None,
            },
        },
        blobs: vec![],
    })?;
    assert!(matches!(
        &other.ingest_events(&net.network, &[forged])?.statuses[0],
        IngestStatus::Rejected(error) if error.contains("one-time reminder")
    ));
    Ok(())
}

#[test]
fn a_paused_schedule_is_not_run_and_a_deleted_one_stays_deleted() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    let (saved, _) = origin.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "r1",
        save_schedule(None, "Digest", 5),
        Provider::Codex,
        3,
    )?;
    let BotChange::Schedule { schedule_id, .. } = saved.change else {
        panic!("schedule change")
    };
    let signed = |action| -> Result<Attestation> {
        net.a
            .fixture_admin()
            .attest(
                &net.network,
                "c",
                EventBody::ScheduleControlled {
                    schedule_id: schedule_id.clone(),
                    action,
                    attestation: None,
                },
            )?
            .attestation()
            .cloned()
            .context("signed")
    };
    let control = |store: &mut Store, action, at| {
        store.control_schedule(
            &net.a,
            &net.network,
            "c",
            &schedule_id,
            action,
            signed(action)?,
            at,
        )
    };
    let every = 5 * 60_000;
    assert!(control(&mut origin, ScheduleAction::Pause, 4)?.is_some());
    assert!(control(&mut origin, ScheduleAction::Pause, 5)?.is_none());
    assert!(
        origin
            .admit_due_schedules(&net.a, &net.network, 1_000 + 3 * every)?
            .is_empty()
    );
    let forged = origin.control_schedule(
        &net.a,
        &net.network,
        "c",
        &schedule_id,
        ScheduleAction::Resume,
        signed(ScheduleAction::Pause)?,
        6,
    );
    assert!(forged.is_err(), "a signature for another action");
    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    assert!(
        !other
            .schedule(&net.network, &schedule_id)?
            .context("paused schedule")?
            .schedule
            .enabled
    );
    assert!(control(&mut origin, ScheduleAction::Resume, 6)?.is_some());
    assert_eq!(
        origin
            .admit_due_schedules(&net.a, &net.network, 1_000 + 3 * every)?
            .len(),
        1
    );
    assert!(control(&mut origin, ScheduleAction::Delete, 7)?.is_some());
    assert!(control(&mut origin, ScheduleAction::Delete, 8)?.is_none());
    assert!(origin.schedule(&net.network, &schedule_id)?.is_none());
    assert!(
        origin
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .is_none()
    );
    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    assert!(other.schedule(&net.network, &schedule_id)?.is_none());
    Ok(())
}

#[test]
fn a_schedule_time_found_too_late_is_skipped() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    store.append_event(&net.a, &net.network, "c", user(&net, "m1", "x"), vec![], 2)?;
    let (saved, _) = store.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "r1",
        save_schedule(None, "Digest", 60),
        Provider::Codex,
        3,
    )?;
    let BotChange::Schedule { schedule_id, .. } = saved.change else {
        panic!("schedule change")
    };
    let every = 60 * 60_000;
    let slot = 1_000 + every;
    let late = slot + scheduler::MAX_LATE_MS + 1;
    for now in [late, late + 1] {
        assert!(
            store
                .admit_due_schedules(&net.a, &net.network, now)?
                .is_empty()
        );
    }
    assert!(store.scheduler_jobs("c")?.is_empty());
    let next = slot + every;
    assert_eq!(
        store.admit_due_schedules(&net.a, &net.network, next + scheduler::MAX_LATE_MS)?,
        vec![("c".to_owned(), schedule_trigger(&schedule_id, next))],
        "a time within the window still runs"
    );
    Ok(())
}
