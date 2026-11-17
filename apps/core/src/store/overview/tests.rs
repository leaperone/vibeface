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
    let measure = || -> Result<(OverviewSnapshot, usize, usize)> {
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
    let (_, before_rows, before_jobs) = measure()?;
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
    let (snapshot, after_rows, after_jobs) = measure()?;
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
