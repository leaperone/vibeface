use super::super::test_support::{
    Server, answer, answered_once, app_server, asks_permission, cleanup, handed, main_input_fact,
    read, running_work, turn, unanswered, with_agent, with_node,
};
use super::super::*;
use crate::config::Config;
use crate::crypto::LocalIdentity;
use crate::model::{AttentionKind, AttentionRequest, Severity};
use crate::runtime::test_support::{bot, node, node_at, provider_work};
use std::sync::atomic::Ordering;

async fn signed_answer_fact(
    node: &Runtime,
    admin: &LocalIdentity,
    question: &str,
    text: &str,
) -> Result<Value> {
    let network = node.network_id().await?;
    let scope = {
        let network = (network).to_owned();
        let question = (question).to_owned();
        node.store
            .call("triage.test.question", move |store| {
                store.question(&network, &question)
            })
            .await
    }?
    .context("question")?["conversation_id"]
        .as_str()
        .context("question scope")?
        .to_owned();
    let signed = admin.attest(
        &network,
        &scope,
        EventBody::MessagePosted {
            message_id: crate::store::questions::answer_id(question),
            role: crate::model::Role::User,
            text: text.into(),
            attachments: vec![],
            report_results: Some(true),
            in_reply_to: None,
            status: None,
            answers: Some(question.into()),
            attestation: None,
            failure: None,
        },
    )?;
    let result = node
        .post_answer(
            &network,
            question,
            text,
            signed.attestation().cloned(),
            AnswerAuthority::Signed,
        )
        .await?;
    Ok(json!({"status":result.0,"message":result.1,"receipt":result.2}))
}

#[tokio::test]
async fn a_held_request_is_answered_by_the_agent_once() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &["t-1"]).await?;
        let (mut server, handed) = app_server(&node).await?;
        node.on_codex_request(&handed).await;
        let requests = node.triage_command(json!({"action":"requests"})).await?;
        let request_id = requests["requests"][0]["request_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let deadline = lock(&node.triage.requests)[&request_id].answer_by_ms;
        {
            let request = (request_id).to_owned();
            node.store
                .call("triage.test.finish_triage_request", move |store| {
                    store.finish_triage_request(&request, false)
                })
                .await
        }?;
        node.on_codex_request(&handed).await;
        assert_eq!(
            lock(&node.triage.requests)[&request_id].answer_by_ms,
            deadline,
            "a repeated live App Server request keeps its original deadline"
        );
        assert_eq!(
            requests["requests"][0]["method"],
            "item/commandExecution/requestApproval"
        );
        let events = node.triage_command(json!({"action":"events"})).await?;
        assert_eq!(events["events"][0]["request_id"], request_id.as_str());
        assert!(
            !events["events"][0]["detail"]
                .as_str()
                .unwrap()
                .contains("rm -rf"),
            "commands stay out of events"
        );

        let respond =
            json!({"action":"respond","request_id":request_id,"result":{"decision":"accept"}});
        let answered = node.triage_command(respond.clone()).await?;
        assert_eq!(
            (answered["answered"].clone(), answered["by"].clone()),
            (json!(true), json!("agent"))
        );
        assert_eq!(
            answered_once(&mut server).await["result"],
            json!({"decision":"accept"})
        );
        assert_eq!(
            node.triage_command(respond).await?,
            answered,
            "answering again returns the first answer"
        );
        assert_eq!(
            node.triage_command(json!({"action":"requests"})).await?["requests"],
            json!([])
        );
        assert_eq!(
            node.triage_command(json!({"action":"events"})).await?["events"],
            json!([])
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
}

async fn hold_hook_request(node: &Runtime, event: HookEvent) -> Result<String> {
    node.ingest_agent_event(event).await?["request_id"]
        .as_str()
        .context("a held request")
        .map(str::to_owned)
}

async fn hook_waiter(node: &Arc<Runtime>, request_id: &str) -> tokio::task::JoinHandle<Value> {
    let node = node.clone();
    let request_id = request_id.to_owned();
    let waiting = tokio::spawn(async move { node.wait_request(&request_id).await });
    tokio::task::yield_now().await;
    waiting
}

fn expire_hook_request(node: &Runtime, request_id: &str) -> Result<()> {
    node.triage
        .requests
        .lock()
        .unwrap()
        .get_mut(request_id)
        .context("held")?
        .answer_by_ms = Some(now_ms() + 50);
    Ok(())
}

async fn hook_reply_or_expiry(
    node: &Arc<Runtime>,
    event: impl Fn(&str) -> Result<HookEvent>,
    ids: [&str; 2],
    source: (&str, &str),
    reply: Value,
    unanswered: &str,
) -> Result<()> {
    let request_id = hold_hook_request(node, event(ids[0])?).await?;
    let listed = node.triage_command(json!({"action":"requests"})).await?["requests"][0].clone();
    assert_eq!(
        (listed["provider"].clone(), listed["kind"].clone()),
        (json!(source.0), json!(source.1))
    );
    let waiting = hook_waiter(node, &request_id).await;
    node.triage_command(json!({"action":"respond","request_id":request_id,"result":reply}))
        .await?;
    assert_eq!(waiting.await?, reply);

    let request_id = hold_hook_request(node, event(ids[1])?).await?;
    expire_hook_request(node, &request_id)?;
    assert_eq!(
        node.wait_request(&request_id).await,
        json!({"decision":null}),
        "{unanswered}"
    );
    Ok(())
}

#[tokio::test]
async fn a_hook_waits_for_the_answer_to_its_request() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        let request_id = hold_hook_request(&node, asks_permission("s1")).await?;
        assert!(
            node.ingest_agent_event(asks_permission("s1")).await?["request_id"].is_null(),
            "a request is held once"
        );
        let listed =
            node.triage_command(json!({"action":"requests"})).await?["requests"][0].clone();
        assert_eq!(
            (listed["provider"].clone(), listed["kind"].clone()),
            (json!("claude"), json!("permission"))
        );
        assert_eq!(
            listed["answer_by_ms"].as_i64().context("a deadline")?
                - listed["received_ms"].as_i64().context("received")?,
            HOOK_ANSWER_WITHIN.as_millis() as i64,
            "a hook's request is held only as long as its session can wait"
        );
        let waiting = hook_waiter(&node, &request_id).await;
        // What a hook's wait sees once the request is gone from the held ones.
        let seen = std::thread::spawn({
            let node = node.clone();
            let request_id = request_id.clone();
            move || loop {
                let requests = node.triage.requests.lock().unwrap();
                if !requests.contains_key(&request_id) {
                    return node
                        .triage
                        .answered
                        .lock()
                        .unwrap()
                        .get(&request_id)
                        .cloned();
                }
            }
        });

        let answered = node
            .triage_command(json!({"action":"respond","request_id":request_id,
                "result":{"behavior":"allow"}}))
            .await?;
        let seen = seen.join().unwrap().context("answered")?;
        assert_eq!(
            (seen["answer"].clone(), seen.get("sending")),
            (json!({"behavior":"allow"}), None),
            "a hook's request is never seen gone and not yet answered"
        );
        assert_eq!(
            (answered["answered"].clone(), answered["by"].clone()),
            (json!(true), json!("agent"))
        );
        assert_eq!(waiting.await?, json!({"behavior":"allow"}));
        assert_eq!(
            node.wait_request(&request_id).await,
            json!({"behavior":"allow"}),
            "a hook that comes to wait late still gets the answer"
        );
        assert_eq!(
            node.triage_command(json!({"action":"events"})).await?["events"],
            json!([])
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn an_unanswered_hook_request_goes_back_to_its_session() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        let request_id = hold_hook_request(&node, asks_permission("s1")).await?;
        expire_hook_request(&node, &request_id)?;
        assert_eq!(
            node.wait_request(&request_id).await,
            json!({"decision":null})
        );
        assert_eq!(
            node.triage_command(json!({"action":"requests"})).await?["requests"],
            json!([])
        );
        let late = node
            .triage_command(json!({"action":"respond","request_id":request_id,
                "result":{"behavior":"allow"}}))
            .await?;
        assert_eq!(
            (late["decision"].clone(), late["by"].clone()),
            (json!("undecided"), json!("fallback")),
            "a late answer gets the first one"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn an_answer_after_the_hook_left_is_not_reported_as_given() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        let request_id = hold_hook_request(&node, asks_permission("s1")).await?;
        let waiting = hook_waiter(&node, &request_id).await;
        waiting.abort();
        let _ = waiting.await;
        let answered = node
            .triage_command(json!({"action":"respond","request_id":request_id,
                "result":{"behavior":"allow"}}))
            .await?;
        assert_eq!(
            (answered["answered"].clone(), answered["error"].clone()),
            (
                json!(false),
                json!("the session no longer waits for this request")
            )
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn answers_given_at_once_all_get_the_first() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        for session in 0..20 {
            let request_id =
                hold_hook_request(&node, asks_permission(&format!("s{session}"))).await?;
            let start = Arc::new(tokio::sync::Barrier::new(4));
            let answers: Vec<_> = (0..4)
                .map(|_| {
                    let (node, request_id, start) =
                        (node.clone(), request_id.clone(), start.clone());
                    tokio::spawn(async move {
                        start.wait().await;
                        node.respond_unclaimed_request(&request_id, json!({"behavior":"allow"}))
                            .await
                    })
                })
                .collect();
            for answer in answers {
                assert_eq!(answer.await??["by"], json!("agent"));
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn peer_respond_uses_the_unclaimed_queue_and_preserves_the_first_answer() -> Result<()> {
    let (ia, ib) = (LocalIdentity::generate(), LocalIdentity::generate());
    let target = ib.device_id()?;
    let membership = ia.sign_network_fixture(1, vec![ia.device_id()?, target.clone()])?;
    let admin = ia.fixture_admin();
    let networked = || crate::net::NetworkConfig {
        bind: Some("127.0.0.1:0".parse().unwrap()),
        ..Default::default()
    };
    let (a, dir_a) = node_at(ia, networked()).await?;
    let (b, dir_b) = node_at(ib, networked()).await?;
    let stranger = LocalIdentity::generate();
    let outsider = crate::net::Net::bind(&stranger.secret_key_hex, &networked(), 0, vec![]).await?;
    let result = async {
        for node in [&a, &b] { node.install_membership(membership.clone()).await?; }
        with_agent(&b);
        bot(&b, &admin, "c", "前端", &[]).await?;
        for node in [&a, &b] { node.start_network().await?; }
        a.net()?.learn(&b.net()?.addr()).await;
        b.net()?.learn(&a.net()?.addr()).await;
        outsider.learn(&b.net()?.addr()).await;
        let network = b.network_id().await?;
        let respond = |request: &Value| json!({"action":"respond","node":target,
            "request_id":request["request_id"],"result":{"behavior":"allow"}});
        let pending = b.ingest_agent_event(asks_permission("remote-pending")).await?;
        let denied = outsider.request(&target, net::RPC,
            &json!({"network_id":network,"type":"respond","request_id":pending["request_id"],"result":{"behavior":"allow"}}), Duration::from_secs(5)).await;
        assert!(denied.is_err(), "a nonmember cannot answer a pending request");
        let first = a.triage_command(respond(&pending)).await?;
        assert_eq!(first["answered"], true);
        let mut repeated = respond(&pending); repeated["result"] = json!({"behavior":"deny"});
        assert_eq!(a.triage_command(repeated).await?, first, "a retry reads the first response without sending another answer");
        for state in ["claimed", "dismissed", "unknown", "question"] {
            let held = b.ingest_agent_event(asks_permission(state)).await?;
            match state {
                "dismissed" => { b.triage_command(json!({"action":"dismiss","event_id":held["event_id"]})).await?; }
                "question" => { lock(&b.triage.requests).get_mut(held["request_id"].as_str().unwrap()).unwrap().question_id = Some("standing-user-question".into()); }
                _ => {
                    let TriageAdmission::Claimed(attempt) = {
                        let network = (network).to_owned();
                        let boot = (b.boot).to_owned();
                        b.store.call("triage.test.admit_triage", move |store| {
                            store.admit_triage(&network, &boot, BATCH, 100, now_ms())
                        }).await
                    }? else { bail!("claim fixture request"); };
                    if state == "unknown" { {
                            let result = Err(anyhow::anyhow!("response outcome unknown"));
                            b.store.call("triage.test.finish_triage", move |store| {
                                store.finish_triage(attempt, &result)
                            }).await
                        }?; }
                }
            }
            assert!(a.triage_command(respond(&held)).await.is_err(), "peer cannot answer {state}");
            assert!(b.triage_command(json!({"action":"respond","request_id":held["request_id"],"result":{"behavior":"allow"}})).await.is_err(), "local caller cannot answer {state}");
            assert!(!lock(&b.triage.answered).contains_key(held["request_id"].as_str().unwrap()));
        }
        Ok::<_, anyhow::Error>(())
    }.await;
    outsider.close().await;
    for node in [&a, &b] {
        node.stop_network().await;
    }
    cleanup(a, dir_a).await?;
    cleanup(b, dir_b).await?;
    result
}

#[tokio::test]
async fn a_pi_session_whose_turn_ended_gets_its_next_message_or_nothing() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        let ended = |turn: &str| {
            triage::from_pi_extension(&json!({"session_id":"p1","turn_id":turn,"cwd":"/w",
                "stop_reason":"stop","last_message":"Tests written."}))
            .context("a turn end")
        };
        hook_reply_or_expiry(
            &node,
            ended,
            ["e1", "e2"],
            ("pi", "continue"),
            json!({"text":"Now run them."}),
            "nothing is sent to a session nobody answered",
        )
        .await?;
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn an_opencode_permission_gets_its_reply_or_goes_back_to_the_session() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        let asks = |request: &str| {
            triage::from_opencode_plugin(&json!({"event":"permission.asked",
                "session_id":"ses_1","request_id":request,"cwd":"/w","permission":"bash"}))
            .context("a permission request")
        };
        hook_reply_or_expiry(
            &node,
            asks,
            ["per_1", "per_2"],
            ("opencode", "permission"),
            json!({"reply":"always"}),
            "an unanswered request is left to the user",
        )
        .await?;
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn without_an_agent_requests_are_declined_and_events_dropped() -> Result<()> {
    with_node(async |node, _, _| {
        let (mut server, handed) = app_server(&node).await?;
        node.on_codex_request(&handed).await;
        assert_eq!(
            answered_once(&mut server).await["result"],
            json!({"decision":"decline"})
        );
        assert_eq!(
            node.triage_command(json!({"action":"requests"})).await?["requests"],
            json!([])
        );
        let recorded = node
            .triage
            .answered
            .lock()
            .unwrap()
            .values()
            .next()
            .cloned()
            .unwrap();
        assert_eq!(
            (recorded["decision"].clone(), recorded["by"].clone()),
            (json!("decline"), json!("fallback"))
        );
        assert_eq!(
            node.ingest_agent_event(turn("s1", "t1", "/x")).await?["status"],
            "dropped"
        );
        assert_eq!(
            node.triage_command(json!({"action":"events"})).await?["events"],
            json!([])
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn native_request_questions_use_the_real_work_turn_without_merging_requests() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "Native target", &[]).await?;
        running_work(&node, &admin, "native-target-work").await?;
        let network = node.network_id().await?;
        for item in ["first", "second"] {
            node.on_codex_request(&handed(
                71,
                "item/commandExecution/requestApproval",
                json!({"threadId":"t-9","turnId":"u-1","itemId":item,"command":"fixture"}),
            ))
            .await;
        }
        let events = node.triage_command(json!({"action":"events"})).await?["events"]
            .as_array()
            .context("native events")?
            .clone();
        assert_eq!(events.len(), 2);
        assert_ne!(events[0]["event_id"], events[1]["event_id"]);
        assert_ne!(events[0]["request_id"], events[1]["request_id"]);
        let mut questions = Vec::new();
        for event in events {
            assert_eq!(
                event["turn_id"],
                format!("request:{}", event["request_id"].as_str().context("request ID")?),
                "the event keeps its per-request deduplication key"
            );
            let raised = node
                .triage_command(json!({"action":"raise","event_id":event["event_id"],
                    "severity":"urgent","question":"allow fixture?","request_id":event["request_id"]}))
                .await?;
            let card = {
                let network = (network).to_owned();
                let question = (raised["question_id"].as_str().context("native question")?).to_owned();
                node.store.call("triage.test.question", move |store| {
                    store.question(&network, &question)
                }).await
            }?.context("saved native question")?;
            assert_eq!(card["target"], json!({"kind":"work","work_id":"native-target-work"}));
            assert_eq!(card["about"]["request_id"], event["request_id"]);
            questions.push(card["question_id"].clone());
        }
        assert_ne!(questions[0], questions[1], "two items in one turn ask independently");
        assert_eq!(unanswered(&node).await?.len(), 2);
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn native_question_targets_require_matching_held_sources_and_keep_hook_turns() -> Result<()> {
    for case in [
        "missing-held",
        "missing-event-request",
        "missing-about-request",
        "wrong-held-id",
        "wrong-event-request",
        "wrong-about-request",
        "provider",
        "thread",
        "node",
        "missing-turn",
        "unknown-turn",
        "transport",
        "hook",
        "held-hook",
    ] {
        with_node(async |node, admin, _| {
            with_agent(&node);
            bot(&node, &admin, "c", "Native source", &[]).await?;
            running_work(&node, &admin, "native-source-work").await?;
            let network = node.network_id().await?;
            node.on_codex_request(&handed(
                72,
                "item/commandExecution/requestApproval",
                json!({"threadId":"t-9","turnId":"u-1","itemId":"source","command":"fixture"}),
            ))
            .await;
            let mut stored =
                node.triage_command(json!({"action":"events"})).await?["events"][0].clone();
            let event = SealedAgentEvent::restore_stored(stored.clone())?;
            let id = event.request_id().clone().context("held request")?;
            let mut about = subject(&event, Some(id.clone()));
            {
                let mut requests = lock(&node.triage.requests);
                let request = requests.get_mut(&id).context("saved request")?;
                match case {
                    "missing-event-request" => stored["request_id"] = Value::Null,
                    "missing-about-request" => about.request_id = None,
                    "wrong-held-id" => request.request_id = "another-request".into(),
                    "wrong-event-request" => stored["request_id"] = json!("another-request"),
                    "wrong-about-request" => about.request_id = Some("another-request".into()),
                    "provider" => request.provider = Provider::Claude,
                    "thread" => request.session_id = "another-thread".into(),
                    "node" => stored["node"] = json!(LocalIdentity::generate().device_id()?),
                    "missing-turn" => request.session_turn.clear(),
                    "unknown-turn" => request.session_turn = "unknown-turn".into(),
                    "transport" => {
                        request.reply = Reply::Hook {
                            answer: tokio::sync::watch::channel(None).0,
                            waited: false,
                        }
                    }
                    "hook" | "held-hook" => {
                        stored["source"] = json!("hook");
                        stored["turn_id"] = json!("u-1");
                        request.session_turn = "not-the-hook-turn".into();
                        if case == "hook" {
                            stored["request_id"] = Value::Null;
                            about.request_id = None;
                        } else {
                            request.reply = Reply::Hook {
                                answer: tokio::sync::watch::channel(None).0,
                                waited: false,
                            };
                        }
                    }
                    _ => {}
                }
                if case == "missing-held" {
                    requests.remove(&id);
                }
            }
            let event = SealedAgentEvent::restore_stored(stored)?;
            let question_id = format!("q-native-source-{case}");
            let raised = node
                .raise(
                    &network,
                    &event,
                    EventBody::QuestionAsked {
                        question_id: question_id.clone(),
                        target: None,
                        attention: AttentionRequest {
                            kind: AttentionKind::Question,
                            device_id: Some(event.node().clone()),
                        },
                        severity: Severity::Attention,
                        text: "fixture source?".into(),
                        choices: vec![],
                        about,
                    },
                )
                .await;
            if matches!(case, "hook" | "held-hook") {
                raised?;
                let card = {
                    let network = (network).to_owned();
                    let question = (question_id).to_owned();
                    node.store
                        .call("triage.test.question", move |store| {
                            store.question(&network, &question)
                        })
                        .await
                }?
                .context("hook question")?;
                assert_eq!(
                    card["target"],
                    json!({"kind":"work","work_id":"native-source-work"}),
                    "{case}"
                );
            } else if raised.is_ok() {
                let card = {
                    let network = (network).to_owned();
                    let question = (question_id).to_owned();
                    node.store
                        .call("triage.test.question", move |store| {
                            store.question(&network, &question)
                        })
                        .await
                }?
                .context("unscoped question")?;
                assert!(
                    card["target"].is_null(),
                    "{case}: mismatched evidence cannot borrow a Work or Main target"
                );
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .with_context(|| case)?;
    }
    Ok(())
}

#[tokio::test]
async fn a_question_holds_the_request_until_it_is_answered() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &["t-1"]).await?;
        let (mut server, handed) = app_server(&node).await?;
        node.on_codex_request(&handed).await;
        let event = node.triage_command(json!({"action":"events"})).await?["events"][0].clone();
        let request_id = event["request_id"].as_str().unwrap().to_owned();
        let raised = node
            .triage_command(json!({"action":"raise","event_id":event["event_id"],
            "severity":"urgent","question":"要清理 build 吗？","request_id":request_id,
            "choices":["允许","拒绝"]}))
            .await?;
        assert_eq!(
            (raised["asked"].clone(), raised["conversation_id"].clone()),
            (json!(true), json!("c"))
        );
        let question = raised["question_id"].as_str().unwrap().to_owned();
        node.expire_requests().await?;
        let held = node.triage_command(json!({"action":"requests"})).await?["requests"][0].clone();
        assert_eq!(
            (held["answer_by_ms"].clone(), held["question_id"].clone()),
            (Value::Null, json!(question))
        );
        let asked = unanswered(&node).await?;
        assert_eq!(asked.len(), 1);
        assert_eq!(
            (
                asked[0]["about"]["request_id"].clone(),
                asked[0]["choices"][1]["label"].clone()
            ),
            (json!(request_id), json!("拒绝"))
        );

        assert_eq!(answer(&node, &question, "拒绝").await?, "answered");
        let jobs = {
            node.store
                .call("triage.test.scheduler_jobs", move |store| {
                    store.scheduler_jobs("c")
                })
                .await
        }?;
        assert!(
            jobs.is_empty(),
            "a held answer does not start a scheduler turn"
        );
        node.expire_requests().await?;
        let held = node.triage_command(json!({"action":"requests"})).await?["requests"][0].clone();
        assert!(
            held["answer_by_ms"].as_i64().is_some(),
            "the stored answer starts the control deadline"
        );
        let respond =
            json!({"action":"respond","request_id":request_id,"result":{"decision":"decline"}});
        assert!(
            node.triage_command(respond).await.is_err(),
            "a Bot cannot substitute its own decision for the stored answer"
        );
        node.pending_provider_launches.store(true, Ordering::SeqCst);
        node.wake_agent().await?;
        node.wake_agent().await?;
        assert_eq!(
            answered_once(&mut server).await["result"],
            json!({"decision":"decline"})
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn explicit_approval_answers_bypass_exhausted_triage_budget_once() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "Approval", &["t-1"]).await?;
        let network = node.network_id().await?;
        let (mut server, handed) = app_server(&node).await?;
        node.on_codex_request(&handed).await;
        let (question, request_id) = raise_for(&node).await?;
        let limit = node.config.daily_decision_limit;
        let budget_network = network.clone();
        node.store
            .call("triage.test.exhaust_explicit_answer_budget", move |store| {
                store.connection.execute(
                    "INSERT INTO budget_days(network_id,day,used) VALUES(?1,date('now'),?2)",
                    rusqlite::params![format!("triage:{budget_network}"), limit],
                )?;
                Ok(())
            })
            .await?;
        node.ingest_agent_event(turn("ordinary-pending", "ordinary-turn", "/tmp"))
            .await?;
        signed_answer_fact(&node, &admin, &question, "允许本次").await?;
        node.expire_requests().await?;
        assert!(
            lock(&node.triage.requests)[&request_id]
                .answer_by_ms
                .is_some()
        );
        node.wake_agent().await?;
        assert_eq!(
            answered_once(&mut server).await["result"],
            json!({"decision":"accept"})
        );
        assert!(!lock(&node.triage.requests).contains_key(&request_id));
        assert_eq!(node.triage_status().await?["budget"]["used"], limit + 1);
        node.wake_agent().await?;
        assert_eq!(
            node.triage_status().await?["budget"]["used"],
            limit + 1,
            "the explicit answer is charged once"
        );
        assert_eq!(
            node.pending_events().await?.len(),
            1,
            "ordinary triage remains paused after the explicit answer"
        );
        assert_eq!(
            node.triage_status().await?["budget"]["paused_reason"],
            "budget_exhausted"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn a_live_workspace_task_gets_its_one_approval_without_a_slot_or_another_turn() -> Result<()>
{
    use crate::workspace::{
        InputIntent, InputRequest, InputRoute, InputTarget, WORKSPACE_SCOPE, WorkspaceCommand,
    };
    with_node(async |node, admin, dir| {
        with_agent(&node);
        {
            let mut settings = node.settings.lock().unwrap();
            settings.roots = vec![dir.canonicalize()?];
            settings.manager.as_mut().context("manager")?.workspace = dir.canonicalize()?;
        }
        bot(&node, &admin, "c", "Task Bot", &[]).await?;
        let network = node.network_id().await?;
        let command = WorkspaceCommand::CreateTask {
            task_id: "held-task".into(),
            channel_id: None,
            parent_task_id: None,
            owner_bot: "c".into(),
            title: "held task".into(),
            request: "do the task".into(),
            expected_result: "result".into(),
            source_events: vec![],
            dependencies: vec![],
            reply_only: false,
        };
        let signed = admin.attest(
            &network,
            WORKSPACE_SCOPE,
            EventBody::WorkspaceCommanded {
                command_id: "create-held-task".into(),
                command: command.clone(),
                acting_assignment: None,
                attestation: None,
                cause: None,
            },
        )?;
        let target = InputTarget::Task {
            task_id: "held-task".into(),
        };
        let input = InputRequest {
            message_id: "task-input".into(),
            route: InputRoute {
                target: target.clone(),
                intent: InputIntent::Continue,
                sequence: 1,
                expected_revision: Some(1),
            },
            text: "do the task".into(),
            attachments: vec![],
            report_results: true,
        };
        let signed_input = admin.attest(
            &network,
            "c",
            crate::workspace::input_event_body(&input, None),
        )?;
        let (trigger, task_attempt, task_job) = {
            let identity = node.identity.clone();
            let network = network.clone();
            let boot = node.boot.clone();
            let dir = dir.to_owned();
            node.store
                .call("triage.test.held_task_setup", move |store| {
                    store.apply_workspace_command(
                        &identity,
                        &network,
                        "create-held-task",
                        command,
                        signed.attestation().cloned(),
                        None,
                        None,
                        now_ms(),
                    )?;
                    store.post_workspace_input(
                        &identity,
                        &network,
                        input,
                        signed_input
                            .attestation()
                            .cloned()
                            .context("signed input")?,
                        now_ms(),
                    )?;
                    let assignment = store
                        .ensure_workspace_assignment(
                            &identity,
                            &network,
                            "held-task",
                            &[dir.canonicalize()?.display().to_string()],
                            now_ms(),
                        )?
                        .context("Task assignment")?;
                    assert_eq!(assignment.trigger_message_id.as_deref(), Some("task-input"));
                    let claimed = store
                        .claim_next_fixture(&identity, &network, "c", &boot, "fixture")?
                        .context("Task claim")?;
                    assert_eq!(
                        claimed.job.scope,
                        crate::scheduling::SessionScope::WorkspaceTask {
                            task_id: "held-task".into()
                        }
                    );
                    let trigger = claimed.job.trigger.clone();
                    let attempt = claimed
                        .job
                        .state
                        .attempt()
                        .context("Task attempt")?
                        .id
                        .clone();
                    let job = store.mark_started("c", &trigger, &attempt)?;
                    store.save_scheduler_native_id(
                        "c",
                        &trigger,
                        &attempt,
                        "t-1",
                        "held-task-token",
                        true,
                    )?;
                    Ok((trigger, attempt, job))
                })
                .await?
        };
        let (mut server, handed) = app_server(&node).await?;
        let native_thread = handed.params["threadId"]
            .as_str()
            .context("native Task thread")?;
        let native_turn = handed.params["turnId"]
            .as_str()
            .context("native Task turn")?;
        let native = crate::scheduler::turn_recorder(
            &node,
            &network,
            &task_job,
            "system",
            Arc::new(std::sync::Mutex::new(Default::default())),
            None,
        );
        (native.record)(native_thread.into(), Some(native_turn.into())).await?;
        native.started.context("Task native start callback")?(native_turn.into()).await?;
        let _cancel = node
            .register_manager_attempt_fixture(
                "held-task-token".into(),
                crate::runtime::AgentAccess::ReadOnly {
                    network_id: network.clone(),
                    conversation_id: "c".into(),
                },
                &task_attempt,
                &trigger,
                Provider::Codex,
            )
            .await?;
        node.on_codex_request(&handed).await;
        let (question, request_id) = raise_for(&node).await?;
        let card = {
            let network = (network).to_owned();
            let question = (question).to_owned();
            node.store
                .call("triage.test.question", move |store| {
                    store.question(&network, &question)
                })
                .await
        }?
        .context("approval")?;
        assert_eq!(card["target"], serde_json::to_value(&target)?);
        assert_eq!(
            card["choices"][0],
            json!({"label":"允许本次","action":"allow_once"})
        );
        assert!(
            signed_answer_fact(&node, &admin, &question, "继续")
                .await
                .is_err()
        );
        assert!(
            node.post_answer(
                &network,
                &question,
                "允许本次",
                None,
                AnswerAuthority::Signed
            )
            .await
            .is_err()
        );
        assert!(
            {
                let network = (network).to_owned();
                let message_id = (crate::store::questions::answer_id(&question)).to_owned();
                node.store
                    .call("triage.test.message", move |store| {
                        store.message(&network, "c", &message_id)
                    })
                    .await
            }?
            .is_none()
        );
        let answered = tokio::time::timeout(
            Duration::from_secs(3),
            signed_answer_fact(&node, &admin, &question, "允许本次"),
        )
        .await??;
        let retry = signed_answer_fact(&node, &admin, &question, "允许本次").await?;
        assert_eq!(answered["message"], retry["message"]);
        assert!(
            node.triage_command(
                json!({"action":"respond","request_id":request_id,"result":{"decision":"accept"}})
            )
            .await
            .is_err()
        );
        node.pending_provider_launches.store(true, Ordering::SeqCst);
        node.wake_agent().await?;
        assert_eq!(
            answered_once(&mut server).await["result"],
            json!({"decision":"accept"})
        );
        assert!(
            node.pending_provider_launches.load(Ordering::SeqCst),
            "an active Task keeps the ordinary launch change pending"
        );
        {
            let network = network.clone();
            let answered = answered.clone();
            node.store
                .call("triage.test.held_task_receipt", move |store| {
                    let jobs = store.scheduler_jobs("c")?;
                    assert_eq!(
                        jobs.len(),
                        1,
                        "the stored approval creates no Task or Main turn"
                    );
                    assert_eq!(
                        jobs[0].state.name(),
                        "running",
                        "the original Task remains active"
                    );
                    assert!(
                        store
                            .held_answer_receipt(
                                &network,
                                "c",
                                answered["message"]["message_id"].as_str().unwrap()
                            )?
                            .is_some()
                    );
                    Ok(())
                })
                .await?
        }
        let local = node.identity.device_id()?;
        let ordinary = EventBody::QuestionAsked {
            question_id: "q-ordinary-task-question".into(),
            target: Some(target.clone()),
            attention: AttentionRequest {
                kind: AttentionKind::Question,
                device_id: Some(local.clone()),
            },
            severity: Severity::Info,
            text: "what next?".into(),
            choices: vec![],
            about: Subject {
                node: local.clone(),
                agent: Some(Provider::Codex),
                session_id: "t-1".into(),
                cwd: String::new(),
                summary: String::new(),
                request_id: None,
            },
        };
        {
            let identity = (node.identity).to_owned();
            let network = (network).to_owned();
            let body = ordinary;
            node.store
                .call("triage.test.ask", move |store| {
                    store.ask(&identity, &network, "c", body, now_ms())
                })
                .await
        }?;
        signed_answer_fact(&node, &admin, "q-ordinary-task-question", "next step").await?;
        let next = {
            let network = network.clone();
            let local = local.clone();
            node.store
                .call("triage.test.ordinary_answer_hold", move |store| {
                    let jobs = store.scheduler_jobs("c")?;
                    let next = jobs
                        .iter()
                        .find(|job| job.kind == crate::scheduling::TriggerKind::Answer)
                        .context("ordinary Answer")?
                        .clone();
                    assert_eq!(
                        next.scope,
                        crate::scheduling::SessionScope::WorkspaceTask {
                            task_id: "held-task".into()
                        }
                    );
                    assert!(
                        store
                            .scheduler_job_hold(
                                &network,
                                &local,
                                &next,
                                Provider::Codex,
                                &crate::config::SchedulingLimits {
                                    machine_slots: 1,
                                    task_limit: 1
                                }
                            )?
                            .is_some(),
                        "the occupied scheduler cannot start the ordinary follow-up"
                    );
                    Ok(next)
                })
                .await?
        };
        node.finish_manager_attempt(&task_attempt).await;
        let identity = node.identity.clone();
        let boot = node.boot.clone();
        node.store
            .call("triage.test.ordinary_answer_claim", move |store| {
                store.finish_job(
                    &identity,
                    &network,
                    "c",
                    &trigger,
                    &task_attempt,
                    crate::scheduling::JobOutcome::Completed,
                    "done",
                    now_ms(),
                )?;
                assert_eq!(
                    store
                        .claim_next_fixture(&identity, &network, "c", &boot, "fixture")?
                        .context("ordinary follow-up")?
                        .job
                        .trigger,
                    next.trigger
                );
                Ok::<_, anyhow::Error>(())
            })
            .await
    })
    .await
}

async fn human_workspace_command(
    node: &Runtime,
    admin: &LocalIdentity,
    id: &str,
    command: crate::workspace::WorkspaceCommand,
) -> Result<()> {
    let network = node.network_id().await?;
    let signed = admin.attest(
        &network,
        crate::workspace::WORKSPACE_SCOPE,
        EventBody::WorkspaceCommanded {
            command_id: id.into(),
            command: command.clone(),
            acting_assignment: None,
            attestation: None,
            cause: None,
        },
    )?;
    node.workspace_command(
        &crate::runtime::Access::User {
            network_id: network,
            scope: crate::runtime::UserScope::Network,
        },
        &json!({"command_id":id,"command":command,"attestation":signed.attestation()}),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn held_work_approvals_recheck_current_assignment_and_hold_the_input_lease() -> Result<()> {
    use crate::workspace::{AssignmentState, TaskAction, WorkspaceCommand};
    use futures_util::SinkExt as _;
    for (change, deny) in [
        ("standalone", false),
        ("shared", false),
        ("shared-stopped", false),
        ("shared-scope", false),
        ("assigned", false),
        ("pause", false),
        ("pause", true),
        ("cancel", false),
        ("cancel", true),
        ("revise", false),
        ("revise", true),
        ("retarget", false),
        ("retarget", true),
        ("native-cwd", false),
        ("native-cwd", true),
    ] {
        let (node, admin, dir) = node().await?;
        let mut socket_task = None;
        let result = async {
            with_agent(&node);
            let root = dir.canonicalize()?;
            let directory = root.join("work");
            std::fs::create_dir(&directory)?;
            {
                let mut settings = node.settings.lock().unwrap();
                settings.roots = vec![root.clone()];
                settings.manager.as_mut().context("manager")?.workspace = directory.clone();
            }
            bot(&node, &admin, "c", "Task Bot", &[]).await?;
            let network = node.network_id().await?;
            let local = node.identity.device_id()?;
            let (mut socket, handed) = app_server(&node).await?;
            let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
            let native_directory = Arc::new(std::sync::Mutex::new(directory.display().to_string()));
            let served_directory = native_directory.clone();
            let native_work_ids = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let served_work_ids = native_work_ids.clone();
            socket_task = Some(tokio::spawn(async move {
                loop {
                    let message = read(&mut socket).await;
                    if message["id"] == 41 && message.get("result").is_some() {
                        let _ = sent.send(message);
                    } else if message.get("id").is_some() && message.get("method").is_some() {
                        let result = match message["method"].as_str() {
                            Some("thread/read" | "thread/resume") => {
                                json!({"thread":{"id":"t-1","cwd":lock(&served_directory).clone(),
                                "turns":[{"id":"u-1","status":"inProgress","items":lock(&served_work_ids).iter().map(|id| json!({"type":"userMessage","clientId":id})).collect::<Vec<_>>()}]}})
                            }
                            _ => json!({}),
                        };
                        if socket
                            .send(
                                json!({"id":message["id"],"result":result})
                                    .to_string()
                                    .into(),
                            )
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }));
            let trigger = if change == "standalone" {
                main_input_fact(&node, &admin, "c", "held-work-origin").await?
            } else {
                human_workspace_command(
                    &node,
                    &admin,
                    "create-held-source",
                    WorkspaceCommand::CreateTask {
                        task_id: "held-source".into(),
                        channel_id: None,
                        parent_task_id: None,
                        owner_bot: "c".into(),
                        title: "source".into(),
                        request: "perform the task".into(),
                        expected_result: "result".into(),
                        source_events: vec![],
                        dependencies: vec![],
                        reply_only: false,
                    },
                )
                .await?;
                let epoch = {
                    let store = (network).to_owned();
                    node.store.call("triage.test.conversation", move |database| {
                        database.conversation(&store, "c")
                    }).await
                }?
                    .context("Bot")?
                    .owner_epoch;
                human_workspace_command(
                    &node,
                    &admin,
                    "assign-held-source",
                    WorkspaceCommand::AssignTask {
                        task_id: "held-source".into(),
                        expected_revision: 1,
                        assignment_id: "held-assignment".into(),
                        bot_id: "c".into(),
                        execution_node: local.clone(),
                        bot_owner_epoch: epoch,
                        channel_owner_epoch: None,
                        work_id: None,
                        resource_roots: vec![directory.display().to_string()],
                        automatic: false,
                    },
                )
                .await?;
                {
                    let identity = (node.identity).to_owned();
                    let network = (network).to_owned();
                    let command = WorkspaceCommand::AssignmentProgress {
                            execution: None,
                            assignment_id: "held-assignment".into(),
                            revision: 1,
                            state: AssignmentState::Accepted,
                            input_event: None,
                            evidence_event: None,
                            result: None,
                    };
                    node.store.call("triage.test.apply_workspace_command", move |store| {
                        store.apply_workspace_command(&identity, &network, "accept-held-source", command, None, None, None, now_ms())
                    }).await
                }?;
                "assignment:held-assignment".into()
            };
            let work_id = crate::work::WorkSpec::id("c", &trigger, "held-child");
            let spec = crate::work::WorkSpec {
                work_id: work_id.clone(),
                trigger,
                call_ref: "held-child".into(),
                cwd: if change == "native-cwd" {
                    String::new()
                } else {
                    directory.display().to_string()
                },
                title: "child".into(),
                thread: Some("t-1".into()),
                ..provider_work(Provider::Codex, &local, &directory, "do the accepted task")
            };
            {
                let identity = (node.identity).to_owned();
                let network = (network).to_owned();
                let spec = spec.clone();
                node.store.call("triage.test.accept_work_fixture", move |store| {
                    store.accept_work_fixture(&identity, &network, spec, now_ms())
                }).await
            }?;
            drop(node.work_start_lease(&network, &spec, Some("t-1")).await?);
            node.signal_work(
                &network,
                &work_id,
                crate::work::Signal::TurnStarted {
                    turn_id: "u-1".into(),
                    epoch: node.codex.epoch(),
                },
            )
            .await?;
            lock(&native_work_ids).push(work_id.clone());
            let mut group_ids = vec![work_id.clone()];
            if change.starts_with("shared") {
                let mut second = spec.clone();
                second.call_ref = "held-second".into();
                second.work_id = crate::work::WorkSpec::id("c", &second.trigger, &second.call_ref);
                if change == "shared-scope" { second.parent = Some("another-task".into()); }
                {
                    let identity = (node.identity).to_owned();
                    let network = (network).to_owned();
                    let spec = second.clone();
                    node.store.call("triage.test.accept_work_fixture", move |store| {
                        store.accept_work_fixture(&identity, &network, spec, now_ms())
                    }).await
                }?;
                if change != "shared-scope" { drop(node.work_start_lease(&network, &second, Some("t-1")).await?); }
                node.signal_work(&network, &second.work_id, crate::work::Signal::TurnStarted { turn_id:"u-1".into(), epoch:node.codex.epoch() }).await?;
                lock(&native_work_ids).push(second.work_id.clone());
                group_ids.push(second.work_id);
            }
            {
                let network = network.clone();
                let work_id = work_id.clone();
                node.store.call("triage.test.shared_work_authority", move |store| {

                    assert_eq!(store.work_native_write_allowed(&network, &work_id)?, change != "shared-scope");
                    assert!(
                        store
                            .work(&network, &work_id)?
                            .context("work")?
                            .parent
                            .is_none()
                    );
                    Ok(())
                }).await?
            }
            node.on_codex_request(&handed).await;
            let (question, request_id) = raise_for(&node).await?;
            signed_answer_fact(
                &node,
                &admin,
                &question,
                if deny { "拒绝" } else { "允许本次" },
            )
            .await?;
            if matches!(change, "standalone" | "assigned" | "shared") {
                let attempt = {
                    let network = network.clone();
                    let boot = node.boot.clone();
                    node.store.call("triage.test.standalone_answer_claim", move |store| {

                        let TriageAdmission::Claimed(attempt) =
                        store.admit_triage(&network, &boot, BATCH, 100, now_ms())?
                        else {
                            bail!("standalone control claim");
                        };
                        Ok(attempt)
                    }).await?
                };
                let leases = node
                    .answer_permission_lease(attempt.answer().context("control")?)
                    .await?;
                for id in &group_ids {
                    assert!(node.work_input_lock(id).await.try_write_owned().is_err(),
                        "the approval lease retains every shared Work input read lock");
                }
                drop(leases);
                {
                    let result = Err(manager::ManagerError::not_started(anyhow::anyhow!(
                        "lease checked before any transport write"
                    )));
                    node.store.call("triage.test.finish_triage", move |store| {
                        store.finish_triage(attempt, &result)
                    }).await
                }?;
                let write = node.work_input_lock(&work_id).await.write_owned().await;
                let wake_node = node.clone();
                let wake = tokio::spawn(async move { wake_node.wake_agent().await });
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert!(
                    received.try_recv().is_err(),
                    "an existing Work writer fences the original transport response"
                );
                drop(write);
                tokio::time::timeout(Duration::from_secs(5), wake).await???;
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(5), received.recv())
                        .await?
                        .context("one response")?["result"],
                    json!({"decision":"accept"})
                );
            } else {
                if change == "shared-stopped" {
                    {
                        let request = (crate::work::StopRequest {
                            work_id:group_ids[1].clone(), conversation:"c".into(), target:node.identity.device_id()?,
                            requested_at_ms:now_ms(), deadline_ms:now_ms()+1000, timed_out:false,
                        }).to_owned();
                        node.store.call("triage.test.save_stop_request", move |store| {
                            store.save_stop_request(&request)
                        }).await
                    }?;
                } else if change == "shared-scope" {
                } else if matches!(change, "retarget" | "native-cwd") {
                    let replacement = root.join("other");
                    std::fs::create_dir(&replacement)?;
                    if change == "retarget" {
                        std::fs::rename(&directory, root.join("previous-work"))?;
                        vibeface_bridge::platform::privatefs::link_dir(&replacement, &directory)?;
                        assert_eq!(
                            node.allowed_path(directory.to_str().context("work directory")?)?,
                            replacement.canonicalize()?,
                            "the Node's broad root alone still permits the replaced path"
                        );
                    } else {
                        *lock(&native_directory) = replacement.display().to_string();
                    }
                } else {
                    let command = match change {
                        "pause" | "cancel" => WorkspaceCommand::ControlTask {
                            task_id: "held-source".into(),
                            expected_revision: 1,
                            action: if change == "pause" {
                                TaskAction::Stop
                            } else {
                                TaskAction::Cancel
                            },
                        },
                        "revise" => WorkspaceCommand::ReviseTask {
                            task_id: "held-source".into(),
                            expected_revision: 1,
                            request: "new task revision".into(),
                            expected_result: "new result".into(),
                            dependencies: vec![],
                        },
                        _ => unreachable!(),
                    };
                    human_workspace_command(&node, &admin, "change-held-source", command).await?;
                }
                {
                    let network = network.clone();
                    let work_id = work_id.clone();
                    node.store.call("triage.test.obsolete_work_authority", move |store| {

                        if !matches!(change, "retarget" | "native-cwd") {
                            assert!(!store.work_native_write_allowed(&network, &work_id)?);
                        }
                        assert_eq!(store.stop_requests()?.is_empty(), change != "shared-stopped",
                            "only the shared-stop fixture has sent a stop request");
                        assert!(
                            !store
                                .work_record(&work_id)?
                                .context("work")?
                                .state
                                .finished()
                        );
                        Ok(())
                    }).await?
                }
                if deny {
                    node.wake_agent().await?;
                    assert_eq!(
                        tokio::time::timeout(Duration::from_secs(5), received.recv())
                            .await?
                            .context("one denial")?["result"],
                        json!({"decision":"decline"})
                    );
                } else {
                    let error = node
                        .wake_agent()
                        .await
                        .expect_err("an obsolete assignment cannot approve its Work");
                    let reason = match change {
                        "shared-stopped" | "shared-scope" => "different native executions or work scopes",
                        "retarget" => "assignment resource roots",
                        "native-cwd" => "native directory differs from the reserved work",
                        _ => "current authority",
                    };
                    assert!(error.to_string().contains(reason), "{change}: {error:#}");
                    assert!(lock(&node.triage.requests).contains_key(&request_id));
                    assert!(lock(&node.triage.answered).is_empty());
                    assert!(received.try_recv().is_err());
                }
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(50), received.recv())
                    .await
                    .is_err(),
                "a control never sends another response"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Some(task) = socket_task {
            task.abort();
        }
        cleanup(node, dir).await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn answer_control_claims_reject_changed_bindings_and_unknown_never_replays() -> Result<()> {
    let (node, admin, dir) = node().await?;
    let before = async {
        with_agent(&node);
        bot(&node, &admin, "c", "Bot", &[]).await?;
        let held = node.ingest_agent_event(asks_permission("answer-control")).await?;
        let event = node.triage_command(json!({"action":"events"})).await?["events"][0].clone();
        let raised = node.triage_command(json!({"action":"raise","event_id":event["event_id"],
            "request_id":held["request_id"],"severity":"attention","question":"run?","choices":["model supplied approval"]})).await?;
        let question = raised["question_id"].as_str().context("question")?;
        let network = node.network_id().await?;
        signed_answer_fact(&node, &admin, question, "允许本次").await?;
        let attempt = {
            let network = network.clone();
            let boot = node.boot.clone();
            node.store.call("triage.test.answer_claim_bindings", move |store| {

                let TriageAdmission::Claimed(attempt) = store.admit_triage(&network, &boot, BATCH, 100, now_ms())? else { bail!("answer claim"); };
                assert_eq!(attempt.events().len(), 1, "an answer claim is never mixed with ordinary events");
                let original = attempt.answer().context("private answer binding")?;
                for changed in [
                    HeldAnswerControl { question_hash: "other".into(), ..original.clone() },
                    HeldAnswerControl { answer_hash: "other".into(), ..original.clone() },
                    HeldAnswerControl { conversation_id: "other".into(), ..original.clone() },
                ] {
                    assert!(store.validate_answer_control_claim(&boot, attempt.id(), &changed).is_err());
                }
                Ok(attempt)
            }).await?
        };
        *lock(&node.triage.active) = Some(attempt.id().into());
        let base = json!({"_attempt":attempt.id(),"action":"respond","request_id":held["request_id"],"result":{"behavior":"allow"}});
        for changed in [
            json!({"_attempt":attempt.id(),"action":"raise","event_id":event["event_id"],"severity":"info","question":"other"}),
            json!({"_attempt":attempt.id(),"action":"dismiss","event_id":event["event_id"]}),
            json!({"_attempt":attempt.id(),"action":"events"}),
            json!({"_attempt":attempt.id(),"action":"respond","request_id":"another-request","result":{"behavior":"allow"}}),
            json!({"_attempt":attempt.id(),"action":"respond","node":"another-node","request_id":held["request_id"],"result":{"behavior":"allow"}}),
            json!({"_attempt":attempt.id(),"action":"respond","request_id":held["request_id"],"result":{"behavior":"allow","updatedInput":{"command":"other"}}}),
            json!({"_attempt":"old-claim","action":"respond","request_id":held["request_id"],"result":{"behavior":"allow"}}),
        ] { assert!(node.triage_command(changed).await.is_err()); }
        assert_eq!(node.triage_command(json!({"_attempt":attempt.id(),"action":"requests"})).await?["requests"].as_array().unwrap().len(), 1);
        let request_id = held["request_id"].as_str().context("request")?;
        let generation = {
            let mut requests = lock(&node.triage.requests);
            let request = requests.get_mut(request_id).context("held request")?;
            std::mem::replace(&mut request.generation, "replacement-transport".into())
        };
        assert!(node.triage_command(base.clone()).await.is_err(), "reconnection cannot borrow an older claim");
        lock(&node.triage.requests).get_mut(request_id).context("held request")?.generation = generation;
        node.local_permission_epoch.fetch_add(1, Ordering::AcqRel);
        assert!(node.triage_command(base.clone()).await.is_err(), "revoked permissions cannot approve a still-live request");
        let failure: Result<()> = Err(anyhow::anyhow!("the control outcome is unknown"));
        {
            let result = failure;
            node.store.call("triage.test.finish_triage", move |store| {
                store.finish_triage(attempt, &result)
            }).await
        }?;
        assert!(node.triage_command(base).await.is_err(), "an unknown claim cannot resume sending");
        *lock(&node.triage.active) = None;
        node.wake_agent().await?;
        assert!(lock(&node.triage.answered).is_empty());
        assert_eq!(node.triage_status().await?["unknown_events"], 1);
        Ok::<_, anyhow::Error>((network, question.to_owned(), request_id.to_owned()))
    }.await;
    node.shutdown().await;
    drop(node);
    let reopened = Arc::new(Runtime::reopen(&dir).await?);
    let result = async {
        let (network, question, request) = before?;
        with_agent(&reopened);
        assert!(lock(&reopened.triage.requests).is_empty());
        reopened.wake_agent().await?;
        assert_eq!(reopened.triage_status().await?["unknown_events"], 1);
        assert!(
            {
                let network = (network).to_owned();
                let message_id = (crate::store::questions::answer_id(&question)).to_owned();
                reopened
                    .store
                    .call("triage.test.held_answer_receipt", move |store| {
                        store.held_answer_receipt(&network, "c", &message_id)
                    })
                    .await
            }?
            .is_some()
        );
        assert!(
            reopened
                .triage_command(
                    json!({"action":"respond","request_id":request,"result":{"behavior":"allow"}})
                )
                .await
                .is_err()
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    cleanup(reopened, dir).await?;
    result
}

#[tokio::test]
async fn held_answer_retry_reads_its_first_response_after_the_transport_is_settled() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "Bot", &["t-1"]).await?;
        let (mut server, handed) = app_server(&node).await?;
        node.on_codex_request(&handed).await;
        let (question, request_id) = raise_for(&node).await?;
        signed_answer_fact(&node, &admin, &question, "允许本次").await?;
        let network = node.network_id().await?;
        let boot = node.boot.clone();
        let attempt = node
            .store
            .call("triage.test.admit_answer_retry", move |store| {
                let TriageAdmission::Claimed(attempt) =
                    store.admit_triage(&network, &boot, BATCH, 100, now_ms())?
                else {
                    bail!("answer claim");
                };
                ensure!(attempt.answer().is_some(), "private answer binding");
                Ok(attempt)
            })
            .await?;
        *lock(&node.triage.active) = Some(attempt.id().into());
        let _active = ActiveTriage(&node.triage);
        let response = json!({"_attempt":attempt.id(), "action":"respond",
            "request_id":request_id,"result":{"decision":"accept"}});
        let first = node.triage_command(response.clone()).await?;
        assert_eq!(first["answered"], true);
        assert!(!lock(&node.triage.requests).contains_key(&request_id));
        assert_eq!(node.triage_command(response.clone()).await?, first);
        let mut changed = response;
        changed["result"] = json!({"decision":"decline"});
        assert!(node.triage_command(changed).await.is_err());
        assert_eq!(
            answered_once(&mut server).await["result"],
            json!({"decision":"accept"})
        );
        node.store
            .call("triage.test.finish_answer_retry", move |store| {
                store.finish_triage(attempt, &Ok(()))
            })
            .await
    })
    .await
}

#[test]
fn answer_results_use_the_original_question_schema_and_text() -> Result<()> {
    let schema = codex_answer_response(
        "item/tool/requestUserInput",
        &json!({"questions":[
        {"id":"database","question":"Which database?","options":[{"label":"SQLite"},{"label":"PostgreSQL"}]}]}),
    );
    assert!(
        validate_answer_result(
            &schema,
            &json!({}),
            "SQLite",
            &json!({"answers":{"database":{"answers":["SQLite"]}}})
        )
        .is_ok()
    );
    for result in [
        json!({"decision":"accept"}),
        json!({"answers":{"other":{"answers":["SQLite"]}}}),
        json!({"answers":{"database":{"answers":["PostgreSQL"]}}}),
        json!({"answers":{"database":{"answers":["SQLite"]}},"permissions":{"network":true}}),
    ] {
        assert!(validate_answer_result(&schema, &json!({}), "SQLite", &result).is_err());
    }
    let event = triage::from_claude_hook(&json!({"hook_event_name":"PermissionRequest","session_id":"session",
        "tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Which database?","options":[{"label":"SQLite"}]}]}}), 10).context("event")?;
    let mut hook = HookRequest {
        provider: event.agent,
        session_id: event.session_id.clone(),
        turn_id: event.turn_id.clone(),
        kind: RequestKind::Question,
        questions: Some(json!([{"question":"Which database?","options":[{"label":"SQLite"}]}])),
    };
    let response = hook_answer_response(&event, Some(&hook))?;
    assert!(
        validate_answer_result(
            &response,
            &json!({}),
            "SQLite",
            &json!({"answers":{"Which database?":"SQLite"}})
        )
        .is_ok()
    );
    hook.session_id = "other-session".into();
    assert!(hook_answer_response(&event, Some(&hook)).is_err());
    Ok(())
}

/// The App Server asks `rpc` for the approval of item `i-1` in turn `u-1` of thread `t-1`;
/// returns what the bridge hands the Node.
async fn ask_approval(node: &Runtime, server: &mut Server, rpc: i64) -> Result<ServerRequest> {
    use futures_util::SinkExt as _;
    let params = json!({"threadId":"t-1","turnId":"u-1","itemId":"i-1","command":"rm -rf build","reason":"clean"});
    let method = "item/commandExecution/requestApproval";
    server
        .send(
            json!({"id":rpc,"method":method,"params":params})
                .to_string()
                .into(),
        )
        .await?;
    while !node.codex.client().snapshot().await["pending_requests"]
        .as_array()
        .is_some_and(|pending| pending.iter().any(|request| request["id"] == rpc))
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(handed(rpc, method, params))
}

async fn raise_for(node: &Runtime) -> Result<(String, String)> {
    let event = node.triage_command(json!({"action":"events"})).await?["events"][0].clone();
    let request_id = event["request_id"]
        .as_str()
        .context("request event")?
        .to_owned();
    let raised = node
        .triage_command(json!({"action":"raise","event_id":event["event_id"],
        "severity":"attention","question":"要清理 build 吗？","request_id":request_id,
        "choices":["允许","拒绝"]}))
        .await?;
    Ok((
        raised["question_id"]
            .as_str()
            .context("question")?
            .to_owned(),
        request_id,
    ))
}

#[tokio::test]
async fn a_request_asked_again_keeps_its_one_question() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &["t-1"]).await?;
        let (mut server, _) = app_server(&node).await?;
        let handed = ask_approval(&node, &mut server, 51).await?;
        node.on_codex_request(&handed).await;
        let (question, request_id) = raise_for(&node).await?;
        node.drop_codex_requests().await?;
        let again = ask_approval(&node, &mut server, 52).await?;
        node.on_codex_request(&again).await;
        let asked = unanswered(&node).await?;
        assert_eq!(asked.len(), 1, "one question for the request asked twice");
        assert_eq!(asked[0]["question_id"], question.as_str());
        let held = node.triage_command(json!({"action":"requests"})).await?["requests"].clone();
        assert_eq!(
            (
                held[0]["request_id"].clone(),
                held[0]["question_id"].clone(),
                held[0]["answer_by_ms"].clone()
            ),
            (json!(request_id), json!(question), Value::Null),
            "the request asked again stays held by its question"
        );
        assert_eq!(
            node.triage_command(json!({"action":"events"})).await?["events"],
            json!([]),
            "the Agent is not asked again"
        );
        node.close_lost_questions().await?;
        assert_eq!(
            unanswered(&node).await?.len(),
            1,
            "a held request keeps its question open"
        );
        assert_eq!(answer(&node, &question, "允许本次").await?, "answered");
        node.wake_agent().await?;
        let sent = loop {
            let message = read(&mut server).await;
            if message.get("result").is_some() {
                break message;
            }
        };
        assert_eq!(
            (sent["id"].clone(), sent["result"].clone()),
            (json!(52), json!({"decision":"accept"})),
            "the answer goes to the request as asked on the latest connection"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn a_question_about_a_lost_request_closes_and_refuses_answers() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &["t-1"]).await?;
        let (mut server, _) = app_server(&node).await?;
        let handed = ask_approval(&node, &mut server, 61).await?;
        node.on_codex_request(&handed).await;
        let (question, _) = raise_for(&node).await?;
        node.drop_codex_requests().await?;
        node.close_lost_questions().await?;
        assert!(
            unanswered(&node).await?.is_empty(),
            "待处理 no longer lists it"
        );
        let network = node.network_id().await?;
        let chat = {
            let network = (network).to_owned();
            node.store
                .call("triage.test.conversation_questions", move |store| {
                    store.conversation_questions(&network, "c")
                })
                .await
        }?;
        assert_eq!(
            (chat[0]["question_id"].clone(), chat[0]["closed"].clone()),
            (json!(question), json!("request_lost"))
        );
        let refused = answer(&node, &question, "允许")
            .await
            .err()
            .context("refused")?;
        assert!(refused.to_string().contains("closed"), "{refused}");

        let late = ask_approval(&node, &mut server, 62).await?;
        node.on_codex_request(&late).await;
        let sent = loop {
            let message = read(&mut server).await;
            if message.get("result").is_some() {
                break message;
            }
        };
        assert_eq!(
            (sent["id"].clone(), sent["result"].clone()),
            (json!(62), json!({"decision":"decline"})),
            "a request asked again after its question closed gets the fallback"
        );
        assert!(unanswered(&node).await?.is_empty(), "and no new question");
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn first_provider_decline_survives_earlier_ranked_answer_and_cache_loss() -> Result<()> {
    with_node(async |node, admin, dir| {
        bot(&node, &admin, "c", "Bot", &[]).await?;
        let network = node.network_id().await?;
        let trigger = main_input_fact(&node, &admin, "c", "source").await?;
        let before = Config::load(&dir)?.roots;
        let folder = dir.join("proposed");
        std::fs::create_dir(&folder)?;
        let asked = node
            .propose_provider_config(&network, "c", &trigger, json!({"roots":[folder]}))
            .await?;
        let question = asked.value["question_id"].as_str().context("question")?;
        answer(&node, question, "不允许").await?;
        let first = node.apply_provider_config(&network, "c", question).await?;
        assert_eq!(first.value["status"], "declined");
        assert_eq!(first.receipt.approval, crate::tools::Approval::Denied);
        let key = question.to_owned();
        let net = network.clone();
        node.store
            .call("test.earlier_answer", move |store| {
                store.project_earlier_answer_fixture(&net, "c", &key, "允许")?;
                assert_eq!(
                    store.question(&net, &key)?.context("standing question")?["answer"]["text"],
                    "允许"
                );
                Ok(())
            })
            .await?;
        assert_eq!(
            node.apply_provider_config(&network, "c", question).await?,
            first
        );
        node.provider_proposals.lock().await.clear();
        assert_eq!(
            node.apply_provider_config(&network, "c", question).await?,
            first
        );
        assert_eq!(Config::load(&dir)?.roots, before);
        Ok(())
    })
    .await
}
