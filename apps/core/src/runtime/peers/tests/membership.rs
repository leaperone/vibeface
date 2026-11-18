use super::super::test_support::{finish_nodes, first_sync, node, stop_nodes};
use super::super::*;
use crate::crypto::LocalIdentity;

#[tokio::test]
async fn remote_handover_distinguishes_unsent_lost_and_versioned_replies() -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Clone, Debug)]
    struct Replies {
        count: Arc<AtomicUsize>,
        body: Arc<std::sync::Mutex<Option<Value>>>,
    }
    impl ProtocolHandler for Replies {
        async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
            while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                let Ok(Some(request)) = net::read_frame(&mut recv).await else {
                    break;
                };
                assert_eq!(request["type"], "hand_over_session");
                self.count.fetch_add(1, Ordering::SeqCst);
                let frame = self.body.lock().unwrap().clone();
                if let Some(frame) = frame {
                    net::write_frame(&mut send, &frame)
                        .await
                        .expect("fixture writes receipt");
                }
                let _ = send.finish();
            }
            Ok(())
        }
    }
    let local = LocalIdentity::generate();
    let remote = LocalIdentity::generate();
    let target = remote.device_id()?;
    let network = local.fixture_network_id()?;
    let state = local.sign_network_fixture(1, vec![local.device_id()?, target.clone()])?;
    let (a, dir) = node(&local).await?;
    let result = async {
        a.install_membership(state).await?;
        let client =
            Arc::new(Net::bind(&local.secret_key_hex, &a.config.network, 0, vec![]).await?);
        ensure!(
            a.net.set(client.clone()).is_ok(),
            "fixture network starts once"
        );
        let peer = Net::bind(&remote.secret_key_hex, &a.config.network, 0, vec![]).await?;
        let replies = Replies {
            count: Default::default(),
            body: Default::default(),
        };
        let router = Router::builder(peer.endpoint.clone())
            .accept(net::RPC, replies.clone())
            .spawn();
        client.learn(&peer.addr()).await;
        let seen = "0123456789abcdef";
        let hand = async |node: &str| {
            a.hand_over_session(&network, "c", node, "thread", seen, Some("d"))
                .await
        };
        let record = |error: &anyhow::Error| {
            crate::tools::ToolFailure::new(
                "hand_over_session",
                &json!({}),
                crate::tools::FailureStage::Execution,
                error,
            )
            .response["call_record"]
                .clone()
        };
        let refused = hand(&LocalIdentity::generate().device_id()?)
            .await
            .unwrap_err();
        assert_eq!(record(&refused)["execution"], "not_started");
        assert_eq!(replies.count.load(Ordering::SeqCst), 0);
        let good = json!({"node":target,"session_id":"thread","conversation_id":"d","muted":false,
            "changed":true,"version":"abcdef0123456789"});
        let mut missing = good.clone();
        missing.as_object_mut().unwrap().remove("version");
        let mut incorrect = good.clone();
        incorrect["version"] = json!(21);
        let mut same = good.clone();
        same["version"] = json!(seen);
        for (i, frame) in [None, Some(missing), Some(incorrect), Some(same)]
            .into_iter()
            .enumerate()
        {
            *replies.body.lock().unwrap() = frame;
            let failure = hand(&target).await.unwrap_err();
            assert_eq!(record(&failure)["execution"], "unknown");
            assert_eq!(record(&failure)["retry"], "none");
            assert_eq!(replies.count.load(Ordering::SeqCst), i + 1);
        }
        *replies.body.lock().unwrap() = Some(good.clone());
        assert_eq!(
            hand(&target).await?.receipt.execution,
            crate::tools::Execution::Applied
        );
        let mut unchanged = good;
        unchanged["changed"] = json!(false);
        unchanged["version"] = json!(seen);
        *replies.body.lock().unwrap() = Some(unchanged);
        assert_eq!(
            hand(&target).await?.receipt.execution,
            crate::tools::Execution::NotStarted
        );
        router.shutdown().await?;
        Ok(())
    }
    .await;
    finish_nodes(result, vec![(a, dir)]).await
}

#[tokio::test]
async fn a_failed_hello_does_not_send_membership_to_a_nonmember() -> Result<()> {
    #[derive(Clone, Debug)]
    struct Probe(Arc<std::sync::Mutex<Vec<Value>>>);
    impl ProtocolHandler for Probe {
        async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
            let seen = self.0.clone();
            net::serve_streams(connection, move |request: Value| {
                seen.lock().unwrap().push(request);
                async { Ok(json!({"error":"fixture hello failure"})) }
            })
            .await;
            Ok(())
        }
    }
    let (identity, stranger) = (LocalIdentity::generate(), LocalIdentity::generate());
    let (node, dir) = node(&identity).await?;
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let probe = Net::bind(
        &stranger.secret_key_hex,
        &net::NetworkConfig {
            bind: Some("127.0.0.1:0".parse()?),
            ..Default::default()
        },
        0,
        vec![],
    )
    .await?;
    let router = Router::builder(probe.endpoint.clone())
        .accept(net::SYNC, Probe(seen.clone()))
        .spawn();
    let result = async {
        let network = identity.fixture_network_id()?;
        node.install_membership(identity.sign_network_fixture(1, vec![identity.device_id()?])?)
            .await?;
        node.start_network().await?;
        node.net()?.learn(&probe.addr()).await;
        assert!(
            node.sync_peer(&network, &stranger.device_id()?)
                .await
                .is_err()
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["type"], "hello");
        assert!(seen[0].get("state").is_none());
        anyhow::Ok(())
    }
    .await;
    let _ = router.shutdown().await;
    probe.close().await;
    finish_nodes(result, vec![(node, dir)]).await
}

#[tokio::test]
async fn a_full_membership_reply_checks_the_recipient_in_its_snapshot() -> Result<()> {
    use crate::identity::{GovernanceEvent, Member, Revocation};
    let (ia, ib, later) = (
        LocalIdentity::generate(),
        LocalIdentity::generate(),
        LocalIdentity::generate(),
    );
    let mut state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
    let network = ia.fixture_network_id()?;
    let (a, dir) = node(&ia).await?;
    let result = async {
        a.install_membership(state.clone()).await?;
        assert_eq!(a.member_node(&ib.device_id()?).await?, network);
        state.append(GovernanceEvent::revoke(
            &ia.fixture_admin(),
            &state.project()?,
            Revocation::new(&ib.device_id()?, None)?,
        )?)?;
        state.append(GovernanceEvent::grant(
            &ia.fixture_admin(),
            &state.project()?,
            Member::node(&later, "after-removal-private")?,
        )?)?;
        let request = json!({"type":"membership","state":state});
        assert!(
            a.handle_sync(&network, &ib.device_id()?, &request)
                .await
                .is_err(),
            "a request admitted earlier must not receive private post-removal membership"
        );
        let reply = a
            .handle_sync(&network, &later.device_id()?, &request)
            .await?;
        let returned: NetworkState = serde_json::from_value(reply["state"].clone())?;
        assert_eq!(
            returned.project()?.node(&later.device_id()?)?.name,
            "after-removal-private"
        );
        anyhow::Ok(())
    }
    .await;
    finish_nodes(result, vec![(a, dir)]).await
}

#[tokio::test]
async fn a_full_membership_exchange_checks_the_recipient_in_its_snapshot() -> Result<()> {
    use crate::identity::{GovernanceEvent, Member, Revocation};

    #[derive(Clone, Debug)]
    struct SnapshotProbe {
        node: Weak<Runtime>,
        snapshot: NetworkState,
        peer: String,
        network: String,
        address: iroh::EndpointAddr,
        seen: Arc<std::sync::Mutex<Vec<Value>>>,
    }
    impl ProtocolHandler for SnapshotProbe {
        async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
            let probe = self.clone();
            net::serve_streams(connection, move |request: Value| {
                let probe = probe.clone();
                async move {
                    probe.seen.lock().unwrap().push(request.clone());
                    if request["type"] == "hello" {
                        let node = probe.node.upgrade().context("fixture Node stopped")?;
                        node.net()?.forget_sync_connection(&probe.peer);
                        node.install_membership(probe.snapshot.clone()).await?;
                        // A discovery address does not grant membership.
                        node.net()?.learn(&probe.address).await;
                        Ok(json!({"network_id":probe.network,"membership":"0".repeat(64)}))
                    } else {
                        Ok(json!({"error":"fixture recorded the membership request"}))
                    }
                }
            })
            .await;
            Ok(())
        }
    }

    for revoke in [false, true] {
        let (ia, ib, later) = (
            LocalIdentity::generate(),
            LocalIdentity::generate(),
            LocalIdentity::generate(),
        );
        let mut snapshot = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
        let network = ia.fixture_network_id()?;
        let (a, dir) = node(&ia).await?;
        a.install_membership(snapshot.clone()).await?;
        if revoke {
            snapshot.append(GovernanceEvent::revoke(
                &ia.fixture_admin(),
                &snapshot.project()?,
                Revocation::new(&ib.device_id()?, None)?,
            )?)?;
        }
        snapshot.append(GovernanceEvent::grant(
            &ia.fixture_admin(),
            &snapshot.project()?,
            Member::node(&later, "after-removal-private")?,
        )?)?;
        let probe = Net::bind(
            &ib.secret_key_hex,
            &net::NetworkConfig {
                bind: Some("127.0.0.1:0".parse()?),
                ..Default::default()
            },
            0,
            vec![],
        )
        .await?;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let router = Router::builder(probe.endpoint.clone())
            .accept(
                net::SYNC,
                SnapshotProbe {
                    node: Arc::downgrade(&a),
                    snapshot,
                    peer: ib.device_id()?,
                    network: network.clone(),
                    address: probe.addr(),
                    seen: seen.clone(),
                },
            )
            .spawn();
        let result = async {
            a.sync_peers
                .lock()
                .unwrap()
                .entry(ib.device_id()?)
                .or_default()
                .running = true;
            a.start_network().await?;
            a.net()?.learn(&probe.addr()).await;
            assert!(a.sync_peer(&network, &ib.device_id()?).await.is_err());
            let seen = seen.lock().unwrap();
            assert!(seen.iter().any(|request| request["type"] == "hello"));
            let full: Vec<_> = seen
                .iter()
                .filter(|request| request["type"] == "membership")
                .collect();
            for request in &full {
                let state: NetworkState = serde_json::from_value(request["state"].clone())?;
                assert_eq!(
                    state.project()?.node(&later.device_id()?)?.name,
                    "after-removal-private"
                );
            }
            assert_eq!(full.len(), usize::from(!revoke));
            anyhow::Ok(())
        }
        .await;
        let _ = router.shutdown().await;
        probe.close().await;
        finish_nodes(result, vec![(a, dir)]).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_node_learns_its_removal_only_from_signed_history() -> Result<()> {
    use crate::identity::{GovernanceAction, GovernanceEvent, Revocation};
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let (member, removed) = (ia.device_id()?, ib.device_id()?);
    let state = ia.sign_network_fixture(1, vec![member.clone(), removed.clone()])?;
    let network = ia.fixture_network_id()?;
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = node(&ib).await?;
    let result = async {
        for node in [&a, &b] {
            node.install_membership(state.clone()).await?;
        }
        let revocation = Revocation::new(&removed, None)?;
        let admin = ia.fixture_admin().device_id()?;
        let action = GovernanceAction::Revoke(revocation.clone());
        let unsigned = GovernanceEvent::unsigned(&state.project()?, &admin, action.clone())?;
        let impostor = GovernanceEvent::unsigned(&state.project()?, &admin, action)?
            .sign(&LocalIdentity::generate().fixture_admin())?;
        for event in [unsigned, impostor] {
            let mut forged = state.clone();
            forged.events.push(event);
            let reply = json!({"type":"revoked","network_id":network,"state":forged});
            assert!(b.accept_removal(&network, &reply).await.is_err());
        }
        let bare = json!({"type":"revoked","network_id":network});
        assert!(b.accept_removal(&network, &bare).await.is_err());
        let held = b.membership_state().await?.project()?;
        assert_eq!(
            held.revocation(&removed),
            None,
            "a forged removal changes nothing"
        );

        let mut next = state.clone();
        next.append(GovernanceEvent::revoke(
            &ia.fixture_admin(),
            &state.project()?,
            revocation.clone(),
        )?)?;
        a.install_membership(next).await?;
        // Keep this round from racing B's background sync for the removed-node quota.
        b.sync_peers
            .lock()
            .unwrap()
            .entry(member.clone())
            .or_default()
            .running = true;
        for node in [&a, &b] {
            node.start_network().await?;
        }
        b.net()?.learn(&a.net()?.addr()).await;
        b.sync_peer(&network, &member).await?;
        let held = b.membership_state().await?.project()?;
        assert_eq!(held.revocation(&removed), Some(&revocation));
        Ok::<_, anyhow::Error>(())
    }
    .await;
    for node in [&a, &b] {
        node.stop_network().await;
    }
    drop((a, b));
    for dir in [dir_a, dir_b] {
        let _ = std::fs::remove_dir_all(dir);
    }
    result
}

/// A join waits on the Node whose invite it used. A manager connected to another member Node
/// is told to reread at once, and lists the request from there.
#[tokio::test(flavor = "multi_thread")]
async fn a_join_held_by_another_node_reaches_managers_on_every_node() -> Result<()> {
    use crate::identity::local::Join;
    use vibeface_secure::join::{INVITE_DOMAIN, InviteRecord, invite_hash, join_commit};

    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
    let network = ia.fixture_network_id()?;
    let admin = ia.fixture_admin();
    let manager = admin.device_id()?;
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = node(&ib).await?;
    let result = async {
        for node in [&a, &b] {
            node.install_membership(state.clone()).await?;
            node.start_network().await?;
        }
        a.net()?.learn(&b.net()?.addr()).await;
        b.net()?.learn(&a.net()?.addr()).await;
        first_sync(&a, &ib.device_id()?).await?;
        first_sync(&b, &ia.device_id()?).await?;

        let pushed = || {
            let mut events = a.events.subscribe();
            let network = network.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let update = events.recv().await?;
                        if update.network_id == network
                            && update.payload == json!({"type":"changed"})
                        {
                            return anyhow::Ok(());
                        }
                    }
                })
                .await
                .context("a manager on the other Node is told to reread")?
            }
        };
        let secret = [7u8; 32];
        let mut record = InviteRecord {
            network_id: network.clone(),
            invite_hash: invite_hash(&secret),
            issuer: manager.clone(),
            expires_at_ms: crate::clock::now_ms() + 60_000,
            signature: String::new(),
        };
        record.signature = admin.sign_payload(INVITE_DOMAIN, &record.signing_payload()?)?;
        let join_id = {
            let manager = manager.clone();
            b.store
                .call("peers.register_invite", move |store| {
                    store.register_invite(&record, &manager)
                })
                .await
        }?;
        let phone = LocalIdentity::generate().device_id()?;
        {
            let join = Join {
                secret: hex::encode(secret),
                public_key: phone.clone(),
                kind: crate::identity::IdentityKind::Client,
                name: "phone".into(),
                commit: join_commit(&phone, &"11".repeat(32))?,
            };
            let phone = phone.clone();
            b.store
                .call("peers.join", move |store| store.join(&join, &phone))
                .await
        }?;
        let listed = a.remote_joins(&manager).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["join_id"], join_id, "A lists the join B holds");

        let waiting = pushed();
        let nonce = json!({"join_id":join_id,"admin_nonce":"22".repeat(32)});
        crate::client::approve_join(&b, &manager, &nonce).await?;
        waiting.await?;
        let waiting = pushed();
        crate::client::approve_join(&b, &manager, &json!({"join_id":join_id,"reject":true}))
            .await?;
        waiting.await?;
        assert!(
            a.remote_joins(&manager).await.is_empty(),
            "a rejected join is gone"
        );
        anyhow::Ok(())
    }
    .await;
    stop_nodes(vec![(a, dir_a), (b, dir_b)]).await?;
    result
}

/// A Node that missed a join notice while its sync still looked fine is told once more, so
/// its managers see the request without waiting for another change.
#[tokio::test(flavor = "multi_thread")]
async fn a_missed_join_notice_is_sent_once_more() -> Result<()> {
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
    let network = ia.fixture_network_id()?;
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = node(&ib).await?;
    let result = async {
        // Sync stays out of the way: only the join notice tells B's clients to reread.
        for (node, peer) in [(&a, &ib), (&b, &ia)] {
            node.install_membership(state.clone()).await?;
            node.sync_peers
                .lock()
                .expect("sync peers")
                .entry(peer.device_id()?)
                .or_default()
                .running = true;
        }
        a.start_network().await?;
        let mut updates = b.events.subscribe();
        a.join_changed(&network);
        tokio::time::sleep(Duration::from_millis(200)).await;
        b.start_network().await?;
        a.net()?.learn(&b.net()?.addr()).await;
        assert!(
            !a.sync_failing(&ib.device_id()?),
            "A's sync with B looks fine"
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let update = updates.recv().await?;
                if update.payload == json!({"type":"changed"}) {
                    return anyhow::Ok(());
                }
            }
        })
        .await
        .context("B's managers are told to reread")??;
        anyhow::Ok(())
    }
    .await;
    stop_nodes(vec![(a, dir_a), (b, dir_b)]).await?;
    result
}
