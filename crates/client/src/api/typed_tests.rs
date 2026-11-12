use super::*;
use crate::connection::tests::{receive, send, server};
use vibeface_secure::{
    Identity,
    membership::{Genesis, Member, NetworkState},
};

#[test]
fn typed_documents_preserve_domain_tags_and_reject_unknown_fields() -> Result<()> {
    let schedule =
        json!({"type":"schedule","conversation_id":"chat","schedule_id":"s1","action":"pause"});
    let parsed: Action = serde_json::from_value(schedule.clone())?;
    assert_eq!(serde_json::to_value(parsed)?, schedule);
    let delivery = json!({"type":"delivery","op":"get","order_id":"o1"});
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<ReadRequest>(delivery.clone())?)?,
        delivery
    );
    for document in [
        json!({"type":"home","extra":true}),
        json!({"type":"delivery","op":"get","order_id":"o1","extra":true}),
        json!({"type":"logs_filtered","sources":[],"extra":true}),
    ] {
        assert!(
            serde_json::from_value::<ReadRequest>(document.clone()).is_err(),
            "accepted unknown read field: {document}"
        );
    }
    for document in [
        json!({"type":"schedule","conversation_id":"chat","schedule_id":"s1","action":"pause","extra":true}),
        json!({"type":"node_settings","roots":[],"manager":{"provider":"codex","workspace":"/tmp","model":"fixture","extra":true}}),
        json!({"type":"provider_settings","provider":"opencode","connect_local":false,"env":{},"opencode_endpoint":{"protocol":"openai","base_url":"http://fixture.invalid","models":[],"extra":true}}),
        json!({"type":"delivery","op":"prepare","intent_id":"i","receiver_peer":"r","provider_peer":"p","mode":"charity","acceptance":"recoverable","name":"x","content_base64":"eA==","max_service_amount":"0","max_gas_amount":"0","extra":true}),
    ] {
        assert!(
            serde_json::from_value::<Action>(document.clone()).is_err(),
            "accepted unknown action field: {document}"
        );
    }
    Ok(())
}

#[test]
fn every_unit_operation_accepts_only_its_discriminants() -> Result<()> {
    let reads = [
        ReadRequest::WriteState,
        ReadRequest::Home,
        ReadRequest::Workspace,
        ReadRequest::WorkspaceOverview,
        ReadRequest::Nodes,
        ReadRequest::NodeSettings,
        ReadRequest::Logs,
        ReadRequest::Diagnostics,
        ReadRequest::OpenWorks,
        ReadRequest::ShareLink,
        ReadRequest::TestCodexAppServer,
        ReadRequest::Questions,
        ReadRequest::Notifications,
        ReadRequest::RelaySettings,
        ReadRequest::TriageSettings,
        ReadRequest::Delivery(DeliveryRead::Status),
        ReadRequest::Delivery(DeliveryRead::List),
        ReadRequest::Delivery(DeliveryRead::Providers),
        ReadRequest::Delivery(DeliveryRead::Budgets),
    ];
    for read in reads {
        let valid = serde_json::to_value(read)?;
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<ReadRequest>(valid.clone())?)?,
            valid
        );
        for extra in [json!(true), Value::Null, json!({"nested":"unexpected"})] {
            let mut invalid = valid.clone();
            invalid["extra"] = extra;
            assert!(
                serde_json::from_value::<ReadRequest>(invalid.clone()).is_err(),
                "accepted unknown read field: {invalid}"
            );
        }
    }
    for action in [Action::TestWebhook, Action::ConfirmWritable] {
        let valid = serde_json::to_value(action)?;
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<Action>(valid.clone())?)?,
            valid
        );
        for extra in [json!(true), Value::Null, json!({"nested":"unexpected"})] {
            let mut invalid = valid.clone();
            invalid["extra"] = extra;
            assert!(
                serde_json::from_value::<Action>(invalid.clone()).is_err(),
                "accepted unknown action field: {invalid}"
            );
        }
    }
    Ok(())
}

#[test]
fn endpoint_and_secret_changes_preserve_omitted_clear_and_set() -> Result<()> {
    let base =
        json!({"type":"provider_settings","provider":"opencode","connect_local":false,"env":{}});
    for endpoint in [
        None,
        Some(Value::Null),
        Some(
            json!({"protocol":"openai_compatible","base_url":"http://fixture.invalid","models":["fixture"]}),
        ),
        Some(
            json!({"protocol":"anthropic","base_url":"http://fixture.invalid","models":[],"api_key":null}),
        ),
        Some(
            json!({"protocol":"openai","base_url":"http://fixture.invalid","models":[],"api_key":"synthetic-secret"}),
        ),
    ] {
        let mut wanted = base.clone();
        if let Some(endpoint) = endpoint {
            wanted["opencode_endpoint"] = endpoint;
        }
        let parsed: Action = serde_json::from_value(wanted.clone())?;
        assert_eq!(serde_json::to_value(parsed)?, wanted);
    }
    let triage = json!({"type":"triage","default_outlet":"record","projects":[],"webhook_url":null,"scan_codex_sessions":false,"codex_sessions":null});
    for secret in [None, Some(Value::Null), Some(json!("synthetic-secret"))] {
        let mut wanted = triage.clone();
        if let Some(secret) = secret {
            wanted["webhook_secret"] = secret;
        }
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<Action>(wanted.clone())?)?,
            wanted
        );
    }
    Ok(())
}

#[test]
fn missing_reads_use_the_web_methods_and_exact_time_window() {
    assert_eq!(
        ReadRequest::OpenWorks.wire(),
        (true, "open_works", json!({}))
    );
    assert_eq!(
        ReadRequest::ShareLink.wire(),
        (false, "share_link", json!({}))
    );
    assert_eq!(
        ReadRequest::TestCodexAppServer.wire(),
        (false, "test_codex_app_server", json!({}))
    );
    assert_eq!(
        ReadRequest::DiagnosticsFiltered {
            since_ms: Some(100),
            until_ms: Some(200)
        }
        .wire(),
        (false, "diagnostics", json!({"since_ms":100,"until_ms":200}))
    );
}

struct UnansweredEvidence {
    response: Option<Value>,
    drop_ack: bool,
    jobs: Vec<Value>,
    writes: std::sync::atomic::AtomicUsize,
}

async fn fixture(
    node: &Identity,
    state: NetworkState,
    client: String,
    pushes_on_write: Option<broadcast::Sender<Value>>,
) -> Result<(Endpoint, tokio::task::JoinHandle<Result<()>>)> {
    fixture_with_unanswered(node, state, client, pushes_on_write, None).await
}

async fn fixture_with_unanswered(
    node: &Identity,
    state: NetworkState,
    client: String,
    pushes_on_write: Option<broadcast::Sender<Value>>,
    unanswered: Option<Arc<UnansweredEvidence>>,
) -> Result<(Endpoint, tokio::task::JoinHandle<Result<()>>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = Endpoint::Gateway(crate::Entry {
        origin: format!("http://{}", listener.local_addr()?),
        node_id: node.device_id(),
    });
    let node = Identity::from_secret_hex(&node.secret_hex())?;
    let task = tokio::spawn(async move {
        let (mut socket, mut channel) = server(listener, &node).await?;
        let mut pushed = pushes_on_write.map(|sender| sender.subscribe());
        loop {
            let request = if let Some(pushes) = pushed.as_mut() {
                tokio::select! {
                    request = receive(&mut socket, &mut channel) => request,
                    payload = pushes.recv() => {
                        if let Ok(payload) = payload { send(&mut socket, &mut channel, payload).await?; }
                        continue;
                    }
                }
            } else {
                receive(&mut socket, &mut channel).await
            };
            let Ok(request) = request else {
                break;
            };
            let result = match request["method"].as_str() {
                Some("authenticate") => {
                    json!({"network_id":state.genesis.network_id,"principal":{"source":"membership","device_id":client,"can_manage":true}})
                }
                Some("directory") => json!({"state":state,"now_ms":vibeface_protocol::now_ms()}),
                Some("subscribe") => json!({}),
                Some("write_state") => json!({"state":"writable"}),
                Some("configure_node") => json!({"settings":request["params"]}),
                Some("rpc")
                    if request["params"]["method"] == "set_owner"
                        || request["params"]["method"] == "dismiss_work" =>
                {
                    let params = &request["params"];
                    let body = if params["method"] == "set_owner" {
                        json!({"type":"conversation_owner","scheduler_node":params["scheduler_node"],"epoch":params["epoch"]})
                    } else {
                        json!({"type":"notification_acked","notification_id":params["work_id"],"client_id":"inbox"})
                    };
                    vibeface_secure::verify(
                        &client,
                        vibeface_secure::join::ATTESTATION_DOMAIN,
                        &vibeface_secure::join::attestation_payload(
                            &state.genesis.network_id,
                            params["conversation_id"].as_str().context("conversation")?,
                            &body.to_string(),
                        )?,
                        params["attestation"]["signature"]
                            .as_str()
                            .context("signature")?,
                    )?;
                    json!({"saved":true})
                }
                Some("rpc")
                    if matches!(
                        request["params"]["method"].as_str(),
                        Some("reply_unanswered" | "dismiss_unanswered")
                    ) =>
                {
                    if let Some(evidence) = &unanswered {
                        evidence.writes.fetch_add(1, Ordering::SeqCst);
                        if evidence.drop_ack {
                            socket.close(None).await?;
                            break;
                        }
                    }
                    if let Some(response) = unanswered
                        .as_ref()
                        .and_then(|evidence| evidence.response.as_ref())
                    {
                        response.clone()
                    } else {
                        let params = &request["params"];
                        let receipts: Vec<_> = params["message_ids"].as_array().context("selected messages")?.iter().map(|id| {
                            json!({"network_id":state.genesis.network_id,"conversation_id":params["conversation_id"],"trigger":format!("message:{}",id.as_str().expect("fixture message ID")),"state":if params["method"] == "dismiss_unanswered" {"cancelled"} else {"queued"}})
                        }).collect();
                        json!({"receipts":receipts})
                    }
                }
                Some("rpc") if request["params"]["method"] == "conversation" => {
                    json!({"conversation":{"conversation_id":request["params"]["conversation_id"],"owner":node.device_id()},"scheduler":{"jobs":unanswered.as_ref().map_or(&[][..], |evidence| evidence.jobs.as_slice())}})
                }
                Some("local_access") => json!({"enabled":request["params"]["enabled"]}),
                other => anyhow::bail!("unexpected typed fixture method {other:?}"),
            };
            send(
                &mut socket,
                &mut channel,
                json!({"id":request["id"],"result":result}),
            )
            .await?;
        }
        Ok(())
    });
    Ok((endpoint, task))
}

#[tokio::test]
async fn reader_reads_and_watches_while_writer_holds_the_outbox() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("native-reader-{}", uuid::Uuid::new_v4()));
    let mut key = ClientKey::create(&dir.join("key"), "fixture password")?;
    let node = Identity::generate()?;
    let state = NetworkState::new(Genesis::create(
        &Identity::generate()?,
        Member::client(&key.identity, "client", true)?,
        Member::node(&node, "node")?,
    )?)?;
    let (writer_endpoint, writer_task) =
        fixture(&node, state.clone(), key.device_id().into(), None).await?;
    let (next_endpoint, next_task) =
        fixture(&node, state.clone(), key.device_id().into(), None).await?;
    let (pushes, _) = broadcast::channel(8);
    let (reader_endpoint, reader_task) = fixture(
        &node,
        state.clone(),
        key.device_id().into(),
        Some(pushes.clone()),
    )
    .await?;
    let Endpoint::Gateway(entry) = &writer_endpoint else {
        unreachable!()
    };
    key.remember(&state.genesis.network_id, entry.clone())?;
    let key = Arc::new(key);
    let writer = Client::open(key.clone(), writer_endpoint, dir.join("outbox")).await?;
    let reader = ClientReader::open(key, reader_endpoint).await?;
    assert_eq!(reader.client_id(), writer.client_id());
    assert_eq!(reader.network_id(), writer.network_id());
    assert_eq!(reader.target_node(), writer.target_node());
    let mut events = reader.subscribe();
    let outbox = writer.outbox.clone();
    let (locked, lock_observed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let holder = tokio::task::spawn_blocking(move || -> Result<()> {
        let _guard = outbox.lock().expect("held fixture outbox");
        locked
            .send(())
            .map_err(|_| anyhow::anyhow!("reader fixture stopped"))?;
        released.recv().context("reader fixture released")?;
        Ok(())
    });
    lock_observed.await?;
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read(ReadRequest::WriteState)
        )
        .await??,
        json!({"state":"writable"})
    );
    release.send(())?;
    holder.await??;
    let written = writer
        .submit(Action::NodeSettings(NodeSettings {
            roots: vec![],
            manager: None,
        }))
        .await?;
    assert_eq!(written.state, OperationState::Accepted);
    pushes.send(json!({"type":"changed","scope":"chat"}))?;
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await??;
    assert_eq!(event.payload, json!({"type":"changed","scope":"chat"}));
    assert_eq!(event.generation, reader.generation());
    writer.close();
    drop(writer);
    assert_eq!(
        reader.read(ReadRequest::WriteState).await?,
        json!({"state":"writable"})
    );
    let next = reader.reconnect(next_endpoint).await?;
    assert_ne!(next.generation(), reader.generation());
    assert_eq!(next.network_id(), reader.network_id());
    assert!(reader.read(ReadRequest::WriteState).await.is_err());
    assert_eq!(
        next.read(ReadRequest::WriteState).await?,
        json!({"state":"writable"})
    );
    next.close();
    assert!(next.read(ReadRequest::WriteState).await.is_err());
    drop(next);
    drop(reader);
    writer_task.await??;
    reader_task.await??;
    next_task.await??;
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn new_signed_writes_verify_and_preserve_original_operation_ids() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("native-signed-controls-{}", uuid::Uuid::new_v4()));
    let mut key = ClientKey::create(&dir.join("key"), "fixture password")?;
    let node = Identity::generate()?;
    let state = NetworkState::new(Genesis::create(
        &Identity::generate()?,
        Member::client(&key.identity, "client", true)?,
        Member::node(&node, "node")?,
    )?)?;
    let (endpoint, task) = fixture(&node, state.clone(), key.device_id().into(), None).await?;
    let Endpoint::Gateway(entry) = &endpoint else {
        unreachable!()
    };
    key.remember(&state.genesis.network_id, entry.clone())?;
    let client = Client::open(Arc::new(key), endpoint, dir.join("outbox")).await?;
    for action in [
        Action::SetOwner {
            conversation_id: "chat".into(),
            scheduler_node: node.device_id(),
            epoch: 2,
        },
        Action::DismissWork {
            conversation_id: "chat".into(),
            work_id: "failed-work".into(),
        },
    ] {
        let staged = client.stage(action.clone()).await?;
        let result = client.submit(action.clone()).await?;
        assert_eq!(staged.id, result.id);
        assert_eq!(result.state, OperationState::Accepted);
        assert_eq!(client.submit(action).await?.id, staged.id);
    }
    assert!(
        client
            .stage(Action::Unanswered {
                conversation_id: "chat".into(),
                message_ids: vec![],
                action: UnansweredAction::Reply
            })
            .await
            .is_err()
    );
    for action in [UnansweredAction::Reply, UnansweredAction::Dismiss] {
        assert_eq!(
            client
                .submit(Action::Unanswered {
                    conversation_id: "chat".into(),
                    message_ids: vec!["original-message".into()],
                    action
                })
                .await?
                .state,
            OperationState::Accepted
        );
    }
    assert_eq!(
        client
            .submit(Action::LocalAccess { enabled: false })
            .await?
            .result,
        Some(json!({"enabled":false}))
    );
    client.close();
    drop(client);
    task.await??;
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn unanswered_requires_full_selected_receipts_and_preserves_unknown_outcomes() -> Result<()> {
    let dir = std::env::temp_dir().join(format!(
        "native-unanswered-receipts-{}",
        uuid::Uuid::new_v4()
    ));
    let mut key = ClientKey::create(&dir.join("key"), "fixture password")?;
    let node = Identity::generate()?;
    let state = NetworkState::new(Genesis::create(
        &Identity::generate()?,
        Member::client(&key.identity, "client", true)?,
        Member::node(&node, "node")?,
    )?)?;
    let receipt = |id: &str, outcome: &str| json!({"network_id":state.genesis.network_id,"conversation_id":"chat","trigger":format!("message:{id}"),"state":outcome});
    let mut wrong_network = receipt("m1", "queued");
    wrong_network["network_id"] = json!("another-network");
    let mut wrong_conversation = receipt("m1", "queued");
    wrong_conversation["conversation_id"] = json!("another-chat");
    let cases = [
        (
            UnansweredAction::Reply,
            json!({"receipts":[receipt("m2","queued"),receipt("m1","completed")]}),
            Some(OperationState::Accepted),
        ),
        (
            UnansweredAction::Dismiss,
            json!({"receipts":[receipt("m1","cancelled"),receipt("m2","cancelled")]}),
            Some(OperationState::Accepted),
        ),
        (
            UnansweredAction::Reply,
            json!({"receipts":[]}),
            Some(OperationState::Unknown),
        ),
        (
            UnansweredAction::Reply,
            json!({"receipts":[receipt("m1","queued")]}),
            Some(OperationState::Unknown),
        ),
        (
            UnansweredAction::Dismiss,
            json!({"receipts":[receipt("m1","cancelled")]}),
            Some(OperationState::Unknown),
        ),
        (
            UnansweredAction::Dismiss,
            json!({"receipts":[receipt("m1","running"),receipt("m2","cancelled")]}),
            Some(OperationState::Unknown),
        ),
        (
            UnansweredAction::Reply,
            json!({"receipts":[receipt("m1","cancelled"),receipt("m2","queued")]}),
            Some(OperationState::Unknown),
        ),
        (
            UnansweredAction::Reply,
            json!({"receipts":[wrong_network,receipt("m2","queued")]}),
            None,
        ),
        (
            UnansweredAction::Reply,
            json!({"receipts":[wrong_conversation,receipt("m2","queued")]}),
            None,
        ),
        (
            UnansweredAction::Reply,
            json!({"receipts":[receipt("unselected","queued"),receipt("m2","queued")]}),
            None,
        ),
        (
            UnansweredAction::Reply,
            json!({"receipts":[receipt("m1","queued"),receipt("m1","queued")]}),
            None,
        ),
        (UnansweredAction::Reply, json!({"receipts":{}}), None),
        (UnansweredAction::Reply, json!({"receipts":[{}]}), None),
    ];
    for (index, (action, response, expected)) in cases.into_iter().enumerate() {
        let evidence = Arc::new(UnansweredEvidence {
            response: Some(response),
            drop_ack: false,
            jobs: vec![],
            writes: 0.into(),
        });
        let (endpoint, task) = fixture_with_unanswered(
            &node,
            state.clone(),
            key.device_id().into(),
            None,
            Some(evidence.clone()),
        )
        .await?;
        let Endpoint::Gateway(entry) = &endpoint else {
            unreachable!()
        };
        key.remember(&state.genesis.network_id, entry.clone())?;
        let client = Client::open(
            Arc::new(ClientKey::open(&dir.join("key"), "fixture password")?),
            endpoint,
            dir.join(format!("outbox-{index}")),
        )
        .await?;
        let original = client
            .stage(Action::Unanswered {
                conversation_id: "chat".into(),
                message_ids: vec!["m1".into(), "m2".into()],
                action,
            })
            .await?;
        let result = client.send_staged(&original.id).await;
        if let Some(expected) = expected {
            let result = result?;
            assert_eq!(result.id, original.id);
            assert_eq!(result.state, expected, "receipt case {index}");
            if expected == OperationState::Unknown {
                let reconciled = client.reconcile(&original.id).await?;
                assert_eq!(reconciled.id, original.id);
                assert_eq!(reconciled.state, OperationState::Unknown);
            }
        } else {
            assert!(result.is_err(), "malformed receipt case {index}");
        }
        if expected != Some(OperationState::Accepted) {
            let pending = client.pending().await?;
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].id, original.id);
            assert_eq!(pending[0].state, OperationState::Unknown);
        }
        assert_eq!(evidence.writes.load(Ordering::SeqCst), 1);
        client.close();
        drop(client);
        task.await??;
    }
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn unanswered_lost_acknowledgement_reads_original_jobs_without_resubmitting() -> Result<()> {
    let dir = std::env::temp_dir().join(format!(
        "native-unanswered-lost-ack-{}",
        uuid::Uuid::new_v4()
    ));
    let mut key = ClientKey::create(&dir.join("key"), "fixture password")?;
    let node = Identity::generate()?;
    let state = NetworkState::new(Genesis::create(
        &Identity::generate()?,
        Member::client(&key.identity, "client", true)?,
        Member::node(&node, "node")?,
    )?)?;
    for (index, (action, job_state)) in [
        (UnansweredAction::Reply, "completed"),
        (UnansweredAction::Dismiss, "cancelled"),
    ]
    .into_iter()
    .enumerate()
    {
        let evidence = Arc::new(UnansweredEvidence {
            response: None,
            drop_ack: true,
            jobs: vec![
                json!({"trigger":"message:m1","state":{"state":job_state}}),
                json!({"trigger":"message:m2","state":{"state":job_state}}),
            ],
            writes: 0.into(),
        });
        let (endpoint, task) = fixture_with_unanswered(
            &node,
            state.clone(),
            key.device_id().into(),
            None,
            Some(evidence.clone()),
        )
        .await?;
        let Endpoint::Gateway(entry) = &endpoint else {
            unreachable!()
        };
        key.remember(&state.genesis.network_id, entry.clone())?;
        let client = Client::open(
            Arc::new(ClientKey::open(&dir.join("key"), "fixture password")?),
            endpoint,
            dir.join(format!("outbox-{index}")),
        )
        .await?;
        let original = client
            .stage(Action::Unanswered {
                conversation_id: "chat".into(),
                message_ids: vec!["m1".into(), "m2".into()],
                action,
            })
            .await?;
        let unknown = client.send_staged(&original.id).await?;
        assert_eq!(unknown.id, original.id);
        assert_eq!(unknown.state, OperationState::Unknown);
        task.await??;
        let (next_endpoint, next_task) = fixture_with_unanswered(
            &node,
            state.clone(),
            client.client_id().into(),
            None,
            Some(evidence.clone()),
        )
        .await?;
        let next = client.reconnect(next_endpoint).await?;
        let reconciled = next.reconcile(&original.id).await?;
        assert_eq!(reconciled.id, original.id);
        assert_eq!(reconciled.state, OperationState::Accepted);
        assert_eq!(
            evidence.writes.load(Ordering::SeqCst),
            1,
            "reconciliation must not repeat either unanswered action"
        );
        next.close();
        drop(next);
        drop(client);
        next_task.await??;
    }
    std::fs::remove_dir_all(dir)?;
    Ok(())
}
