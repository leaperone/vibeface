use super::super::test_support::{
    answer, cleanup, external_work_thread_fact, turn, unanswered, with_agent, with_node,
};
use super::super::*;
use crate::crypto::LocalIdentity;
use crate::identity::Member;
use crate::runtime::test_support::{bot, node, node_at, node_with};

#[test]
fn decisions_reach_a_node_without_public_https_only_through_the_default_gateway() -> Result<()> {
    let local = "http://127.0.0.1:4792";
    assert_eq!(
        decision_url(local, &[])?,
        "http://127.0.0.1:4792/api/decision"
    );
    let other = ("ab".repeat(32), "https://other.example".to_owned());
    let anchor = (
        net::mesh::default_bootstrap_id()?,
        "https://node.vibeface.dev:4792".to_owned(),
    );
    assert_eq!(
        decision_url(local, std::slice::from_ref(&other))?,
        "http://127.0.0.1:4792/api/decision",
        "another device's gateway never receives the decision token"
    );
    assert_eq!(
        decision_url(local, &[other.clone(), anchor.clone()])?,
        "https://node.vibeface.dev:4792/api/decision"
    );
    assert_eq!(
        decision_url("https://192.168.1.2", &[other.clone(), anchor])?,
        "https://node.vibeface.dev:4792/api/decision",
        "a private HTTPS URL is no public answer"
    );
    assert_eq!(
        decision_url("https://node.example/", &[other])?,
        "https://node.example/api/decision"
    );
    Ok(())
}

#[tokio::test]
async fn decision_tokens_answer_questions_once_and_are_revocable() -> Result<()> {
    with_node(async |node, admin, dir| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &["s1"]).await?;
        external_work_thread_fact(&node, &admin, "s1").await?;
        let created = node.create_decision_token("grok").await?;
        let token = created["token"].as_str().unwrap().to_owned();
        let stored = std::fs::read_to_string(dir.join(DECISION_TOKENS))?;
        assert!(
            !stored.contains(&token) && stored.contains(&crate::crypto::digest(token.as_bytes()))
        );
        assert!(!node.triage_settings().await?.to_string().contains(&token));
        assert!(node.decision_token_valid(&token) && !node.decision_token_valid("vfd_wrong"));
        let event = node.ingest_agent_event(turn("s1", "t1", "/w")).await?;
        let raised = node
            .triage_command(json!({"action":"raise","event_id":event["event_id"],
                "severity":"attention","question":"要修吗？","choices":["修","不修"]}))
            .await?;
        let device = node.identity.device_id()?;
        let question = raised["question_id"].as_str().unwrap().to_owned();
        let decision = json!({"device_id":device,"question_id":question,"choice":0});
        let stranger = LocalIdentity::generate().device_id()?;
        let refused = tokio::time::timeout(
            Duration::from_secs(1),
            node.decision_http(
                "not-the-target-token",
                json!({"device_id":stranger,"question_id":question,"choice":0}),
            ),
        )
        .await
        .context("a nonmember is refused before any dial")?
        .unwrap_err();
        let network = node.network_id().await?;
        let expected = {
            let network_id = (network).to_owned();
            let device_id = (stranger).to_owned();
            node.store
                .call("triage.test.authorize_node", move |store| {
                    store.authorize_node(&network_id, &device_id)
                })
                .await
        }
        .unwrap_err();
        assert_eq!(refused.to_string(), expected.to_string());
        let removed = LocalIdentity::generate();
        let removed_id = removed.device_id()?;
        let mut state = node.membership_state().await?;
        state.append(crate::identity::GovernanceEvent::grant(
            &admin,
            &state.project()?,
            Member::node(&removed, "removed")?,
        )?)?;
        state.append(crate::identity::GovernanceEvent::revoke(
            &admin,
            &state.project()?,
            crate::identity::Revocation::new(&removed_id, None)?,
        )?)?;
        node.install_membership(state).await?;
        let refused = node
            .decision_http(
                "not-the-target-token",
                json!({"device_id":removed_id,"question_id":question,"choice":0}),
            )
            .await
            .unwrap_err();
        let expected = {
            let network_id = (network).to_owned();
            let device_id = (removed_id).to_owned();
            node.store
                .call("triage.test.authorize_node", move |store| {
                    store.authorize_node(&network_id, &device_id)
                })
                .await
        }
        .unwrap_err();
        assert_eq!(
            refused.to_string(),
            expected.to_string(),
            "a revoked target is rejected before dialing"
        );
        let refused = node
            .decision_http("vfd_wrong", decision.clone())
            .await
            .unwrap_err();
        assert!(format!("{refused:#}").contains("invalid decision token"));
        let answered = node.decision_http(&token, decision.clone()).await?;
        assert_eq!(
            answered,
            json!({"question_id":question,"answer_id":format!("a-{question}"),
            "status":"answered","text":"修"})
        );
        assert_eq!(
            node.decision_http(&token, decision).await?,
            answered,
            "a retry reads back"
        );
        let other = node
            .decision_http(
                &token,
                json!({"device_id":device,"question_id":question,"text":"不修"}),
            )
            .await?;
        assert_eq!(
            (other["status"].clone(), other["text"].clone()),
            (json!("already_answered"), json!("修"))
        );
        assert!(unanswered(&node).await?.is_empty());
        node.revoke_decision_token(created["token_id"].as_str().unwrap())
            .await?;
        assert!(!node.decision_token_valid(&token));
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_public_node_forwards_decisions_to_any_device_and_the_target_checks_the_token()
-> Result<()> {
    let (it, ip) = (LocalIdentity::generate(), LocalIdentity::generate());
    let (target, public_id) = (it.device_id()?, ip.device_id()?);
    // Two personal networks: the public node is no member of the target's.
    let own = it.sign_network_fixture(1, vec![target.clone()])?;
    let admin = it.fixture_admin();
    let elsewhere = ip.sign_network_fixture(1, vec![public_id.clone()])?;
    let networked = || crate::net::NetworkConfig {
        bind: Some("127.0.0.1:0".parse().unwrap()),
        ..Default::default()
    };
    let (t, dir_t) = node_at(it, networked()).await?;
    let (p, dir_p) = node_with(ip, networked(), |config| config.forwarding.any = true).await?;
    let result = async {
        t.install_membership(own).await?;
        p.install_membership(elsewhere).await?;
        with_agent(&t);
        bot(&t, &admin, "c", "前端", &["s1"]).await?;
        external_work_thread_fact(&t, &admin, "s1").await?;
        let created = t.create_decision_token("grok").await?;
        let token = created["token"].as_str().unwrap().to_owned();
        let event = t.ingest_agent_event(turn("s1", "t1", "/w")).await?;
        let raised = t
            .triage_command(json!({"action":"raise","event_id":event["event_id"],
                "severity":"attention","question":"要修吗？","choices":["修","不修"]}))
            .await?;
        let question = raised["question_id"].as_str().unwrap().to_owned();
        let decision = json!({"device_id":target,"question_id":question,"choice":0});
        for node in [&t, &p] {
            node.start_network().await?;
        }
        p.net()?.learn(&t.net()?.addr()).await;
        // A member Node still forwards only within its own personal network.
        let refused = t
            .decision_http(
                &token,
                json!({"device_id":public_id,"question_id":question,"choice":0}),
            )
            .await
            .unwrap_err();
        let network = t.network_id().await?;
        let expected = {
            let public_id = public_id.clone();
            t.store
                .call("triage.test.authorize_forward_target", move |store| {
                    store.authorize_node(&network, &public_id)
                })
                .await
                .unwrap_err()
        };
        assert_eq!(refused.to_string(), expected.to_string());
        // The public node dials the target without a membership check; the target alone
        // judges the token, and `/api/decision` turns its refusal into 401.
        let refused = p
            .decision_http("vfd_wrong", decision.clone())
            .await
            .unwrap_err();
        assert!(
            format!("{refused:#}").contains("invalid decision token"),
            "{refused:#}"
        );
        let answered = p.decision_http(&token, decision).await?;
        assert_eq!(
            answered,
            json!({"question_id":question,"answer_id":format!("a-{question}"),
            "status":"answered","text":"修"})
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    for node in [&t, &p] {
        node.stop_network().await;
    }
    cleanup(t, dir_t).await?;
    cleanup(p, dir_p).await?;
    result
}

#[tokio::test]
async fn events_dedupe_and_questions_go_once_to_the_owner_and_signed_webhooks() -> Result<()> {
    async fn deliver_until(
        node: &Runtime,
        received: &std::sync::Mutex<Vec<(axum::http::HeaderMap, String)>>,
        expected: usize,
    ) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                node.deliver_webhooks().await?;
                let count = received.lock().unwrap().len();
                if count >= expected {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("queued webhook did not reach the fixture receiver")?
    }

    let (node, admin, dir) = node().await?;
    let received = Arc::new(std::sync::Mutex::new(
        Vec::<(axum::http::HeaderMap, String)>::new(),
    ));
    let sink = received.clone();
    let log = received.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/hook", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |headers: axum::http::HeaderMap, body: String| {
                let sink = sink.clone();
                async move {
                    sink.lock().unwrap().push((headers, body));
                    axum::http::StatusCode::ACCEPTED
                }
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });
    let result = async {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &["s1", "s2"]).await?;
        external_work_thread_fact(&node, &admin, "s1").await?;
        let secret = "fixture-webhook-secret-0123456789";
        let project = dir.join("hooked");
        let project_text = project.to_string_lossy();
        let elsewhere = dir.join("elsewhere");
        let settings = node
            .configure_triage(json!({"default_outlet":"record","projects":[{"prefix":project_text,"outlet":"both"}],
                "webhook_url":url,"scan_codex_sessions":false,"webhook_secret":secret}))
            .await?;
        assert_eq!(settings["webhook_secret_set"], true);
        assert!(!settings.to_string().contains(secret));
        assert!(!std::fs::read_to_string(dir.join("config.json"))?.contains(secret));

        let first = node.ingest_agent_event(turn("s1", "t1", &format!("{project_text}/app"))).await?;
        assert_eq!(first["status"], "accepted");
        let scanned = HookEvent { source: triage::Source::SessionScan, status: Some("completed".into()), ..turn("s1", "t1", &format!("{project_text}/app")) };
        assert_eq!(node.ingest_agent_event(scanned).await?["status"], "duplicate");
        let test = node.ingest_agent_event(HookEvent { test: true, ..turn("s9", "t9", "/x") }).await?;
        assert_eq!(test["test"], true);
        node.ingest_agent_event(turn("s1", "t2", &format!("{project_text}/app"))).await?;
        node.ingest_agent_event(turn("s2", "t1", elsewhere.to_string_lossy().as_ref())).await?;
        let pending = node.triage_command(json!({"action":"events"})).await?;
        assert_eq!(pending["events"].as_array().unwrap().len(), 3);

        let event_id = first["event_id"].as_str().unwrap();
        let raise = json!({"action":"raise","event_id":event_id,"severity":"urgent","question":"测试失败，要修吗？","choices":["修","不修"]});
        let raised = node.triage_command(raise.clone()).await?;
        assert_eq!((raised["asked"].clone(), raised["queued_webhook"].clone()), (json!(true), json!(true)));
        assert_eq!(node.triage_command(raise).await?, raised, "a repeated raise answers the same");
        let pending = node.triage_command(json!({"action":"events"})).await?;
        let rest = |session: &str| {
            pending["events"].as_array().unwrap().iter().find(|e| e["session_id"] == session).unwrap()["event_id"].clone()
        };
        let same_session = node.triage_command(json!({"action":"raise","event_id":rest("s1"),"severity":"info","question":"again"})).await?;
        assert_eq!((same_session["asked"].clone(), same_session["question_id"].clone()), (json!(false), raised["question_id"].clone()),
            "an unanswered question about the session stands");
        let quiet = node.triage_command(json!({"action":"dismiss","event_id":rest("s2")})).await?;
        assert_eq!(quiet["dismissed"], true);
        assert!(node.triage_command(json!({"action":"raise","event_id":"unknown","severity":"info","question":"x"})).await.is_err());
        assert_eq!(node.triage_command(json!({"action":"events"})).await?["events"], json!([]));
        let asked = unanswered(&node).await?;
        assert_eq!(asked.len(), 1);
        assert_eq!((asked[0]["severity"].clone(), asked[0]["text"].clone(), asked[0]["conversation_id"].clone()),
            (json!("urgent"), json!("测试失败，要修吗？"), json!("c")));

        deliver_until(&node, &received, 1).await?;
        node.deliver_webhooks().await?;
        assert_eq!(received.lock().unwrap().len(), 1, "the question is delivered once before its answer");
        let question = raised["question_id"].as_str().unwrap();
        assert_eq!(answer(&node, question, "修").await?, "answered");
        node.send_answered().await?;
        node.send_answered().await?;
        deliver_until(&node, &received, 2).await?;
        node.deliver_webhooks().await?;
        let received = received.lock().unwrap().clone();
        assert_eq!(received.len(), 2, "the question and its answer, once each");
        let (headers, body) = &received[0];
        let timestamp: i64 = headers["x-vibeface-timestamp"].to_str()?.parse()?;
        assert_eq!(headers["x-vibeface-event-id"], question);
        assert_eq!(headers["x-vibeface-signature"].to_str()?, triage::signature(secret.as_bytes(), timestamp, body));
        let body: Value = serde_json::from_str(body)?;
        assert_eq!((body["type"].clone(), body["choices"][1].clone()), (json!("question"), json!({"index":1,"label":"不修"})));
        assert_eq!(body["answer"], json!({"url":node.decision_url().await?,"device_id":node.identity.device_id()?,"question_id":question}));
        let answered: Value = serde_json::from_str(&received[1].1)?;
        let network = node.network_id().await?;
        let hash = {
            let network = (network).to_owned();
            let message_id = (format!("a-{question}")).to_owned();
            node.store.call("triage.test.message", move |store| {
                store.message(&network, "c", &message_id)
            }).await
        }?.context("answer")?.event_hash;
        assert_eq!(answered, json!({"schema_version":1,"type":"answered","event_id":format!("answered-{question}-{}", &hash[..16]),
            "question_id":question,"text":"修"}));
        assert_eq!(node.triage_settings().await?["outbox"]["delivered"], 2);

        let winner = "0".repeat(64);
        {
            let actor_network = network.clone();
            let actor_question = question.to_owned();
            let winner = winner.clone();
            node.store.call("triage.test.sql", move |store| {
                Ok(store.connection.execute(
                        "UPDATE messages SET text='不修',event_hash=?1 WHERE network_id=?2 AND message_id=?3",
                        params![winner, actor_network, format!("a-{actor_question}")],
                    )?)
            }).await
        }?;
        node.send_answered().await?;
        node.send_answered().await?;
        deliver_until(&node, &log, 3).await?;
        node.deliver_webhooks().await?;
        let received = log.lock().unwrap().clone();
        assert_eq!(received.len(), 3, "the winning answer follows once");
        let corrected: Value = serde_json::from_str(&received[2].1)?;
        assert_eq!((corrected["event_id"].clone(), corrected["text"].clone()),
            (json!(format!("answered-{question}-{}", &winner[..16])), json!("不修")));
        assert_eq!(node.test_webhook().await?["ok"], true);

        node.store.call("triage.test.outbox_history", move |store| {

            for index in 0..128 {
                store.connection.execute("INSERT INTO triage_outbox(event_id,body,state,attempts,next_ms) VALUES(?,'history',?,0,0)",
                    params![format!("history-{index}"), if index % 2 == 0 { "delivered" } else { "dead" }])?;
            }
            let now = now_ms();
            store.connection.execute("INSERT INTO triage_outbox(event_id,body,state,attempts,next_ms) VALUES('future','later','pending',0,?1),('due','now','pending',0,?2)",
                params![now + 60_000, now])?;
            let due = outbox_from(&store.connection, now)?;
            assert_eq!(due.len(), 1);
            assert_eq!(due[0].event_id, "due");
            assert_eq!(store.connection.query_row("SELECT count(*) FROM triage_outbox WHERE event_id LIKE 'history-%'", [], |row| row.get::<_, usize>(0))?, 128);
            Ok::<_, anyhow::Error>(())
        }).await
    }
    .await;
    server.abort();
    cleanup(node, dir).await?;
    result
}
