use crate::{
    config::{self, Config},
    crypto::{self, LocalIdentity},
    engine::{Engine, EngineAction, ExecutorConfig, ExecutorKind, ExecutorSession},
    identity::{AuthorizationView, IdentityKind, IdentityStore, NetworkState},
    maintenance::{Busy, Gate},
    model::*,
    network::{IncomingSocket, NetworkHub, Service},
    peer::{Inbound, PeerHub},
    store::Store,
};
use anyhow::{Context as _, Result, bail, ensure};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, broadcast, mpsc};

mod chat_controls;
mod followup;
mod manager_control;
#[cfg(test)]
mod manager_control_tests;
mod observation;
mod schedule_delivery;
pub use manager_control::ManagerAccess;
pub(crate) use observation::ObservationGuard;

#[derive(Clone)]
pub struct Access {
    pub network_id: String,
    pub human: bool,
    pub conversation_id: Option<String>,
    pub manager: Option<ManagerAccess>,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_owner(node: &Runtime) -> Result<LocalIdentity> {
        Ok(LocalIdentity {
            secret_key_hex: crypto::digest(
                format!("runtime-test-root:{}", node.identity.node_id()?).as_bytes(),
            ),
        })
    }
    pub(super) fn fixture() -> Result<PathBuf> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("runtime-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        config::private_write(&dir.join("data-epoch.json"), br#"{"data_epoch":2}"#)?;
        let mut cfg = Config::local("127.0.0.1:4790".parse()?, vec![dir.canonicalize()?], false);
        cfg.codex_socket = Some(dir.join("unavailable.sock"));
        cfg.save(&dir)?;
        config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&LocalIdentity::generate())?,
        )?;
        Ok(dir)
    }
    pub(super) async fn conversation(node: &Runtime) -> Result<String> {
        let owner = test_owner(node)?;
        let state = owner.sign_network_fixture(
            1,
            vec![node.identity.certificate(node.config.roles.clone())?],
        )?;
        let view = node.install_membership(state).await?;
        node.append(
            &view.network_id,
            "chat",
            EventBody::ConversationCreated {
                title: "test".into(),
            },
        )
        .await?;
        Ok(view.network_id)
    }
    async fn queued(
        node: &Runtime,
        network: &str,
        operation: &str,
        action: Value,
    ) -> Result<OperationRequest> {
        let local = node.identity.node_id()?;
        let mut store = node.store.lock().await;
        store.upsert_session(
            network,
            &ExecutionSession {
                network_id: network.into(),
                target_node: local.clone(),
                session_id: operation.into(),
                revision: 0,
                state: "idle".into(),
                executor: "claude_code".into(),
                cwd: node.config.roots[0].to_string_lossy().into_owned(),
                native_session_id: Some("native-test".into()),
            },
        )?;
        let mut request = OperationRequest {
            operation_id: operation.into(),
            network_id: network.into(),
            conversation_id: "chat".into(),
            task_id: None,
            target_node: local,
            session_id: operation.into(),
            expected_revision: 0,
            basis_heads: store.heads(network, "chat")?,
            action,
            resources: vec![node.config.roots[0].to_string_lossy().into_owned()],
            binding: None,
            manager_authority: None,
        };
        store.bind_test_operation(&node.identity, &mut request)?;
        store.accept_operation(network, &request)?;
        Ok(request)
    }
    fn configure_manager(dir: &Path) -> Result<()> {
        let mut config = Config::load(dir)?;
        config.manager = Some(config::ManagerSettings {
            provider: "cursor_sdk".into(),
            workspace: dir.into(),
            model: "auto".into(),
            cursor_sdk_bin: dir.join("must-not-spawn"),
            node_bin: "node".into(),
            codex_sdk_script: dir.join("must-not-run.mjs"),
        });
        config.save(dir)
    }
    async fn task(node: &Arc<Runtime>, network: &str, task_id: &str, report: bool) -> Result<()> {
        let access = Access {
            network_id: network.into(),
            human: true,
            conversation_id: None,
            manager: None,
        };
        node.rpc(
            access.clone(),
            json!({"method":"create_task","conversation_id":"chat","task_id":task_id,
                "title":"test","goal":"test","acceptance":"user verifies"}),
        )
        .await?;
        if report {
            let version = node
                .store
                .lock()
                .await
                .followup_policy(network, "chat", task_id)?
                .version;
            node.rpc(
                access,
                json!({"method":"set_followup","conversation_id":"chat",
                "task_id":task_id,"mode":"report","expected_version":version,
                "scheduler_node":node.identity.node_id()?}),
            )
            .await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn unsupported_epoch_does_not_open_databases_or_modify_the_directory() -> Result<()> {
        let dir = fixture()?;
        std::fs::remove_file(dir.join("data-epoch.json"))?;
        let config = std::fs::read(dir.join("config.json"))?;
        assert!(Runtime::open(&dir).await.is_err());
        assert!(!dir.join("node.sqlite3").exists());
        assert!(!dir.join("identity.sqlite3").exists());
        assert_eq!(std::fs::read(dir.join("config.json"))?, config);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn membership_reopen_recovers_either_durable_store_without_root_secrets() -> Result<()> {
        let dir = fixture()?;
        let node = Runtime::open(&dir).await?;
        assert!(!node.has_membership().await?);
        let owner = test_owner(&node)?;
        let initial = owner.sign_network_fixture(
            1,
            vec![node.identity.certificate(node.config.roles.clone())?],
        )?;
        node.identities.init_network(initial.clone())?;
        drop(node);
        let node = Runtime::open(&dir).await?;
        assert_eq!(node.membership_state().await?, initial);
        assert!(node.has_membership().await?);
        let updated = owner.sign_network_fixture(
            2,
            vec![
                node.identity
                    .certificate(vec![NodeRole::Gateway, NodeRole::Replica])?,
            ],
        )?;
        node.store
            .lock()
            .await
            .install_trust_bundle(&owner.fixture_network_id()?, &updated)?;
        drop(node);
        let node = Runtime::open(&dir).await?;
        assert_eq!(node.membership_state().await?, updated);
        assert_eq!(node.identities.public_state()?, updated);
        assert!(!dir.join("auth.sqlite3").exists());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn executor_followup_only_accepts_its_authorized_dispatched_result() -> Result<()> {
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        task(&node, &network, "task", true).await?;
        task(&node, &network, "unapproved", false).await?;
        let executor = LocalIdentity::generate();
        let origin = executor.node_id()?;
        let owner = test_owner(&node)?;
        let mut store = node.store.lock().await;
        let mut nodes = store
            .trust_bundle(&network)?
            .unwrap()
            .project()?
            .members
            .into_iter()
            .filter(|member| member.kind == IdentityKind::Node)
            .map(|member| member.certificate())
            .collect::<Result<Vec<_>>>()?;
        nodes.push(executor.certificate(vec![NodeRole::Replica, NodeRole::Executor])?);
        store.install_trust_bundle(&network, &owner.sign_network_fixture(2, nodes)?)?;
        let mut request = OperationRequest {
            operation_id: "operation".into(),
            network_id: network.clone(),
            conversation_id: "chat".into(),
            task_id: Some("task".into()),
            target_node: origin.clone(),
            session_id: "session".into(),
            expected_revision: 0,
            basis_heads: store.heads(&network, "chat")?,
            action: json!({"type":"send","text":"test"}),
            resources: vec![],
            binding: None,
            manager_authority: None,
        };
        store.bind_test_operation(&node.identity, &mut request)?;
        let scheduler = node.identity.node_id()?;
        store.append_event(
            &node.identity,
            &network,
            "chat",
            EventBody::DispatchRecorded {
                operation_id: request.operation_id.clone(),
                task_id: request.task_id.clone(),
                target_node: origin.clone(),
                session_id: request.session_id.clone(),
                action: json!(request),
            },
            vec![],
            now_ms(),
        )?;
        let result = EventBody::ExecutionObserved {
            operation_id: request.operation_id.clone(),
            task_id: request.task_id.clone(),
            target_node: origin.clone(),
            session_id: request.session_id.clone(),
            status: "succeeded".into(),
            detail: json!({"result":"done"}),
        };
        let envelope = store.append_event(
            &executor,
            &network,
            "chat",
            result.clone(),
            vec![],
            now_ms(),
        )?;
        store.validate_followup_result(&network, "chat", &envelope.hash, &scheduler)?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &envelope.hash, &origin)
                .is_err()
        );
        assert!(
            store
                .validate_followup_result(&network, "another-chat", &envelope.hash, &scheduler)
                .is_err()
        );
        let message = store.append_event(
            &executor,
            &network,
            "chat",
            EventBody::MessagePosted {
                message_id: "forged-user".into(),
                role: "user".into(),
                text: "wake manager".into(),
                scheduler_node: Some(node.identity.node_id()?),
            },
            vec![],
            now_ms(),
        )?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &message.hash, &scheduler)
                .is_err()
        );
        for (operation, target, status) in [
            ("not-dispatched", origin.as_str(), "succeeded"),
            ("operation", "another-executor", "succeeded"),
            ("operation", origin.as_str(), "unknown"),
        ] {
            let mut invalid = result.clone();
            if let EventBody::ExecutionObserved {
                operation_id,
                target_node,
                status: state,
                ..
            } = &mut invalid
            {
                *operation_id = operation.into();
                *target_node = target.into();
                *state = status.into();
            }
            let invalid =
                store.append_event(&executor, &network, "chat", invalid, vec![], now_ms())?;
            assert!(
                store
                    .validate_followup_result(&network, "chat", &invalid.hash, &scheduler)
                    .is_err()
            );
        }
        let mut unapproved = request.clone();
        unapproved.operation_id = "unapproved-operation".into();
        unapproved.task_id = Some("unapproved".into());
        unapproved.binding = None;
        store.bind_test_operation(&node.identity, &mut unapproved)?;
        store.append_event(
            &node.identity,
            &network,
            "chat",
            EventBody::DispatchRecorded {
                operation_id: unapproved.operation_id.clone(),
                task_id: unapproved.task_id.clone(),
                target_node: origin.clone(),
                session_id: unapproved.session_id.clone(),
                action: json!(unapproved),
            },
            vec![],
            now_ms(),
        )?;
        let invalid = store.append_event(
            &executor,
            &network,
            "chat",
            EventBody::ExecutionObserved {
                operation_id: unapproved.operation_id,
                task_id: unapproved.task_id,
                target_node: origin.clone(),
                session_id: unapproved.session_id,
                status: "succeeded".into(),
                detail: json!({}),
            },
            vec![],
            now_ms(),
        )?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &invalid.hash, &scheduler)
                .is_err()
        );
        store.append_event(
            &node.identity,
            &network,
            "chat",
            EventBody::TaskChanged {
                task_id: "task".into(),
                status: "cancelled".into(),
                detail: "user stopped".into(),
            },
            vec![],
            now_ms(),
        )?;
        assert!(
            store
                .validate_followup_result(&network, "chat", &envelope.hash, &scheduler)
                .is_err()
        );
        drop(store);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn unstarted_followup_survives_reopen_and_claimed_attempt_is_not_repeated() -> Result<()>
    {
        use crate::scheduling::{JobState, TriggerKind};

        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let local = node.identity.node_id()?;
        task(&node, &network, "task", false).await?;
        let mut request = queued(
            &node,
            &network,
            "seed",
            json!({"type":"send","text":"test"}),
        )
        .await?;
        {
            let mut store = node.store.lock().await;
            store.claim_operation(&network, "seed")?;
            store.finish_operation(&network, "seed", OperationState::Failed, json!({}))?;
            request.operation_id = "followup".into();
            request.task_id = Some("task".into());
            request.expected_revision = store
                .session(&network, &request.target_node, &request.session_id)?
                .unwrap()
                .revision;
            request.basis_heads = store.heads(&network, "chat")?;
            store.bind_test_operation(&node.identity, &mut request)?;
            store.append_event(
                &node.identity,
                &network,
                "chat",
                EventBody::DispatchRecorded {
                    operation_id: request.operation_id.clone(),
                    task_id: request.task_id.clone(),
                    target_node: request.target_node.clone(),
                    session_id: request.session_id.clone(),
                    action: json!(request),
                },
                vec![],
                now_ms(),
            )?;
            store.accept_operation(&network, &request)?;
            store.claim_operation(&network, &request.operation_id)?;
        }
        node.finish(
            &request,
            OperationState::Succeeded,
            json!({"result":"done"}),
        )
        .await?;
        node.flush_completions(&network).await?;
        let marker_key = "followup-wait:v1:followup";
        let result_hash = {
            let mut store = node.store.lock().await;
            let result = store
                .context(&network, "chat", &[])?
                .events
                .into_iter()
                .find(|event| {
                    matches!(&event.body, EventBody::ExecutionObserved { operation_id, status, .. }
                        if operation_id == "followup" && status == "succeeded")
                })
                .context("completed operation has no durable result")?;
            let result_hash = crypto::digest(&serde_json::to_vec(&result)?);
            assert!(
                store
                    .local_value(&network, "completion:followup")?
                    .is_none()
            );
            assert_eq!(
                store.local_value(&network, marker_key)?.unwrap()["trigger"],
                result_hash
            );
            assert!(store.scheduler_jobs(&network, "chat", &local)?.is_empty());
            store.seal_binding(&node.identity, request.binding.as_ref().unwrap())?;
            result_hash
        };
        drop(node);

        let node = Arc::new(Runtime::open(&dir).await?);
        node.flush_completions(&network).await?;
        {
            let store = node.store.lock().await;
            assert!(
                store
                    .local_value(&network, "completion:followup")?
                    .is_none()
            );
            assert_eq!(
                store.local_value(&network, marker_key)?.unwrap()["trigger"],
                result_hash
            );
            assert!(store.scheduler_jobs(&network, "chat", &local)?.is_empty());
        }
        let grant = node
            .set_followup(
                &Access {
                    network_id: network.clone(),
                    human: true,
                    conversation_id: Some("chat".into()),
                    manager: None,
                },
                "chat",
                &json!({"task_id":"task","mode":"report","expected_version":0,
                    "scheduler_node":local}),
            )
            .await?;
        assert_eq!(grant["policy"]["mode"], "report");
        assert_eq!(grant["policy"]["version"], 1);
        assert!(node.maybe_followup(&request, &result_hash).await?);
        assert!(node.maybe_followup(&request, &result_hash).await?);
        let queued_job = {
            let store = node.store.lock().await;
            assert!(store.local_value(&network, marker_key)?.is_none());
            let jobs = store.scheduler_jobs(&network, "chat", &local)?;
            assert_eq!(jobs.len(), 1);
            let job = jobs.into_iter().next().unwrap();
            assert_eq!(job.trigger, result_hash);
            assert_eq!(job.kind, TriggerKind::Autonomous);
            assert_eq!(job.state, JobState::Queued);
            job
        };
        drop(node);

        let node = Arc::new(Runtime::open(&dir).await?);
        let attempt = {
            let mut store = node.store.lock().await;
            assert_eq!(
                store.scheduler_jobs(&network, "chat", &local)?,
                vec![queued_job.clone()]
            );
            let claimed = store
                .claim_next(&network, "chat", &local, "followup-boot-a", "test-sdk")?
                .context("authorized result was not queued")?;
            assert_eq!(claimed.job.trigger, result_hash);
            assert_eq!(claimed.job.sequence, queued_job.sequence);
            assert_eq!(claimed.job.message_id, queued_job.message_id);
            let attempt = claimed.job.state.attempt().unwrap().clone();
            assert!(attempt.read_only);
            assert_eq!(attempt.followup_version, Some(1));
            let started =
                store.mark_started(&network, "chat", &local, &result_hash, &attempt.id)?;
            assert_eq!(
                started.state,
                JobState::Running {
                    attempt: attempt.clone()
                }
            );
            attempt
        };
        drop(node);

        let node = Arc::new(Runtime::open(&dir).await?);
        let recovered = {
            let mut store = node.store.lock().await;
            assert!(
                store
                    .claim_next(&network, "chat", &local, "followup-boot-b", "test-sdk")?
                    .is_none()
            );
            let job = store
                .scheduler_job(&network, "chat", &result_hash, &local)?
                .context("started follow-up was lost after reopen")?;
            assert_eq!(
                job.state,
                JobState::Unknown {
                    attempt,
                    detail:
                        "Node restarted after the SDK start boundary; this call was not replayed"
                            .into(),
                    fenced: false,
                }
            );
            job
        };
        node.flush_completions(&network).await?;
        node.finish(
            &request,
            OperationState::Succeeded,
            json!({"result":"done"}),
        )
        .await?;
        {
            let mut store = node.store.lock().await;
            assert_eq!(
                store.scheduler_jobs(&network, "chat", &local)?,
                vec![recovered]
            );
            assert!(
                store
                    .local_value(&network, "completion:followup")?
                    .is_none()
            );
            assert!(store.local_value(&network, marker_key)?.is_none());
            assert!(
                store
                    .claim_next(&network, "chat", &local, "followup-boot-b", "test-sdk")?
                    .is_none()
            );
            assert_eq!(
                store
                    .context(&network, "chat", &[])?
                    .events
                    .iter()
                    .filter(|event| {
                        matches!(&event.body, EventBody::ExecutionObserved { operation_id, status, .. }
                            if operation_id == "followup" && status == "succeeded")
                    })
                    .count(),
                1
            );
        }
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn agent_cannot_reopen_or_start_new_work_for_a_user_finalized_task() -> Result<()> {
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        task(&node, &network, "task", false).await?;
        let human = Access {
            network_id: network.clone(),
            human: true,
            conversation_id: Some("chat".into()),

            manager: None,
        };
        let agent = Access {
            human: false,
            ..human.clone()
        };
        for status in ["accepted", "cancelled"] {
            node.rpc(human.clone(), json!({"method":"update_task","conversation_id":"chat","task_id":"task","status":status})).await?;
            let update = json!({"method":"update_task","conversation_id":"chat","task_id":"task","status":"working"});
            assert!(
                node.rpc(agent.clone(), update.clone())
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("only the user may reopen")
            );
            let request = OperationRequest {
                operation_id: format!("new-{status}"),
                network_id: network.clone(),
                conversation_id: "chat".into(),
                task_id: Some("task".into()),
                target_node: node.identity.node_id()?,
                session_id: format!("session-{status}"),
                expected_revision: 0,
                basis_heads: node.store.lock().await.heads(&network, "chat")?,
                action: json!({"type":"open","executor":"claude_code","cwd":dir}),
                resources: vec![],
                binding: None,
                manager_authority: None,
            };
            assert!(
                node.dispatch(agent.clone(), request.clone())
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("only the user may reopen")
            );
            assert!(
                node.admit_operation(request, false)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("only the user may reopen")
            );
            node.rpc(human.clone(), update).await?;
        }
        assert!(node.store.lock().await.operations(&network)?.is_empty());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn agent_cannot_silently_detach_a_session_from_its_latest_admitted_task() -> Result<()> {
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        task(&node, &network, "task", false).await?;
        task(&node, &network, "next-task", false).await?;
        let human = Access {
            network_id: network.clone(),
            human: true,
            conversation_id: Some("chat".into()),

            manager: None,
        };
        let mut associated = queued(
            &node,
            &network,
            "seed",
            json!({"type":"send","text":"test"}),
        )
        .await?;
        {
            let mut store = node.store.lock().await;
            store.claim_operation(&network, "seed")?;
            store.finish_operation(&network, "seed", OperationState::Succeeded, json!({}))?;
            associated.operation_id = "associated".into();
            associated.task_id = Some("task".into());
            associated.expected_revision = 1;
            associated.binding = None;
            store.bind_test_operation(&node.identity, &mut associated)?;
            store.accept_operation(&network, &associated)?;
            let mut concurrent = associated.clone();
            concurrent.operation_id = "concurrent-untasked-steer".into();
            concurrent.task_id = Some("task".into());
            concurrent.action = json!({"type":"steer","turn_id":"turn","text":"test"});
            concurrent.resources.clear();
            store.accept_control_operation(&network, &concurrent)?;
            for operation in [&associated, &concurrent] {
                store.claim_operation(&network, &operation.operation_id)?;
            }
            for operation in [&associated, &concurrent] {
                store.finish_operation(
                    &network,
                    &operation.operation_id,
                    OperationState::Succeeded,
                    json!({}),
                )?;
            }
        }
        let mut request = associated.clone();
        request.operation_id = "untasked".into();
        request.task_id = None;
        request.expected_revision = 2;
        request.resources.clear();
        for status in ["working", "accepted", "cancelled"] {
            node.rpc(human.clone(), json!({"method":"update_task","conversation_id":"chat","task_id":"task","status":status})).await?;
            for action in [
                json!({"type":"steer","turn_id":"turn","text":"test"}),
                json!({"type":"send","text":"test"}),
                json!({"type":"open","executor":"claude_code","cwd":dir}),
            ] {
                request.action = action;
                assert!(
                    node.admit_operation(request.clone(), false)
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("task_id is required")
                );
            }
        }
        assert_eq!(
            node.admit_operation(associated.clone(), false).await?["operation"]["state"],
            "succeeded"
        );
        let mut unadmitted = request.clone();
        unadmitted.operation_id = "unadmitted".into();
        unadmitted.expected_revision = 99;
        node.append(
            &network,
            "chat",
            EventBody::DispatchRecorded {
                operation_id: unadmitted.operation_id.clone(),
                task_id: None,
                target_node: unadmitted.target_node.clone(),
                session_id: unadmitted.session_id.clone(),
                action: json!(unadmitted),
            },
        )
        .await?;
        assert!(node.admit_operation(request.clone(), false).await.is_err());
        request.action = json!({"type":"steer","turn_id":"turn","text":"test"});
        for (operation, human, task_id) in [
            ("retask", false, Some("next-task")),
            ("human-detach", true, None),
            ("agent-after-detach", false, None),
        ] {
            request.operation_id = operation.into();
            request.task_id = task_id.map(String::from);
            request.basis_heads = node.store.lock().await.heads(&network, "chat")?;
            assert!(node.admit_operation(request.clone(), human).await.is_err());
        }
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn peer_mutations_and_replica_reads_enforce_roles() -> Result<()> {
        let dir = fixture()?;
        configure_manager(&dir)?;
        let mut node = Arc::new(Runtime::open(&dir).await?);
        let owner = LocalIdentity::generate();
        let replica = LocalIdentity::generate();
        let relay = LocalIdentity::generate();
        let scheduler = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        node.install_membership(owner.sign_network_fixture(
            1,
            vec![
                node.identity.certificate(node.config.roles.clone())?,
                owner.certificate(vec![NodeRole::Gateway, NodeRole::Replica])?,
                replica.certificate(vec![NodeRole::Replica])?,
                relay.certificate(vec![NodeRole::Relay])?,
                scheduler.certificate(vec![NodeRole::Scheduler, NodeRole::Replica])?,
            ],
        )?)
        .await?;
        let event = node
            .append(
                &network,
                "chat",
                EventBody::ConversationCreated {
                    title: "roles".into(),
                },
            )
            .await?;
        let request = queued(
            &node,
            &network,
            "unknown-role-test",
            json!({"type":"send","text":"test"}),
        )
        .await?;
        {
            let mut store = node.store.lock().await;
            store.claim_operation(&network, &request.operation_id)?;
            store.finish_operation(
                &network,
                &request.operation_id,
                OperationState::Unknown,
                json!({}),
            )?;
        }
        for (origin, human, expected) in [
            (&scheduler, true, "origin cannot assert human authority"),
            (&scheduler, false, "no peer route available"),
            (&owner, true, "no peer route available"),
        ] {
            let message = Inbound {
                network_id: network.clone(),
                origin: origin.node_id()?,
                request_id: "approval-origin-test".into(),
                payload: json!({"type":"execute","human":human,"request":request,"dispatch_head":"f".repeat(64)}),
                broadcast: false,
            };
            assert_eq!(
                node.handle_peer(&message).await.unwrap_err().to_string(),
                expected
            );
        }
        let missing = crypto::digest(b"not present on the receiver");
        let schedule = json!({"type":"schedule","version":1,"target_node":node.identity.node_id()?,
            "conversation_id":"chat","trigger":missing,"basis_heads":[missing]});
        let resolution = json!({"type":"resolve_unknown","target_node":node.identity.node_id()?,"operation_id":request.operation_id,"outcome":"succeeded","evidence":"verified test fixture process stopped"});
        for origin in [&replica, &relay] {
            let message = Inbound {
                network_id: network.clone(),
                origin: origin.node_id()?,
                request_id: "role-test".into(),
                payload: resolution.clone(),
                broadcast: false,
            };
            assert_eq!(
                node.handle_peer(&message).await.unwrap_err().to_string(),
                "Node lacks required role"
            );
            let message = Inbound {
                payload: schedule.clone(),
                ..message
            };
            assert_eq!(
                node.handle_peer(&message).await.unwrap_err().to_string(),
                "origin cannot schedule"
            );
        }
        assert_eq!(
            node.store
                .lock()
                .await
                .operation(&network, &request.operation_id)?
                .unwrap()
                .state,
            OperationState::Unknown
        );
        for origin in [&owner, &scheduler] {
            let message = Inbound {
                network_id: network.clone(),
                origin: origin.node_id()?,
                request_id: "schedule-role-test".into(),
                payload: schedule.clone(),
                broadcast: false,
            };
            assert_eq!(
                node.handle_peer(&message).await.unwrap_err().to_string(),
                "no peer route available"
            );
        }
        let message = Inbound {
            network_id: network.clone(),
            origin: owner.node_id()?,
            request_id: "gateway-test".into(),
            payload: resolution,
            broadcast: false,
        };
        assert_eq!(
            node.handle_peer(&message).await?["operation"]["state"],
            "succeeded"
        );
        let message = Inbound {
            origin: replica.node_id()?,
            payload: json!({"type":"event","hash":event.hash}),
            ..message
        };
        assert_eq!(
            node.handle_peer(&message).await?["event"]["hash"],
            event.hash
        );
        let blob = BlobRef {
            hash: crypto::digest(b"test"),
            size: 4,
        };
        node.store.lock().await.put_blob(&network, &blob, b"test")?;
        let blob_message = Inbound {
            payload: json!({"type":"blob","hash":blob.hash,"offset":0}),
            ..message.clone()
        };
        assert_eq!(node.handle_peer(&blob_message).await?["data"], "74657374");
        Arc::get_mut(&mut node)
            .unwrap()
            .config
            .roles
            .retain(|r| r != &NodeRole::Replica);
        for message in [&message, &blob_message] {
            assert_eq!(
                node.handle_peer(message).await.unwrap_err().to_string(),
                "node capability is disabled"
            );
        }
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_message_retries_record_one_trigger() -> Result<()> {
        let dir = fixture()?;
        configure_manager(&dir)?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let wrong_entry = node.rpc(Access { network_id: network.clone(), human: true, conversation_id: None, manager: None },
            json!({"method":"message","conversation_id":"chat","message_id":"wrong-entry","text":"same goal","entry_node":"another-gateway"})).await.unwrap_err();
        assert!(wrong_entry.to_string().contains("original gateway"));
        assert!(
            node.store
                .lock()
                .await
                .messages(&network, "chat")?
                .is_empty()
        );
        let barrier = Arc::new(tokio::sync::Barrier::new(16));
        let mut calls = Vec::new();
        for _ in 0..16 {
            let node = node.clone();
            let network = network.clone();
            let barrier = barrier.clone();
            calls.push(tokio::spawn(async move {
                    barrier.wait().await;
                    node.rpc(Access { network_id: network, human: true, conversation_id: None,
    manager: None,
    },
                        json!({"method":"message", "conversation_id":"chat", "message_id":"same-message", "text":"same goal"})).await
                }));
        }
        let mut triggers = BTreeSet::new();
        for call in calls {
            let receipt = call.await??;
            assert_eq!(receipt["state"], "queued");
            assert_eq!(receipt["queued"], true);
            assert_eq!(receipt["receipt"]["target_node"], node.identity.node_id()?);
            triggers.insert(receipt["trigger"].as_str().unwrap().to_owned());
        }
        assert_eq!(triggers.len(), 1);
        let count = node.store.lock().await.context(&network, "chat", &[])?.events.iter()
                .filter(|event| matches!(&event.body, EventBody::MessagePosted { message_id, .. } if message_id == "same-message")).count();
        assert_eq!(count, 1);
        assert_eq!(
            node.store
                .lock()
                .await
                .scheduler_jobs(&network, "chat", &node.identity.node_id()?)?
                .len(),
            1
        );
        {
            let mut store = node.store.lock().await;
            let head = store.heads(&network, "chat")?.remove(0);
            let envelope = store.event(&network, &head)?.context("head missing")?;
            let mut event = store.verify_event(&network, &envelope)?;
            event.sequence += 1;
            event.previous = Some(head.clone());
            event.parents = vec![head, "ab".repeat(32)];
            event.body = EventBody::ContextCheckpoint {
                summary: "child arrived before its other parent".into(),
                covered_heads: vec![],
            };
            store.ingest_event(&network, &node.identity.sign_event(event)?)?;
            assert!(!store.context(&network, "chat", &[])?.missing.is_complete());
        }
        let access = Access {
            network_id: network.clone(),
            human: true,
            conversation_id: None,
            manager: None,
        };
        let duplicate = node.rpc(access.clone(), json!({"method":"message","conversation_id":"chat","message_id":"same-message","text":"same goal"})).await?;
        assert!(triggers.contains(duplicate["trigger"].as_str().context("trigger missing")?));
        let changed = node.rpc(access.clone(), json!({"method":"message","conversation_id":"chat","message_id":"same-message","text":"changed goal"})).await.unwrap_err();
        assert!(changed.to_string().contains("different content"));
        let changed_intent = node.rpc(access.clone(), json!({"method":"message","conversation_id":"chat","message_id":"same-message","text":"same goal","report_results":true})).await.unwrap_err();
        assert!(
            changed_intent
                .to_string()
                .contains("cannot change its follow-up intent")
        );
        let fresh = node.rpc(access, json!({"method":"message","conversation_id":"chat","message_id":"new-message","text":"new goal"})).await.unwrap_err();
        assert!(fresh.to_string().contains("context is incomplete"));
        assert_eq!(
            node.store
                .lock()
                .await
                .scheduler_jobs(&network, "chat", &node.identity.node_id()?)?
                .len(),
            1
        );
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn task_retry_requires_explicit_versioned_grant_without_reopening() -> Result<()> {
        use crate::store::followup::FollowupMode;
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        for (task_id, status) in [
            ("active", "open"),
            ("accepted", "accepted"),
            ("cancelled", "cancelled"),
        ] {
            task(&node, &network, task_id, false).await?;
            if status != "open" {
                node.append(
                    &network,
                    "chat",
                    EventBody::TaskChanged {
                        task_id: task_id.into(),
                        status: status.into(),
                        detail: "user ended this task".into(),
                    },
                )
                .await?;
            }
        }
        drop(node);
        let node = Arc::new(Runtime::open(&dir).await?);
        let access = Access {
            network_id: network.clone(),
            human: true,
            conversation_id: None,
            manager: None,
        };
        for _ in 0..2 {
            for task_id in ["active", "accepted", "cancelled"] {
                let rejected = node.rpc(access.clone(), json!({"method":"create_task","conversation_id":"chat",
                    "task_id":task_id,"title":"test","goal":"test","acceptance":"user verifies","auto_followup":true})).await.unwrap_err();
                assert!(rejected.to_string().contains("task follow-up control"));
                task(&node, &network, task_id, false).await?;
                assert_eq!(
                    node.store
                        .lock()
                        .await
                        .followup_policy(&network, "chat", task_id)?
                        .version,
                    0
                );
            }
        }
        let grant = json!({"method":"set_followup","conversation_id":"chat","task_id":"active",
            "mode":"report","expected_version":0,"scheduler_node":node.identity.node_id()?});
        assert_eq!(
            node.rpc(access.clone(), grant.clone()).await?["policy"]["version"],
            1
        );
        assert!(node.rpc(access, grant).await.is_err());
        let store = node.store.lock().await;
        assert_eq!(
            store.followup_policy(&network, "chat", "active")?.mode,
            FollowupMode::Report
        );
        for (task_id, status) in [("accepted", "accepted"), ("cancelled", "cancelled")] {
            assert_eq!(
                store.followup_policy(&network, "chat", task_id)?.mode,
                FollowupMode::Off
            );
            assert_eq!(
                store
                    .tasks(&network, "chat")?
                    .into_iter()
                    .find(|task| task.task_id == task_id)
                    .unwrap()
                    .status,
                status
            );
        }
        let grants = store
            .context(&network, "chat", &[])?
            .events
            .into_iter()
            .filter(|event| {
                matches!(&event.body, EventBody::DecisionRecorded { tool_configuration, .. }
                if tool_configuration["type"] == "task_followup_grant_v1")
            })
            .count();
        assert_eq!(grants, 1);
        drop(store);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn completion_receipt_recovers_history_and_notifies_once_after_reopen() -> Result<()> {
        let dir = fixture()?;
        let node = Runtime::open(&dir).await?;
        let network = conversation(&node).await?;
        let request = queued(
            &node,
            &network,
            "done",
            json!({"type":"send","text":"test"}),
        )
        .await?;
        let result = json!({"result":"verified completion"});
        {
            let mut store = node.store.lock().await;
            store.claim_operation(&network, "done")?;
            store.local_set(
                &network,
                "completion:done",
                &json!({"state":"succeeded","result":result}),
            )?;
            store.finish_operation(&network, "done", OperationState::Succeeded, result)?;
        }
        drop(node);
        let node = Arc::new(Runtime::open(&dir).await?);
        node.flush_completions(&network).await?;
        node.flush_completions(&network).await?;
        let store = node.store.lock().await;
        let events = store.context(&network, "chat", &[])?.events;
        assert_eq!(events.iter().filter(|e|matches!(&e.body,EventBody::ExecutionObserved{operation_id,status,..} if operation_id==&request.operation_id && status=="succeeded")).count(),1);
        assert_eq!(store.pending_notifications(&network, "client")?.len(), 1);
        assert!(store.local_value(&network, "completion:done")?.is_none());
        drop(store);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn failed_claude_spawn_does_not_leave_a_nonexistent_native_session() -> Result<()> {
        let dir = fixture()?;
        let mut cfg = Config::load(&dir)?;
        cfg.claude_bin = dir.join("missing-claude");
        cfg.save(&dir)?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let request = queued(
            &node,
            &network,
            "not-started",
            json!({"type":"send","text":"test"}),
        )
        .await?;
        {
            let mut store = node.store.lock().await;
            let mut session = store
                .session(&network, &request.target_node, &request.session_id)?
                .unwrap();
            session.native_session_id = None;
            store.upsert_session(&network, &session)?;
        }
        node.clone().execute_operation(request.clone()).await?;
        let store = node.store.lock().await;
        assert_eq!(
            store
                .operation(&network, &request.operation_id)?
                .unwrap()
                .state,
            OperationState::Failed
        );
        assert!(
            store
                .session(&network, &request.target_node, &request.session_id)?
                .unwrap()
                .native_session_id
                .is_none()
        );
        drop(store);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn reconciliation_reports_only_the_executors_confirmed_absence() -> Result<()> {
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let local = node.identity.node_id()?;
        let result = node.reconcile(&network, &local, "never-received").await?;
        assert_eq!(result["state"], "not_found");
        assert_eq!(result["target_node"], local);
        assert!(result["operation"].is_null());
        assert!(result["checked_at_ms"].as_i64().is_some_and(|t| t > 0));
        assert!(
            node.reconcile(&network, "offline-node", "never-received")
                .await
                .is_err()
        );
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn retrying_an_undelivered_message_reuses_its_event_and_pending_delivery() -> Result<()> {
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let access = Access {
            network_id: network.clone(),
            human: true,
            conversation_id: None,
            manager: None,
        };
        let mut request = json!({"method":"message","conversation_id":"chat","message_id":"retry-message","text":"test","scheduler_node":"unregistered-node"});
        assert!(node.rpc(access.clone(), request.clone()).await.is_err());
        assert!(
            node.store
                .lock()
                .await
                .messages(&network, "chat")?
                .is_empty()
        );
        let target = LocalIdentity::generate();
        let other = LocalIdentity::generate();
        let owner = test_owner(&node)?;
        node.install_membership(owner.sign_network_fixture(
            2,
            vec![
                node.identity.certificate(node.config.roles.clone())?,
                target.certificate(vec![NodeRole::Scheduler, NodeRole::Replica])?,
                other.certificate(vec![NodeRole::Scheduler, NodeRole::Replica])?,
            ],
        )?)
        .await?;
        request["scheduler_node"] = json!(target.node_id()?);
        let first = node.rpc(access.clone(), request.clone()).await?;
        assert_eq!(first["state"], "pending_delivery");
        assert_eq!(first["accepted"], true);
        assert_eq!(first["queued"], false);
        let retry = node.rpc(access.clone(), request.clone()).await?;
        assert_eq!(retry["receipt"], first["receipt"]);
        assert_eq!(retry["trigger"], first["trigger"]);
        request["scheduler_node"] = json!(other.node_id()?);
        let changed = node.rpc(access, request).await.unwrap_err();
        assert!(changed.to_string().contains("different content or target"));
        let store = node.store.lock().await;
        assert_eq!(store.messages(&network, "chat")?.len(), 1);
        let pending = store.scheduler_outbox(&network)?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].target_node, target.node_id()?);
        assert_eq!(pending[0].trigger, first["trigger"].as_str().unwrap());
        assert!(!pending[0].delivered);
        drop(store);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn completion_requires_the_exact_persisted_turn_even_after_unknown() -> Result<()> {
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let request = queued(
            &node,
            &network,
            "uncertain",
            json!({"type":"send","text":"test"}),
        )
        .await?;
        {
            let mut store = node.store.lock().await;
            let mut session = store
                .session(&network, &request.target_node, &request.session_id)?
                .unwrap();
            session.executor = "codex".into();
            store.upsert_session(&network, &session)?;
            store.claim_operation(&network, &request.operation_id)?;
        }
        node.finish(
            &request,
            OperationState::Unknown,
            json!({"error":"start response lost"}),
        )
        .await?;
        let completion = json!({"method":"turn/completed","params":{"threadId":"native-test","turn":{"id":"unrelated","status":"completed"}}});
        node.observe_engine(completion.clone()).await?;
        assert_eq!(
            node.store
                .lock()
                .await
                .operation(&network, &request.operation_id)?
                .unwrap()
                .state,
            OperationState::Unknown
        );
        node.store.lock().await.local_set(
            &network,
            "turn:uncertain",
            &json!({"turn":{"id":"expected"}}),
        )?;
        node.observe_engine(completion.clone()).await?;
        assert_eq!(
            node.store
                .lock()
                .await
                .operation(&network, &request.operation_id)?
                .unwrap()
                .state,
            OperationState::Unknown
        );
        let mut matching = completion;
        matching["params"]["turn"]["id"] = json!("expected");
        node.observe_engine(matching).await?;
        assert_eq!(
            node.store
                .lock()
                .await
                .operation(&network, &request.operation_id)?
                .unwrap()
                .state,
            OperationState::Succeeded
        );
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn accepted_recovery_rechecks_config_and_rejects_before_any_external_call() -> Result<()>
    {
        let dir = fixture()?;
        let node = Runtime::open(&dir).await?;
        let network = conversation(&node).await?;
        let request = queued(
            &node,
            &network,
            "queued",
            json!({"type":"send","text":"must not run"}),
        )
        .await?;
        drop(node);
        let mut cfg = Config::load(&dir)?;
        cfg.roles.retain(|r| r != &NodeRole::Executor);
        cfg.save(&dir)?;
        let node = Arc::new(Runtime::open(&dir).await?);
        assert!(node.clone().execute_operation(request).await.is_err());
        assert_eq!(
            node.store
                .lock()
                .await
                .operation(&network, "queued")?
                .unwrap()
                .state,
            OperationState::Rejected
        );
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn stale_accepted_send_is_rejected_after_a_control_advances_revision() -> Result<()> {
        let dir = fixture()?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let network = conversation(&node).await?;
        let request = queued(
            &node,
            &network,
            "send",
            json!({"type":"send","text":"must not run"}),
        )
        .await?;
        {
            let mut store = node.store.lock().await;
            let mut control = request.clone();
            control.operation_id = "control".into();
            control.resources.clear();
            control.action = json!({"type":"interrupt","turn_id":"turn"});
            store.accept_control_operation(&network, &control)?;
            store.claim_operation(&network, "control")?;
            store.finish_operation(&network, "control", OperationState::Succeeded, json!({}))?;
        }
        assert!(node.clone().execute_operation(request).await.is_err());
        assert_eq!(
            node.store
                .lock()
                .await
                .operation(&network, "send")?
                .unwrap()
                .state,
            OperationState::Rejected
        );
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[tokio::test]
    async fn maintenance_waits_for_local_outcomes_and_completion_but_not_remote_operations()
    -> Result<()> {
        let dir = fixture()?;
        let node = Runtime::open(&dir).await?;
        let owner = LocalIdentity::generate();
        let remote = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        node.install_membership(owner.sign_network_fixture(
            1,
            vec![
                node.identity.certificate(node.config.roles.clone())?,
                remote.certificate(vec![NodeRole::Executor, NodeRole::Replica])?,
            ],
        )?)
        .await?;
        node.append(
            &network,
            "chat",
            EventBody::ConversationCreated {
                title: "test".into(),
            },
        )
        .await?;
        node.maintenance_idle().await?;
        let request = queued(
            &node,
            &network,
            "local",
            json!({"type":"send","text":"test"}),
        )
        .await?;
        assert!(
            node.maintenance_idle()
                .await
                .unwrap_err()
                .downcast_ref::<Busy>()
                .is_some()
        );
        node.store.lock().await.claim_operation(&network, "local")?;
        assert!(node.maintenance_idle().await.is_err());
        node.store.lock().await.finish_operation(
            &network,
            "local",
            OperationState::Unknown,
            json!({}),
        )?;
        assert!(node.maintenance_idle().await.is_err());
        node.store.lock().await.finish_operation(
            &network,
            "local",
            OperationState::Succeeded,
            json!({}),
        )?;
        node.maintenance_idle().await?;
        node.store
            .lock()
            .await
            .local_set(&network, "completion:local", &json!({}))?;
        assert!(node.maintenance_idle().await.is_err());
        {
            let mut store = node.store.lock().await;
            store.local_delete(&network, "completion:local")?;
            let mut remote_request = request;
            remote_request.operation_id = "remote".into();
            remote_request.target_node = remote.node_id()?;
            remote_request.resources.clear();
            remote_request.task_id = Some("remote-fixture".into());
            remote_request.binding = None;
            store.bind_test_operation(&node.identity, &mut remote_request)?;
            store.accept_operation(&network, &remote_request)?;
        }
        node.maintenance_idle().await?;
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
#[derive(Clone)]
pub struct Update {
    pub network_id: String,
    pub payload: Value,
}
pub struct Runtime {
    pub config: Config,
    pub identity: LocalIdentity,
    pub store: Arc<Mutex<Store>>,
    pub identities: Arc<IdentityStore>,
    pub network: Arc<NetworkHub>,
    pub hub: Arc<PeerHub>,
    pub events: broadcast::Sender<Update>,
    pub agent_tokens: Mutex<HashMap<String, Access>>,
    manager_controls: Mutex<HashMap<String, Arc<manager_control::ActiveManager>>>,
    schedule_workers: Mutex<BTreeSet<(String, String)>>,
    schedule_wakeup: tokio::sync::Notify,
    observation: std::sync::Mutex<observation::ObservationState>,
    pub tool_url: std::sync::OnceLock<String>,
    pub maintenance: Arc<Gate>,
    completion_lock: Mutex<()>,
    progress_sequences: std::sync::Mutex<HashMap<(String, String), u64>>,
    pub engine: Engine,
    _codex_process: Option<tokio::process::Child>,
    _process_lock: std::fs::File,
    peer_rx: Mutex<Option<mpsc::Receiver<Inbound>>>,
    network_rx: Mutex<Option<mpsc::Receiver<IncomingSocket>>>,
    engine_rx: Mutex<Option<mpsc::UnboundedReceiver<Value>>>,
}
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    let s = v[key]
        .as_str()
        .with_context(|| format!("{key} is required"))?;
    ensure!(!s.trim().is_empty() && s.len() <= 65536, "invalid {key}");
    Ok(s)
}
fn id(v: &Value, key: &str) -> Result<String> {
    let s = field(v, key)?;
    ensure!(
        s.len() <= 128
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c)),
        "invalid {key}"
    );
    Ok(s.into())
}
fn heads(v: &Value) -> Result<Vec<String>> {
    Ok(serde_json::from_value(
        v.get("basis_heads").cloned().unwrap_or(json!([])),
    )?)
}
fn require_task_write(
    store: &Store,
    network: &str,
    scope: &str,
    task_id: &str,
    human: bool,
) -> Result<()> {
    let task = store
        .tasks(network, scope)?
        .into_iter()
        .find(|t| t.task_id == task_id)
        .context("task not found")?;
    ensure!(
        human || !matches!(task.status.as_str(), "accepted" | "cancelled"),
        "only the user may reopen accepted or cancelled tasks"
    );
    Ok(())
}

impl Runtime {
    pub async fn open(dir: &Path) -> Result<Self> {
        let epoch: Value = serde_json::from_slice(&std::fs::read(dir.join("data-epoch.json"))?)?;
        ensure!(
            epoch == json!({"data_epoch":2}),
            "this Node requires an initialized data epoch 2 directory"
        );
        let mut config = Config::load(dir)?;
        let process_lock = config::lock_data_dir(dir)?;
        let identity = config::identity(dir)?;
        let mut store = Store::open(dir.join("node.sqlite3"))?;
        let identities = Arc::new(IdentityStore::open(&dir.join("identity.sqlite3"))?);
        let networks = store.networks()?;
        ensure!(
            networks.len() <= 1,
            "one installation can join only one personal network"
        );
        let stored = networks
            .first()
            .map(|network| store.trust_bundle(network))
            .transpose()?
            .flatten();
        let local = if identities.has_network()? {
            Some(identities.public_state()?)
        } else {
            None
        };
        let state = match (local, stored) {
            (Some(a), Some(b)) => Some(if a.epoch() >= b.epoch() {
                b.merge(&a)?
            } else {
                a.merge(&b)?
            }),
            (a, b) => a.or(b),
        };
        if let Some(state) = state {
            let view = state.project()?;
            store.install_trust_bundle(&view.network_id, &state)?;
            identities.install_state(state)?;
            if view.authorize_node(&identity.node_id()?, None).is_ok() {
                store.save_local_identity(&view.network_id, &identity)?;
            }
        }
        let store = Arc::new(Mutex::new(store));
        let (hub, peer_rx) = PeerHub::new(identity.clone(), store.clone(), &config.roles);
        let (network, network_rx) = NetworkHub::new(identity.clone(), config.network.clone());
        let (events, _) = broadcast::channel(256);
        let (tx, rx) = mpsc::unbounded_channel();
        let managed_socket = config.codex_socket.is_none();
        if managed_socket {
            let socket_dir =
                PathBuf::from("/tmp").join(format!("vibeface-{}", &identity.node_id()?[..32]));
            match std::fs::create_dir(&socket_dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                let metadata = std::fs::symlink_metadata(&socket_dir)?;
                ensure!(
                    metadata.is_dir()
                        && !metadata.file_type().is_symlink()
                        && metadata.uid() == std::fs::metadata(dir)?.uid(),
                    "private socket directory has unexpected ownership"
                );
                std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700))?;
            }
            config.codex_socket = Some(socket_dir.join("codex.sock"));
        }
        let mut codex_process = None;
        if managed_socket && config.roles.contains(&NodeRole::Executor) {
            let socket = config.codex_socket.as_ref().unwrap();
            let existing = tokio::net::UnixStream::connect(socket).await.is_ok();
            if !existing {
                if socket.exists() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::FileTypeExt;
                        ensure!(
                            std::fs::symlink_metadata(socket)?.file_type().is_socket(),
                            "private Codex socket path is occupied by another file"
                        );
                    }
                    std::fs::remove_file(socket)?;
                }
                match tokio::process::Command::new(&config.codex_bin)
                    .args(["app-server", "--listen"])
                    .arg(format!("unix://{}", socket.display()))
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                {
                    Ok(child) => codex_process = Some(child),
                    Err(error) => {
                        tracing::warn!(error=%error,"Codex not available; configure an executor before sending tasks")
                    }
                }
            }
        }
        let engine = Engine::new(
            ExecutorConfig {
                codex_socket: config.codex_socket.clone(),
                claude_bin: config.claude_bin.clone(),
                cursor_bin: config.cursor_bin.clone(),
                timeout_secs: config.timeout_secs,
            },
            tx,
        );
        Ok(Self {
            config,
            identity,
            store,
            identities,
            network,
            hub,
            events,
            agent_tokens: Mutex::new(HashMap::new()),
            manager_controls: Mutex::new(HashMap::new()),
            schedule_workers: Mutex::new(BTreeSet::new()),
            schedule_wakeup: tokio::sync::Notify::new(),
            observation: std::sync::Mutex::new(Default::default()),
            tool_url: std::sync::OnceLock::new(),
            maintenance: Arc::new(Gate::new(dir.join("update-pending.json").try_exists()?)),
            completion_lock: Mutex::new(()),
            progress_sequences: std::sync::Mutex::new(HashMap::new()),
            engine,
            _codex_process: codex_process,
            _process_lock: process_lock,
            peer_rx: Mutex::new(Some(peer_rx)),
            network_rx: Mutex::new(Some(network_rx)),
            engine_rx: Mutex::new(Some(rx)),
        })
    }
    pub async fn install_membership(&self, incoming: NetworkState) -> Result<AuthorizationView> {
        let _work = self.maintenance.enter()?;
        let mut store = self.store.lock().await;
        let mut state = incoming;
        let networks = store.networks()?;
        ensure!(
            networks
                .iter()
                .all(|network| network == &state.genesis.network_id),
            "installation belongs to a different personal network"
        );
        if self.identities.has_network()? {
            let previous = self.identities.public_state()?;
            state = previous.merge(&state)?;
        }
        if let Some(previous) = store.trust_bundle(&state.genesis.network_id)? {
            state = previous.merge(&state)?;
        }
        let view = state.project()?;
        self.identities.install_state(state.clone())?;
        store.install_trust_bundle(&view.network_id, &state)?;
        if view.authorize_node(&self.identity.node_id()?, None).is_ok() {
            store.save_local_identity(&view.network_id, &self.identity)?;
        }
        Ok(view)
    }

    pub async fn has_membership(&self) -> Result<bool> {
        let in_identity = self.identities.has_network()?;
        let in_store = !self.store.lock().await.networks()?.is_empty();
        if !in_identity && !in_store {
            return Ok(false);
        }
        self.membership_state().await?;
        Ok(true)
    }

    pub async fn membership_state(&self) -> Result<NetworkState> {
        let local = if self.identities.has_network()? {
            Some(self.identities.public_state()?)
        } else {
            None
        };
        let mut store = self.store.lock().await;
        let networks = store.networks()?;
        ensure!(
            networks.len() <= 1,
            "one installation can join only one personal network"
        );
        let stored = networks
            .first()
            .map(|network| store.trust_bundle(network))
            .transpose()?
            .flatten();
        let merged = match (&local, &stored) {
            (Some(a), Some(b)) => {
                if a.epoch() >= b.epoch() {
                    b.merge(a)?
                } else {
                    a.merge(b)?
                }
            }
            (Some(state), None) | (None, Some(state)) => state.clone(),
            (None, None) => bail!("personal network is not installed"),
        };
        if stored.as_ref() != Some(&merged) || local.as_ref() != Some(&merged) {
            let _work = self.maintenance.enter()?;
            self.identities.install_state(merged.clone())?;
            store.install_trust_bundle(&merged.genesis.network_id, &merged)?;
            let view = merged.project()?;
            if view.authorize_node(&self.identity.node_id()?, None).is_ok() {
                store.save_local_identity(&view.network_id, &self.identity)?;
            }
        }
        Ok(merged)
    }
    pub fn info(&self) -> Value {
        json!({"node_id":self.identity.node_id().ok(),"version":env!("CARGO_PKG_VERSION"),"roles":self.config.roles,"manager_available":self.config.manager.is_some()})
    }
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        self.start_schedule_delivery();
        self.start_observation_worker();
        let mut receiver = self
            .peer_rx
            .lock()
            .await
            .take()
            .context("runtime already started")?;
        let node = self.clone();
        tokio::spawn(async move {
            while let Some(message) = receiver.recv().await {
                let n = node.clone();
                tokio::spawn(async move {
                    let response = n.handle_peer(&message).await;
                    if !message.broadcast {
                        let value = response.unwrap_or_else(|e| json!({"error":format!("{e:#}")}));
                        let _ = n.hub.respond(&message, value).await;
                    }
                });
            }
        });
        let mut receiver = self
            .engine_rx
            .lock()
            .await
            .take()
            .context("runtime already started")?;
        let node = self.clone();
        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                if let Err(e) = node.observe_engine(event).await {
                    tracing::warn!(error=%e,"could not persist executor event");
                }
            }
        });
        let mut incoming = self
            .network_rx
            .lock()
            .await
            .take()
            .context("runtime already started")?;
        let node = self.clone();
        tokio::spawn(async move {
            while let Some(incoming) = incoming.recv().await {
                let node = node.clone();
                tokio::spawn(async move {
                    let result = match incoming.service {
                        Service::Peer => node.hub.clone().accept_transport(incoming.socket).await,
                        Service::Client => {
                            crate::client::serve(node.clone(), incoming.socket).await
                        }
                    };
                    if let Err(error) = result {
                        tracing::debug!(%error,"authenticated channel closed");
                    }
                });
            }
        });
        let transport = self.network.clone();
        let listen = self.config.listen;
        tokio::spawn(async move {
            if let Err(error) = transport.run(listen).await {
                tracing::warn!(%error,"network discovery stopped");
            }
        });
        let node = self.clone();
        tokio::spawn(async move {
            let connecting = Arc::new(Mutex::new(BTreeSet::new()));
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            loop {
                tick.tick().await;
                let Ok(state) = node.identities.public_state() else {
                    continue;
                };
                let Ok(view) = state.project() else { continue };
                let online = node.hub.online(&view.network_id).await;
                for member in view.members.into_iter().filter(|member| {
                    member.kind == IdentityKind::Node && !view.revoked.contains(&member.subject_id)
                }) {
                    if member.subject_id == node.identity.node_id().unwrap_or_default()
                        || online.contains(&member.subject_id)
                    {
                        continue;
                    }
                    let key = (view.network_id.clone(), member.subject_id.clone());
                    if !connecting.lock().await.insert(key.clone()) {
                        continue;
                    }
                    let node = node.clone();
                    let connecting = connecting.clone();
                    tokio::spawn(async move {
                        if let Err(error) = node
                            .hub
                            .clone()
                            .connect_target(key.0.clone(), key.1.clone(), node.network.clone())
                            .await
                        {
                            tracing::debug!(%error,"personal peer reconnect pending");
                        }
                        connecting.lock().await.remove(&key);
                    });
                }
            }
        });
        let node = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            loop {
                tick.tick().await;
                let networks = node.store.lock().await.networks().unwrap_or_default();
                for network in networks {
                    let _ = node.sync_all(&network).await;
                    let _ = node.flush_completions(&network).await;
                }
            }
        });
        if self.maintenance.is_open() {
            self.recover_operations().await?;
        } else {
            let node = self.clone();
            tokio::spawn(async move {
                let _work = node.maintenance.enter_when_open().await;
                if let Err(error) = node.recover_operations().await {
                    tracing::warn!(%error, "deferred operation recovery failed");
                }
            });
        }
        Ok(())
    }
    async fn recover_operations(self: &Arc<Self>) -> Result<()> {
        let _work = self.maintenance.enter()?;
        let networks = self.store.lock().await.networks()?;
        for network in networks {
            self.flush_completions(&network).await?;
            let operations = self.store.lock().await.operations(&network)?;
            for operation in operations {
                if operation.state == OperationState::Unknown {
                    self.finish(&operation.request,OperationState::Unknown,if operation.result.is_null(){json!({"error":"Node restarted while the external outcome was unresolved; reconcile before retrying"})}else{operation.result}).await?;
                    continue;
                }
                if operation.state != OperationState::Accepted {
                    continue;
                }
                let node = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = node.execute_operation(operation.request).await {
                        tracing::warn!(error=%e,"accepted operation recovery failed");
                    }
                });
            }
        }
        Ok(())
    }
    pub(crate) async fn maintenance_idle(&self) -> Result<()> {
        let local = self.identity.node_id()?;
        let store = self.store.lock().await;
        for network in store.networks()? {
            for operation in store.operations(&network)? {
                if operation.request.target_node != local {
                    continue;
                }
                ensure!(
                    operation.state.terminal(),
                    Busy(format!(
                        "local operation {} is {:?}",
                        operation.request.operation_id, operation.state
                    ))
                );
                ensure!(
                    store
                        .local_value(
                            &network,
                            &format!("completion:{}", operation.request.operation_id)
                        )?
                        .is_none(),
                    Busy("execution completion is pending".into())
                );
            }
            crate::scheduler::maintenance_idle(&store, &network)?;
        }
        Ok(())
    }
    pub async fn authorize_bearer(&self, token: &str) -> Option<Access> {
        self.agent_tokens.lock().await.get(token).cloned()
    }
    pub async fn append(&self, network: &str, scope: &str, body: EventBody) -> Result<SignedEvent> {
        self.append_blobs(network, scope, body, vec![]).await
    }
    pub async fn append_blobs(
        &self,
        network: &str,
        scope: &str,
        body: EventBody,
        blobs: Vec<BlobRef>,
    ) -> Result<SignedEvent> {
        let current = self.membership_state().await?;
        ensure!(
            current.genesis.network_id == network,
            "wrong personal network"
        );
        let event = self.store.lock().await.append_event(
            &self.identity,
            network,
            scope,
            body,
            blobs,
            now_ms(),
        )?;
        self.publish_changed(network, scope, &event.hash);
        Ok(event)
    }
    fn publish_changed(&self, network: &str, scope: &str, event_hash: &str) {
        let _ = self.events.send(Update {
            network_id: network.into(),
            payload: json!({"type":"changed","conversation_id":scope,"event_hash":event_hash}),
        });
    }
    pub async fn raise_notification(
        &self,
        network: &str,
        scope: &str,
        kind: &str,
        text: &str,
    ) -> Result<()> {
        self.append(
            network,
            scope,
            EventBody::NotificationRaised {
                notification_id: uuid::Uuid::new_v4().to_string(),
                kind: kind.into(),
                text: text.chars().take(8192).collect(),
            },
        )
        .await?;
        Ok(())
    }
    async fn require_scope(&self, access: &Access, scope: &str) -> Result<()> {
        let state = self.membership_state().await?;
        ensure!(
            state.genesis.network_id == access.network_id,
            "wrong personal network"
        );
        if let Some(allowed) = &access.conversation_id {
            ensure!(allowed == scope, "outside this tool's conversation");
        }
        let store = self.store.lock().await;
        store.authorize_node(&access.network_id, &self.identity.node_id()?, None)?;
        ensure!(
            store
                .conversations(&access.network_id)?
                .iter()
                .any(|c| c.conversation_id == scope),
            "conversation not found"
        );
        Ok(())
    }
    async fn require_role(&self, network: &str, role: NodeRole) -> Result<()> {
        let state = self.membership_state().await?;
        ensure!(
            state.genesis.network_id == network,
            "wrong personal network"
        );
        ensure!(
            self.config.roles.contains(&role),
            "node capability is disabled"
        );
        self.store
            .lock()
            .await
            .authorize_node(network, &self.identity.node_id()?, Some(role))?;
        Ok(())
    }
    async fn peer_request(&self, network: &str, target: &str, payload: Value) -> Result<Value> {
        let reply = self.hub.request(network, target, payload).await?;
        if let Some(error) = reply["error"].as_str() {
            bail!("{error}");
        }
        Ok(reply)
    }
    pub async fn sync_all(&self, network: &str) -> Result<()> {
        let _work = self.maintenance.enter()?;
        let peers = self.hub.online(network).await;
        let local = self.identity.node_id()?;
        futures_util::stream::iter(peers.into_iter().filter(|p| p != &local))
            .for_each_concurrent(4, |target| async move {
                if let Err(error) = self.sync_from(network, &target).await {
                    tracing::debug!(error=%error,"history sync pending");
                }
            })
            .await;
        Ok(())
    }
    async fn read_peer(&self, network: &str, target: &str, payload: Value) -> Result<Value> {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.peer_request(network, target, payload),
        )
        .await
        .context("history peer is currently unreachable")?
    }
    async fn sync_from(&self, network: &str, target: &str) -> Result<()> {
        self.require_role(network, NodeRole::Replica).await?;
        self.store
            .lock()
            .await
            .authorize_node(network, target, Some(NodeRole::Replica))?;
        let inventory = self
            .read_peer(network, target, json!({"type":"inventory"}))
            .await?;
        let state: NetworkState = serde_json::from_value(inventory["membership"].clone())?;
        ensure!(
            state.genesis.network_id == network,
            "wrong membership network"
        );
        self.install_membership(state).await?;
        let scopes = inventory["scopes"]
            .as_array()
            .context("invalid inventory")?;
        for scope in scopes {
            let scope_id = field(scope, "scope_id")?;
            let wanted: Vec<String> = serde_json::from_value(scope["heads"].clone())?;
            self.fetch_closure(network, target, &wanted).await?;
            let missing = self
                .store
                .lock()
                .await
                .context(network, scope_id, &wanted)?
                .missing;
            if missing.is_complete() {
                let _ = self.events.send(Update {
                    network_id: network.into(),
                    payload: json!({"type":"changed","conversation_id":scope_id}),
                });
            }
        }
        Ok(())
    }
    async fn fetch_closure(&self, network: &str, target: &str, wanted: &[String]) -> Result<()> {
        self.membership_state().await?;
        self.store
            .lock()
            .await
            .authorize_node(network, target, Some(NodeRole::Replica))?;
        let mut pending: Vec<String> = wanted.to_vec();
        let mut seen = BTreeSet::new();
        while let Some(hash) = pending.pop() {
            ensure!(seen.len() < 100_000, "history fetch exceeds bounded batch");
            if !seen.insert(hash.clone()) {
                continue;
            }
            let local = self.store.lock().await.event(network, &hash)?;
            let envelope = match local {
                Some(e) => e,
                None => serde_json::from_value(
                    self.read_peer(network, target, json!({"type":"event","hash":hash}))
                        .await?["event"]
                        .clone(),
                )?,
            };
            let event = self.store.lock().await.verify_event(network, &envelope)?;
            pending.extend(event.parents.clone());
            pending.extend(event.previous.clone());
            for blob in &event.blobs {
                if !self.store.lock().await.has_blob(network, blob)? {
                    let mut bytes = Vec::with_capacity(blob.size as usize);
                    while bytes.len() < (blob.size as usize) {
                        let value=self.read_peer(network,target,json!({"type":"blob","hash":blob.hash,"offset":bytes.len(),"limit":65536})).await?;
                        let encoded = value["data"].as_str().context("blob data is required")?;
                        ensure!(encoded.len() <= 131072, "blob chunk too large");
                        let chunk = hex::decode(encoded)?;
                        ensure!(
                            !chunk.is_empty()
                                && chunk.len() <= 65536
                                && bytes.len() + chunk.len() <= blob.size as usize,
                            "invalid blob chunk"
                        );
                        bytes.extend(chunk);
                    }
                    self.store.lock().await.put_blob(network, blob, &bytes)?;
                }
            }
            let mut store = self.store.lock().await;
            store.authorize_node(network, target, Some(NodeRole::Replica))?;
            store.ingest_event(network, &envelope)?;
        }
        Ok(())
    }
    fn capabilities(&self) -> Value {
        let manager =
            self.config
                .manager
                .as_ref()
                .and_then(|settings| match settings.provider.as_str() {
                    "cursor_sdk" => Some(crate::manager::ManagerKind::CursorSdk),
                    "codex_sdk" => Some(crate::manager::ManagerKind::CodexSdk),
                    _ => None,
                });
        json!({"roots":self.config.roots,"executors":["codex","claude_code","cursor_agent"],
            "executor_capabilities":{
                "codex":{"actions":ExecutorKind::Codex.capabilities(),"verified":false},
                "claude_code":{"actions":ExecutorKind::ClaudeCode.capabilities(),"verified":false},
                "cursor_agent":{"actions":ExecutorKind::CursorAgent.capabilities(),"verified":false}},
            "manager":self.config.manager.as_ref().map(|m|m.provider.clone()),
            "manager_capabilities":manager.map(|kind|kind.capabilities()),"reliable_schedule":{"version":1}})
    }
    pub async fn rpc(self: &Arc<Self>, access: Access, input: Value) -> Result<Value> {
        let write = !matches!(
            input["method"].as_str(),
            Some(
                "nodes"
                    | "conversation"
                    | "conversations"
                    | "session"
                    | "operation"
                    | "notifications"
                    | "export"
            )
        );
        let task = input["task_id"]
            .as_str()
            .or_else(|| input["request"]["task_id"].as_str());
        let _lease = self.manager_tool_lease(&access, write, task).await?;
        self.rpc_inner(access, input).await
    }
    async fn rpc_inner(self: &Arc<Self>, access: Access, input: Value) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        let current = self.membership_state().await?;
        let network = &access.network_id;
        ensure!(
            current.genesis.network_id == *network,
            "wrong personal network"
        );
        self.store
            .lock()
            .await
            .authorize_node(network, &self.identity.node_id()?, None)?;
        let method = field(&input, "method")?;
        if !access.human {
            ensure!(
                matches!(method, "nodes" | "create_task" | "update_task" | "operate"),
                "method unavailable to Agent tokens"
            );
        }
        let scope = input["conversation_id"].as_str().map(String::from);
        if let Some(scope) = &scope
            && method != "create_conversation"
        {
            self.require_scope(&access, scope).await?;
        }
        match method {
            "nodes" => {
                let bundle = self
                    .store
                    .lock()
                    .await
                    .trust_bundle(network)?
                    .context("unknown network")?;
                let online = self.hub.online(network).await;
                let local = self.identity.node_id()?;
                let mut nodes = vec![];
                let view = bundle.project()?;
                let revoked_ids = view.revoked.clone();
                for member in view
                    .members
                    .into_iter()
                    .filter(|member| member.kind == IdentityKind::Node)
                {
                    let mut cert = member.certificate()?;
                    cert.revoked = view.revoked.contains(&cert.node_id);
                    let here = cert.node_id == local;
                    let live = here || online.contains(&cert.node_id);
                    let capabilities = if here {
                        self.capabilities()
                    } else if live && !cert.revoked {
                        tokio::time::timeout(
                            Duration::from_secs(3),
                            self.peer_request(
                                network,
                                &cert.node_id,
                                json!({"type":"capabilities"}),
                            ),
                        )
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    };
                    nodes.push(json!({"node_id":cert.node_id,"roles":cert.roles,"revoked":cert.revoked,"local":here,"online":live&&!cert.revoked,"capabilities":capabilities}));
                }
                let history = bundle.historical_nodes()?;
                for revoked in revoked_ids {
                    if nodes.iter().any(|node| node["node_id"] == revoked) {
                        continue;
                    }
                    if history.iter().any(|node| node.subject_id == revoked) {
                        nodes.push(json!({"node_id":revoked,"roles":[],"revoked":true,"local":false,"online":false,"capabilities":Value::Null}));
                    }
                }
                Ok(json!({"nodes":nodes}))
            }
            "conversations" => {
                Ok(json!({"conversations":self.store.lock().await.conversations(network)?}))
            }
            "create_conversation" => {
                ensure!(access.human, "only the user creates conversations");
                let scope = id(&input, "conversation_id")?;
                let title = field(&input, "title")?;
                ensure!(title.len() <= 512, "title too long");
                {
                    let store = self.store.lock().await;
                    if let Some(old) = store
                        .conversations(network)?
                        .into_iter()
                        .find(|c| c.conversation_id == scope)
                    {
                        ensure!(old.title == title, "conversation ID already exists");
                        return Ok(json!(old));
                    }
                }
                self.append(
                    network,
                    &scope,
                    EventBody::ConversationCreated {
                        title: title.into(),
                    },
                )
                .await?;
                Ok(
                    json!({"conversation_id":scope,"title":title,"heads":self.store.lock().await.heads(network,&scope)?}),
                )
            }
            "conversation" => self.conversation_snapshot(&access, &input).await,
            "message" => self.accept_chat_message(&access, &input).await,
            "set_followup" => {
                let scope = scope.context("conversation_id is required")?;
                let result = self.set_followup(&access, &scope, &input).await?;
                self.schedule_wakeup.notify_one();
                Ok(result)
            }
            "read_artifact" => self.read_artifact(&access, &input).await,
            "task_control" => self.task_control(&access, &input).await,
            "cancel_manager" => self.cancel_manager(&access, &input).await,
            "resolve_manager_unknown" => self.resolve_manager_invocation(&access, &input).await,
            "create_task" => {
                ensure!(
                    input["auto_followup"].as_bool() != Some(true),
                    "use the task follow-up control to grant permission"
                );
                let scope = scope.context("conversation_id is required")?;
                let body = EventBody::TaskCreated {
                    task_id: id(&input, "task_id")?,
                    title: field(&input, "title")?.into(),
                    goal: field(&input, "goal")?.into(),
                    acceptance: field(&input, "acceptance")?.into(),
                };
                let origin = access
                    .manager
                    .as_ref()
                    .map(|m| (m.trigger.as_str(), m.attempt_id.as_str()));
                let event = {
                    let mut store = self.store.lock().await;
                    Self::check_manager_permission(&store, &access, true, None)?;
                    store.create_task_with_origin(
                        &self.identity,
                        network,
                        &scope,
                        body,
                        origin,
                        now_ms(),
                    )?
                };
                self.publish_changed(network, &scope, &event.hash);
                Ok(json!(event))
            }
            "update_task" => {
                let scope = scope.context("conversation_id is required")?;
                let task_id = id(&input, "task_id")?;
                let status = field(&input, "status")?;
                ensure!(
                    matches!(
                        status,
                        "open" | "working" | "blocked" | "needs_review" | "accepted" | "cancelled"
                    ),
                    "invalid task status"
                );
                ensure!(
                    access.human || !matches!(status, "accepted" | "cancelled"),
                    "only the user accepts or cancels tasks"
                );
                let event = {
                    let mut store = self.store.lock().await;
                    Self::check_manager_permission(&store, &access, true, Some(&task_id))?;
                    require_task_write(&store, network, &scope, &task_id, access.human)?;
                    store.append_event(
                        &self.identity,
                        network,
                        &scope,
                        EventBody::TaskChanged {
                            task_id,
                            status: status.into(),
                            detail: input["detail"]
                                .as_str()
                                .unwrap_or("")
                                .chars()
                                .take(65536)
                                .collect(),
                        },
                        vec![],
                        now_ms(),
                    )?
                };
                self.publish_changed(network, &scope, &event.hash);
                Ok(json!(event))
            }
            "rebind_execution" => self.rebind_execution(&access, &input).await,
            "operate" => {
                let request: OperationRequest = serde_json::from_value(input["request"].clone())?;
                self.require_scope(&access, &request.conversation_id)
                    .await?;
                self.dispatch(access, request).await
            }
            "session" => {
                self.session_fact(
                    network,
                    field(&input, "target_node")?,
                    field(&input, "session_id")?,
                )
                .await
            }
            "operation" => {
                let target = field(&input, "target_node")?;
                let op = field(&input, "operation_id")?;
                if target == self.identity.node_id()? {
                    Ok(json!({"operation":self.store.lock().await.operation(network,op)?}))
                } else {
                    self.peer_request(
                        network,
                        target,
                        json!({"type":"operation","operation_id":op}),
                    )
                    .await
                }
            }
            "notifications" => Ok(
                json!({"notifications":self.store.lock().await.pending_notifications(network,field(&input,"client_id")?)?}),
            ),
            "ack" => {
                ensure!(access.human, "only clients acknowledge notifications");
                let notification = id(&input, "notification_id")?;
                let client = id(&input, "client_id")?;
                let pending = self
                    .store
                    .lock()
                    .await
                    .pending_notifications(network, &client)?;
                if let Some(n) = pending
                    .into_iter()
                    .find(|n| n.notification_id == notification)
                {
                    self.append(
                        network,
                        &n.conversation_id,
                        EventBody::NotificationAcknowledged {
                            notification_id: notification,
                            client_id: client,
                        },
                    )
                    .await?;
                }
                Ok(json!({"acknowledged":true}))
            }
            "export" => {
                let scope = scope.context("conversation_id is required")?;
                Ok(json!(
                    self.store.lock().await.export_snapshot(network, &scope)?
                ))
            }
            "reconcile" => {
                ensure!(access.human, "reconciliation requires the user");
                self.reconcile(
                    network,
                    field(&input, "target_node")?,
                    field(&input, "operation_id")?,
                )
                .await
            }
            "resolve_unknown" => {
                ensure!(access.human, "resolution requires the user");
                self.resolve_unknown(network, &input).await
            }
            _ => bail!("unknown method"),
        }
    }
    pub async fn tool(
        self: &Arc<Self>,
        access: Access,
        name: &str,
        args: Value,
        _call_id: &str,
    ) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        ensure!(!access.human, "tool endpoint requires a scoped Agent token");
        let scope = access
            .conversation_id
            .clone()
            .context("missing tool scope")?;
        self.require_scope(&access, &scope).await?;
        let write = !matches!(name, "context" | "nodes" | "execution_status");
        let _lease = self
            .manager_tool_lease(&access, write, args["task_id"].as_str())
            .await?;
        match name {
            "nodes" => self.rpc_inner(access, json!({"method":"nodes"})).await,
            "context" => {
                let context = self
                    .store
                    .lock()
                    .await
                    .context(&access.network_id, &scope, &[])?;
                let offset = args["offset"].as_u64().unwrap_or(0).min(usize::MAX as u64) as usize;
                let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
                if let Some(hash) = args["blob_hash"].as_str() {
                    ensure!(
                        context
                            .events
                            .iter()
                            .filter(|e| e.scope_id == scope)
                            .flat_map(|e| &e.blobs)
                            .any(|b| b.hash == hash),
                        "blob outside this conversation"
                    );
                    let bytes = self
                        .store
                        .lock()
                        .await
                        .get_blob(&access.network_id, hash)?
                        .context("blob not available")?;
                    let end = offset.saturating_add(limit * 1024).min(bytes.len());
                    ensure!(offset <= end, "invalid blob offset");
                    return Ok(
                        json!({"hash":hash,"offset":offset,"data_hex":hex::encode(&bytes[offset..end]),"next_offset":(end<bytes.len()).then_some(end),"size":bytes.len()}),
                    );
                }
                let events: Vec<_> = context
                    .events
                    .into_iter()
                    .filter(|e| e.scope_id == scope)
                    .collect();
                let end = offset.saturating_add(limit).min(events.len());
                ensure!(offset <= end, "invalid event offset");
                Ok(
                    json!({"heads":context.heads,"missing":context.missing,"events":&events[offset..end],"next_offset":(end<events.len()).then_some(end)}),
                )
            }
            "create_task" | "update_task" => {
                let mut request = args;
                ensure!(request.is_object(), "tool arguments must be object");
                request["method"] = json!(name);
                request["conversation_id"] = json!(scope);
                request.as_object_mut().unwrap().remove("auto_followup");
                self.rpc_inner(access, request).await
            }
            "open_execution" | "send_execution" => {
                let target = field(&args, "target_node")?;
                let session = id(&args, "session_id")?;
                let operation = id(&args, "operation_id")?;
                let action = if name == "open_execution" {
                    json!({"type":"open","executor":field(&args,"executor")?,"cwd":field(&args,"cwd")?})
                } else {
                    json!({"type":"send","text":field(&args,"text")?})
                };
                let context = self
                    .store
                    .lock()
                    .await
                    .context(&access.network_id, &scope, &[])?
                    .events;
                let previous = context.into_iter().find_map(|event| match event.body {
                    EventBody::DispatchRecorded {
                        operation_id,
                        action,
                        ..
                    } if event.scope_id == scope && operation_id == operation => Some(action),
                    _ => None,
                });
                if let Some(previous) = previous {
                    let request: OperationRequest = serde_json::from_value(previous)?;
                    let requested_task = args["task_id"].as_str();
                    ensure!(
                        request.target_node == target
                            && request.session_id == session
                            && request.action == action
                            && requested_task
                                .is_none_or(|task| request.task_id.as_deref() == Some(task))
                            && request.expected_revision
                                == args["expected_revision"].as_u64().unwrap_or(0),
                        "operation ID reused with different tool arguments"
                    );
                    return self.dispatch(access, request).await;
                }
                let basis = self.store.lock().await.heads(&access.network_id, &scope)?;
                let request = OperationRequest {
                    operation_id: operation,
                    network_id: access.network_id.clone(),
                    conversation_id: scope,
                    task_id: args["task_id"].as_str().map(String::from),
                    target_node: target.into(),
                    session_id: session,
                    expected_revision: args["expected_revision"].as_u64().unwrap_or(0),
                    basis_heads: basis,
                    action,
                    resources: vec![],
                    binding: None,
                    manager_authority: None,
                };
                self.dispatch(access, request).await
            }
            "execution_status" => {
                self.require_execution_scope(
                    &access,
                    &scope,
                    field(&args, "target_node")?,
                    field(&args, "session_id")?,
                )
                .await?;
                self.session_fact(
                    &access.network_id,
                    field(&args, "target_node")?,
                    field(&args, "session_id")?,
                )
                .await
            }
            _ => bail!("unknown or unavailable tool"),
        }
    }
    async fn cache_context_bindings(&self, network: &str, scope: &str) -> Result<()> {
        let context = self.store.lock().await.context(network, scope, &[])?;
        let mut proofs = context
            .events
            .iter()
            .filter_map(|event| {
                if let EventBody::DispatchRecorded { action, .. } = &event.body {
                    serde_json::from_value::<OperationRequest>(action.clone())
                        .ok()
                        .and_then(|request| request.binding)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        proofs.sort_by_key(|proof| proof.binding.generation);
        let mut store = self.store.lock().await;
        for proof in proofs {
            store.install_historical_binding(&proof)?;
        }
        Ok(())
    }

    async fn confirm_binding(
        self: &Arc<Self>,
        binding: crate::binding::TaskBinding,
    ) -> Result<crate::binding::SignedBinding> {
        let proof = if binding.authority_node == self.identity.node_id()? {
            self.store
                .lock()
                .await
                .bind_task(&self.identity, &binding)?
        } else {
            let reply = self
                .peer_request(
                    &binding.network_id,
                    &binding.authority_node,
                    serde_json::json!({"type":"bind_task","binding":binding}),
                )
                .await
                .context("task binding authority is unreachable; execution was not reassigned")?;
            serde_json::from_value(reply["binding"].clone())?
        };
        self.store.lock().await.install_binding(&proof)?;
        Ok(proof)
    }

    async fn prepare_operation_binding(
        self: &Arc<Self>,
        access: &Access,
        request: &mut OperationRequest,
    ) -> Result<()> {
        self.cache_context_bindings(&request.network_id, &request.conversation_id)
            .await?;
        if let Some(proof) = &request.binding {
            self.store.lock().await.install_binding(proof)?;
            return self.store.lock().await.validate_operation_binding(request);
        }
        if request.task_id.is_none() {
            let context = self.store.lock().await.context(
                &request.network_id,
                &request.conversation_id,
                &[],
            )?;
            if let Some(proof) = context.events.iter().rev().find_map(|event| {
                if let EventBody::DispatchRecorded {
                    action,
                    target_node,
                    session_id,
                    ..
                } = &event.body
                    && target_node == &request.target_node
                    && session_id == &request.session_id
                {
                    serde_json::from_value::<OperationRequest>(action.clone())
                        .ok()
                        .and_then(|request| request.binding)
                } else {
                    None
                }
            }) {
                request.task_id = Some(proof.binding.task_id.clone());
                request.binding = Some(proof);
                return self.store.lock().await.validate_operation_binding(request);
            }
            let task_id = format!(
                "execution-{}",
                crypto::digest(&serde_json::to_vec(&(
                    &request.target_node,
                    &request.session_id
                ))?)
            );
            if !self
                .store
                .lock()
                .await
                .tasks(&request.network_id, &request.conversation_id)?
                .iter()
                .any(|task| task.task_id == task_id)
            {
                let event = {
                    let mut store = self.store.lock().await;
                    Self::check_manager_permission(&store, access, true, Some(&task_id))?;
                    store.create_task_with_origin(
                        &self.identity,
                        &request.network_id,
                        &request.conversation_id,
                        EventBody::TaskCreated {
                            task_id: task_id.clone(),
                            title: "Execution session".into(),
                            goal: "User-requested execution".into(),
                            acceptance: "The user verifies the result".into(),
                        },
                        access
                            .manager
                            .as_ref()
                            .map(|manager| (manager.trigger.as_str(), manager.attempt_id.as_str())),
                        now_ms(),
                    )?
                };
                self.publish_changed(&request.network_id, &request.conversation_id, &event.hash);
            }
            request.task_id = Some(task_id);
        }
        let task_id = request
            .task_id
            .as_ref()
            .context("task binding requires a task")?;
        let (creation, event) = {
            let store = self.store.lock().await;
            let creation =
                store.task_creation(&request.network_id, &request.conversation_id, task_id)?;
            let event = store.verify_event(&request.network_id, &creation)?;
            (creation, event)
        };
        let mut existing = self
            .store
            .lock()
            .await
            .task_binding(&request.network_id, &creation.hash)?;
        if event.author != self.identity.node_id()?
            && existing.as_ref().is_none_or(|proof| {
                proof.binding.target_node != request.target_node
                    || proof.binding.session_id != request.session_id
            })
            && let Ok(reply) = self
                .peer_request(
                    &request.network_id,
                    &event.author,
                    serde_json::json!({"type":"task_binding","task_ref":creation.hash}),
                )
                .await
            && !reply["binding"].is_null()
        {
            let proof: crate::binding::SignedBinding =
                serde_json::from_value(reply["binding"].clone())?;
            self.store.lock().await.install_binding(&proof)?;
            existing = Some(proof);
        }
        if let Some(proof) = existing {
            request.binding = Some(proof);
            return self.store.lock().await.validate_operation_binding(request);
        }
        let open_spec = if request.action["type"] == "open" {
            request.action.clone()
        } else {
            let value = self
                .session_fact(
                    &request.network_id,
                    &request.target_node,
                    &request.session_id,
                )
                .await?;
            serde_json::json!({"type":"open","executor":value["session"]["executor"],"cwd":value["session"]["cwd"]})
        };
        let binding = crate::binding::TaskBinding {
            network_id: request.network_id.clone(),
            conversation_id: request.conversation_id.clone(),
            task_ref: creation.hash,
            task_id: task_id.clone(),
            authority_node: event.author,
            generation: 1,
            target_node: request.target_node.clone(),
            session_id: request.session_id.clone(),
            open_spec,
            previous_binding_hash: None,
            previous_seal: None,
        };
        request.binding = Some(self.confirm_binding(binding).await?);
        self.store.lock().await.validate_operation_binding(request)
    }

    async fn rebind_execution(self: &Arc<Self>, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "moving execution requires the user");
        let scope = field(input, "conversation_id")?;
        self.require_scope(access, scope).await?;
        self.sync_all(&access.network_id).await?;
        self.cache_context_bindings(&access.network_id, scope)
            .await?;
        let creation = self.store.lock().await.task_creation(
            &access.network_id,
            scope,
            field(input, "task_id")?,
        )?;
        let previous = self
            .store
            .lock()
            .await
            .task_binding(&access.network_id, &creation.hash)?
            .context("task has no confirmed execution binding")?;
        let old = &previous.binding;
        let seal = if old.target_node == self.identity.node_id()? {
            self.store
                .lock()
                .await
                .seal_binding(&self.identity, &previous)?
        } else {
            let reply = self
                .peer_request(
                    &access.network_id,
                    &old.target_node,
                    serde_json::json!({"type":"seal_binding","binding":previous,"human":true}),
                )
                .await
                .context("original executor is unreachable; task was not reassigned")?;
            serde_json::from_value(reply["seal"].clone())?
        };
        let next = crate::binding::TaskBinding {
            network_id: old.network_id.clone(),
            conversation_id: old.conversation_id.clone(),
            task_ref: old.task_ref.clone(),
            task_id: old.task_id.clone(),
            authority_node: old.authority_node.clone(),
            generation: old
                .generation
                .checked_add(1)
                .context("binding generation overflow")?,
            target_node: field(input, "target_node")?.into(),
            session_id: id(input, "session_id")?,
            open_spec: serde_json::json!({"type":"open","executor":field(input,"executor")?,"cwd":field(input,"cwd")?}),
            previous_binding_hash: Some(crate::binding::binding_hash(old)?),
            previous_seal: Some(seal),
        };
        Ok(serde_json::json!({"binding":self.confirm_binding(next).await?}))
    }

    pub async fn dispatch(
        self: &Arc<Self>,
        access: Access,
        mut request: OperationRequest,
    ) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        ensure!(
            request.network_id == access.network_id,
            "operation belongs to another network"
        );
        self.require_scope(&access, &request.conversation_id)
            .await?;
        id(&json!({"id":request.operation_id}), "id")?;
        id(&json!({"id":request.session_id}), "id")?;
        ensure!(
            access.human || request.action["type"] != "respond_request",
            "only the user may answer approvals"
        );
        if !access.human && request.action["type"] != "open" {
            self.require_execution_scope(
                &access,
                &request.conversation_id,
                &request.target_node,
                &request.session_id,
            )
            .await?;
        }
        self.store.lock().await.authorize_node(
            &access.network_id,
            &request.target_node,
            Some(NodeRole::Executor),
        )?;
        if !access.human
            && matches!(
                request.action["type"].as_str(),
                Some("open" | "send" | "steer")
            )
            && let Some(task) = &request.task_id
        {
            require_task_write(
                &*self.store.lock().await,
                &request.network_id,
                &request.conversation_id,
                task,
                false,
            )?;
        }
        request.manager_authority = access.manager.as_ref().and_then(|manager| {
            manager
                .followup_version
                .map(|version| ManagerDispatchAuthority {
                    scheduler_node: manager.target_node.clone(),
                    trigger: manager.trigger.clone(),
                    followup_version: version,
                })
        });
        self.prepare_operation_binding(&access, &mut request)
            .await?;
        Self::record_manager_operation(&mut *self.store.lock().await, &access, &request)?;
        let context = self.store.lock().await.context(
            &access.network_id,
            &request.conversation_id,
            &request.basis_heads,
        )?;
        ensure!(
            context.missing.is_complete(),
            "operation basis is incomplete or conflicting"
        );
        let old = context.events.iter().find_map(|e| {
            if let EventBody::DispatchRecorded {
                operation_id,
                action,
                ..
            } = &e.body
            {
                (operation_id == &request.operation_id).then_some((e, action))
            } else {
                None
            }
        });
        let head = if let Some((event, old)) = old {
            ensure!(
                old == &serde_json::to_value(&request)?,
                "operation ID reused with different request"
            );
            crypto::digest(&serde_json::to_vec(event)?)
        } else {
            if !access.human
                && matches!(
                    request.action["type"].as_str(),
                    Some("open" | "send" | "steer")
                )
                && let Some(task) = &request.task_id
            {
                require_task_write(
                    &*self.store.lock().await,
                    &request.network_id,
                    &request.conversation_id,
                    task,
                    false,
                )?;
            }
            let event = {
                let mut store = self.store.lock().await;
                Self::check_manager_permission(&store, &access, true, request.task_id.as_deref())?;
                store.append_event(
                    &self.identity,
                    &access.network_id,
                    &request.conversation_id,
                    EventBody::DispatchRecorded {
                        operation_id: request.operation_id.clone(),
                        task_id: request.task_id.clone(),
                        target_node: request.target_node.clone(),
                        session_id: request.session_id.clone(),
                        action: serde_json::to_value(&request)?,
                    },
                    vec![],
                    now_ms(),
                )?
            };
            self.publish_changed(&access.network_id, &request.conversation_id, &event.hash);
            event.hash
        };
        if request.target_node == self.identity.node_id()? {
            self.admit_operation_checked(request, access.human, Some(&access))
                .await
        } else {
            Self::check_manager_permission(
                &*self.store.lock().await,
                &access,
                true,
                request.task_id.as_deref(),
            )?;
            self.peer_request(&access.network_id,&request.target_node,json!({"type":"execute","request":request,"dispatch_head":head,"human":access.human})).await
        }
    }
    async fn admit_operation(
        self: &Arc<Self>,
        request: OperationRequest,
        human: bool,
    ) -> Result<Value> {
        self.admit_operation_checked(request, human, None).await
    }
    async fn admit_operation_checked(
        self: &Arc<Self>,
        mut request: OperationRequest,
        human: bool,
        manager_access: Option<&Access>,
    ) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        self.require_role(&request.network_id, NodeRole::Executor)
            .await?;
        ensure!(
            request.target_node == self.identity.node_id()?,
            "wrong execution target"
        );
        let kind = field(&request.action, "type")?;
        ensure!(
            human || kind != "respond_request",
            "only the user may answer approvals"
        );
        let mut store = self.store.lock().await;
        if let Some(access) = manager_access {
            Self::check_manager_permission(&store, access, true, request.task_id.as_deref())?;
        }
        let current = store.session(
            &request.network_id,
            &request.target_node,
            &request.session_id,
        )?;
        let existing_operation = store.operation(&request.network_id, &request.operation_id)?;
        if !human && existing_operation.is_none() && matches!(kind, "open" | "send" | "steer") {
            if let Some(task) = &request.task_id {
                require_task_write(
                    &store,
                    &request.network_id,
                    &request.conversation_id,
                    task,
                    false,
                )?;
            } else {
                let previous: Vec<_> = store
                    .operations(&request.network_id)?
                    .into_iter()
                    .map(|operation| operation.request)
                    .filter(|operation| {
                        operation.target_node == request.target_node
                            && operation.session_id == request.session_id
                            && matches!(
                                operation.action["type"].as_str(),
                                Some("open" | "send" | "steer")
                            )
                    })
                    .collect();
                let latest_revision = previous
                    .iter()
                    .map(|operation| operation.expected_revision)
                    .max();
                ensure!(
                    !previous.iter().any(|operation| {
                        Some(operation.expected_revision) == latest_revision
                            && operation.task_id.is_some()
                    }),
                    "task_id is required for new agent work in a task-associated execution session"
                );
            }
        }
        let session = if kind == "open" {
            ensure!(
                current.is_none() || existing_operation.is_some(),
                "execution session already exists"
            );
            let executor: ExecutorKind =
                serde_json::from_value(request.action["executor"].clone())?;
            let cwd = self.allowed_path(field(&request.action, "cwd")?)?;
            current.unwrap_or(ExecutionSession {
                network_id: request.network_id.clone(),
                target_node: request.target_node.clone(),
                session_id: request.session_id.clone(),
                revision: 0,
                state: "opening".into(),
                executor: serde_json::to_value(executor)?.as_str().unwrap().into(),
                cwd: cwd.to_str().context("directory must be UTF-8")?.into(),
                native_session_id: None,
            })
        } else {
            current.context("execution session is not open")?
        };
        let action = self.engine_action(&request.action)?;
        action.validate()?;
        let control = !matches!(action, EngineAction::Open {} | EngineAction::Send { .. });
        if control {
            ensure!(
                request.resources.is_empty(),
                "control operations cannot reserve resources"
            );
        } else {
            let path = self.allowed_path(&session.cwd)?;
            let mut resources = vec![path.to_str().context("directory must be UTF-8")?.to_owned()];
            for resource in &request.resources {
                resources.push(
                    self.allowed_path(resource)?
                        .to_str()
                        .context("resource must be UTF-8")?
                        .to_owned(),
                );
            }
            resources.sort();
            resources.dedup();
            request.resources = resources;
        }
        if let Some(old) = existing_operation {
            ensure!(
                old.request == request,
                "operation ID reused with different body"
            );
            return Ok(json!({"operation":old}));
        }
        let operation = if control {
            store.accept_control_operation(&request.network_id, &request)?
        } else {
            store.accept_operation(&request.network_id, &request)?
        };
        if kind == "open" {
            store.upsert_session(&request.network_id, &session)?;
        }
        drop(store);
        let node = self.clone();
        tokio::spawn(async move {
            if let Err(e) = node.execute_operation(request).await {
                tracing::warn!(error=%e,"operation execution requires reconciliation");
            }
        });
        Ok(json!({"operation":operation}))
    }
    fn allowed_path(&self, path: &str) -> Result<PathBuf> {
        let path = std::fs::canonicalize(path).context("execution path does not exist")?;
        ensure!(
            self.config
                .roots
                .iter()
                .filter_map(|r| std::fs::canonicalize(r).ok())
                .any(|r| path.starts_with(r)),
            "execution path is outside configured roots"
        );
        ensure!(path.is_dir(), "execution path must be a directory");
        Ok(path)
    }
    async fn require_execution_scope(
        &self,
        access: &Access,
        scope: &str,
        target: &str,
        session: &str,
    ) -> Result<()> {
        let context = self
            .store
            .lock()
            .await
            .context(&access.network_id, scope, &[])?;
        ensure!(context.events.iter().any(|e| e.scope_id==scope && matches!(&e.body,EventBody::DispatchRecorded{target_node,session_id,..} if target_node==target && session_id==session)),"execution session is outside this conversation");
        Ok(())
    }
    fn engine_action(&self, action: &Value) -> Result<EngineAction> {
        if action["type"] == "open" {
            let map = action.as_object().context("invalid action")?;
            ensure!(
                map.keys()
                    .all(|k| matches!(k.as_str(), "type" | "executor" | "cwd")),
                "unexpected open field"
            );
            Ok(EngineAction::Open {})
        } else {
            Ok(serde_json::from_value(action.clone())?)
        }
    }
    async fn execute_operation(self: Arc<Self>, request: OperationRequest) -> Result<()> {
        let _work = self.maintenance.enter()?;
        let action = self.engine_action(&request.action)?;
        let preparation: Result<ExecutionSession> = async {
            self.require_role(&request.network_id, NodeRole::Executor)
                .await?;
            let mut store = self.store.lock().await;
            let session = store
                .session(
                    &request.network_id,
                    &request.target_node,
                    &request.session_id,
                )?
                .context("accepted operation has no local session")?;
            store.validate_operation_binding(&request)?;
            self.allowed_path(&session.cwd)?;
            for resource in &request.resources {
                self.allowed_path(resource)?;
            }
            store.claim_operation(&request.network_id, &request.operation_id)?;
            Ok(session)
        }
        .await;
        let session = match preparation {
            Ok(session) => session,
            Err(error) => {
                let accepted = self
                    .store
                    .lock()
                    .await
                    .operation(&request.network_id, &request.operation_id)?
                    .is_some_and(|o| o.state == OperationState::Accepted);
                if accepted {
                    self.finish(
                        &request,
                        OperationState::Rejected,
                        json!({"error":format!("{error:#}")}),
                    )
                    .await?;
                }
                return Err(error);
            }
        };
        let native = ExecutorSession {
            executor: serde_json::from_value(json!(session.executor))?,
            cwd: PathBuf::from(&session.cwd),
            native_session_id: session.native_session_id.clone(),
        };
        let store = self.store.clone();
        let mut persisted = session.clone();
        let network = request.network_id.clone();
        let result = self
            .engine
            .execute(&native, action.clone(), move |id| async move {
                persisted.native_session_id = Some(id);
                store.lock().await.upsert_session(&network, &persisted)?;
                Ok(())
            })
            .await;
        match result {
            Ok(result) => {
                let mut store = self.store.lock().await;
                if store
                    .operation(&request.network_id, &request.operation_id)?
                    .is_some_and(|o| o.state.terminal())
                {
                    return Ok(());
                }
                let mut session = store
                    .session(
                        &request.network_id,
                        &request.target_node,
                        &request.session_id,
                    )?
                    .context("session disappeared")?;
                session.native_session_id = result.native_session_id;
                if native.executor == ExecutorKind::Codex
                    && matches!(action, EngineAction::Send { .. })
                {
                    session.state = "running".into();
                    store.upsert_session(&request.network_id, &session)?;
                    store.local_set(
                        &request.network_id,
                        &format!("turn:{}", request.operation_id),
                        &result.result,
                    )?;
                    drop(store);
                    self.append(
                        &request.network_id,
                        &request.conversation_id,
                        EventBody::ExecutionObserved {
                            operation_id: request.operation_id.clone(),
                            task_id: request.task_id.clone(),
                            target_node: request.target_node.clone(),
                            session_id: request.session_id.clone(),
                            status: "running".into(),
                            detail: result.result,
                        },
                    )
                    .await?;
                    self.reconcile(
                        &request.network_id,
                        &request.target_node,
                        &request.operation_id,
                    )
                    .await?;
                } else {
                    store.upsert_session(&request.network_id, &session)?;
                    drop(store);
                    self.finish(&request, OperationState::Succeeded, result.result)
                        .await?;
                }
            }
            Err(error) => {
                {
                    let mut store = self.store.lock().await;
                    let mut session = store
                        .session(
                            &request.network_id,
                            &request.target_node,
                            &request.session_id,
                        )?
                        .context("session disappeared")?;
                    session.native_session_id = error.native_session_id.clone();
                    store.upsert_session(&request.network_id, &session)?;
                }
                let state = if error.uncertain {
                    OperationState::Unknown
                } else {
                    OperationState::Failed
                };
                self.finish(
                    &request,
                    state,
                    json!({"error":error.message,"native_session_id":error.native_session_id}),
                )
                .await?;
            }
        }
        Ok(())
    }
    async fn finish(
        self: &Arc<Self>,
        request: &OperationRequest,
        state: OperationState,
        result: Value,
    ) -> Result<()> {
        let _work = self.maintenance.enter()?;
        let _completion = self.completion_lock.lock().await;
        let status = serde_json::to_value(&state)?.as_str().unwrap().to_owned();
        let completion_key = format!("completion:{}", request.operation_id);
        {
            let mut store = self.store.lock().await;
            if let Some(old) = store.operation(&request.network_id, &request.operation_id)?
                && old.state.terminal()
                && (old.state != state || old.result != result)
            {
                return Ok(());
            }
            store.local_set(
                &request.network_id,
                &completion_key,
                &json!({"state":state,"result":result}),
            )?;
            store.finish_operation(
                &request.network_id,
                &request.operation_id,
                state.clone(),
                result.clone(),
            )?;
            if let Some(mut session) = store.session(
                &request.network_id,
                &request.target_node,
                &request.session_id,
            )? {
                let outstanding = store
                    .operations(&request.network_id)?
                    .into_iter()
                    .filter(|o| {
                        o.request.target_node == request.target_node
                            && o.request.session_id == request.session_id
                            && !o.state.terminal()
                    })
                    .collect::<Vec<_>>();
                session.state = if outstanding
                    .iter()
                    .any(|o| o.state == OperationState::Unknown)
                {
                    "unknown"
                } else if !outstanding.is_empty() {
                    "running"
                } else {
                    "idle"
                }
                .into();
                store.upsert_session(&request.network_id, &session)?;
            }
        }
        let mut blobs = vec![];
        let mut detail = result;
        let bytes = serde_json::to_vec(&detail)?;
        if bytes.len() > 262144 {
            let blob = BlobRef {
                hash: crypto::digest(&bytes),
                size: bytes.len() as u64,
            };
            self.store
                .lock()
                .await
                .put_blob(&request.network_id, &blob, &bytes)?;
            detail = json!({"result_blob":blob});
            blobs.push(blob);
        }
        let existing=self.store.lock().await.context(&request.network_id,&request.conversation_id,&[])?.events.into_iter().find(|e|matches!(&e.body,EventBody::ExecutionObserved{operation_id,status:s,..} if operation_id==&request.operation_id && s==&status));
        let event_hash = if let Some(existing) = existing {
            crypto::digest(&serde_json::to_vec(&existing)?)
        } else {
            self.append_blobs(
                &request.network_id,
                &request.conversation_id,
                EventBody::ExecutionObserved {
                    operation_id: request.operation_id.clone(),
                    task_id: request.task_id.clone(),
                    target_node: request.target_node.clone(),
                    session_id: request.session_id.clone(),
                    status: status.clone(),
                    detail,
                },
                blobs,
            )
            .await?
            .hash
        };
        if matches!(request.action["type"].as_str(), Some("send")) {
            let notification_id = format!("result:{}:{}", request.operation_id, status);
            let already=self.store.lock().await.context(&request.network_id,&request.conversation_id,&[])?.events.iter().any(|e|matches!(&e.body,EventBody::NotificationRaised{notification_id:id,..} if id==&notification_id));
            if !already {
                self.append(
                    &request.network_id,
                    &request.conversation_id,
                    EventBody::NotificationRaised {
                        notification_id,
                        kind: status.clone(),
                        text: format!(
                            "执行会话 {} 的本轮执行状态为 {}。工单验收仍需本人确认。",
                            request.session_id, status
                        ),
                    },
                )
                .await?;
            }
            if state.terminal() && !self.maybe_followup(request, &event_hash).await? {
                return Ok(());
            }
        }
        if state.terminal() {
            self.progress_sequences
                .lock()
                .expect("progress sequence lock")
                .remove(&(request.network_id.clone(), request.operation_id.clone()));
        }
        self.store
            .lock()
            .await
            .local_delete(&request.network_id, &completion_key)?;
        Ok(())
    }
    async fn flush_completions(self: &Arc<Self>, network: &str) -> Result<()> {
        let _work = self.maintenance.enter()?;
        let operations = self.store.lock().await.operations(network)?;
        for operation in operations {
            let key = format!("completion:{}", operation.request.operation_id);
            let pending = self.store.lock().await.local_value(network, &key)?;
            if let Some(value) = pending {
                self.finish(
                    &operation.request,
                    serde_json::from_value(value["state"].clone())?,
                    value["result"].clone(),
                )
                .await?;
            }
        }
        Ok(())
    }
    async fn session_fact(&self, network: &str, target: &str, session_id: &str) -> Result<Value> {
        if target != self.identity.node_id()? {
            return self
                .read_peer(
                    network,
                    target,
                    json!({"type":"session","session_id":session_id}),
                )
                .await;
        }
        let session = self
            .store
            .lock()
            .await
            .session(network, target, session_id)?
            .context("execution session not found")?;
        let mut snapshot = self.engine.snapshot().await;
        if let Some(requests) = snapshot["pending_requests"].as_array_mut() {
            requests.retain(|r| {
                r["params"]["threadId"].as_str() == session.native_session_id.as_deref()
            });
        }
        let native = ExecutorSession {
            executor: serde_json::from_value(json!(session.executor))?,
            cwd: session.cwd.clone().into(),
            native_session_id: session.native_session_id.clone(),
        };
        let read = self
            .engine
            .execute(&native, EngineAction::Read {}, |_| async {
                bail!("read must not create a session")
            })
            .await;
        let actual = match read {
            Ok(r) => json!({"thread":r.result,"pending_requests":snapshot["pending_requests"]}),
            Err(e) => {
                json!({"error":e.message,"uncertain":e.uncertain,"pending_requests":snapshot["pending_requests"]})
            }
        };
        Ok(json!({"session":session,"actual":actual}))
    }
    pub async fn current_facts(&self, network: &str, scope: &str) -> Result<Value> {
        let context = self.store.lock().await.context(network, scope, &[])?;
        let mut keys = BTreeSet::new();
        let mut operations = BTreeSet::new();
        for event in context.events {
            if event.scope_id != scope {
                continue;
            }
            if let EventBody::DispatchRecorded {
                target_node,
                session_id,
                operation_id,
                ..
            } = event.body
            {
                keys.insert((target_node.clone(), session_id));
                operations.insert((target_node, operation_id));
            }
        }
        let mut sessions = vec![];
        let mut facts = vec![];
        let mut records = vec![];
        for (target, session) in keys {
            match tokio::time::timeout(Duration::from_secs(3),self.session_fact(network,&target,&session)).await {Ok(Ok(value))=>{sessions.push(value["session"].clone());facts.push(value);},_=>facts.push(json!({"target_node":target,"session_id":session,"state":"unreachable","freshness":"unknown"}))}
        }
        for (target, op) in operations {
            if target == self.identity.node_id()? {
                if let Some(record) = self.store.lock().await.operation(network, &op)? {
                    records.push(json!(record));
                }
            } else if let Ok(Ok(value)) = tokio::time::timeout(
                Duration::from_secs(3),
                self.peer_request(
                    network,
                    &target,
                    json!({"type":"operation","operation_id":op}),
                ),
            )
            .await
                && !value["operation"].is_null()
            {
                records.push(value["operation"].clone());
            }
        }
        Ok(json!({"sessions":sessions,"operations":records,"facts":facts}))
    }
    async fn handle_peer(self: &Arc<Self>, message: &Inbound) -> Result<Value> {
        self.membership_state().await?;
        let network = &message.network_id;
        let input = &message.payload;
        self.store
            .lock()
            .await
            .authorize_node(network, &message.origin, None)?;
        let kind = field(input, "type")?;
        let _work = (!matches!(
            kind,
            "capabilities"
                | "inventory"
                | "event"
                | "blob"
                | "operation"
                | "task_binding"
                | "progress_read"
                | "chat_task_status"
        ))
        .then(|| self.maintenance.enter())
        .transpose()?;
        match kind {
            "capabilities" => Ok(self.capabilities()),
            "inventory" => {
                self.require_role(network, NodeRole::Replica).await?;
                self.store.lock().await.authorize_node(
                    network,
                    &message.origin,
                    Some(NodeRole::Replica),
                )?;
                let store = self.store.lock().await;
                let scopes = store
                    .scopes(network)?
                    .into_iter()
                    .map(|s| Ok(json!({"scope_id":s,"heads":store.heads(network,&s)?})))
                    .collect::<Result<Vec<_>>>()?;
                Ok(json!({"membership":store.trust_bundle(network)?,"scopes":scopes}))
            }
            "event" => {
                self.require_role(network, NodeRole::Replica).await?;
                self.store.lock().await.authorize_node(
                    network,
                    &message.origin,
                    Some(NodeRole::Replica),
                )?;
                Ok(json!({"event":self.store.lock().await.event(network,field(input,"hash")?)?}))
            }
            "blob" => {
                self.require_role(network, NodeRole::Replica).await?;
                self.store.lock().await.authorize_node(
                    network,
                    &message.origin,
                    Some(NodeRole::Replica),
                )?;
                let bytes = self
                    .store
                    .lock()
                    .await
                    .get_blob(network, field(input, "hash")?)?
                    .context("blob unavailable")?;
                let offset = input["offset"]
                    .as_u64()
                    .context("offset required")?
                    .min(usize::MAX as u64) as usize;
                let limit = input["limit"].as_u64().unwrap_or(65536).clamp(1, 65536) as usize;
                ensure!(offset <= bytes.len(), "invalid blob offset");
                Ok(
                    json!({"data":hex::encode(&bytes[offset..offset.saturating_add(limit).min(bytes.len())]),"size":bytes.len()}),
                )
            }
            "trust" => {
                let state: NetworkState = serde_json::from_value(input["state"].clone())?;
                ensure!(
                    state.genesis.network_id == *network,
                    "wrong membership network"
                );
                self.install_membership(state).await?;
                Ok(json!({"received":true}))
            }
            "task_binding" => Ok(
                json!({"binding":self.store.lock().await.task_binding(network,field(input,"task_ref")?)?}),
            ),
            "bind_task" => {
                let sender =
                    self.store
                        .lock()
                        .await
                        .authorize_node(network, &message.origin, None)?;
                ensure!(
                    sender.roles.contains(&NodeRole::Scheduler)
                        || sender.roles.contains(&NodeRole::Gateway),
                    "origin cannot propose execution binding"
                );
                let binding: crate::binding::TaskBinding =
                    serde_json::from_value(input["binding"].clone())?;
                ensure!(binding.network_id == *network, "wrong binding network");
                Ok(json!({"binding":self.store.lock().await.bind_task(&self.identity,&binding)?}))
            }
            "seal_binding" => {
                let sender = self.store.lock().await.authorize_node(
                    network,
                    &message.origin,
                    Some(NodeRole::Gateway),
                )?;
                ensure!(
                    input["human"] == true && sender.roles.contains(&NodeRole::Gateway),
                    "sealing execution requires the user"
                );
                let proof: crate::binding::SignedBinding =
                    serde_json::from_value(input["binding"].clone())?;
                ensure!(
                    proof.binding.network_id == *network,
                    "wrong binding network"
                );
                Ok(json!({"seal":self.store.lock().await.seal_binding(&self.identity,&proof)?}))
            }
            "execute" => {
                let sender =
                    self.store
                        .lock()
                        .await
                        .authorize_node(network, &message.origin, None)?;
                ensure!(
                    sender.roles.contains(&NodeRole::Scheduler)
                        || sender.roles.contains(&NodeRole::Gateway),
                    "origin cannot dispatch"
                );
                let human = input["human"].as_bool().unwrap_or(false);
                ensure!(
                    !human || sender.roles.contains(&NodeRole::Gateway),
                    "origin cannot assert human authority"
                );
                let request: OperationRequest = serde_json::from_value(input["request"].clone())?;
                ensure!(request.network_id == *network, "wrong operation network");
                // Admission holds the complete dispatched history before any external call.
                let mut wanted = request.basis_heads.clone();
                wanted.push(field(input, "dispatch_head")?.into());
                self.fetch_closure(network, &message.origin, &wanted)
                    .await?;
                ensure!(
                    self.store
                        .lock()
                        .await
                        .missing_dependencies(network, &wanted)?
                        .is_complete(),
                    "dispatch context is incomplete"
                );
                {
                    let store = self.store.lock().await;
                    let envelope = store
                        .event(network, field(input, "dispatch_head")?)?
                        .context("dispatch event is missing")?;
                    let event = store.verify_event(network, &envelope)?;
                    ensure!(
                        event.author == message.origin
                            && event.scope_id == request.conversation_id
                            && matches!(&event.body, EventBody::DispatchRecorded { operation_id, action, .. }
                            if operation_id == &request.operation_id && action == &serde_json::to_value(&request)?),
                        "execution request does not match its original signed dispatch"
                    );
                    if let Some(authority) = &request.manager_authority {
                        ensure!(
                            !human && authority.scheduler_node == message.origin,
                            "invalid autonomous dispatch authority"
                        );
                    }
                }
                self.cache_context_bindings(network, &request.conversation_id)
                    .await?;
                let proof = request
                    .binding
                    .as_ref()
                    .context("execute requires a confirmed task binding")?;
                self.store.lock().await.install_binding(proof)?;
                self.admit_operation(request, human).await
            }
            "session" => {
                self.session_fact(
                    network,
                    &self.identity.node_id()?,
                    field(input, "session_id")?,
                )
                .await
            }
            "operation" => Ok(
                json!({"operation":self.store.lock().await.operation(network,field(input,"operation_id")?)?}),
            ),
            "schedule" => self.receive_schedule(message).await,
            "manager_status" => {
                self.manager_status(
                    network,
                    field(input, "conversation_id")?,
                    field(input, "trigger")?,
                )
                .await
            }
            "progress_read" => {
                ensure!(
                    !message.broadcast,
                    "progress observation requires a direct request"
                );
                self.read_progress(network, &message.origin, input).await
            }
            "chat_task_status" => {
                let sender =
                    self.store
                        .lock()
                        .await
                        .authorize_node(network, &message.origin, None)?;
                ensure!(
                    sender.roles.contains(&NodeRole::Gateway)
                        || sender.roles.contains(&NodeRole::Scheduler),
                    "origin cannot observe execution controls"
                );
                self.chat_task_status(
                    network,
                    field(input, "conversation_id")?,
                    field(input, "task_id")?,
                )
                .await
            }
            "cancel_manager" | "resolve_manager_unknown" => {
                self.store.lock().await.authorize_node(
                    network,
                    &message.origin,
                    Some(NodeRole::Gateway),
                )?;
                ensure!(
                    input["human"] == true,
                    "management control requires the user"
                );
                let access = Access {
                    network_id: network.clone(),
                    human: true,
                    conversation_id: Some(field(input, "conversation_id")?.into()),
                    manager: None,
                };
                if kind == "cancel_manager" {
                    self.cancel_manager(&access, input).await
                } else {
                    self.resolve_manager_invocation(&access, input).await
                }
            }
            "reconcile" => {
                self.reconcile(
                    network,
                    &self.identity.node_id()?,
                    field(input, "operation_id")?,
                )
                .await
            }
            "resolve_unknown" => {
                self.store.lock().await.authorize_node(
                    network,
                    &message.origin,
                    Some(NodeRole::Gateway),
                )?;
                self.resolve_unknown(network, input).await
            }
            _ => bail!("unknown peer message"),
        }
    }
    async fn observe_engine(self: &Arc<Self>, event: Value) -> Result<()> {
        let _work = self.maintenance.enter_when_open().await;
        let cli_progress = event["kind"] == "executor_progress";
        let method = event["method"].as_str().unwrap_or(if cli_progress {
            "executor/progress"
        } else {
            ""
        });
        let thread = event["params"]["threadId"]
            .as_str()
            .or_else(|| event["params"]["thread_id"].as_str())
            .or_else(|| event["params"]["session_id"].as_str())
            .or_else(|| event["native_session_id"].as_str());
        let networks = self.store.lock().await.networks()?;
        for network in networks {
            let operations = self.store.lock().await.operations(&network)?;
            for operation in operations.into_iter().filter(|o| {
                matches!(o.state, OperationState::Running | OperationState::Unknown)
                    && o.request.action["type"] == "send"
            }) {
                let request = &operation.request;
                let session = self
                    .store
                    .lock()
                    .await
                    .session(&network, &request.target_node, &request.session_id)?
                    .context("running operation lost session")?;
                if method == "bridge.codex_disconnected"
                    && session.executor == "codex"
                    && operation.state == OperationState::Running
                {
                    self.finish(
                        request,
                        OperationState::Unknown,
                        json!({"error":"Codex disconnected; reconcile before any retry"}),
                    )
                    .await?;
                    continue;
                }
                if thread.is_none() || thread != session.native_session_id.as_deref() {
                    continue;
                }
                if cli_progress && event["executor"] != session.executor {
                    continue;
                }
                let sequence = {
                    let mut sequences = self
                        .progress_sequences
                        .lock()
                        .expect("progress sequence lock");
                    let sequence = sequences
                        .entry((network.clone(), request.operation_id.clone()))
                        .or_default();
                    *sequence += 1;
                    *sequence
                };
                let turn = self
                    .store
                    .lock()
                    .await
                    .local_value(&network, &format!("turn:{}", request.operation_id))?;
                self.publish_progress(&network, json!({"type":"execution_progress","conversation_id":request.conversation_id,
                    "task_id":request.task_id,"task_ref":request.binding.as_ref().map(|proof| &proof.binding.task_ref),
                    "target_node":request.target_node,"session_id":request.session_id,"operation_id":request.operation_id,
                    "turn_id":turn.as_ref().and_then(|value|value["turn"]["id"].as_str()),
                    "source":self.identity.node_id()?,"source_node":self.identity.node_id()?,"sequence":sequence,"progress":observation::execution_progress(&event)}));
                if method == "turn/completed" {
                    let turn = field(&event["params"]["turn"], "id")?;
                    let known = self
                        .store
                        .lock()
                        .await
                        .local_value(&network, &format!("turn:{}", request.operation_id))?;
                    if known.as_ref().and_then(|v| v["turn"]["id"].as_str()) != Some(turn) {
                        continue;
                    }
                    let state = if event["params"]["turn"]["status"] == "completed" {
                        OperationState::Succeeded
                    } else {
                        OperationState::Failed
                    };
                    self.finish(request, state, event.clone()).await?;
                } else if !method.ends_with("/delta")
                    && (method == "item/completed"
                        || event.get("id").is_some()
                        || (cli_progress && event["event"]["type"] != "stream_event"))
                {
                    let bytes = serde_json::to_vec(&event)?;
                    let mut blobs = vec![];
                    let detail = if bytes.len() > 262144 {
                        let blob = BlobRef {
                            hash: crypto::digest(&bytes),
                            size: bytes.len() as u64,
                        };
                        self.store.lock().await.put_blob(&network, &blob, &bytes)?;
                        blobs.push(blob.clone());
                        json!({"event_blob":blob})
                    } else {
                        event.clone()
                    };
                    self.append_blobs(
                        &network,
                        &request.conversation_id,
                        EventBody::ExecutionObserved {
                            operation_id: request.operation_id.clone(),
                            task_id: request.task_id.clone(),
                            target_node: request.target_node.clone(),
                            session_id: request.session_id.clone(),
                            status: if event.get("id").is_some() {
                                "needs_input"
                            } else {
                                "progress"
                            }
                            .into(),
                            detail,
                        },
                        blobs,
                    )
                    .await?;
                    if event.get("id").is_some() {
                        self.raise_notification(
                            &network,
                            &request.conversation_id,
                            "needs_input",
                            "执行 Agent 正在等待本人审批或补充输入。",
                        )
                        .await?;
                    }
                }
            }
        }
        Ok(())
    }
    async fn reconcile(
        self: &Arc<Self>,
        network: &str,
        target: &str,
        operation_id: &str,
    ) -> Result<Value> {
        if target != self.identity.node_id()? {
            return self
                .peer_request(
                    network,
                    target,
                    json!({"type":"reconcile","operation_id":operation_id}),
                )
                .await;
        }
        let Some(operation) = self.store.lock().await.operation(network, operation_id)? else {
            return Ok(
                json!({"operation":null,"state":"not_found","target_node":target,"checked_at_ms":now_ms()}),
            );
        };
        if operation.state.terminal() {
            return Ok(json!({"operation":operation}));
        }
        let mut actual = self
            .session_fact(network, target, &operation.request.session_id)
            .await?;
        let known = self
            .store
            .lock()
            .await
            .local_value(network, &format!("turn:{operation_id}"))?;
        if let Some(turn_id) = known.as_ref().and_then(|v| v["turn"]["id"].as_str()) {
            let native_id = actual["session"]["native_session_id"]
                .as_str()
                .context("execution session has no native ID")?;
            let turn = match self.engine.read_turn(native_id, turn_id).await {
                Ok(turn) => turn,
                Err(error) => {
                    actual["reconcile_error"] = json!(format!("{error:#}"));
                    None
                }
            };
            if let Some(turn) = turn
                && matches!(
                    turn["status"].as_str(),
                    Some("completed" | "failed" | "interrupted")
                )
            {
                self.finish(
                    &operation.request,
                    if turn["status"] == "completed" {
                        OperationState::Succeeded
                    } else {
                        OperationState::Failed
                    },
                    json!({"reconciled_turn":turn}),
                )
                .await?;
            }
        }
        Ok(
            json!({"operation":self.store.lock().await.operation(network,operation_id)?,"actual":actual}),
        )
    }
    async fn resolve_unknown(self: &Arc<Self>, network: &str, input: &Value) -> Result<Value> {
        let target = field(input, "target_node")?;
        if target != self.identity.node_id()? {
            let mut value = input.clone();
            value["type"] = json!("resolve_unknown");
            return self.peer_request(network, target, value).await;
        }
        let operation = self
            .store
            .lock()
            .await
            .operation(network, field(input, "operation_id")?)?
            .context("operation not found")?;
        ensure!(
            operation.state == OperationState::Unknown,
            "only unknown outcomes require a human resolution"
        );
        let evidence = field(input, "evidence")?;
        ensure!(
            evidence.len() >= 12,
            "describe how you verified the process and external effects"
        );
        let state = match field(input, "outcome")? {
            "succeeded" => OperationState::Succeeded,
            "failed" => OperationState::Failed,
            _ => bail!("invalid resolution"),
        };
        self.finish(
            &operation.request,
            state,
            json!({"human_verified":true,"evidence":evidence}),
        )
        .await?;
        Ok(
            json!({"operation":self.store.lock().await.operation(network,&operation.request.operation_id)?}),
        )
    }
}
