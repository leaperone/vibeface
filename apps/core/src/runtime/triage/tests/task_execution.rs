use super::super::test_support::{cleanup, controlled_pi, main_input_fact, with_agent, with_node};
use super::super::*;
use crate::crypto::LocalIdentity;
use crate::model::{ResolvedModel, VersionVector};
use crate::runtime::test_support::{
    bot, claim_reply, manager_settings, node, pi_fixture_log, signed_main_input,
};
use std::sync::atomic::Ordering;
use vibeface_bridge::failure::FailureKind;

async fn task_bot(
    node: &Runtime,
    admin: &LocalIdentity,
    dir: &Path,
    mode: &str,
    manager: crate::config::ManagerSettings,
) -> Result<String> {
    controlled_pi(node, dir, mode)?;
    node.settings.lock().unwrap().manager = Some(manager);
    node.tool_url.set("http://127.0.0.1:1".into()).unwrap();
    bot(node, admin, "c", "Bot", &[]).await?;
    node.network_id().await
}

async fn task_at_start(
    node: &Arc<Runtime>,
    network: String,
    request: &crate::work::WorkRequest,
) -> Result<(
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<Result<Option<Value>>>,
)> {
    let claimed = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    *node.claim_to_start_barrier.lock().unwrap() = Some((claimed.clone(), resume.clone()));
    let node = node.clone();
    let conversation = request.conversation.clone();
    let scope = crate::scheduling::SessionScope::Task {
        work_id: request.work_id.clone(),
    };
    let running =
        tokio::spawn(
            async move { crate::scheduler::run(node, network, conversation, scope).await },
        );
    tokio::time::timeout(Duration::from_secs(5), claimed.notified()).await?;
    Ok((resume, running))
}

async fn queued_task(
    node: &Runtime,
    admin: &LocalIdentity,
    scope: &str,
    call: &str,
    provider: Provider,
    model: Option<&str>,
) -> Result<crate::work::WorkRequest> {
    let trigger = main_input_fact(node, admin, scope, "origin").await?;
    let request = crate::work::WorkRequest {
        work_id: crate::work::WorkSpec::id(scope, &trigger, call),
        conversation: scope.into(),
        trigger,
        target: node.identity.device_id()?,
        dispatcher: LocalIdentity::generate().device_id()?,
        call_ref: call.into(),
        kind: crate::work::RequestedWorkKind::Task,
        model: None,
        cwd: "/tmp".into(),
        title: "task".into(),
        prompt: "controlled task".into(),
        continues: None,
        parent: None,
        thread: None,
    };
    let network = node.network_id().await?;
    {
        let identity = (node.identity).to_owned();
        let network = (network).to_owned();
        let request = request.clone();
        let proposed = ResolvedModel {
            provider,
            model: model.map(str::to_owned),
        };
        node.store
            .call("triage.test.accept_work", move |store| {
                store.accept_work(&identity, &network, request, proposed, now_ms())
            })
            .await
    }?;
    Ok(request)
}

#[tokio::test]
async fn queued_task_cancellation_reports_the_committed_interrupt() -> Result<()> {
    with_node(async |node, admin, _dir| {
        bot(&node, &admin, "c", "Ops", &[]).await?;
        let request = queued_task(&node, &admin, "c", "cancel-queued", Provider::Pi, None).await?;
        let network = node.network_id().await?;
        let outcome = node
            .stop_work_outcome(&network, "c", &request.work_id)
            .await?;
        assert_eq!(outcome.receipt.execution, crate::tools::Execution::Applied);
        let work_id = request.work_id;
        node.store
            .call("triage.test.queued_cancel_receipt", move |store| {
                assert!(matches!(
                    store.work_record(&work_id)?.context("work")?.state,
                    crate::work::WorkState::Finished {
                        outcome: crate::model::WorkOutcome::Interrupted
                    }
                ));
                Ok(())
            })
            .await
    })
    .await
}

async fn assert_refused_task(
    node: &Runtime,
    network: &str,
    request: &crate::work::WorkRequest,
    kind: FailureKind,
) -> Result<()> {
    use crate::scheduling::{JobState, TurnExecution};
    let network = network.to_owned();
    let request = request.clone();
    node.store.call("triage.test.refused_task", move |store| {
        let network = network.as_str();
        let job = store
            .scheduler_job(
                &request.conversation,
                &crate::scheduling::task_trigger(&request.work_id),
            )?
            .context("task job")?;
        assert!(
            matches!(&job.state, JobState::Failed { failure, .. } if failure.kind == kind),
            "{job:?}"
        );
        let runtime = job.runtime.as_ref().context("runtime")?;
        assert_eq!(runtime.provider, Provider::Pi);
        assert_eq!(runtime.model.as_deref(), Some("fake/frozen"));
        assert_eq!(runtime.execution, TurnExecution::NotStarted);
        assert_eq!(
            runtime.stage,
            vibeface_bridge::failure::ProviderStage::Validate
        );
        assert_eq!(runtime.switched_from, None);
        assert!(job.retryable() && !job.pending_check && !job.tool_calls);
        assert_eq!(store.work_records()?.len(), 1);
        assert_eq!(
            store
                .scheduler_jobs(&request.conversation)?
                .iter()
                .filter(|job| job.kind == crate::scheduling::TriggerKind::AgentTask)
                .count(),
            1
        );
        let events = crate::store::context_after(
            &store.connection,
            network,
            &request.conversation,
            None,
            &VersionVector::new(),
        )?
        .events;
        assert_eq!(events.iter().filter(|event| matches!(&event.event.body, EventBody::WorkDispatched { work_id, .. } if work_id == &request.work_id)).count(), 1);
        assert_eq!(events.iter().filter(|event| matches!(&event.event.body, EventBody::WorkFinished { work_id, .. } if work_id == &request.work_id)).count(), 1);
        assert_eq!(events.iter().filter(|event| matches!(&event.event.body, EventBody::WorkProgress { work_id, .. } if work_id == &request.work_id)).count(), 0);
        Ok(())
    }).await
}

#[tokio::test]
async fn queued_tasks_are_refused_before_manager_launch_and_active_session_gates() -> Result<()> {
    for (via_tick, current) in [
        (false, None),
        (false, Some(Provider::Claude)),
        (true, None),
        (true, Some(Provider::Claude)),
    ] {
        with_node(async |node, admin, dir| {
            with_agent(&node);
            controlled_pi(&node, &dir, "reply")?;
            bot(&node, &admin, "c", "Bot", &[]).await?;
            let network = node.network_id().await?;
            let (human_trigger, _) = claim_reply(&node, &admin, "c", "human", crate::scheduler::boot_id()).await?;
            let active = {
                let trigger = (human_trigger).to_owned();
                node.store.call("triage.test.scheduler_job", move |store| {
                    store.scheduler_job("c", &trigger)
                }).await
            }?.context("active")?;
            let before = {

                node.store.call("triage.test.sql", move |store| {
                    Ok(store.connection.query_row("SELECT json_array(active,permission_version,session_generation,sdk_config) FROM scheduler_sessions WHERE conversation_id='c'", [], |row| row.get::<_, String>(0))?)
                }).await
            }?;
            let request = queued_task(&node, &admin, "c", "early-refusal", Provider::Pi, Some("fake/frozen")).await?;
            {
                let _controls = node.manager_controls.lock().await;
                node.settings.lock().unwrap().manager = current.map(|provider| crate::config::ManagerSettings { provider, workspace: dir.clone(), model: "changed".into(), reasoning_effort: None, backup: crate::scheduler::BackupPolicy { automatic: true, candidates: vec![crate::scheduler::BackupModel { provider: Provider::Pi, model: "fake/backup".into(), reasoning_effort: None }] } });
                node.pending_provider_launches.store(true, Ordering::SeqCst);
            }
            if via_tick {
                assert!(node.schedule_workers.lock().await.insert((network.clone(), crate::scheduling::SessionKey { conversation_id: "c".into(), scope: crate::scheduling::SessionScope::Main })));
                node.schedule_tick().await?;
            } else {
                crate::scheduler::run(node.clone(), network.clone(), "c".into(), crate::scheduling::SessionScope::Task { work_id: request.work_id.clone() }).await?.context("refusal invocation")?;
            }
            assert_refused_task(&node, &network, &request, if current.is_none() { FailureKind::NoBot } else { FailureKind::ExecutionDisabled }).await?;
            let refused = {
                let trigger = (crate::scheduling::task_trigger(&request.work_id)).to_owned();
                node.store.call("triage.test.scheduler_job", move |store| {
                    store.scheduler_job("c", &trigger)
                }).await
            }?.context("refusal")?;
            node.finish_incompatible_tasks(&network, None).await?;
            assert_eq!({
                let trigger = (refused.trigger).to_owned();
                node.store.call("triage.test.scheduler_job", move |store| {
                    store.scheduler_job("c", &trigger)
                }).await
                }?.context("refusal")?.runtime, refused.runtime);
            assert_eq!(serde_json::to_value({
                let trigger = (human_trigger).to_owned();
                node.store.call("triage.test.scheduler_job", move |store| {
                    store.scheduler_job("c", &trigger)
                }).await
                    }?.context("active")?)?, serde_json::to_value(active)?);
            let after = {

                node.store.call("triage.test.sql", move |store| {
                    Ok(store.connection.query_row("SELECT json_array(active,permission_version,session_generation,sdk_config) FROM scheduler_sessions WHERE conversation_id='c'", [], |row| row.get::<_, String>(0))?)
                }).await
            }?;
            assert_eq!(before, after);
            assert!(pi_fixture_log(&dir)?.is_empty());
            assert!(node.agent_tokens.lock().await.is_empty());
            let retried = node.accept_dispatch(&network, request.clone(), &Value::Null).await?;
            assert!(!retried.created);
            assert_eq!(retried.spec.resolved.model.as_deref(), Some("fake/frozen"));
            let peer_retry = node.accept_remote_dispatch(&network, &request.dispatcher, &json!({"spec":request})).await?;
            assert_eq!(peer_retry["work_id"], request.work_id);
            for resolved in [Value::Null, json!({"provider":"pi","model":"forged"})] {
                let mut input = json!({"spec":request}); input["spec"]["resolved"] = resolved;
                assert!(node.accept_remote_dispatch(&network, &request.dispatcher, &input).await.is_err());
            }
            assert!(node.accept_remote_dispatch(&network, &node.identity.device_id()?, &json!({"spec":request})).await.is_err());
            if via_tick {
                assert!(node.schedule_workers.lock().await.remove(&(network.clone(), crate::scheduling::SessionKey { conversation_id: "c".into(), scope: crate::scheduling::SessionScope::Main })));
            }
            Ok::<_, anyhow::Error>(())
        })
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn a_task_disabled_between_claim_and_start_finishes_without_a_token_or_sdk_call() -> Result<()>
{
    for current in [None, Some(Provider::Claude)] {
        let (node, admin, dir) = node().await?;
        let result = async {
            let manager = crate::config::ManagerSettings {
                backup: crate::scheduler::BackupPolicy {
                    automatic: true,
                    candidates: vec![crate::scheduler::BackupModel {
                        provider: Provider::Pi,
                        model: "fake/backup".into(),
                        reasoning_effort: None,
                    }],
                },
                ..manager_settings(Provider::Pi, dir.clone(), "fake/current".into())
            };
            let network = task_bot(&node, &admin, &dir, "reply", manager.clone()).await?;
            let request = queued_task(
                &node,
                &admin,
                "c",
                "barrier",
                Provider::Pi,
                Some("fake/frozen"),
            )
            .await?;
            let (resume, running) = task_at_start(&node, network.clone(), &request).await?;
            let initial = {
                let trigger = (crate::scheduling::task_trigger(&request.work_id)).to_owned();
                node.store
                    .call("triage.test.scheduler_job", move |store| {
                        store.scheduler_job("c", &trigger)
                    })
                    .await
            }?
            .context("claimed")?;
            assert_eq!(
                initial
                    .runtime
                    .as_ref()
                    .context("initial runtime")?
                    .model
                    .as_deref(),
                Some("fake/frozen")
            );
            {
                let _controls = node.manager_controls.lock().await;
                node.settings.lock().unwrap().manager =
                    current.map(|provider| crate::config::ManagerSettings {
                        provider,
                        ..manager
                    });
            }
            resume.notify_one();
            tokio::time::timeout(Duration::from_secs(10), running)
                .await???
                .context("finished refusal")?;
            assert_refused_task(
                &node,
                &network,
                &request,
                if current.is_none() {
                    FailureKind::NoBot
                } else {
                    FailureKind::ExecutionDisabled
                },
            )
            .await?;
            assert!(node.agent_tokens.lock().await.is_empty());
            assert!(node.manager_controls.lock().await.is_empty());
            assert!(
                {
                    node.store
                        .call("triage.test.pending_checks", move |store| {
                            store.pending_checks()
                        })
                        .await
                }?
                .is_empty()
            );
            assert!(pi_fixture_log(&dir)?.is_empty());
            let active: Option<String> = {
                node.store
                    .call("triage.test.sql", move |store| {
                        Ok(store.connection.query_row(
                            "SELECT active FROM scheduler_sessions WHERE conversation_id='c'",
                            [],
                            |row| row.get(0),
                        )?)
                    })
                    .await
            }?;
            assert_eq!(active, None);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn task_registration_that_cannot_mark_started_leaves_no_token_and_preserves_unknown_evidence()
-> Result<()> {
    let (node, admin, dir) = node().await?;
    let result = async {
        let manager = manager_settings(Provider::Pi, dir.clone(), "fixture".into());
        let network = task_bot(&node, &admin, &dir, "reply", manager).await?;
        let request = queued_task(
            &node,
            &admin,
            "c",
            "mark-failure",
            Provider::Pi,
            Some("fake/frozen"),
        )
        .await?;
        let (resume, running) = task_at_start(&node, network.clone(), &request).await?;
        let trigger = crate::scheduling::task_trigger(&request.work_id);
        let job = {
            let trigger = (trigger).to_owned();
            node.store
                .call("triage.test.scheduler_job", move |store| {
                    store.scheduler_job("c", &trigger)
                })
                .await
        }?
        .context("claim")?;
        let attempt = job.state.attempt().context("attempt")?.id.clone();
        let before_registration = {
            let trigger = trigger.clone();
            let attempt = attempt.clone();
            node.store
                .call("triage.test.before_registration", move |store| {
                    store.mark_started("c", &trigger, &attempt)?;
                    store.update_turn_runtime(
                        "c",
                        &trigger,
                        &attempt,
                        vibeface_bridge::failure::ProviderStage::TurnStart,
                        crate::scheduling::TurnExecution::Unknown,
                    )
                })
                .await?
        };
        resume.notify_one();
        tokio::time::timeout(Duration::from_secs(10), running)
            .await???
            .context("reconciled refusal")?;
        assert!(matches!(
            before_registration.state,
            crate::scheduling::JobState::Running { .. }
        ));
        assert_eq!(
            before_registration
                .runtime
                .as_ref()
                .context("unknown runtime before registration")?
                .execution,
            crate::scheduling::TurnExecution::Unknown
        );
        let job = {
            let trigger = (trigger).to_owned();
            node.store
                .call("triage.test.scheduler_job", move |store| {
                    store.scheduler_job("c", &trigger)
                })
                .await
        }?
        .context("job")?;
        assert!(matches!(
            job.state,
            crate::scheduling::JobState::Reconciling { .. }
        ));
        assert_eq!(
            job.runtime.as_ref().context("runtime")?.execution,
            crate::scheduling::TurnExecution::Unknown
        );
        assert!(job.pending_check);
        assert!(node.agent_tokens.lock().await.is_empty());
        assert!(node.manager_controls.lock().await.is_empty());
        assert!(pi_fixture_log(&dir)?.is_empty());
        assert_eq!(
            {
                node.store
                    .call("triage.test.pending_checks", move |store| {
                        store.pending_checks()
                    })
                    .await
            }?
            .len(),
            1
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    cleanup(node, dir).await?;
    result
}

#[tokio::test]
async fn task_turns_use_frozen_models_and_never_automatic_backup_but_human_turns_can() -> Result<()>
{
    for (kind, accepted, mode) in [
        ("task", Some("fake/frozen"), "reply"),
        ("task-default", None, "reply"),
        ("task-failure", Some("fake/frozen"), "refuse"),
        ("human", None, "reply"),
    ] {
        let (node, admin, dir) = node().await?;
        let result = async {
            let manager = crate::config::ManagerSettings {
                backup: crate::scheduler::BackupPolicy {
                    automatic: true,
                    candidates: vec![crate::scheduler::BackupModel {
                        provider: Provider::Pi,
                        model: "fake/backup".into(),
                        reasoning_effort: None,
                    }],
                },
                ..manager_settings(
                    if kind == "human" {
                        Provider::Claude
                    } else {
                        Provider::Pi
                    },
                    dir.clone(),
                    "fake/changed-current".into(),
                )
            };
            let network = task_bot(&node, &admin, &dir, mode, manager).await?;
            node.providers
                .lock()
                .unwrap()
                .bins
                .insert(Provider::Claude, dir.join("missing-claude"));
            let trigger = if kind == "human" {
                {
                    let identity = (node.identity).to_owned();
                    let network = (network).to_owned();
                    let body = signed_main_input(
                        &admin,
                        &network,
                        "c",
                        "human",
                        "controlled request",
                        vec![],
                        1,
                    )?;
                    node.store
                        .call("triage.test.post_input_fixture", move |store| {
                            store.post_input_fixture(&identity, &network, body, now_ms())
                        })
                        .await
                }?;
                "message:human".into()
            } else {
                let request = queued_task(&node, &admin, "c", kind, Provider::Pi, accepted).await?;
                crate::scheduling::task_trigger(&request.work_id)
            };
            tokio::time::timeout(
                Duration::from_secs(15),
                crate::scheduler::run(
                    node.clone(),
                    network.clone(),
                    "c".into(),
                    match trigger.strip_prefix("task:") {
                        Some(work_id) => crate::scheduling::SessionScope::Task {
                            work_id: work_id.into(),
                        },
                        None => crate::scheduling::SessionScope::Main,
                    },
                ),
            )
            .await??
            .context("turn")?;
            let job = {
                let trigger = (trigger).to_owned();
                node.store
                    .call("triage.test.scheduler_job", move |store| {
                        store.scheduler_job("c", &trigger)
                    })
                    .await
            }?
            .context("job")?;
            let runtime = job.runtime.as_ref().context("runtime")?;
            assert_eq!(runtime.provider, Provider::Pi);
            assert_eq!(
                runtime.model.as_deref(),
                if kind == "human" {
                    Some("fake/backup")
                } else {
                    accepted
                }
            );
            let log = pi_fixture_log(&dir)?;
            assert_eq!(
                log.iter()
                    .filter(|entry| entry.get("prompt").is_some())
                    .count(),
                1
            );
            let launches: Vec<_> = log
                .iter()
                .filter_map(|entry| entry["argv"].as_array())
                .collect();
            assert_eq!(launches.len(), 1);
            let argv = launches[0];
            let model = argv
                .iter()
                .position(|arg| arg == "--model")
                .map(|index| argv[index + 1].as_str().unwrap());
            assert_eq!(
                model,
                if kind == "human" {
                    Some("fake/backup")
                } else {
                    accepted
                }
            );
            if kind == "task-failure" {
                assert!(matches!(
                    job.state,
                    crate::scheduling::JobState::Failed { .. }
                ));
                assert_eq!(
                    runtime.execution,
                    crate::scheduling::TurnExecution::NotStarted
                );
                assert_eq!(runtime.switched_from, None);
            } else {
                assert!(
                    matches!(job.state, crate::scheduling::JobState::Completed { .. }),
                    "{job:?}"
                );
                assert_eq!(runtime.switched_from.is_some(), kind == "human");
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        cleanup(node, dir).await?;
        result?;
    }
    Ok(())
}
