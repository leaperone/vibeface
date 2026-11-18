use super::offline_tests::{Evidence, fixture, input, provider_change};
use super::*;
use vibeface_secure::{
    Identity,
    membership::{Genesis, GovernanceEvent, Member, NetworkState},
};

struct Setup {
    dir: PathBuf,
    client: Client,
    node: Identity,
    state: NetworkState,
    evidence: Arc<Mutex<Evidence>>,
    server: tokio::task::JoinHandle<Result<()>>,
}

impl Setup {
    async fn open(manage: bool) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("action-policy-{}", uuid::Uuid::new_v4()));
        let mut key = ClientKey::create(&dir.join("key"), "fixture password")?;
        let node = Identity::generate()?;
        let manager = Identity::generate()?;
        let mut state = NetworkState::new(Genesis::create(
            &Identity::generate()?,
            Member::client(&manager, "manager", true)?,
            Member::node(&node, "node")?,
        )?)?;
        state.append(GovernanceEvent::grant(
            &manager,
            &state.project()?,
            Member::client(&key.identity, "client", manage)?,
        )?)?;
        let evidence = Arc::new(Mutex::new(Evidence {
            counts: BTreeMap::new(),
            inputs: BTreeMap::new(),
            write_state: "writable".into(),
            revoked: false,
            controls: vec![],
            settings: json!({"roots":[],"manager":null}),
        }));
        let (endpoint, server) = fixture(
            &node.secret_hex(),
            state.clone(),
            key.device_id().into(),
            evidence.clone(),
            None,
        )
        .await?;
        let Endpoint::Gateway(entry) = &endpoint else {
            unreachable!()
        };
        key.remember(&state.genesis.network_id, entry.clone())?;
        let client = Client::open(Arc::new(key), endpoint, dir.join("outbox")).await?;
        assert_eq!(
            client.authority(),
            ClientAuthority::Membership { can_manage: manage }
        );
        Ok(Self {
            dir,
            client,
            node,
            state,
            evidence,
            server,
        })
    }

    async fn finish(self) -> Result<()> {
        self.client.close();
        drop(self.client);
        self.server.await??;
        std::fs::remove_dir_all(self.dir)?;
        Ok(())
    }
}

fn samples(node: &str) -> Result<Vec<(Action, ActionPolicy, &'static str)>> {
    use ActionPolicy::{
        EnableLocalAccess, ManagingMemberWrite, MemberWrite, NodeControl, SafetyStop,
    };
    let documents = [
        (
            json!({"type":"workspace","command":{"kind":"post_channel_message","channel_id":"channel","message_id":"post","text":"fixture"}}),
            MemberWrite,
            "workspace_command",
        ),
        (
            json!({"type":"memory","conversation_id":"chat","memory_id":"memo","base":{"epoch":1},"after":"fixture"}),
            MemberWrite,
            "control_memory",
        ),
        (
            json!({"type":"schedule","conversation_id":"chat","schedule_id":"schedule","action":"pause"}),
            MemberWrite,
            "control_schedule",
        ),
        (
            json!({"type":"set_owner","conversation_id":"chat","scheduler_node":node,"epoch":1}),
            MemberWrite,
            "set_owner",
        ),
        (
            json!({"type":"dismiss_work","conversation_id":"chat","work_id":"work"}),
            MemberWrite,
            "dismiss_work",
        ),
        (
            json!({"type":"unanswered","conversation_id":"chat","message_ids":["message"],"action":"reply"}),
            MemberWrite,
            "reply_unanswered",
        ),
        (
            json!({"type":"unanswered","conversation_id":"chat","message_ids":["message"],"action":"dismiss"}),
            MemberWrite,
            "dismiss_unanswered",
        ),
        (
            json!({"type":"local_access","enabled":false}),
            NodeControl,
            "local_access",
        ),
        (
            json!({"type":"local_access","enabled":true}),
            EnableLocalAccess,
            "local_access",
        ),
        (
            json!({"type":"stop_reply","conversation_id":"chat","trigger":"message:original"}),
            SafetyStop,
            "cancel_manager",
        ),
        (
            json!({"type":"stop_work","conversation_id":"chat","work_id":"work"}),
            SafetyStop,
            "interrupt_work",
        ),
        (
            json!({"type":"answer","conversation_id":"chat","question_id":"question","text":"fixture"}),
            MemberWrite,
            "answer",
        ),
        (
            json!({"type":"create_bot","bot_id":"bot","conversation_id":"chat","name":"fixture","instructions":"fixture","scheduler_node":node}),
            MemberWrite,
            "create_bot",
        ),
        (
            json!({"type":"delete_bot","conversation_id":"chat"}),
            MemberWrite,
            "delete_bot",
        ),
        (
            json!({"type":"set_primary_bot","conversation_id":"chat"}),
            MemberWrite,
            "set_primary_bot",
        ),
        (
            json!({"type":"set_bot_project","conversation_id":"chat"}),
            MemberWrite,
            "set_bot_project",
        ),
        (
            json!({"type":"set_bot_avatar","conversation_id":"chat"}),
            MemberWrite,
            "set_bot_avatar",
        ),
        (
            json!({"type":"node_settings","roots":[],"manager":null}),
            NodeControl,
            "configure_node",
        ),
        (
            json!({"type":"triage","default_outlet":"record","projects":[],"webhook_url":null,"scan_codex_sessions":false,"codex_sessions":null}),
            NodeControl,
            "configure_triage",
        ),
        (
            json!({"type":"relay","volunteer":false}),
            NodeControl,
            "configure_relay",
        ),
        (
            json!({"type":"create_decision_token","label":"fixture"}),
            NodeControl,
            "create_decision_token",
        ),
        (
            json!({"type":"revoke_decision_token","token_id":"token"}),
            NodeControl,
            "revoke_decision_token",
        ),
        (json!({"type":"test_webhook"}), NodeControl, "test_webhook"),
        (
            json!({"type":"confirm_writable"}),
            NodeControl,
            "confirm_writable",
        ),
        (
            json!({"type":"ack_notification","conversation_id":"chat","notification_id":"notification"}),
            MemberWrite,
            "ack",
        ),
        (
            json!({"type":"resolve_check","conversation_id":"chat","check_id":"check","note":"fixture"}),
            MemberWrite,
            "resolve",
        ),
        (
            json!({"type":"resolve_check","conversation_id":"chat","check_id":"work:work","work_id":"work","outcome":"interrupted","note":"fixture"}),
            MemberWrite,
            "resolve",
        ),
        (
            json!({"type":"delivery","op":"configure","enabled":true,"charity_enabled":true,"paid_enabled":false}),
            ManagingMemberWrite,
            "delivery",
        ),
        (
            json!({"type":"delivery","op":"prepare","intent_id":"00000000-0000-4000-8000-000000000001","receiver_peer":node,"provider_peer":node,"mode":"charity","acceptance":"recoverable","name":"fixture","content_base64":"eA==","max_service_amount":"0","max_gas_amount":"0"}),
            ManagingMemberWrite,
            "delivery",
        ),
        (
            json!({"type":"delivery","op":"confirm","intent_id":"00000000-0000-4000-8000-000000000001"}),
            ManagingMemberWrite,
            "delivery",
        ),
        (
            json!({"type":"delivery","op":"accept","order_id":"a".repeat(64)}),
            ManagingMemberWrite,
            "delivery",
        ),
        (
            json!({"type":"delivery","op":"reconcile","order_id":"a".repeat(64)}),
            ManagingMemberWrite,
            "delivery",
        ),
        (
            json!({"type":"delivery","op":"confirm_batch","intent_ids":["00000000-0000-4000-8000-000000000001","00000000-0000-4000-8000-000000000002"]}),
            ManagingMemberWrite,
            "delivery",
        ),
        (
            json!({"type":"delivery","op":"accept_batch","order_ids":["a".repeat(64),"b".repeat(64)]}),
            ManagingMemberWrite,
            "delivery",
        ),
        (
            json!({"type":"delivery","op":"flush_budget","budget_id":"a".repeat(64),"batch_id":"b".repeat(64)}),
            ManagingMemberWrite,
            "delivery",
        ),
    ];
    let mut cases = documents
        .into_iter()
        .map(|(document, policy, method)| Ok((serde_json::from_value(document)?, policy, method)))
        .collect::<Result<Vec<_>>>()?;
    cases.push((
        input(
            "policy-input",
            InputTarget::BotMain {
                bot_id: "bot".into(),
            },
        ),
        MemberWrite,
        "message",
    ));
    cases.push((
        Action::ProviderSettings(provider_change()),
        NodeControl,
        "configure_provider",
    ));
    Ok(cases)
}

#[tokio::test]
async fn every_action_stages_original_wire_with_its_explicit_policy() -> Result<()> {
    let setup = Setup::open(true).await?;
    let cases = samples(setup.client.target_node())?;
    assert_eq!(cases.len(), 37);
    for (action, expected, method) in cases {
        assert_eq!(action.policy(), expected, "{method}");
        let staged = setup.client.stage(action).await?;
        assert_eq!(staged.state, OperationState::Prepared);
        let operation = setup.client.stored(move |db| db.get(&staged.id)).await?;
        assert_eq!(policy::stored_policy(&operation)?, expected, "{method}");
        let request: Value = serde_json::from_slice(&operation.bytes)?;
        assert_eq!(request["id"], operation.snapshot.id);
        let actual_method = if request["method"] == "rpc" {
            &request["params"]["method"]
        } else {
            &request["method"]
        };
        assert_eq!(actual_method, method);
    }
    assert!(setup.evidence.lock().unwrap().counts.is_empty());
    assert!(setup.evidence.lock().unwrap().controls.is_empty());
    setup.finish().await
}

fn representative_actions(id: &str) -> [Action; 5] {
    [
        input(
            id,
            InputTarget::BotMain {
                bot_id: "bot".into(),
            },
        ),
        Action::Delivery(DeliveryAction::Configure {
            enabled: true,
            charity_enabled: true,
            paid_enabled: false,
        }),
        Action::TestWebhook,
        Action::LocalAccess { enabled: true },
        Action::StopWork {
            conversation_id: "chat".into(),
            work_id: id.into(),
        },
    ]
}

#[tokio::test]
async fn stage_and_submit_keep_their_distinct_role_and_gate_admission() -> Result<()> {
    let setup = Setup::open(true).await?;
    use ClientAuthority::{LocalSession, Membership, PrivateDataOnly};
    // Columns are member write, delivery, node control, enable local access, stop.
    let rows = [
        (
            Membership { can_manage: true },
            WriteGate::Writable,
            [true, true, true, true, true],
            [true, true, true, true, true],
        ),
        (
            Membership { can_manage: false },
            WriteGate::Writable,
            [true, false, false, false, true],
            [true, false, false, false, true],
        ),
        (
            LocalSession,
            WriteGate::Writable,
            [false, false, false, false, false],
            [false, false, true, false, false],
        ),
        (
            Membership { can_manage: true },
            WriteGate::Unknown,
            [true, true, true, true, true],
            [true, true, true, true, true],
        ),
        (
            Membership { can_manage: false },
            WriteGate::Restricted,
            [true, false, false, false, true],
            [true, false, false, false, true],
        ),
        (
            Membership { can_manage: true },
            WriteGate::Forked,
            [false, false, false, false, true],
            [false, false, true, true, true],
        ),
        (
            Membership { can_manage: false },
            WriteGate::Forked,
            [false, false, false, false, true],
            [false, false, false, false, true],
        ),
        (
            PrivateDataOnly {
                reason: OfflineBlockReason::Forked,
            },
            WriteGate::Forked,
            [false; 5],
            [false; 5],
        ),
        (
            PrivateDataOnly {
                reason: OfflineBlockReason::Revoked,
            },
            WriteGate::Writable,
            [false; 5],
            [false; 5],
        ),
    ];
    for (row, (authority, gate, stage_allowed, submit_allowed)) in rows.into_iter().enumerate() {
        let mut client = setup.client.clone();
        client.authority = authority;
        client.write_gate = Arc::new(AtomicWriteGate::new(gate));
        client.write_generation = Some(client.write_gate.begin_generation(gate)?);
        for (index, action) in representative_actions(&format!("stage-{row}"))
            .into_iter()
            .enumerate()
        {
            let before = client.pending().await?.len();
            let result = client.stage(action).await;
            assert_eq!(
                result.is_ok(),
                stage_allowed[index],
                "stage row {row}, column {index}"
            );
            if stage_allowed[index] {
                assert_eq!(result?.state, OperationState::Prepared);
            } else {
                assert_eq!(client.pending().await?.len(), before);
            }
        }
        for (index, action) in representative_actions(&format!("submit-{row}"))
            .into_iter()
            .enumerate()
        {
            let before = client.pending().await?.len();
            let result = client.submit(action).await;
            assert_eq!(
                result.is_ok(),
                submit_allowed[index],
                "submit row {row}, column {index}"
            );
            if submit_allowed[index] {
                let snapshot = result?;
                if index == 0 && gate != WriteGate::Writable {
                    assert_eq!(snapshot.state, OperationState::Prepared);
                }
            } else {
                assert_eq!(client.pending().await?.len(), before);
            }
        }
    }
    // The synthetic LocalSession models SDK admission; Core authenticates it separately.
    let mut local = setup.client.clone();
    local.authority = LocalSession;
    assert!(
        local
            .stage(Action::LocalAccess { enabled: false })
            .await
            .is_err()
    );
    assert!(
        local
            .submit(Action::LocalAccess { enabled: false })
            .await
            .is_ok()
    );
    drop(local);
    setup.client.revoked.store(true, Ordering::Release);
    for action in representative_actions("revoked") {
        assert!(setup.client.stage(action.clone()).await.is_err());
        assert!(setup.client.submit(action).await.is_err());
    }
    setup.finish().await
}

#[tokio::test]
async fn sending_saved_intents_rechecks_roles_without_changing_original_requests() -> Result<()> {
    let setup = Setup::open(true).await?;
    let client = &setup.client;
    let mut originals = Vec::new();
    for action in representative_actions("saved") {
        let staged = client.stage(action).await?;
        let id = staged.id.clone();
        let bytes = client.stored(move |db| Ok(db.get(&id)?.bytes)).await?;
        originals.push((staged.id, bytes));
    }
    // The managing-member authorization case has its own gate fixture. The
    // member/local cases below share one gate and retain its confirmed fork.
    let mut managing = client.clone();
    managing.write_gate = Arc::new(AtomicWriteGate::new(WriteGate::Writable));
    managing.write_generation = Some(managing.write_gate.begin_generation(WriteGate::Writable)?);
    let mut member = client.clone();
    member.authority = ClientAuthority::Membership { can_manage: false };
    assert_eq!(
        member.send_staged(&originals[0].0).await?.state,
        OperationState::Accepted
    );
    for index in [1, 2, 3] {
        assert!(member.send_staged(&originals[index].0).await.is_err());
    }
    member.write_gate.confirm_fork();
    assert_eq!(
        member.send_staged(&originals[4].0).await?.state,
        OperationState::Accepted
    );
    assert_eq!(
        client.send_staged(&originals[1].0).await?.state,
        OperationState::Prepared
    );

    let mut local = client.clone();
    local.authority = ClientAuthority::LocalSession;
    assert_eq!(
        local.send_staged(&originals[2].0).await?.state,
        OperationState::Accepted
    );
    // Existing send admission delegates enable permission to Core; only submit rejects it here.
    assert_eq!(
        local.send_staged(&originals[3].0).await?.state,
        OperationState::Accepted
    );
    assert!(
        local
            .submit(Action::LocalAccess { enabled: true })
            .await
            .is_err()
    );
    assert_eq!(
        local
            .submit(Action::LocalAccess { enabled: false })
            .await?
            .state,
        OperationState::Accepted
    );

    assert_eq!(
        managing.send_staged(&originals[1].0).await?.state,
        OperationState::Accepted
    );
    for (id, bytes) in originals {
        assert_eq!(
            client.stored(move |db| Ok(db.get(&id)?.bytes)).await?,
            bytes
        );
    }
    {
        let evidence = setup.evidence.lock().unwrap();
        assert_eq!(
            evidence.counts,
            BTreeMap::from([
                ("saved".into(), 1),
                ("delivery".into(), 1),
                ("test_webhook".into(), 1),
                ("local_access".into(), 2)
            ])
        );
        assert_eq!(
            evidence.controls,
            vec![json!({"method":"interrupt_work","conversation_id":"chat","work_id":"saved"})]
        );
    }
    drop(member);
    drop(local);
    drop(managing);
    setup.finish().await
}

#[tokio::test]
async fn malformed_pending_policy_is_private_and_preserved_across_restart() -> Result<()> {
    let setup = Setup::open(true).await?;
    let private = "synthetic-private-policy-value";
    let invalid = [
        json!({"method":private,"params":{}}),
        json!({"method":"rpc","params":{"method":private}}),
        json!({"method":"delivery","params":{"op":private}}),
        json!({"method":"local_access","params":{"enabled":private}}),
        json!({"method":"local_access","params":{}}),
        json!({"method":"configure_node","params":private}),
        json!({"method":"rpc","params":{"method":"interrupt_work","conversation_id":"chat"}}),
        json!({"method":"rpc","params":{"method":"interrupt_work","conversation_id":"chat","work_id":private}}),
        json!({"method":"rpc","params":{"method":"cancel_manager","conversation_id":"chat","trigger":private}}),
    ];
    let mut originals = Vec::new();
    for (index, request) in invalid.into_iter().enumerate() {
        let mut operation = setup.client.prepare(Action::TestWebhook)?;
        operation.lane = format!("malformed-{index}");
        operation.bytes = serde_json::to_vec(&request)?;
        let bytes = operation.bytes.clone();
        let snapshot = setup
            .client
            .stored(move |db| db.prepare(&operation))
            .await?;
        let error = setup.client.send_staged(&snapshot.id).await.unwrap_err();
        assert_eq!(error.to_string(), "invalid stored operation policy");
        assert!(!format!("{error:?}").contains(private));
        assert!(!format!("{error:#}").contains(private));
        assert_eq!(error.chain().count(), 1);
        originals.push((snapshot.id, bytes));
    }
    let mut unknown = setup.client.prepare(Action::TestWebhook)?;
    unknown.lane = "unknown-corrupt-policy".into();
    unknown.bytes = serde_json::to_vec(&json!({"method":private,"params":{}}))?;
    let unknown_bytes = unknown.bytes.clone();
    let unknown_id = setup
        .client
        .stored(move |db| {
            let snapshot = db.prepare(&unknown)?;
            db.begin(&snapshot.id)?;
            Ok(snapshot.id)
        })
        .await?;
    assert_eq!(
        setup.client.send_staged(&unknown_id).await?.state,
        OperationState::Unknown
    );
    assert_eq!(
        setup.client.reconcile(&unknown_id).await?.state,
        OperationState::Unknown
    );

    let Setup {
        dir,
        client,
        node,
        state,
        evidence,
        server,
    } = setup;
    let key = client.key.clone();
    client.close();
    drop(client);
    server.await??;
    let (endpoint, server) = fixture(
        &node.secret_hex(),
        state,
        key.device_id().into(),
        evidence.clone(),
        None,
    )
    .await?;
    let client = Client::open(key, endpoint, dir.join("outbox")).await?;
    for (id, bytes) in originals {
        assert!(client.send_staged(&id).await.is_err());
        let stored = client.stored(move |db| db.get(&id)).await?;
        assert_eq!(stored.snapshot.state, OperationState::Prepared);
        assert_eq!(stored.bytes, bytes);
    }
    assert_eq!(
        client.send_staged(&unknown_id).await?.state,
        OperationState::Unknown
    );
    assert_eq!(
        client.reconcile(&unknown_id).await?.state,
        OperationState::Unknown
    );
    let stored = client.stored(move |db| db.get(&unknown_id)).await?;
    assert_eq!(stored.bytes, unknown_bytes);
    assert!(evidence.lock().unwrap().counts.is_empty());
    assert!(evidence.lock().unwrap().controls.is_empty());
    client.close();
    drop(client);
    server.await??;
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn offline_fork_proof_does_not_inherit_online_member_stop_permission() -> Result<()> {
    for (manage, forked) in [(false, false), (true, false), (false, true), (true, true)] {
        let setup = Setup::open(manage).await?;
        if forked {
            setup.evidence.lock().unwrap().write_state = "forked".into();
            setup.client.read(ReadRequest::WriteState).await?;
            assert_eq!(
                setup
                    .client
                    .submit(Action::StopWork {
                        conversation_id: "chat".into(),
                        work_id: "online-stop".into()
                    })
                    .await?
                    .state,
                OperationState::Accepted
            );
        }
        let Setup {
            dir,
            client,
            evidence,
            server,
            ..
        } = setup;
        let key = client.key.clone();
        let endpoint = client.endpoint.clone();
        client.close();
        drop(client);
        server.await??;
        let client = Client::open_offline(key, endpoint, dir.join("outbox")).await?;
        assert!(!client.is_online());
        for action in [
            input(
                "offline-input",
                InputTarget::BotMain {
                    bot_id: "bot".into(),
                },
            ),
            Action::StopWork {
                conversation_id: "chat".into(),
                work_id: "offline-stop".into(),
            },
        ] {
            let staged = client.stage(action).await;
            assert_eq!(staged.is_ok(), !forked);
            if !forked {
                assert_eq!(
                    client.send_staged(&staged?.id).await?.state,
                    OperationState::Prepared
                );
            }
        }
        assert_eq!(
            client.stage(Action::TestWebhook).await.is_ok(),
            manage && !forked
        );
        assert!(client.submit(Action::TestWebhook).await.is_err());
        if forked {
            assert_eq!(
                client.authority(),
                ClientAuthority::PrivateDataOnly {
                    reason: OfflineBlockReason::Forked
                }
            );
        }
        assert!(evidence.lock().unwrap().counts.is_empty());
        assert_eq!(evidence.lock().unwrap().controls.len(), usize::from(forked));
        drop(client);
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

#[tokio::test]
async fn staging_rechecks_revocation_and_fork_after_waiting_for_storage() -> Result<()> {
    use std::future::Future;
    let setup = Setup::open(true).await?;
    let rows = [
        (
            representative_actions("revoked-while-queued")[0].clone(),
            true,
            WriteGate::Writable,
            false,
        ),
        (
            representative_actions("forked-while-queued")[0].clone(),
            false,
            WriteGate::Forked,
            false,
        ),
        (Action::TestWebhook, false, WriteGate::Forked, true),
    ];
    for (action, revoke, gate, admitted) in rows {
        let mut client = setup.client.clone();
        client.revoked = Arc::new(AtomicBool::new(false));
        client.write_gate = Arc::new(AtomicWriteGate::new(WriteGate::Writable));
        client.write_generation = Some(client.write_gate.begin_generation(WriteGate::Writable)?);
        let outbox = client.outbox.clone();
        let (locked, entered) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let holder = tokio::task::spawn_blocking(move || {
            let _guard = outbox.lock().unwrap();
            locked.send(()).unwrap();
            released.recv().unwrap();
        });
        entered.await?;
        let staged = client.stage(action);
        tokio::pin!(staged);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(staged.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        client.revoked.store(revoke, Ordering::Release);
        if gate == WriteGate::Forked {
            client.write_gate.confirm_fork();
        }
        release.send(())?;
        holder.await?;
        let result = staged.await;
        assert_eq!(result.is_ok(), admitted);
        if admitted {
            assert_eq!(result?.state, OperationState::Prepared);
        }
    }
    assert_eq!(setup.client.pending().await?.len(), 1);
    assert!(setup.evidence.lock().unwrap().counts.is_empty());
    assert!(setup.evidence.lock().unwrap().controls.is_empty());
    setup.finish().await
}
