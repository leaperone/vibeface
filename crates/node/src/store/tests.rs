use super::{bot_settings::ChangeRequest, *};
use crate::{
    crypto::LocalIdentity,
    providers::Provider,
    scheduling::{
        JobOutcome, JobState, SchedulerJob, Shown, SteerOutcome, TriggerKind, bot_message_trigger,
        message_trigger, schedule_trigger, work_trigger,
    },
    work::{RequestedWorkKind, Signal, WorkRequest, WorkSpec},
};
use serde_json::json;
use vibeface_bridge::failure::{ExecFailure, FailureKind};

impl Store {
    pub(crate) fn accept_work_fixture(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        spec: WorkSpec,
        at: i64,
    ) -> Result<(crate::work::WorkRecord, bool)> {
        let WorkSpec {
            work_id,
            conversation,
            trigger,
            target,
            dispatcher,
            call_ref,
            kind,
            model,
            resolved,
            cwd,
            title,
            prompt,
            continues,
            parent,
            thread,
        } = spec;
        let kind = match kind {
            WorkKind::Agent(provider) => crate::work::RequestedWorkKind::Agent(Some(provider)),
            WorkKind::Task => crate::work::RequestedWorkKind::Task,
        };
        let request = crate::work::WorkRequest {
            work_id,
            conversation,
            trigger,
            target,
            dispatcher,
            call_ref,
            kind,
            model,
            cwd,
            title,
            prompt,
            continues,
            parent,
            thread,
        };
        let admitted = self.accept_work(identity, network, request, resolved, at)?;
        Ok((admitted.record, admitted.created))
    }

    pub(crate) fn claim_next_fixture(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        conversation: &str,
        boot: &str,
        fingerprint: &str,
    ) -> Result<Option<crate::scheduling::ClaimedJob>> {
        self.recover_scheduler_jobs(identity, network, boot)?;
        let mut queued = self.scheduler_jobs(conversation)?;
        queued.retain(|job| matches!(job.state, JobState::Queued));
        queued.sort_by(|left, right| left.priority().cmp(&right.priority()));
        for job in queued {
            let resolved = if job.kind == TriggerKind::AgentTask {
                self.works(network, conversation)?
                    .into_iter()
                    .find(|work| work.work_id == job.reply_to)
                    .context("accepted task")?
                    .resolved
            } else {
                ResolvedModel {
                    provider: Provider::Codex,
                    model: None,
                }
            };
            if let Some(mut claimed) = self.claim_next_in_session(
                identity,
                network,
                &job.session_key(),
                boot,
                &crate::scheduling::SessionConfig {
                    stable_fingerprint: fingerprint.into(),
                    runtime_fingerprint: fingerprint.into(),
                    provider: resolved.provider,
                },
                &crate::config::SchedulingLimits::default(),
            )? {
                let attempt = claimed
                    .job
                    .state
                    .attempt()
                    .context("claimed attempt")?
                    .id
                    .clone();
                let runtime = crate::scheduling::TurnRuntime {
                    provider: resolved.provider,
                    model: resolved.model,
                    reasoning_effort: None,
                    generation: "fixture-generation".into(),
                    session_generation: claimed.session_generation,
                    delivery_id: attempt.clone(),
                    stage: vibeface_bridge::failure::ProviderStage::Validate,
                    execution: crate::scheduling::TurnExecution::NotStarted,
                    attempted_at_ms: 0,
                    switched_from: None,
                    switch_reason: None,
                };
                if claimed.job.runtime.as_ref().is_none_or(|saved| {
                    saved.execution == crate::scheduling::TurnExecution::NotStarted
                }) {
                    claimed.job = self.record_turn_runtime(
                        conversation,
                        &claimed.job.trigger,
                        &attempt,
                        &runtime,
                    )?;
                }
                return Ok(Some(claimed));
            }
        }
        Ok(None)
    }
}

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
                failure: None,
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

fn signed_memory(
    net: &Net,
    store: &Store,
    scope: &str,
    memory_id: &str,
    after: Option<&str>,
) -> Result<EventBody> {
    let base = store
        .memory_state(&net.network, scope, memory_id, None)?
        .current
        .map_or_else(MemoryBase::absent, |state| state.metadata.base());
    net.a.fixture_admin().attest(
        &net.network,
        scope,
        EventBody::MemoryControlled {
            memory_id: memory_id.into(),
            base,
            after: after.map(str::to_owned),
            attestation: None,
        },
    )
}

fn memory_command(
    network: &str,
    scope: &str,
    body: &EventBody,
) -> Result<bot_settings::MemoryControlCommand> {
    let EventBody::MemoryControlled {
        memory_id,
        base,
        after,
        attestation,
    } = body
    else {
        bail!("not memory control")
    };
    Ok(bot_settings::MemoryControlCommand {
        memory_id: memory_id.clone(),
        base: base.clone(),
        after: after.clone(),
        attestation: attestation.clone().context("signed memory")?,
        operation_id: crate::crypto::request_key(network, scope, body, "")?,
    })
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
    let statuses = b_first.ingest_events(&net.network, &b_events)?.statuses;
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
            .statuses,
        vec![IngestStatus::Accepted]
    );
    assert_eq!(
        target
            .ingest_events(&net.network, std::slice::from_ref(&first))?
            .statuses,
        vec![IngestStatus::Duplicate]
    );
    assert!(
        matches!(target.ingest_events(&net.network, &[third])?.statuses[0], IngestStatus::Rejected(ref e) if e.contains("gap"))
    );
    let mut forged = first.clone();
    forged.signature = "00".repeat(64);
    assert_eq!(
        target.ingest_events(&net.network, &[forged])?.statuses,
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
        matches!(target.ingest_events(&net.network, &[foreign])?.statuses[0], IngestStatus::Rejected(ref e) if e.contains("unknown author"))
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
        target.ingest_events(&net.network, &[tampered])?.statuses[0],
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
            .statuses,
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
        target.ingest_events(&net.network, &[next])?.statuses,
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
        gated.ingest_events(&net.network, &[created])?.statuses,
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
            failure: None,
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
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let (trigger, attempt) = (
        claimed.job.trigger.clone(),
        claimed.job.state.attempt().context("attempt")?.id.clone(),
    );
    store.mark_started("c", &trigger, &attempt)?;
    say(&mut store, &net, "m2", true)?;
    say(&mut store, &net, "quiet", false)?;
    let candidates =
        |store: &Store| store.steer_candidates(&net.network, &local, "boot", Provider::Codex);
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
            .steer_candidates(&net.network, &local, "later", Provider::Codex)?
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
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
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
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
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
        kind: WorkKind::Agent(Provider::Codex),
        cwd: "/tmp".into(),
        title: "t".into(),
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
            status: vibeface_bridge::turn::TurnStatus::Completed,
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

/// A dispatch the target never took into the DAG is still the bot's to see: waiting ones are
/// in its prompt, and a refusal or a removed target's unknown answer wakes it once.
#[test]
fn dispatches_the_dag_lacks_reach_the_bot_once() -> Result<()> {
    use crate::work::{Dispatch, DispatchState};
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
            kind: WorkKind::Agent(Provider::Codex),
            cwd: "/tmp".into(),
            title: "t".into(),
            prompt: "p".into(),
            continues: None,
            parent: None,
            thread: None,

            model: None,

            resolved: ResolvedModel {
                provider: Provider::Codex,

                model: None,
            },
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
    let (_, receipt) = store.apply_work_signal(
        &net.a,
        &net.network,
        &running.work_id,
        Signal::TurnCompleted {
            turn_id: "tu".into(),
            status: vibeface_bridge::turn::TurnStatus::Completed,
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
fn many_small_events_condense_into_a_checkpoint() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m0", "修一下登录"),
        vec![],
        2,
    )?;
    let node = net.a.device_id()?;
    let attribute = |store: &mut Store, i: i64| {
        store.append_event(
            &net.a,
            &net.network,
            "c",
            EventBody::SessionAttributed {
                node: node.clone(),
                agent: crate::providers::Provider::Codex,
                session_id: format!("s{i}"),
                cwd: "/w".into(),
                summary: "在跑".into(),
            },
            vec![],
            10 + i,
        )
    };
    for i in 0..600 {
        attribute(&mut store, i)?;
    }
    let works = store.works(&net.network, "c")?;
    let context = super::since_checkpoint_from(&store.connection, &net.network, "c")?;
    let checkpoint = crate::scheduler::checkpoint(&context, &works)
        .context("a long run of events is condensed though its messages are short")?;
    let EventBody::ContextCheckpoint {
        recent_user_requests,
        ..
    } = &checkpoint
    else {
        unreachable!()
    };
    assert_eq!(recent_user_requests, &["修一下登录".to_owned()]);
    store.append_event(&net.a, &net.network, "c", checkpoint, vec![], 1_000)?;
    let recent = super::since_checkpoint_from(&store.connection, &net.network, "c")?;
    assert!(
        recent.events.len() <= 300,
        "a turn reads only the newest events, not {}",
        recent.events.len()
    );
    assert!(
        crate::scheduler::checkpoint(&recent, &works).is_none(),
        "the next turn writes no checkpoint of its own"
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
    conversation(&mut origin, &net)?;
    origin.accept_message(
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
    conversation(&mut former, &net)?;
    former.accept_message(
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
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
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
    let statuses = replica
        .ingest_events(&net.network, &a_log(&a_side)?)?
        .statuses;
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
            .statuses,
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
        &replica.ingest_events(&net.network, &[forged])?.statuses[0],
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
        failure: None,
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
        avatar: None,
        default_avatar: None,
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
            .statuses
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
                    avatar: None,

                    reasoning_effort: None,
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
            matches!(&target.ingest_events(&net.network, &[change(cause, deps)?])?.statuses[0],
                IngestStatus::Rejected(error) if error.contains("user's own signed message")),
            "{cause}"
        );
    }
    assert_eq!(target.bots(&net.network)?[0].name, "Helper");
    assert_eq!(
        target
            .ingest_events(&net.network, &[change("m1", vec![m1.hash.clone()])?])?
            .statuses,
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
fn a_bot_setting_two_devices_changed_at_once_names_the_change_that_lost() -> Result<()> {
    use bot_settings::{Overwritten, Setting};
    let net = net()?;
    let (mut a_side, _) = bot_with_message(&net)?;
    let mut b_side = store(&net)?;
    sync(&a_side, &mut b_side, &net)?;
    b_side.append_event(&net.b, &net.network, "c", user(&net, "m2", "y"), vec![], 4)?;
    let rename_on = |store: &mut Store, identity, trigger, call_ref, name, at| {
        store.change_bot(
            identity,
            &net.network,
            "c",
            trigger,
            call_ref,
            rename(name),
            Provider::Codex,
            at,
        )
    };
    let (alpha, _) = rename_on(&mut a_side, &net.a, "message:m1", "r1", "Alpha", 5)?;
    let (beta, _) = rename_on(&mut b_side, &net.b, "message:m2", "r1", "Beta", 5)?;
    sync(&a_side, &mut b_side, &net)?;
    sync(&b_side, &mut a_side, &net)?;

    let stands = a_side.bots(&net.network)?[0].name.clone();
    let (lost, won, winner) = if stands == "Beta" {
        (alpha, "Beta", &net.b)
    } else {
        (beta, "Alpha", &net.a)
    };
    let expected = vec![Overwritten {
        change_id: lost.change_id.clone(),
        setting: Setting::Name,
        by: winner.device_id()?,
        value: json!(won),
    }];
    for side in [&a_side, &b_side] {
        assert_eq!(
            side.bots(&net.network)?[0].name,
            won,
            "the larger rank still wins"
        );
        assert_eq!(side.overwritten(&net.network, "c")?, expected);
    }

    let replayed = b_side.ingest_events(
        &net.network,
        &a_side.events_from(&net.network, &net.a.device_id()?, 1, 500)?,
    )?;
    assert!(
        replayed
            .statuses
            .iter()
            .all(|status| *status == IngestStatus::Duplicate)
    );
    assert_eq!(
        b_side.overwritten(&net.network, "c")?,
        expected,
        "a replay adds nothing"
    );
    a_side.connection.execute(
        "UPDATE events SET effective=0 WHERE network_id=?",
        [&net.network],
    )?;
    reduce::clear(&a_side.connection, &net.network)?;
    log::recompute(
        &a_side.connection,
        &membership_from(&a_side.connection, &net.network)?,
    )?;
    assert_eq!(
        a_side.overwritten(&net.network, "c")?,
        expected,
        "a rebuild adds nothing"
    );
    let mut late = store(&net)?;
    sync(&b_side, &mut late, &net)?;
    assert_eq!(
        late.overwritten(&net.network, "c")?,
        expected,
        "a new replica agrees"
    );

    let (loser_side, loser, trigger, mine) = if won == "Beta" {
        (&mut a_side, &net.a, "message:m1", "Alpha")
    } else {
        (&mut b_side, &net.b, "message:m2", "Beta")
    };
    rename_on(loser_side, loser, trigger, "r2", mine, 9)?;
    sync(&a_side, &mut b_side, &net)?;
    sync(&b_side, &mut a_side, &net)?;
    for side in [&a_side, &b_side] {
        assert_eq!(side.bots(&net.network)?[0].name, mine);
        assert!(
            side.overwritten(&net.network, "c")?.is_empty(),
            "a change over the one that stood ends it"
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
            skills: None,
            avatar: None,

            reasoning_effort: None,
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
    assert!(
        origin.overwritten(&net.network, "c")?.is_empty(),
        "a change that saw the one before it replaced nothing unseen"
    );
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
    conversation(&mut origin, &net)?;
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
        let statuses = target.ingest_events(&net.network, &events)?.statuses;
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
    sync_at(&a_side, &mut b_side, net, 3)?;
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
        .claim_next_fixture(&net.a, &net.network, "d", "boot", "cfg")?
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
            avatar: None,
            default_avatar: None,

            reasoning_effort: None,
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
        &other.ingest_events(&net.network, &[hijack])?.statuses[0],
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
        avatar: None,
        default_avatar: None,

        reasoning_effort: None,
    }
}

#[test]
fn a_removed_skill_leaves_the_bots_that_used_it() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let mut change = |call_ref: &str, request: ChangeRequest| {
        store.change_bot(
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
    let mut ids = Vec::new();
    for (call_ref, name) in [("s1", "Digest"), ("s2", "Review")] {
        let (saved, _) = change(call_ref, save_skill(None, name, "Steps."))?;
        let BotChange::Skill { skill_id, .. } = saved.change else {
            panic!("skill change")
        };
        ids.push(skill_id);
    }
    let both = [ids[0].as_str(), ids[1].as_str()];
    change("k1", configure("Helper", None, Some(&both), None))?;
    change(
        "r1",
        ChangeRequest::RemoveSkill {
            skill_id: ids[1].clone(),
        },
    )?;
    let helper = store
        .bot_for_conversation(&net.network, "c")?
        .context("helper")?;
    assert_eq!(
        helper.skills,
        Some([ids[0].clone()].into()),
        "the bot lists only skills that still exist"
    );
    let listed = crate::runtime::agent_tools::read_bot(&store, &net.network, "Helper", 100)?;
    assert_eq!(listed["skills"].as_array().map(Vec::len), Some(1));
    Ok(())
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
            provider,
            workspace: "/w".into(),
            model: "auto".into(),
            reasoning_effort: None,
            backup: Default::default(),
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
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
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
fn staged_memory_commits_only_with_a_completed_turn() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    say(&mut store, &net, "m2", true)?;
    let trigger = message_trigger("m2");
    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    store.mark_started("c", &trigger, &attempt)?;
    store.save_scheduler_native_id("c", &trigger, &attempt, "thread", "token", false)?;
    let staged = store.stage_memory(
        &net.network,
        "c",
        &trigger,
        &attempt,
        "remember-1",
        ChangeRequest::Remember {
            memory_id: None,
            text: "prefers staged notes".into(),
        },
        4,
    )?;
    assert_eq!(staged["state"], "staged");
    assert_eq!(
        store.staged_memory_call("c", &trigger, &attempt, "remember-1")?,
        Some(staged)
    );
    assert!(
        store
            .staged_memory_call("c", &trigger, &attempt, "other")?
            .is_none()
    );
    assert!(
        store
            .staged_memory_call("c", &trigger, "other-attempt", "remember-1")
            .is_err()
    );
    assert!(store.memories(&net.network, "c")?.is_empty());
    store.record_manager_turn(
        &attempt,
        "system",
        "thread",
        Some("turn"),
        &Shown::default(),
    )?;
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &attempt,
        JobOutcome::Completed,
        "done",
        5,
    )?;
    assert_eq!(store.memories(&net.network, "c")?.len(), 1);
    assert!(
        store
            .staged_memory_call("c", &trigger, &attempt, "remember-1")?
            .is_none()
    );
    assert!(
        store
            .bot_change(
                &net.network,
                &crate::store::bot_settings::change_id("c", &trigger, "remember-1")
            )?
            .is_some()
    );
    let record = store
        .memory_records(&net.network, "c")?
        .into_iter()
        .next()
        .context("memory record")?;
    assert_eq!(record.epoch, crate::config::DATA_EPOCH);
    assert!(!record.revision.is_empty());
    assert_eq!(record.origin, MemoryOrigin::Agent);
    assert!(!record.user_controlled);
    assert!(record.essential);

    say(&mut store, &net, "m3", true)?;
    let trigger = message_trigger("m3");
    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot-2", "cfg")?
        .context("job")?;
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    store.mark_started("c", &trigger, &attempt)?;
    store.stage_memory(
        &net.network,
        "c",
        &trigger,
        &attempt,
        "remember-2",
        ChangeRequest::Remember {
            memory_id: None,
            text: "discard this note".into(),
        },
        6,
    )?;
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &attempt,
        JobOutcome::Failed(ExecFailure::new(FailureKind::Rejected, "test")),
        "",
        7,
    )?;
    assert_eq!(store.memories(&net.network, "c")?.len(), 1);
    assert!(
        store
            .staged_memory_call("c", &trigger, &attempt, "remember-2")?
            .is_none()
    );
    Ok(())
}

#[test]
fn staged_memory_revision_conflict_preserves_the_newer_user_value() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    say(&mut store, &net, "m2", true)?;
    let trigger = message_trigger("m2");
    let claimed = store
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    store.mark_started("c", &trigger, &attempt)?;
    let staged = store.stage_memory(
        &net.network,
        "c",
        &trigger,
        &attempt,
        "remember-conflict",
        ChangeRequest::Remember {
            memory_id: None,
            text: "agent candidate".into(),
        },
        4,
    )?;
    let memory_id = staged["memory_id"]
        .as_str()
        .context("memory id")?
        .to_owned();
    store.append_event(
        &net.a,
        &net.network,
        "c",
        signed_memory(&net, &store, "c", &memory_id, Some("user value"))?,
        vec![],
        5,
    )?;
    assert_eq!(
        store.stage_memory(
            &net.network,
            "c",
            &trigger,
            &attempt,
            "remember-conflict",
            ChangeRequest::Remember {
                memory_id: None,
                text: "agent candidate".into()
            },
            5,
        )?,
        staged,
        "a retry returns its original snapshot after a concurrent user edit"
    );
    assert!(
        store
            .stage_memory(
                &net.network,
                "c",
                &trigger,
                &attempt,
                "remember-conflict",
                ChangeRequest::Remember {
                    memory_id: None,
                    text: "another candidate".into()
                },
                5,
            )
            .is_err()
    );
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &attempt,
        JobOutcome::Completed,
        "done",
        6,
    )?;
    let conflicts = memory_test_shown(&store)?.memory_conflicts;
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].memory_id, memory_id);
    assert_eq!(conflicts[0].reason, "user_controlled");
    assert_eq!(conflicts[0].expected_revision, "");
    assert!(!conflicts[0].actual_revision.is_empty());
    let record = store
        .memory_records(&net.network, "c")?
        .into_iter()
        .find(|record| record.entry.memory_id == memory_id)
        .context("memory record")?;
    assert_eq!(record.entry.text, "user value");
    assert_eq!(record.origin, MemoryOrigin::User);
    assert!(record.user_controlled);
    Ok(())
}

fn memory_test_shown(store: &Store) -> Result<Shown> {
    let shown: String = store.connection.query_row(
        "SELECT shown FROM scheduler_sessions WHERE conversation_id='c'",
        [],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&shown)?)
}

#[test]
fn memory_review_schema_is_bounded_data_only_and_disabled_by_default() -> Result<()> {
    use bot_settings::{MemoryReview, MemoryReviewPolicy};
    assert_eq!(MemoryReviewPolicy::default(), MemoryReviewPolicy::Disabled);
    let operation = json!({"operation":"add","text":"prefers concise replies"});
    let twenty = json!({"operations":vec![operation.clone(); 20]});
    assert_eq!(
        serde_json::from_value::<MemoryReview>(twenty)?
            .operations
            .len(),
        20
    );
    for refused in [
        json!({"operations":vec![operation; 21]}),
        json!({"operations":[],"tools":["shell"]}),
        json!({"operations":[],"read_only":false}),
        json!({"operations":[{"operation":"tool_call","name":"shell"}]}),
        json!({"operations":[{"operation":"add","text":"safe note","tools":[]}]}),
        json!({"operations":[{"operation":"add","text":"sk-proj-abcdefghijklmnopqrstuvwxyz123456"}]}),
        json!({"operations":[{"operation":"update","memory_id":"m","epoch":1,"text":"safe"}]}),
        json!({"operations":[{"operation":"forget","memory_id":"m","epoch":1,"expected_revision":"bad\nrevision"}]}),
        json!({"operations":[{"operation":"forget","memory_id":"m","epoch":1,"expected_revision":"sk-proj-abcdefghijklmnopqrstuvwxyz123456"}]}),
    ] {
        assert!(
            serde_json::from_value::<MemoryReview>(refused.clone()).is_err(),
            "{refused}"
        );
    }
    Ok(())
}

#[test]
fn memory_review_stages_add_update_and_forget_until_turn_completion() -> Result<()> {
    use bot_settings::{MemoryReview, MemoryReviewOperation, MemoryReviewPolicy};
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    for (call_ref, text) in [
        ("keep", "old preference"),
        ("remove", "obsolete preference"),
    ] {
        origin.change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            call_ref,
            ChangeRequest::Remember {
                memory_id: None,
                text: text.into(),
            },
            Provider::Codex,
            4,
        )?;
    }
    let records = origin.memory_records(&net.network, "c")?;
    let kept = records
        .iter()
        .find(|record| record.entry.text == "old preference")
        .context("kept")?;
    let removed = records
        .iter()
        .find(|record| record.entry.text == "obsolete preference")
        .context("removed")?;
    let review = MemoryReview {
        operations: vec![
            MemoryReviewOperation::Add {
                text: "new preference".into(),
            },
            MemoryReviewOperation::Update {
                memory_id: kept.entry.memory_id.clone(),
                epoch: kept.epoch,
                expected_revision: kept.revision.clone(),
                text: "updated preference".into(),
            },
            MemoryReviewOperation::Forget {
                memory_id: removed.entry.memory_id.clone(),
                epoch: removed.epoch,
                expected_revision: removed.revision.clone(),
            },
        ],
    };
    say(&mut origin, &net, "m2", true)?;
    let claimed = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let trigger = &claimed.job.trigger;
    let attempt = &claimed.job.state.attempt().context("attempt")?.id;
    origin.mark_started("c", trigger, attempt)?;
    let disabled = origin
        .stage_memory_review(
            &net.network,
            "c",
            trigger,
            attempt,
            "review",
            MemoryReviewPolicy::default(),
            &review,
        )
        .unwrap_err();
    assert!(disabled.to_string().contains("disabled"));
    assert!(
        origin
            .staged_memory_overlay("c", trigger, attempt)?
            .is_empty()
    );
    let staged = origin.stage_memory_review(
        &net.network,
        "c",
        trigger,
        attempt,
        "review",
        MemoryReviewPolicy::ReadOnly,
        &review,
    )?;
    assert_eq!(staged.len(), 3);
    assert!(staged.iter().all(|result| result["state"] == "staged"));
    assert_eq!(origin.memories(&net.network, "c")?.len(), 2);
    assert_eq!(
        origin.stage_memory_review(
            &net.network,
            "c",
            trigger,
            attempt,
            "review",
            MemoryReviewPolicy::ReadOnly,
            &review,
        )?,
        staged
    );
    origin.finish_job(
        &net.a,
        &net.network,
        "c",
        trigger,
        attempt,
        JobOutcome::Completed,
        "done",
        6,
    )?;
    let committed = origin.memory_records(&net.network, "c")?;
    assert_eq!(committed.len(), 2);
    assert!(
        committed
            .iter()
            .all(|record| record.origin == MemoryOrigin::Review)
    );
    assert!(
        committed
            .iter()
            .any(|record| record.entry.text == "updated preference")
    );
    assert!(
        committed
            .iter()
            .any(|record| record.entry.text == "new preference")
    );
    let events = origin.version_vector(&net.network)?;
    assert!(
        origin
            .stage_memory_review(
                &net.network,
                "c",
                trigger,
                attempt,
                "review",
                MemoryReviewPolicy::ReadOnly,
                &review,
            )?
            .iter()
            .all(|result| result["state"] == "committed")
    );
    assert_eq!(origin.version_vector(&net.network)?, events);
    assert!(memory_test_shown(&origin)?.memory_conflicts.is_empty());
    let mut replica = store(&net)?;
    all_accepted(&mut replica, &net, &origin)?;
    assert_eq!(
        replica.memories(&net.network, "c")?,
        origin.memories(&net.network, "c")?
    );
    for record in replica.memory_records(&net.network, "c")? {
        assert_eq!(record.origin, MemoryOrigin::Review);
        assert!(
            committed
                .iter()
                .any(|original| original.revision == record.revision)
        );
    }
    Ok(())
}

#[test]
fn memory_review_preserves_user_controlled_notes_and_rolls_back_its_batch() -> Result<()> {
    use bot_settings::{MemoryReview, MemoryReviewOperation, MemoryReviewPolicy};
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    origin.append_event(
        &net.a,
        &net.network,
        "c",
        signed_memory(
            &net,
            &origin,
            "c",
            "manual-note",
            Some("user-owned preference"),
        )?,
        vec![],
        4,
    )?;
    let record = origin.memory_records(&net.network, "c")?.remove(0);
    say(&mut origin, &net, "m2", true)?;
    let claimed = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let trigger = &claimed.job.trigger;
    let attempt = &claimed.job.state.attempt().context("attempt")?.id;
    origin.mark_started("c", trigger, attempt)?;
    for operation in [
        MemoryReviewOperation::Update {
            memory_id: record.entry.memory_id.clone(),
            epoch: record.epoch,
            expected_revision: record.revision.clone(),
            text: "replacement".into(),
        },
        MemoryReviewOperation::Forget {
            memory_id: record.entry.memory_id.clone(),
            epoch: record.epoch,
            expected_revision: record.revision.clone(),
        },
    ] {
        let review = MemoryReview {
            operations: vec![
                MemoryReviewOperation::Add {
                    text: "discard the partial batch".into(),
                },
                operation,
            ],
        };
        let error = origin
            .stage_memory_review(
                &net.network,
                "c",
                trigger,
                attempt,
                "review",
                MemoryReviewPolicy::ReadOnly,
                &review,
            )
            .unwrap_err();
        assert!(error.to_string().contains("user-controlled"));
        assert!(
            origin
                .staged_memory_overlay("c", trigger, attempt)?
                .is_empty()
        );
    }
    origin.finish_job(
        &net.a,
        &net.network,
        "c",
        trigger,
        attempt,
        JobOutcome::Completed,
        "done",
        5,
    )?;
    assert_eq!(origin.memories(&net.network, "c")?, vec![record.entry]);
    Ok(())
}

#[test]
fn memory_review_epoch_conflict_is_visible_without_overwriting_the_note() -> Result<()> {
    use bot_settings::{MemoryReview, MemoryReviewOperation, MemoryReviewPolicy};
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    origin.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "remember",
        ChangeRequest::Remember {
            memory_id: None,
            text: "current preference".into(),
        },
        Provider::Codex,
        4,
    )?;
    let record = origin.memory_records(&net.network, "c")?.remove(0);
    say(&mut origin, &net, "m2", true)?;
    let claimed = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let trigger = &claimed.job.trigger;
    let attempt = &claimed.job.state.attempt().context("attempt")?.id;
    origin.mark_started("c", trigger, attempt)?;
    let review = MemoryReview {
        operations: vec![MemoryReviewOperation::Update {
            memory_id: record.entry.memory_id.clone(),
            epoch: record.epoch + 1,
            expected_revision: record.revision.clone(),
            text: "stale review preference".into(),
        }],
    };
    origin.stage_memory_review(
        &net.network,
        "c",
        trigger,
        attempt,
        "review",
        MemoryReviewPolicy::ReadOnly,
        &review,
    )?;
    origin.finish_job(
        &net.a,
        &net.network,
        "c",
        trigger,
        attempt,
        JobOutcome::Completed,
        "done",
        5,
    )?;
    assert_eq!(origin.memories(&net.network, "c")?, vec![record.entry]);
    let conflicts = memory_test_shown(&origin)?.memory_conflicts;
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].reason, "epoch_changed");
    assert_eq!(conflicts[0].expected_epoch, record.epoch + 1);
    assert_eq!(conflicts[0].actual_epoch, record.epoch);
    assert_eq!(conflicts[0].actual_revision, record.revision);
    Ok(())
}

#[test]
fn staged_memory_detects_a_revision_change_even_when_the_text_returns_to_its_base() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    origin.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "remember",
        ChangeRequest::Remember {
            memory_id: None,
            text: "base preference".into(),
        },
        Provider::Codex,
        4,
    )?;
    let record = origin.memory_records(&net.network, "c")?.remove(0);
    say(&mut origin, &net, "m2", true)?;
    let claimed = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let trigger = &claimed.job.trigger;
    let attempt = &claimed.job.state.attempt().context("attempt")?.id;
    origin.mark_started("c", trigger, attempt)?;
    let request = ChangeRequest::Remember {
        memory_id: Some(record.entry.memory_id.clone()),
        text: "candidate preference".into(),
    };
    let staged = origin.stage_memory(
        &net.network,
        "c",
        trigger,
        attempt,
        "update",
        request.clone(),
        5,
    )?;
    for (index, before, after) in [
        (0, "base preference", "temporary preference"),
        (1, "temporary preference", "base preference"),
    ] {
        origin.append_event(
            &net.a,
            &net.network,
            "c",
            EventBody::BotChanged {
                change_id: format!("concurrent-{index}"),
                cause: Some(Cause {
                    conversation_id: "c".into(),
                    message_id: "m2".into(),
                }),
                change: BotChange::Memory {
                    memory_id: record.entry.memory_id.clone(),
                    before: Some(before.into()),
                    after: Some(after.into()),
                    base: origin
                        .memory_state(&net.network, "c", &record.entry.memory_id, None)?
                        .current
                        .context("base")?
                        .metadata
                        .base(),
                    origin: AutomaticMemoryOrigin::Agent,
                    essential: true,
                },
            },
            vec![],
            6 + index,
        )?;
    }
    assert_eq!(
        origin.stage_memory(&net.network, "c", trigger, attempt, "update", request, 8)?,
        staged
    );
    origin.finish_job(
        &net.a,
        &net.network,
        "c",
        trigger,
        attempt,
        JobOutcome::Completed,
        "done",
        9,
    )?;
    assert_eq!(origin.memories(&net.network, "c")?, vec![record.entry]);
    let conflicts = memory_test_shown(&origin)?.memory_conflicts;
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].reason, "revision_changed");
    assert_eq!(conflicts[0].expected_revision, record.revision);
    assert_ne!(conflicts[0].actual_revision, record.revision);
    Ok(())
}

#[test]
fn staged_memory_is_discarded_for_failed_stopped_interrupted_and_unknown_turns() -> Result<()> {
    for (outcome, pending) in [
        (
            JobOutcome::Failed(ExecFailure::new(FailureKind::Rejected, "failed")),
            0,
        ),
        (JobOutcome::Cancelled("stopped".into()), 0),
        (
            JobOutcome::Interrupted {
                failure: ExecFailure::new(FailureKind::Transport, "interrupted"),
                process_unconfirmed: false,
            },
            0,
        ),
        (
            JobOutcome::Interrupted {
                failure: ExecFailure::new(FailureKind::Restarted, "unknown result"),
                process_unconfirmed: true,
            },
            1,
        ),
    ] {
        let net = net()?;
        let (mut origin, _) = bot_with_message(&net)?;
        say(&mut origin, &net, "m2", true)?;
        let claimed = origin
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .context("job")?;
        let trigger = &claimed.job.trigger;
        let attempt = &claimed.job.state.attempt().context("attempt")?.id;
        origin.mark_started("c", trigger, attempt)?;
        origin.stage_memory(
            &net.network,
            "c",
            trigger,
            attempt,
            "remember",
            ChangeRequest::Remember {
                memory_id: None,
                text: "uncommitted preference".into(),
            },
            4,
        )?;
        origin.finish_job(
            &net.a,
            &net.network,
            "c",
            trigger,
            attempt,
            outcome,
            "partial",
            5,
        )?;
        assert!(origin.memories(&net.network, "c")?.is_empty());
        assert!(
            origin
                .staged_memory_overlay("c", trigger, attempt)?
                .is_empty()
        );
        assert_eq!(origin.pending_checks()?.len(), pending);
    }
    Ok(())
}

#[test]
fn staged_memory_reopens_replays_and_discards_a_restarted_unknown_attempt() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    say(&mut origin, &net, "m2", true)?;
    let claimed = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("job")?;
    let trigger = &claimed.job.trigger;
    let attempt = &claimed.job.state.attempt().context("attempt")?.id;
    origin.mark_started("c", trigger, attempt)?;
    let request = ChangeRequest::Remember {
        memory_id: None,
        text: "persisted candidate".into(),
    };
    let staged = origin.stage_memory(
        &net.network,
        "c",
        trigger,
        attempt,
        "remember",
        request.clone(),
        4,
    )?;
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.artifacts")
        .join(format!("memory-restart-{}", uuid::Uuid::new_v4()));
    crate::config::private_dir(&dir)?;
    let path = dir.join("node.sqlite3");
    origin
        .connection
        .execute("VACUUM INTO ?", [path.to_str().context("path")?])?;
    drop(origin);
    let mut reopened = Store::open(&path)?;
    reopened.recover_scheduler_jobs(&net.a, &net.network, "boot")?;
    assert_eq!(
        reopened.stage_memory(
            &net.network,
            "c",
            trigger,
            attempt,
            "remember",
            request.clone(),
            5
        )?,
        staged
    );
    reopened.finish_job(
        &net.a,
        &net.network,
        "c",
        trigger,
        attempt,
        JobOutcome::Completed,
        "done",
        6,
    )?;
    let initial = reopened.memory_records(&net.network, "c")?.remove(0);
    reopened.append_event(
        &net.a,
        &net.network,
        "c",
        signed_memory(
            &net,
            &reopened,
            "c",
            &initial.entry.memory_id,
            Some("newer committed user preference"),
        )?,
        vec![],
        7,
    )?;
    let version = reopened.version_vector(&net.network)?;
    let replay =
        reopened.stage_memory(&net.network, "c", trigger, attempt, "remember", request, 7)?;
    assert_eq!(replay["state"], "committed");
    assert_eq!(replay["record"]["change"]["after"], "persisted candidate");
    assert_eq!(reopened.version_vector(&net.network)?, version);
    let memory = reopened.memory_records(&net.network, "c")?.remove(0);
    assert_eq!(memory.entry.text, "newer committed user preference");
    say(&mut reopened, &net, "m3", true)?;
    let next = reopened
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
        .context("next job")?;
    let next_attempt = &next.job.state.attempt().context("next attempt")?.id;
    reopened.mark_started("c", &next.job.trigger, next_attempt)?;
    reopened.stage_memory(
        &net.network,
        "c",
        &next.job.trigger,
        next_attempt,
        "discard",
        ChangeRequest::Remember {
            memory_id: None,
            text: "unknown candidate".into(),
        },
        8,
    )?;
    drop(reopened);
    let mut restarted = Store::open(&path)?;
    restarted.recover_scheduler_jobs(&net.a, &net.network, "new-boot")?;
    assert_eq!(
        restarted.memories(&net.network, "c")?,
        vec![memory.entry.clone()]
    );
    assert!(memory_test_shown(&restarted)?.staged_memories.is_empty());
    assert_eq!(restarted.pending_checks()?.len(), 1);
    let replayed = restarted.memory_records(&net.network, "c")?.remove(0);
    assert_eq!(replayed.revision, memory.revision);
    assert_eq!(replayed.source_event_hash, memory.source_event_hash);
    let mut replica = store(&net)?;
    all_accepted(&mut replica, &net, &restarted)?;
    assert_eq!(replica.memories(&net.network, "c")?, vec![memory.entry]);
    assert_eq!(
        replica.memory_records(&net.network, "c")?[0].revision,
        memory.revision
    );
    drop(restarted);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn bot_settings_refuse_secrets() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    let key = "sk-proj-FAKEFAKEFAKEFAKEFAKEFAKE000000";
    for (index, request) in [
        save_skill(None, "Deploy", &format!("Use {key} to call the API")),
        ChangeRequest::SaveSchedule {
            schedule_id: None,
            name: "Nightly".into(),
            prompt: format!("Call the API with {key}"),
            first_run_at_ms: 1_000,
            every_minutes: Some(60),
            enabled: None,
        },
        ChangeRequest::UpdateProfile {
            name: None,
            instructions: Some(format!("Your key is {key}")),
            avatar: None,
            default_avatar: None,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let refused = origin
            .change_bot(
                &net.a,
                &net.network,
                "c",
                "message:m1",
                &format!("s{index}"),
                request,
                Provider::Codex,
                10,
            )
            .unwrap_err();
        assert!(refused.to_string().contains("credential"), "{refused}");
    }
    assert!(origin.skills(&net.network)?.is_empty());
    assert!(origin.schedules(&net.network, "c")?.is_empty());
    Ok(())
}

#[test]
fn shared_records_never_carry_a_known_credential() -> Result<()> {
    let key = "fake-shared-record-credential-0123456789";
    crate::logging::add_credential(key);
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    second_bot(&mut origin, &net, "Ops")?;
    let mut reply = bot_reply("r1", "m1");
    if let EventBody::MessagePosted { text, .. } = &mut reply {
        *text = format!("The key is {key}.");
    }
    origin.append_event(&net.a, &net.network, "c", reply, vec![], 4)?;
    origin.message_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "s1",
        "Ops",
        &format!("Use {key} for the backups"),
        5,
    )?;
    let trigger = message_trigger("m1");
    let spec = WorkSpec {
        work_id: WorkSpec::id("c", &trigger, "call-1"),
        conversation: "c".into(),
        trigger,
        target: net.a.device_id()?,
        dispatcher: net.a.device_id()?,
        call_ref: "call-1".into(),
        kind: WorkKind::Agent(Provider::Codex),
        cwd: "/tmp".into(),
        title: "t".into(),
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
    let id = spec.work_id.clone();
    origin.accept_work_fixture(&net.a, &net.network, spec, 6)?;
    for (signal, at) in [
        (Signal::ThreadCreated("t".into()), 7),
        (
            Signal::TurnStarted {
                turn_id: "u".into(),
                epoch: 1,
            },
            8,
        ),
        (
            Signal::TurnCompleted {
                turn_id: "u".into(),
                status: vibeface_bridge::turn::TurnStatus::Completed,
                text: format!("env printed OPENAI_API_KEY={key}"),
                error: None,
            },
            9,
        ),
    ] {
        origin.apply_work_signal(&net.a, &net.network, &id, signal, at)?;
    }
    let typed = format!("my key is {key}");
    origin.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m2", &typed),
        vec![],
        10,
    )?;

    let texts = |store: &Store| -> Result<Vec<String>> {
        Ok(store
            .messages(&net.network, "c")?
            .into_iter()
            .map(|m| m.text)
            .collect())
    };
    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &origin)?;
    for store in [&origin, &other] {
        let envelopes: Vec<String> = store
            .connection
            .prepare("SELECT envelope FROM events WHERE network_id=?")?
            .query_map([&net.network], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let carrying: Vec<_> = envelopes.iter().filter(|e| e.contains(key)).collect();
        assert_eq!(
            carrying.len(),
            1,
            "only the user's signed message: {carrying:?}"
        );
        let texts = texts(store)?;
        assert!(
            texts.contains(&"The key is [redacted].".to_owned()),
            "{texts:?}"
        );
        assert!(texts.contains(&typed), "{texts:?}");
        assert_eq!(
            store.works(&net.network, "c")?[0].result_text,
            "env printed OPENAI_API_KEY=[redacted]"
        );
        let sent = store.sent_bot_messages(&net.network, "c")?;
        assert_eq!(sent[0].text, "Use [redacted] for the backups");
    }
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
    let statuses = late
        .ingest_events(
            &net.network,
            &[vec![created.clone()], writes.clone()].concat(),
        )?
        .statuses;
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
            agent: Some(crate::providers::Provider::Codex),
            session_id: session.into(),
            cwd: "/w".into(),
            summary: "tests fail".into(),
            request_id: None,
        },

        attention: AttentionRequest {
            kind: AttentionKind::Question,

            device_id: Some(net.a.device_id()?),
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
                failure: None,
            },
        )?
        .attestation()
        .cloned()
        .context("signed answer")
}

fn sync(from: &Store, to: &mut Store, net: &Net) -> Result<()> {
    sync_at(from, to, net, crate::runtime::now_ms())
}

fn sync_at(from: &Store, to: &mut Store, net: &Net, seen_at_ms: i64) -> Result<()> {
    let envelopes: Vec<SignedEvent> = from
        .connection
        .prepare("SELECT envelope FROM events WHERE network_id=? ORDER BY author,seq")?
        .query_map([&net.network], |r| r.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect::<Result<_>>()?;
    to.ingest_events_at(&net.network, &envelopes, seen_at_ms)?;
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
fn the_answer_to_a_settings_card_turns_into_apply_provider_config() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    let mut card = question(&net, "q-settings", "provider-setup-1")?;
    if let EventBody::QuestionAsked { about, .. } = &mut card {
        about.agent = None;
    }
    store.ask(&net.a, &net.network, "c", card, 2)?;
    store.ask(
        &net.a,
        &net.network,
        "c",
        question(&net, "q-session", "t1")?,
        3,
    )?;
    let mut roles = std::collections::BTreeMap::new();
    for (id, time) in [("q-settings", 4), ("q-session", 5)] {
        let signed = Some(answer_signature(&net, "c", id, "继续")?);
        store.accept_answer(&net.a, &net.network, id, "继续", signed, time)?;
        let trigger = message_trigger(&questions::answer_id(id));
        store.admit_delivery(&net.a.device_id()?, &net.network, "c", &trigger)?;
        let job = store
            .scheduler_jobs("c")?
            .into_iter()
            .find(|job| job.trigger == trigger)
            .context("answer job")?;
        let history = store.turn_history(&net.network, "c", &job, None)?;
        let works = store.works(&net.network, "c")?;
        let (prompt, _) =
            crate::scheduler::materialize(&history, &job, &works, &[], json!({}), None, &[], None)?;
        roles.insert(id, prompt.lines().next().unwrap_or_default().to_owned());
    }
    assert!(
        roles["q-settings"].contains("apply_provider_config")
            && !roles["q-settings"].contains("dispatch_work"),
        "a settings card's answer is applied, never sent to its placeholder session: {}",
        roles["q-settings"]
    );
    assert!(
        roles["q-session"].contains("dispatch_work"),
        "an answer about a session still reaches the session"
    );
    Ok(())
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

fn approval(net: &Net, id: &str, request: &str) -> Result<EventBody> {
    let mut body = question(net, id, "t1")?;
    if let EventBody::QuestionAsked { about, .. } = &mut body {
        about.request_id = Some(request.into());
    }
    Ok(body)
}

fn closed(store: &Store, net: &Net, question: &str) -> Result<Value> {
    Ok(store
        .question(&net.network, question)?
        .context("question")?["closed"]
        .clone())
}

fn held(requests: &[&str]) -> std::collections::HashSet<String> {
    requests
        .iter()
        .map(|request| (*request).to_owned())
        .collect()
}

#[test]
fn a_lost_request_closes_its_question_on_every_device_once() -> Result<()> {
    let net = net()?;
    let (mut a, mut b) = (store(&net)?, store(&net)?);
    conversation(&mut a, &net)?;
    a.ask(&net.a, &net.network, "c", approval(&net, "q-1", "r-1")?, 2)?;
    a.ask(&net.a, &net.network, "c", approval(&net, "q-2", "r-2")?, 2)?;
    a.ask(&net.a, &net.network, "c", question(&net, "q-3", "t3")?, 2)?;
    let written = a.close_lost_questions(&net.a, &net.network, &held(&["r-2"]), 3)?;
    assert_eq!(
        written
            .iter()
            .map(|(scope, _)| scope.as_str())
            .collect::<Vec<_>>(),
        vec!["c"],
        "only the request no longer held closes its question"
    );
    assert!(
        a.close_lost_questions(&net.a, &net.network, &held(&["r-2"]), 4)?
            .is_empty(),
        "a closed question is closed once"
    );
    sync(&a, &mut b, &net)?;
    for store in [&a, &b] {
        assert_eq!(unanswered(store, &net)?, vec!["q-2", "q-3"]);
        assert_eq!(closed(store, &net, "q-1")?, "request_lost");
        let chat = store.conversation_questions(&net.network, "c")?;
        assert_eq!(chat.len(), 3, "a closed question stays in its chat");
    }
    let signed = Some(answer_signature(&net, "c", "q-1", "允许")?);
    let refused = b
        .accept_answer(&net.b, &net.network, "q-1", "允许", signed, 5)
        .err()
        .context("answering a closed question fails")?;
    assert!(refused.to_string().contains("closed"), "{refused}");
    assert!(
        b.messages(&net.network, "c")?.is_empty(),
        "nothing is posted"
    );
    Ok(())
}

#[test]
fn only_the_asking_device_closes_its_question() -> Result<()> {
    let net = net()?;
    let (mut a, mut b) = (store(&net)?, store(&net)?);
    conversation(&mut a, &net)?;
    a.ask(&net.a, &net.network, "c", approval(&net, "q-1", "r-1")?, 2)?;
    sync(&a, &mut b, &net)?;
    assert!(
        b.close_lost_questions(&net.b, &net.network, &held(&[]), 3)?
            .is_empty(),
        "another device holds none of this device's requests"
    );
    let forged = EventBody::QuestionClosed {
        question_id: "q-1".into(),
        reason: CloseReason::RequestLost,
    };
    b.append_event(&net.b, &net.network, "c", forged, vec![], 3)?;
    sync(&b, &mut a, &net)?;
    for store in [&a, &b] {
        assert_eq!(unanswered(store, &net)?, vec!["q-1"]);
        assert_eq!(closed(store, &net, "q-1")?, Value::Null);
    }
    Ok(())
}

#[test]
fn an_answer_wins_over_a_close_in_either_order() -> Result<()> {
    let net = net()?;
    let (mut a, mut b) = (store(&net)?, store(&net)?);
    conversation(&mut a, &net)?;
    a.ask(&net.a, &net.network, "c", approval(&net, "q-1", "r-1")?, 2)?;
    a.ask(&net.a, &net.network, "c", approval(&net, "q-2", "r-2")?, 2)?;
    let signed = Some(answer_signature(&net, "c", "q-2", "继续")?);
    a.accept_answer(&net.a, &net.network, "q-2", "继续", signed, 3)?;
    sync(&a, &mut b, &net)?;
    let signed = Some(answer_signature(&net, "c", "q-1", "允许")?);
    b.accept_answer(&net.b, &net.network, "q-1", "允许", signed, 4)?;
    let written = a.close_lost_questions(&net.a, &net.network, &held(&[]), 4)?;
    assert_eq!(written.len(), 1, "an answered question is never closed");
    assert_eq!(closed(&a, &net, "q-1")?, "request_lost");
    sync(&a, &mut b, &net)?;
    sync(&b, &mut a, &net)?;
    for store in [&a, &b] {
        assert!(unanswered(store, &net)?.is_empty());
        let question = store.question(&net.network, "q-1")?.context("question")?;
        assert_eq!(
            (
                question["answer"]["text"].clone(),
                question["closed"].clone()
            ),
            (json!("允许"), Value::Null),
            "the answer stands whether it or the close arrived first"
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

fn session_version(store: &Store, net: &Net, session: &str) -> Result<String> {
    for scope in ["c", "d"] {
        if let Some(row) = store
            .attributed_sessions(&net.network, scope)?
            .into_iter()
            .find(|row| row["session_id"] == session)
        {
            return Ok(row["version"].as_str().context("version")?.to_owned());
        }
    }
    Ok(String::new())
}

#[test]
fn a_hand_over_from_an_old_version_writes_nothing() -> Result<()> {
    let net = net()?;
    let (mut local, _) = bot_with_message(&net)?;
    second_bot(&mut local, &net, "Other")?;
    let about = Subject {
        node: net.a.device_id()?,
        agent: Some(crate::providers::Provider::Codex),
        session_id: "s1".into(),
        cwd: "/w".into(),
        summary: "tests fail".into(),
        request_id: None,
    };
    local.claim_session(&net.a, &net.network, "c", &about, 3)?;
    let seen = session_version(&local, &net, "s1")?;
    let mut hand = |from: &str, seen: &str, target: Option<&str>, at: i64| {
        local.hand_over_session(&net.a, &net.network, from, "s1", seen, target, at)
    };
    let (_, moved, written) = hand("c", &seen, Some("d"), 4)?;
    assert!(written.is_some());
    assert_ne!(moved, seen, "a hand-over gives the session a new version");
    let (_, standing, written) = hand("c", &seen, Some("d"), 4)?;
    assert!(
        written.is_none() && standing == moved,
        "the same call again reads back what stands"
    );
    let (_, back, written) = hand("d", &moved, Some("c"), 5)?;
    assert!(written.is_some(), "the other bot hands it back");
    let stale = hand("c", &seen, Some("d"), 6).expect_err("c's retry is from before");
    assert!(
        stale
            .to_string()
            .contains(&format!("it is at version {back} now")),
        "a retry after the session moved on writes nothing: {stale}"
    );
    assert!(hand("c", &seen, None, 6).is_err(), "nor does a stale mute");
    assert_eq!(session_version(&local, &net, "s1")?, back);
    assert_eq!(
        local
            .session_owner(&net.network, &net.a.device_id()?, "s1")?
            .as_deref(),
        Some("c")
    );
    Ok(())
}

#[test]
fn a_session_owner_is_written_once_and_only_its_bot_hands_it_over() -> Result<()> {
    let net = net()?;
    let (mut local, _) = bot_with_message(&net)?;
    second_bot(&mut local, &net, "Other")?;
    let node = net.a.device_id()?;
    let about = Subject {
        node: node.clone(),
        agent: Some(crate::providers::Provider::Codex),
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
    let session_notifications = local
        .pending_notifications(&net.network, &net.b.device_id()?)?
        .into_iter()
        .filter(|notification| notification.kind == "session_attributed")
        .collect::<Vec<_>>();
    assert_eq!(session_notifications.len(), 1);
    assert_eq!(session_notifications[0].text, "tests fail");
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
    let mut hand = |from: &str, session: &str, target: Option<&str>, at: i64| {
        let seen = session_version(&local, &net, session)?;
        local
            .hand_over_session(&net.a, &net.network, from, session, &seen, target, at)
            .map(|(owner, _, event)| (owner, event))
    };
    assert!(hand("c", "s1", Some("nope"), 5).is_err());
    assert!(hand("c", "s2", None, 5).is_err());
    let theirs = hand("d", "s1", Some("d"), 5).expect_err("d does not follow s1");
    assert!(
        theirs.to_string().contains("Helper follows it"),
        "only the bot that follows a session hands it over: {theirs}"
    );

    let (before, moved) = hand("c", "s1", Some("d"), 5)?;
    assert_eq!((before.as_str(), moved.is_some()), ("c", true));
    let again = hand("c", "s1", Some("d"), 6)?;
    assert!(again.1.is_none(), "a repeated hand-over writes nothing");
    assert!(
        hand("c", "s1", None, 6).is_err(),
        "a session handed over is no longer the sender's"
    );
    assert_eq!(owner(&local)?.as_deref(), Some("d"));
    assert!(local.attributed_sessions(&net.network, "c")?.is_empty());

    let mut hand = |from: &str, session: &str, target: Option<&str>, at: i64| {
        let seen = session_version(&local, &net, session)?;
        local
            .hand_over_session(&net.a, &net.network, from, session, &seen, target, at)
            .map(|(owner, _, event)| (owner, event))
    };
    assert!(hand("d", "s1", None, 7)?.1.is_some());
    assert!(hand("d", "s1", None, 8)?.1.is_none());
    assert!(
        hand("c", "s1", Some("c"), 8).is_err(),
        "a muted session stays with the bot that stopped following it"
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
    assert!(
        other
            .pending_notifications(&net.network, &net.b.device_id()?)?
            .into_iter()
            .all(|notification| notification.kind != "session_attributed"),
        "a projection rebuild does not backfill old session reminders"
    );

    let seen = session_version(&local, &net, "s1")?;
    local.hand_over_session(&net.a, &net.network, "d", "s1", &seen, Some("c"), 10)?;
    assert_eq!(owner(&local)?.as_deref(), Some("c"));
    assert!(
        !local.session_muted(&net.network, &node, "s1")?,
        "handing a muted session to a bot follows it again"
    );
    Ok(())
}

#[test]
fn a_failed_turn_shares_its_kind_and_keeps_the_raw_detail_local() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
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
    conversation(&mut store, &net)?;
    assert_eq!(store.busy_conversations()?.get("c"), None);
    for (id, at) in [("m1", 2), ("m2", 3)] {
        store.accept_message(
            &net.a,
            &net.network,
            "c",
            id,
            "hi",
            vec![],
            true,
            signature(&net, id, "hi")?,
            at,
        )?;
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
    super::scheduler::admit_from(
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
            kind: WorkKind::Agent(Provider::Codex),
            cwd: "/tmp".into(),
            title: "t".into(),
            prompt: "p".repeat(600),
            continues: None,
            parent: None,
            thread: None,

            model: None,

            resolved: ResolvedModel {
                provider: Provider::Codex,

                model: None,
            },
        };
        store.accept_work_fixture(&net.a, &net.network, spec, 200 + i)?;
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

        runtime: None,

        pending_check: false,

        tool_calls: false,
    };
    let history = store.turn_history(&net.network, "c", &job, None)?;
    let materialize = |bot: Value| {
        crate::scheduler::materialize(&history, &job, &works, &[], bot, None, &[], None)
            .map(|(prompt, _)| prompt)
    };
    let newest = json!(
        super::event_about_from(
            &store.connection,
            &net.network,
            "c",
            "message_posted",
            "m48"
        )?
        .context("m48")?
        .event
        .seq
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
        listed(&roomy, "/conversation/messages", "seq").last(),
        Some(&newest),
        "the newest message before the trigger closes the history"
    );

    let notes = (0..8)
        .map(|i| json!({"memory_id":format!("n{i}"),"text":"y".repeat(400)}))
        .collect::<Vec<_>>();
    let full_bot = json!({"configured_bot":{"name":"Helper","instructions":"i".repeat(MAX_INSTRUCTIONS_BYTES)},
        "skills":notes,"schedules":notes,"memory":notes,"sessions":notes,"bots":notes});
    let prompt = materialize(full_bot)?;
    assert!(prompt.len() <= crate::scheduler::PROMPT_LIMIT);
    let tight = prompt_data(&prompt)?;
    let seqs = listed(&tight, "/conversation/messages", "seq");
    assert_eq!(seqs.last(), Some(&newest), "the newest messages stay");
    assert!(
        seqs.windows(2)
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
fn the_bot_prompt_lists_the_newest_sessions_it_follows() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    second_bot(&mut store, &net, "Other")?;
    let node = net.a.device_id()?;
    let about = |session: &str| Subject {
        node: node.clone(),
        agent: Some(crate::providers::Provider::Codex),
        session_id: session.into(),
        cwd: "/work/project".into(),
        summary: format!("{session}: {}", "tests fail again. ".repeat(6)),
        request_id: None,
    };
    for i in 0..20 {
        store.claim_session(
            &net.a,
            &net.network,
            "c",
            &about(&format!("s{i:02}")),
            10 + i,
        )?;
    }
    store.claim_session(&net.a, &net.network, "d", &about("theirs"), 40)?;
    let seen = session_version(&store, &net, "s19")?;
    let (_, version, _) =
        store.hand_over_session(&net.a, &net.network, "c", "s19", &seen, None, 50)?;
    let bot = crate::scheduler::bot_value(&store, &net.network, "c", 100)?;
    let shown = listed(&bot, "/sessions", "session_id");
    let omitted = bot["sessions_omitted"].as_u64().context("omitted")? as usize;
    assert!(
        omitted > 0 && shown.len() + omitted == 20,
        "the index stays within its budget: {} shown, {omitted} omitted",
        shown.len()
    );
    let expected: Vec<Value> = (20 - shown.len()..20)
        .map(|i| json!(format!("s{i:02}")))
        .collect();
    assert_eq!(shown, expected, "the newest sessions, oldest first");
    let muted = &bot["sessions"][shown.len() - 1];
    assert_eq!(
        (
            muted["following"].clone(),
            muted["cwd"].clone(),
            muted["node"].clone(),
            muted["version"].clone()
        ),
        (
            json!(false),
            json!("/work/project"),
            json!(node),
            json!(version)
        ),
        "a bot names the version it sees"
    );
    assert_eq!(bot["sessions"][0]["following"], json!(true));
    let theirs = crate::scheduler::bot_value(&store, &net.network, "d", 100)?;
    assert_eq!(
        listed(&theirs, "/sessions", "session_id"),
        vec![json!("theirs")]
    );
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
    conversation(&mut a_side, &net)?;
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
        .steer_candidates(&net.network, &local, "boot", Provider::Codex)?
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
            kind: WorkKind::Agent(Provider::Codex),
            cwd: "/tmp".into(),
            title: "t".into(),
            prompt: "p".repeat(300),
            continues: None,
            parent: None,
            thread: None,

            model: None,

            resolved: ResolvedModel {
                provider: Provider::Codex,

                model: None,
            },
        })
    };
    for i in 0..3 {
        store.accept_work_fixture(&net.a, &net.network, work(i)?, 10 + i as i64)?;
    }
    let local = net.a.device_id()?;
    let render = |store: &Store, trigger: &str, resumed: Option<&VersionVector>, thread| {
        let job = SchedulerJob {
            conversation_id: "c".into(),
            trigger: message_trigger(trigger),
            kind: TriggerKind::Human,
            sequence: 0,
            basis: store.version_vector(&net.network)?,
            reply_to: trigger.into(),
            message_id: "reply".into(),
            state: JobState::Queued,

            runtime: None,

            pending_check: false,

            tool_calls: false,
        };
        let history = store.turn_history(
            &net.network,
            "c",
            &job,
            resumed.map(|covers| (covers, local.as_str())),
        )?;
        let works = store.works(&net.network, "c")?;
        let bot = crate::scheduler::bot_value(store, &net.network, "c", 100)?;
        let rendered =
            crate::scheduler::materialize(&history, &job, &works, &[], bot, None, &[], thread)?;
        Ok::<_, anyhow::Error>((rendered, job.basis))
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

    store.accept_work_fixture(&net.a, &net.network, work(3)?, 20)?;
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
    assert_eq!(data["trigger"]["event"]["body"]["text"], "next");
    assert_eq!(
        data["conversation"]["messages"],
        json!([]),
        "the trigger is not repeated among the messages"
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
    assert_eq!(
        data["memory_context"]["upserts"][0]["text"],
        "prefers short replies"
    );
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
        dynamic: None,
        staged_memories: Default::default(),
        session_generation: 0,
        session_lineage: Vec::new(),
        ..Shown::default()
    };
    let mut turn = |id: &str, key: &str| -> Result<crate::scheduling::ClaimedJob> {
        say(&mut store, &net, id, true)?;
        let claimed = store
            .claim_next_fixture(&net.a, &net.network, "c", "boot", key)?
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
    assert_eq!(
        turn("m1", "cfg")?.shown,
        Shown {
            session_generation: 1,
            ..Shown::default()
        }
    );
    let resumed = turn("m2", "cfg")?;
    assert_eq!(resumed.previous_native_id.as_deref(), Some("thread"));
    assert_eq!(resumed.shown.session_generation, 1);
    assert!(resumed.shown.session_lineage.is_empty());
    let fresh = turn("m3", "cfg-new-instructions")?;
    assert_eq!(fresh.previous_native_id, None);
    assert_eq!(fresh.shown.session_generation, 2);
    assert_eq!(fresh.shown.session_lineage.len(), 1);
    assert_eq!(fresh.shown.session_lineage[0].generation, 1);
    Ok(())
}

fn job_at(store: &Store, net: &Net, trigger: &str) -> Result<SchedulerJob> {
    Ok(SchedulerJob {
        conversation_id: "c".into(),
        trigger: message_trigger(trigger),
        kind: TriggerKind::Human,
        sequence: 0,
        basis: store.version_vector(&net.network)?,
        reply_to: trigger.into(),
        message_id: "reply".into(),
        state: JobState::Queued,

        runtime: None,

        pending_check: false,

        tool_calls: false,
    })
}

fn message_seq(store: &Store, net: &Net, message: &str) -> Result<u64> {
    Ok(super::event_about_from(
        &store.connection,
        &net.network,
        "c",
        "message_posted",
        message,
    )?
    .context("message")?
    .event
    .seq)
}

#[test]
fn compiled_context_tracks_observed_turn_state_without_private_values() -> Result<()> {
    use crate::scheduling::{ContextProjection, ContextStage, QueueHoldReason};
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    for (index, outcome, expected) in [
        (1, JobOutcome::Completed, ContextStage::Completed),
        (
            2,
            JobOutcome::Interrupted {
                failure: ExecFailure::new(FailureKind::Lost, "unknown"),
                process_unconfirmed: true,
            },
            ContextStage::Unknown,
        ),
    ] {
        let message = format!("context-{index}");
        say(&mut store, &net, &message, true)?;
        let claimed = store
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .context("claim")?;
        let trigger = claimed.job.trigger.clone();
        let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
        let projection = ContextProjection {
            attempt_id: attempt.clone(),
            provider: Provider::Codex,
            stable_fingerprint: crate::crypto::digest(b"stable"),
            dynamic_fingerprint: crate::crypto::digest(b"dynamic"),
            session_generation: claimed.shown.session_generation,
            coverage: Default::default(),
            stable_bytes: 100,
            dynamic_bytes: 200,
            invalidation_reason: None,
            stage: ContextStage::Compiled,
            session_status: None,
        };
        store.record_compiled_context("c", &trigger, &attempt, &projection)?;
        assert_eq!(
            store
                .scheduler_context_projection("c")?
                .context("compiled")?
                .stage,
            ContextStage::Compiled
        );
        assert_eq!(
            store.scheduler_queue_hold(
                &net.network,
                &net.a.device_id()?,
                "c",
                &JobState::Queued
            )?,
            Some(QueueHoldReason::ActiveTurn)
        );
        store.mark_started("c", &trigger, &attempt)?;
        store.save_scheduler_native_id(
            "c",
            &trigger,
            &attempt,
            "private-native-thread",
            "private-bearer",
            false,
        )?;
        let before = memory_test_shown(&store)?;
        let covers: String = store.connection.query_row(
            "SELECT covers FROM scheduler_sessions WHERE conversation_id='c' AND scope='main'",
            [],
            |row| row.get(0),
        )?;
        store.record_context_started(&attempt)?;
        let observed = memory_test_shown(&store)?;
        assert_eq!(
            observed.context.as_ref().context("started callback")?.stage,
            ContextStage::Started
        );
        assert_eq!(observed.memory, before.memory);
        assert_eq!(observed.works, before.works);
        assert_eq!(
            covers,
            store.connection.query_row::<String, _, _>(
                "SELECT covers FROM scheduler_sessions WHERE conversation_id='c' AND scope='main'",
                [],
                |row| row.get(0),
            )?
        );
        store.record_manager_turn(
            &attempt,
            "system",
            "private-native-thread",
            Some("turn"),
            &Shown::default(),
        )?;
        assert_eq!(
            store
                .scheduler_context_projection("c")?
                .context("started")?
                .stage,
            ContextStage::Started
        );
        store.finish_job(
            &net.a,
            &net.network,
            "c",
            &trigger,
            &attempt,
            outcome,
            "reply",
            5,
        )?;
        store.record_context_started(&attempt)?;
        store.record_manager_turn(
            &attempt,
            "system",
            "private-native-thread",
            Some("turn"),
            &Shown::default(),
        )?;
        let context = store
            .scheduler_context_projection("c")?
            .context("terminal")?;
        assert_eq!(context.stage, expected);
        let encoded = serde_json::to_string(&context)?;
        assert!(encoded.len() < 2_048);
        assert!(!encoded.contains("private-native-thread"));
        assert!(!encoded.contains("private-bearer"));
    }
    Ok(())
}

#[test]
fn a_long_event_reads_whole_page_by_page_within_the_cap() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    let text = "好\"\\\n\u{1}a".repeat(20_000);
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m1", &text),
        vec![],
        10,
    )?;
    let seq = message_seq(&store, &net, "m1")?;
    let author = net.a.device_id()?;
    let event = store
        .scope_event(&net.network, "c", &author, seq)?
        .context("m1")?;
    let whole = crate::scheduler::agent_event_body(&event.event.body).to_string();
    let mut read = String::new();
    let mut pages = 0;
    let mut offset = Some(0);
    while let Some(at) = offset {
        let page =
            crate::runtime::agent_tools::event_page(&store, &net.network, "c", &author, seq, at)?;
        assert!(
            page.to_string().len() <= 32_000,
            "one result stays within the cap"
        );
        read.push_str(page["body_json"].as_str().context("body_json")?);
        offset = page["next_offset"].as_u64().map(|next| next as usize);
        pages += 1;
    }
    assert!(pages > 4);
    assert_eq!(read, whole, "the pages joined are the whole body");
    let body: Value = serde_json::from_str(&read)?;
    assert_eq!(body["text"], text);
    Ok(())
}

#[test]
fn history_pages_read_at_the_turn_basis_by_position_and_type() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    for i in 1..=5 {
        store.append_event(
            &net.a,
            &net.network,
            "c",
            user(&net, &format!("m{i}"), &format!("t{i}")),
            vec![],
            10 + i,
        )?;
    }
    let basis = store.version_vector(&net.network)?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m6", "t6"),
        vec![],
        20,
    )?;
    let author = net.a.device_id()?;
    let messages = ["message_posted".to_owned()];
    let ids = |events: Vec<HashedEvent>| -> Vec<String> {
        events
            .into_iter()
            .filter_map(|event| match event.event.body {
                EventBody::MessagePosted { message_id, .. } => Some(message_id),
                _ => None,
            })
            .collect()
    };
    let (newest, more) = store.history_page(&net.network, "c", &basis, None, &messages, 2)?;
    assert_eq!(
        ids(newest),
        ["m5", "m4"],
        "the newest within the basis, not m6 that came after"
    );
    assert!(more);
    let m3 = message_seq(&store, &net, "m3")?;
    let (older, more) = store.history_page(
        &net.network,
        "c",
        &basis,
        Some((&author, m3, false)),
        &messages,
        10,
    )?;
    assert_eq!(ids(older), ["m2", "m1"]);
    assert!(!more);
    let (newer, more) = store.history_page(
        &net.network,
        "c",
        &basis,
        Some((&author, m3, true)),
        &messages,
        10,
    )?;
    assert_eq!(
        ids(newer),
        ["m4", "m5"],
        "a position stays put while new events arrive"
    );
    assert!(!more);
    let (all, _) = store.history_page(
        &net.network,
        "c",
        &basis,
        Some((&author, m3, false)),
        &[],
        10,
    )?;
    assert!(
        all.iter()
            .any(|event| !matches!(event.event.body, EventBody::MessagePosted { .. })),
        "without types every event reads"
    );
    Ok(())
}

#[test]
fn history_search_filters_before_paging_and_keeps_the_recorded_basis() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    for (id, author, text) in [
        ("s1", &net.a, "Needle 100%_ 中文"),
        ("s2", &net.b, "needle from B"),
        ("s3", &net.a, "unrelated text"),
        ("s4", &net.a, "needle again"),
    ] {
        store.append_event(author, &net.network, "c", user(&net, id, text), vec![], 10)?;
    }
    let basis = store.version_vector(&net.network)?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "s5", "needle after basis"),
        vec![],
        11,
    )?;
    let a = net.a.device_id()?;
    let b = net.b.device_id()?;
    let kinds = ["message_posted".into()];
    let filter = || super::HistoryFilter {
        kinds: &kinds,
        author: Some(&a),
        keyword: Some("NEEDLE"),
    };
    let (newest, more) = store.search_history_page(&net.network, "c", &basis, None, filter(), 1)?;
    assert_eq!(newest.len(), 1);
    assert_eq!(newest[0].event.body.subject(), Some("s4"));
    assert!(more);
    let seq = newest[0].event.seq;
    let (older, more) = store.search_history_page(
        &net.network,
        "c",
        &basis,
        Some((&a, seq, false)),
        filter(),
        1,
    )?;
    assert_eq!(older[0].event.body.subject(), Some("s1"));
    assert!(
        !more,
        "nonmatching records cannot invent another matching page"
    );
    let (newer, more) = store.search_history_page(
        &net.network,
        "c",
        &basis,
        Some((&a, older[0].event.seq, true)),
        filter(),
        10,
    )?;
    assert_eq!(newer.len(), 1);
    assert_eq!(newer[0].event.body.subject(), Some("s4"));
    assert!(!more, "events beyond the turn basis are invisible");
    for keyword in ["100%_", "中文"] {
        let (found, more) = store.search_history_page(
            &net.network,
            "c",
            &basis,
            None,
            super::HistoryFilter {
                kinds: &kinds,
                author: None,
                keyword: Some(keyword),
            },
            1,
        )?;
        assert_eq!(found[0].event.body.subject(), Some("s1"));
        assert!(!more);
    }
    let (found, more) = store.search_history_page(
        &net.network,
        "c",
        &basis,
        None,
        super::HistoryFilter {
            kinds: &kinds,
            author: Some(&b),
            keyword: Some("needle"),
        },
        1,
    )?;
    assert_eq!(found[0].event.body.subject(), Some("s2"));
    assert!(!more);
    let (missing, more) =
        store.search_history_page(&net.network, "different-scope", &basis, None, filter(), 1)?;
    assert!(missing.is_empty() && !more);
    Ok(())
}

#[test]
fn a_turn_reads_only_past_the_checkpoint_yet_finds_an_older_trigger() -> Result<()> {
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
    let checkpoint = crate::scheduler::checkpoint(&context, &[]).context("over budget")?;
    store.append_event(&net.a, &net.network, "c", checkpoint, vec![], 50)?;
    let job = job_at(&store, &net, "m0")?;
    let history = store.turn_history(&net.network, "c", &job, None)?;
    assert!(history.checkpoint.is_some());
    let m0 = message_seq(&store, &net, "m0")?;
    assert!(
        history
            .recent
            .events
            .iter()
            .all(|event| event.event.seq > m0),
        "covered events are not loaded"
    );
    assert_eq!(
        history.trigger.as_ref().map(|event| event.event.seq),
        Some(m0),
        "the trigger is found by its index however old"
    );
    let (prompt, _) =
        crate::scheduler::materialize(&history, &job, &[], &[], json!({}), None, &[], None)?;
    let data = prompt_data(&prompt)?;
    assert_eq!(data["trigger"]["event"]["seq"], json!(m0));
    Ok(())
}

#[test]
fn a_resumed_session_is_not_shown_its_own_replies_or_the_trigger_twice() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m1", "first"),
        vec![],
        10,
    )?;
    let covers = store.version_vector(&net.network)?;
    let reply = |id: &str, status| EventBody::MessagePosted {
        message_id: id.into(),
        role: Role::Assistant,
        text: format!("reply {id}"),
        attachments: vec![],
        report_results: None,
        in_reply_to: Some("m1".into()),
        status: Some(status),
        failure: None,
        answers: None,
        attestation: None,
    };
    store.append_event(
        &net.a,
        &net.network,
        "c",
        reply("r1", ReplyStatus::Completed),
        vec![],
        11,
    )?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        reply("r2", ReplyStatus::Failed),
        vec![],
        12,
    )?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        reply("r3", ReplyStatus::Stopped),
        vec![],
        12,
    )?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m2", "second"),
        vec![],
        13,
    )?;
    let job = job_at(&store, &net, "m2")?;
    let local = net.a.device_id()?;
    let texts = |resumed: Option<(&VersionVector, &str)>| -> Result<Vec<Value>> {
        let history = store.turn_history(&net.network, "c", &job, resumed)?;
        let (prompt, _) =
            crate::scheduler::materialize(&history, &job, &[], &[], json!({}), None, &[], None)?;
        Ok(listed(
            &prompt_data(&prompt)?,
            "/conversation/messages",
            "text",
        ))
    };
    assert_eq!(
        texts(Some((&covers, &local)))?,
        [json!("reply r2"), json!("reply r3")],
        "the session holds the reply it completed; a failed or stopped one may never have run there"
    );
    assert_eq!(
        texts(None)?,
        [
            json!("first"),
            json!("reply r1"),
            json!("reply r2"),
            json!("reply r3")
        ],
        "a fresh session reads every reply, and the trigger only as the trigger"
    );
    Ok(())
}

#[test]
fn answers_become_checkpoint_decisions_that_later_checkpoints_keep() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    store.ask(&net.a, &net.network, "c", question(&net, "q-1", "t1")?, 2)?;
    let signed = Some(answer_signature(&net, "c", "q-1", "用方案 B")?);
    store.accept_answer(&net.a, &net.network, "q-1", "用方案 B", signed, 3)?;
    let text = "x".repeat(2_000);
    let mut decisions = Vec::new();
    for round in 0..2 {
        for i in 0..20 {
            let id = format!("m{round}-{i}");
            store.append_event(
                &net.a,
                &net.network,
                "c",
                user(&net, &id, &text),
                vec![],
                10 + 100 * round + i,
            )?;
        }
        let context = super::since_checkpoint_from(&store.connection, &net.network, "c")?;
        let checkpoint = crate::scheduler::checkpoint(&context, &[]).context("over budget")?;
        let EventBody::ContextCheckpoint {
            decisions: found, ..
        } = &checkpoint
        else {
            unreachable!()
        };
        decisions = found.clone();
        store.append_event(
            &net.a,
            &net.network,
            "c",
            checkpoint,
            vec![],
            90 + 100 * round,
        )?;
    }
    assert!(
        decisions
            .iter()
            .any(|decision| decision.contains("q-1") && decision.contains("用方案 B")),
        "the answer survives the rolling summary: {decisions:?}"
    );
    Ok(())
}

#[test]
fn a_turn_marks_attachments_not_on_this_device_pending() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    let content = b"hello".to_vec();
    let file = Attachment {
        hash: crate::crypto::digest(&content),
        size: content.len() as u64,
        name: "a.txt".into(),
    };
    let message = net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::MessagePosted {
            message_id: "m1".into(),
            role: Role::User,
            text: "看附件".into(),
            attachments: vec![file.clone()],
            report_results: Some(true),
            in_reply_to: None,
            status: None,
            answers: None,
            attestation: None,
            failure: None,
        },
    )?;
    store.put_blob(&file.blob(), &content)?;
    store.append_event(&net.a, &net.network, "c", message, vec![file.blob()], 10)?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m2", "再看"),
        vec![],
        11,
    )?;
    let job = job_at(&store, &net, "m2")?;
    let mut history = store.turn_history(&net.network, "c", &job, None)?;
    assert_eq!(
        history
            .shown_events()
            .flat_map(|event| &event.event.blobs)
            .count(),
        1,
        "the attachment the prompt shows is the one a turn fetches"
    );
    history.pending_blobs.insert(file.hash.clone());
    let (prompt, _) =
        crate::scheduler::materialize(&history, &job, &[], &[], json!({}), None, &[], None)?;
    let data = prompt_data(&prompt)?;
    assert_eq!(
        data.pointer("/conversation/messages/0/attachments/0/pending_download"),
        Some(&json!(true))
    );
    Ok(())
}

#[test]
fn a_streamed_event_reads_only_the_runs_on_its_thread() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    for (i, thread) in ["t-a", "t-b"].into_iter().enumerate() {
        let spec = WorkSpec {
            work_id: format!("w{i}"),
            conversation: "c".into(),
            trigger: message_trigger("m"),
            target: net.a.device_id()?,
            dispatcher: net.a.device_id()?,
            call_ref: format!("call-{i}"),
            kind: WorkKind::Agent(Provider::Codex),
            cwd: format!("/tmp/{thread}"),
            title: "t".into(),
            prompt: "p".into(),
            continues: None,
            parent: None,
            thread: Some(thread.into()),

            model: None,

            resolved: ResolvedModel {
                provider: Provider::Codex,

                model: None,
            },
        };
        store.accept_work_fixture(&net.a, &net.network, spec, 10 + i as i64)?;
    }
    let on = |thread| -> Result<Vec<String>> {
        Ok(store
            .work_records_on_thread(thread)?
            .into_iter()
            .map(|record| record.work_id)
            .collect())
    };
    assert_eq!(on("t-a")?, ["w0"]);
    assert_eq!(on("t-b")?, ["w1"]);
    assert_eq!(
        store.work_cwd_on_thread("t-b")?.as_deref(),
        Some("/tmp/t-b")
    );
    assert_eq!(store.work_cwd_on_thread("t-c")?, None);
    Ok(())
}

#[test]
fn an_attachment_page_reads_text_as_text_and_splits_on_characters() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    let attach = |store: &mut Store, id: &str, content: &[u8], time| -> Result<String> {
        let file = Attachment {
            hash: crate::crypto::digest(content),
            size: content.len() as u64,
            name: format!("{id}.bin"),
        };
        let message = net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::MessagePosted {
                message_id: id.into(),
                role: Role::User,
                text: id.into(),
                attachments: vec![file.clone()],
                report_results: Some(true),
                in_reply_to: None,
                status: None,
                answers: None,
                attestation: None,
                failure: None,
            },
        )?;
        store.put_blob(&file.blob(), content)?;
        store.append_event(&net.a, &net.network, "c", message, vec![file.blob()], time)?;
        Ok(file.hash)
    };
    let text = "好".repeat(1_000);
    let hash = attach(&mut store, "m1", text.as_bytes(), 10)?;
    let binary = attach(&mut store, "m2", &[0xff, 0x00, 0xfe], 11)?;
    let page = |hash: &str, offset: u64| {
        crate::runtime::agent_tools::blob_page(
            &store,
            &net.network,
            "c",
            hash,
            &json!({"offset":offset,"limit":1}),
        )
    };
    let mut read = String::new();
    let mut offset = Some(0);
    while let Some(at) = offset {
        let chunk = page(&hash, at)?;
        read.push_str(chunk["text"].as_str().context("UTF-8 comes back as text")?);
        offset = chunk["next_offset"].as_u64();
    }
    assert_eq!(
        read, text,
        "pages split between characters, never inside one"
    );
    assert_eq!(page(&binary, 0)?["data_base64"], "/wD+");
    std::fs::remove_file(store.blob_file(&binary)?)?;
    assert_eq!(page(&binary, 0)?["pending_download"], true);
    Ok(())
}

#[test]
fn work_status_pages_newest_first_within_a_byte_cap() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    for i in 0..30 {
        let spec = WorkSpec {
            work_id: format!("w{i:02}"),
            conversation: "c".into(),
            trigger: message_trigger("m"),
            target: net.a.device_id()?,
            dispatcher: net.a.device_id()?,
            call_ref: format!("call-{i}"),
            kind: WorkKind::Agent(Provider::Codex),
            cwd: "/tmp".into(),
            title: "t".into(),
            prompt: "p".repeat(2_000),
            continues: None,
            parent: None,
            thread: None,

            model: None,

            resolved: ResolvedModel {
                provider: Provider::Codex,

                model: None,
            },
        };
        store.accept_work_fixture(&net.a, &net.network, spec, 10 + i)?;
    }
    let works = store.works(&net.network, "c")?;
    let mut seen = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let page = crate::runtime::agent_tools::work_status(
            &works,
            &[],
            &json!({"before":before,"limit":50}),
        )?;
        assert!(
            page.to_string().len() <= 33_000,
            "a page stays within the cap"
        );
        let ids = listed(&page, "/works", "work_id");
        before = ids.last().and_then(|id| id.as_str()).map(str::to_owned);
        seen.extend(ids);
        if page["more"] != true {
            break;
        }
    }
    let expected: Vec<Value> = (0..30).rev().map(|i| json!(format!("w{i:02}"))).collect();
    assert_eq!(seen, expected, "every work once, newest first");
    Ok(())
}

#[test]
fn the_users_project_choice_replaces_and_clears_a_bots_folder() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    store.append_event(&net.a, &net.network, "c", bot(&net, "Helper")?, vec![], 2)?;
    let set = |project: Option<crate::model::BotProject>| {
        net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::BotProjectSet {
                project,
                attestation: None,
            },
        )
    };
    let project = crate::model::BotProject {
        target_node: net.a.device_id()?,
        cwd: "/srv/demo".into(),
    };
    store.append_event(
        &net.a,
        &net.network,
        "c",
        set(Some(project.clone()))?,
        vec![],
        3,
    )?;
    assert_eq!(store.bots(&net.network)?[0].project, Some(project));
    store.append_event(&net.a, &net.network, "c", set(None)?, vec![], 4)?;
    assert_eq!(store.bots(&net.network)?[0].project, None);
    let unsigned = EventBody::BotProjectSet {
        project: None,
        attestation: None,
    };
    assert!(
        store
            .append_event(&net.a, &net.network, "c", unsigned, vec![], 5)
            .is_err(),
        "only the user moves a bot's folder"
    );
    Ok(())
}

#[test]
fn a_bots_face_from_the_chat_and_from_the_panel_converge_by_rank() -> Result<()> {
    use crate::model::{Avatar, Eyes, Hue, Outline};
    use crate::store::bot_settings::AvatarParts;
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    let hue = |hue: u16| Hue::try_from(hue).map_err(anyhow::Error::msg);
    let panel = Avatar {
        outline: Outline::Bean,
        eyes: Eyes::Dot,
        hue: hue(155)?,
    };
    assert!(
        Hue::try_from(70).is_err(),
        "the halo's amber is no face hue"
    );
    assert!(
        serde_json::from_value::<ChangeRequest>(
            json!({"tool":"update_profile","avatar":{"hue":70}})
        )
        .is_err()
    );
    let parts = |outline, eyes, hue| AvatarParts { outline, eyes, hue };
    let picked = |avatar: Option<AvatarParts>, default_avatar| ChangeRequest::UpdateProfile {
        name: None,
        instructions: None,
        avatar,
        default_avatar,
    };
    let early = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
    let mut change = |call_ref: &str, request: ChangeRequest, at: i64| {
        origin.change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            call_ref,
            request,
            Provider::Codex,
            at,
        )
    };
    let derived = Avatar::derived("bot");
    let cloud = parts(Some(Outline::Cloud), None, None);
    let (first, written) = change("f1", picked(Some(cloud), None), 4)?;
    let chat = Avatar {
        outline: Outline::Cloud,
        ..derived
    };
    assert!(written);
    assert!(
        matches!(first.change, BotChange::Profile { avatar: Some(ref edit), .. }
            if edit.before.is_none() && edit.after == Some(chat)),
        "the named part changes and the rest stays as the id gives it: {:?}",
        first.change
    );
    assert_eq!(
        change("f1", picked(Some(cloud), None), 5)?,
        (first.clone(), false),
        "the same call_ref with the same content reads back"
    );
    let other = change(
        "f1",
        picked(Some(parts(Some(Outline::Bean), None, None)), None),
        5,
    )
    .unwrap_err();
    assert!(other.to_string().contains("call_ref"), "{other}");
    let unchanged = change("f2", picked(Some(cloud), None), 5).unwrap_err();
    assert!(
        unchanged.to_string().contains("nothing to change"),
        "{unchanged}"
    );
    let empty = change("f3", picked(Some(AvatarParts::default()), None), 5).unwrap_err();
    assert!(
        empty.to_string().contains("outline, eyes or hue"),
        "{empty}"
    );
    let both = change("f4", picked(Some(cloud), Some(true)), 5).unwrap_err();
    assert!(both.to_string().contains("not both"), "{both}");
    let late = origin.events_from(
        &net.network,
        &net.a.device_id()?,
        early.len() as u64 + 1,
        100,
    )?;

    let mut panel_device = store(&net)?;
    panel_device.ingest_events(&net.network, &early)?;
    let signed = |version, avatar| {
        net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::BotAvatarSet {
                version,
                avatar,
                attestation: None,
            },
        )
    };
    let unsigned = EventBody::BotAvatarSet {
        version: None,
        avatar: Some(panel),
        attestation: None,
    };
    assert!(
        panel_device
            .append_event(&net.b, &net.network, "c", unsigned, vec![], 9)
            .is_err(),
        "only the user picks a face from the panel"
    );
    let mut forged = signed(None, Some(panel))?;
    if let EventBody::BotAvatarSet { version, .. } = &mut forged {
        *version = Some("0123456789abcdef".into());
    }
    assert!(
        panel_device
            .append_event(&net.b, &net.network, "c", forged, vec![], 9)
            .is_err(),
        "the signature covers the version the user saw"
    );
    panel_device.append_event(
        &net.b,
        &net.network,
        "c",
        signed(None, Some(panel))?,
        vec![],
        9,
    )?;
    let picks = panel_device.events_from(&net.network, &net.b.device_id()?, 1, 100)?;

    let mut faces = Vec::new();
    for batches in [[&early, &late, &picks], [&early, &picks, &late]] {
        let mut device = store(&net)?;
        for batch in batches {
            let statuses = device.ingest_events(&net.network, batch)?.statuses;
            assert!(
                statuses
                    .iter()
                    .all(|status| *status == IngestStatus::Accepted),
                "{statuses:?}"
            );
        }
        faces.push(
            device
                .bot_for_conversation(&net.network, "c")?
                .context("bot")?
                .avatar,
        );
    }
    assert_eq!(
        faces,
        [Some(panel), Some(panel)],
        "the later rank wins in any order"
    );

    let mut back = store(&net)?;
    all_accepted(&mut back, &net, &origin)?;
    back.ingest_events(&net.network, &picks)?;
    let (reset, _) = back.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "f5",
        picked(None, Some(true)),
        Provider::Codex,
        20,
    )?;
    assert!(
        matches!(reset.change, BotChange::Profile { avatar: Some(ref edit), .. }
            if edit.before == Some(panel) && edit.after.is_none()),
        "default_avatar writes null: {:?}",
        reset.change
    );
    let reset_bot = back
        .bot_for_conversation(&net.network, "c")?
        .context("bot")?;
    assert_eq!(
        (reset_bot.avatar, reset_bot.avatar_version),
        (None, Some(super::version(&reset.event_hash))),
        "a later chat change outranks the panel's pick and gives the face a new version"
    );
    second_bot(&mut back, &net, "Ops")?;
    let shown = back
        .bot_for_conversation(&net.network, "c")?
        .context("bot")?
        .shown_avatar();
    assert_eq!((shown.avatar, shown.picked), (derived, false));
    assert_eq!(
        crate::scheduler::bot_value(&back, &net.network, "c", 0)?["configured_bot"]["avatar"],
        json!({"outline":derived.outline,"eyes":derived.eyes,"hue":derived.hue,"picked":false}),
        "a turn sees the face the bot shows"
    );

    let (other_bot, _) = back.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "f6",
        ChangeRequest::ConfigureBot {
            bot: "Ops".into(),
            name: None,
            instructions: None,
            model: None,
            skills: None,
            all_skills: None,
            avatar: Some(parts(None, Some(Eyes::Wide), Some(hue(285)?))),
            default_avatar: None,

            reasoning_effort: None,
        },
        Provider::Codex,
        21,
    )?;
    let ops = back
        .bot_for_conversation(&net.network, "d")?
        .context("ops")?;
    assert_eq!(other_bot.conversation_id, "d");
    assert_eq!(
        ops.avatar,
        Some(Avatar {
            eyes: Eyes::Wide,
            hue: hue(285)?,
            ..Avatar::derived("bot-d")
        }),
        "configure_bot gives another bot a face"
    );
    Ok(())
}

#[test]
fn the_node_derives_the_same_face_from_an_id_as_the_clients() -> Result<()> {
    use crate::model::Avatar;
    #[derive(serde::Deserialize)]
    struct Vector {
        bot_id: String,
        #[serde(flatten)]
        face: serde_json::Value,
    }
    let vectors: Vec<Vector> = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../packages/client/src/avatar/derived-faces.json"
    )))?;
    let (mut hues, mut outlines, mut eyes) = (
        std::collections::BTreeSet::new(),
        std::collections::BTreeSet::new(),
        std::collections::BTreeSet::new(),
    );
    for vector in &vectors {
        let face = Avatar::derived(&vector.bot_id);
        assert_eq!(
            serde_json::to_value(face)?,
            vector.face,
            "{}",
            vector.bot_id
        );
        hues.insert(u16::from(face.hue));
        outlines.insert(serde_json::to_string(&face.outline)?);
        eyes.insert(serde_json::to_string(&face.eyes)?);
    }
    assert_eq!(
        hues.into_iter().collect::<Vec<_>>(),
        crate::model::HUES,
        "the vectors reach every hue, so both lists agree"
    );
    assert_eq!(
        (outlines.len(), eyes.len()),
        (5, 4),
        "the vectors reach every outline and eye shape, so both orders agree"
    );
    Ok(())
}

#[test]
fn read_bot_lists_every_skill_the_prompt_index_leaves_out() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    for i in 0..40 {
        store.change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            &format!("s{i}"),
            save_skill(None, &format!("skill {i:02}"), "steps"),
            Provider::Codex,
            10 + i,
        )?;
    }
    let bot = crate::scheduler::bot_value(&store, &net.network, "c", 100)?;
    let omitted = bot["skills_omitted"].as_u64().context("omitted")?;
    assert!(omitted > 0, "the prompt index stays within its budget");
    let read = crate::runtime::agent_tools::read_bot(&store, &net.network, "Helper", 100)?;
    assert_eq!(read["uses_all_skills"], json!(true));
    let names = listed(&read, "/skills", "name");
    let expected: Vec<Value> = (0..40).map(|i| json!(format!("skill {i:02}"))).collect();
    assert_eq!(
        names, expected,
        "a bot using every skill reads the whole library"
    );
    let ids = listed(&read, "/skills", "skill_id");
    assert!(
        ids.iter().all(Value::is_string),
        "each names the skill_id read_skill takes"
    );
    let chosen: std::collections::BTreeSet<String> = ids[..2]
        .iter()
        .filter_map(|id| id.as_str().map(str::to_owned))
        .collect();
    store.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "pick",
        ChangeRequest::ConfigureBot {
            bot: "Helper".into(),
            name: None,
            instructions: None,
            model: None,
            skills: Some(chosen),
            all_skills: None,
            avatar: None,
            default_avatar: None,

            reasoning_effort: None,
        },
        Provider::Codex,
        60,
    )?;
    let read = crate::runtime::agent_tools::read_bot(&store, &net.network, "Helper", 100)?;
    assert_eq!(read["uses_all_skills"], json!(false));
    assert_eq!(
        listed(&read, "/skills", "skill_id"),
        ids[..2].to_vec(),
        "only those it uses"
    );
    Ok(())
}

#[test]
fn the_bot_prompt_lists_the_newest_messages_it_sent_other_bots() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    second_bot(&mut store, &net, "Docs")?;
    for i in 0..6 {
        store.message_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            &format!("ask-{i}"),
            "Docs",
            &format!("Question {i}: {}", "please check the docs. ".repeat(40)),
            10 + i * 3_600_001,
        )?;
    }
    let bot = crate::scheduler::bot_value(&store, &net.network, "c", 100)?;
    let texts: Vec<String> = listed(&bot, "/sent", "text")
        .iter()
        .filter_map(|text| Some(text.as_str()?[..10].to_owned()))
        .collect();
    let omitted = bot["sent_omitted"].as_u64().context("omitted")? as usize;
    assert!(omitted > 0 && texts.len() + omitted == 6, "{texts:?}");
    let expected: Vec<String> = (6 - texts.len()..6)
        .map(|i| format!("Question {i}"))
        .collect();
    assert_eq!(texts, expected, "the newest sent messages, oldest first");
    assert_eq!(bot["sent"][0]["to"]["name"], json!("Docs"));
    assert_eq!(bot["sent"][0]["to"]["conversation_id"], json!("d"));
    let theirs = crate::scheduler::bot_value(&store, &net.network, "d", 100)?;
    assert_eq!(theirs["sent"], json!([]), "only what this bot sent");
    Ok(())
}

#[test]
fn list_sessions_pages_every_session_with_the_version_a_hand_over_needs() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    second_bot(&mut store, &net, "Other")?;
    let node = net.a.device_id()?;
    for i in 0..40 {
        store.claim_session(
            &net.a,
            &net.network,
            "c",
            &Subject {
                node: node.clone(),
                agent: Some(Provider::Codex),
                session_id: format!("s{i:02}"),
                cwd: "/work/project".into(),
                summary: "tests fail again. ".repeat(6),
                request_id: None,
            },
            10 + i,
        )?;
    }
    let bot = crate::scheduler::bot_value(&store, &net.network, "c", 100)?;
    let shown = listed(&bot, "/sessions", "session_id");
    assert!(
        !shown.contains(&json!("s00")),
        "the oldest falls out of the prompt"
    );
    store.claim_session(
        &net.a,
        &net.network,
        "d",
        &Subject {
            node: node.clone(),
            agent: Some(Provider::Codex),
            session_id: "theirs".into(),
            cwd: "/work/project".into(),
            summary: String::new(),
            request_id: None,
        },
        60,
    )?;
    let mut seen = Vec::new();
    let mut before = Value::Null;
    loop {
        let sessions = store.attributed_sessions(&net.network, "c")?;
        let page = crate::runtime::agent_tools::list_sessions(
            &sessions,
            &json!({"before":before,"limit":7}),
        )?;
        let rows = page["sessions"].as_array().context("sessions")?.clone();
        if before.is_null() {
            let last = rows.last().context("a page has rows")?;
            let version = last["version"].as_str().context("version")?;
            let session = last["session_id"].as_str().context("session")?;
            store.hand_over_session(&net.a, &net.network, "c", session, version, Some("d"), 70)?;
        }
        seen.extend(rows);
        if page["more"] != true {
            break;
        }
        before = page["next"].clone();
    }
    let ids: Vec<Value> = seen.iter().map(|row| row["session_id"].clone()).collect();
    let expected: Vec<Value> = (0..40).rev().map(|i| json!(format!("s{i:02}"))).collect();
    assert_eq!(
        ids, expected,
        "every session once, newest first, though one moved between pages"
    );
    let oldest = seen.last().context("oldest")?;
    let version = oldest["version"].as_str().context("version")?;
    let (_, _, written) =
        store.hand_over_session(&net.a, &net.network, "c", "s00", version, Some("d"), 80)?;
    assert!(
        written.is_some(),
        "a session left out of the prompt is handed over"
    );
    Ok(())
}

#[test]
fn the_bot_prompt_lists_schedules_by_their_next_run() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    for (call_ref, name, at) in [
        ("a", "alpha", 900_000),
        ("b", "beta", 300_000),
        ("g", "gamma", 600_000),
    ] {
        store.change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            call_ref,
            ChangeRequest::SaveSchedule {
                schedule_id: None,
                name: name.into(),
                prompt: "Check the deploy and report what changed.".into(),
                first_run_at_ms: at,
                every_minutes: None,
                enabled: None,
            },
            Provider::Codex,
            10,
        )?;
    }
    let bot = crate::scheduler::bot_value(&store, &net.network, "c", 100)?;
    assert_eq!(
        listed(&bot, "/schedules", "name"),
        vec![json!("beta"), json!("gamma"), json!("alpha")],
        "the soonest run first"
    );
    Ok(())
}

#[test]
fn an_oversized_bot_gives_way_instead_of_failing_the_turn() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m1", &"\n".repeat(4_000)),
        vec![],
        10,
    )?;
    let job = job_at(&store, &net, "m1")?;
    let history = store.turn_history(&net.network, "c", &job, None)?;
    let rows = |key: &str| {
        (0..2)
            .map(|i| json!({"id":format!("{key}{i}"),"text":"\"".repeat(700)}))
            .collect::<Vec<_>>()
    };
    let mut bot = json!({"configured_bot":{"name":"Helper","instructions":"\"".repeat(MAX_INSTRUCTIONS_BYTES)},"now_ms":0});
    for key in ["skills", "schedules", "memory", "sessions", "sent", "bots"] {
        bot[key] = json!(rows(key));
        bot[format!("{key}_omitted")] = json!(0);
    }
    let roots: Vec<std::path::PathBuf> = (0..16)
        .map(|i| format!("/srv/{i}/{}", "deep/".repeat(100)).into())
        .collect();
    let (prompt, shown) =
        crate::scheduler::materialize(&history, &job, &[], &[], bot, None, &roots, None)?;
    assert!(prompt.len() <= crate::scheduler::PROMPT_LIMIT);
    assert_eq!(shown.bot, None, "a bot cut short is shown whole next turn");
    let data = prompt_data(&prompt)?;
    let omitted: u64 = ["skills", "schedules", "memory", "sessions", "sent", "bots"]
        .iter()
        .map(|key| data["bot"][format!("{key}_omitted")].as_u64().unwrap_or(0))
        .sum();
    assert!(omitted > 0, "the bot's indexes give way");
    assert_eq!(
        data["bot"]["configured_bot"]["name"],
        json!("Helper"),
        "the bot itself stays"
    );
    Ok(())
}

#[test]
fn the_prompt_never_carries_the_users_device_signature() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    let message = user(&net, "m1", "deploy it");
    let signature = message
        .attestation()
        .context("the user's message is signed")?
        .signature
        .clone();
    store.append_event(&net.a, &net.network, "c", message, vec![], 10)?;
    let job = job_at(&store, &net, "m1")?;
    let history = store.turn_history(&net.network, "c", &job, None)?;
    let (prompt, _) =
        crate::scheduler::materialize(&history, &job, &[], &[], json!({}), None, &[], None)?;
    let data = prompt_data(&prompt)?;
    assert_eq!(data["trigger"]["event"]["body"]["text"], json!("deploy it"));
    assert!(!prompt.contains(&signature), "no signature in the prompt");
    assert!(!prompt.contains("attestation"));
    let seq = message_seq(&store, &net, "m1")?;
    let page = crate::runtime::agent_tools::event_page(
        &store,
        &net.network,
        "c",
        &net.a.device_id()?,
        seq,
        0,
    )?;
    assert!(
        !page.to_string().contains(&signature),
        "nor in what context reads"
    );
    Ok(())
}

#[test]
fn open_works_list_only_what_still_wants_the_user() -> Result<()> {
    use crate::work::{Dispatch, DispatchState};
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
    let report = super::scheduler::reply_message_id("c", &work_trigger("failed"), 0);
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
    store.accept_message(
        &net.a,
        &net.network,
        "c",
        "m2",
        "retry",
        vec![],
        true,
        signature(&net, "m2", "retry")?,
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
    store.accept_message(
        &net.a,
        &net.network,
        "c",
        "m3",
        "again",
        vec![],
        true,
        signature(&net, "m3", "again")?,
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

fn isolated_task(store: &mut Store, net: &Net, work_id: &str) -> Result<WorkSpec> {
    let spec = WorkSpec {
        work_id: work_id.into(),
        conversation: "c".into(),
        trigger: message_trigger("m1"),
        target: net.a.device_id()?,
        dispatcher: net.a.device_id()?,
        call_ref: work_id.into(),
        kind: WorkKind::Task,
        cwd: "/tmp".into(),
        title: work_id.into(),
        prompt: "read the task".into(),
        continues: None,
        parent: None,
        thread: None,

        model: None,

        resolved: ResolvedModel {
            provider: Provider::Codex,

            model: None,
        },
    };
    assert!(
        store
            .accept_work_fixture(&net.a, &net.network, spec.clone(), 4)?
            .1
    );
    Ok(spec)
}

fn isolated_claim(
    store: &mut Store,
    net: &Net,
    scope: crate::scheduling::SessionScope,
    provider: Provider,
    limits: crate::config::SchedulingLimits,
) -> Result<Option<crate::scheduling::ClaimedJob>> {
    store.claim_next_in_session(
        &net.a,
        &net.network,
        &crate::scheduling::SessionKey {
            conversation_id: "c".into(),
            scope,
        },
        "boot",
        &crate::scheduling::SessionConfig {
            stable_fingerprint: "stable".into(),
            runtime_fingerprint: "runtime".into(),
            provider,
        },
        &limits,
    )
}

#[test]
fn parallel_sessions_keep_native_tokens_shown_and_memory_writers_separate() -> Result<()> {
    use crate::scheduling::{SessionScope, task_trigger};
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let limits = crate::config::SchedulingLimits::default();
    let task1 = isolated_task(&mut store, &net, "parallel-1")?;
    isolated_task(&mut store, &net, "parallel-2")?;
    say(&mut store, &net, "parallel-human", true)?;
    assert!(
        isolated_claim(
            &mut store,
            &net,
            SessionScope::Task {
                work_id: task1.work_id.clone()
            },
            Provider::Codex,
            limits
        )?
        .is_none(),
        "human admission wins even over an older task"
    );
    let main = isolated_claim(
        &mut store,
        &net,
        SessionScope::Main,
        Provider::Codex,
        limits,
    )?
    .context("main")?;
    let first = isolated_claim(
        &mut store,
        &net,
        SessionScope::Task {
            work_id: task1.work_id.clone(),
        },
        Provider::Codex,
        limits,
    )?
    .context("first task")?;
    let second = isolated_claim(
        &mut store,
        &net,
        SessionScope::Task {
            work_id: "parallel-2".into(),
        },
        Provider::Codex,
        limits,
    )?
    .context("second task")?;
    assert_eq!(store.scheduler_jobs("c")?.len(), 3);
    assert!(!store.accept_work_fixture(&net.a, &net.network, task1, 5)?.1);
    assert_eq!(
        store.scheduler_jobs("c")?.len(),
        3,
        "retry keeps the same task job"
    );
    for (index, claimed) in [&main, &first, &second].into_iter().enumerate() {
        let attempt = claimed.job.state.attempt().context("attempt")?;
        let native = format!("native-{index}");
        store.mark_started("c", &claimed.job.trigger, &attempt.id)?;
        store.save_scheduler_native_id(
            "c",
            &claimed.job.trigger,
            &attempt.id,
            &native,
            &format!("token-{index}"),
            false,
        )?;
        let shown = Shown {
            bot: Some(format!("bot-{index}")),
            memory: Some([(format!("memory-{index}"), format!("digest-{index}"))].into()),
            ..Shown::default()
        };
        store.record_manager_turn(&attempt.id, "fixture", &native, Some("turn"), &shown)?;
    }
    let main_attempt = main.job.state.attempt().context("main attempt")?;
    store.stage_memory(
        &net.network,
        "c",
        &main.job.trigger,
        &main_attempt.id,
        "parallel-memory",
        ChangeRequest::Remember {
            memory_id: None,
            text: "prefers isolated tasks".into(),
        },
        6,
    )?;
    assert_eq!(store.scheduler_memory_staged_count("c")?, 1);
    let first_attempt = first.job.state.attempt().context("first attempt")?;
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &first.job.trigger,
        &first_attempt.id,
        JobOutcome::Completed,
        "task done",
        7,
    )?;
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &first.job.trigger,
        &first_attempt.id,
        JobOutcome::Completed,
        "task done",
        8,
    )?;
    assert_eq!(
        store
            .staged_memory_overlay("c", &main.job.trigger, &main_attempt.id)?
            .len(),
        1,
        "task completion leaves the main writer alone"
    );
    assert!(store.memories(&net.network, "c")?.is_empty());
    for (index, claimed) in [&main, &first, &second].into_iter().enumerate() {
        let (native, token, shown): (String, String, String) = store.connection.query_row(
            "SELECT native_id,native_token,shown FROM scheduler_sessions WHERE conversation_id='c' AND scope=?",
            [claimed.job.session_scope().storage_key()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
        assert_eq!(native, format!("native-{index}"));
        assert_eq!(token, format!("token-{index}"));
        let shown: Shown = serde_json::from_str(&shown)?;
        assert_eq!(shown.bot, Some(format!("bot-{index}")));
        assert_eq!(
            shown.memory,
            Some([(format!("memory-{index}"), format!("digest-{index}"))].into())
        );
    }
    store.request_manager_cancel(&net.a, &net.network, "c", &main.job.trigger)?;
    assert!(!store.manager_attempt_allowed(
        &net.network,
        &net.a.device_id()?,
        "c",
        &main.job.trigger,
        &main_attempt.id
    )?);
    assert!(store.manager_attempt_allowed(
        &net.network,
        &net.a.device_id()?,
        "c",
        &second.job.trigger,
        &second.job.state.attempt().context("second attempt")?.id
    )?);
    assert!(matches!(
        store
            .scheduler_job("c", &task_trigger("parallel-2"))?
            .context("second")?
            .state,
        JobState::Running { .. }
    ));
    let finished = store.context(&net.network, "c", None)?.events.into_iter().filter(|event| {
        matches!(&event.event.body, EventBody::WorkFinished { work_id, .. } if work_id == "parallel-1")
    }).count();
    assert_eq!(finished, 1);
    Ok(())
}

#[test]
fn serial_provider_gates_only_its_actual_bot_instance() -> Result<()> {
    use crate::scheduling::{QueueHoldReason, SessionConfig, SessionKey, SessionScope};
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    second_bot(&mut store, &net, "other")?;
    let limits = crate::config::SchedulingLimits::default();
    say(&mut store, &net, "serial-human", true)?;
    isolated_claim(
        &mut store,
        &net,
        SessionScope::Main,
        Provider::Cursor,
        limits,
    )?
    .context("main")?;
    isolated_task(&mut store, &net, "serial-task")?;
    let task = store
        .scheduler_job("c", "task:serial-task")?
        .context("task")?;
    assert_eq!(
        store.scheduler_job_hold(
            &net.network,
            &net.a.device_id()?,
            &task,
            Provider::Cursor,
            &limits
        )?,
        Some(QueueHoldReason::ProviderSerial)
    );
    assert!(
        isolated_claim(
            &mut store,
            &net,
            task.session_scope(),
            Provider::Cursor,
            limits
        )?
        .is_none()
    );
    super::scheduler::admit_from(
        &store.connection,
        &net.network,
        "d",
        "work:other",
        TriggerKind::WorkResult,
        "other",
    )?;
    let other = store
        .claim_next_in_session(
            &net.a,
            &net.network,
            &SessionKey {
                conversation_id: "d".into(),
                scope: SessionScope::Main,
            },
            "boot",
            &SessionConfig {
                stable_fingerprint: "stable".into(),
                runtime_fingerprint: "runtime".into(),
                provider: Provider::Cursor,
            },
            &limits,
        )?
        .context("other bot is not this Cursor instance")?;
    assert_eq!(other.job.conversation_id, "d");
    Ok(())
}

#[test]
fn held_tasks_leave_room_for_humans_and_machine_slots_are_real() -> Result<()> {
    use crate::scheduling::{QueueHoldReason, SessionScope};
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let limits = crate::config::SchedulingLimits {
        machine_slots: 2,
        task_limit: 1,
    };
    isolated_task(&mut store, &net, "limited-1")?;
    isolated_task(&mut store, &net, "limited-2")?;
    isolated_claim(
        &mut store,
        &net,
        SessionScope::Task {
            work_id: "limited-1".into(),
        },
        Provider::Codex,
        limits,
    )?
    .context("first")?;
    let task = store
        .scheduler_job("c", "task:limited-2")?
        .context("second")?;
    assert_eq!(
        store.scheduler_job_hold(
            &net.network,
            &net.a.device_id()?,
            &task,
            Provider::Codex,
            &limits
        )?,
        Some(QueueHoldReason::TaskLimit)
    );
    say(&mut store, &net, "priority-human", true)?;
    isolated_claim(
        &mut store,
        &net,
        SessionScope::Main,
        Provider::Codex,
        limits,
    )?
    .context("human bypasses held task")?;
    let more_tasks = crate::config::SchedulingLimits {
        machine_slots: 2,
        task_limit: 2,
    };
    assert_eq!(
        store.scheduler_job_hold(
            &net.network,
            &net.a.device_id()?,
            &task,
            Provider::Codex,
            &more_tasks
        )?,
        Some(QueueHoldReason::MachineSlot)
    );
    assert!(
        isolated_claim(
            &mut store,
            &net,
            task.session_scope(),
            Provider::Codex,
            more_tasks
        )?
        .is_none()
    );
    Ok(())
}

#[test]
fn restart_recovers_each_scope_and_unknown_turns_are_never_replayed() -> Result<()> {
    use crate::scheduling::{QueueHoldReason, SessionScope};
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let limits = crate::config::SchedulingLimits {
        machine_slots: 3,
        task_limit: 2,
    };
    say(&mut store, &net, "restart-human", true)?;
    isolated_task(&mut store, &net, "restart-task")?;
    let main = isolated_claim(
        &mut store,
        &net,
        SessionScope::Main,
        Provider::Codex,
        limits,
    )?
    .context("main")?;
    let task = isolated_claim(
        &mut store,
        &net,
        SessionScope::Task {
            work_id: "restart-task".into(),
        },
        Provider::Codex,
        limits,
    )?
    .context("task")?;
    let main_attempt = main.job.state.attempt().context("main attempt")?;
    store.mark_started("c", &main.job.trigger, &main_attempt.id)?;
    store.recover_scheduler_jobs(&net.a, &net.network, "new-boot")?;
    assert!(matches!(
        store
            .scheduler_job("c", &main.job.trigger)?
            .context("main")?
            .state,
        JobState::Reconciling { .. }
    ));
    assert!(matches!(
        store
            .scheduler_job("c", &task.job.trigger)?
            .context("task")?
            .state,
        JobState::Queued
    ));
    assert_eq!(store.pending_checks()?.len(), 1);
    say(&mut store, &net, "after-restart-human", true)?;
    let next = store
        .scheduler_job("c", "message:after-restart-human")?
        .context("next")?;
    assert_eq!(
        store.scheduler_job_hold(
            &net.network,
            &net.a.device_id()?,
            &next,
            Provider::Codex,
            &limits
        )?,
        Some(QueueHoldReason::PendingCheck)
    );
    let config = crate::scheduling::SessionConfig {
        stable_fingerprint: "stable".into(),
        runtime_fingerprint: "runtime".into(),
        provider: Provider::Codex,
    };
    let recovered = store
        .claim_next_in_session(
            &net.a,
            &net.network,
            &task.job.session_key(),
            "new-boot",
            &config,
            &limits,
        )?
        .context("unstarted task can run in its own scope")?;
    assert_ne!(
        recovered.job.state.attempt().context("new attempt")?.id,
        task.job.state.attempt().context("old attempt")?.id
    );
    assert_eq!(
        store.pending_checks()?.len(),
        1,
        "claiming another scope does not clear unknown evidence"
    );
    let check = store
        .pending_checks()?
        .into_iter()
        .find(|check| check.attempt_id == main_attempt.id)
        .context("pending check")?;
    store.confirm_manager_stopped(&net.a, &net.network, &check, crate::runtime::now_ms())?;
    assert_eq!(
        store.scheduler_job_hold(
            &net.network,
            &net.a.device_id()?,
            &next,
            Provider::Codex,
            &limits
        )?,
        None
    );
    assert!(matches!(
        store
            .scheduler_job("c", &main.job.trigger)?
            .context("old job")?
            .state,
        JobState::Interrupted { .. }
    ));
    Ok(())
}

#[test]
fn a_routine_is_ephemeral_and_keeps_the_main_session_untouched() -> Result<()> {
    use crate::scheduling::{SessionConfig, SessionScope};
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let (saved, _) = store.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "routine",
        save_schedule(None, "Digest", 5),
        Provider::Codex,
        3,
    )?;
    let BotChange::Schedule { schedule_id, .. } = saved.change else {
        panic!("schedule")
    };
    let due = store.admit_due_schedules(&net.a, &net.network, 1_001)?;
    assert_eq!(due.len(), 1);
    say(&mut store, &net, "routine-main", true)?;
    let limits = crate::config::SchedulingLimits::default();
    let main = isolated_claim(
        &mut store,
        &net,
        SessionScope::Main,
        Provider::Codex,
        limits,
    )?
    .context("main")?;
    let main_attempt = main.job.state.attempt().context("main attempt")?;
    store.save_scheduler_native_id(
        "c",
        &main.job.trigger,
        &main_attempt.id,
        "main-native",
        "main-token",
        true,
    )?;
    let trigger = schedule_trigger(&schedule_id, 1_000);
    let routine_key = crate::scheduling::SessionKey {
        conversation_id: "c".into(),
        scope: SessionScope::Routine {
            trigger: trigger.clone(),
        },
    };
    let config = SessionConfig {
        stable_fingerprint: "stable".into(),
        runtime_fingerprint: "runtime".into(),
        provider: Provider::Codex,
    };
    let routine = store
        .claim_next_in_session(&net.a, &net.network, &routine_key, "boot", &config, &limits)?
        .context("routine runs independently")?;
    assert!(routine.previous_native_id.is_none());
    assert!(routine.previous_token.is_none());
    assert!(routine.covers.is_none());
    assert!(routine.shown.memory.is_none());
    let attempt = routine.job.state.attempt().context("routine attempt")?;
    store.save_scheduler_native_id(
        "c",
        &trigger,
        &attempt.id,
        "routine-native",
        "routine-token",
        true,
    )?;
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &attempt.id,
        JobOutcome::Completed,
        "digest",
        2_000,
    )?;
    let routine_cache: (Option<String>, Option<String>) = store.connection.query_row("SELECT native_id,native_token FROM scheduler_sessions WHERE conversation_id='c' AND scope=?", [routine_key.scope.storage_key()], |row| Ok((row.get(0)?,row.get(1)?)))?;
    assert_eq!(routine_cache, (None, None));
    let main_cache: (String,String) = store.connection.query_row("SELECT native_id,native_token FROM scheduler_sessions WHERE conversation_id='c' AND scope='main'", [], |row| Ok((row.get(0)?,row.get(1)?)))?;
    assert_eq!(main_cache, ("main-native".into(), "main-token".into()));
    assert_eq!(
        store
            .message(&net.network, "c", &routine.job.message_id)?
            .context("ordinary routine event")?
            .text,
        "digest"
    );
    Ok(())
}

#[test]
fn stable_runtime_and_provider_changes_have_distinct_session_lineage() -> Result<()> {
    use crate::scheduling::{SessionConfig, SessionKey, SessionScope};
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let limits = crate::config::SchedulingLimits::default();
    let key = SessionKey {
        conversation_id: "c".into(),
        scope: SessionScope::Main,
    };
    let old = SessionConfig {
        stable_fingerprint: "stable-one".into(),
        runtime_fingerprint: "runtime-one".into(),
        provider: Provider::Codex,
    };
    say(&mut store, &net, "config-one", true)?;
    let first = store
        .claim_next_in_session(&net.a, &net.network, &key, "boot", &old, &limits)?
        .context("first")?;
    let attempt = first.job.state.attempt().context("attempt")?;
    store.save_scheduler_native_id(
        "c",
        &first.job.trigger,
        &attempt.id,
        "old-native",
        "old-token",
        true,
    )?;
    let new = SessionConfig {
        stable_fingerprint: "stable-two".into(),
        runtime_fingerprint: "runtime-two".into(),
        provider: Provider::Cursor,
    };
    let status = store.scheduler_session_status(&key, Some(&new))?;
    assert!(status.resumable && status.pending_generation);
    assert!(
        store.manager_attempt_allowed(
            &net.network,
            &net.a.device_id()?,
            "c",
            &first.job.trigger,
            &attempt.id
        )?,
        "configuration observation does not fence an active old turn"
    );
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &first.job.trigger,
        &attempt.id,
        JobOutcome::Completed,
        "done",
        5,
    )?;
    assert!(
        store
            .scheduler_session_status(&key, Some(&new))?
            .pending_generation
    );
    say(&mut store, &net, "config-two", true)?;
    let second = store
        .claim_next_in_session(&net.a, &net.network, &key, "boot", &new, &limits)?
        .context("second")?;
    let invalidation = second.invalidation.context("invalidation")?;
    assert_eq!(invalidation.reason, "provider_changed");
    assert_eq!(
        invalidation.old_stable_fingerprint.as_deref(),
        Some("stable-one")
    );
    assert_eq!(invalidation.new_stable_fingerprint, "stable-two");
    assert_eq!(
        invalidation.old_runtime_fingerprint.as_deref(),
        Some("runtime-one")
    );
    assert_eq!(invalidation.new_runtime_fingerprint, "runtime-two");
    assert_eq!(invalidation.source_provider, Some(Provider::Codex));
    assert_eq!(invalidation.target_provider, Provider::Cursor);
    assert_eq!(invalidation.source_basis, first.job.basis);
    assert_ne!(invalidation.source_basis, second.job.basis);
    assert_eq!(
        second.shown.session_lineage[0].source_basis,
        first.job.basis
    );
    assert_eq!(second.shown.session_generation, 2);
    assert!(second.previous_native_id.is_none() && second.previous_token.is_none());
    assert_eq!(
        second.shown.session_lineage[0].stable_fingerprint,
        "stable-one"
    );
    assert_eq!(
        second.shown.session_lineage[0].runtime_fingerprint,
        "runtime-one"
    );
    assert_eq!(second.shown.session_lineage[0].provider, Provider::Codex);
    assert!(!store.manager_attempt_allowed(
        &net.network,
        &net.a.device_id()?,
        "c",
        &first.job.trigger,
        &attempt.id
    )?);
    Ok(())
}

#[test]
fn memory_control_preserves_tombstones_receipts_and_scope_isolation() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    second_bot(&mut origin, &net, "Other")?;
    let create = signed_memory(&net, &origin, "c", "same-id", Some("first user value"))?;
    let command = memory_command(&net.network, "c", &create)?;
    let first = origin.control_memory(&net.a, &net.network, "c", command.clone(), 4)?;
    assert_eq!(first.state, MemoryControlStatus::Applied);
    let current = first.current.context("current")?;
    assert_eq!(current.text.as_deref(), Some("first user value"));
    assert_eq!(current.metadata.control_revision, 1);
    assert_eq!(current.metadata.origin, MemoryOrigin::User);
    assert!(current.metadata.user_controlled && current.metadata.essential);
    assert_eq!(
        origin
            .control_memory(&net.a, &net.network, "c", command.clone(), 5)?
            .state,
        MemoryControlStatus::AlreadyRecorded
    );
    assert_eq!(
        origin
            .memory_state(&net.network, "c", "same-id", Some(&command.operation_id))?
            .receipt,
        Some(MemoryReceipt::Recorded)
    );
    let other = signed_memory(&net, &origin, "d", "same-id", Some("independent scope"))?;
    origin.control_memory(
        &net.a,
        &net.network,
        "d",
        memory_command(&net.network, "d", &other)?,
        6,
    )?;
    assert_eq!(
        origin
            .memory_state(&net.network, "d", "same-id", Some(&command.operation_id))?
            .receipt,
        Some(MemoryReceipt::NotSeenHere)
    );
    let delete = signed_memory(&net, &origin, "c", "same-id", None)?;
    let deleted = origin.control_memory(
        &net.a,
        &net.network,
        "c",
        memory_command(&net.network, "c", &delete)?,
        7,
    )?;
    let tombstone = deleted.current.context("tombstone")?;
    assert_eq!(tombstone.text, None);
    assert_eq!(tombstone.metadata.control_revision, 2);
    assert!(tombstone.metadata.user_controlled);
    assert!(origin.memories(&net.network, "c")?.is_empty());
    assert_eq!(
        origin.memories(&net.network, "d")?[0].text,
        "independent scope"
    );
    assert_eq!(
        origin
            .control_memory(&net.a, &net.network, "c", command.clone(), 8)?
            .state,
        MemoryControlStatus::Superseded
    );
    assert_eq!(
        origin
            .memory_state(&net.network, "c", "same-id", Some(&command.operation_id))?
            .receipt,
        Some(MemoryReceipt::Superseded)
    );
    assert_eq!(
        origin
            .memory_state(&net.network, "c", "same-id", Some(&"0".repeat(64)))?
            .receipt,
        Some(MemoryReceipt::NotSeenHere)
    );
    assert_eq!(
        origin
            .memory_state(&net.network, "c", "same-id", None)?
            .receipt,
        None
    );
    assert!(
        origin
            .change_bot(
                &net.a,
                &net.network,
                "c",
                "message:m1",
                "resurrect",
                ChangeRequest::Remember {
                    memory_id: Some("same-id".into()),
                    text: "stale automatic resurrection".into(),
                },
                Provider::Codex,
                9
            )
            .unwrap_err()
            .to_string()
            .contains("user-controlled")
    );
    let restore = signed_memory(&net, &origin, "c", "same-id", Some("explicit restoration"))?;
    let restored = origin.control_memory(
        &net.a,
        &net.network,
        "c",
        memory_command(&net.network, "c", &restore)?,
        10,
    )?;
    assert_eq!(
        restored
            .current
            .context("restored")?
            .metadata
            .control_revision,
        3
    );
    let versions = origin.version_vector(&net.network)?;
    let mut forged = command;
    forged.operation_id = "0".repeat(64);
    assert!(
        origin
            .control_memory(&net.a, &net.network, "c", forged, 11)
            .is_err()
    );
    assert_eq!(origin.version_vector(&net.network)?, versions);
    assert!(
        origin
            .memory_sync_conflicts(&net.network, "c")?
            .items
            .is_empty()
    );
    Ok(())
}

#[test]
fn memory_user_operations_keep_stable_identity_across_carriers_and_rebuild() -> Result<()> {
    let net = net()?;
    let (mut a, _) = bot_with_message(&net)?;
    let mut b = store(&net)?;
    all_accepted(&mut b, &net, &a)?;
    let first_body = signed_memory(&net, &a, "c", "memory", Some("first"))?;
    let first_a = a.append_event(&net.a, &net.network, "c", first_body.clone(), vec![], 10)?;
    let first_b = b.append_event(&net.b, &net.network, "c", first_body.clone(), vec![], 20)?;
    let second_body = signed_memory(&net, &a, "c", "memory", Some("newer"))?;
    let second_a = a.append_event(&net.a, &net.network, "c", second_body.clone(), vec![], 30)?;
    b.ingest_events(
        &net.network,
        &a.events_from(&net.network, &net.a.device_id()?, 1, 500)?,
    )?;
    let second_b = b.append_event(&net.b, &net.network, "c", second_body.clone(), vec![], 40)?;
    let replay = b.append_event(&net.b, &net.network, "c", first_body.clone(), vec![], 50)?;
    let mut expected = None;
    let mut expected_conflicts = None;
    for reverse in [false, true] {
        let mut replica = store(&net)?;
        let mut events = a.events_from(&net.network, &net.a.device_id()?, 1, 500)?;
        let split = events.len();
        events.extend(b.events_from(&net.network, &net.b.device_id()?, 1, 500)?);
        if reverse {
            events.rotate_left(split);
        }
        for event in &events {
            let statuses = replica
                .ingest_events(&net.network, std::slice::from_ref(event))?
                .statuses;
            assert!(
                statuses
                    .iter()
                    .all(|status| matches!(status, IngestStatus::Accepted | IngestStatus::Pending)),
                "{statuses:?}"
            );
        }
        let state = replica
            .memory_state(&net.network, "c", "memory", None)?
            .current
            .context("current")?;
        assert_eq!(state.text.as_deref(), Some("newer"));
        assert_eq!(state.metadata.control_revision, 2);
        assert_eq!(
            state.metadata.source_event_hash,
            second_a.hash.clone().min(second_b.hash.clone())
        );
        assert_eq!(
            state.metadata.revision,
            crate::crypto::request_key(&net.network, "c", &second_body, "")?
        );
        let changed_at: i64 = replica.connection.query_row("SELECT changed_at_ms FROM memories WHERE network_id=? AND conversation_id='c' AND memory_id='memory'", [&net.network], |row| row.get(0))?;
        assert_eq!(
            changed_at,
            if second_a.hash < second_b.hash {
                30
            } else {
                40
            }
        );
        let conflicts = replica.memory_sync_conflicts(&net.network, "c")?;
        assert!(
            conflicts.items.is_empty(),
            "duplicate old carriers are one logical ancestor"
        );
        if let Some(expected) = &expected {
            assert_eq!(&state, expected);
        }
        if let Some(expected) = &expected_conflicts {
            assert_eq!(&conflicts, expected);
        }
        expected = Some(state.clone());
        expected_conflicts = Some(conflicts.clone());
        replica.connection.execute(
            "UPDATE events SET effective=0 WHERE network_id=?",
            [&net.network],
        )?;
        assert!(log::recompute(
            &replica.connection,
            &membership_from(&replica.connection, &net.network)?
        )?);
        assert_eq!(
            replica
                .memory_state(&net.network, "c", "memory", None)?
                .current,
            Some(state)
        );
        assert_eq!(replica.memory_sync_conflicts(&net.network, "c")?, conflicts);
    }
    let context_entries = |store: &Store| -> Result<Vec<crate::agent_context::MemoryContextEntry>> {
        Ok(store
            .memory_records(&net.network, "c")?
            .into_iter()
            .map(|record| crate::agent_context::MemoryContextEntry {
                memory_id: record.entry.memory_id,
                text: record.entry.text,
                epoch: record.epoch,
                revision: record.revision,
            })
            .collect())
    };
    let delivered = crate::agent_context::compile_memory(&context_entries(&a)?, None)?;
    a.ingest_events(
        &net.network,
        &b.events_from(&net.network, &net.b.device_id()?, 1, 500)?,
    )?;
    let duplicate_delivery =
        crate::agent_context::compile_memory(&context_entries(&a)?, Some(&delivered.shown))?;
    assert!(
        matches!(duplicate_delivery.context, crate::agent_context::MemoryContext::Delta { upserts, deleted, omitted: 0, .. }
        if upserts.is_empty() && deleted.is_empty())
    );
    assert_eq!(duplicate_delivery.shown, delivered.shown);
    let mut old_source_base = a
        .memory_state(&net.network, "c", "memory", None)?
        .current
        .context("base")?
        .metadata
        .base();
    old_source_base
        .version
        .as_mut()
        .context("version")?
        .source_event_hash = second_a.hash.max(second_b.hash);
    let delete = net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::MemoryControlled {
            memory_id: "memory".into(),
            base: old_source_base,
            after: None,
            attestation: None,
        },
    )?;
    assert_eq!(
        a.control_memory(
            &net.a,
            &net.network,
            "c",
            memory_command(&net.network, "c", &delete)?,
            60
        )?
        .state,
        MemoryControlStatus::Applied
    );
    assert!(
        a.memory_state(&net.network, "c", "memory", None)?
            .current
            .context("deleted")?
            .text
            .is_none()
    );
    assert_eq!(
        a.control_memory(
            &net.a,
            &net.network,
            "c",
            memory_command(&net.network, "c", &first_body)?,
            70
        )?
        .state,
        MemoryControlStatus::Superseded
    );
    assert_ne!(first_a.hash, first_b.hash);
    assert_ne!(replay.hash, first_a.hash);
    Ok(())
}

#[test]
fn memory_user_tombstones_survive_stale_automatic_branches_in_every_order() -> Result<()> {
    let net = net()?;
    let (mut a, _) = bot_with_message(&net)?;
    a.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "seed",
        ChangeRequest::Remember {
            memory_id: None,
            text: "automatic base".into(),
        },
        Provider::Codex,
        4,
    )?;
    let seed = a.memory_records(&net.network, "c")?.remove(0);
    let mut b = store(&net)?;
    all_accepted(&mut b, &net, &a)?;
    b.change_bot(
        &net.b,
        &net.network,
        "c",
        "message:m1",
        "stale",
        ChangeRequest::Remember {
            memory_id: Some(seed.entry.memory_id.clone()),
            text: "stale automatic branch".into(),
        },
        Provider::Codex,
        50,
    )?;
    let user_body = signed_memory(&net, &a, "c", &seed.entry.memory_id, Some("user value"))?;
    a.append_event(&net.a, &net.network, "c", user_body, vec![], 6)?;
    let delete_body = signed_memory(&net, &a, "c", &seed.entry.memory_id, None)?;
    a.append_event(&net.a, &net.network, "c", delete_body, vec![], 7)?;
    let mut expected = None;
    for reverse in [false, true] {
        let mut replica = store(&net)?;
        let mut events = a.events_from(&net.network, &net.a.device_id()?, 1, 500)?;
        let split = events.len();
        events.extend(b.events_from(&net.network, &net.b.device_id()?, 1, 500)?);
        if reverse {
            events.rotate_left(split);
        }
        replica.ingest_events(&net.network, &events)?;
        let state = replica
            .memory_state(&net.network, "c", &seed.entry.memory_id, None)?
            .current
            .context("tombstone")?;
        assert_eq!(state.text, None);
        assert_eq!(state.metadata.control_revision, 2);
        assert!(state.metadata.user_controlled);
        let conflicts = replica.memory_sync_conflicts(&net.network, "c")?;
        assert_eq!(conflicts.items.len(), 1);
        assert_eq!(
            conflicts.items[0].reason,
            MemorySyncConflictReason::UserProtected
        );
        assert_eq!(conflicts.items[0].winning_revision, state.metadata.revision);
        if let Some(expected) = &expected {
            assert_eq!(&(state.clone(), conflicts.clone()), expected);
        }
        expected = Some((state, conflicts));
    }
    Ok(())
}

#[test]
fn memory_base_authentication_rejects_forged_versions_and_waits_for_real_parents() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    origin.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "seed",
        ChangeRequest::Remember {
            memory_id: None,
            text: "base".into(),
        },
        Provider::Codex,
        4,
    )?;
    let record = origin.memory_records(&net.network, "c")?.remove(0);
    let base = record.metadata().base();
    for (scope, memory_id, mut claimed, direct) in [
        ("c", record.entry.memory_id.as_str(), base.clone(), false),
        ("c", "another-memory", base.clone(), true),
        ("d", record.entry.memory_id.as_str(), base.clone(), true),
        ("c", record.entry.memory_id.as_str(), base.clone(), true),
    ] {
        if scope == "c" && memory_id == record.entry.memory_id && direct {
            claimed
                .version
                .as_mut()
                .context("version")?
                .control_revision = 100;
        }
        let body = net.a.fixture_admin().attest(
            &net.network,
            scope,
            EventBody::MemoryControlled {
                memory_id: memory_id.into(),
                base: claimed,
                after: Some("forged".into()),
                attestation: None,
            },
        )?;
        let envelope = net.b.sign_event(Event {
            version: WIRE_VERSION,
            network_id: net.network.clone(),
            author: net.b.device_id()?,
            seq: 1,
            prev: None,
            deps: if direct {
                vec![record.source_event_hash.clone()]
            } else {
                vec![]
            },
            scope_id: scope.into(),
            created_at_ms: 5,
            body,
            blobs: vec![],
        })?;
        assert!(matches!(
            origin
                .ingest_events(&net.network, &[envelope])?
                .statuses
                .as_slice(),
            [IngestStatus::Rejected(_)]
        ));
    }
    let body = signed_memory(
        &net,
        &origin,
        "c",
        &record.entry.memory_id,
        Some("real dependent user value"),
    )?;
    let dependent = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![record.source_event_hash.clone()],
        scope_id: "c".into(),
        created_at_ms: 6,
        body: body.clone(),
        blobs: vec![],
    })?;
    let mut replica = store(&net)?;
    assert_eq!(
        replica.ingest_events(&net.network, &[dependent])?.statuses,
        vec![IngestStatus::Pending]
    );
    all_accepted(&mut replica, &net, &origin)?;
    let result = replica.memory_state(
        &net.network,
        "c",
        &record.entry.memory_id,
        Some(&crate::crypto::request_key(&net.network, "c", &body, "")?),
    )?;
    assert_eq!(result.receipt, Some(MemoryReceipt::Recorded));
    assert_eq!(
        result.current.context("dependent")?.text.as_deref(),
        Some("real dependent user value")
    );
    let body = net.a.attest(
        &net.network,
        "c",
        EventBody::MemoryControlled {
            memory_id: "node-forgery".into(),
            base: MemoryBase::absent(),
            after: Some("node cannot impersonate user".into()),
            attestation: None,
        },
    )?;
    assert!(
        origin
            .append_event(&net.a, &net.network, "c", body, vec![], 7)
            .is_err()
    );
    assert!(
        origin
            .append_event(
                &net.a,
                &net.network,
                "c",
                EventBody::MemoryControlled {
                    memory_id: "unsigned".into(),
                    base: MemoryBase::absent(),
                    after: Some("unsigned value".into()),
                    attestation: None,
                },
                vec![],
                8
            )
            .is_err()
    );
    assert!(
        serde_json::from_value::<MemoryBase>(json!({"epoch":crate::config::DATA_EPOCH})).is_err()
    );
    let mut overflowing = base;
    overflowing
        .version
        .as_mut()
        .context("version")?
        .control_revision = u32::MAX;
    assert!(overflowing.next_control_revision().is_err());
    for value in [
        json!({"kind":"memory","memory_id":"m","before":null,"after":"value","base":{"epoch":crate::config::DATA_EPOCH,"version":null},"origin":"user","essential":true}),
        json!({"kind":"memory","memory_id":"m","before":null,"after":"value","base":{"epoch":crate::config::DATA_EPOCH,"version":null},"origin":"agent","user_controlled":true,"essential":true}),
    ] {
        assert!(serde_json::from_value::<BotChange>(value).is_err());
    }
    Ok(())
}

#[test]
fn memory_sync_conflicts_follow_logical_bases_for_automatic_and_user_branches() -> Result<()> {
    let net = net()?;
    let (mut a, _) = bot_with_message(&net)?;
    a.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "seed",
        ChangeRequest::Remember {
            memory_id: None,
            text: "seed".into(),
        },
        Provider::Codex,
        4,
    )?;
    let seed = a.memory_records(&net.network, "c")?.remove(0);
    a.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "linear",
        ChangeRequest::Remember {
            memory_id: Some(seed.entry.memory_id.clone()),
            text: "linear update".into(),
        },
        Provider::Codex,
        5,
    )?;
    assert!(a.memory_sync_conflicts(&net.network, "c")?.items.is_empty());
    let mut b = store(&net)?;
    all_accepted(&mut b, &net, &a)?;
    for (store, author, call, text) in [
        (&mut a, &net.a, "left", "left branch"),
        (&mut b, &net.b, "right", "right branch"),
    ] {
        store.change_bot(
            author,
            &net.network,
            "c",
            "message:m1",
            call,
            ChangeRequest::Remember {
                memory_id: Some(seed.entry.memory_id.clone()),
                text: text.into(),
            },
            Provider::Codex,
            6,
        )?;
    }
    a.ingest_events(
        &net.network,
        &b.events_from(&net.network, &net.b.device_id()?, 1, 500)?,
    )?;
    let automatic = a.memory_sync_conflicts(&net.network, "c")?;
    assert_eq!(automatic.items.len(), 1);
    assert_eq!(
        automatic.items[0].reason,
        MemorySyncConflictReason::ConcurrentAutomatic
    );
    let takeover = signed_memory(
        &net,
        &a,
        "c",
        &seed.entry.memory_id,
        a.memory_state(&net.network, "c", &seed.entry.memory_id, None)?
            .current
            .context("current")?
            .text
            .as_deref(),
    )?;
    a.append_event(&net.a, &net.network, "c", takeover, vec![], 7)?;
    let protected = a.memory_sync_conflicts(&net.network, "c")?;
    assert_eq!(protected.items.len(), 1);
    assert_eq!(
        protected.items[0].losing_revision,
        automatic.items[0].losing_revision
    );
    assert_eq!(
        protected.items[0].reason,
        MemorySyncConflictReason::UserProtected
    );
    let versions = a.version_vector(&net.network)?;
    let stale = signed_memory(
        &net,
        &b,
        "c",
        &seed.entry.memory_id,
        Some("stale local edit"),
    )?;
    assert_eq!(
        a.control_memory(
            &net.a,
            &net.network,
            "c",
            memory_command(&net.network, "c", &stale)?,
            8
        )?
        .state,
        MemoryControlStatus::Conflict
    );
    assert_eq!(a.version_vector(&net.network)?, versions);
    Ok(())
}

#[test]
fn memory_sync_conflicts_scan_all_history_before_bounding_private_metadata() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    for index in 0..55 {
        let body = net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::MemoryControlled {
                memory_id: "memory".into(),
                base: MemoryBase::absent(),
                after: Some(format!("private candidate {index}")),
                attestation: None,
            },
        )?;
        origin.append_event(&net.a, &net.network, "c", body, vec![], 10 + index)?;
    }
    let page = origin.memory_sync_conflicts(&net.network, "c")?;
    assert_eq!(page.items.len(), 50);
    assert!(page.has_more);
    assert!(
        page.items
            .windows(2)
            .all(|pair| pair[0].losing_revision < pair[1].losing_revision)
    );
    assert!(
        page.items
            .iter()
            .all(|item| item.reason == MemorySyncConflictReason::ConcurrentUser)
    );
    assert!(!serde_json::to_string(&page)?.contains("private candidate"));
    let mut replica = store(&net)?;
    let events = origin.events_from(&net.network, &net.a.device_id()?, 1, 500)?;
    for chunk in events.chunks(3) {
        replica.ingest_events(&net.network, chunk)?;
    }
    assert_eq!(replica.memory_sync_conflicts(&net.network, "c")?, page);
    assert_eq!(
        replica.memory_state(&net.network, "c", "memory", None)?,
        origin.memory_state(&net.network, "c", "memory", None)?
    );
    Ok(())
}

#[test]
fn memory_control_applies_live_limits_and_rejects_credentials_before_writing() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    for index in 0..MAX_MEMORIES {
        let id = format!("memory-{index}");
        let body = signed_memory(&net, &origin, "c", &id, Some("user note"))?;
        origin.control_memory(
            &net.a,
            &net.network,
            "c",
            memory_command(&net.network, "c", &body)?,
            10 + index as i64,
        )?;
    }
    let extra = signed_memory(&net, &origin, "c", "extra", Some("another user note"))?;
    let version = origin.version_vector(&net.network)?;
    assert!(
        origin
            .control_memory(
                &net.a,
                &net.network,
                "c",
                memory_command(&net.network, "c", &extra)?,
                70
            )
            .is_err()
    );
    assert_eq!(origin.version_vector(&net.network)?, version);
    let delete = signed_memory(&net, &origin, "c", "memory-0", None)?;
    origin.control_memory(
        &net.a,
        &net.network,
        "c",
        memory_command(&net.network, "c", &delete)?,
        71,
    )?;
    assert_eq!(
        origin
            .control_memory(
                &net.a,
                &net.network,
                "c",
                memory_command(&net.network, "c", &extra)?,
                72
            )?
            .state,
        MemoryControlStatus::Applied
    );
    let restore = signed_memory(&net, &origin, "c", "memory-0", Some("restore"))?;
    assert!(
        origin
            .control_memory(
                &net.a,
                &net.network,
                "c",
                memory_command(&net.network, "c", &restore)?,
                73
            )
            .is_err()
    );
    assert_eq!(origin.memories(&net.network, "c")?.len(), MAX_MEMORIES);
    assert_eq!(
        origin
            .memory_state(&net.network, "c", "memory-0", None)?
            .current
            .context("tombstone")?
            .text,
        None
    );
    for text in [
        "x".repeat(MAX_MEMORY_CHARS + 1),
        "password=fixture-value".into(),
    ] {
        let invalid = signed_memory(&net, &origin, "c", "memory-1", Some(&text))?;
        let version = origin.version_vector(&net.network)?;
        assert!(
            origin
                .control_memory(
                    &net.a,
                    &net.network,
                    "c",
                    memory_command(&net.network, "c", &invalid)?,
                    74
                )
                .is_err()
        );
        assert_eq!(origin.version_vector(&net.network)?, version);
    }
    Ok(())
}

#[test]
fn staged_memory_capacity_is_rechecked_at_completion() -> Result<()> {
    for (live_count, staged_count, restore) in [(49, 1, false), (48, 2, false), (49, 1, true)] {
        let net = net()?;
        let (mut origin, _) = bot_with_message(&net)?;
        let tombstone = if restore {
            origin.change_bot(
                &net.a,
                &net.network,
                "c",
                "message:m1",
                "deleted-seed",
                ChangeRequest::Remember {
                    memory_id: None,
                    text: "old automatic note".into(),
                },
                Provider::Codex,
                4,
            )?;
            let memory_id = origin.memories(&net.network, "c")?.remove(0).memory_id;
            origin.change_bot(
                &net.a,
                &net.network,
                "c",
                "message:m1",
                "delete-seed",
                ChangeRequest::Forget {
                    memory_id: memory_id.clone(),
                },
                Provider::Codex,
                5,
            )?;
            origin
                .memory_state(&net.network, "c", &memory_id, None)?
                .current
        } else {
            None
        };
        for index in 0..live_count {
            let body = signed_memory(
                &net,
                &origin,
                "c",
                &format!("user-{index}"),
                Some("saved user note"),
            )?;
            origin.control_memory(
                &net.a,
                &net.network,
                "c",
                memory_command(&net.network, "c", &body)?,
                10 + index,
            )?;
        }
        say(&mut origin, &net, "m2", true)?;
        let trigger = message_trigger("m2");
        let claimed = origin
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .context("job")?;
        let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
        origin.mark_started("c", &trigger, &attempt)?;
        let mut staged_ids = Vec::new();
        for index in 0..staged_count {
            let staged = origin.stage_memory(
                &net.network,
                "c",
                &trigger,
                &attempt,
                &format!("stage-{index}"),
                ChangeRequest::Remember {
                    memory_id: tombstone.as_ref().map(|state| state.memory_id.clone()),
                    text: format!("staged candidate {index}"),
                },
                70,
            )?;
            staged_ids.push(
                staged["memory_id"]
                    .as_str()
                    .context("staged memory ID")?
                    .to_owned(),
            );
        }
        let incoming = signed_memory(
            &net,
            &origin,
            "c",
            "incoming-user",
            Some("new signed user memory"),
        )?;
        origin.control_memory(
            &net.a,
            &net.network,
            "c",
            memory_command(&net.network, "c", &incoming)?,
            71,
        )?;
        origin.finish_job(
            &net.a,
            &net.network,
            "c",
            &trigger,
            &attempt,
            JobOutcome::Completed,
            "completed reply",
            72,
        )?;
        assert_eq!(origin.memories(&net.network, "c")?.len(), MAX_MEMORIES);
        assert!(matches!(
            origin
                .scheduler_job("c", &trigger)?
                .context("completed job")?
                .state,
            JobState::Completed { .. }
        ));
        let reply = origin
            .messages(&net.network, "c")?
            .into_iter()
            .find(|message| message.in_reply_to.as_deref() == Some("m2"))
            .context("reply")?;
        assert_eq!(reply.text, "completed reply");
        assert_eq!(reply.status, Some(ReplyStatus::Completed));
        let conflicts = memory_test_shown(&origin)?.memory_conflicts;
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].reason, "capacity_exceeded");
        assert_eq!(
            conflicts[0].memory_id,
            *staged_ids.last().context("rejected memory")?
        );
        for (index, memory_id) in staged_ids.iter().enumerate() {
            let committed = index < (MAX_MEMORIES - live_count as usize - 1);
            assert_eq!(
                origin
                    .bot_change(
                        &net.network,
                        &bot_settings::change_id("c", &trigger, &format!("stage-{index}"))
                    )?
                    .is_some(),
                committed
            );
            if committed {
                assert_eq!(
                    origin
                        .memory_state(&net.network, "c", memory_id, None)?
                        .current
                        .context("committed memory")?
                        .text,
                    Some(format!("staged candidate {index}"))
                );
            }
        }
        if let Some(tombstone) = tombstone {
            assert_eq!(
                origin
                    .memory_state(&net.network, "c", &tombstone.memory_id, None)?
                    .current
                    .as_ref(),
                Some(&tombstone)
            );
        }
    }
    Ok(())
}

fn switchable_scheduler_session(
    net: &Net,
) -> Result<(
    Store,
    crate::scheduling::ClaimedJob,
    crate::scheduling::SessionConfig,
)> {
    use crate::scheduling::{SessionConfig, SessionKey, SessionScope, TurnExecution, TurnRuntime};
    use vibeface_bridge::failure::ProviderStage;
    let mut store = store(net)?;
    conversation(&mut store, net)?;
    store.append_event(&net.a, &net.network, "c", bot(net, "Helper")?, vec![], 2)?;
    say(&mut store, net, "m1", true)?;
    let config = SessionConfig {
        stable_fingerprint: "stable-before-backup".into(),
        runtime_fingerprint: "runtime-before-backup".into(),
        provider: Provider::Codex,
    };
    let key = SessionKey {
        conversation_id: "c".into(),
        scope: SessionScope::Main,
    };
    let limits = crate::config::SchedulingLimits::default();
    let first = store
        .claim_next_in_session(&net.a, &net.network, &key, "boot", &config, &limits)?
        .context("first turn")?;
    let attempt = first
        .job
        .state
        .attempt()
        .context("first attempt")?
        .id
        .clone();
    store.mark_started("c", &first.job.trigger, &attempt)?;
    store.save_scheduler_native_id(
        "c",
        &first.job.trigger,
        &attempt,
        "cached-thread",
        "cached-token",
        true,
    )?;
    let shown = Shown {
        bot: Some("cached-bot".into()),
        works: [("cached-work".into(), "work-revision".into())].into(),
        memory: Some([("cached-memory".into(), "memory-revision".into())].into()),
        dynamic: Some("cached-context".into()),
        session_generation: first.session_generation,
        ..Shown::default()
    };
    store.record_manager_turn(
        &attempt,
        "fixture",
        "cached-thread",
        Some("old-turn"),
        &shown,
    )?;
    store.finish_job(
        &net.a,
        &net.network,
        "c",
        &first.job.trigger,
        &attempt,
        JobOutcome::Completed,
        "done",
        4,
    )?;
    say(&mut store, net, "switch-next", true)?;
    let mut claimed = store
        .claim_next_in_session(&net.a, &net.network, &key, "boot", &config, &limits)?
        .context("resumed turn")?;
    assert_eq!(claimed.previous_native_id.as_deref(), Some("cached-thread"));
    assert_eq!(claimed.shown.bot.as_deref(), Some("cached-bot"));
    assert!(!claimed.covers.as_ref().context("cached covers")?.is_empty());
    let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
    let runtime = TurnRuntime {
        provider: Provider::Codex,
        model: Some("selected-before-backup".into()),
        reasoning_effort: Some("medium".into()),
        generation: "before-backup".into(),
        session_generation: claimed.session_generation,
        delivery_id: attempt.clone(),
        stage: ProviderStage::Authentication,
        execution: TurnExecution::NotStarted,
        attempted_at_ms: 5,
        switched_from: None,
        switch_reason: None,
    };
    claimed.job = store.record_turn_runtime("c", &claimed.job.trigger, &attempt, &runtime)?;
    Ok((store, claimed, config))
}

fn scheduler_switch_rows(store: &Store) -> Result<Vec<String>> {
    let mut rows = Vec::new();
    for table in ["scheduler_jobs", "scheduler_sessions", "pending_checks"] {
        let mut statement = store
            .connection
            .prepare(&format!("SELECT * FROM {table}"))?;
        let count = statement.column_count();
        for row in statement.query_map([], |row| {
            (0..count)
                .map(|index| row.get::<_, rusqlite::types::Value>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })? {
            rows.push(format!("{table}: {:?}", row?));
        }
    }
    rows.sort();
    Ok(rows)
}

#[test]
fn backup_switch_fences_the_cached_session_without_readmitting_the_job() -> Result<()> {
    use crate::scheduling::{RuntimeSelection, SessionConfig};
    let net = net()?;
    let (mut store, before, old) = switchable_scheduler_session(&net)?;
    let attempt = before.job.state.attempt().context("attempt")?;
    let jobs = store.scheduler_jobs("c")?.len();
    let basis = store.version_vector(&net.network)?;
    let config = SessionConfig {
        stable_fingerprint: "stable-after-backup".into(),
        runtime_fingerprint: "runtime-after-backup".into(),
        provider: Provider::Cursor,
    };
    let switched =
        store.switch_scheduler_session("c", &before.job.trigger, &attempt.id, &config)?;
    assert_eq!(switched.job.trigger, before.job.trigger);
    assert_eq!(switched.job.basis, before.job.basis);
    assert_eq!(switched.job.message_id, before.job.message_id);
    assert_eq!(
        switched.job.state.attempt().context("switched attempt")?.id,
        attempt.id
    );
    assert_eq!(
        switched
            .job
            .state
            .attempt()
            .context("switched attempt")?
            .permission_version,
        attempt.permission_version + 1
    );
    assert_eq!(switched.session_generation, before.session_generation + 1);
    assert_eq!(switched.handoff_revision, before.handoff_revision + 1);
    assert_eq!(switched.handoff_source, before.job.basis);
    assert!(
        switched.previous_native_id.is_none()
            && switched.previous_token.is_none()
            && switched.covers.is_none()
    );
    assert_eq!(
        switched.previous_runtime,
        Some(RuntimeSelection {
            provider: Provider::Codex,
            model: Some("selected-before-backup".into()),
            reasoning_effort: Some("medium".into()),
        })
    );
    let invalidation = switched.invalidation.as_ref().context("invalidation")?;
    assert_eq!(invalidation.reason, "provider_changed");
    assert_eq!(invalidation.source_provider, Some(old.provider));
    assert_eq!(invalidation.target_provider, config.provider);
    assert_eq!(
        invalidation.old_stable_fingerprint.as_deref(),
        Some(old.stable_fingerprint.as_str())
    );
    assert_eq!(
        invalidation.new_stable_fingerprint,
        config.stable_fingerprint
    );
    assert_eq!(
        invalidation.old_runtime_fingerprint.as_deref(),
        Some(old.runtime_fingerprint.as_str())
    );
    assert_eq!(
        invalidation.new_runtime_fingerprint,
        config.runtime_fingerprint
    );
    assert_eq!(
        invalidation.source_basis,
        before.covers.clone().context("old covers")?
    );
    assert_eq!(invalidation.generation, switched.session_generation);
    assert_eq!(
        switched.shown.session_lineage.len(),
        before.shown.session_lineage.len() + 1
    );
    let lineage = switched.shown.session_lineage.last().context("lineage")?;
    assert_eq!(lineage.source_basis, invalidation.source_basis);
    assert_eq!(lineage.generation, before.session_generation);
    assert_eq!(lineage.stable_fingerprint, old.stable_fingerprint);
    assert_eq!(lineage.runtime_fingerprint, old.runtime_fingerprint);
    assert_eq!(lineage.provider, old.provider);
    assert_eq!(lineage.handoff_revision, invalidation.handoff_revision);
    assert_eq!(
        switched.shown,
        Shown {
            session_generation: switched.session_generation,
            session_lineage: switched.shown.session_lineage.clone(),
            ..Shown::default()
        }
    );
    assert_eq!(memory_test_shown(&store)?, switched.shown);
    let persisted: (Option<String>, Option<String>, String, String, String, String, u64, u64) = store.connection.query_row(
        "SELECT native_id,native_token,covers,stable_fingerprint,runtime_fingerprint,provider,permission_version,session_generation FROM scheduler_sessions WHERE conversation_id='c' AND scope='main'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?)),
    )?;
    assert_eq!(
        (persisted.0, persisted.1, persisted.2.as_str()),
        (None, None, "{}")
    );
    assert_eq!(
        (persisted.3, persisted.4, persisted.5),
        (
            config.stable_fingerprint,
            config.runtime_fingerprint,
            enum_text(&config.provider)
        )
    );
    assert_eq!(
        (persisted.6, persisted.7),
        (attempt.permission_version + 1, switched.session_generation)
    );
    assert_eq!(
        store
            .scheduler_job("c", &before.job.trigger)?
            .context("saved job")?,
        switched.job
    );
    assert_eq!(store.scheduler_jobs("c")?.len(), jobs);
    assert_eq!(store.version_vector(&net.network)?, basis);
    Ok(())
}

#[test]
fn rejected_backup_switches_leave_jobs_sessions_and_checks_unchanged() -> Result<()> {
    use crate::scheduling::{SessionConfig, SessionScope, TurnExecution};
    use vibeface_bridge::failure::ProviderStage;
    for (case, expected) in [
        ("task", "only allowed for human turns"),
        ("started", "started execution"),
        ("unknown", "started execution"),
        ("pending", "side effects"),
        ("tools", "side effects"),
        ("partial", "emitted reply text"),
        ("attempt", "attempt does not match"),
        ("inactive", "no longer active"),
        ("missing-runtime", "turn runtime missing"),
        ("unchanged", "configuration did not change"),
    ] {
        let net = net()?;
        let (mut store, mut claimed, old) = switchable_scheduler_session(&net)?;
        let mut config = SessionConfig {
            stable_fingerprint: "stable-backup".into(),
            runtime_fingerprint: "runtime-backup".into(),
            provider: Provider::Cursor,
        };
        if case == "task" {
            let task = isolated_task(&mut store, &net, "backup-task")?;
            let mut runtime = claimed.job.runtime.clone().context("runtime")?;
            claimed = isolated_claim(
                &mut store,
                &net,
                SessionScope::Task {
                    work_id: task.work_id,
                },
                task.resolved.provider,
                crate::config::SchedulingLimits::default(),
            )?
            .context("task claim")?;
            runtime.provider = task.resolved.provider;
            runtime.model = task.resolved.model;
            runtime.reasoning_effort = None;
            runtime.session_generation = claimed.session_generation;
            runtime.delivery_id = claimed
                .job
                .state
                .attempt()
                .context("task attempt")?
                .id
                .clone();
            store.record_turn_runtime("c", &claimed.job.trigger, &runtime.delivery_id, &runtime)?;
        }
        let trigger = claimed.job.trigger.clone();
        let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
        match case {
            "started" | "unknown" => {
                store.update_turn_runtime(
                    "c",
                    &trigger,
                    &attempt,
                    ProviderStage::Run,
                    if case == "started" {
                        TurnExecution::Started
                    } else {
                        TurnExecution::Unknown
                    },
                )?;
            }
            "pending" => {
                assert_eq!(store.connection.execute("INSERT INTO pending_checks(attempt_id,conversation_id,trigger,detail,created_at_ms) VALUES(?,'c',?,'unknown provider result',6)", params![attempt, trigger])?, 1);
            }
            "tools" => {
                store.record_manager_tool_call("c", &trigger, &attempt)?;
            }
            "partial" => {
                assert_eq!(store.connection.execute("UPDATE scheduler_jobs SET partial_text='visible partial' WHERE conversation_id='c' AND trigger=?", [&trigger])?, 1);
            }
            "inactive" => {
                assert_eq!(store.connection.execute("UPDATE scheduler_sessions SET active=NULL WHERE conversation_id='c' AND scope='main'", [])?, 1);
            }
            "missing-runtime" => {
                assert_eq!(store.connection.execute("UPDATE scheduler_jobs SET runtime=NULL WHERE conversation_id='c' AND trigger=?", [&trigger])?, 1);
            }
            "unchanged" => {
                config = old;
            }
            _ => {}
        }
        let before = scheduler_switch_rows(&store)?;
        let job = store.scheduler_job("c", &trigger)?.context("saved job")?;
        let basis = store.version_vector(&net.network)?;
        let result = store.switch_scheduler_session(
            "c",
            &trigger,
            if case == "attempt" {
                "wrong-attempt"
            } else {
                &attempt
            },
            &config,
        );
        let error = result.expect_err(case);
        assert!(error.to_string().contains(expected), "{case}: {error}");
        assert_eq!(scheduler_switch_rows(&store)?, before, "{case}");
        assert_eq!(
            store.scheduler_job("c", &trigger)?.context("saved job")?,
            job,
            "{case}"
        );
        assert_eq!(store.version_vector(&net.network)?, basis, "{case}");
    }
    Ok(())
}

#[test]
fn bot_names_stay_unique_and_the_primary_has_a_named_successor() -> Result<()> {
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    second_bot(&mut origin, &net, "Ops")?;
    assert_eq!(
        origin.primary_successor(&net.network)?.as_deref(),
        Some("d")
    );
    let taken = origin
        .create_bot_in_turn(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            "n1",
            (" ops ", "", None),
            5,
        )
        .unwrap_err();
    assert!(
        taken.to_string().contains("已经有叫「ops」的 Bot"),
        "{taken}"
    );
    let renamed = origin
        .change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            "r1",
            rename("OPS"),
            Provider::Codex,
            6,
        )
        .unwrap_err();
    assert!(renamed.to_string().contains("换个名字"), "{renamed}");
    Ok(())
}
