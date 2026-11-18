use super::super::test_support::{
    answered_once, app_server, attributions, connected, delete_bot, main_input_fact, owner_of,
    state, turn, unanswered, version_of, with_agent, with_node,
};
use super::super::*;
use crate::runtime::test_support::{
    bare_node, bot, provider_work, scripted_app_server, start_reply, start_reply_in,
};
use vibeface_bridge::failure::FailureKind;

#[tokio::test]
async fn a_node_without_a_network_skips_the_session_scan() -> Result<()> {
    let (node, dir) = bare_node().await?;
    let result = node.scan_sources().await;
    node.shutdown().await;
    drop(node);
    std::fs::remove_dir_all(dir)?;
    result
}

#[test]
fn shared_events_keep_relative_context_without_local_identity() {
    let private_root =
        std::env::temp_dir().join(format!("vibeface-private-event-{}", uuid::Uuid::new_v4()));
    let private_project = private_root.join("project");
    let private_project_text = private_project.to_string_lossy();
    let event = SealedAgentEvent::seal(
        HookEvent {
            repo: Some(private_project_text.to_string()),
            last_message: format!("failed in {private_project_text}/src"),
            ..turn("session", "turn", &private_project_text)
        },
        "node",
        "private-host",
        1,
    )
    .unwrap();
    let shared = shared_event(&event, std::slice::from_ref(&private_root));
    assert_eq!(shared.observation().cwd, "workspace-1/project");
    assert!(
        serde_json::to_value(&shared).unwrap()["host"] == "" && shared.observation().repo.is_none()
    );
    assert!(
        !shared
            .observation()
            .last_message
            .contains(private_root.to_string_lossy().as_ref())
    );
    assert_eq!(event.observation().cwd, private_project_text);
    assert_eq!(shared.event_id(), event.event_id());
    assert_eq!(shared_event(&event, &[]).observation().cwd, "workspace");
}

#[tokio::test]
async fn a_deleted_bots_sessions_go_to_the_primary_bot() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        bot(&node, &admin, "d", "后端", &[]).await?;
        bot(&node, &admin, "e", "运维", &[]).await?;
        let network = node.network_id().await?;
        for session in ["s1", "s2", "s3"] {
            node.ingest_agent_event(turn(session, "t1", "/w")).await?;
        }
        for (from, session, to) in [
            ("c", "s1", Some("d")),
            ("c", "s2", Some("d")),
            ("d", "s2", None),
            ("c", "s3", Some("e")),
        ] {
            let seen = version_of(&node, session).await?;
            {
                let identity = (node.identity).to_owned();
                let network = (network).to_owned();
                let from = (from).to_owned();
                let session = (session).to_owned();
                let seen = (seen).to_owned();
                let target = to.map(str::to_owned);
                node.store.call("triage.test.hand_over_session", move |store| {
                    store.hand_over_session(&identity, &network, &from, &session, &seen, target.as_deref(), now_ms())
                }).await
            }?;
        }
        let queued = node.ingest_agent_event(turn("s1", "t2", "/w")).await?;
        assert_eq!(queued["status"], "accepted");
        delete_bot(&node, &admin, "d").await?;

        assert_eq!(owner_of(&node, "s3").await?.as_deref(), Some("e"), "another bot's session stays");
        assert_eq!(owner_of(&node, "s1").await?, None, "a deleted bot follows nothing");
        let raise = json!({"action":"raise","event_id":queued["event_id"],"severity":"attention","question":"测试失败，要修吗？"});
        assert_eq!(
            node.triage_command(raise).await?["conversation_id"], "c",
            "an event queued before the deletion asks in the primary bot's chat"
        );
        let spoke = node.ingest_agent_event(turn("s2", "t2", "/w")).await?;
        assert_eq!(spoke["status"], "accepted", "a deleted bot's mute no longer holds");
        for (session, step) in [("s1", "t3"), ("s3", "t2")] {
            node.ingest_agent_event(turn(session, step, "/w")).await?;
        }
        assert_eq!(owner_of(&node, "s1").await?.as_deref(), Some("c"));
        assert_eq!(owner_of(&node, "s2").await?.as_deref(), Some("c"));
        assert_eq!(owner_of(&node, "s3").await?.as_deref(), Some("e"));
        assert_eq!(attributions(&node, "c").await?, 5, "each session goes back to the primary bot once");

        delete_bot(&node, &admin, "c").await?;
        assert_eq!({
            let network = (network).to_owned();
            node.store.call("triage.test.primary_bot", move |store| {
                store.primary_bot(&network)
            }).await
            }?.as_deref(), Some("e"));
        node.ingest_agent_event(turn("s1", "t4", "/w")).await?;
        assert_eq!(
            owner_of(&node, "s1").await?.as_deref(),
            Some("e"),
            "with the primary bot deleted, its sessions go to the earliest bot left"
        );
        assert_eq!(attributions(&node, "e").await?, 2);
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn a_deleted_bots_work_thread_goes_to_the_primary_bot() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        bot(&node, &admin, "d", "后端", &[]).await?;
        let network = node.network_id().await?;
        let local = node.identity.device_id()?;
        let trigger = main_input_fact(&node, &admin, "d", "m1").await?;
        let spec = crate::work::WorkSpec {
            work_id: "w-d".into(),
            conversation: "d".into(),
            trigger,
            call_ref: "r".into(),
            thread: Some("wt".into()),
            ..provider_work(Provider::Codex, &local, Path::new("/w"), "p")
        };
        {
            let identity = (node.identity).to_owned();
            let network = (network).to_owned();
            node.store.call("triage.test.accept_work_fixture", move |store| {
                store.accept_work_fixture(&identity, &network, spec, now_ms())
            }).await
        }?;
        let queued = node.ingest_agent_event(turn("wt", "t1", "/w")).await?;
        assert_eq!(queued["status"], "accepted");
        assert_eq!(attributions(&node, "d").await?, 0, "a work's thread is its bot's at once");
        node.bind_work(
            &network,
            "d",
            "w-d",
            vibeface_bridge::codex::StartReport::Rejected(
                vibeface_bridge::failure::ExecFailure::new(
                    FailureKind::Rejected,
                    "native start was rejected before Bot deletion",
                ),
            ),
        )
        .await?;
        assert_eq!(
            state(&node, "w-d").await?,
            crate::work::WorkState::Finished {
                outcome: crate::model::WorkOutcome::Rejected,
            }
        );
        delete_bot(&node, &admin, "d").await?;

        let raise = json!({"action":"raise","event_id":queued["event_id"],"severity":"attention","question":"测试失败，要修吗？"});
        assert_eq!(
            node.triage_command(raise).await?["conversation_id"], "c",
            "a deleted bot's work thread asks in the primary bot's chat"
        );
        let next = node.ingest_agent_event(turn("wt", "t2", "/w")).await?;
        assert_eq!(next["status"], "accepted");
        assert_eq!(owner_of(&node, "wt").await?.as_deref(), Some("c"));
        assert_eq!(attributions(&node, "c").await?, 1, "the thread goes to the primary bot once");
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn unattended_sessions_get_no_owner() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        let (mut server, handed) = app_server(&node).await?;
        node.on_codex_request(&handed).await;
        assert_eq!(
            answered_once(&mut server).await["result"],
            json!({"decision":"decline"})
        );
        let status = node.ingest_agent_event(turn("s1", "t1", "/w")).await?;
        assert_eq!(status["status"], "foreign");
        assert_eq!(owner_of(&node, "s1").await?, None);

        bot(&node, &admin, "c", "前端", &[]).await?;
        node.triage_app_server(&json!({"method":"thread/started","params":{"thread":
            {"id":"sub","cwd":"/w","source":{"subAgent":"review"}}}}));
        let status = node.ingest_agent_event(turn("sub", "t1", "/w")).await?;
        assert_eq!(status["status"], "foreign");
        assert_eq!(owner_of(&node, "sub").await?, None);
        let status = node.ingest_agent_event(turn("s1", "t2", "/w")).await?;
        assert_eq!(status["status"], "accepted");
        assert_eq!(owner_of(&node, "s1").await?.as_deref(), Some("c"));
        assert_eq!(attributions(&node, "c").await?, 1);
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn app_server_turns_recover_only_their_missing_thread_metadata() -> Result<()> {
    for case in [
        "missing-start",
        "known",
        "pressure",
        "unknown-source",
        "wrong-thread",
        "missing-thread",
        "missing-source",
        "invalid-source",
        "missing-cwd",
        "invalid-cwd",
        "empty-cwd",
        "oversized-cwd",
        "failed-read",
        "background",
    ] {
        with_node(async |node, admin, _| {
            with_agent(&node);
            bot(&node, &admin, "c", "Metadata", &[]).await?;
            let mut queries = scripted_app_server(&node, move |request| {
                if request["method"] != "thread/read" {
                    return json!({"result":{}});
                }
                let mut thread = json!({"id":"metadata-thread","cwd":"/native/project","source":"cli"});
                match case {
                    "wrong-thread" => {
                        thread["id"] = json!("another-thread");
                        thread["source"] = json!({"subAgent":"review"});
                    }
                    "missing-thread" => thread = Value::Null,
                    "missing-source" => { thread.as_object_mut().unwrap().remove("source"); }
                    "invalid-source" => thread["source"] = json!(42),
                    "missing-cwd" => { thread.as_object_mut().unwrap().remove("cwd"); }
                    "invalid-cwd" => thread["cwd"] = json!(42),
                    "empty-cwd" => thread["cwd"] = json!(""),
                    "oversized-cwd" => thread["cwd"] = json!("x".repeat(4097)),
                    "failed-read" => return json!({"error":{"code":-32000,"message":"unreadable"}}),
                    "background" => thread["source"] = json!({"subAgent":"review"}),
                    _ => {}
                }
                json!({"result":{"thread":thread}})
            }).await?;
            tokio::time::timeout(Duration::from_secs(5), connected(&node)).await?;
            if matches!(case, "known" | "pressure" | "unknown-source") {
                let source = if case == "unknown-source" { Value::Null } else { json!("cli") };
                node.triage_app_server(&json!({"method":"thread/started","params":{"thread":
                    {"id":"metadata-thread","cwd":"/known/project","source":source}}}));
            }
            if case == "pressure" {
                node.triage_app_server(&json!({"method":"item/completed","params":{"threadId":"metadata-thread",
                    "item":{"type":"agentMessage","text":"answer awaiting its turn-end"}}}));
                for index in 0..1100 {
                    let id = format!("background-{index}");
                    node.triage_app_server(&json!({"method":"thread/started","params":{"thread":
                        {"id":id,"cwd":"/background","source":{"subAgent":"review"}}}}));
                    node.triage_app_server(&json!({"method":"item/completed","params":{"threadId":id,
                        "item":{"type":"agentMessage","text":"unconsumed background answer"}}}));
                }
                assert_eq!(node.ingest_agent_event(turn("background-0", "turn", "/background")).await?["status"], "foreign");
            }
            node.triage_app_server(&json!({"method":"turn/completed","params":{
                "threadId":"metadata-thread","turn":{"id":"metadata-turn","status":"completed","items":[]}}}));
            let unverified = matches!(case, "wrong-thread" | "missing-thread" | "missing-source" | "invalid-source" | "failed-read");
            let queued = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if unverified && lock(&node.triage.threads).get("metadata-thread").is_some_and(|thread| thread.pending == 0) {
                        return Ok::<Option<SealedAgentEvent>, anyhow::Error>(None);
                    }
                    if case == "background" && lock(&node.triage.threads).get("metadata-thread").is_some_and(|thread| thread.background == Some(true)) {
                        return Ok::<Option<SealedAgentEvent>, anyhow::Error>(None);
                    }
                    if let Some(event) = {
                        node.store.call("triage.test.triage_events", move |store| {
                            store.triage_events(None)
                        }).await
                    }?.into_iter().next() {
                        return Ok(Some(event));
                    }
                    tokio::task::yield_now().await;
                }
            }).await??;
            if unverified {
                assert!(queued.is_none());
                assert_eq!(owner_of(&node, "metadata-thread").await?, None);
                assert_eq!(lock(&node.triage.threads).get("metadata-thread").and_then(|thread| thread.background), None);
            } else if case == "background" {
                assert!(queued.is_none());
                assert_eq!(owner_of(&node, "metadata-thread").await?, None);
                assert_eq!(node.ingest_agent_event(turn("metadata-thread", "metadata-turn", "/native/project")).await?["status"], "foreign");
            } else {
                let queued = queued.context("queued native event")?;
                assert_eq!(queued.observation().cwd, match case { "missing-start" => "/native/project", "known" | "pressure" => "/known/project", "unknown-source" => "/native/project", _ => "" });
                assert_eq!(queued.observation().session_id, "metadata-thread");
                assert_eq!(queued.observation().turn_id, "metadata-turn");
                assert_eq!(queued.observation().source, triage::Source::AppServer);
                if case == "pressure" {
                    assert_eq!(queued.observation().last_message, "answer awaiting its turn-end");
                    assert_eq!(lock(&node.triage.threads).get("background-0").and_then(|thread| thread.background), Some(true));
                }
                assert_eq!(node.ingest_agent_event(turn("metadata-thread", "metadata-turn", "/native/project")).await?["status"], "duplicate");
                assert_eq!({
                    node.store.call("triage.test.triage_events", move |store| {
                        store.triage_events(None)
                    }).await
                    }?.len(), 1);
            }
            let queries: Vec<_> = std::iter::from_fn(|| queries.try_recv().ok()).collect();
            let reads: Vec<_> = queries.iter().filter(|query| query["method"] == "thread/read").collect();
            if matches!(case, "known" | "pressure") {
                assert!(reads.is_empty());
            } else {
                assert_eq!(reads.len(), 1);
                assert_eq!(reads[0]["params"], json!({"threadId":"metadata-thread","includeTurns":false}));
            }
            assert!(!queries.iter().any(|query| query["method"].as_str().is_some_and(|method| method.starts_with("turn/"))));
            Ok::<_, anyhow::Error>(())
        })
        .await.with_context(|| case)?;
    }
    Ok(())
}

#[tokio::test]
async fn a_check_from_another_app_server_stays_pending() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        bot(&node, &admin, "d", "后端", &[]).await?;
        let mut requests = scripted_app_server(&node, |request| match request["method"].as_str() {
            Some("thread/read") => json!({"result":{"thread":{"turns":[
                {"id":"turn","status":"completed","items":[]}]}}}),
            _ => json!({"result":{}}),
        })
        .await?;
        tokio::time::timeout(Duration::from_secs(5), connected(&node)).await?;
        let network = node.network_id().await?;
        assert_eq!(node.codex.server(), "system");
        let mut foreign_attempt = String::new();
        for (scope, server) in [("c", "own:"), ("d", "system")] {
            let (trigger, attempt) =
                start_reply_in(&node, &admin, scope, &format!("{scope}-m1")).await?;
            if server == "own:" {
                foreign_attempt = attempt.clone();
            }
            let identity = node.identity.clone();
            let network = network.clone();
            node.store
                .call("triage.test.record_lost_manager", move |store| {
                    store.record_manager_turn(
                        &attempt,
                        server,
                        "thread",
                        Some("turn"),
                        &Default::default(),
                    )?;
                    let interrupted = crate::scheduling::JobOutcome::Interrupted {
                        failure: vibeface_bridge::failure::ExecFailure::new(
                            vibeface_bridge::failure::FailureKind::Lost,
                            "lost",
                        ),
                        process_unconfirmed: true,
                    };
                    store.finish_job(
                        &identity,
                        &network,
                        scope,
                        &trigger,
                        &attempt,
                        interrupted,
                        "",
                        now_ms(),
                    )?;
                    Ok(())
                })
                .await?
        }
        node.reconcile_checks(&network).await?;
        node.reconcile_checks(&network).await?;
        let left = {
            node.store
                .call("triage.test.pending_checks", move |store| {
                    store.pending_checks()
                })
                .await
        }?;
        let servers: Vec<_> = left
            .iter()
            .filter_map(|check| check.codex_turn.as_ref())
            .map(|turn| turn.server.as_str())
            .collect();
        assert_eq!(
            servers,
            ["own:"],
            "only this App Server's confirmed terminal turn is settled"
        );
        assert_eq!(
            left[0].attempt_id, foreign_attempt,
            "the original foreign attempt stays pending"
        );
        let network = network.clone();
        node.store
            .call("triage.test.foreign_reconciliation", move |store| {
                let foreign = store
                    .scheduler_job("c", "message:c-m1")?
                    .context("foreign job")?;
                assert!(matches!(
                    foreign.state,
                    crate::scheduling::JobState::Reconciling { .. }
                ));
                assert!(store.message(&network, "c", &foreign.message_id)?.is_none());
                assert!(matches!(
                    store
                        .scheduler_job("d", "message:d-m1")?
                        .context("local job")?
                        .state,
                    crate::scheduling::JobState::Completed { .. }
                ));
                Ok(())
            })
            .await?;
        let sent: Vec<_> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
        assert_eq!(
            sent.iter()
                .filter(|request| request["method"] == "thread/read")
                .count(),
            1
        );
        assert!(
            sent.iter()
                .all(|request| request["method"] != "turn/start"
                    && request["method"] != "turn/steer"),
            "reconciliation never resends either prompt"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn a_bots_reply_thread_is_its_chats_and_takes_no_work() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        let network = node.network_id().await?;
        let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
        {
            let trigger = (trigger).to_owned();
            let attempt_id = (attempt).to_owned();
            node.store
                .call("triage.test.save_scheduler_native_id", move |store| {
                    store.save_scheduler_native_id(
                        "c",
                        &trigger,
                        &attempt_id,
                        "bot-thread",
                        "token",
                        false,
                    )
                })
                .await
        }?;
        let event = node
            .ingest_agent_event(turn("bot-thread", "t1", "/w"))
            .await?;
        assert_eq!(event["status"], "own");
        assert!(
            unanswered(&node).await?.is_empty(),
            "no question about a bot's own thread"
        );
        assert!(
            node.pending_events().await?.is_empty(),
            "a bot's own turn does not wake the Agent"
        );
        let held = turn("bot-thread", "request:r1", "/w");
        assert_eq!(
            node.queue_event(held, Some("r1".into())).await?["status"],
            "accepted"
        );

        let access = crate::runtime::AgentAccess::ReadOnly {
            network_id: network.clone(),
            conversation_id: "c".into(),
        };
        let access = node
            .manager_access_fixture(
                "token",
                access.clone(),
                &attempt,
                &trigger,
                crate::providers::Provider::Codex,
            )
            .await?;
        let args = json!({"thread":"bot-thread","call_ref":"r1","title":"t","prompt":"p"});
        let refused = node.tool(access, "track_work", args).await;
        node.finish_manager_attempt(&attempt).await;
        let refused = refused.expect_err("work into a bot's reply thread");
        assert!(
            refused.to_string().contains("own reply thread"),
            "{refused:#}"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[test]
fn thread_cache_evicts_only_completed_and_consumed_metadata() {
    let mut threads = HashMap::new();
    for index in 0..1024 {
        threads.insert(
            format!("consumed-{index}"),
            ThreadMetadata {
                completed: true,
                background: Some(false),
                touched_ms: index,
                ..Default::default()
            },
        );
    }
    threads.insert(
        "unconsumed".into(),
        ThreadMetadata {
            last_message: "must survive".into(),
            completed: true,
            ..Default::default()
        },
    );
    threads.insert(
        "background".into(),
        ThreadMetadata {
            cwd: "/background".into(),
            background: Some(true),
            ..Default::default()
        },
    );
    threads.insert(
        "ingesting".into(),
        ThreadMetadata {
            completed: true,
            pending: 1,
            ..Default::default()
        },
    );
    trim_threads(&mut threads);
    assert_eq!(threads.len(), 1024);
    assert_eq!(threads["unconsumed"].last_message, "must survive");
    assert_eq!(threads["background"].background, Some(true));
    assert_eq!(threads["background"].cwd, "/background");
    assert_eq!(threads["ingesting"].pending, 1);
}

#[tokio::test]
async fn an_unowned_session_goes_to_the_primary_bot_once() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        bot(&node, &admin, "d", "后端", &[]).await?;
        let network = node.network_id().await?;
        let local = node.identity.device_id()?;
        for step in ["t1", "t2"] {
            let queued = node.ingest_agent_event(turn("s1", step, "/w")).await?;
            assert_eq!(queued["status"], "accepted");
        }
        let again = node.ingest_agent_event(turn("s1", "t1", "/w")).await?;
        assert_eq!(again["status"], "duplicate");
        assert!(unanswered(&node).await?.is_empty(), "nobody is asked whose it is");
        assert_eq!(owner_of(&node, "s1").await?.as_deref(), Some("c"));
        assert_eq!((attributions(&node, "c").await?, attributions(&node, "d").await?), (1, 0));
        let lines = {
            let network = (network).to_owned();
            node.store.call("triage.test.attributed_sessions", move |store| {
                store.attributed_sessions(&network, "c")
            }).await
        }?;
        assert_eq!(
            (lines[0]["session_id"].clone(), lines[0]["summary"].clone(), lines[0]["muted"].clone()),
            (json!("s1"), json!("tests fail"), json!(false))
        );

        let set = admin.attest(
            &network,
            crate::model::BOTS_SCOPE,
            EventBody::PrimaryBotSet {
                conversation_id: "d".into(),
                attestation: None,
            },
        )?;
        {
            let identity = (node.identity).to_owned();
            let network_id = (network).to_owned();
            let body = set;
            let blobs = vec![];
            node.store.call("triage.test.append_event", move |store| {
                store.append_event(&identity, &network_id, crate::model::BOTS_SCOPE, body, blobs, now_ms())
            }).await
        }?;
        node.ingest_agent_event(turn("s1", "t3", "/w")).await?;
        node.ingest_agent_event(turn("s2", "t1", "/w")).await?;
        assert_eq!(owner_of(&node, "s1").await?.as_deref(), Some("c"), "an owner stands");
        assert_eq!(owner_of(&node, "s2").await?.as_deref(), Some("d"), "the next goes to the new primary");
        assert_eq!(attributions(&node, "c").await?, 1);

        let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
        let idle = crate::runtime::AgentAccess::ReadOnly { network_id: network.clone(), conversation_id: "c".into() };
        let live = node.manager_access_fixture(
            "token", idle.clone(), &attempt, &trigger, crate::providers::Provider::Codex,
        ).await?;
        let hand = |access: &crate::runtime::AgentAccess, session: &str, how: Value| {
            let (node, local, access, session) = (&node, &local, access.clone(), session.to_owned());
            async move {
                let seen = version_of(node, &session).await?;
                let mut args = json!({"node":local,"session_id":session,"version":seen});
                args.as_object_mut().unwrap().extend(how.as_object().unwrap().clone());
                node.tool(access, "hand_over_session", args).await
            }
        };
        let turn_result = async {
            assert!(
                hand(&idle, "s1", json!({"to":"后端"})).await.is_err(),
                "only a live bot turn hands a session over"
            );
            let theirs = hand(&live, "s2", json!({"to":"前端"})).await.expect_err("s2 is d's");
            assert!(theirs.to_string().contains("后端 follows it"), "{theirs:#}");
            assert!(hand(&live, "s1", json!({"to":"后端","stop_following":true})).await.is_err());

            let mute = json!({"stop_following":true});
            let muted = hand(&live, "s1", mute.clone()).await?;
            assert_eq!(muted["call_record"]["execution"], "applied");
            assert_eq!((muted["muted"].clone(), muted["changed"].clone()), (json!(true), json!(true)));
            assert_eq!(hand(&live, "s1", mute).await?["changed"], false, "a retried mute writes nothing");
            let quiet = node.ingest_agent_event(turn("s1", "t4", "/w")).await?;
            assert_eq!(quiet["status"], "muted");
            assert!(
                node.pending_events().await?.iter().all(|event| event.event_id().as_str() != quiet["event_id"]),
                "a muted session's events drop"
            );
            assert_eq!(attributions(&node, "c").await?, 1, "a muted session is not claimed again");

            let to_d = json!({"to":"后端"});
            let moved = hand(&live, "s1", to_d.clone()).await?;
            assert_eq!(
                (moved["conversation_id"].clone(), moved["muted"].clone(), moved["changed"].clone()),
                (json!("d"), json!(false), json!(true))
            );
            let retried = hand(&live, "s1", to_d).await?;
            assert_eq!(retried["call_record"]["execution"], "not_started");
            assert_eq!(retried["changed"], false, "a retried hand-over reads back what stands");
            assert_eq!(owner_of(&node, "s1").await?.as_deref(), Some("d"));
            assert_eq!(attributions(&node, "d").await?, 2, "each hand-over is written once");
            assert!({
                let network = (network).to_owned();
                node.store.call("triage.test.attributed_sessions", move |store| {
                    store.attributed_sessions(&network, "c")
                }).await
                }?.is_empty());
            assert!(
                hand(&live, "s1", json!({"stop_following":true})).await.is_err(),
                "a session handed over is no longer the sender's"
            );

            let accepted = node.ingest_agent_event(turn("s1", "t5", "/w")).await?;
            let raise = json!({"action":"raise","event_id":accepted["event_id"],"severity":"attention","question":"测试失败，要修吗？"});
            let raised = node.triage_command(raise).await?;
            assert_eq!(raised["conversation_id"], "d");
            let question = {
                let network = (network).to_owned();
                let question = (raised["question_id"].as_str().context("handed-over question")?).to_owned();
                node.store.call("triage.test.question", move |store| {
                    store.question(&network, &question)
                }).await
            }?.context("saved handed-over question")?;
            assert_eq!(question["target"], json!({"kind":"bot_main","bot_id":"d"}));
            Ok::<_, anyhow::Error>(())
        }
        .await;
        node.finish_manager_attempt(&attempt).await;
        turn_result
    })
    .await
}
