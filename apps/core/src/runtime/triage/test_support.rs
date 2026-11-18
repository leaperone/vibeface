use super::*;
use crate::crypto::LocalIdentity;
use crate::model::VersionVector;
use crate::runtime::test_support::{manager_settings, named_socket, node, provider_work};
use std::collections::BTreeMap;

pub(super) fn with_agent(node: &Runtime) {
    node.settings.lock().unwrap().manager = Some(manager_settings(
        crate::providers::Provider::Codex,
        std::env::temp_dir(),
        "fixture".into(),
    ));
}

pub(super) async fn main_input_fact(
    node: &Runtime,
    admin: &LocalIdentity,
    scope: &str,
    id: &str,
) -> Result<String> {
    let network = node.network_id().await?;
    let identity = node.identity.clone();
    let admin = admin.clone();
    let scope = scope.to_owned();
    let id = id.to_owned();
    node.store
        .call("triage.test.main_input_fact", move |store| {
            let scope = scope.as_str();
            let id = id.as_str();
            let bot = store
                .bot_for_conversation(&network, scope)?
                .context("input Bot")?;
            let target = crate::workspace::InputTarget::BotMain { bot_id: bot.bot_id };
            let trigger = crate::scheduling::message_trigger(id);
            if store.message(&network, scope, id)?.is_none() {
                let posted = admin.attest(
                    &network,
                    scope,
                    EventBody::InputPosted {
                        message_id: id.into(),
                        target: target.clone(),
                        intent: crate::workspace::InputIntent::Continue,
                        sequence: store.messages(&network, scope)?.len() as u64 + 1,
                        expected_revision: None,
                        text: id.into(),
                        attachments: vec![],
                        report_results: Some(true),
                        attestation: None,
                    },
                )?;
                store.append_event(&identity, &network, scope, posted, vec![], now_ms())?;
            }
            ensure!(
                store.trigger_input_target(&network, scope, &trigger)? == Some(target),
                "fixture input has no verified Main target"
            );
            Ok(trigger)
        })
        .await
}

pub(super) async fn complete_tracked_work(
    node: &Arc<Runtime>,
    network: &str,
    work_id: &str,
) -> Result<()> {
    let turn_id = format!("turn-{work_id}");
    node.signal_work(
        network,
        work_id,
        crate::work::Signal::TurnStarted {
            turn_id: turn_id.clone(),
            epoch: node.codex.epoch(),
        },
    )
    .await?;
    let completed = node
        .signal_work(
            network,
            work_id,
            crate::work::Signal::TurnCompleted {
                turn_id,
                status: vibeface_bridge::turn::TurnStatus::Completed,
                text: "done".into(),
                error: None,
            },
        )
        .await?;
    assert_eq!(
        completed.state,
        crate::work::WorkState::Finished {
            outcome: crate::model::WorkOutcome::Completed,
        }
    );
    Ok(())
}

pub(super) async fn external_work_thread_fact(
    node: &Arc<Runtime>,
    admin: &LocalIdentity,
    thread: &str,
) -> Result<()> {
    let trigger = main_input_fact(node, admin, "c", &format!("triage-origin-{thread}")).await?;
    let network = node.network_id().await?;
    let local = node.identity.device_id()?;
    let work_id = format!("work-external-{thread}");
    {
        let identity = (node.identity).to_owned();
        let network = (network).to_owned();
        let spec = crate::work::WorkSpec {
            work_id: work_id.clone(),
            trigger,
            call_ref: work_id.clone(),
            title: "external work".into(),
            thread: Some(thread.into()),
            ..provider_work(
                Provider::Codex,
                &local,
                &node.data_dir.canonicalize()?,
                "external work",
            )
        };
        node.store
            .call("triage.test.accept_work_fixture", move |store| {
                store.accept_work_fixture(&identity, &network, spec, now_ms())
            })
            .await
    }?;
    node.signal_work(
        &network,
        &work_id,
        crate::work::Signal::TurnStarted {
            turn_id: "t1".into(),
            epoch: node.codex.epoch(),
        },
    )
    .await?;
    let event = SealedAgentEvent::seal(turn(thread, "t1", "/w"), &local, "fixture", now_ms())?;
    let thread = thread.to_owned();
    node.store
        .call("triage.test.external_work_target", move |store| {
            let thread = thread.as_str();
            ensure!(
                !own_turn(store, &event)?,
                "an external work is not a coordination turn"
            );
            ensure!(
                store.native_input_target(&network, "c", thread, "t1")?
                    == Some(crate::workspace::InputTarget::Work { work_id }),
                "external session has no verified Work target"
            );
            Ok(())
        })
        .await
}

pub(super) async fn unanswered(node: &Runtime) -> Result<Vec<Value>> {
    let network = node.network_id().await?;
    {
        let network = (network).to_owned();
        node.store
            .call("triage.test.unanswered_questions", move |store| {
                store.unanswered_questions(&network)
            })
            .await
    }
}

pub(super) async fn answer(node: &Runtime, question: &str, text: &str) -> Result<&'static str> {
    let network = node.network_id().await?;
    Ok(node
        .post_answer(
            &network,
            question,
            text,
            None,
            AnswerAuthority::DecisionToken,
        )
        .await?
        .0)
}

pub(super) type Server = tokio_tungstenite::WebSocketStream<vibeface_bridge::platform::ipc::Stream>;

pub(super) async fn read(server: &mut Server) -> Value {
    use futures_util::StreamExt as _;
    let message = server.next().await.unwrap().unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

pub(super) fn handed(id: i64, method: &str, params: Value) -> ServerRequest {
    ServerRequest {
        id: vibeface_protocol::RpcId::Number(id),
        method: method.into(),
        thread_id: params["threadId"].as_str().map(str::to_owned),
        params,
    }
}

pub(super) async fn app_server(node: &Runtime) -> Result<(Server, ServerRequest)> {
    use futures_util::SinkExt as _;
    let mut listener = vibeface_bridge::platform::ipc::Listener::bind(
        &vibeface_bridge::platform::ipc::Endpoint::from_path(&named_socket(node))?,
    )
    .await?;
    let stream = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await??;
    let mut server = tokio_tungstenite::accept_async(stream).await?;
    let initialize = read(&mut server).await;
    let ready = json!({"id":initialize["id"],"result":{"userAgent":"codex/fixture"}});
    server.send(ready.to_string().into()).await?;
    assert_eq!(read(&mut server).await["method"], "initialized");
    let params = json!({"threadId":"t-1","turnId":"u-1","itemId":"fixture-approval","command":"rm -rf build","reason":"clean the build"});
    let request = json!({"id":41,"method":"item/commandExecution/requestApproval","params":params});
    server.send(request.to_string().into()).await?;
    while node.codex.client().snapshot().await["pending_requests"] == json!([]) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok((
        server,
        handed(41, "item/commandExecution/requestApproval", params),
    ))
}

pub(super) async fn answered_once(server: &mut Server) -> Value {
    let answer = read(server).await;
    assert_eq!(answer["id"], 41);
    let again = tokio::time::timeout(Duration::from_millis(300), read(server)).await;
    assert!(again.is_err(), "the request is answered once");
    answer
}

pub(super) async fn cleanup(node: Arc<Runtime>, dir: PathBuf) -> Result<()> {
    node.shutdown().await;
    #[cfg(unix)]
    match std::fs::remove_file(named_socket(&node)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    drop(node);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

pub(super) async fn with_node(
    check: impl AsyncFnOnce(Arc<Runtime>, LocalIdentity, PathBuf) -> Result<()>,
) -> Result<()> {
    let (node, admin, dir) = node().await?;
    let result = check(node.clone(), admin, dir.clone()).await;
    cleanup(node, dir).await?;
    result
}

/// A Codex work on `node` whose turn `u-1` runs in thread `t-9`.
pub(super) async fn running_work(
    node: &Arc<Runtime>,
    admin: &LocalIdentity,
    work_id: &str,
) -> Result<()> {
    let network = node.network_id().await?;
    let local = node.identity.device_id()?;
    let trigger = main_input_fact(node, admin, "c", "m1").await?;
    let spec = crate::work::WorkSpec {
        work_id: work_id.into(),
        trigger,
        call_ref: work_id.into(),
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
    let epoch = node.codex.epoch();
    let turn = crate::work::Signal::TurnStarted {
        turn_id: "u-1".into(),
        epoch,
    };
    let running = node.signal_work(&network, work_id, turn).await?;
    assert!(matches!(
        running.state,
        crate::work::WorkState::Running { .. }
    ));
    Ok(())
}

pub(super) async fn connected(node: &Runtime) {
    while node.codex.status().await["connection"]["state"] != "connected" {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub(super) async fn state(node: &Runtime, work_id: &str) -> Result<crate::work::WorkState> {
    Ok({
        let work_id = (work_id).to_owned();
        node.store
            .call("triage.test.work_record", move |store| {
                store.work_record(&work_id)
            })
            .await
    }?
    .context("work")?
    .state)
}

pub(super) fn asks_permission(session: &str) -> HookEvent {
    HookEvent {
        agent: Provider::Claude.into(),
        event: triage::Kind::NeedsInput,
        hold: Some(RequestKind::Permission),
        ..turn(session, "t1", "/w")
    }
}

pub(super) fn turn(session: &str, turn: &str, cwd: &str) -> HookEvent {
    HookEvent {
        source: triage::Source::Hook,
        session_id: session.into(),
        turn_id: turn.into(),
        cwd: cwd.into(),
        status: Some("failed".into()),
        last_message: "tests fail".into(),
        ..Default::default()
    }
}

pub(super) async fn attributions(node: &Runtime, scope: &str) -> Result<usize> {
    let network = node.network_id().await?;
    let scope = scope.to_owned();
    let context = node
        .store
        .call("triage.test.attributions", move |store| {
            crate::store::context_after(
                &store.connection,
                &network,
                &scope,
                None,
                &VersionVector::new(),
            )
        })
        .await?;
    Ok(context
        .events
        .iter()
        .filter(|event| matches!(event.event.body, EventBody::SessionAttributed { .. }))
        .count())
}

pub(super) async fn owner_of(node: &Runtime, session: &str) -> Result<Option<String>> {
    let network = node.network_id().await?;
    let local = node.identity.device_id()?;
    {
        let network = (network).to_owned();
        let local_node = (local).to_owned();
        let session = (session).to_owned();
        node.store
            .call("triage.test.session_owner", move |store| {
                store.session_owner(&network, &local_node, &session)
            })
            .await
    }
}

/// The version of a session as its bot's chat lists it.
pub(super) async fn version_of(node: &Runtime, session: &str) -> Result<String> {
    let network = node.network_id().await?;
    let session = session.to_owned();
    node.store
        .call("triage.test.session_version", move |store| {
            for scope in ["c", "d", "e"] {
                if let Some(row) = store
                    .attributed_sessions(&network, scope)?
                    .into_iter()
                    .find(|row| row["session_id"] == session)
                {
                    return Ok(row["version"].as_str().context("version")?.to_owned());
                }
            }
            Ok(String::new())
        })
        .await
}

pub(super) async fn delete_bot(
    node: &Runtime,
    admin: &LocalIdentity,
    scope: &str,
) -> Result<String> {
    let network = node.network_id().await?;
    let deleted = admin.attest(
        &network,
        scope,
        EventBody::BotDeleted {
            attestation: None,
            cause: None,
        },
    )?;
    let signed = deleted.attestation().cloned().context("signed")?;
    let written = {
        let identity = (node.identity).to_owned();
        let network = (network).to_owned();
        let conversation = (scope).to_owned();
        let attestation = signed;
        node.store
            .call("triage.test.delete_bot", move |store| {
                store.delete_bot(&identity, &network, &conversation, attestation, now_ms())
            })
            .await
    }?
    .context("deleted")?;
    Ok(written.hash)
}

#[derive(Clone, Copy)]
pub(super) enum CoordinationReply {
    Complete,
    Refused,
    Failed,
    Unknown,
}

pub(super) async fn coordination_app_server(
    node: &Runtime,
) -> Result<tokio::sync::mpsc::UnboundedReceiver<Value>> {
    coordination_server(node, CoordinationReply::Complete).await
}

pub(super) async fn coordination_server(
    node: &Runtime,
    response: CoordinationReply,
) -> Result<tokio::sync::mpsc::UnboundedReceiver<Value>> {
    use futures_util::{SinkExt as _, StreamExt as _};
    let mut listener = vibeface_bridge::platform::ipc::Listener::bind(
        &vibeface_bridge::platform::ipc::Endpoint::from_path(&named_socket(node))?,
    )
    .await?;
    let (sent, received) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let stream = tokio::select! {
                _ = sent.closed() => break,
                accepted = listener.accept() => match accepted { Ok(stream) => stream, Err(_) => break },
            };
            let sent = sent.clone();
            tokio::spawn(async move {
                let Ok(mut server) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                loop {
                    let message = tokio::select! {
                        _ = sent.closed() => break,
                        message = server.next() => match message { Some(Ok(message)) => message, _ => break },
                    };
                    let Ok(request) = serde_json::from_slice::<Value>(&message.into_data()) else {
                        continue;
                    };
                    if request.get("id").is_none() {
                        continue;
                    }
                    let _ = sent.send(request.clone());
                    if request["method"] == "turn/start" {
                        match response {
                            CoordinationReply::Unknown => break,
                            CoordinationReply::Refused => {
                                let reply = json!({"id":request["id"],"error":{"code":-32602,"message":"fixture refused before model start"}});
                                let _ = server.send(reply.to_string().into()).await;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    let result = match request["method"].as_str() {
                        Some("initialize") => json!({"userAgent":"codex/fixture"}),
                        Some("thread/start") => json!({"thread":{"id":"coord-thread"}}),
                        Some("turn/start") => json!({"turn":{"id":"coord-turn"}}),
                        _ => json!({}),
                    };
                    let mut replies = vec![json!({"id":request["id"],"result":result})];
                    if request["method"] == "turn/start" {
                        replies.push(json!({"method":"turn/completed","params":{
                            "threadId":"coord-thread",
                            "turn":{"id":"coord-turn","status":if matches!(response, CoordinationReply::Failed) { "failed" } else { "completed" }}}}));
                    }
                    for reply in replies {
                        let _ = server.send(reply.to_string().into()).await;
                    }
                }
            });
        }
    });
    tokio::time::timeout(Duration::from_secs(5), connected(node)).await?;
    Ok(received)
}

pub(super) fn turn_starts(
    requests: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
) -> Vec<String> {
    std::iter::from_fn(|| requests.try_recv().ok())
        .filter(|request| request["method"] == "turn/start")
        .map(|request| request["params"].to_string())
        .collect()
}

pub(super) fn controlled_pi(node: &Runtime, dir: &Path, mode: &str) -> Result<()> {
    let name = if cfg!(windows) {
        "fake-pi.cmd"
    } else {
        "fake-pi"
    };
    let bin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    vibeface_bridge::platform::program::ensure_runnable(&bin, "controlled pi fixture")?;
    let mut providers = node.providers.lock().unwrap();
    providers.switches.execution = [Provider::Pi].into();
    providers.bins.insert(Provider::Pi, bin);
    providers.env.apply(
        Provider::Pi,
        BTreeMap::from([
            (
                "PI_CODING_AGENT_SESSION_DIR".into(),
                Some(dir.join("pi-sessions").display().to_string()),
            ),
            (
                "FAKE_PI_RECORDS_DIR".into(),
                Some(dir.join("pi-log-records").display().to_string()),
            ),
            ("FAKE_PI_MODE".into(), Some(mode.into())),
        ]),
    )?;
    Ok(())
}
