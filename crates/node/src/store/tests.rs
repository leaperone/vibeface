use super::*;
use crate::{
    crypto::LocalIdentity,
    scheduling::{JobOutcome, JobState, TriggerKind, message_trigger, work_trigger},
    work::{Signal, WorkSpec},
};

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

/// A user message in conversation `c`, signed by the network's client device.
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

/// Two devices write concurrently; any interleaving of their logs converges to the same projections.
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
    // Same message_id from both devices: the earlier one in the shared order wins everywhere.
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
    let statuses = b_first.ingest_events(&net.network, &b_events)?;
    assert!(
        statuses
            .iter()
            .all(|s| matches!(s, IngestStatus::Pending | IngestStatus::Rejected(_)))
    );
    b_first.ingest_events(&net.network, &a_events)?;
    // Events rejected for a gap are fetched again by sync; a second pass completes the log.
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
        target.ingest_events(&net.network, std::slice::from_ref(&first))?,
        vec![IngestStatus::Accepted]
    );
    assert_eq!(
        target.ingest_events(&net.network, std::slice::from_ref(&first))?,
        vec![IngestStatus::Duplicate]
    );
    assert!(
        matches!(target.ingest_events(&net.network, &[third])?[0], IngestStatus::Rejected(ref e) if e.contains("gap"))
    );
    let mut forged = first.clone();
    forged.signature = "00".repeat(64);
    assert_eq!(
        target.ingest_events(&net.network, &[forged])?,
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
        matches!(target.ingest_events(&net.network, &[foreign])?[0], IngestStatus::Rejected(ref e) if e.contains("unknown author"))
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
        target.ingest_events(&net.network, &[tampered])?[0],
        IngestStatus::Rejected(_)
    ));
    Ok(())
}

/// A device that forks its own log does not block the conversation for anyone else.
#[test]
fn a_log_conflict_keeps_the_conversation_appendable() -> Result<()> {
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
        target.ingest_events(&net.network, &[created, kept, fork, after_fork])?,
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
        target.ingest_events(&net.network, &[next])?,
        vec![IngestStatus::Accepted]
    );
    let texts: Vec<_> = target
        .messages(&net.network, "c")?
        .into_iter()
        .map(|m| m.text)
        .collect();
    assert_eq!(texts, vec!["kept", "still works", "a continues"]);
    assert!(target.has_conflicts_by(&net.network, &net.a.device_id()?)?);
    assert!(!target.has_conflicts_by(&net.network, &net.b.device_id()?)?);
    let conflicts = target.conflicts(&net.network)?;
    assert_eq!(conflicts.len(), 2);
    assert_eq!(conflicts[0]["other"]["body"]["text"], "forked");
    assert_eq!(conflicts[0]["kept"]["body"]["text"], "kept");
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
        gated.ingest_events(&net.network, &[created])?,
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
    assert_eq!(store.scheduler_jobs(&net.network, "c")?.len(), 1);
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
    store.mark_started(&net.network, "c", &claimed.job.trigger, &attempt)?;
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
    store.mark_started(&net.network, "c", &claimed.job.trigger, &attempt)?;
    // The Node restarts mid-turn: the reply is recorded as interrupted and the process awaits a check.
    store.recover_scheduler_jobs(&net.a, &net.network, "boot-2")?;
    let reply = store
        .message(&net.network, "c", &claimed.job.message_id)?
        .context("reply")?;
    assert_eq!(reply.status, Some(ReplyStatus::Interrupted));
    assert_eq!(store.pending_checks(&net.network)?.len(), 1);
    assert!(matches!(
        store
            .scheduler_job(&net.network, "c", &message_trigger("m2"))?
            .context("job")?
            .state,
        JobState::Interrupted { .. }
    ));
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
        attempt: "attempt".into(),
        target: net.a.device_id()?,
        dispatcher: net.a.device_id()?,
        call_ref: "call-1".into(),
        executor: Executor::Codex,
        cwd: "/tmp".into(),
        prompt: "p".into(),
        continues: None,
        parent: None,
    };
    assert!(store.accept_work(&net.a, &net.network, spec.clone(), 3)?.1);
    assert!(!store.accept_work(&net.a, &net.network, spec.clone(), 4)?.1);
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
    store.apply_work_signal(
        &net.a,
        &net.network,
        &id,
        Signal::ApprovalRequested {
            rpc_id: serde_json::json!(7),
            method: "item/commandExecution/requestApproval".into(),
            params: serde_json::json!({}),
            kind: "command".into(),
            summary: "npm test".into(),
            epoch: 1,
        },
        7,
    )?;
    let request_id = store.works(&net.network, "c")?[0].open_approvals[0]
        .request_id
        .clone();
    let EventBody::ApprovalAnswered { attestation, .. } = net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::ApprovalAnswered {
            work_id: id.clone(),
            request_id: request_id.clone(),
            decision: ApprovalDecision::Accept,
            attestation: None,
        },
    )?
    else {
        unreachable!()
    };
    store.apply_work_signal(
        &net.a,
        &net.network,
        &id,
        Signal::ApprovalAnswered {
            request_id: request_id.clone(),
            decision: ApprovalDecision::Accept,
            attestation: attestation.context("signed answer")?,
        },
        8,
    )?;
    let works = store.works(&net.network, "c")?;
    assert!(works[0].open_approvals.is_empty());
    assert_eq!(
        (
            works[0].answered_approvals[0].summary.as_str(),
            works[0].answered_approvals[0].decision
        ),
        ("npm test", ApprovalDecision::Accept)
    );
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
        .scheduler_job(&net.network, "c", &work_trigger(&id))?
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

/// A removed target's open work is abandoned by the owner, a late real result still wins, and
/// only the user's signature settles an unverified result.
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
            attempt: "attempt".into(),
            target: target.device_id()?,
            dispatcher: net.a.device_id()?,
            call_ref: call.into(),
            executor: Executor::Codex,
            cwd: "/tmp".into(),
            prompt: "p".into(),
            continues: None,
            parent: None,
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
            10,
        )?
        .context("the settled result reports back")?;
    assert_eq!(
        outcome(&store, &local.work_id)?,
        Some(WorkOutcome::Completed)
    );
    assert!(
        store
            .work_record(&net.network, &local.work_id)?
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
                11
            )
            .is_err()
    );
    Ok(())
}

/// Messages past the prompt budget condense into one checkpoint covering a prefix of the shared
/// order; the newest messages stay verbatim and a covered history needs no second checkpoint.
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
        standing_instructions,
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
    assert!(!standing_instructions.is_empty());
    store.append_event(&net.a, &net.network, "c", checkpoint, vec![], 100)?;
    let context = store.context(&net.network, "c", None)?;
    assert!(crate::scheduler::checkpoint(&context, &works).is_none());
    Ok(())
}

/// After the user moves a conversation, a turn that never started leaves the old owner's queue
/// for the outbox, which delivers it to the new owner by its trigger.
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
        },
    )?;
    store.append_event(&net.a, &net.network, "c", moved, vec![], 3)?;
    assert!(
        store
            .claim_next(&net.a, &net.network, "c", "boot", "cfg")?
            .is_none()
    );
    let trigger = message_trigger("m1");
    assert!(store.scheduler_job(&net.network, "c", &trigger)?.is_none());
    let outbox = store.scheduler_outbox(&net.network)?;
    assert_eq!(
        outbox.iter().map(|e| e.trigger.clone()).collect::<Vec<_>>(),
        vec![trigger]
    );
    Ok(())
}

/// Appending touches only indexed rows: no statement on the write path scans a table.
#[test]
fn append_path_uses_indexes_only() -> Result<()> {
    let net = net()?;
    let store = store(&net)?;
    let author = net.a.device_id()?;
    let statements = [
        "SELECT max_seq,head_hash FROM log_heads WHERE network_id=?1 AND author=?2",
        "SELECT author FROM log_heads WHERE network_id=?1 AND author<>?2",
        "SELECT hash FROM events WHERE network_id=?1 AND scope_id='c' AND author=?2 ORDER BY seq DESC LIMIT 1",
        "SELECT lamport FROM events WHERE network_id=?1 AND hash=?2",
        "SELECT 1 FROM pending_events WHERE network_id=?1 AND hash=?2",
        "SELECT 1 FROM log_conflicts WHERE network_id=?1 AND hash=?2",
        "SELECT hash FROM events WHERE network_id=?1 AND author=?2 AND seq=1",
        "SELECT message_id FROM messages WHERE network_id=?1 AND conversation_id='c' AND message_id=?2",
        "SELECT value FROM local_values WHERE network_id=?1 AND key>=?2 AND key<'z'",
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

/// `cargo test -p vibeface-node -- --ignored append_100k`: the last appends cost about the same as the first.
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
