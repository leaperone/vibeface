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

#[tokio::test(flavor = "current_thread")]
async fn overview_snapshot_keeps_one_wal_view_when_projection_callback_commits() -> Result<()> {
    use crate::scheduling::JobState;

    let directory =
        std::env::temp_dir().join(format!("vibeface-overview-wal-{}", uuid::Uuid::new_v4()));
    crate::config::private_dir(&directory)?;
    let path = directory.join("node.sqlite3");
    let identity = LocalIdentity::generate();
    let network = identity.fixture_network_id()?;
    let local = identity.device_id()?;
    let state = identity.sign_network_fixture(1, vec![local.clone()])?;
    let expected_epoch = state.project()?.epoch;
    let fixture_network = network.clone();
    let fixture_local = local.clone();
    let lock = Arc::new(crate::config::lock_data_dir(&directory)?);
    let actor = StoreActor::open(path.clone(), lock, move |store| {
        store.install_trust_bundle(&fixture_network, &state)?;
        store.connection.execute(
            "INSERT INTO conversations(network_id,conversation_id,title,owner,owner_epoch)
             VALUES(?1,'a','A before',?2,7),(?1,'b','B before',?2,9)",
            params![fixture_network, fixture_local],
        )?;
        store.connection.execute(
            "INSERT INTO bots(network_id,conversation_id,bot_id,name,instructions,event_hash,rank)
             VALUES(?1,'a','bot-a','Bot before','','hash-a','rank'),
                   (?1,'b','bot-b','Bot B','','hash-b','rank')",
            [&fixture_network],
        )?;
        store.connection.execute(
            "INSERT INTO scheduler_jobs(conversation_id,trigger,kind,sequence,basis,reply_to,message_id,state,tool_calls)
             VALUES('a','message:before','human',0,'{}','input','reply-before','queued',0)",
            [],
        )?;
        store.connection.execute(
            "INSERT INTO pending_checks(attempt_id,conversation_id,trigger,detail,created_at_ms)
             VALUES('check-before','a','message:before','fixture',1)",
            [],
        )?;
        store.connection.execute(
            "INSERT INTO log_heads(network_id,author,max_seq,head_hash) VALUES(?1,?2,1,'head-before')",
            params![fixture_network, fixture_local],
        )?;
        Ok(())
    })
    .await?;

    let result = async {
        let query_network = network.clone();
        let query_local = local.clone();
        let before = actor
            .read("overview_wal_committing_projection", move |store| {
                // Use the production actor's read transaction, not a transaction owned by
                // overview_snapshot or the test. The callback runs after row/count selection
                // and job hydration, but before the final head frontier is read.
                assert!(!store.connection.is_autocommit());
                let mut writer = Connection::open(path)?;
                writer.busy_timeout(std::time::Duration::ZERO)?;
                let mode: String =
                    writer.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
                assert_eq!(mode, "wal");
                let mut callbacks = 0;
                let snapshot = store.overview_snapshot(
                    OverviewQuery {
                        network: &query_network,
                        scope: None,
                        local: &query_local,
                        limit: 1,
                        include_finished: false,
                        include_sessions: false,
                    },
                    |store, job, owner_epoch, pending, _, _| {
                        callbacks += 1;
                        assert_eq!(callbacks, 1);
                        assert_eq!(job.conversation_id, "a");
                        assert_eq!(job.trigger, "message:before");
                        assert_eq!(job.state, JobState::Queued);
                        assert!(job.pending_check);
                        assert!(pending);
                        assert_eq!(owner_epoch, 7);

                        let transaction = writer.transaction()?;
                        assert_eq!(transaction.execute(
                            "UPDATE conversations SET title='A after',owner_epoch=8
                             WHERE network_id=?1 AND conversation_id='a'",
                            [&query_network],
                        )?, 1);
                        assert_eq!(transaction.execute(
                            "UPDATE bots SET name='Bot after' WHERE network_id=?1 AND conversation_id='a'",
                            [&query_network],
                        )?, 1);
                        transaction.execute(
                            "INSERT INTO conversations(network_id,conversation_id,title,owner)
                             VALUES(?1,'c','C after',?2)",
                            params![query_network, query_local],
                        )?;
                        transaction.execute(
                            "INSERT INTO bots(network_id,conversation_id,bot_id,name,instructions,event_hash,rank)
                             VALUES(?1,'c','bot-c','Bot C','','hash-c','rank')",
                            [&query_network],
                        )?;
                        assert_eq!(transaction.execute(
                            "UPDATE scheduler_jobs SET state='cancelled',detail='writer committed'
                             WHERE conversation_id='a' AND trigger='message:before'",
                            [],
                        )?, 1);
                        transaction.execute(
                            "INSERT INTO scheduler_jobs(conversation_id,trigger,kind,sequence,basis,reply_to,message_id,state,tool_calls)
                             VALUES('b','message:after','human',1,'{}','input','reply-after','queued',0)",
                            [],
                        )?;
                        assert_eq!(transaction.execute("DELETE FROM pending_checks", [])?, 1);
                        assert_eq!(transaction.execute(
                            "UPDATE log_heads SET max_seq=2,head_hash='head-after'
                             WHERE network_id=?1 AND author=?2",
                            params![query_network, query_local],
                        )?, 1);
                        transaction
                            .commit()
                            .context("external WAL writer must commit while the overview read is open")?;
                        assert!(writer.is_autocommit());
                        let committed_title: String = writer.query_row(
                            "SELECT title FROM conversations WHERE network_id=?1 AND conversation_id='a'",
                            [&query_network],
                            |row| row.get(0),
                        )?;
                        assert_eq!(committed_title, "A after");

                        // Re-read through the production Store after the independent commit:
                        // neither canonical job reads nor fresh SQL may cross the WAL view.
                        let reread = store
                            .scheduler_job("a", "message:before")?
                            .context("original job remains in the read snapshot")?;
                        assert_eq!(reread.state, JobState::Queued);
                        assert!(reread.pending_check);
                        assert!(store.scheduler_job("b", "message:after")?.is_none());
                        assert!(!store.has_conversation(&query_network, "c")?);
                        let facts: (String, u64, String, usize) = store.connection.query_row(
                            "SELECT c.title,c.owner_epoch,b.name,
                                (SELECT count(*) FROM conversations WHERE network_id=?1)
                             FROM conversations c JOIN bots b ON b.network_id=c.network_id
                                AND b.conversation_id=c.conversation_id
                             WHERE c.network_id=?1 AND c.conversation_id='a'",
                            [&query_network],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                        )?;
                        assert_eq!(facts, ("A before".into(), 7, "Bot before".into(), 2));
                        assert_eq!(
                            store.heads(&query_network)?,
                            std::collections::BTreeMap::from([(
                                query_local.clone(),
                                (1, "head-before".into()),
                            )])
                        );
                        Ok(json!({"conversation_id":job.conversation_id,"trigger":job.trigger,
                            "state":job.state.name(),"pending":pending,"owner_epoch":owner_epoch}))
                    },
                )?;
                assert_eq!(callbacks, 1, "fixture must actually reach the commit callback");
                Ok(snapshot)
            })
            .await?;
        assert_eq!(before.network_epoch, expected_epoch);
        assert_eq!(before.conversations.total, 2);
        assert_eq!(before.conversations.omitted(), 1);
        assert_eq!(before.conversations.rows[0]["title"], "A before");
        assert_eq!(before.conversations.rows[0]["owner_epoch"], 7);
        assert_eq!(before.bots.total, 2);
        assert_eq!(before.bots.omitted(), 1);
        assert_eq!(before.bots.rows[0]["name"], "Bot before");
        assert_eq!(before.checks.total, 1);
        assert_eq!(before.checks.rows.len(), 1);
        assert_eq!(before.jobs.total, 1);
        assert_eq!(before.jobs.rows, vec![json!({"conversation_id":"a","trigger":"message:before",
            "state":"queued","pending":true,"owner_epoch":7})]);
        assert_eq!(before.heads, std::collections::BTreeMap::from([(local.clone(), 1)]));

        let expected_after_heads = std::collections::BTreeMap::from([(local.clone(), 2)]);
        let after = actor
            .read("overview_wal_after_commit", move |store| {
                let cancelled = store.scheduler_job("a", "message:before")?.context("cancelled job")?;
                assert!(matches!(cancelled.state, JobState::Cancelled { .. }));
                assert!(!cancelled.pending_check);
                assert!(store.has_conversation(&network, "c")?);
                assert_eq!(store.heads(&network)?[&local], (2, "head-after".into()));
                store.overview_snapshot(
                    OverviewQuery {
                        network: &network,
                        scope: None,
                        local: &local,
                        limit: 1,
                        include_finished: false,
                        include_sessions: false,
                    },
                    |_, job, epoch, pending, _, _| {
                        assert_eq!(job.state, JobState::Queued);
                        assert!(!pending);
                        Ok(json!({"conversation_id":job.conversation_id,"trigger":job.trigger,
                            "owner_epoch":epoch}))
                    },
                )
            })
            .await?;
        assert_eq!(after.network_epoch, expected_epoch);
        assert_eq!(after.conversations.total, 3);
        assert_eq!(after.conversations.omitted(), 2);
        assert_eq!(after.conversations.rows[0]["title"], "A after");
        assert_eq!(after.conversations.rows[0]["owner_epoch"], 8);
        assert_eq!(after.bots.total, 3);
        assert_eq!(after.bots.omitted(), 2);
        assert_eq!(after.bots.rows[0]["name"], "Bot after");
        assert_eq!(after.checks.total, 0);
        assert!(after.checks.rows.is_empty());
        assert_eq!(after.jobs.total, 1);
        assert_eq!(after.jobs.rows, vec![json!({"conversation_id":"b","trigger":"message:after",
            "owner_epoch":9})]);
        assert_eq!(after.heads, expected_after_heads);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    actor.shutdown().await?;
    drop(actor);
    std::fs::remove_dir_all(directory)?;
    result
}
