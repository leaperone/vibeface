use super::{bot_settings::ChangeRequest, *};
use crate::{
    crypto::LocalIdentity,
    providers::Provider,
    scheduling::{
        JobOutcome, JobState, SchedulerJob, Shown, SteerOutcome, TriggerKind, bot_message_trigger,
        message_trigger, schedule_trigger, work_trigger,
    },
    work::{Signal, WorkSpec},
};
use serde_json::json;

struct Net {
    network: String,
    a: LocalIdentity,
    b: LocalIdentity,
    state: NetworkState,
}

fn net() -> Result<Net> {
    let a = LocalIdentity::generate();
    let b = LocalIdentity::generate();
    let state = a.sign_network_fixture(1, vec![a.device_id()?, b.device_id()?])?;
    Ok(Net {
        network: a.fixture_network_id()?,
        a,
        b,
        state,
    })
}

fn store(net: &Net) -> Result<Store> {
    let mut store = Store::open(":memory:")?;
    store.install_trust_bundle(&net.network, &net.state)?;
    Ok(store)
}

fn user(net: &Net, id: &str, text: &str) -> EventBody {
    net.a
        .fixture_admin()
        .attest(
            &net.network,
            "c",
            EventBody::MessagePosted {
                message_id: id.into(),
                role: Role::User,
                text: text.into(),
                attachments: vec![],
                report_results: Some(true),
                in_reply_to: None,
                status: None,
                answers: None,
                attestation: None,
            },
        )
        .expect("fixture client signs")
}

fn signature(net: &Net, id: &str, text: &str) -> Result<Attestation> {
    user(net, id, text)
        .attestation()
        .cloned()
        .context("signed message")
}

type Projection = (Vec<(String, String)>, Vec<String>, Option<Conversation>);

fn projection(store: &Store, net: &Net, scope: &str) -> Result<Projection> {
    let messages = store
        .messages(&net.network, scope)?
        .into_iter()
        .map(|m| (m.message_id, m.text))
        .collect();
    let order = store
        .context(&net.network, scope, None)?
        .events
        .into_iter()
        .map(|e| e.hash)
        .collect();
    Ok((messages, order, store.conversation(&net.network, scope)?))
}

#[test]
fn out_of_order_delivery_converges() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    let mut a_events = vec![];
    let mut b_events = vec![];
    a_events.push(origin.append_event(
        &net.a,
        &net.network,
        "c",
        EventBody::ConversationCreated { title: "t".into() },
        vec![],
        1,
    )?);
    for i in 0..6 {
        let author = if i % 2 == 0 { &net.a } else { &net.b };
        let event = origin.append_event(
            author,
            &net.network,
            "c",
            user(&net, &format!("m{i}"), &format!("text {i}")),
            vec![],
            10 + i,
        )?;
        if i % 2 == 0 {
            a_events.push(event)
        } else {
            b_events.push(event)
        }
    }
    a_events.push(origin.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "same", "from a"),
        vec![],
        50,
    )?);
    b_events.push(origin.append_event(
        &net.b,
        &net.network,
        "c",
        user(&net, "same", "from b"),
        vec![],
        50,
    )?);
    let expected = projection(&origin, &net, "c")?;

    let mut b_first = store(&net)?;
    let statuses = b_first.ingest_events(&net.network, &b_events)?.0;
    assert!(
        statuses
            .iter()
            .all(|s| matches!(s, IngestStatus::Pending | IngestStatus::Rejected(_)))
    );
    b_first.ingest_events(&net.network, &a_events)?;
    b_first.ingest_events(&net.network, &b_events)?;
    assert_eq!(projection(&b_first, &net, "c")?, expected);

    let mut interleaved = store(&net)?;
    for (a, b) in a_events
        .iter()
        .zip(b_events.iter().chain(std::iter::repeat(&b_events[0])))
    {
        interleaved.ingest_events(&net.network, &[b.clone(), a.clone()])?;
    }
    interleaved.ingest_events(&net.network, &b_events)?;
    assert_eq!(projection(&interleaved, &net, "c")?, expected);
    assert_eq!(
        interleaved.version_vector(&net.network)?,
        origin.version_vector(&net.network)?
    );
    Ok(())
}

#[test]
fn duplicates_gaps_and_forgeries_are_rejected() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    let first = origin.append_event(
        &net.a,
        &net.network,
        "c",
        EventBody::ConversationCreated { title: "t".into() },
        vec![],
        1,
    )?;
    let _second =
        origin.append_event(&net.a, &net.network, "c", user(&net, "m1", "x"), vec![], 2)?;
    let third = origin.append_event(&net.a, &net.network, "c", user(&net, "m2", "y"), vec![], 3)?;
    let mut target = store(&net)?;
    assert_eq!(
        target
            .ingest_events(&net.network, std::slice::from_ref(&first))?
            .0,
        vec![IngestStatus::Accepted]
    );
    assert_eq!(
        target
            .ingest_events(&net.network, std::slice::from_ref(&first))?
            .0,
        vec![IngestStatus::Duplicate]
    );
    assert!(
        matches!(target.ingest_events(&net.network, &[third])?.0[0], IngestStatus::Rejected(ref e) if e.contains("gap"))
    );
    let mut forged = first.clone();
    forged.signature = "00".repeat(64);
    assert_eq!(
        target.ingest_events(&net.network, &[forged])?.0,
        vec![IngestStatus::Duplicate]
    );
    let stranger = LocalIdentity::generate();
    let foreign = stranger.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: stranger.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![],
        scope_id: "c".into(),
        created_at_ms: 1,
        body: user(&net, "x", "y"),
        blobs: vec![],
    })?;
    assert!(
        matches!(target.ingest_events(&net.network, &[foreign])?.0[0], IngestStatus::Rejected(ref e) if e.contains("unknown author"))
    );
    let mut tampered = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![],
        scope_id: "c".into(),
        created_at_ms: 1,
        body: user(&net, "x", "y"),
        blobs: vec![],
    })?;
    tampered.signature = net
        .a
        .sign_event(serde_json::from_str::<Event>(&first.canonical_body)?)?
        .signature;
    assert!(matches!(
        target.ingest_events(&net.network, &[tampered])?.0[0],
        IngestStatus::Rejected(_)
    ));
    Ok(())
}

#[test]
fn a_forked_log_stops_counting_and_the_conversation_stays_appendable() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    let created = origin.append_event(
        &net.a,
        &net.network,
        "c",
        EventBody::ConversationCreated { title: "t".into() },
        vec![],
        1,
    )?;
    let kept = origin.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m1", "kept"),
        vec![],
        2,
    )?;
    let fork = net.a.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.a.device_id()?,
        seq: 2,
        prev: Some(created.hash.clone()),
        deps: vec![],
        scope_id: "c".into(),
        created_at_ms: 2,
        body: user(&net, "m1", "forked"),
        blobs: vec![],
    })?;
    let after_fork = net.a.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.a.device_id()?,
        seq: 3,
        prev: Some(fork.hash.clone()),
        deps: vec![],
        scope_id: "c".into(),
        created_at_ms: 3,
        body: user(&net, "m9", "after fork"),
        blobs: vec![],
    })?;
    let mut target = store(&net)?;
    assert_eq!(
        target
            .ingest_events(&net.network, &[created, kept, fork, after_fork])?
            .0,
        vec![
            IngestStatus::Accepted,
            IngestStatus::Accepted,
            IngestStatus::Conflict,
            IngestStatus::Conflict
        ]
    );
    target.append_event(
        &net.b,
        &net.network,
        "c",
        user(&net, "m2", "still works"),
        vec![],
        4,
    )?;
    let next = origin.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m3", "a continues"),
        vec![],
        5,
    )?;
    assert_eq!(
        target.ingest_events(&net.network, &[next])?.0,
        vec![IngestStatus::Accepted]
    );
    let texts: Vec<_> = target
        .messages(&net.network, "c")?
        .into_iter()
        .map(|m| m.text)
        .collect();
    assert_eq!(texts, vec!["still works"]);
    assert!(target.conversation(&net.network, "c")?.is_some());
    assert!(target.has_conflicts_by(&net.network, &net.a.device_id()?)?);
    assert!(!target.has_conflicts_by(&net.network, &net.b.device_id()?)?);
    let conflicts = target.conflicts(&net.network)?;
    assert_eq!(conflicts.len(), 2);
    assert_eq!(conflicts[0]["other"]["body"]["text"], "forked");
    assert_eq!(conflicts[0]["kept"]["body"]["text"], "kept");
    assert_eq!(conflicts[1]["kept"]["body"]["text"], "a continues");
    Ok(())
}

#[test]
fn the_write_gate_stops_local_appends_only() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    let created = origin.append_event(
        &net.a,
        &net.network,
        "c",
        EventBody::ConversationCreated { title: "t".into() },
        vec![],
        1,
    )?;
    let mut gated = store(&net)?;
    for (state, refusal) in [
        (WriteState::Aligning, "aligning"),
        (WriteState::ReadOnly, "read-only"),
        (WriteState::Forked, "forked"),
    ] {
        gated.set_write_state(state)?;
        let error = gated
            .append_event(&net.b, &net.network, "c", user(&net, "m1", "x"), vec![], 2)
            .expect_err("a gated Node does not append");
        assert!(error.to_string().contains(refusal), "{error}");
    }
    assert_eq!(
        gated.ingest_events(&net.network, &[created])?.0,
        vec![IngestStatus::Accepted]
    );
    gated.set_write_state(WriteState::Writable)?;
    gated.append_event(&net.b, &net.network, "c", user(&net, "m1", "x"), vec![], 3)?;
    Ok(())
}

fn conversation(store: &mut Store, net: &Net) -> Result<()> {
    let owner = net
        .a
        .owner_fixture(&net.network, "c", &net.a.device_id()?)?;
    store.create_conversation(&net.a, &net.network, "c", "t", owner, 1)?;
    Ok(())
}

#[test]
fn a_retried_signed_decision_never_changes_the_outcome() -> Result<()> {
    let owner = |node: &LocalIdentity| -> Result<EventBody> {
        Ok(EventBody::ConversationOwner {
            scheduler_node: node.device_id()?,
            epoch: 2,
            attestation: None,
            cause: None,
        })
    };
    let write = |store: &mut Store,
                 net: &Net,
                 at: &mut i64,
                 author: &LocalIdentity,
                 body: EventBody|
     -> Result<String> {
        *at += 1;
        let signed = net.a.fixture_admin().attest(&net.network, "c", body)?;
        Ok(store
            .append_event(author, &net.network, "c", signed, vec![], *at)?
            .hash)
    };
    let outcome = |store: &Store, net: &Net| -> Result<Option<String>> {
        Ok(store
            .conversation(&net.network, "c")?
            .context("conversation")?
            .owner)
    };
    // Hashes are random, so a winner hashed in the upper half is drawn first; each retry then
    // undercuts it with probability at least one half.
    for _ in 0..64 {
        let net = net()?;
        let mut store = store(&net)?;
        conversation(&mut store, &net)?;
        let mut at = 10;
        let mut originals = vec![];
        for body in [owner(&net.a)?, owner(&net.b)?] {
            originals.push(write(&mut store, &net, &mut at, &net.a, body)?);
        }
        let settled = outcome(&store, &net)?;
        let losing_owner = if settled == Some(net.a.device_id()?) {
            &net.b
        } else {
            &net.a
        };
        let winning_owner =
            originals[usize::from(losing_owner.device_id()? == net.a.device_id()?)].clone();
        if winning_owner.as_str() < "8" {
            continue;
        }
        for _ in 0..64 {
            let retry = write(&mut store, &net, &mut at, &net.b, owner(losing_owner)?)?;
            assert_eq!(
                outcome(&store, &net)?,
                settled,
                "a retry changed a settled decision"
            );
            if retry < winning_owner {
                return Ok(());
            }
        }
    }
    panic!("no retry hashed below the winner, so the outer-hash tie-break was never exercised")
}

#[test]
fn the_same_message_id_queues_one_job() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    let (_, first) = store.accept_message(
        &net.a,
        &net.network,
        "c",
        "m1",
        "hi",
        vec![],
        true,
        signature(&net, "m1", "hi")?,
        2,
    )?;
    let (_, second) = store.accept_message(
        &net.a,
        &net.network,
        "c",
        "m1",
        "hi",
        vec![],
        true,
        signature(&net, "m1", "hi")?,
        3,
    )?;
    assert_eq!(first, second);
    assert!(
        store
            .accept_message(
                &net.a,
                &net.network,
                "c",
                "m1",
                "other",
                vec![],
                true,
                signature(&net, "m1", "other")?,
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
    conversation(&mut store, &net)?;
    store.accept_message(
        &net.a,
        &net.network,
        "c",
        "m1",
        "first",
        vec![],
        true,
        signature(&net, "m1", "first")?,
        2,
    )?;
    store.accept_message(
        &net.a,
        &net.network,
        "c",
        "m2",
        "second",
        vec![],
        true,
        signature(&net, "m2", "second")?,
        3,
    )?;
    let claimed = store
        .claim_next(&net.a, &net.network, "c", "boot-1", "cfg")?
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
        .claim_next(&net.a, &net.network, "c", "boot-1", "cfg")?
        .context("job")?;
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    store.mark_started("c", &claimed.job.trigger, &attempt)?;
    store.recover_scheduler_jobs(&net.a, &net.network, "boot-2")?;
    let reply = store
        .message(&net.network, "c", &claimed.job.message_id)?
        .context("reply")?;
    assert_eq!(reply.status, Some(ReplyStatus::Interrupted));
    assert_eq!(store.pending_checks()?.len(), 1);
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
    conversation(&mut store, &net)?;
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

fn say(store: &mut Store, net: &Net, id: &str, report_results: bool) -> Result<()> {
    let signed = net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::MessagePosted {
            message_id: id.into(),
            role: Role::User,
            text: id.into(),
            attachments: vec![],
            report_results: Some(report_results),
            in_reply_to: None,
            status: None,
            answers: None,
            attestation: None,
        },
    )?;
    store.accept_message(
        &net.a,
        &net.network,
        "c",
        id,
        id,
        vec![],
        report_results,
        signed.attestation().cloned().context("signed message")?,
        2,
    )?;
    Ok(())
}

#[test]
fn a_message_joins_the_running_turn_once_or_waits_its_turn() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    let local = net.a.device_id()?;
    say(&mut store, &net, "m1", true)?;
    let claimed = store
        .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
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
        store.begin_steer(&net.network, &local, "boot", "c", trigger, &into)
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
    assert!(
        store
            .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
            .is_none()
    );
    assert!(store.scheduler_maintenance_idle().is_err());
    let unread = JobState::Steering {
        into: into.clone(),
        unread_since_ms: Some(7),
        running_since_ms: Some(6),
    };
    store.settle_steer(&net.a, &net.network, "c", &m2, unread.clone())?;
    assert_eq!(store.scheduler_job("c", &m2)?.context("job")?.state, unread);
    let answered = JobState::Steered {
        into: into.clone(),
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
        .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
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
    assert!(store.begin_steer(&net.network, &local, "boot", "c", &m3, &found[0].into)?);
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
            .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
            .is_none()
    );
    assert!(store.scheduler_job("c", &message_trigger("m4"))?.is_none());
    let JobState::Steering { into, .. } = store.scheduler_job("c", &m3)?.context("job")?.state
    else {
        bail!("m3 is no longer steering");
    };
    let stopped = store
        .request_manager_cancel(&net.a, &net.network, "c", &m3)?
        .context("job")?;
    let unknown = JobState::Steered {
        into,
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
    Ok(())
}

#[test]
fn work_retries_never_start_twice_and_results_report_back() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    store.accept_message(
        &net.a,
        &net.network,
        "c",
        "m1",
        "do it",
        vec![],
        true,
        signature(&net, "m1", "do it")?,
        2,
    )?;
    let trigger = message_trigger("m1");
    let spec = WorkSpec {
        work_id: WorkSpec::id("c", &trigger, "call-1"),
        conversation: "c".into(),
        trigger: trigger.clone(),
        target: net.a.device_id()?,
        dispatcher: net.a.device_id()?,
        call_ref: "call-1".into(),
        kind: WorkKind::Codex,
        cwd: "/tmp".into(),
        title: "t".into(),
        prompt: "p".into(),
        continues: None,
        parent: None,
        thread: None,
    };
    assert!(store.accept_work(&net.a, &net.network, spec.clone(), 3)?.1);
    assert!(!store.accept_work(&net.a, &net.network, spec.clone(), 4)?.1);
    let moved = WorkSpec {
        thread: Some("other".into()),
        ..spec.clone()
    };
    assert!(
        store.accept_work(&net.a, &net.network, moved, 4).is_err(),
        "the same work ID into another thread is a different request"
    );
    let id = spec.work_id.clone();
    store.apply_work_signal(
        &net.a,
        &net.network,
        &id,
        Signal::ThreadCreated("t".into()),
        5,
    )?;
    store.apply_work_signal(
        &net.a,
        &net.network,
        &id,
        Signal::TurnStarted {
            turn_id: "u".into(),
            epoch: 1,
        },
        6,
    )?;
    let (_, receipt) = store.apply_work_signal(
        &net.a,
        &net.network,
        &id,
        Signal::TurnCompleted {
            turn_id: "u".into(),
            status: "completed".into(),
            text: "done".into(),
            error: None,
        },
        9,
    )?;
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

#[test]
fn removed_targets_abandon_and_signed_choices_settle() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    store.accept_message(
        &net.a,
        &net.network,
        "c",
        "m1",
        "go",
        vec![],
        true,
        signature(&net, "m1", "go")?,
        2,
    )?;
    let trigger = message_trigger("m1");
    let spec = |call: &str, target: &LocalIdentity| -> Result<WorkSpec> {
        Ok(WorkSpec {
            work_id: WorkSpec::id("c", &trigger, call),
            conversation: "c".into(),
            trigger: trigger.clone(),
            target: target.device_id()?,
            dispatcher: net.a.device_id()?,
            call_ref: call.into(),
            kind: WorkKind::Codex,
            cwd: "/tmp".into(),
            title: "t".into(),
            prompt: "p".into(),
            continues: None,
            parent: None,
            thread: None,
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
    store.accept_work(&net.b, &net.network, remote.clone(), 3)?;
    let abandoned = store.abandon_works(&net.a, &net.network, &[net.b.device_id()?], 4)?;
    assert_eq!(abandoned, vec![("c".to_owned(), remote.work_id.clone())]);
    assert_eq!(
        outcome(&store, &remote.work_id)?,
        Some(WorkOutcome::Abandoned)
    );
    assert!(
        store
            .abandon_works(&net.a, &net.network, &[net.b.device_id()?], 5)?
            .is_empty()
    );
    let concluded = |outcome| Signal::Concluded {
        outcome,
        text: "late".into(),
        reason: Some("lost".into()),
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
        "a real result beats abandonment"
    );

    let local = spec("local", &net.a)?;
    store.accept_work(&net.a, &net.network, local.clone(), 7)?;
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
    store.accept_work(&net.a, &net.network, running.clone(), 12)?;
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
    let (_, receipt) = store.apply_work_signal(
        &net.a,
        &net.network,
        &running.work_id,
        Signal::TurnCompleted {
            turn_id: "tu".into(),
            status: "completed".into(),
            text: "late".into(),
            error: None,
        },
        15,
    )?;
    assert!(receipt.is_none(), "a late result does not report twice");
    assert_eq!(
        outcome(&store, &running.work_id)?,
        Some(WorkOutcome::Interrupted),
        "the user's settlement stands"
    );
    Ok(())
}

#[test]
fn long_history_condenses_into_a_prefix_checkpoint() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    let text = "x".repeat(2_000);
    for i in 0..20 {
        store.append_event(
            &net.a,
            &net.network,
            "c",
            user(&net, &format!("m{i}"), &text),
            vec![],
            10 + i,
        )?;
    }
    let context = store.context(&net.network, "c", None)?;
    let works = store.works(&net.network, "c")?;
    let checkpoint =
        crate::scheduler::checkpoint(&context, &works).context("history is over budget")?;
    let EventBody::ContextCheckpoint {
        covers,
        recent_user_requests,
        ..
    } = &checkpoint
    else {
        unreachable!()
    };
    let flags: Vec<bool> = context
        .events
        .iter()
        .map(|e| e.event.seq <= covers.get(&e.event.author).copied().unwrap_or(0))
        .collect();
    assert!(
        flags.windows(2).all(|pair| pair[0] >= pair[1]),
        "covers is a prefix"
    );
    assert!(flags.iter().any(|f| *f) && flags.iter().any(|f| !*f));
    assert!(!recent_user_requests.is_empty());
    store.append_event(&net.a, &net.network, "c", checkpoint, vec![], 100)?;
    let context = store.context(&net.network, "c", None)?;
    assert!(crate::scheduler::checkpoint(&context, &works).is_none());
    for i in 20..40 {
        store.append_event(
            &net.a,
            &net.network,
            "c",
            user(&net, &format!("m{i}"), &text),
            vec![],
            200 + i,
        )?;
    }
    let full = store.context(&net.network, "c", None)?;
    let recent = super::since_checkpoint_from(&store.connection, &net.network, "c")?;
    assert!(
        recent.events.len() < full.events.len(),
        "covered events are not read"
    );
    assert_eq!(
        crate::scheduler::checkpoint(&recent, &works),
        crate::scheduler::checkpoint(&full, &works),
        "the next checkpoint is the same from the uncovered events alone"
    );
    Ok(())
}

#[test]
fn a_moved_conversation_hands_queued_turns_to_its_new_owner() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    store.accept_message(
        &net.a,
        &net.network,
        "c",
        "m1",
        "go",
        vec![],
        true,
        signature(&net, "m1", "go")?,
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
            .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
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

#[test]
fn a_move_fences_the_old_owners_running_turn() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    let local = net.a.device_id()?;
    store.accept_message(
        &net.a,
        &net.network,
        "c",
        "m1",
        "go",
        vec![],
        true,
        signature(&net, "m1", "go")?,
        2,
    )?;
    let claimed = store
        .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let trigger = claimed.job.trigger.clone();
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    store.mark_started("c", &trigger, &attempt)?;
    assert!(store.manager_attempt_allowed(&net.network, &local, "c", &trigger, &attempt)?);
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
    assert!(!store.manager_attempt_allowed(&net.network, &local, "c", &trigger, &attempt)?);
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

fn owner(net: &Net, node: &LocalIdentity, epoch: u64) -> Result<EventBody> {
    net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::ConversationOwner {
            scheduler_node: node.device_id()?,
            epoch,
            attestation: None,
            cause: None,
        },
    )
}

fn checkpoint_body(summary: &str) -> EventBody {
    EventBody::ContextCheckpoint {
        covers: VersionVector::new(),
        summary: summary.into(),
        recent_user_requests: vec![],
        open_work: vec![],
        decisions: vec![],
    }
}

fn checkpoints(store: &Store, net: &Net) -> Result<Vec<String>> {
    Ok(store
        .context(&net.network, "c", None)?
        .events
        .into_iter()
        .filter_map(|event| match event.event.body {
            EventBody::ContextCheckpoint { summary, .. } => Some(summary),
            _ => None,
        })
        .collect())
}

#[test]
fn owner_only_writes_follow_the_owner_their_author_knew() -> Result<()> {
    let net = net()?;
    let mut a_side = store(&net)?;
    conversation(&mut a_side, &net)?;
    let mut b_side = store(&net)?;
    sync(&a_side, &mut b_side, &net)?;
    b_side.append_event(
        &net.b,
        &net.network,
        "c",
        owner(&net, &net.b, 2)?,
        vec![],
        2,
    )?;
    b_side.append_event(
        &net.b,
        &net.network,
        "c",
        owner(&net, &net.a, 3)?,
        vec![],
        3,
    )?;
    sync(&b_side, &mut a_side, &net)?;
    a_side.append_event(
        &net.a,
        &net.network,
        "c",
        checkpoint_body("back"),
        vec![],
        4,
    )?;

    let mut replica = store(&net)?;
    let a_log = |side: &Store| side.events_from(&net.network, &net.a.device_id().unwrap(), 1, 500);
    let statuses = replica.ingest_events(&net.network, &a_log(&a_side)?)?.0;
    assert_eq!(statuses.last(), Some(&IngestStatus::Pending));
    let b_log = b_side.events_from(&net.network, &net.b.device_id()?, 1, 500)?;
    replica.ingest_events(&net.network, &b_log)?;
    assert_eq!(checkpoints(&replica, &net)?, ["back"]);

    let moved = b_side.append_event(
        &net.b,
        &net.network,
        "c",
        owner(&net, &net.b, 4)?,
        vec![],
        5,
    )?;
    let late = a_side.append_event(
        &net.a,
        &net.network,
        "c",
        checkpoint_body("late"),
        vec![],
        6,
    )?;
    replica.ingest_events(&net.network, std::slice::from_ref(&moved))?;
    assert_eq!(
        replica
            .ingest_events(&net.network, std::slice::from_ref(&late))?
            .0,
        [IngestStatus::Accepted]
    );

    sync(&b_side, &mut a_side, &net)?;
    let refused = a_side
        .append_event(
            &net.a,
            &net.network,
            "c",
            checkpoint_body("stale"),
            vec![],
            7,
        )
        .unwrap_err();
    assert!(refused.to_string().contains("scheduling Node"), "{refused}");
    let forged = net.a.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.a.device_id()?,
        seq: seq_of(&late)? + 1,
        prev: Some(late.hash.clone()),
        deps: vec![moved.hash.clone()],
        scope_id: "c".into(),
        created_at_ms: 7,
        body: checkpoint_body("stale"),
        blobs: vec![],
    })?;
    assert!(matches!(
        &replica.ingest_events(&net.network, &[forged])?.0[0],
        IngestStatus::Rejected(error) if error.contains("scheduling Node")
    ));
    assert_eq!(checkpoints(&replica, &net)?, ["back", "late"]);
    Ok(())
}

fn bot(net: &Net, name: &str) -> Result<EventBody> {
    net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::BotCreated {
            bot_id: "bot".into(),
            name: name.into(),
            instructions: "be brief".into(),
            project: None,
            attestation: None,
            cause: None,
        },
    )
}

fn bot_reply(id: &str, to: &str) -> EventBody {
    EventBody::MessagePosted {
        message_id: id.into(),
        role: Role::Assistant,
        text: "ok".into(),
        attachments: vec![],
        report_results: None,
        in_reply_to: Some(to.into()),
        status: Some(ReplyStatus::Completed),
        answers: None,
        attestation: None,
    }
}

fn save_skill(skill_id: Option<&str>, name: &str, body: &str) -> ChangeRequest {
    ChangeRequest::SaveSkill {
        skill_id: skill_id.map(Into::into),
        name: name.into(),
        description: format!("{name} in short"),
        body: body.into(),
    }
}

fn save_schedule(schedule_id: Option<&str>, name: &str, every_minutes: u32) -> ChangeRequest {
    ChangeRequest::SaveSchedule {
        schedule_id: schedule_id.map(Into::into),
        name: name.into(),
        prompt: "Summarize yesterday's commits; say so if there are none.".into(),
        first_run_at_ms: 1_000,
        every_minutes: Some(every_minutes),
        enabled: None,
    }
}

fn rename(name: &str) -> ChangeRequest {
    ChangeRequest::UpdateProfile {
        name: Some(name.into()),
        instructions: None,
    }
}

fn bot_projection(store: &Store, net: &Net) -> Result<Value> {
    let changes: Vec<_> = store
        .bot_changes(&net.network, "c")?
        .into_iter()
        .map(|change| (change.change_id, change.event_hash, change.change))
        .collect();
    Ok(serde_json::json!({
        "bots": store.bots(&net.network)?,
        "skills": store.skills(&net.network)?,
        "schedules": store.schedules(&net.network, "c")?,
        "changes": changes,
    }))
}

#[test]
fn a_bot_change_must_follow_the_users_signed_message() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    conversation(&mut origin, &net)?;
    origin.append_event(&net.a, &net.network, "c", bot(&net, "Helper")?, vec![], 2)?;
    let m1 = origin.append_event(&net.a, &net.network, "c", user(&net, "m1", "x"), vec![], 3)?;
    let a1 = origin.append_event(&net.a, &net.network, "c", bot_reply("a1", "m1"), vec![], 4)?;
    let mut target = store(&net)?;
    let a_events = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
    assert!(
        target
            .ingest_events(&net.network, &a_events)?
            .0
            .iter()
            .all(|status| *status == IngestStatus::Accepted)
    );
    let change = |cause: &str, deps: Vec<String>| {
        net.b.sign_event(Event {
            version: WIRE_VERSION,
            network_id: net.network.clone(),
            author: net.b.device_id()?,
            seq: 1,
            prev: None,
            deps,
            scope_id: "c".into(),
            created_at_ms: 5,
            body: EventBody::BotChanged {
                change_id: "change-1".into(),
                cause: Some(Cause {
                    conversation_id: "c".into(),
                    message_id: cause.into(),
                }),
                change: BotChange::Profile {
                    name: Some(Edit {
                        before: "Helper".into(),
                        after: "Nova".into(),
                    }),
                    instructions: None,
                    model: None,
                    skills: None,
                },
            },
            blobs: vec![],
        })
    };
    for (cause, deps) in [
        ("m1", vec![]),
        ("a1", vec![a1.hash.clone()]),
        ("m2", vec![m1.hash.clone()]),
    ] {
        assert!(
            matches!(&target.ingest_events(&net.network, &[change(cause, deps)?])?.0[0],
                IngestStatus::Rejected(error) if error.contains("user's own signed message")),
            "{cause}"
        );
    }
    assert_eq!(target.bots(&net.network)?[0].name, "Helper");
    assert_eq!(
        target
            .ingest_events(&net.network, &[change("m1", vec![m1.hash.clone()])?])?
            .0,
        vec![IngestStatus::Accepted]
    );
    assert_eq!(target.bots(&net.network)?[0].name, "Nova");
    assert_eq!(target.bot_changes(&net.network, "c")?.len(), 1);

    for trigger in ["work:w1", "message:a1"] {
        let error = origin
            .change_bot(
                &net.a,
                &net.network,
                "c",
                trigger,
                "r1",
                rename("Nova"),
                Provider::Codex,
                6,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains(bot_settings::NOT_FROM_USER),
            "{trigger}: {error}"
        );
    }
    Ok(())
}

#[test]
fn bot_changes_converge_and_retries_write_once() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    conversation(&mut origin, &net)?;
    origin.append_event(&net.a, &net.network, "c", bot(&net, "Helper")?, vec![], 2)?;
    origin.append_event(&net.a, &net.network, "c", user(&net, "m1", "x"), vec![], 3)?;
    let change = |store: &mut Store, identity, trigger, call_ref, request, at| {
        store.change_bot(
            identity,
            &net.network,
            "c",
            trigger,
            call_ref,
            request,
            Provider::Codex,
            at,
        )
    };
    let (renamed, written) = change(&mut origin, &net.a, "message:m1", "r1", rename("Nova"), 4)?;
    assert!(written);
    assert_eq!(
        renamed.change,
        BotChange::Profile {
            name: Some(Edit {
                before: "Helper".into(),
                after: "Nova".into()
            }),
            instructions: None,
            model: None,
            skills: None
        }
    );
    assert_eq!(
        change(&mut origin, &net.a, "message:m1", "r1", rename("Nova"), 5)?,
        (renamed.clone(), false)
    );
    for (call_ref, request, error) in [
        ("r1", rename("Orion"), "different change"),
        ("r2", rename("Nova"), "nothing to change"),
    ] {
        let failed = change(&mut origin, &net.a, "message:m1", call_ref, request, 5).unwrap_err();
        assert!(failed.to_string().contains(error), "{failed}");
    }
    let (saved, _) = change(
        &mut origin,
        &net.a,
        "message:m1",
        "r3",
        save_skill(None, "Digest", "List the commits."),
        6,
    )?;
    let BotChange::Skill { skill_id, .. } = saved.change else {
        panic!("skill change")
    };
    let duplicate = change(
        &mut origin,
        &net.a,
        "message:m1",
        "r4",
        save_skill(None, "digest", "Other."),
        6,
    )
    .unwrap_err();
    assert!(duplicate.to_string().contains(&skill_id), "{duplicate}");
    let (schedule, _) = change(
        &mut origin,
        &net.a,
        "message:m1",
        "r5",
        save_schedule(None, "Morning", 1440),
        7,
    )?;
    let BotChange::Schedule { schedule_id, .. } = schedule.change else {
        panic!("schedule change")
    };

    origin.append_event(&net.b, &net.network, "c", user(&net, "m2", "y"), vec![], 8)?;
    change(&mut origin, &net.b, "message:m2", "r1", rename("Orion"), 9)?;
    change(
        &mut origin,
        &net.b,
        "message:m2",
        "r2",
        save_skill(Some(&skill_id), "Digest", "List the commits by author."),
        10,
    )?;
    change(
        &mut origin,
        &net.a,
        "message:m1",
        "r6",
        ChangeRequest::RemoveSkill {
            skill_id: skill_id.clone(),
        },
        11,
    )?;
    change(
        &mut origin,
        &net.a,
        "message:m1",
        "r7",
        save_schedule(Some(&schedule_id), "Morning", 60),
        12,
    )?;
    change(
        &mut origin,
        &net.b,
        "message:m2",
        "r3",
        ChangeRequest::RemoveSchedule {
            schedule_id: schedule_id.clone(),
        },
        13,
    )?;
    let expected = bot_projection(&origin, &net)?;
    assert_eq!(expected["bots"][0]["name"], "Orion");
    assert_eq!(expected["skills"], serde_json::json!([]));
    assert_eq!(expected["schedules"], serde_json::json!([]));
    assert_eq!(expected["changes"].as_array().map(Vec::len), Some(8));

    let a_events = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
    let b_events = origin.events_from(&net.network, &net.b.device_id()?, 1, 100)?;
    let mut a_first = store(&net)?;
    a_first.ingest_events(&net.network, &a_events)?;
    a_first.ingest_events(&net.network, &b_events)?;
    let mut b_first = store(&net)?;
    b_first.ingest_events(&net.network, &b_events)?;
    b_first.ingest_events(&net.network, &a_events)?;
    b_first.ingest_events(&net.network, &b_events)?;
    assert_eq!(bot_projection(&a_first, &net)?, expected);
    assert_eq!(bot_projection(&b_first, &net)?, expected);
    Ok(())
}

#[test]
fn a_schedule_runs_its_latest_time_once() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
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
        .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
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
            .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
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

fn bot_with_message(net: &Net) -> Result<(Store, String)> {
    let mut store = store(net)?;
    conversation(&mut store, net)?;
    store.append_event(&net.a, &net.network, "c", bot(net, "Helper")?, vec![], 2)?;
    let m1 = store.append_event(&net.a, &net.network, "c", user(net, "m1", "x"), vec![], 3)?;
    Ok((store, m1.hash))
}

fn second_bot(store: &mut Store, net: &Net, name: &str) -> Result<()> {
    let owner = net
        .a
        .owner_fixture(&net.network, "d", &net.a.device_id()?)?;
    store.create_conversation(&net.a, &net.network, "d", "t", owner, 1)?;
    let created = net.a.fixture_admin().attest(
        &net.network,
        "d",
        EventBody::BotCreated {
            bot_id: "bot-d".into(),
            name: name.into(),
            instructions: String::new(),
            project: None,
            attestation: None,
            cause: None,
        },
    )?;
    store.append_event(&net.a, &net.network, "d", created, vec![], 2)?;
    Ok(())
}

fn all_accepted(target: &mut Store, net: &Net, source: &Store) -> Result<()> {
    for author in [&net.a, &net.b] {
        let events = source.events_from(&net.network, &author.device_id()?, 1, 500)?;
        let statuses = target.ingest_events(&net.network, &events)?.0;
        assert!(
            statuses
                .iter()
                .all(|status| *status == IngestStatus::Accepted),
            "{statuses:?}"
        );
    }
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
        .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
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

fn scheduled_on_both(net: &Net, every_minutes: u32) -> Result<(Store, Store, String)> {
    let mut a_side = store(net)?;
    conversation(&mut a_side, net)?;
    a_side.append_event(&net.a, &net.network, "c", user(net, "m1", "x"), vec![], 2)?;
    let (saved, _) = a_side.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "r1",
        save_schedule(None, "Digest", every_minutes),
        Provider::Codex,
        3,
    )?;
    let BotChange::Schedule { schedule_id, .. } = saved.change else {
        panic!("schedule change")
    };
    let mut b_side = store(net)?;
    sync(&a_side, &mut b_side, net)?;
    Ok((a_side, b_side, schedule_id))
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
        .claim_next(&net.b, &net.network, "c", "boot", "cfg")?
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
            .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
            .is_none()
    );
    assert!(matches!(
        a_side.scheduler_job("c", &trigger)?.context("job")?.state,
        JobState::Cancelled { ref evidence, .. } if evidence.contains("另一台设备")
    ));
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
            },
        },
        blobs: vec![],
    })?;
    assert!(matches!(
        &other.ingest_events(&net.network, &[forged])?.0[0],
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
            .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
            .is_none()
    );
    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    assert!(other.schedule(&net.network, &schedule_id)?.is_none());
    Ok(())
}

#[test]
fn a_bot_message_wakes_the_other_bot_once() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    second_bot(&mut origin, &net, "Ops")?;
    let send = |store: &mut Store, call_ref: &str, to: &str, text: &str, at| {
        store.message_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            call_ref,
            to,
            text,
            at,
        )
    };
    let (message, receipt, written) = send(&mut origin, "s1", "ops", "Check the backups", 10)?;
    assert!(written);
    assert_eq!(message.role, Role::Bot);
    assert_eq!(
        message.sender.as_ref().map(|s| s.name.as_str()),
        Some("Helper")
    );
    let trigger = bot_message_trigger(&message.message_id);
    assert_eq!(
        (receipt.conversation_id.as_str(), receipt.trigger.as_str()),
        ("d", trigger.as_str())
    );
    let (again, _, written) = send(&mut origin, "s1", "Ops", "Check the backups", 11)?;
    assert!(!written && again.message_id == message.message_id);
    let changed = send(&mut origin, "s1", "d", "Other", 11).unwrap_err();
    assert!(changed.to_string().contains("another message"), "{changed}");
    assert!(send(&mut origin, "s2", "c", "To myself", 11).is_err());
    assert_eq!(origin.scheduler_jobs("d")?.len(), 1);
    let claimed = origin
        .claim_next(&net.a, &net.network, "d", "boot", "cfg")?
        .context("bot message job")?;
    assert_eq!(
        (claimed.job.kind, claimed.job.trigger.as_str()),
        (TriggerKind::BotMessage, trigger.as_str())
    );

    let (reply, _, _) = origin.message_bot(
        &net.a,
        &net.network,
        "d",
        &trigger,
        "s1",
        "Helper",
        "Done",
        12,
    )?;
    let hops = origin
        .context(&net.network, "c", None)?
        .events
        .into_iter()
        .find_map(|event| match event.event.body {
            EventBody::BotMessaged {
                message_id, hops, ..
            } if message_id == reply.message_id => Some(hops),
            _ => None,
        });
    assert_eq!(hops, Some(2));
    let sent = origin.sent_bot_messages(&net.network, "d")?;
    assert_eq!(
        sent.iter()
            .map(|row| (row.message_id.as_str(), row.to.as_str(), row.text.as_str()))
            .collect::<Vec<_>>(),
        [(reply.message_id.as_str(), "c", "Done")],
        "the sender's chat lists what it wrote to another bot"
    );

    for index in 2..=BOT_MESSAGES_PER_HOUR {
        send(
            &mut origin,
            &format!("s{index}"),
            "d",
            &format!("note {index}"),
            20,
        )?;
    }
    let limited = send(&mut origin, "s99", "d", "one more", 21).unwrap_err();
    assert!(limited.to_string().contains("last hour"), "{limited}");
    assert!(send(&mut origin, "s99", "d", "one more", 3_600_020).is_ok());

    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    assert!(other.scheduler_jobs("d")?.is_empty() && other.scheduler_jobs("c")?.is_empty());
    let forged = net.a.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![],
        scope_id: "c".into(),
        created_at_ms: 30,
        body: EventBody::BotMessaged {
            message_id: "bot-message-x".into(),
            from: Sender {
                conversation_id: "d".into(),
                name: "Ops".into(),
            },
            text: "deep".into(),
            hops: MAX_CHAIN_DEPTH + 1,
        },
        blobs: vec![],
    });
    assert!(
        forged.is_err(),
        "hops beyond the limit are not a valid event"
    );
    Ok(())
}

#[test]
fn a_bot_creates_and_configures_bots_for_the_users_message() -> Result<()> {
    let net = net()?;
    let (mut origin, m1) = bot_with_message(&net)?;
    second_bot(&mut origin, &net, "Ops")?;
    let create = |store: &mut Store, trigger: &str, call_ref: &str, name: &str| {
        store.create_bot_in_turn(
            &net.a,
            &net.network,
            "c",
            trigger,
            call_ref,
            (name, "Watch the logs.", None),
            10,
        )
    };
    let (created, new) = create(&mut origin, "message:m1", "b1", "Watcher")?;
    assert!(new);
    assert_eq!(
        create(&mut origin, "message:m1", "b1", "Watcher")?,
        (created.clone(), false)
    );
    assert!(create(&mut origin, "message:m1", "b1", "Other").is_err());
    let refused = create(&mut origin, "work:w1", "b2", "Other").unwrap_err();
    assert!(
        refused.to_string().contains(bot_settings::NOT_FROM_USER),
        "{refused}"
    );
    create(&mut origin, "message:m1", "b2", "Second")?;
    create(&mut origin, "message:m1", "b3", "Third")?;
    let capped = create(&mut origin, "message:m1", "b4", "Fourth").unwrap_err();
    assert!(capped.to_string().contains("at most"), "{capped}");
    let (changed, _) = origin.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "k1",
        ChangeRequest::ConfigureBot {
            bot: "Ops".into(),
            name: Some("Backup".into()),
            instructions: None,
            model: None,
            skills: None,
            all_skills: None,
        },
        Provider::Codex,
        11,
    )?;
    assert_eq!(changed.conversation_id, "d");

    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    let mut names: Vec<_> = other
        .bots(&net.network)?
        .into_iter()
        .map(|bot| bot.name)
        .collect();
    names.sort();
    assert_eq!(names, ["Backup", "Helper", "Second", "Third", "Watcher"]);
    assert_eq!(
        other
            .conversation(&net.network, &created.conversation_id)?
            .and_then(|c| c.owner),
        Some(net.a.device_id()?)
    );

    let hijack = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![m1],
        scope_id: "d".into(),
        created_at_ms: 12,
        body: EventBody::ConversationOwner {
            scheduler_node: net.b.device_id()?,
            epoch: 1,
            attestation: None,
            cause: Some(Cause {
                conversation_id: "c".into(),
                message_id: "m1".into(),
            }),
        },
        blobs: vec![],
    })?;
    assert!(matches!(
        &other.ingest_events(&net.network, &[hijack])?.0[0],
        IngestStatus::Rejected(error) if error.contains("follows its creation")
    ));
    Ok(())
}

fn configure(
    bot: &str,
    model: Option<&str>,
    skills: Option<&[&str]>,
    all_skills: Option<bool>,
) -> ChangeRequest {
    ChangeRequest::ConfigureBot {
        bot: bot.into(),
        name: None,
        instructions: None,
        model: model.map(Into::into),
        skills: skills.map(|ids| ids.iter().map(|id| id.to_string()).collect()),
        all_skills,
    }
}

#[test]
fn a_bot_model_and_skills_follow_the_users_message() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    second_bot(&mut origin, &net, "Ops")?;
    let mut change = |call_ref: &str, request: ChangeRequest| {
        origin.change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            call_ref,
            request,
            Provider::Codex,
            5,
        )
    };
    let (saved, _) = change("s1", save_skill(None, "Digest", "List the commits."))?;
    let BotChange::Skill { skill_id, .. } = saved.change else {
        panic!("skill change")
    };
    change("s2", save_skill(None, "Review", "Review the diff."))?;
    let (modeled, written) = change("k1", configure("Ops", Some(" gpt-x "), None, None))?;
    assert!(written && modeled.conversation_id == "d");
    assert_eq!(
        change("k1", configure("Ops", Some("gpt-x"), None, None))?,
        (modeled, false)
    );
    let same = change("k2", configure("Ops", Some("gpt-x"), None, None)).unwrap_err();
    assert!(same.to_string().contains("nothing to change"), "{same}");
    let only = [skill_id.as_str()];
    change("k3", configure("Helper", None, Some(&only), None))?;
    let unknown = change("k4", configure("Helper", None, Some(&["skill-x"]), None)).unwrap_err();
    assert!(
        unknown.to_string().contains("skill-x not found"),
        "{unknown}"
    );
    let both = change("k5", configure("Ops", None, Some(&only), Some(true))).unwrap_err();
    assert!(both.to_string().contains("skills"), "{both}");
    change("k6", configure("Ops", None, Some(&[]), None))?;
    change("k7", configure("Ops", None, None, Some(true)))?;

    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    for store in [&origin, &other] {
        let bots = store.bots(&net.network)?;
        let helper = bots
            .iter()
            .find(|bot| bot.name == "Helper")
            .context("helper")?;
        let ops = bots.iter().find(|bot| bot.name == "Ops").context("ops")?;
        assert!(helper.models.is_empty());
        assert_eq!(helper.skills, Some([skill_id.clone()].into()));
        assert_eq!(ops.model(Provider::Codex), Some("gpt-x"));
        assert_eq!(ops.skills, None, "all_skills hands back every skill");
    }
    let prompt = |scope| crate::scheduler::bot_value(&other, &net.network, scope, 0);
    let (helper, ops) = (prompt("c")?, prompt("d")?);
    assert_eq!(helper["skills"].as_array().map(Vec::len), Some(1));
    assert_eq!(helper["skills"][0]["skill_id"], skill_id.as_str());
    assert_eq!(ops["skills"].as_array().map(Vec::len), Some(2));
    assert_eq!(ops["configured_bot"]["models"]["codex"], "gpt-x");
    Ok(())
}

#[test]
fn a_bot_runs_the_model_of_its_devices_provider_or_the_devices_default() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    let change = |store: &mut Store, call_ref: &str, model: &str, provider| {
        store.change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            call_ref,
            configure("Helper", Some(model), None, None),
            provider,
            5,
        )
    };
    let runs = |store: &Store, provider: Provider| -> Result<Option<String>> {
        let device = crate::config::ManagerSettings {
            provider: provider.manager_kind(),
            workspace: "/w".into(),
            model: "auto".into(),
            cursor_sdk_bin: "/sdk".into(),
        };
        let bot = store.bot_for_conversation(&net.network, "c")?;
        Ok(crate::scheduler::bot_model(&device, bot.as_ref()))
    };
    let (set, _) = change(&mut origin, "k1", "gpt-x", Provider::Codex)?;
    assert!(matches!(
        set.change,
        BotChange::Profile { model: Some(ModelEdit { provider: Provider::Codex, ref before, ref after }), .. }
            if before.is_empty() && after == "gpt-x"
    ));
    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    assert_eq!(runs(&other, Provider::Codex)?.as_deref(), Some("gpt-x"));
    assert_eq!(
        runs(&other, Provider::Cursor)?,
        None,
        "cursor runs its default"
    );

    change(&mut origin, "k2", "composer-1", Provider::Cursor)?;
    change(&mut origin, "k3", "", Provider::Codex)?;
    let same = change(&mut origin, "k4", "", Provider::Codex).unwrap_err();
    assert!(same.to_string().contains("nothing to change"), "{same}");
    let mut later = store(&net)?;
    all_accepted(&mut later, &net, &origin)?;
    assert_eq!(
        runs(&later, Provider::Codex)?,
        None,
        "an emptied model follows the default"
    );
    assert_eq!(
        runs(&later, Provider::Cursor)?.as_deref(),
        Some("composer-1")
    );
    Ok(())
}

#[test]
fn a_deleted_bot_stops_everywhere_and_stays_deleted() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    second_bot(&mut origin, &net, "Ops")?;
    let (created, _) = origin.create_bot_in_turn(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "b1",
        ("Watcher", "Watch the logs.", None),
        4,
    )?;
    origin.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "r1",
        save_schedule(None, "Digest", 5),
        Provider::Codex,
        4,
    )?;
    let delete = |store: &mut Store, trigger: &str, wanted: &str| {
        store.delete_bot_in_turn(&net.a, &net.network, "c", trigger, wanted, 5)
    };
    let itself = delete(&mut origin, "message:m1", "Helper").unwrap_err();
    assert!(
        itself.to_string().contains("does not delete itself"),
        "{itself}"
    );
    let unasked = delete(&mut origin, "work:w1", "Watcher").unwrap_err();
    assert!(
        unasked.to_string().contains(bot_settings::NOT_FROM_USER),
        "{unasked}"
    );
    let (gone, event) = delete(&mut origin, "message:m1", "Watcher")?;
    assert!(gone == created.conversation_id && event.is_some());
    assert_eq!(
        delete(&mut origin, "message:m1", "Watcher")?,
        (created.conversation_id.clone(), None)
    );

    origin.accept_message(
        &net.a,
        &net.network,
        "c",
        "m2",
        "hi",
        vec![],
        true,
        signature(&net, "m2", "hi")?,
        6,
    )?;
    let signed = net
        .a
        .fixture_admin()
        .attest(
            &net.network,
            "c",
            EventBody::BotDeleted {
                attestation: None,
                cause: None,
            },
        )?
        .attestation()
        .cloned()
        .context("signed")?;
    assert!(
        origin
            .delete_bot(&net.a, &net.network, "c", signed.clone(), 7)?
            .is_some()
    );
    assert!(
        origin
            .delete_bot(&net.a, &net.network, "c", signed, 8)?
            .is_none()
    );
    assert!(
        origin
            .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
            .is_none()
    );
    assert!(
        origin
            .scheduler_jobs("c")?
            .iter()
            .all(|job| matches!(job.state, JobState::Cancelled { .. }))
    );
    let refused = origin
        .accept_message(
            &net.a,
            &net.network,
            "c",
            "m3",
            "again",
            vec![],
            true,
            signature(&net, "m3", "again")?,
            9,
        )
        .unwrap_err();
    assert!(refused.to_string().contains("deleted"), "{refused}");
    assert!(origin.schedules(&net.network, "c")?.is_empty());
    assert!(
        origin
            .admit_due_schedules(&net.a, &net.network, 10_000_000)?
            .is_empty()
    );

    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    for store in [&origin, &other] {
        let names: Vec<_> = store
            .bots(&net.network)?
            .into_iter()
            .map(|bot| bot.name)
            .collect();
        assert_eq!(names, ["Ops"]);
        assert_eq!(store.primary_bot(&net.network)?.as_deref(), Some("d"));
    }
    let unsigned = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![],
        scope_id: "d".into(),
        created_at_ms: 11,
        body: EventBody::BotDeleted {
            attestation: None,
            cause: None,
        },
        blobs: vec![],
    });
    assert!(unsigned.is_err(), "deleting needs the user's signature");
    Ok(())
}

#[test]
fn notes_refuse_secrets_and_sync() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    let remember = |store: &mut Store, trigger: &str, call_ref: &str, text: &str| {
        store.change_bot(
            &net.a,
            &net.network,
            "c",
            trigger,
            call_ref,
            ChangeRequest::Remember {
                memory_id: None,
                text: text.into(),
            },
            Provider::Codex,
            10,
        )
    };
    for (index, refused) in [
        "my key is sk-proj-abcdefghijklmnopqrstuvwxyz123456".to_owned(),
        "Authorization: Bearer abcdefghijklmnopqrstuvwxyz".to_owned(),
        "-----BEGIN OPENSSH PRIVATE KEY-----".to_owned(),
        "x".repeat(MAX_MEMORY_CHARS + 1),
    ]
    .iter()
    .enumerate()
    {
        assert!(
            remember(&mut origin, "message:m1", &format!("n{index}"), refused).is_err(),
            "{refused}"
        );
    }
    let (kept, _) = remember(
        &mut origin,
        "message:m1",
        "k1",
        "Prefers short replies in Chinese",
    )?;
    let BotChange::Memory { memory_id, .. } = kept.change else {
        panic!("note")
    };
    let error = remember(&mut origin, "work:w1", "k2", "Something else").unwrap_err();
    assert!(error.to_string().contains(bot_settings::NOT_FROM_USER));
    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    assert_eq!(
        other.memories(&net.network, "c")?,
        origin.memories(&net.network, "c")?
    );
    origin.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "k3",
        ChangeRequest::Forget { memory_id },
        Provider::Codex,
        11,
    )?;
    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    assert!(other.memories(&net.network, "c")?.is_empty());
    Ok(())
}

#[test]
fn append_path_uses_indexes_only() -> Result<()> {
    let net = net()?;
    let store = store(&net)?;
    let author = net.a.device_id()?;
    let statements = [
        "SELECT max_seq,head_hash FROM log_heads WHERE network_id=?1 AND author=?2",
        "SELECT author FROM log_heads WHERE network_id=?1 AND author<>?2",
        "SELECT hash FROM events WHERE network_id=?1 AND scope_id='c' AND author=?2 AND main=1 ORDER BY seq DESC LIMIT 1",
        "SELECT 1 FROM events WHERE network_id=?1 AND author=?2 AND main=0 LIMIT 1",
        "SELECT lamport FROM events WHERE network_id=?1 AND hash=?2",
        "SELECT author,seq FROM events WHERE network_id=?1 AND hash=?2",
        "SELECT 1 FROM pending_events WHERE network_id=?1 AND hash=?2",
        "SELECT hash FROM events WHERE network_id=?1 AND author=?2 AND seq=1 AND main=1",
        "SELECT message_id FROM messages WHERE network_id=?1 AND conversation_id='c' AND message_id=?2",
    ];
    for sql in statements {
        let mut plan = store
            .connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let details: Vec<String> = plan
            .query_map(params![net.network, author], |r| r.get(3))?
            .collect::<rusqlite::Result<_>>()?;
        assert!(
            details
                .iter()
                .all(|d| !d.starts_with("SCAN") || d.contains("INDEX")),
            "{sql}: {details:?}"
        );
    }
    Ok(())
}

#[test]
#[ignore = "slow: 100k appends"]
fn append_100k_stays_flat() -> Result<()> {
    let net = net()?;
    let dir = std::env::temp_dir().join(format!("vibeface-100k-{}", uuid::Uuid::new_v4()));
    let mut store = Store::open(dir.join("db.sqlite"))?;
    store.install_trust_bundle(&net.network, &net.state)?;
    let batch = |store: &mut Store, from: i64| -> Result<std::time::Duration> {
        let started = std::time::Instant::now();
        for i in from..from + 1000 {
            let author = if i % 3 == 0 { &net.b } else { &net.a };
            store.append_event(
                author,
                &net.network,
                "c",
                user(&net, &format!("m{i}"), "x"),
                vec![],
                i,
            )?;
        }
        Ok(started.elapsed())
    };
    let first = batch(&mut store, 0)?;
    for k in 1..99 {
        batch(&mut store, k * 1000)?;
    }
    let last = batch(&mut store, 99_000)?;
    eprintln!("first 1000: {first:?}, last 1000: {last:?}");
    std::fs::remove_dir_all(&dir)?;
    assert!(last < first * 3, "append cost grew: {first:?} -> {last:?}");
    Ok(())
}

#[test]
fn autonomy_is_not_an_event() {
    let body = serde_json::json!({"type":"autonomy_set","mode":"off","version":1});
    assert!(
        serde_json::from_value::<EventBody>(body).is_err(),
        "the coordination Agent always runs with full permission; there is no autonomy setting"
    );
}

struct Fleet {
    net: Net,
    x: LocalIdentity,
    admin: LocalIdentity,
}

fn fleet() -> Result<Fleet> {
    let a = LocalIdentity::generate();
    let b = LocalIdentity::generate();
    let x = LocalIdentity::generate();
    let state = a.sign_network_fixture(1, vec![a.device_id()?, b.device_id()?, x.device_id()?])?;
    Ok(Fleet {
        admin: a.fixture_admin(),
        net: Net {
            network: a.fixture_network_id()?,
            a,
            b,
            state,
        },
        x,
    })
}

fn seq_of(envelope: &SignedEvent) -> Result<u64> {
    Ok(serde_json::from_str::<Event>(&envelope.canonical_body)?.seq)
}

fn revoke(
    fleet: &Fleet,
    state: &NetworkState,
    device: &LocalIdentity,
    last: Option<&SignedEvent>,
) -> Result<NetworkState> {
    use crate::identity::{GovernanceEvent, Revocation};
    let cutoff = last.map(|event| Ok::<_, anyhow::Error>((seq_of(event)?, event.hash.as_str())));
    let revocation = Revocation::new(&device.device_id()?, cutoff.transpose()?)?;
    let mut next = state.clone();
    next.append(GovernanceEvent::revoke(
        &fleet.admin,
        &state.project()?,
        revocation,
    )?)?;
    Ok(next)
}

fn grant(fleet: &Fleet, state: &NetworkState, node: &LocalIdentity) -> Result<NetworkState> {
    use crate::identity::GovernanceEvent;
    let mut next = state.clone();
    next.append(GovernanceEvent::grant(
        &fleet.admin,
        &state.project()?,
        Member::node(node, "granted")?,
    )?)?;
    Ok(next)
}

fn write(
    store: &mut Store,
    net: &Net,
    author: &LocalIdentity,
    id: &str,
    at: i64,
) -> Result<SignedEvent> {
    store.append_event(author, &net.network, "c", user(net, id, id), vec![], at)
}

fn created(store: &mut Store, net: &Net, author: &LocalIdentity) -> Result<SignedEvent> {
    store.append_event(
        author,
        &net.network,
        "c",
        EventBody::ConversationCreated { title: "t".into() },
        vec![],
        1,
    )
}

enum Piece {
    Governance(NetworkState),
    Events(Vec<SignedEvent>),
}

fn replay(net: &Net, pieces: &[Piece], order: &[usize]) -> Result<Store> {
    let mut replica = store(net)?;
    let events = pieces
        .iter()
        .filter(|piece| matches!(piece, Piece::Events(_)));
    for piece in order.iter().map(|index| &pieces[*index]).chain(events) {
        match piece {
            Piece::Governance(state) => {
                replica.install_trust_bundle(&net.network, state)?;
            }
            Piece::Events(events) => {
                replica.ingest_events(&net.network, events)?;
            }
        }
    }
    Ok(replica)
}

fn dump(store: &Store, network: &str) -> Result<Vec<String>> {
    let mut dump = Vec::new();
    for table in reduce::TABLES {
        let mut statement = store
            .connection
            .prepare(&format!("SELECT * FROM {table} WHERE network_id=?"))?;
        let columns = statement.column_count();
        let mut rows = statement
            .query_map([network], |r| {
                (0..columns)
                    .map(|i| r.get::<_, rusqlite::types::Value>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?
            .map(|row| Ok(format!("{table}: {:?}", row?)))
            .collect::<Result<Vec<_>>>()?;
        rows.sort();
        dump.extend(rows);
    }
    let mut statement = store
        .connection
        .prepare("SELECT hash FROM events WHERE network_id=? AND effective=1 ORDER BY hash")?;
    for hash in statement.query_map([network], |r| r.get::<_, String>(0))? {
        dump.push(format!("effective: {}", hash?));
    }
    Ok(dump)
}

fn permutations(n: usize) -> Vec<Vec<usize>> {
    if n == 0 {
        return vec![vec![]];
    }
    let mut all = Vec::new();
    for rest in permutations(n - 1) {
        for at in 0..=rest.len() {
            let mut order = rest.clone();
            order.insert(at, n - 1);
            all.push(order);
        }
    }
    all
}

fn converges(net: &Net, pieces: &[Piece]) -> Result<Store> {
    let orders = permutations(pieces.len());
    let dumps = orders
        .iter()
        .map(|order| dump(&replay(net, pieces, order)?, &net.network))
        .collect::<Result<Vec<_>>>()?;
    for (order, found) in orders.iter().zip(&dumps) {
        assert_eq!(
            found, &dumps[0],
            "arrival order {order:?} against {:?}",
            orders[0]
        );
    }
    let reference = replay(net, pieces, &orders[0])?;
    reference.connection.execute(
        "UPDATE events SET effective=0 WHERE network_id=?",
        [&net.network],
    )?;
    reduce::clear(&reference.connection, &net.network)?;
    log::recompute(
        &reference.connection,
        &membership_from(&reference.connection, &net.network)?,
    )?;
    assert_eq!(
        dump(&reference, &net.network)?,
        dumps[0],
        "rebuild from scratch"
    );
    Ok(reference)
}

fn effective(store: &Store, net: &Net, events: &[&SignedEvent]) -> Result<Vec<bool>> {
    events
        .iter()
        .map(|event| {
            Ok(store
                .connection
                .query_row(
                    "SELECT effective FROM events WHERE network_id=? AND hash=?",
                    params![net.network, event.hash],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(false))
        })
        .collect()
}

#[test]
fn a_revocation_cutoff_converges_whatever_arrived_first() -> Result<()> {
    let fleet = fleet()?;
    let net = &fleet.net;
    let mut origin = store(net)?;
    let created = created(&mut origin, net, &net.a)?;
    let mut writes = vec![];
    for i in 1..=10 {
        writes.push(write(&mut origin, net, &fleet.x, &format!("x{i}"), 10 + i)?);
    }
    let answer = write(&mut origin, net, &net.b, "b1", 30)?;
    let revoked = revoke(&fleet, &net.state, &fleet.x, Some(&writes[6]))?;
    let pieces = [
        Piece::Events([vec![created], writes[..7].to_vec()].concat()),
        Piece::Events([writes[7..].to_vec(), vec![answer.clone()]].concat()),
        Piece::Governance(revoked),
    ];
    let reference = converges(net, &pieces)?;
    let texts: Vec<_> = reference
        .messages(&net.network, "c")?
        .into_iter()
        .map(|m| m.text)
        .collect();
    let mut expected: Vec<_> = (1..=7).map(|i| format!("x{i}")).collect();
    expected.push("b1".into());
    assert_eq!(texts.len(), expected.len());
    assert!(
        expected.iter().all(|text| texts.contains(text)),
        "{texts:?}"
    );
    assert_eq!(
        effective(&reference, net, &[&writes[7], &writes[9], &answer])?,
        vec![false, false, true]
    );
    Ok(())
}

#[test]
fn a_revocation_arriving_before_the_grant_still_wins() -> Result<()> {
    let fleet = fleet()?;
    let net = &fleet.net;
    let y = LocalIdentity::generate();
    let granted = grant(&fleet, &net.state, &y)?;
    let concurrent = revoke(&fleet, &net.state, &y, None)?;
    let mut origin = store(net)?;
    origin.install_trust_bundle(&net.network, &granted)?;
    let created = created(&mut origin, net, &net.a)?;
    let mut writes = vec![created.clone()];
    for i in 1..=3 {
        writes.push(write(&mut origin, net, &y, &format!("y{i}"), 10 + i)?);
    }
    let pieces = [
        Piece::Events(writes.clone()),
        Piece::Governance(granted),
        Piece::Governance(concurrent),
    ];
    let reference = converges(net, &pieces)?;
    assert!(reference.messages(&net.network, "c")?.is_empty());
    assert_eq!(
        effective(&reference, net, &[&created, &writes[1], &writes[3]])?,
        vec![true, false, false]
    );
    Ok(())
}

#[test]
fn a_checkpoint_racing_a_grant_converges() -> Result<()> {
    use crate::identity::{Revocation, RootCheckpoint};
    let fleet = fleet()?;
    let net = &fleet.net;
    let z = LocalIdentity::generate();
    let granted = grant(&fleet, &net.state, &z)?;
    let mut origin = store(net)?;
    origin.install_trust_bundle(&net.network, &granted)?;
    let created = created(&mut origin, net, &net.a)?;
    let mut x_writes = vec![];
    for i in 1..=5 {
        x_writes.push(write(&mut origin, net, &fleet.x, &format!("x{i}"), 10 + i)?);
    }
    let z_write = write(&mut origin, net, &z, "z1", 20)?;
    let mut recovered = net.state.clone();
    recovered.recover(RootCheckpoint::recovery(
        &net.state,
        &net.a.fixture_root_at(2),
        &net.a.fixture_root_at(3).device_id()?,
        vec![
            Member::client(&fleet.admin, "fixture-admin", true)?,
            Member::node(&net.a, "a")?,
            Member::node(&net.b, "b")?,
        ],
        vec![Revocation::new(
            &fleet.x.device_id()?,
            Some((3, &x_writes[2].hash)),
        )?],
        0,
    )?)?;
    let pieces = [
        Piece::Events([vec![created], x_writes.clone(), vec![z_write.clone()]].concat()),
        Piece::Governance(granted),
        Piece::Governance(recovered),
    ];
    let reference = converges(net, &pieces)?;
    assert_eq!(
        effective(
            &reference,
            net,
            &[&x_writes[2], &x_writes[3], &x_writes[4], &z_write]
        )?,
        vec![true, false, false, false]
    );
    Ok(())
}

#[test]
fn a_forked_log_converges_and_a_revocation_picks_a_branch() -> Result<()> {
    let fleet = fleet()?;
    let net = &fleet.net;
    let mut first = store(net)?;
    let x1 = created(&mut first, net, &fleet.x)?;
    let x2 = write(&mut first, net, &fleet.x, "x2", 2)?;
    let x3a = write(&mut first, net, &fleet.x, "x3a", 3)?;
    let w1 = write(&mut first, net, &net.b, "w1", 4)?;
    let x4a = write(&mut first, net, &fleet.x, "x4a", 5)?;
    let mut second = store(net)?;
    second.ingest_events(&net.network, &[x1.clone(), x2.clone()])?;
    let x3b = write(&mut second, net, &fleet.x, "x3b", 3)?;
    let x4b = write(&mut second, net, &fleet.x, "x4b", 4)?;
    assert!(
        serde_json::from_str::<Event>(&w1.canonical_body)?
            .deps
            .contains(&x3a.hash)
    );
    let base = Piece::Events(vec![x1.clone(), x2.clone()]);
    let branch_a = Piece::Events(vec![x3a.clone(), w1.clone(), x4a.clone()]);
    let branch_b = Piece::Events(vec![x3b.clone(), x4b.clone()]);
    let all = [&x1, &x2, &x3a, &x4a, &x3b, &x4b, &w1];

    let a_only = replay(net, &[base, branch_a], &[0, 1])?;
    assert_eq!(
        effective(&a_only, net, &all[..4])?,
        vec![true, true, true, true]
    );
    let base = Piece::Events(vec![x1.clone(), x2.clone()]);
    let branch_a = Piece::Events(vec![x3a.clone(), w1.clone(), x4a.clone()]);
    let b_only = replay(net, &[base, branch_b], &[0, 1])?;
    assert_eq!(effective(&b_only, net, &[&x3b, &x4b])?, vec![true, true]);

    let base = Piece::Events(vec![x1.clone(), x2.clone()]);
    let branch_b = Piece::Events(vec![x3b.clone(), x4b.clone()]);
    let both = converges(net, &[base, branch_a, branch_b])?;
    assert_eq!(
        effective(&both, net, &all)?,
        vec![true, true, false, false, false, false, true]
    );
    assert!(both.has_conflicts_by(&net.network, &fleet.x.device_id()?)?);
    assert!(!both.conflicts(&net.network)?.is_empty());

    let picked = revoke(&fleet, &net.state, &fleet.x, Some(&x4b))?;
    let pieces = [
        Piece::Events(vec![x1.clone(), x2.clone()]),
        Piece::Events(vec![x3a.clone(), w1.clone(), x4a.clone()]),
        Piece::Events(vec![x3b.clone(), x4b.clone()]),
        Piece::Governance(picked),
    ];
    let reference = converges(net, &pieces)?;
    assert_eq!(
        effective(&reference, net, &all)?,
        vec![true, true, false, false, true, true, true]
    );
    assert!(
        reference.conflicts(&net.network)?.is_empty(),
        "once removed, its fork no longer asks the user to act"
    );
    Ok(())
}

#[test]
fn a_retired_bound_holds_across_recovery_and_old_attestations_verify() -> Result<()> {
    use crate::identity::{Revocation, RootCheckpoint};
    let fleet = fleet()?;
    let net = &fleet.net;
    let mut origin = store(net)?;
    let created = created(&mut origin, net, &net.a)?;
    let mut writes = vec![];
    for i in 1..=8 {
        writes.push(write(&mut origin, net, &fleet.x, &format!("x{i}"), 10 + i)?);
    }
    let replacement = LocalIdentity::generate();
    let mut recovered = net.state.clone();
    recovered.recover(RootCheckpoint::recovery(
        &net.state,
        &net.a.fixture_root_at(2),
        &net.a.fixture_root_at(3).device_id()?,
        vec![
            Member::client(&replacement, "replacement", true)?,
            Member::node(&net.a, "a")?,
            Member::node(&net.b, "b")?,
        ],
        vec![Revocation::new(
            &fleet.x.device_id()?,
            Some((5, &writes[4].hash)),
        )?],
        0,
    )?)?;
    let view = recovered.project()?;
    assert!(view.client(&fleet.admin.device_id()?).is_err());
    assert!(view.attester(&fleet.admin.device_id()?));

    let mut late = store(net)?;
    late.install_trust_bundle(&net.network, &recovered)?;
    let (statuses, _) = late.ingest_events(
        &net.network,
        &[vec![created.clone()], writes.clone()].concat(),
    )?;
    assert!(
        statuses.iter().all(|s| *s == IngestStatus::Accepted),
        "{statuses:?}"
    );

    let pieces = [
        Piece::Events([vec![created], writes.clone()].concat()),
        Piece::Governance(recovered),
    ];
    let reference = converges(net, &pieces)?;
    assert_eq!(
        effective(
            &reference,
            net,
            &[&writes[0], &writes[4], &writes[5], &writes[7]]
        )?,
        vec![true, true, false, false]
    );
    assert_eq!(dump(&late, &net.network)?, dump(&reference, &net.network)?);
    Ok(())
}

#[test]
fn a_rebuild_never_touches_local_ledgers() -> Result<()> {
    let fleet = fleet()?;
    let net = &fleet.net;
    let mut store = store(net)?;
    conversation(&mut store, net)?;
    store.accept_message(
        &net.a,
        &net.network,
        "c",
        "m1",
        "hi",
        vec![],
        true,
        signature(net, "m1", "hi")?,
        2,
    )?;
    write(&mut store, net, &fleet.x, "x1", 3)?;
    let ledgers = |store: &Store| -> Result<Vec<String>> {
        let derived = [
            "events",
            "event_blobs",
            "log_heads",
            "pending_events",
            "networks",
        ];
        let tables: Vec<String> = store
            .connection
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut rows = Vec::new();
        for table in tables
            .iter()
            .filter(|t| !reduce::TABLES.contains(&t.as_str()) && !derived.contains(&t.as_str()))
        {
            let mut statement = store
                .connection
                .prepare(&format!("SELECT * FROM {table}"))?;
            let columns = statement.column_count();
            let mut found = statement
                .query_map([], |r| {
                    (0..columns)
                        .map(|i| r.get::<_, rusqlite::types::Value>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })?
                .map(|row| Ok(format!("{table}: {:?}", row?)))
                .collect::<Result<Vec<_>>>()?;
            found.sort();
            rows.extend(found);
        }
        Ok(rows)
    };
    let before = ledgers(&store)?;
    assert!(before.iter().any(|row| row.starts_with("scheduler_jobs:")));
    assert_eq!(store.messages(&net.network, "c")?.len(), 2);
    assert!(
        store.install_trust_bundle(&net.network, &revoke(&fleet, &net.state, &fleet.x, None)?)?
    );
    assert_eq!(store.messages(&net.network, "c")?.len(), 1);
    assert_eq!(ledgers(&store)?, before);
    Ok(())
}

fn question(net: &Net, id: &str, session: &str) -> Result<EventBody> {
    Ok(EventBody::QuestionAsked {
        question_id: id.into(),
        severity: Severity::Attention,
        text: "要继续吗？".into(),
        choices: vec![Choice {
            label: "继续".into(),
        }],
        about: Subject {
            node: net.a.device_id()?,
            agent: "codex".into(),
            session_id: session.into(),
            cwd: "/w".into(),
            summary: "tests fail".into(),
            request_id: None,
        },
    })
}

fn answer_signature(net: &Net, scope: &str, question: &str, text: &str) -> Result<Attestation> {
    net.a
        .fixture_admin()
        .attest(
            &net.network,
            scope,
            EventBody::MessagePosted {
                message_id: questions::answer_id(question),
                role: Role::User,
                text: text.into(),
                attachments: vec![],
                report_results: Some(true),
                in_reply_to: None,
                status: None,
                answers: Some(question.into()),
                attestation: None,
            },
        )?
        .attestation()
        .cloned()
        .context("signed answer")
}

fn sync(from: &Store, to: &mut Store, net: &Net) -> Result<()> {
    let envelopes: Vec<SignedEvent> = from
        .connection
        .prepare("SELECT envelope FROM events WHERE network_id=? ORDER BY author,seq")?
        .query_map([&net.network], |r| r.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect::<Result<_>>()?;
    to.ingest_events(&net.network, &envelopes)?;
    Ok(())
}

fn unanswered(store: &Store, net: &Net) -> Result<Vec<String>> {
    let mut ids: Vec<String> = store
        .unanswered_questions(&net.network)?
        .iter()
        .map(|question| {
            question["question_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    ids.sort();
    Ok(ids)
}

#[test]
fn an_answer_reaches_the_agent_once_across_retries_and_restarts() -> Result<()> {
    let net = net()?;
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.artifacts")
        .join(format!("store-{}", uuid::Uuid::new_v4()));
    crate::config::private_dir(&dir)?;
    let path = dir.join("node.sqlite3");
    let result = (|| -> Result<()> {
        let mut store = Store::open(&path)?;
        store.install_trust_bundle(&net.network, &net.state)?;
        conversation(&mut store, &net)?;
        store.ask(&net.a, &net.network, "c", question(&net, "q-1", "t1")?, 2)?;
        let signed = Some(answer_signature(&net, "c", "q-1", "允许")?);
        let first = store.accept_answer(&net.a, &net.network, "q-1", "允许", signed.clone(), 3)?;
        assert!(!first.already_answered && first.receipt.is_some());
        let retry = store.accept_answer(&net.a, &net.network, "q-1", "允许", signed, 4)?;
        assert!(!retry.already_answered && retry.message == first.message);
        let webhook = store.accept_answer(&net.a, &net.network, "q-1", "拒绝", None, 5)?;
        assert!(webhook.already_answered && webhook.message.text == "允许");
        drop(store);
        let mut store = Store::open(&path)?;
        let trigger = message_trigger(&questions::answer_id("q-1"));
        store.admit_delivery(&net.a.device_id()?, &net.network, "c", &trigger)?;
        let jobs = store.scheduler_jobs("c")?;
        assert_eq!(
            jobs.iter()
                .map(|job| (job.trigger.clone(), job.kind))
                .collect::<Vec<_>>(),
            vec![(trigger, TriggerKind::Answer)]
        );
        let answers = store
            .context(&net.network, "c", None)?
            .events
            .iter()
            .filter(|event| {
                matches!(
                    &event.event.body,
                    EventBody::MessagePosted {
                        answers: Some(_),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(answers, 1, "one answer event");
        assert!(unanswered(&store, &net)?.is_empty());
        Ok(())
    })();
    std::fs::remove_dir_all(dir)?;
    result
}

#[test]
fn an_answer_on_one_device_is_answered_for_every_reader() -> Result<()> {
    let net = net()?;
    let (mut a, mut b) = (store(&net)?, store(&net)?);
    conversation(&mut a, &net)?;
    a.ask(&net.a, &net.network, "c", question(&net, "q-1", "t1")?, 2)?;
    sync(&a, &mut b, &net)?;
    assert_eq!(unanswered(&b, &net)?, vec!["q-1".to_owned()]);
    let signed = answer_signature(&net, "c", "q-1", "继续")?;
    let signer = signed.device_id.clone();
    let answered = b.accept_answer(&net.b, &net.network, "q-1", "继续", Some(signed), 3)?;
    assert!(b.scheduler_jobs("c")?.is_empty());
    let delivery = b.scheduler_outbox()?;
    assert_eq!(
        delivery
            .iter()
            .map(|entry| entry.trigger.clone())
            .collect::<Vec<_>>(),
        vec![answered.receipt.context("queued for the owner")?.trigger]
    );
    sync(&b, &mut a, &net)?;
    let trigger = &delivery[0].trigger;
    a.admit_delivery(&net.a.device_id()?, &net.network, "c", trigger)?;
    assert_eq!(a.scheduler_jobs("c")?[0].kind, TriggerKind::Answer);
    for store in [&a, &b] {
        assert!(unanswered(store, &net)?.is_empty());
        let questions = store.conversation_questions(&net.network, "c")?;
        assert_eq!(questions[0]["answer"]["text"], "继续");
        assert_eq!(questions[0]["answer"]["message_id"], "a-q-1");
        assert_eq!(
            questions[0]["answer"]["author"],
            signer.as_str(),
            "the signing device answered, not the entry Node"
        );
    }
    Ok(())
}

#[test]
fn the_pending_list_holds_exactly_the_unanswered_questions() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    let owner = net
        .a
        .owner_fixture(&net.network, "d", &net.a.device_id()?)?;
    store.create_conversation(&net.a, &net.network, "d", "t", owner, 1)?;
    for (scope, id) in [("c", "q-1"), ("c", "q-2"), ("d", "q-3")] {
        store.ask(&net.a, &net.network, scope, question(&net, id, id)?, 2)?;
    }
    let signed = Some(answer_signature(&net, "c", "q-2", "继续")?);
    store.accept_answer(&net.a, &net.network, "q-2", "继续", signed, 3)?;
    assert_eq!(unanswered(&store, &net)?, vec!["q-1", "q-3"]);
    let pending = store.unanswered_questions(&net.network)?;
    assert!(pending.iter().all(|q| q["conversation_id"] != Value::Null));
    let chat = store.conversation_questions(&net.network, "c")?;
    assert_eq!(
        chat.iter()
            .map(|q| (q["question_id"].clone(), q["answer"]["text"].clone()))
            .collect::<Vec<_>>(),
        vec![
            (serde_json::json!("q-1"), Value::Null),
            (serde_json::json!("q-2"), serde_json::json!("继续"))
        ]
    );
    Ok(())
}

#[test]
fn a_session_owner_is_written_once_and_follows_the_users_latest_choice() -> Result<()> {
    let net = net()?;
    let (mut local, _) = bot_with_message(&net)?;
    second_bot(&mut local, &net, "Other")?;
    let node = net.a.device_id()?;
    let about = Subject {
        node: node.clone(),
        agent: "codex".into(),
        session_id: "s1".into(),
        cwd: "/w".into(),
        summary: "tests fail".into(),
        request_id: None,
    };
    let owner = |store: &Store| store.session_owner(&net.network, &node, "s1");
    assert!(
        local
            .claim_session(&net.a, &net.network, "c", &about, 3)?
            .is_some()
    );
    assert!(
        local
            .claim_session(&net.a, &net.network, "d", &about, 4)?
            .is_none(),
        "an owned session is claimed once"
    );
    assert!(
        local
            .claim_session(&net.b, &net.network, "c", &about, 4)
            .is_err()
    );
    assert_eq!(owner(&local)?.as_deref(), Some("c"));
    assert!(
        local
            .hand_over_session(&net.a, &net.network, "s1", Some("nope"), 5)
            .is_err()
    );
    assert!(
        local
            .hand_over_session(&net.a, &net.network, "s2", None, 5)
            .is_err()
    );

    let (before, moved) = local.hand_over_session(&net.a, &net.network, "s1", Some("d"), 5)?;
    assert_eq!((before.as_str(), moved.is_some()), ("c", true));
    let again = local.hand_over_session(&net.a, &net.network, "s1", Some("d"), 6)?;
    assert!(again.1.is_none(), "a repeated hand-over writes nothing");
    assert_eq!(owner(&local)?.as_deref(), Some("d"));
    assert!(local.attributed_sessions(&net.network, "c")?.is_empty());

    assert!(
        local
            .hand_over_session(&net.a, &net.network, "s1", None, 7)?
            .1
            .is_some()
    );
    assert!(
        local
            .hand_over_session(&net.a, &net.network, "s1", None, 8)?
            .1
            .is_none()
    );
    assert!(local.session_muted(&net.network, &node, "s1")?);
    assert!(
        local
            .claim_session(&net.a, &net.network, "c", &about, 9)?
            .is_none(),
        "a muted session is not claimed again"
    );
    let rows = local.attributed_sessions(&net.network, "d")?;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0]["muted"].clone(),
            rows[0]["summary"].clone(),
            rows[0]["agent"].clone()
        ),
        (json!(true), json!("tests fail"), json!("codex"))
    );

    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &local)?;
    assert_eq!(owner(&other)?.as_deref(), Some("d"));
    assert!(other.session_muted(&net.network, &node, "s1")?);
    other.connection.execute(
        "UPDATE events SET effective=0 WHERE network_id=?",
        [&net.network],
    )?;
    reduce::clear(&other.connection, &net.network)?;
    log::recompute(
        &other.connection,
        &membership_from(&other.connection, &net.network)?,
    )?;
    assert_eq!(
        owner(&other)?.as_deref(),
        Some("d"),
        "a rebuild reads the same"
    );
    assert!(other.session_muted(&net.network, &node, "s1")?);

    local.hand_over_session(&net.a, &net.network, "s1", Some("c"), 10)?;
    assert_eq!(owner(&local)?.as_deref(), Some("c"));
    assert!(
        !local.session_muted(&net.network, &node, "s1")?,
        "handing a muted session to a bot follows it again"
    );
    Ok(())
}

#[test]
fn a_failed_turn_replies_in_plain_words_and_keeps_the_raw_detail_local() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    let raw = "CURSOR_API_KEY is required for the Cursor SDK manager";
    for (id, outcome) in [
        ("m1", JobOutcome::Failed(raw.into())),
        (
            "m2",
            JobOutcome::Interrupted {
                detail: raw.into(),
                process_unconfirmed: true,
            },
        ),
    ] {
        store.accept_message(
            &net.a,
            &net.network,
            "c",
            id,
            "hi",
            vec![],
            true,
            signature(&net, id, "hi")?,
            2,
        )?;
        let claimed = store
            .claim_next(&net.a, &net.network, "c", "boot-1", "cfg")?
            .context("job")?;
        let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
        store.mark_started("c", &claimed.job.trigger, &attempt)?;
        let job = store.finish_job(
            &net.a,
            &net.network,
            "c",
            &claimed.job.trigger,
            &attempt,
            outcome,
            "",
            3,
        )?;
        let reply = store
            .message(&net.network, "c", &claimed.job.message_id)?
            .context("reply")?;
        assert!(!reply.text.contains("CURSOR_API_KEY"), "{}", reply.text);
        assert!(reply.text.contains("设备 → Bot 与任务"), "{}", reply.text);
        assert!(
            matches!(&job.state, JobState::Failed { detail, .. } | JobState::Interrupted { detail, .. } if detail == raw),
            "the raw cause stays on the local job"
        );
    }
    Ok(())
}

#[test]
fn last_messages_preview_each_bots_newest_message() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    store.append_event(&net.a, &net.network, "c", bot(&net, "Helper")?, vec![], 2)?;
    assert_eq!(store.last_messages(&net.network)?.get("c"), None);

    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m1", "hello"),
        vec![],
        3,
    )?;
    store.append_event(&net.a, &net.network, "c", bot_reply("a1", "m1"), vec![], 4)?;
    let last = store
        .last_messages(&net.network)?
        .remove("c")
        .expect("newest message");
    assert_eq!(
        (last.text.as_str(), last.role.as_str(), last.created_at_ms),
        ("ok", "assistant", 4)
    );

    let long = "长".repeat(200);
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m2", &long),
        vec![],
        5,
    )?;
    let last = store
        .last_messages(&net.network)?
        .remove("c")
        .expect("newest message");
    assert_eq!(last.text, format!("{}…", "长".repeat(120)));
    assert_eq!(
        (last.role.as_str(), last.author),
        ("user", net.a.fixture_admin().device_id()?),
        "the signing device wrote it, not the entry Node"
    );
    Ok(())
}

fn prompt_data(prompt: &str) -> Result<Value> {
    let start = prompt.rfind("\n\n{").context("prompt data")?;
    Ok(serde_json::from_str(&prompt[start + 2..])?)
}

fn listed(data: &Value, pointer: &str, key: &str) -> Vec<Value> {
    data.pointer(pointer)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|row| row[key].clone())
        .collect()
}

#[test]
fn an_oversized_turn_prompt_keeps_the_newest_context_and_fits() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    let text = "x".repeat(3_000);
    for i in 0..40 {
        let message = user(&net, &format!("m{i:02}"), &text);
        store.append_event(&net.a, &net.network, "c", message, vec![], 10 + i)?;
    }
    let context = store.context(&net.network, "c", None)?;
    let checkpoint = crate::scheduler::checkpoint(&context, &[]).context("over budget")?;
    store.append_event(&net.a, &net.network, "c", checkpoint, vec![], 60)?;
    for i in 40..50 {
        let message = user(&net, &format!("m{i:02}"), &text);
        store.append_event(&net.a, &net.network, "c", message, vec![], 100 + i)?;
    }
    for i in 0..30 {
        let spec = WorkSpec {
            work_id: format!("w{i:02}"),
            conversation: "c".into(),
            trigger: message_trigger("m49"),
            target: net.a.device_id()?,
            dispatcher: net.a.device_id()?,
            call_ref: format!("call-{i}"),
            kind: WorkKind::Codex,
            cwd: "/tmp".into(),
            title: "t".into(),
            prompt: "p".repeat(600),
            continues: None,
            parent: None,
            thread: None,
        };
        store.accept_work(&net.a, &net.network, spec, 200 + i)?;
    }
    let context = store.context(&net.network, "c", None)?;
    let works = store.works(&net.network, "c")?;
    let job = SchedulerJob {
        conversation_id: "c".into(),
        trigger: message_trigger("m49"),
        kind: TriggerKind::Human,
        sequence: 0,
        basis: context.basis.clone(),
        reply_to: "m49".into(),
        message_id: "reply".into(),
        state: JobState::Queued,
    };
    let materialize = |bot: Value| {
        crate::scheduler::materialize(&context, &job, &works, bot, None, &[], None, None)
            .map(|(prompt, _)| prompt)
    };
    let newest = json!(
        context
            .events
            .iter()
            .rposition(|event| matches!(event.event.body, EventBody::MessagePosted { .. }))
    );

    let roomy = prompt_data(&materialize(json!({}))?)?;
    let roomy_works = listed(&roomy, "/works", "work_id");
    assert_eq!(
        roomy_works.last(),
        Some(&json!("w29")),
        "the newest work stays"
    );
    assert!(
        roomy_works
            .windows(2)
            .all(|pair| pair[0].as_str() < pair[1].as_str()),
        "works read oldest to newest"
    );
    assert_eq!(
        roomy["earlier_works_omitted"],
        json!(30 - roomy_works.len())
    );
    assert!(roomy["conversation"]["checkpoint"].is_object());
    assert_eq!(
        listed(&roomy, "/conversation/messages", "offset").last(),
        Some(&newest)
    );

    let notes = (0..8)
        .map(|i| json!({"memory_id":format!("n{i}"),"text":"y".repeat(400)}))
        .collect::<Vec<_>>();
    let full_bot = json!({"configured_bot":{"name":"Helper","instructions":"i".repeat(MAX_INSTRUCTIONS_BYTES)},
        "skills":notes,"schedules":notes,"memory":notes,"bots":notes});
    let prompt = materialize(full_bot)?;
    assert!(prompt.len() <= crate::scheduler::PROMPT_LIMIT);
    let tight = prompt_data(&prompt)?;
    let offsets = listed(&tight, "/conversation/messages", "offset");
    assert_eq!(offsets.last(), Some(&newest), "the trigger message stays");
    assert!(
        offsets
            .windows(2)
            .all(|pair| pair[0].as_u64() < pair[1].as_u64())
    );
    assert!(
        listed(&tight, "/works", "work_id").len() < roomy_works.len(),
        "works give way first"
    );
    Ok(())
}

#[test]
fn the_bot_prompt_keeps_its_newest_notes() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    for i in 0..12 {
        store.change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            &format!("k{i}"),
            ChangeRequest::Remember {
                memory_id: None,
                text: format!("Note {i:02}: {}", "keep replies short. ".repeat(20)),
            },
            Provider::Codex,
            10 + i,
        )?;
    }
    let bot = crate::scheduler::bot_value(&store, &net.network, "c", 100)?;
    let notes: Vec<String> = listed(&bot, "/memory", "text")
        .iter()
        .filter_map(|text| Some(text.as_str()?[..7].to_owned()))
        .collect();
    let omitted = bot["memory_omitted"].as_u64().context("omitted")? as usize;
    assert!(omitted > 0 && notes.len() + omitted == 12);
    let expected: Vec<String> = (12 - notes.len()..12)
        .map(|i| format!("Note {i:02}"))
        .collect();
    assert_eq!(notes, expected, "the newest notes, oldest first");
    Ok(())
}

#[test]
fn a_schedule_time_found_too_late_is_skipped() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
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

#[test]
fn a_queued_turn_reads_the_reply_ahead_of_it() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    for (id, at) in [("m1", 2), ("m2", 3)] {
        let signed = signature(&net, id, "question")?;
        store.accept_message(
            &net.a,
            &net.network,
            "c",
            id,
            "question",
            vec![],
            true,
            signed,
            at,
        )?;
    }
    let first = store
        .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
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
        .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
        .context("second turn")?;
    assert_eq!(second.job.trigger, message_trigger("m2"));
    let read = store.context(&net.network, "c", Some(&second.job.basis))?;
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
    conversation(&mut store, &net)?;
    let turn =
        |store: &mut Store, id: &str, started: bool| -> Result<crate::scheduling::ClaimedJob> {
            say(store, &net, id, true)?;
            let claimed = store
                .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
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
                JobOutcome::Failed("the model is unknown".into())
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
        .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
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
    conversation(&mut store, &net)?;
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
            .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
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
    conversation(&mut a_side, &net)?;
    let local = net.a.device_id()?;
    say(&mut a_side, &net, "m1", true)?;
    let first = a_side
        .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
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
    assert!(a_side.begin_steer(&net.network, &local, "boot", "c", &m2, &into)?);
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
        .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
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
    conversation(&mut a_side, &net)?;
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
        .claim_next(&net.b, &net.network, "c", "boot", "cfg")?
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

#[test]
fn a_resumed_codex_turn_sends_only_what_changed() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let work = |i: u64| -> Result<WorkSpec> {
        Ok(WorkSpec {
            work_id: format!("w{i}"),
            conversation: "c".into(),
            trigger: message_trigger("m1"),
            target: net.a.device_id()?,
            dispatcher: net.a.device_id()?,
            call_ref: format!("call-{i}"),
            kind: WorkKind::Codex,
            cwd: "/tmp".into(),
            title: "t".into(),
            prompt: "p".repeat(300),
            continues: None,
            parent: None,
            thread: None,
        })
    };
    for i in 0..3 {
        store.accept_work(&net.a, &net.network, work(i)?, 10 + i as i64)?;
    }
    let render = |store: &Store, trigger: &str, resumed, thread| {
        let context = store.context(&net.network, "c", None)?;
        let job = SchedulerJob {
            conversation_id: "c".into(),
            trigger: message_trigger(trigger),
            kind: TriggerKind::Human,
            sequence: 0,
            basis: context.basis.clone(),
            reply_to: trigger.into(),
            message_id: "reply".into(),
            state: JobState::Queued,
        };
        let works = store.works(&net.network, "c")?;
        let bot = crate::scheduler::bot_value(store, &net.network, "c", 100)?;
        let rendered =
            crate::scheduler::materialize(&context, &job, &works, bot, None, &[], resumed, thread)?;
        Ok::<_, anyhow::Error>((rendered, context.basis))
    };
    let rule = "Operate only inside this personal network";

    let ((cursor, _), _) = render(&store, "m1", None, None)?;
    assert!(
        cursor.contains(rule),
        "Cursor reads the rules in each prompt"
    );
    let fresh = Shown::default();
    let ((first, shown), covers) = render(&store, "m1", None, Some(&fresh))?;
    assert!(!first.contains(rule), "the rules are thread instructions");
    let data = prompt_data(&first)?;
    assert_eq!(data["bot"]["configured_bot"]["name"], "Helper");
    assert_eq!(listed(&data, "/works", "work_id").len(), 3);
    assert_eq!(data["trigger"]["unavailable_tools"], json!([]));

    store.accept_work(&net.a, &net.network, work(3)?, 20)?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m2", "next"),
        vec![],
        21,
    )?;
    let ((resumed, shown), covers) = render(&store, "m2", Some(&covers), Some(&shown))?;
    let data = prompt_data(&resumed)?;
    assert_eq!(data["bot"], json!({"unchanged":true,"now_ms":100}));
    assert_eq!(listed(&data, "/works", "work_id"), [json!("w3")]);
    assert_eq!(data["works_unchanged"], 3);
    assert_eq!(
        listed(&data, "/conversation/messages", "text"),
        [json!("next")]
    );

    store.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m2",
        "note",
        ChangeRequest::Remember {
            memory_id: None,
            text: "prefers short replies".into(),
        },
        Provider::Codex,
        22,
    )?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m3", "again"),
        vec![],
        23,
    )?;
    let ((changed, _), _) = render(&store, "m3", Some(&covers), Some(&shown))?;
    let data = prompt_data(&changed)?;
    assert_eq!(data["bot"]["memory"][0]["text"], "prefers short replies");
    assert_eq!(data["works"], json!([]));
    Ok(())
}

#[test]
fn a_new_session_key_opens_a_new_thread_shown_everything() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    let shown = Shown {
        bot: Some("bot-digest".into()),
        works: [("w1".to_owned(), "work-digest".to_owned())].into(),
    };
    let mut turn = |id: &str, key: &str| -> Result<crate::scheduling::ClaimedJob> {
        say(&mut store, &net, id, true)?;
        let claimed = store
            .claim_next(&net.a, &net.network, "c", "boot", key)?
            .context("job")?;
        let trigger = claimed.job.trigger.clone();
        let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
        store.mark_started("c", &trigger, &attempt)?;
        store.save_scheduler_native_id("c", &trigger, &attempt, "thread", "token", false)?;
        store.record_manager_turn(&attempt, "system", "thread", Some("turn"), &shown)?;
        store.finish_job(
            &net.a,
            &net.network,
            "c",
            &trigger,
            &attempt,
            JobOutcome::Completed,
            "",
            3,
        )?;
        Ok(claimed)
    };
    assert_eq!(turn("m1", "cfg")?.shown, Shown::default());
    let resumed = turn("m2", "cfg")?;
    assert_eq!(resumed.previous_native_id.as_deref(), Some("thread"));
    assert_eq!(resumed.shown, shown);
    let fresh = turn("m3", "cfg-new-instructions")?;
    assert_eq!(fresh.previous_native_id, None);
    assert_eq!(fresh.shown, Shown::default());
    Ok(())
}
