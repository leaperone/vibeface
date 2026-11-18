use super::super::test_support::{
    create_input_bot, finish_nodes, node, posted, ready_member_rpc, stop_nodes,
};
use super::super::*;
use crate::config;
use crate::crypto::LocalIdentity;
use crate::runtime::test_support::manager_settings;

#[tokio::test]
async fn an_online_target_without_the_original_work_journal_allows_one_signed_settlement()
-> Result<()> {
    use crate::model::{ResolvedModel, WorkKind, WorkOutcome};
    use crate::work::{Signal, WorkSpec};

    let (entry, target) = (LocalIdentity::generate(), LocalIdentity::generate());
    let (entry_id, target_id) = (entry.device_id()?, target.device_id()?);
    let network = entry.fixture_network_id()?;
    let state = entry.sign_network_fixture(1, vec![entry_id.clone(), target_id.clone()])?;
    let (a, dir_a) = node(&entry).await?;
    let (b, dir_b) = node(&target).await?;
    let result = async {
        for node in [&a, &b] {
            node.install_membership(state.clone()).await?;
            node.start_network().await?;
        }
        a.net()?.learn(&b.net()?.addr()).await;
        b.net()?.learn(&a.net()?.addr()).await;
        {
            let entry = entry.clone();
            let network = network.clone();
            let entry_id = entry_id.clone();
            a.store.call("peers.fixture", move |store| {
            store.create_conversation(&entry, &network, "c", "Settlement",
                entry.owner_fixture(&network, "c", &entry_id)?, now_ms())?;
            create_input_bot(store, &entry, &entry.fixture_admin(), &network, "c", now_ms())?;
            store.append_event(&entry, &network, "c", posted(&entry, &network, "c", "m1", 1)?, vec![], now_ms())?;

                Ok(())
            }).await?
        }
        b.sync_peer(&network, &entry_id).await?;
        let spec = WorkSpec {
            work_id: WorkSpec::id("c", "message:m1", "original"),
            conversation: "c".into(), trigger: "message:m1".into(),
            target: target_id.clone(), dispatcher: entry_id.clone(), call_ref: "original".into(),
            kind: WorkKind::Agent(Provider::Pi), model: None,
            resolved: ResolvedModel { provider: Provider::Pi, model: None },
            cwd: dir_b.to_string_lossy().into_owned(), title: "original".into(), prompt: "fixture only".into(),
            continues: None, parent: None, thread: None,
        };
        {
            let arg0 = (target).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = spec.clone();
            b.store.call("peers.accept_work_fixture", move |store| store.accept_work_fixture(&arg0, &arg1, arg2, now_ms())).await
        }?;
        a.sync_peer(&network, &target_id).await?;
        ready_member_rpc(&a, &network, &target_id).await?;
        let access = crate::runtime::Access::User { network_id: network.clone(), scope: crate::runtime::UserScope::Conversation("c".into()) };
        let choice = |work_id: &str| -> Result<Value> {
            let signed = entry.fixture_admin().attest(&network, "c", EventBody::WorkFinished {
                work_id: work_id.into(), outcome: WorkOutcome::Completed, result_text: "checked".into(),
                failure_reason: None, attestation: None, failure: None,
            })?;
            Ok(json!({"conversation_id":"c","check_id":format!("work:{work_id}"),
                "outcome":"completed","note":"checked","attestation":signed.attestation()}))
        };
        let answer = choice(&spec.work_id)?;
        assert!(a.resolve_check(&access, &answer).await.is_err(), "the original journal can still report");
        let evidence = || json!({"type":"work_evidence","conversation_id":"c","work_id":spec.work_id});
        assert_eq!(a.peer_request(&network, &target_id, evidence()).await?["vouches"], true);
        assert!(a.peer_request(&network, &target_id, json!({"type":"work_evidence",
            "conversation_id":"other","work_id":spec.work_id})).await.is_err());
        {
            let spec = spec.clone();
            let target_id = target_id.clone();
            b.store.call("peers.fixture", move |store| {
            assert!(store.work_record_in_scope("another-network", "c", &spec.work_id, &target_id).is_err());
            store.connection.execute("DELETE FROM work_runs WHERE work_id=?", [&spec.work_id])?;

                Ok(())
            }).await?
        }
        assert!(a.peer_request(&network, &target_id, json!({"type":"capabilities"})).await?.is_object(),
            "the target is still reachable");
        assert_eq!(a.peer_request(&network, &target_id, evidence()).await?["vouches"], false);
        assert_eq!(a.resolve_check(&access, &answer).await?["resolved"], true);
        assert!(a.resolve_check(&access, &answer).await.is_err(), "the signed result is admitted once");
        b.sync_peer(&network, &entry_id).await?;
        b.sync_peer(&network, &entry_id).await?;
        assert!({
            let arg0 = (spec.work_id).to_owned();
            b.store.call("peers.work_record", move |store| store.work_record(&arg0)).await
        }?.is_none(), "sync does not rebuild an execution");
        assert!({
            let arg0 = (target).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = spec.clone();
            b.store.call("peers.accept_work_fixture", move |store| store.accept_work_fixture(&arg0, &arg1, arg2, now_ms())).await
        }.is_err(),
            "missing admission evidence cannot start the old dispatch again");
        let finished = {
            let arg0 = (network).to_owned();
            let arg1 = (entry_id).to_owned();
            let arg2 = 1;
            let arg3 = 100;
            a.store.call("peers.events_from", move |store| store.events_from(&arg0, &arg1, arg2, arg3)).await
        }?.into_iter()
            .filter_map(|event| serde_json::from_str::<crate::model::Event>(&event.canonical_body).ok())
            .filter(|event| matches!(&event.body, EventBody::WorkFinished { work_id, outcome: WorkOutcome::Completed, .. } if work_id == &spec.work_id)).count();
        assert_eq!(finished, 1);

        let mut unknown = spec.clone();
        unknown.call_ref = "unknown".into();
        unknown.work_id = WorkSpec::id("c", "message:m1", &unknown.call_ref);
        {
            let arg0 = (target).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = unknown.clone();
            b.store.call("peers.accept_work_fixture", move |store| store.accept_work_fixture(&arg0, &arg1, arg2, now_ms())).await
        }?;
        {
            let arg0 = (target).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = (unknown.work_id).to_owned();
            let arg3 = Signal::SubmissionUnverified("fixture cannot prove its outcome".into());
            b.store.call("peers.apply_work_signal", move |store| store.apply_work_signal(&arg0, &arg1, &arg2, arg3, now_ms())).await
        }?;
        a.sync_peer(&network, &target_id).await?;
        assert_eq!({
            let arg0 = (network).to_owned();
            let arg1 = (unknown.work_id).to_owned();
            a.store.call("peers.work", move |store| store.work(&arg0, &arg1)).await
        }?.context("unknown work")?.outcome,
            Some(WorkOutcome::Unverified));
        assert_eq!(a.resolve_check(&access, &choice(&unknown.work_id)?).await?["resolved"], true);
        for node in [&a, &b] {
            assert!(node.agent_tokens.lock().await.is_empty());
            assert!(node.manager_controls.lock().await.is_empty());
            node.store.call("peers.fixture", move |store| {
            let native: u64 = store.connection.query_row(
                "SELECT (SELECT count(*) FROM manager_turns)+(SELECT count(*) FROM manager_processes)",
                [], |row| row.get(0))?;
            assert_eq!(native, 0, "settlement never starts or sends a native turn");

                Ok(())
            }).await?
        }
        Ok(())
    }.await;
    finish_nodes(result, vec![(a, dir_a), (b, dir_b)]).await
}

#[tokio::test(flavor = "current_thread")]
async fn peer_delivery_locks_serialize_same_target_and_allow_other_scopes() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};

    let locks = Arc::new(PeerDeliveryLocks::default());
    let first_active = Arc::new(AtomicBool::new(false));
    let same_peer_entered = Arc::new(AtomicBool::new(false));
    let other_peer_parallel = Arc::new(AtomicBool::new(false));
    let (first_ready_tx, first_ready_rx) = tokio::sync::oneshot::channel();
    let (release_first_tx, release_first_rx) = tokio::sync::oneshot::channel();

    let first = tokio::spawn({
        let locks = locks.clone();
        let first_active = first_active.clone();
        async move {
            let _guard = locks.lock("peer-a:scope-a").await;
            first_active.store(true, Ordering::SeqCst);
            first_ready_tx.send(()).expect("first waiter");
            release_first_rx.await.expect("first release");
            first_active.store(false, Ordering::SeqCst);
        }
    });
    first_ready_rx.await?;

    let same_peer = tokio::spawn({
        let locks = locks.clone();
        let first_active = first_active.clone();
        let same_peer_entered = same_peer_entered.clone();
        async move {
            let _guard = locks.lock("peer-a:scope-a").await;
            assert!(!first_active.load(Ordering::SeqCst));
            same_peer_entered.store(true, Ordering::SeqCst);
        }
    });
    let other_peer = tokio::spawn({
        let locks = locks.clone();
        let first_active = first_active.clone();
        let other_peer_parallel = other_peer_parallel.clone();
        async move {
            let _guard = locks.lock("peer-a:scope-b").await;
            other_peer_parallel.store(first_active.load(Ordering::SeqCst), Ordering::SeqCst);
        }
    });

    other_peer.await?;
    assert!(other_peer_parallel.load(Ordering::SeqCst));
    assert!(!same_peer_entered.load(Ordering::SeqCst));

    release_first_tx.send(()).expect("release first peer");
    first.await?;
    same_peer.await?;
    assert!(same_peer_entered.load(Ordering::SeqCst));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn synced_assignments_run_only_after_explicit_delivery_and_are_admitted_once() -> Result<()> {
    use crate::workspace::{WORKSPACE_SCOPE, WorkspaceCommand};
    fn apply(
        sender: &LocalIdentity,
        store: &mut Store,
        network: &str,
        command_id: &str,
        command: WorkspaceCommand,
    ) -> Result<()> {
        let signed = sender.fixture_admin().attest(
            network,
            WORKSPACE_SCOPE,
            EventBody::WorkspaceCommanded {
                command_id: command_id.into(),
                command: command.clone(),
                acting_assignment: None,
                attestation: None,
                cause: None,
            },
        )?;
        store.apply_workspace_command(
            sender,
            network,
            command_id,
            command,
            signed.attestation().cloned(),
            None,
            None,
            now_ms(),
        )?;
        Ok(())
    }
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
                    let grant = sender.owner_fixture(&network, "c", &owner_id)?;
                    store.create_conversation(
                        &sender,
                        &network,
                        "c",
                        "assigned Bot",
                        grant,
                        now_ms(),
                    )?;
                    let bot = sender.fixture_admin().attest(
                        &network,
                        "c",
                        EventBody::BotCreated {
                            bot_id: "bot".into(),
                            name: "assigned Bot".into(),
                            instructions: "run the assigned task".into(),
                            project: None,
                            attestation: None,
                            cause: None,
                        },
                    )?;
                    store.append_event(&sender, &network, "c", bot, vec![], now_ms())?;
                    let epoch = store
                        .conversation(&network, "c")?
                        .context("Bot conversation")?
                        .owner_epoch;
                    for (command_id, command) in [
                        (
                            "create-task",
                            WorkspaceCommand::CreateTask {
                                task_id: "task".into(),
                                channel_id: None,
                                parent_task_id: None,
                                owner_bot: "bot".into(),
                                title: "assigned task".into(),
                                request: "perform the assigned task".into(),
                                expected_result: "verified result".into(),
                                source_events: vec![],
                                dependencies: vec![],
                                reply_only: false,
                            },
                        ),
                        (
                            "assign-task",
                            WorkspaceCommand::AssignTask {
                                task_id: "task".into(),
                                expected_revision: 1,
                                assignment_id: "assignment".into(),
                                bot_id: "bot".into(),
                                execution_node: owner_id.clone(),
                                bot_owner_epoch: epoch,
                                channel_owner_epoch: None,
                                work_id: None,
                                resource_roots: vec![],
                                automatic: false,
                            },
                        ),
                    ] {
                        apply(&sender, store, &network, command_id, command)?;
                    }
                    assert!(
                        store
                            .scheduler_outbox()?
                            .iter()
                            .any(|entry| entry.trigger == "assignment:assignment")
                    );

                    Ok(())
                })
                .await?
        }
        let history = {
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
            let arg1 = (history).to_owned();
            r.store
                .call("peers.ingest_events", move |store| {
                    store.ingest_events(&arg0, &arg1)
                })
                .await
        }?;
        assert!(
            {
                let arg0 = (network).to_owned();
                let arg1 = ("assignment").to_owned();
                r.store
                    .call("peers.workspace_assignment", move |store| {
                        store.workspace_assignment(&arg0, &arg1)
                    })
                    .await
            }?
            .is_some()
        );
        assert!(
            {
                let arg0 = ("c").to_owned();
                r.store
                    .call("peers.scheduler_jobs", move |store| {
                        store.scheduler_jobs(&arg0)
                    })
                    .await
            }?
            .is_empty(),
            "sync stores assignment intent without scheduling it"
        );
        assert!(
            r.store
                .call("peers.scheduler_outbox", move |store| store
                    .scheduler_outbox())
                .await?
                .is_empty(),
            "sync never creates a new delivery"
        );
        let command = {
            let arg0 = (network).to_owned();
            let arg1 = ("c").to_owned();
            let arg2 = ("assignment:assignment").to_owned();
            a.store
                .call("peers.peer_delivery_command", move |store| {
                    store.peer_delivery_command(&arg0, &arg1, &arg2)
                })
                .await
        }?;
        assert_eq!(command.target_node, owner_id);
        assert_eq!(command.envelope.target, "workspace_task:task");
        assert_eq!(command.envelope.revision, Some(1));
        assert!(!command.envelope.causal_refs.is_empty());
        {
            let network = network.clone();
            let sender_id = sender_id.clone();
            let command = command.clone();
            let owner_id = owner_id.clone();
            r.store
                .call("peers.fixture", move |store| {
                    store.save_peer_command(&network, &sender_id, &command.envelope, now_ms())?;
                    let receipt = store
                        .admit_delivery(&owner_id, &network, "c", "assignment:assignment")?
                        .context("owner persisted admission")?;
                    assert_eq!(receipt.trigger, "assignment:assignment");
                    assert_eq!(
                        store.pending_peer_commands(&network)?[0].state,
                        PeerAdmissionState::Saved
                    );

                    Ok(())
                })
                .await?
        }
        {
            let arg0 = (owner).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = ("accepted-for-work").to_owned();
            let arg3 = WorkspaceCommand::AssignmentProgress {
                execution: None,
                assignment_id: "assignment".into(),
                revision: 1,
                state: crate::workspace::AssignmentState::Accepted,
                input_event: None,
                evidence_event: None,
                result: None,
            };
            let arg4 = None;
            let arg5 = None;
            let arg6 = None;
            r.store
                .call("peers.apply_workspace_command", move |store| {
                    store.apply_workspace_command(
                        &arg0,
                        &arg1,
                        &arg2,
                        arg3,
                        arg4,
                        arg5,
                        arg6,
                        now_ms(),
                    )
                })
                .await
        }?;
        let execution = {
            let arg0 = (network).to_owned();
            let arg1 = (owner_id).to_owned();
            let arg2 = 1;
            let arg3 = 20;
            r.store
                .call("peers.events_from", move |store| {
                    store.events_from(&arg0, &arg1, arg2, arg3)
                })
                .await
        }?;
        {
            let arg0 = (network).to_owned();
            let arg1 = (execution).to_owned();
            a.store
                .call("peers.ingest_events", move |store| {
                    store.ingest_events(&arg0, &arg1)
                })
                .await
        }?;
        a.settings.lock().unwrap().roots = vec![dir_a.clone()];
        a.settings.lock().unwrap().manager = Some(manager_settings(
            crate::providers::Provider::Claude,
            dir_a.clone(),
            "test-model".into(),
        ));
        let work = crate::work::WorkRequest {
            work_id: crate::work::WorkSpec::id("c", "assignment:assignment", "assigned-work"),
            conversation: "c".into(),
            trigger: "assignment:assignment".into(),
            target: sender_id.clone(),
            dispatcher: owner_id.clone(),
            call_ref: "assigned-work".into(),
            kind: crate::work::RequestedWorkKind::Task,
            model: None,
            cwd: dir_a.display().to_string(),
            title: "assigned work".into(),
            prompt: "perform assigned file work".into(),
            continues: None,
            parent: None,
            thread: None,
        };
        let work_command = {
            let arg0 = (network).to_owned();
            let arg1 = (work).to_owned();
            r.store
                .call("peers.peer_dispatch_command", move |store| {
                    store.peer_dispatch_command(&arg0, &arg1)
                })
                .await
        }?
        .envelope;
        assert_eq!(work_command.revision, Some(1));
        assert!(
            work_command
                .causal_refs
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        {
            let arg0 = (network).to_owned();
            let arg1 = (owner_id).to_owned();
            let arg2 = (work_command).to_owned();
            a.store
                .call("peers.save_peer_command", move |store| {
                    store.save_peer_command(&arg0, &arg1, &arg2, now_ms())
                })
                .await
        }?;
        assert!(
            a.accept_dispatch(&network, work.clone(), &Value::Null)
                .await?
                .created,
            "a legitimate assignment can dispatch actual Work on another Node"
        );
        assert!(
            !a.accept_dispatch(&network, work.clone(), &Value::Null)
                .await?
                .created
        );
        let mut outside = work.clone();
        outside.call_ref = "outside-root".into();
        outside.work_id =
            crate::work::WorkSpec::id("c", "assignment:assignment", &outside.call_ref);
        outside.cwd = dir_r.display().to_string();
        assert!(
            a.accept_dispatch(&network, outside, &Value::Null)
                .await
                .err()
                .is_some_and(|error| error.to_string().contains("outside configured roots")),
            "assignment authority does not bypass target roots"
        );
        {
            let sender = sender.clone();
            let network = network.clone();
            let owner_id = owner_id.clone();
            let command = command.clone();
            a.store
                .call("peers.fixture", move |store| {
                    apply(
                        &sender,
                        store,
                        &network,
                        "revise-after-admission",
                        WorkspaceCommand::ReviseTask {
                            task_id: "task".into(),
                            expected_revision: 1,
                            request: "new revision after original admission".into(),
                            expected_result: "new verified result".into(),
                            dependencies: vec![],
                        },
                    )?;
                    let changed_owner = sender.fixture_admin().attest(
                        &network,
                        "c",
                        EventBody::ConversationOwner {
                            scheduler_node: owner_id.clone(),
                            epoch: command.envelope.owner_epoch + 1,
                            attestation: None,
                            cause: None,
                        },
                    )?;
                    store.append_event(&sender, &network, "c", changed_owner, vec![], now_ms())?;

                    Ok(())
                })
                .await?
        }
        let changed = {
            let arg0 = (network).to_owned();
            let arg1 = (sender_id).to_owned();
            let arg2 = 1;
            let arg3 = 30;
            a.store
                .call("peers.events_from", move |store| {
                    store.events_from(&arg0, &arg1, arg2, arg3)
                })
                .await
        }?;
        {
            let arg0 = (network).to_owned();
            let arg1 = (changed).to_owned();
            r.store
                .call("peers.ingest_events", move |store| {
                    store.ingest_events(&arg0, &arg1)
                })
                .await
        }?;
        {
            let network = network.clone();
            let owner_id = owner_id.clone();
            r.store
                .call("peers.fixture", move |store| {
                    assert_eq!(
                        store
                            .workspace_task(&network, "task")?
                            .context("revised task")?
                            .revision,
                        2
                    );
                    assert!(
                        store
                            .admit_delivery(&owner_id, &network, "c", "assignment:assignment")
                            .is_err(),
                        "a fresh stale assignment must fail its current fences"
                    );

                    Ok(())
                })
                .await?
        }
        assert!(
            {
                let arg0 = (network).to_owned();
                let arg1 = (sender_id).to_owned();
                let arg2 = (work_command).to_owned();
                a.store
                    .call("peers.peer_owner_proof", move |store| {
                        store.peer_owner_proof(&arg0, &arg1, &arg2)
                    })
                    .await
            }?
            .is_some(),
            "a later revision does not reinterpret the original admitted Work source frame"
        );
        assert_eq!(
            a.store
                .call("peers.work_records", move |store| store.work_records())
                .await?
                .len(),
            1
        );
        let reloaded = Store::open_readonly(dir_r.join("node.sqlite3"))?;
        assert_eq!(
            reloaded.pending_peer_commands(&network)?[0].state,
            PeerAdmissionState::Saved,
            "the owner job survived while its peer receipt was still unwritten"
        );
        assert_eq!(reloaded.scheduler_jobs("c")?.len(), 1);
        drop(reloaded);
        for node in [&a, &r] {
            node.start_network().await?;
        }
        a.net()?.learn(&r.net()?.addr()).await;
        r.net()?.learn(&a.net()?.addr()).await;
        let saved = a
            .peer_request(
                &network,
                &owner_id,
                json!({"type":"deliver","command":command.envelope}),
            )
            .await?;
        assert!(matches!(
            saved["admission"]["state"].as_str(),
            Some("saved" | "owner_accepted")
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let read = a
                    .peer_request(
                        &network,
                        &owner_id,
                        json!({"type":"deliver","command":command.envelope}),
                    )
                    .await?;
                if read["admission"]["state"] == "owner_accepted" {
                    assert_eq!(read["receipt"]["trigger"], "assignment:assignment");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            anyhow::Ok(())
        })
        .await
        .context("explicit delivery admits the assignment")??;
        {
            let arg0 = (network).to_owned();
            let arg1 = (changed).to_owned();
            r.store
                .call("peers.ingest_events", move |store| {
                    store.ingest_events(&arg0, &arg1)
                })
                .await
        }?;
        let jobs = {
            let arg0 = ("c").to_owned();
            r.store
                .call("peers.scheduler_jobs", move |store| {
                    store.scheduler_jobs(&arg0)
                })
                .await
        }?;
        assert_eq!(
            jobs.len(),
            1,
            "repeated delivery and historical sync do not duplicate execution"
        );
        assert_eq!(jobs[0].kind, crate::scheduling::TriggerKind::WorkspaceTask);
        assert_eq!(
            jobs[0].scope,
            crate::scheduling::SessionScope::WorkspaceTask {
                task_id: "task".into()
            }
        );
        assert!(jobs[0].runtime.is_none());
        let mut changed_intent = command.envelope.clone();
        changed_intent.revision = Some(2);
        assert!(
            a.peer_request(
                &network,
                &owner_id,
                json!({"type":"deliver","command":changed_intent})
            )
            .await
            .is_err(),
            "recovery cannot reinterpret the original command as the newer revision"
        );
        anyhow::Ok(())
    }
    .await;
    stop_nodes(vec![(a, dir_a), (r, dir_r)]).await?;
    result
}

#[tokio::test]
async fn a_saved_cancel_prevents_a_delayed_dispatch_and_survives_reload() -> Result<()> {
    use crate::work::{RequestedWorkKind, WorkRequest, WorkSpec};
    let (sender, target) = (LocalIdentity::generate(), LocalIdentity::generate());
    let (sender_id, target_id) = (sender.device_id()?, target.device_id()?);
    let state = sender.sign_network_fixture(1, vec![sender_id.clone(), target_id.clone()])?;
    let network = sender.fixture_network_id()?;
    let (node, dir) = node(&target).await?;
    let result = async {
        node.install_membership(state).await?;
        node.settings.lock().unwrap().roots = vec![dir.clone()];
        node.settings.lock().unwrap().manager = Some(manager_settings(
            crate::providers::Provider::Claude,
            dir.clone(),
            "test-model".into(),
        ));
        {
            let sender = sender.clone();
            let network = network.clone();
            let sender_id = sender_id.clone();
            node.store
                .call("peers.fixture", move |store| {
                    let grant = sender.owner_fixture(&network, "c", &sender_id)?;
                    store.create_conversation(&sender, &network, "c", "Task", grant, now_ms())?;
                    create_input_bot(
                        store,
                        &sender,
                        &sender.fixture_admin(),
                        &network,
                        "c",
                        now_ms(),
                    )?;
                    store.append_event(
                        &sender,
                        &network,
                        "c",
                        posted(&sender, &network, "c", "m1", 1)?,
                        vec![],
                        now_ms(),
                    )?;

                    Ok(())
                })
                .await?
        }
        let spec = WorkRequest {
            work_id: WorkSpec::id("c", "message:m1", "delayed"),
            conversation: "c".into(),
            trigger: "message:m1".into(),
            target: target_id,
            dispatcher: sender_id.clone(),
            call_ref: "delayed".into(),
            kind: RequestedWorkKind::Task,
            model: None,
            cwd: dir.display().to_string(),
            title: "delayed".into(),
            prompt: "do not run".into(),
            continues: None,
            parent: None,
            thread: None,
        };
        let dispatch = {
            let arg0 = (network).to_owned();
            let arg1 = (spec).to_owned();
            node.store
                .call("peers.peer_dispatch_command", move |store| {
                    store.peer_dispatch_command(&arg0, &arg1)
                })
                .await
        }?
        .envelope;
        let stop_payload = PeerCommand::Stop {
            conversation_id: "c".into(),
            work_id: spec.work_id.clone(),
        };
        let stop = PeerEnvelope {
            command_id: stop_payload.id(&network),
            payload: stop_payload,
            ..dispatch.clone()
        };
        let guard = node.work_input_lock(&spec.work_id).await.read_owned().await;
        let saved = tokio::time::timeout(
            Duration::from_secs(1),
            node.accept_peer_envelope(&network, &sender_id, stop.clone()),
        )
        .await
        .context("saving a cancel does not wait for an in-flight Provider read guard")??;
        assert_eq!(saved["admission"]["state"], "saved");
        assert!(!saved["confirmed"].as_bool().unwrap_or(true));
        assert!({
            let arg0 = (network).to_owned();
            let arg1 = ("c").to_owned();
            let arg2 = (spec.work_id).to_owned();
            node.store
                .call("peers.peer_cancelled", move |store| {
                    store.peer_cancelled(&arg0, &arg1, &arg2)
                })
                .await
        }?);
        let reopened = Store::open_readonly(dir.join("node.sqlite3"))?;
        assert!(
            reopened.peer_cancelled(&network, "c", &spec.work_id)?,
            "the durable intent is the dispatch fence"
        );
        drop((guard, reopened));
        node.accept_peer_envelope(&network, &sender_id, dispatch.clone())
            .await?;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let answer = node
                    .accept_peer_envelope(&network, &sender_id, dispatch.clone())
                    .await?;
                if answer["admission"]["state"] == "rejected" {
                    assert!(
                        answer["rejected"]
                            .as_str()
                            .is_some_and(|reason| reason.contains("cancelled"))
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            anyhow::Ok(())
        })
        .await
        .context("delayed dispatch is rejected by the earlier cancel")??;
        assert!(
            {
                let arg0 = (spec.work_id).to_owned();
                node.store
                    .call("peers.work_record", move |store| store.work_record(&arg0))
                    .await
            }?
            .is_none()
        );
        assert!(
            {
                let arg0 = ("c").to_owned();
                node.store
                    .call("peers.scheduler_jobs", move |store| {
                        store.scheduler_jobs(&arg0)
                    })
                    .await
            }?
            .is_empty()
        );
        let mut allowed = spec.clone();
        allowed.call_ref = "allowed".into();
        allowed.work_id = WorkSpec::id("c", "message:m1", "allowed");
        let allowed_command = node
            .store
            .call("peers.peer_dispatch_command", {
                let network = network.to_owned();
                let allowed = allowed.to_owned();
                move |store| store.peer_dispatch_command(&network, &allowed)
            })
            .await?
            .envelope;
        let _inbound = node
            .peer_delivery_locks
            .lock(&format!("inbound:{}", allowed_command.lane(&network)))
            .await;
        node.store
            .call("peers.save_peer_command", {
                let network = network.to_owned();
                let sender_id = sender_id.to_owned();
                let allowed_command = allowed_command.to_owned();
                move |store| {
                    store.save_peer_command(&network, &sender_id, &allowed_command, now_ms())
                }
            })
            .await?;
        let pending = node
            .store
            .call("peers.pending_peer_commands", {
                let network = network.to_owned();
                move |store| store.pending_peer_commands(&network)
            })
            .await?
            .into_iter()
            .find(|pending| pending.envelope.command_id == allowed_command.command_id)
            .context("saved dispatch")?;
        node.store
            .call("peers.execute_batch", {
                move |store| Ok(store.connection.execute_batch("PRAGMA query_only=ON")?)
            })
            .await?;
        let failed = node.process_peer_command(&network, &pending).await;
        node.store
            .call("peers.execute_batch", {
                move |store| Ok(store.connection.execute_batch("PRAGMA query_only=OFF")?)
            })
            .await?;
        assert!(
            failed.unwrap_err().is::<rusqlite::Error>(),
            "a storage failure stays retryable"
        );
        let pending = node
            .store
            .call("peers.pending_peer_commands", {
                let network = network.to_owned();
                move |store| store.pending_peer_commands(&network)
            })
            .await?
            .into_iter()
            .find(|pending| pending.envelope.command_id == allowed_command.command_id)
            .context("dispatch remains pending after a write failure")?;
        assert_eq!(pending.state, PeerAdmissionState::Saved);
        assert!(
            node.accept_dispatch(&network, allowed.clone(), &Value::Null)
                .await?
                .created,
            "a different uncancelled work remains admissible"
        );
        assert!(
            {
                let arg0 = (allowed.work_id).to_owned();
                node.store
                    .call("peers.work_record", move |store| store.work_record(&arg0))
                    .await
            }?
            .is_some()
        );
        let stop_payload = PeerCommand::Stop {
            conversation_id: "c".into(),
            work_id: allowed.work_id.clone(),
        };
        let held_stop = PeerEnvelope {
            command_id: stop_payload.id(&network),
            payload: stop_payload,
            ..allowed_command.clone()
        };
        {
            let arg0 = (network).to_owned();
            let arg1 = (sender_id).to_owned();
            let arg2 = (held_stop).to_owned();
            node.store
                .call("peers.save_peer_command", move |store| {
                    store.save_peer_command(&arg0, &arg1, &arg2, now_ms())
                })
                .await
        }?;
        node.interrupt_work(&network, &allowed.work_id).await?;
        let owner_change = sender.fixture_admin().attest(
            &network,
            "c",
            EventBody::ConversationOwner {
                scheduler_node: sender_id.clone(),
                epoch: allowed_command.owner_epoch + 1,
                attestation: None,
                cause: None,
            },
        )?;
        {
            let arg0 = (target).to_owned();
            let arg1 = (network).to_owned();
            let arg2 = ("c").to_owned();
            let arg3 = owner_change;
            let arg4 = vec![];
            node.store
                .call("peers.append_event", move |store| {
                    store.append_event(&arg0, &arg1, &arg2, arg3, arg4, now_ms())
                })
                .await
        }?;
        node.settings.lock().unwrap().manager = None;
        for command_id in [&allowed_command.command_id, &held_stop.command_id] {
            let pending = node
                .store
                .call("peers.pending_peer_commands", {
                    let network = network.to_owned();
                    move |store| store.pending_peer_commands(&network)
                })
                .await?
                .into_iter()
                .find(|pending| pending.envelope.command_id == *command_id)
                .context("unwritten peer receipt")?;
            node.store
                .call("peers.execute_batch", {
                    move |store| Ok(store.connection.execute_batch("PRAGMA query_only=ON")?)
                })
                .await?;
            let unwritten = node.process_peer_command(&network, &pending).await;
            node.store
                .call("peers.execute_batch", {
                    move |store| Ok(store.connection.execute_batch("PRAGMA query_only=OFF")?)
                })
                .await?;
            assert!(
                unwritten.unwrap_err().is::<rusqlite::Error>(),
                "a failed receipt write does not reject or repeat the work"
            );
            assert!(node.process_peer_command(&network, &pending).await?);
            let proof = node
                .accept_peer_envelope(&network, &sender_id, pending.envelope)
                .await?;
            assert_eq!(
                proof["admission"]["state"], "owner_accepted",
                "persisted Work and stop facts precede the newer epoch fence"
            );
        }
        assert_eq!(
            node.store
                .call("peers.work_records", move |store| store.work_records())
                .await?
                .len(),
            1
        );
        assert_eq!(
            {
                let arg0 = ("c").to_owned();
                node.store
                    .call("peers.scheduler_jobs", move |store| {
                        store.scheduler_jobs(&arg0)
                    })
                    .await
            }?
            .len(),
            1
        );
        assert!(
            {
                let arg0 = (network).to_owned();
                let arg1 = ("c").to_owned();
                let arg2 = (spec.work_id).to_owned();
                node.store
                    .call("peers.peer_cancelled", move |store| {
                        store.peer_cancelled(&arg0, &arg1, &arg2)
                    })
                    .await
            }?,
            "an accepted cancel does not revive the same Work after an epoch change"
        );
        anyhow::Ok(())
    }
    .await;
    drop(node);
    let _ = std::fs::remove_dir_all(dir);
    result
}

#[tokio::test]
async fn accepted_tasks_survive_disabled_or_changed_bot_settings_and_concurrent_retries()
-> Result<()> {
    use crate::{
        model::ResolvedModel,
        providers::Provider,
        work::{RequestedWorkKind, WorkRequest, WorkSpec},
    };
    let (sender, target) = (LocalIdentity::generate(), LocalIdentity::generate());
    let state = sender.sign_network_fixture(1, vec![sender.device_id()?, target.device_id()?])?;
    let network = sender.fixture_network_id()?;
    let (node, dir) = node(&target).await?;
    node.install_membership(state).await?;
    node.settings.lock().unwrap().roots = vec![dir.clone()];
    let initial = manager_settings(Provider::Claude, dir.clone(), "first-model".into());
    node.settings.lock().unwrap().manager = Some(initial.clone());
    {
        let sender = sender.clone();
        let network = network.clone();
        node.store
            .call("peers.fixture", move |store| {
                let owner = sender.owner_fixture(&network, "c", &sender.device_id()?)?;
                store.create_conversation(&sender, &network, "c", "Task", owner, now_ms())?;
                create_input_bot(
                    store,
                    &sender,
                    &sender.fixture_admin(),
                    &network,
                    "c",
                    now_ms(),
                )?;
                store.post_input_fixture(
                    &sender,
                    &network,
                    posted(&sender, &network, "c", "m1", 1)?,
                    now_ms(),
                )?;

                Ok(())
            })
            .await?
    }
    let spec = WorkRequest {
        work_id: WorkSpec::id("c", "message:m1", "same"),
        conversation: "c".into(),
        trigger: "message:m1".into(),
        target: target.device_id()?,
        dispatcher: sender.device_id()?,
        call_ref: "same".into(),
        kind: RequestedWorkKind::Task,
        model: Some("  ".into()),
        cwd: dir.display().to_string(),
        title: "task".into(),
        prompt: "go".into(),
        continues: None,
        parent: None,
        thread: None,
    };
    let (first, second) = tokio::join!(
        node.accept_dispatch(&network, spec.clone(), &Value::Null),
        node.accept_dispatch(&network, spec.clone(), &Value::Null),
    );
    assert_eq!(u8::from(first?.created) + u8::from(second?.created), 1);
    node.settings.lock().unwrap().manager = None;
    assert!(
        !node
            .accept_dispatch(&network, spec.clone(), &Value::Null)
            .await?
            .created
    );
    node.settings.lock().unwrap().manager = Some(config::ManagerSettings {
        provider: Provider::Pi,
        model: "changed-model".into(),
        ..initial
    });
    assert!(
        !node
            .accept_dispatch(&network, spec.clone(), &Value::Null)
            .await?
            .created
    );
    let changed = WorkRequest {
        model: Some("conflict".into()),
        ..spec.clone()
    };
    assert!(
        format!(
            "{:#}",
            node.accept_dispatch(&network, changed, &Value::Null)
                .await
                .unwrap_err()
        )
        .contains("changed: model")
    );
    let changed = WorkRequest {
        prompt: "different".into(),
        ..spec.clone()
    };
    assert!(
        node.accept_dispatch(&network, changed, &Value::Null)
            .await
            .is_err()
    );
    {
        let network = network.clone();
        node.store
            .call("peers.fixture", move |store| {
                let works = store.works(&network, "c")?;
                assert_eq!(works.len(), 1);
                assert_eq!(works[0].model, None);
                assert_eq!(
                    works[0].resolved,
                    ResolvedModel {
                        provider: Provider::Claude,
                        model: Some("first-model".into())
                    }
                );
                assert_eq!(
                    store
                        .scheduler_jobs("c")?
                        .iter()
                        .filter(|job| job.kind == crate::scheduling::TriggerKind::AgentTask)
                        .count(),
                    1
                );

                Ok(())
            })
            .await?
    }
    drop(node);
    let node = Arc::new(Runtime::reopen(&dir).await?);
    assert!(
        !node
            .accept_dispatch(&network, spec, &Value::Null)
            .await?
            .created
    );
    drop(node);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}
