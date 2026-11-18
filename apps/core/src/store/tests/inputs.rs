use super::super::test_support::{bot_conversation, net, owner, say, store};
use super::super::*;
use crate::providers::Provider;
use crate::scheduling::{JobOutcome, JobState, SchedulerJob, SteerOutcome, message_trigger};
use vibeface_bridge::failure::{ExecFailure, FailureKind};

#[test]
fn three_inputs_keep_the_active_attempt_and_preserve_the_attachment_in_order() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    let local = net.a.device_id()?;
    say(&mut store, &net, "host", true)?;
    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("host claim")?;
    let trigger = claimed.job.trigger.clone();
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    let basis = claimed.job.basis.clone();
    store.mark_started("c", &trigger, &attempt)?;
    store.record_manager_turn(
        &attempt,
        "system",
        "thread",
        Some("turn"),
        &Default::default(),
    )?;
    say(&mut store, &net, "m2", true)?;
    let content = b"the complete attachment";
    let file = Attachment {
        hash: crate::crypto::digest(content),
        size: content.len() as u64,
        name: "requirements.txt".into(),
    };
    store.put_blob(&file.blob(), content)?;
    let signed = net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::InputPosted {
            message_id: "m3".into(),
            target: crate::workspace::InputTarget::BotMain {
                bot_id: "bot".into(),
            },
            intent: crate::workspace::InputIntent::Continue,
            sequence: 3,
            expected_revision: None,
            text: "read the attachment".into(),
            attachments: vec![file.clone()],
            report_results: Some(true),
            attestation: None,
        },
    )?;
    store.post_input_fixture(&net.a, &net.network, signed, 3)?;
    say(&mut store, &net, "m4", true)?;
    let candidates = store.steer_candidates(&net.network, &local, "boot")?;
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.message_id.as_str())
            .collect::<Vec<_>>(),
        ["m2", "m3", "m4"]
    );
    assert_eq!(candidates[1].attachments, vec![file.clone()]);
    let (path, bytes) = store.verified_input_attachment(&net.network, "c", "m3", &file)?;
    assert!(path.is_absolute());
    assert_eq!(bytes, content);
    for candidate in candidates {
        assert!(store.begin_steer(
            &net.network,
            &local,
            "boot",
            ("c", &candidate.trigger),
            &candidate.into,
            Some(&candidate.source_event.hash)
        )?);
        store.record_steer_submission(
            "c",
            &candidate.trigger,
            &candidate.into,
            Some(Some("turn".into())),
        )?;
        let input = store
            .scheduler_job("c", &candidate.trigger)?
            .context("input")?;
        assert!(
            matches!(input.state,JobState::Steering { into,receipt:Some(receipt),uncertain:false,.. }
            if into.attempt_id==attempt && into.turn_id=="turn" && receipt.actual_turn_id.as_deref()==Some("turn"))
        );
    }
    let host = store.scheduler_job("c", &trigger)?.context("host")?;
    assert!(matches!(host.state,JobState::Running { attempt:active } if active.id==attempt));
    assert_eq!(host.basis, basis);
    assert!(
        store
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .is_none()
    );
    Ok(())
}

#[test]
fn a_message_joins_the_running_turn_once_or_waits_its_turn() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    let local = net.a.device_id()?;
    say(&mut store, &net, "m1", true)?;
    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let (trigger, attempt) = (
        claimed.job.trigger.clone(),
        claimed.job.state.attempt().context("attempt")?.id.clone(),
    );
    store.mark_started("c", &trigger, &attempt)?;
    say(&mut store, &net, "m2", true)?;
    say(&mut store, &net, "quiet", false)?;
    let candidates = |store: &Store| store.steer_candidates(&net.network, &local, "boot");
    assert!(candidates(&store)?.is_empty());
    store.record_manager_turn(&attempt, "system", "thread", None, &Default::default())?;
    assert!(candidates(&store)?.is_empty());
    store.record_manager_turn(
        &attempt,
        "system",
        "thread",
        Some("turn"),
        &Default::default(),
    )?;
    assert!(
        store
            .steer_candidates(&net.network, &local, "later")?
            .is_empty()
    );
    let found = candidates(&store)?;
    let ids: Vec<_> = found.iter().map(|c| c.message_id.as_str()).collect();
    assert_eq!(ids, ["m2"]);
    let into = found[0].into.clone();
    assert_eq!(
        (into.thread_id.as_str(), into.turn_id.as_str()),
        ("thread", "turn")
    );
    let m2 = message_trigger("m2");
    let begin = |store: &mut Store, trigger: &str| {
        store.begin_steer(&net.network, &local, "boot", ("c", trigger), &into, None)
    };

    assert!(begin(&mut store, &m2)?);
    assert!(!begin(&mut store, &m2)?);
    assert!(candidates(&store)?.is_empty());
    store.settle_steer(&net.a, &net.network, "c", &m2, JobState::Queued)?;
    assert_eq!(
        store.scheduler_job("c", &m2)?.context("job")?.state,
        JobState::Queued
    );

    assert!(begin(&mut store, &m2)?);
    assert!(store.steer_host_active("c", &attempt)?);
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &attempt,
        JobOutcome::Completed,
        "done",
        3,
    )?;
    assert!(!store.steer_host_active("c", &attempt)?);
    let steering = store.scheduler_job("c", &m2)?.context("steering job")?;
    let mut queued = steering.clone();
    queued.state = JobState::Queued;
    let hold = |job: &SchedulerJob| {
        store.scheduler_job_hold(
            &net.network,
            &local,
            job,
            Provider::Codex,
            &crate::config::SchedulingLimits::default(),
        )
    };
    assert_eq!(
        hold(&queued)?,
        Some(crate::scheduling::QueueHoldReason::SteeringSettlement)
    );
    assert_eq!(hold(&steering)?, None);

    assert!(
        store
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .is_none()
    );
    assert!(store.scheduler_maintenance_idle().is_err());
    let unread = JobState::Steering {
        into: into.clone(),
        receipt: None,
        uncertain: false,
        unread_since_ms: Some(7),
        running_since_ms: Some(6),
    };
    store.settle_steer(&net.a, &net.network, "c", &m2, unread.clone())?;
    assert_eq!(store.scheduler_job("c", &m2)?.context("job")?.state, unread);
    let answered = JobState::Steered {
        into: into.clone(),
        receipt: None,
        outcome: SteerOutcome::Answered,
    };
    store.settle_steer(&net.a, &net.network, "c", &m2, answered.clone())?;
    store.settle_steer(&net.a, &net.network, "c", &m2, JobState::Queued)?;
    let steered = store.scheduler_job("c", &m2)?.context("job")?;
    assert_eq!(steered.state, answered);
    assert!(
        store
            .message(&net.network, "c", &steered.message_id)?
            .is_none()
    );
    let next = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    assert_eq!(next.job.trigger, message_trigger("quiet"));

    let attempt = next.job.state.attempt().context("attempt")?.id.clone();
    store.mark_started("c", &next.job.trigger, &attempt)?;
    store.record_manager_turn(
        &attempt,
        "system",
        "thread",
        Some("turn-2"),
        &Default::default(),
    )?;
    say(&mut store, &net, "m3", true)?;
    let found = candidates(&store)?;
    assert_eq!(found.len(), 1);
    let m3 = message_trigger("m3");
    assert!(store.begin_steer(
        &net.network,
        &local,
        "boot",
        ("c", &m3),
        &found[0].into,
        None
    )?);
    say(&mut store, &net, "m4", true)?;
    store.request_manager_cancel(&net.a, &net.network, "c", &m3)?;
    assert!(matches!(
        store
            .scheduler_job("c", &next.job.trigger)?
            .context("job")?
            .state,
        JobState::Cancelling { .. }
    ));
    assert!(candidates(&store)?.is_empty());
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &next.job.trigger,
        &attempt,
        JobOutcome::Cancelled("stopped".into()),
        "",
        4,
    )?;

    store.append_event(
        &net.a,
        &net.network,
        "c",
        owner(&net, &net.b, 2)?,
        vec![],
        5,
    )?;
    assert!(
        store
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .is_none()
    );
    assert!(store.scheduler_job("c", &message_trigger("m4"))?.is_none());
    let JobState::Steering {
        into,
        receipt: None,
        ..
    } = store.scheduler_job("c", &m3)?.context("job")?.state
    else {
        bail!("m3 is no longer an unacknowledged steer");
    };
    let stopped = store
        .request_manager_cancel(&net.a, &net.network, "c", &m3)?
        .context("job")?;
    let unknown = JobState::Steered {
        into,
        receipt: None,
        outcome: SteerOutcome::Unknown,
    };
    assert_eq!(
        (stopped.trigger.as_str(), &stopped.state),
        (m3.as_str(), &unknown)
    );
    assert_eq!(
        store.scheduler_job("c", &m3)?.context("job")?.state,
        unknown
    );
    assert!(!store.steering_jobs()?.iter().any(|job| job.trigger == m3));
    Ok(())
}

#[test]
fn a_steer_target_waiting_for_reconciliation_is_not_treated_as_active() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    let local = net.a.device_id()?;

    say(&mut store, &net, "m1", true)?;
    let first = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let (trigger, attempt) = (
        first.job.trigger.clone(),
        first.job.state.attempt().context("attempt")?.id.clone(),
    );
    store.mark_started("c", &trigger, &attempt)?;
    store.record_manager_turn(
        &attempt,
        "system",
        "thread",
        Some("turn"),
        &Default::default(),
    )?;

    say(&mut store, &net, "m2", true)?;
    let into = store
        .steer_candidates(&net.network, &local, "boot")?
        .pop()
        .context("steer candidate")?
        .into;
    let m2 = message_trigger("m2");
    assert!(store.begin_steer(&net.network, &local, "boot", ("c", &m2), &into, None)?);

    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &attempt,
        JobOutcome::Interrupted {
            failure: ExecFailure::new(
                FailureKind::Lost,
                "the reply process ended unexpectedly; its result is unverified",
            ),
            process_unconfirmed: true,
        },
        "",
        3,
    )?;

    assert!(
        !store.steer_host_active("c", &attempt)?,
        "a reconciliating host no longer guarantees a running target turn"
    );

    let stopped = store
        .request_manager_cancel(&net.a, &net.network, "c", &m2)?
        .context("steering message")?;
    assert_eq!(
        stopped.state,
        JobState::Steered {
            into,
            receipt: None,
            outcome: SteerOutcome::Unknown
        }
    );
    assert!(!store.steering_jobs()?.iter().any(|job| job.trigger == m2));
    Ok(())
}
