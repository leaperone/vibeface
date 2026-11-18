use super::super::bot_settings::ChangeRequest;
use super::super::test_support::{
    Fleet, Net, Piece, all_accepted, bot_conversation, bot_reply, bot_with_message, converges,
    conversation, created, dump, effective, fleet, net, owner, revoke, save_skill, second_bot,
    seq_of, store, sync, user, work_spec, write,
};
use super::super::*;
use crate::providers::Provider;
use crate::scheduling::message_trigger;
use crate::work::Signal;

#[test]
fn one_personal_network_is_enforced_before_writes_and_on_reads() -> Result<()> {
    let other = net()?;
    let net = net()?;
    let dir = std::env::temp_dir().join(format!("vibeface-one-network-{}", uuid::Uuid::new_v4()));
    let path = dir.join("node.sqlite3");
    let mut store = Store::open(&path)?;
    store.install_trust_bundle(&net.network, &net.state)?;
    let before: Vec<(String, String)> = store
        .connection
        .prepare("SELECT key,value FROM identity_meta ORDER BY key")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert!(!store.install_trust_bundle(&net.network, &net.state)?);
    assert!(
        store
            .install_trust_bundle(&other.network, &other.state)
            .is_err()
    );
    assert_eq!(store.network()?, Some(net.network.clone()));
    assert_eq!(store.public_state()?, net.state);
    let after: Vec<(String, String)> = store
        .connection
        .prepare("SELECT key,value FROM identity_meta ORDER BY key")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(after, before, "a foreign bundle has no local side effects");
    store.connection.execute(
        "INSERT INTO networks(network_id,bundle) VALUES(?,?)",
        params![other.network, serde_json::to_string(&other.state)?],
    )?;
    assert!(store.network().is_err());
    assert!(store.public_state().is_err());
    assert!(store.has_network().is_err());
    assert!(
        store
            .install_trust_bundle(&net.network, &net.state)
            .is_err()
    );
    drop(store);
    assert!(Store::open(&path).is_err());
    assert!(Store::open_readonly(&path).is_err());
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn the_write_gate_stops_local_appends_only() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    bot_conversation(&mut origin, &net)?;
    let prelude = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
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
        gated.ingest_events(&net.network, &prelude)?.statuses,
        vec![IngestStatus::Accepted; prelude.len()]
    );
    gated.set_write_state(WriteState::Writable)?;
    gated.append_event(&net.b, &net.network, "c", user(&net, "m1", "x"), vec![], 3)?;
    Ok(())
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
    Ok(context_after(
        &store.connection,
        &net.network,
        "c",
        None,
        &VersionVector::new(),
    )?
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
    for work_id in ["unknown-back", "unknown-late"] {
        b_side.append_event(
            &net.b,
            &net.network,
            "c",
            EventBody::WorkDispatched {
                work_id: work_id.into(),
                trigger: "message:request".into(),
                dispatcher: net.a.device_id()?,
                call_ref: work_id.into(),
                kind: WorkKind::Agent(Provider::Codex),
                model: None,
                resolved: ResolvedModel {
                    provider: Provider::Codex,
                    model: None,
                },
                cwd: "/tmp".into(),
                title: "unknown result".into(),
                prompt: "work".into(),
                continues: None,
                parent: None,
                thread: None,
            },
            vec![],
            1,
        )?;
    }
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
    let unknown = |work_id: &str| EventBody::WorkFinished {
        work_id: work_id.into(),
        outcome: WorkOutcome::Unverified,
        result_text: "the target's result is unknown".into(),
        failure_reason: None,
        failure: None,
        attestation: None,
    };
    a_side.append_event(
        &net.a,
        &net.network,
        "c",
        unknown("unknown-back"),
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
    let late_unknown = a_side.append_event(
        &net.a,
        &net.network,
        "c",
        unknown("unknown-late"),
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
    assert_eq!(
        replica
            .ingest_events(&net.network, std::slice::from_ref(&late_unknown))?
            .statuses,
        [IngestStatus::Accepted]
    );
    let a_log = a_log(&a_side)?;
    let b_log = b_side.events_from(&net.network, &net.b.device_id()?, 1, 500)?;
    for order in [[&a_log, &b_log], [&b_log, &a_log]] {
        let mut reordered = store(&net)?;
        for log in order {
            reordered.ingest_events(&net.network, log)?;
        }
        for work in ["unknown-back", "unknown-late"] {
            assert_eq!(
                reordered
                    .works(&net.network, "c")?
                    .into_iter()
                    .find(|row| row.work_id == work)
                    .context("unknown result")?
                    .outcome,
                Some(WorkOutcome::Unverified)
            );
        }
    }

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
    assert!(
        a_side
            .append_event(
                &net.a,
                &net.network,
                "c",
                unknown("unknown-stale"),
                vec![],
                7
            )
            .is_err()
    );
    let forged = net.a.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.a.device_id()?,
        seq: seq_of(&late_unknown)? + 1,
        prev: Some(late_unknown.hash.clone()),
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

#[test]
fn internal_authorization_errors_roll_back_the_whole_ingested_batch() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    conversation(&mut origin, &net)?;
    let mut replica = store(&net)?;
    sync(&origin, &mut replica, &net)?;
    let before = replica.heads(&net.network)?;
    replica.connection.execute(
        "UPDATE events SET envelope='{' WHERE kind='conversation_owner'",
        [],
    )?;
    let created = origin.append_event(
        &net.b,
        &net.network,
        "other",
        EventBody::ConversationCreated {
            title: "must roll back".into(),
        },
        vec![],
        2,
    )?;
    let checkpoint = origin.append_event(
        &net.a,
        &net.network,
        "c",
        checkpoint_body("must roll back"),
        vec![],
        3,
    )?;
    assert!(
        replica
            .ingest_events(&net.network, &[created, checkpoint])
            .is_err()
    );
    assert_eq!(replica.heads(&net.network)?, before);
    assert!(replica.conversation(&net.network, "other")?.is_none());
    Ok(())
}

#[test]
fn narrowed_attesters_disable_stored_signatures_and_their_causes_without_erasing_history()
-> Result<()> {
    let mut net = net()?;
    let setup = LocalIdentity::generate();
    net.state.append(crate::identity::GovernanceEvent::grant(
        &net.a.fixture_admin(),
        &net.state.project()?,
        Member::client(&setup, "setup", false)?,
    )?)?;
    let mut store = store(&net)?;
    let owner = setup.attest(
        &net.network,
        "c",
        EventBody::ConversationOwner {
            scheduler_node: net.a.device_id()?,
            epoch: 1,
            attestation: None,
            cause: None,
        },
    )?;
    store.create_conversation(
        &net.a,
        &net.network,
        "c",
        "t",
        FirstOwner {
            scheduler_node: net.a.device_id()?,
            attestation: owner.attestation().cloned().context("signed owner")?,
        },
        1,
    )?;
    let created = setup.attest(
        &net.network,
        "c",
        EventBody::BotCreated {
            bot_id: "bot".into(),
            name: "Helper".into(),
            instructions: "be brief".into(),
            project: None,
            attestation: None,
            cause: None,
        },
    )?;
    store.append_event(&net.a, &net.network, "c", created, vec![], 2)?;
    let request = store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "request", "go"),
        vec![],
        2,
    )?;
    let deleted = store.append_event(
        &net.b,
        &net.network,
        "other-bot",
        EventBody::BotDeleted {
            attestation: None,
            cause: Some(Cause {
                conversation_id: "c".into(),
                message_id: "request".into(),
            }),
        },
        vec![],
        3,
    )?;
    let parent = net.a.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.a.device_id()?,
        seq: seq_of(&request)? + 1,
        prev: Some(request.hash.clone()),
        deps: vec![],
        scope_id: "other".into(),
        created_at_ms: 4,
        body: EventBody::ConversationCreated {
            title: "dependency".into(),
        },
        blobs: vec![],
    })?;
    let waiting = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: seq_of(&deleted)? + 1,
        prev: Some(deleted.hash.clone()),
        deps: vec![parent.hash.clone()],
        scope_id: "c".into(),
        created_at_ms: 5,
        body: user(&net, "waiting", "wait"),
        blobs: vec![],
    })?;
    assert_eq!(
        store
            .ingest_events(&net.network, std::slice::from_ref(&waiting))?
            .statuses,
        [IngestStatus::Pending]
    );
    let original = net.state.project()?;
    let mut narrowed = original.clone();
    let client = net.a.fixture_admin().device_id()?;
    narrowed.attesters.retain(|id| id != &client);
    let rejected = net.b.sign_event(Event {
        body: user(&net, "new", "not authorized"),
        ..serde_json::from_str::<Event>(&waiting.canonical_body)?
    })?;
    assert!(matches!(
        log::ingest_from(
            &store.connection,
            &narrowed,
            &rejected,
            6,
            &mut log::Placed::default()
        )?,
        IngestStatus::Rejected(_)
    ));
    assert!(log::recompute(&store.connection, &narrowed)?);
    assert_eq!(
        store
            .conversation(&net.network, "c")?
            .context("conversation")?
            .owner,
        Some(net.a.device_id()?)
    );
    assert_eq!(
        store
            .bot_for_conversation(&net.network, "c")?
            .context("retained bot")?
            .bot_id,
        "bot"
    );
    assert!(store.messages(&net.network, "c")?.is_empty());
    let deleted_count: i64 =
        store
            .connection
            .query_row("SELECT count(*) FROM bot_deletions", [], |row| row.get(0))?;
    assert_eq!(deleted_count, 0);
    let mut placed = log::Placed::default();
    assert_eq!(
        log::ingest_from(&store.connection, &narrowed, &parent, 7, &mut placed)?,
        IngestStatus::Accepted
    );
    for envelope in [&request, &deleted, &waiting] {
        let stored: (bool, String) = store.connection.query_row(
            "SELECT effective,envelope FROM events WHERE network_id=? AND hash=?",
            params![net.network, envelope.hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert!(!stored.0);
        assert_eq!(stored.1, serde_json::to_string(envelope)?);
    }
    assert!(store.work_records()?.is_empty());
    assert!(store.scheduler_jobs("c")?.is_empty());
    assert!(log::recompute(&store.connection, &original)?);
    assert_eq!(
        store
            .messages(&net.network, "c")?
            .into_iter()
            .map(|message| message.message_id)
            .collect::<Vec<_>>(),
        ["request", "waiting"]
    );
    assert_eq!(
        effective(&store, &net, &[&request, &deleted, &waiting])?,
        [true; 3]
    );
    assert!(store.work_records()?.is_empty());
    assert!(store.scheduler_jobs("c")?.is_empty());
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
    let spec = work_spec(&net.a.device_id()?, &trigger, "call-1", Provider::Codex);
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

#[test]
fn a_revocation_cutoff_converges_whatever_arrived_first() -> Result<()> {
    let fleet = fleet()?;
    let net = &fleet.net;
    let mut origin = store(net)?;
    let prelude = created(&mut origin, net, &net.a)?;
    let mut writes = vec![];
    for i in 1..=10 {
        writes.push(write(&mut origin, net, &fleet.x, &format!("x{i}"), 10 + i)?);
    }
    let answer = write(&mut origin, net, &net.b, "b1", 30)?;
    let revoked = revoke(&fleet, &net.state, &fleet.x, Some(&writes[6]))?;
    let pieces = [
        Piece::Events([prelude.clone(), writes[..7].to_vec()].concat()),
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
    let prelude = created(&mut origin, net, &net.a)?;
    let created = prelude.last().context("Bot prelude")?.clone();
    let mut writes = vec![];
    for i in 1..=3 {
        writes.push(write(&mut origin, net, &y, &format!("y{i}"), 10 + i)?);
    }
    let pieces = [
        Piece::Events([prelude, writes.clone()].concat()),
        Piece::Governance(granted),
        Piece::Governance(concurrent),
    ];
    let reference = converges(net, &pieces)?;
    assert!(reference.messages(&net.network, "c")?.is_empty());
    assert_eq!(
        effective(&reference, net, &[&created, &writes[0], &writes[2]])?,
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
    let prelude = created(&mut origin, net, &net.a)?;
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
        Piece::Events([prelude.clone(), x_writes.clone(), vec![z_write.clone()]].concat()),
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
fn a_retired_bound_holds_across_recovery_and_old_attestations_verify() -> Result<()> {
    use crate::identity::{Revocation, RootCheckpoint};
    let fleet = fleet()?;
    let net = &fleet.net;
    let mut origin = store(net)?;
    let prelude = created(&mut origin, net, &net.a)?;
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
        .ingest_events(&net.network, &[prelude.clone(), writes.clone()].concat())?
        .statuses;
    assert!(
        statuses.iter().all(|s| *s == IngestStatus::Accepted),
        "{statuses:?}"
    );

    let pieces = [
        Piece::Events([prelude.clone(), writes.clone()].concat()),
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
