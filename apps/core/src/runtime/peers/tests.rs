use super::test_support::{create_input_bot, node, pagination_reply, posted, stop_nodes};
use super::*;
use crate::crypto::LocalIdentity;

mod address_exchange;
mod alignment;
mod attachments;
mod discovery;
mod forks;
mod input_delivery;
mod lifecycle;
mod membership;
mod recovery;
mod relay;
mod sync;
mod work_delivery;

// The slow-test runner selects this exact module path.
/// A round that leaves events waiting on other authors past the pending limit is followed
/// by another one right away instead of the next Hello a minute later.
#[tokio::test(flavor = "multi_thread")]
async fn a_round_left_behind_pulls_again_without_waiting_for_hello() -> Result<()> {
    let [il, ic, ih, ij] = [
        LocalIdentity::generate(),
        LocalIdentity::generate(),
        LocalIdentity::generate(),
        LocalIdentity::generate(),
    ];
    let device = |id: &LocalIdentity| id.device_id().unwrap();
    let state =
        il.sign_network_fixture(1, vec![device(&il), device(&ic), device(&ih), device(&ij)])?;
    let network = il.fixture_network_id()?;
    let (l, dir_l) = node(&il).await?;
    let (c, dir_c) = node(&ic).await?;
    let result = async {
        for node in [&l, &c] {
            node.install_membership(state.clone()).await?;
        }
        {
            let il = il.clone();
            let ih = ih.clone();
            let ij = ij.clone();
            let network = network.clone();
            l.store
                .call("peers.catch_up_fixture", move |store| {
                    store.create_conversation(
                        &il,
                        &network,
                        "c",
                        "Catch up",
                        il.owner_fixture(&network, "c", &device(&il))?,
                        0,
                    )?;
                    create_input_bot(store, &il, &il.fixture_admin(), &network, "c", 0)?;
                    for i in 0..40_000 {
                        let author = if i % 2 == 0 { &ih } else { &ij };
                        let id = format!("m{i}");
                        let body = if i < 2 {
                            posted(&il, &network, "c", &id, 1)?
                        } else {
                            pagination_reply(id)
                        };
                        store.append_event(author, &network, "c", body, vec![], i)?;
                    }
                    Ok(())
                })
                .await?
        }
        let target = {
            let arg0 = (network).to_owned();
            l.store
                .call("peers.heads", move |store| store.heads(&arg0))
                .await
        }?;
        let peer = device(&il);
        // Anything that wakes C during its round would also start the next one at once: L's
        // own Hello, or an address record C has not seen. Only C's schedule decides here.
        l.sync_peers
            .lock()
            .expect("sync peers")
            .entry(device(&ic))
            .or_default()
            .running = true;
        c.sync_peers
            .lock()
            .expect("sync peers")
            .entry(peer.clone())
            .or_default()
            .running = true;
        for node in [&l, &c] {
            node.start_network().await?;
        }
        c.net()?.learn(&l.net()?.addr()).await;
        let book = async |node: &Runtime| -> Result<Vec<(String, u64)>> {
            Ok(node
                .store
                .call("peers.address_book", move |store| store.address_book())
                .await?
                .iter()
                .filter_map(|signed| signed.open(&network).ok())
                .map(|record| (record.device_id, record.seq))
                .collect())
        };
        let mut settled = Vec::new();
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let now = vec![book(&l).await?, book(&c).await?];
                if now.iter().all(|records| records.len() == 1) && now == settled {
                    break anyhow::Ok(());
                }
                settled = now;
                tokio::time::sleep(Duration::from_secs(6)).await;
            }
        })
        .await
        .context("the address books settle")??;
        let records = l
            .store
            .call("peers.address_book", move |store| store.address_book())
            .await?;
        c.merge_addresses(&network, records).await?;
        c.sync_peers
            .lock()
            .expect("sync peers")
            .entry(peer.clone())
            .or_default()
            .running = false;
        c.wake_sync(&peer);
        let synced = || {
            c.sync_peers
                .lock()
                .expect("sync peers")
                .get(&peer)
                .and_then(|s| s.synced_ms)
        };
        tokio::time::timeout(Duration::from_secs(300), async {
            while synced().is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("the first round ends")?;
        let first = {
            let arg0 = (network).to_owned();
            c.store
                .call("peers.heads", move |store| store.heads(&arg0))
                .await
        }?;
        assert_ne!(first, target, "one round stops at the pending limit");
        tokio::time::timeout(Duration::from_secs(10), async {
            while {
                let arg0 = (network).to_owned();
                c.store
                    .call("peers.heads", move |store| store.heads(&arg0))
                    .await
            }? == first
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            anyhow::Ok(())
        })
        .await
        .context("C pulls again right after a round that left it behind")??;
        tokio::time::timeout(Duration::from_secs(300), async {
            while {
                let arg0 = (network).to_owned();
                c.store
                    .call("peers.heads", move |store| store.heads(&arg0))
                    .await
            }? != target
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            anyhow::Ok(())
        })
        .await
        .context("C catches up")??;
        anyhow::Ok(())
    }
    .await;
    stop_nodes(vec![(l, dir_l), (c, dir_c)]).await?;
    result
}
