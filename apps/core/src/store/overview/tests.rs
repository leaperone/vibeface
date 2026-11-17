use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn limits_bound_payload_decode_even_when_precise_counts_scan_history() -> Result<()> {
    let identity = LocalIdentity::generate();
    let network = identity.fixture_network_id()?;
    let local = identity.device_id()?;
    let state = identity.sign_network_fixture(1, vec![local.clone()])?;
    let mut store = Store::open(":memory:")?;
    store.install_trust_bundle(&network, &state)?;
    store.connection.execute(
        "INSERT INTO conversations(network_id,conversation_id,title,owner) VALUES(?1,'a','A',?2)",
        params![network, local],
    )?;
    store.connection.execute("INSERT INTO bots(network_id,conversation_id,bot_id,name,instructions,event_hash,rank) VALUES(?1,'a','bot','Bot','','hash','rank')",[&network])?;
    let dispatch = json!({"type":"work_dispatched","work_id":"first","trigger":"message:input","dispatcher":local,
        "call_ref":"first","kind":"task","resolved":{"provider":"codex","model":null},"cwd":"/fixture","title":"First","prompt":"private prompt"});
    store.connection.execute(
        "INSERT INTO works(network_id,work_id,conversation_id,target_node,dispatch,dispatch_rank)
        VALUES(?1,'first','a',?2,?3,'0000')",
        params![network, local, dispatch.to_string()],
    )?;
    store.connection.execute("INSERT INTO scheduler_jobs(conversation_id,trigger,kind,sequence,basis,reply_to,message_id,state,attempt_id,attempt_boot,attempt_permission,tool_calls)
        VALUES('a','first','human',0,'{}','input','reply','completed','attempt','boot',1,0)",[])?;
    let query = OverviewQuery {
        network: &network,
        scope: Some("a"),
        local: &local,
        limit: 1,
        include_finished: true,
        include_sessions: true,
    };
    let measure = |store: &Store| -> Result<(OverviewSnapshot, usize, usize)> {
        DECODED_ROWS.with(|count| count.set(0));
        let mut decoded_jobs = 0;
        let snapshot = store.overview_snapshot(query, |_, job, _, _, _, _| {
            decoded_jobs += 1;
            Ok(json!({"conversation_id":job.conversation_id,"trigger":job.trigger}))
        })?;
        Ok((
            snapshot,
            DECODED_ROWS.with(std::cell::Cell::get),
            decoded_jobs,
        ))
    };
    let (_, before_rows, before_jobs) = measure(&store)?;
    // These projections are deliberately impossible to hydrate. Counting and SQL selection
    // may inspect their small status/identity fields, but omitted payloads must never decode.
    for index in 0..2000 {
        let work = format!("historical-{index:05}");
        store.connection.execute("INSERT INTO works(network_id,work_id,conversation_id,target_node,dispatch,dispatch_rank,finish)
            VALUES(?1,?2,'a',?3,?4,?2,?5)",params![network,work,local,
                json!({"type":"work_dispatched","kind":"invalid omitted kind","prompt":"x".repeat(4096)}).to_string(),
                json!({"type":"work_finished","outcome":"completed","result_text":"y".repeat(4096)}).to_string()])?;
        store.connection.execute("INSERT INTO scheduler_jobs(conversation_id,trigger,kind,sequence,basis,reply_to,message_id,state,tool_calls)
            VALUES('a',?1,'human',?2,'omitted invalid basis','input',?1,'completed',0)",params![work,index+1])?;
    }
    let steps = Arc::new(AtomicUsize::new(0));
    let counted = steps.clone();
    store.connection.progress_handler(
        100,
        Some(move || {
            counted.fetch_add(100, Ordering::Relaxed);
            false
        }),
    )?;
    let (snapshot, after_rows, after_jobs) = measure(&store)?;
    store.connection.progress_handler(0, None::<fn() -> bool>)?;
    assert_eq!(snapshot.tasks.total, 2001);
    assert_eq!(snapshot.jobs.total, 2001);
    assert_eq!(snapshot.tasks.rows.len(), 1);
    assert_eq!(snapshot.jobs.rows.len(), 1);
    assert_eq!((after_rows, after_jobs), (before_rows, before_jobs));
    assert!(
        steps.load(Ordering::Relaxed) < 1_000_000,
        "unexpected repeated historical scan: {} VM steps",
        steps.load(Ordering::Relaxed)
    );
    Ok(())
}

#[test]
fn default_open_prefix_and_counts_share_newest_ended_across_large_tails() -> Result<()> {
    let identity = LocalIdentity::generate();
    let network = identity.fixture_network_id()?;
    let local = identity.device_id()?;
    let mut store = Store::open(":memory:")?;
    store.install_trust_bundle(
        &network,
        &identity.sign_network_fixture(1, vec![local.clone()])?,
    )?;
    store.connection.execute(
        "INSERT INTO conversations(network_id,conversation_id,title,owner) VALUES(?1,'a','A',?2)",
        params![network, local],
    )?;
    store.connection.execute("INSERT INTO bots(network_id,conversation_id,bot_id,name,instructions,event_hash,rank) VALUES(?1,'a','bot','Bot','','hash','rank')",[&network])?;
    let population = 2000usize;
    for index in 0..population * 2 {
        let work = format!("work-{index:05}");
        let dispatch = json!({"type":"work_dispatched","work_id":work,"trigger":"message:input","dispatcher":local,
            "call_ref":work,"kind":"task","resolved":{"provider":"codex","model":null},"cwd":"/fixture",
            "title":work,"prompt":"private","parent":"parent"});
        let outcome = if index < population {
            Some("failed")
        } else if index % 2 == 0 {
            Some("unverified")
        } else {
            None
        };
        let finish=outcome.map(|outcome|json!({"type":"work_finished","work_id":work,"outcome":outcome,"result_text":"private"}).to_string());
        store.connection.execute("INSERT INTO works(network_id,work_id,conversation_id,target_node,dispatch,dispatch_rank,finish,finish_rank)
            VALUES(?1,?2,'a',?3,?4,?5,?6,?5)",params![network,work,local,dispatch.to_string(),format!("{index:08}"),finish])?;
    }
    let query = OverviewQuery {
        network: &network,
        scope: None,
        local: &local,
        limit: 1,
        include_finished: false,
        include_sessions: false,
    };
    let steps = Arc::new(AtomicUsize::new(0));
    let counted = steps.clone();
    store.connection.progress_handler(
        100,
        Some(move || {
            counted.fetch_add(100, Ordering::Relaxed);
            false
        }),
    )?;
    let snapshot =
        store.overview_snapshot(query, |_, _, _, _, _, _| bail!("fixture has no jobs"))?;
    store.connection.progress_handler(0, None::<fn() -> bool>)?;
    assert_eq!(snapshot.tasks.total, population + 1);
    assert_eq!(snapshot.tasks.rows[0]["work_id"], "work-01999");
    assert_eq!(snapshot.omitted_finished, population);
    assert_eq!(snapshot.relations.total, population * 2);
    assert_eq!(
        snapshot.relations.rows[0],
        json!({"from_work":"work-02000","to_work":"parent","kind":"parent"})
    );
    assert!(
        steps.load(Ordering::Relaxed) < 3_000_000,
        "default overview repeated historical scans: {} VM steps",
        steps.load(Ordering::Relaxed)
    );
    // The aggregate is over all ended work: a later successful result clears the previous
    // newest failure, even while newer unfinished/unverified work remains open.
    store.connection.execute("UPDATE works SET finish=?1 WHERE work_id='work-02001'",[json!({"type":"work_finished","work_id":"work-02001","outcome":"completed","result_text":"done"}).to_string()])?;
    let snapshot =
        store.overview_snapshot(query, |_, _, _, _, _, _| bail!("fixture has no jobs"))?;
    assert_eq!(snapshot.tasks.total, population - 1);
    assert_eq!(snapshot.tasks.rows[0]["work_id"], "work-02000");
    assert_eq!(snapshot.omitted_finished, population + 1);
    assert_eq!(snapshot.relations.total, (population - 1) * 2);
    Ok(())
}
