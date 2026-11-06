use super::{bot_settings::ChangeRequest, *};
use crate::{
    crypto::LocalIdentity,
    providers::Provider,
    scheduling::{
        JobOutcome, JobState, SchedulerJob, SessionScope, Shown, SteerOutcome, TriggerKind,
        bot_message_trigger, message_trigger, schedule_trigger, work_trigger,
    },
    work::{RequestedWorkKind, Signal, WorkRequest, WorkSpec},
};
use serde_json::json;
use vibeface_bridge::failure::{ExecFailure, FailureKind};

#[test]
fn first_release_schema_reopens_and_rejects_old_databases_without_modifying_them() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("vibeface-schema-{}", uuid::Uuid::new_v4()));
    crate::config::private_dir(&dir)?;
    let path = dir.join("node.sqlite3");
    let store = Store::open(&path)?;
    let version: u32 = store
        .connection
        .pragma_query_value(None, "user_version", |row| row.get(0))?;
    assert_eq!(version, SCHEMA_VERSION);
    store.connection.execute(
        "INSERT INTO identity_meta(key,value) VALUES('retained','value')",
        [],
    )?;
    drop(store);
    let store = Store::open(&path)?;
    let value: String = store.connection.query_row(
        "SELECT value FROM identity_meta WHERE key='retained'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(value, "value");
    drop(store);
    for version in [0, 2, 12] {
        let db = Connection::open(&path)?;
        db.pragma_update(None, "user_version", version)?;
        drop(db);
        let before = std::fs::read(&path)?;
        assert!(Store::open(&path).is_err());
        assert!(Store::open_readonly(&path).is_err());
        assert_eq!(std::fs::read(&path)?, before);
    }
    let view_path = dir.join("view.sqlite3");
    let db = Connection::open(&view_path)?;
    db.execute_batch("CREATE VIEW old_view AS SELECT 1;")?;
    drop(db);
    let before = std::fs::read(&view_path)?;
    assert!(Store::open(&view_path).is_err());
    assert_eq!(std::fs::read(&view_path)?, before);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

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
fn native_start_reservations_cover_initial_resume_and_reject_cancelled_work() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
    say(&mut store, &net, "source", true)?;
    let local = net.a.device_id()?;
    for (reference, thread, stopped) in [
        ("fresh", None, false),
        ("resume", Some("known-thread"), false),
        ("stopped", None, true),
    ] {
        let spec = WorkSpec {
            work_id: WorkSpec::id("c", "message:source", reference),
            conversation: "c".into(),
            trigger: "message:source".into(),
            target: local.clone(),
            dispatcher: local.clone(),
            call_ref: reference.into(),
            kind: WorkKind::Agent(Provider::Codex),
            model: None,
            resolved: ResolvedModel {
                provider: Provider::Codex,
                model: None,
            },
            cwd: "/work".into(),
            title: "work".into(),
            prompt: "read only".into(),
            continues: None,
            parent: None,
            thread: thread.map(str::to_owned),
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
fn three_inputs_keep_the_active_attempt_and_preserve_the_attachment_in_order() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    conversation(&mut store, &net)?;
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
        EventBody::MessagePosted {
            message_id: "m3".into(),
            role: Role::User,
            text: "read the attachment".into(),
            attachments: vec![file.clone()],
            report_results: Some(true),
            in_reply_to: None,
            status: None,
            failure: None,
            answers: None,
            attestation: None,
        },
    )?;
    store.accept_message(
        &net.a,
        &net.network,
        "c",
        "m3",
        "read the attachment",
        vec![file.clone()],
        true,
        signed.attestation().cloned().context("signature")?,
        3,
    )?;
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
    conversation(&mut store, &net)?;
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
fn a_signed_main_input_authorizes_bot_lifecycle_locally_and_on_replicas() -> Result<()> {
    let (net, mut origin) = workspace_fixture()?;
    let input = workspace_input_fixture(
        &net,
        &mut origin,
        "main-profile",
        None,
        crate::workspace::InputIntent::Continue,
        3,
    )?;
    let trigger = message_trigger(&input.input.message_id);
    origin.change_bot(
        &net.a,
        &net.network,
        "c",
        &trigger,
        "rename-main",
        rename("Nova"),
        Provider::Codex,
        4,
    )?;
    let (created, new) = origin.create_bot_in_turn(
        &net.a,
        &net.network,
        "c",
        &trigger,
        "create-main",
        ("Typed child", "follow the signed Main input", None),
        5,
    )?;
    assert!(new);
    let (_, deleted) = origin.delete_bot_in_turn(
        &net.a,
        &net.network,
        "c",
        &trigger,
        &created.conversation_id,
        6,
    )?;
    assert!(deleted.is_some());
    assert_eq!(origin.bots(&net.network)?[0].name, "Nova");
    assert_eq!(origin.bot_changes(&net.network, "c")?.len(), 1);
    let mut replica = store(&net)?;
    let events = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
    assert_eq!(
        replica.ingest_events(&net.network, &events)?.statuses,
        vec![IngestStatus::Accepted; events.len()]
    );
    assert_eq!(replica.bots(&net.network)?[0].name, "Nova");
    assert_eq!(replica.bot_changes(&net.network, "c")?.len(), 1);
    assert_eq!(
        replica
            .conversation(&net.network, &created.conversation_id)?
            .and_then(|conversation| conversation.owner),
        Some(net.a.device_id()?)
    );
    assert!(replica.bot_deleted(&net.network, &created.conversation_id)?);
    Ok(())
}

#[test]
fn typed_bot_changes_reject_task_answers_forged_causes_and_indirect_sources() -> Result<()> {
    use crate::workspace::InputIntent;
    let (net, mut origin) = workspace_fixture()?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "create-task",
        workspace_task_command("task-source", None),
        3,
    )?;
    let main = workspace_input_fixture(
        &net,
        &mut origin,
        "main-source",
        None,
        InputIntent::Continue,
        4,
    )?;
    let task = workspace_input_fixture(
        &net,
        &mut origin,
        "task-input",
        Some("task-source"),
        InputIntent::Continue,
        5,
    )?;
    let answer = origin.append_event(
        &net.a,
        &net.network,
        "c",
        net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::MessagePosted {
                message_id: "a-q-source".into(),
                role: Role::User,
                text: "answer only".into(),
                attachments: vec![],
                report_results: Some(true),
                in_reply_to: None,
                status: None,
                answers: Some("q-source".into()),
                attestation: None,
                failure: None,
            },
        )?,
        vec![],
        6,
    )?;
    let indirect = origin.append_event(
        &net.a,
        &net.network,
        "c",
        bot_reply("after-main", "main-source"),
        vec![],
        7,
    )?;
    second_bot(&mut origin, &net, "Other")?;
    let mut target = store(&net)?;
    let events = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
    assert_eq!(
        target.ingest_events(&net.network, &events)?.statuses,
        vec![IngestStatus::Accepted; events.len()]
    );
    let change = |conversation: &str, message: &str, deps: Vec<String>| {
        net.b.sign_event(Event {
            version: WIRE_VERSION,
            network_id: net.network.clone(),
            author: net.b.device_id()?,
            seq: 1,
            prev: None,
            deps,
            scope_id: "c".into(),
            created_at_ms: 8,
            body: EventBody::BotChanged {
                change_id: "typed-change".into(),
                cause: Some(Cause {
                    conversation_id: conversation.into(),
                    message_id: message.into(),
                }),
                change: BotChange::Profile {
                    name: Some(Edit {
                        before: "Workspace Bot".into(),
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
    for (conversation, message, deps) in [
        ("c", "task-input", vec![task.source_event]),
        ("c", "a-q-source", vec![answer.hash]),
        ("c", "wrong-input", vec![main.source_event.clone()]),
        ("other", "main-source", vec![main.source_event.clone()]),
        ("c", "main-source", vec![indirect.hash]),
        ("c", "main-source", vec![]),
    ] {
        assert!(
            matches!(&target.ingest_events(&net.network, &[change(conversation, message, deps)?])?.statuses[0],
                IngestStatus::Rejected(error) if error.contains("user's own signed message")),
            "{conversation}/{message}"
        );
        assert_eq!(
            target
                .bot_for_conversation(&net.network, "c")?
                .context("source Bot")?
                .name,
            "Workspace Bot"
        );
        assert!(target.bot_changes(&net.network, "c")?.is_empty());
    }
    let hijack = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![main.source_event.clone()],
        scope_id: "d".into(),
        created_at_ms: 8,
        body: EventBody::ConversationOwner {
            scheduler_node: net.b.device_id()?,
            epoch: 1,
            attestation: None,
            cause: Some(Cause {
                conversation_id: "c".into(),
                message_id: "main-source".into(),
            }),
        },
        blobs: vec![],
    })?;
    assert!(
        matches!(&target.ingest_events(&net.network, &[hijack])?.statuses[0],
            IngestStatus::Rejected(error) if error.contains("a created bot's first owner follows its creation"))
    );
    assert_eq!(
        target
            .conversation(&net.network, "d")?
            .and_then(|conversation| conversation.owner),
        Some(net.a.device_id()?)
    );
    assert_eq!(
        target
            .ingest_events(
                &net.network,
                &[change("c", "main-source", vec![main.source_event])?],
            )?
            .statuses,
        vec![IngestStatus::Accepted]
    );
    assert_eq!(
        target
            .bot_for_conversation(&net.network, "c")?
            .context("source Bot")?
            .name,
        "Nova"
    );
    assert_eq!(target.bot_changes(&net.network, "c")?.len(), 1);
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
    let unread = origin.pending_notifications(&net.network, "web")?;
    assert!(
        unread.iter().any(|note| note.kind == "bot_message"
            && note.conversation_id == "d"
            && note.notification_id == message.event_hash),
        "the receiving chat counts the message as unread"
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

fn question_conversation(store: &mut Store, net: &Net) -> Result<()> {
    conversation(store, net)?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        bot(net, "Question Bot")?,
        vec![],
        1,
    )?;
    Ok(())
}

fn question(store: &Store, net: &Net, scope: &str, id: &str, session: &str) -> Result<EventBody> {
    Ok(EventBody::QuestionAsked {
        question_id: id.into(),
        target: Some(crate::workspace::InputTarget::BotMain {
            bot_id: store
                .bot_for_conversation(&net.network, scope)?
                .context("question fixture requires a real bot")?
                .bot_id,
        }),
        severity: Severity::Attention,
        text: "要继续吗？".into(),
        choices: vec![Choice {
            label: "继续".into(),
            action: None,
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

#[test]
fn main_projection_excludes_task_channel_and_unknown_replies() -> Result<()> {
    use crate::workspace::InputTarget;
    let net = net()?;
    let (mut origin, source) = bot_with_message(&net)?;
    let append_signed =
        |store: &mut Store, scope: &str, body: serde_json::Value, at| -> Result<()> {
            let body =
                net.a
                    .fixture_admin()
                    .attest(&net.network, scope, serde_json::from_value(body)?)?;
            store.append_event(&net.a, &net.network, scope, body, vec![], at)?;
            Ok(())
        };
    append_signed(
        &mut origin,
        "workspace",
        json!({
            "type":"workspace_commanded","command_id":"projection-channel",
            "command":{"kind":"create_channel","channel_id":"projection-channel","title":"Channel",
                "purpose":"Scope regression","members":["bot"],"coordinator_bot":"bot",
                "coordinator_node":net.a.device_id()?}
        }),
        4,
    )?;
    append_signed(
        &mut origin,
        "workspace",
        json!({
            "type":"workspace_commanded","command_id":"projection-task",
            "command":{"kind":"create_task","task_id":"projection-task","channel_id":"projection-channel",
                "owner_bot":"bot","title":"Task","request":"Keep this task private",
                "expected_result":"Result","source_events":[source],"dependencies":[],"reply_only":false}
        }),
        5,
    )?;
    let mut main_reply = bot_reply("main-reply", "m1");
    if let EventBody::MessagePosted { text, .. } = &mut main_reply {
        *text = "main visible".into();
    }
    origin.append_event(&net.a, &net.network, "c", main_reply, vec![], 6)?;
    let main_work = isolated_task(&mut origin, &net, "projection-main-work")?;
    assert!(super::scope_projection::work_is_main_from(
        &origin.connection,
        &net.network,
        "c",
        &main_work.work_id
    )?);
    assert!(
        !super::scope_projection::trigger_is_main_from(
            &origin.connection,
            &net.network,
            "c",
            &crate::scheduling::task_trigger(&main_work.work_id)
        )?,
        "Main work facts do not make its native task session a Main reply"
    );
    let assignment_target = InputTarget::ChannelTask {
        channel_id: "projection-channel".into(),
        task_id: "projection-task".into(),
        bot_id: "bot".into(),
    };
    let resolved = origin.resolve_input_target(&net.network, &assignment_target)?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "projection-assign",
        crate::workspace::WorkspaceCommand::AssignTask {
            task_id: "projection-task".into(),
            expected_revision: 1,
            assignment_id: "projection-assignment".into(),
            bot_id: resolved.bot_id,
            execution_node: resolved.owner_node,
            bot_owner_epoch: resolved.owner_epoch,
            channel_owner_epoch: resolved.channel_owner_epoch,
            work_id: None,
            resource_roots: vec![],
            automatic: false,
        },
        7,
    )?;
    let assignment_trigger = "assignment:projection-assignment";
    assert_eq!(
        origin.trigger_input_target(&net.network, "c", assignment_trigger)?,
        Some(assignment_target.clone())
    );
    let derived = WorkSpec {
        work_id: "projection-assignment-work".into(),
        trigger: assignment_trigger.into(),
        call_ref: "assignment-derived".into(),
        kind: WorkKind::Agent(Provider::Codex),
        ..main_work.clone()
    };
    origin.accept_work_fixture(&net.a, &net.network, derived.clone(), 8)?;
    assert!(
        origin
            .workspace_assignment(&net.network, "projection-assignment")?
            .context("source assignment")?
            .work_id
            .is_none()
    );
    assert_eq!(
        origin.work_input_target(&net.network, "c", &derived.work_id)?,
        Some(assignment_target.clone())
    );
    assert!(!super::scope_projection::work_is_main_from(
        &origin.connection,
        &net.network,
        "c",
        &derived.work_id
    )?);
    second_bot(&mut origin, &net, "Other Projection")?;
    assert_eq!(
        origin.trigger_input_target(&net.network, "d", assignment_trigger)?,
        None
    );
    assert_eq!(
        origin.trigger_input_target(&net.network, "c", "assignment:missing")?,
        None
    );
    origin.apply_work_signal(
        &net.a,
        &net.network,
        &derived.work_id,
        Signal::ThreadCreated("assignment-native-thread".into()),
        8,
    )?;
    origin.apply_work_signal(
        &net.a,
        &net.network,
        &derived.work_id,
        Signal::TurnStarted {
            turn_id: "assignment-native-turn".into(),
            epoch: 1,
        },
        9,
    )?;
    assert_eq!(
        origin.native_input_target(&net.network, "c", "assignment-native-thread", "")?,
        Some(assignment_target.clone())
    );
    let assignment = origin
        .workspace_assignment(&net.network, "projection-assignment")?
        .context("source assignment")?;
    let admission = super::scheduler::admit_assignment_from(
        &origin.connection,
        &net.network,
        &net.a.device_id()?,
        &assignment,
    )?;
    let task_job = origin
        .scheduler_job("c", &admission.trigger)?
        .context("Task job")?;
    super::scheduler::admit_from(
        &origin.connection,
        &net.network,
        "c",
        "message:m1",
        TriggerKind::Human,
        "m1",
    )?;
    let main_job = origin
        .scheduler_job("c", "message:m1")?
        .context("Main job")?;
    assert!(!origin.scheduler_native_in_scope(
        &net.network,
        &main_job,
        "assignment-native-thread"
    )?);
    assert!(origin.scheduler_native_in_scope(
        &net.network,
        &task_job,
        "assignment-native-thread"
    )?);
    assert!(
        origin
            .native_input_target(&net.network, "d", "assignment-native-thread", "")
            .is_err()
    );
    assert_eq!(
        origin.native_input_target(&net.network, "c", "unknown-external-thread", "")?,
        None
    );
    assert!(origin.scheduler_native_in_scope(
        &net.network,
        &main_job,
        "unknown-external-thread"
    )?);
    assert!(!origin.scheduler_native_in_scope(
        &net.network,
        &task_job,
        "unknown-external-thread"
    )?);
    for index in 0..2 {
        let work = WorkSpec {
            work_id: format!("main-native-work-{index}"),
            call_ref: format!("main-native-{index}"),
            kind: WorkKind::Agent(Provider::Codex),
            ..main_work.clone()
        };
        origin.accept_work_fixture(&net.a, &net.network, work.clone(), 10)?;
        origin.apply_work_signal(
            &net.a,
            &net.network,
            &work.work_id,
            Signal::ThreadCreated("main-native-thread".into()),
            11,
        )?;
        origin.apply_work_signal(
            &net.a,
            &net.network,
            &work.work_id,
            Signal::TurnStarted {
                turn_id: format!("main-native-turn-{index}"),
                epoch: 1,
            },
            12,
        )?;
        assert_eq!(
            origin.native_input_target(
                &net.network,
                "c",
                "main-native-thread",
                &format!("main-native-turn-{index}")
            )?,
            Some(InputTarget::Work {
                work_id: work.work_id.clone()
            })
        );
        if index == 0 {
            let mut own_job = main_job.clone();
            own_job.scope = SessionScope::Task {
                work_id: work.work_id,
            };
            assert!(origin.scheduler_native_in_scope(
                &net.network,
                &own_job,
                "main-native-thread"
            )?);
        }
    }
    assert!(origin.scheduler_native_in_scope(&net.network, &main_job, "main-native-thread")?);
    assert!(!origin.scheduler_native_in_scope(&net.network, &task_job, "main-native-thread")?);
    let mut own_job = main_job.clone();
    own_job.scope = SessionScope::Task {
        work_id: "main-native-work-0".into(),
    };
    assert!(!origin.scheduler_native_in_scope(&net.network, &own_job, "main-native-thread")?);
    assert!(
        origin
            .native_input_target(&net.network, "c", "main-native-thread", "")
            .unwrap_err()
            .to_string()
            .contains("conflicting input targets")
    );
    let conflicting = WorkSpec {
        work_id: "conflicting-native-work".into(),
        trigger: "message:m1".into(),
        call_ref: "conflicting-native".into(),
        kind: WorkKind::Agent(Provider::Codex),
        ..main_work.clone()
    };
    origin.accept_work_fixture(&net.a, &net.network, conflicting.clone(), 10)?;
    origin.apply_work_signal(
        &net.a,
        &net.network,
        &conflicting.work_id,
        Signal::ThreadCreated("assignment-native-thread".into()),
        11,
    )?;
    origin.apply_work_signal(
        &net.a,
        &net.network,
        &conflicting.work_id,
        Signal::TurnStarted {
            turn_id: "conflicting-native-turn".into(),
            epoch: 1,
        },
        12,
    )?;
    assert!(
        origin
            .scheduler_native_in_scope(&net.network, &main_job, "assignment-native-thread")
            .is_err()
    );
    assert!(
        origin
            .scheduler_native_in_scope(&net.network, &task_job, "assignment-native-thread")
            .is_err()
    );
    let targets = [
        InputTarget::Task {
            task_id: "projection-task".into(),
        },
        assignment_target,
    ];
    for (index, target) in targets.iter().enumerate() {
        let input_id = format!("private-input-{index}");
        append_signed(
            &mut origin,
            "c",
            json!({
                "type":"input_posted","message_id":input_id,"target":target,"intent":"continue",
                "sequence":index + 1,"expected_revision":1,"text":"private input","report_results":true
            }),
            7 + index as i64 * 2,
        )?;
        let private_work = WorkSpec {
            work_id: format!("projection-private-work-{index}"),
            trigger: message_trigger(&input_id),
            call_ref: format!("private-call-{index}"),
            ..main_work.clone()
        };
        origin.accept_work_fixture(
            &net.a,
            &net.network,
            private_work.clone(),
            8 + index as i64 * 2,
        )?;
        assert_eq!(
            origin.trigger_input_target(&net.network, "c", &work_trigger(&private_work.work_id))?,
            Some(target.clone())
        );
        assert!(!super::scope_projection::work_is_main_from(
            &origin.connection,
            &net.network,
            "c",
            &private_work.work_id
        )?);
        let reply = origin.append_event(
            &net.a,
            &net.network,
            "c",
            bot_reply(&format!("private-reply-{index}"), &input_id),
            vec![],
            8 + index as i64 * 2,
        )?;
        assert_eq!(
            origin
                .pending_notifications(&net.network, "web")?
                .into_iter()
                .find(|notification| notification.notification_id == reply.hash)
                .and_then(|notification| notification.target),
            Some(target.clone())
        );
    }
    let unknown = origin.append_event(
        &net.a,
        &net.network,
        "c",
        bot_reply("unknown-reply", "missing-source"),
        vec![],
        12,
    )?;
    assert_eq!(
        origin.trigger_input_target(&net.network, "c", "work:missing-work")?,
        None
    );
    assert!(!super::scope_projection::trigger_is_main_from(
        &origin.connection,
        &net.network,
        "c",
        "work:missing-work"
    )?);
    assert_eq!(
        origin
            .main_messages(&net.network, "c")?
            .iter()
            .map(|message| message.message_id.as_str())
            .collect::<Vec<_>>(),
        ["m1", "main-reply"]
    );
    assert_eq!(
        origin.last_messages(&net.network)?["c"].text,
        "main visible"
    );
    let notifications = origin.pending_notifications(&net.network, "web")?;
    assert_eq!(
        notifications
            .iter()
            .find(|note| note.notification_id == unknown.hash)
            .and_then(|note| note.target.as_ref()),
        None
    );
    for notification in notifications
        .into_iter()
        .filter(|notification| matches!(notification.target, Some(InputTarget::BotMain { .. })))
    {
        let body = net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::NotificationAcked {
                notification_id: notification.notification_id,
                client_id: "web".into(),
                attestation: None,
            },
        )?;
        origin.append_event(&net.a, &net.network, "c", body, vec![], 13)?;
    }
    let remaining = origin.pending_notifications(&net.network, "web")?;
    assert_eq!(
        remaining.len(),
        3,
        "Main ack preserves both private scopes and unknown notifications"
    );
    assert!(
        remaining
            .iter()
            .all(|notification| !matches!(notification.target, Some(InputTarget::BotMain { .. })))
    );
    let mut replica = store(&net)?;
    all_accepted(&mut replica, &net, &origin)?;
    assert_eq!(
        replica.pending_notifications(&net.network, "web")?,
        remaining
    );
    assert_eq!(
        replica.last_messages(&net.network)?,
        origin.last_messages(&net.network)?
    );
    assert!(
        replica.scheduler_jobs("c")?.is_empty(),
        "sync never schedules a private input"
    );
    Ok(())
}

#[test]
fn native_targets_require_one_exact_work_or_one_shared_task_target() -> Result<()> {
    use crate::workspace::{InputIntent, InputTarget, WorkspaceCommand};
    for case in ["main", "lineage", "task", "channel", "unknown", "foreign"] {
        let (net, mut origin) = workspace_fixture()?;
        let local = net.a.device_id()?;
        let scoped = matches!(case, "task" | "channel");
        if case == "channel" {
            workspace_command_fixture(
                &net,
                &mut origin,
                "channel",
                WorkspaceCommand::CreateChannel {
                    channel_id: "native-channel".into(),
                    title: "native channel".into(),
                    purpose: "shared native target".into(),
                    members: vec!["bot".into()],
                    coordinator_bot: "bot".into(),
                    coordinator_node: local.clone(),
                },
                3,
            )?;
        }
        if scoped {
            let mut command = workspace_task_command("native-task", None);
            if case == "channel"
                && let WorkspaceCommand::CreateTask { channel_id, .. } = &mut command
            {
                *channel_id = Some("native-channel".into());
            }
            workspace_command_fixture(&net, &mut origin, "task", command, 4)?;
        }
        let expected = match case {
            "task" => InputTarget::Task {
                task_id: "native-task".into(),
            },
            "channel" => InputTarget::ChannelTask {
                channel_id: "native-channel".into(),
                task_id: "native-task".into(),
                bot_id: "bot".into(),
            },
            _ => InputTarget::Work {
                work_id: "native-first".into(),
            },
        };
        let input = net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::InputPosted {
                message_id: "native-source".into(),
                target: if scoped {
                    expected.clone()
                } else {
                    InputTarget::BotMain {
                        bot_id: "bot".into(),
                    }
                },
                intent: InputIntent::Continue,
                sequence: 1,
                expected_revision: scoped.then_some(1),
                text: "native source".into(),
                attachments: vec![],
                report_results: Some(true),
                attestation: None,
            },
        )?;
        origin.append_event(&net.a, &net.network, "c", input, vec![], 5)?;
        super::scheduler::admit_from(
            &origin.connection,
            &net.network,
            "c",
            "message:native-source",
            TriggerKind::Human,
            "native-source",
        )?;
        let source_job = origin
            .scheduler_job("c", "message:native-source")?
            .context("native source job")?;
        let add = |store: &mut Store, id: &str, conversation: &str, trigger: &str| -> Result<()> {
            store.accept_work_fixture(
                &net.a,
                &net.network,
                WorkSpec {
                    work_id: id.into(),
                    conversation: conversation.into(),
                    trigger: trigger.into(),
                    target: local.clone(),
                    dispatcher: local.clone(),
                    call_ref: id.into(),
                    kind: WorkKind::Agent(Provider::Codex),
                    model: None,
                    resolved: ResolvedModel {
                        provider: Provider::Codex,
                        model: None,
                    },
                    cwd: "/workspace/native".into(),
                    title: id.into(),
                    prompt: id.into(),
                    continues: None,
                    parent: None,
                    thread: Some("exact-native-thread".into()),
                },
                6,
            )?;
            store.apply_work_signal(
                &net.a,
                &net.network,
                id,
                Signal::TurnStarted {
                    turn_id: "exact-native-turn".into(),
                    epoch: 1,
                },
                7,
            )?;
            assert_eq!(
                store
                    .work_record(id)?
                    .context("registered native work")?
                    .state
                    .handle()
                    .map(|handle| handle.turn_id.as_str()),
                Some("exact-native-turn")
            );
            Ok(())
        };
        add(&mut origin, "native-first", "c", "message:native-source")?;
        assert_eq!(
            origin.native_input_target(
                &net.network,
                "c",
                "exact-native-thread",
                "exact-native-turn"
            )?,
            Some(expected.clone()),
            "{case}: the single genuine binding is visible"
        );
        assert_eq!(
            origin.native_input_target(&net.network, "c", "exact-native-thread", "wrong-turn")?,
            None,
            "{case}: an unknown turn never borrows Main"
        );
        if case == "foreign" {
            second_bot(&mut origin, &net, "Foreign")?;
        }
        add(
            &mut origin,
            "native-second",
            if case == "foreign" { "d" } else { "c" },
            if case == "lineage" {
                "task:native-first"
            } else if case == "unknown" {
                "message:missing"
            } else {
                "message:native-source"
            },
        )?;
        let target = origin.native_input_target(
            &net.network,
            "c",
            "exact-native-thread",
            "exact-native-turn",
        );
        if scoped || case == "lineage" {
            assert_eq!(
                target?,
                Some(expected),
                "{case}: only the same exact Task target converges"
            );
            assert!(origin.scheduler_native_in_scope(
                &net.network,
                &source_job,
                "exact-native-thread"
            )?);
            if case == "lineage" {
                let mut own_job = source_job.clone();
                own_job.scope = SessionScope::Task {
                    work_id: "native-first".into(),
                };
                assert!(origin.scheduler_native_in_scope(
                    &net.network,
                    &own_job,
                    "exact-native-thread"
                )?);
                assert!(!super::scope_projection::work_is_main_from(
                    &origin.connection,
                    &net.network,
                    "c",
                    "native-second"
                )?);
            }
        } else {
            let error = target.unwrap_err().to_string();
            assert!(
                error.contains(match case {
                    "unknown" => "unknown input target",
                    "foreign" => "another conversation",
                    _ => "conflicting input targets",
                }),
                "{case}: {error}"
            );
            let in_scope =
                origin.scheduler_native_in_scope(&net.network, &source_job, "exact-native-thread");
            if case == "main" {
                assert!(in_scope?);
            } else {
                assert!(in_scope.is_err(), "{case}: invalid bindings stay rejected");
            }
        }
    }
    Ok(())
}

#[test]
fn a_question_target_is_shared_and_does_not_turn_unknown_into_main() -> Result<()> {
    use crate::workspace::InputTarget;
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    let work = isolated_task(&mut origin, &net, "work-question")?;
    let job = origin
        .claim_next_fixture(
            &net.a,
            &net.network,
            "c",
            "projection-boot",
            "projection-cfg",
        )?
        .context("question work")?
        .job;
    origin.record_manager_turn(
        &job.state.attempt().context("attempt")?.id,
        "system",
        "private-thread",
        Some("private-turn"),
        &Shown::default(),
    )?;
    let target = origin
        .native_input_target(&net.network, "c", "private-thread", "private-turn")?
        .context("verified question target")?;
    assert_eq!(
        target,
        InputTarget::Work {
            work_id: work.work_id
        }
    );
    assert_eq!(
        origin.native_input_target(&net.network, "c", "external-thread", "external-turn")?,
        None
    );
    let mut body = question(&origin, &net, "c", "q-scoped", "private-thread")?;
    if let EventBody::QuestionAsked {
        target: question_target,
        ..
    } = &mut body
    {
        *question_target = Some(target.clone());
    }
    origin.ask(&net.a, &net.network, "c", body, 4)?;
    let mut answer = user(&net, "m-answer", "continue");
    if let EventBody::MessagePosted {
        message_id,
        answers,
        attestation,
        ..
    } = &mut answer
    {
        *message_id = "a-q-scoped".into();
        *answers = Some("q-scoped".into());
        *attestation = None;
    }
    let answer = net.a.fixture_admin().attest(&net.network, "c", answer)?;
    origin.append_event(&net.a, &net.network, "c", answer, vec![], 5)?;
    origin.append_event(
        &net.a,
        &net.network,
        "c",
        bot_reply("question-reply", "a-q-scoped"),
        vec![],
        6,
    )?;
    let mut unknown = question(&origin, &net, "c", "q-unknown-scope", "external-thread")?;
    if let EventBody::QuestionAsked { target, .. } = &mut unknown {
        *target = None;
    }
    origin.ask(&net.a, &net.network, "c", unknown, 7)?;
    let mut replica = store(&net)?;
    all_accepted(&mut replica, &net, &origin)?;
    for node in [&origin, &replica] {
        assert_eq!(
            node.question(&net.network, "q-scoped")?
                .context("question")?["target"],
            json!(target)
        );
        assert_eq!(
            node.main_messages(&net.network, "c")?
                .iter()
                .map(|message| message.message_id.as_str())
                .collect::<Vec<_>>(),
            ["m1"]
        );
        assert_eq!(
            node.trigger_input_target(&net.network, "c", &message_trigger("a-q-scoped"))?,
            Some(target.clone())
        );
        let notifications = node.pending_notifications(&net.network, "web")?;
        assert_eq!(
            notifications
                .iter()
                .filter(|note| note.target == Some(target.clone()))
                .count(),
            2
        );
        assert_eq!(
            notifications
                .iter()
                .filter(|note| note.target.is_none())
                .count(),
            1
        );
    }
    Ok(())
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
        question_conversation(&mut store, &net)?;
        store.ask(
            &net.a,
            &net.network,
            "c",
            question(&store, &net, "c", "q-1", "t1")?,
            2,
        )?;
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
    question_conversation(&mut store, &net)?;
    let mut card = question(&store, &net, "c", "q-settings", "provider-setup-1")?;
    if let EventBody::QuestionAsked { about, .. } = &mut card {
        about.agent = None;
    }
    store.ask(&net.a, &net.network, "c", card, 2)?;
    store.ask(
        &net.a,
        &net.network,
        "c",
        question(&store, &net, "c", "q-session", "t1")?,
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
    question_conversation(&mut a, &net)?;
    a.ask(
        &net.a,
        &net.network,
        "c",
        question(&a, &net, "c", "q-1", "t1")?,
        2,
    )?;
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

fn approval(store: &Store, net: &Net, id: &str, request: &str) -> Result<EventBody> {
    let mut body = question(store, net, "c", id, "t1")?;
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
    question_conversation(&mut a, &net)?;
    a.ask(
        &net.a,
        &net.network,
        "c",
        approval(&a, &net, "q-1", "r-1")?,
        2,
    )?;
    a.ask(
        &net.a,
        &net.network,
        "c",
        approval(&a, &net, "q-2", "r-2")?,
        2,
    )?;
    a.ask(
        &net.a,
        &net.network,
        "c",
        question(&a, &net, "c", "q-3", "t3")?,
        2,
    )?;
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
    question_conversation(&mut a, &net)?;
    a.ask(
        &net.a,
        &net.network,
        "c",
        approval(&a, &net, "q-1", "r-1")?,
        2,
    )?;
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
    question_conversation(&mut a, &net)?;
    a.ask(
        &net.a,
        &net.network,
        "c",
        approval(&a, &net, "q-1", "r-1")?,
        2,
    )?;
    a.ask(
        &net.a,
        &net.network,
        "c",
        approval(&a, &net, "q-2", "r-2")?,
        2,
    )?;
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
    question_conversation(&mut store, &net)?;
    second_bot(&mut store, &net, "Other Questions")?;
    for (scope, id) in [("c", "q-1"), ("c", "q-2"), ("d", "q-3")] {
        store.ask(
            &net.a,
            &net.network,
            scope,
            question(&store, &net, scope, id, id)?,
            2,
        )?;
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
    let work = WorkSpec {
        work_id: "handoff-history".into(),
        conversation: "c".into(),
        trigger: "message:m1".into(),
        target: net.a.device_id()?,
        dispatcher: net.a.device_id()?,
        call_ref: "handoff-history".into(),
        kind: WorkKind::Agent(Provider::Codex),
        model: None,
        resolved: ResolvedModel {
            provider: Provider::Codex,
            model: None,
        },
        cwd: "/w".into(),
        title: "handoff".into(),
        prompt: "handoff".into(),
        continues: None,
        parent: None,
        thread: Some("s1".into()),
    };
    local.accept_work_fixture(&net.a, &net.network, work.clone(), 4)?;
    assert!(
        local
            .native_input_target(&net.network, "d", "s1", "")
            .is_err()
    );
    let seen = session_version(&local, &net, "s1")?;
    let hand = |store: &mut Store, from: &str, seen: &str, target: Option<&str>, at: i64| {
        store.hand_over_session(&net.a, &net.network, from, "s1", seen, target, at)
    };
    let (_, moved, written) = hand(&mut local, "c", &seen, Some("d"), 4)?;
    assert!(written.is_some());
    assert_ne!(moved, seen, "a hand-over gives the session a new version");
    let (_, standing, written) = hand(&mut local, "c", &seen, Some("d"), 4)?;
    assert!(
        written.is_none() && standing == moved,
        "the same call again reads back what stands"
    );
    assert!(
        local
            .native_input_target(&net.network, "d", "s1", "")
            .is_err(),
        "an attribution does not release a Starting foreign Work"
    );
    let (_, back, written) = hand(&mut local, "d", &moved, Some("c"), 5)?;
    assert!(written.is_some(), "the other bot hands it back");
    let stale = hand(&mut local, "c", &seen, Some("d"), 6).expect_err("c's retry is from before");
    assert!(
        stale
            .to_string()
            .contains(&format!("it is at version {back} now")),
        "a retry after the session moved on writes nothing: {stale}"
    );
    assert!(
        hand(&mut local, "c", &seen, None, 6).is_err(),
        "nor does a stale mute"
    );
    assert_eq!(session_version(&local, &net, "s1")?, back);
    assert_eq!(
        local
            .session_owner(&net.network, &net.a.device_id()?, "s1")?
            .as_deref(),
        Some("c")
    );
    local.apply_work_signal(
        &net.a,
        &net.network,
        &work.work_id,
        Signal::StartRejected(ExecFailure::new(
            FailureKind::Rejected,
            "native start rejected",
        )),
        7,
    )?;
    assert!(
        local
            .native_input_target(&net.network, "d", "s1", "")
            .is_err(),
        "a terminal Work needs explicit current attribution before changing Bot"
    );
    let seen = session_version(&local, &net, "s1")?;
    local.hand_over_session(&net.a, &net.network, "c", "s1", &seen, Some("d"), 8)?;
    assert_eq!(
        local.native_input_target(&net.network, "d", "s1", "")?,
        Some(crate::workspace::InputTarget::BotMain {
            bot_id: "bot-d".into()
        })
    );
    assert!(
        local
            .native_input_target(&net.network, "c", "s1", "")
            .is_err(),
        "a completed Work does not retain the old Bot's access after handoff"
    );
    for index in 0..2 {
        local.accept_work_fixture(
            &net.a,
            &net.network,
            WorkSpec {
                work_id: format!("handoff-active-{index}"),
                call_ref: format!("handoff-active-{index}"),
                ..work.clone()
            },
            9 + index,
        )?;
    }
    assert!(
        local
            .native_input_target(&net.network, "d", "s1", "")
            .is_err(),
        "new active foreign references keep the thread closed after an earlier handoff"
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
        scope: crate::scheduling::SessionScope::Main,
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
            scope: SessionScope::Main,
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
        scope: crate::scheduling::SessionScope::Main,
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
        let mut projection = ContextProjection {
            network_id: net.network.clone(),
            scope: SessionScope::Main,
            sources: vec![],
            sources_omitted: 0,
            inputs: vec![],
            inputs_omitted: 0,
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
        if index == 2 {
            projection.coverage.messages_omitted = 1;
        }
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
        let covers_before_native: String = store.connection.query_row(
            "SELECT covers FROM scheduler_sessions WHERE conversation_id='c' AND scope='main'",
            [],
            |row| row.get(0),
        )?;
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
        assert_eq!(
            covers, covers_before_native,
            "creating a session shows no history"
        );
        store.record_context_started(&attempt)?;
        let observed = memory_test_shown(&store)?;
        assert_eq!(
            observed.context.as_ref().context("started callback")?.stage,
            ContextStage::Started
        );
        assert_eq!(observed.memory, before.memory);
        assert_eq!(observed.works, before.works);
        let covers_started: String = store.connection.query_row(
            "SELECT covers FROM scheduler_sessions WHERE conversation_id='c' AND scope='main'",
            [],
            |row| row.get(0),
        )?;
        if index == 1 {
            assert_eq!(
                serde_json::from_str::<VersionVector>(&covers_started)?,
                claimed.job.basis
            );
        } else {
            assert_eq!(covers_started, covers, "omitted history remains unread");
        }
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
    question_conversation(&mut store, &net)?;
    store.ask(
        &net.a,
        &net.network,
        "c",
        question(&store, &net, "c", "q-1", "t1")?,
        2,
    )?;
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
    let (mut store, _) = bot_with_message(&net)?;
    let local = net.a.device_id()?;
    let first = WorkSpec {
        work_id: "w0".into(),
        conversation: "c".into(),
        trigger: message_trigger("m1"),
        target: local.clone(),
        dispatcher: local.clone(),
        call_ref: "call-0".into(),
        kind: WorkKind::Agent(Provider::Codex),
        cwd: "/tmp/t-a".into(),
        title: "t".into(),
        prompt: "p".into(),
        continues: None,
        parent: None,
        thread: Some("t-a".into()),
        model: None,
        resolved: ResolvedModel {
            provider: Provider::Codex,
            model: None,
        },
    };
    store.accept_work_fixture(&net.a, &net.network, first.clone(), 10)?;
    store.accept_work_fixture(
        &net.a,
        &net.network,
        WorkSpec {
            work_id: "w1".into(),
            call_ref: "call-1".into(),
            cwd: "/tmp/t-b".into(),
            thread: None,
            ..first.clone()
        },
        11,
    )?;
    store.apply_work_signal(
        &net.a,
        &net.network,
        "w1",
        Signal::ThreadCreated("t-b".into()),
        12,
    )?;
    let on = |store: &Store, thread| -> Result<Vec<String>> {
        Ok(store
            .work_records_on_thread(thread)?
            .into_iter()
            .map(|record| record.work_id)
            .collect())
    };
    assert_eq!(on(&store, "t-a")?, ["w0"]);
    assert_eq!(on(&store, "t-b")?, ["w1"]);
    assert_eq!(
        store.work_cwd_on_thread("t-b")?.as_deref(),
        Some("/tmp/t-b")
    );
    assert_eq!(store.work_cwd_on_thread("t-c")?, None);
    store.apply_work_signal(
        &net.a,
        &net.network,
        "w0",
        Signal::StartRejected(ExecFailure::new(
            FailureKind::Rejected,
            "native start rejected",
        )),
        13,
    )?;
    store.apply_work_signal(
        &net.a,
        &net.network,
        "w1",
        Signal::TurnStarted {
            turn_id: "u-b".into(),
            epoch: 1,
        },
        14,
    )?;
    store.apply_work_signal(
        &net.a,
        &net.network,
        "w1",
        Signal::TurnCompleted {
            turn_id: "u-b".into(),
            status: vibeface_bridge::turn::TurnStatus::Completed,
            text: "done".into(),
            error: None,
        },
        15,
    )?;
    assert!(on(&store, "t-a")?.is_empty());
    assert!(
        on(&store, "t-b")?.is_empty(),
        "stream routing excludes finished journals"
    );
    for thread in ["t-a", "t-b"] {
        assert_eq!(
            store.local_work_provider_on_thread(&net.network, &local, thread)?,
            Some(Provider::Codex)
        );
        assert_eq!(
            store.local_work_provider_on_thread("another-network", &local, thread)?,
            None
        );
        assert_eq!(
            store.local_work_provider_on_thread(&net.network, &net.b.device_id()?, thread)?,
            None
        );
    }
    let mut replica = Store::open(":memory:")?;
    replica.install_trust_bundle(&net.network, &net.state)?;
    all_accepted(&mut replica, &net, &store)?;
    assert_eq!(
        replica.local_work_provider_on_thread(&net.network, &local, "t-b")?,
        None,
        "shared Work evidence without a local journal cannot select a Provider"
    );
    store.accept_work_fixture(
        &net.a,
        &net.network,
        WorkSpec {
            work_id: "conflicting-provider".into(),
            call_ref: "conflicting-provider".into(),
            kind: WorkKind::Agent(Provider::Claude),
            resolved: ResolvedModel {
                provider: Provider::Claude,
                model: None,
            },
            ..first
        },
        16,
    )?;
    assert!(
        store
            .local_work_provider_on_thread(&net.network, &local, "t-a")
            .is_err()
    );
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
    store.message_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "serial-other",
        "d",
        "other input",
        5,
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

fn workspace_fixture() -> Result<(Net, Store)> {
    let net = net()?;
    let mut origin = store(&net)?;
    origin.create_conversation(
        &net.a,
        &net.network,
        "c",
        "workspace",
        net.a
            .owner_fixture(&net.network, "c", &net.a.device_id()?)?,
        1,
    )?;
    origin.append_event(
        &net.a,
        &net.network,
        "c",
        bot(&net, "Workspace Bot")?,
        vec![],
        2,
    )?;
    Ok((net, origin))
}
fn workspace_command_fixture(
    net: &Net,
    origin: &mut Store,
    id: &str,
    command: crate::workspace::WorkspaceCommand,
    at: i64,
) -> Result<crate::workspace::WorkspaceCommandReceipt> {
    let signed = net.a.fixture_admin().attest(
        &net.network,
        crate::workspace::WORKSPACE_SCOPE,
        EventBody::WorkspaceCommanded {
            acting_assignment: None,
            command_id: id.into(),
            command: command.clone(),
            attestation: None,
            cause: None,
        },
    )?;
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        id,
        command,
        signed.attestation().cloned(),
        None,
        None,
        at,
    )
}
fn workspace_task_command(id: &str, parent: Option<&str>) -> crate::workspace::WorkspaceCommand {
    crate::workspace::WorkspaceCommand::CreateTask {
        task_id: id.into(),
        channel_id: None,
        parent_task_id: parent.map(str::to_owned),
        owner_bot: "bot".into(),
        title: format!("task {id}"),
        request: format!("perform {id}"),
        expected_result: "verified result".into(),
        source_events: vec![],
        dependencies: vec![],
        reply_only: false,
    }
}
fn workspace_input_fixture(
    net: &Net,
    origin: &mut Store,
    id: &str,
    task: Option<&str>,
    intent: crate::workspace::InputIntent,
    at: i64,
) -> Result<crate::workspace::WorkspaceInput> {
    use crate::workspace::*;
    let target = task.map_or_else(
        || InputTarget::BotMain {
            bot_id: "bot".into(),
        },
        |id| InputTarget::Task { task_id: id.into() },
    );
    let expected_revision = task
        .map(|id| {
            origin
                .workspace_task(&net.network, id)?
                .map(|task| task.revision)
                .context("task exists")
        })
        .transpose()?;
    let input = InputRequest {
        message_id: id.into(),
        route: InputRoute {
            target: target.clone(),
            intent,
            sequence: at as u64,
            expected_revision,
        },
        text: format!("input {id}"),
        attachments: vec![],
        report_results: Some(true),
    };
    let signed = net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::InputPosted {
            message_id: input.message_id.clone(),
            target,
            intent,
            sequence: at as u64,
            expected_revision: input.route.expected_revision,
            text: input.text.clone(),
            attachments: vec![],
            report_results: Some(true),
            attestation: None,
        },
    )?;
    origin.post_workspace_input(
        &net.a,
        &net.network,
        input,
        signed.attestation().cloned().context("signed input")?,
        at,
    )
}

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
    let (history, truncated) = origin.workspace_history(&net.network, None, Some("t1"), 100)?;
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
            work_id: WorkSpec::id("c", &trigger, call_ref),
            conversation: "c".into(),
            trigger,
            target: net.a.device_id()?,
            dispatcher: net.a.device_id()?,
            call_ref: call_ref.into(),
            kind: WorkKind::Agent(Provider::Codex),
            model: None,
            resolved: ResolvedModel {
                provider: Provider::Codex,
                model: None,
            },
            cwd: "/workspace/history".into(),
            title: call_ref.into(),
            prompt: "report this work".into(),
            continues: None,
            parent: None,
            thread: None,
        };
        origin.accept_work_fixture(&net.a, &net.network, work.clone(), 19)?;
        origin.apply_work_signal(
            &net.a,
            &net.network,
            &work.work_id,
            Signal::ThreadCreated(format!("{call_ref}-thread")),
            20,
        )?;
        origin.apply_work_signal(
            &net.a,
            &net.network,
            &work.work_id,
            Signal::TurnStarted {
                turn_id: format!("{call_ref}-turn"),
                epoch: 1,
            },
            21,
        )?;
        works.push(work);
    }
    for ended in [false, true] {
        if ended {
            for work in &works {
                origin.apply_work_signal(
                    &net.a,
                    &net.network,
                    &work.work_id,
                    Signal::TurnCompleted {
                        turn_id: format!("{}-turn", work.call_ref),
                        status: vibeface_bridge::turn::TurnStatus::Completed,
                        text: format!("{} result", work.call_ref),
                        error: None,
                    },
                    22,
                )?;
            }
        }
        for (channel, task, expected_work) in [
            (None, Some("history-task"), &works[0].work_id),
            (Some("history-channel"), None, &works[0].work_id),
            (None, Some("t2"), &works[1].work_id),
        ] {
            let (history, truncated) =
                origin.workspace_history(&net.network, channel, task, 100)?;
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
            super::HistoryFilter {
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
            .workspace_history(&net.b.fixture_network_id()?, None, Some("t1"), 100)
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
    origin.connection.execute("UPDATE scheduler_jobs SET state='reconciling',attempt_id='attempt' WHERE conversation_id='c' AND trigger='message:defer'",[])?;
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
    origin.connection.execute(
        "UPDATE scheduler_jobs SET state='running' WHERE trigger='message:defer'",
        [],
    )?;
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
        origin
            .workspace_task(&net.network, "parallel8")?
            .unwrap()
            .state,
        TaskState::WaitingUser
    );
    Ok(())
}

#[test]
fn workspace_old_revision_results_and_unknown_assignments_do_not_release_current_work() -> Result<()>
{
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "task",
        workspace_task_command("task", None),
        3,
    )?;
    let input = workspace_input_fixture(
        &net,
        &mut origin,
        "m",
        Some("task"),
        InputIntent::Continue,
        4,
    )?;
    let assignment = origin
        .ensure_workspace_assignment(
            &net.a,
            &net.network,
            "task",
            &["/workspace/project".into()],
            5,
        )?
        .context("assignment")?;
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
    assert_eq!(
        origin.workspace_task(&net.network, "task")?.unwrap().state,
        TaskState::UpdatePending
    );
    workspace_command_fixture(
        &net,
        &mut origin,
        "task2",
        workspace_task_command("task2", None),
        11,
    )?;
    workspace_input_fixture(
        &net,
        &mut origin,
        "m2",
        Some("task2"),
        InputIntent::Continue,
        12,
    )?;
    let other = origin
        .ensure_workspace_assignment(
            &net.a,
            &net.network,
            "task2",
            &["/workspace/project/sub".into()],
            13,
        )?
        .context("assignment2")?;
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
        origin.workspace_task(&net.network, "task2")?.unwrap().state,
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
    workspace_command_fixture(
        &net,
        &mut origin,
        "create",
        workspace_task_command("task", None),
        3,
    )?;
    let initial = workspace_input_fixture(
        &net,
        &mut origin,
        "m1",
        Some("task"),
        InputIntent::Continue,
        4,
    )?;
    let assignment = origin
        .ensure_workspace_assignment(
            &net.a,
            &net.network,
            "task",
            &["/workspace/revision".into()],
            5,
        )?
        .context("assignment")?;
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
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "accepted",
        command(1, AssignmentState::Accepted, None, None, None),
        None,
        None,
        None,
        6,
    )?;
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "adopt-r1",
        command(
            1,
            AssignmentState::Adopted,
            Some(initial.source_event),
            None,
            None,
        ),
        None,
        None,
        None,
        7,
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
    assert_ne!(
        origin.workspace_task(&net.network, "task")?.unwrap().state,
        TaskState::Completed
    );
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
    assert_eq!(
        origin.workspace_task(&net.network, "task")?.unwrap().state,
        TaskState::Completed
    );
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
        conversation: "c".into(),
        trigger,
        target: net.a.device_id()?,
        dispatcher: net.a.device_id()?,
        call_ref: "existing-work".into(),
        kind: WorkKind::Agent(Provider::Codex),
        model: None,
        resolved: ResolvedModel {
            provider: Provider::Codex,
            model: None,
        },
        cwd: "/workspace/work".into(),
        title: "existing work".into(),
        prompt: "original work request".into(),
        continues: None,
        parent: None,
        thread: None,
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
        origin
            .workspace_assignment(&net.network, "attached-assignment")?
            .unwrap()
            .state,
        AssignmentState::Proposed
    );
    assert_eq!(
        origin
            .workspace_task(&net.network, "attached")?
            .unwrap()
            .state,
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
        origin
            .workspace_assignment(&net.network, "attached-assignment")?
            .unwrap()
            .state,
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
    origin.apply_work_signal(
        &net.a,
        &net.network,
        &work_id,
        Signal::TurnCompleted {
            turn_id: "turn".into(),
            status: vibeface_bridge::turn::TurnStatus::Completed,
            text: "original work finished".into(),
            error: None,
        },
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
    b.apply_workspace_command(
        &net.b,
        &net.network,
        "accepted-b",
        progress(AssignmentState::Accepted, None, None),
        None,
        None,
        None,
        7,
    )?;
    b.apply_workspace_command(
        &net.b,
        &net.network,
        "adopted-b",
        progress(AssignmentState::Adopted, Some(assignment.event_hash), None),
        None,
        None,
        None,
        8,
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
    let (history, _) = a.workspace_history(&net.network, None, Some("task"), 100)?;
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
    workspace_command_fixture(
        &net,
        &mut origin,
        "create",
        workspace_task_command("task", None),
        3,
    )?;
    let input = workspace_input_fixture(
        &net,
        &mut origin,
        "m1",
        Some("task"),
        InputIntent::Continue,
        4,
    )?;
    let assignment = origin
        .ensure_workspace_assignment(&net.a, &net.network, "task", &[], 5)?
        .context("assignment")?;
    let progress = |revision, state, source| WorkspaceCommand::AssignmentProgress {
        execution: None,
        assignment_id: assignment.assignment_id.clone(),
        revision,
        state,
        input_event: source,
        evidence_event: None,
        result: None,
    };
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "accepted",
        progress(1, AssignmentState::Accepted, None),
        None,
        None,
        None,
        6,
    )?;
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "adopt-r1",
        progress(1, AssignmentState::Adopted, Some(input.source_event)),
        None,
        None,
        None,
        7,
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
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "channel-accepted",
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
        9,
    )?;
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        "channel-adopted",
        WorkspaceCommand::AssignmentProgress {
            execution: None,
            assignment_id: assignment.assignment_id.clone(),
            revision: 1,
            state: AssignmentState::Adopted,
            input_event: Some(input.source_event),
            evidence_event: None,
            result: None,
        },
        None,
        None,
        None,
        10,
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
    workspace_command_fixture(
        &net,
        &mut origin,
        "create",
        workspace_task_command("task", None),
        3,
    )?;
    workspace_input_fixture(
        &net,
        &mut origin,
        "original",
        Some("task"),
        InputIntent::Continue,
        4,
    )?;
    let original = origin
        .ensure_workspace_assignment(&net.a, &net.network, "task", &[], 5)?
        .unwrap();
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
        origin
            .workspace_assignment(&net.network, &original.assignment_id)?
            .unwrap()
            .state,
        AssignmentState::Stopped
    );
    let (net, mut uncertain) = workspace_fixture()?;
    workspace_command_fixture(
        &net,
        &mut uncertain,
        "create",
        workspace_task_command("task", None),
        3,
    )?;
    workspace_input_fixture(
        &net,
        &mut uncertain,
        "inflight",
        Some("task"),
        InputIntent::Continue,
        4,
    )?;
    let inflight = uncertain
        .ensure_workspace_assignment(&net.a, &net.network, "task", &[], 5)?
        .unwrap();
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
    uncertain.started_workspace_assignment(&net.a, &net.network, &job, 6)?;
    assert_eq!(
        uncertain
            .workspace_assignment(&net.network, &inflight.assignment_id)?
            .unwrap()
            .state,
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
        workspace_command_fixture(
            &net,
            &mut origin,
            "create",
            workspace_task_command("task", None),
            3,
        )?;
        workspace_input_fixture(
            &net,
            &mut origin,
            "input",
            Some("task"),
            InputIntent::Continue,
            4,
        )?;
        let assignment = origin
            .ensure_workspace_assignment(&net.a, &net.network, "task", &[], 5)?
            .context("assignment")?;
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
            work_id: WorkSpec::id("c", &trigger, call_ref),
            conversation: "c".into(),
            trigger: trigger.clone(),
            target: local.clone(),
            dispatcher: local.clone(),
            call_ref: call_ref.into(),
            kind: WorkKind::Agent(Provider::Codex),
            model: None,
            resolved: ResolvedModel {
                provider: Provider::Codex,
                model: None,
            },
            cwd: "/workspace".into(),
            title: "native work".into(),
            prompt: "perform the accepted task".into(),
            continues: None,
            parent: None,
            thread: None,
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
fn workspace_lifecycle_waits_for_native_start_and_keeps_revision_adoption_explicit() -> Result<()> {
    use crate::scheduling::TurnExecution;
    use crate::workspace::*;
    use vibeface_bridge::failure::ProviderStage;
    let (net, mut origin) = workspace_fixture()?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "create",
        workspace_task_command("task", None),
        3,
    )?;
    workspace_input_fixture(
        &net,
        &mut origin,
        "initial",
        Some("task"),
        InputIntent::Continue,
        4,
    )?;
    let assignment = origin
        .ensure_workspace_assignment(&net.a, &net.network, "task", &[], 5)?
        .context("assignment")?;
    let queued = origin
        .scheduler_job("c", &message_trigger("initial"))?
        .context("queued job")?;
    origin.started_workspace_assignment(&net.a, &net.network, &queued, 6)?;
    assert_eq!(
        origin.workspace_task(&net.network, "task")?.unwrap().state,
        TaskState::NotStarted
    );
    let claimed = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "workspace-start")?
        .context("claimed job")?
        .job;
    let attempt = claimed.state.attempt().context("attempt")?.id.clone();
    origin.started_workspace_assignment(&net.a, &net.network, &claimed, 7)?;
    origin.mark_started("c", &claimed.trigger, &attempt)?;
    origin.started_workspace_assignment(&net.a, &net.network, &claimed, 8)?;
    assert_eq!(
        origin
            .workspace_assignment(&net.network, &assignment.assignment_id)?
            .unwrap()
            .state,
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
        origin
            .workspace_assignment(&net.network, &assignment.assignment_id)?
            .unwrap()
            .state,
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
    origin.started_workspace_assignment(&net.a, &net.network, &claimed, 10)?;
    let started = origin
        .workspace_assignment(&net.network, &assignment.assignment_id)?
        .unwrap();
    assert_eq!(started.state, AssignmentState::Adopted);
    assert_eq!(started.accepted_revision, Some(1));
    assert_eq!(started.adopted_revision, Some(1));
    assert!(started.completed_revision.is_none());
    assert!(started.result.is_none());
    assert_eq!(
        origin.workspace_task(&net.network, "task")?.unwrap().state,
        TaskState::Running
    );
    let recorded = origin.version_vector(&net.network)?;
    origin.started_workspace_assignment(&net.a, &net.network, &claimed, 11)?;
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
    origin.started_workspace_assignment(&net.a, &net.network, &claimed, 14)?;
    let unchanged = origin
        .workspace_assignment(&net.network, &assignment.assignment_id)?
        .unwrap();
    assert_eq!(unchanged.started_revision, 1);
    assert_eq!(unchanged.adopted_revision, Some(1));
    assert_eq!(
        origin.workspace_task(&net.network, "task")?.unwrap().state,
        TaskState::UpdatePending
    );
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
    assert_eq!(
        origin.workspace_task(&net.network, "task")?.unwrap().state,
        TaskState::UpdatePending
    );
    Ok(())
}

#[test]
fn workspace_cancelled_before_native_start_does_not_claim_revision_adoption() -> Result<()> {
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "create",
        workspace_task_command("task", None),
        3,
    )?;
    workspace_input_fixture(
        &net,
        &mut origin,
        "initial",
        Some("task"),
        InputIntent::Continue,
        4,
    )?;
    let assignment = origin
        .ensure_workspace_assignment(&net.a, &net.network, "task", &[], 5)?
        .context("assignment")?;
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
    origin.apply_workspace_input_control(&net.network, &net.a.device_id()?, "retry")?;
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
    conversation(&mut origin, &net)?;
    say(&mut origin, &net, "source", true)?;
    let local = net.a.device_id()?;
    for (call_ref, stop) in [("write", false), ("cancelled", true)] {
        let spec = WorkSpec {
            work_id: WorkSpec::id("c", "message:source", call_ref),
            conversation: "c".into(),
            trigger: "message:source".into(),
            target: local.clone(),
            dispatcher: local.clone(),
            call_ref: call_ref.into(),
            kind: WorkKind::Agent(Provider::Pi),
            model: None,
            resolved: ResolvedModel {
                provider: Provider::Pi,
                model: None,
            },
            cwd: "/work".into(),
            title: "work".into(),
            prompt: "perform work".into(),
            continues: None,
            parent: None,
            thread: None,
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
            conversation: "c".into(),
            trigger,
            target: node_b.clone(),
            dispatcher: node_b.clone(),
            call_ref: "ordinary".into(),
            kind: WorkKind::Agent(Provider::Codex),
            model: None,
            resolved: ResolvedModel {
                provider: Provider::Codex,
                model: None,
            },
            cwd: "/workspace/shared".into(),
            title: "ordinary work".into(),
            prompt: "existing write".into(),
            continues: None,
            parent: None,
            thread: None,
        },
        5,
    )?;
    b.apply_work_signal(
        &net.b,
        &net.network,
        &work_id,
        Signal::ThreadCreated("ordinary-thread".into()),
        6,
    )?;
    b.apply_work_signal(
        &net.b,
        &net.network,
        &work_id,
        Signal::TurnStarted {
            turn_id: "ordinary-turn".into(),
            epoch: 1,
        },
        7,
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
    b.apply_work_signal(
        &net.b,
        &net.network,
        &work_id,
        Signal::TurnCompleted {
            turn_id: "ordinary-turn".into(),
            status: vibeface_bridge::turn::TurnStatus::Completed,
            text: "done".into(),
            error: None,
        },
        10,
    )?;
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
        conversation: "c".into(),
        trigger: format!("assignment:{}", assignment.assignment_id),
        target: local.clone(),
        dispatcher: local.clone(),
        call_ref: "automatic-child-work".into(),
        kind: WorkKind::Agent(Provider::Codex),
        model: None,
        resolved: ResolvedModel {
            provider: Provider::Codex,
            model: None,
        },
        cwd: "/workspace/child".into(),
        title: "child".into(),
        prompt: "child".into(),
        continues: None,
        parent: None,
        thread: None,
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
    workspace_command_fixture(
        &net,
        &mut origin,
        "create",
        workspace_task_command("task", None),
        3,
    )?;
    workspace_input_fixture(
        &net,
        &mut origin,
        "original",
        Some("task"),
        InputIntent::Continue,
        4,
    )?;
    let first = origin
        .ensure_workspace_assignment(&net.a, &net.network, "task", &["/workspace/a".into()], 5)?
        .unwrap();
    let human = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "allocation-fence")?
        .unwrap()
        .job;
    let human_attempt = human.state.attempt().unwrap().id.clone();
    origin.mark_started("c", &human.trigger, &human_attempt)?;
    origin.started_workspace_assignment(&net.a, &net.network, &human, 6)?;
    assert!(origin.manager_attempt_allowed(
        &net.network,
        &local,
        "c",
        &human.trigger,
        &human_attempt
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
        conversation: "c".into(),
        trigger: format!("assignment:{}", first.assignment_id),
        target: local.clone(),
        dispatcher: local.clone(),
        call_ref: "fixed-result-work".into(),
        kind: WorkKind::Agent(Provider::Codex),
        model: None,
        resolved: ResolvedModel {
            provider: Provider::Codex,
            model: None,
        },
        cwd: "/workspace/a".into(),
        title: "result".into(),
        prompt: "result".into(),
        continues: None,
        parent: None,
        thread: None,
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
        origin.manager_attempt_allowed(
            &net.network,
            &local,
            "c",
            &result.trigger,
            &result_attempt
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
        !origin.manager_attempt_allowed(
            &net.network,
            &local,
            "c",
            &result.trigger,
            &result_attempt
        )?,
        "new disjoint resources do not restore the old assignment's live input authority"
    );
    Ok(())
}

#[test]
fn workspace_task_answer_keeps_the_assignment_from_the_signed_question_basis() -> Result<()> {
    use crate::scheduling::TurnExecution;
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
    origin.started_workspace_assignment(&net.a, &net.network, &running, 6)?;
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
        origin
            .workspace_assignment(&net.network, &first.assignment_id)?
            .unwrap()
            .state,
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
    origin.started_workspace_assignment(&net.a, &net.network, &answering, 12)?;
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

#[test]
fn workspace_release_retries_only_a_shared_proven_startup_failure_on_the_original_assignment()
-> Result<()> {
    use crate::scheduling::TurnExecution;
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "create",
        workspace_task_command("task", None),
        3,
    )?;
    let input = workspace_input_fixture(
        &net,
        &mut origin,
        "initial",
        Some("task"),
        InputIntent::Continue,
        4,
    )?;
    let assignment = origin
        .ensure_workspace_assignment(&net.a, &net.network, "task", &[], 5)?
        .unwrap();
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
    let (history, _) = origin.workspace_history(&net.network, None, Some("task"), 100)?;
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
        origin
            .workspace_assignment(&net.network, &assignment.assignment_id)?
            .unwrap()
            .state,
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
        replica
            .workspace_assignment(&net.network, &assignment.assignment_id)?
            .unwrap()
            .state,
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
    origin.started_workspace_assignment(&net.a, &net.network, &retry, 11)?;
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
    assert_eq!(
        origin.workspace_task(&net.network, "task")?.unwrap().state,
        TaskState::Completed
    );
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
        workspace_command_fixture(
            &net,
            &mut origin,
            "create",
            workspace_task_command("task", None),
            3,
        )?;
        let input = workspace_input_fixture(
            &net,
            &mut origin,
            "initial",
            Some("task"),
            InputIntent::Continue,
            4,
        )?;
        let assignment = origin
            .ensure_workspace_assignment(&net.a, &net.network, "task", &[], 5)?
            .unwrap();
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
            origin
                .workspace_assignment(&net.network, &assignment.assignment_id)?
                .unwrap()
                .state,
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
fn two_accepted_ordinary_works_compete_for_one_native_reservation() -> Result<()> {
    for (thread, winner_cwd) in [
        (None, "/same-workspace"),
        (Some("existing-thread"), "/same-workspace"),
        (None, "/aliased-workspace"),
        (Some("existing-thread"), "/aliased-workspace"),
    ] {
        let net = net()?;
        let mut origin = store(&net)?;
        conversation(&mut origin, &net)?;
        say(&mut origin, &net, "source", true)?;
        let local = net.a.device_id()?;
        let make = |call_ref: &str, cwd: &str| WorkSpec {
            work_id: WorkSpec::id("c", "message:source", call_ref),
            conversation: "c".into(),
            trigger: "message:source".into(),
            target: local.clone(),
            dispatcher: local.clone(),
            call_ref: call_ref.into(),
            kind: WorkKind::Agent(Provider::Pi),
            model: None,
            resolved: ResolvedModel {
                provider: Provider::Pi,
                model: None,
            },
            cwd: cwd.into(),
            title: "work".into(),
            prompt: "do the work".into(),
            continues: None,
            parent: None,
            thread: thread.map(str::to_owned),
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
        origin.apply_work_signal(
            &net.a,
            &net.network,
            &winner.work_id,
            Signal::TurnCompleted {
                turn_id: "winner-turn".into(),
                status: vibeface_bridge::turn::TurnStatus::Completed,
                text: "done".into(),
                error: None,
            },
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
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "create",
        workspace_task_command("task", None),
        3,
    )?;
    let input = workspace_input_fixture(
        &net,
        &mut origin,
        "input",
        Some("task"),
        InputIntent::Continue,
        4,
    )?;
    let assignment = origin
        .ensure_workspace_assignment(&net.a, &net.network, "task", &["/workspace".into()], 5)?
        .context("assignment")?;
    for (command_id, state, input_event, at) in [
        ("accepted", AssignmentState::Accepted, None, 6),
        (
            "adopted",
            AssignmentState::Adopted,
            Some(input.source_event),
            7,
        ),
    ] {
        origin.apply_workspace_command(
            &net.a,
            &net.network,
            command_id,
            WorkspaceCommand::AssignmentProgress {
                execution: None,
                assignment_id: assignment.assignment_id.clone(),
                revision: 1,
                state,
                input_event,
                evidence_event: None,
                result: None,
            },
            None,
            None,
            None,
            at,
        )?;
    }
    let assignment = origin
        .workspace_assignment(&net.network, &assignment.assignment_id)?
        .context("adopted assignment")?;
    let local = net.a.device_id()?;
    let trigger = format!("assignment:{}", assignment.assignment_id);
    let make = |call_ref: &str, cwd: &str| WorkSpec {
        work_id: WorkSpec::id("c", &trigger, call_ref),
        conversation: "c".into(),
        trigger: trigger.clone(),
        target: local.clone(),
        dispatcher: local.clone(),
        call_ref: call_ref.into(),
        kind: WorkKind::Agent(Provider::Pi),
        model: None,
        resolved: ResolvedModel {
            provider: Provider::Pi,
            model: None,
        },
        cwd: cwd.into(),
        title: "work".into(),
        prompt: "do the work".into(),
        continues: None,
        parent: None,
        thread: None,
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
    origin.apply_work_signal(
        &net.a,
        &net.network,
        &winner.work_id,
        Signal::TurnCompleted {
            turn_id: "winner-turn".into(),
            status: vibeface_bridge::turn::TurnStatus::Completed,
            text: "done".into(),
            error: None,
        },
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
        conversation(&mut origin, &net)?;
        say(&mut origin, &net, "source", true)?;
        let local = net.a.device_id()?;
        let make = |call_ref: &str| WorkSpec {
            work_id: WorkSpec::id("c", "message:source", call_ref),
            conversation: "c".into(),
            trigger: "message:source".into(),
            target: local.clone(),
            dispatcher: local.clone(),
            call_ref: call_ref.into(),
            kind: WorkKind::Agent(Provider::Codex),
            model: None,
            resolved: ResolvedModel {
                provider: Provider::Codex,
                model: None,
            },
            cwd: "/shared".into(),
            title: "work".into(),
            prompt: "p".into(),
            continues: None,
            parent: None,
            thread: Some("native".into()),
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

fn workspace_signed_input_fixture(
    net: &Net,
    origin: &mut Store,
    author: &LocalIdentity,
    input: crate::workspace::InputRequest,
    at: i64,
) -> Result<crate::workspace::WorkspaceInput> {
    let conversation = origin
        .resolve_input_target(&net.network, &input.route.target)?
        .conversation_id;
    let signed = net.a.fixture_admin().attest(
        &net.network,
        &conversation,
        EventBody::InputPosted {
            message_id: input.message_id.clone(),
            target: input.route.target.clone(),
            intent: input.route.intent,
            sequence: input.route.sequence,
            expected_revision: input.route.expected_revision,
            text: input.text.clone(),
            attachments: input.attachments.clone(),
            report_results: input.report_results,
            attestation: None,
        },
    )?;
    origin.post_workspace_input(
        author,
        &net.network,
        input,
        signed.attestation().cloned().context("input signature")?,
        at,
    )
}

fn workspace_started_job_fixture(net: &Net, origin: &mut Store, at: i64) -> Result<SchedulerJob> {
    let job = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "task-lifecycle")?
        .context("claimable workspace job")?
        .job;
    let attempt = job.state.attempt().context("attempt")?.id.clone();
    origin.mark_started("c", &job.trigger, &attempt)?;
    origin.update_turn_runtime(
        "c",
        &job.trigger,
        &attempt,
        vibeface_bridge::failure::ProviderStage::Run,
        crate::scheduling::TurnExecution::Started,
    )?;
    origin.started_workspace_assignment(&net.a, &net.network, &job, at)?;
    origin
        .scheduler_job("c", &job.trigger)?
        .context("started job")
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
                report_results: Some(reports),
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
            conversation: "c".into(),
            trigger: origin.scheduler_work_trigger(&net.network, &local, &manager)?,
            target: local.clone(),
            dispatcher: local.clone(),
            call_ref: "native".into(),
            kind: WorkKind::Agent(Provider::Codex),
            model: None,
            resolved: ResolvedModel {
                provider: Provider::Codex,
                model: None,
            },
            cwd: "/workspace".into(),
            title: "native task".into(),
            prompt: "perform the task".into(),
            continues: None,
            parent: None,
            thread: None,
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
                origin
                    .workspace_assignment(&net.network, &assignment.assignment_id)?
                    .unwrap()
                    .state,
                AssignmentState::Adopted
            );
            assert_eq!(
                origin.workspace_task(&net.network, "task")?.unwrap().state,
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
                origin.workspace_task(&net.network, "task")?.unwrap().state,
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
                pending_replica.workspace_task(&net.network, "task")?.unwrap().state,
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
                    origin
                        .workspace_assignment(&net.network, &assignment.assignment_id)?
                        .unwrap()
                        .state,
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
                origin
                    .workspace_assignment(&net.network, &assignment.assignment_id)?
                    .unwrap()
                    .state,
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
                origin
                    .workspace_assignment(&net.network, &assignment.assignment_id)?
                    .unwrap()
                    .state,
                AssignmentState::Adopted,
                "an older completed reply cannot steal the final settlement"
            );
            let final_job = if answer_last {
                origin.settle_workspace_assignment(&net.a, &net.network, &summarized, 27)?;
                assert_eq!(origin.workspace_assignment(&net.network, &assignment.assignment_id)?.unwrap().state, AssignmentState::Adopted);
                let answer = workspace_started_job_fixture(&net, &mut origin, 28)?;
                assert_eq!(answer.kind, TriggerKind::Answer);
                let answered = origin.finish_job(&net.a, &net.network, "c", &answer.trigger,
                    &answer.state.attempt().unwrap().id, JobOutcome::Completed, "answer final summary", 29)?;
                origin.settle_workspace_assignment(&net.a, &net.network, &summarized, 30)?;
                assert_eq!(origin.workspace_assignment(&net.network, &assignment.assignment_id)?.unwrap().state, AssignmentState::Adopted);
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
                    origin.workspace_task(&net.network, "task")?.unwrap().state,
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
            origin.workspace_task(&net.network, "task")?.unwrap().state,
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
            report_results: Some(true),
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
        origin
            .workspace_assignment(&net.network, &first.assignment_id)?
            .unwrap()
            .state,
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

#[test]
fn resource_waiting_inputs_keep_their_ids_and_share_the_first_assignment() -> Result<()> {
    use crate::scheduling::{QueueHoldReason, TurnExecution};
    use crate::workspace::*;
    let (net, mut origin) = workspace_fixture()?;
    let local = net.a.device_id()?;
    second_bot(&mut origin, &net, "Waiting Bot")?;
    workspace_input_fixture(&net, &mut origin, "writer", None, InputIntent::Continue, 3)?;
    let writer = WorkSpec {
        work_id: WorkSpec::id("c", &message_trigger("writer"), "writer"),
        conversation: "c".into(),
        trigger: message_trigger("writer"),
        target: local.clone(),
        dispatcher: local.clone(),
        call_ref: "writer".into(),
        kind: WorkKind::Agent(Provider::Codex),
        model: None,
        resolved: ResolvedModel {
            provider: Provider::Codex,
            model: None,
        },
        cwd: "/workspace/shared".into(),
        title: "existing writer".into(),
        prompt: "existing writer".into(),
        continues: None,
        parent: None,
        thread: None,
    };
    origin.accept_work_fixture(&net.a, &net.network, writer.clone(), 4)?;
    origin.apply_work_signal(
        &net.a,
        &net.network,
        &writer.work_id,
        Signal::ThreadCreated("writer-thread".into()),
        5,
    )?;
    origin.apply_work_signal(
        &net.a,
        &net.network,
        &writer.work_id,
        Signal::TurnStarted {
            turn_id: "writer-turn".into(),
            epoch: 1,
        },
        6,
    )?;
    let mut task = workspace_task_command("waiting", None);
    if let WorkspaceCommand::CreateTask { owner_bot, .. } = &mut task {
        *owner_bot = "bot-d".into();
    }
    workspace_command_fixture(&net, &mut origin, "create-waiting", task, 7)?;
    let roots = vec!["/workspace/shared/sub".into()];
    for index in 0..crate::scheduling::MAX_PENDING_JOBS {
        let source = format!("inbox-fill-{index}");
        super::scheduler::admit_from(
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
            report_results: Some(true),
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
                report_results: Some(true),
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
        origin.started_workspace_assignment(&net.a, &net.network, &job, 17 + index as i64)?;
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
            origin
                .workspace_assignment(&net.network, &assignment.assignment_id)?
                .unwrap()
                .state,
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
            report_results: Some(true),
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
                b.workspace_assignment(&net.network, &first.assignment_id)?
                    .unwrap()
                    .state,
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
    let error = origin
        .admit_workspace_input(&net.network, &local, "stale")
        .expect_err("a newly admitted input cannot use an obsolete task revision");
    assert!(error.to_string().contains("input task revision changed"));
    assert!(
        origin
            .scheduler_job("c", &message_trigger("stale"))?
            .is_none()
    );
    assert!(
        origin
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "stale-revision")?
            .is_none()
    );
    Ok(())
}
