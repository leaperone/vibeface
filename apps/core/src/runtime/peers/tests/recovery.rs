use super::super::test_support::{create_input_bot, finish_nodes, node, stop_nodes};
use super::super::*;
use crate::crypto::LocalIdentity;
use crate::runtime::test_support::{provider_work, signed_main_input};

/// A recovery by `ia`'s current root keeping `members`, signed at `issued`, on `state`.
fn recovery_on(
    ia: &LocalIdentity,
    state: &NetworkState,
    members: Vec<crate::identity::Member>,
    retired: Vec<crate::identity::Revocation>,
    issued: u64,
) -> Result<(NetworkState, crate::identity::RootCheckpoint, String)> {
    let next = LocalIdentity::generate().device_id()?;
    let checkpoint = crate::identity::RootCheckpoint::recovery(
        state,
        &ia.fixture_root_at(2),
        &next,
        members,
        retired,
        issued,
    )?;
    let mut waiting = state.clone();
    waiting.recover(checkpoint.clone())?;
    Ok((waiting, checkpoint, next))
}

/// `waiting` with the fixture manager's veto of `checkpoint`, signed at `at`.
fn vetoed(
    ia: &LocalIdentity,
    waiting: &NetworkState,
    checkpoint: &crate::identity::RootCheckpoint,
    at: u64,
) -> Result<NetworkState> {
    use crate::identity::{GovernanceAction, GovernanceEvent};
    let veto = GovernanceEvent::unsigned(
        &waiting.project_at(checkpoint.issued_at_ms)?,
        &ia.fixture_admin().device_id()?,
        GovernanceAction::Veto {
            checkpoint: checkpoint.hash()?,
            at_ms: at,
        },
    )?
    .sign(&ia.fixture_admin())?;
    let mut state = waiting.clone();
    state.append(veto)?;
    Ok(state)
}

/// Already overdue by the protocol's clock-skew allowance.
fn overdue() -> u64 {
    now_ms() as u64 - crate::identity::RECOVERY_DELAY_MS - crate::identity::RECOVERY_SKEW_MS
}

/// A Node that slept through the wait puts the recovery in effect by the wall clock; the
/// veto it missed, signed in time, puts the old directory back when it arrives. Sync goes on.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_flipped_early_rolls_back_on_a_late_veto_and_keeps_syncing() -> Result<()> {
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
    let network = ia.fixture_network_id()?;
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = node(&ib).await?;
    let result = async {
        for node in [&a, &b] {
            node.install_membership(state.clone()).await?;
        }
        let attacker = LocalIdentity::generate();
        let mut members = state.project()?.members;
        members.push(crate::identity::Member::client(
            &attacker, "attacker", true,
        )?);
        let issued = overdue();
        let (waiting, checkpoint, next) = recovery_on(&ia, &state, members, vec![], issued)?;
        let flipped = a.install_membership(waiting.clone()).await?;
        assert_eq!(flipped.epoch, 3);
        assert!(flipped.client(&attacker.device_id()?).is_ok());
        assert!(a.recovery_since_ms(&next).await?.is_some());
        b.install_membership(vetoed(&ia, &waiting, &checkpoint, issued + 1)?)
            .await?;
        let owner = ia.owner_fixture(&network, "c", &ib.device_id()?)?;
        {
            let arg0 = (ib).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = ("c").to_owned();
            let arg3 = ("t").to_owned();
            let arg4 = owner;
            let arg5 = 1;
            b.store
                .call("peers.create_conversation", move |store| {
                    store.create_conversation(&arg0, &arg1, &arg2, &arg3, arg4, arg5)
                })
                .await
        }?;
        for node in [&a, &b] {
            node.start_network().await?;
        }
        a.net()?.learn(&b.net()?.addr()).await;
        a.sync_peer(&network, &ib.device_id()?).await?;
        let view = a.membership_state().await?.project()?;
        assert_eq!(view.epoch, 2);
        assert_eq!(view.stopped, vec![checkpoint.hash()?]);
        assert!(view.client(&attacker.device_id()?).is_err());
        assert_eq!(a.membership_state().await?, b.membership_state().await?);
        assert!(
            {
                let network = network.clone();
                a.store
                    .call("peers.conversation", move |store| {
                        store.conversation(&network, "c")
                    })
                    .await
            }?
            .is_some(),
            "the data log synced too"
        );
        anyhow::Ok(())
    }
    .await;
    stop_nodes(vec![(a, dir_a), (b, dir_b)]).await?;
    result
}

/// Two Nodes that both slept through the veto agree with each other first, then with the
/// Node that holds the veto; all three end on the old directory.
#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_that_slept_through_a_veto_converge_with_the_one_that_holds_it() -> Result<()> {
    let ids = [
        LocalIdentity::generate(),
        LocalIdentity::generate(),
        LocalIdentity::generate(),
    ];
    let ia = &ids[0];
    let devices = ids
        .iter()
        .map(|id| id.device_id())
        .collect::<Result<Vec<_>>>()?;
    let state = ia.sign_network_fixture(1, devices.clone())?;
    let network = ia.fixture_network_id()?;
    let mut nodes = vec![];
    for id in &ids {
        nodes.push(node(id).await?);
    }
    let result = async {
        let [(a, _), (b, _), (c, _)] = &nodes[..] else {
            unreachable!("three nodes")
        };
        let attacker = LocalIdentity::generate();
        let mut members = state.project()?.members;
        members.push(crate::identity::Member::client(
            &attacker, "attacker", true,
        )?);
        let issued = overdue();
        let (waiting, checkpoint, _) = recovery_on(ia, &state, members, vec![], issued)?;
        for node in [a, b] {
            node.install_membership(waiting.clone()).await?;
            assert_eq!(node.membership_state().await?.project()?.epoch, 3);
        }
        c.install_membership(vetoed(ia, &waiting, &checkpoint, issued + 1)?)
            .await?;
        for node in [a, b] {
            node.start_network().await?;
        }
        a.net()?.learn(&b.net()?.addr()).await;
        a.sync_peer(&network, &devices[1]).await?;
        for node in [a, b] {
            assert_eq!(node.membership_state().await?.project()?.epoch, 3);
        }
        c.start_network().await?;
        for (from, to) in [(a, c), (b, a), (c, a), (c, b)] {
            from.net()?.learn(&to.net()?.addr()).await;
        }
        a.sync_peer(&network, &devices[2]).await?;
        b.sync_peer(&network, &devices[0]).await?;
        let expected = c.membership_state().await?;
        for node in [a, b, c] {
            assert_eq!(node.membership_state().await?, expected);
            let view = node.membership_state().await?.project()?;
            assert_eq!((view.epoch, &view.stopped), (2, &vec![checkpoint.hash()?]));
            assert!(view.client(&attacker.device_id()?).is_err());
        }
        anyhow::Ok(())
    }
    .await;
    stop_nodes(nodes).await?;
    result
}

/// A Node that put in effect a recovery leaving out a peer refuses that peer; the peer, which
/// holds the veto, hands it over anyway, and both end on the old directory.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_refusing_a_peer_it_saw_removed_takes_its_veto() -> Result<()> {
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
    let network = ia.fixture_network_id()?;
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = node(&ib).await?;
    let result = async {
        for node in [&a, &b] {
            node.install_membership(state.clone()).await?;
        }
        let view = state.project()?;
        let kept: Vec<_> = view
            .members
            .iter()
            .filter(|member| member.device_id != ib.device_id().unwrap_or_default())
            .cloned()
            .collect();
        let retired = view.retire(&kept, &{
            let arg0 = (network).to_owned();
            a.store
                .call("peers.heads", move |store| store.heads(&arg0))
                .await
        }?)?;
        let issued = overdue();
        let (waiting, checkpoint, _) = recovery_on(&ia, &state, kept, retired, issued)?;
        let flipped = a.install_membership(waiting.clone()).await?;
        assert!(flipped.revocation(&ib.device_id()?).is_some());
        b.install_membership(vetoed(&ia, &waiting, &checkpoint, issued + 1)?)
            .await?;
        for (node, peer) in [(&a, &ib), (&b, &ia)] {
            node.sync_peers
                .lock()
                .unwrap()
                .entry(peer.device_id()?)
                .or_default()
                .running = true;
            node.start_network().await?;
        }
        b.net()?.learn(&a.net()?.addr()).await;
        let peer = ia.device_id()?;
        let error = b
            .sync_peer(&network, &peer)
            .await
            .expect_err("the retained veto disproves the removed-peer Hello");
        assert_eq!(
            error.to_string(),
            "the peer refused this Node without a removal",
            "the first sync must deliver the veto before returning its semantic refusal"
        );
        assert_eq!(a.membership_state().await?, b.membership_state().await?);
        assert_eq!(a.membership_state().await?.project()?.epoch, 2);
        b.sync_peer(&network, &peer).await?;
        anyhow::Ok(())
    }
    .await;
    stop_nodes(vec![(a, dir_a), (b, dir_b)]).await?;
    result
}

#[tokio::test(flavor = "multi_thread")]
async fn an_undisclosable_removal_still_accepts_a_members_late_veto() -> Result<()> {
    use crate::identity::{GovernanceEvent, Member, Revocation};
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let (manager_b, manager_c, manager_d) = (
        LocalIdentity::generate(),
        LocalIdentity::generate(),
        LocalIdentity::generate(),
    );
    let admin = ia.fixture_admin();
    let mut base = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
    for manager in [&manager_b, &manager_d] {
        base.append(GovernanceEvent::grant(
            &admin,
            &base.project()?,
            Member::client(manager, "manager", true)?,
        )?)?;
    }
    let network = ia.fixture_network_id()?;
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = node(&ib).await?;
    let result = async {
        let issued = overdue();
        let (waiting, checkpoint, _) =
            recovery_on(&ia, &base, base.project()?.members, vec![], issued)?;
        let mut removed = waiting.clone();
        let view = removed.project()?;
        let mut private = waiting.clone();
        private.append(GovernanceEvent::grant(
            &manager_d,
            &view,
            Member::client(&manager_c, "private-manager", true)?,
        )?)?;
        removed.append(GovernanceEvent::revoke(
            &admin,
            &view,
            Revocation::new(&ib.device_id()?, None)?,
        )?)?;
        removed.append(GovernanceEvent::revoke(
            &manager_b,
            &view,
            Revocation::new(&admin.device_id()?, None)?,
        )?)?;
        removed = removed.merge(&private)?;
        assert!(removed.project()?.node(&ib.device_id()?).is_ok());
        removed.append(GovernanceEvent::revoke(
            &manager_c,
            &private.project()?,
            Revocation::new(&manager_b.device_id()?, None)?,
        )?)?;
        assert!(removed.project()?.removed_node(&ib.device_id()?));
        assert!(removed.removal_state(&ib.device_id()?, None).is_err());
        a.install_membership(removed).await?;
        b.install_membership(vetoed(&ia, &waiting, &checkpoint, issued + 1)?)
            .await?;
        b.sync_peers
            .lock()
            .unwrap()
            .entry(ia.device_id()?)
            .or_default()
            .running = true;
        for node in [&a, &b] {
            node.start_network().await?;
        }
        b.net()?.learn(&a.net()?.addr()).await;
        assert!(
            b.net()?
                .request(
                    &ia.device_id()?,
                    net::SYNC,
                    &b.hello(&network).await?,
                    PEER_TIMEOUT
                )
                .await
                .is_err(),
            "the hello discloses no unproved later member"
        );
        assert!(
            b.sync_peer(&network, &ia.device_id()?).await.is_err(),
            "the repair does not claim a completed history sync"
        );
        assert!(
            a.membership_state()
                .await?
                .project()?
                .node(&ib.device_id()?)
                .is_ok()
        );
        assert_eq!(a.membership_state().await?, b.membership_state().await?);
        let owner = ia.owner_fixture(&network, "after-veto", &ia.device_id()?)?;
        a.store
            .call("peers.create_conversation", {
                let network = network.to_owned();
                let ia = ia.to_owned();
                let owner = owner.to_owned();
                move |store| {
                    store.create_conversation(
                        &ia,
                        &network,
                        "after-veto",
                        "restored",
                        owner,
                        now_ms(),
                    )
                }
            })
            .await?;
        b.sync_peer(&network, &ia.device_id()?).await?;
        assert!(
            b.store
                .call("peers.conversation", {
                    let network = network.to_owned();
                    move |store| store.conversation(&network, "after-veto")
                })
                .await?
                .is_some(),
            "the next authenticated connection completes normal sync"
        );
        anyhow::Ok(())
    }
    .await;
    finish_nodes(result, vec![(a, dir_a), (b, dir_b)]).await
}

/// A stolen, removed Node with the recovery file signs a backdated recovery and pushes it:
/// the checkpoint stays out, so nothing skips the wait or the veto.
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_node_cannot_push_a_backdated_recovery() -> Result<()> {
    use crate::identity::{GovernanceEvent, Revocation};
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
    let network = ia.fixture_network_id()?;
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = node(&ib).await?;
    let result = async {
        let mut removed = state.clone();
        removed.append(GovernanceEvent::revoke(
            &ia.fixture_admin(),
            &state.project()?,
            Revocation::new(&ib.device_id()?, None)?,
        )?)?;
        a.install_membership(removed.clone()).await?;
        let before = a.membership_state().await?;
        let attacker = LocalIdentity::generate();
        let mut members = state.project()?.members;
        members.push(crate::identity::Member::client(
            &attacker, "attacker", true,
        )?);
        let (forged, checkpoint, _) = recovery_on(&ia, &removed, members, vec![], overdue())?;
        assert!(forged.project()?.client(&attacker.device_id()?).is_ok());
        assert!(forged.project()?.node(&ib.device_id()?).is_ok());
        assert!(before.removal_state(&ib.device_id()?, None).is_ok());
        assert!(
            before
                .removal_state(&ib.device_id()?, Some(&forged))
                .is_err(),
            "the local denial cannot override the unaccepted recovery directory"
        );
        for node in [&a, &b] {
            node.start_network().await?;
        }
        b.net()?.learn(&a.net()?.addr()).await;
        let reply = b
            .net()?
            .request(
                &ia.device_id()?,
                net::SYNC,
                &json!({"type":"membership","network_id":network,"state":forged}),
                PEER_TIMEOUT,
            )
            .await;
        assert!(reply.is_err(), "an unprovable denial fails closed");
        assert_eq!(a.membership_state().await?, before);
        assert!(
            !a.membership_state()
                .await?
                .checkpoints
                .contains(&checkpoint)
        );
        let view = a.membership_state().await?.project()?;
        assert!(view.client(&attacker.device_id()?).is_err());
        anyhow::Ok(())
    }
    .await;
    stop_nodes(vec![(a, dir_a), (b, dir_b)]).await?;
    result
}

#[tokio::test(flavor = "multi_thread")]
async fn removed_node_sync_bounds_frames_streams_and_stalled_requests() -> Result<()> {
    use crate::identity::{GovernanceEvent, Revocation};
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
    let network = ia.fixture_network_id()?;
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = node(&ib).await?;
    let result = async {
        let mut removed = state.clone();
        removed.append(GovernanceEvent::revoke(
            &ia.fixture_admin(), &state.project()?, Revocation::new(&ib.device_id()?, None)?,
        )?)?;
        a.install_membership(removed).await?;
        b.install_membership(state).await?;
        // Only the explicit requests exercise this quota, without B's periodic sync racing them.
        b.sync_peers.lock().unwrap().entry(ia.device_id()?).or_default().running = true;
        for node in [&a, &b] { node.start_network().await?; }
        let limits = a.removed_sync_limits.for_peer(&ib.device_id()?);
        let first = b.net()?.endpoint.connect(a.net()?.addr(), net::SYNC).await?;
        let (mut stalled, mut reply) = first.open_bi().await?;
        stalled.write_all(&[0]).await?;
        tokio::time::timeout(Duration::from_secs(3), async {
            while limits.1.available_permits() != 0 { tokio::task::yield_now().await; }
        }).await.context("the incomplete request did not acquire its quota")?;
        let second = b.net()?.endpoint.connect(a.net()?.addr(), net::SYNC).await?;
        assert!(tokio::time::timeout(Duration::from_secs(3), net::call(&second,
            &json!({"type":"hello","network_id":network})))
            .await?.is_err(), "the same removed Node cannot bypass its quota with another connection");
        assert!(!matches!(tokio::time::timeout(REMOVED_SYNC_TIMEOUT + Duration::from_secs(3),
            net::read_frame(&mut reply)).await?, Ok(Some(_))), "a stalled request is closed within the deadline");
        drop(stalled);
        tokio::time::timeout(Duration::from_secs(3), async {
            while limits.1.available_permits() != 1 { tokio::task::yield_now().await; }
        }).await.context("the stalled request kept its quota")?;
        let oversized = net::call(&second, &json!({"type":"membership","network_id":network,"padding":"x".repeat(REMOVED_SYNC_BYTES)})).await;
        assert!(oversized.unwrap_err().to_string().contains("frame too large"));
        tokio::time::timeout(Duration::from_secs(3), async {
            while limits.1.available_permits() != 1 { tokio::task::yield_now().await; }
        }).await.context("the oversized request kept its quota")?;
        let bounded = net::call(&second, &json!({"type":"hello","network_id":network})).await?;
        assert_eq!(bounded["type"], "revoked", "a small request still receives its signed denial");
        anyhow::Ok(())
    }.await;
    finish_nodes(result, vec![(a, dir_a), (b, dir_b)]).await
}

/// The lost device was itself a Node and never comes back: the recovery that leaves it out
/// takes effect when the wall clock reaches its time.
#[tokio::test]
async fn a_recovery_leaving_out_a_lost_node_takes_effect_on_time() -> Result<()> {
    let (ia, lost) = (LocalIdentity::generate(), LocalIdentity::generate());
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, lost.device_id()?])?;
    let network = ia.fixture_network_id()?;
    let (a, dir) = node(&ia).await.context("open lost-Node recovery fixture")?;
    let result = async {
        a.install_membership(state.clone())
            .await
            .context("install initial lost-Node recovery membership")?;
        let view = state.project()?;
        let kept: Vec<_> = view
            .members
            .iter()
            .filter(|member| member.device_id != lost.device_id().unwrap_or_default())
            .cloned()
            .collect();
        let retired = view.retire(&kept, &{
            let arg0 = (network).to_owned();
            a.store
                .call("peers.heads", move |store| store.heads(&arg0))
                .await
                .context("read lost-Node recovery retirement heads")
        }?)?;
        let issued = now_ms() as u64 + 1_000 - crate::identity::RECOVERY_DELAY_MS;
        let (waiting, checkpoint, next) = recovery_on(&ia, &state, kept, retired, issued)?;
        let effective_at = checkpoint.effective_at_ms();
        assert_eq!(
            a.install_membership(waiting)
                .await
                .context("install waiting lost-Node recovery")?
                .epoch,
            2,
            "waiting"
        );
        assert!(a.recovery_since_ms(&next).await?.is_none());
        a.start_recovery_timer();
        let deadline = Instant::now() + Duration::from_secs(10);
        while a
            .recovery_since_ms(&next)
            .await
            .context("read lost-Node recovery activation marker")?
            .is_none()
        {
            ensure!(Instant::now() < deadline, "the recovery never took effect");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let installed = a
            .membership_state()
            .await
            .context("read activated lost-Node recovery membership")?;
        // The marker records a past activation; a later wall-clock sample can precede it.
        let before = installed.project_at(effective_at - 1)?;
        assert_eq!(before.epoch, 2);
        assert_eq!(before.recovery_key, view.recovery_key);
        assert!(before.node(&lost.device_id()?).is_ok());
        assert!(before.revocation(&lost.device_id()?).is_none());
        let view = installed.project_at(effective_at)?;
        assert_eq!(view.epoch, 3);
        assert_eq!(view.recovery_key, next);
        assert!(view.node(&lost.device_id()?).is_err());
        assert!(view.revocation(&lost.device_id()?).is_some());
        anyhow::Ok(())
    }
    .await;
    finish_nodes(result, vec![(a, dir)]).await
}

/// A recovery leaving out a Node records nothing about its work: a late veto can put the
/// Node back, and a record in the shared log could not be taken back. A manager's signed
/// removal leaves its work unverified.
#[tokio::test]
async fn a_recovery_undone_by_a_late_veto_adds_nothing_to_the_shared_log() -> Result<()> {
    use crate::{
        identity::{GovernanceEvent, Revocation},
        model::WorkOutcome,
        work::WorkSpec,
    };
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, ib.device_id()?])?;
    let network = ia.fixture_network_id()?;
    let (a, dir) = node(&ia).await?;
    let result = async {
        a.install_membership(state.clone()).await?;
        let trigger = crate::scheduling::message_trigger("m1");
        let target = ib.device_id()?;
        let work = WorkSpec {
            work_id: WorkSpec::id("c", &trigger, "remote"),
            trigger: trigger.clone(),
            dispatcher: ia.device_id()?,
            call_ref: "remote".into(),
            ..provider_work(Provider::Codex, &target, Path::new("/tmp"), "p")
        };
        {
            let ia = ia.clone();
            let ib = ib.clone();
            let network = network.clone();
            let work = work.clone();
            a.store
                .call("peers.fixture", move |store| {
                    let owner = ia.owner_fixture(&network, "c", &ia.device_id()?)?;
                    store.create_conversation(&ia, &network, "c", "t", owner, 1)?;
                    create_input_bot(store, &ia, &ia.fixture_admin(), &network, "c", 1)?;
                    store.post_input_fixture(
                        &ia,
                        &network,
                        signed_main_input(
                            &ia.fixture_admin(),
                            &network,
                            "c",
                            "m1",
                            "go",
                            vec![],
                            1,
                        )?,
                        2,
                    )?;
                    store.accept_work_fixture(&ib, &network, work.clone(), 3)?;

                    Ok(())
                })
                .await?
        }
        let shared = async || -> Result<(i64, Option<WorkOutcome>)> {
            let network = network.clone();
            let work = work.clone();
            a.store
                .call("peers.shared_work_snapshot", move |store| {
                    let count = store.connection.query_row(
                        "SELECT count(*) FROM events WHERE network_id=?",
                        [&network],
                        |r| r.get(0),
                    )?;
                    let outcome = store
                        .works(&network, "c")?
                        .into_iter()
                        .find(|w| w.work_id == work.work_id)
                        .context("work")?
                        .outcome;
                    Ok((count, outcome))
                })
                .await
        };
        let before = shared().await?;
        assert_eq!(before.1, None);

        let view = state.project()?;
        let kept: Vec<_> = view
            .members
            .iter()
            .filter(|member| member.device_id != work.target)
            .cloned()
            .collect();
        let retired = view.retire(&kept, &{
            let arg0 = (network).to_owned();
            a.store
                .call("peers.heads", move |store| store.heads(&arg0))
                .await
        }?)?;
        let issued = overdue();
        let (waiting, checkpoint, _) = recovery_on(&ia, &state, kept, retired, issued)?;
        let flipped = a.install_membership(waiting.clone()).await?;
        assert!(flipped.revocation(&work.target).is_some());
        a.unverify_revoked_works(&network).await;
        assert_eq!(shared().await?, before, "a recovery records nothing");

        let back = a
            .install_membership(vetoed(&ia, &waiting, &checkpoint, issued + 1)?)
            .await?;
        assert_eq!(back.epoch, 2);
        assert!(back.node(&work.target).is_ok());
        assert_eq!(shared().await?, before, "nor does rolling it back");

        let heads = {
            let arg0 = (network).to_owned();
            a.store
                .call("peers.heads", move |store| store.heads(&arg0))
                .await
        }?;
        let (seq, hash) = &heads[&work.target];
        let removal = GovernanceEvent::revoke(
            &ia.fixture_admin(),
            &back,
            Revocation::new(&work.target, Some((*seq, hash.as_str())))?,
        )?;
        let mut removed = a.membership_state().await?;
        removed.append(removal)?;
        a.install_membership(removed).await?;
        assert_eq!(shared().await?.1, Some(WorkOutcome::Unverified));
        anyhow::Ok(())
    }
    .await;
    drop(a);
    std::fs::remove_dir_all(dir)?;
    result
}

/// Nothing new arrives when a recovery's wait ends, so the Node installs the history again
/// itself, once.
#[tokio::test]
async fn a_waiting_recovery_is_installed_once_when_due() -> Result<()> {
    use crate::identity::{RECOVERY_DELAY_MS, RootCheckpoint};
    let ia = LocalIdentity::generate();
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?])?;
    let (a, dir) = node(&ia).await?;
    let result = async {
        a.install_membership(state.clone()).await?;
        let next = LocalIdentity::generate().device_id()?;
        let members = state.project()?.members;
        let issued = now_ms() as u64 + 1_000 - RECOVERY_DELAY_MS;
        let mut waiting = state.clone();
        waiting.recover(RootCheckpoint::recovery(
            &state,
            &ia.fixture_root_at(2),
            &next,
            members,
            vec![],
            issued,
        )?)?;
        let expected_epoch = waiting.project_at(issued + RECOVERY_DELAY_MS)?.epoch;
        let mut changes = a.membership.subscribe();
        a.install_membership(waiting).await?;
        a.start_recovery_timer();
        let installed_at = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                changes.changed().await?;
                if let Some(at) = a.recovery_since_ms(&next).await? {
                    break anyhow::Ok(at);
                }
            }
        })
        .await??;
        let (_, cached) = a.ready_client_membership().await?;
        let effective = cached.context("committed membership")?.project()?;
        assert_eq!(effective.epoch, expected_epoch);
        assert_eq!(effective.recovery_key, next);
        assert!(effective.node(&ia.device_id()?).is_ok());
        assert!(a.settle_recovery().await?.is_none());
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            a.recovery_since_ms(&next).await?,
            Some(installed_at),
            "the durable recovery marker must not be rewritten"
        );
        anyhow::Ok(())
    }
    .await;
    a.shutdown().await;
    drop(a);
    std::fs::remove_dir_all(dir)?;
    result
}

#[tokio::test]
async fn a_waiting_recovery_wakes_authority_observers_while_the_writer_is_held() -> Result<()> {
    use crate::identity::{RECOVERY_DELAY_MS, RootCheckpoint};
    let identity = LocalIdentity::generate();
    let state = identity.sign_network_fixture(1, vec![identity.device_id()?])?;
    let (node, directory) = node(&identity).await?;
    node.install_membership(state.clone()).await?;
    let before_epoch = state.project()?.epoch;
    let next = LocalIdentity::generate().device_id()?;
    let issued = now_ms() as u64 + 1_000 - RECOVERY_DELAY_MS;
    let mut waiting = state.clone();
    waiting.recover(RootCheckpoint::recovery(
        &state,
        &identity.fixture_root_at(2),
        &next,
        state.project()?.members,
        vec![],
        issued,
    )?)?;
    node.install_membership(waiting).await?;
    let mut changed = node.membership.subscribe();
    let (entered, running) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let writer_node = node.clone();
    let writer = tokio::spawn(async move {
        writer_node
            .store
            .call("fixture_hold_authority_writer", move |store| {
                let transaction = store.connection.transaction()?;
                let count: i64 =
                    transaction.query_row("SELECT count(*) FROM networks", [], |row| row.get(0))?;
                assert_eq!(count, 1);
                let _ = entered.send(());
                held.recv()?;
                transaction.commit()?;
                Ok(())
            })
            .await
    });
    let result = async {
        tokio::time::timeout(Duration::from_secs(2), running).await??;
        let projected = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                changed.changed().await?;
                let (_, state) = node.ready_client_membership().await?;
                let view = state.context("cached authority")?.project()?;
                if view.epoch != before_epoch {
                    break anyhow::Ok(view);
                }
            }
        })
        .await??;
        assert!(
            !writer.is_finished(),
            "permission notification must not wait for the writer"
        );
        assert_eq!(projected.recovery_key, next);
        assert!(projected.epoch > before_epoch);
        anyhow::Ok(())
    }
    .await;
    let _ = release.send(());
    let written = writer.await;
    node.shutdown().await;
    drop(node);
    std::fs::remove_dir_all(directory)?;
    written??;
    result
}
