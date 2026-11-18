use super::super::test_support::{
    create_input_bot, node, pagination_reply, posted, remove_directory,
};
use super::super::*;
use crate::crypto::LocalIdentity;

#[tokio::test]
async fn diverged_logs_walk_back_to_the_fork_and_converge() -> Result<()> {
    use std::sync::Mutex;
    let (ia, ix) = (LocalIdentity::generate(), LocalIdentity::generate());
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ix.device_id()?])?;
    let network = ia.fixture_network_id()?;
    let author = ix.device_id()?;
    let replica = || -> Result<Store> {
        let mut store = Store::open(":memory:")?;
        store.install_trust_bundle(&network, &state)?;
        Ok(store)
    };
    let (mut p, mut q) = (replica()?, replica()?);
    p.create_conversation(
        &ix,
        &network,
        "c",
        "t",
        ia.owner_fixture(&network, "c", &author)?,
        1,
    )?;
    create_input_bot(&mut p, &ix, &ia.fixture_admin(), &network, "c", 1)?;
    p.append_event(
        &ix,
        &network,
        "c",
        posted(&ia, &network, "c", "x2", 1)?,
        vec![],
        2,
    )?;
    let shared = p.events_from(&network, &author, 1, 10)?;
    for (index, id) in ["a3", "a4", "a5"].into_iter().enumerate() {
        p.append_event(
            &ix,
            &network,
            "c",
            posted(&ia, &network, "c", id, index as u64 + 2)?,
            vec![],
            3,
        )?;
    }
    q.ingest_events(&network, &shared)?;
    for (index, id) in ["b3", "b4", "b5", "b6"].into_iter().enumerate() {
        q.append_event(
            &ix,
            &network,
            "c",
            posted(&ia, &network, "c", id, index as u64 + 2)?,
            vec![],
            3,
        )?;
    }
    let p = crate::store::actor::StoreActor::from_store(p)?;
    let q = crate::store::actor::StoreActor::from_store(q)?;
    let pulls = Mutex::new(0);
    let fetch = async |to: &crate::store::actor::StoreActor,
                       from: &crate::store::actor::StoreActor|
           -> Result<bool> {
        let their = {
            let network = network.clone();
            let author = author.clone();
            from.call("fork_fixture.source_head", move |store| {
                Ok(store.heads(&network)?[&author].clone())
            })
            .await?
        };
        let our_max = {
            let network = network.clone();
            let author = author.clone();
            to.call("fork_fixture.target_head", move |store| {
                Ok(store.heads(&network)?[&author].0)
            })
            .await?
        };
        reconcile_log(
            our_max,
            (their.0, &their.1),
            |hash| {
                let network = network.clone();
                async move {
                    to.call("fork_fixture.event", move |store| {
                        Ok(store.event(&network, &hash)?.is_some())
                    })
                    .await
                }
            },
            |seq, limit| {
                *pulls.lock().unwrap() += 1;
                let network = network.clone();
                let author = author.clone();
                async move {
                    from.call("fork_fixture.events", move |store| {
                        store.events_from(&network, &author, seq, limit)
                    })
                    .await
                }
            },
            |events| {
                let network = network.clone();
                async move {
                    to.call("fork_fixture.ingest", move |store| {
                        store.ingest_events(&network, &events)?;
                        Ok(())
                    })
                    .await
                }
            },
        )
        .await
    };
    assert!(fetch(&q, &p).await?);
    assert!(
        *pulls.lock().unwrap() <= 4,
        "walk-back probes in doubling steps"
    );
    assert!(fetch(&p, &q).await?);
    assert!(!fetch(&q, &p).await? && !fetch(&p, &q).await?);
    let evidence = async |store: &crate::store::actor::StoreActor| -> Result<(Vec<String>, Vec<String>, String)> {
        let network = network.clone();
        store.call("fork_fixture.evidence", move |store| {
            let hashes = |sql: &str| -> Result<Vec<String>> {
                Ok(store.connection.prepare(sql)?
                    .query_map([&network], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?)
            };
            Ok((
                hashes("SELECT hash FROM events WHERE network_id=? ORDER BY hash")?,
                hashes("SELECT hash FROM events WHERE network_id=? AND effective=1 ORDER BY hash")?,
                store.fork_digest(&network)?,
            ))
        }).await
    };
    let (stored, effective, forks) = evidence(&p).await?;
    assert_eq!((stored.len(), effective.len()), (11, 4));
    assert!(!forks.is_empty());
    assert_eq!(evidence(&q).await?, (stored, effective, forks));
    p.shutdown().await?;
    q.shutdown().await?;
    Ok(())
}

/// Fork evidence longer than one round's pages arrives over the following rounds, which run
/// right away.
#[tokio::test(flavor = "multi_thread")]
async fn fork_evidence_past_one_rounds_pages_arrives_in_the_next_rounds() -> Result<()> {
    let [il, ic] = [LocalIdentity::generate(), LocalIdentity::generate()];
    let mut forked = [LocalIdentity::generate(), LocalIdentity::generate()];
    forked.sort_by_key(|identity| identity.device_id().unwrap());
    let [f1, f2] = &forked;
    let device = |id: &LocalIdentity| id.device_id().unwrap();
    let state =
        il.sign_network_fixture(1, vec![device(&il), device(&ic), device(f1), device(f2)])?;
    let network = il.fixture_network_id()?;
    let (l, dir_l) = node(&il).await?;
    let (c, dir_c) = node(&ic).await?;
    let scratch_dir = dir_l.with_extension("other-branch");
    let mut other = crate::store::Store::open(scratch_dir.join("db.sqlite"))?;
    let result = async {
        for node in [&l, &c] {
            node.install_membership(state.clone()).await?;
        }
        other.install_trust_bundle(&network, &state)?;
        let long = MAX_FORK_PAGES * crate::store::MAX_PULL_EVENTS + 100;
        {
            let il = il.clone();
            let f1 = f1.clone();
            let f2 = f2.clone();
            let network = network.clone();
            l.store
                .call("peers.fixture", move |store| {
                    store.create_conversation(
                        &il,
                        &network,
                        "c",
                        "Fork",
                        il.owner_fixture(&network, "c", &device(&il))?,
                        0,
                    )?;
                    create_input_bot(store, &il, &il.fixture_admin(), &network, "c", 0)?;
                    other.ingest_events(
                        &network,
                        &store.events_from(&network, &device(&il), 1, 10)?,
                    )?;
                    for i in 0..long {
                        let id = format!("f{i}");
                        // Only the first event needs an input graph; the rest fill fork pages.
                        let body = if i == 0 {
                            posted(&il, &network, "c", &id, 1)?
                        } else {
                            pagination_reply(id)
                        };
                        store.append_event(&f1, &network, "c", body, vec![], i as i64)?;
                    }
                    store.append_event(
                        &f2,
                        &network,
                        "c",
                        posted(&il, &network, "c", "g", 1)?,
                        vec![],
                        0,
                    )?;
                    for (author, id) in [(&f1, "f-other"), (&f2, "g-other")] {
                        let branch = other.append_event(
                            author,
                            &network,
                            "c",
                            posted(&il, &network, "c", id, 1)?,
                            vec![],
                            0,
                        )?;
                        store.ingest_events(&network, &[branch])?;
                    }

                    Ok(())
                })
                .await?
        }
        let digest = {
            let arg0 = (network).to_owned();
            l.store
                .call("peers.fork_digest", move |store| store.fork_digest(&arg0))
                .await
        }?;
        assert!(!digest.is_empty(), "L holds both forks");
        for node in [&l, &c] {
            node.start_network().await?;
        }
        c.net()?.learn(&l.net()?.addr()).await;
        c.wake_sync(&il.device_id()?);
        tokio::time::timeout(Duration::from_secs(240), async {
            while {
                let arg0 = (network).to_owned();
                c.store
                    .call("peers.fork_digest", move |store| store.fork_digest(&arg0))
                    .await
            }? != digest
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            anyhow::Ok(())
        })
        .await
        .context("C holds all of L's fork evidence")??;
        anyhow::Ok(())
    }
    .await;
    let mut cleanup_errors = Vec::new();
    if let Err(error) = remove_directory(&scratch_dir)
        .with_context(|| format!("remove fork branch fixture {}", scratch_dir.display()))
    {
        cleanup_errors.push(format!("{error:#}"));
    }
    let retired_l = Arc::downgrade(&l);
    let retired_c = Arc::downgrade(&c);
    l.shutdown().await;
    c.shutdown().await;
    drop(l);
    drop(c);
    if retired_l.upgrade().is_some() {
        cleanup_errors
            .push("first fork fixture Runtime still has a strong owner after shutdown".into());
    } else if let Err(error) = std::fs::remove_dir_all(&dir_l)
        .with_context(|| format!("remove first fork fixture directory {}", dir_l.display()))
    {
        cleanup_errors.push(format!("{error:#}"));
    }
    if retired_c.upgrade().is_some() {
        cleanup_errors
            .push("second fork fixture Runtime still has a strong owner after shutdown".into());
    } else if let Err(error) = std::fs::remove_dir_all(&dir_c)
        .with_context(|| format!("remove second fork fixture directory {}", dir_c.display()))
    {
        cleanup_errors.push(format!("{error:#}"));
    }
    let cleanup = if cleanup_errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(cleanup_errors.join("; ")))
    };
    match (result, cleanup) {
        (Err(body), Err(cleanup)) => Err(body.context(format!("cleanup also failed: {cleanup:#}"))),
        (Err(body), Ok(())) => Err(body),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Ok(()), Ok(())) => Ok(()),
    }
}
