mod handoff;
mod observation;
mod task_business;
mod gateway_forward;
mod gateway_target;
pub(crate) use gateway_forward::GatewaySourceRequest;

mod dispatch;
mod node_policy_read;

use crate::identity::{
    policy::{FreshnessProof, RequestTransport},
    task_execution_context::TaskScopedAuthorization,
};
use crate::{
    config::{self, Config},
    crypto::{self, LocalIdentity},
    engine::{
        Engine, EngineAction, ExecutionContext, ExecutorConfig, ExecutorKind, ExecutorSession,
    },
    maintenance::{Busy, Gate},
    model::*,
    network::{IncomingSocket, NetworkHub, Service},
    peer::{Inbound, PeerHub},
    store::Store,
};
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, broadcast, mpsc};
use vibeface_secure::authorization::{
    AUTH_VERSION, Actor, Audience, CheckStage, DeviceId, FreshnessChallenge, FreshnessReceipt,
    INVOCATION_MAX_AGE_MS, Invocation, Nonce, ObjectHash, PolicyDomain, PublicKey, ResourceId,
    ResourceRef, ServiceId, Signed, SpaceId, TransportEvidence, VerifiedAuthorization,
    VerifiedClientAuthorization,
    gateway_forward::GatewayOriginal,
    node_execution_policy::VerifiedTaskCurrentPolicy,
    prepare::{PrepareIntent, VerifiedPrepareAuthorization},
};
use vibeface_secure::signature_verification::SignatureVerification;

#[derive(Clone)]
pub struct Access {
    pub network_id: String,
    pub conversation_id: Option<String>,
    authorization: TaskScopedAuthorization,
    freshness: FreshnessProof,
    transport: RequestTransport,
    body: Value,
    verification: SignatureVerification,
}

struct TaskScopedPrepareAuthorization {
    intent: Signed<PrepareIntent>,
    authorization: VerifiedPrepareAuthorization,
    task_policy: Option<VerifiedTaskCurrentPolicy>,
    transport: RequestTransport,
}

impl TaskScopedPrepareAuthorization {
    fn verify_in(
        connection: &rusqlite::Connection,
        policy: &crate::identity::journal::PolicyState,
        source: &PublicKey,
        intent: Signed<PrepareIntent>,
        body: &Value,
        audience: &Audience,
        transport: RequestTransport,
        now: u64,
    ) -> Result<Self> {
        let task_policy =
            crate::identity::foreign_execution_policy::current_task_prepare_policy_in(
                connection, policy, source, &intent, body, now,
            )?;
        let state = task_policy
            .as_ref()
            .map_or(policy.verified(), |task| task.state());
        let authorization = match &transport {
            RequestTransport::Existing(transport) => state.verify_prepare_intent(
                &intent,
                body,
                authorization_action(body)?,
                audience,
                transport,
                now,
            )?,
            RequestTransport::DirectedGateway { verified, original } => {
                ensure!(
                    matches!(
                        original,
                        GatewayOriginal::Prepare { body: raw, intent: signed }
                            if raw == body && signed == &intent
                    ),
                    "gateway prepare differs from its original"
                );
                state.verify_gateway_prepare_intent(
                    &intent,
                    body,
                    authorization_action(body)?,
                    audience,
                    verified,
                    now,
                )?
            }
        };
        let mut usage = authorization.proof_usage().clone();
        if let Some(task) = &task_policy {
            usage.merge(task.proof_usage());
        }
        usage.record_signed(&intent)?;
        if let Some(counts) = SignatureVerification::current_counts() {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        Ok(Self {
            intent,
            authorization,
            task_policy,
            transport,
        })
    }

    fn check_same_policy(&self, current: &Self) -> Result<()> {
        let same = match (&self.task_policy, &current.task_policy) {
            (None, None) => true,
            (Some(original), Some(current)) => {
                original.request() == current.request()
                    && original.snapshot() == current.snapshot()
                    && original.witness() == current.witness()
            }
            _ => false,
        };
        ensure!(same, "NeedPolicySync: prepare Task policy changed");
        Ok(())
    }
}

impl Access {
    fn account_actor(&self) -> bool {
        matches!(self.authorization.actor(), Actor::Account(_))
    }

    fn require_body(&self, body: &Value) -> Result<()> {
        ensure!(
            &self.body == body,
            "authorization belongs to another request"
        );
        ensure!(
            body["space_id"].as_str() == Some(self.network_id.as_str()),
            "authorization belongs to another space"
        );
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::identity::test_support::TestNetwork;
    use vibeface_secure::authorization::{ResourceId, SessionRef};

    impl Runtime {
        pub(crate) async fn hold_handoff_resolution_for_test(&self) -> tokio::sync::MutexGuard<'_, ()> {
            self.handoff_resolution_lock.lock().await
        }
    }

    fn fixture() -> Result<PathBuf> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("runtime-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        config::private_write(
            &dir.join("data-epoch.json"),
            &serde_json::to_vec(&config::data_format())?,
        )?;
        let mut cfg = Config::local("127.0.0.1:4790".parse()?, vec![dir.canonicalize()?], false);
        cfg.codex_socket = Some(dir.join("unavailable.sock"));
        cfg.claude_bin = dir.join("missing-claude");
        cfg.save(&dir)?;
        config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&LocalIdentity::generate())?,
        )?;
        drop(Store::initialize(&dir.join("node.sqlite3"))?);
        Ok(dir)
    }

    pub(crate) async fn runtime_fixture() -> Result<(TestNetwork, Arc<Runtime>)> {
        let network = TestNetwork::new()?;
        let dir = &network.directory;
        config::private_write(
            &dir.join("data-epoch.json"),
            &serde_json::to_vec(&config::data_format())?,
        )?;
        let mut cfg = Config::local(
            "127.0.0.1:4790".parse()?,
            vec![network.project.clone()],
            false,
        );
        cfg.codex_socket = Some(dir.join("unavailable.sock"));
        cfg.claude_bin = dir.join("missing-claude");
        cfg.codex_bin = dir.join("missing-codex");
        cfg.save(dir)?;
        config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&network.node)?,
        )?;
        let mut store = network.at(&dir.join("node.sqlite3"))?;
        network.conversation(&mut store, "conversation")?;
        drop(store);
        let node = Arc::new(Runtime::open(dir).await?);
        Ok((network, node))
    }

    async fn access(network: &TestNetwork, node: &Runtime, body: &Value) -> Result<Access> {
        let now = now_ms() as u64;
        let mut store = node.store.lock().await;
        let (authorization, transport) = network.authorization(&store, body, now)?;
        let stage = if body["method"] != "operate" && authorization.action().is_read() {
            CheckStage::RemoteRead
        } else {
            CheckStage::Admit
        };
        let freshness = network.freshness(&mut store, &authorization, stage, now)?;
        Ok(Access {
            network_id: network.space.to_string(),
            conversation_id: body["conversation_id"].as_str().map(str::to_owned),
            authorization: TaskScopedAuthorization::from_primary(authorization),
            freshness,
            transport: RequestTransport::Existing(transport),
            body: body.clone(),
            verification: SignatureVerification::new(),
        })
    }

    async fn rpc(network: &TestNetwork, node: &Arc<Runtime>, body: Value) -> Result<Value> {
        node.rpc(access(network, node, &body).await?, body).await
    }

    async fn task(
        network: &TestNetwork,
        node: &Arc<Runtime>,
        name: &str,
        auto: bool,
    ) -> Result<Value> {
        let mut body = network.body("create_task", "conversation");
        body["task_id"] = json!(name);
        body["title"] = json!(name);
        body["goal"] = json!("complete the requested work");
        body["acceptance"] = json!("the user verifies the result");
        body["auto_followup"] = json!(auto);
        rpc(network, node, body).await
    }

    async fn change_task(
        network: &TestNetwork,
        node: &Arc<Runtime>,
        name: &str,
        state: &str,
    ) -> Result<()> {
        let mut body = network.body("update_task", "conversation");
        body["task_id"] = json!(name);
        body["task_ref"] = json!(
            node.store
                .lock()
                .await
                .task_creation(network.space.as_str(), "conversation", name)?
                .hash
        );
        body["status"] = json!(state);
        body["detail"] = json!("the user changed this task");
        rpc(network, node, body).await?;
        Ok(())
    }

    async fn queued(
        network: &TestNetwork,
        node: &Arc<Runtime>,
        name: &str,
        executor: &str,
    ) -> Result<OperationRequest> {
        queued_with_manager(network, node, name, executor, None).await
    }

    async fn queued_with_manager(
        network: &TestNetwork,
        node: &Arc<Runtime>,
        name: &str,
        executor: &str,
        manager: Option<&AgentSession>,
    ) -> Result<OperationRequest> {
        task(network, node, name, false).await?;
        let creation =
            node.store
                .lock()
                .await
                .task_creation(network.space.as_str(), "conversation", name)?;
        let mut bind = network.body("bind_execution", "conversation");
        bind["task_id"] = json!(name);
        bind["task_ref"] = json!(creation.hash);
        bind["target_node"] = json!(node.identity.node_id()?);
        bind["session_id"] = json!(name);
        bind["executor"] = json!(executor);
        bind["cwd"] = json!(network.project);
        let binding = serde_json::from_value(rpc(network, node, bind).await?["binding"].clone())?;
        let mut open = OperationRequest {
            operation_id: format!("open:{name}"),
            network_id: network.space.to_string(),
            conversation_id: "conversation".into(),
            task_id: Some(name.into()),
            target_node: node.identity.node_id()?,
            session_id: name.into(),
            expected_revision: 0,
            basis_heads: node
                .store
                .lock()
                .await
                .heads(network.space.as_str(), "conversation")?,
            action: json!({"type":"open","executor":executor,"cwd":network.project}),
            resources: vec![network.project.to_string_lossy().into_owned()],
            binding: Some(binding),
        };
        admit(network, node, &open).await?;
        claim(network, node, &open).await?;
        node.finish(
            &open,
            OperationState::Succeeded,
            json!({"native_session_id":"native-test"}),
        )
        .await?;
        let mut store = node.store.lock().await;
        let mut session = store
            .session(network.space.as_str(), &open.target_node, &open.session_id)?
            .context("open did not persist its session")?;
        session.native_session_id = Some("native-test".into());
        store.upsert_session(network.space.as_str(), &session)?;
        open.operation_id = name.into();
        open.expected_revision = session.revision;
        open.basis_heads = store.heads(network.space.as_str(), "conversation")?;
        open.action = json!({"type":"send","text":"test"});
        let request = open;
        drop(store);
        if let Some(manager) = manager {
            let mut body = network.body("operate", "conversation");
            body["authorization_id"] = json!(request.operation_id);
            body["request"] = json!(request);
            body["sdk_session"] = json!(manager.sdk_session);
            body["parent_invocation"] = json!(manager.parent_invocation);
            let access = node.authorize_agent(manager, body).await?;
            node.store.lock().await.accept_request_authorized_operation(
                &node.identity,
                &access.authorization,
                &access.freshness,
                &access.transport,
                &access.body,
                &request,
                now_ms() as u64,
            )?;
        } else {
            admit(network, node, &request).await?;
        }
        Ok(request)
    }

    async fn admit(
        network: &TestNetwork,
        node: &Runtime,
        request: &OperationRequest,
    ) -> Result<OperationRecord> {
        let mut body = network.body("operate", "conversation");
        body["authorization_id"] = json!(request.operation_id);
        body["request"] = json!(request);
        let access = access(network, node, &body).await?;
        node.store.lock().await.accept_request_authorized_operation(
            &node.identity,
            &access.authorization,
            &access.freshness,
            &access.transport,
            &body,
            request,
            now_ms() as u64,
        )
    }

    async fn claim(
        network: &TestNetwork,
        node: &Runtime,
        request: &OperationRequest,
    ) -> Result<()> {
        let now = now_ms() as u64;
        let mut store = node.store.lock().await;
        let auth =
            store.operation_authorization(network.space.as_str(), &request.operation_id, now)?;
        let proof = network.freshness(&mut store, &auth, CheckStage::Start, now)?;
        store.claim_authorized_operation(
            network.space.as_str(),
            &request.operation_id,
            &proof,
            now,
        )?;
        Ok(())
    }

    async fn operation(network: &TestNetwork, node: &Runtime, id: &str) -> Result<OperationRecord> {
        node.store
            .lock()
            .await
            .operation(network.space.as_str(), id)?
            .context("operation is missing")
    }

    async fn agent(network: &TestNetwork, node: &Arc<Runtime>) -> Result<AgentSession> {
        let trigger = network.message(
            &mut *node.store.lock().await,
            "conversation",
            "manage these tasks",
        )?;
        let audience = node.audience()?;
        let session = SessionRef {
            target: audience.device,
            session_id: trigger.hash.clone(),
        };
        let descriptor = node.store.lock().await.agent_grant(
            network.space.as_str(),
            "conversation",
            &audience.service,
            &session,
            &trigger.hash,
            now_ms() as u64,
        )?;
        node.claim_manager(
            &descriptor,
            &trigger.hash,
            "runtime-tests",
            &crypto::digest(b"runtime manager configuration"),
        )
        .await?;
        Ok(descriptor)
    }

    fn derived_result(
        space: &str,
        resource: &ResourceId,
        request: &OperationRequest,
        turn: &str,
    ) -> Value {
        json!({"execution_context":{"space_id":space,"resource_id":resource,"operation_id":request.operation_id,"container_id":null},"event":{"method":"turn/completed","params":{"threadId":"native-test","turn":{"id":turn,"status":"completed"}}}})
    }

    #[tokio::test]
    async fn unsupported_epoch_does_not_open_databases_or_modify_the_directory() -> Result<()> {
        let dir = fixture()?;
        std::fs::remove_file(dir.join("data-epoch.json"))?;
        let config = std::fs::read(dir.join("config.json"))?;
        let database = std::fs::read(dir.join("node.sqlite3"))?;
        assert!(Runtime::open(&dir).await.is_err());
        assert_eq!(std::fs::read(dir.join("node.sqlite3"))?, database);
        assert!(!dir.join("identity.sqlite3").exists());
        assert_eq!(std::fs::read(dir.join("config.json"))?, config);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn missing_or_empty_ready_database_is_rejected_before_lock_creation() -> Result<()> {
        for empty in [false, true] {
            let dir = fixture()?;
            if empty {
                std::fs::write(dir.join("node.sqlite3"), [])?;
            } else {
                std::fs::remove_file(dir.join("node.sqlite3"))?;
            }
            let snapshot = || -> Result<BTreeSet<(String, Vec<u8>)>> {
                std::fs::read_dir(&dir)?
                    .map(|entry| {
                        let entry = entry?;
                        Ok((
                            entry.file_name().to_string_lossy().into_owned(),
                            std::fs::read(entry.path())?,
                        ))
                    })
                    .collect()
            };
            let before = snapshot()?;
            assert!(Runtime::open(&dir).await.is_err());
            assert!(!dir.join("node.lock").exists());
            assert_eq!(snapshot()?, before);
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn policy_reopen_uses_one_store_without_root_secrets() -> Result<()> {
        let (network, node) = runtime_fixture().await?;
        let heads = node
            .store
            .lock()
            .await
            .authorization_state()
            .head(&PolicyDomain::Space(network.space.clone()))
            .cloned();
        assert!(node.has_membership().await?);
        drop(node);
        let node = Runtime::open(&network.directory).await?;
        assert_eq!(
            node.store
                .lock()
                .await
                .authorization_state()
                .head(&PolicyDomain::Space(network.space.clone()))
                .cloned(),
            heads
        );
        assert_eq!(
            node.store
                .lock()
                .await
                .messages(network.space.as_str(), "conversation")?
                .len(),
            0
        );
        assert!(!network.directory.join("auth.sqlite3").exists());
        assert!(!network.directory.join("identity.sqlite3").exists());
        for entry in std::fs::read_dir(&network.directory)? {
            assert!(!entry?.file_name().to_string_lossy().contains("root"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn executor_followup_only_accepts_its_authorized_dispatched_result() -> Result<()> {
        let (network, node) = runtime_fixture().await?;
        let request = queued(&network, &node, "work", "codex").await?;
        task(&network, &node, "work", true).await?;
        claim(&network, &node, &request).await?;
        node.finish(
            &request,
            OperationState::Succeeded,
            json!({"result":"done"}),
        )
        .await?;
        let mut store = node.store.lock().await;
        let result=store.context(network.space.as_str(),"conversation",&[])?.events.into_iter().find(|event|matches!(&event.body,EventBody::ExecutionObserved{operation_id,status,..} if operation_id=="work" && status=="succeeded")).context("result missing")?;
        let hash = crypto::digest(&serde_json::to_vec(&result)?);
        require_executor_followup(
            &store,
            network.space.as_str(),
            &node.identity.node_id()?,
            "conversation",
            &hash,
        )?;
        assert!(
            require_executor_followup(
                &store,
                network.space.as_str(),
                "other-node",
                "conversation",
                &hash
            )
            .is_err()
        );
        assert!(
            require_executor_followup(
                &store,
                network.space.as_str(),
                &node.identity.node_id()?,
                "other-conversation",
                &hash
            )
            .is_err()
        );
        let message = network.message(
            &mut store,
            "conversation",
            "a user message is not an execution result",
        )?;
        assert!(
            require_executor_followup(
                &store,
                network.space.as_str(),
                &node.identity.node_id()?,
                "conversation",
                &message.hash
            )
            .is_err()
        );
        let mut forged = store.event(network.space.as_str(), &hash)?.unwrap();
        forged.canonical_body = forged.canonical_body.replace("succeeded", "unknown");
        assert!(store.ingest_event(network.space.as_str(), &forged).is_err());
        drop(store);
        change_task(&network, &node, "work", "cancelled").await?;
        assert!(
            require_executor_followup(
                &*node.store.lock().await,
                network.space.as_str(),
                &node.identity.node_id()?,
                "conversation",
                &hash
            )
            .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn unstarted_followup_survives_reopen_and_claimed_attempt_is_not_repeated() -> Result<()>
    {
        let mut phase = "create fixture";
        let outcome: Result<()> = async {
            let (network, mut node) = runtime_fixture().await?;
            phase = "admit the original Manager";
            let manager = agent(&network, &node).await?;
            phase = "queue the Service execution";
            let request =
                queued_with_manager(&network, &node, "followup", "codex", Some(&manager)).await?;
            task(&network, &node, "followup", true).await?;
            let settings = config::ManagerSettings {
                provider: "codex_sdk".into(),
                model: "unused".into(),
            };
            Arc::get_mut(&mut node)
                .context("fixture unexpectedly shares Runtime")?
                .config
                .manager = Some(settings.clone());
            phase = "claim the original execution";
            claim(&network, &node, &request).await?;
            phase = "record result while original Manager is busy";
            node.finish(
                &request,
                OperationState::Succeeded,
                json!({"result":"done"}),
            )
            .await?;
            let result = node
                .store
                .lock()
                .await
                .context(network.space.as_str(), "conversation", &[])?
                .events
                .into_iter()
                .find(|event| {
                    matches!(&event.body, EventBody::ExecutionObserved { operation_id, status, .. }
                if operation_id == "followup" && status == "succeeded")
                })
                .context("followup result is missing")?;
            let hash = crypto::digest(&serde_json::to_vec(&result)?);
            {
                let mut store = node.store.lock().await;
                assert!(
                    store
                        .continuation(network.space.as_str(), &hash, "v2")?
                        .is_none(),
                    "busy Manager consumed a continuation"
                );
                assert!(
                    store
                        .local_value(network.space.as_str(), "completion:followup")?
                        .is_some()
                );
                let value = store
                    .local_value(
                        network.space.as_str(),
                        &crate::scheduler::key(
                            "invocation",
                            "conversation",
                            &manager.sdk_session.session_id,
                        ),
                    )?
                    .context("original Manager claim is missing")?;
                let mut invocation: crate::scheduler::Invocation = serde_json::from_value(value)?;
                assert!(invocation.state == crate::scheduler::InvocationState::Running);
                invocation.state = crate::scheduler::InvocationState::Completed;
                crate::scheduler::finish(
                    &mut store,
                    network.space.as_str(),
                    "conversation",
                    &invocation,
                )?;
            }
            drop(node);
            let mut cfg = Config::load(&network.directory)?;
            cfg.manager = Some(settings.clone());
            cfg.save(&network.directory)?;
            phase = "reopen before continuation";
            let node = Arc::new(Runtime::open(&network.directory).await?);
            phase = "admit and drain the continuation";
            node.flush_completions(network.space.as_str()).await?;
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    if node
                        .store
                        .lock()
                        .await
                        .continuation(network.space.as_str(), &hash, "v2")
                        .unwrap()
                        .is_some_and(|record| {
                            matches!(record.state.as_str(), "completed" | "failed" | "unknown")
                        })
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            let first = node
                .store
                .lock()
                .await
                .continuation(network.space.as_str(), &hash, "v2")?
                .context("followup claim missing")?;
            assert_eq!(
                first.state, "failed",
                "a pre-SDK failure became an uncertain execution"
            );
            assert_eq!(
                crate::scheduler::status(&node, network.space.as_str(), "conversation", &hash)
                    .await?["state"],
                "failed"
            );
            node.flush_completions(network.space.as_str()).await?;
            assert_eq!(
                node.store
                    .lock()
                    .await
                    .continuation(network.space.as_str(), &hash, "v2")?
                    .unwrap(),
                first
            );
            let session = {
                let mut store = node.store.lock().await;
                assert!(
                    store
                        .local_value(network.space.as_str(), "completion:followup")?
                        .is_none()
                );
                assert_eq!(
                    store.connection.query_row(
                        "SELECT count(*) FROM accepted_authorizations WHERE action='agent_start'",
                        [],
                        |row| row.get::<_, u64>(0)
                    )?,
                    2
                );
                network.message(
                    &mut store,
                    "conversation",
                    "new context after the admitted continuation",
                )?;
                store.agent_grant(
                    network.space.as_str(),
                    "conversation",
                    &manager.service,
                    &SessionRef {
                        target: manager.device.clone(),
                        session_id: hash.clone(),
                    },
                    &hash,
                    now_ms() as u64,
                )?
            };
            phase = "retry the original signed Manager snapshot";
            let retried = node
                .claim_manager(
                    &session,
                    &hash,
                    crate::scheduler::boot_id(),
                    &crypto::digest(&serde_json::to_vec(&settings)?),
                )
                .await?;
            assert!(
                !retried.fresh,
                "the original continuation was admitted again after context advanced"
            );
            assert!(retried.invocation.state != crate::scheduler::InvocationState::Running);
            let mut another_task = network.body("create_task", "conversation");
            another_task["task_id"] = json!("unrelated");
            assert!(node.authorize_agent(&session, another_task).await.is_err());
            phase = "create the other legitimate task session";
            let other = queued(&network, &node, "another-task", "codex").await?;
            let mut another_session = network.body("session", "conversation");
            another_session["target_node"] = json!(request.target_node);
            another_session["session_id"] = json!(other.session_id);
            another_session["sdk_session"] = json!(session.sdk_session);
            another_session["parent_invocation"] = json!(session.parent_invocation);
            assert!(
                node.authorize_agent(&session, another_session)
                    .await
                    .is_err()
            );
            phase = "cancel the task";
            change_task(&network, &node, "followup", "cancelled").await?;
            assert!(
                node.store
                    .lock()
                    .await
                    .refresh_agent_session(&session, now_ms() as u64)
                    .is_err()
            );
            phase = "revoke the original account grant";
            network.revoke_client(&mut *node.store.lock().await)?;
            drop(node);
            phase = "reopen signed history after revocation";
            let reopened = Runtime::open(&network.directory).await?;
            assert_eq!(
                reopened
                    .store
                    .lock()
                    .await
                    .continuation(network.space.as_str(), &hash, "v2")?
                    .unwrap(),
                first
            );
            assert!(reopened._codex_process.lock().await.is_none());
            Ok(())
        }
        .await;
        outcome.with_context(|| format!("continuation regression failed during: {phase}"))
    }

    #[tokio::test]
    async fn agent_cannot_reopen_or_start_new_work_for_a_user_finalized_task() -> Result<()> {
        let (network, node) = runtime_fixture().await?;
        let session = agent(&network, &node).await?;
        let request = queued(&network, &node, "finalized", "codex").await?;
        claim(&network, &node, &request).await?;
        node.finish(
            &request,
            OperationState::Succeeded,
            json!({"result":"done"}),
        )
        .await?;
        for state in ["accepted", "cancelled"] {
            change_task(&network, &node, "finalized", state).await?;
            let update =
                json!({"task_id":"finalized","status":"working","detail":"agent tries to reopen"});
            assert!(
                node.tool(
                    session.clone(),
                    "update_task",
                    update,
                    &uuid::Uuid::new_v4().to_string()
                )
                .await
                .is_err()
            );
            let open = json!({"task_id":"finalized","operation_id":format!("new-{state}"),"session_id":format!("new-{state}"),"target_node":node.identity.node_id()?,"executor":"codex","cwd":network.project});
            assert!(
                node.tool(
                    session.clone(),
                    "open_execution",
                    open,
                    &uuid::Uuid::new_v4().to_string()
                )
                .await
                .is_err()
            );
            assert!(
                node.store
                    .lock()
                    .await
                    .operation(network.space.as_str(), &format!("new-{state}"))?
                    .is_none()
            );
            change_task(&network, &node, "finalized", "working").await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn agent_cannot_silently_detach_a_session_from_its_latest_admitted_task() -> Result<()> {
        let (network, node) = runtime_fixture().await?;
        let request = queued(&network, &node, "associated", "codex").await?;
        let session = agent(&network, &node).await?;
        let before = operation(&network, &node, "associated").await?;
        let mut detached = request.clone();
        detached.operation_id = "detached".into();
        detached.task_id = None;
        let mut body = network.body("operate", "conversation");
        body["authorization_id"] = json!(detached.operation_id);
        body["request"] = json!(detached);
        body["sdk_session"] = json!(session.sdk_session);
        body["parent_invocation"] = json!(session.parent_invocation);
        let result = node.authorize_agent(&session, body.clone()).await;
        if let Ok(access) = result {
            assert!(node.rpc(access, body).await.is_err());
        }
        assert!(
            node.store
                .lock()
                .await
                .operation(network.space.as_str(), "detached")?
                .is_none()
        );
        assert_eq!(
            operation(&network, &node, "associated").await?.request,
            before.request
        );
        assert_eq!(
            node.store.lock().await.task_binding(
                network.space.as_str(),
                &request.binding.as_ref().unwrap().binding.task_ref
            )?,
            request.binding
        );
        Ok(())
    }

    #[tokio::test]
    async fn peer_mutations_and_replica_reads_require_signed_resource_authority() -> Result<()> {
        let (network, node) = runtime_fixture().await?;
        let origin = node.identity.node_id()?;
        for payload in [
            json!({"type":"schedule","human":true}),
            json!({"type":"execute","human":true}),
            json!({"type":"event","hash":"x"}),
            json!({"type":"blob","hash":"x","offset":0}),
        ] {
            let message = Inbound {
                network_id: network.space.to_string(),
                origin: origin.clone(),
                request_id: uuid::Uuid::new_v4().to_string(),
                payload,
                broadcast: false,
                verification: SignatureVerification::new(),
            };
            assert!(node.handle_peer(&message).await.is_err());
        }
        let body = network.body("conversations", "conversation");
        let auth = access(&network, &node, &body).await?;
        let mut invocation = auth.authorization.signed_invocation().payload.clone();
        invocation.presenter = node.audience()?.device;
        let invocation = Signed::sign(invocation, &network.client)?;
        let message = Inbound {
            network_id: network.space.to_string(),
            origin: origin.clone(),
            request_id: "actual-peer-read".into(),
            payload: json!({"type":"rpc","authorization_body":body,"invocation":invocation}),
            broadcast: false,
            verification: SignatureVerification::new(),
        };
        assert_eq!(
            node.handle_peer(&message).await?["conversations"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let mut changed = message.clone();
        changed.payload["authorization_body"]["resource_id"] =
            json!(ResourceId::from_bytes([42; 32]));
        assert!(node.handle_peer(&changed).await.is_err());
        assert!(
            node.store
                .lock()
                .await
                .operations(network.space.as_str())?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_message_retries_record_one_trigger() -> Result<()> {
        let (network, node) = runtime_fixture().await?;
        let mut body = network.body("message", "conversation");
        body["message_id"] = json!("same-message");
        body["text"] = json!("same goal");
        body["scheduler_node"] = json!(node.identity.node_id()?);
        let access = access(&network, &node, &body).await?;
        let barrier = Arc::new(tokio::sync::Barrier::new(16));
        let mut calls = vec![];
        for _ in 0..16 {
            let node = node.clone();
            let access = access.clone();
            let body = body.clone();
            let barrier = barrier.clone();
            calls.push(tokio::spawn(async move {
                barrier.wait().await;
                node.rpc(access, body).await
            }));
        }
        let mut triggers = BTreeSet::new();
        for call in calls {
            triggers.insert(call.await??["trigger"].as_str().unwrap().to_owned());
        }
        assert_eq!(triggers.len(), 1);
        assert_eq!(
            node.store
                .lock()
                .await
                .messages(network.space.as_str(), "conversation")?
                .iter()
                .filter(|message| message.message_id == "same-message")
                .count(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn task_retry_recovers_missing_grant_once_without_reopening() -> Result<()> {
        let (network, node) = runtime_fixture().await?;
        for (name, state) in [
            ("active", "open"),
            ("accepted", "accepted"),
            ("cancelled", "cancelled"),
        ] {
            task(&network, &node, name, false).await?;
            if state != "open" {
                change_task(&network, &node, name, state).await?;
            }
        }
        drop(node);
        let node = Arc::new(Runtime::open(&network.directory).await?);
        for _ in 0..2 {
            for name in ["active", "accepted", "cancelled"] {
                task(&network, &node, name, true).await?;
            }
        }
        let store = node.store.lock().await;
        assert!(followup_authorized(
            &store,
            network.space.as_str(),
            "conversation",
            "active"
        )?);
        assert!(!followup_authorized(
            &store,
            network.space.as_str(),
            "conversation",
            "accepted"
        )?);
        assert!(!followup_authorized(
            &store,
            network.space.as_str(),
            "conversation",
            "cancelled"
        )?);
        assert_eq!(store.context(network.space.as_str(),"conversation",&[])?.events.iter().filter(|event|matches!(&event.body,EventBody::DecisionRecorded{decision_id,..} if decision_id.starts_with("auto:"))).count(),1);
        assert_eq!(
            store
                .tasks(network.space.as_str(), "conversation")?
                .iter()
                .find(|task| task.task_id == "accepted")
                .unwrap()
                .status,
            "accepted"
        );
        drop(store);
        task(&network, &node, "wrongpayload", false).await?;
        let before = node
            .store
            .lock()
            .await
            .context(network.space.as_str(), "conversation", &[])?
            .events
            .len();
        for empty in [false, true] {
            let mut body = network.body("create_task", "conversation");
            let task_id = if empty { "active" } else { "wrongpayload" };
            body["task_id"] = json!(task_id);
            body["title"] = json!(task_id);
            body["goal"] = json!(if empty {
                "complete the requested work"
            } else {
                "substitute another task goal"
            });
            body["acceptance"] = json!("the user verifies the result");
            body["auto_followup"] = json!(true);
            let access = access(&network, &node, &body).await?;
            let events = if empty {
                vec![]
            } else {
                vec![(
                    "conversation".into(),
                    EventBody::DecisionRecorded {
                        decision_id: format!("auto:{task_id}"),
                        text: format!(
                            "User enabled bounded automatic follow-up for task {task_id}"
                        ),
                        effective_constraints: vec![format!("auto_followup:{task_id}")],
                        tool_configuration: json!({"auto_followup":task_id}),
                    },
                    vec![],
                )]
            };
            let mut store = node.store.lock().await;
            assert!(
                store
                    .append_request_authorized_events(
                        &node.identity,
                        &access.authorization,
                        &access.freshness,
                        &access.transport,
                        &body,
                        events,
                        now_ms()
                    )
                    .is_err()
            );
            let accepted:bool=store.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM accepted_authorizations WHERE space_id=? AND authorization_id=?)",
                rusqlite::params![network.space.as_str(),body["authorization_id"].as_str().unwrap()],|row|row.get(0))?;
            assert!(!accepted);
            assert_eq!(
                store
                    .context(network.space.as_str(), "conversation", &[])?
                    .events
                    .len(),
                before
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn service_decision_text_cannot_grant_account_followup_authority() -> Result<()> {
        let (network, node) = runtime_fixture().await?;
        task(&network, &node, "spoofed", false).await?;
        let session = agent(&network, &node).await?;
        let text = "User enabled bounded automatic follow-up for task spoofed";
        node.record_agent_result(&session,json!({
            "authorization_id":"spoofed-followup-result","decision_id":"auto:spoofed","text":text,
            "effective_constraints":["auto_followup:spoofed"],"tool_configuration":{"auto_followup":"spoofed"},
            "message_id":"actual-manager-result","output":"normal management output","blobs":[],
        })).await?;
        {
            let store = node.store.lock().await;
            let events = store
                .context(network.space.as_str(), "conversation", &[])?
                .events;
            assert_eq!(events.iter().filter(|event|matches!(&event.body,EventBody::DecisionRecorded{decision_id,..} if decision_id=="auto:spoofed")
                && matches!(&event.cause,EventCause::Invocation{admission} if matches!(admission.invocation.payload.actor,Actor::Service(_)))).count(),1);
            assert!(!followup_authorized(
                &store,
                network.space.as_str(),
                "conversation",
                "spoofed"
            )?);
        }
        task(&network, &node, "spoofed", true).await?;
        let mut store = node.store.lock().await;
        assert!(followup_authorized(
            &store,
            network.space.as_str(),
            "conversation",
            "spoofed"
        )?);
        let events = store
            .context(network.space.as_str(), "conversation", &[])?
            .events;
        assert_eq!(events.iter().filter(|event|matches!(&event.body,EventBody::TaskCreated{task_id,..} if task_id=="spoofed")).count(),1);
        assert_eq!(events.iter().filter(|event|matches!(&event.body,EventBody::DecisionRecorded{decision_id,..} if decision_id=="auto:spoofed")
            && matches!(&event.cause,EventCause::Invocation{admission} if matches!(admission.invocation.payload.actor,Actor::Account(_)))).count(),1);
        network.revoke_client(&mut store)?;
        assert!(!followup_authorized(
            &store,
            network.space.as_str(),
            "conversation",
            "spoofed"
        )?);
        Ok(())
    }

    #[tokio::test]
    async fn completion_receipt_recovers_history_and_notifies_once_after_reopen() -> Result<()> {
        let (network, node) = runtime_fixture().await?;
        let request = queued(&network, &node, "done", "codex").await?;
        claim(&network, &node, &request).await?;
        let result = json!({"result":"verified completion"});
        {
            let mut store = node.store.lock().await;
            store.local_set(
                network.space.as_str(),
                "completion:done",
                &json!({"state":"succeeded","result":result}),
            )?;
            store.finish_operation(
                network.space.as_str(),
                "done",
                OperationState::Succeeded,
                result,
            )?;
        }
        drop(node);
        let node = Arc::new(Runtime::open(&network.directory).await?);
        node.flush_completions(network.space.as_str()).await?;
        node.flush_completions(network.space.as_str()).await?;
        let store = node.store.lock().await;
        let events = store
            .context(network.space.as_str(), "conversation", &[])?
            .events;
        assert_eq!(events.iter().filter(|event|matches!(&event.body,EventBody::ExecutionObserved{operation_id,status,..} if operation_id=="done" && status=="succeeded")).count(),1);
        assert_eq!(
            store
                .pending_notifications(network.space.as_str(), "client")?
                .len(),
            1
        );
        assert!(
            store
                .local_value(network.space.as_str(), "completion:done")?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_claude_spawn_does_not_leave_a_nonexistent_native_session() -> Result<()> {
        let (network, node) = runtime_fixture().await?;
        let request = queued(&network, &node, "not-started", "claude_code").await?;
        {
            let mut store = node.store.lock().await;
            let mut session = store
                .session(
                    network.space.as_str(),
                    &request.target_node,
                    &request.session_id,
                )?
                .unwrap();
            session.native_session_id = None;
            store.upsert_session(network.space.as_str(), &session)?;
        }
        node.clone().execute_operation(request.clone()).await?;
        assert_eq!(
            operation(&network, &node, &request.operation_id)
                .await?
                .state,
            OperationState::Failed
        );
        assert!(
            node.store
                .lock()
                .await
                .session(
                    network.space.as_str(),
                    &request.target_node,
                    &request.session_id
                )?
                .unwrap()
                .native_session_id
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn reconciliation_reports_only_the_executors_confirmed_absence() -> Result<()> {
        let (network, node) = runtime_fixture().await?;
        let local = node.identity.node_id()?;
        let result = node
            .reconcile(network.space.as_str(), &local, "never-received")
            .await?;
        assert_eq!(result["state"], "not_found");
        assert_eq!(result["target_node"], local);
        assert!(result["operation"].is_null());
        assert!(
            result["checked_at_ms"]
                .as_i64()
                .is_some_and(|value| value > 0)
        );
        assert!(
            node.reconcile(network.space.as_str(), "offline-node", "never-received")
                .await
                .is_err()
        );
        let request = queued(&network, &node, "reconcile-only", "codex").await?;
        claim(&network, &node, &request).await?;
        node.finish(
            &request,
            OperationState::Unknown,
            json!({"error":"unknown outcome"}),
        )
        .await?;
        let before = operation(&network, &node, &request.operation_id).await?;
        let snapshot = {
            let store = node.store.lock().await;
            (
                serde_json::to_value(store.context(
                    network.space.as_str(),
                    "conversation",
                    &[],
                )?)?,
                store.connection.query_row(
                    "SELECT count(*) FROM authorization_nonces",
                    [],
                    |row| row.get::<_, u64>(0),
                )?,
                store.connection.query_row(
                    "SELECT count(*) FROM resource_reservations",
                    [],
                    |row| row.get::<_, u64>(0),
                )?,
            )
        };
        let mut body = network.body("reconcile", "conversation");
        body["target_node"] = json!(request.target_node);
        body["operation_id"] = json!(request.operation_id);
        let access = access(&network, &node, &body).await?;
        assert_eq!(access.freshness.challenge.stage, CheckStage::RemoteRead);
        let changes = node.store.lock().await.connection.total_changes();
        let observed = node.rpc(access, body).await?;
        assert_eq!(observed["operation"]["state"], "unknown");
        let store = node.store.lock().await;
        assert_eq!(
            store
                .operation(network.space.as_str(), &request.operation_id)?
                .unwrap(),
            before
        );
        assert_eq!(
            serde_json::to_value(store.context(network.space.as_str(), "conversation", &[])?)?,
            snapshot.0
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM authorization_nonces", [], |row| row
                    .get::<_, u64>(
                    0
                ))?,
            snapshot.1
        );
        assert_eq!(
            store.connection.query_row(
                "SELECT count(*) FROM resource_reservations",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            snapshot.2
        );
        assert_eq!(
            store.connection.total_changes(),
            changes,
            "reconciliation changed persistent state"
        );
        drop(store);
        assert!(node._codex_process.lock().await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn retrying_an_undelivered_message_reuses_its_event_and_attempts_scheduling() -> Result<()>
    {
        let (network, mut node) = runtime_fixture().await?;
        Arc::get_mut(&mut node)
            .unwrap()
            .config
            .roles
            .retain(|role| role != &NodeRole::Scheduler);
        Arc::get_mut(&mut node).unwrap().config.manager = Some(config::ManagerSettings {
            provider: "codex_sdk".into(),
            model: "unused".into(),
        });
        let mut body = network.body("message", "conversation");
        body["message_id"] = json!("retry-message");
        body["text"] = json!("test");
        body["scheduler_node"] = json!(node.identity.node_id()?);
        let original = access(&network, &node, &body).await?;
        assert!(node.rpc(original.clone(), body.clone()).await.is_err());
        let first = node
            .store
            .lock()
            .await
            .messages(network.space.as_str(), "conversation")?;
        assert_eq!(first.len(), 1);
        assert!(node.rpc(original, body.clone()).await.is_err());
        let second = node
            .store
            .lock()
            .await
            .messages(network.space.as_str(), "conversation")?;
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].event_hash, first[0].event_hash);
        body["authorization_id"] = json!("other-scheduler");
        body["scheduler_node"] = json!("different-node");
        assert!(rpc(&network, &node, body).await.is_err());
        assert_eq!(
            node.store
                .lock()
                .await
                .messages(network.space.as_str(), "conversation")?
                .len(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn completion_requires_the_exact_persisted_turn_and_preserves_unknown() -> Result<()> {
        let (network, node) = runtime_fixture().await?;
        let request = queued(&network, &node, "uncertain", "codex").await?;
        claim(&network, &node, &request).await?;
        node.observe_engine(derived_result(
            network.space.as_str(),
            &network.resource,
            &request,
            "unrelated",
        ))
        .await?;
        assert_eq!(
            operation(&network, &node, "uncertain").await?.state,
            OperationState::Running
        );
        node.store.lock().await.local_set(
            network.space.as_str(),
            "turn:uncertain",
            &json!({"turn":{"id":"expected"}}),
        )?;
        node.observe_engine(derived_result(
            network.space.as_str(),
            &network.resource,
            &request,
            "unrelated",
        ))
        .await?;
        assert_eq!(
            operation(&network, &node, "uncertain").await?.state,
            OperationState::Running
        );
        let mut wrong = derived_result(
            network.space.as_str(),
            &network.resource,
            &request,
            "expected",
        );
        wrong["execution_context"]["resource_id"] = json!(ResourceId::from_bytes([7; 32]));
        assert!(node.observe_engine(wrong).await.is_err());
        assert_eq!(
            operation(&network, &node, "uncertain").await?.state,
            OperationState::Running
        );
        node.observe_engine(derived_result(
            network.space.as_str(),
            &network.resource,
            &request,
            "expected",
        ))
        .await?;
        assert_eq!(
            operation(&network, &node, "uncertain").await?.state,
            OperationState::Succeeded
        );
        let request = queued(&network, &node, "late-result", "codex").await?;
        claim(&network, &node, &request).await?;
        node.finish(
            &request,
            OperationState::Unknown,
            json!({"error":"start response lost"}),
        )
        .await?;
        node.store.lock().await.local_set(
            network.space.as_str(),
            "turn:late-result",
            &json!({"turn":{"id":"late-turn"}}),
        )?;
        let before = operation(&network, &node, "late-result").await?;
        let changes = node.store.lock().await.connection.total_changes();
        node.observe_engine(derived_result(
            network.space.as_str(),
            &network.resource,
            &request,
            "late-turn",
        ))
        .await?;
        node.finish(&request, OperationState::Succeeded, json!({"late":true}))
            .await?;
        assert_eq!(operation(&network, &node, "late-result").await?, before);
        assert_eq!(node.store.lock().await.connection.total_changes(), changes);
        Ok(())
    }

    #[tokio::test]
    async fn accepted_recovery_rechecks_config_and_rejects_before_any_external_call() -> Result<()>
    {
        let (network, node) = runtime_fixture().await?;
        let request = queued(&network, &node, "queued", "codex").await?;
        drop(node);
        let mut cfg = Config::load(&network.directory)?;
        cfg.roles.retain(|role| role != &NodeRole::Executor);
        cfg.save(&network.directory)?;
        let node = Arc::new(Runtime::open(&network.directory).await?);
        assert!(node.clone().execute_operation(request).await.is_err());
        assert_eq!(
            operation(&network, &node, "queued").await?.state,
            OperationState::Rejected
        );
        assert!(node._codex_process.lock().await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn stale_accepted_send_is_rejected_after_a_control_advances_revision() -> Result<()> {
        let (network, node) = runtime_fixture().await?;
        let request = queued(&network, &node, "send", "codex").await?;
        let mut control = request.clone();
        control.operation_id = "control".into();
        control.resources.clear();
        control.action = json!({"type":"interrupt","turn_id":"turn"});
        admit(&network, &node, &control).await?;
        claim(&network, &node, &control).await?;
        node.finish(
            &control,
            OperationState::Succeeded,
            json!({"interrupted":"turn"}),
        )
        .await?;
        assert!(node.clone().execute_operation(request).await.is_err());
        assert_eq!(
            operation(&network, &node, "send").await?.state,
            OperationState::Rejected
        );
        assert!(node._codex_process.lock().await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn maintenance_waits_for_local_outcomes_and_completion_but_not_remote_operations()
    -> Result<()> {
        let (network, node) = runtime_fixture().await?;
        node.maintenance_idle().await?;
        let request = queued(&network, &node, "local", "codex").await?;
        assert!(
            node.maintenance_idle()
                .await
                .unwrap_err()
                .downcast_ref::<Busy>()
                .is_some()
        );
        claim(&network, &node, &request).await?;
        assert!(node.maintenance_idle().await.is_err());
        node.finish(
            &request,
            OperationState::Unknown,
            json!({"error":"unresolved"}),
        )
        .await?;
        assert!(node.maintenance_idle().await.is_err());
        node.finish(
            &request,
            OperationState::Succeeded,
            json!({"result":"verified"}),
        )
        .await?;
        node.maintenance_idle().await?;
        node.store.lock().await.local_set(
            network.space.as_str(),
            "completion:local",
            &json!({"state":"succeeded","result":{}}),
        )?;
        assert!(node.maintenance_idle().await.is_err());
        node.store
            .lock()
            .await
            .local_delete(network.space.as_str(), "completion:local")?;
        let remote = TestNetwork::new()?;
        let mut store = node.store.lock().await;
        remote.install(&mut store)?;
        remote.conversation(&mut store, "conversation")?;
        let request = remote.request(&mut store, "remote", "remote")?;
        remote.admit(&mut store, &request)?;
        drop(store);
        node.maintenance_idle().await?;
        Ok(())
    }
    mod remote_sdk {
        use super::*;
        use crate::{
            binding::SignedBinding,
            identity::{
                execution_dispatch::tests::Fixture,
                foreign_execution_policy::{
                    current_task_prepare_policy_in, task_policy_for_binding_in,
                    tests::{
                        UnseededFixture, import_current_policy, unseeded_fixture_with_context,
                    },
                },
                test_support::now as fixture_now,
            },
            network::{MAX_FRAME_BYTES, Service},
        };
        use vibeface_secure::authorization::{
            Action, EventRef, InvocationContext, JournalEntry,
            execution_handoff::*,
            node_execution_policy::{PolicyCarrierOrigin, SdkPolicySourceProvenance},
            prepare::{PrepareContext, PrepareIntent, VerifiedRemoteExecution},
            remote_execution_policy::RemoteExecutionPolicySelection,
        };

        fn checked<T>(phase: &str, action: impl FnOnce() -> Result<T>) -> Result<T> {
            let signatures = SignatureVerification::new();
            let result = signatures.run(action).with_context(|| phase.to_owned())?;
            ensure!(
                signatures.counts().actual_verifications <= 32,
                "remote SDK fixture {phase} exceeded its crypto budget"
            );
            Ok(result)
        }

        struct Peers(Vec<tokio::task::JoinHandle<()>>);

        impl Drop for Peers {
            fn drop(&mut self) {
                for task in &self.0 {
                    task.abort();
                }
            }
        }

        struct Remote {
            peers: Peers,
            node: Arc<Runtime>,
            c: Arc<Mutex<Store>>,
            c_runtime: Arc<Runtime>,
            task_creator: Option<TestNetwork>,
            project: TestNetwork,
            executor: TestNetwork,
            execution: ResourceRef,
            grant: ObjectHash,
            binding: SignedBinding,
        }

        impl Remote {
            async fn new() -> Result<Self> {
                Box::pin(Self::from_fixture(false)).await
            }

            async fn from_fixture(foreign_task: bool) -> Result<Self> {
                let project = TestNetwork::new()?;
                let executor = TestNetwork::new()?;
                let mut store = project.at(&project.directory.join("foreign-approval.sqlite3"))?;
                project.conversation(&mut store, "conversation")?;
                let task_creator = foreign_task.then(TestNetwork::new).transpose()?;
                let task = if let Some(creator) = &task_creator {
                    assert_ne!(creator.account, executor.account);
                    assert_ne!(creator.account, project.account);
                    creator.install(&mut store)?;
                    Fixture::foreign_account_task(&project, creator, &mut store)?.0
                } else {
                    project.task(&mut store, "conversation", "remote-task")?
                };
                let target_key = PublicKey::parse(&executor.node.public_key_hex()?)?;
                let context = PrepareContext {
                    context: InvocationContext {
                        conversation: Some("conversation".into()),
                        task: Some(EventRef::parse(&task.hash)?),
                        execution_session: Some(SessionRef {
                            target: DeviceId::from_key(&target_key),
                            session_id: "remote-session".into(),
                        }),
                        executor_kind: Some("codex".into()),
                        workspace_resource: Some(project.resource.clone()),
                        sdk_session: None,
                    },
                    binding_generation: Some(1),
                    canonical_cwd: Some(executor.project.to_string_lossy().into_owned()),
                    execution_target_public_key: Some(target_key),
                };
                let mut fixture =
                    unseeded_fixture_with_context(project, executor, store, context.clone(), true)?;
                let remote = import_current_policy(&mut fixture)?;
                let binding = Self::bind(&mut fixture, &remote)?;
                assert!(
                    fixture
                        .store
                        .authorization_state()
                        .account(&fixture.executor.account)
                        .is_none()
                );
                let UnseededFixture {
                    project,
                    executor,
                    store,
                    mut target,
                    intent,
                    ..
                } = fixture;
                let execution = intent.payload.selection.execution_resource.clone();
                let grant = intent.payload.selection.execution_grant.clone();
                store
                    .connection
                    .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
                drop(store);
                std::fs::rename(
                    project.directory.join("foreign-approval.sqlite3"),
                    project.directory.join("node.sqlite3"),
                )?;
                project.install(&mut target)?;
                target.local_set(
                    executor.space.as_str(),
                    &format!("resource-root:{}", execution.resource),
                    &json!(executor.project),
                )?;
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let url = format!("ws://{}/api/network/ws", listener.local_addr()?);
                Self::configure(&executor, vec![], vec![url.clone()])?;
                drop(executor.at(&executor.directory.join("node.sqlite3"))?);
                let c_runtime = Arc::new(Runtime::open(&executor.directory).await?);
                *c_runtime.store.lock().await = target;
                Self::configure(&project, vec![url], vec![])?;
                let node = Arc::new(
                    Runtime::open(&project.directory)
                        .await
                        .context("reopen the real bound Project database")?,
                );
                assert!(
                    node.store
                        .lock()
                        .await
                        .authorization_state()
                        .account(&executor.account)
                        .is_none()
                );
                let mut value = Self {
                    peers: Peers(vec![]),
                    node,
                    c: c_runtime.store.clone(),
                    c_runtime,
                    task_creator,
                    project,
                    executor,
                    execution,
                    grant,
                    binding,
                };
                value.assert_cold_current(Action::ExecutionOpen).await?;
                value.connect(listener).await?;
                Ok(value)
            }

            fn bind(
                fixture: &mut UnseededFixture,
                remote: &VerifiedRemoteExecution,
            ) -> Result<SignedBinding> {
                let context = &fixture.intent.payload.context;
                let selection = &fixture.intent.payload.selection;
                let local = PublicKey::parse(&fixture.project.node.public_key_hex()?)?;
                let body = json!({
                    "method":"bind_execution",
                    "space_id":fixture.project.space,
                    "resource_id":fixture.project.resource,
                    "authorization_id":"remote-sdk-bind",
                    "binding_generation":1,
                    "execution_resource":selection.execution_resource,
                    "target_public_key":selection.target_public_key,
                    "execution_grant":selection.execution_grant,
                    "conversation_id":"conversation",
                    "task_id":"remote-task",
                    "task_ref":context.context.task,
                    "target_node":fixture.executor.node.node_id()?,
                    "session_id":"remote-session",
                    "executor":"codex",
                    "cwd":fixture.executor.project,
                });
                let prepared = checked("native bind prepare", || {
                    remote
                        .prepare(
                            &fixture.source,
                            &body.to_string(),
                            &serde_json::to_string(context)?,
                            "execution_open",
                            "remote-sdk-bind",
                            local.as_str(),
                            &fixture.project.client,
                            fixture_now() as f64,
                        )
                        .map_err(anyhow::Error::msg)
                })?;
                let prepared: Signed<PrepareIntent> = serde_json::from_str(&prepared)?;
                checked("current bind prepare policy", || {
                    current_task_prepare_policy_in(
                        &fixture.store.connection,
                        &fixture.store.policy,
                        &local,
                        &prepared,
                        &body,
                        fixture_now(),
                    )?
                    .context("SDK fixture bind has no completed foreign policy")
                })?;
                let invocation = checked("native bind sign", || {
                    remote
                        .sign(
                            &fixture.source,
                            &serde_json::to_string(&prepared.payload.request)?,
                            &body.to_string(),
                            "execution_open",
                            "remote-sdk-bind",
                            local.as_str(),
                            &fixture.project.client,
                            fixture_now() as f64,
                        )
                        .map_err(anyhow::Error::msg)
                })?;
                let invocation: Signed<Invocation> = serde_json::from_str(&invocation)?;
                let value = &invocation.payload;
                let facts = vibeface_secure::authorization::RequestFacts {
                    resource: value.resource.clone(),
                    action: value.action,
                    audience: value.audience.clone(),
                    operation_id: value.operation_id.clone(),
                    body_hash: value.body_hash.clone(),
                    context: value.context.clone(),
                };
                let transport = TransportEvidence::DirectNoise {
                    device: DeviceId::from_key(&PublicKey::of(&fixture.project.client)),
                };
                let scoped = checked("scoped bind authorization", || {
                    fixture.store.authorize_scoped_request_for_body(
                        &fixture.project.node,
                        &invocation,
                        &facts,
                        &transport,
                        &body,
                        fixture_now(),
                    )
                })?;
                let dispatch = checked("scoped bind facts", || {
                    fixture.store.execution_dispatch_facts_in_scope(
                        &scoped,
                        &body,
                        scoped.audience(),
                        fixture_now(),
                    )
                })?;
                let challenge = checked("scoped bind challenge", || {
                    fixture
                        .store
                        .issue_scoped_freshness(&scoped, CheckStage::Admit, fixture_now())
                })?;
                let mut receipts = checked("B bind receipt", || {
                    fixture.store.sign_current_receipts(
                        &challenge,
                        &fixture.project.node,
                        fixture_now(),
                    )
                })?;
                receipts.extend(checked("C bind receipt", || {
                    fixture.target.sign_current_receipts(
                        &challenge,
                        &fixture.executor.node,
                        fixture_now(),
                    )
                })?);
                checked("persist scoped bind", || {
                    fixture.store.bind_execution_dispatch(
                        &fixture.project.node,
                        &dispatch,
                        &scoped,
                        &FreshnessProof {
                            challenge,
                            receipts,
                        },
                        &transport,
                        &body,
                        fixture_now(),
                    )
                })
            }

            fn configure(
                network: &TestNetwork,
                bootstrap_urls: Vec<String>,
                advertise_urls: Vec<String>,
            ) -> Result<()> {
                config::private_write(
                    &network.directory.join("data-epoch.json"),
                    &serde_json::to_vec(&config::data_format())?,
                )?;
                let mut cfg = Config::local(
                    "127.0.0.1:4790".parse()?,
                    vec![network.project.clone()],
                    false,
                );
                cfg.codex_socket = Some(network.directory.join("unavailable.sock"));
                cfg.claude_bin = network.directory.join("missing-claude");
                cfg.codex_bin = network.directory.join("missing-codex");
                cfg.network.bootstrap_urls = bootstrap_urls;
                cfg.network.advertise_urls = advertise_urls;
                cfg.save(&network.directory)?;
                config::private_write(
                    &network.directory.join("identity.json"),
                    &serde_json::to_vec(&network.node)?,
                )
            }

            async fn assert_cold_current(&self, action: Action) -> Result<()> {
                let store = self.node.store.lock().await;
                assert!(
                    store
                        .authorization_state()
                        .account(&self.executor.account)
                        .is_none()
                );
                let error = checked("cold Task Current lookup", || {
                    task_policy_for_binding_in(
                        &store.connection,
                        &store.policy,
                        &PublicKey::parse(&self.project.node.public_key_hex()?)?,
                        &self.binding,
                        &ResourceRef {
                            space: self.project.space.clone(),
                            resource: self.project.resource.clone(),
                        },
                        &RemoteExecutionPolicySelection {
                            execution_resource: self.execution.clone(),
                            target_public_key: PublicKey::parse(
                                &self.executor.node.public_key_hex()?,
                            )?,
                            execution_grant: self.grant.clone(),
                        },
                        action,
                        now_ms() as u64,
                    )
                })
                .err()
                .context("reopened SDK fixture unexpectedly restored Task Current")?;
                assert!(
                    error
                        .root_cause()
                        .to_string()
                        .starts_with("NeedPolicySync:")
                );
                Ok(())
            }

            fn primary_journal(store: &Store) -> Result<Vec<String>> {
                let mut query = store
                    .connection
                    .prepare("SELECT entry FROM policy_journal ORDER BY ordinal")?;
                Ok(query
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?)
            }

            fn sdk_sources(
                store: &Store,
                sdk: &AgentSession,
                operation_id: &str,
                body_hash: Option<&ObjectHash>,
            ) -> Result<usize> {
                let mut query = store.connection.prepare(
                    "SELECT value FROM identity_meta WHERE key GLOB 'task-execution-policy-v2:sdk-source:*'",
                )?;
                let mut count = 0;
                for value in query.query_map([], |row| row.get::<_, String>(0))? {
                    let source: SdkPolicySourceProvenance = serde_json::from_str(&value?)?;
                    if source.session.sdk_session == sdk.sdk_session
                        && source.child_facts.context.sdk_session.as_ref() == Some(&sdk.sdk_session)
                        && source.child_facts.operation_id == operation_id
                        && body_hash.is_none_or(|hash| source.child_facts.body_hash == *hash)
                    {
                        count += 1;
                    }
                }
                Ok(count)
            }

            async fn close(&mut self) {
                for task in self.peers.0.drain(..) {
                    task.abort();
                    let _ = task.await;
                }
            }

            async fn sync_policy(&self) -> Result<()> {
                let journal: Vec<JournalEntry> = {
                    let store = self.node.store.lock().await;
                    let mut query = store
                        .connection
                        .prepare("SELECT entry FROM policy_journal ORDER BY ordinal")?;
                    query
                        .query_map([], |row| row.get::<_, String>(0))?
                        .map(|row| Ok(serde_json::from_str(&row?)?))
                        .collect::<Result<_>>()?
                };
                self.c
                    .lock()
                    .await
                    .install_policy_journal(&journal, now_ms() as u64)
            }

            async fn connect(&mut self, listener: tokio::net::TcpListener) -> Result<()> {
                use axum::{
                    Router,
                    extract::{State, WebSocketUpgrade},
                    routing::get,
                };
                async fn upgrade(
                    State(hub): State<Arc<NetworkHub>>,
                    ws: WebSocketUpgrade,
                ) -> impl axum::response::IntoResponse {
                    ws.max_message_size(MAX_FRAME_BYTES)
                        .max_frame_size(MAX_FRAME_BYTES)
                        .on_upgrade(|socket| async move {
                            let _ = hub.accept(socket).await;
                        })
                }
                let router = Router::new()
                    .route("/api/network/ws", get(upgrade))
                    .with_state(self.c_runtime.network.clone());
                self.peers.0.push(tokio::spawn(async move {
                    let _ = axum::serve(listener, router).await;
                }));
                let mut receiver = self
                    .c_runtime
                    .network_rx
                    .lock()
                    .await
                    .take()
                    .context("C fixture network receiver is missing")?;
                let server = self.c_runtime.clone();
                self.peers.0.push(tokio::spawn(async move {
                    let mut clients = tokio::task::JoinSet::new();
                    while let Some(incoming) = receiver.recv().await {
                        assert_eq!(incoming.service, Service::Client);
                        let server = server.clone();
                        clients.spawn(async move {
                            let _ = crate::client::serve(server, incoming.socket).await;
                        });
                    }
                }));
                let target = self.executor.node.node_id()?;
                let records = self.node.network.resolve(&target).await?;
                assert!(!records.is_empty());
                assert!(
                    self.node
                        .hub
                        .online(self.project.space.as_str())
                        .await
                        .is_empty()
                );
                Ok(())
            }

            async fn dispatch(
                &self,
                sdk: &AgentSession,
                id: &str,
                previous: Option<&ExecutionHandoffResult>,
            ) -> Result<SignedExecutionHandoff> {
                let snapshot = previous.and_then(|value| value.receipt.payload.session.as_ref());
                let request = OperationRequest {
                    operation_id: id.into(),
                    network_id: self.project.space.to_string(),
                    conversation_id: "conversation".into(),
                    task_id: Some("remote-task".into()),
                    target_node: self.executor.node.node_id()?,
                    session_id: "remote-session".into(),
                    expected_revision: snapshot.map_or(0, |value| value.revision),
                    basis_heads: self
                        .node
                        .store
                        .lock()
                        .await
                        .heads(self.project.space.as_str(), "conversation")?,
                    action: if previous.is_some() {
                        json!({"type":"send","text":"continue this task"})
                    } else {
                        self.binding.binding.open_spec.clone()
                    },
                    resources: vec![
                        self.executor
                            .project
                            .to_str()
                            .context("C path is not UTF-8")?
                            .into(),
                    ],
                    binding: Some(self.binding.clone()),
                };
                let mut body = json!({"method":"operate","space_id":self.project.space,"resource_id":self.project.resource,
                    "authorization_id":id,"execution_resource":self.execution,"execution_grant":self.grant,
                    "target_public_key":self.executor.node.public_key_hex()?,"request":request,
                    "conversation_id":"conversation","sdk_session":sdk.sdk_session,"parent_invocation":sdk.parent_invocation});
                if let Some(previous) = previous {
                    body["execution_session_receipt"] = json!(previous.receipt);
                }
                self.sync_policy().await?;
                let (pins, journal) = {
                    let store = self.node.store.lock().await;
                    assert!(
                        store
                            .authorization_state()
                            .account(&self.executor.account)
                            .is_none()
                    );
                    assert_eq!(Self::sdk_sources(&store, sdk, id, None)?, 0);
                    (store.policy.pins().to_vec(), Self::primary_journal(&store)?)
                };
                let access = self.node.authorize_agent(sdk, body).await?;
                let mut store = self.node.store.lock().await;
                assert_eq!(store.policy.pins(), pins.as_slice());
                assert_eq!(Self::primary_journal(&store)?, journal);
                assert!(
                    store
                        .authorization_state()
                        .account(&self.executor.account)
                        .is_none()
                );
                let task = access
                    .authorization
                    .task_policy()
                    .context("SDK authorization did not refresh a Task Current policy")?;
                assert!(task.request().node_request().is_some());
                let target = PublicKey::parse(&self.executor.node.public_key_hex()?)?;
                assert!(matches!(
                    task.carrier_origin(),
                    PolicyCarrierOrigin::DirectNoise(peer) if peer.public_key() == &target
                ));
                assert_eq!(task.snapshot().issuer, target);
                assert_eq!(task.witness().issuer, target);
                assert!(!task.original_page_envelope_hashes().is_empty());
                assert!(
                    Self::sdk_sources(&store, sdk, id, Some(&ObjectHash::body(&access.body)?))? > 0
                );
                let wire = access.verification.run(|| {
                    let dispatch = store.execution_dispatch_facts_in_scope(
                        &access.authorization,
                        &access.body,
                        access.authorization.audience(),
                        now_ms() as u64,
                    )?;
                    store.accept_execution_request_dispatch(
                        &self.project.node,
                        &dispatch,
                        &access.authorization,
                        &access.freshness,
                        &access.transport,
                        &access.body,
                        &request,
                        now_ms() as u64,
                    )?;
                    let (wire, fresh) = store.prepare_execution_dispatch_handoff(
                        &self.project.node,
                        self.project.space.as_str(),
                        id,
                        now_ms() as u64,
                    )?;
                    ensure!(fresh, "SDK fixture did not create a new production handoff");
                    Ok(wire)
                })?;
                ensure!(
                    access.verification.counts().actual_verifications <= 32,
                    "native SDK authorization exceeded its crypto budget"
                );
                Ok(wire)
            }

            async fn c_freshness(
                &self,
                wire: &SignedExecutionHandoff,
                stage: CheckStage,
            ) -> Result<ExecutionHandoffFreshness> {
                let transport = TransportEvidence::VerifiedPeer {
                    origin: self.node.audience()?.device,
                };
                let challenge = self.c.lock().await.issue_execution_handoff_challenge(
                    wire,
                    &transport,
                    stage,
                    now_ms() as u64,
                )?;
                let mut receipts = self
                    .node
                    .store
                    .lock()
                    .await
                    .sign_execution_handoff_receipts(
                        &challenge,
                        &self.project.node,
                        now_ms() as u64,
                    )?;
                receipts.extend(self.c.lock().await.sign_execution_handoff_receipts(
                    &challenge,
                    &self.executor.node,
                    now_ms() as u64,
                )?);
                Ok(ExecutionHandoffFreshness {
                    challenge,
                    receipts,
                })
            }

            async fn proof(
                &self,
                wire: &SignedExecutionHandoff,
                proof: Option<&ExecutionHandoffFreshness>,
            ) -> Result<()> {
                let transport = TransportEvidence::VerifiedPeer {
                    origin: self.node.audience()?.device,
                };
                let usage = self.c.lock().await.execution_handoff_proof_usage(
                    wire,
                    &transport,
                    proof,
                    now_ms() as u64,
                )?;
                usage.enforce_limits()?;
                eprintln!(
                    "remote_sdk_proof_counts {}",
                    serde_json::to_string(&usage.counts())?
                );
                Ok(())
            }

            async fn execute_manual_c_fixture(
                &self,
                wire: SignedExecutionHandoff,
            ) -> Result<ExecutionHandoffResult> {
                self.sync_policy().await?;
                let transport = TransportEvidence::VerifiedPeer {
                    origin: self.node.audience()?.device,
                };
                let admit = self.c_freshness(&wire, CheckStage::Admit).await?;
                self.proof(&wire, Some(&admit)).await?;
                self.c.lock().await.accept_execution_handoff(
                    &self.executor.node,
                    &wire,
                    &admit,
                    &transport,
                    now_ms() as u64,
                )?;
                let start = self.c_freshness(&wire, CheckStage::Start).await?;
                let result = {
                    let mut c = self.c.lock().await;
                    let current = c.execution_handoff_authorization(
                        self.execution.space.as_str(),
                        self.project.space.as_str(),
                        &wire.invocation.payload.operation_id,
                        now_ms() as u64,
                    )?;
                    let (running, fresh, _) = c.claim_execution_handoff(
                        &self.executor.node,
                        &current,
                        &start,
                        now_ms() as u64,
                    )?;
                    ensure!(fresh, "SDK fixture C operation was already claimed");
                    let mut session = running
                        .execution_session
                        .context("C did not create its private session")?;
                    session.state = "idle".into();
                    session.native_session_id = Some("private-fixture-provider-session".into());
                    let done = c.finish_execution_handoff(
                        &self.executor.node,
                        self.execution.space.as_str(),
                        self.project.space.as_str(),
                        &wire.invocation.payload.operation_id,
                        OperationState::Succeeded,
                        json!({"fixture_completed":true}),
                        Some(&session),
                        now_ms() as u64,
                    )?;
                    assert_eq!(
                        c.connection
                            .query_row("SELECT COUNT(*) FROM events", [], |row| row
                                .get::<_, u64>(0))?,
                        0
                    );
                    ExecutionHandoffResult {
                        handoff: wire,
                        receipt: done.receipt.context("C fixture receipt is missing")?,
                        result: done.result,
                    }
                };
                self.node
                    .store
                    .lock()
                    .await
                    .apply_execution_handoff_receipt(
                        &self.project.node,
                        &result,
                        now_ms() as u64,
                    )?;
                Ok(result)
            }
        }

        #[tokio::test]
        async fn initial_sdk_discovers_its_accounts_bound_task_only() -> Result<()> {
            let mut remote = Box::pin(Remote::new()).await?;
            let outcome: Result<()> = async {
                let sdk = agent(&remote.project, &remote.node).await?;
                let discovered = remote.node.tool(
                    sdk.clone(),
                    "nodes",
                    json!({"task_id":"remote-task","executor":"codex","execution_action":"send"}),
                    "sdk-bound-targets",
                ).await?;
                let targets = discovered["execution_targets"].as_array()
                    .context("nodes tool omitted its execution targets")?;
                assert_eq!(targets.len(), 1);
                assert_eq!(targets[0]["execution_grant"], json!(remote.grant));
                assert_eq!(targets[0]["target_node"], remote.executor.node.node_id()?);
                assert_eq!(targets[0]["task_ref"], remote.binding.binding.task_ref);
                assert_eq!(targets[0]["binding_status"], "bound");
                assert_eq!(targets[0]["connection_status"], "verification_required");
                assert!(targets[0].get("ready").is_none());
                {
                    let mut store = remote.node.store.lock().await;
                    remote.project.task(&mut store, "conversation", "unbound")?;
                    remote.project.conversation(&mut store, "elsewhere")?;
                    remote.project.task(&mut store, "elsewhere", "other-conversation")?;
                }
                assert!(
                    remote.node
                        .tool(
                            sdk.clone(),
                            "nodes",
                            json!({"task_id":"unbound","executor":"codex","execution_action":"send"}),
                            "sdk-unbound-targets",
                        )
                        .await
                        .is_err()
                );
                assert!(
                    remote.node
                        .tool(
                            sdk.clone(),
                            "nodes",
                            json!({"task_id":"other-conversation","executor":"codex","execution_action":"send"}),
                            "sdk-other-conversation-targets",
                        )
                        .await
                        .is_err()
                );
                Ok(())
            }
            .await;
            remote.close().await;
            outcome
        }

        #[tokio::test]
        async fn initial_sdk_rejects_a_foreign_accounts_bound_task() -> Result<()> {
            let mut remote = Box::pin(Remote::from_fixture(true)).await?;
            let outcome: Result<()> = async {
                let sdk = agent(&remote.project, &remote.node).await?;
                {
                    let store = remote.node.store.lock().await;
                    let creation = store.task_creation(remote.project.space.as_str(), "conversation", "remote-task")?;
                    let event = store.verify_event(remote.project.space.as_str(), &creation)?;
                    let EventCause::Invocation { admission } = event.cause else {
                        bail!("foreign Task has no actual Account admission");
                    };
                    assert_eq!(admission.invocation.payload.actor, Actor::Account(remote.task_creator.as_ref().context("foreign Task creator is missing")?.account.clone()));
                    assert_ne!(admission.invocation.payload.actor, sdk.initiator);
                    assert!(store.task_binding(remote.project.space.as_str(), &creation.hash)?.is_some());
                }
                let error = remote.node.tool(
                    sdk.clone(),
                    "nodes",
                    json!({"task_id":"remote-task","executor":"codex","execution_action":"send"}),
                    "sdk-foreign-account-targets",
                ).await.unwrap_err();
                assert_eq!(error.to_string(), "initial SDK discovery requires its own task or its initiator's confirmed binding");
                Ok(())
            }.await;
            remote.close().await;
            outcome
        }

        #[tokio::test]
        async fn account_task_sdk_continuation_keeps_the_original_message_and_observed_trigger()
        -> Result<()> {
            let mut remote = Box::pin(Remote::new()).await?;
            let mut phase = "enable the account task follow-up";
            let outcome: Result<()> = async {
                let mut body = remote.project.body("create_task", "conversation");
                body["task_id"] = json!("remote-task");
                body["title"] = json!("remote-task");
                body["goal"] = json!("work");
                body["acceptance"] = json!("review result");
                body["auto_followup"] = json!(true);
                rpc(&remote.project, &remote.node, body).await?;
                phase = "admit the real initial SDK Manager";
                let initial = agent(&remote.project, &remote.node).await?;
                let message = initial.sdk_session.session_id.clone();
                phase = "authorize and build the initial SDK open";
                remote.assert_cold_current(Action::ExecutionOpen).await?;
                let open = remote.dispatch(&initial, "sdk-open", None).await?;
                phase = "complete the open with the manual C Store fixture";
                let opened = remote.execute_manual_c_fixture(open).await?;
                phase = "authorize and build the initial SDK send";
                let send = remote.dispatch(&initial, "sdk-first-send", Some(&opened)).await?;
                phase = "complete the send with the manual C Store fixture";
                let sent = remote.execute_manual_c_fixture(send).await?;
                let trigger = {
                    let mut store = remote.node.store.lock().await;
                    let observed = store.context(remote.project.space.as_str(), "conversation", &[])?.events.into_iter()
                        .find(|event| matches!(&event.body, EventBody::ExecutionObserved { operation_id, status, .. } if operation_id == "sdk-first-send" && status == "succeeded"))
                        .context("B did not record the actual C result")?;
                    let trigger = crypto::digest(&serde_json::to_vec(&observed)?);
                    let saved = store.local_value(remote.project.space.as_str(), &crate::scheduler::key("invocation", "conversation", &initial.sdk_session.session_id))?
                        .context("initial Manager invocation is missing")?;
                    let mut invocation: crate::scheduler::Invocation = serde_json::from_value(saved)?;
                    invocation.state = crate::scheduler::InvocationState::Completed;
                    crate::scheduler::finish(&mut store, remote.project.space.as_str(), "conversation", &invocation)?;
                    trigger
                };
                phase = "reopen persisted originals without Task Current before continuation";
                *remote.node.store.lock().await = Store::open(remote.project.directory.join("node.sqlite3"))?;
                phase = "grant and claim the real Observed SDK continuation";
                let continuation = remote.node.store.lock().await.agent_grant(remote.project.space.as_str(), "conversation", &initial.service,
                    &SessionRef { target: initial.device.clone(), session_id: trigger.clone() }, &trigger, now_ms() as u64)?;
                remote.node.claim_manager(&continuation, &trigger, "remote-sdk-continuation", &crypto::digest(b"runtime manager configuration")).await?;
                remote.assert_cold_current(Action::ExecutionSend).await?;
                phase = "authorize and build the continuation SDK send";
                let wire = remote.dispatch(&continuation, "sdk-continuation-send", Some(&sent)).await?;
                let origin = wire.handoff.sdk_origin.as_ref().context("continuation handoff has no SDK origin")?;
                assert_eq!(origin.sdk_trigger, EventRef::parse(&trigger)?);
                assert_eq!(origin.message_event, EventRef::parse(&message)?);
                assert_ne!(origin.sdk_trigger, origin.message_event);
                phase = "verify the actual continuation carrier proof budget";
                remote.sync_policy().await?;
                let admit = remote.c_freshness(&wire, CheckStage::Admit).await?;
                remote.proof(&wire, Some(&admit)).await?;
                {
                    let mut store = remote.node.store.lock().await;
                    remote.project.task(&mut store, "conversation", "unrelated-task")?;
                }
                assert!(remote.node.tool(
                    continuation.clone(),
                    "nodes",
                    json!({"task_id":"unrelated-task","executor":"codex","execution_action":"send"}),
                    "sdk-continuation-unrelated-targets",
                ).await.is_err());
                assert_eq!(remote.c.lock().await.connection.query_row("SELECT COUNT(*) FROM events", [], |row| row.get::<_, u64>(0))?, 0);
                assert!(remote.node._codex_process.lock().await.is_none());
                Ok(())
            }.await;
            remote.close().await;
            outcome.with_context(|| format!("remote SDK continuation failed during {phase}"))
        }
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
    pub network: Arc<NetworkHub>,
    pub hub: Arc<PeerHub>,
    pub events: broadcast::Sender<Update>,
    pub agent_tokens: Mutex<HashMap<String, AgentSession>>,
    peer_connecting: Arc<Mutex<BTreeSet<(String, String)>>>,
    pub tool_url: std::sync::OnceLock<String>,
    pub maintenance: Arc<Gate>,
    completion_lock: Mutex<()>,
    handoff_resolution_lock: Mutex<()>,
    execution_dispatch_polls: Mutex<BTreeSet<(String, String)>>,
    execution_leases: Mutex<BTreeSet<(String, String)>>,
    pub engine: Engine,
    _codex_process: Mutex<Option<tokio::process::Child>>,
    managed_codex: bool,
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

fn task_views(store: &Store, space: &str, scope: &str) -> Result<Vec<Value>> {
    store
        .tasks(space, scope)?
        .into_iter()
        .map(|task| {
            let creation = store.task_creation(space, scope, &task.task_id)?;
            let event = store.verify_event(space, &creation)?;
            let authority = store.authorize_node(space, &event.author, None)?;
            let binding = store.task_binding(space, &creation.hash)?;
            let mut value = serde_json::to_value(task)?;
            value["task_creation"] = serde_json::to_value(creation)?;
            value["authority_public_key"] = json!(authority.public_key);
            value["binding"] = serde_json::to_value(binding)?;
            Ok(value)
        })
        .collect()
}

fn session_views(
    store: &Store,
    space: &str,
    scope: &str,
    operations: &[OperationRecord],
) -> Result<Vec<Value>> {
    store
        .sessions(space)?
        .into_iter()
        .filter_map(|session| {
            let request = operations
                .iter()
                .rev()
                .find(|operation| {
                    operation.request.target_node == session.target_node
                        && operation.request.session_id == session.session_id
                })?
                .request
                .clone();
            Some((|| -> Result<Value> {
                let mut value = serde_json::to_value(session)?;
                value["task_id"] = json!(request.task_id);
                value["binding"] = json!(request.binding);
                if let Some(task) = &request.task_id {
                    let creation = store.task_creation(space, scope, task)?;
                    let event = store.verify_event(space, &creation)?;
                    value["authority_public_key"] =
                        json!(store.authorize_node(space, &event.author, None)?.public_key);
                    value["task_creation"] = json!(creation);
                }
                Ok(value)
            })())
        })
        .collect()
}

fn execution_context(
    store: &Store,
    request: &OperationRequest,
    deadline: u64,
) -> Result<ExecutionContext> {
    use vibeface_secure::authorization::{PolicyDomain, SpaceRoot};
    let proof = store.operation_admission_proof(&request.network_id, &request.operation_id)?;
    let invocation = &proof.invocation.payload;
    ensure!(
        invocation.resource.space.as_str() == request.network_id,
        "execution admission belongs to another space"
    );
    let policies = store.authorization_state();
    let document = policies
        .space(&invocation.resource.space)
        .context("execution space policy is missing")?;
    let private = match &document.root {
        SpaceRoot::Personal { owner, .. } => match &invocation.actor {
            Actor::Account(account) => account == owner,
            Actor::Service(service) if invocation.initiator == Actor::Account(owner.clone()) => {
                policies.account(owner).is_some_and(|account| {
                    account.devices.iter().any(|hash| {
                        policies.device(hash).is_some_and(|certificate| {
                            certificate.payload.owner == PolicyDomain::Account(owner.clone())
                                && certificate.payload.device == invocation.device
                                && certificate.payload.subject == Actor::Service(service.clone())
                        })
                    })
                })
            }
            _ => false,
        },
        SpaceRoot::Team { .. } => false,
    };
    Ok(ExecutionContext {
        space_id: invocation.resource.space.clone(),
        resource_id: invocation.resource.resource.clone(),
        operation_id: request.operation_id.clone(),
        shared: !private,
        lease_expires_at_ms: deadline,
    })
}
fn require_task_write(
    store: &Store,
    network: &str,
    scope: &str,
    task_id: &str,
    account_actor: bool,
) -> Result<()> {
    let task = store
        .tasks(network, scope)?
        .into_iter()
        .find(|t| t.task_id == task_id)
        .context("task not found")?;
    ensure!(
        account_actor || !matches!(task.status.as_str(), "accepted" | "cancelled"),
        "only the user may reopen accepted or cancelled tasks"
    );
    Ok(())
}

fn followup_authorized(store: &Store, network: &str, scope: &str, task: &str) -> Result<bool> {
    let Some(task) = store
        .tasks(network, scope)?
        .into_iter()
        .find(|record| record.task_id == task)
    else {
        return Ok(false);
    };
    if matches!(task.status.as_str(), "accepted" | "cancelled") {
        return Ok(false);
    }
    let events = crate::identity::services::retained_agent_events(
        &store.connection,
        network,
        &task.event_hash,
    )?;
    ensure!(
        events
            .get(&task.event_hash)
            .is_some_and(|event| event.event.scope_id == scope),
        "follow-up task belongs to another conversation"
    );
    for stored in events.values() {
        let event = &stored.event;
        if event.scope_id != scope
            || !matches!(&event.body,EventBody::DecisionRecorded {decision_id,effective_constraints,tool_configuration,..}
            if decision_id==&format!("auto:{}",task.task_id)
                && effective_constraints.contains(&format!("auto_followup:{}",task.task_id))
                && tool_configuration["auto_followup"]==task.task_id)
        {
            continue;
        }
        let EventCause::Invocation { admission } = &event.cause else {
            continue;
        };
        let source = &admission.source_body;
        if !matches!(admission.invocation.payload.actor, Actor::Account(_))
            || source["method"] != "create_task"
            || source["auto_followup"] != true
            || source["task_id"] != task.task_id
            || source["title"] != task.title
            || source["goal"] != task.goal
            || source["acceptance"] != task.acceptance
        {
            continue;
        }
        if crate::identity::event_authorization::current_event_authorization(
            &store.policy,
            &store.connection,
            event,
            &events,
            now_ms() as u64,
        )
        .is_ok_and(|auth| matches!(auth.actor(), Actor::Account(_)))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn require_executor_followup(
    store: &Store,
    network: &str,
    origin: &str,
    scope: &str,
    trigger: &str,
) -> Result<()> {
    let envelope = store
        .event(network, trigger)?
        .context("follow-up result is missing")?;
    let event = store.verify_event(network, &envelope)?;
    ensure!(
        event.author == origin && event.scope_id == scope,
        "executor may only schedule its own result in this conversation"
    );
    let EventBody::ExecutionObserved {
        operation_id,
        task_id: Some(task),
        target_node,
        session_id,
        status,
        ..
    } = &event.body
    else {
        bail!("executor follow-up requires a task result");
    };
    ensure!(
        target_node == origin && matches!(status.as_str(), "succeeded" | "failed" | "rejected"),
        "executor follow-up requires its known terminal result"
    );
    ensure!(
        followup_authorized(store, network, scope, task)?,
        "automatic follow-up needs an active task and user authorization"
    );
    let events =
        crate::identity::services::retained_agent_events(&store.connection, network, trigger)?;
    let causal = crate::store::closure_hashes(&events, &[trigger.to_owned()]);
    ensure!(
        causal
            .iter()
            .filter_map(|hash| events.get(hash))
            .any(|stored| {
                let dispatch = &stored.event;
                let EventBody::DispatchRecorded {
                    operation_id: id,
                    task_id,
                    target_node: target,
                    session_id: session,
                    action,
                } = &dispatch.body
                else {
                    return false;
                };
                dispatch.scope_id == scope
                    && id == operation_id
                    && task_id.as_ref() == Some(task)
                    && target == origin
                    && session == session_id
                    && store
                        .authorize_node(network, &dispatch.author, None)
                        .is_ok()
                    && crate::identity::event_authorization::current_event_authorization(
                        &store.policy,
                        &store.connection,
                        dispatch,
                        &events,
                        now_ms() as u64,
                    )
                    .is_ok_and(|auth| {
                        auth.action() == vibeface_secure::authorization::Action::ExecutionSend
                    })
                    && serde_json::from_value::<OperationRequest>(action.clone()).is_ok_and(
                        |request| {
                            request.network_id == network
                                && request.conversation_id == scope
                                && request.operation_id == *operation_id
                                && request.task_id.as_ref() == Some(task)
                                && request.target_node == origin
                                && request.session_id == *session_id
                                && request.action["type"] == "send"
                        },
                    )
            }),
        "executor result has no matching dispatch"
    );
    Ok(())
}

impl Runtime {
    pub(crate) fn local_audience(&self) -> Result<Audience> {
        self.audience()
    }

    fn check_client_policy_dependencies(
        client: &VerifiedClientAuthorization,
        request: &Invocation,
        body: &Value,
    ) -> Result<bool> {
        let fields = ["execution_resource", "target_public_key", "execution_grant"];
        if fields.iter().all(|field| body.get(*field).is_none()) {
            ensure!(
                request.expected_policies == client.heads(),
                "client request changed its verified policy dependencies"
            );
            return Ok(false);
        }
        ensure!(
            fields
                .iter()
                .all(|field| body.get(*field).is_some_and(|value| !value.is_null()))
                && matches!(
                    body["method"].as_str(),
                    Some("operate" | "bind_execution" | "rebind_execution" | "resolve_unknown")
                ),
            "forwarded execution selection is incomplete or unsupported"
        );
        let _: ResourceRef = serde_json::from_value(body["execution_resource"].clone())?;
        let key: PublicKey = serde_json::from_value(body["target_public_key"].clone())?;
        let _: ObjectHash = serde_json::from_value(body["execution_grant"].clone())?;
        ensure!(
            request
                .context
                .execution_session
                .as_ref()
                .is_some_and(|session| {
                    session.target == DeviceId::from_key(&key)
                        && session.target != request.audience.device
                })
                && request.expected_policies.len()
                    <= vibeface_secure::authorization::MAX_POLICY_DOMAINS
                && request
                    .expected_policies
                    .windows(2)
                    .all(|pair| pair[0].domain < pair[1].domain)
                && client
                    .heads()
                    .iter()
                    .all(|head| request.expected_policies.contains(head)),
            "forwarded execution changed its source dependencies or target"
        );
        Ok(true)
    }

    fn client_forward_target(
        &self,
        store: &Store,
        client: &VerifiedClientAuthorization,
        now: u64,
    ) -> Result<String> {
        ensure!(
            now < client.expires_at_ms(),
            "forwarding client policy expired"
        );
        let snapshot = store.connection.unchecked_transaction()?;
        let current =
            crate::identity::journal::PolicyState::reload_verified(&snapshot, &store.policy)?;
        crate::identity::client_policy_read::check_heads_in(&snapshot, &current, client.heads())?;
        let local = self.audience()?;
        let gateway = current
            .verified()
            .verify_transport_device(&local.device, now)?;
        ensure!(
            gateway.payload.subject == Actor::Service(local.service)
                && gateway.payload.public_key.as_str() == self.identity.public_key_hex()?,
            "forwarding Gateway differs from its current service certificate"
        );
        current.verified().verify_account_client_device(
            &client.scope().account_id,
            &client.scope().device_id,
            now,
        )?;
        let certificate = current.verified().verify_service_device(
            &client.scope().space_id,
            &client.audience().device,
            now,
        )?;
        ensure!(
            certificate.payload.subject == Actor::Service(client.audience().service.clone())
                && certificate.payload.public_key == *client.audience_public_key(),
            "forwarded target differs from the verified service pin"
        );
        let target = crypto::fingerprint(certificate.payload.public_key.as_str())?;
        snapshot.commit()?;
        Ok(target)
    }

    pub async fn forward_client_request(
        &self,
        client: &VerifiedClientAuthorization,
        body: &Value,
        invocation: &Signed<Invocation>,
        noise_public_key: &str,
    ) -> Result<Value> {
        self.forward_client_request_kind(client, body, invocation, noise_public_key, "rpc")
            .await
    }

    pub async fn remote_subscription_cursor(
        &self,
        client: &VerifiedClientAuthorization,
        body: &Value,
        invocation: &Signed<Invocation>,
        noise_public_key: &str,
    ) -> Result<String> {
        ensure!(
            authorization_action(body)?.is_read(),
            "subscription requires read permission"
        );
        let result = self
            .forward_client_request_kind(client, body, invocation, noise_public_key, "subscription")
            .await?;
        Ok(field(&result, "cursor")?.to_owned())
    }

    async fn forward_client_request_kind(
        &self,
        client: &VerifiedClientAuthorization,
        body: &Value,
        invocation: &Signed<Invocation>,
        noise_public_key: &str,
        kind: &str,
    ) -> Result<Value> {
        let verification = SignatureVerification::new();
        let now = now_ms() as u64;
        let value = &invocation.payload;
        let device = DeviceId::from_key(&PublicKey::parse(noise_public_key)?);
        ensure!(
            invocation.issuer.as_str() == noise_public_key
                && device == client.scope().device_id
                && value.device == device
                && value.actor == Actor::Account(client.scope().account_id.clone())
                && value.initiator == value.actor
                && value.initiating_grant.is_none(),
            "forwarded actor differs from the authenticated client"
        );
        ensure!(
            value.version == AUTH_VERSION
                && value.audience == *client.audience()
                && value.presenter == self.audience()?.device
                && value.grant_leaf == *client.grant_leaf(),
            "forwarded authorization targets another route"
        );
        Self::check_client_policy_dependencies(client, value, body)?;
        ensure!(
            value.resource.space == client.scope().space_id
                && body["space_id"].as_str() == Some(client.scope().space_id.as_str())
                && body["resource_id"].as_str() == Some(value.resource.resource.as_str())
                && client
                    .resources()
                    .iter()
                    .any(|record| record.payload.resource == value.resource.resource),
            "forwarded resource is outside the verified projection"
        );
        ensure!(
            authorization_action(body)? == value.action
                && ObjectHash::body(body)? == value.body_hash
                && body["authorization_id"].as_str() == Some(value.operation_id.as_str()),
            "forwarded request differs from signed intent"
        );
        ensure!(
            value.not_before_ms <= now
                && now < value.expires_at_ms
                && value.expires_at_ms - value.not_before_ms <= INVOCATION_MAX_AGE_MS,
            "forwarded invocation expired"
        );
        verification.run(|| invocation.verify_signature())?;
        let target = {
            let store = self.store.lock().await;
            let _verification = verification.enter();
            self.client_forward_target(&store, client, now)?
        };
        self.peer_request(
            client.scope().space_id.as_str(),
            &target,
            json!({
                "type":kind,"authorization_body":body,"invocation":invocation,
            }),
        )
        .await
    }

    fn audience(&self) -> Result<Audience> {
        let key = PublicKey::parse(&self.identity.public_key_hex()?)?;
        Ok(Audience {
            service: ServiceId::from_key(&key),
            device: DeviceId::from_key(&key),
        })
    }

    async fn normalize_request_body(
        self: &Arc<Self>,
        space: &SpaceId,
        body: Value,
    ) -> Result<Value> {
        self.normalize_request_body_with_verification(space, body, &SignatureVerification::new())
            .await
    }

    async fn normalize_request_body_with_verification(
        self: &Arc<Self>,
        space: &SpaceId,
        mut body: Value,
        verification: &SignatureVerification,
    ) -> Result<Value> {
        ensure!(body.is_object(), "request body must be an object");
        ensure!(
            body["space_id"].as_str() == Some(space.as_str()),
            "request belongs to another space"
        );
        id(&body, "authorization_id")?;
        authorization_action(&body)?;
        if self.is_execution_metadata(&body)? || self.is_execution_resolution(&body)? {
            return Ok(body);
        }
        if self.is_execution_dispatch(&body)? {
            return self
                .normalize_execution_dispatch_with_verification(body, verification)
                .await;
        }
        if body["method"] == "message" {
            if body.get("scheduler_node").is_none() {
                body["scheduler_node"] = json!(self.identity.node_id()?);
            }
            ensure!(field(&body, "text")?.len() <= 65_536, "message is too long");
        }
        if body["method"] == "update_task" {
            if body.get("detail").is_none() {
                body["detail"] = json!("");
            }
            ensure!(
                body["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.len() <= 65_536),
                "task detail is too long"
            );
        }
        if body["method"] == "operate" {
            let mut request: OperationRequest = serde_json::from_value(body["request"].clone())?;
            ensure!(
                request.network_id == space.as_str()
                    && body["authorization_id"] == request.operation_id,
                "operation space or authorization ID mismatch"
            );
            self.prepare_operation_binding(&mut request, verification)
                .await?;
            if request.action["type"] == "open" {
                request.action["cwd"] = json!(
                    self.allowed_path(field(&request.action, "cwd")?)?
                        .to_str()
                        .context("directory must be UTF-8")?
                );
            }
            let mut resources = request
                .resources
                .iter()
                .map(|path| {
                    Ok(self
                        .allowed_path(path)?
                        .to_str()
                        .context("resource must be UTF-8")?
                        .to_owned())
                })
                .collect::<Result<Vec<_>>>()?;
            if request.action["type"] == "open" {
                resources.push(field(&request.action, "cwd")?.to_owned());
            } else if request.action["type"] == "send" {
                let session = self
                    .store
                    .lock()
                    .await
                    .session(
                        &request.network_id,
                        &request.target_node,
                        &request.session_id,
                    )?
                    .context("execution session is not open")?;
                resources.push(
                    self.allowed_path(&session.cwd)?
                        .to_str()
                        .context("directory must be UTF-8")?
                        .to_owned(),
                );
            }
            resources.sort();
            resources.dedup();
            request.resources = resources;
            body["request"] = serde_json::to_value(request)?;
        }
        Ok(body)
    }

    pub async fn prepare_client_request(
        self: &Arc<Self>,
        client: &VerifiedClientAuthorization,
        body: Value,
        intent: Signed<vibeface_secure::authorization::prepare::PrepareIntent>,
        noise_public_key: &str,
    ) -> Result<Value> {
        let verification = SignatureVerification::new();
        let claim = &intent.payload.request;
        let device = DeviceId::from_key(&PublicKey::parse(noise_public_key)?);
        ensure!(
            intent.issuer.as_str() == noise_public_key
                && device == client.scope().device_id
                && claim.device == device
                && claim.actor == Actor::Account(client.scope().account_id.clone())
                && claim.initiator == claim.actor
                && claim.initiating_grant.is_none(),
            "prepare actor differs from authenticated client"
        );
        ensure!(
            claim.audience == *client.audience()
                && claim.grant_leaf == *client.grant_leaf()
                && claim.resource.space == client.scope().space_id,
            "prepare intent differs from selected client authorization"
        );
        let remote = Self::check_client_policy_dependencies(client, claim, &body)?;
        ensure!(
            client
                .resources()
                .iter()
                .any(|record| record.payload.resource == claim.resource.resource),
            "prepare resource is outside verified projection"
        );
        let local = self.audience()?;
        let transport = if claim.presenter == device {
            TransportEvidence::DirectNoise { device }
        } else {
            ensure!(
                claim.presenter == local.device,
                "prepare targets another Gateway presenter"
            );
            TransportEvidence::GatewayNoise {
                device,
                gateway: local.device.clone(),
            }
        };
        if client.audience() == &local {
            let prepared = {
                let store = self.store.lock().await;
                verification.run(|| -> Result<_> {
                    let snapshot = store.connection.unchecked_transaction()?;
                    let current = crate::identity::journal::PolicyState::reload_verified(
                        &snapshot,
                        &store.policy,
                    )?;
                    let prepared = TaskScopedPrepareAuthorization::verify_in(
                        &snapshot,
                        &current,
                        &PublicKey::parse(&self.identity.public_key_hex()?)?,
                        intent.clone(),
                        &body,
                        &local,
                        RequestTransport::Existing(transport),
                        now_ms() as u64,
                    )?;
                    snapshot.commit()?;
                    Ok(prepared)
                })?
            };
            return self.prepare_authorized_body(prepared, &verification).await;
        }
        ensure!(
            claim.presenter == local.device,
            "forwarded prepare requires this Gateway presenter"
        );
        let now = now_ms() as u64;
        if remote {
            ensure!(
                serde_json::to_vec(&body)?.len() <= vibeface_secure::authorization::MAX_PROOF_BYTES
                    && serde_json::to_vec(&intent)?.len()
                        <= vibeface_secure::authorization::MAX_PROOF_BYTES,
                "forwarded prepare exceeds the proof byte limit"
            );
            ensure!(
                claim.version == AUTH_VERSION
                    && claim.context.sdk_session.is_none()
                    && body["space_id"].as_str() == Some(claim.resource.space.as_str())
                    && body["resource_id"].as_str() == Some(claim.resource.resource.as_str())
                    && authorization_action(&body)? == claim.action
                    && client.actions().contains(&claim.action)
                    && ObjectHash::body(&body)? == claim.body_hash
                    && body["authorization_id"].as_str() == Some(claim.operation_id.as_str()),
                "forwarded prepare differs from its signed request"
            );
            ensure!(
                claim.not_before_ms <= now
                    && now < claim.expires_at_ms
                    && claim.expires_at_ms - claim.not_before_ms <= INVOCATION_MAX_AGE_MS,
                "forwarded prepare expired"
            );
            verification.run(|| intent.verify_signature())?;
        } else {
            let store = self.store.lock().await;
            let action = authorization_action(&body)?;
            verification.run(|| {
                store.authorization_state().verify_prepare_intent(
                    &intent,
                    &body,
                    action,
                    client.audience(),
                    &transport,
                    now,
                )
            })?;
        }
        let target = {
            let store = self.store.lock().await;
            let _verification = verification.enter();
            self.client_forward_target(&store, client, now)?
        };
        self.peer_request(
            client.scope().space_id.as_str(),
            &target,
            json!({"type":"prepare","authorization_body":body,"intent":intent}),
        )
        .await
    }

    async fn prepare_authorized_body(
        self: &Arc<Self>,
        scoped: TaskScopedPrepareAuthorization,
        verification: &SignatureVerification,
    ) -> Result<Value> {
        let prepared = &scoped.authorization;
        ensure!(
            prepared.audience() == &self.audience()?,
            "prepare reached another resource host"
        );
        let body = self
            .normalize_request_body_with_verification(
                &prepared.resource().space,
                prepared.raw_body().clone(),
                verification,
            )
            .await?;
        let now = now_ms() as u64;
        let original = &scoped.intent.payload.request;
        self.check_execution_actor(&body, original)?;
        let facts = {
            let store = self.store.lock().await;
            let _verification = verification.enter();
            let snapshot = store.connection.unchecked_transaction()?;
            let current =
                crate::identity::journal::PolicyState::reload_verified(&snapshot, &store.policy)?;
            let rechecked = TaskScopedPrepareAuthorization::verify_in(
                &snapshot,
                &current,
                &PublicKey::parse(&self.identity.public_key_hex()?)?,
                scoped.intent.clone(),
                prepared.raw_body(),
                prepared.audience(),
                scoped.transport.clone(),
                now,
            )?;
            scoped.check_same_policy(&rechecked)?;
            let state = rechecked
                .task_policy
                .as_ref()
                .map_or(current.verified(), |task| task.state());
            let facts = if self.is_execution_metadata(&body)? {
                ensure!(
                    prepared.execution_destination().is_none(),
                    "metadata prepare cannot select executable C authority"
                );
                ensure!(
                    matches!(original.actor, Actor::Account(_)),
                    "Project execution metadata requires an account actor"
                );
                self.authorization_facts(&store, &body, prepared.audience(), now)?
            } else if let Some(selected) = prepared.execution_destination() {
                ensure!(
                    self.is_execution_resolution(&body)? || self.is_execution_dispatch(&body)?,
                    "remote prepare lost its destination"
                );
                let facts = crate::identity::policy::request_execution_dispatch_facts_from(
                    &snapshot,
                    state,
                    &body,
                    prepared.audience(),
                    selected,
                    now,
                )?;
                let current = crate::identity::policy::execution_destination_for_body(
                    state, &facts, &body, now,
                )?
                .context("normalized prepare has no execution destination")?;
                ensure!(
                    current.resource() == selected.resource()
                        && current.audience() == selected.audience()
                        && current.public_key() == selected.public_key()
                        && current.grant_leaf() == selected.grant_leaf()
                        && current.heads() == selected.heads(),
                    "normalization changed the independently verified execution destination"
                );
                selected.check_facts(&facts)?;
                facts
            } else {
                ensure!(
                    !self.is_execution_resolution(&body)? && !self.is_execution_dispatch(&body)?,
                    "prepare has no verified execution destination"
                );
                self.authorization_facts(&store, &body, prepared.audience(), now)?
            };
            snapshot.commit()?;
            facts
        };
        ensure!(
            facts.resource == *prepared.resource()
                && facts.action == prepared.action()
                && facts.operation_id == prepared.operation_id()
                && facts.audience == *prepared.audience()
                && facts.context == original.context,
            "normalization changed authorized prepare target or context"
        );
        let mut invocation = original.clone();
        invocation.resource = facts.resource;
        invocation.action = facts.action;
        invocation.audience = facts.audience;
        invocation.operation_id = facts.operation_id;
        invocation.body_hash = facts.body_hash;
        invocation.context = facts.context;
        invocation.nonce = Nonce::random()?;
        invocation.not_before_ms = now;
        invocation.expires_at_ms = now.saturating_add(INVOCATION_MAX_AGE_MS);
        Ok(json!({"invocation":invocation,"body":body}))
    }

    async fn freshness(
        &self,
        authorization: &VerifiedAuthorization,
        stage: CheckStage,
    ) -> Result<FreshnessProof> {
        self.freshness_with_verification(authorization, stage, &SignatureVerification::new())
            .await
    }

    async fn freshness_with_verification(
        &self,
        authorization: &VerifiedAuthorization,
        stage: CheckStage,
        verification: &SignatureVerification,
    ) -> Result<FreshnessProof> {
        let now = now_ms() as u64;
        let challenge = {
            let mut store = self.store.lock().await;
            verification.run(|| store.issue_freshness(authorization, stage, now))?
        };
        self.collect_freshness_challenge_with_verification(
            authorization.resource().space.as_str(),
            &challenge,
            verification,
        )
        .await
    }

    async fn freshness_scoped_with_verification(
        &self,
        authorization: &TaskScopedAuthorization,
        transport: &RequestTransport,
        stage: CheckStage,
        verification: &SignatureVerification,
    ) -> Result<FreshnessProof> {
        let now = now_ms() as u64;
        let challenge = {
            let mut store = self.store.lock().await;
            verification.run(|| {
                store.issue_scoped_freshness_for_transport(authorization, transport, stage, now)
            })?
        };
        self.collect_freshness_challenge_in_scope_with_verification(
            authorization.resource().space.as_str(), &challenge, Some((authorization, transport)), verification,
        ).await
    }

    pub(crate) async fn collect_freshness_challenge(
        &self,
        space: &str,
        challenge: &FreshnessChallenge,
    ) -> Result<FreshnessProof> {
        self.collect_freshness_challenge_with_verification(
            space,
            challenge,
            &SignatureVerification::new(),
        )
        .await
    }

    pub(crate) async fn collect_freshness_challenge_with_verification(
        &self,
        space: &str,
        challenge: &FreshnessChallenge,
        verification: &SignatureVerification,
    ) -> Result<FreshnessProof> {
        self.collect_freshness_challenge_in_scope_with_verification(space, challenge, None, verification).await
    }

    async fn collect_freshness_challenge_in_scope_with_verification(
        &self,
        space: &str,
        challenge: &FreshnessChallenge,
        authorization: Option<(&TaskScopedAuthorization, &RequestTransport)>,
        verification: &SignatureVerification,
    ) -> Result<FreshnessProof> {
        let (mut receipts, authorities) = {
            let store = self.store.lock().await;
            let receipts = verification
                .run(|| store.sign_current_receipts(challenge, &self.identity, now_ms() as u64))?;
            let policies = authorization.map_or(store.authorization_state(), |(scope, _)| scope.state(store.authorization_state()));
            let mut authorities = BTreeSet::new();
            for stamp in &challenge.policies {
                if receipts
                    .iter()
                    .any(|receipt| receipt.payload.current.domain == stamp.domain)
                {
                    continue;
                }
                let authority = match &stamp.domain {
                    PolicyDomain::Account(id) => {
                        &policies.account(id)
                            .context("account policy missing")?
                            .authority
                    }
                    PolicyDomain::Space(id) => {
                        &policies.space(id)
                            .context("space policy missing")?
                            .authority
                    }
                };
                authorities.insert(crypto::fingerprint(authority.public_key.as_str())?);
            }
            (receipts, authorities)
        };
        for target in authorities {
            if let Some((scoped, transport)) = authorization.filter(|(scoped, _)| {
                scoped.task_policy().is_some_and(|task| {
                    crypto::fingerprint(task.selection().target_public_key.as_str())
                        .is_ok_and(|node| node == target)
                })
            }) {
                let prepared = {
                    let store = self.store.lock().await;
                    verification.run(|| {
                        store.task_execution_freshness_read(
                            &self.identity, scoped, transport, challenge, now_ms() as u64,
                        )
                    })?
                };
                let mut client = crate::client::NodePolicyReadClient::connect(
                    &self.network, &self.identity, &prepared.trust,
                ).await?;
                let reply = client.read(
                    &crate::identity::node_execution_policy::NodeReadRequest::Freshness(
                        prepared.request,
                    ),
                ).await?;
                let crate::identity::node_execution_policy::NodeReadResponse::Receipts(current) = reply else {
                    bail!("Node freshness returned another policy read stage");
                };
                receipts.extend(current);
                continue;
            }
            let reply = self
                .peer_request(
                    space,
                    &target,
                    json!({"type":"freshness_receipt","challenge":challenge}),
                )
                .await?;
            let current: Vec<Signed<FreshnessReceipt>> =
                serde_json::from_value(reply["receipts"].clone())?;
            receipts.extend(current);
        }
        receipts.sort_by(|left, right| {
            left.payload
                .current
                .domain
                .cmp(&right.payload.current.domain)
        });
        ensure!(
            receipts.len() == challenge.policies.len(),
            "current policy authority is unavailable"
        );
        Ok(FreshnessProof {
            challenge: challenge.clone(),
            receipts,
        })
    }

    pub async fn authorize_rpc(
        &self,
        body: Value,
        invocation: Signed<Invocation>,
        transport: TransportEvidence,
    ) -> Result<Access> {
        self.authorize_rpc_with_verification(
            body,
            invocation,
            transport,
            SignatureVerification::new(),
        )
        .await
    }

    async fn authorize_rpc_with_verification(
        &self,
        body: Value,
        invocation: Signed<Invocation>,
        transport: TransportEvidence,
        verification: SignatureVerification,
    ) -> Result<Access> {
        self.authorize_rpc_for_transport(
            body,
            invocation,
            RequestTransport::Existing(transport),
            verification,
        )
        .await
    }

    async fn authorize_rpc_for_transport(
        &self,
        body: Value,
        invocation: Signed<Invocation>,
        transport: RequestTransport,
        verification: SignatureVerification,
    ) -> Result<Access> {
        let space = SpaceId::parse(field(&body, "space_id")?)?;
        let audience = self.audience()?;
        self.check_execution_actor(&body, &invocation.payload)?;
        let authorization = {
            let store = self.store.lock().await;
            verification.run(|| {
                let raw = vibeface_secure::authorization::RequestFacts {
                    resource: vibeface_secure::authorization::ResourceRef {
                        space: space.clone(),
                        resource: vibeface_secure::authorization::ResourceId::parse(field(
                            &body,
                            "resource_id",
                        )?)?,
                    },
                    action: authorization_action(&body)?,
                    audience: audience.clone(),
                    operation_id: field(&body, "authorization_id")?.to_owned(),
                    body_hash: ObjectHash::body(&body)?,
                    context: invocation.payload.context.clone(),
                };
                let raw_authorization = store.authorize_scoped_request_for_transport(
                    &self.identity,
                    &invocation,
                    &raw,
                    &transport,
                    &body,
                    now_ms() as u64,
                )?;
                let facts = self
                    .authorization_facts_in_scope(&store, &raw_authorization, &body, &audience, now_ms() as u64)
                    .map_err(|_| anyhow::anyhow!("request facts do not match authorization"))?;
                store.authorize_scoped_request_for_transport(
                    &self.identity,
                    &invocation,
                    &facts,
                    &transport,
                    &body,
                    now_ms() as u64,
                )
            })?
        };
        ensure!(
            authorization.resource().space == space,
            "wrong authorization space"
        );
        let stage = if body["method"] != "operate" && authorization.action().is_read() {
            CheckStage::RemoteRead
        } else {
            CheckStage::Admit
        };
        let freshness = self
            .freshness_scoped_with_verification(&authorization, &transport, stage, &verification)
            .await?;
        Ok(Access {
            network_id: space.to_string(),
            conversation_id: body["conversation_id"].as_str().map(str::to_owned),
            authorization,
            freshness,
            transport,
            body,
            verification,
        })
    }

    pub async fn validate_subscription(&self, access: &Access) -> Result<()> {
        ensure!(
            access.authorization.action().is_read() && access.body["method"] != "operate",
            "subscription requires a read-only request"
        );
        let mut store = self.store.lock().await;
        let _verification = access.verification.enter();
        store.authorize_request_read(
            &access.authorization,
            &access.freshness,
            &access.transport,
            now_ms() as u64,
        )
    }

    pub async fn refresh_subscription(&self, access: &Access) -> Result<Access> {
        let current = self
            .authorize_rpc_for_transport(
                access.body.clone(),
                access.authorization.signed_invocation().clone(),
                access.transport.clone(),
                SignatureVerification::new(),
            )
            .await?;
        self.validate_subscription(&current).await?;
        Ok(current)
    }

    pub async fn subscription_allows(&self, access: &Access, update: &Update) -> Result<bool> {
        if update.network_id != access.network_id {
            return Ok(false);
        }
        let verification = SignatureVerification::new();
        let authorization = {
            let store = self.store.lock().await;
            let _verification = verification.enter();
            let facts = store.request_facts(&access.body, &self.audience()?)?;
            match store.authorize_request(
                access.authorization.signed_invocation(),
                &facts,
                access
                    .transport
                    .existing()
                    .context("subscription requires an ordinary client transport")?,
                now_ms() as u64,
            ) {
                Ok(authorization) => authorization,
                Err(_) => return Ok(false),
            }
        };
        let freshness = match self
            .freshness_with_verification(&authorization, CheckStage::RemoteRead, &verification)
            .await
        {
            Ok(freshness) => freshness,
            Err(_) => return Ok(false),
        };
        let mut store = self.store.lock().await;
        let _verification = verification.enter();
        if store
            .authorize_request_read(
                &authorization,
                &freshness,
                &access.transport,
                now_ms() as u64,
            )
            .is_err()
        {
            return Ok(false);
        }
        let scope = update.payload["conversation_id"]
            .as_str()
            .context("update has no resource scope")?;
        Ok(store
            .authorized_conversation_ids(&authorization)?
            .iter()
            .any(|allowed| allowed == scope))
    }

    pub async fn open(dir: &Path) -> Result<Self> {
        config::verify_data_format(dir)?;
        let mut config = Config::load(dir)?;
        let process_lock = config::lock_data_dir(dir)?;
        let identity = config::identity(dir)?;
        let store = Store::open(dir.join("node.sqlite3"))?;
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
        let engine = Engine::new(
            ExecutorConfig {
                codex_socket: config.codex_socket.clone(),
                claude_bin: config.claude_bin.clone(),
                cursor_bin: config.cursor_bin.clone(),
                timeout_secs: config.timeout_secs,
                execution_isolation: config.execution_isolation.clone(),
                data_dir: Some(dir.to_path_buf()),
            },
            tx,
        );
        Ok(Self {
            config,
            identity,
            store,
            network,
            hub,
            events,
            agent_tokens: Mutex::new(HashMap::new()),
            peer_connecting: Arc::new(Mutex::new(BTreeSet::new())),
            tool_url: std::sync::OnceLock::new(),
            maintenance: Arc::new(Gate::new(dir.join("update-pending.json").try_exists()?)),
            completion_lock: Mutex::new(()),
            handoff_resolution_lock: Mutex::new(()),
            execution_dispatch_polls: Mutex::new(BTreeSet::new()),
            execution_leases: Mutex::new(BTreeSet::new()),
            engine,
            _codex_process: Mutex::new(None),
            managed_codex: managed_socket,
            _process_lock: process_lock,
            peer_rx: Mutex::new(Some(peer_rx)),
            network_rx: Mutex::new(Some(network_rx)),
            engine_rx: Mutex::new(Some(rx)),
        })
    }
    pub async fn has_membership(&self) -> Result<bool> {
        Ok(!self.store.lock().await.networks()?.is_empty())
    }

    async fn ensure_personal_provider(&self) -> Result<()> {
        if !self.managed_codex {
            return Ok(());
        }
        let mut managed = self._codex_process.lock().await;
        if let Some(child) = managed.as_mut() {
            ensure!(
                child.try_wait()?.is_none(),
                "managed Provider exited; reconcile active operations before restarting"
            );
            return Ok(());
        }
        let socket = self
            .config
            .codex_socket
            .as_ref()
            .context("private Provider socket is not configured")?;
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            return Ok(());
        }
        if socket.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                ensure!(
                    std::fs::symlink_metadata(socket)?.file_type().is_socket(),
                    "private Provider path is occupied"
                );
            }
            std::fs::remove_file(socket)?;
        }
        let mut command = tokio::process::Command::new(&self.config.codex_bin);
        command
            .args(["app-server", "--listen"])
            .arg(format!("unix://{}", socket.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        *managed = Some(
            command
                .spawn()
                .context("managed Provider could not start")?,
        );
        Ok(())
    }

    pub fn info(&self) -> Value {
        json!({"node_id":self.identity.node_id().ok(),"version":env!("CARGO_PKG_VERSION"),"roles":self.config.roles,"manager_available":self.config.manager.is_some()})
    }
    pub async fn start(self: &Arc<Self>) -> Result<()> {
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
        let local = self.identity.node_id()?;
        for network in networks {
            self.flush_completions(&network).await?;
            let operations = self.store.lock().await.operations(&network)?;
            for operation in operations {
                if operation.request.target_node != local {
                    continue;
                }
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
        self.recover_handoffs().await?;
        self.recover_execution_dispatches().await?;
        Ok(())
    }
    pub(crate) async fn maintenance_idle(&self) -> Result<()> {
        let local = self.identity.node_id()?;
        let store = self.store.lock().await;
        let pending_handoffs: u64 = store.connection.query_row(
            "SELECT count(*) FROM execution_handoffs WHERE state IN ('accepted','running','unknown')",
            [], |row| row.get(0),
        )?;
        ensure!(
            pending_handoffs == 0,
            Busy("delegated execution requires completion or reconciliation".into())
        );
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
    pub async fn authorize_bearer(&self, token: &str) -> Option<AgentSession> {
        self.agent_tokens.lock().await.get(token).cloned()
    }
    fn publish_changed(&self, space: &str, conversation: &str, event_hash: &str) {
        let _ = self.events.send(Update { network_id:space.to_owned(),payload:json!({"type":"changed","conversation_id":conversation,"event_hash":event_hash}) });
    }

    async fn append_authorized(
        &self,
        access: &Access,
        scope: &str,
        body: EventBody,
    ) -> Result<SignedEvent> {
        let event = {
            let mut store = self.store.lock().await;
            access.verification.run(|| {
                store.append_request_authorized_event(
                    &self.identity,
                    &access.authorization,
                    &access.freshness,
                    &access.transport,
                    &access.body,
                    scope,
                    body,
                    vec![],
                    now_ms(),
                )
            })?
        };
        self.publish_changed(&access.network_id, scope, &event.hash);
        Ok(event)
    }
    async fn append_operation_fact(
        &self,
        request: &OperationRequest,
        body: EventBody,
        blobs: Vec<BlobRef>,
    ) -> Result<SignedEvent> {
        self.append_operation_fact_with_verification(
            request,
            body,
            blobs,
            &SignatureVerification::new(),
        )
        .await
    }

    async fn append_operation_fact_with_verification(
        &self,
        request: &OperationRequest,
        body: EventBody,
        blobs: Vec<BlobRef>,
        verification: &SignatureVerification,
    ) -> Result<SignedEvent> {
        let event = {
            let mut store = self.store.lock().await;
            let _verification = verification.enter();
            let source = store
                .operation_dispatch(&request.network_id, &request.operation_id)?
                .context("admitted operation lost its dispatch proof")?;
            store.append_derived_event(
                &self.identity,
                &request.network_id,
                &source.hash,
                body,
                blobs,
                now_ms(),
            )?
        };
        self.publish_changed(&request.network_id, &request.conversation_id, &event.hash);
        Ok(event)
    }
    async fn require_scope(&self, access: &Access, scope: &str) -> Result<()> {
        if let Some(allowed) = &access.conversation_id {
            ensure!(allowed == scope, "outside this request's conversation");
        }
        let store = self.store.lock().await;
        let _verification = access.verification.enter();
        let invocation = &access.authorization.signed_invocation().payload;
        let facts = vibeface_secure::authorization::RequestFacts {
            resource: invocation.resource.clone(), action: invocation.action,
            audience: invocation.audience.clone(), operation_id: invocation.operation_id.clone(),
            body_hash: invocation.body_hash.clone(), context: invocation.context.clone(),
        };
        access.authorization.check_current_in(&store.connection, store.authorization_state(), &facts, now_ms() as u64)?;
        ensure!(
            store
                .authorized_conversation_ids(&access.authorization)?
                .iter()
                .any(|allowed| allowed == scope),
            "conversation resource is outside the grant"
        );
        Ok(())
    }
    async fn require_role(&self, network: &str, role: NodeRole) -> Result<()> {
        self.require_role_with_verification(network, role, &SignatureVerification::new())
            .await
    }

    async fn require_role_with_verification(
        &self,
        network: &str,
        role: NodeRole,
        verification: &SignatureVerification,
    ) -> Result<()> {
        SpaceId::parse(network)?;
        ensure!(
            self.config.roles.contains(&role),
            "node capability is disabled"
        );
        let store = self.store.lock().await;
        verification.run(|| store.authorize_node(network, &self.identity.node_id()?, None))?;
        Ok(())
    }
    async fn peer_request(&self, network: &str, target: &str, payload: Value) -> Result<Value> {
        if !self
            .hub
            .online(network)
            .await
            .iter()
            .any(|peer| peer == target)
        {
            let key = (network.to_owned(), target.to_owned());
            if self.peer_connecting.lock().await.insert(key.clone()) {
                let hub = self.hub.clone();
                let transport = self.network.clone();
                let connecting = self.peer_connecting.clone();
                tokio::spawn(async move {
                    if let Err(error) = hub
                        .connect_target(key.0.clone(), key.1.clone(), transport)
                        .await
                    {
                        tracing::debug!(%error,"scoped peer route unavailable");
                    }
                    connecting.lock().await.remove(&key);
                });
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if self
                        .hub
                        .online(network)
                        .await
                        .iter()
                        .any(|peer| peer == target)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .context("authenticated resource host is unreachable")?;
        }
        let reply = self.hub.request(network, target, payload).await?;
        if let Some(error) = reply["error"].as_str() {
            bail!("{error}");
        }
        Ok(reply)
    }
    pub async fn sync_all(&self, space: &str) -> Result<()> {
        let _work = self.maintenance.enter()?;
        if !self.config.roles.contains(&NodeRole::Replica) {
            return Ok(());
        }
        let service = self.audience()?.service;
        let targets =
            self.store
                .lock()
                .await
                .replication_targets(space, &service, now_ms() as u64)?;
        for (resource, audience, target) in targets {
            if target == self.identity.node_id()? {
                continue;
            }
            if let Err(error) = self
                .sync_resource_from(space, &resource, &audience, &target)
                .await
            {
                tracing::debug!(%error,"authorized resource synchronization is pending");
            }
        }
        Ok(())
    }

    async fn service_read_peer(
        &self,
        space: &str,
        resource: &vibeface_secure::authorization::ResourceId,
        audience: &Audience,
        target: &str,
        mut body: Value,
    ) -> Result<Value> {
        body["space_id"] = json!(space);
        body["resource_id"] = json!(resource);
        body["authorization_id"] = json!(uuid::Uuid::new_v4().to_string());
        let identity = vibeface_secure::Identity::from_secret_hex(&self.identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let key = PublicKey::of(&identity);
        let service = ServiceId::from_key(&key);
        let template =
            self.store
                .lock()
                .await
                .service_grant(&body, audience, &service, now_ms() as u64)?;
        ensure!(
            template.actor == Actor::Service(service)
                && template.device == DeviceId::from_key(&key),
            "replication grant belongs to another service device"
        );
        let invocation = Signed::sign(template, &identity)?;
        let kind = match field(&body, "method")? {
            "replica_inventory" => "inventory",
            "replica_event" => "event",
            "replica_blob" => "blob",
            _ => bail!("invalid replica read"),
        };
        tokio::time::timeout(
            Duration::from_secs(10),
            self.peer_request(
                space,
                target,
                json!({"type":kind,"authorization_body":body,"invocation":invocation}),
            ),
        )
        .await
        .context("authorized history peer is unreachable")?
    }

    async fn sync_resource_from(
        &self,
        space: &str,
        resource: &vibeface_secure::authorization::ResourceId,
        audience: &Audience,
        target: &str,
    ) -> Result<()> {
        let mut after: Option<String> = None;
        loop {
            let inventory = self
                .service_read_peer(
                    space,
                    resource,
                    audience,
                    target,
                    json!({"method":"replica_inventory","after":after,"limit":128}),
                )
                .await?;
            let wanted: Vec<String> = serde_json::from_value(inventory["events"].clone())?;
            self.fetch_resource_closure(space, resource, audience, target, &wanted)
                .await?;
            let next = inventory["next_cursor"].as_str().map(str::to_owned);
            if next.is_none() {
                break;
            }
            ensure!(next > after, "resource inventory did not advance");
            after = next;
        }
        Ok(())
    }

    async fn fetch_resource_closure(
        &self,
        space: &str,
        resource: &vibeface_secure::authorization::ResourceId,
        audience: &Audience,
        target: &str,
        wanted: &[String],
    ) -> Result<()> {
        let mut pending = wanted.to_vec();
        let mut seen = BTreeSet::new();
        while let Some(hash) = pending.pop() {
            ensure!(
                seen.len() < 100_000,
                "resource history exceeds bounded batch"
            );
            if !seen.insert(hash.clone()) {
                continue;
            }
            let local = self.store.lock().await.event(space, &hash)?;
            let envelope: SignedEvent = match local {
                Some(event) => event,
                None => serde_json::from_value(
                    self.service_read_peer(
                        space,
                        resource,
                        audience,
                        target,
                        json!({"method":"replica_event","hash":hash}),
                    )
                    .await?["event"]
                        .clone(),
                )?,
            };
            let event = self.store.lock().await.verify_event(space, &envelope)?;
            ensure!(
                event.resource_id == *resource,
                "history event is outside the authorized resource"
            );
            pending.extend(event.parents.clone());
            pending.extend(event.previous.clone());
            for blob in &event.blobs {
                if self.store.lock().await.has_blob(space, blob)? {
                    continue;
                }
                let mut bytes = Vec::with_capacity(blob.size as usize);
                while bytes.len() < blob.size as usize {
                    let chunk = self.service_read_peer(space,resource,audience,target,json!({"method":"replica_blob","hash":blob.hash,"offset":bytes.len(),"limit":65536})).await?;
                    ensure!(
                        chunk["hash"] == blob.hash
                            && chunk["offset"].as_u64() == Some(bytes.len() as u64)
                            && chunk["size"].as_u64() == Some(blob.size),
                        "attachment chunk belongs to another object"
                    );
                    let encoded = chunk["data"]
                        .as_str()
                        .context("attachment chunk data is required")?;
                    ensure!(encoded.len() <= 131072, "attachment chunk is too large");
                    let data = hex::decode(encoded)?;
                    ensure!(
                        !data.is_empty()
                            && data.len() <= 65536
                            && bytes.len() + data.len() <= blob.size as usize,
                        "invalid attachment chunk"
                    );
                    bytes.extend(data);
                }
                self.store.lock().await.put_blob(space, blob, &bytes)?;
            }
            self.store.lock().await.ingest_event(space, &envelope)?;
            self.publish_changed(space, &event.scope_id, &envelope.hash);
        }
        Ok(())
    }
    fn capabilities(&self) -> Value {
        json!({"executors":["codex","claude_code","cursor_agent"],"manager":self.config.manager.as_ref().map(|m|m.provider.clone())})
    }
    pub async fn rpc(self: &Arc<Self>, access: Access, input: Value) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        access.require_body(&input)?;
        self.check_execution_actor(&input, &access.authorization.signed_invocation().payload)?;
        let network = &access.network_id;
        {
            let store = self.store.lock().await;
            let _verification = access.verification.enter();
            let facts =
                self.authorization_facts_in_scope(&store, &access.authorization, &input, &self.audience()?, now_ms() as u64)?;
            store.authorize_scoped_request_for_transport(
                &self.identity,
                access.authorization.signed_invocation(),
                &facts,
                &access.transport,
                &input,
                now_ms() as u64,
            )?;
        }
        let method = field(&input, "method")?;
        let scope = input["conversation_id"].as_str().map(String::from);
        if let Some(scope) = &scope
            && method != "create_conversation"
        {
            self.require_scope(&access, scope).await?;
        }
        match method {
            "execution_share_request" => {
                ensure!(
                    access.account_actor(),
                    "execution sharing requires the user"
                );
                let store = self.store.lock().await;
                access.verification.run(|| {
                    store.authorize_request_read(
                        &access.authorization,
                        &access.freshness,
                        &access.transport,
                        now_ms() as u64,
                    )?;
                    store.execution_share_request_for_transport(
                        &self.identity,
                        &access.authorization,
                        &access.transport,
                        &input,
                        now_ms() as u64,
                    )
                })
            }
            "nodes" => {
                let mut store = self.store.lock().await;
                let _verification = access.verification.enter();
                store.authorize_request_read(
                    &access.authorization,
                    &access.freshness,
                    &access.transport,
                    now_ms() as u64,
                )?;
                ensure!(
                    *access.authorization.audience() == self.audience()?,
                    "grant targets another service"
                );
                Ok(
                    json!({"nodes":[{"node_id":self.identity.node_id()?,"audience":self.audience()?,"roles":self.config.roles,"local":true,"online":true,"capabilities":self.capabilities()}]}),
                )
            }
            "conversations" => {
                let mut store = self.store.lock().await;
                let _verification = access.verification.enter();
                store.authorize_request_read(
                    &access.authorization,
                    &access.freshness,
                    &access.transport,
                    now_ms() as u64,
                )?;
                let allowed = store.authorized_conversation_ids(&access.authorization)?;
                let conversations = store
                    .conversations(network)?
                    .into_iter()
                    .filter(|conversation| allowed.contains(&conversation.conversation_id))
                    .collect::<Vec<_>>();
                Ok(json!({"conversations":conversations}))
            }
            "create_conversation" => {
                ensure!(
                    access.account_actor(),
                    "only the user creates conversations"
                );
                let scope = id(&input, "conversation_id")?;
                let title = field(&input, "title")?;
                ensure!(title.len() <= 512, "title too long");
                {
                    let store = self.store.lock().await;
                    let _verification = access.verification.enter();
                    if let Some(old) = store
                        .conversations(network)?
                        .into_iter()
                        .find(|c| c.conversation_id == scope)
                    {
                        ensure!(old.title == title, "conversation ID already exists");
                        return Ok(json!(old));
                    }
                }
                self.append_authorized(
                    &access,
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
            "conversation" => {
                let scope = scope.context("conversation_id is required")?;
                let mut store = self.store.lock().await;
                let _verification = access.verification.enter();
                store.authorize_request_read(
                    &access.authorization,
                    &access.freshness,
                    &access.transport,
                    now_ms() as u64,
                )?;
                let context = store.context(network, &scope, &heads(&input)?)?;
                let operations = store
                    .operations(network)?
                    .into_iter()
                    .filter(|operation| operation.request.conversation_id == scope)
                    .collect::<Vec<_>>();
                let sessions = session_views(&store, network, &scope, &operations)?;
                let operations = operations
                    .iter()
                    .map(|operation| dispatch::execution_operation_view(&store, operation))
                    .collect::<Result<Vec<_>>>()?;
                Ok(
                    json!({"conversation":store.conversations(network)?.into_iter().find(|conversation|conversation.conversation_id==scope),"messages":store.messages(network,&scope)?,"tasks":task_views(&store,network,&scope)?,"heads":context.heads,"missing":context.missing,"sessions":sessions,"operations":operations}),
                )
            }
            "context" => {
                let scope = scope.context("conversation_id is required")?;
                let mut store = self.store.lock().await;
                let _verification = access.verification.enter();
                store.authorize_request_read(
                    &access.authorization,
                    &access.freshness,
                    &access.transport,
                    now_ms() as u64,
                )?;
                let context = store.context(network, &scope, &[])?;
                let offset = input["offset"].as_u64().unwrap_or(0).min(usize::MAX as u64) as usize;
                let limit = input["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
                if let Some(hash) = input["blob_hash"].as_str() {
                    ensure!(
                        context
                            .events
                            .iter()
                            .filter(|event| event.scope_id == scope)
                            .flat_map(|event| &event.blobs)
                            .any(|blob| blob.hash == hash),
                        "attachment is outside the authorized resource"
                    );
                    let bytes = store
                        .get_blob(network, hash)?
                        .context("attachment unavailable")?;
                    let end = offset.saturating_add(limit * 1024).min(bytes.len());
                    ensure!(offset <= end, "invalid attachment offset");
                    return Ok(
                        json!({"hash":hash,"offset":offset,"data_hex":hex::encode(&bytes[offset..end]),"next_offset":(end<bytes.len()).then_some(end),"size":bytes.len()}),
                    );
                }
                let events = context
                    .events
                    .into_iter()
                    .filter(|event| event.scope_id == scope)
                    .collect::<Vec<_>>();
                let end = offset.saturating_add(limit).min(events.len());
                ensure!(offset <= end, "invalid event offset");
                Ok(
                    json!({"heads":context.heads,"missing":context.missing,"events":&events[offset..end],"next_offset":(end<events.len()).then_some(end)}),
                )
            }
            "message" => {
                ensure!(
                    access.account_actor(),
                    "agents cannot recursively invoke managers"
                );
                let scope = scope.context("conversation_id is required")?;
                let message_id = id(&input, "message_id")?;
                let text = field(&input, "text")?.to_owned();
                let complete = {
                    let store = self.store.lock().await;
                    access.verification.run(|| {
                        Ok::<_, anyhow::Error>(
                            store
                                .context(network, &scope, &heads(&input)?)?
                                .missing
                                .is_complete(),
                        )
                    })?
                };
                ensure!(complete, "context is incomplete; wait for synchronization");
                let target = input["scheduler_node"]
                    .as_str()
                    .unwrap_or(&self.identity.node_id()?)
                    .to_owned();
                ensure!(
                    target == self.identity.node_id()?,
                    "message must target this resource host's scheduler"
                );
                let trigger = {
                    let mut store = self.store.lock().await;
                    let _verification = access.verification.enter();
                    let duplicate = store
                        .messages(network, &scope)?
                        .into_iter()
                        .find(|message| message.message_id == message_id);
                    if let Some(old) = duplicate {
                        ensure!(
                            old.text == text && old.role == "user",
                            "message ID reused with different content"
                        );
                        let envelope = store
                            .event(network, &old.event_hash)?
                            .context("message event missing")?;
                        let event = store.verify_event(network, &envelope)?;
                        ensure!(
                            matches!(&event.body, EventBody::MessagePosted { scheduler_node: Some(selected), .. } if selected == &target),
                            "retry this message through its original scheduler; send a new message to continue elsewhere"
                        );
                        old.event_hash
                    } else {
                        let event = store.append_request_authorized_event(
                            &self.identity,
                            &access.authorization,
                            &access.freshness,
                            &access.transport,
                            &access.body,
                            &scope,
                            EventBody::MessagePosted {
                                message_id,
                                role: "user".into(),
                                text,
                                scheduler_node: Some(target.clone()),
                            },
                            vec![],
                            now_ms(),
                        )?;
                        self.publish_changed(network, &scope, &event.hash);
                        event.hash
                    }
                };
                if target == self.identity.node_id()? {
                    if self.config.manager.is_some() {
                        self.require_role_with_verification(
                            network,
                            NodeRole::Scheduler,
                            &access.verification,
                        )
                        .await?;
                        self.spawn_scheduler(network.clone(), scope, trigger.clone())
                            .await?;
                    }
                    Ok(
                        json!({"state":"accepted","trigger":trigger,"manager_started":self.config.manager.is_some()}),
                    )
                } else {
                    self.peer_request(
                        network,
                        &target,
                        json!({"type":"schedule","conversation_id":scope,"trigger":trigger}),
                    )
                    .await
                }
            }
            "create_task" => {
                ensure!(
                    access.account_actor() || input["auto_followup"].as_bool() != Some(true),
                    "Agent cannot grant follow-up authority"
                );
                let scope = scope.context("conversation_id is required")?;
                let task_id = id(&input, "task_id")?;
                let title = field(&input, "title")?;
                let goal = field(&input, "goal")?;
                let acceptance = field(&input, "acceptance")?;
                let mut store = self.store.lock().await;
                let _verification = access.verification.enter();
                let existing = store
                    .tasks(network, &scope)?
                    .into_iter()
                    .find(|task| task.task_id == task_id);
                let terminal = existing
                    .as_ref()
                    .is_some_and(|task| matches!(task.status.as_str(), "accepted" | "cancelled"));
                let mut events = Vec::new();
                if let Some(old) = &existing {
                    ensure!(
                        old.title == title && old.goal == goal && old.acceptance == acceptance,
                        "task ID reused with different content"
                    );
                } else {
                    events.push((
                        scope.clone(),
                        EventBody::TaskCreated {
                            task_id: task_id.clone(),
                            title: title.into(),
                            goal: goal.into(),
                            acceptance: acceptance.into(),
                        },
                        vec![],
                    ));
                }
                if input["auto_followup"].as_bool() == Some(true)
                    && !terminal
                    && !followup_authorized(&store, network, &scope, &task_id)?
                {
                    events.push((
                        scope.clone(),
                        EventBody::DecisionRecorded {
                            decision_id: format!("auto:{task_id}"),
                            text: format!(
                                "User enabled bounded automatic follow-up for task {task_id}"
                            ),
                            effective_constraints: vec![format!("auto_followup:{task_id}")],
                            tool_configuration: json!({"auto_followup":task_id}),
                        },
                        vec![],
                    ));
                }
                let appended = if events.is_empty() {
                    Vec::new()
                } else {
                    store.append_request_authorized_events(
                        &self.identity,
                        &access.authorization,
                        &access.freshness,
                        &access.transport,
                        &access.body,
                        events,
                        now_ms(),
                    )?
                };
                for event in &appended {
                    self.publish_changed(network, &scope, &event.hash);
                }
                let creation = store.task_creation(network, &scope, &task_id)?;
                let event = store.verify_event(network, &creation)?;
                let authority = store.authorize_node(network, &event.author, None)?;
                let task = task_views(&store, network, &scope)?
                    .into_iter()
                    .find(|task| task["task_id"].as_str() == Some(task_id.as_str()))
                    .context("created task projection is unavailable")?;
                Ok(
                    json!({"task":task,"creation":creation,"authority_public_key":authority.public_key,"binding":store.task_binding(network,&creation.hash)?}),
                )
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
                    access.account_actor() || !matches!(status, "accepted" | "cancelled"),
                    "only the user accepts or cancels tasks"
                );
                let event = {
                    let mut store = self.store.lock().await;
                    let _verification = access.verification.enter();
                    require_task_write(&store, network, &scope, &task_id, access.account_actor())?;
                    store.append_request_authorized_event(
                        &self.identity,
                        &access.authorization,
                        &access.freshness,
                        &access.transport,
                        &access.body,
                        &scope,
                        EventBody::TaskChanged {
                            task_id,
                            status: status.into(),
                            detail: input["detail"]
                                .as_str()
                                .context("task detail is required")?
                                .to_owned(),
                        },
                        vec![],
                        now_ms(),
                    )?
                };
                self.publish_changed(network, &scope, &event.hash);
                Ok(json!(event))
            }
            "agent_notification" => {
                ensure!(
                    matches!(access.authorization.actor(), Actor::Service(_)),
                    "manager notifications require service authority"
                );
                let scope = scope.context("conversation_id is required")?;
                let event = self
                    .append_authorized(
                        &access,
                        &scope,
                        EventBody::NotificationRaised {
                            notification_id: id(&input, "notification_id")?,
                            kind: id(&input, "kind")?,
                            text: field(&input, "text")?.to_owned(),
                        },
                    )
                    .await?;
                Ok(json!({"event":event}))
            }
            "agent_result" => {
                ensure!(
                    matches!(access.authorization.actor(), Actor::Service(_)),
                    "manager results require service authority"
                );
                let scope = scope.context("conversation_id is required")?;
                let blobs: Vec<BlobRef> = serde_json::from_value(input["blobs"].clone())?;
                let events = vec![
                    (
                        scope.clone(),
                        EventBody::DecisionRecorded {
                            decision_id: id(&input, "decision_id")?,
                            text: field(&input, "text")?.to_owned(),
                            effective_constraints: serde_json::from_value(
                                input["effective_constraints"].clone(),
                            )?,
                            tool_configuration: input["tool_configuration"].clone(),
                        },
                        vec![],
                    ),
                    (
                        scope.clone(),
                        EventBody::MessagePosted {
                            message_id: id(&input, "message_id")?,
                            role: "assistant".into(),
                            text: field(&input, "output")?.to_owned(),
                            scheduler_node: None,
                        },
                        blobs,
                    ),
                ];
                let appended = {
                    let mut store = self.store.lock().await;
                    access.verification.run(|| {
                        store.append_request_authorized_events(
                            &self.identity,
                            &access.authorization,
                            &access.freshness,
                            &access.transport,
                            &access.body,
                            events,
                            now_ms(),
                        )
                    })?
                };
                for event in &appended {
                    self.publish_changed(network, &scope, &event.hash);
                }
                Ok(json!({"events":appended}))
            }
            "bind_execution" => {
                let mut store = self.store.lock().await;
                let _verification = access.verification.enter();
                let binding = if self.is_execution_dispatch(&input)? {
                    let dispatch = store.execution_dispatch_facts_in_scope(
                        &access.authorization,
                        &input,
                        access.authorization.audience(),
                        now_ms() as u64,
                    )?;
                    store.bind_execution_request_dispatch(
                        &self.identity,
                        &dispatch,
                        &access.authorization,
                        &access.freshness,
                        &access.transport,
                        &access.body,
                        now_ms() as u64,
                    )?
                } else {
                    store.bind_task_request_authorized(
                        &self.identity,
                        &access.authorization,
                        &access.freshness,
                        &access.transport,
                        &access.body,
                        now_ms() as u64,
                    )?
                };
                Ok(json!({"binding":binding}))
            }
            "seal_execution" => {
                let seal = {
                    let mut store = self.store.lock().await;
                    access.verification.run(|| {
                        store.seal_task_request_authorized(
                            &self.identity,
                            &access.authorization,
                            &access.freshness,
                            &access.transport,
                            &input,
                            now_ms() as u64,
                        )
                    })?
                };
                Ok(json!({"seal":seal}))
            }
            "seal_remote_execution" => self.seal_remote_execution(&access, &input).await,
            "remote_execution_seal_status" => {
                let store = self.store.lock().await;
                let seal = access.verification.run(|| {
                    store.remote_execution_seal_status_request_authorized(
                        &self.identity,
                        &access.authorization,
                        &access.freshness,
                        &access.transport,
                        &input,
                        now_ms() as u64,
                    )
                })?;
                Ok(json!({"seal":seal}))
            }
            "rebind_execution" => self.rebind_execution(&access, &input).await,
            "operate" => {
                let request: OperationRequest = serde_json::from_value(input["request"].clone())?;
                self.require_scope(&access, &request.conversation_id)
                    .await?;
                self.dispatch(access, request).await
            }
            "session" => {
                let target = field(&input, "target_node")?;
                if target != self.identity.node_id()? {
                    return self
                        .remote_session_status(&access, target, field(&input, "session_id")?)
                        .await;
                }
                ensure!(
                    target == self.identity.node_id()?,
                    "query the target service directly"
                );
                let mut store = self.store.lock().await;
                let _verification = access.verification.enter();
                store.authorize_request_read(
                    &access.authorization,
                    &access.freshness,
                    &access.transport,
                    now_ms() as u64,
                )?;
                Ok(json!({"session":store.session(network,target,field(&input,"session_id")?)?}))
            }
            "operation" => {
                let target = field(&input, "target_node")?;
                if target != self.identity.node_id()? {
                    return self
                        .remote_operation_status(&access, target, field(&input, "operation_id")?)
                        .await;
                }
                ensure!(
                    target == self.identity.node_id()?,
                    "query the target service directly"
                );
                let mut store = self.store.lock().await;
                let _verification = access.verification.enter();
                store.authorize_request_read(
                    &access.authorization,
                    &access.freshness,
                    &access.transport,
                    now_ms() as u64,
                )?;
                Ok(json!({"operation":store.operation(network,field(&input,"operation_id")?)?}))
            }
            "notifications" => {
                ensure!(
                    input["client_id"].as_str() == Some(access.authorization.device().as_str()),
                    "notification client differs from authenticated device"
                );
                let mut store = self.store.lock().await;
                let _verification = access.verification.enter();
                store.authorize_request_read(
                    &access.authorization,
                    &access.freshness,
                    &access.transport,
                    now_ms() as u64,
                )?;
                let allowed = store.authorized_conversation_ids(&access.authorization)?;
                let notifications = store
                    .pending_notifications(network, access.authorization.device().as_str())?
                    .into_iter()
                    .filter(|notification| allowed.contains(&notification.conversation_id))
                    .collect::<Vec<_>>();
                Ok(json!({"notifications":notifications}))
            }
            "ack" => {
                ensure!(
                    access.account_actor(),
                    "only clients acknowledge notifications"
                );
                let notification = id(&input, "notification_id")?;
                let client = id(&input, "client_id")?;
                ensure!(
                    client == access.authorization.device().as_str(),
                    "notification client differs from authenticated device"
                );
                let pending = self
                    .store
                    .lock()
                    .await
                    .pending_notifications(network, &client)?;
                if let Some(n) = pending
                    .into_iter()
                    .find(|n| n.notification_id == notification)
                {
                    self.append_authorized(
                        &access,
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
                let mut store = self.store.lock().await;
                let _verification = access.verification.enter();
                store.authorize_request_read(
                    &access.authorization,
                    &access.freshness,
                    &access.transport,
                    now_ms() as u64,
                )?;
                Ok(json!(store.export_snapshot(network, &scope)?))
            }
            "reconcile" => {
                ensure!(access.account_actor(), "reconciliation requires the user");
                {
                    let store = self.store.lock().await;
                    access.verification.run(|| {
                        store.authorize_request_read(
                            &access.authorization,
                            &access.freshness,
                            &access.transport,
                            now_ms() as u64,
                        )
                    })?;
                }
                let target = field(&input, "target_node")?;
                if target != self.identity.node_id()? {
                    self.remote_operation_status(&access, target, field(&input, "operation_id")?)
                        .await
                } else {
                    self.reconcile(network, target, field(&input, "operation_id")?)
                        .await
                }
            }
            "resolve_unknown" => {
                ensure!(access.account_actor(), "resolution requires the user");
                if self.is_execution_resolution(&input)? {
                    self.resolve_execution_dispatch(&access).await
                } else {
                    self.resolve_unknown(&access, &input).await
                }
            }
            _ => bail!("unknown method"),
        }
    }
    pub async fn tool(
        self: &Arc<Self>,
        session: AgentSession,
        name: &str,
        args: Value,
        call_id: &str,
    ) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        ensure!(args.is_object(), "tool arguments must be an object");
        let verification = SignatureVerification::new();
        let execution_args = matches!(
            name,
            "bind_execution" | "open_execution" | "send_execution" | "execution_status"
        )
        .then(|| args.clone());
        {
            let store = self.store.lock().await;
            verification.run(|| {
                store.manager_tool_authorization(
                    session.space_id.as_str(),
                    &format!("manager:{}", session.sdk_session.session_id),
                    now_ms() as u64,
                )
            })?;
        }
        let mut body = match name {
            "nodes" => {
                let mut body = args;
                body["method"] = json!("nodes");
                body
            }
            "context" => {
                let mut body = args;
                body["method"] = json!("context");
                body
            }
            "create_task" | "update_task" => {
                let mut body = args;
                body["method"] = json!(name);
                ensure!(
                    body["auto_followup"].as_bool() != Some(true),
                    "Agent cannot grant follow-up authority"
                );
                body
            }
            "bind_execution" => {
                let mut body = args;
                let task_id = id(&body, "task_id")?;
                let store = self.store.lock().await;
                verification.run(|| -> Result<()> {
                    let creation = store.task_creation(
                        session.space_id.as_str(),
                        &session.conversation_id,
                        &task_id,
                    )?;
                    let event = store.verify_event(session.space_id.as_str(), &creation)?;
                    ensure!(
                        event.resource_id == session.resource_id,
                        "task belongs to another resource"
                    );
                    body["method"] = json!("bind_execution");
                    body["task_ref"] = json!(creation.hash);
                    Ok(())
                })?;
                body
            }
            "open_execution" | "send_execution" => {
                let operation = id(&args, "operation_id")?;
                let action = if name == "open_execution" {
                    json!({"type":"open","executor":field(&args,"executor")?,"cwd":field(&args,"cwd")?})
                } else {
                    json!({"type":"send","text":field(&args,"text")?})
                };
                let request = OperationRequest {
                    operation_id: operation.clone(),
                    network_id: session.space_id.to_string(),
                    conversation_id: session.conversation_id.clone(),
                    task_id: Some(id(&args, "task_id")?),
                    target_node: field(&args, "target_node")?.to_owned(),
                    session_id: id(&args, "session_id")?,
                    expected_revision: args["expected_revision"].as_u64().unwrap_or(0),
                    basis_heads: self
                        .store
                        .lock()
                        .await
                        .heads(session.space_id.as_str(), &session.conversation_id)?,
                    action,
                    resources: vec![],
                    binding: None,
                };
                json!({"method":"operate","request":request,"authorization_id":operation})
            }
            "execution_status" => {
                json!({"method":"session","target_node":field(&args,"target_node")?,"session_id":field(&args,"session_id")?})
            }
            _ => bail!("unknown or unavailable tool"),
        };
        body["space_id"] = json!(session.space_id);
        body["resource_id"] = json!(session.resource_id);
        body["conversation_id"] = json!(session.conversation_id);
        body["sdk_session"] = json!(session.sdk_session);
        body["parent_invocation"] = json!(session.parent_invocation);
        if body.get("authorization_id").is_none() {
            body["authorization_id"] = json!(call_id);
        }
        if let Some(args) = execution_args {
            self.prepare_execution_tool_body(&session, name, &args, &mut body, &verification)
                .await?;
        }
        let body = self
            .normalize_request_body_with_verification(&session.space_id, body, &verification)
            .await?;
        let access = self
            .authorize_agent_with_verification(&session, body, verification.clone())
            .await?;
        let body = access.body.clone();
        let mut result = self.rpc(access, body.clone()).await?;
        if name == "nodes" {
            result["execution_targets"] = serde_json::to_value(
                self.execution_tool_targets(&session, &body, &verification)
                    .await?,
            )?;
        }
        Ok(result)
    }

    async fn authorize_agent(&self, session: &AgentSession, body: Value) -> Result<Access> {
        self.authorize_agent_with_verification(session, body, SignatureVerification::new())
            .await
    }

    async fn authorize_agent_with_verification(
        &self,
        session: &AgentSession,
        mut body: Value,
        verification: SignatureVerification,
    ) -> Result<Access> {
        let now = now_ms() as u64;
        let mut session = {
            let mut store = self.store.lock().await;
            verification.run(|| -> Result<_> {
            let session = store.refresh_agent_session(session, now)?;
            let origin = store.agent_origin(&session, now)?;
            if let Some((task, reference)) = origin.task {
                ensure!(
                    body["method"] != "create_task",
                    "automatic continuation cannot create another task"
                );
                for claimed in [
                    body["task_id"].as_str(),
                    body["request"]["task_id"].as_str(),
                ]
                .into_iter()
                .flatten()
                {
                    ensure!(
                        claimed == task,
                        "automatic continuation cannot switch tasks"
                    );
                }
                body["task_id"] = json!(task);
                body["task_ref"] = json!(reference);
                let saved: Option<String> = if body["method"] == "agent_start" {
                    use rusqlite::OptionalExtension;
                    store.connection.query_row("SELECT source_body FROM accepted_authorizations WHERE space_id=? AND authorization_id=?",
                        rusqlite::params![session.space_id.as_str(), body["authorization_id"].as_str().context("agent start has no operation ID")?],
                        |row| row.get(0)).optional()?
                } else {
                    None
                };
                body["basis_heads"] = match saved {
                    Some(saved) => serde_json::from_str::<Value>(&saved)?["basis_heads"].clone(),
                    None => {
                        json!(store.retained_resource_heads(session.space_id.as_str(), &session.resource_id, &session.conversation_id)?)
                    }
                };
            }
            Ok(session)
            })?
        };
        let task_policy = self
            .ensure_sdk_execution_task_policy(&session, &body)
            .await?;
        let now = now_ms() as u64;
        let facts = {
            let store = self.store.lock().await;
            verification.run(|| -> Result<_> {
                let origin = store.agent_origin(&session, now)?;
                let facts = if let Some(task) = &task_policy {
                    store
                        .execution_dispatch_facts_with_task_policy(
                            task,
                            &body,
                            &self.audience()?,
                            now,
                        )?
                        .facts()
                        .clone()
                } else {
                    self.authorization_facts(&store, &body, &self.audience()?, now)?
                };
                let policies = task_policy
                    .as_ref()
                    .map_or(store.authorization_state(), |task| task.state());
                let delegated = policies.verify_service_delegation(
                    &origin.parent,
                    &session.service,
                    &session.grant_leaf,
                    &facts,
                    now,
                )?;
                ensure!(
                    delegated.device() == &session.device
                        && delegated.initiator() == &session.initiator
                        && delegated.initiating_grant() == session.initiating_grant.as_ref(),
                    "SDK request differs from its current admitted delegation"
                );
                let mut expected: BTreeMap<_, _> = delegated
                    .heads()
                    .iter()
                    .cloned()
                    .map(|head| (head.domain.clone(), head))
                    .collect();
                if let Some(destination) = crate::identity::policy::execution_destination_for_body(
                    policies, &facts, &body, now,
                )? {
                    for head in destination.heads() {
                        ensure!(
                            expected
                                .insert(head.domain.clone(), head.clone())
                                .is_none_or(|previous| previous == *head),
                            "SDK execution destination has conflicting policy heads"
                        );
                    }
                }
                session.expected_policies = expected.into_values().collect();
                session.expires_at_ms = session.expires_at_ms.min(delegated.expires_at_ms());
                if let Some(task) = &task_policy {
                    session.expires_at_ms = session.expires_at_ms.min(task.expires_at_ms());
                }
                Ok(facts)
            })?
        };
        let identity = vibeface_secure::Identity::from_secret_hex(&self.identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let key = PublicKey::of(&identity);
        ensure!(
            ServiceId::from_key(&key) == session.service
                && DeviceId::from_key(&key) == session.device,
            "Agent session is owned by another service"
        );
        ensure!(now < session.expires_at_ms, "Agent delegation expired");
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Service(session.service.clone()),
                device: session.device.clone(),
                initiator: session.initiator,
                initiating_grant: session.initiating_grant,
                resource: facts.resource,
                action: facts.action,
                audience: facts.audience,
                presenter: session.device,
                operation_id: facts.operation_id,
                body_hash: facts.body_hash,
                grant_leaf: session.grant_leaf,
                expected_policies: session.expected_policies,
                context: facts.context,
                nonce: Nonce::random()?,
                not_before_ms: now,
                expires_at_ms: now
                    .saturating_add(INVOCATION_MAX_AGE_MS)
                    .min(session.expires_at_ms),
            },
            &identity,
        )?;
        self.authorize_rpc_with_verification(
            body,
            invocation,
            TransportEvidence::LocalAgentAdapter {
                service: session.service,
                session: session.sdk_session,
            },
            verification,
        )
        .await
    }

    pub(crate) async fn claim_manager(
        &self,
        session: &AgentSession,
        trigger: &str,
        boot: &str,
        sdk_config: &str,
    ) -> Result<crate::identity::admission::ManagerAdmission> {
        let body = json!({"method":"agent_start","space_id":session.space_id,
            "resource_id":session.resource_id,"authorization_id":format!("manager:{trigger}"),
            "conversation_id":session.conversation_id,"trigger":trigger,
            "scheduler_node":self.identity.node_id()?,
            "sdk_session":session.sdk_session,"parent_invocation":session.parent_invocation,
            "sdk_config":sdk_config});
        let access = self.authorize_agent(session, body.clone()).await?;
        self.store.lock().await.claim_authorized_manager(
            &access.authorization,
            &access.freshness,
            access
                .transport
                .existing()
                .context("manager admission requires a local adapter transport")?,
            &access.body,
            boot,
            sdk_config,
            now_ms() as u64,
        )
    }

    pub(crate) async fn renew_manager_authorization(
        &self,
        context: &ExecutionContext,
    ) -> Result<u64> {
        let auth = self.store.lock().await.manager_authorization(
            context.space_id.as_str(),
            &context.operation_id,
            now_ms() as u64,
        )?;
        let proof = self.freshness(&auth, CheckStage::Renew).await?;
        self.store.lock().await.renew_authorized_manager(
            context.space_id.as_str(),
            &context.operation_id,
            &proof,
            now_ms() as u64,
        )
    }

    pub async fn record_agent_result(
        self: &Arc<Self>,
        session: &AgentSession,
        mut body: Value,
    ) -> Result<Value> {
        body["method"] = json!("agent_result");
        body["space_id"] = json!(session.space_id);
        body["resource_id"] = json!(session.resource_id);
        body["conversation_id"] = json!(session.conversation_id);
        body["sdk_session"] = json!(session.sdk_session);
        body["parent_invocation"] = json!(session.parent_invocation);
        let access = self.authorize_agent(session, body.clone()).await?;
        let body = access.body.clone();
        self.rpc(access, body).await
    }

    pub async fn agent_notification(
        self: &Arc<Self>,
        session: &AgentSession,
        kind: &str,
        text: &str,
    ) -> Result<Value> {
        let id = format!("manager:{}:{}", session.sdk_session.session_id, kind);
        let body = json!({
            "method":"agent_notification","space_id":session.space_id,"resource_id":session.resource_id,
            "conversation_id":session.conversation_id,"sdk_session":session.sdk_session,"parent_invocation":session.parent_invocation,
            "authorization_id":id,"notification_id":id,"kind":kind,"text":text,
        });
        let access = self.authorize_agent(session, body.clone()).await?;
        let body = access.body.clone();
        self.rpc(access, body).await
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

    async fn prepare_operation_binding(
        self: &Arc<Self>,
        request: &mut OperationRequest,
        verification: &SignatureVerification,
    ) -> Result<()> {
        if request.binding.is_none() {
            let task = request
                .task_id
                .as_ref()
                .context("create a task and explicitly bind its execution before operating")?;
            let store = self.store.lock().await;
            let _verification = verification.enter();
            let creation =
                store.task_creation(&request.network_id, &request.conversation_id, task)?;
            request.binding = Some(
                store
                    .task_binding(&request.network_id, &creation.hash)?
                    .context("signed bind_execution must complete before operating")?,
            );
        }
        let store = self.store.lock().await;
        verification.run(|| store.validate_operation_binding(request))
    }

    async fn rebind_execution(self: &Arc<Self>, access: &Access, input: &Value) -> Result<Value> {
        access.require_body(input)?;
        let binding = {
            let mut store = self.store.lock().await;
            access.verification.run(|| {
                let now = now_ms() as u64;
                if self.is_execution_dispatch(input)? {
                    let dispatch = store.execution_dispatch_facts_in_scope(
                        &access.authorization,
                        input,
                        access.authorization.audience(),
                        now,
                    )?;
                    store.rebind_execution_dispatch_request_authorized(
                        &self.identity,
                        &dispatch,
                        &access.authorization,
                        &access.freshness,
                        &access.transport,
                        input,
                        now,
                    )
                } else {
                    store.rebind_task_request_authorized(
                        &self.identity,
                        &access.authorization,
                        &access.freshness,
                        &access.transport,
                        input,
                        now,
                    )
                }
            })?
        };
        Ok(json!({"binding":binding}))
    }

    pub async fn dispatch(
        self: &Arc<Self>,
        access: Access,
        request: OperationRequest,
    ) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        ensure!(
            request.network_id == access.network_id,
            "operation belongs to another space"
        );
        self.require_scope(&access, &request.conversation_id)
            .await?;
        ensure!(
            serde_json::to_value(&request)? == access.body["request"],
            "execution request differs from signed body"
        );
        if self.is_execution_dispatch(&access.body)? {
            return self.dispatch_execution(&access, request).await;
        }
        ensure!(
            request.target_node == self.identity.node_id()?,
            "ordinary execution must target its authorized audience"
        );
        {
            let store = self.store.lock().await;
            access
                .verification
                .run(|| store.validate_operation_binding(&request))?;
        }
        self.admit_operation(request, &access).await
    }
    async fn admit_operation(
        self: &Arc<Self>,
        request: OperationRequest,
        access: &Access,
    ) -> Result<Value> {
        let _work = self.maintenance.enter()?;
        self.require_role_with_verification(
            &request.network_id,
            NodeRole::Executor,
            &access.verification,
        )
        .await?;
        ensure!(
            request.target_node == self.identity.node_id()?,
            "wrong execution target"
        );
        let kind = field(&request.action, "type")?;
        ensure!(
            access.account_actor() || kind != "respond_request",
            "only the user may answer approvals"
        );
        let mut store = self.store.lock().await;
        let _verification = access.verification.enter();
        let current = store.session(
            &request.network_id,
            &request.target_node,
            &request.session_id,
        )?;
        let existing_operation = store.operation(&request.network_id, &request.operation_id)?;
        if !access.account_actor()
            && existing_operation.is_none()
            && matches!(kind, "open" | "send" | "steer")
        {
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
            ensure!(
                request.resources == resources,
                "execution resources differ from signed canonical request"
            );
        }
        if let Some(old) = existing_operation {
            ensure!(
                old.request == request,
                "operation ID reused with different body"
            );
            let operation = store.accept_request_authorized_operation(
                &self.identity,
                &access.authorization,
                &access.freshness,
                &access.transport,
                &access.body,
                &request,
                now_ms() as u64,
            )?;
            return Ok(json!({"operation":operation}));
        }
        let operation = store.accept_request_authorized_operation(
            &self.identity,
            &access.authorization,
            &access.freshness,
            &access.transport,
            &access.body,
            &request,
            now_ms() as u64,
        )?;
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

    async fn supervise_execution(self: &Arc<Self>, mut context: ExecutionContext) {
        let key = (context.space_id.to_string(), context.operation_id.clone());
        if !self.execution_leases.lock().await.insert(key.clone()) {
            return;
        }
        let node = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let operation = node
                    .store
                    .lock()
                    .await
                    .operation(context.space_id.as_str(), &context.operation_id);
                match operation {
                    Ok(Some(record)) if record.state.terminal() => break,
                    Ok(Some(record)) if record.state == OperationState::Running => {}
                    _ => {
                        let _ = node
                            .stop_expired_execution(&context, "execution outcome is unresolved")
                            .await;
                        break;
                    }
                }
                let renewal: Result<u64> = async {
                    let auth = node.store.lock().await.operation_authorization(
                        context.space_id.as_str(),
                        &context.operation_id,
                        now_ms() as u64,
                    )?;
                    let proof = node.freshness(&auth, CheckStage::Renew).await?;
                    let deadline = node.store.lock().await.renew_authorized_operation(
                        context.space_id.as_str(),
                        &context.operation_id,
                        &proof,
                        now_ms() as u64,
                    )?;
                    if context.shared {
                        node.engine.renew_operation(&context, deadline).await?;
                    }
                    Ok(deadline)
                }
                .await;
                match renewal {
                    Ok(deadline) => context.lease_expires_at_ms = deadline,
                    Err(error) => {
                        let _ = node
                            .stop_expired_execution(
                                &context,
                                &format!("authorization lease could not renew: {error:#}"),
                            )
                            .await;
                        break;
                    }
                }
            }
            node.execution_leases.lock().await.remove(&key);
        });
    }

    async fn cancel_personal_execution(
        &self,
        context: &ExecutionContext,
        affected: &mut Vec<(String, String)>,
    ) -> Result<()> {
        let turn = {
            let store = self.store.lock().await;
            let operation = store
                .operation(context.space_id.as_str(), &context.operation_id)?
                .context("cancelled operation is missing")?;
            let session = store
                .session(
                    context.space_id.as_str(),
                    &operation.request.target_node,
                    &operation.request.session_id,
                )?
                .context("cancelled execution session is missing")?;
            let turn = store.local_value(
                context.space_id.as_str(),
                &format!("turn:{}", context.operation_id),
            )?;
            session
                .native_session_id
                .zip(turn.and_then(|value| value["turn"]["id"].as_str().map(str::to_owned)))
        };
        if let Some((native_id, turn_id)) = turn
            && self
                .engine
                .cancel_personal_turn(context, &native_id, &turn_id)
                .await
                .is_ok()
        {
            return Ok(());
        }
        let mut managed = self._codex_process.lock().await;
        let child = managed.as_mut().context("original turn cancellation could not be confirmed and this Node does not own the Provider process")?;
        let pid = child
            .id()
            .context("managed Provider process has no live identity")?;
        {
            let store = self.store.lock().await;
            for space in store.networks()? {
                for operation in store
                    .operations(&space)?
                    .into_iter()
                    .filter(|record| !record.state.terminal())
                {
                    if let Some(session) = store.session(
                        &space,
                        &operation.request.target_node,
                        &operation.request.session_id,
                    )? && session.executor == "codex"
                        && !execution_context(&store, &operation.request, 0)?.shared
                    {
                        affected.push((space.clone(), operation.request.operation_id));
                    }
                }
            }
        }
        let group = format!("-{pid}");
        let _ = tokio::process::Command::new("/bin/kill")
            .args(["-15", &group])
            .status()
            .await?;
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
            let _ = tokio::process::Command::new("/bin/kill")
                .args(["-9", &group])
                .status()
                .await?;
            tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .context("managed Provider did not exit after cancellation")??;
        }
        let still_alive = tokio::process::Command::new("/bin/kill")
            .args(["-0", &group])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await?
            .success();
        ensure!(
            !still_alive,
            "managed Provider process group still has live processes"
        );
        Ok(())
    }

    async fn stop_expired_execution(
        self: &Arc<Self>,
        context: &ExecutionContext,
        reason: &str,
    ) -> Result<()> {
        let mut affected = self
            .engine
            .affected_operations(context)
            .await
            .into_iter()
            .map(|id| (context.space_id.to_string(), id))
            .collect::<Vec<_>>();
        affected.push((context.space_id.to_string(), context.operation_id.clone()));
        let cancellation = if context.shared {
            self.engine
                .cancel_operation(context)
                .await
                .map(|operations| {
                    affected.extend(
                        operations
                            .into_iter()
                            .map(|id| (context.space_id.to_string(), id)),
                    );
                })
        } else {
            self.cancel_personal_execution(context, &mut affected).await
        };
        affected.sort();
        affected.dedup();
        for (space, operation_id) in affected {
            let record = self.store.lock().await.operation(&space, &operation_id)?;
            if let Some(record) = record
                && !record.state.terminal()
            {
                self.finish(&record.request, OperationState::Unknown, json!({
                    "error":reason,"cancellation_confirmed":cancellation.is_ok(),
                    "cancellation_error":cancellation.as_ref().err().map(|error|format!("{error:#}")),
                })).await?;
            }
        }
        cancellation
    }
    async fn execute_operation(self: Arc<Self>, request: OperationRequest) -> Result<()> {
        let _work = self.maintenance.enter()?;
        let action = self.engine_action(&request.action)?;
        let preparation: Result<(ExecutionSession, ExecutionContext)> = async {
            self.require_role(&request.network_id, NodeRole::Executor)
                .await?;
            let authorization = self.store.lock().await.operation_authorization(
                &request.network_id,
                &request.operation_id,
                now_ms() as u64,
            )?;
            let freshness = self.freshness(&authorization, CheckStage::Start).await?;
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
            let (_, deadline) = store.claim_authorized_operation(
                &request.network_id,
                &request.operation_id,
                &freshness,
                now_ms() as u64,
            )?;
            let context = execution_context(&store, &request, deadline)?;
            Ok((session, context))
        }
        .await;
        let (session, context) = match preparation {
            Ok(prepared) => prepared,
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
        if !context.shared && native.executor == ExecutorKind::Codex {
            if let Err(error) = self.ensure_personal_provider().await {
                self.finish(&request, OperationState::Failed,
                    json!({"error":format!("private Provider could not prepare before dispatch: {error:#}")})).await?;
                return Err(error);
            }
        }
        self.supervise_execution(context.clone()).await;
        let store = self.store.clone();
        let mut persisted = session.clone();
        let network = request.network_id.clone();
        let result = self
            .engine
            .execute_authorized(&context, &native, action.clone(), move |id| async move {
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
                    self.append_operation_fact(
                        &request,
                        EventBody::ExecutionObserved {
                            operation_id: request.operation_id.clone(),
                            task_id: request.task_id.clone(),
                            target_node: request.target_node.clone(),
                            session_id: request.session_id.clone(),
                            status: "running".into(),
                            detail: result.result,
                        },
                        vec![],
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
        self.finish_with_verification(request, state, result, &SignatureVerification::new())
            .await
    }

    async fn finish_with_verification(
        self: &Arc<Self>,
        request: &OperationRequest,
        mut state: OperationState,
        mut result: Value,
        verification: &SignatureVerification,
    ) -> Result<()> {
        let _work = self.maintenance.enter()?;
        let _completion = self.completion_lock.lock().await;
        let current = self
            .store
            .lock()
            .await
            .operation(&request.network_id, &request.operation_id)?
            .context("completed operation is missing")?;
        if current.state == OperationState::Unknown && state.terminal() {
            return Ok(());
        }
        if state.terminal() && !current.state.terminal() {
            let context = {
                let store = self.store.lock().await;
                verification.run(|| execution_context(&store, request, 0))?
            };
            let affected = self.engine.affected_operations(&context).await;
            if let Err(error) = self.engine.complete_operation(&context).await {
                state = OperationState::Unknown;
                result = json!({"error":format!("execution cleanup could not be confirmed: {error:#}"),"observed_result":result});
                let mut store = self.store.lock().await;
                let _verification = verification.enter();
                for operation_id in affected
                    .into_iter()
                    .filter(|id| id != &request.operation_id)
                {
                    if let Some(operation) = store.operation(&request.network_id, &operation_id)?
                        && !operation.state.terminal()
                    {
                        store.local_set(
                            &request.network_id,
                            &format!("completion:{operation_id}"),
                            &json!({"state":OperationState::Unknown,"result":result}),
                        )?;
                        store.finish_operation(
                            &request.network_id,
                            &operation_id,
                            OperationState::Unknown,
                            result.clone(),
                        )?;
                    }
                }
            }
        }
        let status = serde_json::to_value(&state)?.as_str().unwrap().to_owned();
        let completion_key = format!("completion:{}", request.operation_id);
        {
            let mut store = self.store.lock().await;
            let _verification = verification.enter();
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
        let existing = {
            let store = self.store.lock().await;
            verification.run(|| Ok::<_, anyhow::Error>(store.context(&request.network_id, &request.conversation_id, &[])?.events.into_iter().find(|event| matches!(&event.body, EventBody::ExecutionObserved { operation_id, status: observed, .. } if operation_id == &request.operation_id && observed == &status))))?
        };
        let event_hash = if let Some(existing) = existing {
            crypto::digest(&serde_json::to_vec(&existing)?)
        } else {
            self.append_operation_fact_with_verification(
                request,
                EventBody::ExecutionObserved {
                    operation_id: request.operation_id.clone(),
                    task_id: request.task_id.clone(),
                    target_node: request.target_node.clone(),
                    session_id: request.session_id.clone(),
                    status: status.clone(),
                    detail,
                },
                blobs,
                verification,
            )
            .await?
            .hash
        };
        if matches!(request.action["type"].as_str(), Some("send")) {
            let notification_id = format!("result:{}:{}", request.operation_id, status);
            let already = {
                let store = self.store.lock().await;
                verification.run(|| Ok::<_, anyhow::Error>(store.context(&request.network_id, &request.conversation_id, &[])?.events.iter().any(|event| matches!(&event.body, EventBody::NotificationRaised { notification_id: id, .. } if id == &notification_id))))?
            };
            if !already {
                let event = {
                    let mut store = self.store.lock().await;
                    verification.run(|| {
                        store.append_derived_event(
                            &self.identity,
                            &request.network_id,
                            &event_hash,
                            EventBody::NotificationRaised {
                                notification_id,
                                kind: status.clone(),
                                text: format!(
                                    "执行会话 {} 的本轮执行状态为 {}。工单验收仍需本人确认。",
                                    request.session_id, status
                                ),
                            },
                            vec![],
                            now_ms(),
                        )
                    })?
                };
                self.publish_changed(&request.network_id, &request.conversation_id, &event.hash);
            }
            if state.terminal()
                && !self
                    .maybe_followup(request, &event_hash, verification)
                    .await?
            {
                return Ok(());
            }
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
        ensure!(
            target == self.identity.node_id()?,
            "read another service through a signed request to its audience"
        );
        let session = self
            .store
            .lock()
            .await
            .session(network, target, session_id)?
            .context("execution session not found")?;
        let context = {
            let store = self.store.lock().await;
            let operation = store
                .operations(network)?
                .into_iter()
                .rev()
                .find(|record| {
                    record.request.target_node == target && record.request.session_id == session_id
                })
                .context("execution session has no admitted operation")?;
            execution_context(&store, &operation.request, 0)?
        };
        let snapshot = self
            .engine
            .snapshot_authorized(&context)
            .await
            .unwrap_or_else(|error| json!({"error":format!("{error:#}"),"pending_requests":[]}));
        let read = match &session.native_session_id {
            Some(native_id) => {
                self.engine
                    .read_session_authorized(&context, native_id)
                    .await
            }
            None => Err(anyhow::anyhow!(
                "execution session has no recorded native ID"
            )),
        };
        let actual = match read {
            Ok(thread) => json!({"thread":thread,"pending_requests":snapshot["pending_requests"]}),
            Err(error) => {
                json!({"error":format!("{error:#}"),"freshness":"unknown","pending_requests":snapshot["pending_requests"]})
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
        let verification = message.verification.clone();
        let envelope = &message.payload;
        let kind = field(envelope, "type")?;
        let certificate = {
            let store = self.store.lock().await;
            verification.run(|| store.authorize_transport_node(&message.origin, now_ms() as u64))?
        };
        if matches!(
            kind,
            "execution_observation"
                | "execution_handoff_observe"
                | "execution_handoff_observation_receipts"
        ) {
            let origin = DeviceId::from_key(&PublicKey::parse(&certificate.public_key)?);
            return self
                .handle_execution_observation_peer(message, origin, &verification)
                .await;
        }
        if matches!(
            kind,
            "execution_handoff_submit"
                | "execution_handoff_status"
                | "execution_handoff_receipts"
                | "execution_handoff_resolve"
                | "execution_handoff_resolution_receipts"
        ) {
            let origin = DeviceId::from_key(&PublicKey::parse(&certificate.public_key)?);
            return self
                .handle_handoff_peer(message, origin, &verification)
                .await;
        }
        if kind == "freshness_receipt" {
            let challenge: FreshnessChallenge =
                serde_json::from_value(envelope["challenge"].clone())?;
            ensure!(
                challenge.policies.len() <= vibeface_secure::authorization::MAX_POLICY_DOMAINS,
                "freshness proof exceeds policy budget"
            );
            let receipts = {
                let store = self.store.lock().await;
                verification.run(|| {
                    store.sign_current_receipts(&challenge, &self.identity, now_ms() as u64)
                })?
            };
            return Ok(json!({"receipts":receipts}));
        }
        ensure!(
            matches!(
                kind,
                "prepare" | "subscription" | "rpc" | "execute" | "inventory" | "event" | "blob"
            ),
            "unsupported peer operation"
        );
        let body = envelope["authorization_body"].clone();
        ensure!(
            body["space_id"].as_str() == Some(message.network_id.as_str()),
            "peer authorization belongs to another space"
        );
        if kind == "prepare" {
            let intent: Signed<PrepareIntent> = serde_json::from_value(envelope["intent"].clone())?;
            let origin = DeviceId::from_key(&PublicKey::parse(&certificate.public_key)?);
            let audience = self.audience()?;
            let prepared = {
                let store = self.store.lock().await;
                verification.run(|| -> Result<_> {
                    let snapshot = store.connection.unchecked_transaction()?;
                    let current = crate::identity::journal::PolicyState::reload_verified(
                        &snapshot,
                        &store.policy,
                    )?;
                    let prepared = TaskScopedPrepareAuthorization::verify_in(
                        &snapshot,
                        &current,
                        &PublicKey::parse(&self.identity.public_key_hex()?)?,
                        intent,
                        &body,
                        &audience,
                        RequestTransport::Existing(TransportEvidence::VerifiedPeer { origin }),
                        now_ms() as u64,
                    )?;
                    snapshot.commit()?;
                    Ok(prepared)
                })?
            };
            return self.prepare_authorized_body(prepared, &verification).await;
        }
        let expected_method = match kind {
            "execute" => Some("operate"),
            "inventory" => Some("replica_inventory"),
            "event" => Some("replica_event"),
            "blob" => Some("replica_blob"),
            _ => None,
        };
        if let Some(expected) = expected_method {
            ensure!(
                body["method"] == expected,
                "peer method differs from signed request"
            );
        }
        let invocation: Signed<Invocation> =
            serde_json::from_value(envelope["invocation"].clone())?;
        let origin = DeviceId::from_key(&PublicKey::parse(&certificate.public_key)?);
        let access = self
            .authorize_rpc_with_verification(
                body.clone(),
                invocation,
                TransportEvidence::VerifiedPeer { origin },
                verification.clone(),
            )
            .await?;
        ensure!(
            *access.authorization.audience() == self.audience()?,
            "peer request reached another service"
        );
        if kind == "subscription" {
            ensure!(
                access.authorization.action().is_read(),
                "subscription requires read permission"
            );
            let store = self.store.lock().await;
            let _verification = verification.enter();
            store.authorize_request_read(
                &access.authorization,
                &access.freshness,
                &access.transport,
                now_ms() as u64,
            )?;
            let mut heads = vec![];
            for scope in store.authorized_conversation_ids(&access.authorization)? {
                heads.push((scope.clone(), store.heads(&access.network_id, &scope)?));
            }
            heads.sort_by(|left, right| left.0.cmp(&right.0));
            return Ok(json!({"cursor":ObjectHash::body(&heads)?}));
        }
        if matches!(kind, "inventory" | "event" | "blob") {
            self.require_role_with_verification(
                &message.network_id,
                NodeRole::Replica,
                &verification,
            )
            .await?;
            let mut store = self.store.lock().await;
            let _verification = verification.enter();
            store.authorize_request_read(
                &access.authorization,
                &access.freshness,
                &access.transport,
                now_ms() as u64,
            )?;
            return crate::replication::query(&store, &access.authorization, &body);
        }
        self.rpc(access, body).await
    }
    async fn observe_engine(self: &Arc<Self>, envelope: Value) -> Result<()> {
        let _work = self.maintenance.enter_when_open().await;
        let context = &envelope["execution_context"];
        let space = SpaceId::parse(field(context, "space_id")?)?;
        let operation_id = id(context, "operation_id")?;
        let operation = self
            .store
            .lock()
            .await
            .operation(space.as_str(), &operation_id)?;
        let Some(operation) = operation else {
            return self
                .observe_handoff(
                    &envelope,
                    ResourceRef {
                        space: space.clone(),
                        resource: ResourceId::parse(field(context, "resource_id")?)?,
                    },
                    &operation_id,
                )
                .await;
        };
        if !matches!(
            operation.state,
            OperationState::Running | OperationState::Unknown
        ) || operation.request.action["type"] != "send"
        {
            return Ok(());
        }
        let request = &operation.request;
        let expected = execution_context(&*self.store.lock().await, request, 0)?;
        ensure!(
            context["resource_id"].as_str() == Some(expected.resource_id.as_str()),
            "Provider event belongs to another resource"
        );
        ensure!(
            if expected.shared {
                context["container_id"]
                    .as_str()
                    .is_some_and(|id| id.len() == 64)
            } else {
                context["container_id"].is_null()
            },
            "Provider event isolation context differs"
        );
        let event = &envelope["event"];
        let cli_progress = event["kind"] == "executor_progress";
        let method = event["method"].as_str().unwrap_or(if cli_progress {
            "executor/progress"
        } else {
            ""
        });
        let session = self
            .store
            .lock()
            .await
            .session(space.as_str(), &request.target_node, &request.session_id)?
            .context("running operation lost its session")?;
        if method == "bridge.codex_disconnected" && session.executor == "codex" {
            if operation.state == OperationState::Running {
                self.finish(
                    request,
                    OperationState::Unknown,
                    json!({"error":"Codex disconnected; reconcile before any retry"}),
                )
                .await?;
            }
            return Ok(());
        }
        let thread = event["params"]["threadId"]
            .as_str()
            .or_else(|| event["params"]["thread_id"].as_str())
            .or_else(|| event["params"]["session_id"].as_str())
            .or_else(|| event["native_session_id"].as_str());
        if thread.is_none()
            || thread != session.native_session_id.as_deref()
            || (cli_progress && event["executor"] != session.executor)
        {
            return Ok(());
        }
        let _ = self.events.send(Update {
            network_id: space.to_string(),
            payload: json!({
                "type":"execution_progress","conversation_id":request.conversation_id,
                "session_id":request.session_id,"event":event,
            }),
        });
        if method == "turn/completed" && operation.state == OperationState::Running {
            let turn = field(&event["params"]["turn"], "id")?;
            let known = self
                .store
                .lock()
                .await
                .local_value(space.as_str(), &format!("turn:{operation_id}"))?;
            if known
                .as_ref()
                .and_then(|value| value["turn"]["id"].as_str())
                != Some(turn)
            {
                return Ok(());
            }
            let state = if event["params"]["turn"]["status"] == "completed" {
                OperationState::Succeeded
            } else {
                OperationState::Failed
            };
            self.finish(request, state, event.clone()).await?;
        }
        Ok(())
    }
    async fn reconcile(
        self: &Arc<Self>,
        network: &str,
        target: &str,
        operation_id: &str,
    ) -> Result<Value> {
        ensure!(
            target == self.identity.node_id()?,
            "reconcile through a signed request to the target service"
        );
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
            let context = execution_context(&*self.store.lock().await, &operation.request, 0)?;
            let turn = match self
                .engine
                .read_turn_authorized(&context, native_id, turn_id)
                .await
            {
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
                actual["reconciled_turn"] = turn;
            }
        }
        Ok(
            json!({"operation":self.store.lock().await.operation(network,operation_id)?,"actual":actual}),
        )
    }
    async fn resolve_unknown(self: &Arc<Self>, access: &Access, input: &Value) -> Result<Value> {
        access.require_body(input)?;
        let operation = {
            let mut store = self.store.lock().await;
            access.verification.run(|| {
                store.resolve_request_authorized_operation(
                    &self.identity,
                    &access.authorization,
                    &access.freshness,
                    &access.transport,
                    input,
                    now_ms() as u64,
                )
            })?
        };
        self.publish_changed(&access.network_id, &operation.request.conversation_id, "");
        Ok(json!({"operation":operation}))
    }
    fn spawn_scheduler(
        self: &Arc<Self>,
        network: String,
        scope: String,
        trigger: String,
    ) -> futures_util::future::BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let work = self.maintenance.enter()?;
            let audience = self.audience()?;
            let session = vibeface_secure::authorization::SessionRef {
                target: audience.device.clone(),
                session_id: trigger.clone(),
            };
            let access = self.store.lock().await.agent_grant(
                &network,
                &scope,
                &audience.service,
                &session,
                &trigger,
                now_ms() as u64,
            )?;
            let node = self.clone();
            tokio::spawn(async move {
                let _work = work;
                if let Err(error) = crate::scheduler::run(
                    node.clone(),
                    access.clone(),
                    scope.clone(),
                    trigger.clone(),
                )
                .await
                    && crate::scheduler::status(&node, &network, &scope, &trigger)
                        .await
                        .is_ok_and(|status| status["state"] == "not_found")
                {
                    let _ = node
                        .agent_notification(
                            &access,
                            "manager_failed",
                            &format!("管理 Agent 未完成：{error:#}"),
                        )
                        .await;
                }
            });
            Ok(())
        })
    }
    fn maybe_followup<'a>(
        self: &'a Arc<Self>,
        request: &'a OperationRequest,
        result_hash: &'a str,
        verification: &'a SignatureVerification,
    ) -> futures_util::future::BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            if request.action["type"] != "send" {
                return Ok(true);
            }
            let Some(task) = &request.task_id else {
                return Ok(true);
            };
            let authorized = {
                let store = self.store.lock().await;
                verification.run(|| {
                    followup_authorized(&store, &request.network_id, &request.conversation_id, task)
                })?
            };
            if !authorized {
                return Ok(true);
            }
            {
                let store = self.store.lock().await;
                let _verification = verification.enter();
                let result = store
                    .event(&request.network_id, result_hash)?
                    .context("continuation result is missing")?;
                let result = store.verify_event(&request.network_id, &result)?;
                if !matches!(&result.cause, EventCause::Derived { admission, .. }
                    if matches!(admission.invocation.payload.actor, Actor::Service(_)))
                {
                    return Ok(true);
                }
            }
            if self.config.manager.is_none() || !self.config.roles.contains(&NodeRole::Scheduler) {
                return Ok(false);
            }
            let audience = self.audience()?;
            let session = vibeface_secure::authorization::SessionRef {
                target: audience.device,
                session_id: result_hash.to_owned(),
            };
            let access = {
                let mut store = self.store.lock().await;
                let _verification = verification.enter();
                if store
                    .continuation(&request.network_id, result_hash, "v2")?
                    .is_some()
                    || !followup_authorized(
                        &store,
                        &request.network_id,
                        &request.conversation_id,
                        task,
                    )?
                {
                    return Ok(true);
                }
                let access = store.agent_grant(
                    &request.network_id,
                    &request.conversation_id,
                    &audience.service,
                    &session,
                    result_hash,
                    now_ms() as u64,
                )?;
                access
            };
            let sdk_config =
                crypto::digest(&serde_json::to_vec(self.config.manager.as_ref().unwrap())?);
            let admission = match self
                .claim_manager(
                    &access,
                    result_hash,
                    crate::scheduler::boot_id(),
                    &sdk_config,
                )
                .await
            {
                Ok(admission) => admission,
                Err(error)
                    if error
                        .downcast_ref::<crate::scheduler::ManagerBusy>()
                        .is_some() =>
                {
                    return Ok(false);
                }
                Err(error) => return Err(error),
            };
            let work = self.maintenance.enter()?;
            let node = self.clone();
            let request = request.clone();
            let source = result_hash.to_owned();
            tokio::spawn(async move {
                let _work = work;
                let outcome = crate::scheduler::run_admitted(
                    node.clone(),
                    access,
                    request.conversation_id.clone(),
                    source.clone(),
                    Some(admission),
                )
                .await;
                let (state, value) = match outcome {
                    Ok(value) if value["state"] == "completed" => ("completed", value),
                    Ok(value) if value["state"] == "failed" => ("failed", value),
                    Ok(value) => ("unknown", value),
                    Err(error) => {
                        let state = crate::scheduler::status(
                            &node,
                            &request.network_id,
                            &request.conversation_id,
                            &source,
                        )
                        .await;
                        let confirmed_failed = state.is_ok_and(|value| value["state"] == "failed");
                        (
                            if confirmed_failed {
                                "failed"
                            } else {
                                "unknown"
                            },
                            json!({"error":format!("{error:#}")}),
                        )
                    }
                };
                if let Ok(target) = node.identity.node_id() {
                    let _ = node.store.lock().await.finish_continuation(
                        &request.network_id,
                        &source,
                        "v2",
                        &target,
                        state,
                        value,
                    );
                }
            });
            Ok(true)
        })
    }
}
