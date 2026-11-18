use super::super::test_support::{bot_conversation, net, store, user};
use super::super::*;

#[test]
fn long_history_condenses_into_a_prefix_checkpoint() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
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
    let no_covers = VersionVector::new();
    let context = context_after(&store.connection, &net.network, "c", None, &no_covers)?;
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
    let context = context_after(&store.connection, &net.network, "c", None, &no_covers)?;
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
    let full = context_after(&store.connection, &net.network, "c", None, &no_covers)?;
    let recent = crate::store::since_checkpoint_from(&store.connection, &net.network, "c")?;
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
    bot_conversation(&mut store, &net)?;
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
    let context = crate::store::since_checkpoint_from(&store.connection, &net.network, "c")?;
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
    let recent = crate::store::since_checkpoint_from(&store.connection, &net.network, "c")?;
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
