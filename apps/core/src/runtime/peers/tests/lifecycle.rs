use super::super::test_support::{create_input_bot, first_sync, node, posted, stop_nodes};
use super::super::*;
use crate::crypto::LocalIdentity;

#[tokio::test]
async fn shutdown_releases_runtime_with_an_active_peer_sync_job() -> Result<()> {
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = node(&ib).await?;
    let peer = ib.device_id()?;
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, peer.clone()])?;
    let result = async {
        for node in [&a, &b] {
            node.install_membership(state.clone()).await?;
            node.start_network().await?;
        }
        a.net()?.learn(&b.net()?.addr()).await;
        first_sync(&a, &peer).await?;
        ensure!(
            a.sync_peers
                .lock()
                .expect("sync peers")
                .get(&peer)
                .is_some_and(|sync| sync.synced_ms.is_some()),
            "the peer sync completed before shutdown"
        );
        anyhow::Ok(())
    }
    .await;
    let weak = Arc::downgrade(&a);
    a.stop_network().await;
    a.shutdown().await;
    a.shutdown().await;
    b.shutdown().await;
    drop(a);
    drop(b);

    assert!(
        weak.upgrade().is_none(),
        "shutdown releases active peer loop owners"
    );
    std::fs::remove_dir_all(&dir_a).with_context(|| {
        format!(
            "remove first active-peer fixture directory {}",
            dir_a.display()
        )
    })?;
    std::fs::remove_dir_all(&dir_b).with_context(|| {
        format!(
            "remove second active-peer fixture directory {}",
            dir_b.display()
        )
    })?;
    result
}

#[tokio::test]
async fn shutdown_releases_runtime_after_controlled_writer_start() -> Result<()> {
    let identity = LocalIdentity::generate();
    let (node, dir) = node(&identity).await?;
    assert!(
        node.manager().is_none(),
        "fixture has no configured Provider"
    );
    let (events, receiver) = vibeface_bridge::codex::event_channel();
    node.start_writers(receiver);
    let weak = Arc::downgrade(&node);

    node.shutdown().await;
    node.shutdown().await;
    drop(node);

    assert!(
        weak.upgrade().is_none(),
        "shutdown releases all Runtime writer workers"
    );
    drop(events);
    let reopened = Runtime::open(&dir)
        .await
        .context("reopen a fixture after all Runtime writer workers stopped")?;
    reopened.shutdown().await;
    drop(reopened);
    std::fs::remove_dir_all(&dir)
        .with_context(|| format!("remove writer-start fixture directory {}", dir.display()))?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_joins_a_queued_peer_command_worker_and_reopens_the_data_dir() -> Result<()> {
    let (sender, owner) = (LocalIdentity::generate(), LocalIdentity::generate());
    let (sender_id, owner_id) = (sender.device_id()?, owner.device_id()?);
    let network = sender.fixture_network_id()?;
    let state = sender.sign_network_fixture(1, vec![sender_id.clone(), owner_id.clone()])?;
    let (node, dir) = node(&owner).await?;
    let weak = Arc::downgrade(&node);
    let result = async {
        node.install_membership(state).await?;
        {
            let sender = sender.clone();
            let network = network.clone();
            let owner_id = owner_id.clone();
            let sender_id = sender_id.clone();
            node.store
                .call("peers.fixture", move |store| {
                    let grant = sender.owner_fixture(&network, "c", &owner_id)?;
                    store.create_conversation(
                        &sender,
                        &network,
                        "c",
                        "peer shutdown",
                        grant,
                        now_ms(),
                    )?;
                    create_input_bot(
                        store,
                        &sender,
                        &sender.fixture_admin(),
                        &network,
                        "c",
                        now_ms(),
                    )?;
                    store.append_event(
                        &sender,
                        &network,
                        "c",
                        posted(&sender, &network, "c", "peer-shutdown-input", 1)?,
                        vec![],
                        now_ms(),
                    )?;
                    store.post_input_fixture(
                        &sender,
                        &network,
                        posted(&sender, &network, "c", "peer-shutdown-input", 1)?,
                        now_ms(),
                    )?;
                    let command = store.peer_delivery_command(
                        &network,
                        "c",
                        "message:peer-shutdown-input",
                    )?;
                    store.save_peer_command(&network, &sender_id, &command.envelope, now_ms())?;

                    Ok(())
                })
                .await?
        }
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let barrier_node = node.clone();
        let blocked = tokio::spawn(async move {
            barrier_node
                .store
                .call("peers.shutdown_barrier", move |_store| {
                    let _ = entered_tx.send(());
                    release_rx.recv().context("release peer shutdown barrier")?;
                    Ok(())
                })
                .await
        });
        entered_rx.await?;
        let owners_before = Arc::strong_count(&node);
        let queued = node.clone();
        let queued_network = network.clone();
        node.background_tasks.spawn(async move {
            if let Err(error) = queued.drive_peer_commands(&queued_network).await {
                tracing::debug!(%error, "peer command stopped during shutdown");
            }
        })?;
        tokio::task::yield_now().await;
        assert!(
            Arc::strong_count(&node) > owners_before,
            "queued peer command worker retains the Runtime until shutdown joins it"
        );
        let stopping = node.clone();
        let shutdown = tokio::spawn(async move { stopping.shutdown().await });
        tokio::task::yield_now().await;
        release_tx
            .send(())
            .context("release StoreActor for shutdown")?;
        blocked.await??;
        shutdown.await?;
        anyhow::Ok(())
    }
    .await;
    drop(node);
    ensure!(
        weak.upgrade().is_none(),
        "shutdown must join the queued peer command worker"
    );
    let reopened = Runtime::open(&dir)
        .await
        .context("reopen fixture after peer command worker shutdown")?;
    reopened.shutdown().await;
    drop(reopened);
    std::fs::remove_dir_all(&dir).with_context(|| {
        format!(
            "remove peer command shutdown fixture directory {}",
            dir.display()
        )
    })?;
    result
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sync_policy_redial_preserves_active_streams_and_other_protocols() -> Result<()> {
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
    let network = ia.fixture_network_id()?;
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = node(&ib).await?;
    let result = async {
        for (node, peer) in [(&a, &ib), (&b, &ia)] {
            node.install_membership(state.clone()).await?;
            node.sync_peers
                .lock()
                .unwrap()
                .entry(peer.device_id()?)
                .or_default()
                .running = true;
            node.start_network().await?;
        }
        let peer = ia.device_id()?;
        let net = b.net()?.clone();
        net.learn(&a.net()?.addr()).await;
        let sync = net.connect(&peer, net::SYNC).await?;
        let rpc = net.connect(&peer, net::RPC).await?;
        let (mut send, mut recv) = sync.open_bi().await?;
        net.forget_sync_connection(&peer);
        assert!(
            sync.close_reason().is_none(),
            "an active sync stream must finish"
        );
        assert!(
            rpc.close_reason().is_none(),
            "another protocol must stay connected"
        );
        net::write_frame(&mut send, &b.hello(&network).await?).await?;
        send.finish()?;
        let reply = tokio::time::timeout(PEER_TIMEOUT, net::read_frame(&mut recv))
            .await??
            .context("the active sync stream returned no reply")?;
        assert_eq!(reply["network_id"].as_str(), Some(network.as_str()));
        assert_ne!(
            net.connect(&peer, net::SYNC).await?.stable_id(),
            sync.stable_id()
        );
        assert_eq!(
            net.connect(&peer, net::RPC).await?.stable_id(),
            rpc.stable_id()
        );
        anyhow::Ok(())
    }
    .await;
    stop_nodes(vec![(a, dir_a), (b, dir_b)]).await?;
    result
}
