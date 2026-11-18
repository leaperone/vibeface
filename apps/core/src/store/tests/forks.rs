use super::super::test_support::{
    Piece, bot_conversation, converges, created, effective, fleet, net, replay, revoke, seq_of,
    store, user, workspace_assigned_task_fixture, workspace_fixture, write,
};
use super::super::*;

#[test]
fn causal_clocks_preserve_exact_fork_history_across_import_orders_and_recomputation() -> Result<()>
{
    let (net, mut origin) = workspace_fixture()?;
    workspace_assigned_task_fixture(
        &net,
        &mut origin,
        "create-causal-task",
        "causal-task",
        "causal-task-input",
        &[],
        3,
    )?;
    let prelude = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
    let base = prelude.last().context("prelude")?;
    let left = origin.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "left", "left"),
        vec![],
        10,
    )?;
    let left_tail = origin.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "left-tail", "left tail"),
        vec![],
        11,
    )?;
    let right = net.a.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.a.device_id()?,
        seq: seq_of(base)? + 1,
        prev: Some(base.hash.clone()),
        deps: vec![],
        scope_id: "c".into(),
        created_at_ms: 10,
        body: user(&net, "right", "right"),
        blobs: vec![],
    })?;
    let right_tail = net.a.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.a.device_id()?,
        seq: seq_of(base)? + 2,
        prev: Some(right.hash.clone()),
        deps: vec![],
        scope_id: "c".into(),
        created_at_ms: 11,
        body: net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::ConversationOwner {
                scheduler_node: net.b.device_id()?,
                epoch: 2,
                attestation: None,
                cause: None,
            },
        )?,
        blobs: vec![],
    })?;
    let mut deps = vec![left_tail.hash.clone(), right_tail.hash.clone()];
    deps.sort();
    let merged = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps,
        scope_id: "c".into(),
        created_at_ms: 12,
        body: user(&net, "merged", "both fork branches"),
        blobs: vec![],
    })?;
    let after_merge = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 2,
        prev: Some(merged.hash.clone()),
        deps: vec![],
        scope_id: "c".into(),
        created_at_ms: 13,
        body: user(&net, "after-merge", "after merge"),
        blobs: vec![],
    })?;
    let orders = [
        vec![
            left.clone(),
            left_tail.clone(),
            right.clone(),
            right_tail.clone(),
            merged.clone(),
            after_merge.clone(),
        ],
        vec![
            right.clone(),
            right_tail.clone(),
            left.clone(),
            left_tail.clone(),
            merged.clone(),
            after_merge.clone(),
        ],
        vec![
            merged.clone(),
            after_merge.clone(),
            left.clone(),
            left_tail.clone(),
            right.clone(),
            right_tail.clone(),
        ],
    ];
    let mut projected = None;
    for (index, order) in orders.into_iter().enumerate() {
        let mut target = store(&net)?;
        target.ingest_events(&net.network, &prelude)?;
        let ingested = if index == 2 {
            let waiting = target.ingest_events(&net.network, &order[..2])?;
            assert_eq!(
                waiting.statuses,
                [IngestStatus::Pending, IngestStatus::Pending]
            );
            assert!(target.event(&net.network, &merged.hash)?.is_none());
            assert!(target.event(&net.network, &after_merge.hash)?.is_none());
            assert_eq!(
                target.ingest_events(&net.network, &order[..2])?.statuses,
                [IngestStatus::Duplicate, IngestStatus::Duplicate],
            );
            let mut resumed = target.ingest_events(&net.network, &order[2..])?;
            resumed.statuses = [waiting.statuses, resumed.statuses].concat();
            resumed
        } else {
            target.ingest_events(&net.network, &order)?
        };
        assert!(
            !ingested
                .statuses
                .iter()
                .any(|status| matches!(status, IngestStatus::Rejected(_)))
        );
        assert!(
            ingested.rebuilt,
            "fork evidence must rebuild effective projections"
        );
        if index == 2 {
            assert_eq!(
                &ingested.statuses[..2],
                &[IngestStatus::Pending, IngestStatus::Pending]
            );
        }
        let pending: i64 =
            target
                .connection
                .query_row("SELECT count(*) FROM pending_events", [], |row| row.get(0))?;
        assert_eq!(
            pending, 0,
            "both missing dependencies and their waiting child cascaded"
        );
        for envelope in prelude.iter().chain(&order) {
            let event: Event = serde_json::from_str(&envelope.canonical_body)?;
            workspace::assert_causal_reads_match_recursive_history(
                &target.connection,
                &net.network,
                &event,
            )?;
            assert_eq!(
                serde_json::to_value(log::CausalClock::stored(
                    &target.connection,
                    &net.network,
                    &envelope.hash
                )?)?,
                serde_json::to_value(log::CausalClock::before(
                    &target.connection,
                    &net.network,
                    &event
                )?)?,
            );
        }
        let right_event: Event = serde_json::from_str(&right_tail.canonical_body)?;
        let right_clock = log::CausalClock::before(&target.connection, &net.network, &right_event)?;
        assert!(right_clock.contains(&target.connection, &net.network, &right.hash)?);
        assert!(!right_clock.contains(&target.connection, &net.network, &left.hash)?);
        assert_eq!(seq_of(&left)?, seq_of(&right)?);
        let merged_event: Event = serde_json::from_str(&merged.canonical_body)?;
        let clock = log::CausalClock::before(&target.connection, &net.network, &merged_event)?;
        assert!(clock.contains(&target.connection, &net.network, &left_tail.hash)?);
        assert!(clock.contains(&target.connection, &net.network, &right_tail.hash)?);
        assert_eq!(
            serde_json::to_value(&clock)?
                .as_object()
                .expect("clock map")
                .len(),
            2
        );
        assert_eq!(
            clock.version_vector().len(),
            1,
            "one actor still needs two exact chains"
        );
        let snapshots = || -> Result<Vec<(String, String, String)>> {
            Ok(target
                .connection
                .prepare("SELECT hash,causal_chain,causal_clock FROM events ORDER BY hash")?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?)
        };
        let before = snapshots()?;
        assert!(!log::recompute(
            &target.connection,
            &membership_from(&target.connection, &net.network)?
        )?);
        assert_eq!(
            snapshots()?,
            before,
            "recomputation never changes immutable ancestry"
        );
        assert_eq!(
            target.ingest_events(&net.network, &order)?.statuses,
            vec![IngestStatus::Duplicate; order.len()]
        );
        let after: Vec<(String, String, String)> = target
            .connection
            .prepare("SELECT hash,causal_chain,causal_clock FROM events ORDER BY hash")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        assert_eq!(
            after, before,
            "duplicates do not allocate chains or replace clocks"
        );
        let state: String = target.connection.query_row(
            "SELECT state FROM workspace_projection WHERE network_id=?",
            [&net.network],
            |row| row.get(0),
        )?;
        let state: Value = serde_json::from_str(&state)?;
        if let Some(expected) = &projected {
            assert_eq!(
                &state, expected,
                "local chain allocation does not affect shared projection"
            );
        } else {
            projected = Some(state);
        }
    }
    Ok(())
}

#[test]
fn a_forked_log_stops_counting_and_the_conversation_stays_appendable() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    bot_conversation(&mut origin, &net)?;
    let prelude = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
    let created = prelude.last().context("Bot prelude")?.clone();
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
        seq: seq_of(&created)? + 1,
        prev: Some(created.hash.clone()),
        deps: vec![],
        scope_id: "c".into(),
        created_at_ms: 2,
        body: user(&net, "fork-input", "forked"),
        blobs: vec![],
    })?;
    let after_fork = net.a.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.a.device_id()?,
        seq: seq_of(&created)? + 2,
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
            .ingest_events(
                &net.network,
                &[prelude, vec![kept, fork, after_fork]].concat()
            )?
            .statuses,
        vec![
            IngestStatus::Accepted,
            IngestStatus::Accepted,
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
fn a_forked_log_converges_and_a_revocation_picks_a_branch() -> Result<()> {
    let fleet = fleet()?;
    let net = &fleet.net;
    let mut first = store(net)?;
    let prelude = created(&mut first, net, &fleet.x)?;
    let x1 = prelude.last().context("Bot prelude")?.clone();
    let x2 = write(&mut first, net, &fleet.x, "x2", 2)?;
    let x3a = write(&mut first, net, &fleet.x, "x3a", 3)?;
    let w1 = write(&mut first, net, &net.b, "w1", 4)?;
    let x4a = write(&mut first, net, &fleet.x, "x4a", 5)?;
    let mut second = store(net)?;
    second.ingest_events(&net.network, &[prelude.clone(), vec![x2.clone()]].concat())?;
    let x3b = write(&mut second, net, &fleet.x, "x3b", 3)?;
    let x4b = write(&mut second, net, &fleet.x, "x4b", 4)?;
    assert!(
        serde_json::from_str::<Event>(&w1.canonical_body)?
            .deps
            .contains(&x3a.hash)
    );
    let base = Piece::Events([prelude.clone(), vec![x2.clone()]].concat());
    let branch_a = Piece::Events(vec![x3a.clone(), w1.clone(), x4a.clone()]);
    let branch_b = Piece::Events(vec![x3b.clone(), x4b.clone()]);
    let all = [&x1, &x2, &x3a, &x4a, &x3b, &x4b, &w1];

    let a_only = replay(net, &[base, branch_a], &[0, 1])?;
    assert_eq!(
        effective(&a_only, net, &all[..4])?,
        vec![true, true, true, true]
    );
    let base = Piece::Events([prelude.clone(), vec![x2.clone()]].concat());
    let branch_a = Piece::Events(vec![x3a.clone(), w1.clone(), x4a.clone()]);
    let b_only = replay(net, &[base, branch_b], &[0, 1])?;
    assert_eq!(effective(&b_only, net, &[&x3b, &x4b])?, vec![true, true]);

    let base = Piece::Events([prelude.clone(), vec![x2.clone()]].concat());
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
        Piece::Events([prelude.clone(), vec![x2.clone()]].concat()),
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
