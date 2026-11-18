use super::super::test_support::{
    cleanup, connected, delete_bot, main_input_fact, running_work, state, with_agent, with_node,
};
use super::super::*;
use crate::crypto::LocalIdentity;
use crate::runtime::test_support::{bot, node_at, provider_work, scripted_app_server};
use std::sync::atomic::Ordering;

async fn answering_app_server(
    node: &Runtime,
) -> Result<tokio::sync::mpsc::UnboundedReceiver<Value>> {
    scripted_app_server(node, |_| json!({"result":{}})).await
}

#[tokio::test]
async fn a_work_stopped_while_starting_is_interrupted_once_bound() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        let mut requests = answering_app_server(&node).await?;
        connected(&node).await;
        let network = node.network_id().await?;
        let local = node.identity.device_id()?;
        let spec = crate::work::WorkSpec {
            work_id: "w-stop".into(),
            call_ref: "r".into(),
            thread: Some("t-9".into()),
            ..provider_work(Provider::Codex, &local, Path::new("/tmp"), "p")
        };
        {
            let identity = (node.identity).to_owned();
            let network = (network).to_owned();
            node.store
                .call("triage.test.accept_work_fixture", move |store| {
                    store.accept_work_fixture(&identity, &network, spec, now_ms())
                })
                .await
        }?;
        let stopped = node.interrupt_work_outcome(&network, "w-stop").await?;
        assert_eq!(stopped.receipt.execution, crate::tools::Execution::Pending);
        assert_eq!(stopped.value["waiting_for_start"], true);
        let waiting = {
            node.store
                .call("triage.test.stop_requests", move |store| {
                    store.stop_requests()
                })
                .await
        }?;
        assert!(waiting.iter().any(|request| request.work_id == "w-stop"));

        let epoch = node.codex.epoch();
        let turn = crate::work::Signal::TurnStarted {
            turn_id: "u-1".into(),
            epoch,
        };
        let bound = node.signal_work(&network, "w-stop", turn).await?;
        assert!(matches!(
            bound.state,
            crate::work::WorkState::Interrupting { .. }
        ));
        let interrupt = loop {
            let request = tokio::time::timeout(Duration::from_secs(5), requests.recv())
                .await?
                .context("the App Server closed")?;
            if request["method"] == "turn/interrupt" {
                break request;
            }
        };
        assert_eq!(
            interrupt["params"],
            json!({"threadId":"t-9","turnId":"u-1"})
        );
        let held = {
            node.store
                .call("triage.test.stop_requests", move |store| {
                    store.stop_requests()
                })
                .await
        }?;
        assert!(
            held.iter()
                .any(|request| request.work_id == "w-stop" && !request.timed_out),
            "the stop is held until the turn ends"
        );
        node.deliver_work_requests(&network).await?;
        node.reconcile_works(&network).await?;
        while let Ok(request) = requests.try_recv() {
            assert_ne!(request["method"], "turn/interrupt", "the stop is sent once");
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
}

/// The Node's turn reads and interrupts, in order. The bridge's initial snapshot and
/// read-only fallbacks use `includeTurns: false`; they are not the Node's work probes.
fn methods(requests: &mut tokio::sync::mpsc::UnboundedReceiver<Value>) -> Vec<String> {
    std::iter::from_fn(|| requests.try_recv().ok())
        .filter(|request| match request["method"].as_str() {
            Some("turn/interrupt") => true,
            Some("thread/read") => request["params"]["includeTurns"] == true,
            _ => false,
        })
        .filter_map(|request| request["method"].as_str().map(str::to_owned))
        .collect()
}

async fn held_stop(node: &Runtime, work_id: &str) -> Result<Option<crate::work::StopRequest>> {
    Ok({
        node.store
            .call("triage.test.stop_requests", move |store| {
                store.stop_requests()
            })
            .await
    }?
    .into_iter()
    .find(|request| request.work_id == work_id))
}

/// A `turn/start` stuck past the send window is not written off as never run: the work is
/// unverified, and the turn that shows up late is followed to its result.
#[tokio::test]
async fn a_send_stuck_past_its_window_is_unverified_then_followed() -> Result<()> {
    use crate::model::{WorkOutcome, WorkStatus};
    use std::sync::atomic::AtomicBool;
    with_node(async |node, admin, _| {
        bot(&node, &admin, "c", "前端", &[]).await?;
        let started = Arc::new(AtomicBool::new(false));
        let seen = started.clone();
        let _requests =
            scripted_app_server(&node, move |request| match request["method"].as_str() {
                Some("thread/read") if seen.load(Ordering::SeqCst) => {
                    json!({"result":{"thread":{"turns":[{"id":"u-1","status":"inProgress",
                        "items":[{"type":"userMessage","clientId":"w-late"}]}]}}})
                }
                Some("thread/read") => json!({"result":{"thread":{"turns":[]}}}),
                _ => json!({"result":{}}),
            })
            .await?;
        connected(&node).await;
        let network = node.network_id().await?;
        let local = node.identity.device_id()?;
        let trigger = main_input_fact(&node, &admin, "c", "m1").await?;
        let spec = crate::work::WorkSpec {
            work_id: "w-late".into(),
            trigger,
            call_ref: "late".into(),
            ..provider_work(Provider::Codex, &local, Path::new("/tmp"), "p")
        };
        let window = crate::work::SEND_WINDOW_MS + crate::work::RECONCILE_GRACE_MS;
        {
            let identity = (node.identity).to_owned();
            let network = (network).to_owned();
            node.store
                .call("triage.test.accept_work_fixture", move |store| {
                    store.accept_work_fixture(&identity, &network, spec, now_ms() - window)
                })
                .await
        }?;
        {
            let identity = (node.identity).to_owned();
            let network = (network).to_owned();
            let signal = crate::work::Signal::ThreadCreated("t-late".into());
            node.store
                .call("triage.test.apply_work_signal", move |store| {
                    store.apply_work_signal(&identity, &network, "w-late", signal, now_ms())
                })
                .await
        }?;
        node.reconcile_works(&network).await?;
        let outcome = async || -> Result<(Option<WorkOutcome>, WorkStatus)> {
            let work = {
                let network = (network).to_owned();
                node.store
                    .call("triage.test.works", move |store| store.works(&network, "c"))
                    .await
            }?
            .into_iter()
            .find(|work| work.work_id == "w-late")
            .context("work")?;
            Ok((work.outcome, work.state))
        };
        assert_eq!(
            outcome().await?,
            (Some(WorkOutcome::Unverified), WorkStatus::Unverified),
            "an unseen send is not recorded as refused"
        );
        assert!(matches!(
            state(&node, "w-late").await?,
            crate::work::WorkState::Unverified { handle: None, thread: Some(ref thread), .. }
                if thread == "t-late"
        ));
        assert!(
            node.bind_work(
                &network,
                "c",
                "w-late",
                vibeface_bridge::codex::StartReport::Thread {
                    thread_id: "t-late".into()
                }
            )
            .await
            .is_err(),
            "past its window the work takes no new thread"
        );

        started.store(true, Ordering::SeqCst);
        let late = vibeface_bridge::codex::StartReport::Turn {
            turn_id: "u-1".into(),
        };
        node.bind_work(&network, "c", "w-late", late).await?;
        assert!(matches!(
            state(&node, "w-late").await?,
            crate::work::WorkState::Running { ref handle } if handle.turn_id == "u-1"
        ));
        let done = crate::work::Signal::TurnCompleted {
            turn_id: "u-1".into(),
            status: vibeface_bridge::turn::TurnStatus::Completed,
            text: "done".into(),
            error: None,
        };
        node.signal_work(&network, "w-late", done).await?;
        assert_eq!(
            outcome().await?,
            (Some(WorkOutcome::Completed), WorkStatus::Finished),
            "the late turn's real result replaces the unknown one"
        );
        assert!(
            {
                let trigger = (crate::scheduling::work_trigger("w-late")).to_owned();
                node.store
                    .call("triage.test.scheduler_job", move |store| {
                        store.scheduler_job("c", &trigger)
                    })
                    .await
            }?
            .is_some(),
            "and reports back"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn reconciliation_reads_a_running_turn_whose_end_was_missed() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        let mut requests =
            scripted_app_server(&node, move |request| match request["method"].as_str() {
                Some("thread/read") => json!({"result":{"thread":{"turns":[
                    {"id":"u-1","status":"completed","items":[]}]}}}),
                _ => json!({"result":{}}),
            })
            .await?;
        connected(&node).await;
        let network = node.network_id().await?;
        running_work(&node, &admin, "w-missed").await?;
        node.reconcile_works(&network).await?;
        assert_eq!(methods(&mut requests), [] as [&str; 0], "not due yet");

        node.live_probe_at
            .store(now_ms() - 60_000, std::sync::atomic::Ordering::Relaxed);
        node.reconcile_works(&network).await?;
        // The background reconciler may read too; reads are harmless, a resend is not.
        let sent = methods(&mut requests);
        assert!(
            !sent.is_empty() && sent.iter().all(|method| method == "thread/read"),
            "read, never resent: {sent:?}"
        );
        assert!(matches!(
            state(&node, "w-missed").await?,
            crate::work::WorkState::Finished {
                outcome: crate::model::WorkOutcome::Completed
            }
        ));
        node.reconcile_works(&network).await?;
        assert_eq!(methods(&mut requests), [] as [&str; 0]);
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn a_held_stop_outlives_a_failed_interrupt_and_ends_with_its_work() -> Result<()> {
    use std::sync::atomic::AtomicBool;
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        let refuse = Arc::new(AtomicBool::new(true));
        let refusing = refuse.clone();
        let mut requests =
            scripted_app_server(&node, move |request| match request["method"].as_str() {
                Some("turn/interrupt") if refusing.load(Ordering::SeqCst) => {
                    json!({"error":{"code":-32603,"message":"busy"}})
                }
                Some("thread/read") => json!({"result":{"thread":{"turns":[
                    {"id":"u-1","status":"inProgress","items":[]}]}}}),
                _ => json!({"result":{}}),
            })
            .await?;
        connected(&node).await;
        let network = node.network_id().await?;
        running_work(&node, &admin, "w-held").await?;

        let stopped = node.interrupt_work(&network, "w-held").await?;
        assert_eq!(stopped["retrying"], true);
        assert!(matches!(
            state(&node, "w-held").await?,
            crate::work::WorkState::Running { .. }
        ));
        assert!(
            held_stop(&node, "w-held").await?.is_some(),
            "a failed interrupt keeps the stop"
        );
        assert_eq!(methods(&mut requests), ["turn/interrupt"]);
        node.reconcile_works(&network).await?;
        assert_eq!(methods(&mut requests), [] as [&str; 0], "retries back off");

        // Only the pacing is in memory, as after a restart; the stop itself is stored.
        refuse.store(false, Ordering::SeqCst);
        node.stop_retries.lock().unwrap().clear();
        node.reconcile_works(&network).await?;
        assert_eq!(
            methods(&mut requests),
            ["thread/read", "turn/interrupt"],
            "the turn is read before the interrupt is sent again"
        );
        assert!(matches!(
            state(&node, "w-held").await?,
            crate::work::WorkState::Interrupting { .. }
        ));
        let held = held_stop(&node, "w-held").await?.context("held")?;
        assert!(!held.timed_out && held.deadline_ms > now_ms());
        node.reconcile_works(&network).await?;
        assert_eq!(
            methods(&mut requests),
            [] as [&str; 0],
            "before the deadline"
        );

        // The stored deadline passes with the turn still running: it is asked again and
        // shows as still trying.
        let mut late = held;
        late.deadline_ms = now_ms() - 1;
        {
            let request = (late).to_owned();
            node.store
                .call("triage.test.save_stop_request", move |store| {
                    store.save_stop_request(&request)
                })
                .await
        }?;
        node.reconcile_works(&network).await?;
        assert_eq!(methods(&mut requests), ["thread/read", "turn/interrupt"]);
        assert!(held_stop(&node, "w-held").await?.context("held")?.timed_out);
        assert!(matches!(
            state(&node, "w-held").await?,
            crate::work::WorkState::Interrupting { .. }
        ));

        let ended = crate::work::Signal::TurnCompleted {
            turn_id: "u-1".into(),
            status: vibeface_bridge::turn::TurnStatus::Interrupted,
            text: String::new(),
            error: None,
        };
        node.signal_work(&network, "w-held", ended).await?;
        assert!(held_stop(&node, "w-held").await?.is_none());
        node.stop_retries.lock().unwrap().clear();
        node.reconcile_works(&network).await?;
        let again = node.interrupt_work(&network, "w-held").await?;
        assert_eq!(again["retrying"], false);
        assert_eq!(
            methods(&mut requests),
            [] as [&str; 0],
            "an ended turn is never interrupted again"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_past_its_deadline_reaches_a_device_that_comes_back() -> Result<()> {
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let target = ib.device_id()?;
    let membership = ia.sign_network_fixture(1, vec![ia.device_id()?, target.clone()])?;
    let admin = ia.fixture_admin();
    let networked = || crate::net::NetworkConfig {
        bind: Some("127.0.0.1:0".parse().expect("address")),
        ..Default::default()
    };
    let (a, dir_a) = node_at(ia, networked()).await?;
    let (b, dir_b) = node_at(ib, networked()).await?;
    let result = async {
        for node in [&a, &b] {
            node.install_membership(membership.clone()).await?;
        }
        let network = b.network_id().await?;
        bot(&b, &admin, "c", "前端", &[]).await?;
        let mut requests = answering_app_server(&b).await?;
        connected(&b).await;
        running_work(&b, &admin, "w-far").await?;

        // B is offline and the stop is past its deadline.
        a.start_network().await?;
        let at = now_ms();
        let stop = crate::work::StopRequest {
            work_id: "w-far".into(),
            conversation: "c".into(),
            target: target.clone(),
            requested_at_ms: at - 60_000,
            deadline_ms: at - 30_000,
            timed_out: true,
        };
        {
            let request = (stop).to_owned();
            a.store
                .call("triage.test.save_stop_request", move |store| {
                    store.save_stop_request(&request)
                })
                .await
        }?;
        a.deliver_work_requests(&network).await?;
        assert!(
            held_stop(&a, "w-far")
                .await?
                .is_some_and(|held| held.timed_out),
            "an unconfirmed stop is never given up"
        );

        b.start_network().await?;
        a.net()?.learn(&b.net()?.addr()).await;
        // Retry by hand: the backoff from the offline attempts must not outlast the timeout.
        let interrupt = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                a.stop_retries.lock().unwrap().clear();
                a.deliver_work_requests(&network).await?;
                let Ok(request) =
                    tokio::time::timeout(Duration::from_millis(500), requests.recv()).await
                else {
                    continue;
                };
                let request = request.context("the App Server closed")?;
                if request["method"] == "turn/interrupt" {
                    return Ok::<_, anyhow::Error>(request);
                }
            }
        })
        .await??;
        assert_eq!(
            interrupt["params"],
            json!({"threadId":"t-9","turnId":"u-1"})
        );
        while held_stop(&a, "w-far").await?.is_some() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // B records the stop once the App Server has taken the interrupt, which the
        // fixture answers before reporting it here.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !matches!(
                state(&b, "w-far").await?,
                crate::work::WorkState::Interrupting { .. }
            ) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("B did not record the interrupt")??;
        assert!(
            held_stop(&b, "w-far").await?.is_some(),
            "B holds the stop it took"
        );

        let ended = crate::work::Signal::TurnCompleted {
            turn_id: "u-1".into(),
            status: vibeface_bridge::turn::TurnStatus::Interrupted,
            text: String::new(),
            error: None,
        };
        b.signal_work(&network, "w-far", ended).await?;
        a.sync_peer(&network, &target).await?;
        assert!(
            {
                let network = (network).to_owned();
                a.store
                    .call("triage.test.works", move |store| store.works(&network, "c"))
                    .await
            }?
            .iter()
            .any(|work| work.work_id == "w-far" && work.outcome.is_some())
        );
        {
            let request = (stop).to_owned();
            a.store
                .call("triage.test.save_stop_request", move |store| {
                    store.save_stop_request(&request)
                })
                .await
        }?;
        a.stop_retries.lock().unwrap().clear();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                a.deliver_work_requests(&network).await?;
                if held_stop(&a, "w-far").await?.is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("terminal work drains the asynchronous stop worker")??;
        assert!(held_stop(&a, "w-far").await?.is_none());
        b.stop_retries.lock().unwrap().clear();
        b.reconcile_works(&network).await?;
        assert_eq!(
            methods(&mut requests),
            [] as [&str; 0],
            "an ended work gets no stop"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    for node in [&a, &b] {
        node.stop_network().await;
    }
    cleanup(a, dir_a).await?;
    cleanup(b, dir_b).await?;
    result
}

#[tokio::test]
async fn deleting_a_bot_stops_the_works_it_dispatched() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        bot(&node, &admin, "d", "后端", &[]).await?;
        let network = node.network_id().await?;
        let local = node.identity.device_id()?;
        let spec = |work_id: &str, conversation: &str| crate::work::WorkSpec {
            work_id: work_id.into(),
            conversation: conversation.into(),
            call_ref: "r".into(),
            thread: Some(format!("t-{work_id}")),
            ..provider_work(Provider::Codex, &local, Path::new("/w"), "p")
        };
        for (work_id, conversation) in [("w-d", "d"), ("w-c", "c")] {
            {
                let identity = (node.identity).to_owned();
                let network = (network).to_owned();
                let spec = spec(work_id, conversation);
                node.store
                    .call("triage.test.accept_work_fixture", move |store| {
                        store.accept_work_fixture(&identity, &network, spec, now_ms())
                    })
                    .await
            }?;
        }
        let hash = delete_bot(&node, &admin, "d").await?;
        node.bot_deleted(&network, "d", &hash);
        let late = {
            let identity = (node.identity).to_owned();
            let network = (network).to_owned();
            let spec = spec("w-late", "d");
            node.store
                .call("triage.test.accept_work_fixture", move |store| {
                    store.accept_work_fixture(&identity, &network, spec, now_ms())
                })
                .await
        };
        assert!(
            late.is_err(),
            "a dispatch that arrives after the deletion does not start"
        );
        let stopping = |requests: &[crate::work::StopRequest], work: &str| {
            requests.iter().any(|request| request.work_id == work)
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let held = loop {
            let held = {
                node.store
                    .call("triage.test.stop_requests", move |store| {
                        store.stop_requests()
                    })
                    .await
            }?;
            if stopping(&held, "w-d") || tokio::time::Instant::now() > deadline {
                break held;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(
            stopping(&held, "w-d"),
            "the deleted bot's work is being stopped"
        );
        assert!(!stopping(&held, "w-c"), "another bot's work keeps running");
        Ok::<_, anyhow::Error>(())
    })
    .await
}
