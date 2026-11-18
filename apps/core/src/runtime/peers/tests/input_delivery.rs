use super::super::test_support::{
    create_input_bot, finish_nodes, first_sync, node, posted, ready_member_rpc, stop_nodes,
};
use super::super::*;
use crate::crypto::LocalIdentity;

#[tokio::test(flavor = "multi_thread")]
async fn missing_dependencies_do_not_block_another_scope_on_the_same_peer() -> Result<()> {
    let (sender, owner) = (LocalIdentity::generate(), LocalIdentity::generate());
    let (sender_id, owner_id) = (sender.device_id()?, owner.device_id()?);
    let state = sender.sign_network_fixture(1, vec![sender_id.clone(), owner_id.clone()])?;
    let network = sender.fixture_network_id()?;
    let (a, dir_a) = node(&sender).await?;
    let (r, dir_r) = node(&owner).await?;
    let result = async {
        for node in [&a, &r] {
            node.install_membership(state.clone()).await?;
        }
        {
            let sender = sender.clone();
            let network = network.clone();
            let owner_id = owner_id.clone();
            a.store
                .call("peers.fixture", move |store| {
                    for scope in ["slow", "active"] {
                        let grant = sender.owner_fixture(&network, scope, &owner_id)?;
                        store.create_conversation(
                            &sender,
                            &network,
                            scope,
                            scope,
                            grant,
                            now_ms(),
                        )?;
                        create_input_bot(
                            store,
                            &sender,
                            &sender.fixture_admin(),
                            &network,
                            scope,
                            now_ms(),
                        )?;
                    }

                    Ok(())
                })
                .await?
        }
        let initial = {
            let arg0 = (network).to_owned();
            let arg1 = (sender_id).to_owned();
            let arg2 = 1;
            let arg3 = 10;
            a.store
                .call("peers.events_from", move |store| {
                    store.events_from(&arg0, &arg1, arg2, arg3)
                })
                .await
        }?;
        {
            let arg0 = (network).to_owned();
            let arg1 = (initial).to_owned();
            r.store
                .call("peers.ingest_events", move |store| {
                    store.ingest_events(&arg0, &arg1)
                })
                .await
        }?;
        {
            let arg0 = (sender).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = posted(&sender, &network, "slow", "slow-input", 1)?;
            a.store
                .call("peers.post_input_fixture", move |store| {
                    store.post_input_fixture(&arg0, &arg1, arg2, now_ms())
                })
                .await
        }?;
        let active = {
            let arg0 = (owner).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = ("active").to_owned();
            let arg3 = posted(&sender, &network, "active", "active-input", 1)?;
            let arg4 = vec![];
            r.store
                .call("peers.append_event", move |store| {
                    store.append_event(&arg0, &arg1, &arg2, arg3, arg4, now_ms())
                })
                .await
        }?;
        {
            let arg0 = (network).to_owned();
            let arg1 = ([active]).to_owned();
            a.store
                .call("peers.ingest_events", move |store| {
                    store.ingest_events(&arg0, &arg1)
                })
                .await
        }?;
        {
            let arg0 = (sender).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = posted(&sender, &network, "active", "active-input", 1)?;
            a.store
                .call("peers.post_input_fixture", move |store| {
                    store.post_input_fixture(&arg0, &arg1, arg2, now_ms())
                })
                .await
        }?;
        assert!(
            {
                let arg0 = ("active").to_owned();
                r.store
                    .call("peers.scheduler_jobs", move |store| {
                        store.scheduler_jobs(&arg0)
                    })
                    .await
            }?
            .is_empty()
        );
        for (node, peer) in [(&a, &owner_id), (&r, &sender_id)] {
            node.sync_peers
                .lock()
                .expect("sync peers")
                .entry(peer.clone())
                .or_default()
                .running = true;
            node.start_network().await?;
        }
        a.net()?.learn(&r.net()?.addr()).await;
        r.net()?.learn(&a.net()?.addr()).await;
        let slow = {
            let arg0 = (network).to_owned();
            let arg1 = ("slow").to_owned();
            let arg2 = ("message:slow-input").to_owned();
            a.store
                .call("peers.peer_delivery_command", move |store| {
                    store.peer_delivery_command(&arg0, &arg1, &arg2)
                })
                .await
        }?;
        let receipt = a
            .peer_request(
                &network,
                &owner_id,
                json!({"type":"deliver","command":slow.envelope}),
            )
            .await?;
        assert!(matches!(
            receipt["admission"]["state"].as_str(),
            Some("saved" | "waiting_dependencies")
        ));
        assert!(
            receipt.get("receipt").is_none(),
            "saved ingress is not owner admission"
        );
        a.deliver_outbox().await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while {
                let arg0 = ("active").to_owned();
                let arg1 = ("message:active-input").to_owned();
                r.store
                    .call("peers.scheduler_job", move |store| {
                        store.scheduler_job(&arg0, &arg1)
                    })
                    .await
            }?
            .is_none()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            anyhow::Ok(())
        })
        .await
        .context(
            "the active scope is admitted while the same peer still lacks the slow source",
        )??;
        assert!(
            {
                let arg0 = (network).to_owned();
                let arg1 = ("slow").to_owned();
                let arg2 = ("slow-input").to_owned();
                r.store
                    .call("peers.message", move |store| {
                        store.message(&arg0, &arg1, &arg2)
                    })
                    .await
            }?
            .is_none()
        );
        assert!(
            {
                let arg0 = ("slow").to_owned();
                r.store
                    .call("peers.scheduler_jobs", move |store| {
                        store.scheduler_jobs(&arg0)
                    })
                    .await
            }?
            .is_empty()
        );
        assert_ne!(
            {
                let arg0 = (network).to_owned();
                let arg1 = ("slow").to_owned();
                let arg2 = ("message:slow-input").to_owned();
                a.store
                    .call("peers.mailbox_for_test", move |store| {
                        store.mailbox_for_test(&arg0, &arg1, &arg2)
                    })
                    .await
            }?
            .context("slow mailbox")?
            .state,
            crate::store::mailbox::MailboxState::Delivered
        );
        let mut changed = slow.envelope.clone();
        changed.revision = Some(2);
        assert!(
            a.peer_request(
                &network,
                &owner_id,
                json!({"type":"deliver","command":changed})
            )
            .await
            .is_err(),
            "the same command cannot change its intent"
        );
        let active = {
            let arg0 = (network).to_owned();
            let arg1 = ("active").to_owned();
            let arg2 = ("message:active-input").to_owned();
            a.store
                .call("peers.peer_delivery_command", move |store| {
                    store.peer_delivery_command(&arg0, &arg1, &arg2)
                })
                .await
        }?;
        let read = a
            .peer_request(
                &network,
                &owner_id,
                json!({"type":"deliver","command":active.envelope}),
            )
            .await?;
        assert_eq!(read["admission"]["state"], "owner_accepted");
        assert_eq!(
            {
                let arg0 = ("active").to_owned();
                r.store
                    .call("peers.scheduler_jobs", move |store| {
                        store.scheduler_jobs(&arg0)
                    })
                    .await
            }?
            .len(),
            1
        );
        anyhow::Ok(())
    }
    .await;
    stop_nodes(vec![(a, dir_a), (r, dir_r)]).await?;
    result
}

#[tokio::test(flavor = "multi_thread")]
async fn held_answers_route_to_the_original_request_node_and_never_become_jobs() -> Result<()> {
    let (sender, request_node) = (LocalIdentity::generate(), LocalIdentity::generate());
    let (sender_id, request_node_id) = (sender.device_id()?, request_node.device_id()?);
    let network = sender.fixture_network_id()?;
    let state = sender.sign_network_fixture(1, vec![sender_id.clone(), request_node_id.clone()])?;
    let (a, dir_a) = node(&sender).await?;
    let (r, dir_r) = node(&request_node).await?;
    let result = async {
        for node in [&a, &r] {
            node.install_membership(state.clone()).await?;
        }
        {
            let sender = sender.clone();
            let network = network.clone();
            let sender_id = sender_id.clone();
            a.store
                .call("peers.fixture", move |store| {
                    let grant = sender.owner_fixture(&network, "c", &sender_id)?;
                    store.create_conversation(
                        &sender,
                        &network,
                        "c",
                        "answer control",
                        grant,
                        now_ms(),
                    )?;

                    Ok(())
                })
                .await?
        }
        let initial = {
            let arg0 = (network).to_owned();
            let arg1 = (sender_id).to_owned();
            let arg2 = 1;
            let arg3 = 10;
            a.store
                .call("peers.events_from", move |store| {
                    store.events_from(&arg0, &arg1, arg2, arg3)
                })
                .await
        }?;
        {
            let arg0 = (network).to_owned();
            let arg1 = (initial).to_owned();
            r.store
                .call("peers.ingest_events", move |store| {
                    store.ingest_events(&arg0, &arg1)
                })
                .await
        }?;
        {
            let arg0 = (request_node).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = ("c").to_owned();
            let arg3 = EventBody::QuestionAsked {
                question_id: "q-held".into(),
                target: None,
                attention: AttentionRequest {
                    kind: AttentionKind::Question,
                    device_id: Some(request_node_id.clone()),
                },
                severity: Severity::Attention,
                text: "Continue the held request?".into(),
                choices: vec![],
                about: Subject {
                    node: request_node_id.clone(),
                    agent: Some(crate::providers::Provider::Codex),
                    session_id: "original-session".into(),
                    cwd: dir_r.display().to_string(),
                    summary: "held request".into(),
                    request_id: Some("original-request".into()),
                },
            };
            r.store
                .call("peers.ask", move |store| {
                    store.ask(&arg0, &arg1, &arg2, arg3, now_ms())
                })
                .await
        }?;
        let question = {
            let arg0 = (network).to_owned();
            let arg1 = (request_node_id).to_owned();
            let arg2 = 1;
            let arg3 = 10;
            r.store
                .call("peers.events_from", move |store| {
                    store.events_from(&arg0, &arg1, arg2, arg3)
                })
                .await
        }?;
        {
            let arg0 = (network).to_owned();
            let arg1 = (question).to_owned();
            a.store
                .call("peers.ingest_events", move |store| {
                    store.ingest_events(&arg0, &arg1)
                })
                .await
        }?;
        let answer = sender.fixture_admin().attest(
            &network,
            "c",
            EventBody::MessagePosted {
                message_id: "a-q-held".into(),
                role: Role::User,
                text: "continue".into(),
                attachments: vec![],
                report_results: Some(true),
                in_reply_to: None,
                status: None,
                failure: None,
                answers: Some("q-held".into()),
                attestation: None,
            },
        )?;
        {
            let arg0 = (sender).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = ("q-held").to_owned();
            let arg3 = ("continue").to_owned();
            let arg4 = answer.attestation().cloned();
            a.store
                .call("peers.accept_answer", move |store| {
                    store.accept_answer(&arg0, &arg1, &arg2, &arg3, arg4, now_ms())
                })
                .await
        }?;
        assert!(
            {
                let arg0 = ("c").to_owned();
                a.store
                    .call("peers.scheduler_jobs", move |store| {
                        store.scheduler_jobs(&arg0)
                    })
                    .await
            }?
            .is_empty(),
            "the Bot owner does not schedule an Answer for a held request elsewhere"
        );
        let answer_events = {
            let arg0 = (network).to_owned();
            let arg1 = (sender_id).to_owned();
            let arg2 = 1;
            let arg3 = 20;
            a.store
                .call("peers.events_from", move |store| {
                    store.events_from(&arg0, &arg1, arg2, arg3)
                })
                .await
        }?;
        {
            let arg0 = (network).to_owned();
            let arg1 = (answer_events).to_owned();
            r.store
                .call("peers.ingest_events", move |store| {
                    store.ingest_events(&arg0, &arg1)
                })
                .await
        }?;
        assert!(
            {
                let arg0 = ("c").to_owned();
                r.store
                    .call("peers.scheduler_jobs", move |store| {
                        store.scheduler_jobs(&arg0)
                    })
                    .await
            }?
            .is_empty()
        );
        assert!(
            {
                let arg0 = (request_node_id).to_owned();
                let arg1 = (network).to_owned();
                let arg2 = ("c").to_owned();
                let arg3 = ("message:a-q-held").to_owned();
                r.store
                    .call("peers.admit_delivery", move |store| {
                        store.admit_delivery(&arg0, &arg1, &arg2, &arg3)
                    })
                    .await
            }?
            .is_none(),
            "direct delivery waits for the live control receipt instead of creating a scheduler job"
        );
        let command = {
            let arg0 = (network).to_owned();
            let arg1 = ("c").to_owned();
            let arg2 = ("message:a-q-held").to_owned();
            a.store
                .call("peers.peer_delivery_command", move |store| {
                    store.peer_delivery_command(&arg0, &arg1, &arg2)
                })
                .await
        }?;
        assert_eq!(command.target_node, request_node_id);
        assert!(command.envelope.target.starts_with("held_request:"));
        assert_eq!(command.envelope.owner_epoch, 0);
        assert_eq!(command.envelope.causal_refs.len(), 2);
        let expired = r
            .accept_held_answer_delivery(&network, "c", "a-q-held")
            .await
            .expect_err("an expired request is a definite refusal");
        assert!(expired.is::<AdmissionRejected>());
        let saved = r
            .store
            .call("peers.save_peer_command", {
                let network = network.to_owned();
                let sender_id = sender_id.to_owned();
                let command = command.to_owned();
                move |store| {
                    store.save_peer_command(&network, &sender_id, &command.envelope, now_ms())
                }
            })
            .await?;
        r.store
            .call("peers.execute_batch", move |store| {
                Ok(store.connection.execute_batch("PRAGMA query_only=ON")?)
            })
            .await?;
        let unwritten = r.process_peer_command(&network, &saved).await;
        r.store
            .call("peers.execute_batch", move |store| {
                Ok(store.connection.execute_batch("PRAGMA query_only=OFF")?)
            })
            .await?;
        assert!(
            unwritten.unwrap_err().is::<rusqlite::Error>(),
            "a failed refusal receipt write stays retryable"
        );
        let pending = r
            .store
            .call("peers.pending_peer_commands", {
                let network = network.to_owned();
                move |store| store.pending_peer_commands(&network)
            })
            .await?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].state, PeerAdmissionState::Saved);
        assert!(
            r.store
                .call("peers.scheduler_jobs", move |store| store
                    .scheduler_jobs("c"))
                .await?
                .is_empty()
        );
        assert!(
            r.store
                .call("peers.held_answer_receipt", {
                    let network = network.to_owned();
                    move |store| store.held_answer_receipt(&network, "c", "a-q-held")
                })
                .await?
                .is_none()
        );
        for node in [&a, &r] {
            node.start_network().await?;
        }
        a.net()?.learn(&r.net()?.addr()).await;
        r.net()?.learn(&a.net()?.addr()).await;
        ready_member_rpc(&a, &network, &request_node_id).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let answer = a
                    .peer_request(
                        &network,
                        &request_node_id,
                        json!({"type":"deliver","command":command.envelope}),
                    )
                    .await?;
                assert_ne!(
                    answer["admission"]["state"], "owner_accepted",
                    "a missing live transport is never accepted as an Answer job"
                );
                if answer["admission"]["state"] == "rejected" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            anyhow::Ok(())
        })
        .await
        .context("an answer for a request without a live transport fails explicitly")??;
        let rejected = r
            .store
            .call("peers.save_peer_command", {
                let network = network.to_owned();
                let sender_id = sender_id.to_owned();
                let command = command.to_owned();
                move |store| {
                    store.save_peer_command(&network, &sender_id, &command.envelope, now_ms())
                }
            })
            .await?;
        assert_eq!(rejected.state, PeerAdmissionState::Rejected);
        assert!(
            r.store
                .call("peers.pending_peer_commands", {
                    let network = network.to_owned();
                    move |store| store.pending_peer_commands(&network)
                })
                .await?
                .is_empty()
        );
        assert!(
            r.store
                .call("peers.scheduler_jobs", move |store| store
                    .scheduler_jobs("c"))
                .await?
                .is_empty()
        );
        assert!(
            r.store
                .call("peers.held_answer_receipt", {
                    let network = network.to_owned();
                    move |store| store.held_answer_receipt(&network, "c", "a-q-held")
                })
                .await?
                .is_none()
        );
        anyhow::Ok(())
    }
    .await;
    finish_nodes(result, vec![(a, dir_a), (r, dir_r)]).await
}

/// A member Node that is off costs only its own messages a wait: the others' are handed over
/// in the same round.
#[tokio::test(flavor = "multi_thread")]
async fn an_offline_owner_does_not_hold_up_delivery_to_online_ones() -> Result<()> {
    let [ia, ib, ix] = [
        LocalIdentity::generate(),
        LocalIdentity::generate(),
        LocalIdentity::generate(),
    ];
    let device = |id: &LocalIdentity| id.device_id().unwrap();
    let state = ia.sign_network_fixture(1, vec![device(&ia), device(&ib), device(&ix)])?;
    let network = ia.fixture_network_id()?;
    let (a, dir_a) = node(&ia).await?;
    let (b, dir_b) = node(&ib).await?;
    let (x, dir_x) = node(&ix).await?;
    let result = async {
        for node in [&a, &b, &x] {
            node.install_membership(state.clone()).await?;
            node.start_network().await?;
        }
        a.net()?.learn(&b.net()?.addr()).await;
        a.net()?.learn(&x.net()?.addr()).await;
        b.net()?.learn(&a.net()?.addr()).await;
        x.stop_network().await;
        {
            let ia = ia.clone();
            let ix = ix.clone();
            let ib = ib.clone();
            let network = network.clone();
            a.store
                .call("peers.fixture", move |store| {
                    for (scope, owner) in [("a1", &ix), ("a2", &ix), ("c", &ib)] {
                        let owner = ia.owner_fixture(&network, scope, &device(owner))?;
                        store.create_conversation(&ia, &network, scope, scope, owner, now_ms())?;
                        create_input_bot(
                            store,
                            &ia,
                            &ia.fixture_admin(),
                            &network,
                            scope,
                            now_ms(),
                        )?;
                    }

                    Ok(())
                })
                .await?
        }
        first_sync(&b, &device(&ia)).await?;
        {
            let ia = ia.clone();
            let network = network.clone();
            a.store
                .call("peers.fixture", move |store| {
                    for (scope, id) in [("a1", "x1"), ("a2", "x2"), ("c", "b1")] {
                        store.post_input_fixture(
                            &ia,
                            &network,
                            posted(&ia, &network, scope, id, 1)?,
                            now_ms(),
                        )?;
                    }
                    assert_eq!(store.scheduler_outbox()?.len(), 3);

                    Ok(())
                })
                .await?
        }
        a.outbox_wakeup.notify_one();
        let trigger = crate::scheduling::message_trigger("b1");
        tokio::time::timeout(Duration::from_secs(8), async {
            // B stores the job before A reads its receipt and drops the entry.
            while {
                let arg0 = ("c").to_owned();
                let arg1 = (trigger).to_owned();
                b.store
                    .call("peers.scheduler_job", move |store| {
                        store.scheduler_job(&arg0, &arg1)
                    })
                    .await
            }?
            .is_none()
                || a.store
                    .call("peers.scheduler_outbox", move |store| {
                        store.scheduler_outbox()
                    })
                    .await?
                    .len()
                    != 2
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            anyhow::Ok(())
        })
        .await
        .context("the online owner gets its message while the offline one is still dialed")??;
        anyhow::Ok(())
    }
    .await;
    stop_nodes(vec![(a, dir_a), (b, dir_b), (x, dir_x)]).await?;
    result
}
