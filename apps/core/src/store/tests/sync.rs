use super::super::test_support::{
    Net, bot, bot_conversation, fleet, net, owner, revoke, signed_user, store, user, write,
};
use super::super::*;

type Projection = (Vec<(String, String)>, Vec<String>, Option<Conversation>);

fn projection(store: &Store, net: &Net, scope: &str) -> Result<Projection> {
    let messages = store
        .messages(&net.network, scope)?
        .into_iter()
        .map(|m| (m.message_id, m.text))
        .collect();
    let order = context_after(
        &store.connection,
        &net.network,
        scope,
        None,
        &VersionVector::new(),
    )?
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
    bot_conversation(&mut origin, &net)?;
    let mut a_events = origin.events_from(&net.network, &net.a.device_id()?, 1, 100)?;
    let mut b_events = vec![];
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
        user(&net, "same", "same signed intent"),
        vec![],
        50,
    )?);
    assert!(
        origin
            .append_event(
                &net.b,
                &net.network,
                "c",
                user(&net, "same", "changed signed intent"),
                vec![],
                50,
            )
            .is_err()
    );
    b_events.push(origin.append_event(
        &net.b,
        &net.network,
        "c",
        user(&net, "same", "same signed intent"),
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
    origin.append_event(
        &net.a,
        &net.network,
        "c",
        owner(&net, &net.a, 1)?,
        vec![],
        1,
    )?;
    origin.append_event(&net.a, &net.network, "c", bot(&net, "Helper")?, vec![], 1)?;
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

    let dir = std::env::temp_dir().join(format!("vibeface-pending-{}", uuid::Uuid::new_v4()));
    let path = dir.join("node.sqlite3");
    let mut waiting = Store::open(&path)?;
    waiting.install_trust_bundle(&net.network, &net.state)?;
    let child = net.b.sign_event(Event {
        version: WIRE_VERSION,
        network_id: net.network.clone(),
        author: net.b.device_id()?,
        seq: 1,
        prev: None,
        deps: vec![first.hash.clone()],
        scope_id: "child".into(),
        created_at_ms: 2,
        body: EventBody::ConversationCreated {
            title: "waiting".into(),
        },
        blobs: vec![],
    })?;
    assert_eq!(
        waiting
            .ingest_events(&net.network, std::slice::from_ref(&child))?
            .statuses,
        [IngestStatus::Pending]
    );
    let mut forged_parent = first.clone();
    forged_parent.signature = "00".repeat(64);
    assert!(matches!(
        waiting
            .ingest_events(&net.network, &[forged_parent])?
            .statuses[0],
        IngestStatus::Rejected(_)
    ));
    drop(waiting);
    let mut waiting = Store::open(&path)?;
    let placed = waiting.ingest_events(&net.network, std::slice::from_ref(&first))?;
    assert_eq!(placed.statuses, [IngestStatus::Accepted]);
    assert_eq!(
        placed
            .stored
            .iter()
            .map(|event| event.hash.as_str())
            .collect::<Vec<_>>(),
        [first.hash.as_str(), child.hash.as_str()]
    );
    assert!(waiting.work_records()?.is_empty());
    assert!(waiting.scheduler_jobs("child")?.is_empty());
    drop(waiting);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn pending_capacity_is_reserved_for_verified_authorized_authors() -> Result<()> {
    let mut net = net()?;
    let mut nodes = vec![net.a.device_id()?, net.b.device_id()?];
    for _ in nodes.len()..crate::identity::MAX_MEMBERS - 1 {
        nodes.push(LocalIdentity::generate().device_id()?);
    }
    let share = log::MAX_PENDING_EVENTS / nodes.len() as i64;
    net.state = net.a.sign_network_fixture(1, nodes)?;
    let mut store = store(&net)?;
    let missing = crate::crypto::digest(b"not yet received");
    let pending = |author: &LocalIdentity, at: i64| -> Result<SignedEvent> {
        author.sign_event(Event {
            version: WIRE_VERSION,
            network_id: net.network.clone(),
            author: author.device_id()?,
            seq: 1,
            prev: None,
            deps: vec![missing.clone()],
            scope_id: format!("waiting-{at}"),
            created_at_ms: at,
            body: EventBody::ConversationCreated {
                title: "waiting".into(),
            },
            blobs: vec![],
        })
    };
    for index in 0..share {
        assert_eq!(
            store
                .ingest_events(&net.network, &[pending(&net.a, index)?])?
                .statuses,
            [IngestStatus::Pending]
        );
    }
    assert!(matches!(
        store
            .ingest_events(&net.network, &[pending(&net.a, share)?])?
            .statuses[0],
        IngestStatus::Rejected(_)
    ));
    let outsider = LocalIdentity::generate();
    assert!(
        matches!(store.ingest_events(&net.network, &[pending(&outsider, share + 1)?])?.statuses[0], IngestStatus::Rejected(ref why) if why.contains("unknown author"))
    );
    let mut forged = pending(&net.b, share + 2)?;
    forged.signature = "00".repeat(64);
    assert!(matches!(
        store.ingest_events(&net.network, &[forged])?.statuses[0],
        IngestStatus::Rejected(_)
    ));
    assert_eq!(
        store
            .ingest_events(&net.network, &[pending(&net.b, share + 3)?])?
            .statuses,
        [IngestStatus::Pending]
    );
    let counts: (i64, i64, i64) = store.connection.query_row(
        "SELECT count(*),count(*) FILTER (WHERE author=?1),count(*) FILTER (WHERE author=?2) FROM pending_events",
        params![net.a.device_id()?, net.b.device_id()?],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(counts, (share + 1, share, 1));
    Ok(())
}

#[test]
fn a_rebuild_never_touches_local_ledgers() -> Result<()> {
    let fleet = fleet()?;
    let net = &fleet.net;
    let mut store = store(net)?;
    bot_conversation(&mut store, net)?;
    store.post_input_fixture(&net.a, &net.network, signed_user(net, "m1", "hi", true)?, 2)?;
    write(&mut store, net, &fleet.x, "x1", 3)?;
    let ledgers = |store: &Store| -> Result<Vec<String>> {
        let shared_or_query_cache = [
            "events",
            "event_blobs",
            "log_heads",
            "pending_events",
            "networks",
            // Rebuilds advance this local read-cache epoch to invalidate old cursors.
            // It is not an execution, scheduling, identity or side-effect ledger.
            "conversation_history_revisions",
        ];
        let tables: Vec<String> = store
            .connection
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut rows = Vec::new();
        for table in tables.iter().filter(|t| {
            !reduce::TABLES.contains(&t.as_str()) && !shared_or_query_cache.contains(&t.as_str())
        }) {
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
